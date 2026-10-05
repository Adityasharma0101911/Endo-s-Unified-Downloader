use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align, Align2, Color32, FontFamily, FontId, Frame, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, Theme, Ui, Vec2};
use egui_phosphor::regular as icon;
use hyperfetch_core::chunk::ChunkSnapshot;
use hyperfetch_core::history::{HistoryEntry, HistoryStatus};
use hyperfetch_core::ingest::{self, truncate_chars};
use hyperfetch_core::media;
use hyperfetch_core::queue::{QueueItem, QueueItemStatus};

use crate::settings::{Settings, BROWSERS, MEDIA_PRESETS};
use crate::util::{self, format_bytes, format_duration, lock, Verdict};
use crate::{Answer, App, Dialog, Origin, Tab, VerifyRequest, GRAPH_WINDOW};

// ---- Look ----------------------------------------------------------------------------------

/// The colors of one theme.
#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    side: Color32,
    card: Color32,
    field: Color32,
    hover: Color32,
    border: Color32,
    text: Color32,
    strong: Color32,
    muted: Color32,
    dim: Color32,
    accent: Color32,
    green: Color32,
    amber: Color32,
    red: Color32,
    cyan: Color32,
}

const DARK: Palette = Palette {
    bg: Color32::from_rgb(17, 19, 24),
    side: Color32::from_rgb(12, 14, 18),
    card: Color32::from_rgb(24, 27, 34),
    field: Color32::from_rgb(32, 36, 45),
    hover: Color32::from_rgb(42, 47, 58),
    border: Color32::from_rgb(46, 51, 63),
    text: Color32::from_rgb(226, 230, 238),
    strong: Color32::WHITE,
    muted: Color32::from_rgb(152, 162, 179),
    dim: Color32::from_rgb(107, 114, 128),
    accent: Color32::from_rgb(59, 130, 246),
    green: Color32::from_rgb(16, 185, 129),
    amber: Color32::from_rgb(234, 179, 8),
    red: Color32::from_rgb(239, 68, 68),
    cyan: Color32::from_rgb(56, 189, 248),
};

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(244, 245, 248),
    side: Color32::from_rgb(233, 235, 240),
    card: Color32::WHITE,
    field: Color32::from_rgb(241, 243, 246),
    hover: Color32::from_rgb(226, 230, 236),
    border: Color32::from_rgb(216, 220, 228),
    text: Color32::from_rgb(17, 24, 39),
    strong: Color32::BLACK,
    muted: Color32::from_rgb(75, 85, 99),
    dim: Color32::from_rgb(125, 133, 145),
    accent: Color32::from_rgb(37, 99, 235),
    green: Color32::from_rgb(4, 140, 98),
    amber: Color32::from_rgb(180, 83, 9),
    red: Color32::from_rgb(220, 38, 38),
    cyan: Color32::from_rgb(2, 132, 199),
};

fn palette(ui: &Ui) -> Palette {
    if ui.visuals().dark_mode {
        DARK
    } else {
        LIGHT
    }
}

/// The named family of Inter SemiBold (see [`setup`]), for titles and names.
fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

fn bold(text: impl Into<String>) -> RichText {
    RichText::new(text).family(semibold())
}

/// Loads Inter 4.1 (OFL, in `assets/`; its Private Use Area glyphs were cut out, as they would
/// hide the icons) with the Phosphor icons right behind it, and the dark and light looks; the
/// `theme` setting picks one every frame (see [`render`]).
pub fn setup(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let inter: [(&str, &'static [u8]); 2] = [
        ("Inter", include_bytes!("../assets/Inter-Regular.ttf")),
        ("Inter-SemiBold", include_bytes!("../assets/Inter-SemiBold.ttf")),
    ];
    for (name, bytes) in inter {
        fonts.font_data.insert(name.into(), egui::FontData::from_static(bytes));
    }
    fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "Inter".into());
    // Second in line: icons come from it, anything else Inter lacks from egui's own fonts.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    let mut semibold_fonts = fonts.families[&FontFamily::Proportional].clone();
    semibold_fonts[0] = "Inter-SemiBold".into();
    fonts.families.insert(semibold(), semibold_fonts);
    ctx.set_fonts(fonts);

    for theme in [Theme::Dark, Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            style.visuals = visuals(theme);
            style.spacing.item_spacing = Vec2::new(8.0, 6.0);
            style.spacing.button_padding = Vec2::new(10.0, 4.0);
            style.spacing.interact_size.y = 24.0;
            style.text_styles = [
                (egui::TextStyle::Small, FontId::proportional(11.5)),
                (egui::TextStyle::Body, FontId::proportional(14.0)),
                (egui::TextStyle::Button, FontId::proportional(14.0)),
                (egui::TextStyle::Heading, FontId::new(22.0, semibold())),
                (egui::TextStyle::Monospace, FontId::monospace(12.5)),
            ]
            .into();
        });
    }
}

fn visuals(theme: Theme) -> egui::Visuals {
    let dark = theme == Theme::Dark;
    let (mut v, p) = if dark { (egui::Visuals::dark(), DARK) } else { (egui::Visuals::light(), LIGHT) };
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.extreme_bg_color = p.field;
    v.faint_bg_color = p.field;
    v.hyperlink_color = p.accent;
    v.slider_trailing_fill = true;
    v.selection.bg_fill = p.accent.gamma_multiply(if dark { 0.55 } else { 0.25 });
    v.selection.stroke = Stroke::new(1.0, if dark { Color32::from_rgb(191, 219, 254) } else { p.accent });
    let w = &mut v.widgets;
    for (state, fill, stroke, text) in [
        (&mut w.noninteractive, p.card, p.border, p.text),
        (&mut w.inactive, p.field, p.border, p.text),
        (&mut w.hovered, p.hover, p.dim, p.strong),
        (&mut w.active, p.hover, p.accent, p.strong),
        (&mut w.open, p.field, p.border, p.text),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0, stroke);
        state.fg_stroke = Stroke::new(1.0, text);
        state.rounding = egui::Rounding::same(6.0);
    }
    v
}

/// The `theme` setting: 0 follows the system, 1 is dark, 2 light.
fn theme_preference(theme: usize) -> egui::ThemePreference {
    match theme {
        1 => egui::ThemePreference::Dark,
        2 => egui::ThemePreference::Light,
        _ => egui::ThemePreference::System,
    }
}

fn card(ui: &Ui) -> Frame {
    let p = palette(ui);
    Frame::none().fill(p.card).stroke(Stroke::new(1.0, p.border)).inner_margin(16.0).rounding(10.0)
}

fn primary_button(label: &str, fill: Color32) -> egui::Button<'static> {
    egui::Button::new(bold(label).color(Color32::WHITE)).fill(fill).stroke(Stroke::NONE)
}

/// A frameless icon button, named by its tooltip.
fn icon_button(ui: &mut Ui, glyph: &str, tip: &str, enabled: bool) -> egui::Response {
    let button = egui::Button::new(RichText::new(glyph).size(17.0)).frame(false);
    ui.add_enabled(enabled, button).on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A status chip: `text` on a tint of `color`.
fn chip(ui: &mut Ui, text: &str, color: Color32) {
    Frame::none().fill(color.gamma_multiply(0.15)).rounding(10.0).inner_margin(Margin::symmetric(8.0, 2.0)).show(ui, |ui| {
        ui.add(egui::Label::new(bold(text).size(11.0).color(color)).selectable(false));
    });
}

/// An on/off switch.
fn toggle(ui: &mut Ui, on: &mut bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(36.0, 20.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, ""));
    let p = palette(ui);
    let t = ui.ctx().animate_bool(response.id, *on);
    ui.painter().rect_filled(rect, 10.0, if *on { p.accent } else { p.dim.gamma_multiply(0.6) });
    let x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, t);
    ui.painter().circle_filled(Pos2::new(x, rect.center().y), 7.0, Color32::WHITE);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A tinted strip: `glyph` and `text` on the left, the buttons `buttons` adds (right to left) on
/// the right. Returns the text's response, for a tooltip.
fn banner(ui: &mut Ui, color: Color32, glyph: &str, text: impl Into<egui::WidgetText>, buttons: impl FnOnce(&mut Ui)) -> egui::Response {
    ui.add_space(8.0);
    Frame::none()
        .fill(color.gamma_multiply(0.1))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
        .inner_margin(Margin::symmetric(12.0, 8.0))
        .rounding(8.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    buttons(ui);
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.label(RichText::new(glyph).size(16.0).color(color));
                        ui.add(egui::Label::new(text).wrap())
                    })
                    .inner
                })
                .inner
            })
            .inner
        })
        .inner
}

fn error_alert(ui: &mut Ui, text: &str) {
    let p = palette(ui);
    banner(ui, p.red, icon::WARNING_CIRCLE, RichText::new(text).color(p.text), |_| {});
}

/// Lays `add` out left to right in the width left over when `reserve` points stay free on the
/// right.
fn left_part(ui: &mut Ui, reserve: f32, add: impl FnOnce(&mut Ui)) {
    let size = Vec2::new((ui.available_width() - reserve).max(40.0), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), add);
}

fn report(app: &mut App, result: std::io::Result<()>) {
    if let Err(e) = result {
        app.notice = Some(Err(format!("Could not open the file manager: {}", e)));
    }
}

/// Chip text and color for a download's status.
fn status_badge(app: &App, item: &QueueItem, p: &Palette) -> (&'static str, Color32) {
    match &item.status {
        QueueItemStatus::Queued => ("QUEUED", p.dim),
        QueueItemStatus::Downloading if app.is_resolving(item.id) => ("RESOLVING", p.amber),
        QueueItemStatus::Downloading if item.is_finishing() => ("FINISHING", p.cyan),
        QueueItemStatus::Downloading if app.stalled_for(item.id).is_some() => ("STALLED", p.amber),
        QueueItemStatus::Downloading => ("DOWNLOADING", p.accent),
        QueueItemStatus::Pausing => ("PAUSING", p.muted),
        QueueItemStatus::Paused => ("PAUSED", p.muted),
        QueueItemStatus::Completed => ("COMPLETED", p.green),
        QueueItemStatus::Failed(_) => ("FAILED", p.red),
        QueueItemStatus::AuthRequired => ("NEEDS AUTH", p.amber),
    }
}

