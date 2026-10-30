use std::collections::{HashMap, HashSet};

use hyperfetch_core::queue::{Priority, Remaining};

use super::*;
use crate::slow::{self, Fix};

const AUTH_REQUIRED: &str =
    "Authorization header not saved: enter it again under Advanced options, then Resume to continue from the partial file.";
/// What the row of a download whose link expired says (see `QueueItem::link_expired`).
const LINK_EXPIRED: &str = "Link expired: get a new link";

/// The page `item` came from, to open: the referer it was sent with, when that is a link.
fn page_of(item: &QueueItem) -> Option<&str> {
    item.options.referer.as_deref().filter(|page| page.starts_with("https://") || page.starts_with("http://"))
}

/// Whether `item` is stopped and over plain links, so that it can take a new one.
fn takes_new_link(item: &QueueItem) -> bool {
    matches!(item.status, QueueItemStatus::Paused | QueueItemStatus::Failed(_)) && item.relinkable()
}

pub(super) fn download_details(app: &mut App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    progress_card(app, ui, item);
    ui.add_space(6.0);
    let (status, color) = status_line(app, item, &p);
    ui.label(RichText::new(status).color(color));
    if takes_new_link(item) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let open = app.new_link.as_ref().is_some_and(|new_link| new_link.id == item.id);
            let about = "Its link expired? Paste a fresh one to the same file: the download resumes where it stopped";
            if !open && ui.add(button(format!("{} New link…", icon::LINK))).on_hover_text(about).clicked() {
                app.new_link = Some(crate::NewLink::new(item.id));
            }
            if let Some(page) = page_of(item) {
                if ui.add(button(format!("{} Open the page", icon::ARROW_SQUARE_OUT))).on_hover_text(page).clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(page));
                }
            }
        });
        new_link_box(app, ui, item);
    }
    // While its bytes come in, it can tell what holds it back.
    if item.status == QueueItemStatus::Downloading && !app.is_resolving(item.id) && !item.is_finishing() {
        let open = egui::Id::new(("why slow", item.id));
        ui.add_space(4.0);
        if ui.add(button(format!("{} Why is it slow?", icon::GAUGE))).clicked() {
            ui.data_mut(|d| {
                let shown = d.get_temp_mut_or_default::<bool>(open);
                *shown = !*shown;
            });
        }
        if ui.data(|d| d.get_temp::<bool>(open)).unwrap_or(false) {
            why_slow(app, ui, item);
        }
    }

    let view = app.jobs.get(&item.id);
    let chunks: &[ChunkSnapshot] = view.map_or(&[], |v| &v.chunks);
    // Once the engine has stopped, chunks it last reported as in flight are no longer being fetched.
    let live = view.is_some_and(|v| v.running.is_some());
    if !chunks.is_empty() {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(bold("Chunks").color(p.strong));
            let about = format!("{} ranges, each in the colour of the connection that holds it", chunks.len());
            ui.label(RichText::new(about).small().color(p.muted));
        });
        ui.add_space(4.0);
        chunk_map(ui, item.id, chunks, item.total_bytes, live);
        ui.add_space(8.0);
        chunk_table(ui, item.id, chunks, live);
    }
    ui.add_space(12.0);
    throughput_graph(ui, item.id, view.map(|view| &view.speed_history), eased_speed(ui, item), live);
}

/// The New link box of stopped download `item`, when open: a fresh link to paste, checked to be
/// the same file before it is taken (see `App::check_new_link`), and why one was refused.
fn new_link_box(app: &mut App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    let Some(new_link) = app.new_link.as_mut().filter(|new_link| new_link.id == item.id) else { return };
    let (mut check, mut cancel) = (false, false);
    ui.add_space(8.0);
    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(bold("New link").color(p.strong));
        let about = "A fresh link to the same file, from the page it came from. It is checked to be the same size first; the \
                     download then resumes where it stopped.";
        ui.label(RichText::new(about).small().color(p.muted));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 190.0).max(120.0);
            let field = egui::TextEdit::singleline(&mut new_link.text).hint_text("https://…").desired_width(width);
            let field = ui.add_enabled(!new_link.checking, field);
            if std::mem::take(&mut new_link.focus) {
                field.request_focus();
                field.scroll_to_me(Some(Align::Center));
            }
            let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let label = if new_link.checking { "Checking…" } else { "Use this link" };
            let ready = !new_link.checking && !new_link.text.trim().is_empty();
            check = ui.add_enabled(ready, primary_button(label, p.accent)).clicked() || entered && ready;
            cancel = ui.add(button("Cancel")).clicked();
        });
        if let Some(why) = &new_link.refused {
            error_alert(ui, why);
        }
    });
    if cancel {
        app.new_link = None;
    } else if check {
        app.check_new_link();
    }
}

