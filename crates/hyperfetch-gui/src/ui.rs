use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::Instant;

use eframe::egui;
use egui::{Color32, Pos2, Rect, RichText, Stroke, Vec2};
use hyperfetch_core::chunk::ChunkSnapshot;
use hyperfetch_core::history::HistoryStatus;
use hyperfetch_core::queue::{QueueItem, QueueItemStatus};

use crate::settings::{BROWSERS, MEDIA_PRESETS};
use crate::util::{self, format_bytes, format_duration, lock, truncate_chars, Verdict};
use crate::{App, Dialog, Tab, VerifyRequest, GRAPH_WINDOW};

const TEXT: Color32 = Color32::from_rgb(228, 232, 240);
const MUTED: Color32 = Color32::from_rgb(148, 163, 184);
const DIM: Color32 = Color32::from_rgb(113, 113, 122);
const SLATE: Color32 = Color32::from_rgb(100, 116, 139);
const BLUE: Color32 = Color32::from_rgb(37, 99, 235);
const BRIGHT_BLUE: Color32 = Color32::from_rgb(59, 130, 246);
const CYAN: Color32 = Color32::from_rgb(56, 189, 248);
const GREEN: Color32 = Color32::from_rgb(16, 185, 129);
const AMBER: Color32 = Color32::from_rgb(234, 179, 8);
const RED: Color32 = Color32::from_rgb(239, 68, 68);

const AUTH_REQUIRED: &str =
    "Authorization header not saved: enter it again under Advanced Options > Auth, then Resume to continue from the partial file.";

fn card() -> egui::Frame {
    egui::Frame::none()
        .fill(Color32::from_rgb(24, 26, 33))
        .stroke(Stroke::new(1.0, Color32::from_rgb(42, 45, 56)))
        .inner_margin(12.0)
        .rounding(6.0)
}

fn error_alert(ui: &mut egui::Ui, text: &str) {
    ui.add_space(6.0);
    egui::Frame::none()
        .fill(Color32::from_rgb(45, 20, 20))
        .stroke(Stroke::new(1.0, Color32::from_rgb(220, 38, 38)))
        .inner_margin(8.0)
        .rounding(4.0)
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(Color32::from_rgb(254, 202, 202)).size(12.0));
        });
}

/// Placeholder text, dimmed explicitly because the theme overrides every text color.
fn hint_text(text: &str) -> RichText {
    RichText::new(text).color(Color32::from_rgb(90, 96, 110))
}

fn primary_button(label: &str, fill: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(label).strong().color(Color32::WHITE)).fill(fill)
}

fn report(app: &mut App, result: std::io::Result<()>) {
    if let Err(e) = result {
        app.notice = Some(Err(format!("Could not open the file manager: {}", e)));
    }
}

/// Badge text and color for a download's status.
fn status_badge(app: &App, item: &QueueItem) -> (&'static str, Color32) {
    match &item.status {
        QueueItemStatus::Queued => ("QUEUED", SLATE),
        QueueItemStatus::Downloading if app.is_resolving(item.id) => ("RESOLVING", AMBER),
        QueueItemStatus::Downloading if item.is_finishing() => ("FINISHING", CYAN),
        QueueItemStatus::Downloading if app.stalled_for(item.id).is_some() => ("STALLED", AMBER),
        QueueItemStatus::Downloading => ("DOWNLOADING", BRIGHT_BLUE),
        QueueItemStatus::Pausing => ("PAUSING", MUTED),
        QueueItemStatus::Paused => ("PAUSED", MUTED),
        QueueItemStatus::Completed => ("COMPLETED", GREEN),
        QueueItemStatus::Failed(_) => ("FAILED", RED),
        QueueItemStatus::AuthRequired => ("NEEDS AUTH", AMBER),
    }
}

pub fn render(app: &mut App, ui: &mut egui::Ui) {
    ui.add_space(8.0);
    header(app, ui);
    clipboard_banner(app, ui);
    if let Some(notice) = app.notice.clone() {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let (text, color) = match &notice {
                Ok(text) => (text, GREEN),
                Err(text) => (text, RED),
            };
            ui.label(RichText::new(text).size(12.0).color(color));
            if ui.small_button("x").on_hover_text("Dismiss").clicked() {
                app.notice = None;
            }
        });
    }
    ui.add_space(8.0);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match app.tab {
        Tab::Downloader => downloader_tab(app, ui),
        Tab::Queue => queue_tab(app, ui),
        Tab::History => history_tab(app, ui),
    });
}

fn tab_button(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    let button = egui::Button::new(RichText::new(label).strong().color(if selected { Color32::WHITE } else { MUTED }))
        .fill(if selected { BLUE } else { Color32::from_rgb(30, 32, 40) });
    ui.add_sized([100.0, 24.0], button).clicked()
}

