#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod queue_store;
mod settings;
mod ui;
mod util;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::Color32;
use hyperfetch_core::chunk::ChunkSnapshot;
use hyperfetch_core::engine::{DownloadEngine, DownloadOptions, EngineSnapshot};
use hyperfetch_core::history::{DownloadHistoryManager, HistoryEntry};
use hyperfetch_core::queue::{DownloadQueue, QueueItem};
use hyperfetch_core::verify::{self, BuildVerificationResult};
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;
use url::Url;

use settings::Settings;
use util::{lock, Verdict};

/// How long closing the window waits for downloads to save their resume state.
const EXIT_GRACE: Duration = Duration::from_secs(3);
/// Span of the throughput graph.
const GRAPH_WINDOW: Duration = Duration::from_secs(60);
/// No new bytes for this long shows the stalled indicator.
const STALL_HINT: Duration = Duration::from_secs(5);
/// How often the clipboard is checked for links.
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Downloader,
    Queue,
    History,
}

#[derive(Debug, Clone, Copy)]
enum Dialog {
    SaveDir,
    CookiesFile,
    VerifyFile,
}

/// Results of background work, delivered to the UI thread (each send also requests a repaint).
enum AppEvent {
    JobFinished { id: usize, result: Result<(PathBuf, Option<u64>), String> },
    /// The history as read by the `generation`-th history operation to run.
    History(Result<(u64, Vec<HistoryEntry>), String>),
    /// Leftovers of a stopped download were deleted (the count), or kept because of the error.
    Discarded { id: usize, name: String, result: Result<usize, String> },
    Verified(Result<(BuildVerificationResult, Vec<Url>), String>),
    RepairFinished(Result<(), String>),
    Picked(Dialog, Option<PathBuf>),
    Pasted(Result<String, String>),
    ClipboardLink(String),
}

/// Handles to a running engine task.
struct Running {
    /// Asks the task to call `engine.cancel()`; the task keeps awaiting `run()` so the engine
    /// saves its resume state.
    cancel: Arc<Notify>,
    task: JoinHandle<()>,
    /// Latest snapshot only; older ones are overwritten.
    snapshot: Arc<Mutex<Option<EngineSnapshot>>>,
}

/// Live telemetry of one download, kept by the GUI next to its queue item.
#[derive(Default)]
struct JobView {
    running: Option<Running>,
    chunks: Vec<ChunkSnapshot>,
    mirror_speeds: Vec<(usize, String, f64)>,
    active_workers: usize,
    speed_history: VecDeque<(Instant, f64)>,
    started: Option<Instant>,
    /// Duration of the last finished run.
    elapsed: Duration,
    last_progress: Option<Instant>,
    got_snapshot: bool,
}

impl JobView {
    fn elapsed(&self) -> Duration {
        match (&self.running, self.started) {
            (Some(_), Some(started)) => started.elapsed(),
            _ => self.elapsed,
        }
    }

    /// How long no new bytes have arrived, once that counts as stalled.
    fn stalled_for(&self) -> Option<Duration> {
        let since = self.last_progress?.elapsed();
        (self.running.is_some() && self.got_snapshot && since >= STALL_HINT).then_some(since)
    }
}

#[derive(Clone)]
struct VerifyRequest {
    path: PathBuf,
    expected_size: Option<u64>,
    checksum: Option<String>,
}

struct Verification {
    result: BuildVerificationResult,
    /// Mirrors recorded for exactly this file (only looked up when it is incomplete).
    repair_urls: Vec<Url>,
}

struct Repair {
    /// Final path of the file being repaired.
    target: PathBuf,
    cancel: Arc<AtomicBool>,
    task: JoinHandle<()>,
    progress: Arc<Mutex<(u64, u64)>>,
}

struct App {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    events_tx: mpsc::Sender<AppEvent>,
    events_rx: mpsc::Receiver<AppEvent>,

    settings: Settings,
    tab: Tab,
    url_input: String,
    /// Per-download inputs; never saved.
    checksum_input: String,
    auth_input: String,
    queue_input: String,
    show_advanced: bool,
    form_error: Option<String>,
    queue_error: Option<String>,
    notice: Option<Result<String, String>>,

    clipboard_enabled: Arc<AtomicBool>,
    /// Last clipboard text the watcher saw or the app copied itself; never offered again.
    clipboard_seen: Arc<Mutex<String>>,
    clipboard_banner: Option<String>,

    queue: DownloadQueue,
    /// Saves the queue in the background; `None` if its thread could not start.
    queue_saver: Option<queue_store::Saver>,
    /// Revision of the queue last handed to the saver.
    queue_saved: u64,
    jobs: HashMap<usize, JobView>,
    /// The download shown on the Downloader tab.
    focused: Option<usize>,

    anim_job: Option<usize>,
    anim_progress: f64,
    anim_speed: f64,
    last_frame: Instant,
    pulse_phase: f32,

