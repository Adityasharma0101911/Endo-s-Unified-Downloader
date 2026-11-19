use super::*;

/// The rail's width: labels beside the icons, or only the icons in a narrow window (under 820
/// points), where the page needs the room.
pub(super) fn width(ctx: &egui::Context) -> f32 {
    if ctx.screen_rect().width() < 820.0 {
        68.0
    } else {
        212.0
    }
}

/// A page in the rail. Coming under the pointer it brightens; becoming selected, its icon pops
/// and tilts. A count that changes pops and flashes. The selection pill is painted by [`sidebar`].
/// Without room for labels, only the icon shows and the label is its tooltip.
fn nav_item(ui: &mut Ui, glyph: &str, label: &str, count: Option<usize>, selected: bool, labels: bool) -> egui::Response {
    let p = palette(ui);
    let ctx = ui.ctx().clone();
    let id = egui::Id::new(("nav", label));
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label));
    let hover = anim::presence(&ctx, id.with("hover"), response.hovered(), 0.12);
    let chosen = anim::presence(&ctx, id.with("selected"), selected, 0.2);
    let painter = ui.painter();
    if !selected {
        painter.rect_filled(rect, 10.0, p.hover.gamma_multiply(0.7 * hover));
    }
    let icon_color = p.muted.lerp_to_gamma(p.text, hover).lerp_to_gamma(p.link, chosen);
    let text_color = p.text.lerp_to_gamma(p.strong, hover.max(chosen));
    let family = if selected { semibold() } else { FontFamily::Proportional };
    let picked = anim::changed(&ctx, id.with("picked"), selected, 0.35);
    let (scale, angle) = if selected { (1.0 + 0.22 * anim::bump(picked), -0.3 * anim::bump(picked)) } else { (1.0, 0.0) };
    let icon_at = if labels { rect.left_center() + Vec2::new(22.0, 0.0) } else { rect.center() };
    anim::paint_icon(painter, icon_at, glyph, 19.0 * scale, icon_color, angle);
    if labels {
        painter.text(rect.left_center() + Vec2::new(44.0, 0.0), Align2::LEFT_CENTER, label, FontId::new(14.5, family), text_color);
    }
    if let Some(count) = count.filter(|&n| n > 0 && labels) {
        let t = anim::bump(anim::changed(&ctx, id.with("count"), count, 0.35));
        let font = FontId::proportional(12.0 * (1.0 + 0.35 * t));
        painter.text(rect.right_center() - Vec2::new(12.0, 0.0), Align2::RIGHT_CENTER, count.to_string(), font, p.muted.lerp_to_gamma(p.link, t));
    }
    let response = if labels { response } else { response.on_hover_text(label) };
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// How many times `n` has gone up since `id` was first seen: a key for [`anim::changed`] that a
/// fall (a download removed) leaves alone.
pub(super) fn rises(ctx: &egui::Context, id: egui::Id, n: usize) -> usize {
    ctx.data_mut(|d| {
        let (last, rises) = d.get_temp_mut_or_insert_with(id, || (n, 0usize));
        if n > *last {
            *rises += 1;
        }
        *last = n;
        *rises
    })
}

/// The top of the selection pill: eased from where it is to `top` when the selection moves.
fn pill_top(ctx: &egui::Context, top: f32) -> f32 {
    let id = egui::Id::new("nav pill");
    let t = anim::ease(anim::changed(ctx, id, top.to_bits(), 0.25));
    ctx.data_mut(|d| {
        let [from, to, shown] = d.get_temp_mut_or_insert_with(id.with("top"), || [top; 3]);
        if *to != top {
            (*from, *to) = (*shown, top);
        }
        *shown = egui::lerp(*from..=top, t);
        *shown
    })
}

