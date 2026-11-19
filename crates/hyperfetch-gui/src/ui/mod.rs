mod add;
mod anim;
mod backdrop;
mod chooser;
mod command;
#[cfg(test)]
mod harness;
mod history;
mod kinds;
mod queue;
mod settings;
mod sidebar;
mod sources;

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

use crate::desktop::WhenDone;
use crate::settings::{Settings, BROWSERS, MEDIA_PRESETS};
use crate::util::{self, format_bytes, format_duration, lock, Verdict};
use crate::{Answer, App, Dialog, Origin, Tab, VerifyRequest, GRAPH_WINDOW};

use add::{add_link, channel_prompt, clipboard_banner, ffmpeg_notice, ffmpeg_prompt, front_door, listing_prompt, notice_banner, undo_toast, update_banner};
use history::library_page;
use queue::{detail_page, queue_page};
use settings::settings_page;
use sidebar::sidebar;
use sources::sources_page;

// ---- Look ----------------------------------------------------------------------------------

/// The colours of one theme ("midnight drift" dark, "pale paper" light).
#[derive(Clone, Copy)]
struct Palette {
    /// The window behind everything; the moving background washes over it.
    bg: Color32,
    /// The navigation rail, laid over the background a little translucent.
    side: Color32,
    card: Color32,
    field: Color32,
    hover: Color32,
    border: Color32,
    text: Color32,
    strong: Color32,
    muted: Color32,
    dim: Color32,
    /// Filled buttons, bars and selection: white text reads on it.
    accent: Color32,
    /// The accent as text and thin lines on the page (links, the focus ring).
    link: Color32,
    green: Color32,
    amber: Color32,
    red: Color32,
    cyan: Color32,
    /// One colour per connection of a download, none of them green (done) or red (failed).
    lanes: [Color32; 6],
}

const DARK: Palette = Palette {
    bg: Color32::from_rgb(10, 13, 26),
    side: Color32::from_rgb(7, 9, 18),
    card: Color32::from_rgb(20, 25, 44),
    field: Color32::from_rgb(27, 33, 56),
    hover: Color32::from_rgb(38, 46, 76),
    border: Color32::from_rgb(44, 52, 84),
    text: Color32::from_rgb(224, 229, 243),
    strong: Color32::from_rgb(248, 250, 255),
    muted: Color32::from_rgb(156, 166, 194),
    dim: Color32::from_rgb(122, 132, 165),
    accent: Color32::from_rgb(72, 98, 230),
    link: Color32::from_rgb(150, 172, 255),
    green: Color32::from_rgb(52, 211, 153),
    amber: Color32::from_rgb(245, 185, 66),
    red: Color32::from_rgb(248, 113, 113),
    cyan: Color32::from_rgb(94, 214, 240),
    lanes: [
        Color32::from_rgb(110, 140, 255),
        Color32::from_rgb(176, 132, 255),
        Color32::from_rgb(94, 214, 240),
        Color32::from_rgb(255, 160, 96),
        Color32::from_rgb(240, 128, 200),
        Color32::from_rgb(230, 200, 90),
    ],
};

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(244, 242, 236),
    side: Color32::from_rgb(234, 231, 222),
    card: Color32::from_rgb(253, 252, 249),
    field: Color32::from_rgb(237, 235, 228),
    hover: Color32::from_rgb(226, 223, 214),
    border: Color32::from_rgb(218, 214, 204),
    text: Color32::from_rgb(26, 29, 41),
    strong: Color32::from_rgb(8, 10, 18),
    muted: Color32::from_rgb(82, 87, 104),
    dim: Color32::from_rgb(108, 112, 126),
    accent: Color32::from_rgb(44, 76, 200),
    link: Color32::from_rgb(44, 76, 200),
    green: Color32::from_rgb(4, 120, 87),
    amber: Color32::from_rgb(168, 86, 6),
    red: Color32::from_rgb(200, 30, 30),
    cyan: Color32::from_rgb(3, 105, 161),
    lanes: [
        Color32::from_rgb(44, 76, 200),
        Color32::from_rgb(124, 58, 237),
        Color32::from_rgb(8, 130, 170),
        Color32::from_rgb(217, 96, 20),
        Color32::from_rgb(200, 40, 120),
        Color32::from_rgb(170, 120, 0),
    ],
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
            link: m(self.link, other.link),
            green: m(self.green, other.green),
            amber: m(self.amber, other.amber),
            red: m(self.red, other.red),
            cyan: m(self.cyan, other.cyan),
            lanes: std::array::from_fn(|n| m(self.lanes[n], other.lanes[n])),
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

/// White, or the darkest ink, whichever reads on `fill`.
fn on_color(fill: Color32) -> Color32 {
    let lin = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    let luminance = 0.2126 * lin(fill.r()) + 0.7152 * lin(fill.g()) + 0.0722 * lin(fill.b());
    if luminance > 0.3 { Color32::from_rgb(8, 10, 18) } else { Color32::WHITE }
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
            style.spacing.button_padding = Vec2::new(12.0, 5.0);
            style.spacing.interact_size.y = 26.0;
            style.spacing.scroll = egui::style::ScrollStyle::thin();
            style.text_styles = [
                (egui::TextStyle::Small, FontId::proportional(12.0)),
                (egui::TextStyle::Body, FontId::proportional(14.0)),
                (egui::TextStyle::Button, FontId::proportional(14.0)),
                (egui::TextStyle::Heading, FontId::new(24.0, semibold())),
                (egui::TextStyle::Monospace, FontId::monospace(12.5)),
            ]
            .into();
        });
    }
}

