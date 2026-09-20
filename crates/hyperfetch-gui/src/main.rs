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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuiTab {
    Downloader,
    BatchQueue,
}

struct DownloaderApp {
    url_input: String,
    save_dir: String,
    connections: usize,
    status: DownloadStatus,
    status_message: String,

    // Advanced options
    show_advanced: bool,
    checksum_input: String,
    cookies_path_input: String,
    proxy_input: String,
    auth_header_input: String,
    media_preset_idx: usize,
    browser_cookies_idx: usize,

    // Tabs & Batch Queue
    active_tab: GuiTab,
    queue: hyperfetch_core::queue::DownloadQueue,
    queue_url_input: String,

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

            show_advanced: false,
            checksum_input: String::new(),
            cookies_path_input: String::new(),
            proxy_input: String::new(),
            auth_header_input: String::new(),
            media_preset_idx: 0,
            browser_cookies_idx: 0,

            active_tab: GuiTab::Downloader,
            queue: hyperfetch_core::queue::DownloadQueue::new(),
            queue_url_input: String::new(),

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
        self.start_download_internal(false);
    }

    fn resume_download(&mut self) {
        self.start_download_internal(true);
    }

    fn start_download_internal(&mut self, is_resume: bool) {
        let trimmed = self.url_input.trim();
        if trimmed.is_empty() {
            self.status = DownloadStatus::Failed("Please provide a valid download URL".to_string());
            return;
        }

        // Check for blob / UUID URL
        if trimmed.starts_with("blob:") || (trimmed.contains("youtube.com") && trimmed.split('/').last().map_or(false, |s| s.len() == 36 && s.matches('-').count() == 4)) {
            self.status = DownloadStatus::Failed(
                "Browser-internal blob memory buffer detected. Browser blob: URLs exist only in temporary browser memory and cannot be downloaded by external tools. Please copy the standard video URL from your browser address bar (e.g. https://www.youtube.com/watch?v=... or https://youtu.be/...)."
                    .to_string(),
            );
            return;
        }

        let mut urls = Vec::new();
        for u in trimmed.split_whitespace() {
            if hyperfetch_core::torrent::is_magnet_uri(u) {
                match hyperfetch_core::torrent::parse_magnet_uri(u) {
                    Ok(magnet) => {
                        if !magnet.web_seeds.is_empty() {
                            urls.extend(magnet.web_seeds);
                            continue;
                        } else {
                            self.status = DownloadStatus::Failed(format!(
                                "Magnet link ingested ({}), but no HTTP web seeds were found in the magnet URI.",
                                magnet.info_hash
                            ));
                            return;
                        }
                    }
                    Err(e) => {
                        self.status = DownloadStatus::Failed(format!("Invalid magnet URI: {}", e));
                        return;
                    }
                }
            }

            match Url::parse(u) {
                Ok(url) => urls.push(url),
                Err(e) => {
                    self.status = DownloadStatus::Failed(format!("Invalid URL '{}': {}", u, e));
                    return;
                }
            }
        }

        self.status = DownloadStatus::Resolving;
        self.status_message = if is_resume {
            "Resuming multi-connection download...".to_string()
        } else {
            "Resolving mirrors and probing endpoints...".to_string()
        };

        if !is_resume {
            self.total_bytes = 0;
            self.downloaded_bytes = 0;
            self.progress_ratio = 0.0;
            self.target_filepath = None;
            self.chunks.clear();
        }

        self.speed_bytes_per_sec = 0.0;
        self.start_time = Some(Instant::now());
        self.elapsed_secs = 0;
        self.eta_secs = None;

        self.cancel_flag.store(false, Ordering::Relaxed);
        let cancel_flag = Arc::clone(&self.cancel_flag);

        let (sync_snapshot_tx, sync_snapshot_rx) = std::sync::mpsc::channel::<EngineSnapshot>();
        self.snapshot_rx = Some(sync_snapshot_rx);

        let (sync_result_tx, sync_result_rx) = std::sync::mpsc::channel::<Result<PathBuf, String>>();
        self.result_rx = Some(sync_result_rx);

        let connections = self.connections;
        let save_dir = PathBuf::from(&self.save_dir);

        let checksum_opt = self.checksum_input.trim().to_string();
        let cookies_opt = self.cookies_path_input.trim().to_string();
        let proxy_opt = self.proxy_input.trim().to_string();
        let auth_opt = self.auth_header_input.trim().to_string();
        let media_preset_idx = self.media_preset_idx;
        let browser_cookies_idx = self.browser_cookies_idx;

        let (async_snapshot_tx, mut async_snapshot_rx) = broadcast::channel::<EngineSnapshot>(128);

        // Bridge Tokio broadcast to standard mpsc channel for UI thread
        self.tokio_rt.spawn(async move {
            loop {
                match async_snapshot_rx.recv().await {
                    Ok(snapshot) => {
                        if sync_snapshot_tx.send(snapshot).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });

        // Spawn engine download task
        self.tokio_rt.spawn(async move {
            let media_preset = match media_preset_idx {
                0 => Some(hyperfetch_core::media::MediaQualityPreset::BestVideoAudio),
                1 => Some(hyperfetch_core::media::MediaQualityPreset::Fhd1080p),
                2 => Some(hyperfetch_core::media::MediaQualityPreset::Hd720p),
                3 => Some(hyperfetch_core::media::MediaQualityPreset::AudioMp3),
                4 => Some(hyperfetch_core::media::MediaQualityPreset::AudioM4a),
                _ => Some(hyperfetch_core::media::MediaQualityPreset::BestVideoAudio),
            };

            let browser_cookies = match browser_cookies_idx {
                1 => Some(hyperfetch_core::media::BrowserCookieSource::Chrome),
                2 => Some(hyperfetch_core::media::BrowserCookieSource::Edge),
                3 => Some(hyperfetch_core::media::BrowserCookieSource::Firefox),
                4 => Some(hyperfetch_core::media::BrowserCookieSource::Brave),
                5 => Some(hyperfetch_core::media::BrowserCookieSource::Opera),
                6 => Some(hyperfetch_core::media::BrowserCookieSource::Vivaldi),
                _ => None,
            };

            let options = DownloadOptions {
                num_connections: connections,
                base_chunk_size: 4 * 1024 * 1024,
                min_steal_threshold: 1024 * 1024,
                output_path: Some(save_dir),
                expected_checksum: if checksum_opt.is_empty() { None } else { Some(checksum_opt) },
                cookies_path: if cookies_opt.is_empty() { None } else { Some(PathBuf::from(cookies_opt)) },
                proxy: if proxy_opt.is_empty() { None } else { Some(proxy_opt) },
                auth_header: if auth_opt.is_empty() { None } else { Some(auth_opt) },
                media_preset,
                browser_cookies,
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
        self.status_message = "Download cancelled / paused".to_string();
        self.speed_bytes_per_sec = 0.0;
    }

    fn delete_leftovers(&mut self) {
        use hyperfetch_core::state::DownloadState;
        if let Some(ref path) = self.target_filepath {
            let _ = std::fs::remove_file(path);
            let state_file = DownloadState::state_file_path(path);
            let _ = std::fs::remove_file(state_file);
        } else {
            let trimmed = self.url_input.trim();
            for u in trimmed.split_whitespace() {
                if let Ok(url) = Url::parse(u) {
                    if let Some(filename) = url.path_segments().and_then(|s| s.last()) {
                        if !filename.is_empty() {
                            let path = PathBuf::from(&self.save_dir).join(filename);
                            let _ = std::fs::remove_file(&path);
                            let state_file = DownloadState::state_file_path(&path);
                            let _ = std::fs::remove_file(state_file);
                        }
                    }
                }
            }
        }
        self.reset_state();
        self.status_message = "Leftover files permanently deleted".to_string();
    }

    fn start_over(&mut self) {
        self.delete_leftovers();
        self.start_download();
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
                if self.target_filepath.is_none() {
                    self.target_filepath = snapshot.target_path;
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

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let is_queue = app.active_tab == GuiTab::BatchQueue;
            let queue_btn = egui::Button::new(
                egui::RichText::new(format!("Queue ({})", app.queue.items().len()))
                    .strong()
                    .color(if is_queue { Color32::WHITE } else { Color32::from_rgb(148, 163, 184) }),
            )
            .fill(if is_queue { Color32::from_rgb(37, 99, 235) } else { Color32::from_rgb(30, 32, 40) });
            if ui.add_sized([100.0, 24.0], queue_btn).clicked() {
                app.active_tab = GuiTab::BatchQueue;
            }

            let is_dl = app.active_tab == GuiTab::Downloader;
            let dl_btn = egui::Button::new(
                egui::RichText::new("Downloader")
                    .strong()
                    .color(if is_dl { Color32::WHITE } else { Color32::from_rgb(148, 163, 184) }),
            )
            .fill(if is_dl { Color32::from_rgb(37, 99, 235) } else { Color32::from_rgb(30, 32, 40) });
            if ui.add_sized([100.0, 24.0], dl_btn).clicked() {
                app.active_tab = GuiTab::Downloader;
            }
        });
    });

    ui.add_space(10.0);

    match app.active_tab {
        GuiTab::Downloader => render_downloader_tab(app, ui),
        GuiTab::BatchQueue => render_queue_tab(app, ui),
    }
}

fn render_downloader_tab(app: &mut DownloaderApp, ui: &mut egui::Ui) {
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
                        .hint_text("Paste media link or file URL (YouTube, Archive.org, Vimeo, Reddit, etc.)"),
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

            // Blob URL warning banner
            let is_blob = app.url_input.trim().starts_with("blob:")
                || (app.url_input.contains("youtube.com") && app.url_input.trim().split('/').last().map_or(false, |s| s.len() == 36 && s.matches('-').count() == 4));
            if is_blob {
                ui.add_space(6.0);
                egui::Frame::none()
                    .fill(Color32::from_rgb(45, 20, 20))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(220, 38, 38)))
                    .inner_margin(8.0)
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Notice: The URL entered is a browser-internal blob memory buffer. Web browsers generate these internally in RAM and they cannot be downloaded by external tools. Please copy the actual YouTube video link from your browser's address bar (e.g. https://www.youtube.com/watch?v=... or https://youtu.be/...).")
                                .color(Color32::from_rgb(254, 202, 202))
                                .size(12.0),
                        );
                    });
            }

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
                ui.add(egui::Slider::new(&mut app.connections, 1..=64).text("connections"));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match app.status {
                        DownloadStatus::Downloading | DownloadStatus::Resolving => {
                            if ui.button(egui::RichText::new("Pause / Cancel").color(Color32::from_rgb(239, 68, 68))).clicked() {
                                app.cancel_download();
                            }
                        }
                        DownloadStatus::Cancelled => {
                            if ui.button("Start Over").clicked() {
                                app.start_over();
                            }
                            if ui.button(egui::RichText::new("Delete Leftovers").color(Color32::from_rgb(239, 68, 68))).clicked() {
                                app.delete_leftovers();
                            }
                            let resume_btn = egui::Button::new(
                                egui::RichText::new("Resume Download")
                                    .strong()
                                    .color(Color32::from_rgb(255, 255, 255)),
                            )
                            .fill(Color32::from_rgb(16, 185, 129));
                            if ui.add_sized([130.0, 26.0], resume_btn).clicked() {
                                app.resume_download();
                            }
                        }
                        DownloadStatus::Failed(_) => {
                            if ui.button("Start Over").clicked() {
                                app.start_over();
                            }
                            if ui.button(egui::RichText::new("Delete Leftovers").color(Color32::from_rgb(239, 68, 68))).clicked() {
                                app.delete_leftovers();
                            }
                            let retry_btn = egui::Button::new(
                                egui::RichText::new("Retry / Resume")
                                    .strong()
                                    .color(Color32::from_rgb(255, 255, 255)),
                            )
                            .fill(Color32::from_rgb(16, 185, 129));
                            if ui.add_sized([120.0, 26.0], retry_btn).clicked() {
                                app.resume_download();
                            }
                        }
                        DownloadStatus::Completed => {
                            if let Some(ref path) = app.target_filepath {
                                if ui.button("Open File").clicked() {
                                    #[cfg(target_os = "windows")]
                                    let _ = std::process::Command::new("cmd").args(["/C", "start", "", &path.to_string_lossy()]).spawn();
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

            // Collapsible Advanced Options
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
            let adv_text = if app.show_advanced { "[-] Advanced Options (Checksum, Cookies, Proxy)" } else { "[+] Advanced Options (Checksum, Cookies, Proxy)" };
            if ui.button(egui::RichText::new(adv_text).size(12.0).color(Color32::from_rgb(148, 163, 184))).clicked() {
                app.show_advanced = !app.show_advanced;
            }

            if app.show_advanced {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Checksum:").size(12.0));
                    ui.add_sized(
                        [ui.available_width() - 10.0, 24.0],
                        egui::TextEdit::singleline(&mut app.checksum_input)
                            .hint_text("Optional: sha256:..., md5:..., blake3:..., or hex"),
                    );
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Cookies:").size(12.0));
                    ui.add_sized(
                        [ui.available_width() - 95.0, 24.0],
                        egui::TextEdit::singleline(&mut app.cookies_path_input)
                            .hint_text("Optional: path to Netscape cookies.txt"),
                    );
                    if ui.button("Browse...").clicked() {
                        if let Some(file) = rfd::FileDialog::new().add_filter("Text", &["txt"]).pick_file() {
                            app.cookies_path_input = file.to_string_lossy().to_string();
                        }
                    }
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Proxy:").size(12.0));
                    ui.add_sized(
                        [ui.available_width() - 10.0, 24.0],
                        egui::TextEdit::singleline(&mut app.proxy_input)
                            .hint_text("Optional: http://127.0.0.1:8080 or socks5://127.0.0.1:1080"),
                    );
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Auth:").size(12.0));
                    ui.add_sized(
                        [ui.available_width() - 10.0, 24.0],
                        egui::TextEdit::singleline(&mut app.auth_header_input)
                            .hint_text("Optional: Bearer <token>"),
                    );
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Media Quality:").size(12.0));
                    egui::ComboBox::from_id_salt("media_preset_combo")
                        .selected_text(match app.media_preset_idx {
                            0 => "Best Available (Merged MP4)",
                            1 => "1080p FHD (Merged MP4)",
                            2 => "720p HD (Merged MP4)",
                            3 => "Audio Only (MP3)",
                            4 => "Audio Only (M4A)",
                            _ => "Best Available",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut app.media_preset_idx, 0, "Best Available (Merged MP4)");
                            ui.selectable_value(&mut app.media_preset_idx, 1, "1080p FHD (Merged MP4)");
                            ui.selectable_value(&mut app.media_preset_idx, 2, "720p HD (Merged MP4)");
                            ui.selectable_value(&mut app.media_preset_idx, 3, "Audio Only (MP3)");
                            ui.selectable_value(&mut app.media_preset_idx, 4, "Audio Only (M4A)");
                        });
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Browser Cookies:").size(12.0));
                    egui::ComboBox::from_id_salt("browser_cookies_combo")
                        .selected_text(match app.browser_cookies_idx {
                            0 => "None",
                            1 => "Google Chrome",
                            2 => "Microsoft Edge",
                            3 => "Mozilla Firefox",
                            4 => "Brave Browser",
                            5 => "Opera",
                            6 => "Vivaldi",
                            _ => "None",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut app.browser_cookies_idx, 0, "None");
                            ui.selectable_value(&mut app.browser_cookies_idx, 1, "Google Chrome");
                            ui.selectable_value(&mut app.browser_cookies_idx, 2, "Microsoft Edge");
                            ui.selectable_value(&mut app.browser_cookies_idx, 3, "Mozilla Firefox");
                            ui.selectable_value(&mut app.browser_cookies_idx, 4, "Brave Browser");
                            ui.selectable_value(&mut app.browser_cookies_idx, 5, "Opera");
                            ui.selectable_value(&mut app.browser_cookies_idx, 6, "Vivaldi");
                        });
                });
            }
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
            let end_ratio = ((chunk.range_end + 1) as f32 / total_b).clamp(0.0, 1.0);
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

