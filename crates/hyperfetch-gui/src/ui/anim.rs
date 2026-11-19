//! The motion toolkit: short, eased animations that a new target interrupts, keyed by stable ids
//! (a download's, a setting's, a page's). Under Reduce motion (see [`set_reduced`]) transitions
//! are instant and nothing moves by itself. Idle means idle: a helper asks for frames only while
//! what it draws is still moving, and [`tick`], the clock of continuous motion, only while called.

use std::f32::consts::{PI, TAU};
use std::hash::Hash;
use std::time::Duration;

use eframe::egui;
use egui::emath::{easing, Rot2};
use egui::epaint::TextShape;
use egui::{Color32, Context, FontId, Id, Mesh, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, UiBuilder, Vec2};

/// How long things take to fade or slide in, in seconds.
pub const APPEAR: f32 = 0.25;
/// Where a page or card rises from: 8 points below its place.
pub const RISE: Vec2 = Vec2::new(0.0, 8.0);
/// Where a banner drops from: 6 points above its place.
pub const DROP: Vec2 = Vec2::new(0.0, -6.0);

// ---- Reduce motion --------------------------------------------------------------------------

fn reduced_id() -> Id {
    Id::new("reduce_motion")
}

/// Turns Reduce motion on or off (`render` does, every frame, from the setting), egui's own
/// animations (menus, collapsing, scrolling) included.
pub fn set_reduced(ctx: &Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(reduced_id(), on));
    // egui's default animation time.
    let time = if on { 0.0 } else { 1.0 / 12.0 };
    if ctx.style().animation_time != time {
        ctx.all_styles_mut(|style| style.animation_time = time);
    }
}

/// Whether Reduce motion is on.
pub fn reduced(ctx: &Context) -> bool {
    ctx.data(|d| d.get_temp(reduced_id())).unwrap_or(false)
}

// ---- Easing ---------------------------------------------------------------------------------

/// Fast start, gentle stop: the curve for nearly everything.
pub fn ease(t: f32) -> f32 {
    easing::cubic_out(t.clamp(0.0, 1.0))
}

/// Like [`ease`], but shoots a little past 1.0 before settling: for things that pop in.
pub fn overshoot(t: f32) -> f32 {
    easing::back_out(t.clamp(0.0, 1.0))
}

/// 0.0 up to 1.0 at half way and back to 0.0: for a pop or a flash.
pub fn bump(t: f32) -> f32 {
    (t.clamp(0.0, 1.0) * PI).sin()
}

/// The delay of the `index`th of several things that appear together: a short step each, capped
/// so that a long list is never kept waiting.
pub fn stagger(index: usize) -> f32 {
    index.min(8) as f32 * 0.035
}

// ---- Timing ---------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Since {
    key: Id,
    start: f64,
}

/// Seconds since `key` last changed for `id`. The first time `id` is seen counts as a change
/// when `first_sight` is true, else as long ago.
pub fn since(ctx: &Context, id: Id, key: impl Hash, first_sight: bool) -> f32 {
    let key = Id::new(key);
    let now = ctx.input(|i| i.time);
    ctx.data_mut(|d| {
        let since = d.get_temp_mut_or_insert_with(id, || Since { key, start: if first_sight { now } else { f64::NEG_INFINITY } });
        if since.key != key {
            *since = Since { key, start: now };
        }
        (now - since.start) as f32
    })
}

/// 0.0 to 1.0 (linear) over `secs` once `elapsed` is past `delay`, asking for frames until then;
/// 1.0 at once under Reduce motion.
pub fn progress(ctx: &Context, elapsed: f32, delay: f32, secs: f32) -> f32 {
    if reduced(ctx) {
        return 1.0;
    }
    let t = ((elapsed - delay) / secs).clamp(0.0, 1.0);
    if t < 1.0 {
        ctx.request_repaint();
    }
    t
}