/// The look of `theme` in the colors `p`: soft rounded controls, the caret and selection in the
/// accent, floating windows on a soft shadow.
fn visuals(theme: Theme, p: Palette) -> egui::Visuals {
    let dark = theme == Theme::Dark;
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_rounding = egui::Rounding::same(14.0);
    v.menu_rounding = egui::Rounding::same(10.0);
    let shade = Color32::from_black_alpha(if dark { 110 } else { 38 });
    v.window_shadow = egui::Shadow { offset: Vec2::new(0.0, 14.0), blur: 36.0, spread: 0.0, color: shade };
    v.popup_shadow = egui::Shadow { offset: Vec2::new(0.0, 8.0), blur: 22.0, spread: 0.0, color: shade };
    v.extreme_bg_color = p.field;
    v.faint_bg_color = p.field;
    v.hyperlink_color = p.link;
    v.slider_trailing_fill = true;
    v.text_cursor.stroke = Stroke::new(2.0, p.link);
    v.selection.bg_fill = p.accent.gamma_multiply(if dark { 0.55 } else { 0.22 });
    v.selection.stroke = Stroke::new(1.0, p.link);
    let w = &mut v.widgets;
    for (state, fill, stroke, text) in [
        (&mut w.noninteractive, p.card, p.border, p.text),
        (&mut w.inactive, p.field, p.border, p.text),
        (&mut w.hovered, p.hover, p.dim, p.strong),
        (&mut w.active, p.hover, p.link, p.strong),
        (&mut w.open, p.field, p.border, p.text),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0, stroke);
        state.fg_stroke = Stroke::new(1.0, text);
        state.rounding = egui::Rounding::same(8.0);
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

/// The soft shadow under a card, `lift` (0.0 resting to 1.0 raised) off the page.
fn card_shadow(ui: &Ui, lift: f32) -> egui::Shadow {
    let dark = ui.visuals().dark_mode;
    let alpha = if dark { 70.0 + 50.0 * lift } else { 18.0 + 22.0 * lift };
    egui::Shadow { offset: Vec2::new(0.0, 4.0 + 6.0 * lift), blur: 16.0 + 14.0 * lift, spread: 0.0, color: Color32::from_black_alpha(alpha as u8) }
}

fn card(ui: &Ui) -> Frame {
    let p = palette(ui);
    Frame::none().fill(p.card).stroke(Stroke::new(1.0, p.border)).inner_margin(18.0).rounding(14.0).shadow(card_shadow(ui, 0.0))
}

/// A button in `fill` with text that reads on it (see [`eased_button`]).
fn primary_button(label: &str, fill: Color32) -> impl egui::Widget {
    let button = egui::Button::new(bold(label).color(on_color(fill)));
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
        let (rect, response) = ui.allocate_at_least(Vec2::new(size.x + 4.0, size.y.max(ui.spacing().interact_size.y)), Sense::click());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), tip));
        let hover = anim::presence(ui.ctx(), response.id.with("hover"), response.hovered() && ui.is_enabled(), 0.15);
        let press = anim::presence(ui.ctx(), response.id.with("press"), response.is_pointer_button_down_on(), 0.08);
        if ui.is_rect_visible(rect) {
            let w = &ui.visuals().widgets;
            if hover > 0.0 {
                ui.painter().rect_filled(rect.expand(2.0), 8.0, w.hovered.weak_bg_fill.gamma_multiply(0.7 * hover));
            }
            let color = w.inactive.fg_stroke.color.lerp_to_gamma(w.hovered.fg_stroke.color, hover);
            let center = rect.center() - Vec2::new(0.0, 1.0 * hover - press);
            anim::paint_icon(ui.painter(), center, glyph, 17.0 * (1.0 + 0.1 * hover - 0.1 * press), color, 0.0);
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
        ui.add(egui::Label::new(bold(text).size(11.5).color(color)).selectable(false));
    });
}

/// The status chip of the thing `id` names (a download): its colour cross-fades when the status
/// changes, and its tint gently breathes while `live` (downloading).
fn status_chip(ui: &mut Ui, id: egui::Id, text: &str, color: Color32, live: bool) {
    let color = anim::color(ui.ctx(), id.with("status chip"), color, 0.35);
    let glow = if live { anim::pulse(ui.ctx(), 2.4) } else { 0.0 };
    tinted_chip(ui, text, color, 0.15 + 0.1 * glow);
}

/// An on/off switch: the knob slides over and the track's colour follows; the knob swells a
/// little under the pointer.
fn toggle(ui: &mut Ui, on: &mut bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(38.0, 22.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, ""));
    let p = palette(ui);
    let t = anim::presence(ui.ctx(), response.id, *on, 0.18);
    let hover = anim::presence(ui.ctx(), response.id.with("hover"), response.hovered() && ui.is_enabled(), 0.12);
    ui.painter().rect_filled(rect, 11.0, p.dim.gamma_multiply(0.55).lerp_to_gamma(p.accent, t));
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    ui.painter().circle_filled(Pos2::new(x, rect.center().y), 7.5 + hover, Color32::WHITE);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A progress bar `size` big whose fill eases to `ratio` (keyed by `id`, the thing it measures)
/// with a light sweeping over it while `active` (bytes arriving), and `text`, when given, on it.
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
    if ratio > 0.0 {
        // The filled part keeps the bar's round ends at any length, and so does the light on it.
        let done = Rect::from_min_size(rect.min, Vec2::new(rect.width() * ratio, rect.height()));
        let half = anim::fill_half(rect, done.right());
        ui.painter().add(egui::Shape::convex_polygon(anim::outline(done, half), color, Stroke::NONE));
        if active {
            anim::sweep(ui, done, half);
        }
    }
    if let Some(text) = text {
        let galley = text.into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Button);
        let pos = rect.left_center() + Vec2::new(ui.spacing().item_spacing.x, -galley.size().y / 2.0);
        ui.painter().with_clip_rect(rect).galley(pos, galley, p.strong);
    }
    response
}

