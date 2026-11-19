//! The ambient field behind every page: soft motes drifting over a slow two-tone wash, drawn
//! with the painter (a few hundred small fans and two soft discs a frame). It reacts to the app:
//! idle, the motes drift; while downloads run they gather a current in the accent colour whose
//! pace follows the total speed (log-scaled, capped); a finished download sends one soft ripple
//! out from the middle, a failed one tints the field red for a moment.
//!
//! It costs nothing when it cannot be seen: with the setting off, under Reduce motion (the
//! window hidden in the tray counts), minimized or unfocused, its clock stops and it asks for no
//! frames; a still picture remains. Running, it asks for one frame every 33 ms (30 a second).

use std::time::Duration;

use eframe::egui;
use egui::{Color32, Context, Id, Mesh, Pos2, Rect, Shape, Vec2};

use super::{anim, Palette};

/// The frame interval while the field moves: 30 frames a second.
pub const FRAME: Duration = Duration::from_millis(33);
/// How long a ripple travels, and a failure's tint fades, in seconds of the field's clock.
const RIPPLE: f64 = 1.6;
const TINT: f64 = 1.2;
/// The current's top pace, in points a second, reached near 1 GB/s.
const CURRENT: f32 = 70.0;

/// What the field reacts to this frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mood {
    /// All running downloads' speed together, in bytes a second.
    pub speed: f64,
    /// How many downloads have completed, and failed, so far: a rise is news.
    pub completed: usize,
    pub failed: usize,
}

/// The field's state between frames.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Field {
    /// Seconds the field has moved for: it stops while the field is still.
    pub clock: f64,
    /// The input time of the last frame it moved on.
    last: Option<f64>,
    /// The current, eased: 0.0 idle to 1.0 at the top speed.
    pub flow: f32,
    /// How far the current has carried the motes, in points.
    pub carried: f32,
    /// The field clock when the last ripple, and the last tint, started.
    ripple: Option<f64>,
    tint: Option<f64>,
    /// The counts of [`Mood`] last frame.
    seen: Option<(usize, usize)>,
}

pub fn field_id() -> Id {
    Id::new("backdrop field")
}

/// The current a total `speed` (bytes a second) makes: log-scaled from 256 KiB/s, 1.0 at 1 GiB/s.
pub fn flow_for(speed: f64) -> f32 {
    let unit = 256.0 * 1024.0;
    ((1.0 + speed.max(0.0) / unit).ln() / (1.0 + (1u64 << 30) as f64 / unit).ln()).clamp(0.0, 1.0) as f32
}

/// Mote count, pace and brightness of `intensity` (0 calm, 1 normal, 2 lively), for a window of
/// `area` square points: more motes in a bigger window, never more than 270.
pub fn character(intensity: usize, area: f32) -> (usize, f32, f32) {
    let (count, pace, light) = match intensity {
        0 => (70.0, 0.6, 0.75),
        2 => (180.0, 1.5, 1.2),
        _ => (120.0, 1.0, 1.0),
    };
    let scale = (area / (1280.0 * 800.0)).clamp(0.5, 1.5);
    (((count * scale) as usize).min(270), pace, light)
}

/// A number in [0, 1) from `n` and `salt`: the motes' fixed traits.
fn hash(n: u32, salt: u32) -> f32 {
    let mut x = n.wrapping_mul(0x9E37_79B9) ^ salt.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^= x >> 16;
    (x >> 8) as f32 / (1u32 << 24) as f32
}

/// Where mote `n` is in `rect` at the field's state, and how near it is (0.35 far to 1.0 near),
/// moving at `pace`. A function of the state alone: the same frames give the same field.
pub fn mote(n: u32, field: &Field, rect: Rect, pace: f32) -> (Pos2, f32) {
    let (u, v, z) = (hash(n, 1), hash(n, 2), 0.35 + 0.65 * hash(n, 3));
    let (phase, wobble, rate) = (hash(n, 4) * std::f32::consts::TAU, 6.0 + 18.0 * z, 0.05 + 0.12 * hash(n, 5));
    let clock = field.clock as f32;
    let margin = 24.0;
    let (w, h) = (rect.width() + 2.0 * margin, rect.height() + 2.0 * margin);
    // Idle they drift right and a little up; the current carries the near ones further.
    let x = u * w + z * (clock * 6.0 * pace + field.carried) + wobble * (clock * rate * pace + phase).sin();
    let y = v * h - z * clock * 3.0 * pace + 0.6 * wobble * (clock * rate * 0.8 * pace + 2.0 * phase).cos();
    (Pos2::new(rect.left() - margin + x.rem_euclid(w), rect.top() - margin + y.rem_euclid(h)), z)
}