    history: Vec<HistoryEntry>,
    /// Serializes history operations and counts them in the order they run.
    history_sequence: Arc<Mutex<u64>>,
    history_applied: u64,
    history_search: String,
    verify_request: Option<VerifyRequest>,
    verifying: bool,
    verification: Option<Verification>,
    verify_message: Option<String>,
    repair: Option<Repair>,

    dialog_open: bool,
    pending_dialog: Option<Dialog>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Handle) -> Self {
        apply_theme(&cc.egui_ctx);
        let settings = Settings::load();
        let (queue, queue_problem) = queue_store::load(&queue_store::path());
        let queue_saver = queue_store::Saver::spawn(queue_store::path())
            .inspect_err(|e| tracing::warn!("Queue changes will only be saved on exit: {}", e))
            .ok();
        let (events_tx, events_rx) = mpsc::channel();
        let clipboard_enabled = Arc::new(AtomicBool::new(settings.clipboard_watch));
        let clipboard_seen = Arc::new(Mutex::new(String::new()));
        spawn_clipboard_watcher(
            Arc::clone(&clipboard_enabled),
            Arc::clone(&clipboard_seen),
            events_tx.clone(),
            cc.egui_ctx.clone(),
        );

        let mut app = Self {
            rt,
            ctx: cc.egui_ctx.clone(),
            events_tx,
            events_rx,
            settings,
            tab: Tab::Downloader,
            url_input: String::new(),
            checksum_input: String::new(),
            auth_input: String::new(),
            queue_input: String::new(),
            show_advanced: false,
            form_error: None,
            queue_error: None,
            notice: queue_problem.map(Err),
            clipboard_enabled,
            clipboard_seen,
            clipboard_banner: None,
            queue_saved: queue.revision(),
            queue,
            queue_saver,
            jobs: HashMap::new(),
            focused: None,
            anim_job: None,
            anim_progress: 0.0,
            anim_speed: 0.0,
            last_frame: Instant::now(),
            pulse_phase: 0.0,
            history: Vec::new(),
            history_sequence: Arc::new(Mutex::new(0)),
            history_applied: 0,
            history_search: String::new(),
            verify_request: None,
            verifying: false,
            verification: None,
            verify_message: None,
            repair: None,
            dialog_open: false,
            pending_dialog: None,
        };
        app.refresh_history();
        app
    }

    /// Runs `fut` on the runtime and delivers its event to the UI thread.
    fn spawn_event(&self, fut: impl Future<Output = AppEvent> + Send + 'static) -> JoinHandle<()> {
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            if tx.send(fut.await).is_ok() {
                ctx.request_repaint();
            }
        })
    }

    fn focused_item(&self) -> Option<&QueueItem> {
        self.focused.and_then(|id| self.queue.get_item(id))
    }

    fn is_resolving(&self, id: usize) -> bool {
        self.jobs.get(&id).is_none_or(|view| !view.got_snapshot)
    }

    /// Once every byte has arrived the engine verifies and renames the file without progress
    /// reports; that is not a stall.
    fn stalled_for(&self, id: usize) -> Option<Duration> {
        if self.queue.get_item(id).is_some_and(QueueItem::is_finishing) {
            return None;
        }
        self.jobs.get(&id).and_then(JobView::stalled_for)
    }

    /// The download's file is being repaired, so it must not be started or cleaned up.
    fn under_repair(&self, id: usize) -> bool {
        self.repair.as_ref().is_some_and(|repair| self.queue.get_item(id).is_some_and(|item| item.targets(&repair.target)))
    }

    // ---- history -------------------------------------------------------------------------

    /// Applies `op` to the on-disk history off the UI thread (the core reloads the file under its
    /// lock, so entries written meanwhile by the engine or the CLI are kept) and shows the result.
    fn update_history(&mut self, op: impl FnOnce(&mut DownloadHistoryManager) + Send + 'static) {
        let sequence = Arc::clone(&self.history_sequence);
        self.spawn_event(async move {
            let result = unblock(move || {
                in_sequence(&sequence, || {
                    let mut manager = DownloadHistoryManager::load();
                    op(&mut manager);
                    manager.entries().to_vec()
                })
            })
            .await;
            AppEvent::History(result)
        });
    }

    fn refresh_history(&mut self) {
        self.update_history(|_| {});
    }

    // ---- downloads -----------------------------------------------------------------------

    /// Adds the download described by `text` (mirrors of one file) with the current settings.
    fn add_download(&mut self, text: &str, checksum: &str, auth: &str) -> Result<usize, String> {
        let urls = util::parse_urls(text)?;
        let options = self.settings.download_options(&urls, checksum, auth)?;
        Ok(self.queue.add_item(urls, options))
    }

    /// Adds a download, starts it immediately and shows it on the Downloader tab. A file that is
    /// already downloading is shown instead of being started twice.
    fn download_now(&mut self, text: &str, checksum: &str, auth: &str) {
        self.tab = Tab::Downloader;
        let id = match self.add_download(text, checksum, auth) {
            Ok(id) => id,
            Err(e) => {
                self.form_error = Some(e);
                return;
            }
        };
        self.form_error = None;
        self.notice = None;
        if let Some(existing) = self.queue.active_conflict(id) {
            self.queue.remove_item(id);
            self.focused = Some(existing);
            self.notice = Some(Err("That file is already downloading; showing it instead".to_string()));
            return;
        }
        self.focused = Some(id);
        self.url_input.clear();
        self.checksum_input.clear();
        self.start_job(id, false);
    }

    /// Adds each non-empty line of the queue input as one download.
    fn add_queue_input(&mut self) {
        let lines: Vec<String> =
            self.queue_input.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
        if lines.is_empty() {
            self.queue_error = Some("Enter one download per line".to_string());
            return;
        }
        let checksum = self.checksum_input.trim().to_string();
        if lines.len() > 1 && !checksum.is_empty() {
            self.queue_error = Some(
                "The checksum in Advanced Options is for a single file: add that download on its own".to_string(),
            );
            return;
        }
        let auth = self.auth_input.clone();
        let mut errors = Vec::new();
        let mut rejected = Vec::new();
        for (n, line) in lines.iter().enumerate() {
            if let Err(e) = self.add_download(line, &checksum, &auth) {
                errors.push(format!("Line {}: {}", n + 1, e));
                rejected.push(line.as_str());
            }
        }
        // Keep the lines that failed so they can be corrected.
        self.queue_input = rejected.join("\n");
        self.queue_error = (!errors.is_empty()).then(|| errors.join("\n"));
        if errors.len() < lines.len() {
            self.checksum_input.clear();
        }
    }

    /// Starts (or resumes) the engine for a queue item. `fresh` first deletes the item's partial
    /// files so the download starts from zero. Returns false if nothing was started.
    fn start_job(&mut self, id: usize, fresh: bool) -> bool {
        if let Some(other) = self.queue.active_conflict(id) {
            self.notice = Some(Err(format!(
                "Download #{} is already fetching the same file; pause it before starting #{}",
                other, id
            )));
            return false;
        }
        if self.under_repair(id) {
            self.notice = Some(Err(format!("The file of download #{} is being repaired; wait for the repair to finish", id)));
            return false;
        }
        let Some(item) = self.queue.get_item(id) else { return false };
        let (urls, options) = (item.urls.clone(), item.options.clone());
        let leftovers_of = if fresh { item.target_path.clone() } else { None };
        if !self.queue.mark_started(id) {
            return false;
        }
        if fresh {
            self.queue.reset_progress(id);
        }

        let cancel = Arc::new(Notify::new());
        let snapshot = Arc::new(Mutex::new(None));
        let task = {
            let (cancel, snapshot, ctx, tx) =
                (Arc::clone(&cancel), Arc::clone(&snapshot), self.ctx.clone(), self.events_tx.clone());
            self.rt.spawn(async move {
                let result = run_job(urls, options, leftovers_of, cancel, snapshot, ctx.clone()).await;
                if tx.send(AppEvent::JobFinished { id, result }).is_ok() {
                    ctx.request_repaint();
                }
            })
        };
        self.jobs.insert(
            id,
            JobView {
                running: Some(Running { cancel, task, snapshot }),
                started: Some(Instant::now()),
                ..JobView::default()
            },
        );
        true
    }

    /// Resumes a restored download with the Authorization header from the form, which is never saved.
    fn resume_with_auth(&mut self, id: usize) {
        let auth = self.auth_input.trim().to_string();
        if !auth.is_empty() && self.queue.provide_auth(id, auth) {
            self.start_job(id, false);
        }
    }

    /// Asks a running download to stop; it stays "Pausing" until the engine has saved its state.
    fn pause_job(&mut self, id: usize) {
        let Some(running) = self.jobs.get(&id).and_then(|view| view.running.as_ref()) else { return };
        if self.queue.mark_pausing(id) {
            running.cancel.notify_one();
        }
    }

    /// Deletes the partial file and resume state of a stopped download, then removes it from the
    /// list. Nothing is deleted while any download (in this app or another) is using the file.
    fn discard_job(&mut self, id: usize) {
        if let Some(other) = self.queue.active_conflict(id) {
            self.notice = Some(Err(format!("Download #{} is using the same partial file right now", other)));
            return;
        }
        if self.under_repair(id) {
            self.notice = Some(Err(format!("The file of download #{} is being repaired right now", id)));
            return;
        }
        let Some(item) = self.queue.get_item(id).filter(|item| !item.status.is_active()) else { return };
        match item.target_path.clone() {
            Some(final_path) => {
                let name = item.filename.clone();
                self.spawn_event(async move {
                    AppEvent::Discarded { id, name, result: discard_leftovers(final_path).await }
                });
            }
            None => {
                if self.remove_job(id) {
                    self.notice = Some(Ok("Removed the download; no partial file was recorded for it".to_string()));
                }
            }
        }
    }

    /// Removes a stopped download from the list, keeping its files. Running ones are kept.
    fn remove_job(&mut self, id: usize) -> bool {
        if !self.queue.remove_item(id) {
            return false;
        }
        self.jobs.remove(&id);
        if self.focused == Some(id) {
            self.new_download();
        }
        true
    }

    /// Clears completed downloads, or every download that is not running.
    fn clear_queue(&mut self, completed_only: bool) {
        if completed_only {
            self.queue.clear_completed();
        } else {
            self.queue.clear_inactive();
        }
        let queue = &self.queue;
        self.jobs.retain(|id, _| queue.get_item(*id).is_some());
        if self.focused.is_some_and(|id| self.queue.get_item(id).is_none()) {
            self.new_download();
        }
    }

    /// Leaves the shown download running in the queue and clears the form for a new one.
    fn new_download(&mut self) {
        self.focused = None;
        self.url_input.clear();
        self.checksum_input.clear();
        self.form_error = None;
        self.notice = None;
    }

    fn show_job(&mut self, id: usize) {
        self.focused = Some(id);
        self.form_error = None;
        self.notice = None;
        self.tab = Tab::Downloader;
    }

    fn drain_snapshots(&mut self) {
        let now = Instant::now();
        for (&id, view) in &mut self.jobs {
            let Some(snapshot) = view.running.as_ref().and_then(|r| lock(&r.snapshot).take()) else { continue };
            let previous = self.queue.get_item(id).map_or(0, |item| item.downloaded_bytes);
            if !view.got_snapshot || snapshot.downloaded_bytes > previous {
                view.last_progress = Some(now);
            }
            view.got_snapshot = true;
            view.active_workers = snapshot.active_workers;
            view.speed_history.push_back((now, snapshot.speed_bytes_per_sec));
            while view.speed_history.front().is_some_and(|(t, _)| now.duration_since(*t) > GRAPH_WINDOW) {
                view.speed_history.pop_front();
            }
            self.queue.apply_snapshot(id, &snapshot);
            view.mirror_speeds = snapshot.mirror_speeds;
            if !snapshot.chunks.is_empty() {
                view.chunks = snapshot.chunks;
            }
        }
    }

    /// Starts queued downloads while fewer than the configured number are running.
    fn run_scheduler(&mut self) {
        if !self.settings.auto_run_queue {
            return;
        }
        while let Some(id) = self.queue.next_to_start(self.settings.max_concurrent.max(1)) {
            if !self.start_job(id, false) {
                break;
            }
        }
    }

    // ---- verify & repair -----------------------------------------------------------------

    fn verify(&mut self, request: VerifyRequest) {
        if self.verifying || self.repair.is_some() {
            return;
        }
        self.verifying = true;
        self.verification = None;
        self.verify_message = None;
        self.tab = Tab::History;
        self.verify_request = Some(request.clone());
        self.spawn_event(async move {
            let result = unblock(move || {
                let result =
                    verify::verify_build_file(&request.path, request.expected_size, request.checksum.as_deref())?;
                let repair_urls = if util::verdict(&result) == Verdict::Incomplete {
                    util::repair_urls_for(&result.file_path)
                } else {
                    Vec::new()
                };
                Ok((result, repair_urls))
            })
            .await
            .and_then(|r| r);
            AppEvent::Verified(result)
        });
    }

    fn start_repair(&mut self) {
        let Some(verification) = &self.verification else { return };
        if self.repair.is_some() {
            return;
        }
        let result = &verification.result;
        let Some(total_size) = result.expected_size else {
            self.verify_message = Some("Cannot repair: the expected file size is unknown".to_string());
            return;
        };
        if verification.repair_urls.is_empty() {
            self.verify_message =
                Some("Cannot repair: no download URLs are recorded for exactly this file".to_string());
            return;
        }
        let target = util::final_path_of(&result.file_path);
        if let Some(id) = self.queue.active_on_target(&target) {
            self.verify_message =
                Some(format!("Cannot repair: download #{} is fetching this file right now; pause it first", id));
            return;
        }
        let (path, missing, urls) =
            (result.file_path.clone(), result.missing_ranges.clone(), verification.repair_urls.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new((0, missing.iter().map(|r| r.len()).sum())));
        let task = {
            let (cancel, progress, ctx) = (Arc::clone(&cancel), Arc::clone(&progress), self.ctx.clone());
            self.spawn_event(async move {
                let on_progress = move |done, total| {
                    *lock(&progress) = (done, total);
                    ctx.request_repaint();
                };
                let result =
                    verify::repair_missing_ranges(&path, total_size, &missing, &urls, Some(cancel), on_progress).await;
                AppEvent::RepairFinished(result)
            })
        };
        self.repair = Some(Repair { target, cancel, task, progress });
        self.verify_message = None;
    }

    // ---- events --------------------------------------------------------------------------

    fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::JobFinished { id, result } => {
                if let Some(view) = self.jobs.get_mut(&id) {
                    if let (Some(_), Some(started)) = (view.running.take(), view.started) {
                        view.elapsed = started.elapsed();
                    }
                    // The last snapshot predates the final bytes.
                    if result.is_ok() {
                        for chunk in &mut view.chunks {
                            chunk.downloaded_bytes = chunk.total_bytes;
                            chunk.status = "Completed".to_string();
                        }
                    }
                }
                self.queue.finish(id, result);
                // The engine records completed downloads in the history.
                self.refresh_history();
            }
            AppEvent::History(result) => match result {
                Ok((generation, entries)) if generation > self.history_applied => {
                    self.history_applied = generation;
                    self.history = entries;
                }
                Ok(_) => {}
                Err(e) => self.notice = Some(Err(format!("Could not read the download history: {}", e))),
            },
            AppEvent::Discarded { id, name, result } => match result {
                Ok(removed) => {
                    self.remove_job(id);
                    self.notice = Some(Ok(match removed {
                        0 => format!("Removed the download; no leftover files of {} were found", name),
                        n => format!("Removed the download and deleted {} leftover file(s) of {}", n, name),
                    }));
                }
                Err(e) => self.notice = Some(Err(format!("Kept the leftovers of {}: {}", name, e))),
            },
            AppEvent::Verified(result) => {
                self.verifying = false;
                match result {
                    Ok((result, repair_urls)) => self.verification = Some(Verification { result, repair_urls }),
                    Err(e) => self.verify_message = Some(format!("Could not verify: {}", e)),
                }
            }
            AppEvent::RepairFinished(result) => {
                self.repair = None;
                match result {
                    Ok(()) => {
                        // Show the file's state after the repair; a repaired `.part` has its final name now.
                        if let Some(request) = self.verify_request.clone() {
                            self.verify(VerifyRequest { path: util::final_path_of(&request.path), ..request });
                        }
                        self.verify_message = Some("Repair finished.".to_string());
                    }
                    Err(e) if e == "Repair cancelled by user" => {
                        self.verify_message = Some("Repair cancelled; the bytes fetched so far are kept".to_string());
                    }
                    Err(e) => self.verify_message = Some(format!("Repair failed: {}", e)),
                }
            }
            AppEvent::Picked(dialog, path) => {
                self.dialog_open = false;
                let Some(path) = path else { return };
                match dialog {
                    Dialog::SaveDir => self.settings.save_dir = path.to_string_lossy().into_owned(),
                    Dialog::CookiesFile => self.settings.cookies_path = path.to_string_lossy().into_owned(),
                    Dialog::VerifyFile => self.verify(VerifyRequest { path, expected_size: None, checksum: None }),
                }
            }
            AppEvent::Pasted(result) => match result {
                Ok(text) => self.url_input = text.trim().to_string(),
                Err(e) => self.form_error = Some(format!("Could not read the clipboard: {}", e)),
            },
            AppEvent::ClipboardLink(link) => {
                if self.clipboard_enabled.load(Ordering::Relaxed) && self.url_input.trim() != link {
                    self.clipboard_banner = Some(link);
                }
            }        }
    }

    fn open_dialog(&mut self, dialog: Dialog, frame: &eframe::Frame) {
        let picker = rfd::AsyncFileDialog::new().set_parent(frame);
        self.dialog_open = true;
        let picked = move |handle: Option<rfd::FileHandle>| AppEvent::Picked(dialog, handle.map(|h| h.path().to_path_buf()));
        match dialog {
            Dialog::SaveDir => {
                let pick = picker.set_directory(&self.settings.save_dir).pick_folder();
                self.spawn_event(async move { picked(pick.await) });
            }
            Dialog::CookiesFile => {
                let pick = picker.add_filter("Netscape cookies", &["txt"]).pick_file();
                self.spawn_event(async move { picked(pick.await) });
            }
            Dialog::VerifyFile => {
                let pick = picker.set_directory(&self.settings.save_dir).pick_file();
                self.spawn_event(async move { picked(pick.await) });
            }
        }
    }

    fn paste_url(&mut self) {
        self.spawn_event(async {
            let text = unblock(|| arboard::Clipboard::new().and_then(|mut c| c.get_text()).map_err(|e| e.to_string()))
                .await
                .and_then(|r| r);
            AppEvent::Pasted(text)
        });
    }

    /// Copies text to the clipboard without the watcher offering it back as a download.
    fn copy_text(&mut self, text: String) {
        *lock(&self.clipboard_seen) = text.clone();
        self.ctx.copy_text(text);
    }

    /// Eases the displayed progress and speed of the shown download; true while still moving.
    fn animate(&mut self) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f64().clamp(0.001, 0.1);
        self.last_frame = now;
        self.pulse_phase = (self.pulse_phase + dt as f32 * 3.5) % std::f32::consts::TAU;

        let (progress, speed) = self
            .focused_item()
            .map_or((0.0, 0.0), |item| (item.progress_ratio, item.speed_bytes_per_sec));
        if self.anim_job != self.focused {
            self.anim_job = self.focused;
            self.anim_progress = progress;
            self.anim_speed = speed;
        }
        self.anim_speed += (speed - self.anim_speed) * (dt * 8.0).min(1.0);
        self.anim_progress += (progress - self.anim_progress) * (dt * 10.0).min(1.0);
        let speed_settled = (speed - self.anim_speed).abs() < 1.0;
        let progress_settled = (progress - self.anim_progress).abs() < 1e-4;
        if speed_settled {
            self.anim_speed = speed;
        }
        if progress_settled {
            self.anim_progress = progress;
        }
        !(speed_settled && progress_settled)
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        while let Ok(event) = self.events_rx.try_recv() {
            self.handle_event(event);
        }
        self.drain_snapshots();
        self.run_scheduler();
        let animating = self.animate();

        egui::CentralPanel::default().show(ctx, |ui| ui::render(self, ui));

        if let Some(dialog) = self.pending_dialog.take() {
            self.open_dialog(dialog, frame);
        }
        if self.queue.revision() != self.queue_saved {
            self.queue_saved = self.queue.revision();
            if let Some(saver) = &self.queue_saver {
                saver.save(self.queue.clone());
            }
        }
        // Background work wakes the UI when it has news; otherwise only animations and the
        // once-a-second clocks (elapsed time, stall timer) need frames.
        if animating {
            ctx.request_repaint();
        } else if self.queue.active_count() > 0 || self.verifying || self.repair.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    /// Stops every download so it saves its resume state (and kills yt-dlp), waiting briefly,
    /// then saves the queue.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.settings.clipboard_watch = self.clipboard_enabled.load(Ordering::Relaxed);
        if let Err(e) = self.settings.save() {
            tracing::warn!("Failed to save settings: {}", e);
        }
        let mut tasks = Vec::new();
        for (&id, view) in &mut self.jobs {
            if let Some(running) = view.running.take() {
                self.queue.mark_pausing(id);
                running.cancel.notify_one();
                tasks.push(running.task);
            }
        }
        if let Some(repair) = self.repair.take() {
            repair.cancel.store(true, Ordering::Relaxed);
            tasks.push(repair.task);
        }
        if !wait_for_tasks(&self.rt, tasks, EXIT_GRACE) {
            tracing::warn!("Some downloads did not stop within {}s", EXIT_GRACE.as_secs());
        }
        // Record how the stopped downloads ended; any still stopping are saved as paused.
        while let Ok(event) = self.events_rx.try_recv() {
            if let AppEvent::JobFinished { id, result } = event {
                self.queue.finish(id, result);
            }
        }
        let queue = self.queue.clone();
        match self.queue_saver.take() {
            Some(saver) => saver.finish(queue),
            None => {
                if let Err(e) = queue_store::save(&queue_store::path(), queue) {
                    tracing::warn!("Failed to save the queue: {}", e);
                }
            }
        }
    }
}