/// Grey bars where the lines of something still being read will show, one light sweeping over
/// them all: `rows` gives each line's bar as a share of the width, and the width of a second
/// one at its right end (0.0: none), as for a size.
fn skeleton(ui: &mut Ui, rows: &[(f32, f32)]) {
    let p = palette(ui);
    let line = ui.spacing().interact_size.y;
    let (area, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), rows.len() as f32 * line), Sense::hover());
    for (n, &(share, right)) in rows.iter().enumerate() {
        let row = Rect::from_min_size(area.min + Vec2::new(0.0, n as f32 * line), Vec2::new(area.width(), line));
        let bar = |left: f32, width: f32| Rect::from_min_size(Pos2::new(left, row.center().y - 5.0), Vec2::new(width, 10.0));
        for bar in [bar(row.left(), share * row.width()), bar(row.right() - right, right)].into_iter().filter(|it| it.width() > 0.0) {
            ui.painter().rect_filled(bar, 5.0, p.hover);
            // Every row's light is at the same place: one light, seen through each bar.
            let mut lit = ui.new_child(egui::UiBuilder::new().max_rect(row));
            lit.set_clip_rect(bar.intersect(ui.clip_rect()));
            anim::sheen(&lit, row);
        }
    }
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
    let p = palette(ui);
    let text = text.into();
    let t = anim::appear(ui.ctx(), id, 0.0);
    ui.add_space(8.0);
    anim::shifted(ui, anim::ease(t), anim::DROP, |ui| {
        Frame::none()
            .fill(p.card.lerp_to_gamma(color, 0.1))
            .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
            .inner_margin(Margin::symmetric(14.0, 9.0))
            .rounding(12.0)
            .shadow(card_shadow(ui, 0.0))
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

/// The word and colour of a download's status; its icon (see `queue::status_icon`) and the
/// words say it too, never the colour alone.
fn status_badge(app: &App, item: &QueueItem, p: &Palette) -> (&'static str, Color32) {
    match &item.status {
        QueueItemStatus::Queued => ("Waiting", p.muted),
        QueueItemStatus::Downloading if app.is_resolving(item.id) => ("Connecting", p.amber),
        QueueItemStatus::Downloading if item.is_finishing() => ("Finishing", p.cyan),
        QueueItemStatus::Downloading if app.stalled_for(item.id).is_some() => ("Stalled", p.amber),
        QueueItemStatus::Downloading => ("Downloading", p.link),
        QueueItemStatus::Pausing => ("Pausing", p.muted),
        QueueItemStatus::Paused => ("Paused", p.muted),
        QueueItemStatus::Completed => ("Done", p.green),
        QueueItemStatus::Failed(_) if item.link_expired() => ("Link expired", p.amber),
        QueueItemStatus::Failed(_) => ("Failed", p.red),
        QueueItemStatus::AuthRequired => ("Needs its header", p.amber),
    }
}

/// What ffmpeg is for, and what installing it takes.
const FFMPEG_ABOUT: &str = if cfg!(target_os = "macos") {
    "ffmpeg joins separate video and audio (the best quality) and makes MP3/M4A files. Without it videos download in a \
     lower quality and audio presets fail. Martin Riedl's static GPL build is checked and installed for your user only; \
     it takes about 150 MB on disk."
} else {
    "ffmpeg joins separate video and audio (the best quality) and makes MP3/M4A files. Without it \
     videos download in a lower quality and audio presets fail. The GPL build yt-dlp's makers publish is checked and \
     installed for your user only; it takes about 330 MB on disk."
};

/// `text` with its shortcuts named as on this system's keyboards: Ctrl+V, or ⌘V on macOS.
fn keys(text: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(target_os = "macos") {
        text.replace("Ctrl+", "⌘").into()
    } else {
        text.into()
    }
}

// ---- Frame ---------------------------------------------------------------------------------

/// The pages in the order of the rail, for Ctrl+1 to Ctrl+4 and for which way a page slides in.
const PAGES: [Tab; 4] = [Tab::Downloads, Tab::Library, Tab::Sources, Tab::Settings];

/// Whether the window is wide enough to show a download's details beside the list rather than
/// in its place: 1040 points left of the rail.
fn wide(ctx: &egui::Context) -> bool {
    ctx.screen_rect().width() - sidebar::width(ctx) >= 1040.0
}

/// Draws the window: the moving background under everything, the rail, the shown download's
/// details beside the list when there is room, and the page under the pulse bar. A new page
/// fades in sliding a little from the side it lies on in the rail; a theme switch cross-fades
/// the colors.
pub fn render(app: &mut App, ctx: &egui::Context) {
    ctx.set_theme(theme_preference(app.settings.theme));
    // Hidden in the tray nothing is seen, so nothing moves: on macOS and Linux a hidden window
    // still gets the frames motion asks for.
    anim::set_reduced(ctx, app.settings.reduces_motion() || app.hidden);
    anim::glide_scroll(ctx);
    follow_theme(ctx);
    let p = palette_of(ctx);
    backdrop::paint(ctx, &p, ctx.theme() == Theme::Dark, app.settings.moving_background, app.settings.background_intensity, mood(app));
    queue::observe(app, ctx);
    shortcuts(app, ctx);
    sidebar(app, ctx);
    let beside = wide(ctx) && app.tab == Tab::Downloads;
    if let Some(item) = app.focused_item().cloned().filter(|_| beside) {
        let frame = Frame::none().fill(p.card).inner_margin(Margin::symmetric(20.0, 18.0)).stroke(Stroke::new(1.0, p.border));
        egui::SidePanel::right("detail").resizable(false).exact_width(440.0).frame(frame).show(ctx, |ui| {
            egui::ScrollArea::vertical().id_salt("detail scroll").auto_shrink([false, false]).show(ui, |ui| {
                ui.set_max_width(ui.available_width() - 8.0);
                anim::fade_in(ui, egui::Id::new(("detail in", item.id)), 0.0, Vec2::new(16.0, 0.0), |ui| detail_page(app, ui, &item, false));
            });
        });
    }
    let frame = Frame::central_panel(&ctx.style()).fill(Color32::TRANSPARENT).inner_margin(Margin { left: 28.0, right: 24.0, top: 16.0, bottom: 12.0 });
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let in_place = app.tab == Tab::Downloads && !beside && app.focused.is_some();
        let page = (app.tab as u8, app.focused.filter(|_| in_place));
        let shown = anim::ease(anim::changed(ctx, egui::Id::new("page"), page, 0.28));
        let from = page_slide(ctx, app.tab, in_place);
        pulse_bar(app, ui);
        if app.closing {
            let text = "Finishing the live recordings; the window closes once they are saved. Close it again to quit now and cut them off.";
            banner(ui, p.amber, icon::WARNING, RichText::new(text).color(p.text), |_| {});
        }
        countdown_banner(app, ui);
        update_banner(app, ui);
        clipboard_banner(app, ui);
        listing_prompt(app, ui);
        channel_prompt(app, ui);
        ffmpeg_prompt(app, ui);
        notice_banner(app, ui);
        ffmpeg_notice(ui, media::installing_ffmpeg());
        ui.add_space(14.0);
        anim::shifted(ui, shown, from, |ui| match app.tab {
            Tab::Downloads => match app.focused_item().cloned().filter(|_| in_place) {
                Some(item) => {
                    egui::ScrollArea::vertical().id_salt("detail scroll").auto_shrink([false, false]).show(ui, |ui| {
                        ui.set_max_width(ui.available_width() - 12.0);
                        detail_page(app, ui, &item, true);
                    });
                }
                None => {
                    // A tall card under the bar (a stream's tracks, a ZIP's files) scrolls, leaving
                    // the list room when there is one.
                    let room = if app.queue.items().is_empty() { ui.available_height() } else { ui.available_height() * 0.62 };
                    let door = egui::ScrollArea::vertical().id_salt("front door").max_height(room).auto_shrink([false, true]);
                    door.show(ui, |ui| {
                        ui.set_max_width(ui.available_width() - 12.0);
                        front_door(app, ui);
                    });
                    queue_page(app, ui);
                }
            },
            // Its list scrolls by itself, showing only the rows in view.
            Tab::Library => library_page(app, ui),
            tab => {
                page_title(tab, ui);
                egui::ScrollArea::vertical().id_salt(("page scroll", tab as u8)).auto_shrink([false, false]).show(ui, |ui| {
                    // Room for the scroll bar.
                    ui.set_max_width(ui.available_width() - 12.0);
                    if tab == Tab::Sources {
                        sources_page(app, ui);
                    } else {
                        settings_page(app, ui);
                    }
                });
            }
        });
    });
    undo_toast(app, ctx);
    shortcuts_window(ctx);
    command::palette(app, ctx);
    focus_ring(ctx);
}