const AUTH_REQUIRED: &str =
    "Authorization header not saved: enter it again under Advanced options, then Resume to continue from the partial file.";

/// What ffmpeg is for, and what installing it takes.
const FFMPEG_ABOUT: &str = "ffmpeg joins separate video and audio (the best quality) and makes MP3/M4A files. Without it \
     videos download in a lower quality and audio presets fail. The GPL build yt-dlp's makers publish is checked and \
     installed for your user only; it takes about 330 MB on disk.";

// ---- Frame ---------------------------------------------------------------------------------

pub fn render(app: &mut App, ctx: &egui::Context) {
    ctx.set_theme(theme_preference(app.settings.theme));
    shortcuts(app, ctx);
    sidebar(app, ctx);
    let frame = Frame::central_panel(&ctx.style()).inner_margin(Margin::symmetric(24.0, 18.0));
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        page_title(app, ui);
        if app.closing {
            let p = palette(ui);
            let text = "Finishing the live recordings; the window closes once they are saved. Close it again to quit now and cut them off.";
            banner(ui, p.amber, icon::WARNING, RichText::new(text).color(p.text), |_| {});
        }
        update_banner(app, ui);
        clipboard_banner(app, ui);
        listing_prompt(app, ui);
        ffmpeg_prompt(app, ui);
        notice_banner(app, ui);
        ffmpeg_notice(ui, media::installing_ffmpeg());
        ui.add_space(12.0);
        match app.tab {
            // Its list scrolls by itself, showing only the rows in view.
            Tab::Queue => queue_page(app, ui),
            tab => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // Room for the scroll bar.
                    ui.set_max_width(ui.available_width() - 12.0);
                    match tab {
                        Tab::Downloader => add_page(app, ui),
                        Tab::History => history_page(app, ui),
                        _ => settings_page(app, ui),
                    }
                });
            }
        }
    });
}

/// Ctrl+1 to Ctrl+4 switch pages; outside a text box, Ctrl+V adds the copied link(s) to the
/// queue and Delete removes the download selected in the queue.
fn shortcuts(app: &mut App, ctx: &egui::Context) {
    let typing = ctx.wants_keyboard_input();
    let (page, pasted, delete) = ctx.input_mut(|i| {
        let pages =
            [(egui::Key::Num1, Tab::Downloader), (egui::Key::Num2, Tab::Queue), (egui::Key::Num3, Tab::History), (egui::Key::Num4, Tab::Settings)];
        let page = pages.into_iter().find(|&(key, _)| i.consume_key(egui::Modifiers::COMMAND, key)).map(|(_, tab)| tab);
        let pasted = i.events.iter().find_map(|e| match e {
            egui::Event::Paste(text) if !typing => Some(text.clone()),
            _ => None,
        });
        (page, pasted, !typing && i.consume_key(egui::Modifiers::NONE, egui::Key::Delete))
    });
    if let Some(tab) = page {
        go(app, tab);
    }
    for line in pasted.as_deref().unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()) {
        add_link(app, line);
    }
    if let Some(id) = app.selected.filter(|_| delete && app.tab == Tab::Queue) {
        if app.queue.get_item(id).is_some_and(|item| item.status.is_active()) {
            app.notice = Some(Err(format!("Download #{} is running: pause it before removing it", id)));
        } else if app.remove_job(id) {
            app.selected = None;
        }
    }
}

fn go(app: &mut App, tab: Tab) {
    if tab == Tab::History {
        app.refresh_history();
    }
    app.tab = tab;
}

/// Adds `link` to the queue as a link from the clipboard, which the form's checksum and
/// Authorization header don't apply to. A link to read first (a .torrent, playlist, folder) is
/// read.
fn add_link(app: &mut App, link: &str) {
    if ingest::needs_reading(link) {
        app.read_document(link.to_string(), Origin::Dropped, String::new(), String::new());
    } else {
        app.notice = Some(app.add_download(link, "", "").map(|id| format!("Added #{} to the queue", id)));
    }
}

fn set_clipboard_watch(app: &mut App, on: bool) {
    app.settings.clipboard_watch = on;
    app.clipboard_enabled.store(on, Ordering::Relaxed);
    if !on {
        app.clipboard_banner = None;
    }
}

fn nav_item(ui: &mut Ui, glyph: &str, label: &str, count: Option<usize>, selected: bool) -> egui::Response {
    let p = palette(ui);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label));
    let fill = if selected {
        p.accent.gamma_multiply(0.18)
    } else if response.hovered() {
        p.hover
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 8.0, fill);
    let (icon_color, text_color, family) =
        if selected { (p.accent, p.strong, semibold()) } else { (p.muted, p.text, FontFamily::Proportional) };
    painter.text(rect.left_center() + Vec2::new(12.0, 0.0), Align2::LEFT_CENTER, glyph, FontId::proportional(18.0), icon_color);
    painter.text(rect.left_center() + Vec2::new(42.0, 0.0), Align2::LEFT_CENTER, label, FontId::new(14.0, family), text_color);
    if let Some(count) = count.filter(|&n| n > 0) {
        painter.text(rect.right_center() - Vec2::new(12.0, 0.0), Align2::RIGHT_CENTER, count.to_string(), FontId::proportional(12.0), p.muted);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn sidebar(app: &mut App, ctx: &egui::Context) {
    let p = if ctx.style().visuals.dark_mode { DARK } else { LIGHT };
    let frame = Frame::none().fill(p.side).inner_margin(Margin::symmetric(12.0, 16.0));
    egui::SidePanel::left("nav").resizable(false).exact_width(196.0).frame(frame).show(ctx, |ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::hover());
            ui.painter().rect_filled(rect, 9.0, p.accent);
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon::DOWNLOAD_SIMPLE, FontId::proportional(20.0), Color32::WHITE);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(bold("Endo's").size(15.0).color(p.strong));
                ui.label(RichText::new("Unified Downloader").size(11.5).color(p.muted));
            });
        });
        ui.add_space(20.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        let pages = [
            (Tab::Downloader, icon::PLUS_CIRCLE, "Add", None),
            (Tab::Queue, icon::QUEUE, "Queue", Some(app.queue.items().len())),
            (Tab::History, icon::CLOCK_COUNTER_CLOCKWISE, "History", Some(app.history.len())),
            (Tab::Settings, icon::GEAR_SIX, "Settings", None),
        ];
        for (n, (tab, glyph, label, count)) in pages.into_iter().enumerate() {
            if nav_item(ui, glyph, label, count, app.tab == tab).on_hover_text(format!("Ctrl+{}", n + 1)).clicked() {
                go(app, tab);
            }
        }

        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).small().color(p.dim));
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::CIRCLE_HALF).size(16.0).color(p.muted));
                ui.label("Theme");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().button_padding = Vec2::new(5.0, 2.0);
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (value, glyph, name) in [(2, icon::SUN, "Light"), (1, icon::MOON, "Dark"), (0, icon::MONITOR, "Follow the system")] {
                        ui.selectable_value(&mut app.settings.theme, value, glyph).on_hover_text(name);
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::CLIPBOARD_TEXT).size(16.0).color(p.muted));
                ui.label("Clipboard watch");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut on = app.settings.clipboard_watch;
                    if toggle(ui, &mut on).on_hover_text("Offer links you copy anywhere as downloads").changed() {
                        set_clipboard_watch(app, on);
                    }
                });
            });
        });
    });
}

fn page_title(app: &App, ui: &mut Ui) {
    let p = palette(ui);
    let (title, about) = match app.tab {
        Tab::Downloader => match app.focused_item() {
            Some(item) => (format!("Download #{}", item.id), "It keeps going in the queue while you look elsewhere."),
            None => ("Add a download".to_string(), "Files, videos, playlists, channels, torrents, magnets and cloud folders."),
        },
        Tab::Queue => (
            "Queue".to_string(),
            "Click a download to select it; Delete removes it, a double click shows it. Ctrl+V adds a copied link.",
        ),
        Tab::History => ("History".to_string(), "Finished downloads, with their links and checksums."),
        Tab::Settings => ("Settings".to_string(), "Saved when the app closes; downloads started or queued afterwards use them."),
    };
    ui.label(RichText::new(title).heading().color(p.strong));
    ui.label(RichText::new(about).color(p.muted));
}

fn notice_banner(app: &mut App, ui: &mut Ui) {
    let Some(notice) = app.notice.clone() else { return };
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
}

/// Asks whether ffmpeg may be installed, while a media download waits for the answer (see
/// `App::start_job`).
fn ffmpeg_prompt(app: &mut App, ui: &mut Ui) {
    if !app.asks_about_ffmpeg() {
        return;
    }
    let p = palette(ui);
    let text = "ffmpeg is not installed. Install it for the best video quality and MP3/M4A files (about 200 MB)?";
    banner(ui, p.amber, icon::FILM_STRIP, RichText::new(text).color(p.text), |ui| {
        if ui.add(primary_button("Install ffmpeg", p.accent)).clicked() {
            app.answer_ffmpeg(true);
        }
        if ui.button("Continue without").clicked() {
            app.answer_ffmpeg(false);
        }
    })
    .on_hover_text(FFMPEG_ABOUT);
}

/// While ffmpeg is `installing`, a notice, and a frame a second so that it goes when the install
/// ends, also when no download runs any more (a cancelled video leaves the install to finish).
fn ffmpeg_notice(ui: &mut Ui, installing: bool) {
    if installing {
        let p = palette(ui);
        let text = "Installing ffmpeg (about 200 MB download); videos that need it wait until it is ready.";
        banner(ui, p.amber, icon::FILM_STRIP, RichText::new(text).color(p.text), |_| {});
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }
}

/// Offers the newer version a check found, until dismissed for this run. Installing in place
/// works on Windows only; elsewhere What's new leads to the download.
fn update_banner(app: &mut App, ui: &mut Ui) {
    let Some(update) = app.update.clone() else { return };
    let p = palette(ui);
    let text = if app.updating { "Downloading the update…".to_string() } else { format!("Version {} is available.", update.version) };
    banner(ui, p.green, icon::ARROW_CIRCLE_UP, bold(text).color(p.text), |ui| {
        if icon_button(ui, icon::X, "Dismiss", !app.updating).clicked() {
            app.update = None;
        }
        if ui.button("What's new").clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(&update.page));
        }
        if cfg!(windows) && ui.add_enabled(!app.updating, primary_button("Update and restart", p.accent)).clicked() {
            app.install_update();
        }
    });
}