fn render_queue_tab(app: &mut DownloaderApp, ui: &mut egui::Ui) {
    egui::Frame::none()
        .fill(Color32::from_rgb(24, 26, 33))
        .stroke(Stroke::new(1.0, Color32::from_rgb(42, 45, 56)))
        .inner_margin(12.0)
        .rounding(6.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Add to Queue:").strong().size(13.0));
                ui.add_sized(
                    [ui.available_width() - 100.0, 26.0],
                    egui::TextEdit::singleline(&mut app.queue_url_input)
                        .hint_text("Enter file URL or media link to enqueue"),
                );
                if ui.button("Add Item").clicked() {
                    let trimmed = app.queue_url_input.trim();
                    if !trimmed.is_empty() {
                        if let Ok(u) = Url::parse(trimmed) {
                            let save_dir = PathBuf::from(&app.save_dir);
                            let options = DownloadOptions {
                                num_connections: app.connections,
                                base_chunk_size: 4 * 1024 * 1024,
                                min_steal_threshold: 1024 * 1024,
                                output_path: Some(save_dir.clone()),
                                ..Default::default()
                            };
                            app.queue.add_item(vec![u], save_dir, options);
                            app.queue_url_input.clear();
                        }
                    }
                }
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Clear Completed").clicked() {
                    app.queue.retain_items(|i| i.status != hyperfetch_core::queue::QueueItemStatus::Completed);
                }
                if ui.button("Clear All").clicked() {
                    app.queue.clear();
                }
            });
        });

    ui.add_space(10.0);
    ui.label(egui::RichText::new("BATCH DOWNLOAD QUEUE").strong().size(13.0));
    ui.add_space(4.0);

    egui::Frame::none()
        .fill(Color32::from_rgb(24, 26, 33))
        .stroke(Stroke::new(1.0, Color32::from_rgb(42, 45, 56)))
        .inner_margin(8.0)
        .rounding(6.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(350.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if app.queue.items().is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(30.0);
                            ui.label(
                                egui::RichText::new("Queue is empty. Add URLs above to build a batch queue.")
                                    .color(Color32::from_rgb(113, 113, 122)),
                            );
                            ui.add_space(30.0);
                        });
                    } else {
                        ui.horizontal(|ui| {
                            ui.add_sized([40.0, 20.0], egui::Label::new(egui::RichText::new("ID").strong().size(11.0)));
                            ui.add_sized([220.0, 20.0], egui::Label::new(egui::RichText::new("File").strong().size(11.0)));
                            ui.add_sized([100.0, 20.0], egui::Label::new(egui::RichText::new("Status").strong().size(11.0)));
                            ui.add_sized([120.0, 20.0], egui::Label::new(egui::RichText::new("Actions").strong().size(11.0)));
                        });
                        ui.separator();

                        let mut to_remove = None;
                        let mut to_load = None;
                        for item in app.queue.items() {
                            ui.horizontal(|ui| {
                                ui.add_sized([40.0, 18.0], egui::Label::new(format!("#{}", item.id)));
                                ui.add_sized([220.0, 18.0], egui::Label::new(&item.filename));
                                let status_str = format!("{:?}", item.status);
                                ui.add_sized([100.0, 18.0], egui::Label::new(status_str));
                                if ui.small_button("Download").clicked() {
                                    to_load = Some(item.urls.clone());
                                }
                                if ui.small_button("Remove").clicked() {
                                    to_remove = Some(item.id);
                                }
                            });
                        }

                        if let Some(id) = to_remove {
                            app.queue.remove_item(id);
                        }
                        if let Some(urls) = to_load {
                            app.url_input = urls.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(" ");
                            app.active_tab = GuiTab::Downloader;
                            app.start_download();
                        }
                    }
                });
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