/// What holds running download `item` back (see `slow::findings`), coming in one after another,
/// each with the button of its fix when the app has one; or that nothing in the app does.
fn why_slow(app: &mut App, ui: &mut Ui, item: &QueueItem) {
    let p = palette(ui);
    let Some(view) = app.jobs.get(&item.id) else { return };
    let running = || app.queue.items().iter().filter(|other| other.status == QueueItemStatus::Downloading);
    // shortcut: downloads share a server when their links' do, not where those redirect to; the
    // engine's own host slots would be exact.
    let server = |other: &QueueItem| other.urls.first().map(hyperfetch_core::hosts::HostKey::of);
    let on_host: usize = running().filter(|other| server(*other) == server(item)).map(|other| app.jobs.get(&other.id).map_or(0, |v| v.active_workers)).sum();
    let limited: f64 = running().filter(|other| other.options.max_speed == item.options.max_speed).map(|other| other.speed_bytes_per_sec).sum();
    let found = slow::findings(item, view, on_host, limited);
    let mut fix = None;
    ui.add_space(8.0);
    card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(bold("What holds it back").color(p.strong));
        ui.add_space(4.0);
        if found.is_empty() {
            ui.label(RichText::new("Nothing in the app: the server or your internet line sets its speed.").color(p.muted));
        }
        for (n, finding) in found.iter().enumerate() {
            anim::fade_in(ui, egui::Id::new(("why slow", item.id, n)), anim::stagger(n), anim::RISE, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(change) = finding.fix {
                            let name = match change {
                                Fix::NoLimit => "Lift the limit for it".to_string(),
                                Fix::Connections(n) => format!("Use {n} connections"),
                            };
                            if ui.add(button(name)).on_hover_text("Starts it again from where it is, with this change").clicked() {
                                fix = Some(change);
                            }
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.label(RichText::new(icon::INFO).color(p.amber));
                            ui.add(egui::Label::new(finding.text.as_str()).wrap());
                        });
                    });
                });
            });
        }
    });
    if let Some(change) = fix {
        app.restart_with(item.id, |options| match change {
            Fix::NoLimit => options.max_speed = None,
            Fix::Connections(n) => options.num_connections = n,
        });
    }
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
        // Shared with the download's row in the queue, so the bar carries on from where it was.
        let id = egui::Id::new(("item", item.id));
        let fill = if item.status == QueueItemStatus::Completed { p.green } else { p.accent };
        let fill = anim::color(ui.ctx(), id.with("bar"), fill, 0.35);
        let ratio = item.progress_ratio as f32;
        let text = util::progress_text(total, downloaded, eased_progress(ui, id, ratio) as f64, 1, view.map(|_| elapsed));
        let size = Vec2::new(ui.available_width(), 20.0);
        progress_bar(ui, id, size, ratio, fill, downloading, Some(RichText::new(text).color(p.strong).into()));
        ui.add_space(10.0);

        let eta = match item.status {
            QueueItemStatus::Completed => "Done".to_string(),
            _ if downloading => util::eta_secs(total, downloaded, item.speed_bytes_per_sec).map_or_else(|| "--:--".to_string(), format_duration),
            _ => "--:--".to_string(),
        };
        let speed = eased_speed(ui, item);
        ui.columns(4, |cols| {
            let transferred =
                if total > 0 { format!("{} / {}", format_bytes(downloaded), format_bytes(total)) } else { format_bytes(downloaded) };
            metric(&mut cols[0], "Transferred", transferred, None);
            metric(&mut cols[1], "Speed", format!("{}/s", format_bytes(speed as u64)), Some(p.cyan));
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

/// The progress `progress_bar(ui, id, ..)` shows on its way to `ratio`, for numbers that keep up
/// with the bar.
fn eased_progress(ui: &Ui, id: egui::Id, ratio: f32) -> f32 {
    // The same target as the bar's, clamped alike: a different one would restart its easing.
    anim::value(ui.ctx(), id.with("progress"), ratio.clamp(0.0, 1.0), 0.35)
}

/// The download's speed, eased to each new reading.
fn eased_speed(ui: &Ui, item: &QueueItem) -> f64 {
    anim::value(ui.ctx(), egui::Id::new(("item", item.id, "speed")), item.speed_bytes_per_sec as f32, 0.35) as f64
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
        QueueItemStatus::Failed(error) if item.link_expired() => {
            (format!("{}. Its server refused the old one ({}), as it does once a link's time is up.", LINK_EXPIRED, error), p.amber)
        }
        QueueItemStatus::Failed(error) => (format!("Error: {}", error), p.red),
        QueueItemStatus::AuthRequired => (AUTH_REQUIRED.to_string(), p.amber),
    }
}

/// A worker owns the chunk ("Worker n" from the range engine, "Downloading" from the HLS engine).
fn is_active_chunk(chunk: &ChunkSnapshot) -> bool {
    chunk.status.starts_with("Worker") || chunk.status == "Downloading"
}

/// The colour of connection `worker`: the theme's `worker`th, around again past the last.
fn lane(p: &Palette, worker: usize) -> Color32 {
    p.lanes[worker % p.lanes.len()]
}

/// How long a piece split off another (see [`chunk_map`]) takes to turn from its colour to its own.
const SPLIT: f64 = 0.4;

/// What a download's chunk map showed, kept between frames to tell pieces split off others.
#[derive(Clone, Default)]
struct Pieces {
    /// The pass that last drew the map; a gap means it was away and starts over.
    pass: Option<u64>,
    /// The id the next piece split off gets.
    next: usize,
    /// Pieces split off while the map showed: id, when, and the colour of the piece cut.
    split: Vec<(usize, f64, Color32)>,
}

/// The ranges of download `item` side by side, each in the colour of the connection that holds
/// it: each fills smoothly as its bytes come in and turns green when done; one handed out but not
/// yet receiving glows while the download runs. A piece a connection takes over from the end of
/// another's (work stealing) splits off: a light flashes on the cut, and the piece turns from the
/// other's colour to its own.
fn chunk_map(ui: &mut Ui, item: usize, chunks: &[ChunkSnapshot], total_bytes: u64, live: bool) {
    let p = palette(ui);
    let ctx = ui.ctx().clone();
    let height = 22.0;
    let (response, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), height), Sense::hover());
    let rect = response.rect;
    let key = egui::Id::new(("chunk map", item));
    // Where tests find it.
    ui.interact(rect, key, Sense::hover());
    painter.rect_filled(rect, 6.0, p.field);
    if chunks.is_empty() || total_bytes == 0 {
        return;
    }
    let base = |chunk: &ChunkSnapshot| match chunk.worker_id {
        _ if chunk.total_bytes > 0 && chunk.downloaded_bytes >= chunk.total_bytes => p.green,
        Some(worker) => lane(&p, worker),
        None if chunk.status.starts_with("Failed") => p.red,
        None => p.dim,
    };
    // Pieces get the next free id as they are split off; ones that come with the map are no news.
    let (now, pass) = (ctx.input(|i| i.time), ctx.cumulative_pass_nr());
    let mut pieces: Pieces = ctx.data_mut(|d| d.remove_temp(key)).unwrap_or_default();
    if pieces.pass.is_some_and(|last| last + 1 == pass) {
        for chunk in chunks.iter().filter(|chunk| chunk.id >= pieces.next) {
            if let Some(cut) = chunks.iter().find(|other| other.range_end + 1 == chunk.range_start) {
                pieces.split.push((chunk.id, now, base(cut)));
            }
        }
    }
    pieces.split.retain(|&(_, at, _)| !anim::reduced(&ctx) && now - at < SPLIT);
    pieces.pass = Some(pass);
    pieces.next = pieces.next.max(chunks.iter().map(|chunk| chunk.id + 1).max().unwrap_or(0));

    let painter = painter.with_clip_rect(rect.shrink(1.0));
    let x = |bytes: u64| rect.min.x + (bytes as f32 / total_bytes as f32).clamp(0.0, 1.0) * rect.width();
    let glow = if live { anim::pulse(&ctx, 1.8) } else { 0.0 };
    let mut cuts = Vec::new();
    for chunk in chunks {
        let (left, right) = (x(chunk.range_start), x(chunk.range_end + 1));
        let seg_rect = Rect::from_x_y_ranges(left..=right.max(left + 1.0), rect.min.y + 1.0..=rect.max.y - 1.0);
        // Keyed like the chunk table's bars.
        let id = egui::Id::new(("chunk", item, chunk.id));
        let mut color = anim::color(&ctx, id.with("bar"), base(chunk), 0.3);
        if let Some(&(_, at, from)) = pieces.split.iter().find(|split| split.0 == chunk.id) {
            let t = anim::ease(anim::progress(&ctx, (now - at) as f32, 0.0, SPLIT as f32));
            color = from.lerp_to_gamma(color, t);
            cuts.push((left, t));
        }

        if chunk.total_bytes > 0 && chunk.downloaded_bytes > 0 {
            painter.rect_filled(seg_rect, 0.0, color.gamma_multiply(0.3));
            // Where its bytes reach in the file, so that a piece cut shorter keeps its fill still.
            let reach = (chunk.range_start + chunk.downloaded_bytes.min(chunk.total_bytes)) as f32 / total_bytes as f32;
            let reach = rect.min.x + anim::value(&ctx, id.with("reach"), reach.clamp(0.0, 1.0), 0.35) * rect.width();
            painter.rect_filled(Rect::from_x_y_ranges(left..=reach.clamp(left, seg_rect.right()), seg_rect.y_range()), 0.0, color);
        } else if live && is_active_chunk(chunk) {
            // Handed out but no bytes yet.
            painter.rect_filled(seg_rect, 0.0, color.gamma_multiply(0.35 + glow * 0.5));
        } else if chunk.status.starts_with("Failed") {
            painter.rect_filled(seg_rect, 0.0, color.gamma_multiply(0.6));
        } else {
            painter.rect_filled(seg_rect, 0.0, p.border);
        }
        painter.line_segment([seg_rect.right_top(), seg_rect.right_bottom()], Stroke::new(1.0, p.card));
    }
    for (left, t) in cuts {
        let light = Stroke::new(2.0, Color32::WHITE.gamma_multiply(1.0 - t));
        painter.line_segment([Pos2::new(left, rect.min.y + 1.0), Pos2::new(left, rect.max.y - 1.0)], light);
    }
    painter.rect_stroke(rect, 6.0, Stroke::new(1.0, p.border));
    ctx.data_mut(|d| d.insert_temp(key, pieces));
}