/// 0.0 to 1.0 (linear, [`APPEAR`] long) as `id` is first seen, starting `delay` seconds later:
/// for things that fade or slide in.
pub fn appear(ctx: &Context, id: Id, delay: f32) -> f32 {
    progress(ctx, since(ctx, id, (), true), delay, APPEAR)
}

/// 0.0 to 1.0 (linear) over `secs` since `key` last changed for `id`; 1.0 the first time.
pub fn changed(ctx: &Context, id: Id, key: impl Hash, secs: f32) -> f32 {
    progress(ctx, since(ctx, id, key, false), 0.0, secs)
}

/// 0.0 to 1.0 eased towards `on` in `secs`, reversing from where it is when `on` flips: for
/// hover, press, selection, show and hide.
pub fn presence(ctx: &Context, id: Id, on: bool, secs: f32) -> f32 {
    let secs = if reduced(ctx) { 0.0 } else { secs };
    ctx.animate_bool_with_time_and_easing(id, on, secs, easing::cubic_out)
}

/// A number eased (linearly) to `target` in `secs` from where it is when the target moves: for
/// progress and counters.
pub fn value(ctx: &Context, id: Id, target: f32, secs: f32) -> f32 {
    if reduced(ctx) {
        return target;
    }
    ctx.animate_value_with_time(id, target, secs)
}

#[derive(Clone, Copy)]
struct Fade {
    from: Color32,
    to: Color32,
    start: f64,
}

/// `target`, cross-faded in `secs` from the colour `id` showed whenever it changes: for a
/// status that changes colour.
pub fn color(ctx: &Context, id: Id, target: Color32, secs: f32) -> Color32 {
    let secs = if reduced(ctx) { 0.0 } else { secs };
    let now = ctx.input(|i| i.time);
    let at = |fade: &Fade| {
        let t = if secs > 0.0 { ((now - fade.start) / secs as f64).clamp(0.0, 1.0) as f32 } else { 1.0 };
        (fade.from.lerp_to_gamma(fade.to, ease(t)), t < 1.0)
    };
    let (shown, moving) = ctx.data_mut(|d| {
        let fade = d.get_temp_mut_or_insert_with(id, || Fade { from: target, to: target, start: f64::NEG_INFINITY });
        if fade.to != target {
            *fade = Fade { from: at(fade).0, to: target, start: now };
        }
        at(fade)
    });
    if moving {
        ctx.request_repaint();
    }
    shown
}

/// A sideways offset, in points, that shakes and settles when `key` changes for `id`: for a
/// failure. 0.0 the first time.
pub fn shake(ctx: &Context, id: Id, key: impl Hash) -> f32 {
    let t = changed(ctx, id, key, 0.45);
    (t * TAU * 3.0).sin() * (1.0 - t) * 4.0
}

// ---- Continuous motion ----------------------------------------------------------------------

/// The clock of motion that runs by itself (spinners, pulses, sheens): the time in seconds, and
/// a frame within 16 ms, each time it is called; None under Reduce motion or while the window is
/// minimized, when such motion stays still. Call it only while the thing that moves is visible
/// and active.
pub fn tick(ctx: &Context) -> Option<f64> {
    // Minimized, the window keeps its old size, so everything on it still counts as visible.
    if reduced(ctx) || ctx.input(|i| i.viewport().minimized == Some(true)) {
        return None;
    }
    ctx.request_repaint_after(Duration::from_millis(16));
    Some(ctx.input(|i| i.time))
}

/// 0.0 up to 1.0 and back every `period` seconds while called (see [`tick`]); 0.0 under Reduce
/// motion.
pub fn pulse(ctx: &Context, period: f32) -> f32 {
    tick(ctx).map_or(0.0, |time| (0.5 - 0.5 * (time * std::f64::consts::TAU / period as f64).cos()) as f32)
}

// ---- Springs --------------------------------------------------------------------------------

/// How long a [`roll`]ing number takes to cover half the way to a new reading.
pub const ROLL: f32 = 0.12;
/// The half-life of things that glide into place: the focus ring, a lifted card.
pub const GLIDE: f32 = 0.07;

