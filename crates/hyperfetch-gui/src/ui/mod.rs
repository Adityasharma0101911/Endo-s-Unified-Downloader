mod add;
mod anim;
#[cfg(test)]
mod harness;
mod history;
mod queue;
mod settings;
mod sidebar;

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

use add::{add_link, add_page, clipboard_banner, ffmpeg_notice, ffmpeg_prompt, listing_prompt, notice_banner, update_banner};
use history::history_page;
use queue::queue_page;
use settings::settings_page;
use sidebar::sidebar;

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

impl Palette {
    /// This palette `t` of the way to `other`.
    fn mix(self, other: Self, t: f32) -> Self {
        let m = |a: Color32, b: Color32| a.lerp_to_gamma(b, t);
        Self {
            bg: m(self.bg, other.bg),
            side: m(self.side, other.side),
            card: m(self.card, other.card),
            field: m(self.field, other.field),
            hover: m(self.hover, other.hover),
            border: m(self.border, other.border),
            text: m(self.text, other.text),
            strong: m(self.strong, other.strong),
            muted: m(self.muted, other.muted),
            dim: m(self.dim, other.dim),
            accent: m(self.accent, other.accent),
            green: m(self.green, other.green),
            amber: m(self.amber, other.amber),
            red: m(self.red, other.red),
            cyan: m(self.cyan, other.cyan),
        }
    }
}

fn palette(ui: &Ui) -> Palette {
    palette_of(ui.ctx())
}

/// The colors of the current theme, cross-faded from the other theme's for a moment after a
/// switch (see [`follow_theme`]).
fn palette_of(ctx: &egui::Context) -> Palette {
    match anim::presence(ctx, egui::Id::new("theme"), ctx.theme() == Theme::Dark, 0.3) {
        t if t >= 1.0 => DARK,
        t if t <= 0.0 => LIGHT,
        t => LIGHT.mix(DARK, t),
    }
}