fn chunk_table(ui: &mut Ui, item: usize, chunks: &[ChunkSnapshot], live: bool) {
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
                    let id = egui::Id::new(("chunk", item, chunk.id));
                    let percent = format!("{:.0}%", eased_progress(ui, id, ratio) * 100.0);
                    let busy = live && is_active_chunk(chunk);
                    progress_bar(ui, id, Vec2::new(140.0, 18.0), ratio, p.accent, busy, Some(RichText::new(percent).small().into()));
                    let color = if chunk.status == "Completed" {
                        p.green
                    } else if live && is_active_chunk(chunk) {
                        chunk.worker_id.map_or(p.cyan, |worker| lane(&p, worker))
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

/// The speed of download `item` over the last minute. A new sample rises from the one before
/// and the scale eases to a new peak; the newest point glows while the download runs.
fn throughput_graph(ui: &mut Ui, item: usize, history: Option<&VecDeque<(Instant, f64)>>, current: f64, live: bool) {
    let p = palette(ui);
    let id = egui::Id::new(("graph", item));
    let now = Instant::now();
    let window = GRAPH_WINDOW.as_secs_f32();
    // (age in seconds, speed) of the samples inside the window.
    let mut samples: Vec<(f32, f64)> = history
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

        let max_y = anim::value(ui.ctx(), id.with("scale"), (peak * 1.15).max(1024.0 * 1024.0) as f32, 0.4) as f64;
        if let Some((_, speed)) = samples.last_mut() {
            *speed = anim::value(ui.ctx(), id.with("tip"), *speed as f32, 0.3) as f64;
        }
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
                let pulse = if live { anim::pulse(ui.ctx(), 1.8) } else { 0.0 };
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

#[derive(Clone, Copy)]
enum RowAction {
    Select,
    Start,
    Pause,
    Show,
    Remove,
    Open,
    Reveal,
    /// Shows the download with its track chooser open (see `App::list_tracks`).
    Tracks,
    /// Puts it first among the waiting downloads (see `DownloadQueue::download_next`).
    Next,
    /// Moves it to this place of the list.
    MoveTo(usize),
    Priority(Priority),
    /// Shows it with its New link box open (see `App::check_new_link`).
    NewLink,
    /// Opens the page it came from in the browser.
    OpenPage,
}

/// The order and priority of `item`, the `index`th of `len` downloads: Download next, Move up and
/// Move down, and for a waiting one its priority; for a stopped one, New link… and the page it
/// came from. Its row's ⋯ menu and right-click menu.
fn order_menu(ui: &mut Ui, item: &QueueItem, index: usize, len: usize, action: &mut Option<(usize, RowAction)>) {
    let waiting = item.status == QueueItemStatus::Queued;
    let moves = [
        (waiting, icon::ARROW_LINE_UP, "Download next", RowAction::Next),
        (index > 0, icon::ARROW_UP, "Move up", RowAction::MoveTo(index.saturating_sub(1))),
        (index + 1 < len, icon::ARROW_DOWN, "Move down", RowAction::MoveTo(index + 1)),
    ];
    // Download next only where it applies; the others greyed out at the ends.
    for (enabled, glyph, name, what) in moves.into_iter().filter(|&(enabled, _, name, _)| enabled || name != "Download next") {
        if ui.add_enabled(enabled, egui::Button::new(format!("{glyph} {name}"))).clicked() {
            *action = Some((item.id, what));
            ui.close_menu();
        }
    }
    if waiting {
        ui.separator();
        ui.label(RichText::new("Priority: which waiting download starts first").small().color(palette(ui).muted));
        for (priority, name) in [(Priority::High, "High"), (Priority::Normal, "Normal"), (Priority::Low, "Low")] {
            if ui.selectable_label(item.priority == priority, name).clicked() {
                *action = Some((item.id, RowAction::Priority(priority)));
                ui.close_menu();
            }
        }
    }
    if takes_new_link(item) {
        ui.separator();
        let page = page_of(item).map(|_| (icon::ARROW_SQUARE_OUT, "Open the page", RowAction::OpenPage));
        for (glyph, name, what) in std::iter::once((icon::LINK, "New link…", RowAction::NewLink)).chain(page) {
            if ui.button(format!("{glyph} {name}")).clicked() {
                *action = Some((item.id, what));
                ui.close_menu();
            }
        }
    }
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

/// The second line of a queue row: where it is (`ratio` as shown), how fast, how long still, its peers and upload,
/// and what post-processing said.
fn row_summary(app: &App, item: &QueueItem, ratio: f64) -> String {
    let view = app.jobs.get(&item.id);
    let elapsed = view.map(|view| view.elapsed().as_secs());
    let mut parts = vec![format!("#{}", item.id)];
    if item.status == QueueItemStatus::Completed {
        parts.push(format_bytes(item.total_bytes.max(item.downloaded_bytes)));
    } else {
        parts.push(util::progress_text(item.total_bytes, item.downloaded_bytes, ratio, 0, elapsed));
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

/// How long a removed row lingers: fading out for the first half, then closing its gap.
const GHOST: f64 = 0.4;

/// A removed download's row, still fading out.
#[derive(Clone)]
struct Ghost {
    item: QueueItem,
    /// Where it was listed.
    index: usize,
    /// When it was removed.
    since: f64,
}

/// What the queue list showed, kept between frames to tell rows that come and go.
#[derive(Clone, Default)]
struct Rows {
    /// The pass that last drew the list; a gap means the page was away and starts over.
    pass: Option<u64>,
    /// The queue's revision then, and a copy of its downloads (only taken again when it moves).
    revision: u64,
    items: Vec<QueueItem>,
    ghosts: Vec<Ghost>,
    /// Rows that changed places: id, how many rows below its new place it was (above it when
    /// negative), and when.
    moves: Vec<(usize, f32, f64)>,
}

/// How long a row that changed places takes to glide to its new one.
const GLIDE: f64 = 0.3;

/// How many rows below its place (above it when negative) row `id` still shows while it glides
/// there (see [`Rows::moves`]).
fn glide(moves: &[(usize, f32, f64)], id: usize, now: f64) -> f32 {
    moves.iter().find(|m| m.0 == id).map_or(0.0, |&(_, from, since)| from * (1.0 - anim::ease(((now - since) / GLIDE) as f32)))
}

/// Notes what changed since the list was drawn on the frame before (`pass - 1`): removed
/// downloads become ghosts, rows that changed places glide there from where they showed, and the
/// ids returned are new. A list that was not drawn then (the page just opened) starts over:
/// nothing is new, removed or moved. No ghosts or glides under Reduce motion.
fn track(rows: &mut Rows, items: &[QueueItem], revision: u64, pass: u64, now: f64, reduced: bool) -> Vec<usize> {
    let continuous = rows.pass.is_some_and(|last| last + 1 == pass);
    let mut new = Vec::new();
    if !continuous || rows.revision != revision {
        if continuous {
            let ids: HashSet<usize> = items.iter().map(|item| item.id).collect();
            let old: HashSet<usize> = rows.items.iter().map(|item| item.id).collect();
            for (index, item) in rows.items.iter().enumerate().filter(|(_, item)| !ids.contains(&item.id)) {
                rows.ghosts.push(Ghost { item: item.clone(), index, since: now });
            }
            new = items.iter().map(|item| item.id).filter(|id| !old.contains(id)).collect();
            // Places among the rows there before and still: rows that come or go move none.
            let was: HashMap<usize, usize> =
                rows.items.iter().filter(|item| ids.contains(&item.id)).enumerate().map(|(place, item)| (item.id, place)).collect();
            for (place, item) in items.iter().filter(|item| old.contains(&item.id)).enumerate() {
                if was[&item.id] != place {
                    let from = was[&item.id] as f32 - place as f32 + glide(&rows.moves, item.id, now);
                    rows.moves.retain(|m| m.0 != item.id);
                    rows.moves.push((item.id, from, now));
                }
            }
        }
        rows.items = items.to_vec();
        rows.revision = revision;
    }
    rows.pass = Some(pass);
    rows.ghosts.retain(|ghost| !reduced && now - ghost.since < GHOST);
    rows.moves.retain(|&(_, _, since)| !reduced && now - since < GLIDE);
    new
}

/// One line of the queue list.
struct Line<'a> {
    item: &'a QueueItem,
    /// How far (0.0 to 1.0) the ghost of a removed download has faded out.
    ghost: Option<f32>,
    /// Added since the frame before: it fades in.
    new: bool,
    /// How many rows down it still is while ghosts above it close their gap (see [`GHOST`]), or
    /// it glides to a new place (see [`GLIDE`]).
    drop: f32,
}

/// The lines of the list: `items`, ghosts fading out where they were, the rows under a ghost
/// closing its gap pushed down by what is left of it, and rows that changed places on their way;
/// and how many rows of gap are still closing under the last line.
// ponytail: ghosts of separate removals less than GHOST apart may land a row off; placing them
// by their neighbours would be exact.
fn lines<'a>(items: &'a [QueueItem], rows: &'a Rows, new: &[usize], now: f64) -> (Vec<Line<'a>>, f32) {
    let mut lines: Vec<Line> = items.iter().map(|item| Line { item, ghost: None, new: new.contains(&item.id), drop: 0.0 }).collect();
    let mut ghosts: Vec<&Ghost> = rows.ghosts.iter().collect();
    ghosts.sort_by_key(|ghost| ghost.index);
    for ghost in ghosts {
        let t = ((now - ghost.since) / GHOST) as f32;
        lines.insert(ghost.index.min(lines.len()), Line { item: &ghost.item, ghost: Some(t), new: false, drop: 0.0 });
    }
    let mut drop = 0.0;
    lines.retain_mut(|line| match line.ghost {
        // Faded out: its row is gone, and what is under it slides up.
        Some(t) if t >= 0.5 => {
            drop += 1.0 - anim::ease(2.0 * t - 1.0);
            false
        }
        Some(t) => {
            line.ghost = Some(2.0 * t);
            true
        }
        None => {
            line.drop = drop + glide(&rows.moves, line.item.id, now);
            true
        }
    });
    (lines, drop)
}

/// How far (0.0 to 1.0) `item`'s row is into its status, which changed when its kind did (not
/// its progress or error text) or when it started or stopped `waiting`; and, just after it
/// failed, how far its row shakes sideways.
fn row_motion(ctx: &egui::Context, item: &QueueItem, waiting: bool) -> (f32, f32) {
    let id = egui::Id::new(("queue row", item.id));
    let kind = (std::mem::discriminant(&item.status), waiting);
    let t = anim::changed(ctx, id.with("status"), kind, 0.45);
    // Keyed by every change, so that failing again after a retry shakes again.
    let shake = anim::shake(ctx, id.with("shake"), kind);
    (t, if matches!(item.status, QueueItemStatus::Failed(_)) { shake } else { 0.0 })
}

/// The icon that leads a queue row, `t` (0.0 to 1.0) into its status: a spinner while it waits,
/// an arrow bobbing while data comes in (still when `stalled`), a check that draws itself on
/// completion; the others pop and fade in.
fn status_icon(ui: &mut Ui, item: &QueueItem, waiting: bool, stalled: bool, color: Color32, t: f32) {
    const SIZE: f32 = 16.0;
    if waiting {
        anim::spinner(ui, SIZE, color);
        return;
    }
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(SIZE), Sense::hover());
    let glyph = match item.status {
        QueueItemStatus::Completed => {
            ui.painter().circle_stroke(rect.center(), SIZE / 2.0 - 1.0, Stroke::new(1.5, color));
            anim::paint_check(ui.painter(), rect.shrink(3.5), anim::ease(t), Stroke::new(2.0, color));
            return;
        }
        QueueItemStatus::Downloading => icon::ARROW_DOWN,
        QueueItemStatus::Failed(_) => icon::WARNING_CIRCLE,
        QueueItemStatus::AuthRequired => icon::LOCK_SIMPLE,
        QueueItemStatus::Queued => icon::CLOCK,
        QueueItemStatus::Paused | QueueItemStatus::Pausing => icon::PAUSE,
    };
    let bob = if item.status == QueueItemStatus::Downloading && !stalled && ui.is_rect_visible(rect) { 2.0 - 4.0 * anim::pulse(ui.ctx(), 1.1) } else { 0.0 };
    let size = 15.0 * (0.6 + 0.4 * anim::overshoot(t));
    anim::paint_icon(ui.painter(), rect.center() + Vec2::new(0.0, bob), glyph, size, color.gamma_multiply(anim::ease(t)), 0.0);
}

/// One line of the queue list: the row of `line.item`, with its status, name and buttons, then
/// progress, speed, time left, peers and notes; or why it failed (in full on hover), or why one
/// restored without its Authorization header waits. A click selects it, a double click shows it,
/// and it can be dragged (see `queue_page`); its ⋯ button and a right click open its
/// [`order_menu`]. A High or Low priority shows by its name. It fades in when new and out as a
/// ghost, flashes green on completion and shakes on failure. Returns the row's response.
fn queue_row(app: &App, ui: &mut Ui, line: &Line, action: &mut Option<(usize, RowAction)>) -> egui::Response {
    let p = palette(ui);
    let item = line.item;
    let ctx = ui.ctx().clone();
    let (slot, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::hover());
    let id = egui::Id::new(("queue row", item.id));
    let downloading = item.status == QueueItemStatus::Downloading;
    let waiting = item.status == QueueItemStatus::Pausing || downloading && (app.is_resolving(item.id) || item.is_finishing());
    let (t, shake) = row_motion(&ctx, item, waiting);
    let (opacity, shift) = match line.ghost {
        Some(gone) => (1.0 - anim::ease(gone), Vec2::new(24.0 * anim::ease(gone), 0.0)),
        None => {
            let shown = anim::ease(anim::progress(&ctx, anim::since(&ctx, id.with("in"), (), line.new), 0.0, anim::APPEAR));
            (shown, anim::RISE * (1.0 - shown))
        }
    };
    let stride = ROW_HEIGHT + ui.spacing().item_spacing.y;
    let rect = slot.translate(shift + Vec2::new(shake, line.drop * stride));
    // Clicks go where the row is drawn, also while it slides into a closing gap.
    let response = ui.interact(rect, id.with("row"), Sense::click_and_drag());
    let mut ignored = None;
    let action = if line.ghost.is_some() { &mut ignored } else { action };
    if response.double_clicked() {
        *action = Some((item.id, RowAction::Show));
    } else if response.clicked() {
        *action = Some((item.id, RowAction::Select));
    }
    // Its place in the list, for its order menu; a ghost has none.
    let items = app.queue.items();
    let place = items.iter().position(|other| other.id == item.id).filter(|_| line.ghost.is_none());
    if let Some(index) = place {
        response.context_menu(|ui| order_menu(ui, item, index, items.len(), action));
    }
    // Pulses only in view: the list lays out one row past its bottom edge.
    let seen = ui.is_rect_visible(rect);
    let inner = egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(12.0, 7.0))).layout(Layout::top_down(Align::LEFT));
    let mut ui = ui.new_child(inner);
    // A row being dragged stays dimmed in its place until it is dropped.
    ui.multiply_opacity(if response.dragged() { 0.5 * opacity } else { opacity });

    // The selected row's highlight is the list's (it slides); the others light up under the pointer.
    let hover = anim::presence(&ctx, id.with("hover"), response.hovered() && line.ghost.is_none(), 0.12);
    if hover > 0.0 && app.selected != Some(item.id) {
        ui.painter().rect_filled(rect, 8.0, p.hover.gamma_multiply(0.6 * hover));
    }
    let flash = match item.status {
        QueueItemStatus::Completed => Some(p.green),
        QueueItemStatus::Failed(_) => Some(p.red),
        _ => None,
    };
    if let Some(color) = flash.filter(|_| t < 1.0) {
        ui.painter().rect_filled(rect, 8.0, color.gamma_multiply(0.25 * (1.0 - anim::ease(t))));
    }

    let item_id = egui::Id::new(("item", item.id));
    let (status, color) = status_badge(app, item, &p);
    let actions = row_actions(&item.status);
    // A stream's row offers its track chooser too, left of the others.
    let tracks = crate::tracks::offered(item);
    ui.horizontal(|ui| {
        status_icon(ui, item, waiting, app.stalled_for(item.id).is_some(), color, t);
        status_chip(ui, item_id, status, color, downloading && !waiting && seen);
        left_part(ui, (actions.len() + usize::from(tracks) + 1) as f32 * 30.0, |ui| {
            let mark = match item.priority {
                Priority::High => Some((icon::CARET_DOUBLE_UP, p.amber, "High priority: starts before the other waiting downloads")),
                Priority::Low => Some((icon::CARET_DOUBLE_DOWN, p.dim, "Low priority: starts after the other waiting downloads")),
                Priority::Normal => None,
            };
            if let Some((glyph, color, tip)) = mark {
                ui.add(egui::Label::new(RichText::new(glyph).color(color)).selectable(false)).on_hover_text(tip);
            }
            let name = egui::Label::new(bold(&item.filename).color(p.strong)).truncate().selectable(false);
            ui.add(name).on_hover_text(&item.filename);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let more = icon_button(ui, icon::DOTS_THREE_VERTICAL, "More: order, priority, new link", true);
            let menu = id.with("menu");
            if more.clicked() {
                ui.memory_mut(|m| m.toggle_popup(menu));
            }
            if let Some(index) = place {
                egui::popup_below_widget(ui, menu, &more, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                    ui.set_min_width(170.0);
                    order_menu(ui, item, index, items.len(), action);
                });
            }
            for &(glyph, tip, what) in actions.iter().rev() {
                if icon_button(ui, glyph, tip, true).clicked() {
                    *action = Some((item.id, what));
                }
            }
            if tracks && icon_button(ui, icon::SLIDERS_HORIZONTAL, "Choose tracks…", true).clicked() {
                *action = Some((item.id, RowAction::Tracks));
            }
        });
    });
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        let (text, color) = match &item.status {
            QueueItemStatus::Failed(_) if item.link_expired() => (LINK_EXPIRED.to_string(), p.amber),
            QueueItemStatus::Failed(error) => (truncate_chars(error, 200), p.red),
            QueueItemStatus::AuthRequired => ("Authorization header not saved: open Details to enter it again and resume".to_string(), p.amber),
            status => {
                let bar = match status {
                    QueueItemStatus::Completed => p.green,
                    QueueItemStatus::Downloading => p.accent,
                    _ => p.dim,
                };
                let bar = anim::color(&ctx, item_id.with("bar"), bar, 0.35);
                let ratio = item.progress_ratio as f32;
                progress_bar(ui, item_id, Vec2::new(140.0, 6.0), ratio, bar, downloading, None);
                (row_summary(app, item, eased_progress(ui, item_id, ratio) as f64), p.muted)
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
    response
}

/// When everything still to come (see `DownloadQueue::remaining`) is done at the speed the
/// running downloads have together: "at least" when some have no known size, which are left out;
/// None without an estimate (no speed yet, or no size known).
fn finish_time(left: Remaining) -> Option<String> {
    let secs = util::eta_secs(left.bytes, 0, left.speed)?;
    Some(match left.unknown {
        0 => format!("All done in about {}", format_duration(secs)),
        n => format!("All done in at least {} ({} size unknown)", format_duration(secs), n),
    })
}

pub(super) fn queue_page(app: &mut App, ui: &mut Ui) {
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
                let clear_all = ui.add(button(format!("{} Clear all", icon::BROOM)));
                if clear_all.on_hover_text("Remove every download that is not running (files are kept)").clicked() {
                    app.clear_queue(false);
                }
                if ui.add(button("Clear completed")).clicked() {
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
            if let Some(text) = finish_time(app.queue.remaining(app.settings.auto_run_queue)) {
                let about = "The bytes left of the running downloads and of those waiting to start, at the speed the running ones \
                             have together; paused and stopped ones are left out.";
                ui.label(RichText::new(text).small().color(p.muted)).on_hover_text(about);
            }
        }
        // For this session only; it applies once.
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            egui::ComboBox::from_id_salt("when_done").selected_text(app.when_done.name()).show_ui(ui, |ui| {
                for (choice, name) in WhenDone::CHOICES {
                    ui.selectable_value(&mut app.when_done, choice, name);
                }
            });
            ui.label(RichText::new("When the queue is done:").color(p.muted));
        });
    });
    ui.add_space(4.0);

    let mut action = None;
    let shown: &App = app;
    let ctx = ui.ctx().clone();
    let list = egui::Id::new("queue rows");
    let mut rows: Rows = ctx.data_mut(|d| d.remove_temp(list)).unwrap_or_default();
    let now = ctx.input(|i| i.time);
    let new = track(&mut rows, items, shown.queue.revision(), ctx.cumulative_pass_nr(), now, anim::reduced(&ctx));
    if !rows.ghosts.is_empty() || !rows.moves.is_empty() {
        ctx.request_repaint();
    }
    let (lines, closing) = lines(items, &rows, &new, now);
    card(ui).inner_margin(4.0).show(ui, |ui| {
        if lines.is_empty() {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.label(RichText::new(icon::QUEUE).size(28.0).color(p.dim));
                ui.label(RichText::new(keys("The queue is empty. Add links above, or press Ctrl+V with one copied.")).color(p.muted));
                ui.add_space(30.0);
            });
            return;
        }
        ui.spacing_mut().item_spacing.y = 2.0;
        let scroll = egui::ScrollArea::vertical().id_salt("queue_rows").auto_shrink([false, false]).max_height(ui.available_height().max(120.0));
        scroll.show_rows(ui, ROW_HEIGHT, lines.len(), |ui, range| {
            ui.set_max_width(ui.available_width() - 10.0);
            let stride = ROW_HEIGHT + ui.spacing().item_spacing.y;
            // The selection's highlight sits on its row as drawn, and slides over from the row
            // selected before: from where it was then, relative to the new row as that moves.
            if let Some(index) = lines.iter().position(|line| line.ghost.is_none() && shown.selected == Some(line.item.id)) {
                let row = index as f32 + lines[index].drop;
                let key = list.with("selection");
                let t = anim::changed(ui.ctx(), key, shown.selected, 0.2);
                let (last, from) = ui.data(|d| d.get_temp::<(f32, f32)>(key.with("at"))).unwrap_or((row, 0.0));
                let from = if t == 0.0 { last - row } else { from };
                let y = row + from * (1.0 - anim::ease(t));
                ui.data_mut(|d| d.insert_temp(key.with("at"), (y, from)));
                let at = y - range.start as f32;
                let rect = Rect::from_min_size(ui.cursor().min + Vec2::new(0.0, at * stride), Vec2::new(ui.available_width(), ROW_HEIGHT));
                ui.painter().rect_filled(rect, 8.0, p.accent.gamma_multiply(0.14));
                let stripe = Rect::from_min_size(rect.min + Vec2::new(0.0, 10.0), Vec2::new(3.0, rect.height() - 20.0));
                ui.painter().rect_filled(stripe, 2.0, p.accent);
            }
            // Where the list's first row is, in view or not.
            let top = ui.cursor().min.y - range.start as f32 * stride;
            let mut dragged = None;
            for line in &lines[range] {
                let row = queue_row(shown, ui, line, &mut action);
                if line.ghost.is_none() && (row.dragged() || row.drag_stopped()) {
                    dragged = Some((line.item.id, row.drag_stopped()));
                }
            }
            // A row being dragged lands in the gap nearest the pointer, which a line shows;
            // dropped there, it moves.
            if let (Some((id, dropped)), Some(pointer)) = (dragged, ui.ctx().pointer_latest_pos()) {
                let from = items.iter().position(|item| item.id == id).unwrap_or_default();
                let gap = ((pointer.y - top) / stride).round().clamp(0.0, items.len() as f32) as usize;
                let to = if gap > from { gap - 1 } else { gap };
                if to != from {
                    let y = top + gap as f32 * stride - ui.spacing().item_spacing.y / 2.0;
                    let (left, right) = (ui.max_rect().left() + 4.0, ui.max_rect().right() - 4.0);
                    ui.painter().line_segment([Pos2::new(left, y), Pos2::new(right, y)], Stroke::new(2.0, p.accent));
                    ui.painter().circle_filled(Pos2::new(left, y), 4.0, p.accent);
                    if dropped {
                        action = Some((id, RowAction::MoveTo(to)));
                    }
                }
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            // The list's end comes up with the gap closing under its last line, so that a list
            // scrolled to its end does not jump as a removed row's room goes.
            ui.expand_to_include_y(ui.min_rect().bottom() + closing * stride);
        });
    });
    ctx.data_mut(|d| d.insert_temp(list, rows));

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
        RowAction::Next => {
            app.queue.download_next(id);
        }
        RowAction::MoveTo(index) => {
            app.queue.move_item(id, index);
        }
        RowAction::Priority(priority) => {
            if let Some(item) = app.queue.get_item_mut(id) {
                item.priority = priority;
            }
        }
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
        RowAction::NewLink => {
            app.show_job(id);
            app.new_link = Some(crate::NewLink::new(id));
        }
        RowAction::OpenPage => {
            if let Some(page) = app.queue.get_item(id).and_then(page_of) {
                ui.ctx().open_url(egui::OpenUrl::new_tab(page));
            }
        }
        RowAction::Tracks => {
            app.show_job(id);
            // Open already: listing again would throw away what is ticked.
            let open = app.tracks.as_ref().is_some_and(|chooser| chooser.item == Some(id));
            if let Some(url) = app.queue.get_item(id).and_then(|item| item.urls.first().cloned()).filter(|_| !open) {
                app.list_tracks(Some(id), url);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hyperfetch_core::engine::DownloadOptions;
    use hyperfetch_core::queue::DownloadQueue;

    use super::super::harness::Harness;
    use super::*;

    fn add(queue: &mut DownloadQueue, name: &str) -> usize {
        queue.add_item(vec![url::Url::parse(&format!("https://example.com/{}", name)).unwrap()], DownloadOptions::default())
    }

    /// What to do when the queue is done is picked on the queue page; once it is done, the
    /// countdown shows over every page until Cancel.
    #[test]
    fn the_queue_done_choice_counts_down_until_cancelled() {
        let mut h = Harness::new();
        h.app.tab = Tab::Queue;
        assert!(h.settle());
        h.click("Do nothing");
        h.frames(10);
        h.click("Shut down");
        assert_eq!(h.app.when_done, WhenDone::ShutDown);
        let id = h.add("big.iso");
        h.progress(id, 0.5);
        h.app.handle_event(crate::AppEvent::JobFinished { id, result: Ok((std::env::temp_dir().join("big.iso"), Some(1))) });
        assert!(h.app.countdown.is_some());
        h.app.tab = Tab::Settings;
        assert!(h.settle());
        assert!(h.texts().iter().any(|(text, _)| text.starts_with("The queue is done. Shutting down in ")), "{:?}", h.texts());
        h.click("Cancel");
        assert!(h.app.countdown.is_none());
        h.wait_for_background();
    }

    /// While downloads run, the queue's header says when everything to come is done at their
    /// speed together: "about", or "at least" when some have no known size yet, which are left
    /// out; waiting ones count only when the queue starts them by itself. With nothing running it
    /// says nothing.
    #[test]
    fn the_header_says_when_the_queue_will_be_done() {
        let mut h = Harness::new();
        h.app.settings.auto_run_queue = true;
        let [a, _] = ["a.iso", "b.iso"].map(|name| h.add(name));
        h.app.tab = Tab::Queue;
        let done = |h: &mut Harness| {
            h.frames(2);
            h.texts().into_iter().map(|(text, _)| text).find(|text| text.starts_with("All done"))
        };
        assert_eq!(done(&mut h), None, "nothing runs");
        // 50 MiB left at 3 MB/s.
        h.progress(a, 0.5);
        assert_eq!(done(&mut h).as_deref(), Some("All done in at least 00:18 (1 size unknown)"));
        h.app.settings.auto_run_queue = false;
        assert_eq!(done(&mut h).as_deref(), Some("All done in about 00:18"));
        h.finish(a, None);
        assert_eq!(done(&mut h), None);
    }

    /// "Why is it slow?" on a running download's page lists what holds it back: here the speed
    /// limit, which it runs at. The fix lifts the limit for this download, which pauses and at
    /// once starts again from where it was, without it.
    #[test]
    fn why_is_it_slow_finds_the_speed_limit_and_lifts_it() {
        let mut h = Harness::new();
        // Started again, it fails at once instead of fetching anything.
        h.app.engines = std::sync::Arc::new(|_: Vec<url::Url>, _: DownloadOptions| Err("not downloaded here".to_string()));
        let id = h.add("big.iso");
        h.app.queue.get_item_mut(id).unwrap().options.max_speed = Some(3_000_000);
        // At 3 MB/s.
        h.progress(id, 0.4);
        h.app.focused = Some(id);
        h.frames(10);
        assert!(h.find("What holds it back").is_none());
        h.click("Why is it slow?");
        h.frames(30);
        let lines: Vec<String> = h.texts().into_iter().map(|(text, _)| text).collect();
        assert!(lines.iter().any(|text| text.starts_with("Your speed limit holds it")), "{lines:?}");
        h.click("Lift the limit for it");
        let item = h.app.queue.get_item(id).unwrap();
        assert_eq!((item.options.max_speed, item.status.clone()), (None, QueueItemStatus::Pausing));
        h.app.handle_event(crate::AppEvent::JobFinished { id, result: Err(hyperfetch_core::engine::CANCELLED.to_string()) });
        assert_eq!(h.app.queue.get_item(id).unwrap().status, QueueItemStatus::Downloading, "started again at once");
        h.wait_for(|app| matches!(app.queue.get_item(id).unwrap().status, QueueItemStatus::Failed(_)));
        h.wait_for_background();
    }

    /// Each line's file name, whether it is a ghost, and how many rows down it is pushed.
    fn shown(queue: &DownloadQueue, rows: &Rows, now: f64) -> Vec<(String, bool, f32)> {
        lines(queue.items(), rows, &[], now).0.iter().map(|line| (line.item.filename.clone(), line.ghost.is_some(), line.drop)).collect()
    }

    /// Removed downloads linger where they were, fade out, then close their gap as the rows under
    /// them slide up; added ones are new. Nothing is new or removed when the list starts over,
    /// and there are no ghosts under Reduce motion.
    #[test]
    fn removed_rows_linger_as_ghosts_and_added_ones_are_new() {
        let mut queue = DownloadQueue::new();
        let ids: Vec<usize> = ["a", "b", "c", "d"].iter().map(|name| add(&mut queue, name)).collect();
        let mut rows = Rows::default();
        assert!(track(&mut rows, queue.items(), queue.revision(), 1, 0.0, false).is_empty(), "the first sight is not news");
        assert!(track(&mut rows, queue.items(), queue.revision(), 2, 0.5, false).is_empty());

        queue.remove_item(ids[1]);
        queue.remove_item(ids[3]);
        let e = add(&mut queue, "e");
        assert_eq!(track(&mut rows, queue.items(), queue.revision(), 3, 1.0, false), [e]);
        let row = |name: &str, ghost, drop| (name.to_string(), ghost, drop);
        let all = [row("a", false, 0.0), row("b", true, 0.0), row("c", false, 0.0), row("d", true, 0.0), row("e", false, 0.0)];
        assert_eq!(shown(&queue, &rows, 1.0), all, "the ghosts stay where they were");

        // Half way through closing their gaps: c is pushed down by b's, e by both.
        track(&mut rows, queue.items(), queue.revision(), 4, 1.0 + GHOST * 0.75, false);
        let closing = shown(&queue, &rows, 1.0 + GHOST * 0.75);
        let names: Vec<_> = closing.iter().map(|(name, ghost, _)| (name.as_str(), *ghost)).collect();
        assert_eq!(names, [("a", false), ("c", false), ("e", false)]);
        let gap = 1.0 - anim::ease(0.5);
        assert!(closing[0].2 == 0.0 && (closing[1].2 - gap).abs() < 1e-6 && (closing[2].2 - 2.0 * gap).abs() < 1e-6, "{closing:?}");

        track(&mut rows, queue.items(), queue.revision(), 5, 1.01 + GHOST, false);
        assert!(rows.ghosts.is_empty() && shown(&queue, &rows, 1.01 + GHOST).iter().all(|(_, ghost, drop)| !ghost && *drop == 0.0));

        // The page was away for a while (frames went by without the list): it starts over.
        queue.remove_item(ids[0]);
        add(&mut queue, "f");
        assert!(track(&mut rows, queue.items(), queue.revision(), 9, 5.0, false).is_empty());
        assert!(rows.ghosts.is_empty());

        queue.remove_item(ids[2]);
        track(&mut rows, queue.items(), queue.revision(), 10, 6.0, true);
        assert!(rows.ghosts.is_empty(), "no ghosts under Reduce motion");
    }

    /// A row's status changes when its kind does, not its progress or error text: then it runs
    /// from 0.0 to 1.0, and a failure shakes it; retrying does not. Seen first, nothing moves.
    #[test]
    fn a_row_reacts_to_a_new_status_only() {
        let ctx = egui::Context::default();
        let at = |time: f64, item: &QueueItem| {
            let mut out = (0.0, 0.0);
            let _ = ctx.run(egui::RawInput { time: Some(time), ..Default::default() }, |ctx| out = row_motion(ctx, item, false));
            out
        };
        let mut queue = DownloadQueue::new();
        let id = add(&mut queue, "a.iso");
        queue.mark_started(id);
        let mut item = queue.get_item(id).unwrap().clone();
        assert_eq!(at(0.0, &item), (1.0, 0.0), "seen first");
        item.progress_ratio = 0.5;
        item.downloaded_bytes = 1 << 20;
        assert_eq!(at(0.1, &item), (1.0, 0.0), "progress is no news");

        item.status = QueueItemStatus::Completed;
        assert_eq!(at(1.0, &item).0, 0.0);
        assert!((at(1.0 + 0.45 / 2.0, &item).0 - 0.5).abs() < 1e-3);
        assert_eq!(at(2.0, &item), (1.0, 0.0));

        item.status = QueueItemStatus::Failed("404".to_string());
        at(3.0, &item);
        let (t, shake) = at(3.05, &item);
        assert!(t < 1.0 && shake != 0.0, "a failure shakes: {shake}");
        assert_eq!(at(4.0, &item), (1.0, 0.0));
        item.status = QueueItemStatus::Failed("timed out".to_string());
        assert_eq!(at(4.1, &item), (1.0, 0.0), "another error is the same status");

        item.status = QueueItemStatus::Downloading;
        at(5.0, &item);
        let (t, shake) = at(5.05, &item);
        assert!(t < 1.0 && shake == 0.0, "a retry does not shake");
    }

    /// The queue page with downloads named item-00.iso and on, its list scrolled `by` points down
    /// with the mouse wheel, settled; their ids.
    fn scrolled(h: &mut Harness, rows: usize, by: f32) -> Vec<usize> {
        let ids = (0..rows).map(|n| h.add(&format!("item-{n:02}.iso"))).collect();
        h.app.tab = Tab::Queue;
        h.frames(30);
        h.hover(h.find("item-00.iso").unwrap().center());
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: Vec2::new(0.0, -by), modifiers: egui::Modifiers::NONE };
        h.frame(vec![wheel]);
        // egui spreads a wheel turn over several frames.
        h.frames(60);
        h.leave();
        assert!(h.settle());
        ids
    }

    /// While the gap of removed rows closes, a row under it is drawn lower than its place in the
    /// list: a click where it shows selects it, and the selection's highlight sits on it.
    #[test]
    fn a_row_sliding_into_a_closing_gap_takes_clicks_where_it_shows() {
        let mut h = Harness::new();
        for name in ["a.iso", "b.iso", "c.iso"] {
            let id = h.add(name);
            h.progress(id, 1.0);
            h.finish(id, None);
        }
        let run = h.add("run.iso");
        h.progress(run, 0.5);
        h.app.tab = Tab::Queue;
        h.frames(60);
        h.app.clear_queue(true);
        // Past the ghosts' fade, a quarter into the closing.
        h.frames(16);
        h.click_at(h.find("run.iso").unwrap().center());
        assert_eq!(h.app.selected, Some(run));
        h.frame(Vec::new());
        let drawn = h.find("run.iso").unwrap().top();
        let selection = egui::Id::new("queue rows").with("selection").with("at");
        let (y, _) = h.app.ctx.data(|d| d.get_temp::<(f32, f32)>(selection)).unwrap();
        h.frames(30);
        let down = drawn - h.find("run.iso").unwrap().top();
        assert!(down > 20.0, "still sliding: {down}");
        assert!((y * (ROW_HEIGHT + 2.0) - down).abs() < 1.0, "the highlight is {y} rows down, the row {down} pt");
    }

    /// With the list scrolled to its end, a row removed near the bottom closes its room without a
    /// jump: the rows above come down smoothly, those under it stay about where they were.
    #[test]
    fn a_list_scrolled_to_its_end_closes_a_gap_without_jumping() {
        let mut h = Harness::new();
        scrolled(&mut h, 12, 1000.0);
        let at = |h: &Harness| [h.find("item-08.iso").unwrap().top(), h.find("item-11.iso").unwrap().top()];
        let [above, under] = at(&h);
        h.click_at(h.find_near("item-10.iso", icon::X).unwrap().center());
        assert_eq!(h.app.queue.items().len(), 11);
        let mut last = above;
        for _ in 0..40 {
            h.frame(Vec::new());
            let [now_above, now_under] = at(&h);
            assert!((now_above - last).abs() < 20.0, "the rows above jump: {last} to {now_above}");
            assert!((now_under - under).abs() < 20.0, "the rows under move: {under} to {now_under}");
            last = now_above;
        }
        assert!((last - above - (ROW_HEIGHT + 2.0)).abs() < 0.5, "the rows above end a row lower: {above} to {last}");
        assert_eq!(at(&h)[1], under);
    }

    /// A running download in the row the list lays out just past its bottom edge asks for no
    /// frames: nothing that moves shows. (Scrolled half a row, that row is out of sight.)
    #[test]
    fn a_running_row_out_of_view_keeps_still() {
        let mut h = Harness::new();
        let ids = scrolled(&mut h, 12, 30.0);
        let row = |h: &Harness, id: usize| h.app.ctx.read_response(egui::Id::new(("queue row", id)).with("row"));
        let hidden = ids.iter().copied().find(|&id| row(&h, id).is_some_and(|r| !r.interact_rect.is_positive()));
        let hidden = hidden.expect("a row laid out out of view");
        h.progress(hidden, 0.5);
        assert_eq!(h.frames(60), Duration::MAX);
    }

    /// `n` frames, and what `f` sees after each.
    fn over<R>(h: &mut Harness, n: usize, f: impl Fn(&Harness) -> R) -> Vec<R> {
        (0..n).map(|_| (h.frame(Vec::new()), f(&*h)).1).collect()
    }

    /// A download added while the queue shows fades into its row rising from a little below, and
    /// the Queue count in the sidebar pops; then frames stop. Under Reduce motion both are in
    /// place at once.
    #[test]
    fn a_new_download_rises_into_its_row_and_the_count_pops() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            h.add("a.iso");
            h.app.tab = Tab::Queue;
            assert!(h.settle());
            let place = h.find("a.iso").unwrap().top() + ROW_HEIGHT + 2.0;
            h.add("b.iso");
            let seen = over(&mut h, 30, |h| (h.find("b.iso").map(|rect| rect.top()), h.find_near("Queue", "2").unwrap().height()));
            let tops: Vec<f32> = seen.iter().filter_map(|(top, _)| *top).collect();
            let heights: Vec<f32> = seen.iter().map(|(_, height)| *height).collect();
            let (rest, last) = (heights[heights.len() - 1], tops[tops.len() - 1]);
            assert_eq!(last, place);
            if reduce {
                assert!(tops.len() == 30 && tops.iter().all(|&top| top == place), "in place at once: {tops:?}");
                assert!(heights.iter().all(|&height| height == rest), "no pop: {heights:?}");
            } else {
                assert!(tops[0] > place + 2.0 && tops.windows(2).all(|w| w[0] >= w[1]), "rises: {tops:?}");
                assert!(heights.iter().any(|&height| height > rest + 1.0), "the count pops: {heights:?}");
            }
            // The Add page's Download button says Added for 1.2 s after an add (see `just_added`).
            assert_eq!(h.frames(60), Duration::MAX, "and keeps still");
        }
    }

    /// Removing a download fades its row out, then the rows under it glide up into its room, none
    /// jumping; then frames stop. Under Reduce motion they are in their new places at once.
    #[test]
    fn rows_glide_up_into_a_removed_rows_room() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            for name in ["a.iso", "b.iso", "c.iso", "d.iso"] {
                h.add(name);
            }
            h.app.tab = Tab::Queue;
            assert!(h.settle());
            let stride = ROW_HEIGHT + 2.0;
            let place = h.find("d.iso").unwrap().top() - stride;
            h.click_at(h.find_near("b.iso", icon::X).unwrap().center());
            assert_eq!(h.app.queue.items().len(), 3);
            let tops = over(&mut h, 40, |h| h.find("d.iso").unwrap().top());
            assert_eq!(tops[tops.len() - 1], place);
            if reduce {
                assert!(tops.iter().all(|&top| top == place), "at once: {tops:?}");
            } else {
                assert_eq!(tops[0], place + stride, "it waits while the removed row fades");
                assert!(tops.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] < stride / 3.0), "glides: {tops:?}");
            }
            h.leave();
            assert_eq!(h.frames(30), Duration::MAX, "and keeps still");
        }
    }

    /// A server of two files by the first segment of their path, as a download's probe asks for
    /// them: `same/…`, of 1000 bytes, and `other/…`, of 2000. Its address.
    fn sizes_server() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        // Two probes of a HEAD and a GET each, and room for retries.
        std::thread::spawn(move || {
            for mut socket in listener.incoming().take(16).flatten() {
                let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
                let (mut head, mut buf) = (Vec::new(), [0u8; 1024]);
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head);
                let size = if head.contains(" /other/") { 2000 } else { 1000 };
                let answer = if head.starts_with("HEAD ") {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n")
                } else {
                    format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/{size}\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx")
                };
                let _ = socket.write_all(answer.as_bytes());
            }
        });
        base
    }

    /// A download whose link expired says so on its row, with New link… in its ⋯ menu. There a
    /// link to a file of another size is refused, saying why, and nothing changes; one to the same
    /// file is taken, its resume state keeps what was downloaded, and it resumes.
    #[test]
    fn an_expired_link_takes_a_new_one_once_it_is_the_same_file() {
        use hyperfetch_core::state::DownloadState;
        let mut h = Harness::new();
        // Resumed, it fails at once instead of fetching anything.
        h.app.engines = std::sync::Arc::new(|_: Vec<url::Url>, _: DownloadOptions| Err("not downloaded here".to_string()));
        let server = sizes_server();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("talk.mp4");
        // What the engine left: 400 bytes of 1000, the link it had them from.
        let old = "https://cdn.example/talk.mp4?sig=old";
        let state_path = DownloadState::state_file_path(&dir.path().join("talk.mp4.part"));
        std::fs::write(dir.path().join("talk.mp4.part"), [0u8; 1000]).unwrap();
        let mut state = DownloadState::new("talk.mp4".into(), 1000, 256 << 10, vec![old.into()]);
        state.completed_ranges.push(hyperfetch_core::range::ByteRange::new(0, 399).unwrap());
        state.save_atomic(&state_path).unwrap();
        let saved = std::fs::read(&state_path).unwrap();
        let from_page = DownloadOptions { referer: Some("https://page.example/talk".into()), ..Default::default() };
        let id = h.app.queue.add_item(vec![url::Url::parse(old).unwrap()], from_page);
        h.app.queue.mark_started(id);
        let snapshot = hyperfetch_core::engine::EngineSnapshot {
            total_bytes: 1000,
            downloaded_bytes: 400,
            progress_ratio: 0.4,
            target_path: Some(target.clone()),
            ..Default::default()
        };
        h.app.queue.apply_snapshot(id, &snapshot);
        h.app.queue.finish(id, Err(format!("{old}: HTTP 403 Forbidden")));
        h.app.tab = Tab::Queue;
        h.frames(5);
        assert!(h.find(LINK_EXPIRED).is_some() && h.find("LINK EXPIRED").is_some(), "the row says so");

        from_menu(&mut h, "talk.mp4", "New link…");
        h.frames(20);
        assert!(h.find("Open the page").is_some(), "the page it came from is known");
        h.paste(&format!("{server}other/talk.mp4?sig=new"));
        h.click("Use this link");
        h.wait_for(|app| app.new_link.as_ref().is_some_and(|new_link| new_link.refused.is_some()));
        h.frames(3);
        let why = "This link is for a different file: 2.0 KiB, not 1000 bytes";
        assert!(h.texts().iter().any(|(text, _)| text == why), "{:?}", h.texts());
        let item = h.app.queue.get_item(id).unwrap();
        assert!(item.link_expired() && item.urls[0].as_str() == old, "nothing changes");
        assert_eq!(std::fs::read(&state_path).unwrap(), saved);

        let fresh = format!("{server}same/renamed.mp4?sig=new");
        h.app.new_link.as_mut().unwrap().text = fresh.clone();
        h.frames(2);
        h.click("Use this link");
        h.wait_for(|app| app.new_link.is_none());
        let item = h.app.queue.get_item(id).unwrap().clone();
        assert_eq!(item.urls, [url::Url::parse(&fresh).unwrap()]);
        assert_eq!((item.options.output_path.as_deref(), item.downloaded_bytes), (Some(target.as_path()), 400), "into its own file");
        let state = DownloadState::load_from_path(&state_path).unwrap().unwrap();
        assert!(state.mirrors.contains(&fresh) && state.completed_ranges.len() == 1, "{:?}", state.mirrors);
        assert!(matches!(&h.app.notice, Some(Ok(notice)) if notice.starts_with("New link taken")), "{:?}", h.app.notice);
        // It resumed: the stand-in engine fails it at once.
        h.wait_for(|app| app.queue.get_item(id).unwrap().status == QueueItemStatus::Failed("not downloaded here".to_string()));
        h.wait_for_background();
    }

    /// The queue's file names, top to bottom.
    fn order(h: &Harness) -> Vec<String> {
        h.app.queue.items().iter().map(|item| item.filename.clone()).collect()
    }

    /// Opens the ⋯ menu of `name`'s row and clicks `entry` in it.
    fn from_menu(h: &mut Harness, name: &str, entry: &str) {
        h.click_at(h.find_near(name, icon::DOTS_THREE_VERTICAL).unwrap().center());
        // A menu is laid out unseen the first time.
        h.frames(2);
        h.click(entry);
    }

    /// "Download next" in a waiting row's ⋯ menu puts it first among the waiting downloads (a
    /// paused one keeps its place); the rows glide to their new places, none jumping, then frames
    /// stop. Under Reduce motion they are in place at once.
    #[test]
    fn download_next_moves_a_row_up_and_the_rows_glide_there() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            let [paused, ..] = ["paused.iso", "b.iso", "c.iso", "d.iso"].map(|name| h.add(name));
            h.app.queue.mark_started(paused);
            h.app.queue.mark_pausing(paused);
            h.app.queue.finish(paused, Err(hyperfetch_core::engine::CANCELLED.to_string()));
            h.app.tab = Tab::Queue;
            assert!(h.settle());
            let stride = ROW_HEIGHT + 2.0;
            let place = h.find("b.iso").unwrap().top();
            from_menu(&mut h, "d.iso", "Download next");
            assert_eq!(order(&h), ["paused.iso", "d.iso", "b.iso", "c.iso"]);
            let tops = over(&mut h, 40, |h| (h.find("d.iso").unwrap().top(), h.find("c.iso").unwrap().top()));
            let (d, c): (Vec<f32>, Vec<f32>) = tops.into_iter().unzip();
            assert_eq!((d[d.len() - 1], c[c.len() - 1]), (place, place + 2.0 * stride));
            if reduce {
                assert!(d.iter().all(|&top| top == place) && c.iter().all(|&top| top == place + 2.0 * stride), "at once: {d:?} {c:?}");
            } else {
                assert_eq!(d[0], place + 2.0 * stride, "it starts from where it was");
                assert!(d.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] < stride / 2.0), "glides up: {d:?}");
                assert!(c[0] < c[c.len() - 1] && c.windows(2).all(|w| w[1] >= w[0]), "c glides down: {c:?}");
            }
            h.leave();
            assert_eq!(h.frames(30), Duration::MAX, "and keeps still");
        }
    }

    /// A waiting row's ⋯ menu sets its priority: a High or Low one shows a mark by its name, a
    /// Normal one none, and the High one starts first.
    #[test]
    fn a_waiting_row_takes_a_priority_from_its_menu() {
        let mut h = Harness::new();
        let [_, b] = ["a.iso", "b.iso"].map(|name| h.add(name));
        h.app.tab = Tab::Queue;
        assert!(h.settle());
        let marked = |h: &Harness, glyph: &str| {
            let name = h.find("b.iso").unwrap();
            h.find(glyph).is_some_and(|mark| (mark.center().y - name.center().y).abs() < 4.0 && mark.right() <= name.left())
        };
        assert!(!marked(&h, icon::CARET_DOUBLE_UP));
        from_menu(&mut h, "b.iso", "High");
        assert_eq!(h.app.queue.get_item(b).unwrap().priority, Priority::High);
        h.frames(2);
        assert!(marked(&h, icon::CARET_DOUBLE_UP), "a mark before its name");
        assert_eq!(h.app.queue.next_to_start(1, |_| false), Some(b), "it starts first");
        from_menu(&mut h, "b.iso", "Low");
        h.frames(2);
        assert!(marked(&h, icon::CARET_DOUBLE_DOWN) && !marked(&h, icon::CARET_DOUBLE_UP));
        from_menu(&mut h, "b.iso", "Normal");
        h.frames(2);
        assert!(h.find(icon::CARET_DOUBLE_UP).is_none() && h.find(icon::CARET_DOUBLE_DOWN).is_none(), "a Normal one shows none");
    }

    /// A row dragged over the list shows where it would land, as a line in the gap nearest the
    /// pointer; dropped there it moves, and once it has glided into place frames stop. A row
    /// dropped on its own place stays.
    #[test]
    fn a_dragged_row_lands_where_the_line_shows() {
        let mut h = Harness::new();
        let ids = ["a.iso", "b.iso", "c.iso", "d.iso"].map(|name| h.add(name));
        h.app.tab = Tab::Queue;
        assert!(h.settle());
        let b = h.app.ctx.read_response(egui::Id::new(("queue row", ids[1])).with("row")).unwrap().rect;
        let landing = h.find("b.iso").unwrap().top();
        // The dot at the left end of the line in the gap above b, as painted.
        let dot = |h: &Harness| {
            let [r, _, blue, _] = h.shot(Some(Rect::from_min_size(Pos2::new(b.left() + 4.0, b.top() - 1.0), Vec2::splat(1.0)))).get_pixel(0, 0).0;
            blue > 180 && r < 120
        };
        assert!(!dot(&h));
        let (from, to) = (h.find("d.iso").unwrap().center(), Pos2::new(b.center().x, b.top() + 10.0));
        h.press(from);
        for n in 1..=6 {
            h.hover(from.lerp(to, n as f32 / 6.0));
        }
        assert!(dot(&h), "the line shows the gap above b");
        assert_eq!(order(&h), ["a.iso", "b.iso", "c.iso", "d.iso"], "nothing moves before the drop");
        h.release(to);
        assert_eq!(order(&h), ["a.iso", "d.iso", "b.iso", "c.iso"]);
        h.leave();
        assert!(h.settle());
        assert_eq!(h.find("d.iso").unwrap().top(), landing);
        assert!(!dot(&h), "the line is gone");

        let a = h.find("a.iso").unwrap().center();
        h.press(a);
        h.hover(a + Vec2::new(0.0, 10.0));
        h.hover(a + Vec2::new(0.0, 20.0));
        h.release(a + Vec2::new(0.0, 20.0));
        assert_eq!(order(&h), ["a.iso", "d.iso", "b.iso", "c.iso"], "dropped on its own place");
    }

    /// A download that completes flashes its row green as its check draws itself; one that fails
    /// flashes red and shakes. Then frames stop. Under Reduce motion nothing flashes or moves.
    #[test]
    fn a_finished_row_glows_and_a_failed_one_shakes() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            let [done, failed] = ["done.iso", "failed.iso"].map(|name| h.add(name));
            h.progress(done, 0.9);
            h.progress(failed, 0.4);
            h.app.tab = Tab::Queue;
            h.frames(60);
            // Inside the row, left of its icon, where only the card shows.
            let spot = |h: &Harness| {
                let row = h.app.ctx.read_response(egui::Id::new(("queue row", done)).with("row")).unwrap().rect;
                let pixel = h.shot(Some(Rect::from_min_size(row.left_center() + Vec2::new(3.0, 0.0), Vec2::splat(1.0)))).get_pixel(0, 0).0;
                [pixel[0], pixel[1], pixel[2]]
            };
            let card = [DARK.card.r(), DARK.card.g(), DARK.card.b()];
            assert_eq!(spot(&h), card);
            h.finish(done, None);
            h.finish(failed, Some("The server answered 404 Not Found"));
            let (mut xs, mut glow) = (Vec::new(), Vec::new());
            for n in 0..50 {
                h.frame(Vec::new());
                xs.push(h.find("failed.iso").unwrap().left());
                if n % 5 == 0 {
                    glow.push(spot(&h));
                }
            }
            let rest = xs[xs.len() - 1];
            assert_eq!(glow[glow.len() - 1], card, "the glow is gone");
            if reduce {
                assert!(glow.iter().all(|&g| g == card) && xs.iter().all(|&x| x == rest), "no glow or shake: {glow:?} {xs:?}");
            } else {
                let greens: Vec<u8> = glow.iter().map(|g| g[1]).collect();
                assert!(greens[0] > card[1] + 10 && greens.windows(2).all(|w| w[1] <= w[0]), "glows and fades: {greens:?}");
                assert!(xs.iter().any(|&x| (x - rest).abs() > 1.0), "shakes: {xs:?}");
            }
            assert_eq!(h.frames(2), Duration::MAX, "and keeps still");
        }
    }

    /// The chunk map paints each connection's piece in its own colour. When a connection takes
    /// over the end of another's piece, a light flashes on the cut and the new piece turns from
    /// the other's colour to its own; then frames stop. Under Reduce motion it has its own colour
    /// at once and nothing flashes.
    #[test]
    fn the_chunk_map_colours_each_connection_and_splits_a_stolen_piece() {
        for reduce in [false, true] {
            let mut h = Harness::new();
            h.app.settings.reduce_motion = Some(reduce);
            let id = h.add("ubuntu.iso");
            h.progress(id, 0.5);
            h.app.focused = Some(id);
            // Paused, so that nothing else on its page moves.
            h.app.jobs.get_mut(&id).unwrap().running = None;
            h.app.queue.get_item_mut(id).unwrap().status = QueueItemStatus::Paused;
            assert!(h.settle());
            let map = h.app.ctx.read_response(egui::Id::new(("chunk map", id))).unwrap().rect;
            // The colour shown `share` of the way along the map.
            let at = |h: &Harness, share: f32| {
                let spot = Rect::from_min_size(Pos2::new(map.left() + share * map.width(), map.center().y), Vec2::splat(1.0));
                let [r, g, b, _] = h.shot(Some(spot)).get_pixel(0, 0).0;
                Color32::from_rgb(r, g, b)
            };
            // Each quarter is half done: its first eighth filled.
            for n in 0..4 {
                assert_eq!(at(&h, n as f32 / 4.0 + 1.0 / 16.0), DARK.lanes[n], "connection {n}");
            }

            // Connection 4 takes the last quarter of the second piece and gets 80 % of it.
            let quarter = (100u64 << 20) / 4;
            let cut = quarter + quarter * 3 / 4;
            let chunks = &mut h.app.jobs.get_mut(&id).unwrap().chunks;
            (chunks[1].range_end, chunks[1].total_bytes) = (cut - 1, cut - quarter);
            let status = "Worker 4".to_string();
            chunks.push(ChunkSnapshot { id: 4, range_start: cut, range_end: 2 * quarter - 1, downloaded_bytes: quarter / 5, total_bytes: quarter / 4, status, worker_id: Some(4) });
            let (stolen, seam) = ((cut + quarter / 10) as f32 / (4 * quarter) as f32, cut as f32 / (4 * quarter) as f32);
            let delay = h.frame(Vec::new());
            // The light's edge is smoothed, so the pixel under it shows a little of the piece; no
            // connection's colour or the card is that light in every channel.
            let white = |color: Color32| color.r().min(color.g()).min(color.b()) > 150;
            if reduce {
                assert!(delay == Duration::MAX && at(&h, stolen) == DARK.lanes[4] && !white(at(&h, seam)), "split at once");
            } else {
                assert!(delay < Duration::MAX && at(&h, stolen) != DARK.lanes[4], "turning");
                assert!(white(at(&h, seam)), "the cut flashes: {:?}", at(&h, seam));
                assert!(h.settle());
                assert!(at(&h, stolen) == DARK.lanes[4] && !white(at(&h, seam)), "split");
            }
        }
    }
}