#[derive(Clone, Copy)]
struct Spring {
    pos: f32,
    vel: f32,
    time: f64,
    /// The target it moves to, and how far it was from it when that target came.
    target: f32,
    span: f32,
}

/// One step of a critically damped spring from `pos` (moving at `vel`) towards `target`, `dt`
/// seconds long. `half_life` sets its pace (from rest: 40 % of the way in one, 95 % in about
/// six); it never overshoots.
fn spring_step(pos: f32, vel: f32, target: f32, half_life: f32, dt: f32) -> (f32, f32) {
    let y = 2.0 * std::f32::consts::LN_2 / half_life.max(1e-4);
    let (j0, eydt) = (pos - target, (-y * dt).exp());
    let j1 = vel + j0 * y;
    (eydt * (j0 + j1 * dt) + target, eydt * (vel - j1 * y * dt))
}

/// `target`, followed by a critically damped spring keyed by `id`: no snapping, no overshoot,
/// a new target taken up with the speed it already has. At the target the first time `id` is
/// seen, at once under Reduce motion; asks for frames only while it moves.
pub fn spring(ctx: &Context, id: Id, target: f32, half_life: f32) -> f32 {
    let now = ctx.input(|i| i.time);
    let reduced = reduced(ctx);
    let (pos, moving) = ctx.data_mut(|d| {
        let s = d.get_temp_mut_or_insert_with(id, || Spring { pos: target, vel: 0.0, time: now, target, span: 0.0 });
        let dt = ((now - s.time) as f32).clamp(0.0, 0.1);
        s.time = now;
        if s.target != target {
            (s.target, s.span) = (target, (target - s.pos).abs());
        }
        if reduced || !target.is_finite() || !s.pos.is_finite() {
            (s.pos, s.vel) = (target, 0.0);
        } else {
            (s.pos, s.vel) = spring_step(s.pos, s.vel, target, half_life, dt);
            // Settled: within a hundredth of a point (or of a unit), or a thousandth of the way
            // it came (a speed falling from megabytes to nothing), and nearly still.
            let near = 0.01_f32.max(target.abs() * 1e-4).max(s.span * 1e-3);
            if (s.pos - target).abs() < near && s.vel.abs() < near * 10.0 {
                (s.pos, s.vel) = (target, 0.0);
            }
        }
        (s.pos, s.pos != target)
    });
    if moving {
        ctx.request_repaint();
    }
    pos
}

/// A number (a speed, a percentage, seconds left) rolling smoothly to each new reading.
pub fn roll(ctx: &Context, id: Id, target: f32) -> f32 {
    spring(ctx, id, target, ROLL)
}

/// `rect` gliding to each new place and size (see [`spring`]).
pub fn glide_rect(ctx: &Context, id: Id, rect: Rect) -> Rect {
    let [x0, y0, x1, y1] = [rect.min.x, rect.min.y, rect.max.x, rect.max.y];
    let at = |n: u8, v: f32| spring(ctx, id.with(n), v, GLIDE);
    Rect::from_min_max(Pos2::new(at(0, x0), at(1, y0)), Pos2::new(at(2, x1), at(3, y1)))
}

// ---- Scrolling ------------------------------------------------------------------------------

/// The share of a mouse wheel's turn still to scroll after `dt` seconds: it eases out over about
/// a quarter of a second (egui's own reaches 90 % in a tenth).
fn wheel_left(dt: f32) -> f32 {
    (-dt / 0.085).exp()
}