/// Keeps egui's own look (panels, text, fields) in step with [`palette_of`] while it fades.
fn follow_theme(ctx: &egui::Context) {
    let p = palette_of(ctx);
    if ctx.style().visuals.panel_fill != p.bg {
        let theme = ctx.theme();
        ctx.style_mut_of(theme, |style| style.visuals = visuals(theme, p));
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
        ("Inter", include_bytes!("../../assets/Inter-Regular.ttf")),
        ("Inter-SemiBold", include_bytes!("../../assets/Inter-SemiBold.ttf")),
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
            style.visuals = visuals(theme, if theme == Theme::Dark { DARK } else { LIGHT });
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

/// The look of `theme` in the colors `p`.
fn visuals(theme: Theme, p: Palette) -> egui::Visuals {
    let dark = theme == Theme::Dark;
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
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

/// A button in `fill` with white text (see [`eased_button`]).
fn primary_button(label: &str, fill: Color32) -> impl egui::Widget {
    let button = egui::Button::new(bold(label).color(Color32::WHITE));
    move |ui: &mut Ui| eased_button(ui, button, Some(fill))
}

/// An ordinary button (as `ui.button`) whose hover and press ease (see [`eased_button`]).
fn button(text: impl Into<egui::WidgetText>) -> impl egui::Widget {
    let button = egui::Button::new(text);
    move |ui: &mut Ui| eased_button(ui, button, None)
}

/// `button` on a background that brightens and grows a point as the pointer comes over it and
/// dips while pressed: `fill`, else the look of an ordinary button.
fn eased_button(ui: &mut Ui, button: egui::Button<'static>, fill: Option<Color32>) -> egui::Response {
    let background = ui.painter().add(egui::Shape::Noop);
    let response = ui.add(button.fill(Color32::TRANSPARENT).stroke(Stroke::NONE));
    let hover = anim::presence(ui.ctx(), response.id.with("hover"), response.hovered() && ui.is_enabled(), 0.12);
    let press = anim::presence(ui.ctx(), response.id.with("press"), response.is_pointer_button_down_on(), 0.08);
    let w = &ui.visuals().widgets;
    let (rest, lit, stroke) = match fill {
        Some(fill) => (fill, fill.lerp_to_gamma(Color32::WHITE, 0.14), Stroke::NONE),
        None => {
            let stroke = w.inactive.bg_stroke.color.lerp_to_gamma(w.hovered.bg_stroke.color, hover);
            (w.inactive.weak_bg_fill, w.hovered.weak_bg_fill, Stroke::new(1.0, stroke))
        }
    };
    let color = rest.lerp_to_gamma(lit, hover).lerp_to_gamma(Color32::BLACK, 0.12 * press);
    let rect = response.rect.expand(hover - press);
    ui.painter().set(background, egui::epaint::RectShape::new(rect, w.inactive.rounding, color, stroke));
    response
}

/// A frameless icon button, named by its tooltip; its icon grows a little and lifts as the
/// pointer comes over it, and sinks while pressed.
fn icon_button(ui: &mut Ui, glyph: &str, tip: &str, enabled: bool) -> egui::Response {
    let button = |ui: &mut Ui| {
        let size = ui.painter().layout_no_wrap(glyph.to_owned(), FontId::proportional(17.0), Color32::WHITE).size();
        let (rect, response) = ui.allocate_at_least(Vec2::new(size.x, size.y.max(ui.spacing().interact_size.y)), Sense::click());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), tip));
        let hover = anim::presence(ui.ctx(), response.id.with("hover"), response.hovered() && ui.is_enabled(), 0.15);
        let press = anim::presence(ui.ctx(), response.id.with("press"), response.is_pointer_button_down_on(), 0.08);
        if ui.is_rect_visible(rect) {
            let w = &ui.visuals().widgets;
            let color = w.inactive.fg_stroke.color.lerp_to_gamma(w.hovered.fg_stroke.color, hover);
            let center = rect.center() - Vec2::new(0.0, 1.5 * hover - press);
            anim::paint_icon(ui.painter(), center, glyph, 17.0 * (1.0 + 0.15 * hover - 0.12 * press), color, 0.0);
        }
        response
    };
    ui.add_enabled(enabled, button).on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A status chip: `text` on a tint of `color`.
fn chip(ui: &mut Ui, text: &str, color: Color32) {
    tinted_chip(ui, text, color, 0.15);
}

/// [`chip`] on a tint of `color` `tint` strong.
fn tinted_chip(ui: &mut Ui, text: &str, color: Color32, tint: f32) {
    Frame::none().fill(color.gamma_multiply(tint)).rounding(10.0).inner_margin(Margin::symmetric(8.0, 2.0)).show(ui, |ui| {
        ui.add(egui::Label::new(bold(text).size(11.0).color(color)).selectable(false));
    });
}

/// The status chip of the thing `id` names (a download): its colour cross-fades when the status
/// changes, and its tint gently pulses while `live` (downloading).
fn status_chip(ui: &mut Ui, id: egui::Id, text: &str, color: Color32, live: bool) {
    let color = anim::color(ui.ctx(), id.with("status chip"), color, 0.35);
    let glow = if live { anim::pulse(ui.ctx(), 1.8) } else { 0.0 };
    tinted_chip(ui, text, color, 0.15 + 0.13 * glow);
}

/// An on/off switch: the knob slides over and the track's colour follows; the knob swells a
/// little under the pointer.
fn toggle(ui: &mut Ui, on: &mut bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(36.0, 20.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, ""));
    let p = palette(ui);
    let t = anim::presence(ui.ctx(), response.id, *on, 0.18);
    let hover = anim::presence(ui.ctx(), response.id.with("hover"), response.hovered() && ui.is_enabled(), 0.12);
    ui.painter().rect_filled(rect, 10.0, p.dim.gamma_multiply(0.6).lerp_to_gamma(p.accent, t));
    let x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, t);
    ui.painter().circle_filled(Pos2::new(x, rect.center().y), 7.0 + hover, Color32::WHITE);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A progress bar `size` big whose fill eases to `ratio` (keyed by `id`, the thing it measures)
