//! The command palette (Ctrl+K, or the search button in the pulse bar): one box that finds an
//! action, a page, a download, something in the Library, a kind of source or a setting, and does
//! it on Enter. Up and Down choose, Escape or a click outside closes.

use super::*;

/// What the palette keeps between frames.
#[derive(Clone, Default)]
struct Finder {
    open: bool,
    query: String,
    /// The result Enter runs, by its place in the list.
    chosen: usize,
    /// The frame it opened on: the box takes the focus then.
    focus: bool,
    /// How many times it has opened, so that each opening drops in.
    session: u64,
}

fn finder_id() -> egui::Id {
    egui::Id::new("command palette")
}

/// Opens the palette, empty.
pub(super) fn open(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        let session = d.get_temp::<Finder>(finder_id()).map_or(0, |finder| finder.session) + 1;
        d.insert_temp(finder_id(), Finder { open: true, focus: true, session, ..Default::default() });
    });
}

pub(super) fn is_open(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<Finder>(finder_id())).is_some_and(|finder| finder.open)
}

/// What a result does.
#[derive(Clone)]
enum Hit {
    Go(Tab),
    NewDownload,
    PauseAll,
    ResumeAll,
    ClearCompleted,
    CheckFile,
    Shortcuts,
    Theme,
    SpeedLimit,
    Clipboard,
    Show(usize),
    Find(String),
    Link(String),
    /// Save the web page at this link as one file.
    SavePage(String),
    Settings(String),
}

/// The results for `query` (lower case): each one's icon, text, what it adds in grey, and what it
/// does.
fn results(app: &App, query: &str) -> Vec<(&'static str, String, String, Hit)> {
    let mut out = Vec::new();
    let raw = query.trim();
    let query = raw.to_lowercase();
    let hit = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);
    // Something to download, typed or pasted straight in.
    if let Some((kind, source)) = kinds::recognise(raw).filter(|_| raw.contains(':') || raw.contains('/')) {
        let about = if source.is_empty() { kind.name.to_string() } else { format!("{}, from {source}", kind.name) };
        out.push((kind.glyph, "Put it in the link bar".to_string(), about, Hit::Link(raw.to_string())));
        if kind.choice == kinds::Choice::Page {
            out.push((icon::FILE_HTML, "Save this page as one file".to_string(), "Opens offline".to_string(), Hit::SavePage(raw.to_string())));
        }
    }
    // The page in the link bar, as one file.
    let bar = app.url_input.trim();
    if kinds::recognise(bar).is_some_and(|(kind, _)| kind.choice == kinds::Choice::Page) && !raw.contains('/') && (hit("Save this page as one file") || hit("offline")) {
        out.push((icon::FILE_HTML, "Save this page as one file".to_string(), truncate_chars(bar, 48), Hit::SavePage(bar.to_string())));
    }
    let running = app.queue.items().iter().any(|item| item.status == QueueItemStatus::Downloading);
    let limit = crate::settings::speed_limit_bytes(app.settings.max_speed, app.settings.max_speed_in_mb).is_some();
    let actions = [
        (true, icon::PLUS, "New download", "Paste or drop anything", Hit::NewDownload),
        (running, icon::PAUSE, "Pause all", "Every running download", Hit::PauseAll),
        (!running, icon::PLAY, "Resume all", "Start the queue", Hit::ResumeAll),
        (true, icon::BROOM, "Clear completed", "Off the list; the files are kept", Hit::ClearCompleted),
        (true, icon::SHIELD_CHECK, "Check a file…", "Against what its download recorded", Hit::CheckFile),
        (true, icon::CIRCLE_HALF, "Switch the theme", "Light, dark or as the system", Hit::Theme),
        (limit, icon::GAUGE, "Speed limit on or off", "For new and resumed downloads", Hit::SpeedLimit),
        (true, icon::CLIPBOARD_TEXT, "Watch the clipboard on or off", "Offer links you copy", Hit::Clipboard),
        (true, icon::KEYBOARD, "Keyboard shortcuts", "F1", Hit::Shortcuts),
    ];
    for (offered, glyph, name, about, what) in actions {
        if offered && (hit(name) || hit(about)) {
            out.push((glyph, name.to_string(), about.to_string(), what));
        }
    }
    let pages = [
        (Tab::Downloads, icon::DOWNLOAD_SIMPLE, "Downloads", "Ctrl+1"),
        (Tab::Library, icon::BOOKS, "Library", "Ctrl+2"),
        (Tab::Sources, icon::COMPASS, "Sources", "Ctrl+3"),
        (Tab::Settings, icon::GEAR_SIX, "Settings", "Ctrl+4"),
    ];
    for (tab, glyph, name, key) in pages {
        if hit(name) {
            out.push((glyph, format!("Go to {name}"), keys(key).into_owned(), Hit::Go(tab)));
        }
    }
    if !query.is_empty() {
        for item in app.queue.items().iter().filter(|item| hit(&item.filename)).take(6) {
            let (status, _) = status_badge(app, item, &DARK);
            out.push((icon::DOWNLOAD_SIMPLE, item.filename.clone(), status.to_string(), Hit::Show(item.id)));
        }
        for entry in app.history.iter().filter(|entry| hit(&entry.file_name)).take(4) {
            out.push((icon::BOOKS, entry.file_name.clone(), "In the Library".to_string(), Hit::Find(entry.file_name.clone())));
        }
        for kind in kinds::KINDS.iter().filter(|kind| !kind.example.is_empty() && hit(kind.name)).take(3) {
            out.push((kind.glyph, format!("Try {}", kind.name.to_lowercase()), kind.example.to_string(), Hit::Link(kind.example.to_string())));
        }
        out.push((icon::GEAR_SIX, format!("Search settings for \u{201c}{raw}\u{201d}"), String::new(), Hit::Settings(raw.to_string())));
    }
    out
}

