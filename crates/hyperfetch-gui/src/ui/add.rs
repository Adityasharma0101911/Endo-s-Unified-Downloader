use hyperfetch_core::zipdir::ZipEntry;
use url::Url;

use super::*;


/// Adds the links of `text` from the clipboard to the queue, which the form's checksum and
/// Authorization header don't apply to: a paste with secrets or a copied request as
/// `App::add_pasted` does, else each line as one download. A link to read first (a .torrent,
/// playlist, folder) is read.
pub(super) fn add_link(app: &mut App, text: &str) {
    match app.add_pasted(text, "", "", Origin::Dropped) {
        Some(Ok((ids, _))) if ids.is_empty() => return,
        Some(added) => {
            app.notice = Some(added.map(|(ids, notes)| {
                let added = match &ids[..] {
                    [id] => format!("Added #{} to the queue", id),
                    _ => format!("Added {} downloads to the queue", ids.len()),
                };
                std::iter::once(added).chain(notes).collect::<Vec<_>>().join("\n")
            }));
            return;
        }
        None => {}
    }
    // A link with ranges stands for its links, all lines together at most so many.
    let expanded = match ingest::expand_lines(text.lines().map(str::trim).filter(|l| !l.is_empty())) {
        Ok(expanded) => expanded,
        Err(e) => return app.notice = Some(Err(e)),
    };
    for links in expanded {
        let links = match links {
            Ok(links) => links,
            Err(e) => {
                app.notice = Some(Err(e));
                continue;
            }
        };
        for link in links {
            if ingest::needs_reading(&link) {
                app.read_document(link, Origin::Dropped, String::new(), String::new());
            } else {
                app.notice = Some(app.add_download(&link, "", "").map(|id| format!("Added #{} to the queue", id)));
            }
        }
    }
}

/// Shows the banner `show` makes of `now`. As `now` comes, its room opens over 0.2 s, pushing
/// what is under it down smoothly; once it is gone (dismissed, answered, over), what it last
/// showed fades out while its room closes up, its buttons no longer working.
pub(super) fn fading<T: Clone + Send + Sync + 'static>(ui: &mut Ui, name: &str, now: Option<T>, show: impl FnOnce(&mut Ui, T)) {
    let id = egui::Id::new(("banner ghost", name));
    let live = now.is_some();
    let shown = anim::presence(ui.ctx(), id, live, 0.2);
    if let Some(now) = &now {
        ui.data_mut(|d| d.insert_temp(id, now.clone()));
    }
    let Some(value) = now.or_else(|| ui.data(|d| d.get_temp::<T>(id))) else { return };
    if live && shown == 1.0 {
        return show(ui, value);
    }
    if shown == 0.0 && !live {
        return ui.data_mut(|d| {
            d.remove::<T>(id);
            d.remove::<f32>(id.with("height"));
        });
    }
    // Cut to the room it has (its height last frame), so that what moves never overlaps it.
    let height = ui.data(|d| d.get_temp::<f32>(id.with("height"))).unwrap_or(if live { 0.0 } else { f32::INFINITY });
    let rect = ui.available_rect_before_wrap();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(*ui.layout()));
    let clip = child.clip_rect();
    child.set_clip_rect(clip.intersect(Rect::from_x_y_ranges(clip.x_range(), rect.top()..=rect.top() + height * shown)));
    child.multiply_opacity(shown);
    if !live {
        child.disable();
    }
    show(&mut child, value);
    let height = child.min_rect().height() + ui.spacing().item_spacing.y;
    ui.data_mut(|d| d.insert_temp(id.with("height"), height));
    ui.add_space(height * shown);
}

pub(super) fn notice_banner(app: &mut App, ui: &mut Ui) {
    fading(ui, "notice", app.notice.clone(), |ui, notice| {
        let p = palette(ui);
        let (text, color, glyph) = match &notice {
            Ok(text) => (text, p.green, icon::CHECK_CIRCLE),
            Err(text) => (text, p.red, icon::WARNING_CIRCLE),
        };
        banner(ui, color, glyph, RichText::new(text).color(p.text), |ui| {
            if icon_button(ui, icon::X, "Dismiss", true).clicked() {
                app.notice = None;
            }
        });
    });
}

/// For 8 s after downloads left the queue (Remove, Clear), a note at the bottom of the window
/// says so and offers Undo, which puts them back where they were. It floats over the page, so
/// that nothing on it moves; it slides up and fades in, and out again.
pub(super) fn undo_toast(app: &mut App, ctx: &egui::Context) {
    const SHOWN: f64 = 8.0;
    let now = ctx.input(|i| i.time);
    if app.removed.as_ref().is_some_and(|(_, at)| now - at >= SHOWN) {
        app.removed = None;
    }
    let id = egui::Id::new("undo toast");
    if let Some((items, _)) = &app.removed {
        let text = match &items[..] {
            [(_, item)] => format!("Removed {}", truncate_chars(&item.filename, 50)),
            items => format!("Removed {} downloads", items.len()),
        };
        ctx.data_mut(|d| d.insert_temp(id, text));
    }
    let live = app.removed.is_some();
    let shown = anim::ease(anim::presence(ctx, id, live, 0.2));
    let Some(text) = ctx.data(|d| d.get_temp::<String>(id)).filter(|_| shown > 0.0) else { return };
    let p = palette_of(ctx);
    let rise = Vec2::new(0.0, -24.0 + 12.0 * (1.0 - shown));
    egui::Area::new(id).order(egui::Order::Foreground).anchor(Align2::CENTER_BOTTOM, rise).interactable(live).show(ctx, |ui| {
        ui.multiply_opacity(shown);
        let shadow = egui::Shadow { offset: Vec2::new(0.0, 6.0), blur: 18.0, spread: 0.0, color: Color32::from_black_alpha(70) };
        Frame::none().fill(p.card).stroke(Stroke::new(1.0, p.border)).rounding(10.0).shadow(shadow).inner_margin(Margin::symmetric(14.0, 8.0)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::TRASH).size(16.0).color(p.muted));
                ui.label(RichText::new(text).color(p.text));
                ui.add_space(8.0);
                if ui.add(primary_button("Undo", p.accent)).clicked() {
                    app.undo_remove();
                }
                if icon_button(ui, icon::X, "Dismiss", true).clicked() {
                    app.removed = None;
                }
            });
        });
    });
    // A frame when its time is up, so that it goes by itself.
    if let Some((_, at)) = &app.removed {
        ctx.request_repaint_after(Duration::from_secs_f64((at + SHOWN - now).clamp(0.0, SHOWN)));
    }
}

/// The first-run card: the three steps to a first download, until it is closed.
pub(super) fn welcome(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    anim::fade_in(ui, egui::Id::new("welcome"), 0.0, anim::RISE, |ui| {
        card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(bold("Welcome").size(16.0).color(p.strong));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(primary_button("Got it", p.accent)).clicked() {
                        app.settings.welcomed = true;
                    }
                });
            });
            ui.label(RichText::new("Three steps to your first download:").color(p.muted));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                chip(ui, "1", p.accent);
                ui.label(RichText::new("Choose where downloads are saved.").color(p.text));
                if ui.add_enabled(!app.dialog_open, button("Choose folder\u{2026}")).clicked() {
                    app.pending_dialog = Some(Dialog::SaveDir);
                }
            });
            ui.horizontal(|ui| {
                chip(ui, "2", p.accent);
                ui.label(RichText::new("Add the browser extension, which sends the videos and links of the pages you visit.").color(p.text));
                if let Some(dir) = hyperfetch_core::updater::extension_dir().filter(|dir| dir.is_dir()) {
                    let about = "Chrome, Edge and Brave load it from this folder: Extensions, Developer mode, Load unpacked.";
                    if ui.add(button("Open its folder")).on_hover_text(about).clicked() {
                        if let Err(e) = util::open_path(&dir) {
                            app.notice = Some(Err(format!("Could not open {}: {}", dir.display(), e)));
                        }
                    }
                }
            });
            ui.horizontal(|ui| {
                chip(ui, "3", p.accent);
                ui.label(RichText::new(keys("Paste a link in the bar above and press Download, or press Ctrl+V anywhere in the window.")).color(p.text));
            });
        });
    });
    ui.add_space(12.0);
}