/// What the moving background reacts to: the running downloads' speed together, and how many
/// have completed and failed.
fn mood(app: &App) -> backdrop::Mood {
    let items = app.queue.items();
    backdrop::Mood {
        speed: items.iter().filter(|item| item.status == QueueItemStatus::Downloading).map(|item| item.speed_bytes_per_sec.max(0.0)).sum(),
        completed: items.iter().filter(|item| item.status == QueueItemStatus::Completed).count(),
        failed: items.iter().filter(|item| matches!(item.status, QueueItemStatus::Failed(_))).count(),
    }
}

/// Where a page slides in from as it fades in: from the right when it lies further down the rail
/// than the page before it (or opens a download's details), from the left when higher up.
fn page_slide(ctx: &egui::Context, tab: Tab, detail: bool) -> Vec2 {
    let place = PAGES.iter().position(|&page| page == tab).unwrap_or(0) as f32 * 2.0 + f32::from(u8::from(detail));
    let id = egui::Id::new("page place");
    let (before, now) = ctx.data_mut(|d| {
        let seen = d.get_temp_mut_or_insert_with(id, || (place, place));
        if seen.1 != place {
            *seen = (seen.1, place);
        }
        *seen
    });
    if now >= before { Vec2::new(18.0, 4.0) } else { Vec2::new(-18.0, 4.0) }
}

/// The pages' titles over their contents: what the page is for, in a line.
fn page_title(tab: Tab, ui: &mut Ui) {
    let p = palette(ui);
    let (title, about) = match tab {
        Tab::Downloads => ("Downloads", ""),
        Tab::Library => ("Library", "Everything downloaded, with its links and checksums: search it, check a file, get it again."),
        Tab::Sources => ("Sources", "What the app downloads from. Try an example, or set up an account or a helper tool."),
        Tab::Settings => ("Settings", "Saved when the app closes; downloads started or queued afterwards use them."),
    };
    ui.label(RichText::new(title).heading().color(p.strong));
    if !about.is_empty() {
        ui.label(RichText::new(about).color(p.muted));
    }
    ui.add_space(12.0);
}

/// The speed of every download together over the last minute, a value a second (oldest first).
fn total_speed_history(app: &App) -> [f64; 60] {
    // Counted back from the newest sample: the graph moves when a reading comes, never between.
    let newest = app.jobs.values().filter_map(|view| view.speed_history.back().map(|(at, _)| *at)).max();
    let now = newest.unwrap_or_else(Instant::now);
    let mut buckets = [0.0f64; 60];
    for view in app.jobs.values() {
        let (mut sums, mut seen) = ([0.0f64; 60], [0u32; 60]);
        for (at, speed) in &view.speed_history {
            let age = now.saturating_duration_since(*at).as_secs() as usize;
            if age < 60 {
                sums[59 - age] += speed;
                seen[59 - age] += 1;
            }
        }
        for n in 0..60 {
            if seen[n] > 0 {
                buckets[n] += sums[n] / seen[n] as f64;
            }
        }
    }
    buckets
}