/// Mouse wheel notches eased out over about a quarter of a second, for every scroll area of the
/// frame: call it first thing each frame. Trackpads and Ctrl+wheel zoom are left as they come;
/// under Reduce motion a notch scrolls at once.
pub fn glide_scroll(ctx: &Context) {
    let id = Id::new("wheel glide");
    let reduced = reduced(ctx);
    let (pending, now) = ctx.input_mut(|i| {
        let (mut smooth, mut notches) = (Vec2::ZERO, Vec2::ZERO);
        let screen = i.screen_rect().height();
        for event in &i.events {
            let egui::Event::MouseWheel { unit, delta, modifiers } = event else { continue };
            let mut delta = match unit {
                egui::MouseWheelUnit::Point => *delta,
                egui::MouseWheelUnit::Line => 50.0 * *delta,
                egui::MouseWheelUnit::Page => screen * *delta,
            };
            if modifiers.shift {
                delta = Vec2::new(delta.x + delta.y, 0.0);
            }
            if modifiers.ctrl || modifiers.mac_cmd || modifiers.command {
                continue;
            }
            // As egui tells a trackpad's fine steps from a wheel's notches.
            let fine = matches!(unit, egui::MouseWheelUnit::Point) && delta.length() < 8.0;
            if fine {
                smooth += delta;
            } else {
                notches += delta;
            }
        }
        (notches, (i.time, i.stable_dt.min(0.1), smooth))
    });
    let (time, dt, smooth) = now;
    let left = ctx.data_mut(|d| {
        let state = d.get_temp_mut_or_insert_with(id, || (Vec2::ZERO, time));
        let dt = if time > state.1 { dt } else { 0.0 };
        state.1 = time;
        state.0 += pending;
        let keep = if reduced { 0.0 } else { wheel_left(dt) };
        let mut step = state.0 * (1.0 - keep);
        if (state.0 - step).length() < 0.5 {
            step = state.0;
        }
        state.0 -= step;
        (step, state.0)
    });
    let (step, rest) = left;
    // What egui eased out of earlier notches itself is replaced by this, the trackpad's kept.
    ctx.input_mut(|i| i.smooth_scroll_delta = smooth + step);
    if rest != Vec2::ZERO {
        ctx.request_repaint();
    }
}

// ---- Containers -----------------------------------------------------------------------------

/// Shows `add` `t` opaque and `(1 - t) * from` away from its place, while what follows is laid
/// out as if it were in place: motion that never moves other widgets.
pub fn shifted<R>(ui: &mut Ui, t: f32, from: Vec2, add: impl FnOnce(&mut Ui) -> R) -> R {
    let offset = from * (1.0 - t);
    let rect = ui.available_rect_before_wrap().translate(offset);
    let mut child = ui.new_child(UiBuilder::new().max_rect(rect).layout(*ui.layout()));
    if t < 1.0 {
        child.multiply_opacity(t);
    }
    let inner = add(&mut child);
    ui.advance_cursor_after_rect(child.min_rect().translate(-offset));
    inner
}

/// `add` fading in and sliding from `from` into place the first time `id` is seen, `delay`
/// seconds late (see [`stagger`]): for cards, rows and banners.
pub fn fade_in<R>(ui: &mut Ui, id: Id, delay: f32, from: Vec2, add: impl FnOnce(&mut Ui) -> R) -> R {
    let t = ease(appear(ui.ctx(), id, delay));
    shifted(ui, t, from, add)
}

// ---- Painting -------------------------------------------------------------------------------

/// Paints the icon `glyph` centred on `center`, `size` points big, turned `angle` radians
/// clockwise around its centre.
pub fn paint_icon(painter: &Painter, center: Pos2, glyph: &str, size: f32, color: Color32, angle: f32) {
    let galley = painter.layout_no_wrap(glyph.to_owned(), FontId::proportional(size), color);
    let pos = center - Rot2::from_angle(angle) * (galley.size() / 2.0);
    painter.add(TextShape::new(pos, galley, color).with_angle(angle));
}

/// The icon `glyph` laid out like a label of it at `size`, drawn `scale` times as big and turned
/// `angle` radians around its centre, so that motion never moves its neighbours.
pub fn icon(ui: &mut Ui, glyph: &str, size: f32, color: Color32, scale: f32, angle: f32) -> Response {
    let room = ui.painter().layout_no_wrap(glyph.to_owned(), FontId::proportional(size), color).size();
    let (rect, response) = ui.allocate_exact_size(room, Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_icon(ui.painter(), rect.center(), glyph, size * scale, color, angle);
    }
    response
}

