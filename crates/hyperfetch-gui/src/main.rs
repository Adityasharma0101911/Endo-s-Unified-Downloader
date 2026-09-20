#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke, Vec2};
use hyperfetch_core::chunk::ChunkSnapshot;
use hyperfetch_core::engine::{DownloadEngine, DownloadOptions, EngineSnapshot};
use tokio::sync::broadcast;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
enum DownloadStatus {
    Idle,
    Resolving,
    Downloading,
    Completed,
    Failed(String),
    Cancelled,
}

struct DownloaderApp {
    url_input: String,
    save_dir: String,
    connections: usize,
    status: DownloadStatus,
    status_message: String,

    // Metrics
    total_bytes: u64,
    downloaded_bytes: u64,
    speed_bytes_per_sec: f64,
    progress_ratio: f64,
    start_time: Option<Instant>,
    elapsed_secs: u64,
    eta_secs: Option<u64>,
    target_filepath: Option<PathBuf>,

    // Chunks
    chunks: Vec<ChunkSnapshot>,

    // Threading & sync
    cancel_flag: Arc<AtomicBool>,
    snapshot_rx: Option<std::sync::mpsc::Receiver<EngineSnapshot>>,
    result_rx: Option<std::sync::mpsc::Receiver<Result<PathBuf, String>>>,
    tokio_rt: Arc<tokio::runtime::Runtime>,
}

impl DownloaderApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        // Setup dark theme
        let mut style = (*_cc.egui_ctx.style()).clone();
        style.visuals.dark_mode = true;
        style.visuals.override_text_color = Some(Color32::from_rgb(228, 232, 240));
        style.visuals.window_fill = Color32::from_rgb(18, 20, 24);
        style.visuals.panel_fill = Color32::from_rgb(18, 20, 24);
        style.visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(26, 28, 35);
        style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(32, 35, 45);
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(45, 50, 65);
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(30, 64, 175);
        _cc.egui_ctx.set_style(style);

        let default_dir = std::env::var("USERPROFILE")
            .map(|p| PathBuf::from(p).join("Downloads"))
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .to_string();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to initialize Tokio runtime");

        Self {
            url_input: String::new(),
            save_dir: default_dir,
            connections: 16,
            status: DownloadStatus::Idle,
            status_message: "Ready to accelerate download".to_string(),

            total_bytes: 0,
            downloaded_bytes: 0,
            speed_bytes_per_sec: 0.0,
            progress_ratio: 0.0,
            start_time: None,
            elapsed_secs: 0,
            eta_secs: None,
            target_filepath: None,

            chunks: Vec::new(),

            cancel_flag: Arc::new(AtomicBool::new(false)),
            snapshot_rx: None,
            result_rx: None,
            tokio_rt: Arc::new(rt),
        }
    }

    fn start_download(&mut self) {
        let trimmed = self.url_input.trim();
        if trimmed.is_empty() {
            self.status = DownloadStatus::Failed("Please provide a valid download URL".to_string());
            return;
        }

        let mut urls = Vec::new();
        for u in trimmed.split_whitespace() {
            match Url::parse(u) {
                Ok(url) => urls.push(url),
                Err(e) => {
                    self.status = DownloadStatus::Failed(format!("Invalid URL '{}': {}", u, e));
                    return;
                }
            }
        }

        self.status = DownloadStatus::Resolving;
        self.status_message = "Resolving mirrors and probing endpoints...".to_string();
        self.total_bytes = 0;
        self.downloaded_bytes = 0;
        self.speed_bytes_per_sec = 0.0;
        self.progress_ratio = 0.0;
        self.start_time = Some(Instant::now());
        self.elapsed_secs = 0;
        self.eta_secs = None;
        self.target_filepath = None;
        self.chunks.clear();

        self.cancel_flag.store(false, Ordering::Relaxed);
        let cancel_flag = Arc::clone(&self.cancel_flag);

        let (sync_snapshot_tx, sync_snapshot_rx) = std::sync::mpsc::channel::<EngineSnapshot>();
        self.snapshot_rx = Some(sync_snapshot_rx);

        let (sync_result_tx, sync_result_rx) = std::sync::mpsc::channel::<Result<PathBuf, String>>();
        self.result_rx = Some(sync_result_rx);

        let connections = self.connections;
        let save_dir = PathBuf::from(&self.save_dir);

        let (async_snapshot_tx, mut async_snapshot_rx) = broadcast::channel::<EngineSnapshot>(128);

        // Bridge Tokio broadcast to standard mpsc channel for UI thread
        self.tokio_rt.spawn(async move {
            while let Ok(snapshot) = async_snapshot_rx.recv().await {
                if sync_snapshot_tx.send(snapshot).is_err() {
                    break;
                }
            }
        });

        // Spawn engine download task
        self.tokio_rt.spawn(async move {
            let options = DownloadOptions {
                num_connections: connections,
                base_chunk_size: 4 * 1024 * 1024,
                min_steal_threshold: 1024 * 1024,
                output_path: Some(save_dir),
            };

            let engine = DownloadEngine::new(urls, options);

            // Spawn cancellation listener
            let cancel_watcher = Arc::clone(&cancel_flag);
            let result = tokio::select! {
                res = engine.run(Some(async_snapshot_tx)) => res,
                _ = async {
                    while !cancel_watcher.load(Ordering::Relaxed) {
                        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    }
                    engine.cancel();
                } => Err("Download cancelled by user".to_string()),
            };

            let _ = sync_result_tx.send(result);
        });
    }

    fn cancel_download(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        self.status = DownloadStatus::Cancelled;
        self.status_message = "Download cancelled".to_string();
        self.speed_bytes_per_sec = 0.0;
    }

    fn reset_state(&mut self) {
        self.status = DownloadStatus::Idle;
        self.status_message = "Ready to accelerate download".to_string();
        self.url_input.clear();
        self.total_bytes = 0;
        self.downloaded_bytes = 0;
        self.speed_bytes_per_sec = 0.0;
        self.progress_ratio = 0.0;
        self.start_time = None;
        self.elapsed_secs = 0;
        self.eta_secs = None;
        self.target_filepath = None;
        self.chunks.clear();
    }
}

