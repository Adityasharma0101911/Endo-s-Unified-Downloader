//! The Sources page: what the app downloads from, made from [`kinds::KINDS`]: each kind with what
//! it does, what it needs (your own account, a helper tool) and an example to try; and the helper
//! tools with whether they are there yet. A new source is a new row of `KINDS`, not a new screen.

use super::*;
use kinds::{Kind, Needs, GROUPS, KINDS};

/// Height of a kind's tile: all the same, so that the grid lines up.
const TILE: f32 = 128.0;

/// The helper tools the app uses, what for, and where it found each (None: it fetches it the
/// first time it is needed, or it is not there).
type Tools = Vec<(&'static str, &'static str, Option<std::path::PathBuf>)>;

/// The helper tools as found when the page opened: looked for again each time it opens.
fn tools(ctx: &egui::Context) -> Tools {
    let id = egui::Id::new("source tools");
    let visit = ctx.cumulative_pass_nr();
    // Kept while the page shows from one frame to the next; a gap means it opened again.
    if let Some((last, tools)) = ctx.data(|d| d.get_temp::<(u64, Tools)>(id)) {
        if last + 1 >= visit {
            ctx.data_mut(|d| d.insert_temp(id, (visit, tools.clone())));
            return tools;
        }
    }
    let tools: Tools = vec![
        ("yt-dlp", "Videos, playlists and channels from video sites", media::find_ytdlp_path()),
        ("ffmpeg", "Joins a video's picture and sound, converts streams", media::find_ffmpeg_path()),
        ("gallery-dl", "Image galleries, picture by picture", hyperfetch_core::gallery::find_gallery_dl()),
    ];
    ctx.data_mut(|d| d.insert_temp(id, (visit, tools.clone())));
    tools
}

pub(super) fn sources_page(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    tools_card(app, ui);
    for (n, group) in GROUPS.into_iter().enumerate() {
        let mut kinds: Vec<&'static Kind> = KINDS.iter().filter(|kind| kind.group == group).collect();
        // The ones there is something to try with first.
        kinds.sort_by_key(|kind| kind.example.is_empty());
        if kinds.is_empty() {
            continue;
        }
        anim::fade_in(ui, egui::Id::new(("sources group", n)), anim::stagger(n + 1), anim::RISE, |ui| {
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.label(bold(group).size(15.0).color(p.strong));
                ui.label(RichText::new(format!("{}", kinds.len())).small().color(p.dim));
            });
            ui.add_space(6.0);
            let columns = ((ui.available_width() + 10.0) / 290.0).floor().clamp(1.0, 4.0) as usize;
            for row in kinds.chunks(columns) {
                ui.columns(columns, |cols| {
                    for (col, kind) in cols.iter_mut().zip(row) {
                        tile(app, col, kind);
                    }
                });
                ui.add_space(8.0);
            }
        });
    }
}

/// The helper tools: each with what it is for and whether it is there, by its icon and in words.
fn tools_card(app: &mut App, ui: &mut Ui) {
    let p = palette(ui);
    let found = tools(ui.ctx());
    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::WRENCH).size(18.0).color(p.link));
            ui.label(bold("Helper tools").size(15.0).color(p.strong));
            ui.label(RichText::new("open-source programs some sources need, fetched from their official releases").small().color(p.muted));
        });
        ui.add_space(6.0);
        for (name, about, path) in &found {
            ui.horizontal(|ui| {
                let (glyph, color, state) = match path {
                    Some(_) => (icon::CHECK_CIRCLE, p.green, "Ready"),
                    // ffmpeg is installed only once you say so (the switch below, or the question).
                    None if *name == "ffmpeg" && app.settings.install_ffmpeg != Some(true) => (icon::CLOUD_ARROW_DOWN, p.muted, "Asked for before the first video that needs it"),
                    None => (icon::CLOUD_ARROW_DOWN, p.muted, "Fetched the first time it is needed"),
                };
                ui.add_sized([20.0, 22.0], egui::Label::new(RichText::new(glyph).color(color)));
                ui.allocate_ui_with_layout(Vec2::new(96.0, 22.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_width(96.0);
                    ui.label(bold(*name).color(p.text));
                });
                left_part(ui, 260.0, |ui| {
                    ui.add(egui::Label::new(RichText::new(*about).color(p.muted)).truncate());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = ui.label(RichText::new(state).small().color(color));
                    if let Some(path) = path {
                        label.on_hover_text(path.display().to_string());
                    }
                });
            });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let mut install = app.settings.install_ffmpeg == Some(true);
            if toggle(ui, &mut install).on_hover_text(FFMPEG_ABOUT).changed() {
                app.answer_ffmpeg(install);
            }
            ui.label(RichText::new("Install ffmpeg when a video needs it").color(p.text)).on_hover_text(FFMPEG_ABOUT);
        });
    });
}