/// with a light sweeping over it while `active`, and `text`, when given, on it.
fn progress_bar(ui: &mut Ui, id: egui::Id, size: Vec2, ratio: f32, color: Color32, active: bool, text: Option<egui::WidgetText>) -> egui::Response {
    let p = palette(ui);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let ratio = anim::value(ui.ctx(), id.with("progress"), ratio.clamp(0.0, 1.0), 0.35);
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let rounding = rect.height() / 2.0;
    // A thin bar sits on a card, where the field color hardly shows.
    ui.painter().rect_filled(rect, rounding, if rect.height() < 10.0 { p.border } else { p.field });
    let done = Rect::from_min_size(rect.min, Vec2::new(rect.width() * ratio, rect.height()));
    if ratio > 0.0 {
        ui.painter().rect_filled(done, rounding, color);
        if active {
            anim::sheen(ui, done.shrink2(Vec2::new(rounding, 0.0)));
        }
    }
    if let Some(text) = text {
        let galley = text.into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Button);
        let pos = rect.left_center() + Vec2::new(ui.spacing().item_spacing.x, -galley.size().y / 2.0);
        ui.painter().with_clip_rect(rect).galley(pos, galley, p.strong);
    }
    response
}

/// A tinted strip: `glyph` and `text` on the left, the buttons `buttons` adds (right to left) on
/// the right. Returns the text's response, for a tooltip. It drops in and fades in the first
/// time its text shows, its icon popping.
fn banner(ui: &mut Ui, color: Color32, glyph: &str, text: impl Into<egui::WidgetText>, buttons: impl FnOnce(&mut Ui)) -> egui::Response {
    let text = text.into();
    let id = egui::Id::new(("banner", glyph, text.text()));
    keyed_banner(ui, id, color, glyph, text, buttons)
}

/// [`banner`], dropping in the first time `id` shows rather than its text: for one whose text
/// changes as it goes on.
fn keyed_banner(ui: &mut Ui, id: egui::Id, color: Color32, glyph: &str, text: impl Into<egui::WidgetText>, buttons: impl FnOnce(&mut Ui)) -> egui::Response {
    let text = text.into();
    let t = anim::appear(ui.ctx(), id, 0.0);
    ui.add_space(8.0);
    anim::shifted(ui, anim::ease(t), anim::DROP, |ui| {
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
                            anim::icon(ui, glyph, 16.0, color, 0.5 + 0.5 * anim::overshoot(t), 0.0);
                            ui.add(egui::Label::new(text).wrap())
                        })
                        .inner
                    })
                    .inner
                })
                .inner
            })
            .inner
    })
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

/// What ffmpeg is for, and what installing it takes.
const FFMPEG_ABOUT: &str = "ffmpeg joins separate video and audio (the best quality) and makes MP3/M4A files. Without it \
     videos download in a lower quality and audio presets fail. The GPL build yt-dlp's makers publish is checked and \
     installed for your user only; it takes about 330 MB on disk.";

// ---- Frame ---------------------------------------------------------------------------------