/// Asks whether ffmpeg may be installed, while a media download waits for the answer (see
/// `App::start_job`).
pub(super) fn ffmpeg_prompt(app: &mut App, ui: &mut Ui) {
    fading(ui, "ffmpeg prompt", app.asks_about_ffmpeg().then_some(()), |ui, ()| {
        let p = palette(ui);
        let text = format!("ffmpeg is not installed. Install it for the best video quality and MP3/M4A files ({})?", media::FFMPEG_DOWNLOAD);
        banner(ui, p.amber, icon::FILM_STRIP, RichText::new(text).color(p.text), |ui| {
            if ui.add(primary_button("Install ffmpeg", p.accent)).clicked() {
                app.answer_ffmpeg(true);
            }
            if ui.add(button("Continue without")).clicked() {
                app.answer_ffmpeg(false);
            }
        })
        .on_hover_text(FFMPEG_ABOUT);
    });
}

/// While ffmpeg is `installing`, a notice, and a frame a second so that it goes when the install
/// ends, also when no download runs any more (a cancelled video leaves the install to finish).
pub(super) fn ffmpeg_notice(ui: &mut Ui, installing: bool) {
    fading(ui, "ffmpeg install", installing.then_some(()), |ui, ()| {
        let p = palette(ui);
        let text = format!("Installing ffmpeg ({} download); videos that need it wait until it is ready.", media::FFMPEG_DOWNLOAD);
        banner(ui, p.amber, icon::FILM_STRIP, RichText::new(text).color(p.text), |_| {});
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    });
}

/// Offers the newer version a check found, until dismissed for this run. Installing in place
/// works on Windows and macOS; elsewhere What's new leads to the download.
pub(super) fn update_banner(app: &mut App, ui: &mut Ui) {
    fading(ui, "update", app.update.clone(), |ui, update| {
        let p = palette(ui);
        let text = if app.updating { "Downloading the update…".to_string() } else { format!("Version {} is available.", update.version) };
        // Keyed by the version, so that it does not drop in again as it starts downloading.
        let id = egui::Id::new(("update banner", &update.version));
        keyed_banner(ui, id, p.green, icon::ARROW_CIRCLE_UP, bold(text).color(p.text), |ui| {
            if icon_button(ui, icon::X, "Dismiss", !app.updating).clicked() {
                app.update = None;
            }
            if ui.add(button("What's new")).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(&update.page));
            }
            let installs = cfg!(any(windows, target_os = "macos"));
            if installs && ui.add_enabled(!app.updating, primary_button("Update and restart", p.accent)).clicked() {
                app.install_update();
            }
        });
    });
}

pub(super) fn clipboard_banner(app: &mut App, ui: &mut Ui) {
    fading(ui, "clipboard", app.clipboard_banner.clone(), |ui, (shown, link)| {
        let p = palette(ui);
        let text = RichText::new(format!("Copied link: {}", truncate_chars(&shown, 55))).color(p.text);
        banner(ui, p.accent, icon::CLIPBOARD_TEXT, text, |ui| {
            if icon_button(ui, icon::X, "Dismiss", true).clicked() {
                app.clipboard_banner = None;
            }
            if ui.add(button("Paste")).on_hover_text("Put it in the link box").clicked() {
                app.clipboard_banner = None;
                app.new_download();
                app.url_input = link.clone();
                app.tab = Tab::Downloads;
            }
            if ui.add(button("Add to queue")).clicked() {
                app.clipboard_banner = None;
                add_link(app, &link);
            }
            if ui.add(primary_button("Download now", p.accent)).clicked() {
                app.clipboard_banner = None;
                app.download_now(&link, "", "", true);
            }
        })
        .on_hover_text(&shown);
    });
}

/// Asks before adding the many downloads a large .metalink, .meta4, .torrent or playlist lists,
/// and whether a video link that names its playlist too means the video or the whole playlist:
/// at once, the video can be picked while the playlist is read.
pub(super) fn listing_prompt(app: &mut App, ui: &mut Ui) {
    let asked = app.listings.first().map(|listing| {
        let input = listing.input.clone();
        let video = listing.video.is_some();
        let playlist_ready = listing.tasks.as_ref().is_some_and(|tasks| !tasks.is_empty());
        let (text, all) = if video {
            let text = format!("{} is a video in a playlist. Download:", truncate_chars(&input, 55));
            let all = match listing.tasks.as_deref() {
                None => "Whole playlist (reading...)".to_string(),
                Some([]) => "Whole playlist (nothing new)".to_string(),
                Some(tasks) => format!("Whole playlist ({})", tasks.len()),
            };
            (text, all)
        } else {
            (format!("{} lists {}. Add them all?", truncate_chars(&input, 55), listing.summary()), "Add all".to_string())
        };
        (input, video, playlist_ready, text, all)
    });
    fading(ui, "listing", asked, |ui, (input, video, playlist_ready, text, all)| {
        let p = palette(ui);
        banner(ui, p.amber, icon::LIST_CHECKS, RichText::new(text).color(p.text), |ui| {
            if ui.add(button("Cancel")).clicked() {
                app.answer_listing(Answer::Cancel);
            }
            if !video {
                if ui.add(primary_button(&all, p.accent)).clicked() {
                    app.answer_listing(Answer::All);
                }
                return;
            }
            if ui.add_enabled(playlist_ready, button(&all)).clicked() {
                app.answer_listing(Answer::All);
            }
            if ui.add(primary_button("This video", p.accent)).clicked() {
                app.answer_listing(Answer::Video);
            }
        })
        .on_hover_text(&input);
    });
}

/// 0.0 to 1.0 (linear) over 1.2 s since a download was last added to the queue; 1.0 at first.
/// The sidebar calls it every frame, so that a download added on another page counts as long ago
/// once the Add page shows.
pub(super) fn just_added(app: &App, ctx: &egui::Context) -> f32 {
    let added = super::sidebar::rises(ctx, egui::Id::new("queue size"), app.queue.items().len());
    anim::changed(ctx, egui::Id::new("just added"), added, 1.2)
}

/// While files are dragged over the window, a drop zone over all of it, gently pulsing; it fades
/// in and out.
pub(super) fn drop_zone(ctx: &egui::Context) {
    let dragging = ctx.input(|i| !i.raw.hovered_files.is_empty());
    let shown = anim::presence(ctx, egui::Id::new("drop zone"), dragging, 0.15);
    if shown == 0.0 {
        return;
    }
    let p = palette_of(ctx);
    let pulse = if dragging { anim::pulse(ctx, 1.2) } else { 0.0 };
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop zone")));
    let zone = ctx.screen_rect().shrink(16.0 + 4.0 * pulse);
    painter.rect_filled(ctx.screen_rect(), 0.0, p.bg.gamma_multiply(0.88 * shown));
    let edge = Stroke::new(2.0, p.accent.gamma_multiply((0.55 + 0.45 * pulse) * shown));
    painter.rect(zone, 14.0, p.accent.gamma_multiply(0.06 * shown), edge);
    let center = zone.center() - Vec2::new(0.0, 22.0 + 6.0 * pulse);
    anim::paint_icon(&painter, center, icon::FILE_ARROW_DOWN, 52.0, p.accent.gamma_multiply(shown), 0.0);
    let text = "Drop .torrent, .metalink, .meta4 or .txt files to add every file they list";
    painter.text(zone.center() + Vec2::new(0.0, 22.0), Align2::CENTER_TOP, text, FontId::proportional(15.0), p.text.gamma_multiply(shown));
}

/// `add` drawn `dx` points to the side, what follows laid out as if it were not: for a shake.
pub(super) fn nudged(ui: &mut Ui, dx: f32, add: impl FnOnce(&mut Ui)) {
    let offset = Vec2::new(dx, 0.0);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(ui.available_rect_before_wrap().translate(offset)).layout(*ui.layout()));
    add(&mut child);
    ui.advance_cursor_after_rect(child.min_rect().translate(-offset));
}