/// One kind of source: its icon, name and what the app does with it; what it needs, with Set up
/// for an account (Settings, searched for it); and Try it, which puts its example in the link bar.
fn tile(app: &mut App, ui: &mut Ui, kind: &'static Kind) {
    let p = palette(ui);
    let id = egui::Id::new(("source tile", kind.name));
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), TILE), Sense::hover());
    let response = ui.interact(rect, id, Sense::hover());
    let hover = anim::presence(ui.ctx(), id.with("hover"), response.hovered(), 0.15);
    let lifted = rect.translate(Vec2::new(0.0, -1.5 * hover));
    ui.painter().add(card_shadow(ui, hover).as_shape(lifted, 14.0));
    ui.painter().rect(lifted, 14.0, p.card.lerp_to_gamma(p.hover, 0.2 * hover), Stroke::new(1.0, p.border.lerp_to_gamma(p.dim, 0.5 * hover)));
    let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(lifted.shrink(14.0)).layout(Layout::top_down(Align::LEFT)));
    ui.horizontal(|ui| {
        let (badge, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
        ui.painter().circle_filled(badge.center(), 15.0, p.accent.gamma_multiply(0.16));
        anim::paint_icon(ui.painter(), badge.center(), kind.glyph, 16.0, p.link, 0.0);
        ui.add(egui::Label::new(bold(kind.name).color(p.strong)).truncate());
    });
    ui.add_space(4.0);
    // Two lines at most, ending in "…": the rest on hover.
    let mut about = egui::text::LayoutJob::single_section(kind.about.to_string(), egui::TextFormat::simple(egui::TextStyle::Small.resolve(ui.style()), p.muted));
    about.wrap.max_rows = 2;
    ui.add(egui::Label::new(about).wrap()).on_hover_text(kind.about);
    let bottom = Rect::from_min_max(Pos2::new(lifted.left() + 14.0, lifted.bottom() - 40.0), lifted.max - Vec2::splat(12.0));
    let mut row = ui.new_child(egui::UiBuilder::new().max_rect(bottom).layout(Layout::left_to_right(Align::Center)));
    match kind.needs {
        Needs::Account(what) => {
            row.label(RichText::new(format!("{} Your account", icon::KEY)).small().color(p.amber)).on_hover_text(format!("Uses your own {what} account"));
            if row.add(egui::Button::new(RichText::new("Set up").small().color(p.link)).frame(false)).clicked() {
                app.tab = Tab::Settings;
                app.settings_search = what.to_string();
            }
        }
        Needs::Tool(tool) => {
            row.label(RichText::new(format!("{} {tool}", icon::WRENCH)).small().color(p.dim)).on_hover_text(format!("Uses {tool}, fetched the first time it is needed"));
        }
        Needs::Nothing => {}
    }
    if !kind.example.is_empty() {
        row.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add(button(RichText::new("Try it").color(p.text))).on_hover_text(kind.example).clicked() {
                app.url_input = kind.example.to_string();
                app.form_error = None;
                app.tab = Tab::Downloads;
                app.focused = None;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness::Harness;
    use super::*;

    /// Every kind is on the page in its group; Try it puts a kind's example in the link bar on
    /// the Downloads page, which then says what it is; Set up opens Settings searched for the
    /// account. Once in, the page asks for no frames.
    #[test]
    fn every_kind_is_listed_and_its_example_is_tried() {
        let mut h = Harness::new();
        h.app.tab = Tab::Sources;
        assert!(h.settle(), "keeps still");
        assert!(h.find("Helper tools").is_some() && h.find("yt-dlp").is_some(), "{:?}", h.texts());
        assert!(h.find("Files & links").is_some() && h.find("A metalink").is_some(), "{:?}", h.texts());
        let tile = h.app.ctx.read_response(egui::Id::new(("source tile", "A file"))).unwrap().rect;
        let try_it = h.texts().into_iter().find(|(text, rect)| text == "Try it" && tile.contains(rect.center())).expect("its Try it").1;
        h.click_at(try_it.center());
        assert_eq!((h.app.tab, h.app.url_input.as_str()), (Tab::Downloads, "https://ftp.gnu.org/gnu/hello/hello-2.12.1.tar.gz"));
        assert!(h.settle() && h.find("from ftp.gnu.org").is_some(), "{:?}", h.texts());
        for kind in KINDS {
            assert!(GROUPS.contains(&kind.group));
        }
    }
}