fn header(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.heading(RichText::new("ENDO'S UNIFIED DOWNLOADER").strong().size(19.0).color(Color32::WHITE));
        ui.add_space(8.0);
        let (text, color) = app.focused_item().map_or(("READY", SLATE), |item| status_badge(app, item));
        ui.label(RichText::new(format!("[ {} ]", text)).monospace().size(13.0).color(color));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if tab_button(ui, &format!("History ({})", app.history.len()), app.tab == Tab::History) {
                app.tab = Tab::History;
                app.refresh_history();
            }
            if tab_button(ui, &format!("Queue ({})", app.queue.items().len()), app.tab == Tab::Queue) {
                app.tab = Tab::Queue;
            }
            if tab_button(ui, "Downloader", app.tab == Tab::Downloader) {
                app.tab = Tab::Downloader;
            }

            let watching = app.settings.clipboard_watch;
            let clip = egui::Button::new(
                RichText::new(if watching { "Clipboard Watch: ON" } else { "Clipboard Watch: OFF" })
                    .size(11.0)
                    .color(if watching { CYAN } else { MUTED }),
            )
            .fill(Color32::from_rgb(26, 28, 35));
            if ui.add_sized([135.0, 24.0], clip).clicked() {
                app.settings.clipboard_watch = !watching;
                app.clipboard_enabled.store(!watching, Ordering::Relaxed);
                if watching {
                    app.clipboard_banner = None;
                }
            }
        });
    });
}

fn clipboard_banner(app: &mut App, ui: &mut egui::Ui) {
    let Some(link) = app.clipboard_banner.clone() else { return };
    ui.add_space(6.0);
    egui::Frame::none()
        .fill(Color32::from_rgb(22, 27, 38))
        .stroke(Stroke::new(1.0, BLUE))
        .inner_margin(8.0)
        .rounding(4.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Clipboard Link Detected:").strong().color(CYAN));
                ui.label(RichText::new(truncate_chars(&link, 55)).monospace().color(TEXT)).on_hover_text(&link);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(RichText::new("Dismiss").size(11.0)).clicked() {
                        app.clipboard_banner = None;
                    }
                    if ui.button(RichText::new("Paste").size(11.0)).clicked() {
                        app.clipboard_banner = None;
                        app.new_download();
                        app.url_input = link.clone();
                        app.tab = Tab::Downloader;
                    }
                    if ui.button(RichText::new("Add to Queue").size(11.0)).clicked() {
                        app.clipboard_banner = None;
                        // Link from the clipboard: the per-download checksum and Authorization header don't apply.
                        app.notice = Some(app.add_download(&link, "", "").map(|id| format!("Added #{} to the queue", id)));
                    }
                    if ui.add(primary_button("Download Now", BLUE)).clicked() {
                        app.clipboard_banner = None;
                        app.download_now(&link, "", "");
                    }
                });
            });
        });
}

// ---- Downloader tab ----------------------------------------------------------------------

fn downloader_tab(app: &mut App, ui: &mut egui::Ui) {
    let focused = app.focused_item().cloned();
    let active = focused.as_ref().is_some_and(|item| item.status.is_active());

    card().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("URL:").strong().size(13.0));
            let width = ui.available_width() - 85.0;
            match &focused {
                // The shown download keeps its own URLs; "New Download" frees the field.
                Some(item) => {
                    let urls = item.urls.iter().map(|u| u.as_str()).collect::<Vec<_>>().join(" ");
                    ui.add_sized([width, 26.0], egui::TextEdit::singleline(&mut urls.as_str()));
                }
                None => {
                    let edit = egui::TextEdit::singleline(&mut app.url_input)
                        .hint_text(hint_text("File URL, media link (YouTube, Vimeo, Reddit, ...) or magnet link with web seeds"));
                    let response = ui.add_sized([width, 26.0], edit);
                    if response.changed() {
                        app.form_error = None;
                    }
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        start_from_form(app);
                    }
                    if ui.button("Paste").clicked() {
                        app.paste_url();
                    }
                }
            }
        });

        if focused.is_none() && util::is_blob_url(&app.url_input) {
            error_alert(ui, util::BLOB_MESSAGE);
        }
        if let Some(error) = &app.form_error {
            error_alert(ui, error);
        }
        if let Some(QueueItemStatus::Failed(error)) = focused.as_ref().map(|item| &item.status) {
            error_alert(ui, &format!("Download failed: {}", error));
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Save to:").strong().size(13.0));
            ui.add_sized([ui.available_width() - 85.0, 26.0], egui::TextEdit::singleline(&mut app.settings.save_dir));
            if ui.add_enabled(!app.dialog_open, egui::Button::new("Browse...")).clicked() {
                app.pending_dialog = Some(Dialog::SaveDir);
            }
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("Streams:");
            ui.add_enabled(!active, egui::Slider::new(&mut app.settings.connections, 1..=64).text("connections"))
                .on_disabled_hover_text("A running download keeps the connection count it started with");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                download_actions(app, ui, focused.as_ref());
            });
        });

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);
        let marker = if app.show_advanced { "[-]" } else { "[+]" };
        let advanced = format!("{} Advanced Options (Checksum, Cookies, Proxy, Speed Limit, Retries)", marker);
        if ui.button(RichText::new(advanced).size(12.0).color(MUTED)).clicked() {
            app.show_advanced = !app.show_advanced;
        }
        if app.show_advanced {
            advanced_options(app, ui);
        }
    });

    ui.add_space(10.0);
    progress_card(app, ui, focused.as_ref());

    let view = focused.as_ref().and_then(|item| app.jobs.get(&item.id));
    let chunks: &[ChunkSnapshot] = view.map_or(&[], |v| &v.chunks);
    // Once the engine has stopped, chunks it last reported as in flight are no longer being fetched.
    let live = view.is_some_and(|v| v.running.is_some());
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("CHUNK ALLOCATION & WORK-STEALING MAP").strong().size(13.0));
        if !chunks.is_empty() {
            ui.label(RichText::new(format!("({} dynamic chunks)", chunks.len())).size(11.0).color(MUTED));
        }
    });
    ui.add_space(4.0);
    chunk_map(ui, chunks, focused.as_ref().map_or(0, |item| item.total_bytes), app.pulse_phase, live);
    ui.add_space(10.0);
    chunk_table(ui, chunks, live);

    ui.add_space(6.0);
    let (status, color) = status_line(app, focused.as_ref());
    ui.label(RichText::new(status).size(12.0).color(color));

    ui.add_space(10.0);
    let history = focused.as_ref().and_then(|item| app.jobs.get(&item.id)).map(|view| &view.speed_history);
    throughput_graph(ui, history, app.anim_speed, app.pulse_phase);
}

