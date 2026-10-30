use super::*;

/// A page in the sidebar. Coming under the pointer it brightens; becoming selected, its icon pops
/// and tilts. A count that changes pops and flashes. The selection pill is painted by [`sidebar`].
fn nav_item(ui: &mut Ui, glyph: &str, label: &str, count: Option<usize>, selected: bool) -> egui::Response {
    let p = palette(ui);
    let ctx = ui.ctx().clone();
    let id = egui::Id::new(("nav", label));
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label));
    let hover = anim::presence(&ctx, id.with("hover"), response.hovered(), 0.12);
    let chosen = anim::presence(&ctx, id.with("selected"), selected, 0.2);
    let painter = ui.painter();
    if !selected {
        painter.rect_filled(rect, 8.0, p.hover.gamma_multiply(hover));
    }
    let icon_color = p.muted.lerp_to_gamma(p.text, hover).lerp_to_gamma(p.accent, chosen);
    let text_color = p.text.lerp_to_gamma(p.strong, hover.max(chosen));
    let family = if selected { semibold() } else { FontFamily::Proportional };
    let picked = anim::changed(&ctx, id.with("picked"), selected, 0.35);
    let (scale, angle) = if selected { (1.0 + 0.22 * anim::bump(picked), -0.3 * anim::bump(picked)) } else { (1.0, 0.0) };
    anim::paint_icon(painter, rect.left_center() + Vec2::new(21.0, 0.0), glyph, 18.0 * scale, icon_color, angle);
    painter.text(rect.left_center() + Vec2::new(42.0, 0.0), Align2::LEFT_CENTER, label, FontId::new(14.0, family), text_color);
    if let Some(count) = count.filter(|&n| n > 0) {
        let t = anim::bump(anim::changed(&ctx, id.with("count"), count, 0.35));
        let font = FontId::proportional(12.0 * (1.0 + 0.35 * t));
        painter.text(rect.right_center() - Vec2::new(12.0, 0.0), Align2::RIGHT_CENTER, count.to_string(), font, p.muted.lerp_to_gamma(p.accent, t));
    }
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

pub(super) fn sidebar(app: &mut App, ctx: &egui::Context) {
    let p = palette_of(ctx);
    // Kept current every frame, so that the Add page's check means "just now" (see `add::just_added`).
    super::add::just_added(app, ctx);
    let completed = app.queue.items().iter().filter(|item| item.status == QueueItemStatus::Completed).count();
    let finished = anim::changed(ctx, egui::Id::new("logo bob"), rises(ctx, egui::Id::new("completed"), completed), 0.6);
    let frame = Frame::none().fill(p.side).inner_margin(Margin::symmetric(12.0, 16.0));
    egui::SidePanel::left("nav").resizable(false).exact_width(196.0).frame(frame).show(ctx, |ui| {
        ui.horizontal(|ui| {
            // The logo dips and the tile swells as a download finishes.
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::hover());
            ui.painter().rect_filled(rect.expand(1.5 * anim::bump(finished)), 9.0, p.accent);
            paint_logo(ui.painter(), rect.translate(Vec2::new(0.0, 4.0 * anim::bump(finished))), Color32::WHITE, p.accent);
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
        // The pill slides from the old page to the new one, under the items.
        let pill = ui.painter().add(egui::Shape::Noop);
        let mut rects = [Rect::NOTHING; 4];
        for (n, (tab, glyph, label, count)) in pages.into_iter().enumerate() {
            let response = nav_item(ui, glyph, label, count, app.tab == tab);
            rects[n] = response.rect;
            if response.on_hover_text(keys(&format!("Ctrl+{}", n + 1)).into_owned()).clicked() {
                go(app, tab);
            }
        }
        if let Some(n) = pages.iter().position(|page| page.0 == app.tab) {
            let rect = rects[n].translate(Vec2::new(0.0, pill_top(ctx, rects[n].top()) - rects[n].top()));
            ui.painter().set(pill, egui::epaint::RectShape::filled(rect, 8.0, p.accent.gamma_multiply(0.18)));
        }

        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).small().color(p.dim));
            ui.horizontal(|ui| {
                // Half a turn as the theme changes.
                let turn = anim::ease(anim::changed(ctx, egui::Id::new("theme icon"), app.settings.theme, 0.4));
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

    /// A page change slides the selection pill down from Add to Settings over a few frames, never
    /// back, after which frames stop; under Reduce motion it is there at once.
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
}