/// Draws the window. A new page (or another download shown) fades in rising 8 points, its title
/// and contents together; a theme switch cross-fades the colors.
pub fn render(app: &mut App, ctx: &egui::Context) {
    ctx.set_theme(theme_preference(app.settings.theme));
    anim::set_reduced(ctx, app.settings.reduces_motion());
    follow_theme(ctx);
    shortcuts(app, ctx);
    sidebar(app, ctx);
    let frame = Frame::central_panel(&ctx.style()).inner_margin(Margin::symmetric(24.0, 18.0));
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let page = (app.tab as u8, app.focused.filter(|_| app.tab == Tab::Downloader));
        let shown = anim::ease(anim::changed(ctx, egui::Id::new("page"), page, 0.25));
        anim::shifted(ui, shown, anim::RISE, |ui| page_title(app, ui));
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
        anim::shifted(ui, shown, anim::RISE, |ui| match app.tab {
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
        });
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

fn set_clipboard_watch(app: &mut App, on: bool) {
    app.settings.clipboard_watch = on;
    app.clipboard_enabled.store(on, Ordering::Relaxed);
    if !on {
        app.clipboard_banner = None;
    }
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

fn combo(ui: &mut Ui, id: &str, value: &mut usize, names: &[&str]) {
    let selected = names.get(*value).or(names.first()).copied().unwrap_or_default();
    egui::ComboBox::from_id_salt(id).selected_text(selected).show_ui(ui, |ui| {
        for (i, name) in names.iter().enumerate() {
            ui.selectable_value(value, i, *name);
        }
    });
}

#[cfg(test)]
mod tests {
    use egui::{Key, Modifiers};
    use hyperfetch_core::engine::EngineSnapshot;
    use hyperfetch_core::p2p::TorrentProgress;

    use super::harness::{Harness, TOGGLE};
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

    /// The pages by their names in the sidebar.
    const PAGES: [(&str, Tab); 4] = [("Queue", Tab::Queue), ("History", Tab::History), ("Settings", Tab::Settings), ("Add", Tab::Downloader)];

    /// A frame at the next vsync: continuous motion.
    const SOON: Duration = Duration::from_millis(16);

    /// Idle means idle: with nothing running, the pointer gone and the transitions over, no page
    /// (reached through the sidebar) asks for another frame, in either theme; under Reduce motion
    /// not even just after a page change.
    #[test]
    fn idle_pages_ask_for_no_frames() {
        let mut h = Harness::new();
        h.add("queued.iso");
        h.app.notice = Some(Ok("Added #1 to the queue".to_string()));
        assert!(h.settle());
        for theme in [1, 2] {
            h.app.settings.theme = theme;
            for (label, tab) in PAGES {
                h.click(label);
                h.wait_for_background();
                assert_eq!(h.app.tab, tab);
                assert!(h.leave() < Duration::MAX, "{label} fades in");
                assert_eq!(h.frames(40), Duration::MAX, "{label} in theme {theme}");
            }
        }

        h.app.settings.reduce_motion = Some(true);
        for (label, _) in PAGES {
            h.click(label);
            h.wait_for_background();
            h.leave();
            assert_eq!(h.frames(2), Duration::MAX, "{label} under Reduce motion");
        }
    }

    /// A running download keeps frames coming while it shows, in the queue or on its own page;
    /// none while another page shows or the window is minimized, and none once it is done.
    #[test]
    fn a_running_download_asks_for_frames_only_while_it_shows() {
        let mut h = Harness::new();
        let id = h.add("big.iso");
        h.progress(id, 0.4);
        assert!(h.settle(), "the Add page shows no download");
        h.click("Queue");
        h.leave();
        assert!(h.frames(40) <= SOON, "its row moves");
        h.minimized = true;
        assert_eq!(h.frames(40), Duration::MAX, "minimized");
        h.minimized = false;
        assert!(h.frames(2) <= SOON, "restored");
        h.click("Settings");
        h.leave();
        assert_eq!(h.frames(40), Duration::MAX, "out of sight");
        h.click("Queue");
        h.double_click("big.iso");
        h.leave();
        assert!(h.app.tab == Tab::Downloader && h.app.focused == Some(id), "a double click shows it");
        assert!(h.frames(40) <= SOON, "its page moves");
        h.minimized = true;
        assert_eq!(h.frames(40), Duration::MAX, "its page, minimized");
        h.minimized = false;
        h.finish(id, None);
        assert_eq!(h.frames(60), Duration::MAX, "done");
    }

    /// Under Reduce motion a page change lands at once: the frame right after it is the picture
    /// a second later, a running download is still and no frames are asked for; a hover too.
    /// With motion on, that frame is caught on the way.
    #[test]
    fn reduce_motion_lands_at_once_and_keeps_still() {
        for reduce in [true, false] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            let id = h.add("big.iso");
            h.progress(id, 0.4);
            assert!(h.settle());
            h.click("Queue");
            h.leave();
            let soon = h.shot(None);
            let delay = h.frames(60);
            let later = h.shot(None);
            if !reduce {
                assert!(delay <= SOON && soon != later, "with motion, the page is still coming in");
                continue;
            }
            assert!(delay == Duration::MAX && soon == later, "under Reduce motion, the page is in at once");

            h.hover(h.find("Clear completed").unwrap().center());
            let soon = h.shot(None);
            assert_eq!(h.frames(60), Duration::MAX);
            assert!(soon == h.shot(None), "a hover lands at once");
        }
    }

    /// A button under the pointer lights up: its background turns from the field colour to the
    /// hover colour.
    #[test]
    fn a_hovered_button_lights_up() {
        let mut h = Harness::new();
        assert!(h.settle());
        h.click("Queue");
        h.leave();
        assert!(h.settle());
        let button = h.find("Clear completed").unwrap();
        // In the button's padding, left of its text.
        let spot = Rect::from_min_size(Pos2::new(button.min.x - 4.0, button.center().y), Vec2::splat(1.0));
        let before = *h.shot(Some(spot)).get_pixel(0, 0);
        h.hover(button.center());
        assert!(h.settle());
        let after = *h.shot(Some(spot)).get_pixel(0, 0);
        assert_eq!(before.0[..3], [DARK.field.r(), DARK.field.g(), DARK.field.b()]);
        assert_eq!(after.0[..3], [DARK.hover.r(), DARK.hover.g(), DARK.hover.b()]);
    }

    /// The sidebar, the Add form, a queue row's buttons, the Settings switches and the
    /// clipboard switch, driven as a user would, do what they did before any motion.
    #[test]
    fn pages_forms_rows_and_switches_work_through_input() {
        let mut h = Harness::new();
        assert!(h.settle());
        for (label, tab) in PAGES {
            h.click(label);
            assert_eq!(h.app.tab, tab, "{label}");
        }

        // The Add form: typing a link, the advanced options, Ctrl+V outside the link box.
        let input = h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect;
        h.click_at(input.center());
        h.type_text("https://example.com/a.iso");
        assert_eq!(h.app.url_input, "https://example.com/a.iso");
        h.key(Key::Escape, Modifiers::NONE);
        let advanced = "Advanced options: checksum and Authorization header";
        h.click(advanced);
        assert!(h.app.show_advanced);
        assert!(h.settle() && h.find("Checksum").is_some());
        h.click(advanced);
        assert!(!h.app.show_advanced);
        assert!(h.settle() && h.find("Checksum").is_none());
        h.paste("https://example.com/b.iso");
        assert_eq!(h.app.queue.items().iter().map(|item| item.filename.as_str()).collect::<Vec<_>>(), ["b.iso"]);
        assert!(h.settle());
        h.click_at(h.find_near("Added #1 to the queue", icon::X).unwrap().center());
        assert_eq!(h.app.notice, None);

        // Queue rows: a click selects, Delete removes, X removes, Pause pauses, a double click
        // shows, Clear all clears. Clicks are kept apart so as not to make double clicks.
        let [c, d, e] = ["c.iso", "d.iso", "e.iso"].map(|name| h.add(name));
        h.progress(e, 0.5);
        h.click("Queue");
        h.frames(30);
        h.click("b.iso");
        assert_eq!(h.app.selected, Some(1));
        h.key(Key::Delete, Modifiers::NONE);
        assert!(h.app.queue.get_item(1).is_none() && h.app.selected.is_none());
        h.frames(30);
        h.click_at(h.find_near("c.iso", icon::X).unwrap().center());
        assert!(h.app.queue.get_item(c).is_none());
        h.frames(30);
        h.click_at(h.find_near("e.iso", icon::PAUSE).unwrap().center());
        assert_eq!(h.app.queue.get_item(e).unwrap().status, QueueItemStatus::Pausing);
        h.frames(30);
        h.double_click("d.iso");
        assert!(h.app.tab == Tab::Downloader && h.app.focused == Some(d));
        h.click("Queue");
        h.frames(30);
        h.click("Clear all");
        assert_eq!(h.app.queue.items().iter().map(|item| item.id).collect::<Vec<_>>(), [e], "the running one stays");

        // Settings: a switch, the theme, Reduce motion; the sidebar's clipboard switch.
        h.click("Settings");
        assert!(h.settle());
        let run = h.app.settings.auto_run_queue;
        h.click_at(h.find_near("Run the queue automatically", TOGGLE).unwrap().center());
        assert_eq!(h.app.settings.auto_run_queue, !run);
        h.click("Light");
        assert_eq!(h.app.settings.theme, 2);
        h.click_at(h.find_near("Reduce motion", "On").unwrap().center());
        assert_eq!(h.app.settings.reduce_motion, Some(true));
        let watch = h.app.settings.clipboard_watch;
        h.click_at(h.find_near("Clipboard watch", TOGGLE).unwrap().center());
        assert_eq!(h.app.settings.clipboard_watch, !watch);
    }

    /// Every page lays out in both themes with a download in each state, a torrent's peers and
    /// post-processing notes among them, and the theme setting picks the look.
    #[test]
    fn every_page_shows_in_both_themes() {
        let mut h = Harness::new();
        h.add("queued.iso");
        let running = h.add("ubuntu.torrent");
        h.app.queue.mark_started(running);
        let torrent = TorrentProgress { peers: 12, uploaded_bytes: 5 << 20, upload_speed: 2048.0, seeding: false };
        let snapshot = EngineSnapshot {
            total_bytes: 100 << 20,
            downloaded_bytes: 40 << 20,
            speed_bytes_per_sec: 3e6,
            progress_ratio: 0.4,
            torrent: Some(torrent.clone()),
            ..Default::default()
        };
        h.app.queue.apply_snapshot(running, &snapshot);
        h.app.jobs.insert(running, JobView { torrent: Some(torrent), got_snapshot: true, ..Default::default() });
        let done = h.add("done.zip");
        h.app.queue.mark_started(done);
        let notes = vec!["Unpacked into done".to_string(), "VirusTotal: 0 of 70 engines flag this file".to_string()];
        h.app.queue.apply_snapshot(done, &EngineSnapshot { notes, ..Default::default() });
        h.finish(done, None);
        let failed = h.add("gone.bin");
        h.app.queue.mark_started(failed);
        h.finish(failed, Some("The server answered 404 Not Found"));
        h.app.notice = Some(Ok("Added #4 to the queue".to_string()));

        for (theme, dark) in [(1, true), (2, false)] {
            h.app.settings.theme = theme;
            for (_, tab) in PAGES {
                h.app.tab = tab;
                h.frames(2);
                assert_eq!(h.app.ctx.style().visuals.dark_mode, dark);
            }
            for shown in [running, done] {
                h.app.focused = Some(shown);
                h.frames(1);
            }
            h.app.focused = None;
        }
        assert_eq!(h.app.queue.items().len(), 4, "showing changes nothing");
    }

    /// Ctrl+V outside a text box adds the copied links to the queue; in one, it is typed there.
    /// Delete removes the selected download, but not one that is running; Ctrl+number switches
    /// pages.
    #[test]
    fn keyboard_shortcuts() {
        let mut h = Harness::new();
        h.paste("https://example.com/a.iso\n\nhttps://example.com/b.iso");
        let names: Vec<_> = h.app.queue.items().iter().map(|item| item.filename.clone()).collect();
        assert_eq!(names, ["a.iso", "b.iso"]);

        let input = h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect;
        h.click_at(input.center());
        h.paste("https://example.com/c.iso");
        assert_eq!(h.app.queue.items().len(), 2);
        assert_eq!(h.app.url_input, "https://example.com/c.iso");
        h.key(Key::Escape, Modifiers::NONE);

        h.key(Key::Num2, Modifiers::COMMAND);
        assert!(h.app.tab == Tab::Queue);
        let (first, second) = (h.app.queue.items()[0].id, h.app.queue.items()[1].id);
        h.app.queue.mark_started(second);
        h.app.selected = Some(second);
        h.key(Key::Delete, Modifiers::NONE);
        assert_eq!(h.app.queue.items().len(), 2, "a running download stays");
        h.app.selected = Some(first);
        h.key(Key::Delete, Modifiers::NONE);
        assert_eq!(h.app.queue.items().iter().map(|item| item.id).collect::<Vec<_>>(), [second]);
        assert_eq!(h.app.selected, None);

        h.key(Key::Num4, Modifiers::COMMAND);
        assert!(h.app.tab == Tab::Settings);
    }
}