fn start_from_form(app: &mut App) {
    let (text, checksum, auth) = (app.url_input.clone(), app.checksum_input.clone(), app.auth_input.clone());
    app.download_now(&text, &checksum, &auth);
}

fn download_actions(app: &mut App, ui: &mut egui::Ui, item: Option<&QueueItem>) {
    let Some(item) = item else {
        if ui.add_sized([130.0, 28.0], primary_button("Start Download", BLUE)).clicked() {
            start_from_form(app);
        }
        return;
    };
    let id = item.id;
    match &item.status {
        QueueItemStatus::Queued => {
            if ui.button("New Download").clicked() {
                app.new_download();
            }
            if ui.add_sized([110.0, 26.0], primary_button("Start Now", BLUE)).clicked() {
                app.start_job(id, false);
            }
        }
        QueueItemStatus::Downloading => {
            if ui.add_sized([100.0, 26.0], egui::Button::new(RichText::new("Pause").strong().color(RED))).clicked() {
                app.pause_job(id);
            }
        }
        QueueItemStatus::Pausing => {
            ui.label(RichText::new("Pausing... saving resume state").color(MUTED));
            ui.add(egui::Spinner::new());
        }
        QueueItemStatus::Paused | QueueItemStatus::Failed(_) => {
            let resume = if item.status == QueueItemStatus::Paused { "Resume" } else { "Retry" };
            if ui.add_sized([110.0, 26.0], primary_button(resume, GREEN)).on_hover_text("Continue from the saved state").clicked() {
                app.start_job(id, false);
            }
            if ui.button("Start Over").on_hover_text("Delete the partial file and download from the beginning").clicked() {
                app.start_job(id, true);
            }
            let delete = egui::Button::new(RichText::new("Delete Leftovers").color(RED));
            if ui.add(delete).on_hover_text("Remove this download and delete its partial file and resume state").clicked() {
                app.discard_job(id);
            }
            if ui.button("New Download").on_hover_text("Keep this download in the queue and start another").clicked() {
                app.new_download();
            }
        }
        QueueItemStatus::AuthRequired => {
            let ready = !app.auth_input.trim().is_empty();
            let resume = ui
                .add_enabled_ui(ready, |ui| ui.add_sized([110.0, 26.0], primary_button("Resume", GREEN)))
                .inner
                .on_hover_text("Continue from the saved state with the Authorization header entered under Advanced Options")
                .on_disabled_hover_text("Enter the Authorization header under Advanced Options > Auth first");
            if resume.clicked() {
                app.resume_with_auth(id);
            }
            if ui.button("New Download").on_hover_text("Keep this download in the queue and start another").clicked() {
                app.new_download();
            }
        }
        QueueItemStatus::Completed => {
            if ui.button("Download Another").clicked() {
                app.new_download();
            }
            if let Some(path) = &item.target_path {
                let idle = !app.verifying && app.repair.is_none();
                if ui.add_enabled(idle, egui::Button::new(RichText::new("Verify").color(CYAN))).clicked() {
                    app.verify(VerifyRequest {
                        path: path.clone(),
                        expected_size: (item.total_bytes > 0).then_some(item.total_bytes),
                        checksum: item.options.expected_checksum.clone(),
                    });
                }
                if ui.button("Open Folder").clicked() {
                    report(app, util::reveal_in_folder(path));
                }
                if ui.button("Open File").clicked() {
                    report(app, util::open_path(path));
                }
            }
        }
    }
}

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.0));
        ui.add_sized([ui.available_width() - 10.0, 24.0], egui::TextEdit::singleline(value).hint_text(hint_text(hint)));
    });
}

fn combo(ui: &mut egui::Ui, id: &str, value: &mut usize, names: &[&str]) {
    let selected = names.get(*value).or(names.first()).copied().unwrap_or_default();
    egui::ComboBox::from_id_salt(id).selected_text(selected).show_ui(ui, |ui| {
        for (i, name) in names.iter().enumerate() {
            ui.selectable_value(value, i, *name);
        }
    });
}