/// The pulse bar over every page: the speed of everything together as one big rolling number
/// with its last minute under it, how many downloads run, wait and are done, Pause all or Resume
/// all, the speed limit's switch, and the command palette.
fn pulse_bar(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let ctx = ui.ctx().clone();
    let items = app.queue.items();
    let running = items.iter().filter(|item| item.status == QueueItemStatus::Downloading).count();
    let waiting = items.iter().filter(|item| matches!(item.status, QueueItemStatus::Queued | QueueItemStatus::Paused)).count();
    let done = items.iter().filter(|item| item.status == QueueItemStatus::Completed).count();
    let speed: f64 = items.iter().filter(|item| item.status == QueueItemStatus::Downloading).map(|item| item.speed_bytes_per_sec.max(0.0)).sum();
    let paused = items.iter().any(|item| item.status == QueueItemStatus::Paused);
    let shown = anim::roll(&ctx, egui::Id::new("total speed"), speed as f32).max(0.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::hover());
    let mut bar = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
    let ui = &mut bar;
    ui.spacing_mut().item_spacing.x = 10.0;
    // The number: arrow, speed, unit; idle, a quiet word.
    let (number, unit) = if running > 0 {
        let text = format_bytes(shown as u64);
        match text.rsplit_once(' ') {
            Some((n, unit)) => (n.to_string(), format!("{unit}/s")),
            None => (text, "/s".to_string()),
        }
    } else {
        ("Idle".to_string(), String::new())
    };
    let arrow = if running > 0 { p.link } else { p.dim };
    anim::icon(ui, icon::ARROW_DOWN, 20.0, arrow, 1.0, 0.0);
    ui.add(egui::Label::new(RichText::new(number).font(FontId::new(26.0, semibold())).color(if running > 0 { p.strong } else { p.muted })).selectable(false));
    if !unit.is_empty() {
        ui.add(egui::Label::new(RichText::new(unit).color(p.muted)).selectable(false));
    }
    spark(ui, &total_speed_history(app), running > 0);
    ui.add_space(8.0);
    for (count, word, filter) in [(running, "running", 1usize), (waiting, "waiting", 2), (done, "done", 4)] {
        let text = RichText::new(format!("{count} {word}")).color(if count > 0 { p.text } else { p.dim });
        let tip = format!("Show the {word} downloads");
        if ui.add(egui::Button::new(text).frame(false)).on_hover_text(tip).clicked() {
            app.tab = Tab::Downloads;
            app.focused = None;
            app.queue_filter = filter;
        }
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let search = button(RichText::new(format!("{}  {}", icon::MAGNIFYING_GLASS, keys("Ctrl+K"))).color(p.muted));
        if ui.add(search).on_hover_text("Find an action, a setting or a download").clicked() {
            command::open(ui.ctx());
        }
        limit_switch(app, ui);
        if running > 0 {
            if ui.add(button(format!("{} Pause all", icon::PAUSE))).on_hover_text("Pause every running download and stop the queue").clicked() {
                app.pause_all();
            }
        } else if paused || (waiting > 0 && !app.settings.auto_run_queue) {
            if ui.add(button(format!("{} Resume all", icon::PLAY))).on_hover_text("Start the queue and resume every paused download").clicked() {
                app.resume_all();
            }
        }
    });
}

/// The last minute of `speeds` as a small filled line, 96 points wide; its newest point lit
/// while `live`. It keeps still: it is over every page, where a running download asks for no
/// frames.
fn spark(ui: &mut Ui, speeds: &[f64; 60], live: bool) {
    let p = palette(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(96.0, 28.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let peak = speeds.iter().copied().fold(0.0, f64::max).max(1.0);
    let points: Vec<Pos2> = speeds
        .iter()
        .enumerate()
        .map(|(n, speed)| Pos2::new(rect.left() + rect.width() * n as f32 / 59.0, rect.bottom() - 1.0 - (*speed / peak) as f32 * (rect.height() - 4.0)))
        .collect();
    let mut mesh = egui::Mesh::default();
    for point in &points {
        mesh.colored_vertex(*point, p.link.gamma_multiply(0.22));
        mesh.colored_vertex(Pos2::new(point.x, rect.bottom()), p.link.gamma_multiply(0.0));
    }
    for i in 0..(points.len() as u32 - 1) {
        let n = 2 * i;
        mesh.add_triangle(n, n + 1, n + 3);
        mesh.add_triangle(n, n + 3, n + 2);
    }
    ui.painter().add(egui::Shape::mesh(mesh));
    let line = if live { p.link } else { p.dim.gamma_multiply(0.6) };
    ui.painter().add(egui::Shape::line(points.clone(), Stroke::new(1.5, line)));
    if live {
        if let Some(&last) = points.last() {
            ui.painter().circle_filled(last, 4.0, p.link.gamma_multiply(0.3));
            ui.painter().circle_filled(last, 2.0, p.link);
        }
    }
}

/// The speed limit's switch in the pulse bar, when a limit is set: on, new and resumed downloads
/// keep to it; off, they run free, the limit kept for later. Running downloads keep the limit
/// they started with until "Apply now" restarts them with the new one. Without a limit, a quiet
/// way to set one.
fn limit_switch(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let s = &app.settings;
    let Some(limit) = crate::settings::speed_limit_bytes(s.max_speed, s.max_speed_in_mb) else {
        if ui.add(egui::Button::new(RichText::new(format!("{} No speed limit", icon::GAUGE)).color(p.dim)).frame(false)).on_hover_text("Set one in Settings").clicked() {
            app.tab = Tab::Settings;
            app.settings_search = "Speed limit".to_string();
        }
        return;
    };
    let wanted = s.speed_limit_on.then_some(limit);
    let stale: Vec<usize> = app
        .queue
        .items()
        .iter()
        .filter(|item| item.status == QueueItemStatus::Downloading && item.options.max_speed != wanted)
        .map(|item| item.id)
        .collect();
    if !stale.is_empty() && ui.add(button("Apply now")).on_hover_text("Restart the running downloads with this limit; they go on from where they are").clicked() {
        for id in stale {
            app.restart_with(id, move |options| options.max_speed = wanted);
        }
    }
    let mut on = app.settings.speed_limit_on;
    let text = format!("Limit {}/s", format_bytes(limit));
    let tip = "New and resumed downloads keep to the limit while it is on";
    if toggle(ui, &mut on).on_hover_text(tip).changed() {
        app.settings.speed_limit_on = on;
    }
    ui.add(egui::Label::new(RichText::new(text).color(if on { p.text } else { p.dim })).selectable(false));
}

/// Where it is kept whether the list of keyboard shortcuts shows.
fn shortcuts_id() -> egui::Id {
    egui::Id::new("shortcuts shown")
}

/// The list of keyboard shortcuts, in the middle of the window: F1 or Shortcuts in the rail
/// opens and closes it, Escape closes it.
fn shortcuts_window(ctx: &egui::Context) {
    let mut open = ctx.data(|d| d.get_temp::<bool>(shortcuts_id())).unwrap_or(false);
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F1)) {
        open = !open;
    }
    if open && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        open = false;
    }
    if open {
        let p = palette_of(ctx);
        let window = egui::Window::new("Keyboard shortcuts").open(&mut open).collapsible(false).resizable(false);
        window.anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            egui::Grid::new("shortcuts").num_columns(2).spacing(Vec2::new(24.0, 8.0)).show(ui, |ui| {
                for (key, what) in [
                    ("Ctrl+1 to Ctrl+4", "Downloads, Library, Sources, Settings"),
                    ("Ctrl+K", "Find an action, a setting or a download"),
                    ("Ctrl+V", "Add the copied link or links to the queue (outside a text box)"),
                    ("Enter", "In the link bar: start the download. On a download: show its details"),
                    ("Up, Down", "Move between downloads"),
                    ("Space", "Pause or resume the selected download"),
                    ("Delete", "Remove the selected download (its files are kept; Undo brings it back)"),
                    ("Escape", "Close the details, the palette or this list"),
                    ("Tab", "Move between buttons and fields"),
                    ("Drag", "Move a download up or down the list"),
                    ("F1", "This list"),
                ] {
                    ui.label(bold(keys(key)).color(p.strong));
                    ui.label(RichText::new(what).color(p.muted));
                    ui.end_row();
                }
            });
        });
    }
    ctx.data_mut(|d| d.insert_temp(shortcuts_id(), open));
}

