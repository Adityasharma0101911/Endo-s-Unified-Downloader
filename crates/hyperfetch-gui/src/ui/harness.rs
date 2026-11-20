//! A window without a window, for tests: drives the real app's UI ([`render`]) in an
//! `egui::Context` with synthetic input and a hand-stepped clock, finds widgets by the text they
//! show, tells whether a frame asks for another, and paints frames to PNG with a small CPU
//! rasterizer. Nothing touches the screen, the real mouse or the keyboard.

use std::collections::HashMap;
use std::path::PathBuf;

use egui::epaint::{ImageData, ImageDelta, Primitive, Vertex};
use egui::{Event, FullOutput, Key, Modifiers, PointerButton, Shape, TextureId};
use hyperfetch_core::queue::DownloadQueue;
use image::RgbaImage;

use super::*;

/// The window's size, in points.
pub const SCREEN: Vec2 = Vec2::new(980.0, 720.0);
/// A frame's length: 60 frames a second.
pub const DT: f64 = 1.0 / 60.0;
/// The name [`Harness::find`] gives an on/off switch (see [`toggle`]): its knob.
pub const TOGGLE: &str = "toggle";

/// A texture as egui sent it: premultiplied sRGBA.
struct Texture {
    size: [usize; 2],
    texels: Vec<Color32>,
}

/// The app in a headless window. Each [`Harness::frame`] runs [`render`] once, [`DT`] after the
/// one before.
pub struct Harness {
    pub app: App,
    /// What the last frame drew and asked for.
    pub output: FullOutput,
    /// The clock, in seconds; only frames move it.
    pub time: f64,
    /// Pixels per point, as a display scaled to 125 % has 1.25.
    pub ppp: f32,
    /// Whether the window is minimized (it keeps its size, as on Windows).
    pub minimized: bool,
    /// Whether the window has the keyboard focus (the moving background stills without it).
    pub focused: bool,
    /// The window's size, in points ([`SCREEN`] at first).
    pub size: Vec2,
    modifiers: Modifiers,
    hovered_files: Vec<egui::HoveredFile>,
    textures: HashMap<TextureId, Texture>,
    freed: Vec<TextureId>,
    // After `app`, which holds a handle to it.
    _rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

impl Harness {
    /// The app on its Add page with the fonts and looks of [`setup`], dark, motion on, one pixel a
    /// point; its save folder and history are temporary.
    pub fn new() -> Self {
        crate::tests::isolate_history();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        setup(&ctx);
        let save_dir = dir.path().to_string_lossy().into_owned();
        // The moving background is off unless a test turns it on: an idle window asks for no frames.
        let settings = Settings { save_dir, theme: 1, reduce_motion: Some(false), show_all_settings: true, welcomed: true, moving_background: false, ..Settings::default() };
        Self {
            app: App::with(ctx, rt.handle().clone(), settings, DownloadQueue::new(), None),
            output: FullOutput::default(),
            time: 0.0,
            ppp: 1.0,
            minimized: false,
            focused: true,
            size: SCREEN,
            modifiers: Modifiers::NONE,
            hovered_files: Vec::new(),
            textures: HashMap::new(),
            freed: Vec::new(),
            _rt: rt,
            _dir: dir,
        }
    }

    // ---- Frames ----------------------------------------------------------------------------