impl eframe::App for DownloaderApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Handle incoming snapshots
        if let Some(ref rx) = self.snapshot_rx {
            while let Ok(snapshot) = rx.try_recv() {
                if self.status == DownloadStatus::Resolving {
                    self.status = DownloadStatus::Downloading;
                    self.status_message = "Accelerating multi-connection download...".to_string();
                }
                self.total_bytes = snapshot.total_bytes;
                self.downloaded_bytes = snapshot.downloaded_bytes;
                self.speed_bytes_per_sec = snapshot.speed_bytes_per_sec;
                self.progress_ratio = snapshot.progress_ratio;
                if !snapshot.chunks.is_empty() {
                    self.chunks = snapshot.chunks;
                }

                if let Some(start) = self.start_time {
                    self.elapsed_secs = start.elapsed().as_secs();
                }

                if self.speed_bytes_per_sec > 1024.0 && self.total_bytes > self.downloaded_bytes {
                    let remaining = self.total_bytes - self.downloaded_bytes;
                    self.eta_secs = Some((remaining as f64 / self.speed_bytes_per_sec) as u64);
                } else if self.total_bytes > 0 && self.downloaded_bytes >= self.total_bytes {
                    self.eta_secs = Some(0);
                }
            }
        }

        // Handle download completion or failure
        if let Some(ref rx) = self.result_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(path) => {
                        self.status = DownloadStatus::Completed;
                        self.status_message = format!("Completed: {}", path.display());
                        self.target_filepath = Some(path);
                        self.progress_ratio = 1.0;
                        self.downloaded_bytes = self.total_bytes;
                        self.speed_bytes_per_sec = 0.0;
                        self.eta_secs = Some(0);
                    }
                    Err(err) => {
                        if self.status != DownloadStatus::Cancelled {
                            self.status = DownloadStatus::Failed(err.clone());
                            self.status_message = format!("Error: {}", err);
                        }
                    }
                }
                self.snapshot_rx = None;
                self.result_rx = None;
            }
        }

        // Request continuous repaint while downloading for smooth 60 FPS progress
        if self.status == DownloadStatus::Downloading {
            ctx.request_repaint();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            render_ui(self, ui);
        });
    }
}