/// The front door: one large bar to paste or drop anything into, always at the top of the
/// Downloads page. As soon as something is in it, a card under it says in plain words what it
/// is (see `kinds::recognise`), where it comes from and the two or three choices that matter for
/// that kind, with one Download button; More options holds the rest, and stays open once opened.
pub(super) fn front_door(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let ctx = ui.ctx().clone();
    let id = egui::Id::new("url_input");
    let lit = anim::presence(&ctx, id.with("lit"), ctx.memory(|m| m.has_focus(id)) || !app.url_input.trim().is_empty(), 0.22);
    let recognised = kinds::recognise(&app.url_input);
    let glyph = recognised.as_ref().map_or(icon::LINK, |(kind, _)| kind.glyph);
    let frame = Frame::none()
        .fill(p.card)
        .stroke(Stroke::new(1.0 + lit, p.border.lerp_to_gamma(p.link, lit)))
        .rounding(16.0)
        .inner_margin(Margin { left: 16.0, right: 8.0, top: 8.0, bottom: 8.0 })
        .shadow(card_shadow(ui, 0.3 + 0.7 * lit));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            anim::icon(ui, glyph, 20.0, p.muted.lerp_to_gamma(p.link, lit), 1.0, 0.0);
            let edit = egui::TextEdit::singleline(&mut app.url_input)
                .id(id)
                .hint_text("Paste or drop anything: a link, a magnet, a playlist, a .torrent…")
                .font(FontId::proportional(16.0))
                .frame(false)
                .margin(Vec2::new(6.0, 10.0))
                .desired_width((ui.available_width() - 214.0).max(120.0));
            let response = ui.add(edit);
            if response.changed() {
                app.form_error = None;
            }
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                start_from_form(app);
            }
            if ui.add_sized([86.0, 38.0], button(format!("{} Paste", icon::CLIPBOARD_TEXT))).on_hover_text("Paste the copied link here").clicked() {
                app.paste_url();
            }
            if download_button(app, ui).clicked() {
                start_from_form(app);
            }
        });
    });
    if ingest::is_blob_url(&app.url_input) {
        error_alert(ui, ingest::BLOB_MESSAGE);
    }
    // A new error shakes its alert; it opens and closes its room smoothly. The shake follows
    // every change, but an alert fading out stays still.
    let dx = anim::shake(&ctx, egui::Id::new("form error"), &app.form_error);
    let dx = if app.form_error.is_some() { dx } else { 0.0 };
    fading(ui, "form error", app.form_error.clone(), |ui, error| nudged(ui, dx, |ui| error_alert(ui, &error)));
    let shown = recognised.map(|(kind, source)| (kind.name, source));
    fading(ui, "recognised", shown, |ui, (name, source)| {
        let Some(kind) = kinds::KINDS.iter().chain(std::iter::once(&kinds::SEVERAL)).find(|kind| kind.name == name) else { return };
        ui.add_space(8.0);
        preview_card(app, ui, kind, &source);
    });
    ui.add_space(14.0);
}

/// The card under the link bar: what the input is, where from and what it holds, the choices
/// that matter for its kind (the quality of a video, a stream's tracks, a ZIP's files), where it
/// goes, and More options.
fn preview_card(app: &mut App, ui: &mut Ui, kind: &kinds::Kind, source: &str) {
    let p = palette(ui);
    card(ui).inner_margin(Margin::symmetric(18.0, 14.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 20.0, p.accent.gamma_multiply(0.16));
            anim::paint_icon(ui.painter(), rect.center(), kind.glyph, 20.0, p.link, 0.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal(|ui| {
                    ui.label(bold(kind.name).size(16.0).color(p.strong));
                    if !source.is_empty() {
                        ui.label(RichText::new(format!("from {source}")).color(p.muted));
                    }
                });
                // A file's link ends in its name; a video page's ends in "watch": the site names it.
                match shown_name(&app.url_input, kind) {
                    Some(name) => {
                        ui.add(egui::Label::new(RichText::new(&name).color(p.text)).truncate()).on_hover_text(&name);
                    }
                    None if kind.name != kinds::SEVERAL.name => {
                        let what = if kind.choice == kinds::Choice::Quality { "Its title" } else { "Its name" };
                        ui.label(RichText::new(format!("{what} comes from the site once it starts")).color(p.muted));
                    }
                    None => {}
                }
                ui.add(egui::Label::new(RichText::new(kind.about).small().color(p.muted)).wrap());
                match kind.needs {
                    kinds::Needs::Account(what) => {
                        ui.label(RichText::new(format!("{} Uses your own account: Settings, {what}", icon::KEY)).small().color(p.amber));
                    }
                    kinds::Needs::Tool(tool) => {
                        ui.label(RichText::new(format!("{} {tool} is fetched the first time it is needed", icon::WRENCH)).small().color(p.dim));
                    }
                    kinds::Needs::Nothing => {}
                }
            });
        });
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            if kind.choice == kinds::Choice::Quality {
                ui.label(RichText::new(icon::FILM_STRIP).size(16.0).color(p.muted)).on_hover_text("Quality");
                combo(ui, "media_preset_combo", &mut app.settings.media_preset, &MEDIA_PRESETS);
            }
            if kind.choice == kinds::Choice::Files {
                ui.label(RichText::new(format!("{} You pick its files once it is read", icon::LIST_CHECKS)).color(p.muted));
            }
            // Brackets that are part of the file's name: the link as written, a range no more.
            if kinds::range_span(&app.url_input).is_ok_and(|span| span.is_some()) {
                let tip = "Its brackets are part of the link, not a range: they are written as %5B and %5D, which the server reads the same";
                if ui.add(button(format!("{} Take as one link", icon::LINK))).on_hover_text(tip).clicked() {
                    app.url_input = ingest::as_written(&app.url_input);
                }
            }
            // Beside Download (what the page holds): the page itself, as one file.
            if kind.choice == kinds::Choice::Page {
                let link = Url::parse(app.url_input.trim()).ok();
                let saving = app.saving_page.is_some();
                let label = if saving { format!("{} Saving the page…", icon::HOURGLASS) } else { format!("{} Save this page as one file", icon::FILE_HTML) };
                let tip = "Its images, styles and frames put into one HTML file that opens offline; its scripts left out unless More options keeps them";
                if ui.add_enabled(link.is_some() && !saving, button(label)).on_hover_text(tip).clicked() {
                    if let Some(link) = link {
                        app.save_page(link);
                    }
                }
            }
            ui.label(RichText::new(icon::FOLDER_OPEN).size(16.0).color(p.muted)).on_hover_text("Save to");
            let folder = std::path::Path::new(&app.settings.save_dir);
            let name = folder.file_name().map_or_else(|| app.settings.save_dir.clone(), |n| n.to_string_lossy().into_owned());
            ui.label(RichText::new(name).color(p.text)).on_hover_text(&app.settings.save_dir);
            if ui.add_enabled(!app.dialog_open, button("Change…")).on_hover_text("Choose where downloads are saved").clicked() {
                app.pending_dialog = Some(Dialog::SaveDir);
            }
            // One click back to a folder saved to before.
            let recent = app.settings.recent_dirs.clone();
            if !recent.is_empty() {
                let menu = ui.make_persistent_id("recent folders");
                let open = icon_button(ui, icon::CLOCK_COUNTER_CLOCKWISE, "Recent folders", true);
                if open.clicked() {
                    ui.memory_mut(|m| m.toggle_popup(menu));
                }
                egui::popup_below_widget(ui, menu, &open, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                    ui.set_min_width(260.0);
                    for dir in recent {
                        if ui.selectable_label(false, &dir).clicked() {
                            app.settings.set_save_dir(dir);
                        }
                    }
                });
            }
        });
        zip_preview(app, ui);
        update_offer(app, ui);
        stream_tracks(app, ui, None);
        ui.add_space(4.0);
        more_options(app, ui);
    });
}

/// The name the input gives what it downloads, shown on the preview card: a magnet's own name, or
/// the last part of a link of a `kind` that ends in its file's name (see `kinds::named_by_link`).
fn shown_name(input: &str, kind: &kinds::Kind) -> Option<String> {
    // A link with ranges: how many links, from the first file to the last.
    if let Ok(Some((count, first, last))) = kinds::range_span(input) {
        if count > 1 {
            let name = |link: &str| Url::parse(link).ok().and_then(|url| url.path_segments()?.filter(|s| !s.is_empty()).next_back().map(percent_decoded));
            let (from, to) = match (name(&first), name(&last)) {
                (Some(from), Some(to)) if from != to => (from, to),
                _ => (first, last),
            };
            return Some(format!("{count} links, {from} to {to}"));
        }
    }
    let first = ingest::split_tokens(input.trim()).into_iter().next()?;
    let url = Url::parse(&first).ok()?;
    if url.scheme() == "magnet" {
        return url.query_pairs().find(|(key, _)| key == "dn").map(|(_, name)| name.into_owned()).filter(|name| !name.trim().is_empty());
    }
    let url = Some(url).filter(|url| kinds::named_by_link(kind) && matches!(url.scheme(), "http" | "https" | "ftp" | "ftps" | "sftp"))?;
    let last = url.path_segments()?.filter(|s| !s.is_empty()).next_back()?;
    let name = percent_decoded(last);
    (!name.is_empty()).then_some(name)
}

