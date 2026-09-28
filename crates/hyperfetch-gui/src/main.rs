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
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use egui::Color32;
use hyperfetch_core::chunk::ChunkSnapshot;
use hyperfetch_core::engine::{build_client, ClientKey, DownloadEngine, DownloadOptions, EngineSnapshot, SharedLimits};
use hyperfetch_core::history::{DownloadHistoryManager, HistoryEntry};
use hyperfetch_core::ingest::{self, Task};
use hyperfetch_core::media;
use hyperfetch_core::queue::{DownloadQueue, QueueItem};
use hyperfetch_core::verify::{self, BuildVerificationResult};
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;
use url::Url;

use settings::Settings;
use util::{lock, Verdict};

/// How long closing the window waits for downloads to save their resume state. Live recordings
/// are finished before it closes (see [`stop_recordings`]).
const EXIT_GRACE: Duration = Duration::from_secs(3);
/// Span of the throughput graph.
const GRAPH_WINDOW: Duration = Duration::from_secs(60);
/// No new bytes for this long shows the stalled indicator.
const STALL_HINT: Duration = Duration::from_secs(5);
/// How often the clipboard is checked for links.
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);
/// A .metalink, .meta4 or .torrent listing more files than this is added only once the user
/// agrees.
const CONFIRM_FILES: usize = 50;

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

/// Where a .metalink, .meta4 or .torrent came from, which decides what happens to its downloads.
#[derive(Clone, Copy)]
enum Origin {
    /// The URL box: the first download starts now and is shown; errors show in the form.
    Form,
    /// This line (1-based) of the queue input: errors show there, with the line kept.
    QueueLine(usize),
    /// A file dropped on the window, or a clipboard link added to the queue: the outcome shows as
    /// a notice.
    Dropped,
}

/// The downloads a .metalink, .meta4 or .torrent (or a folder, feed or playlist link) lists,
/// waiting for the user to agree to add that many, or to pick the one video a video link that
/// names its playlist too stands for.
struct Listing {
    origin: Origin,
    input: String,
    checksum: String,
    auth: String,
    /// None while the playlist of a video link is still being read (see `App::read_document`).
    tasks: Option<Vec<Task>>,
    /// The video the input names, offered in place of its whole playlist.
    video: Option<Task>,
}

/// What the user answers a listing waiting for it.
#[derive(Clone, Copy)]
enum Answer {
    Cancel,
    /// Only the video the input names (see `Listing::video`).
    Video,
    All,
}

impl Listing {
    /// "120 files (4.20 GiB)", the size shown when the document gives every file's.
    fn summary(&self) -> String {
        let tasks = self.tasks.as_deref().unwrap_or_default();
        match tasks.iter().try_fold(0u64, |total, t| total.checked_add(t.size?)) {
            Some(total) => format!("{} files ({})", tasks.len(), util::format_bytes(total)),
            None => format!("{} files", tasks.len()),
        }
    }
}

/// Results of background work, delivered to the UI thread (each send also requests a repaint).
enum AppEvent {
    /// The downloads a .metalink, .meta4 or .torrent, or a folder, feed or playlist lists, with
    /// the form's checksum and Authorization header when it was added, and the notes the listing
    /// left (what it left out).
    Read { origin: Origin, input: String, checksum: String, auth: String, result: Result<Vec<Task>, String>, notes: Vec<String> },
    JobFinished { id: usize, result: Result<(PathBuf, Option<u64>), String> },
    /// The history as read by the `generation`-th history operation to run.
    History(Result<(u64, Vec<HistoryEntry>), String>),
    /// Leftovers of a stopped download were deleted (the count), or kept because of the error.
    Discarded { id: usize, name: String, result: Result<usize, String> },
    /// A file verified, with the links to repair it from, or why none can be used.
    Verified(Result<(BuildVerificationResult, Result<Vec<Url>, String>), String>),
    RepairFinished(Result<(), String>),
    Picked(Dialog, Option<PathBuf>),
    Pasted(Result<String, String>),
    ClipboardLink(String),
    /// Whether an ffmpeg was found, looked for at launch while the user has not said whether one
    /// may be installed.
    FfmpegFound(bool),
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
    /// Its last snapshot was a live recording's (see `EngineSnapshot::is_recording`).
    recording: bool,
}

impl JobView {
    /// Recording a live stream now, which stopping finishes.
    fn records(&self) -> bool {
        self.running.is_some() && self.recording
    }

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

/// Creates the engine of a download (blocking: it may build an HTTP client). See [`shared_clients`].
type EngineMaker = Arc<dyn Fn(Vec<Url>, DownloadOptions) -> Result<DownloadEngine, String> + Send + Sync>;

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
    /// Documents being read now, and the task reading each; asking again for one of them does
    /// nothing.
    reading: HashMap<String, JoinHandle<()>>,
    /// Large listings, and video links that name their playlist, waiting for the user's answer,
    /// first come first.
    listings: Vec<Listing>,

    clipboard_enabled: Arc<AtomicBool>,
    /// Last clipboard text the watcher saw or the app copied itself; never offered again.
    clipboard_seen: Arc<Mutex<String>>,
    clipboard_banner: Option<String>,