fn clipboard_banner(app: &mut App, ui: &mut Ui) {
    let Some(link) = app.clipboard_banner.clone() else { return };
    let p = palette(ui);
    let text = RichText::new(format!("Copied link: {}", truncate_chars(&link, 55))).color(p.text);
    banner(ui, p.accent, icon::CLIPBOARD_TEXT, text, |ui| {
        if icon_button(ui, icon::X, "Dismiss", true).clicked() {
            app.clipboard_banner = None;
        }
        if ui.button("Paste").on_hover_text("Put it in the link box").clicked() {
            app.clipboard_banner = None;
            app.new_download();
            app.url_input = link.clone();
            app.tab = Tab::Downloader;
        }
        if ui.button("Add to queue").clicked() {
            app.clipboard_banner = None;
            add_link(app, &link);
        }
        if ui.add(primary_button("Download now", p.accent)).clicked() {
            app.clipboard_banner = None;
            app.download_now(&link, "", "");
        }
    })
    .on_hover_text(&link);
}

/// Asks before adding the many downloads a large .metalink, .meta4, .torrent or playlist lists,
/// and whether a video link that names its playlist too means the video or the whole playlist:
/// at once, the video can be picked while the playlist is read.
fn listing_prompt(app: &mut App, ui: &mut Ui) {
    let Some(listing) = app.listings.first() else { return };
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
    let p = palette(ui);
    banner(ui, p.amber, icon::LIST_CHECKS, RichText::new(text).color(p.text), |ui| {
        if ui.button("Cancel").clicked() {
            app.answer_listing(Answer::Cancel);
        }
        if !video {
            if ui.add(primary_button(&all, p.accent)).clicked() {
                app.answer_listing(Answer::All);
            }
            return;
        }
        if ui.add_enabled(playlist_ready, egui::Button::new(&all)).clicked() {
            app.answer_listing(Answer::All);
        }
        if ui.add(primary_button("This video", p.accent)).clicked() {
            app.answer_listing(Answer::Video);
        }
    })
    .on_hover_text(&input);
}

// ---- Add page -------------------------------------------------------------------------------

fn add_page(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let focused = app.focused_item().cloned();
    let active = focused.as_ref().is_some_and(|item| item.status.is_active());

    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        match &focused {
            Some(item) => {
                ui.horizontal(|ui| {
                    let (status, color) = status_badge(app, item, &p);
                    chip(ui, status, color);
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
                    if ui.add_sized([86.0, 32.0], egui::Button::new(format!("{} Paste", icon::CLIPBOARD_TEXT))).clicked() {
                        app.paste_url();
                    }
                    let download = primary_button(&format!("{} Download", icon::DOWNLOAD_SIMPLE), p.accent);
                    if ui.add_sized([110.0, 32.0], download).clicked() {
                        start_from_form(app);
                    }
                });
            }
        }

        if focused.is_none() && ingest::is_blob_url(&app.url_input) {
            error_alert(ui, ingest::BLOB_MESSAGE);
        }
        if let Some(error) = app.form_error.clone() {
            error_alert(ui, &error);
        }
        if let Some(QueueItemStatus::Failed(error)) = focused.as_ref().map(|item| &item.status) {
            error_alert(ui, &format!("Download failed: {}", error));
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::FOLDER_OPEN).size(16.0).color(p.muted)).on_hover_text("Save to");
            ui.add(egui::TextEdit::singleline(&mut app.settings.save_dir).desired_width(ui.available_width() - 96.0));
            if ui.add_enabled(!app.dialog_open, egui::Button::new("Browse…")).clicked() {
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
        let caret = if app.show_advanced { icon::CARET_DOWN } else { icon::CARET_RIGHT };
        let label = RichText::new(format!("{} Advanced options: checksum and Authorization header", caret)).color(p.muted);
        if ui.add(egui::Button::new(label).frame(false)).clicked() {
            app.show_advanced = !app.show_advanced;
        }
        if app.show_advanced {
            advanced_options(app, ui);
        }
    });

    ui.add_space(12.0);
    match &focused {
        Some(item) => download_details(app, ui, item),
        None => {
            card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(bold("Tips").color(p.strong));
                for tip in [
                    "Ctrl+V outside a text box adds the copied link to the queue.",
                    "Drop a .torrent, .metalink or .meta4 file on the window to add every file it lists.",
                    "Links to the same file from several servers, separated by spaces, download from all of them at once.",
                    "The browser extension sends the videos and links of the pages you visit.",
                ] {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon::INFO).color(p.accent));
                        ui.label(RichText::new(tip).color(p.muted));
                    });
                }
            });
        }
    }
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
            if ui.button(RichText::new(format!("{} Pause", icon::PAUSE)).color(p.red)).clicked() {
                app.pause_job(id);
            }
        }
        QueueItemStatus::Pausing => {
            ui.add(egui::Spinner::new());
            let text = if app.is_recording(id) { "Stopping: finishing the recording..." } else { "Pausing: saving the resume state..." };
            ui.label(RichText::new(text).color(p.muted));
        }
        QueueItemStatus::Paused | QueueItemStatus::Failed(_) => {
            let resume = if item.status == QueueItemStatus::Paused { "Resume" } else { "Retry" };
            let button = primary_button(&format!("{} {}", icon::PLAY, resume), p.green);
            if ui.add(button).on_hover_text("Continue from the saved state").clicked() {
                app.start_job(id, false);
            }
            let start_over = ui.button(format!("{} Start over", icon::ARROW_COUNTER_CLOCKWISE));
            if start_over.on_hover_text("Delete the partial file and download from the beginning").clicked() {
                app.start_job(id, true);
            }
            let delete = egui::Button::new(RichText::new(format!("{} Delete leftovers", icon::TRASH)).color(p.red));
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
                if ui.button(format!("{} Show in folder", icon::FOLDER_OPEN)).clicked() {
                    report(app, util::reveal_in_folder(path));
                }
                let idle = !app.verifying && app.repair.is_none();
                if ui.add_enabled(idle, egui::Button::new(format!("{} Verify", icon::SHIELD_CHECK))).clicked() {
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
        let new = ui.button(format!("{} New download", icon::PLUS));
        if new.on_hover_text("Keep this download in the queue and start another").clicked() {
            app.new_download();
        }
    });
}

fn combo(ui: &mut Ui, id: &str, value: &mut usize, names: &[&str]) {
    let selected = names.get(*value).or(names.first()).copied().unwrap_or_default();
    egui::ComboBox::from_id_salt(id).selected_text(selected).show_ui(ui, |ui| {
        for (i, name) in names.iter().enumerate() {
            ui.selectable_value(value, i, *name);
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

fn download_details(app: &App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    progress_card(app, ui, item);
    ui.add_space(6.0);
    let (status, color) = status_line(app, item, &p);
    ui.label(RichText::new(status).color(color));

    let view = app.jobs.get(&item.id);
    let chunks: &[ChunkSnapshot] = view.map_or(&[], |v| &v.chunks);
    // Once the engine has stopped, chunks it last reported as in flight are no longer being fetched.
    let live = view.is_some_and(|v| v.running.is_some());
    if !chunks.is_empty() {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(bold("Chunks").color(p.strong));
            let about = format!("{} ranges, handed to connections as they free up", chunks.len());
            ui.label(RichText::new(about).small().color(p.muted));
        });
        ui.add_space(4.0);
        chunk_map(ui, chunks, item.total_bytes, app.pulse_phase, live);
        ui.add_space(8.0);
        chunk_table(ui, chunks, live);
    }
    ui.add_space(12.0);
    throughput_graph(ui, view.map(|view| &view.speed_history), app.anim_speed, app.pulse_phase);
}

fn metric(ui: &mut Ui, label: &str, value: String, color: Option<Color32>) {
    let p = palette(ui);
    ui.label(RichText::new(label).small().color(p.muted));
    // Truncated: a wrapped line in a column is stretched to its width.
    let text = bold(&value).size(15.0).color(color.unwrap_or(p.strong));
    ui.add(egui::Label::new(text).truncate()).on_hover_text(value);
}

fn progress_card(app: &App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    let view = app.jobs.get(&item.id);
    let torrent = view.and_then(|view| view.torrent.as_ref());
    let downloading = item.status == QueueItemStatus::Downloading;
    let (downloaded, total) = (item.downloaded_bytes, item.total_bytes);
    let elapsed = view.map_or(0, |view| view.elapsed().as_secs());
    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let fill = if item.status == QueueItemStatus::Completed { p.green } else { p.accent };
        let text = util::progress_text(total, downloaded, app.anim_progress, 1, view.map(|_| elapsed));
        let bar = egui::ProgressBar::new(app.anim_progress as f32).fill(fill).desired_height(20.0).animate(downloading);
        ui.add(bar.text(RichText::new(text).color(p.strong)));
        ui.add_space(10.0);

        let eta = match item.status {
            QueueItemStatus::Completed => "Done".to_string(),
            _ if downloading => util::eta_secs(total, downloaded, item.speed_bytes_per_sec).map_or_else(|| "--:--".to_string(), format_duration),
            _ => "--:--".to_string(),
        };
        ui.columns(4, |cols| {
            let transferred =
                if total > 0 { format!("{} / {}", format_bytes(downloaded), format_bytes(total)) } else { format_bytes(downloaded) };
            metric(&mut cols[0], "Transferred", transferred, None);
            metric(&mut cols[1], "Speed", format!("{}/s", format_bytes(app.anim_speed as u64)), Some(p.cyan));
            metric(&mut cols[2], "Elapsed / ETA", format!("{} / {}", format_duration(elapsed), eta), None);
            let (label, connections) = match (torrent, view) {
                (Some(torrent), _) if downloading => ("Peers", torrent.peers.to_string()),
                (_, Some(view)) if downloading => ("Connections", format!("{} active", view.active_workers)),
                _ => ("Connections", "none".to_string()),
            };
            metric(&mut cols[3], label, connections, None);
        });

        if let Some(torrent) = torrent {
            ui.add_space(6.0);
            let mut text = format!(
                "{} Uploaded {} at {}/s",
                icon::UPLOAD_SIMPLE,
                format_bytes(torrent.uploaded_bytes),
                format_bytes(torrent.upload_speed as u64)
            );
            if torrent.seeding {
                text.push_str(", seeding");
            }
            ui.label(RichText::new(text).color(p.muted));
        }
        for note in &item.notes {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::INFO).color(p.accent));
                ui.add(egui::Label::new(note.as_str()).wrap());
            });
        }

        if let Some(stalled) = app.stalled_for(item.id).filter(|_| downloading) {
            ui.add_space(6.0);
            let text = format!(
                "Stalled: no data received for {}s. Stalled connections are retried after {}s without data.",
                stalled.as_secs(),
                item.options.stall_timeout_secs
            );
            ui.label(bold(text).color(p.amber));
        }

        if let Some(view) = view.filter(|view| view.mirror_speeds.len() > 1) {
            ui.add_space(8.0);
            ui.label(bold("Mirrors").size(12.0).color(p.muted));
            egui::Grid::new("mirror_speeds").num_columns(3).striped(true).spacing([24.0, 2.0]).show(ui, |ui| {
                for (id, host, speed) in &view.mirror_speeds {
                    ui.label(RichText::new(format!("#{}", id)).small());
                    ui.label(RichText::new(host).small().monospace());
                    ui.label(RichText::new(format!("{}/s", format_bytes(*speed as u64))).small().color(p.cyan));
                    ui.end_row();
                }
            });
        }
    });
}