/// `text` with its `%XX` escapes decoded, as a link's path shows a file name.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut n = 0;
    while n < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[n], bytes.get(n + 1).copied().and_then(hex), bytes.get(n + 2).copied().and_then(hex)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                n += 3;
            }
            (byte, ..) => {
                out.push(byte);
                n += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// More options: a caret that turns, and the options dropping in, their room opening (and
/// closing) smoothly: connections, a checksum, an older version and an Authorization header, all
/// for this download only.
fn more_options(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let open = anim::presence(ui.ctx(), egui::Id::new("advanced options"), app.show_advanced, 0.18);
    let clicked = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let caret = anim::icon(ui, icon::CARET_RIGHT, 14.0, p.muted, 1.0, open * std::f32::consts::FRAC_PI_2);
            let button = ui.add(egui::Button::new(RichText::new("More options").color(p.muted)).frame(false));
            let button = button.on_hover_text("Connections, checksum, an older version, an Authorization header");
            button.clicked() || ui.interact(caret.rect, egui::Id::new("advanced caret"), Sense::click()).clicked()
        })
        .inner;
    if clicked {
        app.show_advanced = !app.show_advanced;
    }
    let shown = app.show_advanced.then_some(());
    fading(ui, "advanced options", shown, |ui, ()| anim::shifted(ui, open, anim::DROP, |ui| advanced_options(app, ui)));
}

/// The file an earlier download of the single link in the box finished into, when it is still
/// there (history keeps links without their secrets; one that had any is not matched).
fn copy_on_disk(app: &App) -> Option<std::path::PathBuf> {
    use hyperfetch_core::history::{is_redacted, redact_url};
    let link = app.url_input.trim();
    let url = Url::parse(link).ok().filter(|u| matches!(u.scheme(), "http" | "https" | "ftp" | "ftps") && !link.contains(char::is_whitespace))?;
    let saved = redact_url(url.as_str());
    let finished = app.history.iter().find(|e| e.status == HistoryStatus::Completed && e.urls.iter().any(|u| *u == saved && !is_redacted(u)))?;
    finished.file_path.is_file().then(|| finished.file_path.clone())
}

/// For a link whose earlier download's file is still on disk: Update from the copy on disk, which
/// downloads it again in that file's place only if the server has a newer one, reusing what the
/// copy still holds when the server publishes block checksums (see `App::download_now`).
fn update_offer(app: &mut App, ui: &mut Ui) {
    let Some(copy) = copy_on_disk(app) else { return };
    let p = palette(ui);
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(icon::INFO).color(p.accent));
        let name = copy.file_name().unwrap_or_default().to_string_lossy();
        ui.label(RichText::new(format!("{name} is on disk from an earlier download of this link.")).color(p.muted));
        let hint = "Download it again in its place only if the server has a newer version; what the copy still holds is                     reused when the server publishes block checksums (a .zsync file or a metalink's pieces)";
        if ui.add(button(format!("{} Update from the copy on disk", icon::ARROWS_CLOCKWISE))).on_hover_text(hint).clicked() {
            app.update_on_disk = Some(copy.clone());
            start_from_form(app);
        }
    });
}

/// The link in `input` when it is a single http(s) link to a .zip, whose contents can be listed.
fn zip_link(input: &str) -> Option<Url> {
    let input = input.trim();
    let url = Url::parse(input).ok().filter(|u| matches!(u.scheme(), "http" | "https") && !input.contains(char::is_whitespace))?;
    url.path().to_ascii_lowercase().ends_with(".zip").then_some(url)
}

/// For a single .zip link in the box, See what's inside: the ZIP's files with their sizes and
/// the total, read without downloading it (see `App::preview_zip`); or why it could not be.
fn zip_preview(app: &mut App, ui: &mut Ui) {
    let Some(url) = zip_link(&app.url_input) else { return };
    let p = palette(ui);
    match app.zip_preview.as_ref().filter(|(link, _)| link == url.as_str()).map(|(_, listed)| listed) {
        None => {
            ui.add_space(4.0);
            let see = ui.add(button(format!("{} See what's inside", icon::FILE_ZIP)));
            if see.on_hover_text("List the files in the ZIP without downloading it").clicked() {
                app.preview_zip(url);
            }
        }
        Some(None) => {
            ui.horizontal(|ui| {
                anim::spinner(ui, 14.0, p.muted);
                ui.label(RichText::new("Reading what the ZIP holds…").color(p.muted));
            });
            // Files with their sizes, then the total.
            skeleton(ui, &[(0.42, 56.0), (0.3, 48.0), (0.36, 60.0), (0.26, 0.0)]);
        }
        Some(Some(Err(error))) => error_alert(ui, error),
        Some(Some(Ok(_))) => zip_entries(app, ui),
    }
}

/// For a single HLS or DASH link in the box, Choose tracks…, which reads the stream and opens its
/// track chooser here (see `App::list_tracks`); for the download shown, its chooser once opened
/// (from its queue row or its own button).
pub(super) fn stream_tracks(app: &mut App, ui: &mut Ui, focused: Option<&QueueItem>) {
    let open = match focused {
        Some(item) => app.tracks.as_ref().is_some_and(|chooser| chooser.item == Some(item.id)),
        None => {
            let Some(url) = crate::tracks::stream_link(&app.url_input) else { return };
            let open = app.tracks.as_ref().is_some_and(|chooser| chooser.item.is_none() && chooser.link == url.as_str());
            if !open {
                ui.add_space(4.0);
                let choose = ui.add(button(format!("{} Choose tracks…", icon::SLIDERS_HORIZONTAL)));
                if choose.on_hover_text("Pick the video, audio, subtitles and parts of the stream to download").clicked() {
                    app.list_tracks(None, url);
                }
            }
            open
        }
    };
    if open {
        super::chooser::chooser(app, ui);
    }
}

/// Asks which channels of the first channel list waiting to record: its channels with their
/// groups to tick, filtered by what is typed, then a download each or none.
pub(super) fn channel_prompt(app: &mut App, ui: &mut Ui) {
    let asked = app.channel_lists.first().map(|list| (list.input.clone(), list.channels.len()));
    fading(ui, "channels", asked, |ui, (input, count)| {
        let p = palette(ui);
        let ticked = app.channel_lists.first().filter(|list| list.input == input).map_or(0, |list| list.channels.iter().filter(|(_, on)| *on).count());
        let text = format!("{} is a list of {} channels. Tick the ones to record:", truncate_chars(&input, 55), count);
        banner(ui, p.amber, icon::TELEVISION, RichText::new(text).color(p.text), |ui| {
            if ui.add(button("Cancel")).clicked() {
                app.answer_channels(false);
            }
            let add = if ticked == 1 { "Add 1 channel".to_string() } else { format!("Add {} channels", ticked) };
            if ui.add_enabled(ticked > 0, primary_button(&add, p.accent)).clicked() {
                app.answer_channels(true);
            }
        })
        .on_hover_text(&input);
        // Answered, its ghost fades out as the banner alone.
        let Some(list) = app.channel_lists.first_mut().filter(|list| list.input == input) else { return };
        let filter = list.filter.trim().to_string();
        // Matched again only when the filter changes: a list may hold 100,000 channels.
        let shown = match list.shown.take() {
            Some((key, shown)) if key == filter => shown,
            _ => {
                let matches = |text: &str| hyperfetch_core::pattern_matches(&filter, text);
                (0..list.channels.len())
                    .filter(|&at| {
                        let channel = &list.channels[at].0;
                        filter.is_empty() || matches(&channel.name) || channel.group.as_deref().is_some_and(matches)
                    })
                    .collect()
            }
        };
        ui.add_space(4.0);
        card(ui).inner_margin(10.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::singleline(&mut list.filter).hint_text("Filter by name or group (* for any run)").desired_width(280.0);
                ui.add(edit);
                let mut all = !shown.is_empty() && shown.iter().all(|&at| list.channels[at].1);
                if ui.checkbox(&mut all, format!("All {} shown", shown.len())).changed() {
                    for &at in &shown {
                        list.channels[at].1 = all;
                    }
                }
            });
            let row = ui.spacing().interact_size.y;
            let most = 10.0 * (row + ui.spacing().item_spacing.y);
            egui::ScrollArea::vertical().id_salt("channels").max_height(most).auto_shrink([false, true]).show_rows(ui, row, shown.len(), |ui, rows| {
                for &at in &shown[rows] {
                    let (channel, on) = &mut list.channels[at];
                    ui.horizontal(|ui| {
                        ui.checkbox(on, &channel.name);
                        if let Some(group) = &channel.group {
                            ui.label(RichText::new(group).small().color(p.muted));
                        }
                    });
                }
            });
        });
        list.shown = Some((filter, shown));
    });
}