/// Steps the field by this frame and paints it on the background layer, under every panel:
/// the wash always (still when the field is), the motes when `on` and motion is allowed.
pub fn paint(ctx: &Context, p: &Palette, dark: bool, on: bool, intensity: usize, mood: Mood) {
    let rect = ctx.screen_rect();
    let still = anim::reduced(ctx) || !on;
    let seen = ctx.input(|i| i.focused && i.viewport().minimized != Some(true));
    let moving = !still && seen;
    let now = ctx.input(|i| i.time);
    let field = ctx.data_mut(|d| {
        let field = d.get_temp_mut_or_default::<Field>(field_id());
        step(field, now, moving, mood);
        *field
    });
    if moving {
        ctx.request_repaint_after(FRAME);
    }
    let painter = ctx.layer_painter(egui::LayerId::background());
    painter.rect_filled(rect, 0.0, p.bg);
    wash(&painter, rect, &field, p, dark);
    if !still {
        motes(&painter, rect, &field, p, dark, intensity);
    }
}

/// Moves `field` on to input time `now` when `moving`; notes a completion or failure as news.
fn step(field: &mut Field, now: f64, moving: bool, mood: Mood) {
    let counts = (mood.completed, mood.failed);
    let news = field.seen.map(|(done, failed)| (counts.0 > done, counts.1 > failed));
    field.seen = Some(counts);
    if !moving {
        field.last = None;
        return;
    }
    let dt = field.last.map_or(0.0, |last| (now - last).clamp(0.0, 0.1));
    field.last = Some(now);
    field.clock += dt;
    let ease = 1.0 - (-dt as f32 / 0.8).exp();
    field.flow += (flow_for(mood.speed) - field.flow) * ease;
    field.carried += field.flow * CURRENT * dt as f32;
    if let Some((done, failed)) = news {
        if done {
            field.ripple = Some(field.clock);
        }
        if failed {
            field.tint = Some(field.clock);
        }
    }
}

/// 1.0 just after `start` down to 0.0 `secs` later, on the field's clock.
fn fading(field: &Field, start: Option<f64>, secs: f64) -> f32 {
    start.map_or(0.0, |start| (1.0 - (field.clock - start) / secs).clamp(0.0, 1.0) as f32)
}

/// A soft disc: `color` at `center`, fading to nothing at `radius`.
fn glow(mesh: &mut Mesh, center: Pos2, radius: f32, color: Color32, sides: u32) {
    let first = mesh.vertices.len() as u32;
    mesh.colored_vertex(center, color);
    for k in 0..sides {
        let angle = k as f32 / sides as f32 * std::f32::consts::TAU;
        mesh.colored_vertex(center + radius * Vec2::angled(angle), Color32::TRANSPARENT);
    }
    for k in 0..sides {
        mesh.add_triangle(first, first + 1 + k, first + 1 + (k + 1) % sides);
    }
}

/// The two slow tones: a cool one high on the left, a second low on the right, orbiting a
/// little as the field moves; the first takes the accent while downloads run, and the whole
/// field reddens for a moment after a failure.
fn wash(painter: &egui::Painter, rect: Rect, field: &Field, p: &Palette, dark: bool) {
    let t = field.clock as f32;
    let size = rect.width().max(rect.height());
    let at = |x: f32, y: f32| rect.min + Vec2::new(x * rect.width(), y * rect.height());
    let (first, second) = if dark {
        (Color32::from_rgba_unmultiplied(48, 70, 160, 70), Color32::from_rgba_unmultiplied(18, 96, 120, 52))
    } else {
        (Color32::from_rgba_unmultiplied(255, 205, 160, 80), Color32::from_rgba_unmultiplied(150, 176, 226, 70))
    };
    let first = first.lerp_to_gamma(p.accent.gamma_multiply(if dark { 0.35 } else { 0.25 }), 0.6 * field.flow);
    let mut mesh = Mesh::default();
    glow(&mut mesh, at(0.16 + 0.05 * (t * 0.05).sin(), 0.1 + 0.05 * (t * 0.04).cos()), 0.78 * size, first, 32);
    glow(&mut mesh, at(0.88 + 0.04 * (t * 0.035).cos(), 0.92 + 0.05 * (t * 0.045).sin()), 0.7 * size, second, 32);
    painter.add(Shape::mesh(mesh));
    let tint = fading(field, field.tint, TINT);
    if tint > 0.0 {
        painter.rect_filled(rect, 0.0, p.red.gamma_multiply(0.07 * tint));
    }
}