fn status_line(app: &App, item: &QueueItem, p: &Palette) -> (String, Color32) {
    match &item.status {
        QueueItemStatus::Queued => ("Queued: starts when a download slot is free (see the Queue)".to_string(), p.muted),
        QueueItemStatus::Downloading if app.is_resolving(item.id) => ("Resolving mirrors and probing endpoints...".to_string(), p.muted),
        QueueItemStatus::Downloading if item.is_finishing() => {
            (format!("Finishing {}: verifying the file and moving it into place...", item.filename), p.muted)
        }
        QueueItemStatus::Downloading => (format!("Downloading {}", item.filename), p.muted),
        QueueItemStatus::Pausing if app.is_recording(item.id) => ("Stopping: yt-dlp is finishing the recording...".to_string(), p.muted),
        QueueItemStatus::Pausing => ("Pausing: saving resume state...".to_string(), p.muted),
        QueueItemStatus::Paused => {
            ("Paused. Resume continues where it stopped; Start over deletes the partial file first.".to_string(), p.muted)
        }
        QueueItemStatus::Completed => {
            let path = item.target_path.as_ref().map_or_else(|| item.filename.clone(), |p| p.display().to_string());
            (format!("Completed: {}", path), p.green)
        }
        QueueItemStatus::Failed(error) => (format!("Error: {}", error), p.red),
        QueueItemStatus::AuthRequired => (AUTH_REQUIRED.to_string(), p.amber),
    }
}

/// A worker owns the chunk ("Worker n" from the range engine, "Downloading" from the HLS engine).
fn is_active_chunk(chunk: &ChunkSnapshot) -> bool {
    chunk.status.starts_with("Worker") || chunk.status == "Downloading"
}

fn chunk_map(ui: &mut Ui, chunks: &[ChunkSnapshot], total_bytes: u64, pulse_phase: f32, live: bool) {
    let p = palette(ui);
    let height = 22.0;
    let (response, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), height), Sense::hover());
    let rect = response.rect;
    painter.rect_filled(rect, 6.0, p.field);
    if chunks.is_empty() || total_bytes == 0 {
        return;
    }
    let painter = painter.with_clip_rect(rect.shrink(1.0));
    let total = total_bytes as f32;
    let width = rect.width();
    for chunk in chunks {
        let start_ratio = (chunk.range_start as f32 / total).clamp(0.0, 1.0);
        let end_ratio = ((chunk.range_end + 1) as f32 / total).clamp(0.0, 1.0);
        let seg_x = rect.min.x + start_ratio * width;
        let seg_w = ((end_ratio - start_ratio) * width).max(1.0);
        let seg_rect = Rect::from_min_size(Pos2::new(seg_x, rect.min.y + 1.0), Vec2::new(seg_w, height - 2.0));

        if chunk.total_bytes > 0 && chunk.downloaded_bytes >= chunk.total_bytes {
            painter.rect_filled(seg_rect, 0.0, p.green);
        } else if chunk.total_bytes > 0 && chunk.downloaded_bytes > 0 {
            let filled = (chunk.downloaded_bytes as f32 / chunk.total_bytes as f32).clamp(0.0, 1.0);
            painter.rect_filled(seg_rect, 0.0, p.accent.gamma_multiply(0.3));
            let fill_rect = Rect::from_min_size(seg_rect.min, Vec2::new(seg_w * filled, height - 2.0));
            painter.rect_filled(fill_rect, 0.0, p.accent);
        } else if live && is_active_chunk(chunk) {
            // Assigned but no bytes yet: pulsing.
            let pulse = (pulse_phase.sin() + 1.0) * 0.5;
            painter.rect_filled(seg_rect, 0.0, p.cyan.gamma_multiply(0.35 + pulse * 0.5));
        } else if chunk.status.starts_with("Failed") {
            painter.rect_filled(seg_rect, 0.0, p.red.gamma_multiply(0.6));
        } else {
            painter.rect_filled(seg_rect, 0.0, p.border);
        }
        painter.line_segment(
            [Pos2::new(seg_x + seg_w, rect.min.y + 1.0), Pos2::new(seg_x + seg_w, rect.max.y - 1.0)],
            Stroke::new(1.0, p.card),
        );
    }
    painter.rect_stroke(rect, 6.0, Stroke::new(1.0, p.border));
}

fn chunk_table(ui: &mut Ui, chunks: &[ChunkSnapshot], live: bool) {
    let p = palette(ui);
    card(ui).inner_margin(10.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let columns = [(50.0, "ID"), (160.0, "Byte range"), (130.0, "Downloaded"), (140.0, "Progress"), (160.0, "Status")];
        ui.horizontal(|ui| {
            for (width, title) in columns {
                ui.add_sized([width, 20.0], egui::Label::new(bold(title).size(12.0).color(p.muted)));
            }
        });
        ui.separator();
        // Only visible rows are laid out: an HLS stream can have thousands of segments.
        let row_height = 18.0 + ui.spacing().item_spacing.y;
        let scroll = egui::ScrollArea::vertical().id_salt("chunk_table").max_height(200.0).auto_shrink([false, true]);
        scroll.show_rows(ui, row_height, chunks.len(), |ui, rows| {
            for chunk in &chunks[rows] {
                ui.horizontal(|ui| {
                    ui.add_sized([50.0, 18.0], egui::Label::new(format!("#{}", chunk.id)));
                    let range = format!("{} - {}", format_bytes(chunk.range_start), format_bytes(chunk.range_end));
                    ui.add_sized([160.0, 18.0], egui::Label::new(range));
                    let done = format!("{} / {}", format_bytes(chunk.downloaded_bytes), format_bytes(chunk.total_bytes));
                    ui.add_sized([130.0, 18.0], egui::Label::new(done));
                    let ratio = if chunk.total_bytes > 0 {
                        (chunk.downloaded_bytes as f32 / chunk.total_bytes as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    ui.add_sized([140.0, 18.0], egui::ProgressBar::new(ratio).fill(p.accent).show_percentage());
                    let color = if chunk.status == "Completed" {
                        p.green
                    } else if live && is_active_chunk(chunk) {
                        p.cyan
                    } else if chunk.status.starts_with("Failed") {
                        p.red
                    } else {
                        p.muted
                    };
                    let status = if !live && is_active_chunk(chunk) { "Stopped" } else { chunk.status.as_str() };
                    ui.add_sized([160.0, 18.0], egui::Label::new(RichText::new(status).color(color)).truncate())
                        .on_hover_text(&chunk.status);
                });
            }
        });
    });
}

fn throughput_graph(ui: &mut Ui, history: Option<&VecDeque<(Instant, f64)>>, current: f64, pulse_phase: f32) {
    let p = palette(ui);
    let now = Instant::now();
    let window = GRAPH_WINDOW.as_secs_f32();
    // (age in seconds, speed) of the samples inside the window.
    let samples: Vec<(f32, f64)> = history
        .into_iter()
        .flatten()
        .map(|(t, speed)| (now.duration_since(*t).as_secs_f32(), *speed))
        .filter(|(age, _)| *age <= window)
        .collect();
    let peak = samples.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);
    let avg = if samples.is_empty() { 0.0 } else { samples.iter().map(|(_, s)| s).sum::<f64>() / samples.len() as f64 };

    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(bold("Throughput").color(p.strong));
            ui.label(RichText::new("last 60 seconds").small().color(p.muted));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let stats = format!(
                    "Peak {}/s  ·  Avg {}/s  ·  Now {}/s",
                    format_bytes(peak as u64),
                    format_bytes(avg as u64),
                    format_bytes(current as u64)
                );
                ui.label(RichText::new(stats).small().color(p.muted));
            });
        });
        ui.add_space(6.0);

        let graph_height = 90.0;
        let (response, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), graph_height), Sense::hover());
        let rect = response.rect;
        painter.rect_filled(rect, 6.0, p.field);
        let y_mid = rect.center().y;
        painter.line_segment([Pos2::new(rect.min.x, y_mid), Pos2::new(rect.max.x, y_mid)], Stroke::new(1.0, p.border));

        let max_y = (peak * 1.15).max(1024.0 * 1024.0);
        for (y, value) in [(rect.min.y + 4.0, max_y), (y_mid + 2.0, max_y / 2.0)] {
            let label = format!("{}/s", format_bytes(value as u64));
            painter.text(Pos2::new(rect.min.x + 6.0, y), Align2::LEFT_TOP, label, FontId::proportional(10.0), p.dim);
        }

        let points: Vec<Pos2> = samples
            .iter()
            .map(|(age, speed)| {
                let x = rect.min.x + (1.0 - age / window) * rect.width();
                let y = (rect.max.y - 2.0) - (*speed as f32 / max_y as f32).clamp(0.0, 1.0) * (rect.height() - 6.0);
                Pos2::new(x, y)
            })
            .collect();
        if points.len() >= 2 {
            // Area fill as one mesh with a vertical gradient.
            let mut mesh = egui::Mesh::default();
            for point in &points {
                mesh.colored_vertex(*point, p.accent.gamma_multiply(0.25));
                mesh.colored_vertex(Pos2::new(point.x, rect.max.y - 1.0), p.accent.gamma_multiply(0.02));
            }
            for i in 0..(points.len() as u32 - 1) {
                let (top_left, bot_left, top_right, bot_right) = (i * 2, i * 2 + 1, i * 2 + 2, i * 2 + 3);
                mesh.add_triangle(top_left, bot_left, bot_right);
                mesh.add_triangle(top_left, bot_right, top_right);
            }
            painter.add(egui::Shape::mesh(mesh));
            painter.add(egui::Shape::line(points.clone(), Stroke::new(2.0, p.accent)));

            if let Some(&last) = points.last() {
                let pulse = (pulse_phase.sin() + 1.0) * 0.5;
                painter.circle_filled(last, 4.0 + pulse * 3.0, p.accent.gamma_multiply(0.15 + pulse * 0.2));
                painter.circle(last, 3.5, p.card, Stroke::new(2.0, p.accent));
            }
        }

        if let Some(hover) = response.hover_pos().filter(|pos| rect.contains(*pos)) {
            let target_age = (1.0 - (hover.x - rect.min.x) / rect.width()) * window;
            let nearest = samples.iter().min_by(|(a, _), (b, _)| (a - target_age).abs().total_cmp(&(b - target_age).abs()));
            if let Some((_, speed)) = nearest {
                painter.line_segment([Pos2::new(hover.x, rect.min.y), Pos2::new(hover.x, rect.max.y)], Stroke::new(1.0, p.muted));
                response.show_tooltip_text(format!("T-{}s: {}/s", target_age as u64, format_bytes(*speed as u64)));
            }
        }
    });
}