async fn unblock<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| format!("Background task failed: {}", e))
}

/// Waits up to `grace` for `tasks` from a thread outside the runtime (such as the UI thread).
/// True if all of them finished.
fn wait_for_tasks(rt: &tokio::runtime::Handle, tasks: Vec<JoinHandle<()>>, grace: Duration) -> bool {
    // The timer must be created inside the runtime: outside it, tokio panics.
    rt.block_on(async move {
        let wait_all = async {
            for task in tasks {
                let _ = task.await;
            }
        };
        tokio::time::timeout(grace, wait_all).await.is_ok()
    })
}

/// Runs a history operation after every earlier one has finished and numbers it in that order,
/// so the latest number always belongs to the latest read of the file.
fn in_sequence<T>(sequence: &Mutex<u64>, op: impl FnOnce() -> T) -> (u64, T) {
    let mut generation = lock(sequence);
    *generation += 1;
    (*generation, op())
}

/// Deletes the partial files of `final_path` unless a download (in any process) holds it.
async fn discard_leftovers(final_path: PathBuf) -> Result<usize, String> {
    unblock(move || hyperfetch_core::discard_partial(&final_path)).await?
}

/// One engine run. A cancel request calls `engine.cancel()` and keeps awaiting `run()`, so the
/// engine flushes data, saves its resume state and stops yt-dlp before this returns.
async fn run_job(
    urls: Vec<Url>,
    options: DownloadOptions,
    leftovers_of: Option<PathBuf>,
    cancel: Arc<Notify>,
    slot: Arc<Mutex<Option<EngineSnapshot>>>,
    ctx: egui::Context,
) -> Result<(PathBuf, Option<u64>), String> {
    if let Some(final_path) = leftovers_of {
        discard_leftovers(final_path).await.map_err(|e| format!("Could not start over: {}", e))?;
    }
    // The engine treats a missing output directory as a file name.
    if let Some(dir) = options.output_path.clone() {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("Cannot create the download folder {}: {}", dir.display(), e))?;
    }
    // Building the engine reads the cookies file and TLS roots.
    let engine = unblock(move || DownloadEngine::new(urls, options)).await?;

    let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(16);
    let forward = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(snapshot) => {
                    *lock(&slot) = Some(snapshot);
                    ctx.request_repaint();
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let run = engine.run(Some(tx));
    tokio::pin!(run);
    let finished = tokio::select! {
        result = &mut run => Some(result),
        _ = cancel.notified() => None,
    };
    let result = match finished {
        Some(result) => result,
        None => {
            engine.cancel();
            run.await
        }
    };
    forward.abort();

    let path = result?;
    let size = tokio::fs::metadata(&path).await.ok().map(|m| m.len());
    Ok((path, size))
}