fn advanced_options(app: &mut App, ui: &mut egui::Ui) {
    text_row(ui, "Checksum:", &mut app.checksum_input, "Optional, this download only: sha256:..., md5:..., blake3:..., or hex");

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Cookies:").size(12.0));
        ui.add_sized(
            [ui.available_width() - 95.0, 24.0],
            egui::TextEdit::singleline(&mut app.settings.cookies_path).hint_text(hint_text("Optional: path to Netscape cookies.txt")),
        );
        if ui.add_enabled(!app.dialog_open, egui::Button::new("Browse...")).clicked() {
            app.pending_dialog = Some(Dialog::CookiesFile);
        }
    });

    text_row(ui, "Proxy:", &mut app.settings.proxy, "Optional: http://127.0.0.1:8080 or socks5://127.0.0.1:1080");
    text_row(ui, "Auth:", &mut app.auth_input, "Optional Authorization header, e.g. Bearer <token> (not saved, not sent to clipboard links)");

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Media Quality:").size(12.0));
        combo(ui, "media_preset_combo", &mut app.settings.media_preset, &MEDIA_PRESETS);
        ui.add_space(12.0);
        ui.label(RichText::new("Browser Cookies:").size(12.0));
        combo(ui, "browser_cookies_combo", &mut app.settings.browser_cookies, &BROWSERS);
    });

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Speed Limit:").size(12.0));
        ui.add(egui::DragValue::new(&mut app.settings.max_speed).range(0.0..=1_000_000.0).speed(1.0).max_decimals(1));
        let in_mb = &mut app.settings.max_speed_in_mb;
        egui::ComboBox::from_id_salt("speed_unit_combo")
            .width(64.0)
            .selected_text(if *in_mb { "MB/s" } else { "KB/s" })
            .show_ui(ui, |ui| {
                ui.selectable_value(in_mb, false, "KB/s");
                ui.selectable_value(in_mb, true, "MB/s");
            });
        ui.label(RichText::new("(0 = unlimited)").size(11.0).color(MUTED));
        ui.add_space(12.0);
        ui.label(RichText::new("Max Retries:").size(12.0));
        ui.add(egui::DragValue::new(&mut app.settings.max_retries).range(0..=100))
            .on_hover_text("Failed attempts per chunk before the download fails; attempts that made progress don't count");
        ui.add_space(12.0);
        ui.label(RichText::new("Stall Timeout:").size(12.0));
        ui.add(egui::DragValue::new(&mut app.settings.stall_timeout_secs).range(5..=600).suffix(" s"))
            .on_hover_text("A connection that receives nothing for this long is retried");
    });
    ui.label(RichText::new("Settings apply to downloads started or queued afterwards.").size(11.0).color(DIM));
}

fn metric(ui: &mut egui::Ui, label: &str, value: String, color: Option<Color32>) {
    ui.vertical(|ui| {
        ui.label(RichText::new(label).size(11.0).color(MUTED));
        let text = RichText::new(value).strong().size(13.0);
        ui.label(match color {
            Some(color) => text.color(color),
            None => text,
        });
    });
}

fn progress_card(app: &App, ui: &mut egui::Ui, item: Option<&QueueItem>) {
    let view = item.and_then(|item| app.jobs.get(&item.id));
    let downloading = item.is_some_and(|item| item.status == QueueItemStatus::Downloading);
    card().show(ui, |ui| {
        ui.add(
            egui::ProgressBar::new(app.anim_progress as f32)
                .animate(downloading)
                .text(format!("{:.1}%", (app.anim_progress * 100.0).clamp(0.0, 100.0))),
        );
        ui.add_space(8.0);

        let (downloaded, total) = item.map_or((0, 0), |item| (item.downloaded_bytes, item.total_bytes));
        let eta = match item {
            Some(item) if item.status == QueueItemStatus::Completed => "Done".to_string(),
            Some(item) if downloading => util::eta_secs(item.total_bytes, item.downloaded_bytes, item.speed_bytes_per_sec)
                .map_or_else(|| "--:--".to_string(), format_duration),
            _ => "--:--".to_string(),
        };
        let elapsed = view.map_or(0, |view| view.elapsed().as_secs());
        ui.columns(4, |cols| {
            let transferred = if total > 0 {
                format!("{} / {}", format_bytes(downloaded), format_bytes(total))
            } else {
                format_bytes(downloaded)
            };
            metric(&mut cols[0], "Transferred", transferred, None);
            metric(&mut cols[1], "Speed", format!("{}/s", format_bytes(app.anim_speed as u64)), Some(CYAN));
            metric(&mut cols[2], "Elapsed / ETA", format!("{} / {}", format_duration(elapsed), eta), None);
            let connections = match view {
                Some(view) if downloading => format!("{} active", view.active_workers),
                _ => "none".to_string(),
            };
            metric(&mut cols[3], "Connections", connections, None);
        });

        if let (Some(item), true) = (item, downloading) {
            if let Some(stalled) = app.stalled_for(item.id) {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "STALLED: no data received for {}s. Stalled connections are retried after {}s without data.",
                        stalled.as_secs(),
                        item.options.stall_timeout_secs
                    ))
                    .strong()
                    .color(AMBER),
                );
            }
        }

        if let Some(view) = view.filter(|view| view.mirror_speeds.len() > 1) {
            ui.add_space(8.0);
            ui.label(RichText::new("MIRRORS").strong().size(11.0).color(MUTED));
            egui::Grid::new("mirror_speeds").num_columns(3).striped(true).spacing([24.0, 2.0]).show(ui, |ui| {
                for (id, host, speed) in &view.mirror_speeds {
                    ui.label(RichText::new(format!("#{}", id)).size(11.0));
                    ui.label(RichText::new(host).size(11.0).monospace());
                    ui.label(RichText::new(format!("{}/s", format_bytes(*speed as u64))).size(11.0).color(CYAN));
                    ui.end_row();
                }
            });
        }
    });
}

