use super::*;

use super::queue::download_details;

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
    for link in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if ingest::needs_reading(link) {
            app.read_document(link.to_string(), Origin::Dropped, String::new(), String::new());
        } else {
            app.notice = Some(app.add_download(link, "", "").map(|id| format!("Added #{} to the queue", id)));
        }
    }
}

/// Shows the banner `show` makes of `now`. As `now` comes, its room opens over 0.2 s, pushing
/// what is under it down smoothly; once it is gone (dismissed, answered, over), what it last
/// showed fades out while its room closes up, its buttons no longer working.
fn fading<T: Clone + Send + Sync + 'static>(ui: &mut Ui, name: &str, now: Option<T>, show: impl FnOnce(&mut Ui, T)) {
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
                app.tab = Tab::Downloader;
            }
            if ui.add(button("Add to queue")).clicked() {
                app.clipboard_banner = None;
                add_link(app, &link);
            }
            if ui.add(primary_button("Download now", p.accent)).clicked() {
                app.clipboard_banner = None;
                app.download_now(&link, "", "");
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
fn nudged(ui: &mut Ui, dx: f32, add: impl FnOnce(&mut Ui)) {
    let offset = Vec2::new(dx, 0.0);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(ui.available_rect_before_wrap().translate(offset)).layout(*ui.layout()));
    add(&mut child);
    ui.advance_cursor_after_rect(child.min_rect().translate(-offset));
}