/// The navigation rail: the logo, the four pages (Downloads, Library, Sources, Settings) with
/// the selection pill gliding between them, and at the bottom the theme, the shortcuts and the
/// clipboard watch. Translucent, so the moving background shows faintly through it.
pub(super) fn sidebar(app: &mut App, ctx: &egui::Context) {
    let p = palette_of(ctx);
    // Kept current every frame, so that the Download button's check means "just now" (see
    // `add::just_added`).
    super::add::just_added(app, ctx);
    let completed = app.queue.items().iter().filter(|item| item.status == QueueItemStatus::Completed).count();
    let finished = anim::changed(ctx, egui::Id::new("logo bob"), rises(ctx, egui::Id::new("completed"), completed), 0.6);
    let rail = width(ctx);
    let labels = rail > 100.0;
    let frame = Frame::none().fill(p.side.gamma_multiply(0.86)).inner_margin(Margin::symmetric(12.0, 18.0));
    egui::SidePanel::left("nav").resizable(false).exact_width(rail).show_separator_line(false).frame(frame).show(ctx, |ui| {
        ui.horizontal(|ui| {
            // The logo dips and the tile swells as a download finishes.
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
            ui.painter().rect_filled(rect.expand(1.5 * anim::bump(finished)), 10.0, p.accent);
            paint_logo(ui.painter(), rect.translate(Vec2::new(0.0, 4.0 * anim::bump(finished))), Color32::WHITE, p.accent);
            if labels {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(bold("Endo's").size(15.5).color(p.strong));
                    ui.label(RichText::new("Unified Downloader").size(11.5).color(p.muted));
                });
            }
        });
        ui.add_space(26.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        let pages = [
            (Tab::Downloads, icon::DOWNLOAD_SIMPLE, "Downloads", Some(app.queue.items().len())),
            (Tab::Library, icon::BOOKS, "Library", Some(app.history.len())),
            (Tab::Sources, icon::COMPASS, "Sources", None),
            (Tab::Settings, icon::GEAR_SIX, "Settings", None),
        ];
        // The pill slides from the old page to the new one, under the items.
        let pill = ui.painter().add(egui::Shape::Noop);
        let mut rects = [Rect::NOTHING; 4];
        for (n, (tab, glyph, label, count)) in pages.into_iter().enumerate() {
            let response = nav_item(ui, glyph, label, count, app.tab == tab, labels);
            rects[n] = response.rect;
            if response.on_hover_text(keys(&format!("Ctrl+{}", n + 1)).into_owned()).clicked() {
                // Downloads again closes the details shown in its place.
                if tab == Tab::Downloads {
                    app.focused = None;
                }
                go(app, tab);
            }
        }
        if let Some(n) = pages.iter().position(|page| page.0 == app.tab) {
            let rect = rects[n].translate(Vec2::new(0.0, pill_top(ctx, rects[n].top()) - rects[n].top()));
            let shape = egui::epaint::RectShape::new(rect, 10.0, p.accent.gamma_multiply(0.2), Stroke::new(1.0, p.link.gamma_multiply(0.25)));
            ui.painter().set(pill, shape);
        }

        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            if labels {
                ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).small().color(p.dim));
            }
            ui.horizontal(|ui| {
                // Half a turn as the theme changes.
                let turn = anim::ease(anim::changed(ctx, egui::Id::new("theme icon"), app.settings.theme, 0.4));
                if !labels {
                    // One button that goes round dark, light and the system's.
                    let next = match app.settings.theme { 1 => 2, 2 => 0, _ => 1 };
                    if icon_button(ui, icon::CIRCLE_HALF, "Theme: dark, light or the system's", true).clicked() {
                        app.settings.theme = next;
                    }
                    return;
                }
                anim::icon(ui, icon::CIRCLE_HALF, 16.0, p.muted, 1.0, std::f32::consts::PI * (turn - 1.0));
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
                let open_list = |ctx: &egui::Context| {
                    ctx.data_mut(|d| {
                        let open = d.get_temp_mut_or_default::<bool>(super::shortcuts_id());
                        *open = !*open;
                    })
                };
                if !labels {
                    if icon_button(ui, icon::KEYBOARD, "The keyboard shortcuts (F1)", true).clicked() {
                        open_list(ctx);
                    }
                    return;
                }
                ui.label(RichText::new(icon::KEYBOARD).size(16.0).color(p.muted));
                let list = ui.add(egui::Button::new("Shortcuts").frame(false));
                if list.on_hover_text("The keyboard shortcuts (F1)").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    open_list(ctx);
                }
            });
            if labels {
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
            }
        });
    });
    // The drop zone covers the whole window; this runs once a frame on every page.
    super::add::drop_zone(ctx);
}