    /// Runs one frame with `events`; how soon it asks for the next (`Duration::MAX`: never).
    pub fn frame(&mut self, events: Vec<Event>) -> Duration {
        self.time += DT;
        let mut input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)),
            time: Some(self.time),
            predicted_dt: DT as f32,
            modifiers: self.modifiers,
            events,
            hovered_files: self.hovered_files.clone(),
            focused: self.focused,
            system_theme: Some(Theme::Dark),
            ..Default::default()
        };
        let viewport = input.viewports.entry(egui::ViewportId::ROOT).or_default();
        viewport.native_pixels_per_point = Some(self.ppp);
        viewport.minimized = Some(self.minimized);
        for id in self.freed.drain(..) {
            self.textures.remove(&id);
        }
        let ctx = self.app.ctx.clone();
        self.output = ctx.run(input, |ctx| render(&mut self.app, ctx));
        for (id, delta) in std::mem::take(&mut self.output.textures_delta.set) {
            self.set_texture(id, delta);
        }
        self.freed = std::mem::take(&mut self.output.textures_delta.free);
        self.delay()
    }

    /// How soon the last frame asked for the next one (`Duration::MAX`: never).
    pub fn delay(&self) -> Duration {
        self.output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
    }

    /// `n` frames without input; how soon the last asks for the next.
    pub fn frames(&mut self, n: usize) -> Duration {
        (0..n).fold(Duration::MAX, |_, _| self.frame(Vec::new()))
    }

    /// Waits (up to 10 s of real time) for the work input started off the UI thread (the History
    /// page's read) to end, its call for a frame made, so that the call never lands in the middle
    /// of a check. What it reports stays unread, as before.
    pub fn wait_for_background(&self) {
        let start = Instant::now();
        while self._rt.metrics().num_alive_tasks() > 0 && start.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
    }

    /// Handles what the app's background work sends, as each frame of the real window does,
    /// until `done`; panics after 20 s of real time.
    pub fn wait_for(&mut self, done: impl Fn(&App) -> bool) {
        let start = Instant::now();
        while !done(&self.app) {
            assert!(start.elapsed() < Duration::from_secs(20), "timed out");
            std::thread::sleep(Duration::from_millis(20));
            while let Ok(event) = self.app.events_rx.try_recv() {
                self.app.handle_event(event);
            }
        }
    }

    /// Frames without input until one asks for no other, up to 10 s of them: whether it came.
    pub fn settle(&mut self) -> bool {
        (0..600).any(|_| self.frame(Vec::new()) == Duration::MAX)
    }

    // ---- Input -----------------------------------------------------------------------------

    pub fn hover(&mut self, pos: Pos2) -> Duration {
        self.frame(vec![Event::PointerMoved(pos)])
    }

    /// The pointer leaves the window.
    pub fn leave(&mut self) -> Duration {
        self.frame(vec![Event::PointerGone])
    }

    /// Presses the primary button at `pos`, held until [`Harness::release`].
    pub fn press(&mut self, pos: Pos2) -> Duration {
        self.frame(vec![Event::PointerMoved(pos), pointer_button(pos, true)])
    }

    pub fn release(&mut self, pos: Pos2) -> Duration {
        self.frame(vec![pointer_button(pos, false)])
    }

    /// A click at `pos`: pressed one frame, released the next.
    pub fn click_at(&mut self, pos: Pos2) -> Duration {
        self.press(pos);
        self.release(pos)
    }

    /// A click on what shows `label` (see [`Harness::find`]); panics when nothing does.
    pub fn click(&mut self, label: &str) -> Duration {
        let rect = self.find(label).unwrap_or_else(|| panic!("nothing shows {label:?}"));
        self.click_at(rect.center())
    }

    /// A double click on what shows `label`, after a pause long enough (0.67 s) that egui does
    /// not take it for the end of a triple click.
    pub fn double_click(&mut self, label: &str) -> Duration {
        self.frames(40);
        self.click(label);
        self.click(label)
    }

    /// `key` pressed and released with `modifiers` held.
    pub fn key(&mut self, key: Key, modifiers: Modifiers) -> Duration {
        self.modifiers = modifiers;
        let event = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
        let delay = self.frame(vec![event(true), event(false)]);
        self.modifiers = Modifiers::NONE;
        delay
    }

    /// Types `text` into the text box that has the focus.
    pub fn type_text(&mut self, text: &str) -> Duration {
        self.frame(vec![Event::Text(text.to_string())])
    }

    /// Pastes `text` as Ctrl+V does.
    pub fn paste(&mut self, text: &str) -> Duration {
        self.frame(vec![Event::Paste(text.to_string())])
    }

    /// Files dragged over the window from now on, or no longer (no drop).
    pub fn drag_files(&mut self, over: bool) -> Duration {
        let file = egui::HoveredFile { path: Some(PathBuf::from("ubuntu.torrent")), mime: String::new() };
        self.hovered_files = if over { vec![file] } else { Vec::new() };
        self.frame(Vec::new())
    }

    // ---- Finding ---------------------------------------------------------------------------

    /// Every text the last frame shows in view, with where: labels, buttons' text, icon glyphs,
    /// and the knob of each on/off switch as [`TOGGLE`]; in painting order.
    pub fn texts(&self) -> Vec<(String, Rect)> {
        fn walk(shape: &Shape, clip: Rect, out: &mut Vec<(String, Rect)>) {
            let (text, rect) = match shape {
                Shape::Vec(shapes) => return shapes.iter().for_each(|shape| walk(shape, clip, out)),
                Shape::Text(text) => (text.galley.text().to_string(), text.galley.rect.translate(text.pos.to_vec2())),
                Shape::Circle(c) if c.fill == Color32::WHITE && (7.0..=8.5).contains(&c.radius) => {
                    (TOGGLE.to_string(), Rect::from_center_size(c.center, Vec2::splat(2.0 * c.radius)))
                }
                _ => return,
            };
            let rect = rect.intersect(clip);
            if rect.is_positive() {
                out.push((text, rect));
            }
        }
        let mut out = Vec::new();
        for clipped in &self.output.shapes {
            walk(&clipped.shape, clipped.clip_rect, &mut out);
        }
        out
    }

    /// Where the last frame first shows `label`: as a text of its own, or after an icon (a
    /// button's "⏵ Pause" shows "Pause").
    pub fn find(&self, label: &str) -> Option<Rect> {
        self.texts().into_iter().find(|(text, _)| shows(text, label)).map(|(_, rect)| rect)
    }

    /// The `label` closest to the line of `anchor`: the X on the row of a file, the switch of a
    /// setting.
    pub fn find_near(&self, anchor: &str, label: &str) -> Option<Rect> {
        let anchor = self.find(anchor)?.center();
        let far = |rect: &Rect| ((rect.center().y - anchor.y).abs(), (rect.center().x - anchor.x).abs());
        let found = self.texts().into_iter().filter(|(text, _)| shows(text, label)).map(|(_, rect)| rect);
        found.min_by(|a, b| far(a).partial_cmp(&far(b)).unwrap())
    }

    // ---- Fixtures --------------------------------------------------------------------------

    /// Queues a download of https://example.com/`name`; its id. Nothing is fetched.
    pub fn add(&mut self, name: &str) -> usize {
        let url = url::Url::parse(&format!("https://example.com/{name}")).unwrap();
        self.app.queue.add_item(vec![url], hyperfetch_core::engine::DownloadOptions::default())
    }

    /// Download `id` running, as far as the window can tell (no engine runs): `ratio` of 100 MiB
    /// in, in four chunks, after a minute at 2 to 3.5 MB/s.
    pub fn progress(&mut self, id: usize, ratio: f64) {
        const TOTAL: u64 = 100 << 20;
        let quarter = TOTAL / 4;
        let chunks: Vec<ChunkSnapshot> = (0..4)
            .map(|n| ChunkSnapshot {
                id: n,
                range_start: n as u64 * quarter,
                range_end: (n as u64 + 1) * quarter - 1,
                downloaded_bytes: (ratio * quarter as f64) as u64,
                total_bytes: quarter,
                status: if ratio >= 1.0 { "Completed".to_string() } else { format!("Worker {n}") },
                worker_id: Some(n),
            })
            .collect();
        self.app.queue.mark_started(id);
        let snapshot = hyperfetch_core::engine::EngineSnapshot {
            total_bytes: TOTAL,
            downloaded_bytes: (ratio * TOTAL as f64) as u64,
            speed_bytes_per_sec: 3e6,
            progress_ratio: ratio,
            chunks: chunks.clone(),
            ..Default::default()
        };
        self.app.queue.apply_snapshot(id, &snapshot);
        let now = Instant::now();
        let speed_history = (0..60).map(|s| (now - Duration::from_secs(60 - s), 2e6 + 1.5e6 * (s as f64 / 6.0).sin().abs())).collect();
        let running = crate::Running { cancel: Default::default(), task: self._rt.spawn(async {}), snapshot: Default::default(), live: Default::default() };
        let view = crate::JobView { running: Some(running), chunks, speed_history, active_workers: 4, got_snapshot: true, ..Default::default() };
        self.app.jobs.insert(id, view);
    }

    /// Download `id` done: completed, or failed with `error`.
    pub fn finish(&mut self, id: usize, error: Option<&str>) {
        if let Some(view) = self.app.jobs.get_mut(&id) {
            view.running = None;
        }
        let path = std::env::temp_dir().join(self.app.queue.get_item(id).unwrap().filename.clone());
        self.app.queue.finish(id, error.map_or(Ok((path, Some(100 << 20))), |error| Err(error.to_string())));
    }

    // ---- Painting --------------------------------------------------------------------------

    fn set_texture(&mut self, id: TextureId, delta: ImageDelta) {
        let (size, texels) = match delta.image {
            ImageData::Color(image) => (image.size, image.pixels.clone()),
            ImageData::Font(font) => (font.size, font.srgba_pixels(None).collect()),
        };
        let Some([x, y]) = delta.pos else {
            self.textures.insert(id, Texture { size, texels });
            return;
        };
        let texture = self.textures.get_mut(&id).expect("a patch of a texture egui sent before");
        for (row, patch) in texels.chunks_exact(size[0]).enumerate() {
            let start = (y + row) * texture.size[0] + x;
            texture.texels[start..start + size[0]].copy_from_slice(patch);
        }
    }

    /// The last frame as the screen would show it, `ppp` pixels a point, cut to `area` (in
    /// points) when given.
    pub fn shot(&self, area: Option<Rect>) -> RgbaImage {
        let ppp = self.output.pixels_per_point;
        let [w, h] = [self.size.x, self.size.y].map(|side| (side * ppp).round() as usize);
        let mut pixels = vec![rgba(self.app.ctx.style().visuals.panel_fill); w * h];
        let meshes: Vec<_> = (self.app.ctx.tessellate(self.output.shapes.clone(), ppp).into_iter())
            .filter_map(|clipped| {
                let Primitive::Mesh(mesh) = clipped.primitive else { return None };
                let clip = clipped.clip_rect;
                let [x0, x1] = [clip.min.x, clip.max.x].map(|x| (x * ppp).clamp(0.0, w as f32).round() as usize);
                let [y0, y1] = [clip.min.y, clip.max.y].map(|y| (y * ppp).clamp(0.0, h as f32).round() as usize);
                Some(([x0, y0, x1, y1], mesh))
            })
            .collect();
        // Bands of rows, painted side by side: a thread each.
        let band = h.div_ceil(std::thread::available_parallelism().map_or(1, |n| n.get()));
        let (meshes, textures) = (&meshes, &self.textures);
        std::thread::scope(|scope| {
            for (n, rows) in pixels.chunks_mut(band * w).enumerate() {
                scope.spawn(move || {
                    let (top, bottom) = (n * band, n * band + rows.len() / w);
                    for ([x0, y0, x1, y1], mesh) in meshes {
                        let clip = [*x0, (*y0).max(top), *x1, (*y1).min(bottom)];
                        for triangle in mesh.indices.chunks_exact(3).filter(|_| clip[1] < clip[3]) {
                            let corners = [0, 1, 2].map(|k| &mesh.vertices[triangle[k] as usize]);
                            fill(rows, w, top, clip, corners, ppp, &textures[&mesh.texture_id]);
                        }
                    }
                });
            }
        });
        let image = RgbaImage::from_fn(w as u32, h as u32, |x, y| {
            let [r, g, b, _] = pixels[y as usize * w + x as usize].map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
            image::Rgba([r, g, b, 255])
        });
        match area {
            Some(area) => {
                let [x, y, right, bottom] = [area.min.x, area.min.y, area.max.x, area.max.y].map(|v| (v * ppp).round().max(0.0) as u32);
                let (right, bottom) = (right.min(w as u32), bottom.min(h as u32));
                image::imageops::crop_imm(&image, x, y, right - x, bottom - y).to_image()
            }
            None => image,
        }
    }

    /// The frames `ms` milliseconds after the last one (0: it), cut to `area`, side by side: an
    /// animation in one picture.
    pub fn frame_strip(&mut self, ms: &[u64], area: Option<Rect>) -> RgbaImage {
        let start = self.time;
        let shots: Vec<RgbaImage> = ms
            .iter()
            .map(|&ms| {
                while self.time + 1e-6 < start + ms as f64 / 1000.0 {
                    self.frame(Vec::new());
                }
                self.shot(area)
            })
            .collect();
        side_by_side(&shots)
    }
}