#[cfg(windows)]
fn clipboard_sequence() -> Option<u32> {
    // SAFETY: takes no arguments and only reads a counter.
    let seq = unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() };
    (seq != 0).then_some(seq)
}

#[cfg(not(windows))]
fn clipboard_sequence() -> Option<u32> {
    None
}

/// Watches the clipboard on its own thread (one `Clipboard` for its lifetime) and reports new links.
fn spawn_clipboard_watcher(
    enabled: Arc<AtomicBool>,
    seen: Arc<Mutex<String>>,
    tx: mpsc::Sender<AppEvent>,
    ctx: egui::Context,
) {
    let spawned = std::thread::Builder::new().name("clipboard-watcher".to_string()).spawn(move || {
        let mut clipboard: Option<arboard::Clipboard> = None;
        let mut last_sequence = None;
        loop {
            std::thread::sleep(CLIPBOARD_POLL);
            if !enabled.load(Ordering::Relaxed) {
                continue;
            }
            // On Windows only open the clipboard when its contents changed.
            let sequence = clipboard_sequence();
            if sequence.is_some() && sequence == last_sequence {
                continue;
            }
            if clipboard.is_none() {
                clipboard = arboard::Clipboard::new().ok();
            }
            let Some(board) = clipboard.as_mut() else { continue };
            let text = match board.get_text() {
                Ok(text) => text,
                // Held by another program: try again next time.
                Err(arboard::Error::ClipboardOccupied) => continue,
                Err(_) => {
                    last_sequence = sequence;
                    continue;
                }
            };
            last_sequence = sequence;
            let text = text.trim();
            {
                let mut seen = lock(&seen);
                if *seen == text {
                    continue;
                }
                text.clone_into(&mut seen);
            }
            if let Some(link) = util::clipboard_link(text) {
                if tx.send(AppEvent::ClipboardLink(link)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("Clipboard watcher unavailable: {}", e);
    }
}

fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals.dark_mode = true;
    style.visuals.override_text_color = Some(Color32::from_rgb(228, 232, 240));
    style.visuals.window_fill = Color32::from_rgb(18, 20, 24);
    style.visuals.panel_fill = Color32::from_rgb(18, 20, 24);
    style.visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(26, 28, 35);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(32, 35, 45);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(45, 50, 65);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(30, 64, 175);
    ctx.set_style(style);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt().try_init();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let handle = runtime.handle().clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 720.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("Endo's Unified Downloader"),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Endo's Unified Downloader",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, handle)))),
    );
    // Downloads were stopped in `on_exit`; leftover blocking work (e.g. hashing for a verify)
    // must not keep the process alive.
    runtime.shutdown_background();
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperfetch_core::state::DownloadState;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal HTTP/1.1 file server with range support, sending about 800 KB/s per connection.
    async fn serve(body: Arc<Vec<u8>>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body = Arc::clone(&body);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    let last = body.len() - 1;
                    let range = request.lines().find_map(|l| l.strip_prefix("range: bytes=")).map(|r| {
                        let (a, b) = r.trim().split_once('-').unwrap();
                        (a.parse::<usize>().unwrap(), b.parse::<usize>().map_or(last, |b| b.min(last)))
                    });
                    let (start, end) = range.unwrap_or((0, last));
                    let status = match range {
                        Some(_) => format!("206 Partial Content\r\nContent-Range: bytes {}-{}/{}", start, end, body.len()),
                        None => "200 OK".to_string(),
                    };
                    let head = format!(
                        "HTTP/1.1 {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        status,
                        end + 1 - start
                    );
                    if socket.write_all(head.as_bytes()).await.is_err() || request.starts_with("head") {
                        return;
                    }
                    for piece in body[start..=end].chunks(8 * 1024) {
                        if socket.write_all(piece).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                });
            }
        });
        addr
    }

    async fn run(
        url: &Url,
        options: &DownloadOptions,
        leftovers_of: Option<PathBuf>,
        pause_after: Option<Duration>,
    ) -> Result<(PathBuf, Option<u64>), String> {
        let cancel = Arc::new(Notify::new());
        let slot = Arc::new(Mutex::new(None));
        let job = tokio::spawn(run_job(
            vec![url.clone()],
            options.clone(),
            leftovers_of,
            Arc::clone(&cancel),
            Arc::clone(&slot),
            egui::Context::default(),
        ));
        if let Some(delay) = pause_after {
            tokio::time::sleep(delay).await;
            assert!(lock(&slot).is_some(), "snapshots reach the UI slot");
            cancel.notify_one();
        }
        tokio::time::timeout(Duration::from_secs(60), job).await.unwrap().unwrap()
    }

    /// The `.part.hfstate` and `.part` of a download of `final_path`.
    fn part_files(final_path: &std::path::Path) -> (PathBuf, PathBuf) {
        let mut part = final_path.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        (DownloadState::state_file_path(&part), part)
    }

    /// Closing the window runs on the UI thread, which is not part of the runtime.
    #[test]
    fn exit_waits_for_tasks_from_outside_the_runtime() {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let rt = runtime.handle();
        let quick = rt.spawn(async { tokio::time::sleep(Duration::from_millis(50)).await });
        assert!(wait_for_tasks(rt, vec![quick], Duration::from_secs(10)));
        let stuck = rt.spawn(std::future::pending());
        let started = Instant::now();
        assert!(!wait_for_tasks(rt, vec![stuck], Duration::from_millis(100)), "the grace period ends the wait");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(wait_for_tasks(rt, Vec::new(), Duration::ZERO));
    }

    /// A read that completes later wins, even if it was issued earlier, and it sees every
    /// earlier write.
    #[test]
    fn history_operations_run_and_count_in_order() {
        let sequence = Arc::new(Mutex::new(0));
        let file = Arc::new(Mutex::new(vec!["old entry"]));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        // A slow "clear" holds its turn while a "refresh" is issued.
        let clear = {
            let (sequence, file) = (Arc::clone(&sequence), Arc::clone(&file));
            std::thread::spawn(move || {
                in_sequence(&sequence, || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    lock(&file).clear();
                    lock(&file).clone()
                })
            })
        };
        entered_rx.recv().unwrap();
        let refresh = {
            let (sequence, file) = (Arc::clone(&sequence), Arc::clone(&file));
            std::thread::spawn(move || in_sequence(&sequence, || lock(&file).clone()))
        };
        std::thread::sleep(Duration::from_millis(100));
        release_tx.send(()).unwrap();
        let (clear, refresh) = (clear.join().unwrap(), refresh.join().unwrap());
        assert_eq!(clear, (1, vec![]));
        assert_eq!(refresh, (2, vec![]), "the refresh reads after the clear and is numbered after it");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pause_keeps_resume_state_and_start_over_restarts() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ENDO_HISTORY_PATH", dir.path().join("history.json"));
        let body: Arc<Vec<u8>> = Arc::new((0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect());
        let addr = serve(Arc::clone(&body)).await;
        // The folder does not exist yet: the job creates it instead of the engine treating it as a file name.
        let downloads = dir.path().join("downloads");
        let options = DownloadOptions { num_connections: 2, output_path: Some(downloads.clone()), ..Default::default() };

        // Pause: run() returns only after saving the resume state next to the .part.
        let url = Url::parse(&format!("http://{}/file.bin", addr)).unwrap();
        let final_path = downloads.join("file.bin");
        let (hfstate, part) = part_files(&final_path);
        assert!(run(&url, &options, None, Some(Duration::from_millis(700))).await.is_err());
        assert!(part.exists() && !final_path.exists());
        let saved = DownloadState::load_from_path(&hfstate).unwrap().expect("resume state saved");
        assert!(!saved.completed_ranges.is_empty(), "progress was recorded");

        // Resume completes the same file.
        let (path, size) = run(&url, &options, None, None).await.unwrap();
        assert_eq!((path, size), (final_path.clone(), Some(body.len() as u64)));
        assert!(std::fs::read(&final_path).unwrap() == *body);
        assert!(!part.exists() && !hfstate.exists());

        // Corrupt already-downloaded bytes of a paused download: a resume keeps them, Start Over
        // deletes the partial file and fetches everything again.
        for start_over in [false, true] {
            let name = format!("other-{}.bin", start_over);
            let url = Url::parse(&format!("http://{}/{}", addr, name)).unwrap();
            let other = downloads.join(&name);
            let (_, other_part) = part_files(&other);
            assert!(run(&url, &options, None, Some(Duration::from_millis(700))).await.is_err());
            let mut file = std::fs::OpenOptions::new().write(true).open(&other_part).unwrap();
            std::io::Write::write_all(&mut file, &[0xAA; 16 * 1024]).unwrap();
            drop(file);
            let leftovers_of = start_over.then(|| other.clone());
            run(&url, &options, leftovers_of, None).await.unwrap();
            assert_eq!(std::fs::read(&other).unwrap() == *body, start_over);
        }

        // Start Over never deletes a partial file that another download holds.
        let url = Url::parse(&format!("http://{}/held.bin", addr)).unwrap();
        let held = downloads.join("held.bin");
        let (held_state, held_part) = part_files(&held);
        assert!(run(&url, &options, None, Some(Duration::from_millis(700))).await.is_err());
        let claim = hyperfetch_core::claim_target(&held).unwrap().expect("nothing is downloading it");
        let error = run(&url, &options, Some(held.clone()), None).await.unwrap_err();
        assert!(error.starts_with("Could not start over") && error.contains("still being downloaded"), "{}", error);
        assert!(held_part.exists() && held_state.exists(), "the partial file is kept");
        drop(claim);
        assert!(discard_leftovers(held.clone()).await.unwrap() >= 2);
        assert!(!held_part.exists() && !held_state.exists());
        assert!(std::fs::read(&final_path).unwrap() == *body, "other files are untouched");
    }
}