/// The seconds left before the computer sleeps or shuts down, the queue being done, with Cancel.
fn countdown_banner(app: &mut App, ui: &mut Ui) {
    let Some((action, at)) = app.countdown else { return };
    let p = palette(ui);
    let left = at.saturating_duration_since(Instant::now()).as_secs_f32().ceil();
    let doing = if action == WhenDone::Sleep { "Going to sleep" } else { "Shutting down" };
    let text = format!("The queue is done. {} in {} s.", doing, left);
    keyed_banner(ui, egui::Id::new("countdown"), p.amber, icon::POWER, RichText::new(text).color(p.text), |ui| {
        if ui.add(button("Cancel")).clicked() {
            app.countdown = None;
        }
    });
}

/// Whether the keyboard is driving (Tab or an arrow was pressed since the last click): the
/// focus ring shows only then.
fn keyboard_driving(ctx: &egui::Context) -> bool {
    let id = egui::Id::new("keyboard driving");
    let (keys, click) = ctx.input(|i| {
        let keys = i.events.iter().any(|e| {
            matches!(e, egui::Event::Key { key: egui::Key::Tab | egui::Key::ArrowUp | egui::Key::ArrowDown | egui::Key::ArrowLeft | egui::Key::ArrowRight, pressed: true, .. })
        });
        (keys, i.pointer.any_pressed())
    });
    ctx.data_mut(|d| {
        let driving = d.get_temp_mut_or_default::<bool>(id);
        if click {
            *driving = false;
        } else if keys {
            *driving = true;
        }
        *driving
    })
}

/// Where the focus ring goes when no widget has the keyboard's focus: the selected download's
/// card, which the list notes here as it draws it.
fn ring_target_id() -> egui::Id {
    egui::Id::new("focus ring target")
}

/// A soft ring in the accent around what the keyboard is on, gliding from one place to the next:
/// the focused button or switch, else the selected download's card. Only while the keyboard
/// drives; a text box shows its own caret.
fn focus_ring(ctx: &egui::Context) {
    let target = ctx.data_mut(|d| {
        let target = d.get_temp::<Rect>(ring_target_id());
        d.remove::<Rect>(ring_target_id());
        target
    });
    if !keyboard_driving(ctx) || ctx.wants_keyboard_input() {
        return;
    }
    let focused = ctx.memory(|m| m.focused()).and_then(|id| ctx.read_response(id)).map(|response| response.rect.expand(3.0));
    let Some(rect) = focused.or(target) else { return };
    let p = palette_of(ctx);
    let shown = anim::glide_rect(ctx, egui::Id::new("focus ring"), rect);
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("focus ring")));
    painter.rect_stroke(shown.expand(2.0), 12.0, Stroke::new(4.0, p.link.gamma_multiply(0.18)));
    painter.rect_stroke(shown, 10.0, Stroke::new(2.0, p.link));
}

/// Ctrl+1 to Ctrl+4 (⌘ on macOS) switch pages and Ctrl+K opens the palette; outside a text box,
/// Ctrl+V adds the copied link(s) to the queue, Up and Down move between the downloads, Enter
/// shows the selected one, Space pauses or resumes it, Delete (or a Mac's delete key, Backspace)
/// removes it, and Escape closes its details.
fn shortcuts(app: &mut App, ctx: &egui::Context) {
    let typing = ctx.wants_keyboard_input();
    // A button or switch with the focus takes Space and Enter itself.
    let free = !typing && ctx.memory(|m| m.focused()).is_none() && !command::is_open(ctx);
    let (page, palette, pasted, delete, step, enter, space, escape) = ctx.input_mut(|i| {
        let keys = [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4];
        let page = PAGES.into_iter().zip(keys).find(|&(_, key)| i.consume_key(egui::Modifiers::COMMAND, key)).map(|(tab, _)| tab);
        let palette = i.consume_key(egui::Modifiers::COMMAND, egui::Key::K);
        let pasted = i.events.iter().find_map(|e| match e {
            egui::Event::Paste(text) if !typing => Some(text.clone()),
            _ => None,
        });
        let delete = !typing
            && (i.consume_key(egui::Modifiers::NONE, egui::Key::Delete)
                || (cfg!(target_os = "macos") && i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace)));
        let step = if !free {
            0
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
            1
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
            -1
        } else {
            0
        };
        let enter = free && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
        let space = free && i.consume_key(egui::Modifiers::NONE, egui::Key::Space);
        let escape = free && i.key_pressed(egui::Key::Escape);
        (page, palette, pasted, delete, step, enter, space, escape)
    });
    if let Some(tab) = page {
        go(app, tab);
    }
    if palette {
        command::open(ctx);
    }
    if let Some(text) = pasted {
        add_link(app, &text);
    }
    if app.tab != Tab::Downloads {
        return;
    }
    if step != 0 {
        let shown = queue::visible_ids(app);
        let at = app.selected.and_then(|id| shown.iter().position(|&other| other == id));
        let next = match at {
            Some(at) => (at as isize + step).clamp(0, shown.len() as isize - 1) as usize,
            None => 0,
        };
        app.selected = shown.get(next).copied().or(app.selected);
    }
    if let Some(id) = app.selected.filter(|_| enter) {
        app.show_job(id);
    }
    if let Some(item) = app.selected.and_then(|id| app.queue.get_item(id)).filter(|_| space).cloned() {
        match item.status {
            QueueItemStatus::Downloading => app.pause_job(item.id),
            QueueItemStatus::Queued | QueueItemStatus::Paused | QueueItemStatus::Failed(_) => {
                app.start_job(item.id, false);
            }
            _ => {}
        }
    }
    if escape && app.focused.is_some() {
        app.focused = None;
    }
    if let Some(id) = app.selected.filter(|_| delete) {
        if app.queue.get_item(id).is_some_and(|item| item.status.is_active()) {
            app.notice = Some(Err(format!("Download #{} is running: pause it before removing it", id)));
        } else if app.remove_job(id) {
            app.selected = None;
            if app.focused == Some(id) {
                app.focused = None;
            }
        }
    }
}