/// `images` left to right, a grey gap between them.
pub fn side_by_side(images: &[RgbaImage]) -> RgbaImage {
    const GAP: u32 = 6;
    let width = images.iter().map(|i| i.width() + GAP).sum::<u32>().saturating_sub(GAP);
    let height = images.iter().map(RgbaImage::height).max().unwrap_or(0);
    let mut out = RgbaImage::from_pixel(width, height, image::Rgba([128, 128, 128, 255]));
    let mut x = 0;
    for image in images {
        image::imageops::replace(&mut out, image, x as i64, 0);
        x += image.width() + GAP;
    }
    out
}

/// Saves `image` as `name`.png in the folder `ENDO_CAPTURE_DIR` names, else `target/ui-captures`.
pub fn save(image: &RgbaImage, name: &str) -> PathBuf {
    let dir = std::env::var_os("ENDO_CAPTURE_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-captures"), PathBuf::from);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.png"));
    image.save(&path).unwrap();
    path
}

fn pointer_button(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
}

/// Whether a text shown as `text` is `label`, on its own or after an icon.
fn shows(text: &str, label: &str) -> bool {
    text == label || text.split_once(' ').is_some_and(|(glyph, rest)| rest == label && glyph.chars().count() == 1)
}

fn rgba(color: Color32) -> [f32; 4] {
    color.to_array().map(|c| c as f32 / 255.0)
}