fn status_line(app: &App, item: Option<&QueueItem>) -> (String, Color32) {
    let Some(item) = item else { return ("Ready to accelerate download".to_string(), MUTED) };
    match &item.status {
        QueueItemStatus::Queued => ("Queued: starts when a download slot is free (see the Queue tab)".to_string(), MUTED),
        QueueItemStatus::Downloading if app.is_resolving(item.id) => {
            ("Resolving mirrors and probing endpoints...".to_string(), MUTED)
        }
        QueueItemStatus::Downloading if item.is_finishing() => {
            (format!("Finishing {}: verifying the file and moving it into place...", item.filename), MUTED)
        }
        QueueItemStatus::Downloading => (format!("Downloading {}", item.filename), MUTED),
        QueueItemStatus::Pausing => ("Pausing: saving resume state...".to_string(), MUTED),
        QueueItemStatus::Paused => (
            "Paused. Resume continues where it stopped; Start Over deletes the partial file first.".to_string(),
            MUTED,
        ),
        QueueItemStatus::Completed => {
            let path = item.target_path.as_ref().map_or_else(|| item.filename.clone(), |p| p.display().to_string());
            (format!("Completed: {}", path), GREEN)
        }
        QueueItemStatus::Failed(error) => (format!("Error: {}", error), RED),
        QueueItemStatus::AuthRequired => (AUTH_REQUIRED.to_string(), AMBER),
    }
}

/// A worker owns the chunk ("Worker n" from the range engine, "Downloading" from the HLS engine).
fn is_active_chunk(chunk: &ChunkSnapshot) -> bool {
    chunk.status.starts_with("Worker") || chunk.status == "Downloading"
}

fn chunk_map(ui: &mut egui::Ui, chunks: &[ChunkSnapshot], total_bytes: u64, pulse_phase: f32, live: bool) {
    let canvas_height = 26.0;
    let (response, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), canvas_height), egui::Sense::hover());
    let rect = response.rect;
    painter.rect_filled(rect, 4.0, Color32::from_rgb(26, 28, 36));
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, Color32::from_rgb(45, 48, 60)));
    if chunks.is_empty() || total_bytes == 0 {
        return;
    }

    let total = total_bytes as f32;
    let width = rect.width();
    for chunk in chunks {
        let start_ratio = (chunk.range_start as f32 / total).clamp(0.0, 1.0);
        let end_ratio = ((chunk.range_end + 1) as f32 / total).clamp(0.0, 1.0);
        let seg_x = rect.min.x + start_ratio * width;
        let seg_w = ((end_ratio - start_ratio) * width).max(1.0);
        let seg_rect = Rect::from_min_size(Pos2::new(seg_x, rect.min.y + 1.0), Vec2::new(seg_w, canvas_height - 2.0));

        if chunk.total_bytes > 0 && chunk.downloaded_bytes >= chunk.total_bytes {
            painter.rect_filled(seg_rect, 0.0, GREEN);
        } else if chunk.total_bytes > 0 && chunk.downloaded_bytes > 0 {
            let filled = (chunk.downloaded_bytes as f32 / chunk.total_bytes as f32).clamp(0.0, 1.0);
            painter.rect_filled(seg_rect, 0.0, Color32::from_rgb(30, 58, 138));
            let fill_rect = Rect::from_min_size(seg_rect.min, Vec2::new(seg_w * filled, canvas_height - 2.0));
            painter.rect_filled(fill_rect, 0.0, BRIGHT_BLUE);
        } else if live && is_active_chunk(chunk) {
            // Assigned but no bytes yet: pulsing cyan.
            let pulse = (pulse_phase.sin() + 1.0) * 0.5;
            let color = Color32::from_rgb((14.0 + pulse * 20.0) as u8, (116.0 + pulse * 45.0) as u8, (144.0 + pulse * 70.0) as u8);
            painter.rect_filled(seg_rect, 0.0, color);
        } else if chunk.status.starts_with("Failed") {
            painter.rect_filled(seg_rect, 0.0, Color32::from_rgb(127, 29, 29));
        } else {
            painter.rect_filled(seg_rect, 0.0, Color32::from_rgb(39, 39, 42));
        }
        painter.line_segment(
            [Pos2::new(seg_x + seg_w, rect.min.y + 1.0), Pos2::new(seg_x + seg_w, rect.max.y - 1.0)],
            Stroke::new(1.0, Color32::from_rgb(18, 20, 24)),
        );
    }
}

fn chunk_table(ui: &mut egui::Ui, chunks: &[ChunkSnapshot], live: bool) {
    ui.label(RichText::new("STREAM / CHUNK DETAILS").strong().size(13.0));
    ui.add_space(4.0);
    card().inner_margin(8.0).show(ui, |ui| {
        if chunks.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.label(RichText::new("No active download streams. Enter a URL above and click 'Start Download'.").color(DIM));
                ui.add_space(20.0);
            });
            return;
        }
        ui.horizontal(|ui| {
            for (width, title) in [(50.0, "ID"), (160.0, "Byte Range"), (130.0, "Downloaded"), (140.0, "Progress"), (160.0, "Status")] {
                ui.add_sized([width, 20.0], egui::Label::new(RichText::new(title).strong().size(11.0)));
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
                    ui.add_sized(
                        [160.0, 18.0],
                        egui::Label::new(format!("{} - {}", format_bytes(chunk.range_start), format_bytes(chunk.range_end))),
                    );
                    ui.add_sized(
                        [130.0, 18.0],
                        egui::Label::new(format!("{} / {}", format_bytes(chunk.downloaded_bytes), format_bytes(chunk.total_bytes))),
                    );
                    let ratio = if chunk.total_bytes > 0 {
                        (chunk.downloaded_bytes as f32 / chunk.total_bytes as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    ui.add_sized([140.0, 18.0], egui::ProgressBar::new(ratio).show_percentage());
                    let color = if chunk.status == "Completed" {
                        GREEN
                    } else if live && is_active_chunk(chunk) {
                        CYAN
                    } else if chunk.status.starts_with("Failed") {
                        RED
                    } else {
                        MUTED
                    };
                    let status = if !live && is_active_chunk(chunk) { "Stopped" } else { chunk.status.as_str() };
                    ui.add_sized([160.0, 18.0], egui::Label::new(RichText::new(status).color(color)).truncate())
                        .on_hover_text(&chunk.status);
                });
            }
        });
    });
}

