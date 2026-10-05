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
        let settings = Settings { save_dir, theme: 1, reduce_motion: Some(false), ..Settings::default() };
        Self {
            app: App::with(ctx, rt.handle().clone(), settings, DownloadQueue::new(), None),
            output: FullOutput::default(),
            time: 0.0,
            ppp: 1.0,
            minimized: false,
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
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
            time: Some(self.time),
            predicted_dt: DT as f32,
            modifiers: self.modifiers,
            events,
            hovered_files: self.hovered_files.clone(),
            focused: true,
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
        let running = crate::Running { cancel: Default::default(), task: self._rt.spawn(async {}), snapshot: Default::default() };
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
        let [w, h] = [SCREEN.x, SCREEN.y].map(|side| (side * ppp).round() as usize);
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

    /// The rasterizer paints what egui draws: the sidebar's colour on the left, the page's on the
    /// right, light text on the dark theme; at 150 % the picture is half as big again.
    #[test]
    fn frames_paint_like_the_screen() {
        let mut h = Harness::new();
        assert!(h.settle());
        let shot = h.shot(None);
        assert_eq!((shot.width(), shot.height()), (980, 720));
        let at = |x: u32, y: u32| shot.get_pixel(x, y).0;
        assert_eq!(at(100, 400), [DARK.side.r(), DARK.side.g(), DARK.side.b(), 255]);
        assert_eq!(at(970, 710), [DARK.bg.r(), DARK.bg.g(), DARK.bg.b(), 255]);
        let title = h.find("Add a download").unwrap();
        let brightest = (title.min.x as u32..title.max.x as u32).flat_map(|x| (title.min.y as u32..title.max.y as u32).map(move |y| (x, y)));
        assert!(brightest.map(|(x, y)| at(x, y)[0]).max().unwrap() > 230, "the title's white text shows");

        h.ppp = 1.5;
        h.settle();
        let shot = h.shot(Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 50.0))));
        assert_eq!((shot.width(), shot.height()), (150, 75));
    }

    fn area(x0: f32, y0: f32, x1: f32, y1: f32) -> Option<Rect> {
        Some(Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1)))
    }

    /// Every animation of the window as frame strips (see [`Harness::frame_strip`]; file names
    /// end in the theme), and every page at 100, 125 and 150 %, into `ENDO_CAPTURE_DIR`, else
    /// target/ui-captures.
    #[test]
    #[ignore = "writes pictures to look at; run with --ignored"]
    fn capture_gallery() {
        const SIDEBAR: Option<Rect> = Some(Rect { min: Pos2::ZERO, max: Pos2::new(196.0, 260.0) });
        let top = area(196.0, 0.0, 980.0, 280.0);
        let page = area(196.0, 0.0, 980.0, 720.0);
        for (theme, dark) in [(1, "dark"), (2, "light")] {
            // A settled window in the theme, on `tab`, with `setup` done.
            let scene = |tab: Tab, setup: &dyn Fn(&mut Harness)| {
                let mut h = Harness::new();
                h.app.settings.theme = theme;
                h.app.tab = tab;
                setup(&mut h);
                // A second for things to come in (a running download never settles).
                h.frames(60);
                h
            };
            let shoot = |h: &mut Harness, name: &str, ms: &[u64], area: Option<Rect>| {
                save(&h.frame_strip(ms, area), &format!("{name}-{dark}"));
            };
            let four = |h: &mut Harness| ["a.iso", "b.iso", "c.iso", "d.iso"].map(|name| h.add(name));

            // Shared: page change, theme cross-fade, buttons, icon buttons, switches, banners.
            let mut h = scene(Tab::Downloader, &|_| {});
            h.click("Queue");
            shoot(&mut h, "page-change", &[0, 33, 67, 133, 250], top);
            let mut h = scene(Tab::Downloader, &|_| {});
            h.click(if theme == 1 { icon::SUN } else { icon::MOON });
            shoot(&mut h, "theme-switch", &[0, 50, 100, 200, 350], None);
            let mut h = scene(Tab::Queue, &|_| {});
            // The button, not the card's title of the same name.
            let button = h.find(&format!("{} Add to queue", icon::PLUS)).unwrap();
            h.hover(button.center());
            shoot(&mut h, "button-hover", &[0, 33, 67, 150], Some(button.expand2(Vec2::new(70.0, 14.0))));
            h.press(button.center());
            shoot(&mut h, "button-press", &[0, 33, 100], Some(button.expand2(Vec2::new(70.0, 14.0))));
            let mut h = scene(Tab::Queue, &|h| {
                four(h);
            });
            let x = h.find_near("b.iso", icon::X).unwrap();
            h.hover(x.center());
            shoot(&mut h, "icon-button-hover", &[0, 33, 67, 150], Some(x.expand2(Vec2::new(120.0, 24.0))));
            let mut h = scene(Tab::Settings, &|_| {});
            let row = h.find("Run the queue automatically").unwrap();
            h.click_at(h.find_near("Run the queue automatically", TOGGLE).unwrap().center());
            shoot(&mut h, "toggle-and-row-flash", &[0, 50, 100, 200, 500, 900], area(196.0, row.min.y - 20.0, 980.0, row.max.y + 30.0));
            let mut h = scene(Tab::Downloader, &|_| {});
            h.paste("https://example.com/a.iso");
            shoot(&mut h, "banner-added-badge", &[0, 33, 83, 167, 400, 1000], area(0.0, 0.0, 980.0, 260.0));
            let mut h = scene(Tab::Downloader, &|h| h.app.notice = Some(Ok("Added #1 to the queue".to_string())));
            h.click_at(h.find_near("Added #1 to the queue", icon::X).unwrap().center());
            shoot(&mut h, "banner-dismiss", &[0, 50, 100, 150, 250], top);

            // Sidebar: pill, hover, logo.
            let mut h = scene(Tab::Downloader, &|_| {});
            h.click("Settings");
            shoot(&mut h, "sidebar-pill", &[0, 50, 100, 150, 250], SIDEBAR);
            let mut h = scene(Tab::Downloader, &|_| {});
            h.hover(h.find("History").unwrap().center());
            shoot(&mut h, "nav-hover", &[0, 33, 67, 150], SIDEBAR);
            let mut h = scene(Tab::Queue, &|h| {
                let id = h.add("a.iso");
                h.progress(id, 0.9);
            });
            h.finish(1, None);
            shoot(&mut h, "logo-bob", &[0, 100, 200, 300, 450, 650], area(0.0, 0.0, 196.0, 64.0));

            // Add page.
            let mut h = scene(Tab::Downloader, &|_| {});
            h.drag_files(true);
            shoot(&mut h, "drop-zone", &[0, 67, 150, 600, 1200], None);
            h.drag_files(false);
            shoot(&mut h, "drop-zone-out", &[0, 50, 100, 200], None);
            let mut h = scene(Tab::Downloader, &|h| {
                let id = h.add("ubuntu.iso");
                h.progress(id, 0.96);
                h.app.focused = Some(id);
            });
            h.finish(1, None);
            shoot(&mut h, "download-complete", &[0, 100, 200, 350, 600], top);
            let mut h = scene(Tab::Downloader, &|h| {
                let id = h.add("gone.bin");
                h.progress(id, 0.4);
                h.app.focused = Some(id);
            });
            h.finish(1, Some("The server answered 404 Not Found"));
            shoot(&mut h, "download-fail-shake", &[0, 50, 100, 150, 250, 500], area(196.0, 60.0, 980.0, 360.0));
            let mut h = scene(Tab::Downloader, &|_| {});
            let input = h.app.ctx.read_response(egui::Id::new("url_input")).unwrap().rect;
            h.click_at(input.center());
            h.type_text("not a link");
            h.key(Key::Enter, Modifiers::NONE);
            shoot(&mut h, "form-error-shake", &[0, 50, 100, 150, 250, 500], area(196.0, 60.0, 980.0, 300.0));
            // Fixed, the alert fades out still.
            h.click_at(input.center());
            h.type_text("x");
            shoot(&mut h, "form-error-cleared", &[0, 50, 100, 150, 250], area(196.0, 60.0, 980.0, 300.0));
            let mut h = scene(Tab::Downloader, &|h| {
                let page = "https://example.com/releases".to_string();
                h.app.update = Some(hyperfetch_core::updater::Update { version: "9.9.0".into(), tag: "v9.9.0".into(), page });
            });
            // As Update and restart does, without fetching anything: the banner stays put.
            h.app.updating = true;
            shoot(&mut h, "update-banner-downloading", &[0, 50, 100, 250], area(196.0, 0.0, 980.0, 180.0));
            let mut h = scene(Tab::Downloader, &|_| {});
            h.click("Advanced options: checksum and Authorization header");
            shoot(&mut h, "advanced-options", &[0, 50, 100, 200], area(196.0, 120.0, 980.0, 420.0));
            let mut h = Harness::new();
            h.app.settings.theme = theme;
            h.frame(Vec::new());
            shoot(&mut h, "tips-first-sight", &[0, 50, 100, 200, 400], area(196.0, 200.0, 980.0, 480.0));

            // Settings.
            let mut h = scene(Tab::Downloader, &|_| {});
            h.click("Settings");
            shoot(&mut h, "settings-open", &[0, 50, 100, 200, 350, 600], page);

            // Queue.
            let list = area(196.0, 300.0, 980.0, 720.0);
            let mut h = scene(Tab::Queue, &|h| {
                four(h);
            });
            h.paste("https://example.com/e.iso");
            shoot(&mut h, "queue-row-added", &[0, 50, 100, 250], list);
            let mut h = scene(Tab::Queue, &|h| {
                four(h);
            });
            h.click_at(h.find_near("b.iso", icon::X).unwrap().center());
            shoot(&mut h, "queue-row-removed", &[0, 67, 133, 200, 267, 400], list);
            let mut h = scene(Tab::Queue, &|h| {
                for name in ["a.iso", "b.iso", "c.iso"] {
                    let id = h.add(name);
                    h.progress(id, 1.0);
                    h.finish(id, None);
                }
                let run = h.add("run.iso");
                h.progress(run, 0.5);
                h.app.selected = Some(run);
            });
            // The selected row slides up into the gap, its highlight on it.
            h.app.clear_queue(true);
            shoot(&mut h, "queue-clear-completed", &[0, 150, 230, 270, 310, 350, 450], list);
            let mut h = scene(Tab::Queue, &|h| {
                let [_, resolving, downloading, failed] = four(h);
                h.app.queue.mark_started(resolving);
                h.progress(downloading, 0.45);
                h.progress(failed, 0.2);
                h.finish(failed, Some("The server answered 404 Not Found"));
                let done = h.add("e.iso");
                h.progress(done, 1.0);
                h.finish(done, None);
            });
            shoot(&mut h, "queue-status-icons", &[0, 250, 500, 750], list);
            let mut h = scene(Tab::Queue, &|h| {
                let [a, b, ..] = four(h);
                h.progress(a, 0.3);
                h.progress(b, 0.95);
            });
            h.progress(1, 0.7);
            h.finish(2, None);
            shoot(&mut h, "queue-progress-and-complete", &[0, 67, 133, 200, 350, 600], list);
            let mut h = scene(Tab::Queue, &|h| {
                let [_, b, ..] = four(h);
                h.progress(b, 0.6);
            });
            h.finish(2, Some("The server answered 404 Not Found"));
            shoot(&mut h, "queue-fail", &[0, 33, 67, 133, 250, 500], list);
            let mut h = scene(Tab::Queue, &|h| {
                four(h);
            });
            h.click("a.iso");
            // Long enough apart not to make a double click.
            h.frames(30);
            h.click("d.iso");
            shoot(&mut h, "queue-selection", &[0, 50, 100, 200], list);
            h.hover(h.find("b.iso").unwrap().center());
            shoot(&mut h, "queue-row-hover", &[0, 33, 67, 150], list);
            let mut h = scene(Tab::Downloader, &|h| {
                let id = h.add("ubuntu.iso");
                h.progress(id, 0.3);
                h.app.focused = Some(id);
            });
            h.progress(1, 0.6);
            shoot(&mut h, "download-details", &[0, 100, 200, 400], page);

            // History.
            let mut h = scene(Tab::Downloader, &|h| {
                for n in 0..6 {
                    let name = format!("file-{n}.zip");
                    let mut entry = HistoryEntry::new(name.clone(), std::env::temp_dir().join(&name), 5 << 20, vec![format!("https://example.com/{name}")]);
                    entry.blake3_hash = Some("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262".to_string());
                    h.app.history.push(entry);
                }
            });
            h.click("History");
            shoot(&mut h, "history-open", &[0, 50, 100, 200, 350, 500], page);
            h.app.verification = Some(crate::Verification {
                result: hyperfetch_core::verify::BuildVerificationResult {
                    file_path: std::env::temp_dir().join("file-1.zip"),
                    expected_size: Some(5 << 20),
                    actual_size: 3 << 20,
                    has_state_file: true,
                    missing_ranges: Vec::new(),
                    is_complete: false,
                    checksum_match: None,
                    status_message: "2 MiB are missing; the download that made it can fill them in.".to_string(),
                },
                repair_urls: vec![url::Url::parse("https://example.com/file-1.zip").unwrap()],
            });
            shoot(&mut h, "verification", &[0, 50, 100, 250], top);

            // Every page at each display scale.
            for ppp in [1.0, 1.25, 1.5] {
                let mut h = scene(Tab::Queue, &|h| {
                    let [a, b, c, _] = four(h);
                    h.progress(a, 0.45);
                    h.progress(b, 1.0);
                    h.finish(b, None);
                    h.progress(c, 0.2);
                    h.finish(c, Some("The server answered 404 Not Found"));
                    h.app.focused = Some(a);
                });
                h.ppp = ppp;
                let shots: Vec<RgbaImage> = [Tab::Downloader, Tab::Queue, Tab::History, Tab::Settings]
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
}