    queue: DownloadQueue,
    engines: EngineMaker,
    /// Saves the queue in the background; `None` if its thread could not start.
    queue_saver: Option<queue_store::Saver>,
    /// Revision of the queue last handed to the saver.
    queue_saved: u64,
    jobs: HashMap<usize, JobView>,
    /// The download shown on the Downloader tab.
    focused: Option<usize>,
    /// Whether an ffmpeg was found (see [`AppEvent::FfmpegFound`]); None until that is known.
    ffmpeg_found: Option<bool>,
    /// Media downloads started (the id, and whether from zero) while the user has not said whether
    /// ffmpeg may be installed and none is found: they start once the user answers.
    ffmpeg_waiting: Vec<(usize, bool)>,

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
    /// The window was asked to close and stays open until the live recordings have finished
    /// their files (see [`stop_recordings`]).
    closing: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Handle) -> Self {
        apply_theme(&cc.egui_ctx);
        let settings = Settings::load();
        let (queue, queue_problem) = queue_store::load(&queue_store::path());
        let queue_saver = queue_store::Saver::spawn(queue_store::path())
            .inspect_err(|e| tracing::warn!("Queue changes will only be saved on exit: {}", e))
            .ok();
        let mut app = Self::with(cc.egui_ctx.clone(), rt, settings, queue, queue_saver);
        app.notice = queue_problem.map(Err);
        spawn_clipboard_watcher(
            Arc::clone(&app.clipboard_enabled),
            Arc::clone(&app.clipboard_seen),
            app.events_tx.clone(),
            cc.egui_ctx.clone(),
        );
        app.refresh_history();
        if app.settings.install_ffmpeg.is_none() {
            let found = unblock(hyperfetch_core::media::find_ffmpeg_path);
            app.spawn_event(async { AppEvent::FfmpegFound(found.await.is_ok_and(|found| found.is_some())) });
        }
        app
    }

    /// The app with `settings` and `queue`, before it watches the clipboard or reads the history.
    fn with(
        ctx: egui::Context,
        rt: tokio::runtime::Handle,
        settings: Settings,
        queue: DownloadQueue,
        queue_saver: Option<queue_store::Saver>,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        let clipboard_enabled = Arc::new(AtomicBool::new(settings.clipboard_watch));
        Self {
            rt,
            ctx,
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
            notice: None,
            reading: HashMap::new(),
            listings: Vec::new(),
            clipboard_enabled,
            clipboard_seen: Arc::new(Mutex::new(String::new())),
            clipboard_banner: None,
            queue_saved: queue.revision(),
            queue,
            engines: shared_clients(),
            queue_saver,
            jobs: HashMap::new(),
            focused: None,
            ffmpeg_found: None,
            ffmpeg_waiting: Vec::new(),
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
            closing: false,
        }
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

    /// The download records a live stream (see [`JobView::records`]).
    fn is_recording(&self, id: usize) -> bool {
        self.jobs.get(&id).is_some_and(JobView::records)
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

    /// Adds the download described by `text` (links to one file) with the current settings.
    fn add_download(&mut self, text: &str, checksum: &str, auth: &str) -> Result<usize, String> {
        let task = ingest::link_task(&ingest::split_tokens(text))?;
        let options = task_options(&self.settings, &task, checksum, auth)?;
        Ok(queue_task(&mut self.queue, task, options))
    }

    /// Reads the downloads a .metalink, .meta4 or .torrent, or a folder, feed or playlist link
    /// lists (see `ingest::needs_reading`) off the UI thread and adds them as `origin` says. A
    /// document already being read is not read again.
    fn read_document(&mut self, input: String, origin: Origin, checksum: String, auth: String) {
        let read = read_listing(&self.settings, input.clone());
        self.start_reading(input, origin, checksum, auth, read);
    }

    /// [`App::read_document`] with `read`, which reads `input`. A video link that names its
    /// playlist asks at once whether the video or the whole playlist is meant, while the playlist
    /// is read (see [`playlist_video`]).
    fn start_reading(
        &mut self,
        input: String,
        origin: Origin,
        checksum: String,
        auth: String,
        read: impl Future<Output = (Result<Vec<Task>, String>, Vec<String>)> + Send + 'static,
    ) {
        let shown = ingest::truncate_chars(&input, 60);
        if self.reading.contains_key(&input) {
            self.notice = Some(Ok(format!("Still reading {}...", shown)));
            return;
        }
        self.notice = Some(Ok(format!("Reading {}...", shown)));
        let prompt = playlist_video(&input).map(|video| Listing {
            origin,
            input: input.clone(),
            checksum: checksum.clone(),
            auth: auth.clone(),
            tasks: None,
            video: Some(video),
        });
        let key = input.clone();
        let reading = self.spawn_event(async move {
            let (result, notes) = read.await;
            AppEvent::Read { origin, input, checksum, auth, result, notes }
        });
        self.reading.insert(key, reading);
        self.listings.extend(prompt);
    }

    /// Takes what reading `input` gave: fills in the playlist its prompt waits for (see
    /// [`App::start_reading`]), or adds the downloads as `origin` says, then shows the notes a
    /// listing left. A reading answered or cancelled meanwhile was stopped, and what it read is
    /// not wanted.
    fn read_done(&mut self, origin: Origin, input: String, checksum: String, auth: String, result: Result<Vec<Task>, String>, notes: Vec<String>) {
        if self.reading.remove(&input).is_none() {
            return;
        }
        let listed = result.is_ok();
        self.listed(origin, input, checksum, auth, result);
        if listed {
            self.notice = with_notes(self.notice.take(), &notes);
        }
    }

    /// [`App::read_done`] with what was read, before its notes are shown.
    fn listed(&mut self, origin: Origin, input: String, checksum: String, auth: String, result: Result<Vec<Task>, String>) {
        self.notice = None;
        let shown = ingest::truncate_chars(&input, 60);
        if let Some(at) = self.listings.iter().position(|l| l.input == input && l.tasks.is_none()) {
            // yt-dlp found the video alone: nothing to choose.
            let alone = |tasks: &[Task], video: &Option<Task>| matches!((tasks, video), ([one], Some(video)) if one.urls == video.urls);
            match result {
                Ok(tasks) if !alone(&tasks, &self.listings[at].video) => self.listings[at].tasks = Some(tasks),
                result => {
                    let Listing { origin, checksum, auth, video, .. } = self.listings.remove(at);
                    match result {
                        Ok(tasks) => {
                            self.add_listing(origin, &input, &checksum, &auth, Ok(tasks));
                        }
                        // The link is a video all the same.
                        Err(e) => {
                            if self.add_listing(origin, &input, &checksum, &auth, Ok(video.into_iter().collect())) {
                                self.notice = Some(Err(format!("Added the video of {} alone: {}", shown, e)));
                            }
                        }
                    }
                }
            }
            return;
        }
        match result {
            // Everything it lists was downloaded before: nothing to do, not a failure.
            Ok(tasks) if tasks.is_empty() => {
                let why = "all it lists was downloaded before (untick Only new items to get it all again)";
                self.notice = Some(Ok(format!("Nothing new in {}: {}", shown, why)));
            }
            Ok(tasks) if tasks.len() > CONFIRM_FILES => {
                self.listings.push(Listing { origin, input, checksum, auth, tasks: Some(tasks), video: None })
            }
            result => {
                self.add_listing(origin, &input, &checksum, &auth, result);
            }
        }
    }

    /// Adds what a document listed (or shows why it was refused) as `origin` says: the form
    /// starts the first download, a queue line that failed is put back with its error. True when
    /// it was added.
    fn add_listing(&mut self, origin: Origin, input: &str, checksum: &str, auth: &str, result: Result<Vec<Task>, String>) -> bool {
        let added = result.and_then(|tasks| queue_listed(&mut self.queue, &self.settings, tasks, checksum, auth));
        let ok = added.is_ok();
        match (origin, added) {
            (Origin::Form, Ok(ids)) => {
                if let Some(&first) = ids.first() {
                    self.start_added(first);
                }
                if ids.len() > 1 && self.notice.is_none() {
                    self.notice = Some(Ok(format!("Started the first of {} downloads; the others wait in the queue", ids.len())));
                }
            }
            (Origin::Form, Err(e)) => self.form_error = Some(e),
            (Origin::QueueLine(_) | Origin::Dropped, Ok(ids)) => {
                let name = ingest::truncate_chars(input, 60);
                self.notice = Some(Ok(format!("Added {} download(s) from {} to the queue", ids.len(), name)));
            }
            (Origin::QueueLine(line), Err(e)) => keep_refused_line(&mut self.queue_input, &mut self.queue_error, line, input, &e),
            (Origin::Dropped, Err(e)) => self.notice = Some(Err(e)),
        }
        ok
    }

    /// Answers the first listing waiting: adds its downloads, or the one video, or drops them.
    /// The whole playlist of a video link can be picked once it has been read; picking the video
    /// or nothing before then stops reading it.
    fn answer_listing(&mut self, answer: Answer) {
        match self.listings.first() {
            Some(listing) if listing.tasks.is_some() || !matches!(answer, Answer::All) => {}
            _ => return,
        }
        let Listing { origin, input, checksum, auth, tasks, video } = self.listings.remove(0);
        if tasks.is_none() {
            if let Some(reading) = self.reading.remove(&input) {
                reading.abort();
            }
        }
        let chosen = match answer {
            Answer::Cancel => return,
            Answer::Video => video.into_iter().collect(),
            Answer::All => tasks.unwrap_or_default(),
        };
        self.add_listing(origin, &input, &checksum, &auth, Ok(chosen));
    }

    /// Adds the downloads of a .metalink, .meta4 or .torrent file dropped on the window.
    fn add_dropped(&mut self, path: PathBuf) {
        let input = path.to_string_lossy().into_owned();
        if ingest::needs_reading(&input) {
            self.read_document(input, Origin::Dropped, String::new(), String::new());
        } else {
            self.notice = Some(Err(format!("{} is not a .metalink, .meta4 or .torrent file", path.display())));
        }
    }

    /// Adds a download, starts it immediately and shows it on the Downloader tab. A file that is
    /// already downloading is shown instead of being started twice. A .metalink, .meta4 or
    /// .torrent, or a folder, feed or playlist link, is read first; the first download it lists
    /// starts, the others wait in the queue.
    fn download_now(&mut self, text: &str, checksum: &str, auth: &str) {
        self.tab = Tab::Downloader;
        if ingest::needs_reading(text) {
            self.form_error = None;
            self.read_document(text.trim().to_string(), Origin::Form, checksum.trim().to_string(), auth.to_string());
            return;
        }
        match self.add_download(text, checksum, auth) {
            Ok(id) => self.start_added(id),
            Err(e) => self.form_error = Some(e),
        }
    }

    /// Starts a download just added from the form and shows it, clearing the form. A file that is
    /// already downloading is shown instead of being started twice.
    fn start_added(&mut self, id: usize) {
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

    /// Adds each non-empty line of the queue input as one download; a .metalink, .meta4 or
    /// .torrent line, or a folder, feed or playlist link, adds every file it lists once it has
    /// been read.
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
            if ingest::needs_reading(line) {
                self.read_document(line.clone(), Origin::QueueLine(n + 1), checksum.clone(), auth.clone());
            } else if let Err(e) = self.add_download(line, &checksum, &auth) {
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
    /// files so the download starts from zero. Returns false if nothing was started. A media
    /// download waits while the user has not said whether ffmpeg may be installed and none is
    /// found (see [`App::answer_ffmpeg`]); it installs ffmpeg as the user said last, whatever the
    /// setting was when it was queued.
    fn start_job(&mut self, id: usize, fresh: bool) -> bool {
        if self.waits_for_ffmpeg_answer(id) {
            if !self.ffmpeg_waiting.iter().any(|&(waiting, _)| waiting == id) {
                self.ffmpeg_waiting.push((id, fresh));
            }
            return false;
        }
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
        let (urls, mut options, folder) = (item.urls.clone(), item.options.clone(), util::folder_to_create(item));
        options.install_ffmpeg = self.settings.install_ffmpeg == Some(true);
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
            let engines = Arc::clone(&self.engines);
            self.rt.spawn(async move {
                let result = run_job(&engines, urls, options, folder, leftovers_of, cancel, snapshot, ctx.clone()).await;
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

    /// Whether download `id` goes to yt-dlp and must wait for the user to say whether ffmpeg may
    /// be installed: they have not, and no ffmpeg was found (or that is not known yet).
    fn waits_for_ffmpeg_answer(&self, id: usize) -> bool {
        let media = |item: &QueueItem| item.options.media_preset.is_some() || item.urls.iter().any(media::is_supported_media_site);
        self.settings.install_ffmpeg.is_none() && self.ffmpeg_found != Some(true) && self.queue.get_item(id).is_some_and(media)
    }

    /// Whether the user is to be asked whether ffmpeg may be installed: a download waits for it.
    fn asks_about_ffmpeg(&self) -> bool {
        self.settings.install_ffmpeg.is_none()
            && self.ffmpeg_found == Some(false)
            && self.ffmpeg_waiting.iter().any(|&(id, _)| self.queue.get_item(id).is_some())
    }

    /// Takes the user's answer whether ffmpeg may be installed, kept as the setting, and starts
    /// the downloads that waited for it.
    fn answer_ffmpeg(&mut self, install: bool) {
        self.settings.install_ffmpeg = Some(install);
        self.start_waiting();
    }

    /// Starts the downloads that waited for the user's answer about ffmpeg (see
    /// [`App::start_job`]), as they were asked to start.
    fn start_waiting(&mut self) {
        for (id, fresh) in std::mem::take(&mut self.ffmpeg_waiting) {
            self.start_job(id, fresh);
        }
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
            view.recording = snapshot.is_recording();
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

    /// Starts queued downloads while fewer than the configured number are running; none while the
    /// window waits to close.
    fn run_scheduler(&mut self) {
        if !self.settings.auto_run_queue || self.closing {
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
                    Ok(Vec::new())
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
        let options = self.settings.tuning();
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
                    verify::repair_missing_ranges(&path, total_size, &missing, &urls, &options, Some(cancel), on_progress)
                        .await;
                AppEvent::RepairFinished(result)
            })
        };
        self.repair = Some(Repair { target, cancel, task, progress });
        self.verify_message = None;
    }

    // ---- events --------------------------------------------------------------------------

    fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Read { origin, input, checksum, auth, result, notes } => self.read_done(origin, input, checksum, auth, result, notes),
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
                    Ok((result, repair_urls)) => {
                        // Links saved without their secret cannot repair it: the user is told so.
                        let repair_urls = repair_urls.unwrap_or_else(|e| {
                            self.verify_message = Some(e);
                            Vec::new()
                        });
                        self.verification = Some(Verification { result, repair_urls });
                    }
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
            }
            AppEvent::FfmpegFound(found) => {
                self.ffmpeg_found = Some(found);
                // Nothing to ask: what waited to know it starts.
                if found {
                    self.start_waiting();
                }
            }
        }
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
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for path in dropped {
            self.add_dropped(path);
        }
        self.drain_snapshots();
        // Asked to close, the window first stays open for the live recordings to finish; asked
        // again, it closes at once.
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing && stop_recordings(&mut self.queue, &self.jobs) {
            self.closing = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.closing && !self.jobs.values().any(JobView::records) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
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
    /// then saves the queue. Live recordings have finished by now, unless the user closed the
    /// window a second time while they did (see [`stop_recordings`]).
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

/// Engine options for `task` with `settings`, the per-download checksum (else the task's own;
/// never for a document downloaded itself, as it is for the file the document lists) and
/// Authorization header (never for the hosts a document lists). A task that names its file is
/// saved as that (sub)path of the save folder; one in a folder, in that folder of the save folder.
fn task_options(settings: &Settings, task: &Task, checksum: &str, auth: &str) -> Result<DownloadOptions, String> {
    let checksum = if task.document_itself { "" } else { checksum };
    let checksum = match checksum.trim() {
        "" => task.checksum.as_deref().unwrap_or_default(),
        typed => typed,
    };
    let auth = if task.from_document { "" } else { auth };
    let mut options = settings.download_options(&task.urls, checksum, auth)?;
    for part in [&task.folder, &task.name].into_iter().flatten() {
        options.output_path = options.output_path.map(|path| path.join(part));
    }
    options.media_name = task.media_name.clone();
    Ok(options)
}

/// Engine options for the downloads a .metalink, .meta4 or .torrent lists, each saved under the
/// save folder joined with its (sub)path; an error if one of them is refused. The form's checksum
/// applies to the file a document of one file lists, not to a document ingest leaves to the
/// engine to download itself; its Authorization header `auth` is for the hosts the user
/// typed (the document's own, when ingest leaves the link to the engine), never for those a
/// document lists.
fn document_options(settings: &Settings, tasks: &[Task], checksum: &str, auth: &str) -> Result<Vec<DownloadOptions>, String> {
    if tasks.len() > 1 && !checksum.trim().is_empty() {
        return Err(format!("The checksum in Advanced Options is for a single file, but this lists {} files", tasks.len()));
    }
    tasks.iter().map(|task| task_options(settings, task, checksum, auth)).collect()
}

/// Queues the downloads a .metalink, .meta4 or .torrent lists; nothing if one is refused.
fn queue_listed(
    queue: &mut DownloadQueue,
    settings: &Settings,
    tasks: Vec<Task>,
    checksum: &str,
    auth: &str,
) -> Result<Vec<usize>, String> {
    let options = document_options(settings, &tasks, checksum, auth)?;
    Ok(tasks.into_iter().zip(options).map(|(task, options)| queue_task(queue, task, options)).collect())
}

/// Queues one download; one its input named is shown under that name at once. Its target is
/// left for the engine to report, so Start Over and Delete Leftovers never reach a file another
/// download holds under that name.
fn queue_task(queue: &mut DownloadQueue, task: Task, options: DownloadOptions) -> usize {
    match task.name {
        Some(_) => queue.add_named_item(task.urls, options),
        None => queue.add_item(task.urls, options),
    }
}

/// Puts queue line `line` back into the queue input with its error, as a line refused at once is,
/// so it can be corrected.
fn keep_refused_line(queue_input: &mut String, queue_error: &mut Option<String>, line: usize, input: &str, error: &str) {
    if !queue_input.is_empty() {
        queue_input.push('\n');
    }
    queue_input.push_str(input);
    let error = format!("Line {}: {}", line, error);
    *queue_error = Some(match queue_error.take() {
        Some(errors) => format!("{}\n{}", errors, error),
        None => error,
    });
}

/// `notice` followed by the notes a listing left (what it left out), shown as a warning.
fn with_notes(notice: Option<Result<String, String>>, notes: &[String]) -> Option<Result<String, String>> {
    if notes.is_empty() {
        return notice;
    }
    let shown = notice.map(|n| n.unwrap_or_else(|e| e));
    Some(Err(shown.into_iter().chain(notes.iter().cloned()).collect::<Vec<_>>().join("\n")))
}

/// The video `input` names when it is one video link that names its playlist too (see
/// `media::video_in_playlist`): what the user may pick instead of the playlist.
fn playlist_video(input: &str) -> Option<Task> {
    let [token] = &ingest::split_tokens(input)[..] else { return None };
    // As ingest takes it: a "leaving this site" link is its target.
    ingest::link_task(&[token]).ok().filter(|task| matches!(&task.urls[..], [url] if hyperfetch_core::media::video_in_playlist(url)))
}

/// How the links in `input` are listed with `settings`: a video link that names its playlist
/// lists the playlist, for the user to pick it or the video (see [`playlist_video`]).
fn read_options(settings: &Settings, input: &str) -> ingest::ListOptions {
    ingest::ListOptions { whole_playlist: playlist_video(input).is_some(), ..settings.list_options() }
}

/// Reads the downloads `input` (a local or remote .metalink, .meta4 or .torrent, or a link that
/// lists many) lists, fetching through the proxy setting, and the notes the listing left.
fn read_listing(settings: &Settings, input: String) -> impl Future<Output = (Result<Vec<Task>, String>, Vec<String>)> + Send + 'static {
    let (notes, noted) = mpsc::channel();
    let list = ingest::ListOptions { notes: Some(notes), ..read_options(settings, &input) };
    async move {
        let read = async {
            let tokens = ingest::input_tokens(&input).await;
            let http = ingest::descriptor_client(list.proxy.as_deref())?;
            ingest::ingest(&tokens, &http, &list).await
        };
        let result = read.await;
        // What was left out and what could not be read alike: the notice shows them as errors.
        (result, noted.try_iter().map(|note| note.text).collect())
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

/// Stops the live recordings among `jobs` (see [`JobView::records`]) for the window to close, and
/// returns whether there are any. Stopping finishes a recording's file, which takes as long as
/// the file is big (yt-dlp remuxes it), so the window waits for them rather than cut them off
/// after [`EXIT_GRACE`].
fn stop_recordings(queue: &mut DownloadQueue, jobs: &HashMap<usize, JobView>) -> bool {
    let mut any = false;
    for (&id, view) in jobs {
        let Some(running) = view.running.as_ref().filter(|_| view.recording) else { continue };
        any = true;
        if queue.mark_pausing(id) {
            running.cancel.notify_one();
        }
    }
    any
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

/// Engines that share one HTTP client per [`ClientKey`], so a download reuses the connections and
/// TLS sessions of earlier ones (see [`cached_client`]), and the speed limit: downloads running at
/// once stay within it together.
fn shared_clients() -> EngineMaker {
    let cache = Mutex::new(HashMap::new());
    let limits = SharedLimits::default();
    Arc::new(move |urls, options| {
        let client = cached_client(&cache, &options, build_client)?;
        Ok(DownloadEngine::with_client(urls, options, client).sharing_limit(&limits))
    })
}

/// The client `cache` holds for the key of `options`, built with `build` if there is none yet or
/// the cookies file changed since, so cookies exported anew reach the next download. A client that
/// fails to build is not kept: each download needing it reports the error.
fn cached_client<C: Clone>(
    cache: &Mutex<HashMap<ClientKey, (C, Option<SystemTime>)>>,
    options: &DownloadOptions,
    build: impl FnOnce(&DownloadOptions) -> Result<C, String>,
) -> Result<C, String> {
    let key = ClientKey::of(options);
    let cookies_changed_at = options.cookies_path.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    // Held while building, so downloads starting together build their client once.
    let mut cache = lock(cache);
    if let Some((client, _)) = cache.get(&key).filter(|(_, built_for)| *built_for == cookies_changed_at) {
        return Ok(client.clone());
    }
    let client = build(options)?;
    cache.insert(key, (client.clone(), cookies_changed_at));
    Ok(client)
}

/// One engine run, saving into `folder` (created first if missing; see
/// [`util::folder_to_create`]). A cancel request calls `engine.cancel()` and keeps awaiting
/// `run()`, so the engine flushes data, saves its resume state and stops yt-dlp before this
/// returns.
#[allow(clippy::too_many_arguments)]
async fn run_job(
    engines: &EngineMaker,
    urls: Vec<Url>,
    options: DownloadOptions,
    folder: Option<PathBuf>,
    leftovers_of: Option<PathBuf>,
    cancel: Arc<Notify>,
    slot: Arc<Mutex<Option<EngineSnapshot>>>,
    ctx: egui::Context,
) -> Result<(PathBuf, Option<u64>), String> {
    if let Some(final_path) = leftovers_of {
        discard_leftovers(final_path).await.map_err(|e| format!("Could not start over: {}", e))?;
    }
    // The engine treats a missing output directory as a file name.
    if let Some(dir) = folder {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("Cannot create the download folder {}: {}", dir.display(), e))?;
    }
    // Building the engine may read the cookies file and TLS roots.
    let engine = {
        let engines = Arc::clone(engines);
        unblock(move || engines(urls, options)).await??
    };

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
    // Started again to stop a live recording (a debug build has a console), this only does that.
    hyperfetch_core::media::serve_ctrl_c();
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
        let (url, options, cancel_job, job_slot) = (url.clone(), options.clone(), Arc::clone(&cancel), Arc::clone(&slot));
        let job = tokio::spawn(async move {
            let engines = shared_clients();
            let folder = options.output_path.clone();
            run_job(&engines, vec![url], options, folder, leftovers_of, cancel_job, job_slot, egui::Context::default()).await
        });
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

    /// Downloads with the same client settings get the same client; other settings, a changed
    /// cookies file or an earlier failed build get a new one.
    #[test]
    fn downloads_share_a_client_per_key() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = dir.path().join("cookies.txt");
        std::fs::write(&cookies, "# Netscape HTTP Cookie File\n").unwrap();
        let cache = Mutex::new(HashMap::new());
        let builds = std::cell::Cell::new(0);
        let build = |_: &DownloadOptions| {
            builds.set(builds.get() + 1);
            Ok::<_, String>(builds.get())
        };
        let plain = DownloadOptions::default();
        let other_folder = DownloadOptions { output_path: Some(dir.path().into()), num_connections: 2, ..plain.clone() };
        let proxied = DownloadOptions { proxy: Some("http://127.0.0.1:9".into()), ..plain.clone() };
        let with_cookies = DownloadOptions { cookies_path: Some(cookies.clone()), ..plain.clone() };

        assert_eq!(cached_client(&cache, &plain, build), Ok(1));
        assert_eq!(cached_client(&cache, &other_folder, build), Ok(1), "same key, same client");
        assert_eq!(cached_client(&cache, &proxied, build), Ok(2));
        assert_eq!(cached_client(&cache, &with_cookies, build), Ok(3));
        assert_eq!(cached_client(&cache, &with_cookies, build), Ok(3));
        let later = std::fs::metadata(&cookies).unwrap().modified().unwrap() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&cookies).unwrap().set_modified(later).unwrap();
        assert_eq!(cached_client(&cache, &with_cookies, build), Ok(4), "a changed cookies file is read again");
        assert_eq!(builds.get(), 4);

        let failing = DownloadOptions { proxy: Some("http://127.0.0.1:10".into()), ..plain };
        assert!(cached_client(&cache, &failing, |_: &DownloadOptions| Err::<u32, _>("bad proxy".to_string())).is_err());
        assert_eq!(cached_client(&cache, &failing, build), Ok(5), "a failed build is not kept");
    }

    /// Closing the window stops the live recordings, which then finish their files while it
    /// waits, and only them; with none running it closes at once.
    #[tokio::test]
    async fn closing_the_window_stops_the_live_recordings_first() {
        use hyperfetch_core::queue::QueueItemStatus;
        let mut queue = DownloadQueue::new();
        let url = || vec![Url::parse("https://example.com/live/index.m3u8").unwrap()];
        let (live, file) = (queue.add_item(url(), DownloadOptions::default()), queue.add_item(url(), DownloadOptions::default()));
        assert!(queue.mark_started(live) && queue.mark_started(file));
        let running = |recording| JobView {
            running: Some(Running { cancel: Arc::new(Notify::new()), task: tokio::spawn(std::future::pending()), snapshot: Arc::default() }),
            recording,
            ..JobView::default()
        };
        let mut jobs = HashMap::from([(file, running(false))]);
        assert!(!stop_recordings(&mut queue, &jobs), "nothing records");

        jobs.insert(live, running(true));
        assert!(stop_recordings(&mut queue, &jobs));
        assert_eq!(queue.get_item(live).map(|item| &item.status), Some(&QueueItemStatus::Pausing));
        assert_eq!(queue.get_item(file).map(|item| &item.status), Some(&QueueItemStatus::Downloading));
        let asked = jobs[&live].running.as_ref().map(|running| Arc::clone(&running.cancel)).unwrap();
        tokio::time::timeout(Duration::from_secs(5), asked.notified()).await.expect("the recording is asked to stop");
        // Still finishing: the window waits; done, it closes.
        assert!(stop_recordings(&mut queue, &jobs));
        jobs.get_mut(&live).unwrap().running = None;
        assert!(!stop_recordings(&mut queue, &jobs));
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

    /// Points download history at a file of this test process, once for every test, so no test
    /// touches the user's history or switches the file while another runs.
    fn isolate_history() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let path = std::env::temp_dir().join(format!("hf-gui-test-history-{}.json", std::process::id()));
            std::env::set_var("ENDO_HISTORY_PATH", path);
        });
    }

    /// A download a document or magnet named is saved as that (sub)path of the save folder,
    /// checked against the document's checksum unless the form gives one.
    #[test]
    fn named_downloads_are_saved_under_their_path_in_the_save_folder() {
        let settings = Settings { save_dir: "dl".into(), ..Settings::default() };
        let url = Url::parse("https://m.example/disc.iso").unwrap();
        let listed = format!("sha256:{}", "ab".repeat(32));
        let named = Task {
            urls: vec![url.clone()],
            name: Some(PathBuf::from("release").join("disc.iso")),
            checksum: Some(listed.clone()),
            ..Task::default()
        };
        let options = task_options(&settings, &named, "", "").unwrap();
        assert_eq!(options.output_path, Some(PathBuf::from("dl").join("release").join("disc.iso")));
        assert_eq!(options.expected_checksum, Some(listed));
        let typed = format!("md5:{}", "cd".repeat(16));
        assert_eq!(task_options(&settings, &named, &typed, "").unwrap().expected_checksum, Some(typed));

        let plain = Task { urls: vec![url], ..Task::default() };
        assert_eq!(task_options(&settings, &plain, " ", "").unwrap().output_path, Some(PathBuf::from("dl")));
    }

    /// The Authorization header goes to the hosts the user typed, never to those a document lists.
    #[test]
    fn the_authorization_header_skips_the_hosts_a_document_lists() {
        let settings = Settings { save_dir: "dl".into(), ..Settings::default() };
        let typed = Task { urls: vec![Url::parse("https://a.example/f.iso").unwrap()], ..Task::default() };
        let listed = Task { urls: typed.urls.clone(), from_document: true, ..Task::default() };
        assert_eq!(task_options(&settings, &typed, "", "Bearer t").unwrap().auth_header.as_deref(), Some("Bearer t"));
        assert_eq!(task_options(&settings, &listed, "", "Bearer t").unwrap().auth_header, None);
    }

    /// A task in a folder is saved in that folder of the save folder, as its name if it has one;
    /// one without a name is queued for the engine to name, in a folder made for it.
    #[test]
    fn a_task_in_a_folder_is_saved_in_it() {
        let settings = Settings { save_dir: "dl".into(), ..Settings::default() };
        let urls = vec![Url::parse("https://m.example/ep1").unwrap()];
        let named = Task { urls: urls.clone(), folder: Some("Show".into()), name: Some("ep1.mp3".into()), ..Task::default() };
        let path = task_options(&settings, &named, "", "").unwrap().output_path;
        assert_eq!(path, Some(PathBuf::from("dl").join("Show").join("ep1.mp3")));
        // A playlist entry, which yt-dlp names by its title and id.
        let template = "%(title)s [%(id)s].%(ext)s".to_string();
        let unnamed = Task { urls, folder: Some("Show".into()), media_name: Some(template.clone()), ..Task::default() };
        let mut queue = DownloadQueue::new();
        let [id] = queue_listed(&mut queue, &settings, vec![unnamed], "", "").unwrap()[..] else { panic!("one download") };
        let item = queue.get_item(id).unwrap();
        assert_eq!(util::folder_to_create(item), Some(PathBuf::from("dl").join("Show")));
        assert_eq!(item.options.media_name, Some(template));
    }

    /// Each file a document lists keeps its own path and checksum; the form's checksum only fits a
    /// document of one file, and one refused file refuses them all.
    #[test]
    fn a_document_adds_each_file_it_lists_or_none() {
        let settings = Settings { save_dir: "dl".into(), ..Settings::default() };
        let file = |name: &str, checksum: Option<String>| Task {
            urls: vec![Url::parse(&format!("https://m.example/{}", name)).unwrap()],
            name: Some(PathBuf::from("pack").join(name)),
            checksum,
            ..Task::default()
        };
        let (a, b) = (format!("sha256:{}", "aa".repeat(32)), format!("md5:{}", "bb".repeat(16)));
        let tasks = [file("a.bin", Some(a.clone())), file("b.bin", Some(b.clone()))];
        let options = document_options(&settings, &tasks, " ", "").unwrap();
        let saved: Vec<_> = options.iter().map(|o| (o.output_path.clone(), o.expected_checksum.clone())).collect();
        assert_eq!(
            saved,
            [
                (Some(PathBuf::from("dl").join("pack").join("a.bin")), Some(a)),
                (Some(PathBuf::from("dl").join("pack").join("b.bin")), Some(b)),
            ]
        );
        assert!(options.iter().all(|o| o.auth_header.is_none()));

        let typed = format!("sha256:{}", "cc".repeat(32));
        assert!(document_options(&settings, &tasks, &typed, "").unwrap_err().contains("lists 2 files"));
        let one = [file("a.bin", None)];
        assert_eq!(document_options(&settings, &one, &typed, "").unwrap()[0].expected_checksum, Some(typed));
        let refused = [file("a.bin", None), file("b.bin", Some("crc32:1234".to_string()))];
        assert!(document_options(&settings, &refused, "", "").is_err());
    }

    /// A document's downloads are queued all together or not at all; a queue line whose document
    /// could not be read is put back with its error; a large listing says how much it adds.
    #[test]
    fn a_read_document_is_queued_whole_or_its_line_is_kept() {
        let settings = Settings { save_dir: "dl".into(), ..Settings::default() };
        let file = |n: u32| Task {
            urls: vec![Url::parse(&format!("https://m.example/{}.bin", n)).unwrap()],
            name: Some(PathBuf::from(format!("{}.bin", n))),
            size: Some(1024),
            from_document: true,
            ..Task::default()
        };
        let mut queue = DownloadQueue::new();
        let ids = queue_listed(&mut queue, &settings, vec![file(1), file(2)], "", "Bearer t").unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|&id| queue.get_item(id).is_some_and(|item| item.names_file && item.target_path.is_none())));
        assert!(ids.iter().all(|&id| queue.get_item(id).is_some_and(|item| item.options.auth_header.is_none())), "a listed host got it");
        // A document its host would not hand over without a login is left to the engine as the
        // link typed, which the header is for; the form's checksum is for the file it lists.
        let link = Task { urls: vec![Url::parse("https://tracker.example/dl/1.torrent").unwrap()], document_itself: true, ..Task::default() };
        let typed = format!("sha256:{}", "cc".repeat(32));
        let [id] = queue_listed(&mut queue, &settings, vec![link], &typed, "Bearer t").unwrap()[..] else { panic!("one download") };
        let options = queue.get_item(id).map(|item| (item.options.auth_header.clone(), item.options.expected_checksum.clone()));
        assert_eq!(options, Some((Some("Bearer t".to_string()), None)));
        queue.remove_item(id);
        let refused = Task { checksum: Some("crc32:1".to_string()), ..file(3) };
        assert!(queue_listed(&mut queue, &settings, vec![file(4), refused], "", "").is_err());
        assert_eq!(queue.items().len(), 2, "nothing of a refused document is queued");

        let (mut input, mut errors) = ("https://ok.example/f".to_string(), None);
        keep_refused_line(&mut input, &mut errors, 2, "https://a.example/x.torrent", "Cannot fetch");
        keep_refused_line(&mut input, &mut errors, 3, "y.meta4", "Cannot read");
        assert_eq!(input, "https://ok.example/f\nhttps://a.example/x.torrent\ny.meta4");
        assert_eq!(errors.as_deref(), Some("Line 2: Cannot fetch\nLine 3: Cannot read"));

        let listing = |tasks| Listing { origin: Origin::Dropped, input: String::new(), checksum: String::new(), auth: String::new(), tasks: Some(tasks), video: None };
        assert_eq!(listing((1..=60).map(file).collect()).summary(), "60 files (60.00 KiB)");
        assert_eq!(listing(vec![file(1), Task { size: None, ..file(2) }]).summary(), "2 files");
    }

    /// A video link that names its playlist too is read as the playlist, for the user to pick it
    /// or the one video, which is then downloaded as the link it is.
    #[test]
    fn a_video_in_a_playlist_offers_the_video_or_the_whole_playlist() {
        let settings = Settings { only_new: false, latest: 5, ..Settings::default() };
        let link = "https://www.youtube.com/watch?v=jNQXAC9IVRw&list=PLbpi6ZahtOH6Blw3RGYpWkSByi_T7Rygb";
        for input in [link.to_string(), format!("https://www.youtube.com/redirect?q={}", url::form_urlencoded::byte_serialize(link.as_bytes()).collect::<String>())] {
            let video = playlist_video(&input).unwrap_or_else(|| panic!("{input}"));
            assert_eq!(video.urls, [Url::parse(link).unwrap()]);
            let list = read_options(&settings, &input);
            assert!(list.whole_playlist && !list.only_new);
            assert_eq!(list.latest, Some(5));
        }
        let mirrored = format!("{link} https://mirror.example/v.mp4");
        for other in ["https://www.youtube.com/watch?v=jNQXAC9IVRw", "https://www.youtube.com/playlist?list=PL1", "https://www.youtube.com/@NASA", &mirrored] {
            assert!(playlist_video(other).is_none(), "{other}");
            assert!(!read_options(&settings, other).whole_playlist, "{other}");
        }
    }

    /// The app, saving into `dir`, on `rt`: a runtime that never runs what it is given here, so
    /// nothing is read or downloaded.
    fn test_app(rt: &tokio::runtime::Runtime, dir: &std::path::Path) -> App {
        let settings = Settings { save_dir: dir.to_string_lossy().into_owned(), ..Settings::default() };
        App::with(egui::Context::default(), rt.handle().clone(), settings, DownloadQueue::new(), None)
    }

    /// The first link of each download queued.
    fn queued(app: &App) -> Vec<String> {
        app.queue.items().iter().map(|item| item.urls[0].to_string()).collect()
    }

    /// A video link that names its playlist asks at once: the video can be picked while the
    /// playlist is read, which stops the reading (what it gives later is dropped), the whole
    /// playlist once it has been read. yt-dlp finding the video alone adds it without asking, and
    /// a playlist that cannot be read adds the video, saying why, whatever the link came from.
    #[test]
    fn a_video_in_a_playlist_is_asked_about_at_once_and_added_as_answered() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        let link = "https://www.youtube.com/watch?v=jNQXAC9IVRw&list=PLbpi6ZahtOH6Blw3RGYpWkSByi_T7Rygb";
        let entry = |id: &str| Task {
            urls: vec![Url::parse(&format!("https://www.youtube.com/watch?v={id}")).unwrap()],
            folder: Some("Top".into()),
            ..Task::default()
        };
        let ask = |app: &mut App| app.start_reading(link.to_string(), Origin::QueueLine(1), String::new(), String::new(), std::future::pending());
        let read = |app: &mut App, result: Result<Vec<Task>, String>| {
            let (input, checksum, auth) = (link.to_string(), String::new(), String::new());
            app.handle_event(AppEvent::Read { origin: Origin::QueueLine(1), input, checksum, auth, result, notes: Vec::new() })
        };

        ask(&mut app);
        assert_eq!((app.listings.len(), app.listings[0].tasks.is_none()), (1, true));
        app.answer_listing(Answer::All);
        assert_eq!(app.listings.len(), 1, "the playlist is not read yet");
        read(&mut app, Ok(vec![entry("a"), entry("b")]));
        assert_eq!(app.listings[0].tasks.as_ref().map(Vec::len), Some(2));
        app.answer_listing(Answer::All);
        assert_eq!(queued(&app), ["https://www.youtube.com/watch?v=a", "https://www.youtube.com/watch?v=b"]);
        assert!(app.listings.is_empty() && app.reading.is_empty());

        ask(&mut app);
        app.answer_listing(Answer::Video);
        assert_eq!(queued(&app)[2], link);
        assert!(app.reading.is_empty(), "still reading the playlist");
        read(&mut app, Ok(vec![entry("c")]));
        assert_eq!((queued(&app).len(), app.listings.len()), (3, 0));

        ask(&mut app);
        read(&mut app, Ok(vec![Task { urls: vec![Url::parse(link).unwrap()], ..Task::default() }]));
        assert_eq!((queued(&app).len(), app.listings.len()), (4, 0));

        ask(&mut app);
        read(&mut app, Err("ERROR: [youtube:tab] PL1: The playlist does not exist.".to_string()));
        assert_eq!((queued(&app).len(), app.listings.len()), (5, 0));
        let notice = app.notice.clone().and_then(Result::err).unwrap_or_default();
        assert!(notice.starts_with("Added the video of https://www.youtube.com/watch") && notice.ends_with("does not exist."), "{notice}");

        // Nothing new in the playlist: the video alone can be picked.
        ask(&mut app);
        read(&mut app, Ok(Vec::new()));
        assert_eq!(app.listings[0].tasks.as_deref(), Some(&[][..]));
        app.answer_listing(Answer::Cancel);
        assert_eq!((queued(&app).len(), app.listings.len()), (5, 0));
    }

    /// A video waits, queued, while the user has not said whether ffmpeg may be installed and
    /// none is found, and then starts, installing it as the user answered; a file never waits,
    /// nor does a video once an ffmpeg is found.
    #[test]
    fn a_video_waits_for_the_answer_whether_ffmpeg_may_be_installed() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        let built = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&built);
        app.engines = Arc::new(move |urls: Vec<Url>, options: DownloadOptions| {
            lock(&seen).push((urls[0].host_str().unwrap_or_default().to_string(), options.install_ffmpeg));
            Err("not downloaded here".to_string())
        });
        let video = |app: &mut App| app.add_download("https://www.youtube.com/watch?v=jNQXAC9IVRw", "", "").unwrap();
        let first = video(&mut app);
        let file = app.add_download("https://a.example/f.iso", "", "").unwrap();
        // Whether ffmpeg is found is not known yet: nothing to ask, but the video waits.
        assert!(!app.start_job(first, true) && !app.asks_about_ffmpeg());
        assert!(app.start_job(file, false), "a file does not need ffmpeg");
        app.handle_event(AppEvent::FfmpegFound(false));
        assert!(app.asks_about_ffmpeg());
        assert!(!app.start_job(first, false), "still waiting");
        assert_eq!(app.ffmpeg_waiting, [(first, true)], "started as it was asked to first");

        app.answer_ffmpeg(true);
        assert_eq!(app.settings.install_ffmpeg, Some(true));
        assert!(!app.asks_about_ffmpeg() && app.ffmpeg_waiting.is_empty());
        assert!(app.jobs.get(&first).is_some_and(|view| view.running.is_some()), "started once answered");
        rt.block_on(async {
            while lock(&built).len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let mut built = lock(&built).clone();
        built.sort();
        assert_eq!(built, [("a.example".to_string(), false), ("www.youtube.com".to_string(), true)], "the file started before the answer");

        // Found: nothing waits, and nothing is installed without an answer.
        let mut app = test_app(&rt, dir.path());
        app.handle_event(AppEvent::FfmpegFound(true));
        let second = video(&mut app);
        assert!(app.start_job(second, false) && !app.asks_about_ffmpeg());
    }

    /// A playlist or channel with nothing new is nothing to do: said so, and its queue line is
    /// not kept as an error.
    #[test]
    fn a_listing_with_nothing_new_is_a_notice() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        let channel = "https://www.youtube.com/@NASA";
        app.start_reading(channel.to_string(), Origin::QueueLine(2), String::new(), String::new(), std::future::pending());
        assert!(app.listings.is_empty());
        let (input, checksum, auth) = (channel.to_string(), String::new(), String::new());
        app.handle_event(AppEvent::Read { origin: Origin::QueueLine(2), input, checksum, auth, result: Ok(Vec::new()), notes: Vec::new() });
        assert!(app.queue.items().is_empty());
        assert_eq!((app.queue_input.as_str(), app.queue_error.as_deref()), ("", None));
        let notice = app.notice.clone().and_then(Result::ok).unwrap_or_default();
        assert!(notice.starts_with("Nothing new in https://www.youtube.com/@NASA:") && notice.contains("Only new items"), "{notice}");
    }

    /// A remote document is fetched through the proxy setting: its host does not exist, so only
    /// the proxy can answer.
    #[tokio::test]
    async fn documents_are_read_through_the_proxy_setting() {
        let xml = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="a.bin"><url>https://m.example/a.bin</url></file></metalink>"#;
        let proxy = serve(Arc::new(xml.to_vec())).await;
        let settings = Settings { proxy: format!("http://{}", proxy), ..Settings::default() };
        let (tasks, notes) = read_listing(&settings, "http://documents.invalid/list.meta4".to_string()).await;
        assert_eq!(tasks.unwrap().iter().map(|t| t.name.clone()).collect::<Vec<_>>(), [Some(PathBuf::from("a.bin"))]);
        assert!(notes.is_empty(), "{notes:?}");
    }

    /// What a folder listing left out reaches the notice, as a warning after what was added.
    #[tokio::test]
    async fn a_listing_tells_what_it_left_out() {
        // MediaFire's folder API, which answers the folder's name, files and (no) subfolders
        // alike here: one of the two files is behind a password.
        let json = br#"{"response":{"result":"Success","folder_info":{"name":"Pack"},"folder_content":{"files":[
            {"quickkey":"lockedfile0001","filename":"a.bin","password_protected":"yes"},
            {"quickkey":"openfile000002","filename":"b.bin","password_protected":"no"}],"more_chunks":"no"}}}"#;
        let proxy = serve(Arc::new(json.to_vec())).await;
        let settings = Settings { proxy: format!("http://{}", proxy), ..Settings::default() };
        let (tasks, notes) = read_listing(&settings, "http://www.mediafire.com/folder/pack00000001".to_string()).await;
        assert_eq!(tasks.unwrap().len(), 1);
        assert_eq!(notes, ["1 files of the MediaFire folder are protected by a password and were left out"]);

        let added = Some(Ok("Added 1 download(s) from x to the queue".to_string()));
        let shown = with_notes(added, &notes);
        assert_eq!(shown, Some(Err(format!("Added 1 download(s) from x to the queue\n{}", notes[0]))));
        assert_eq!(with_notes(None, &notes), Some(Err(notes[0].clone())));
        assert_eq!(with_notes(None, &[]), None);
    }

    /// A download its input named is shown under that name at once and saved as that file, in
    /// folders made for it, never into a folder of that name.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_named_download_is_saved_as_its_file() {
        isolate_history();
        let dir = tempfile::tempdir().unwrap();
        let body: Arc<Vec<u8>> = Arc::new((0..64 * 1024u32).map(|i| (i % 251) as u8).collect());
        let addr = serve(Arc::clone(&body)).await;
        let downloads = dir.path().join("downloads");
        let settings = Settings { save_dir: downloads.to_string_lossy().into_owned(), connections: 2, ..Settings::default() };
        let task = Task {
            urls: vec![Url::parse(&format!("http://{}/get?id=7", addr)).unwrap()],
            name: Some(PathBuf::from("release").join("disc.iso")),
            ..Task::default()
        };
        let options = task_options(&settings, &task, "", "").unwrap();
        let mut queue = DownloadQueue::new();
        let id = queue_task(&mut queue, task, options);
        let item = queue.get_item(id).unwrap().clone();
        assert_eq!(item.filename, "disc.iso");
        // Only the engine says which file is its own: Start Over and Delete Leftovers of a named
        // download that never started must not reach another download's partial file.
        assert_eq!(item.target_path, None);

        let (engines, cancel, slot) = (shared_clients(), Arc::new(Notify::new()), Arc::new(Mutex::new(None)));
        let folder = util::folder_to_create(&item);
        let job = run_job(&engines, item.urls, item.options, folder, None, cancel, slot, egui::Context::default());
        let (path, size) = tokio::time::timeout(Duration::from_secs(60), job).await.unwrap().unwrap();
        assert_eq!(path, downloads.join("release").join("disc.iso"));
        assert_eq!(size, Some(body.len() as u64));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pause_keeps_resume_state_and_start_over_restarts() {
        isolate_history();
        let dir = tempfile::tempdir().unwrap();
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