/// A spinning arc `size` points wide whose length breathes; a still one under Reduce motion.
pub fn spinner(ui: &mut Ui, size: f32, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    if ui.is_rect_visible(rect) {
        let (start, sweep) = match tick(ui.ctx()) {
            Some(time) => ((time * 5.0 % std::f64::consts::TAU) as f32, 3.4 + 1.4 * (time * 2.2).sin() as f32),
            None => (-PI / 2.0, 1.5 * PI),
        };
        let radius = size / 2.0 - 1.5;
        let points = (0..=24).map(|i| rect.center() + radius * Vec2::angled(start + sweep * i as f32 / 24.0)).collect();
        ui.painter().add(Shape::line(points, Stroke::new(2.0, color)));
    }
    response
}

/// Paints a check mark in `rect`, drawn `t` (0.0 to 1.0) of the way, short stroke first: a check
/// that draws itself as `t` grows.
pub fn paint_check(painter: &Painter, rect: Rect, t: f32, stroke: Stroke) {
    let [a, b, c] = [Vec2::new(0.18, 0.55), Vec2::new(0.42, 0.78), Vec2::new(0.84, 0.26)].map(|f| rect.lerp_inside(f));
    let (first, second) = (a.distance(b), b.distance(c));
    let drawn = t.clamp(0.0, 1.0) * (first + second);
    let points = if drawn <= first {
        vec![a, a + (b - a) * (drawn / first)]
    } else {
        vec![a, b, b + (c - b) * ((drawn - first) / second)]
    };
    painter.add(Shape::line(points, stroke));
}

/// Half the height of the pill `rect` (a bar whose ends are half circles) at `x`.
pub fn pill_half(rect: Rect, x: f32) -> f32 {
    let r = rect.height() / 2.0;
    let d = (rect.left() + r - x).max(x - (rect.right() - r)).clamp(0.0, r);
    (r * r - d * d).sqrt()
}

/// Half the height, at each x, of the part of the pill `track` filled from its left end to
/// `reach`: round at both ends like the track itself, also when it is shorter than the bar is
/// high (a round end sliding in, never a squashed or square one).
pub fn fill_half(track: Rect, reach: f32) -> impl Fn(f32) -> f32 + Copy {
    let fill = Rect::from_min_max(Pos2::new(reach - (reach - track.left()).max(track.height()), track.top()), Pos2::new(reach, track.bottom()));
    move |x| pill_half(track, x).min(pill_half(fill, x))
}

/// The outline of the shape `half` high (see [`fill_half`]) around the middle line of `rect`,
/// from the left end of `rect` to its right end: a convex polygon.
pub fn outline(rect: Rect, half: impl Fn(f32) -> f32) -> Vec<Pos2> {
    let cap = (rect.height() / 2.0).min(rect.width() / 2.0);
    let steps = 10;
    let left = (0..=steps).map(|i| rect.left() + cap * i as f32 / steps as f32);
    let right = (0..=steps).map(|i| rect.right() - cap + cap * i as f32 / steps as f32);
    let sides: Vec<(f32, f32)> = left.chain(right).map(|x| (x, half(x))).collect();
    let y = rect.center().y;
    let top = sides.iter().map(|&(x, h)| Pos2::new(x, y - h));
    // Where the shape comes to a point the top has it already.
    let bottom = sides.iter().rev().filter(|&&(_, h)| h > 0.05).map(|&(x, h)| Pos2::new(x, y + h));
    let mut points: Vec<Pos2> = top.chain(bottom).collect();
    points.dedup();
    points
}

/// A soft light sweeping across `rect` (and clipped to it) every 1.6 s, while called: for a bar
/// that is filling. Nothing under Reduce motion or when out of view.
pub fn sheen(ui: &Ui, rect: Rect) {
    sweep(ui, rect, |_| rect.height() / 2.0);
}