// ---- Queue page -----------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum RowAction {
    Select,
    Start,
    Pause,
    Show,
    Remove,
    Open,
    Reveal,
}

/// Height of one queue row: every row has the same, so only the rows in view are laid out.
const ROW_HEIGHT: f32 = 58.0;

/// The buttons of a queue row for a download in `status`: icon, tooltip and action.
fn row_actions(status: &QueueItemStatus) -> &'static [(&'static str, &'static str, RowAction)] {
    use RowAction::*;
    match status {
        QueueItemStatus::Queued => &[(icon::PLAY, "Start", Start), (icon::INFO, "Details", Show), (icon::X, "Remove (files are kept)", Remove)],
        QueueItemStatus::Downloading => &[(icon::PAUSE, "Pause", Pause), (icon::INFO, "Details", Show)],
        QueueItemStatus::Pausing => &[(icon::INFO, "Details", Show)],
        QueueItemStatus::Paused => &[(icon::PLAY, "Resume", Start), (icon::INFO, "Details", Show), (icon::X, "Remove (files are kept)", Remove)],
        QueueItemStatus::Failed(_) => {
            &[(icon::ARROW_CLOCKWISE, "Retry", Start), (icon::INFO, "Details", Show), (icon::X, "Remove (files are kept)", Remove)]
        }
        QueueItemStatus::AuthRequired => {
            &[(icon::INFO, "Details: enter the Authorization header there to resume", Show), (icon::X, "Remove (files are kept)", Remove)]
        }
        QueueItemStatus::Completed => &[
            (icon::FILE, "Open the file", Open),
            (icon::FOLDER_OPEN, "Show in its folder", Reveal),
            (icon::INFO, "Details", Show),
            (icon::X, "Remove from the list (the file is kept)", Remove),
        ],
    }
}

/// The second line of a queue row: where it is, how fast, how long still, its peers and upload,
/// and what post-processing said.
fn row_summary(app: &App, item: &QueueItem) -> String {
    let view = app.jobs.get(&item.id);
    let elapsed = view.map(|view| view.elapsed().as_secs());
    let mut parts = vec![format!("#{}", item.id)];
    if item.status == QueueItemStatus::Completed {
        parts.push(format_bytes(item.total_bytes.max(item.downloaded_bytes)));
    } else {
        parts.push(util::progress_text(item.total_bytes, item.downloaded_bytes, item.progress_ratio, 0, elapsed));
        if item.total_bytes > 0 {
            parts.push(format!("{} of {}", format_bytes(item.downloaded_bytes), format_bytes(item.total_bytes)));
        }
    }
    if item.status == QueueItemStatus::Downloading {
        parts.push(format!("{}/s", format_bytes(item.speed_bytes_per_sec as u64)));
        if let Some(eta) = util::eta_secs(item.total_bytes, item.downloaded_bytes, item.speed_bytes_per_sec) {
            parts.push(format!("{} left", format_duration(eta)));
        }
    }
    if let Some(torrent) = view.and_then(|view| view.torrent.as_ref()) {
        parts.push(format!("{} peers", torrent.peers));
        parts.push(format!("{} {} up", icon::UPLOAD_SIMPLE, format_bytes(torrent.uploaded_bytes)));
        if torrent.seeding {
            parts.push("seeding".to_string());
        }
    }
    parts.extend(item.notes.iter().cloned());
    parts.join("  ·  ")
}

/// A thin progress bar.
fn thin_bar(ui: &mut Ui, width: f32, ratio: f32, color: Color32) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 6.0), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, p.border);
    let done = Rect::from_min_size(rect.min, Vec2::new(rect.width() * ratio.clamp(0.0, 1.0), rect.height()));
    ui.painter().rect_filled(done, 3.0, color);
}

/// One row of the queue list for `item`: status, name and buttons, then progress, speed, time
/// left, peers and notes; or why it failed (in full on hover), or why one restored without its
/// Authorization header waits. A click selects it, a double click shows it.
fn queue_row(app: &App, ui: &mut Ui, item: &QueueItem, action: &mut Option<(usize, RowAction)>) {
    let p = palette(ui);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());
    if app.selected == Some(item.id) {
        ui.painter().rect_filled(rect, 8.0, p.accent.gamma_multiply(0.14));
        let stripe = Rect::from_min_size(rect.min + Vec2::new(0.0, 10.0), Vec2::new(3.0, rect.height() - 20.0));
        ui.painter().rect_filled(stripe, 2.0, p.accent);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 8.0, p.hover.gamma_multiply(0.6));
    }
    if response.double_clicked() {
        *action = Some((item.id, RowAction::Show));
    } else if response.clicked() {
        *action = Some((item.id, RowAction::Select));
    }

    let inner = egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(12.0, 7.0))).layout(Layout::top_down(Align::LEFT));
    let mut ui = ui.new_child(inner);
    let (status, color) = status_badge(app, item, &p);
    let actions = row_actions(&item.status);
    ui.horizontal(|ui| {
        chip(ui, status, color);
        left_part(ui, actions.len() as f32 * 30.0, |ui| {
            let name = egui::Label::new(bold(&item.filename).color(p.strong)).truncate().selectable(false);
            ui.add(name).on_hover_text(&item.filename);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            for &(glyph, tip, what) in actions.iter().rev() {
                if icon_button(ui, glyph, tip, true).clicked() {
                    *action = Some((item.id, what));
                }
            }
        });
    });
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        let (text, color) = match &item.status {
            QueueItemStatus::Failed(error) => (truncate_chars(error, 200), p.red),
            QueueItemStatus::AuthRequired => ("Authorization header not saved: open Details to enter it again and resume".to_string(), p.amber),
            status => {
                let bar = match status {
                    QueueItemStatus::Completed => p.green,
                    QueueItemStatus::Downloading => p.accent,
                    _ => p.dim,
                };
                thin_bar(ui, 140.0, item.progress_ratio as f32, bar);
                (row_summary(app, item), p.muted)
            }
        };
        let label = ui.add(egui::Label::new(RichText::new(text).small().color(color)).truncate().selectable(false));
        match &item.status {
            QueueItemStatus::Failed(error) => {
                label.on_hover_text(error);
            }
            _ if !item.notes.is_empty() => {
                label.on_hover_text(item.notes.join("\n"));
            }
            _ => {}
        }
    });
}

fn queue_page(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(bold("Add to queue").color(p.strong));
        let about = "One download per line; links to the same file from several servers go on one line, separated by \
                     spaces. A .torrent or .metalink (link, file path, or file dropped on the window) adds every file it \
                     lists, a playlist or channel every video. Uses the save folder and Settings.";
        ui.label(RichText::new(about).small().color(p.muted));
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut app.queue_input)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("https://example.com/file1.iso\nhttps://example.com/file2.zip https://mirror.example.org/file2.zip"),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.add(primary_button(&format!("{} Add to queue", icon::PLUS), p.accent)).clicked() {
                app.add_queue_input();
            }
            ui.add_space(12.0);
            toggle(ui, &mut app.settings.auto_run_queue).on_hover_text("Start queued downloads by themselves");
            ui.label("Run automatically,");
            ui.add(egui::DragValue::new(&mut app.settings.max_concurrent).range(1..=8));
            ui.label("at once");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let clear_all = ui.button(format!("{} Clear all", icon::BROOM));
                if clear_all.on_hover_text("Remove every download that is not running (files are kept)").clicked() {
                    app.clear_queue(false);
                }
                if ui.button("Clear completed").clicked() {
                    app.clear_queue(true);
                }
            });
        });
        if let Some(error) = app.queue_error.clone() {
            error_alert(ui, &error);
        }
    });

    ui.add_space(12.0);
    let items = app.queue.items();
    ui.horizontal(|ui| {
        ui.label(bold(format!("{} downloads", items.len())).color(p.strong));
        let running = app.queue.active_count();
        if running > 0 {
            ui.label(RichText::new(format!("{} running", running)).color(p.accent));
        }
    });
    ui.add_space(4.0);

    let mut action = None;
    let shown: &App = app;
    card(ui).inner_margin(4.0).show(ui, |ui| {
        if items.is_empty() {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.label(RichText::new(icon::QUEUE).size(28.0).color(p.dim));
                ui.label(RichText::new("The queue is empty. Add links above, or press Ctrl+V with one copied.").color(p.muted));
                ui.add_space(30.0);
            });
            return;
        }
        ui.spacing_mut().item_spacing.y = 2.0;
        let scroll = egui::ScrollArea::vertical().id_salt("queue_rows").auto_shrink([false, false]).max_height(ui.available_height().max(120.0));
        scroll.show_rows(ui, ROW_HEIGHT, items.len(), |ui, rows| {
            ui.set_max_width(ui.available_width() - 10.0);
            for item in &items[rows] {
                queue_row(shown, ui, item, &mut action);
            }
        });
    });

    let Some((id, action)) = action else { return };
    app.selected = Some(id);
    let target = app.queue.get_item(id).and_then(|item| item.target_path.clone());
    match action {
        RowAction::Select => {}
        RowAction::Start => {
            app.start_job(id, false);
        }
        RowAction::Pause => app.pause_job(id),
        RowAction::Show => app.show_job(id),
        RowAction::Remove => {
            app.remove_job(id);
        }
        RowAction::Open => {
            if let Some(path) = target {
                report(app, util::open_path(&path));
            }
        }
        RowAction::Reveal => {
            if let Some(path) = target {
                report(app, util::reveal_in_folder(&path));
            }
        }
    }
}