pub(super) fn add_page(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let focused = app.focused_item().cloned();
    let active = focused.as_ref().is_some_and(|item| item.status.is_active());

    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        match &focused {
            Some(item) => {
                ui.horizontal(|ui| {
                    let (status, color) = status_badge(app, item, &p);
                    let done = item.status == QueueItemStatus::Completed;
                    // Kept current while shown, so that the check draws itself as the download completes.
                    let drawn = anim::ease(anim::changed(ui.ctx(), egui::Id::new(("completed check", item.id)), done, 0.5));
                    status_chip(ui, egui::Id::new(("item", item.id)), status, color, item.status == QueueItemStatus::Downloading);
                    if done {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                        ui.painter().circle_filled(rect.center(), 9.0 * anim::overshoot(drawn), p.green.gamma_multiply(0.2));
                        anim::paint_check(ui.painter(), rect.shrink(3.0), drawn, Stroke::new(2.0, p.green));
                    }
                    ui.add(egui::Label::new(bold(&item.filename).size(16.0).color(p.strong)).truncate()).on_hover_text(&item.filename);
                });
                ui.add_space(4.0);
                // The shown download keeps its own links; New download frees the box.
                let urls = item.urls.iter().map(|u| u.as_str()).collect::<Vec<_>>().join(" ");
                ui.add(egui::TextEdit::singleline(&mut urls.as_str()).desired_width(f32::INFINITY));
            }
            None => {
                ui.horizontal(|ui| {
                    let edit = egui::TextEdit::singleline(&mut app.url_input)
                        .id(egui::Id::new("url_input"))
                        .hint_text("File link, video page (YouTube, Vimeo, Reddit, ...), playlist or channel, magnet, .torrent or .metalink")
                        .margin(Vec2::new(10.0, 7.0))
                        .desired_width(ui.available_width() - 212.0);
                    let response = ui.add(edit);
                    if response.changed() {
                        app.form_error = None;
                    }
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        start_from_form(app);
                    }
                    if ui.add_sized([86.0, 32.0], button(format!("{} Paste", icon::CLIPBOARD_TEXT))).clicked() {
                        app.paste_url();
                    }
                    if download_button(app, ui).clicked() {
                        start_from_form(app);
                    }
                });
            }
        }

        if focused.is_none() && ingest::is_blob_url(&app.url_input) {
            error_alert(ui, ingest::BLOB_MESSAGE);
        }
        // A new error shakes its alert, as does the shown download failing; both open and close
        // their room smoothly. The shake follows every change, but an alert fading out stays still.
        let dx = anim::shake(ui.ctx(), egui::Id::new("form error"), &app.form_error);
        let dx = if app.form_error.is_some() { dx } else { 0.0 };
        fading(ui, "form error", app.form_error.clone(), |ui, error| nudged(ui, dx, |ui| error_alert(ui, &error)));
        if let Some(item) = &focused {
            let error = match &item.status {
                QueueItemStatus::Failed(error) => Some(format!("Download failed: {}", error)),
                _ => None,
            };
            let dx = anim::shake(ui.ctx(), egui::Id::new(("failed", item.id)), error.is_some());
            let dx = if error.is_some() { dx } else { 0.0 };
            fading(ui, &format!("failed {}", item.id), error, |ui, error| nudged(ui, dx, |ui| error_alert(ui, &error)));
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::FOLDER_OPEN).size(16.0).color(p.muted)).on_hover_text("Save to");
            ui.add(egui::TextEdit::singleline(&mut app.settings.save_dir).desired_width(ui.available_width() - 96.0));
            if ui.add_enabled(!app.dialog_open, button("Browse…")).clicked() {
                app.pending_dialog = Some(Dialog::SaveDir);
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::LIGHTNING).size(16.0).color(p.muted)).on_hover_text("Connections");
            ui.add_enabled(!active, egui::Slider::new(&mut app.settings.connections, 1..=64).text("connections"))
                .on_disabled_hover_text("A running download keeps the connection count it started with");
            ui.add_space(12.0);
            ui.label(RichText::new(icon::FILM_STRIP).size(16.0).color(p.muted)).on_hover_text("Media quality");
            combo(ui, "media_preset_combo", &mut app.settings.media_preset, &MEDIA_PRESETS);
        });
        if let Some(item) = &focused {
            ui.add_space(8.0);
            ui.horizontal(|ui| download_actions(app, ui, item));
        }

        ui.add_space(4.0);
        // The caret turns down and the options drop in, their room opening (and closing) smoothly.
        let open = anim::presence(ui.ctx(), egui::Id::new("advanced options"), app.show_advanced, 0.18);
        let clicked = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let caret = anim::icon(ui, icon::CARET_RIGHT, 14.0, p.muted, 1.0, open * std::f32::consts::FRAC_PI_2);
                let label = RichText::new("Advanced options: checksum and Authorization header").color(p.muted);
                let button = ui.add(egui::Button::new(label).frame(false));
                button.clicked() || ui.interact(caret.rect, egui::Id::new("advanced caret"), Sense::click()).clicked()
            })
            .inner;
        if clicked {
            app.show_advanced = !app.show_advanced;
        }
        let shown = app.show_advanced.then_some(());
        fading(ui, "advanced options", shown, |ui, ()| anim::shifted(ui, open, anim::DROP, |ui| advanced_options(app, ui)));
    });

    ui.add_space(12.0);
    match &focused {
        Some(item) => download_details(app, ui, item),
        None => {
            card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(bold("Tips").color(p.strong));
                // The first time they show, the tips come in one after another.
                for (n, tip) in [
                    "Ctrl+V outside a text box adds the copied link to the queue.",
                    "Drop a .torrent, .metalink or .meta4 file on the window to add every file it lists.",
                    "Links to the same file from several servers, separated by spaces, download from all of them at once.",
                    "The browser extension sends the videos and links of the pages you visit.",
                ]
                .into_iter()
                .enumerate()
                {
                    anim::fade_in(ui, egui::Id::new(("tip", n)), anim::stagger(n + 1), anim::RISE, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icon::INFO).color(p.accent));
                            ui.label(RichText::new(keys(tip)).color(p.muted));
                        });
                    });
                }
            });
        }
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

fn start_from_form(app: &mut App) {
    let (text, checksum, auth) = (app.url_input.clone(), app.checksum_input.clone(), app.auth_input.clone());
    app.download_now(&text, &checksum, &auth);
}