fn throughput_graph(ui: &mut egui::Ui, history: Option<&VecDeque<(Instant, f64)>>, current: f64, pulse_phase: f32) {
    let now = Instant::now();
    let window = GRAPH_WINDOW.as_secs_f32();
    // (age in seconds, speed) of the samples inside the window.
    let samples: Vec<(f32, f64)> = history
        .into_iter()
        .flatten()
        .map(|(t, speed)| (now.duration_since(*t).as_secs_f32(), *speed))
        .filter(|(age, _)| *age <= window)
        .collect();
    let peak = samples.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);
    let avg = if samples.is_empty() { 0.0 } else { samples.iter().map(|(_, s)| s).sum::<f64>() / samples.len() as f64 };

    ui.horizontal(|ui| {
        ui.label(RichText::new("THROUGHPUT GRAPH (LAST 60 SECONDS)").strong().size(12.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let stats = format!(
                "Peak: {}/s  |  Avg: {}/s  |  Current: {}/s",
                format_bytes(peak as u64),
                format_bytes(avg as u64),
                format_bytes(current as u64)
            );
            ui.label(RichText::new(stats).size(11.0).monospace().color(MUTED));
        });
    });
    ui.add_space(4.0);

    let graph_height = 80.0;
    let (response, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), graph_height), egui::Sense::hover());
    let rect = response.rect;
    painter.rect_filled(rect, 4.0, Color32::from_rgb(20, 22, 28));
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, Color32::from_rgb(38, 42, 53)));
    let y_mid = rect.center().y;
    painter.line_segment(
        [Pos2::new(rect.min.x, y_mid), Pos2::new(rect.max.x, y_mid)],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(45, 48, 60, 100)),
    );

    let max_y = (peak * 1.15).max(1024.0 * 1024.0);
    for (y, value) in [(rect.min.y + 4.0, max_y), (y_mid + 2.0, max_y / 2.0)] {
        painter.text(
            Pos2::new(rect.min.x + 6.0, y),
            egui::Align2::LEFT_TOP,
            format!("{}/s", format_bytes(value as u64)),
            egui::FontId::monospace(9.0),
            SLATE,
        );
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
        for p in &points {
            mesh.colored_vertex(*p, Color32::from_rgba_unmultiplied(37, 99, 235, 40));
            mesh.colored_vertex(Pos2::new(p.x, rect.max.y - 1.0), Color32::from_rgba_unmultiplied(37, 99, 235, 5));
        }
        for i in 0..(points.len() as u32 - 1) {
            let (top_left, bot_left, top_right, bot_right) = (i * 2, i * 2 + 1, i * 2 + 2, i * 2 + 3);
            mesh.add_triangle(top_left, bot_left, bot_right);
            mesh.add_triangle(top_left, bot_right, top_right);
        }
        painter.add(egui::Shape::mesh(mesh));
        painter.add(egui::Shape::line(points.clone(), Stroke::new(2.0, CYAN)));

        if let Some(&last) = points.last() {
            let pulse = (pulse_phase.sin() + 1.0) * 0.5;
            painter.circle_filled(last, 4.0 + pulse * 3.0, Color32::from_rgba_unmultiplied(56, 189, 248, (40.0 + pulse * 50.0) as u8));
            painter.circle_filled(last, 3.0, Color32::WHITE);
        }
    }

    if let Some(hover) = response.hover_pos().filter(|p| rect.contains(*p)) {
        let target_age = (1.0 - (hover.x - rect.min.x) / rect.width()) * window;
        let nearest = samples
            .iter()
            .min_by(|(a, _), (b, _)| (a - target_age).abs().total_cmp(&(b - target_age).abs()));
        if let Some((_, speed)) = nearest {
            painter.line_segment([Pos2::new(hover.x, rect.min.y), Pos2::new(hover.x, rect.max.y)], Stroke::new(1.0, MUTED));
            response.show_tooltip_text(format!("T-{}s: {}/s", target_age as u64, format_bytes(*speed as u64)));
        }
    }
}

// ---- Queue tab ---------------------------------------------------------------------------

enum RowAction {
    Start,
    Pause,
    Show,
    Remove,
    Open,
    Reveal,
}