// ---- History page ---------------------------------------------------------------------------

fn history_page(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::MAGNIFYING_GLASS).size(16.0).color(p.muted));
            let search = egui::TextEdit::singleline(&mut app.history_search)
                .hint_text("Filter by file name, link or hash")
                .desired_width((ui.available_width() - 360.0).max(120.0));
            ui.add(search);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button(RichText::new(format!("{} Clear history", icon::TRASH)).color(p.red)).clicked() {
                    app.update_history(|history| history.clear());
                }
                if ui.button(format!("{} Refresh", icon::ARROWS_CLOCKWISE)).clicked() {
                    app.refresh_history();
                }
                let can_verify = !app.dialog_open && !app.verifying && app.repair.is_none();
                let verify = egui::Button::new(format!("{} Verify a file…", icon::SHIELD_CHECK));
                if ui.add_enabled(can_verify, verify).on_hover_text("Check a file against what its download recorded").clicked() {
                    app.pending_dialog = Some(Dialog::VerifyFile);
                }
            });
        });
        verification_card(app, ui);
    });

    ui.add_space(12.0);
    let search = util::history_search_key(&app.history_search);
    let entries: Vec<HistoryEntry> = app
        .history
        .iter()
        .filter(|e| {
            search.is_empty()
                || e.file_name.to_lowercase().contains(&search)
                || e.urls.iter().any(|u| u.to_lowercase().contains(&search))
                || e.blake3_hash.as_ref().is_some_and(|h| h.to_lowercase().contains(&search))
        })
        .cloned()
        .collect();
    card(ui).inner_margin(4.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if entries.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.label(RichText::new(icon::CLOCK_COUNTER_CLOCKWISE).size(28.0).color(p.dim));
                ui.label(RichText::new("No downloads recorded in history yet.").color(p.muted));
                ui.add_space(30.0);
            });
            return;
        }
        for (n, entry) in entries.iter().enumerate() {
            if n > 0 {
                ui.separator();
            }
            history_row(app, ui, entry);
        }
    });
}

fn history_row(app: &mut App, ui: &mut Ui, entry: &HistoryEntry) {
    let p = palette(ui);
    let idle = !app.verifying && app.repair.is_none();
    Frame::none().inner_margin(Margin::symmetric(12.0, 6.0)).show(ui, |ui| {
        ui.horizontal(|ui| {
            let (badge, color) = match entry.status {
                HistoryStatus::Completed => ("COMPLETED", p.green),
                HistoryStatus::Failed(_) => ("FAILED", p.red),
                HistoryStatus::Cancelled => ("CANCELLED", p.muted),
            };
            chip(ui, badge, color);
            left_part(ui, 5.0 * 30.0, |ui| {
                ui.add(egui::Label::new(bold(&entry.file_name).color(p.strong)).truncate()).on_hover_text(&entry.file_name);
                ui.label(RichText::new(format_bytes(entry.file_size)).small().color(p.muted));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, icon::TRASH, "Remove from the history", true).clicked() {
                    let id = entry.id.clone();
                    app.update_history(move |history| {
                        history.remove_entry(&id);
                    });
                }
                if icon_button(ui, icon::DOWNLOAD_SIMPLE, "Download again", true).clicked() {
                    match util::redownload_input(&entry.urls) {
                        Ok(input) => {
                            app.new_download();
                            app.url_input = input;
                            app.tab = Tab::Downloader;
                        }
                        Err(e) => app.notice = Some(Err(e)),
                    }
                }
                if icon_button(ui, icon::SHIELD_CHECK, "Verify & repair", idle).clicked() {
                    app.verify(VerifyRequest {
                        path: entry.file_path.clone(),
                        expected_size: (entry.file_size > 0).then_some(entry.file_size),
                        checksum: None,
                    });
                }
                if icon_button(ui, icon::FOLDER_OPEN, "Show in its folder", true).clicked() {
                    report(app, util::reveal_in_folder(&entry.file_path));
                }
                if let Some(url) = entry.urls.first() {
                    if icon_button(ui, icon::COPY, "Copy the link", true).clicked() {
                        app.copy_text(url.clone());
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            if let Some(hash) = &entry.blake3_hash {
                ui.label(RichText::new(format!("BLAKE3 {}", truncate_chars(hash, 19))).small().monospace().color(p.dim)).on_hover_text(hash);
            }
            let path = entry.file_path.display().to_string();
            ui.add(egui::Label::new(RichText::new(&path).small().monospace().color(p.dim)).truncate()).on_hover_text(path);
        });
    });
}

fn verification_card(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    if app.verifying {
        let name = app
            .verify_request
            .as_ref()
            .and_then(|r| r.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.label(RichText::new(format!("Verifying {}... hashing a large file can take a while", name)).color(p.muted));
        });
    }

    if let Some(verification) = &app.verification {
        let result = &verification.result;
        let verdict = util::verdict(result);
        let (badge, color) = match verdict {
            Verdict::Verified => ("VERIFIED", p.green),
            Verdict::Incomplete => ("INCOMPLETE", p.amber),
            Verdict::Mismatch => ("CHECKSUM MISMATCH", p.red),
            Verdict::Unverified => ("UNVERIFIED", p.muted),
        };
        let name = result.file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let message = result.status_message.clone();
        let can_repair = !verification.repair_urls.is_empty();
        let repair_progress = app.repair.as_ref().map(|r| *lock(&r.progress));
        let (mut dismiss, mut repair, mut cancel) = (false, false, false);

        ui.add_space(10.0);
        Frame::none()
            .fill(color.gamma_multiply(0.08))
            .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
            .inner_margin(12.0)
            .rounding(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    chip(ui, badge, color);
                    ui.label(bold(name).color(p.strong));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        dismiss = ui.add_enabled(repair_progress.is_none(), egui::Button::new("Dismiss")).clicked();
                        if verdict == Verdict::Incomplete && repair_progress.is_none() {
                            repair = ui
                                .add_enabled(can_repair, primary_button("Repair missing chunks now", p.accent))
                                .on_disabled_hover_text("No download URLs are recorded for exactly this file")
                                .clicked();
                        }
                    });
                });
                ui.label(RichText::new(message).color(p.text));

                if let Some((done, total)) = repair_progress {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        cancel = ui.button(RichText::new("Cancel repair").color(p.red)).clicked();
                        let ratio = if total > 0 { (done as f32 / total as f32).clamp(0.0, 1.0) } else { 0.0 };
                        let text = format!("Repairing missing chunks: {} / {}", format_bytes(done), format_bytes(total));
                        ui.add(egui::ProgressBar::new(ratio).fill(p.accent).show_percentage().text(text));
                    });
                }
            });

        if dismiss {
            app.verification = None;
            app.verify_message = None;
        }
        if repair {
            app.start_repair();
        }
        if cancel {
            if let Some(repair) = &app.repair {
                repair.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    if let Some(message) = &app.verify_message {
        ui.add_space(4.0);
        ui.label(RichText::new(message).small().color(p.muted));
    }
}

// ---- Settings page --------------------------------------------------------------------------

fn section(ui: &mut Ui, glyph: &str, title: &str, add: impl FnOnce(&mut Ui)) {
    let p = palette(ui);
    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(glyph).size(18.0).color(p.accent));
            ui.label(bold(title).size(16.0).color(p.strong));
        });
        add(ui);
    });
    ui.add_space(12.0);
}