/// The marks of the app's logo (`assets/logo.svg`) in `color` on a plate of `plate`, `rect` being
/// the logo's 256-unit square: three bars, the arrowhead and the line under it.
fn paint_logo(painter: &egui::Painter, rect: Rect, color: Color32, plate: Color32) {
    let unit = rect.width() / 256.0;
    let at = |x: f32, y: f32| rect.min + Vec2::new(x, y) * unit;
    for (x, y, w, h, alpha) in [(74.0, 52.0, 22.0, 72.0, 0.65), (117.0, 38.0, 22.0, 86.0, 1.0), (160.0, 52.0, 22.0, 72.0, 0.65), (88.0, 218.0, 80.0, 6.0, 0.4)] {
        painter.rect_filled(Rect::from_min_max(at(x, y), at(x + w, y + h)), 4.0 * unit, color.gamma_multiply(alpha));
    }
    // The arrowhead is concave: a triangle, with the plate painted over its notch.
    painter.add(egui::Shape::convex_polygon(vec![at(52.0, 136.0), at(204.0, 136.0), at(128.0, 206.0)], color, Stroke::NONE));
    painter.add(egui::Shape::convex_polygon(vec![at(88.0, 136.0), at(168.0, 136.0), at(128.0, 172.0)], plate, Stroke::NONE));
}

#[cfg(test)]
mod tests {
    use super::super::harness::Harness;
    use super::*;

    /// The top of the selection pill as last drawn.
    fn pill(h: &Harness) -> f32 {
        h.app.ctx.data(|d| d.get_temp::<[f32; 3]>(egui::Id::new("nav pill").with("top"))).unwrap()[2]
    }

    /// A page change slides the selection pill down from Downloads to Settings over a few
    /// frames, never back, after which frames stop; under Reduce motion it is there at once.
    #[test]
    fn the_selection_pill_slides_to_the_new_page() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            assert!(h.settle());
            let start = pill(&h);
            h.click("Settings");
            h.leave();
            let tops: Vec<f32> = std::iter::once(pill(&h)).chain((0..30).map(|_| (h.frame(Vec::new()), pill(&h)).1)).collect();
            let end = *tops.last().unwrap();
            assert!(end - start > 100.0, "three items down: {start} to {end}");
            if reduce {
                assert!(tops.iter().all(|&top| top == end), "at once: {tops:?}");
            } else {
                let between = tops.iter().filter(|&&top| start < top && top < end).count();
                assert!(between >= 5 && tops.windows(2).all(|w| w[0] <= w[1]), "slides: {tops:?}");
            }
            // The Settings page's cards may still be coming in.
            assert_eq!(h.frames(30), Duration::MAX, "and keeps still");
        }
    }

    /// In a narrow window the rail keeps only its icons, each named by its tooltip, and still
    /// switches pages.
    #[test]
    fn a_narrow_window_keeps_the_rails_icons() {
        let mut h = Harness::new();
        h.size = Vec2::new(760.0, 640.0);
        assert!(h.settle());
        assert!(h.find("Library").is_none() && h.find(icon::BOOKS).is_some(), "{:?}", h.texts());
        h.click(icon::BOOKS);
        assert_eq!(h.app.tab, Tab::Library);
        assert!(h.settle());
    }
}