fn run(app: &mut App, ctx: &egui::Context, what: Hit) {
    match what {
        Hit::Go(tab) => {
            if tab == Tab::Library {
                app.refresh_history();
            }
            app.tab = tab;
            if tab == Tab::Downloads {
                app.focused = None;
            }
        }
        Hit::NewDownload => {
            app.new_download();
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("url_input")));
        }
        Hit::PauseAll => {
            app.pause_all();
        }
        Hit::ResumeAll => app.resume_all(),
        Hit::ClearCompleted => app.clear_queue(true),
        Hit::CheckFile => {
            if !app.dialog_open && !app.verifying && app.repair.is_none() {
                app.tab = Tab::Library;
                app.pending_dialog = Some(Dialog::VerifyFile);
            }
        }
        Hit::Shortcuts => ctx.data_mut(|d| d.insert_temp(shortcuts_id(), true)),
        Hit::Theme => app.settings.theme = [2, 0, 1][app.settings.theme.min(2)],
        Hit::SpeedLimit => app.settings.speed_limit_on = !app.settings.speed_limit_on,
        Hit::Clipboard => set_clipboard_watch(app, !app.settings.clipboard_watch),
        Hit::Show(id) => app.show_job(id),
        Hit::Find(name) => {
            app.history_search = name;
            app.tab = Tab::Library;
        }
        Hit::Link(link) => {
            app.new_download();
            app.url_input = link;
            app.tab = Tab::Downloads;
            app.focused = None;
        }
        Hit::SavePage(link) => {
            if let Ok(url) = url::Url::parse(&link) {
                app.url_input = link;
                app.tab = Tab::Downloads;
                app.focused = None;
                app.save_page(url);
            }
        }
        Hit::Settings(text) => {
            app.settings_search = text;
            app.tab = Tab::Settings;
        }
    }
}