/// [`sheen`] over the shape `half` high at each x around the middle line of `rect` (see
/// [`fill_half`]), so that the light follows a bar's round ends.
pub fn sweep(ui: &Ui, rect: Rect, half: impl Fn(f32) -> f32) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    let Some(time) = tick(ui.ctx()) else { return };
    let band = (rect.height() * 4.0).max(48.0);
    let start = rect.left() - band + (time / 1.6).fract() as f32 * (rect.width() + band);
    let (from, to) = (start.max(rect.left()), (start + band).min(rect.right()));
    if to <= from {
        return;
    }
    let y = rect.center().y;
    let slices = ((to - from) / 3.0).ceil().max(1.0) as u32;
    let mut mesh = Mesh::default();
    for i in 0..=slices {
        let x = from + (to - from) * i as f32 / slices as f32;
        // Brightest in the middle of the band, nothing at its edges.
        let light = 1.0 - (2.0 * (x - start) / band - 1.0).abs();
        let color = Color32::from_white_alpha((60.0 * light.clamp(0.0, 1.0)) as u8);
        let h = half(x);
        mesh.colored_vertex(Pos2::new(x, y - h), color);
        mesh.colored_vertex(Pos2::new(x, y + h), color);
    }
    for i in 0..slices {
        let i = 2 * i;
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 1, i + 3, i + 2);
    }
    ui.painter().with_clip_rect(rect.intersect(ui.clip_rect())).add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one frame at `time` seconds and returns what `f` gave and how soon the frame after
    /// was asked for.
    fn at<R>(ctx: &Context, time: f64, mut f: impl FnMut(&Context) -> R) -> (R, Duration) {
        let mut out = None;
        let output = ctx.run(egui::RawInput { time: Some(time), ..Default::default() }, |ctx| out = Some(f(ctx)));
        (out.unwrap(), output.viewport_output[&egui::ViewportId::ROOT].repaint_delay)
    }

    /// A context past its first frame, which always asks for a second one.
    fn fresh() -> Context {
        let ctx = Context::default();
        at(&ctx, 0.0, |_| ());
        at(&ctx, 0.0, |_| ());
        ctx
    }

    /// What `f` gives at `time` and a moment later, the same, after which no frame is asked for
    /// (egui follows each request for a frame with one more).
    fn settled<R: PartialEq + std::fmt::Debug>(ctx: &Context, time: f64, mut f: impl FnMut(&Context) -> R) -> R {
        let (first, _) = at(ctx, time, &mut f);
        let (second, delay) = at(ctx, time + 0.05, &mut f);
        assert_eq!(first, second);
        assert_eq!(delay, Duration::MAX, "still asking for frames");
        second
    }

    #[test]
    fn a_filled_bar_is_round_at_both_ends_however_short() {
        let track = Rect::from_min_size(Pos2::new(10.0, 0.0), Vec2::new(140.0, 18.0));
        for reach in [11.0, 14.0, 19.0, 28.0, 80.0, 150.0] {
            let half = fill_half(track, reach);
            let filled = Rect::from_min_max(track.min, Pos2::new(reach, track.bottom()));
            // Never outside the track, nothing past its reach, and its ends come to a point.
            for x in (0..=1500).map(|i| 10.0 + i as f32 * 0.1) {
                assert!(half(x) <= pill_half(track, x) + 1e-3, "reach {reach}, x {x}");
            }
            assert!(half(reach) < 0.01 && half(track.left()) < 0.01, "reach {reach}");
            let points = outline(filled, half);
            assert!(points.len() >= 6, "reach {reach}: {points:?}");
            assert!(points.iter().all(|p| filled.expand(0.01).contains(*p)), "reach {reach}: {points:?}");
        }
        // A long fill is as high as the bar in its middle.
        assert_eq!(fill_half(track, 150.0)(80.0), 9.0);
    }

    #[test]
    fn easing_starts_at_zero_and_ends_at_one() {
        for f in [ease, overshoot] {
            for (t, want) in [(0.0, 0.0), (1.0, 1.0), (-1.0, 0.0), (2.0, 1.0)] {
                assert!((f(t) - want).abs() < 1e-6, "{t} gives {}", f(t));
            }
        }
        assert!(overshoot(0.6) > 1.0, "overshoots before settling");
        assert!(bump(0.0).abs() < 1e-6 && bump(1.0).abs() < 1e-6 && (bump(0.5) - 1.0).abs() < 1e-6);
        assert_eq!([stagger(0), stagger(1), stagger(8), stagger(500)], [0.0, 0.035, 0.28, 0.28]);
    }

    /// A change starts from 0.0 and settles at 1.0, asking for frames only on the way; the first
    /// sight of a key is no change, but the first sight of something appearing is.
    #[test]
    fn transitions_run_once_and_then_stop_asking_for_frames() {
        let ctx = fresh();
        let id = Id::new("status");
        assert_eq!(settled(&ctx, 1.0, |ctx| changed(ctx, id, "queued", 0.3)), 1.0);
        assert_eq!(at(&ctx, 2.0, |ctx| changed(ctx, id, "running", 0.3)), (0.0, Duration::ZERO));
        let (t, delay) = at(&ctx, 2.15, |ctx| changed(ctx, id, "running", 0.3));
        assert!((t - 0.5).abs() < 1e-3 && delay == Duration::ZERO);
        assert_eq!(settled(&ctx, 2.4, |ctx| changed(ctx, id, "running", 0.3)), 1.0);

        let row = Id::new("row 7");
        assert_eq!(at(&ctx, 3.0, |ctx| appear(ctx, row, 0.1)).0, 0.0, "waits out its delay");
        assert!((at(&ctx, 3.1 + APPEAR as f64 / 2.0, |ctx| appear(ctx, row, 0.1)).0 - 0.5).abs() < 1e-3);
        assert_eq!(settled(&ctx, 4.0, |ctx| appear(ctx, row, 0.1)), 1.0);

        let chip = Id::new("chip");
        assert_eq!(at(&ctx, 5.0, |ctx| color(ctx, chip, Color32::RED, 0.3)).0, Color32::RED);
        at(&ctx, 5.15, |ctx| color(ctx, chip, Color32::GREEN, 0.3));
        let (mid, delay) = at(&ctx, 5.3, |ctx| color(ctx, chip, Color32::GREEN, 0.3));
        assert!(mid != Color32::RED && mid != Color32::GREEN && delay == Duration::ZERO, "{mid:?}");
        assert_eq!(settled(&ctx, 6.0, |ctx| color(ctx, chip, Color32::GREEN, 0.3)), Color32::GREEN);
    }

    /// A spring takes up a new target smoothly, never past it, settles on it exactly and then
    /// asks for no frames; seen first, or under Reduce motion, it is at its target at once.
    #[test]
    fn a_spring_glides_without_overshoot_and_settles() {
        let ctx = fresh();
        let id = Id::new("spring");
        assert_eq!(settled(&ctx, 1.0, |ctx| spring(ctx, id, 10.0, 0.1)), 10.0);
        let mut seen = Vec::new();
        let mut time = 2.0;
        loop {
            let (value, delay) = at(&ctx, time, |ctx| spring(ctx, id, 50.0, 0.1));
            seen.push(value);
            time += 1.0 / 60.0;
            if delay == Duration::MAX || time > 4.0 {
                break;
            }
        }
        assert_eq!(*seen.last().unwrap(), 50.0, "settles exactly: {seen:?}");
        assert!(seen.windows(2).all(|w| w[0] <= w[1]) && seen.iter().all(|&v| v <= 50.0), "no overshoot: {seen:?}");
        assert!(seen.iter().filter(|&&v| v > 10.0 && v < 50.0).count() >= 10, "glides: {seen:?}");
        assert!(time < 3.5, "within a second and a half");
        // Under Reduce motion: there at once.
        at(&ctx, 5.0, |ctx| set_reduced(ctx, true));
        assert_eq!(at(&ctx, 5.1, |ctx| spring(ctx, id, -20.0, 0.1)), (-20.0, Duration::MAX));
    }

    /// One frame at `time` with `events`, after `glide_scroll`: how far a scroll area under the
    /// pointer moves this frame (what `glide_scroll` leaves for it), and how soon the next frame
    /// is asked for. The harness's page tests scroll real pages with it.
    fn scroll_frame(ctx: &Context, time: f64, events: Vec<egui::Event>) -> (f32, Duration) {
        let mut moved = 0.0;
        let input = egui::RawInput {
            time: Some(time),
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 300.0))),
            events,
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            glide_scroll(ctx);
            moved = -ctx.input(|i| i.smooth_scroll_delta.y);
        });
        (moved, output.viewport_output[&egui::ViewportId::ROOT].repaint_delay)
    }

    /// A mouse wheel's notch scrolls the list smoothly, easing out over a quarter of a second or
    /// so, then frames stop; under Reduce motion it is there at once.
    #[test]
    fn a_wheel_notch_glides_and_then_stops() {
        for reduce in [false, true] {
            let ctx = Context::default();
            ctx.data_mut(|d| d.insert_temp(reduced_id(), reduce));
            let mut time = 1.0;
            for _ in 0..3 {
                scroll_frame(&ctx, time, vec![egui::Event::PointerMoved(Pos2::new(200.0, 150.0))]);
                time += 1.0 / 60.0;
            }
            let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: Vec2::new(0.0, -300.0), modifiers: egui::Modifiers::NONE };
            let mut offsets = vec![scroll_frame(&ctx, time, vec![wheel]).0];
            let mut delay = Duration::ZERO;
            for _ in 0..90 {
                time += 1.0 / 60.0;
                let (moved, next) = scroll_frame(&ctx, time, Vec::new());
                offsets.push(offsets.last().unwrap() + moved);
                delay = next;
            }
            let end = *offsets.last().unwrap();
            assert!((end - 300.0).abs() < 1.0, "the whole notch: {offsets:?}");
            assert!(offsets.windows(2).all(|w| w[0] <= w[1] + 1e-3), "only forward: {offsets:?}");
            assert_eq!(delay, Duration::MAX, "then still");
            if reduce {
                assert!(offsets[1] > 299.0, "at once: {offsets:?}");
            } else {
                assert!(offsets[1] < 150.0 && offsets[8] < 299.0 && offsets[8] > 150.0, "eases out: {offsets:?}");
            }
        }
    }

    /// Under Reduce motion every transition is over at once and nothing asks for frames, the
    /// clock of continuous motion included.
    #[test]
    fn reduce_motion_makes_everything_instant() {
        let ctx = fresh();
        let id = Id::new("thing");
        at(&ctx, 1.0, |ctx| {
            set_reduced(ctx, true);
            changed(ctx, id, 1, 0.3);
            presence(ctx, id, false, 0.3);
        });
        let (values, delay) = at(&ctx, 2.0, |ctx| {
            (
                changed(ctx, id, 2, 0.3),
                appear(ctx, Id::new("new row"), 0.2),
                presence(ctx, id, true, 0.3),
                value(ctx, id, 0.75, 0.3),
                color(ctx, id, Color32::RED, 0.3),
                tick(ctx),
                pulse(ctx, 1.0),
                shake(ctx, id, 3),
                ctx.style().animation_time,
            )
        });
        assert_eq!(values, (1.0, 1.0, 1.0, 0.75, Color32::RED, None, 0.0, 0.0, 0.0));
        assert_eq!(delay, Duration::MAX);

        at(&ctx, 3.0, |ctx| set_reduced(ctx, false));
        let (time, delay) = at(&ctx, 4.0, tick);
        assert!(time == Some(4.0) && delay <= Duration::from_millis(16), "the clock asks for a frame within 16 ms: {delay:?}");
        assert_eq!(at(&ctx, 5.0, |_| ()).1, Duration::MAX, "and none once it is no longer called");
    }
}