/// The motes, nearer ones bigger and brighter; the current turns them towards the accent, a
/// ripple pushes them out and lights them as it passes.
fn motes(painter: &egui::Painter, rect: Rect, field: &Field, p: &Palette, dark: bool, intensity: usize) {
    let (count, pace, light) = character(intensity, rect.area());
    let ripple = field.ripple.map(|start| (field.clock - start) as f32).filter(|age| *age < RIPPLE as f32);
    let tint = fading(field, field.tint, TINT);
    let base = if dark { Color32::from_rgb(178, 196, 255) } else { Color32::from_rgb(54, 62, 92) };
    let color = base.lerp_to_gamma(p.accent, 0.7 * field.flow).lerp_to_gamma(p.red, 0.6 * tint);
    let (near, far) = if dark { (0.42, 0.08) } else { (0.2, 0.04) };
    let center = rect.center();
    // Sized once: seven vertices and six triangles a mote.
    let mut mesh = Mesh { indices: Vec::with_capacity(count * 18), vertices: Vec::with_capacity(count * 7), texture_id: Default::default() };
    for n in 0..count as u32 {
        let (mut pos, z) = mote(n, field, rect, pace);
        let mut lit = 0.0;
        if let Some(age) = ripple {
            let away = pos - center;
            let d = away.length().max(1.0);
            let front = (-((d - age * 520.0) / 50.0).powi(2)).exp() * (1.0 - age / RIPPLE as f32);
            pos += away / d * 16.0 * front;
            lit = front;
        }
        let twinkle = 0.85 + 0.15 * (field.clock as f32 * (0.4 + hash(n, 6)) + 6.0 * hash(n, 7)).sin();
        let alpha = ((far + (near - far) * z) * light * twinkle + 0.35 * lit).clamp(0.0, 1.0);
        let radius = (0.9 + 2.3 * z) * 2.2;
        glow(&mut mesh, pos, radius, color.gamma_multiply(alpha), 6);
    }
    painter.add(Shape::mesh(mesh));
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::harness::Harness;
    use super::*;

    /// A frame asked for 33 ms on: egui reports it less the frame it expects to take (16.7 ms).
    pub(in crate::ui) fn paced(delay: Duration) -> bool {
        delay > Duration::from_millis(15) && delay <= FRAME
    }

    fn field(h: &Harness) -> Field {
        h.app.ctx.data(|d| d.get_temp::<Field>(field_id())).unwrap_or_default()
    }

    /// On and in view, the field asks for a frame every 33 ms and no sooner; with the window
    /// unfocused or minimized, the setting off or Reduce motion on, it asks for none at all, and
    /// its clock stops.
    #[test]
    fn the_field_moves_only_while_it_can_be_seen() {
        let mut h = Harness::new();
        assert!(h.settle(), "off in tests: an idle window is still");
        h.app.settings.moving_background = true;
        h.frames(2);
        assert!(paced(h.frames(10)), "on and focused: 30 frames a second");
        let moving = field(&h).clock;
        assert!(moving > 0.1, "{moving}");

        for (what, change) in [
            ("unfocused", (|h: &mut Harness| h.focused = false) as fn(&mut Harness)),
            ("minimized", |h| h.minimized = true),
            ("off", |h| h.app.settings.moving_background = false),
            ("reduce motion", |h| h.app.settings.reduce_motion = Some(true)),
        ] {
            change(&mut h);
            let clock = field(&h).clock;
            assert_eq!(h.frames(30), Duration::MAX, "{what}");
            assert_eq!(field(&h).clock, clock, "{what}: the clock stops");
            (h.focused, h.minimized, h.app.settings.moving_background, h.app.settings.reduce_motion) = (true, false, true, Some(false));
            h.frames(2);
            assert!(paced(h.frames(5)), "back after {what}");
        }
    }

    /// The same frames give the same field: two windows stepped alike agree mote for mote, the
    /// motes move as the clock does, and a still field keeps them where they were.
    #[test]
    fn the_motes_move_with_the_clock_alone() {
        let run = |frames: usize| {
            let mut h = Harness::new();
            h.app.settings.moving_background = true;
            h.frames(frames);
            field(&h)
        };
        let (a, b) = (run(40), run(40));
        assert_eq!(a, b);
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(980.0, 720.0));
        let at = |field: &Field| (0..50).map(|n| mote(n, field, rect, 1.0).0).collect::<Vec<_>>();
        assert_ne!(at(&Field::default()), at(&a), "they moved");
        assert!(at(&a).iter().all(|pos| rect.expand(24.0).contains(*pos)), "inside the window, give or take its margin");

        let mut h = Harness::new();
        h.app.settings.moving_background = true;
        h.frames(10);
        h.focused = false;
        let still = field(&h);
        h.frames(30);
        assert_eq!(at(&field(&h)), at(&still), "a still field keeps its motes");
    }

    /// Downloads running give the field a current that grows with their speed; one finishing
    /// sends a ripple, one failing tints it.
    #[test]
    fn the_field_follows_the_downloads() {
        assert!(flow_for(0.0) == 0.0 && flow_for(1e6) < flow_for(1e8) && flow_for(1e12) == 1.0);
        let mut h = Harness::new();
        h.app.settings.moving_background = true;
        let [a, b] = ["a.iso", "b.iso"].map(|name| h.add(name));
        h.progress(a, 0.5);
        h.progress(b, 0.5);
        h.frames(60);
        let running = field(&h);
        assert!(running.flow > 0.1, "a current: {running:?}");
        assert!(running.ripple.is_none() && running.tint.is_none());
        h.finish(a, None);
        h.finish(b, Some("The server answered 404 Not Found"));
        h.frames(2);
        let after = field(&h);
        assert!(after.ripple.is_some() && after.tint.is_some(), "{after:?}");
        assert!(character(2, 1280.0 * 800.0).0 <= 270 && character(0, 1e9).0 <= 270);
    }
}