fn queue_tab(app: &mut App, ui: &mut egui::Ui) {
    card().show(ui, |ui| {
        ui.label(RichText::new("Add to Queue").strong().size(13.0));
        ui.label(
            RichText::new("One download per line; separate mirrors of the same file with spaces. Uses the folder and options from the Downloader tab.")
                .size(11.0)
                .color(MUTED),
        );
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut app.queue_input)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text(hint_text("https://example.com/file1.iso\nhttps://example.com/file2.zip https://mirror.example.org/file2.zip")),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.add(primary_button("Add to Queue", BLUE)).clicked() {
                app.add_queue_input();
            }
            ui.add_space(16.0);
            ui.checkbox(&mut app.settings.auto_run_queue, "Auto-run queue");
            ui.label("Concurrent downloads:");
            ui.add(egui::DragValue::new(&mut app.settings.max_concurrent).range(1..=8));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Clear All").on_hover_text("Remove every download that is not running (files are kept)").clicked() {
                    app.clear_queue(false);
                }
                if ui.button("Clear Completed").clicked() {
                    app.clear_queue(true);
                }
            });
        });
        if let Some(error) = &app.queue_error {
            error_alert(ui, error);
        }
    });

    ui.add_space(10.0);
    ui.label(RichText::new("BATCH DOWNLOAD QUEUE").strong().size(13.0));
    ui.add_space(4.0);

    let items = app.queue.items().to_vec();
    let mut action = None;
    card().inner_margin(8.0).show(ui, |ui| {
        if items.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.label(RichText::new("Queue is empty. Add URLs above to build a batch queue.").color(DIM));
                ui.add_space(30.0);
            });
            return;
        }
        ui.horizontal(|ui| {
            for (width, title) in [(40.0, "ID"), (220.0, "File"), (95.0, "Status"), (150.0, "Progress"), (90.0, "Speed"), (150.0, "Actions")] {
                ui.add_sized([width, 20.0], egui::Label::new(RichText::new(title).strong().size(11.0)));
            }
        });
        ui.separator();
        for item in &items {
            ui.horizontal(|ui| {
                ui.add_sized([40.0, 18.0], egui::Label::new(format!("#{}", item.id)));
                ui.add_sized([220.0, 18.0], egui::Label::new(&item.filename).truncate()).on_hover_text(&item.filename);
                let (status, color) = status_badge(app, item);
                ui.add_sized([95.0, 18.0], egui::Label::new(RichText::new(status).monospace().size(11.0).color(color)));
                let progress = if item.total_bytes > 0 {
                    format!("{:.0}%", item.progress_ratio * 100.0)
                } else {
                    format_bytes(item.downloaded_bytes)
                };
                ui.add_sized([150.0, 16.0], egui::ProgressBar::new(item.progress_ratio as f32).text(progress));
                let speed = if item.status == QueueItemStatus::Downloading {
                    format!("{}/s", format_bytes(item.speed_bytes_per_sec as u64))
                } else {
                    String::new()
                };
                ui.add_sized([90.0, 18.0], egui::Label::new(RichText::new(speed).color(CYAN)));

                let mut button = |label: &str, what: RowAction| {
                    if ui.small_button(label).clicked() {
                        action = Some((item.id, what));
                    }
                };
                match &item.status {
                    QueueItemStatus::Queued => {
                        button("Start", RowAction::Start);
                        button("Details", RowAction::Show);
                        button("Remove", RowAction::Remove);
                    }
                    QueueItemStatus::Downloading => {
                        button("Pause", RowAction::Pause);
                        button("Details", RowAction::Show);
                    }
                    QueueItemStatus::Pausing => button("Details", RowAction::Show),
                    QueueItemStatus::Paused | QueueItemStatus::Failed(_) => {
                        let label = if item.status == QueueItemStatus::Paused { "Resume" } else { "Retry" };
                        button(label, RowAction::Start);
                        button("Details", RowAction::Show);
                        button("Remove", RowAction::Remove);
                    }
                    QueueItemStatus::AuthRequired => {
                        button("Details", RowAction::Show);
                        button("Remove", RowAction::Remove);
                    }
                    QueueItemStatus::Completed => {
                        button("Open", RowAction::Open);
                        button("Folder", RowAction::Reveal);
                        button("Remove", RowAction::Remove);
                    }
                }
            });
            match &item.status {
                QueueItemStatus::Failed(error) => {
                    ui.label(RichText::new(truncate_chars(error, 160)).size(11.0).color(RED)).on_hover_text(error);
                }
                QueueItemStatus::AuthRequired => {
                    ui.label(RichText::new("Authorization header not saved: open Details to enter it again and resume").size(11.0).color(AMBER));
                }
                _ => {}
            }
        }
    });

    let Some((id, action)) = action else { return };
    let target = app.queue.get_item(id).and_then(|item| item.target_path.clone());
    match action {
        RowAction::Start => {
            app.start_job(id, false);
        }
        RowAction::Pause => app.pause_job(id),
        RowAction::Show => app.show_job(id),
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
    }
}

// ---- History tab -------------------------------------------------------------------------