/// One row of a Settings section: `title` and what it does on the left, `control` on the right
/// (laid out right to left).
fn setting(ui: &mut Ui, title: &str, about: &str, control: impl FnOnce(&mut Ui)) {
    let p = palette(ui);
    ui.separator();
    ui.horizontal(|ui| {
        let width = (ui.available_width() * 0.5).clamp(160.0, 380.0);
        let size = Vec2::new(width, ui.spacing().interact_size.y);
        ui.allocate_ui_with_layout(size, Layout::top_down(Align::LEFT), |ui| {
            ui.set_width(width);
            ui.label(RichText::new(title).color(p.text));
            if !about.is_empty() {
                ui.label(RichText::new(about).small().color(p.muted));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
}

/// A setting with an on/off switch; true when it was switched.
fn switch(ui: &mut Ui, title: &str, about: &str, on: &mut bool) -> bool {
    let mut changed = false;
    setting(ui, title, about, |ui| changed = toggle(ui, on).changed());
    changed
}

fn text_setting(ui: &mut Ui, title: &str, about: &str, value: &mut String, hint: &str, secret: bool) {
    setting(ui, title, about, |ui| {
        ui.add(egui::TextEdit::singleline(value).hint_text(hint).password(secret).desired_width(ui.available_width()));
    });
}

fn settings_page(app: &mut App, ui: &mut Ui) {
    section(ui, icon::SLIDERS, "General", |ui| general_settings(app, ui));
    section(ui, icon::NETWORK, "Connections & networks", |ui| connection_settings(&mut app.settings, ui));
    section(ui, icon::MAGNET, "BitTorrent", |ui| torrent_settings(app, ui));
    section(ui, icon::CLOUD_ARROW_DOWN, "Debrid & hosts", |ui| host_settings(app, ui));
    section(ui, icon::FILM_STRIP, "Media", |ui| media_settings(app, ui));
    section(ui, icon::PACKAGE, "Post-processing", |ui| post_settings(&mut app.settings, ui));
    section(ui, icon::PUZZLE_PIECE, "Browser extension", |ui| {
        let p = palette(ui);
        ui.separator();
        let text = "The extension hands this app the links, videos and cookies of the pages you visit, on this computer \
                    only; there is nothing to set up here. What it sends cannot change these settings: post-processing, \
                    the command run after downloads, networks and seeding stay as set here.";
        ui.add(egui::Label::new(RichText::new(text).color(p.muted)).wrap());
    });
    section(ui, icon::ARROW_CIRCLE_UP, "Updates", |ui| {
        setting(ui, "Version", "", |ui| {
            if ui.button("Check now").clicked() {
                app.check_for_update(true);
            }
            ui.label(env!("CARGO_PKG_VERSION"));
        });
        let about = "Asks GitHub whether a newer release is out and offers it; nothing is installed unless you choose to, \
                     and only releases signed by the maintainer are.";
        switch(ui, "Check for updates at startup", about, &mut app.settings.check_updates);
    });
    section(ui, icon::WRENCH, "Advanced", |ui| {
        let s = &mut app.settings;
        setting(ui, "Retries", "Failed attempts per chunk before the download fails; attempts that made progress don't count.", |ui| {
            ui.add(egui::DragValue::new(&mut s.max_retries).range(0..=100));
        });
        setting(ui, "Stall timeout", "A connection that receives nothing for this long is retried.", |ui| {
            ui.add(egui::DragValue::new(&mut s.stall_timeout_secs).range(5..=600).suffix(" s"));
        });
        let about = "Wait until each finished file is on the disk before showing it as done. Slower; without it a power \
                     loss right after a download finishes can damage the file (Verify finds that).";
        switch(ui, "Flush finished files to disk", about, &mut s.fsync_on_complete);
    });
}

fn general_settings(app: &mut App, ui: &mut Ui) {
    setting(ui, "Save folder", "Where new downloads go.", |ui| {
        if ui.add_enabled(!app.dialog_open, egui::Button::new("Browse…")).clicked() {
            app.pending_dialog = Some(Dialog::SaveDir);
        }
        ui.add(egui::TextEdit::singleline(&mut app.settings.save_dir).desired_width(ui.available_width()));
    });
    setting(ui, "Theme", "", |ui| {
        for (value, name) in [(2, "Light"), (1, "Dark"), (0, "System")] {
            ui.selectable_value(&mut app.settings.theme, value, name);
        }
    });
    let mut watch = app.settings.clipboard_watch;
    if switch(ui, "Watch the clipboard", "Offer links you copy anywhere as downloads.", &mut watch) {
        set_clipboard_watch(app, watch);
    }
    let s = &mut app.settings;
    switch(ui, "Run the queue automatically", "Start queued downloads by themselves.", &mut s.auto_run_queue);
    setting(ui, "Downloads at once", "How many queued downloads run together.", |ui| {
        ui.add(egui::DragValue::new(&mut s.max_concurrent).range(1..=8));
    });
}

fn connection_settings(s: &mut Settings, ui: &mut Ui) {
    setting(ui, "Connections per download", "More can be faster; some servers allow only a few.", |ui| {
        ui.add(egui::Slider::new(&mut s.connections, 1..=64));
    });
    setting(ui, "Connections per server", "All running downloads together, to one server (0 = no limit).", |ui| {
        ui.add(egui::DragValue::new(&mut s.max_connections_per_host).range(0..=256));
    });
    setting(ui, "Speed limit", "For all downloads together (0 = unlimited).", |ui| {
        let in_mb = &mut s.max_speed_in_mb;
        egui::ComboBox::from_id_salt("speed_unit_combo").width(70.0).selected_text(if *in_mb { "MB/s" } else { "KB/s" }).show_ui(
            ui,
            |ui| {
                ui.selectable_value(in_mb, false, "KB/s");
                ui.selectable_value(in_mb, true, "MB/s");
            },
        );
        ui.add(egui::DragValue::new(&mut s.max_speed).range(0.0..=1_000_000.0).speed(1.0).max_decimals(1));
    });
    text_setting(ui, "Proxy", "Every connection goes through it.", &mut s.proxy, "http://127.0.0.1:8080 or socks5://127.0.0.1:1080", false);
    let about = "Several proxies, comma-separated; connections take turns over them.";
    text_setting(ui, "Proxy pool", about, &mut s.proxy_pool, "http://p1:8080, http://p2:8080", false);
    let about = "Spread connections over every network this computer is on (Wi-Fi and Ethernet, say), or the ones chosen below.";
    switch(ui, "Use several networks", about, &mut s.multi_network);
    ui.add_enabled_ui(s.multi_network, |ui| network_choice(ui, &mut s.bind_addresses));
}

/// The local addresses to connect from: the usable networks (looked up once, and again on
/// request) to tick, and the addresses as text.
fn network_choice(ui: &mut Ui, chosen: &mut Vec<String>) {
    let p = palette(ui);
    let id = egui::Id::new("usable_networks");
    let found = ui.data(|d| d.get_temp::<Vec<(String, String)>>(id)).unwrap_or_else(|| {
        let found: Vec<(String, String)> =
            hyperfetch_core::netif::usable().into_iter().map(|n| (n.name, n.address.to_string())).collect();
        ui.data_mut(|d| d.insert_temp(id, found.clone()));
        found
    });
    setting(ui, "Networks to use", "None ticked: every usable network.", |ui| {
        if icon_button(ui, icon::ARROWS_CLOCKWISE, "Look for networks again", true).clicked() {
            ui.data_mut(|d| d.remove::<Vec<(String, String)>>(id));
        }
        ui.vertical(|ui| {
            if found.is_empty() {
                ui.label(RichText::new("No usable network found").color(p.muted));
            }
            for (name, address) in &found {
                let mut on = chosen.iter().any(|c| c.trim() == address);
                if ui.checkbox(&mut on, format!("{} ({})", name, address)).changed() {
                    if on {
                        chosen.push(address.clone());
                    } else {
                        chosen.retain(|c| c.trim() != address);
                    }
                }
            }
        });
    });
    setting(ui, "Addresses", "The local IP addresses to connect from, comma-separated.", |ui| {
        // Split and joined on the same comma, so typing is never undone.
        let mut text = chosen.join(",");
        let edit = egui::TextEdit::singleline(&mut text).hint_text("192.168.1.20, 10.0.0.5").desired_width(ui.available_width());
        if ui.add(edit).changed() {
            *chosen = if text.trim().is_empty() { Vec::new() } else { text.split(',').map(str::to_string).collect() };
        }
    });
    if let Some(bad) = chosen.iter().map(|a| a.trim()).find(|a| !a.is_empty() && a.parse::<std::net::IpAddr>().is_err()) {
        error_alert(ui, &format!("{} is not an IP address; it is left out", bad));
    }
}

fn torrent_settings(app: &mut App, ui: &mut Ui) {
    let s = &mut app.settings;
    let about = "Magnet links and .torrent files without web seeds download from the BitTorrent swarm.";
    // Turned off: no torrent goes on seeding, listening or forwarding a port.
    if switch(ui, "Download from peers", about, &mut s.p2p) && !s.p2p {
        app.rt.spawn(hyperfetch_core::p2p::shutdown());
    }
    ui.add_enabled_ui(s.p2p, |ui| {
        let about = "Keep sharing a finished torrent until it has uploaded this many times its size (0 = don't seed).";
        setting(ui, "Seed ratio", about, |ui| {
            ui.add(egui::DragValue::new(&mut s.seed_ratio).range(0.0..=100.0).speed(0.05).max_decimals(2));
        });
        setting(ui, "Seed time limit", "Stop sharing after this long, whichever comes first (0 = no limit).", |ui| {
            ui.add(egui::DragValue::new(&mut s.seed_minutes).range(0..=100_000).suffix(" min"));
        });
        let about = "The port peers connect to (0 = the default range). Port and forwarding changes apply once no torrent is running.";
        setting(ui, "Listen port", about, |ui| {
            ui.add(egui::DragValue::new(&mut s.bt_port).range(0..=65535));
        });
        let about = "Ask the router to forward that port, so more peers can reach this computer.";
        switch(ui, "Port forwarding (UPnP)", about, &mut s.bt_upnp);
    });
}

/// Debrid services by the name the settings keep ("" = auto-detect).
const DEBRID_SERVICES: [(&str, &str); 5] = [
    ("", "Auto-detect"),
    ("realdebrid", "Real-Debrid"),
    ("alldebrid", "AllDebrid"),
    ("torbox", "TorBox"),
    ("premiumize", "Premiumize"),
];

fn debrid_combo(ui: &mut Ui, provider: &mut String) {
    // Older versions saved "real-debrid".
    let current = provider.trim().to_lowercase().replace('-', "");
    let shown = DEBRID_SERVICES.iter().find(|(key, _)| *key == current).map_or_else(|| provider.clone(), |(_, name)| name.to_string());
    egui::ComboBox::from_id_salt("debrid_provider_combo").selected_text(shown).show_ui(ui, |ui| {
        for (key, name) in DEBRID_SERVICES {
            if ui.selectable_label(current == key, name).clicked() {
                *provider = key.to_string();
            }
        }
    });
}

fn host_settings(app: &mut App, ui: &mut Ui) {
    let about = "Unlocks hoster links, and magnets, at full speed through your account.";
    setting(ui, "Debrid service", about, |ui| debrid_combo(ui, &mut app.settings.debrid_provider));
    let about = "Saved in the settings only, never with the queue or in the history.";
    text_setting(ui, "Debrid API key", about, &mut app.settings.debrid_api_key, "Real-Debrid, AllDebrid, TorBox or Premiumize key", true);
    let about = "With a key set, magnet links go to the debrid service first; a torrent it has cached arrives at once.";
    switch(ui, "Magnets through debrid", about, &mut app.settings.debrid_magnets);
    let about = "Lists whole Google Drive folders with sizes and checksums. Sent only to Google's Drive API and kept until \
                 the app closes; set ENDO_GOOGLE_API_KEY to have it at every launch. Without one a folder is read from its \
                 public page, which has no sizes or checksums.";
    text_setting(ui, "Google API key", about, &mut app.settings.google_api_key, "Optional", true);
    setting(ui, "Browser cookies", "Sign in to sites as in this browser, for videos and folders that need it.", |ui| {
        combo(ui, "browser_cookies_combo", &mut app.settings.browser_cookies, &BROWSERS);
    });
    setting(ui, "Cookies file", "A Netscape cookies.txt, for sites that need you signed in.", |ui| {
        if ui.add_enabled(!app.dialog_open, egui::Button::new("Browse…")).clicked() {
            app.pending_dialog = Some(Dialog::CookiesFile);
        }
        let edit = egui::TextEdit::singleline(&mut app.settings.cookies_path).hint_text("Optional").desired_width(ui.available_width());
        ui.add(edit);
    });
    let about = "Sent as the Referer header, for hosts that check where a download comes from.";
    text_setting(ui, "Referer", about, &mut app.settings.referer, "https://example.com/", false);
}

fn media_settings(app: &mut App, ui: &mut Ui) {
    setting(ui, "Quality", "For videos and audio from sites like YouTube.", |ui| {
        combo(ui, "media_preset_combo", &mut app.settings.media_preset, &MEDIA_PRESETS);
    });
    let s = &mut app.settings;
    let about = "Languages to save a video's subtitles in, as .srt or .vtt files next to it: the site's own, else its automatic captions.";
    text_setting(ui, "Subtitles", about, &mut s.subtitles, "en,es or all", false);
    let about = "Title, artist, date, description, link and chapters inside the file (needs ffmpeg).";
    switch(ui, "Embed tags and chapters", about, &mut s.embed_metadata);
    let about = "Where the site keeps it, from its start instead of from now. Stop finishes the recording and keeps it.";
    switch(ui, "Record live streams from the start", about, &mut s.live_from_start);
    let about = "A stream or premiere that has not begun is waited for, checking every 1 to 10 minutes. Stop ends the wait.";
    switch(ui, "Wait for scheduled streams", about, &mut s.wait_for_video);
    let about = "An HLS (m3u8) stream is remuxed into an MP4 without re-encoding; needs ffmpeg, without it the .ts is kept.";
    switch(ui, "Convert HLS streams to MP4", about, &mut s.hls_to_mp4);
    let about = "Imgur, Pixiv, DeviantArt, ArtStation, Flickr, Tumblr, Pinterest, boorus, Bluesky and Reddit galleries; \
                 gallery-dl is installed when first needed.";
    switch(ui, "Image galleries with gallery-dl", about, &mut s.gallery_dl);
    let about = "Leave out the videos, tracks, episodes and files of a playlist, channel, feed or folder downloaded before.";
    switch(ui, "Only new playlist items", about, &mut s.only_new);
    setting(ui, "Newest items only", "Only this many of a channel's, playlist's or feed's newest items (0 = all).", |ui| {
        ui.add(egui::DragValue::new(&mut s.latest).range(0..=100_000));
    });
    let mut install = app.settings.install_ffmpeg == Some(true);
    if switch(ui, "Install ffmpeg when a video needs it", FFMPEG_ABOUT, &mut install) {
        app.answer_ffmpeg(install);
    }
}

fn post_settings(s: &mut Settings, ui: &mut Ui) {
    ui.add_enabled_ui(cfg!(windows), |ui| {
        let about = "Windows then asks before running a downloaded program, as it does for browser downloads.";
        switch(ui, "Mark as downloaded from the internet", about, &mut s.mark_of_the_web);
    });
    switch(ui, "Unpack archives", "Zip, 7z, rar and tar files unpack into a folder next to them.", &mut s.auto_extract);
    ui.add_enabled_ui(s.auto_extract, |ui| {
        switch(ui, "Delete archives after unpacking", "Only once everything is unpacked.", &mut s.delete_archives);
    });
    let about = "Single files move into Video, Music, Pictures, Documents, Archives or Programs in the save folder.";
    switch(ui, "Sort into folders", about, &mut s.sort_downloads);
    let about = "Sends the file's SHA-256 only, never the file, and notes how many engines flag it.";
    switch(ui, "Look files up on VirusTotal", about, &mut s.virustotal_check);
    ui.add_enabled_ui(s.virustotal_check, |ui| {
        let about = "Saved in the settings only, never with the queue or in the history.";
        text_setting(ui, "VirusTotal API key", about, &mut s.virustotal_api_key, "Your VirusTotal key", true);
    });
    let about = "A program and its arguments, run without a shell after each download. {path}, {dir}, {name} and {url} are filled in.";
    text_setting(ui, "Run after each download", about, &mut s.run_after, "e.g. C:\\Tools\\scan.exe \"{path}\"", false);
}

#[cfg(test)]
mod tests {
    use hyperfetch_core::engine::{DownloadOptions, EngineSnapshot};
    use hyperfetch_core::p2p::TorrentProgress;
    use hyperfetch_core::queue::DownloadQueue;

    use super::*;
    use crate::JobView;

    /// The delay before the next frame after one with the ffmpeg notice, `installing` or not. The
    /// first frame asks for a second one at once, to lay itself out.
    fn next_frame_after(installing: bool) -> Duration {
        let ctx = egui::Context::default();
        let frame = || {
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| ffmpeg_notice(ui, installing));
            });
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
        };
        frame();
        frame()
    }

    /// While ffmpeg installs, frames keep coming though no download runs, so the notice goes when
    /// the install ends.
    #[test]
    fn the_ffmpeg_notice_keeps_frames_coming_while_it_shows() {
        assert!(next_frame_after(true) <= Duration::from_secs(1));
        assert_eq!(next_frame_after(false), Duration::MAX);
    }

    /// An app with the fonts and looks of `setup`, its save folder in `dir`.
    fn test_app(rt: &tokio::runtime::Runtime, dir: &std::path::Path) -> App {
        let ctx = egui::Context::default();
        setup(&ctx);
        let settings = Settings { save_dir: dir.to_string_lossy().into_owned(), ..Settings::default() };
        App::with(ctx, rt.handle().clone(), settings, DownloadQueue::new(), None)
    }

    /// One frame of the whole window with these `events`.
    fn frame(app: &mut App, events: Vec<egui::Event>) {
        let ctx = app.ctx.clone();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(980.0, 720.0))),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| render(app, ctx));
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    fn link(name: &str) -> Vec<url::Url> {
        vec![url::Url::parse(&format!("https://example.com/{}", name)).unwrap()]
    }

    /// Every page lays out in both themes with a download in each state, a torrent's peers and
    /// post-processing notes among them, and the theme setting picks the look.
    #[test]
    fn every_page_shows_in_both_themes() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        let options = DownloadOptions::default();
        app.queue.add_item(link("queued.iso"), options.clone());
        let running = app.queue.add_item(link("ubuntu.torrent"), options.clone());
        app.queue.mark_started(running);
        let torrent = TorrentProgress { peers: 12, uploaded_bytes: 5 << 20, upload_speed: 2048.0, seeding: false };
        let snapshot = EngineSnapshot {
            total_bytes: 100 << 20,
            downloaded_bytes: 40 << 20,
            speed_bytes_per_sec: 3e6,
            progress_ratio: 0.4,
            torrent: Some(torrent.clone()),
            ..Default::default()
        };
        app.queue.apply_snapshot(running, &snapshot);
        app.jobs.insert(running, JobView { torrent: Some(torrent), got_snapshot: true, ..Default::default() });
        let done = app.queue.add_item(link("done.zip"), options.clone());
        app.queue.mark_started(done);
        let notes = vec!["Unpacked into done".to_string(), "VirusTotal: 0 of 70 engines flag this file".to_string()];
        app.queue.apply_snapshot(done, &EngineSnapshot { notes, ..Default::default() });
        app.queue.finish(done, Ok((dir.path().join("done.zip"), Some(1 << 20))));
        let failed = app.queue.add_item(link("gone.bin"), options);
        app.queue.mark_started(failed);
        app.queue.finish(failed, Err("The server answered 404 Not Found".to_string()));
        app.notice = Some(Ok("Added #4 to the queue".to_string()));

        for (theme, dark) in [(1, true), (2, false)] {
            app.settings.theme = theme;
            for tab in [Tab::Downloader, Tab::Queue, Tab::History, Tab::Settings] {
                app.tab = tab;
                frame(&mut app, Vec::new());
                frame(&mut app, Vec::new());
                assert_eq!(app.ctx.style().visuals.dark_mode, dark);
            }
            app.focused = Some(running);
            app.tab = Tab::Downloader;
            frame(&mut app, Vec::new());
            app.focused = Some(done);
            frame(&mut app, Vec::new());
            app.focused = None;
        }
        assert_eq!(app.queue.items().len(), 4, "showing changes nothing");
    }

    /// Ctrl+V outside a text box adds the copied link to the queue; in one, it is typed there.
    /// Delete removes the selected download, but not one that is running; Ctrl+number switches
    /// pages.
    #[test]
    fn keyboard_shortcuts() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        frame(&mut app, vec![egui::Event::Paste("https://example.com/a.iso\n\nhttps://example.com/b.iso".to_string())]);
        let names: Vec<_> = app.queue.items().iter().map(|item| item.filename.clone()).collect();
        assert_eq!(names, ["a.iso", "b.iso"]);

        app.ctx.memory_mut(|m| m.request_focus(egui::Id::new("url_input")));
        frame(&mut app, Vec::new());
        frame(&mut app, vec![egui::Event::Paste("https://example.com/c.iso".to_string())]);
        assert_eq!(app.queue.items().len(), 2);
        assert_eq!(app.url_input, "https://example.com/c.iso");
        app.ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("url_input")));

        frame(&mut app, vec![key(egui::Key::Num2, egui::Modifiers::COMMAND)]);
        assert!(app.tab == Tab::Queue);
        let (first, second) = (app.queue.items()[0].id, app.queue.items()[1].id);
        app.queue.mark_started(second);
        app.selected = Some(second);
        frame(&mut app, vec![key(egui::Key::Delete, egui::Modifiers::NONE)]);
        assert_eq!(app.queue.items().len(), 2, "a running download stays");
        app.selected = Some(first);
        frame(&mut app, vec![key(egui::Key::Delete, egui::Modifiers::NONE)]);
        assert_eq!(app.queue.items().iter().map(|item| item.id).collect::<Vec<_>>(), [second]);
        assert_eq!(app.selected, None);

        frame(&mut app, vec![key(egui::Key::Num4, egui::Modifiers::COMMAND)]);
        assert!(app.tab == Tab::Settings);
    }
}