/// The texel at `uv`, blended between the four nearest (as the GPU's linear filter does).
fn sample(texture: &Texture, uv: Pos2) -> [f32; 4] {
    let [w, h] = texture.size;
    let (x, y) = (uv.x * w as f32 - 0.5, uv.y * h as f32 - 0.5);
    let (fx, fy) = (x - x.floor(), y - y.floor());
    let at = |dx: f32, dy: f32| {
        let column = ((x.floor() + dx) as isize).clamp(0, w as isize - 1) as usize;
        let row = ((y.floor() + dy) as isize).clamp(0, h as isize - 1) as usize;
        rgba(texture.texels[row * w + column])
    };
    let mix = |a: [f32; 4], b: [f32; 4], t: f32| [0, 1, 2, 3].map(|c| a[c] + (b[c] - a[c]) * t);
    mix(mix(at(0.0, 0.0), at(1.0, 0.0), fx), mix(at(0.0, 1.0), at(1.0, 1.0), fx), fy)
}

/// Paints one triangle of a mesh into `rows` (`w` wide, the first at `top`) inside `clip` (x0,
/// y0, x1, y1 in pixels): its texture times its corners' colours, both in gamma space, blended
/// over what is there with premultiplied alpha, as egui_glow does.
fn fill(rows: &mut [[f32; 4]], w: usize, top: usize, clip: [usize; 4], mut corners: [&Vertex; 3], ppp: f32, texture: &Texture) {
    let edge = |a: Vec2, b: Vec2, q: Vec2| (b.x - a.x) * (q.y - a.y) - (b.y - a.y) * (q.x - a.x);
    let mut p = corners.map(|v| v.pos.to_vec2() * ppp);
    let area = edge(p[0], p[1], p[2]);
    if area == 0.0 {
        return;
    }
    if area < 0.0 {
        p.swap(1, 2);
        corners.swap(1, 2);
    }
    let area = area.abs();
    // A pixel right on an edge two triangles share belongs to one of them only.
    let owns = |a: Vec2, b: Vec2| b.y > a.y || (b.y == a.y && b.x > a.x);
    let colors = corners.map(|v| rgba(v.color));
    let plain = corners.iter().all(|v| v.uv == egui::epaint::WHITE_UV);
    let [x0, x1] = [p.iter().map(|q| q.x).fold(f32::INFINITY, f32::min), p.iter().map(|q| q.x).fold(0.0, f32::max)];
    let [y0, y1] = [p.iter().map(|q| q.y).fold(f32::INFINITY, f32::min), p.iter().map(|q| q.y).fold(0.0, f32::max)];
    let xs = (x0.floor().max(0.0) as usize).max(clip[0])..(x1.ceil() as usize).min(clip[2]);
    for y in (y0.floor().max(0.0) as usize).max(clip[1])..(y1.ceil() as usize).min(clip[3]) {
        'pixel: for x in xs.clone() {
            let q = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
            let mut weights = [0.0; 3];
            for (k, (i, j)) in [(1, 2), (2, 0), (0, 1)].into_iter().enumerate() {
                let e = edge(p[i], p[j], q);
                if e < 0.0 || (e == 0.0 && !owns(p[i], p[j])) {
                    continue 'pixel;
                }
                weights[k] = e / area;
            }
            let mut src = [0, 1, 2, 3].map(|c| (0..3).map(|k| colors[k][c] * weights[k]).sum::<f32>());
            if !plain {
                let uv = (0..3).fold(Pos2::ZERO, |uv, k| uv + corners[k].uv.to_vec2() * weights[k]);
                let texel = sample(texture, uv);
                src = [0, 1, 2, 3].map(|c| src[c] * texel[c]);
            }
            let dst = &mut rows[(y - top) * w + x];
            *dst = [0, 1, 2, 3].map(|c| src[c] + dst[c] * (1.0 - src[3]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How far apart two colours are, the largest of their channels' differences.
    fn apart(pixel: [u8; 4], color: Color32) -> u8 {
        [pixel[0].abs_diff(color.r()), pixel[1].abs_diff(color.g()), pixel[2].abs_diff(color.b())].into_iter().max().unwrap()
    }

    /// The rasterizer paints what egui draws: the rail's colour on the left (over the still
    /// background), the background's dark wash on the right, light text on the dark theme; at 150 % the picture is
    /// half as big again.
    #[test]
    fn frames_paint_like_the_screen() {
        let mut h = Harness::new();
        assert!(h.settle());
        let shot = h.shot(None);
        assert_eq!((shot.width(), shot.height()), (980, 720));
        let at = |x: u32, y: u32| shot.get_pixel(x, y).0;
        assert!(apart(at(100, 400), DARK.side) < 12, "the rail: {:?}", at(100, 400));
        // The page shows the background's still wash through it: dark, a little blue.
        assert!(at(970, 710)[..3].iter().all(|&c| c < 90) && at(970, 710)[2] >= at(970, 710)[0], "the page: {:?}", at(970, 710));
        let title = h.find("Nothing downloading yet").unwrap();
        let brightest = (title.min.x as u32..title.max.x as u32).flat_map(|x| (title.min.y as u32..title.max.y as u32).map(move |y| (x, y)));
        assert!(brightest.map(|(x, y)| at(x, y)[0]).max().unwrap() > 200, "the title's light text shows");

        h.ppp = 1.5;
        h.settle();
        let shot = h.shot(Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 50.0))));
        assert_eq!((shot.width(), shot.height()), (150, 75));
    }

    /// The moving background asks for a frame every 33 ms while it is on and the window is seen
    /// and focused; none while the window is in the background, minimized, hidden, the background
    /// off or motion reduced. An idle window with it off asks for none.
    #[test]
    fn the_background_moves_only_while_it_is_on_and_seen() {
        let mut h = Harness::new();
        assert!(h.settle());
        assert_eq!(h.frames(10), Duration::MAX, "off: still");
        h.app.settings.moving_background = true;
        assert!(super::backdrop::tests::paced(h.frames(10)), "on and focused");
        h.focused = false;
        assert_eq!(h.frames(10), Duration::MAX, "in the background");
        h.focused = true;
        h.minimized = true;
        assert_eq!(h.frames(10), Duration::MAX, "minimized");
        h.minimized = false;
        h.app.hide_window();
        assert_eq!(h.frames(10), Duration::MAX, "hidden in the tray");
        h.app.show_window();
        h.app.settings.reduce_motion = Some(true);
        assert_eq!(h.frames(10), Duration::MAX, "reduced");
        h.app.settings.reduce_motion = Some(false);
        assert!(super::backdrop::tests::paced(h.frames(10)), "on again");
    }

    fn area(x0: f32, y0: f32, x1: f32, y1: f32) -> Option<Rect> {
        Some(Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1)))
    }

    /// Twenty downloads in every state: waiting, running at different speeds and places, paused,
    /// failed, a link expired, done, a High and a Low one; long and right-to-left names.
    fn busy(h: &mut Harness) {
        let names = [
            "ubuntu-24.04.2-desktop-amd64.iso",
            "Big Buck Bunny 4K.mp4",
            "fedora-workstation-41.iso",
            "blender-4.3.2-windows-x64.zip",
            "linux-6.12.tar.xz",
            "debian-12.9.0-amd64-netinst.iso",
            "podcast episode 214 - the long one.mp3",
            "openstreetmap-monaco-latest.osm.pbf",
            "gpt2-model.safetensors",
            "LibreOffice_24.8.4_Win_x86-64.msi",
            "archlinux-2025.01.01-x86_64.iso",
            "pexels-sunset-timelapse.mov",
            "rust-1.84.0-x86_64-pc-windows-msvc.msi",
            "Attention Is All You Need.pdf",
            "steam-deck-recovery-4.img.bz2",
            "very long name of a file that goes on and on so that it has to be cut short somewhere.tar.gz",
            "\u{627}\u{644}\u{62a}\u{642}\u{631}\u{64a}\u{631} 2024.pdf",
            "node-v22.13.0-x64.msi",
            "sodium-fabric-0.6.6.jar",
            "photos-2024-backup.7z",
        ];
        let ids: Vec<usize> = names.iter().map(|name| h.add(&name.replace(' ', "%20"))).collect();
        for (n, &id) in ids.iter().enumerate() {
            match n % 10 {
                0 | 3 | 6 => h.progress(id, 0.15 + 0.04 * n as f64),
                1 => {
                    h.progress(id, 1.0);
                    h.finish(id, None);
                }
                2 => {
                    h.progress(id, 0.4);
                    h.finish(id, Some("The server answered 404 Not Found"));
                }
                4 => {
                    h.app.queue.mark_started(id);
                    h.app.queue.mark_pausing(id);
                    h.app.queue.finish(id, Err(hyperfetch_core::engine::CANCELLED.to_string()));
                }
                5 => h.app.queue.get_item_mut(id).unwrap().priority = hyperfetch_core::queue::Priority::High,
                7 => h.app.queue.get_item_mut(id).unwrap().priority = hyperfetch_core::queue::Priority::Low,
                8 => {
                    h.progress(id, 0.3);
                    h.finish(id, Some("https://cdn.example/f?sig=old: HTTP 403 Forbidden"));
                }
                _ => {}
            }
        }
        // Different speeds.
        for (n, &id) in ids.iter().enumerate() {
            if let Some(item) = h.app.queue.get_item_mut(id).filter(|item| item.status == QueueItemStatus::Downloading) {
                item.speed_bytes_per_sec = 1.2e6 * (1 + n % 7) as f64;
            }
        }
    }

    /// A Library of `n` downloads over the last weeks, a few failed.
    fn library(h: &mut Harness, n: usize) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        for k in 0..n {
            let name = format!("download-{k:03}.{}", ["iso", "zip", "mp4", "pdf", "tar.gz"][k % 5]);
            let urls = vec![format!("https://mirror.example.org/{name}")];
            let mut entry = HistoryEntry::new(name.clone(), std::env::temp_dir().join(&name), (k as u64 + 1) * (7 << 20), urls);
            entry.completed_at = Some(now - (k as u64 * k as u64 * 1800));
            entry.blake3_hash = Some("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262".to_string());
            if k % 9 == 4 {
                entry.status = HistoryStatus::Failed("The server answered 404 Not Found".to_string());
            }
            h.app.history.push(entry);
        }
    }

    /// The wheel turned `by` points down over the middle of the window, settled.
    fn wheel(h: &mut Harness, by: f32) {
        h.hover(Pos2::new(h.size.x * 0.6, h.size.y * 0.6));
        h.frame(vec![egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: Vec2::new(0.0, -by), modifiers: Modifiers::NONE }]);
        h.frames(40);
        h.leave();
    }

    /// Every page, card and animation of the window as pictures into `ENDO_CAPTURE_DIR`, else
    /// target/ui-captures: the pages at 100, 125 and 150 %, in both themes, busy and empty, wide
    /// and narrow; the link bar's card for each kind of link; a download's details beside the list
    /// and in its place; the moving background at each intensity; and frame strips of the motion
    /// (file names end in the theme).
    #[test]
    #[ignore = "writes pictures to look at; run with --ignored"]
    fn capture_gallery() {
        for (theme, dark) in [(1, "dark"), (2, "light")] {
            // A settled window in the theme, `size` points big, with `setup` done.
            let scene = |size: Vec2, setup: &dyn Fn(&mut Harness)| {
                let mut h = Harness::new();
                h.size = size;
                h.app.settings.theme = theme;
                setup(&mut h);
                // A second for things to come in (a running download never settles).
                h.frames(60);
                h
            };
            let shoot = |h: &mut Harness, name: &str, ms: &[u64], area: Option<Rect>| {
                save(&h.frame_strip(ms, area), &format!("{name}-{dark}"));
            };
            let still = |h: &mut Harness, name: &str| {
                save(&h.shot(None), &format!("{name}-{dark}"));
            };
            let wide = Vec2::new(1440.0, 900.0);

            // Pages.
            let mut h = scene(SCREEN, &|_| {});
            still(&mut h, "downloads-empty");
            let mut h = scene(SCREEN, &|h| h.app.settings.welcomed = false);
            still(&mut h, "downloads-welcome");
            let mut h = scene(SCREEN, &busy);
            still(&mut h, "downloads-busy");
            let mut h = scene(wide, &|h| {
                busy(h);
                h.app.settings.moving_background = true;
            });
            still(&mut h, "downloads-busy-wide");
            let mut h = scene(wide, &|h| {
                busy(h);
                h.app.focused = Some(1);
                h.app.selected = Some(1);
            });
            still(&mut h, "detail-beside");
            let mut h = scene(SCREEN, &|h| {
                busy(h);
                h.app.focused = Some(1);
            });
            still(&mut h, "detail-in-place");
            wheel(&mut h, 600.0);
            still(&mut h, "detail-in-place-lower");
            let mut h = scene(SCREEN, &|h| {
                busy(h);
                h.app.focused = Some(3);
            });
            still(&mut h, "detail-failed");
            let mut h = scene(SCREEN, &|h| {
                library(h, 60);
                h.app.tab = Tab::Library;
            });
            still(&mut h, "library");
            wheel(&mut h, 900.0);
            still(&mut h, "library-scrolled");
            let mut h = scene(SCREEN, &|h| h.app.tab = Tab::Library);
            still(&mut h, "library-empty");
            let mut h = scene(SCREEN, &|h| h.app.tab = Tab::Sources);
            still(&mut h, "sources");
            wheel(&mut h, 1100.0);
            still(&mut h, "sources-lower");
            let mut h = scene(SCREEN, &|h| h.app.tab = Tab::Settings);
            still(&mut h, "settings");
            let mut h = scene(SCREEN, &|h| {
                h.app.tab = Tab::Settings;
                h.app.settings_search = "speed".to_string();
            });
            still(&mut h, "settings-search");
            let mut h = scene(SCREEN, &busy);
            super::command::open(&h.app.ctx);
            h.frames(2);
            h.type_text("pa");
            h.frames(30);
            still(&mut h, "palette");
            let mut h = scene(Vec2::new(760.0, 640.0), &busy);
            still(&mut h, "narrow");
            let mut h = scene(SCREEN, &busy);
            h.app.notice = Some(Err("Could not reach the server: the connection timed out. Check your internet connection, then Retry.".to_string()));
            h.frames(30);
            still(&mut h, "error-banner");

            // The link bar's card for each kind of link.
            let links = [
                ("file", "https://ftp.gnu.org/gnu/hello/hello-2.12.1.tar.gz"),
                ("video", "https://www.youtube.com/watch?v=aqz-KE-bpKQ"),
                ("playlist", "https://www.youtube.com/@BlenderOfficial/videos"),
                ("torrent", "https://webtorrent.io/torrents/big-buck-bunny.torrent"),
                ("magnet", "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny"),
                ("stream", "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8"),
                ("zip", "https://example.com/pack.zip"),
                ("paper", "https://arxiv.org/abs/1706.03762"),
                ("container", "docker.io/library/alpine:latest"),
                ("page", "https://www.gnu.org/philosophy/free-sw.html"),
                ("several", "https://example.com/a.iso\nhttps://example.com/b.iso\nhttps://example.com/c.iso"),
            ];
            let top = area(0.0, 0.0, SCREEN.x, 420.0);
            for (name, link) in links {
                let h = scene(SCREEN, &|h| h.app.url_input = link.to_string());
                save(&h.shot(top), &format!("preview-{name}-{dark}"));
            }
            let mut h = scene(SCREEN, &|h| {
                h.app.url_input = links[0].1.to_string();
                h.app.show_advanced = true;
            });
            still(&mut h, "preview-more-options");
            // Saving a web page as one file: its card with More options, while it saves, and done.
            let page = "https://www.gnu.org/philosophy/free-sw.html";
            let mut h = scene(SCREEN, &|h| {
                h.app.url_input = page.to_string();
                h.app.show_advanced = true;
            });
            still(&mut h, "page-more-options");
            let mut h = scene(SCREEN, &|h| {
                h.app.url_input = page.to_string();
                h.app.saving_page = Some(page.to_string());
            });
            still(&mut h, "page-saving");
            let mut h = scene(SCREEN, &|h| {
                h.app.notice = Some(Ok("Saved the page as one file: What is Free Software.html (412.0 KiB; 9 images, styles and frames put in)".to_string()));
            });
            still(&mut h, "page-saved");

            // The moving background at each intensity, on the empty page.
            for (intensity, name) in [(0, "calm"), (1, "normal"), (2, "lively")] {
                let mut h = scene(Vec2::new(1280.0, 800.0), &|h| {
                    h.app.settings.moving_background = true;
                    h.app.settings.background_intensity = intensity;
                });
                still(&mut h, &format!("background-{name}"));
            }
            let mut h = scene(Vec2::new(1280.0, 800.0), &|h| {
                h.app.settings.moving_background = true;
                busy(h);
                h.app.tab = Tab::Sources;
            });
            shoot(&mut h, "background-current", &[0, 500, 1000], None);

            // Motion.
            let mut h = scene(SCREEN, &|_| {});
            h.click("Library");
            shoot(&mut h, "page-change", &[0, 33, 67, 133, 250], None);
            let mut h = scene(SCREEN, &|_| {});
            h.click(if theme == 1 { icon::SUN } else { icon::MOON });
            shoot(&mut h, "theme-switch", &[0, 50, 100, 200, 350], None);
            let mut h = scene(SCREEN, &|_| {});
            let input = h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect;
            h.click_at(input.center());
            h.type_text("https://ftp.gnu.org/gnu/hello/hello-2.12.1.tar.gz");
            shoot(&mut h, "preview-appears", &[0, 50, 100, 200, 350], top);
            let mut h = scene(SCREEN, &|_| {});
            h.click_at(input.center());
            h.type_text("not a link");
            h.key(Key::Enter, Modifiers::NONE);
            shoot(&mut h, "form-error-shake", &[0, 50, 100, 150, 250, 500], top);
            let list = area(0.0, 140.0, SCREEN.x, SCREEN.y);
            let four = |h: &mut Harness| ["a.iso", "b.iso", "c.iso", "d.iso"].map(|name| h.add(name));
            let mut h = scene(SCREEN, &|h| {
                four(h);
            });
            h.paste("https://example.com/e.iso");
            shoot(&mut h, "card-added", &[0, 50, 100, 250], list);
            let mut h = scene(SCREEN, &|h| {
                four(h);
            });
            h.click_at(h.find_near("b.iso", icon::X).unwrap().center());
            shoot(&mut h, "card-removed", &[0, 67, 133, 200, 267, 400], list);
            let mut h = scene(SCREEN, &|h| {
                let [a, b, ..] = four(h);
                h.progress(a, 0.3);
                h.progress(b, 0.95);
            });
            h.progress(1, 0.7);
            h.finish(2, None);
            shoot(&mut h, "card-progress-and-complete", &[0, 67, 133, 200, 350, 600], list);
            let mut h = scene(SCREEN, &|h| {
                let [_, b, ..] = four(h);
                h.progress(b, 0.6);
            });
            h.finish(2, Some("The server answered 404 Not Found"));
            shoot(&mut h, "card-fail", &[0, 33, 67, 133, 250, 500], list);
            let mut h = scene(SCREEN, &|h| {
                four(h);
            });
            h.key(Key::ArrowDown, Modifiers::NONE);
            h.frames(20);
            h.key(Key::ArrowDown, Modifiers::NONE);
            shoot(&mut h, "focus-ring-glide", &[0, 50, 100, 200], list);
            h.hover(h.find("c.iso").unwrap().center());
            shoot(&mut h, "card-hover-lift", &[0, 50, 100, 200], list);
            let mut h = scene(SCREEN, &|h| {
                let id = h.add("ubuntu.iso");
                h.progress(id, 0.5);
                h.app.focused = Some(id);
            });
            let quarter = (100u64 << 20) / 4;
            let chunks = &mut h.app.jobs.get_mut(&1).unwrap().chunks;
            (chunks[1].range_end, chunks[1].total_bytes) = (quarter * 7 / 4 - 1, quarter * 3 / 4);
            let status = "Worker 4".to_string();
            chunks.push(ChunkSnapshot { id: 4, range_start: quarter * 7 / 4, range_end: 2 * quarter - 1, downloaded_bytes: 0, total_bytes: quarter / 4, status, worker_id: Some(4) });
            if let Some(map) = h.app.ctx.read_response(egui::Id::new(("chunk map", 1usize))) {
                shoot(&mut h, "chunk-map-split", &[0, 50, 100, 200, 450], Some(map.rect.expand(8.0)));
            }
            let mut h = scene(SCREEN, &busy);
            h.key(Key::K, Modifiers::COMMAND);
            shoot(&mut h, "palette-opens", &[0, 50, 100, 250], None);
            let mut h = scene(SCREEN, &|_| {});
            h.drag_files(true);
            shoot(&mut h, "drop-zone", &[0, 67, 150, 600], None);
            let mut h = scene(SCREEN, &|h| library(h, 6));
            h.click("Library");
            shoot(&mut h, "library-open", &[0, 50, 100, 200, 350], None);
            h.app.verification = Some(crate::Verification {
                result: hyperfetch_core::verify::BuildVerificationResult {
                    file_path: std::env::temp_dir().join("download-001.zip"),
                    expected_size: Some(5 << 20),
                    actual_size: 3 << 20,
                    has_state_file: true,
                    missing_ranges: Vec::new(),
                    is_complete: false,
                    checksum_match: None,
                    status_message: "2 MiB are missing; the download that made it can fill them in.".to_string(),
                },
                repair_urls: vec![url::Url::parse("https://example.com/download-001.zip").unwrap()],
            });
            shoot(&mut h, "verification", &[0, 50, 100, 250], area(0.0, 0.0, SCREEN.x, 360.0));

            // Every page at each display scale.
            for ppp in [1.0, 1.25, 1.5] {
                let mut h = scene(SCREEN, &busy);
                h.ppp = ppp;
                let shots: Vec<RgbaImage> = [Tab::Downloads, Tab::Library, Tab::Sources, Tab::Settings]
                    .into_iter()
                    .map(|tab| {
                        h.app.tab = tab;
                        h.frames(30);
                        h.shot(None)
                    })
                    .collect();
                save(&side_by_side(&shots), &format!("pages-{}-{dark}", (ppp * 100.0) as u32));
            }
        }
    }

    /// How long the window takes to draw a frame (its UI and the tessellation that turns it into
    /// triangles, both on the UI thread) with twenty downloads, most running, and the moving
    /// background on, over 600 frames. Printed, and added to the CI job's summary; run in release.
    #[test]
    #[ignore = "measures; run with --release --ignored --nocapture"]
    fn frame_time() {
        let mut h = Harness::new();
        h.size = Vec2::new(1440.0, 900.0);
        h.app.settings.moving_background = true;
        busy(&mut h);
        for id in 1..=20 {
            if h.app.queue.get_item(id).is_some_and(|item| item.status == QueueItemStatus::Queued) && id % 3 != 0 {
                h.progress(id, 0.05 * id as f64);
            }
        }
        h.frames(60);
        let mut times: Vec<f64> = (0..600)
            .map(|n| {
                // Bytes keep coming: every frame a download moves on.
                let id = 1 + n % 20;
                if let Some(item) = h.app.queue.get_item_mut(id).filter(|item| item.status == QueueItemStatus::Downloading) {
                    item.downloaded_bytes += 1 << 16;
                    item.progress_ratio = (item.progress_ratio + 0.0005).min(0.99);
                }
                let start = Instant::now();
                h.frame(Vec::new());
                let _ = h.app.ctx.tessellate(h.output.shapes.clone(), h.output.pixels_per_point);
                start.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        times.sort_by(f64::total_cmp);
        let running = h.app.queue.items().iter().filter(|item| item.status == QueueItemStatus::Downloading).count();
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        let build = if cfg!(debug_assertions) { "debug" } else { "release" };
        let line = format!(
            "Frame time over 600 frames, 20 downloads ({running} running), moving background on, 1440x900, {build} build: mean {mean:.2} ms, median {:.2} ms, 95th percentile {:.2} ms, worst {:.2} ms",
            times[300], times[570], times[599]
        );
        println!("{line}");
        if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
            use std::io::Write;
            let _ = std::fs::OpenOptions::new().append(true).open(summary).and_then(|mut file| writeln!(file, "{line}"));
        }
    }
}