/// A ZIP's entries, a line each with its size, in a list that scrolls past twelve of them: a file
/// with a box to tick, greyed with the reason when it cannot be unpacked on its own. Then how many
/// there are and their size, unpacked and packed, All to tick every file or none, and Download
/// selected (see `App::download_zip_entries`).
fn zip_entries(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let Some((_, Some(Ok(entries)))) = app.zip_preview.as_mut() else { return };
    ui.add_space(4.0);
    let row = ui.spacing().interact_size.y;
    let most = 12.0 * (row + ui.spacing().item_spacing.y);
    let tickable = |entry: &ZipEntry| !entry.is_folder() && entry.unsupported().is_none();
    egui::ScrollArea::vertical().id_salt("zip entries").max_height(most).auto_shrink([false, true]).show_rows(ui, row, entries.len(), |ui, rows| {
        for (entry, on) in &mut entries[rows] {
            ui.horizontal(|ui| {
                left_part(ui, 96.0, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    let name = RichText::new(&entry.name).color(p.text);
                    let shown = match entry.unsupported() {
                        _ if entry.is_folder() => {
                            // In line with the names of the files beside their boxes.
                            ui.add_space(ui.spacing().icon_width + ui.spacing().icon_spacing);
                            ui.label(name.color(p.muted))
                        }
                        Some(why) => ui.add_enabled(false, egui::Checkbox::new(&mut false, name)).on_disabled_hover_text(why),
                        None => ui.checkbox(on, name),
                    };
                    shown.on_hover_text(&entry.name);
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(RichText::new(format_bytes(entry.size)).color(p.muted)));
            });
        }
    });
    let (size, packed) = entries.iter().fold((0u64, 0u64), |(size, packed), (e, _)| (size.saturating_add(e.size), packed.saturating_add(e.compressed)));
    let total = format!("{} entries, {} ({} compressed)", entries.len(), format_bytes(size), format_bytes(packed));
    let (files, ticked) = (entries.iter().filter(|(e, _)| tickable(e)).count(), entries.iter().filter(|(_, on)| *on).count());
    let mut go = false;
    ui.horizontal(|ui| {
        let mut all = files > 0 && ticked == files;
        if ui.add_enabled(files > 0, egui::Checkbox::new(&mut all, "All")).changed() {
            entries.iter_mut().for_each(|(entry, on)| *on = all && tickable(entry));
        }
        ui.label(RichText::new(total).small().color(p.muted));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let download = primary_button(&format!("{} Download selected", icon::DOWNLOAD_SIMPLE), p.accent);
            let hint = "Download only the ticked files, each into its folder under Save to, unpacked and checked against its CRC-32";
            go = ui.add_enabled(ticked > 0, download).on_hover_text(hint).on_disabled_hover_text("Tick the files to download first").clicked();
        });
    });
    if go {
        app.download_zip_entries();
    }
}

/// The form's Download button. For a moment after a download is added (Ctrl+V, the clipboard
/// banner) it turns green and says Added, its check drawing itself.
fn download_button(app: &App, ui: &mut Ui) -> egui::Response {
    let p = palette(ui);
    let added = just_added(app, ui.ctx());
    let green = anim::presence(ui.ctx(), egui::Id::new("download added"), added < 1.0, 0.15);
    let fill = p.accent.lerp_to_gamma(p.green, green);
    if added >= 1.0 {
        return ui.add_sized([110.0, 32.0], primary_button(&format!("{} Download", icon::DOWNLOAD_SIMPLE), fill));
    }
    // Room for the check, which is painted.
    let label = "     Added";
    let response = ui.add_sized([110.0, 32.0], primary_button(label, fill));
    let font = FontId::new(14.0, semibold());
    let width = |text: &str| ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE).size().x;
    let left = response.rect.center().x - width(label) / 2.0;
    let room = width(label) - width("Added");
    let check = Rect::from_center_size(Pos2::new(left + room / 2.0 - 1.0, response.rect.center().y), Vec2::splat(15.0));
    anim::paint_check(ui.painter(), check, anim::ease(added * 3.0), Stroke::new(2.0, Color32::WHITE));
    response
}

pub(super) fn start_from_form(app: &mut App) {
    let (text, checksum, auth) = (app.url_input.clone(), app.checksum_input.clone(), app.auth_input.clone());
    app.download_now(&text, &checksum, &auth, true);
}