fn go(app: &mut App, tab: Tab) {
    if tab == Tab::Library {
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

    /// The pages by their names in the rail.
    const RAIL: [(&str, Tab); 4] = [("Library", Tab::Library), ("Sources", Tab::Sources), ("Settings", Tab::Settings), ("Downloads", Tab::Downloads)];

    /// A frame at the next vsync: continuous motion.
    const SOON: Duration = Duration::from_millis(16);

    /// Idle means idle: with nothing running, the pointer gone and the transitions over, no page
    /// (reached through the rail) asks for another frame, in either theme; under Reduce motion
    /// not even just after a page change.
    #[test]
    fn idle_pages_ask_for_no_frames() {
        let mut h = Harness::new();
        h.add("queued.iso");
        h.app.notice = Some(Ok("Added #1 to the queue".to_string()));
        assert!(h.settle());
        for theme in [1, 2] {
            h.app.settings.theme = theme;
            for (label, tab) in RAIL {
                h.click(label);
                h.wait_for_background();
                assert_eq!(h.app.tab, tab);
                assert!(h.leave() < Duration::MAX, "{label} fades in");
                assert_eq!(h.frames(40), Duration::MAX, "{label} in theme {theme}");
            }
        }

        h.app.settings.reduce_motion = Some(true);
        for (label, _) in RAIL {
            h.click(label);
            h.wait_for_background();
            h.leave();
            assert_eq!(h.frames(2), Duration::MAX, "{label} under Reduce motion");
        }
    }

    /// A running download keeps frames coming while it shows, in the list or in its details;
    /// none while another page shows or the window is minimized or hidden in the tray, and none
    /// once it is done.
    #[test]
    fn a_running_download_asks_for_frames_only_while_it_shows() {
        let mut h = Harness::new();
        let id = h.add("big.iso");
        h.progress(id, 0.4);
        h.leave();
        assert!(h.frames(40) <= SOON, "its card moves");
        h.minimized = true;
        assert_eq!(h.frames(40), Duration::MAX, "minimized");
        h.minimized = false;
        assert!(h.frames(2) <= SOON, "restored");
        // A window hidden in the tray keeps its size and is not minimized on macOS and Linux.
        h.app.hide_window();
        assert_eq!(h.frames(40), Duration::MAX, "hidden");
        h.app.show_window();
        assert!(h.frames(2) <= SOON, "shown");
        h.click("Settings");
        h.leave();
        assert_eq!(h.frames(40), Duration::MAX, "out of sight");
        h.click("Downloads");
        h.double_click("big.iso");
        h.leave();
        assert!(h.app.tab == Tab::Downloads && h.app.focused == Some(id), "a double click shows it");
        assert!(h.frames(40) <= SOON, "its details move");
        h.minimized = true;
        assert_eq!(h.frames(40), Duration::MAX, "its details, minimized");
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
            h.app.tab = Tab::Settings;
            assert!(h.settle());
            h.click("Downloads");
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
        h.add("a.iso");
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

    /// The rail, the link bar, a card's buttons, the Settings switches and the clipboard switch,
    /// driven as a user would, do what they did before the redesign.
    #[test]
    fn pages_forms_rows_and_switches_work_through_input() {
        let mut h = Harness::new();
        assert!(h.settle());
        for (label, tab) in RAIL {
            h.click(label);
            assert_eq!(h.app.tab, tab, "{label}");
        }

        // The link bar: typing a link, More options, Ctrl+V outside the bar.
        let input = h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect;
        h.click_at(input.center());
        h.type_text("https://example.com/a.iso");
        assert_eq!(h.app.url_input, "https://example.com/a.iso");
        h.key(Key::Escape, Modifiers::NONE);
        assert!(h.settle() && h.find("A file").is_some(), "it says what it is: {:?}", h.texts());
        h.click("More options");
        assert!(h.app.show_advanced);
        assert!(h.settle() && h.find("Checksum").is_some());
        h.click("More options");
        assert!(!h.app.show_advanced);
        assert!(h.settle() && h.find("Checksum").is_none());
        h.app.url_input.clear();
        h.paste("https://example.com/b.iso");
        assert_eq!(h.app.queue.items().iter().map(|item| item.filename.as_str()).collect::<Vec<_>>(), ["b.iso"]);
        assert!(h.settle());
        h.click_at(h.find_near("Added #1 to the queue", icon::X).unwrap().center());
        assert_eq!(h.app.notice, None);

        // Cards: a click selects, Delete removes, X removes, Pause pauses, a double click shows,
        // Clear all clears. Clicks are kept apart so as not to make double clicks.
        let [c, d, e] = ["c.iso", "d.iso", "e.iso"].map(|name| h.add(name));
        h.progress(e, 0.5);
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
        assert!(h.app.tab == Tab::Downloads && h.app.focused == Some(d));
        h.click("Downloads");
        h.frames(30);
        assert_eq!(h.app.focused, None, "the rail's Downloads closes the details");
        queue::from_toolbar_menu(&mut h, "Clear all");
        assert_eq!(h.app.queue.items().iter().map(|item| item.id).collect::<Vec<_>>(), [e], "the running one stays");

        // Settings: a switch, the theme, Reduce motion; the rail's clipboard switch.
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
            for tab in PAGES {
                h.app.tab = tab;
                h.frames(2);
                assert_eq!(h.app.ctx.style().visuals.dark_mode, dark);
            }
            h.app.tab = Tab::Downloads;
            for shown in [running, done, failed] {
                h.app.focused = Some(shown);
                h.frames(1);
            }
            h.app.focused = None;
        }
        assert_eq!(h.app.queue.items().len(), 4, "showing changes nothing");
    }

    /// A download checked against its site's checksum says so: on its card and in its details.
    /// One without says nothing of it.
    #[test]
    fn a_site_checked_download_says_so() {
        let mut h = Harness::new();
        let done = h.add("site.iso");
        h.app.queue.mark_started(done);
        let checked = EngineSnapshot { checked_against: Some("the site's SHA-256".into()), ..Default::default() };
        h.app.queue.apply_snapshot(done, &crate::with_check_note(checked));
        h.finish(done, None);
        let plain = h.add("plain.iso");
        h.app.queue.mark_started(plain);
        h.app.queue.apply_snapshot(plain, &crate::with_check_note(EngineSnapshot::default()));
        h.finish(plain, None);
        assert!(h.app.queue.get_item(plain).unwrap().notes.is_empty());

        assert!(h.settle());
        let row = h.texts().into_iter().find(|(text, _)| text.contains("Checked against the site's SHA-256")).map(|(_, rect)| rect);
        let name = h.find("site.iso").unwrap();
        assert!(row.is_some_and(|row| row.top() > name.top() && row.top() - name.top() < 60.0), "under its name: {:?}", h.texts());
        h.double_click("site.iso");
        assert!(h.settle());
        assert!(h.texts().iter().filter(|(text, _)| text.contains("Checked against the site's SHA-256")).count() >= 1, "{:?}", h.texts());
    }

    /// A download removed with Delete comes back in its place with Undo; so do those Clear all took.
    #[test]
    fn removed_downloads_come_back_with_undo() {
        let ids = |h: &Harness| h.app.queue.items().iter().map(|item| item.id).collect::<Vec<_>>();
        let mut h = Harness::new();
        let (a, b, c) = (h.add("a.iso"), h.add("b.iso"), h.add("c.iso"));
        h.frames(30);
        assert!(h.find("Undo").is_none());
        h.app.selected = Some(b);
        h.key(Key::Delete, Modifiers::NONE);
        assert_eq!(ids(&h), [a, c]);
        h.frames(30);
        assert!(h.find("Removed b.iso").is_some(), "{:?}", h.texts());
        h.click("Undo");
        h.frames(30);
        assert_eq!(ids(&h), [a, b, c]);
        assert!(h.find("Undo").is_none());

        h.app.clear_queue(false);
        assert!(ids(&h).is_empty());
        h.frames(30);
        assert!(h.find("Removed 3 downloads").is_some(), "{:?}", h.texts());
        h.app.undo_remove();
        assert_eq!(ids(&h), [a, b, c]);
        // Left alone, the offer goes by itself.
        h.app.clear_queue(false);
        assert!(h.settle() && h.app.removed.is_none() && h.find("Undo").is_none());
    }

    /// F1 opens the list of shortcuts and Escape closes it; so does the rail's Shortcuts.
    #[test]
    fn the_shortcuts_list_opens_and_closes() {
        let mut h = Harness::new();
        assert!(h.settle() && h.find("Keyboard shortcuts").is_none());
        h.key(Key::F1, Modifiers::NONE);
        h.frames(20);
        assert!(h.find("Keyboard shortcuts").is_some() && h.find("This list").is_some(), "{:?}", h.texts());
        h.key(Key::Escape, Modifiers::NONE);
        h.frames(20);
        assert!(h.find("Keyboard shortcuts").is_none());
        h.click("Shortcuts");
        h.frames(20);
        assert!(h.find("Keyboard shortcuts").is_some());
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
        assert!(h.app.tab == Tab::Library);
        h.key(Key::Num1, Modifiers::COMMAND);
        assert!(h.app.tab == Tab::Downloads);
        let (first, second) = (h.app.queue.items()[0].id, h.app.queue.items()[1].id);
        h.app.queue.mark_started(second);
        h.app.selected = Some(second);
        h.key(Key::Delete, Modifiers::NONE);
        assert_eq!(h.app.queue.items().len(), 2, "a running download stays");
        h.app.selected = Some(first);
        h.key(Key::Delete, Modifiers::NONE);
        assert_eq!(h.app.queue.items().iter().map(|item| item.id).collect::<Vec<_>>(), [second]);
        assert_eq!(h.app.selected, None);

        h.key(Key::Num3, Modifiers::COMMAND);
        assert!(h.app.tab == Tab::Sources);
        h.key(Key::Num4, Modifiers::COMMAND);
        assert!(h.app.tab == Tab::Settings);
    }

    /// Up and Down move the selection between the downloads, Enter shows the selected one,
    /// Escape closes it, and Space pauses and resumes it; meanwhile the focus ring glides to the
    /// selected card and then keeps still.
    #[test]
    fn the_keyboard_moves_between_downloads() {
        let mut h = Harness::new();
        h.app.engines = std::sync::Arc::new(|_, _| Err("not downloaded here".to_string()));
        let [a, b, c] = ["a.iso", "b.iso", "c.iso"].map(|name| h.add(name));
        assert!(h.settle());
        h.key(Key::ArrowDown, Modifiers::NONE);
        assert_eq!(h.app.selected, Some(a));
        h.key(Key::ArrowDown, Modifiers::NONE);
        h.key(Key::ArrowDown, Modifiers::NONE);
        h.key(Key::ArrowDown, Modifiers::NONE);
        assert_eq!(h.app.selected, Some(c), "stops at the last");
        h.key(Key::ArrowUp, Modifiers::NONE);
        assert_eq!(h.app.selected, Some(b));
        h.leave();
        assert!(h.settle(), "the ring glides, then keeps still");
        h.key(Key::Enter, Modifiers::NONE);
        assert_eq!(h.app.focused, Some(b));
        h.frames(20);
        h.key(Key::Escape, Modifiers::NONE);
        assert_eq!(h.app.focused, None);
        h.progress(b, 0.5);
        h.key(Key::Space, Modifiers::NONE);
        assert_eq!(h.app.queue.get_item(b).unwrap().status, QueueItemStatus::Pausing);
    }
}