/// The buttons for the shown download, left to right, and New download on the right.
fn download_actions(app: &mut App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    let id = item.id;
    match &item.status {
        QueueItemStatus::Queued => {
            if ui.add(primary_button(&format!("{} Start now", icon::PLAY), p.accent)).clicked() {
                app.start_job(id, false);
            }
        }
        QueueItemStatus::Downloading => {
            if ui.add(button(RichText::new(format!("{} Pause", icon::PAUSE)).color(p.red))).clicked() {
                app.pause_job(id);
            }
        }
        QueueItemStatus::Pausing => {
            anim::spinner(ui, 16.0, p.muted);
            let text = if app.is_recording(id) { "Stopping: finishing the recording..." } else { "Pausing: saving the resume state..." };
            ui.label(RichText::new(text).color(p.muted));
        }
        QueueItemStatus::Paused | QueueItemStatus::Failed(_) => {
            let resume = if item.status == QueueItemStatus::Paused { "Resume" } else { "Retry" };
            let go_on = primary_button(&format!("{} {}", icon::PLAY, resume), p.green);
            if ui.add(go_on).on_hover_text("Continue from the saved state").clicked() {
                app.start_job(id, false);
            }
            let start_over = ui.add(button(format!("{} Start over", icon::ARROW_COUNTER_CLOCKWISE)));
            if start_over.on_hover_text("Delete the partial file and download from the beginning").clicked() {
                app.start_job(id, true);
            }
            let delete = button(RichText::new(format!("{} Delete leftovers", icon::TRASH)).color(p.red));
            if ui.add(delete).on_hover_text("Remove this download and delete its partial file and resume state").clicked() {
                app.discard_job(id);
            }
        }
        QueueItemStatus::AuthRequired => {
            let ready = !app.auth_input.trim().is_empty();
            let resume = ui
                .add_enabled_ui(ready, |ui| ui.add(primary_button(&format!("{} Resume", icon::PLAY), p.green)))
                .inner
                .on_hover_text("Continue from the saved state with the Authorization header entered under Advanced options")
                .on_disabled_hover_text("Enter the Authorization header under Advanced options first");
            if resume.clicked() {
                app.resume_with_auth(id);
            }
        }
        QueueItemStatus::Completed => {
            if let Some(path) = &item.target_path {
                if ui.add(primary_button(&format!("{} Open file", icon::FILE), p.accent)).clicked() {
                    report(app, util::open_path(path));
                }
                if ui.add(button(format!("{} Show in folder", icon::FOLDER_OPEN))).clicked() {
                    report(app, util::reveal_in_folder(path));
                }
                let idle = !app.verifying && app.repair.is_none();
                if ui.add_enabled(idle, button(format!("{} Verify", icon::SHIELD_CHECK))).clicked() {
                    app.verify(VerifyRequest {
                        path: path.clone(),
                        expected_size: (item.total_bytes > 0).then_some(item.total_bytes),
                        checksum: item.options.expected_checksum.clone(),
                    });
                }
            }
        }
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let new = ui.add(button(format!("{} New download", icon::PLUS)));
        if new.on_hover_text("Keep this download in the queue and start another").clicked() {
            app.new_download();
        }
    });
}

/// The inputs that belong to one download and are never saved.
fn advanced_options(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    for (label, value, hint) in [
        ("Checksum", &mut app.checksum_input, "Optional, this download only: sha256:..., sha512:..., sha1:..., md5:..., blake3:..., or hex"),
        ("Authorization", &mut app.auth_input, "Optional header, e.g. Bearer <token>: not saved, never sent with clipboard links"),
    ] {
        ui.horizontal(|ui| {
            left_part(ui, ui.available_width() - 110.0, |ui| {
                ui.label(label);
            });
            ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(f32::INFINITY));
        });
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new("Cookies, proxy, quality, speed limit and the rest are under").small().color(p.muted));
        if ui.link(RichText::new("Settings").small()).clicked() {
            app.tab = Tab::Settings;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::harness::Harness;
    use super::*;

    /// A download added with the Add page shown turns its Download button into Added for a
    /// moment, after which frames stop; one added on another page is old news by the time the
    /// Add page shows.
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

        h.app.tab = Tab::Queue;
        h.paste("https://example.com/b.iso");
        h.frames(90);
        h.app.tab = Tab::Downloader;
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

    /// A notice opens its room over a few frames, pushing the card under it down smoothly; once
    /// dismissed it fades out from what it last said and closes the room, then is forgotten and
    /// frames stop.
    #[test]
    fn a_notice_opens_and_closes_its_room_smoothly() {
        let mut h = Harness::new();
        let ghost = |h: &Harness| h.app.ctx.data(|d| d.get_temp::<Result<String, String>>(egui::Id::new(("banner ghost", "notice"))));
        assert!(h.settle());
        let card = |h: &Harness| h.find("Tips").unwrap().top();
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
}