/// The inputs that belong to one download and are never saved.
fn advanced_options(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let label = |ui: &mut Ui, text: &str| {
        left_part(ui, ui.available_width() - 110.0, |ui| {
            ui.label(text);
        })
    };
    if kinds::recognise(&app.url_input).is_some_and(|(kind, _)| kind.choice == kinds::Choice::Page) {
        ui.horizontal(|ui| {
            label(ui, "Saved page");
            let tip = "Put the page's scripts in the saved file too, where they run when it is opened. Left out, the file shows the \
                       page as the server sent it and runs nothing.";
            toggle(ui, &mut app.keep_scripts).on_hover_text(tip);
            ui.label(RichText::new("Keep its scripts").color(p.text)).on_hover_text(tip);
        });
    }
    ui.horizontal(|ui| {
        label(ui, "Connections");
        ui.add(egui::Slider::new(&mut app.settings.connections, 1..=64)).on_hover_text("More can be faster; some servers allow only a few");
    });
    ui.horizontal(|ui| {
        label(ui, "Quality");
        combo(ui, "media_preset_more", &mut app.settings.media_preset, &MEDIA_PRESETS);
    });
    let checksum = "Optional, this download only: sha256:..., sha512:..., sha1:..., md5:..., blake3:..., or hex";
    ui.horizontal(|ui| {
        label(ui, "Checksum");
        ui.add(egui::TextEdit::singleline(&mut app.checksum_input).hint_text(checksum).desired_width(f32::INFINITY));
    });
    ui.horizontal(|ui| {
        label(ui, "Older version");
        let hint = "Optional file: what the new one still holds is copied from it, when the server publishes block checksums";
        // Room for Browse… and the field's own margins.
        ui.add(egui::TextEdit::singleline(&mut app.delta_input).hint_text(hint).desired_width(ui.available_width() - 112.0));
        if ui.add_enabled(!app.dialog_open, button("Browse…")).clicked() {
            app.pending_dialog = Some(Dialog::OlderVersion);
        }
    });
    let auth = "Optional header, e.g. Bearer <token>: not saved, never sent with clipboard links";
    ui.horizontal(|ui| {
        label(ui, "Authorization");
        ui.add(egui::TextEdit::singleline(&mut app.auth_input).hint_text(auth).desired_width(f32::INFINITY));
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Cookies, proxy, quality, speed limit and the rest are under").small().color(p.muted));
        if ui.link(RichText::new("Settings").small()).clicked() {
            app.tab = Tab::Settings;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::harness::{Harness, TOGGLE};
    use super::*;

    /// A page with a picture and a script, on this computer, for 20 s at most: every answer of a
    /// fixed length, robots.txt not found.
    fn serve_page() -> std::net::SocketAddr {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while std::time::Instant::now() < until {
                let Ok((mut socket, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                };
                let _ = socket.set_nonblocking(false);
                let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let path = String::from_utf8_lossy(&request).split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, kind, body): (&str, &str, &[u8]) = match path.as_str() {
                    "/about.html" => ("200 OK", "text/html; charset=utf-8", b"<html><head><title>About us</title></head><body><h1>Hi</h1><img src=\"dot.png\"><script>document.title = 'x'</script></body></html>"),
                    "/dot.png" => ("200 OK", "image/png", b"PNG"),
                    _ => ("404 Not Found", "text/plain", b"no"),
                };
                let head = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).and_then(|_| socket.write_all(body));
            }
        });
        addr
    }

    /// A web page's card says the site names it, and offers to save it as one file: that saves it
    /// into the save folder off the UI thread, named after its title, its picture put in and its
    /// script left out; the notice says so and the Library keeps it. Keep its scripts, in More
    /// options, keeps the script in the next one.
    #[test]
    fn a_web_page_is_saved_as_one_file() {
        let addr = serve_page();
        let mut h = Harness::new();
        h.app.url_input = format!("http://{addr}/about.html");
        h.frames(30);
        assert!(h.find("A web page").is_some() && h.find("Its name comes from the site once it starts").is_some(), "{:?}", h.texts());
        assert!(h.find("about.html").is_none(), "no name made up from the link");
        h.click("Save this page as one file");
        assert!(h.app.saving_page.is_some());
        h.wait_for(|app| app.saving_page.is_none());
        assert!(matches!(&h.app.notice, Some(Ok(notice)) if notice.starts_with("Saved the page as one file: About us.html")), "{:?}", h.app.notice);
        let saved = std::path::Path::new(&h.app.settings.save_dir).join("About us.html");
        let html = std::fs::read_to_string(&saved).unwrap();
        assert!(html.contains("data:image/png;base64,") && !html.contains("<script>"), "{html}");
        h.wait_for(|app| app.history.iter().any(|entry| entry.file_path == saved));

        h.app.show_advanced = true;
        h.frames(30);
        h.click_at(h.find_near("Keep its scripts", TOGGLE).unwrap().center());
        assert!(h.app.keep_scripts);
        h.click("Save this page as one file");
        h.wait_for(|app| app.saving_page.is_none());
        let kept = std::fs::read_to_string(std::path::Path::new(&h.app.settings.save_dir).join("About us (1).html")).unwrap();
        assert!(kept.contains("<script>"), "{kept}");
        h.wait_for_background();
    }

    /// A video page's link ends in "watch", a playlist's in "videos": the card names neither,
    /// and says the site will; a file's link names its file, a magnet its own name.
    #[test]
    fn the_card_names_only_what_the_link_names() {
        let named = |text: &str| shown_name(text, kinds::recognise(text).unwrap().0);
        assert_eq!(named("https://www.youtube.com/watch?v=aqz-KE-bpKQ"), None);
        assert_eq!(named("https://www.youtube.com/@BlenderOfficial/videos"), None);
        assert_eq!(named("https://arxiv.org/abs/1706.03762"), None);
        assert_eq!(named("https://ftp.gnu.org/gnu/hello/hello-2.12.1.tar.gz").as_deref(), Some("hello-2.12.1.tar.gz"));
        assert_eq!(named("magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny").as_deref(), Some("Big Buck Bunny"));
        let mut h = Harness::new();
        h.app.url_input = "https://www.youtube.com/watch?v=aqz-KE-bpKQ".to_string();
        h.frames(30);
        assert!(h.find("Its title comes from the site once it starts").is_some() && h.find("watch").is_none(), "{:?}", h.texts());
    }

    /// A download added with the Downloads page shown turns its Download button into Added for a
    /// moment, after which frames stop; one added on another page is old news by the time the
    /// Downloads page shows.
    #[test]
    fn the_download_button_says_added_just_after_an_add() {
        let mut h = Harness::new();
        h.frames(30);
        assert_eq!(just_added(&h.app, &h.app.ctx), 1.0, "nothing was added");

        h.paste("https://example.com/a.iso");
        h.frames(1);
        assert_eq!(h.app.queue.items().len(), 1);
        assert!(just_added(&h.app, &h.app.ctx) < 1.0, "added just now");
        assert_eq!(h.frames(90), Duration::MAX, "and then idle");
        assert_eq!(just_added(&h.app, &h.app.ctx), 1.0);

        h.app.tab = Tab::Library;
        h.paste("https://example.com/b.iso");
        h.frames(90);
        h.app.tab = Tab::Downloads;
        h.frames(1);
        assert_eq!(just_added(&h.app, &h.app.ctx), 1.0, "added a while ago");

        // A removal is not an addition.
        let first = h.app.queue.items()[0].id;
        h.app.queue.remove_item(first);
        h.frames(1);
        assert_eq!(just_added(&h.app, &h.app.ctx), 1.0);
    }

    /// A link pasted into the form with its password starts with it: one download, a note naming
    /// what it uses, the form cleared, the password shown nowhere. Ctrl+V outside the form queues
    /// a copied link with its password the same way.
    #[test]
    fn a_pasted_password_is_used_and_never_shown() {
        const SECRET: &str = "xq9-hunter2";
        let mut h = Harness::new();
        h.app.engines = std::sync::Arc::new(|_, _| Err("not downloaded here".to_string()));
        assert!(h.settle());
        let input = h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect;
        h.click_at(input.center());
        h.paste(&format!("https://h.example/f.zip\nPassword: {SECRET}"));
        assert!(h.app.url_input.contains(SECRET), "typed into the form");
        h.click("Download");
        h.frames(2);
        let items = h.app.queue.items();
        assert_eq!((items.len(), items[0].options.password.as_deref()), (1, Some(SECRET)));
        assert_eq!(h.app.url_input, "", "the form clears");
        h.settle();
        assert!(h.texts().iter().any(|(text, _)| text.contains("Using the password from your paste for h.example")), "{:?}", h.app.notice);
        assert!(h.texts().iter().all(|(text, _)| !text.contains(SECRET)));

        h.click("New download");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        h.paste(&format!("https://g.example/g.zip pw: {SECRET}"));
        let last = h.app.queue.items().last().unwrap();
        assert_eq!((last.urls[0].as_str(), last.options.password.as_deref()), ("https://g.example/g.zip", Some(SECRET)));
        h.settle();
        assert!(h.texts().iter().any(|(text, _)| text.contains("Using the password from your paste for g.example")), "{:?}", h.app.notice);
        assert!(h.texts().iter().all(|(text, _)| !text.contains(SECRET)));
    }

    /// A ZIP of `files` (name, size) as `zipdir::list_zip` reads one: only its central directory
    /// and end record are real, and each entry claims half its size packed (no file data is sent).
    fn zip_of(files: &[(&str, u32)]) -> Vec<u8> {
        let mut out = b"data".to_vec();
        let offset = out.len() as u32;
        for (name, size) in files {
            out.extend(0x0201_4b50u32.to_le_bytes());
            out.extend([0u8; 16]);
            out.extend([size / 2, *size].iter().flat_map(|v| v.to_le_bytes()));
            out.extend((name.len() as u16).to_le_bytes());
            out.extend([0u8; 16]);
            out.extend(name.as_bytes());
        }
        let directory = out.len() as u32 - offset;
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0u16, 0, files.len() as u16, files.len() as u16].iter().flat_map(|v| v.to_le_bytes()));
        out.extend([directory, offset].iter().flat_map(|v| v.to_le_bytes()));
        out.extend(0u16.to_le_bytes());
        out
    }

    /// One .zip link typed in the box offers See what's inside, which lists the ZIP (from a local
    /// server) with each file's size and the total; a file that is no ZIP says so. Two links, or
    /// a link to another file, offer nothing.
    #[test]
    fn see_whats_inside_a_zip_link() {
        let server = tokio::runtime::Runtime::new().unwrap();
        let zip = zip_of(&[("docs/", 0), ("docs/readme.txt", 2048), ("setup.exe", 3 << 20)]);
        let addr = server.block_on(crate::tests::serve(std::sync::Arc::new(zip)));
        let other = server.block_on(crate::tests::serve(std::sync::Arc::new(vec![0; 1000])));
        let link = format!("http://{addr}/pack.ZIP");
        assert_eq!(zip_link(&format!(" {link} ")).map(String::from), Some(link.clone()));
        for no in [format!("{link} {link}"), format!("http://{addr}/a.iso"), "ftp://h.example/a.zip".to_string()] {
            assert!(zip_link(&no).is_none(), "{no}");
        }

        let mut h = Harness::new();
        assert!(h.settle());
        assert!(h.find("See what's inside").is_none());
        let input = h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect;
        h.click_at(input.center());
        h.type_text(&link);
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle());
        h.click("See what's inside");
        h.wait_for(|app| app.zip_preview.as_ref().is_some_and(|(_, listed)| listed.is_some()));
        assert!(h.settle());
        for shown in ["docs/", "docs/readme.txt", "2.00 KiB", "setup.exe", "3.00 MiB", "3 entries, 3.00 MiB (1.50 MiB compressed)"] {
            assert!(h.find(shown).is_some(), "{shown} in {:?}", h.texts());
        }
        assert!(h.find("See what's inside").is_none());

        h.click_at(input.center());
        h.key(egui::Key::A, egui::Modifiers::COMMAND);
        h.type_text(&format!("http://{other}/fake.zip"));
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle() && h.find("setup.exe").is_none(), "another link's list goes");
        h.click("See what's inside");
        h.wait_for(|app| app.zip_preview.as_ref().is_some_and(|(_, listed)| listed.is_some()));
        assert!(h.settle());
        assert!(h.texts().iter().any(|(text, _)| text.starts_with("The file is not a ZIP")), "{:?}", h.texts());
    }

    /// The files of a listed ZIP have boxes to tick (not its folders, nor an encrypted file), All
    /// ticks them all or none, and Download selected downloads only those ticked, each into its
    /// folder under the save folder: the first starts, the others wait.
    /// A link whose earlier download's file is still there offers Update from the copy on disk,
    /// which replaces that file, only by a newer one; the older version under Advanced options
    /// is the download's `delta_from`, and one that is not a file is refused.
    #[test]
    fn a_copy_on_disk_is_updated_and_an_older_version_reused() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("nightly.iso");
        std::fs::write(&copy, b"old").unwrap();
        let link = "https://example.com/nightly.iso";
        let mut h = Harness::new();
        let mut entry = HistoryEntry::new("nightly.iso".into(), copy.clone(), 3, vec![link.to_string()]);
        entry.status = HistoryStatus::Completed;
        h.app.history = vec![entry];
        assert!(h.settle());
        let input = h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect;
        h.click_at(input.center());
        h.type_text(link);
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle());
        h.click("Update from the copy on disk");
        let item = &h.app.queue.items()[0];
        assert!(item.options.only_if_newer && item.options.output_path.as_deref() == Some(copy.as_path()));

        h.app.new_download();
        h.app.url_input = "https://example.com/b.iso".into();
        h.app.delta_input = dir.path().join("missing.iso").to_string_lossy().into_owned();
        start_from_form(&mut h.app);
        assert!(h.app.form_error.as_deref().is_some_and(|e| e.contains("not a file")), "{:?}", h.app.form_error);
        h.app.delta_input = copy.to_string_lossy().into_owned();
        start_from_form(&mut h.app);
        let item = h.app.queue.items().iter().find(|item| item.filename == "b.iso").unwrap();
        assert_eq!((item.options.delta_from.as_deref(), item.options.only_if_newer), (Some(copy.as_path()), false));
        assert!(h.app.delta_input.is_empty(), "the form clears as it starts");
    }

    /// A link with ranges in the link bar says how many links it stands for, and Download queues
    /// each; so do the queue input and a paste. A range past the ceiling, one that counts down, or
    /// one in a link read first says so, and Take as one link takes it as written.
    #[test]
    fn a_link_with_ranges_queues_each_of_its_links() {
        let mut h = Harness::new();
        h.app.settings.auto_run_queue = false;
        h.app.url_input = "https://h.example/img[001-120].jpg".into();
        h.frames(30);
        assert!(h.find("Several links").is_some() && h.find("120 links, img001.jpg to img120.jpg").is_some());
        h.click("Download");
        let names = |h: &Harness| h.app.queue.items().iter().map(|item| item.filename.clone()).collect::<Vec<_>>();
        let queued = names(&h);
        assert_eq!((queued.len(), queued[0].as_str(), queued[9].as_str(), queued[119].as_str()), (120, "img001.jpg", "img010.jpg", "img120.jpg"));
        assert!(h.app.url_input.is_empty());

        h.app.url_input = "https://h.example/[1-20000].jpg".into();
        start_from_form(&mut h.app);
        assert!(h.app.form_error.as_deref().is_some_and(|e| e.contains("more than 10000 links")), "{:?}", h.app.form_error);
        h.app.url_input.clear();
        h.frames(5);

        h.app.queue_input = "https://h.example/[a-c].bin\nhttps://h.example/[3-1].bin".into();
        h.app.add_queue_input();
        assert_eq!(names(&h)[120..], ["a.bin", "b.bin", "c.bin"]);
        assert_eq!(h.app.queue_input, "https://h.example/[3-1].bin", "kept for correcting");
        assert!(h.app.queue_error.as_deref().is_some_and(|e| e.starts_with("Line 2: The range [3-1] counts down")), "{:?}", h.app.queue_error);

        h.paste("https://p.example/[1-2].iso");
        assert_eq!(names(&h)[123..], ["1.iso", "2.iso"]);

        // A link read first takes no range; brackets of a file's name are one click from taken
        // as written.
        h.app.url_input = "https://h.example/[1-3].torrent".into();
        start_from_form(&mut h.app);
        assert!(h.app.form_error.as_deref().is_some_and(|e| e.contains("read first")), "{:?}", h.app.form_error);
        h.app.form_error = None;
        h.app.url_input = "https://h.example/Hits%20[1990-1999].zip".into();
        h.frames(30);
        h.click("Take as one link");
        assert_eq!(h.app.url_input, "https://h.example/Hits%20%5B1990-1999%5D.zip");
        h.frames(5);
        assert!(h.find("Several links").is_none() && h.find("Take as one link").is_none());
    }

    /// A folder saved to before is one click away beside Save to: picked, downloads go there, and
    /// the folder it replaces becomes the recent one.
    #[test]
    fn a_recent_folder_is_one_click_away() {
        let mut h = Harness::new();
        let first = h.app.settings.save_dir.clone();
        let other = std::env::temp_dir().join("elsewhere").to_string_lossy().into_owned();
        h.app.settings.set_save_dir(other.clone());
        h.app.url_input = "https://h.example/a.iso".into();
        h.frames(30);
        h.click(icon::CLOCK_COUNTER_CLOCKWISE);
        h.frames(5);
        h.click(&first);
        assert_eq!((h.app.settings.save_dir.clone(), h.app.settings.recent_dirs.clone()), (first, vec![other]));
    }

    #[test]
    fn download_files_ticked_in_a_zip() {
        let crc = |data: &[u8]| !data.iter().fold(!0u32, |c, &b| (0..8).fold(c ^ u32::from(b), |c, _| if c & 1 == 1 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 }));
        let le16 = |values: &[u16]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let le32 = |values: &[u32]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let files: [(&str, &[u8], u16); 4] = [("docs/", b"", 0), ("docs/readme.txt", b"read me", 0), ("tools/run.sh", b"echo hi", 0), ("secret.txt", b"shh", 1)];
        let (mut zip, mut directory) = (Vec::new(), Vec::new());
        for (name, data, flags) in files {
            let (at, sums) = (zip.len() as u32, le32(&[crc(data), data.len() as u32, data.len() as u32]));
            zip.extend([le32(&[0x0403_4b50]), le16(&[20, flags, 0, 0, 0]), sums.clone(), le16(&[name.len() as u16, 0]), name.as_bytes().to_vec(), data.to_vec()].concat());
            directory.extend([le32(&[0x0201_4b50]), le16(&[20, 20, flags, 0, 0, 0]), sums, le16(&[name.len() as u16, 0, 0, 0, 0]), le32(&[0, at])].concat());
            directory.extend(name.as_bytes());
        }
        let (at, size) = (zip.len() as u32, directory.len() as u32);
        zip.extend([directory, le32(&[0x0605_4b50]), le16(&[0, 0, 4, 4]), le32(&[size, at]), le16(&[0])].concat());
        let server = tokio::runtime::Runtime::new().unwrap();
        let addr = server.block_on(crate::tests::serve(std::sync::Arc::new(zip)));

        let mut h = Harness::new();
        assert!(h.settle());
        let input = h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect;
        h.click_at(input.center());
        h.type_text(&format!("http://{addr}/pack.zip"));
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle());
        h.click("See what's inside");
        h.wait_for(|app| app.zip_preview.as_ref().is_some_and(|(_, listed)| listed.is_some()));
        assert!(h.settle());
        let ticked = |h: &Harness| match &h.app.zip_preview {
            Some((_, Some(Ok(entries)))) => entries.iter().filter(|(_, on)| *on).map(|(e, _)| e.name.clone()).collect::<Vec<_>>(),
            _ => panic!("no listing"),
        };
        let all = h.find_near("Download selected", "All").unwrap().center();
        h.click_at(all);
        assert!(h.settle());
        assert_eq!(ticked(&h), ["docs/readme.txt", "tools/run.sh"], "neither the folder nor the encrypted file");
        h.click_at(all);
        assert!(h.settle() && ticked(&h).is_empty());
        h.click("Download selected");
        assert!(h.settle() && h.app.queue.items().is_empty(), "nothing is ticked");
        h.click("docs/readme.txt");
        h.click("tools/run.sh");
        assert!(h.settle());
        h.click("Download selected");
        let items: Vec<(String, Option<String>)> =
            h.app.queue.items().iter().map(|item| (item.filename.clone(), item.options.zip_entry.as_ref().map(|e| e.name.clone()))).collect();
        assert_eq!(items, [("readme.txt".to_string(), Some("docs/readme.txt".to_string())), ("run.sh".to_string(), Some("tools/run.sh".to_string()))]);
        assert!(h.app.zip_preview.is_none());
        let first = h.app.queue.items()[0].id;
        h.wait_for(|app| app.queue.get_item(first).is_some_and(|item| item.status == QueueItemStatus::Completed));
        let saved = std::path::PathBuf::from(&h.app.settings.save_dir);
        assert_eq!(std::fs::read(saved.join("docs").join("readme.txt")).unwrap(), b"read me");
        assert!(!saved.join("pack.zip").exists());
    }

    /// A dropped channel list shows its channels and groups to tick, which a filter narrows and
    /// All shown ticks; each ticked channel becomes a download named after it that records it as
    /// long as Settings say. Nothing can be added before a channel is ticked.
    #[test]
    fn a_dropped_channel_list_offers_its_channels_to_tick() {
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("tv.m3u");
        let entries = [("News", "News One", "news1.m3u8"), ("News", "News Two", "news2.m3u8"), ("Sport", "Sport HD", "sport.ts")];
        let text: String = entries.iter().map(|(group, name, file)| format!("#EXTINF:-1 group-title=\"{group}\",{name}\nhttp://tv.example/{file}\n")).collect();
        std::fs::write(&list, format!("#EXTM3U\n{text}")).unwrap();
        let mut h = Harness::new();
        (h.app.settings.auto_run_queue, h.app.settings.record_minutes) = (false, 60);
        h.app.add_dropped(list.clone());
        h.wait_for(|app| !app.channel_lists.is_empty());
        assert!(h.settle());
        assert!(h.texts().iter().any(|(text, _)| text.ends_with("is a list of 3 channels. Tick the ones to record:")), "{:?}", h.texts());
        for shown in ["News One", "News Two", "Sport HD", "News", "Sport"] {
            assert!(h.find(shown).is_some(), "{shown} in {:?}", h.texts());
        }
        h.click("Add 0 channels");
        assert_eq!(h.app.channel_lists.len(), 1, "nothing ticked, nothing to add");

        h.click("Filter by name or group (* for any run)");
        h.type_text("news");
        h.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(h.settle() && h.find("Sport HD").is_none());
        h.click("All 2 shown");
        assert!(h.settle());
        h.click("Add 2 channels");
        assert!(h.app.channel_lists.is_empty());
        let added: Vec<_> = h.app.queue.items().iter().map(|item| (item.filename.as_str(), item.options.channel.as_deref(), item.options.record_seconds)).collect();
        assert_eq!(added, [("News One.ts", Some("News One"), Some(3600)), ("News Two.ts", Some("News Two"), Some(3600))]);
    }

    /// While a ZIP's files or a stream's tracks are read, grey bars shaped like the lines to come
    /// show under the spinner, a light sweeping over them; once read they go and frames stop.
    /// Under Reduce motion they keep still and ask for no frames.
    #[test]
    fn a_link_being_read_shows_shimmering_rows() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            for (link, reading) in [("https://example.com/pack.zip", "Reading what the ZIP holds…"), ("https://example.com/v/master.m3u8", "Reading the stream's tracks…")] {
                h.app.url_input = link.to_string();
                if link.ends_with(".zip") {
                    h.app.zip_preview = Some((link.to_string(), None));
                } else {
                    h.app.tracks = Some(crate::tracks::Chooser { item: None, link: link.to_string(), info: None, picks: Default::default() });
                }
                h.frames(60);
                let label = h.find(reading).unwrap();
                let rows = Some(Rect::from_x_y_ranges(212.0..=980.0, label.bottom() + 4.0..=label.bottom() + 70.0));
                // Shots a tenth of a second apart, over half the light's sweep.
                let shots: Vec<_> = (0..9).map(|_| (h.frames(6), h.shot(rows))).collect();
                let bar = [DARK.hover.r(), DARK.hover.g(), DARK.hover.b()];
                let bars = shots[0].1.pixels().filter(|px| px.0[..3] == bar).count();
                assert!(bars > 500, "bars under {reading}: {bars}");
                let moved = shots.windows(2).any(|w| w[0].1 != w[1].1);
                if reduce {
                    assert!(shots.iter().all(|(delay, _)| *delay == Duration::MAX) && !moved, "still under {reading}");
                } else {
                    assert!(shots.iter().all(|(delay, _)| *delay <= Duration::from_millis(16)) && moved, "the light sweeps under {reading}");
                }
                h.app.zip_preview = Some((link.to_string(), Some(Err("The file is not a ZIP".to_string()))));
                h.app.tracks = None;
                assert!(h.settle() && h.find(reading).is_none(), "read: the bars go");
            }
        }
    }

    /// A notice opens its room over a few frames, pushing the card under it down smoothly; once
    /// dismissed it fades out from what it last said and closes the room, then is forgotten and
    /// frames stop.
    #[test]
    fn a_notice_opens_and_closes_its_room_smoothly() {
        let mut h = Harness::new();
        let ghost = |h: &Harness| h.app.ctx.data(|d| d.get_temp::<Result<String, String>>(egui::Id::new(("banner ghost", "notice"))));
        assert!(h.settle());
        // The link bar, under the banners.
        let card = |h: &Harness| h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect.top();
        let top = card(&h);
        let mut tops = Vec::new();
        h.app.notice = Some(Ok("Added #1 to the queue".to_string()));
        for _ in 0..20 {
            h.frame(Vec::new());
            tops.push(card(&h));
        }
        assert_eq!(h.frames(30), Duration::MAX);
        let room = card(&h) - top;
        assert!(room > 30.0, "the notice takes room: {room}");
        let steps: Vec<f32> = std::iter::once(top).chain(tops).collect::<Vec<_>>().windows(2).map(|w| w[1] - w[0]).collect();
        assert!(steps.iter().all(|&step| (0.0..room / 2.0).contains(&step)), "no jump: {steps:?}");

        h.app.notice = None;
        assert!(h.frames(2) < Duration::MAX, "fading out");
        assert_eq!(ghost(&h), Some(Ok("Added #1 to the queue".to_string())));
        assert!(card(&h) > top, "closing");
        assert_eq!(h.frames(30), Duration::MAX, "gone");
        assert_eq!(ghost(&h), None);
        assert_eq!(card(&h), top);
    }
    /// A new user's Downloads page starts with the three steps, until Got it.
    #[test]
    fn the_welcome_shows_until_it_is_closed() {
        let mut h = Harness::new();
        assert!(h.settle() && h.find("Welcome").is_none());
        h.app.settings.welcomed = false;
        h.frames(40);
        assert!(h.find("Welcome").is_some() && h.find("Try one of these").is_some(), "{:?}", h.texts());
        h.click("Got it");
        h.frames(5);
        assert!(h.app.settings.welcomed && h.find("Welcome").is_none());
    }
}