/// The palette, when open: the window dimmed under it, a box to type in, and the results, the
/// chosen one lit. It drops in from a little above.
pub(super) fn palette(app: &mut App, ctx: &egui::Context) {
    let Some(mut finder) = ctx.data(|d| d.get_temp::<Finder>(finder_id())).filter(|finder| finder.open) else { return };
    let p = palette_of(ctx);
    let shown = anim::ease(anim::progress(ctx, anim::since(ctx, finder_id().with("opened"), finder.session, true), 0.0, anim::APPEAR));
    let screen = ctx.screen_rect();
    let mut close = false;
    // The dimmed window: a click there closes.
    egui::Area::new(finder_id().with("dim")).order(egui::Order::Middle).fixed_pos(screen.min).show(ctx, |ui| {
        let (rect, response) = ui.allocate_exact_size(screen.size(), Sense::click());
        ui.painter().rect_filled(rect, 0.0, Color32::from_black_alpha((90.0 * shown) as u8));
        close |= response.clicked();
    });
    let (up, down, enter, escape) = ctx.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
        )
    });
    let found = results(app, &finder.query);
    let moved = up || down;
    if down {
        finder.chosen = (finder.chosen + 1).min(found.len().saturating_sub(1));
    }
    if up {
        finder.chosen = finder.chosen.saturating_sub(1);
    }
    finder.chosen = finder.chosen.min(found.len().saturating_sub(1));
    let width = (screen.width() - 32.0).min(580.0);
    let at = Pos2::new(screen.center().x - width / 2.0, screen.top() + 84.0 - 10.0 * (1.0 - shown));
    let mut ran = None;
    egui::Area::new(finder_id().with("box")).order(egui::Order::Foreground).fixed_pos(at).show(ctx, |ui| {
        ui.multiply_opacity(shown);
        let frame = Frame::none().fill(p.card).stroke(Stroke::new(1.0_f32, p.border)).rounding(16.0).inner_margin(10.0).shadow(card_shadow(ui, 1.0));
        frame.show(ui, |ui| {
            ui.set_width(width - 20.0);
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                ui.label(RichText::new(icon::MAGNIFYING_GLASS).size(18.0).color(p.muted));
                let edit = egui::TextEdit::singleline(&mut finder.query)
                    .id(finder_id().with("query"))
                    .hint_text("Find an action, a download, a setting, or paste a link")
                    .font(FontId::proportional(16.0))
                    .frame(false)
                    .margin(Vec2::new(4.0, 8.0))
                    .desired_width(ui.available_width());
                let response = ui.add(edit);
                if std::mem::take(&mut finder.focus) {
                    response.request_focus();
                }
                if response.changed() {
                    finder.chosen = 0;
                }
            });
            ui.separator();
            if found.is_empty() {
                ui.label(RichText::new("Nothing found").color(p.muted));
            }
            egui::ScrollArea::vertical().max_height(360.0).auto_shrink([false, true]).show(ui, |ui| {
                for (n, (glyph, text, about, what)) in found.iter().enumerate() {
                    let chosen = n == finder.chosen;
                    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
                    let lit = anim::presence(ui.ctx(), finder_id().with(("lit", n)), chosen || response.hovered(), 0.1);
                    if lit > 0.0 {
                        ui.painter().rect_filled(rect, 10.0, p.accent.gamma_multiply(0.16 * lit));
                    }
                    if chosen && moved {
                        response.scroll_to_me(None);
                    }
                    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                    anim::paint_icon(&painter, rect.left_center() + Vec2::new(18.0, 0.0), glyph, 16.0, if chosen { p.link } else { p.muted }, 0.0);
                    let name = painter.text(rect.left_center() + Vec2::new(40.0, 0.0), Align2::LEFT_CENTER, text, FontId::proportional(14.0), p.strong);
                    if !about.is_empty() {
                        let room = rect.right() - name.right() - 24.0;
                        let about = truncate_chars(about, (room / 6.5).max(4.0) as usize);
                        painter.text(rect.right_center() - Vec2::new(12.0, 0.0), Align2::RIGHT_CENTER, about, FontId::proportional(12.0), p.dim);
                    }
                    if response.clicked() {
                        ran = Some(what.clone());
                    }
                }
            });
        });
    });
    if enter {
        ran = found.get(finder.chosen).map(|(.., what)| what.clone());
    }
    if let Some(what) = ran {
        run(app, ctx, what);
        close = true;
    }
    if close || escape {
        finder = Finder { session: finder.session, ..Default::default() };
    }
    ctx.data_mut(|d| d.insert_temp(finder_id(), finder));
}

#[cfg(test)]
mod tests {
    use super::super::harness::Harness;
    use super::*;
    use egui::{Key, Modifiers};

    /// Ctrl+K opens the palette; typing narrows it, Enter runs the chosen result, Escape closes.
    #[test]
    fn the_palette_finds_and_runs() {
        let mut h = Harness::new();
        h.add("ubuntu.iso");
        assert!(h.settle());
        h.key(Key::K, Modifiers::COMMAND);
        h.frames(10);
        assert!(is_open(&h.app.ctx) && h.find("New download").is_some(), "{:?}", h.texts());
        h.type_text("libr");
        h.frames(2);
        assert!(h.find("Go to Library").is_some(), "{:?}", h.texts());
        h.key(Key::Enter, Modifiers::NONE);
        assert!(!is_open(&h.app.ctx) && h.app.tab == Tab::Library);

        h.key(Key::K, Modifiers::COMMAND);
        h.frames(2);
        h.type_text("ubuntu");
        h.frames(2);
        assert!(h.find("ubuntu.iso").is_some(), "{:?}", h.texts());
        h.key(Key::ArrowDown, Modifiers::NONE);
        // The palette's, over the card's under it.
        let result = h.texts().into_iter().filter(|(text, _)| text == "ubuntu.iso").last().unwrap().1;
        h.click_at(result.center());
        assert!(!is_open(&h.app.ctx) && h.app.tab == Tab::Downloads && h.app.focused == Some(1));

        h.key(Key::K, Modifiers::COMMAND);
        h.frames(2);
        h.paste("https://example.com/disc.iso");
        h.frames(2);
        h.key(Key::Enter, Modifiers::NONE);
        assert_eq!(h.app.url_input, "https://example.com/disc.iso");
        assert!(h.app.queue.items().len() == 1, "only put in the bar");

        h.key(Key::K, Modifiers::COMMAND);
        h.frames(2);
        h.key(Key::Escape, Modifiers::NONE);
        assert!(!is_open(&h.app.ctx));
        assert!(h.settle(), "and keeps still");
    }
}