fn history_tab(app: &mut App, ui: &mut egui::Ui) {
    card().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Search History:").strong().size(13.0));
            ui.add_sized(
                [ui.available_width() - 330.0, 26.0],
                egui::TextEdit::singleline(&mut app.history_search).hint_text(hint_text("Filter by filename, URL, or hash...")),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Clear History").clicked() {
                    app.update_history(|history| history.clear());
                }
                if ui.button("Refresh").clicked() {
                    app.refresh_history();
                }
                let can_verify = !app.dialog_open && !app.verifying && app.repair.is_none();
                if ui.add_enabled(can_verify, egui::Button::new("Verify Build File...")).clicked() {
                    app.pending_dialog = Some(Dialog::VerifyFile);
                }
            });
        });
        verification_card(app, ui);
    });

    ui.add_space(8.0);
    card().show(ui, |ui| {
        let search = app.history_search.trim().to_lowercase();
        let entries: Vec<_> = app
            .history
            .iter()
            .filter(|e| {
                search.is_empty()
                    || e.file_name.to_lowercase().contains(&search)
                    || e.urls.iter().any(|u| u.to_lowercase().contains(&search))
                    || e.blake3_hash.as_ref().is_some_and(|h| h.to_lowercase().contains(&search))
            })
            .cloned()
            .collect();

        if entries.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.label(RichText::new("No downloads recorded in history yet.").color(DIM).size(13.0));
                ui.add_space(30.0);
            });
            return;
        }
        let idle = !app.verifying && app.repair.is_none();
        for entry in entries {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    let (badge, color) = match entry.status {
                        HistoryStatus::Completed => ("[ COMPLETED ]", GREEN),
                        HistoryStatus::Failed(_) => ("[ FAILED ]", RED),
                        HistoryStatus::Cancelled => ("[ CANCELLED ]", MUTED),
                    };
                    ui.label(RichText::new(badge).monospace().strong().size(11.0).color(color));
                    ui.label(RichText::new(&entry.file_name).strong().size(13.0).color(Color32::WHITE));
                    ui.label(RichText::new(format!("({})", format_bytes(entry.file_size))).size(11.0).color(MUTED));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(RichText::new("Remove").size(11.0).color(RED)).clicked() {
                            let id = entry.id.clone();
                            app.update_history(move |history| {
                                history.remove_entry(&id);
                            });
                        }
                        if ui.button(RichText::new("Redownload").size(11.0)).clicked() {
                            app.new_download();
                            app.url_input = entry.urls.join(" ");
                            app.tab = Tab::Downloader;
                        }
                        let verify = egui::Button::new(RichText::new("Verify & Repair").size(11.0).color(CYAN));
                        if ui.add_enabled(idle, verify).clicked() {
                            app.verify(VerifyRequest {
                                path: entry.file_path.clone(),
                                expected_size: (entry.file_size > 0).then_some(entry.file_size),
                                checksum: None,
                            });
                        }
                        if ui.button(RichText::new("Open Folder").size(11.0)).clicked() {
                            report(app, util::reveal_in_folder(&entry.file_path));
                        }
                        if let Some(url) = entry.urls.first() {
                            if ui.button(RichText::new("Copy Link").size(11.0)).clicked() {
                                app.copy_text(url.clone());
                            }
                        }
                    });
                });

                ui.horizontal(|ui| {
                    if let Some(hash) = &entry.blake3_hash {
                        ui.label(RichText::new(format!("BLAKE3: {} |", truncate_chars(hash, 19))).size(11.0).monospace().color(DIM))
                            .on_hover_text(hash);
                    }
                    let path = entry.file_path.display().to_string();
                    let label = RichText::new(format!("Path: {}", path)).size(11.0).monospace().color(DIM);
                    ui.add(egui::Label::new(label).truncate()).on_hover_text(path);
                });
            });
            ui.add_space(4.0);
        }
    });
}

fn verification_card(app: &mut App, ui: &mut egui::Ui) {
    if app.verifying {
        let name = app
            .verify_request
            .as_ref()
            .and_then(|r| r.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.label(RichText::new(format!("Verifying {}... hashing a large file can take a while", name)).color(MUTED));
        });
    }

    if let Some(verification) = &app.verification {
        let result = &verification.result;
        let verdict = util::verdict(result);
        let (badge, color) = match verdict {
            Verdict::Verified => ("[ VERIFIED ]", GREEN),
            Verdict::Incomplete => ("[ INCOMPLETE ]", AMBER),
            Verdict::Mismatch => ("[ CHECKSUM MISMATCH ]", RED),
            Verdict::Unverified => ("[ UNVERIFIED ]", MUTED),
        };
        let name = result.file_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let message = result.status_message.clone();
        let can_repair = !verification.repair_urls.is_empty();
        let repair_progress = app.repair.as_ref().map(|r| *lock(&r.progress));
        let (mut dismiss, mut repair, mut cancel) = (false, false, false);

        ui.add_space(10.0);
        egui::Frame::none()
            .fill(Color32::from_rgb(18, 24, 38))
            .stroke(Stroke::new(1.0, color))
            .inner_margin(10.0)
            .rounding(4.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(badge).monospace().strong().color(color));
                    ui.label(RichText::new(name).strong().color(Color32::WHITE));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        dismiss = ui.add_enabled(repair_progress.is_none(), egui::Button::new("Dismiss")).clicked();
                        if verdict == Verdict::Incomplete && repair_progress.is_none() {
                            let button = primary_button("Repair Missing Chunks Now", BLUE);
                            repair = ui
                                .add_enabled(can_repair, button)
                                .on_disabled_hover_text("No download URLs are recorded for exactly this file")
                                .clicked();
                        }
                    });
                });
                ui.label(RichText::new(message).color(TEXT).size(12.0));

                if let Some((done, total)) = repair_progress {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        cancel = ui.button(RichText::new("Cancel Repair").color(RED)).clicked();
                        let ratio = if total > 0 { (done as f32 / total as f32).clamp(0.0, 1.0) } else { 0.0 };
                        ui.add(egui::ProgressBar::new(ratio).show_percentage().text(format!(
                            "Repairing missing chunks: {} / {}",
                            format_bytes(done),
                            format_bytes(total)
                        )));
                    });
                }
            });

        if dismiss {
            app.verification = None;
            app.verify_message = None;
        }
        if repair {
            app.start_repair();
        }
        if cancel {
            if let Some(repair) = &app.repair {
                repair.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    if let Some(message) = &app.verify_message {
        ui.add_space(4.0);
        ui.label(RichText::new(message).size(11.0).color(MUTED));
    }
}