fn render_ui(app: &mut DownloaderApp, ui: &mut egui::Ui) {
    ui.add_space(8.0);

    // Header Bar
    ui.horizontal(|ui| {
        ui.heading(
            egui::RichText::new("ENDO'S UNIFIED DOWNLOADER")
                .strong()
                .size(19.0)
                .color(Color32::from_rgb(255, 255, 255)),
        );

        ui.add_space(8.0);
        let badge_color = match app.status {
            DownloadStatus::Idle => Color32::from_rgb(100, 116, 139),
            DownloadStatus::Resolving => Color32::from_rgb(234, 179, 8),
            DownloadStatus::Downloading => Color32::from_rgb(59, 130, 246),
            DownloadStatus::Completed => Color32::from_rgb(16, 185, 129),
            DownloadStatus::Failed(_) => Color32::from_rgb(239, 68, 68),
            DownloadStatus::Cancelled => Color32::from_rgb(148, 163, 184),
        };

        let status_text = match app.status {
            DownloadStatus::Idle => "READY",
            DownloadStatus::Resolving => "RESOLVING",
            DownloadStatus::Downloading => "DOWNLOADING",
            DownloadStatus::Completed => "COMPLETED",
            DownloadStatus::Failed(_) => "FAILED",
            DownloadStatus::Cancelled => "CANCELLED",
        };

        ui.label(
            egui::RichText::new(format!("[ {} ]", status_text))
                .monospace()
                .size(13.0)
                .color(badge_color),
        );
    });

    ui.add_space(10.0);

    // Input Group Box
    egui::Frame::none()
        .fill(Color32::from_rgb(24, 26, 33))
        .stroke(Stroke::new(1.0, Color32::from_rgb(42, 45, 56)))
        .inner_margin(12.0)
        .rounding(6.0)
        .show(ui, |ui| {
            // URL Input Row
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("URL:").strong().size(13.0));
                let text_edit = ui.add_sized(
                    [ui.available_width() - 85.0, 26.0],
                    egui::TextEdit::singleline(&mut app.url_input)
                        .hint_text("Paste media link or file URL (Archive.org, Vimeo, Reddit, Twitter, etc.)"),
                );

                if ui.button("Paste").clicked() {
                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                        if let Ok(text) = clipboard.get_text() {
                            app.url_input = text.trim().to_string();
                        }
                    }
                }
                text_edit
            });

            ui.add_space(8.0);

            // Save Directory Row
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Save to:").strong().size(13.0));
                ui.add_sized(
                    [ui.available_width() - 85.0, 26.0],
                    egui::TextEdit::singleline(&mut app.save_dir),
                );

                if ui.button("Browse...").clicked() {
                    if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                        app.save_dir = folder.to_string_lossy().to_string();
                    }
                }
            });

            ui.add_space(8.0);

            // Options and Actions Row
            ui.horizontal(|ui| {
                ui.label("Streams:");
                ui.add(egui::Slider::new(&mut app.connections, 1..=32).text("connections"));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match app.status {
                        DownloadStatus::Downloading | DownloadStatus::Resolving => {
                            if ui.button(egui::RichText::new("Cancel").color(Color32::from_rgb(239, 68, 68))).clicked() {
                                app.cancel_download();
                            }
                        }
                        DownloadStatus::Completed => {
                            if let Some(ref path) = app.target_filepath {
                                if ui.button("Open File").clicked() {
                                    #[cfg(target_os = "windows")]
                                    let _ = std::process::Command::new("explorer").arg(path).spawn();
                                    #[cfg(not(target_os = "windows"))]
                                    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
                                }
                                if ui.button("Open Folder").clicked() {
                                    if let Some(parent) = path.parent() {
                                        #[cfg(target_os = "windows")]
                                        let _ = std::process::Command::new("explorer").arg(parent).spawn();
                                        #[cfg(not(target_os = "windows"))]
                                        let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
                                    }
                                }
                            }
                            if ui.button("Download Another").clicked() {
                                app.reset_state();
                            }
                        }
                        _ => {
                            let btn = egui::Button::new(
                                egui::RichText::new("Start Download")
                                    .strong()
                                    .color(Color32::from_rgb(255, 255, 255)),
                            )
                            .fill(Color32::from_rgb(37, 99, 235));

                            if ui.add_sized([130.0, 28.0], btn).clicked() {
                                app.start_download();
                            }
                        }
                    }
                });
            });
        });

    ui.add_space(10.0);

    // Overall Progress & Metric Cards
    egui::Frame::none()
        .fill(Color32::from_rgb(24, 26, 33))
        .stroke(Stroke::new(1.0, Color32::from_rgb(42, 45, 56)))
        .inner_margin(12.0)
        .rounding(6.0)
        .show(ui, |ui| {
            // Main Progress Bar
            let progress_percent = (app.progress_ratio * 100.0).clamp(0.0, 100.0);
            let bar_text = format!("{:.1}%", progress_percent);
            ui.add(
                egui::ProgressBar::new(app.progress_ratio as f32)
                    .show_percentage()
                    .animate(app.status == DownloadStatus::Downloading)
                    .text(bar_text),
            );

            ui.add_space(8.0);

            // Metrics Grid (4 columns)
            ui.columns(4, |cols| {
                // Column 1: Downloaded / Total
                cols[0].vertical(|ui| {
                    ui.label(egui::RichText::new("Transferred").size(11.0).color(Color32::from_rgb(148, 163, 184)));
                    let text = if app.total_bytes > 0 {
                        format!("{} / {}", format_bytes(app.downloaded_bytes), format_bytes(app.total_bytes))
                    } else {
                        format_bytes(app.downloaded_bytes)
                    };
                    ui.label(egui::RichText::new(text).strong().size(13.0));
                });

                // Column 2: Speed
                cols[1].vertical(|ui| {
                    ui.label(egui::RichText::new("Speed").size(11.0).color(Color32::from_rgb(148, 163, 184)));
                    let speed_text = format!("{}/s", format_bytes(app.speed_bytes_per_sec as u64));
                    ui.label(egui::RichText::new(speed_text).strong().size(13.0).color(Color32::from_rgb(56, 189, 248)));
                });

                // Column 3: Elapsed & ETA
                cols[2].vertical(|ui| {
                    ui.label(egui::RichText::new("Elapsed / ETA").size(11.0).color(Color32::from_rgb(148, 163, 184)));
                    let eta_str = match app.eta_secs {
                        Some(0) => "Done".to_string(),
                        Some(s) => format_duration(s),
                        None => "--:--".to_string(),
                    };
                    let time_text = format!("{} / {}", format_duration(app.elapsed_secs), eta_str);
                    ui.label(egui::RichText::new(time_text).strong().size(13.0));
                });

                // Column 4: Connections
                cols[3].vertical(|ui| {
                    ui.label(egui::RichText::new("Active Streams").size(11.0).color(Color32::from_rgb(148, 163, 184)));
                    let streams_text = format!("{} connections", app.connections);
                    ui.label(egui::RichText::new(streams_text).strong().size(13.0));
                });
            });
        });

    ui.add_space(10.0);

    // Visual Multi-Segment Chunk Map (IDM-style)
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("CHUNK ALLOCATION & WORK-STEALING MAP").strong().size(13.0));
        if !app.chunks.is_empty() {
            ui.label(
                egui::RichText::new(format!("({} dynamic chunks)", app.chunks.len()))
                    .size(11.0)
                    .color(Color32::from_rgb(148, 163, 184)),
            );
        }
    });

    ui.add_space(4.0);

    // Custom Canvas for Chunk Visualizer
    let canvas_height = 26.0;
    let (response, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), canvas_height), egui::Sense::hover());
    let rect = response.rect;

    // Background of chunk bar
    painter.rect_filled(rect, 4.0, Color32::from_rgb(26, 28, 36));
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, Color32::from_rgb(45, 48, 60)));

    if !app.chunks.is_empty() && app.total_bytes > 0 {
        let total_b = app.total_bytes as f32;
        let width = rect.width();

        for chunk in &app.chunks {
            let start_ratio = (chunk.range_start as f32 / total_b).clamp(0.0, 1.0);
            let end_ratio = (chunk.range_end as f32 / total_b).clamp(0.0, 1.0);
            let seg_x = rect.min.x + (start_ratio * width);
            let seg_w = ((end_ratio - start_ratio) * width).max(1.0);

            let seg_rect = Rect::from_min_size(Pos2::new(seg_x, rect.min.y + 1.0), Vec2::new(seg_w, canvas_height - 2.0));

            if chunk.downloaded_bytes >= chunk.total_bytes && chunk.total_bytes > 0 {
                // Completed: solid green
                painter.rect_filled(seg_rect, 0.0, Color32::from_rgb(16, 185, 129));
            } else if chunk.downloaded_bytes > 0 && chunk.total_bytes > 0 {
                // In-progress: background blue + bright fill
                let filled_ratio = (chunk.downloaded_bytes as f32 / chunk.total_bytes as f32).clamp(0.0, 1.0);
                painter.rect_filled(seg_rect, 0.0, Color32::from_rgb(30, 58, 138));

                let fill_w = seg_w * filled_ratio;
                let fill_rect = Rect::from_min_size(Pos2::new(seg_x, rect.min.y + 1.0), Vec2::new(fill_w, canvas_height - 2.0));
                painter.rect_filled(fill_rect, 0.0, Color32::from_rgb(59, 130, 246));
            } else if chunk.status.contains("Worker") {
                // Assigned worker: active cyan stroke
                painter.rect_filled(seg_rect, 0.0, Color32::from_rgb(14, 116, 144));
            } else {
                // Pending: dark gray
                painter.rect_filled(seg_rect, 0.0, Color32::from_rgb(39, 39, 42));
            }

            // Segment border separator
            painter.line_segment(
                [Pos2::new(seg_x + seg_w, rect.min.y + 1.0), Pos2::new(seg_x + seg_w, rect.max.y - 1.0)],
                Stroke::new(1.0, Color32::from_rgb(18, 20, 24)),
            );
        }
    }

    ui.add_space(10.0);

    // Detailed Per-Chunk Table Header
    ui.label(egui::RichText::new("STREAM / CHUNK DETAILS").strong().size(13.0));
    ui.add_space(4.0);

    egui::Frame::none()
        .fill(Color32::from_rgb(24, 26, 33))
        .stroke(Stroke::new(1.0, Color32::from_rgb(42, 45, 56)))
        .inner_margin(8.0)
        .rounding(6.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(200.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if app.chunks.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(20.0);
                            ui.label(
                                egui::RichText::new("No active download streams. Enter a URL above and click 'Start Download'.")
                                    .color(Color32::from_rgb(113, 113, 122)),
                            );
                            ui.add_space(20.0);
                        });
                    } else {
                        // Header row
                        ui.horizontal(|ui| {
                            ui.add_sized([50.0, 20.0], egui::Label::new(egui::RichText::new("ID").strong().size(11.0)));
                            ui.add_sized([160.0, 20.0], egui::Label::new(egui::RichText::new("Byte Range").strong().size(11.0)));
                            ui.add_sized([130.0, 20.0], egui::Label::new(egui::RichText::new("Downloaded").strong().size(11.0)));
                            ui.add_sized([140.0, 20.0], egui::Label::new(egui::RichText::new("Progress").strong().size(11.0)));
                            ui.add_sized([120.0, 20.0], egui::Label::new(egui::RichText::new("Status").strong().size(11.0)));
                        });

                        ui.separator();

                        for chunk in &app.chunks {
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

                                let status_color = if chunk.status == "Completed" {
                                    Color32::from_rgb(16, 185, 129)
                                } else if chunk.status.contains("Worker") {
                                    Color32::from_rgb(56, 189, 248)
                                } else {
                                    Color32::from_rgb(148, 163, 184)
                                };
                                ui.add_sized([120.0, 18.0], egui::Label::new(egui::RichText::new(&chunk.status).color(status_color)));
                            });
                        }
                    }
                });
        });

    ui.add_space(6.0);

    // Status Footer
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(&app.status_message)
                .size(12.0)
                .color(Color32::from_rgb(148, 163, 184)),
        );
    });
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn format_duration(seconds: u64) -> String {
    let hrs = seconds / 3600;
    let mins = (seconds % 3600) / 60;
    let secs = seconds % 60;

    if hrs > 0 {
        format!("{:02}:{:02}:{:02}", hrs, mins, secs)
    } else {
        format!("{:02}:{:02}", mins, secs)
    }
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 680.0])
            .with_min_inner_size([750.0, 500.0])
            .with_title("Endo's Unified Downloader"),
        ..Default::default()
    };

    eframe::run_native(
        "Endo's Unified Downloader",
        options,
        Box::new(|cc| Ok(Box::new(DownloaderApp::new(cc)))),
    )
}
