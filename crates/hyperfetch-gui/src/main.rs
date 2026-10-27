#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod desktop;
mod ipc;
mod queue_store;
mod recording;
mod settings;
mod taskbar;
mod tray;
mod ui;
mod util;

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use hyperfetch_core::chunk::ChunkSnapshot;
use hyperfetch_core::engine::{build_client, ClientKey, DownloadEngine, DownloadOptions, EngineSnapshot, SharedLimits};
use hyperfetch_core::history::{DownloadHistoryManager, HistoryEntry};
use hyperfetch_core::ingest::{self, Task};
use hyperfetch_core::media;
use hyperfetch_core::paste;
use hyperfetch_core::queue::{DownloadQueue, QueueItem, QueueItemStatus};
use hyperfetch_core::updater::{self, Installed, Update};
use hyperfetch_core::verify::{self, BuildVerificationResult};
use hyperfetch_core::zipdir::ZipEntry;
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;
use url::Url;

use desktop::WhenDone;
use settings::Settings;
use tray::TrayAction;
use util::{lock, Verdict};

/// The window's title and the app's name.
const TITLE: &str = "Endo's Unified Downloader";
/// Set for the build an update starts, which must not take the closing app for another copy
/// already running (see `main`).
const RESTARTED: &str = "ENDO_RESTARTED";
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
/// Why the form's checksum is refused for several downloads.
const SINGLE_CHECKSUM: &str = "The checksum in Advanced Options is for a single file: add that download on its own";
/// How long "See what's inside" waits for a ZIP's list of files, all its requests together (each
/// gives up after 30 s without an answer: see `zipdir::list_zip`).
const ZIP_PREVIEW_LIMIT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 60 });

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    /// The link form, and the download shown (see `App::focused`).
    Downloader,
    Queue,
    History,
    Settings,
}

#[derive(Debug, Clone, Copy)]
enum Dialog {
    SaveDir,
    CookiesFile,
    VerifyFile,
    /// The folder of the `n`th sorting category (see `Settings::categories`).
    CategoryFolder(usize),
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
    /// Clipboard text to offer as a download: what the banner shows of it, and the text.
    ClipboardLink(String, String),
    /// The text of a .txt dropped on the window, or why it could not be read.
    DroppedText(Result<String, String>),
    /// Whether an ffmpeg was found, looked for at launch while the user has not said whether one
    /// may be installed.
    FfmpegFound(bool),
    /// A download the browser extension sent to `POST /add`.
    RemoteAdd(ipc::RemoteAddPayload),
    /// A recording the browser extension finished, to be joined into the save folder.
    Recorded(recording::Finished),
    /// The notice saying where a recording was saved, or why it could not be joined.
    Merged(Result<String, String>),
    /// What looking for a newer release found; `manual` when the user asked (Check now), which
    /// says so also when there is none.
    UpdateChecked { result: Result<Option<Update>, String>, manual: bool },
    /// The update the user chose was installed (the app then restarts), or why not.
    UpdateInstalled(Result<Installed, String>),
    /// A choice in the tray's menu, or a second launch asking for the window (`POST /show`).
    Tray(TrayAction),
    /// What the ZIP at `link` holds (see [`App::preview_zip`]), or why it could not be listed.
    ZipListed { link: String, result: Result<Vec<ZipEntry>, String> },
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
    /// Peers and upload of a BitTorrent download, from its last snapshot.
    torrent: Option<hyperfetch_core::p2p::TorrentProgress>,
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
    /// Lists the extension's media card sent to download whole, being read: by link, what the
    /// card chose for each of their downloads, and the browser's cookies saved for them.
    media_lists: HashMap<String, (ipc::MediaChoice, Option<PathBuf>)>,

    clipboard_enabled: Arc<AtomicBool>,
    /// Last clipboard text the watcher saw or the app copied itself; never offered again.
    clipboard_seen: Arc<Mutex<String>>,
    /// What the clipboard offers: what the banner shows of it (never a secret), and the text.
    clipboard_banner: Option<(String, String)>,

    queue: DownloadQueue,
    engines: EngineMaker,
    /// Saves the queue in the background; `None` if its thread could not start.
    queue_saver: Option<queue_store::Saver>,
    /// Revision of the queue last handed to the saver.
    queue_saved: u64,
    jobs: HashMap<usize, JobView>,
    /// The download shown on the Downloader tab.
    focused: Option<usize>,
    /// The download selected in the queue list, which Delete removes.
    selected: Option<usize>,
    /// Whether an ffmpeg was found (see [`AppEvent::FfmpegFound`]); None until that is known.
    ffmpeg_found: Option<bool>,
    /// Media downloads started (the id, and whether from zero) while the user has not said whether
    /// ffmpeg may be installed and none is found: they start once the user answers.
    ffmpeg_waiting: Vec<(usize, bool)>,

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
    /// A newer release offered in a banner, until the user dismisses it.
    update: Option<Update>,
    /// The offered update is being downloaded and installed.
    updating: bool,
    /// The task installing it, which closing the window waits for (see `on_exit`).
    install_task: Option<JoinHandle<()>>,
    /// The program to start once the window has closed: the new build an update installed. Read
    /// by `main` after the window closes.
    restart: Arc<Mutex<Option<PathBuf>>>,

    /// The tray icon, while there is one (see `tray::start`).
    tray: Option<tray::Tray>,
    /// The window is hidden in the tray.
    hidden: bool,
    /// Hide the window in the tray once its first frame is up (a start hidden).
    hide_at_start: bool,
    /// The window closes for good: Quit in the tray, an update, the queue done with "Quit the
    /// app" chosen. Closing it otherwise may only hide it (see `Settings::close_to_tray`).
    quitting: bool,
    /// What to do once the queue is done; this session only.
    when_done: WhenDone,
    /// Sleep or shut down at this time, unless cancelled.
    countdown: Option<(WhenDone, Instant)>,
    /// Sleeps or shuts down the computer (see `desktop::power`).
    power: desktop::Power,
    /// Writes launch at startup and the `endo-dl:` link (see `desktop::write`).
    writer: desktop::Writer,
    /// The local time the schedule was last checked at (see `desktop::schedule_due`).
    schedule_seen: Option<(u8, u16)>,
    /// Paused downloads Resume all or the schedule's start resumes, first come first, as the
    /// queue has room for them (see `App::run_scheduler`).
    resuming: Vec<usize>,
    /// The window's taskbar button (Windows), and the progress it shows.
    taskbar: Option<taskbar::Taskbar>,
    taskbar_shown: Option<taskbar::Progress>,
    /// A download failed since the queue last stood still: the taskbar's progress is red.
    batch_failed: bool,
    /// The ZIP link the Add page lists the contents of, and what it holds once read.
    zip_preview: Option<(String, Option<Result<Vec<ZipEntry>, String>>)>,
}

impl App {
    /// The app with its saved settings and queue, its tray icon, the local API and the
    /// background work; hidden in the tray when `minimized` (or the setting says so). It
    /// registers the `endo-dl:` link when it takes `links` (see `desktop::takes_links`).
    fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Handle, restart: Arc<Mutex<Option<PathBuf>>>, minimized: bool, links: bool) -> Self {
        ui::setup(&cc.egui_ctx);
        let settings = Settings::load();
        let (queue, queue_problem) = queue_store::load(&queue_store::path());
        let queue_saver = queue_store::Saver::spawn(queue_store::path())
            .inspect_err(|e| tracing::warn!("Queue changes will only be saved on exit: {}", e))
            .ok();
        let mut app = Self::with(cc.egui_ctx.clone(), rt.clone(), settings, queue, queue_saver);
        app.notice = queue_problem.map(Err);
        app.restart = restart;
        let (tx, ctx) = (app.events_tx.clone(), cc.egui_ctx.clone());
        app.tray = tray::start(
            move |action| {
                if tx.send(AppEvent::Tray(action)).is_ok() {
                    ctx.request_repaint();
                }
            },
            &rt,
        );
        app.taskbar = taskbar::Taskbar::new(TITLE);
        if cfg!(windows) && links {
            app.register_protocol();
        }
        // Written again: the program may have moved.
        if app.settings.launch_at_startup {
            app.set_launch_at_startup(true);
        }
        if minimized || app.settings.start_minimized {
            // eframe shows the window after its first frame: it comes up minimized, and hides
            // in the tray on the next (see `update`).
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            app.hide_at_start = app.tray.is_some();
        }
        // What an earlier update left next to the exe; it no longer runs.
        rt.spawn_blocking(|| {
            updater::cleanup_old();
            updater::refresh_extension();
        });
        if app.settings.check_updates {
            app.check_for_update(false);
        }
        spawn_clipboard_watcher(
            Arc::clone(&app.clipboard_enabled),
            Arc::clone(&app.clipboard_seen),
            app.events_tx.clone(),
            cc.egui_ctx.clone(),
        );
        ipc::spawn(app.events_tx.clone(), cc.egui_ctx.clone(), &rt, desktop::data_tag(&settings::data_dir()));
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
            media_lists: HashMap::new(),
            clipboard_enabled,
            clipboard_seen: Arc::new(Mutex::new(String::new())),
            clipboard_banner: None,
            queue_saved: queue.revision(),
            queue,
            engines: shared_clients(),
            queue_saver,
            jobs: HashMap::new(),
            focused: None,
            selected: None,
            ffmpeg_found: None,
            ffmpeg_waiting: Vec::new(),
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
            update: None,
            updating: false,
            install_task: None,
            restart: Arc::default(),
            tray: None,
            hidden: false,
            hide_at_start: false,
            quitting: false,
            when_done: WhenDone::Nothing,
            countdown: None,
            power: Box::new(desktop::power),
            writer: Box::new(desktop::write),
            schedule_seen: None,
            resuming: Vec::new(),
            taskbar: None,
            taskbar_shown: None,
            batch_failed: false,
            zip_preview: None,
        }
    }

    /// Lists what the ZIP at `url` holds without downloading it (see `zipdir::list_zip`), through
    /// the proxy and with the cookies file and User-Agent of the settings; the Add page shows it.
    /// It gives up after [`ZIP_PREVIEW_LIMIT`].
    fn preview_zip(&mut self, url: Url) {
        let link = url.to_string();
        self.zip_preview = Some((link.clone(), None));
        let options = self.settings.download_options(std::slice::from_ref(&url), "", "");
        self.spawn_event(async move {
            let listed = async {
                let options = options?;
                let client = unblock(move || build_client(&options)).await??;
                hyperfetch_core::zipdir::list_zip(&client, &url).await
            };
            let result = tokio::time::timeout(ZIP_PREVIEW_LIMIT, listed).await.unwrap_or_else(|_| {
                Err(format!("Gave up listing the ZIP: the server took over {} seconds", ZIP_PREVIEW_LIMIT.as_secs()))
            });
            AppEvent::ZipListed { link, result }
        });
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

    /// Queues each link of `text` when it is a paste with a password, key or sign-in, or a copied
    /// request (see `paste::parse`), with what the paste gave that link; the form's Authorization
    /// header `auth` wins over a pasted one. A link to read first is read as `origin` says, with
    /// its pasted password (a Gofile folder's) and Authorization header alone. None when `text` is neither: it is any other input.
    /// Else the ids queued and a note per download naming what of the paste it uses (never a
    /// value), or why nothing was added.
    fn add_pasted(&mut self, text: &str, checksum: &str, auth: &str, origin: Origin) -> Option<Result<(Vec<usize>, Vec<String>), String>> {
        let links = match paste::parse(text)? {
            Ok(links) => links,
            Err(e) => return Some(Err(e)),
        };
        if links.len() > 1 && !checksum.trim().is_empty() {
            return Some(Err(SINGLE_CHECKSUM.to_string()));
        }
        let (reads, files): (Vec<_>, Vec<_>) = links.into_iter().partition(|link| ingest::needs_reading(link.url.as_str()));
        // All checked before any is queued.
        let queued = files.iter().map(|link| {
            let task = ingest::link_task(&[link.url.as_str()])?;
            let mut options = task_options(&self.settings, &task, checksum, auth)?;
            paste::apply(link, &mut options);
            Ok((task, options))
        });
        let queued = match queued.collect::<Result<Vec<_>, String>>() {
            Ok(queued) => queued,
            Err(e) => return Some(Err(e)),
        };
        for link in reads {
            let input = link.url.to_string();
            let read = read_listing(&self.settings, input.clone(), Some(&link));
            // A queue line is the one the link is on, put back should reading it fail.
            let origin = match origin {
                Origin::QueueLine(_) => {
                    let at = format!("{}{}", link.url.host_str().unwrap_or_default(), link.url.path());
                    Origin::QueueLine(text.lines().position(|line| line.contains(&at)).map_or(1, |n| n + 1))
                }
                origin => origin,
            };
            let auth = if auth.trim().is_empty() { link.secrets.auth_header.unwrap_or_default() } else { auth.to_string() };
            self.start_reading(input, origin, checksum.trim().to_string(), auth, read);
        }
        let notes = files.iter().filter_map(|link| link.secrets.note(&link.url)).collect();
        Some(Ok((queued.into_iter().map(|(task, options)| queue_task(&mut self.queue, task, options)).collect(), notes)))
    }

    /// Reads the downloads a .metalink, .meta4 or .torrent, or a folder, feed or playlist link
    /// lists (see `ingest::needs_reading`) off the UI thread and adds them as `origin` says. A
    /// document already being read is not read again.
    fn read_document(&mut self, input: String, origin: Origin, checksum: String, auth: String) {
        let read = read_listing(&self.settings, input.clone(), None);
        self.start_reading(input, origin, checksum, auth, read);
    }

    /// [`App::read_document`] with `read`, which reads `input`. A video link that names its
    /// playlist asks at once whether the video or the whole playlist is meant, while the playlist
    /// is read (see [`playlist_video`]); not one whose playlist the media card chose.
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
        let prompt = playlist_video(&input).filter(|_| !self.media_lists.contains_key(&input)).map(|video| Listing {
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
                self.media_lists.remove(&input);
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
        let media = self.media_lists.remove(input);
        let added = result.and_then(|tasks| queue_listed(&mut self.queue, &self.settings, tasks, checksum, auth, media.as_ref()));
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
            Answer::Cancel => {
                self.media_lists.remove(&input);
                return;
            }
            Answer::Video => video.into_iter().collect(),
            Answer::All => tasks.unwrap_or_default(),
        };
        self.add_listing(origin, &input, &checksum, &auth, Ok(chosen));
    }

    /// Adds the downloads of a .metalink, .meta4 or .torrent file dropped on the window, or of a
    /// .txt list of links (read off the UI thread, see [`App::add_dropped_text`]).
    fn add_dropped(&mut self, path: PathBuf) {
        let input = path.to_string_lossy().into_owned();
        if ingest::needs_reading(&input) {
            self.read_document(input, Origin::Dropped, String::new(), String::new());
        } else if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("txt")) {
            self.spawn_event(async move {
                let read = tokio::fs::read(&path).await.map_err(|e| e.to_string()).and_then(|bytes| ingest::decode_text(&bytes));
                AppEvent::DroppedText(read.map_err(|e| format!("Could not read {}: {}", path.display(), e)))
            });
        } else {
            self.notice = Some(Err(format!("{} is not a .metalink, .meta4, .torrent or .txt file", path.display())));
        }
    }

    /// Queues the links of a dropped .txt as the queue input's (see [`App::add_lines`]), without
    /// the form's checksum and Authorization header; what is refused waits in the queue input,
    /// after what was typed there, for correcting.
    fn add_dropped_text(&mut self, text: Result<String, String>) {
        self.notice = None;
        let (kept, error, _) = match text {
            Ok(text) => self.add_lines(&text, "", ""),
            Err(e) => return self.notice = Some(Err(e)),
        };
        if !kept.is_empty() {
            if !self.queue_input.is_empty() {
                self.queue_input.push('\n');
            }
            self.queue_input.push_str(&kept);
        }
        if let Some(error) = error {
            self.queue_error = Some(error);
            self.notice = Some(Err("Some of the dropped list could not be added: see the Queue page".to_string()));
        } else {
            self.notice.get_or_insert_with(|| Ok("Added the dropped list to the queue".to_string()));
        }
    }

    /// Adds a download, starts it immediately and shows it on the Downloader tab. A file that is
    /// already downloading is shown instead of being started twice. A .metalink, .meta4 or
    /// .torrent, or a folder, feed or playlist link, is read first; the first download it lists
    /// starts, the others wait in the queue.
    fn download_now(&mut self, text: &str, checksum: &str, auth: &str) {
        self.tab = Tab::Downloader;
        // The first link of a paste with secrets starts, the others wait; the form clears as
        // it starts (it held the secrets).
        match self.add_pasted(text, checksum, auth, Origin::Form) {
            Some(Ok((ids, mut notes))) => {
                self.form_error = None;
                let Some(&first) = ids.first() else { return };
                self.start_added(first);
                if ids.len() > 1 {
                    notes.push(format!("Started the first of {} downloads; the others wait in the queue", ids.len()));
                }
                if self.notice.is_none() && !notes.is_empty() {
                    self.notice = Some(Ok(notes.join("\n")));
                }
                return;
            }
            Some(Err(e)) => return self.form_error = Some(e),
            None => {}
        }
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
        let text = std::mem::take(&mut self.queue_input);
        let (checksum, auth) = (self.checksum_input.trim().to_string(), self.auth_input.clone());
        let (kept, error, added) = self.add_lines(&text, &checksum, &auth);
        (self.queue_input, self.queue_error) = (kept, error);
        if added {
            self.checksum_input.clear();
        }
    }

    /// Queues the downloads of `text` (the queue input, a dropped .txt): a paste with secrets or
    /// copied requests as [`App::add_pasted`] does, else each non-empty line as one download. The
    /// text to keep for correcting (all of a paste refused, the lines refused), why, and whether
    /// anything was added.
    fn add_lines(&mut self, text: &str, checksum: &str, auth: &str) -> (String, Option<String>, bool) {
        // A link to read is put back on its line should reading it fail.
        match self.add_pasted(text, checksum, auth, Origin::QueueLine(1)) {
            Some(Ok((ids, notes))) => {
                // What the paste leaves that is an input of its own (a magnet, a local .torrent).
                for (n, line) in paste::other_inputs(text) {
                    self.read_document(line.to_string(), Origin::QueueLine(n), checksum.to_string(), auth.to_string());
                }
                // With links to read alone, "Reading…" stays.
                if !ids.is_empty() {
                    let added = format!("Added {} download(s) to the queue", ids.len());
                    self.notice = Some(Ok(std::iter::once(added).chain(notes).collect::<Vec<_>>().join("\n")));
                }
                return (String::new(), None, true);
            }
            Some(Err(e)) => return (text.to_string(), Some(e), false),
            None => {}
        }
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if lines.is_empty() {
            return (String::new(), Some("Enter one download per line".to_string()), false);
        }
        if lines.len() > 1 && !checksum.is_empty() {
            return (text.to_string(), Some(SINGLE_CHECKSUM.to_string()), false);
        }
        let mut errors = Vec::new();
        let mut rejected = Vec::new();
        for (n, &line) in lines.iter().enumerate() {
            if ingest::needs_reading(line) {
                self.read_document(line.to_string(), Origin::QueueLine(n + 1), checksum.to_string(), auth.to_string());
            } else if let Err(e) = self.add_download(line, checksum, auth) {
                errors.push(format!("Line {}: {}", n + 1, e));
                rejected.push(line);
            }
        }
        // Keep the lines that failed so they can be corrected.
        (rejected.join("\n"), (!errors.is_empty()).then(|| errors.join("\n")), errors.len() < lines.len())
    }

    /// Starts (or resumes) the engine for a queue item. `fresh` first deletes the item's partial
    /// files so the download starts from zero. Returns false if nothing was started. A media
    /// download waits while the user has not said whether ffmpeg may be installed and none is
    /// found (see [`App::answer_ffmpeg`]), and then starts as it was asked to last (a Resume
    /// after a Start Over resumes); it installs ffmpeg as the user said last, whatever the
    /// setting was when it was queued.
    fn start_job(&mut self, id: usize, fresh: bool) -> bool {
        if self.waits_for_ffmpeg_answer(id) {
            self.ffmpeg_waiting.retain(|&(waiting, _)| waiting != id);
            self.ffmpeg_waiting.push((id, fresh));
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
        self.settings.fill_keys(&mut options);
        if options.post.sort {
            // As they are now, for every download that sorts (see `Settings::categories`).
            hyperfetch_core::postprocess::set_categories(self.settings.sort_categories());
        }
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
        let media = |item: &QueueItem| {
            item.options.media_preset.is_some() || item.options.dash || item.urls.iter().any(media::is_supported_media_site)
        };
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

    /// Resumes a restored download with the Authorization header from the form, which is never
    /// saved; `user:pass`, or the link with its `user:pass@`, stands for its Basic credentials.
    fn resume_with_auth(&mut self, id: usize) {
        let auth = paste::authorization(&self.auth_input);
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
            view.torrent.clone_from(&snapshot.torrent);
            view.mirror_speeds = snapshot.mirror_speeds;
            if !snapshot.chunks.is_empty() {
                view.chunks = snapshot.chunks;
            }
        }
    }

    /// Starts the paused downloads waiting to resume (see `App::resuming`), then queued ones,
    /// while fewer than the configured number are running; none while the window waits to close.
    /// One that cannot start yet (a video waiting for the answer about ffmpeg, a file under
    /// repair) holds up none after it.
    fn run_scheduler(&mut self) {
        if !self.settings.auto_run_queue || self.closing {
            return;
        }
        let max = self.settings.max_concurrent.max(1);
        while self.queue.active_count() < max && !self.resuming.is_empty() {
            let id = self.resuming.remove(0);
            if self.queue.get_item(id).is_some_and(|item| item.status == QueueItemStatus::Paused) {
                self.start_job(id, false);
            }
        }
        let mut held = HashSet::new();
        while let Some(id) = self.queue.next_to_start(max, |id| held.contains(&id)) {
            if !self.start_job(id, false) {
                held.insert(id);
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
                    let running = view.running.take();
                    // The last snapshot (with the post-processing notes) may not have been drawn yet.
                    if let Some(snapshot) = running.as_ref().and_then(|r| lock(&r.snapshot).take()) {
                        self.queue.apply_snapshot(id, &snapshot);
                    }
                    if let (Some(_), Some(started)) = (running, view.started) {
                        view.elapsed = started.elapsed();
                    }
                    // No snapshot tells of its seeding any more.
                    view.torrent = None;
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
                // One that ended by itself (not paused) may have been the queue's last.
                let ended = self.queue.get_item(id).map(|item| item.status.clone());
                self.batch_failed |= matches!(ended, Some(QueueItemStatus::Failed(_)));
                if matches!(ended, Some(QueueItemStatus::Completed | QueueItemStatus::Failed(_))) && self.queue_done() {
                    self.queue_finished();
                }
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
                    Dialog::CategoryFolder(n) => {
                        if let Some(category) = self.settings.categories.get_mut(n) {
                            category.folder = Some(path);
                        }
                    }
                }
            }
            AppEvent::Pasted(result) => match result {
                Ok(text) => self.url_input = text.trim().to_string(),
                Err(e) => self.form_error = Some(format!("Could not read the clipboard: {}", e)),
            },
            AppEvent::ClipboardLink(shown, text) => {
                if self.clipboard_enabled.load(Ordering::Relaxed) && self.url_input.trim() != text {
                    self.clipboard_banner = Some((shown, text));
                }
            }
            AppEvent::DroppedText(text) => self.add_dropped_text(text),
            AppEvent::FfmpegFound(found) => {
                self.ffmpeg_found = Some(found);
                // Nothing to ask: what waited to know it starts.
                if found {
                    self.start_waiting();
                }
            }
            AppEvent::RemoteAdd(payload) => self.add_remote(payload),
            AppEvent::Recorded(finished) => {
                let save_dir = PathBuf::from(self.settings.save_dir.trim());
                let recovered = finished.recovered;
                // One an earlier run left says so, or the notice would come out of nowhere.
                let said = move |notice: String| {
                    if recovered {
                        format!("Recovered a recording the app was taking when it last closed: {}", notice)
                    } else {
                        notice
                    }
                };
                self.spawn_event(async move {
                    let merged = unblock(move || recording::merge(&finished, &save_dir, media::find_ffmpeg_path().as_deref())).await;
                    AppEvent::Merged(merged.and_then(|notice| notice).map(said).map_err(said))
                });
            }
            AppEvent::Merged(notice) => self.notice = Some(notice),
            AppEvent::UpdateChecked { result, manual } => match result {
                Ok(Some(update)) => self.update = Some(update),
                Ok(None) if manual => {
                    self.notice = Some(Ok(format!("You have the latest version ({}).", env!("CARGO_PKG_VERSION"))))
                }
                Err(e) if manual => self.notice = Some(Err(format!("Could not check for updates: {}", e))),
                Err(e) => tracing::warn!("Could not check for updates: {}", e),
                Ok(None) => {}
            },
            AppEvent::UpdateInstalled(result) => {
                self.updating = false;
                self.install_task = None;
                match result {
                    // Closed the way the user closes it: the queue is saved and live recordings
                    // finish first; `main` then starts the new build.
                    Ok(installed) => {
                        let version = self.update.take().map(|update| update.version).unwrap_or_default();
                        self.notice = Some(Ok(format!("Updated to {}; restarting...", version)));
                        *lock(&self.restart) = Some(installed.own);
                        self.quit();
                    }
                    Err(e) => self.notice = Some(Err(format!("Could not update: {}", e))),
                }
            }
            AppEvent::Tray(action) => self.tray_action(action),
            // Only for the link still asked about.
            AppEvent::ZipListed { link, result } => {
                if let Some((_, listed)) = self.zip_preview.as_mut().filter(|(asked, _)| *asked == link) {
                    *listed = Some(result);
                }
            }
        }
    }

    /// Adds a download the browser extension sent, with what the browser sent
    /// to fetch it applied to this download alone (see [`App::add_browser_download`]). It starts
    /// now when the download shown is not running and the URL box is empty, else it waits in the
    /// queue. A link to read first (see `ingest::needs_reading`) is read as one typed in is,
    /// without the browser's request; not one sent with a file name, which is the media the
    /// extension saw the page fetch, whatever its path looks like (`/feed/index.m3u8`), nor one
    /// sent with the playlist the page built, of which it is only the base. A link the media card
    /// sent that is only a list is read whatever it chose; a video link that names its playlist
    /// too is the one video, unless the card chose the list.
    fn add_remote(&mut self, payload: ipc::RemoteAddPayload) {
        if !payload.urls.is_empty() {
            return self.add_remote_batch(&payload);
        }
        let url = payload.url.trim().to_string();
        if url.is_empty() {
            return;
        }
        // The Anna's Archive resolver sends these to Anna's Archive: only its own cookies go there.
        let annas = Url::parse(&url).is_ok_and(|u| hyperfetch_core::resolver::AnnaArchiveResolver::can_handle(&u));
        if let Some(cookies) = payload.cookies.as_deref().filter(|c| annas && !c.trim().is_empty()) {
            std::env::set_var("ANNAS_ARCHIVE_COOKIE", cookies.trim());
            std::env::set_var("HYPERFETCH_COOKIES", cookies.trim());
        }
        let start_now = !self.focused_item().is_some_and(|item| item.status.is_active()) && self.url_input.trim().is_empty();
        let link = Url::parse(&url).ok();
        // The whole list is read as the link it is: YouTube refuses `playlist?list=` for its mixes.
        let reads = match &payload.media {
            Some(choice) => ingest::needs_reading(&url) && (choice.playlist || !link.as_ref().is_some_and(media::video_in_playlist)),
            None => ingest::needs_reading(&url) && payload.file_name().is_none() && payload.playlist.is_none(),
        };
        // Each download of a media list the card sent is made as it chose, with the browser's
        // cookies; a feed or folder as the Settings say.
        if let (Some(choice), Some(at)) = (&payload.media, link.as_ref().filter(|at| reads && media::lists(at))) {
            let cookies = payload.cookies_file(at).and_then(|text| ipc::save_cookies(at, &text));
            self.media_lists.insert(url.clone(), (choice.clone(), cookies));
        }
        let shown = ingest::truncate_chars(&url, 60);
        if reads {
            if start_now {
                self.download_now(&url, "", "");
                self.notice = Some(Ok(format!("Started remote download: {}", shown)));
            } else {
                self.read_document(url, Origin::Dropped, String::new(), String::new());
            }
            return;
        }
        match self.add_browser_download(&url, &payload) {
            Ok(id) if start_now => {
                self.tab = Tab::Downloader;
                self.start_added(id);
                // Unless it says the file is downloading already.
                self.notice.get_or_insert_with(|| Ok(format!("Started remote download: {}", shown)));
            }
            Ok(id) => self.notice = Some(Ok(format!("Added remote download #{} to queue: {}", id, shown))),
            Err(e) => self.notice = Some(Err(format!("Could not add remote download: {}", e))),
        }
    }

    /// Queues every link of a batch the extension's link grabber sent (see
    /// `ipc::RemoteAddPayload::batch`); the queue runs them. A link to read first is read as a
    /// dropped file is.
    fn add_remote_batch(&mut self, payload: &ipc::RemoteAddPayload) {
        let (mut added, mut errors) = (0, Vec::new());
        for link in payload.batch() {
            if ingest::needs_reading(&link.url) {
                self.read_document(link.url.clone(), Origin::Dropped, String::new(), String::new());
            } else if let Err(e) = self.add_browser_download(&link.url, &link) {
                errors.push(format!("{}: {}", ingest::truncate_chars(&link.url, 60), e));
            } else {
                added += 1;
            }
        }
        let shown = format!("Added {} download(s) from the browser to the queue", added);
        self.notice = Some(match errors.len() {
            0 => Ok(shown),
            // A few reasons are enough; a thousand would bury the notice.
            n => Err(format!("{}; {} refused:\n{}", shown, n, errors[..n.min(3)].join("\n"))),
        });
    }

    /// Queues the download of `url` as the browser asked for it: under its file name, with its
    /// referer (over the Settings one), User-Agent and other headers, its Authorization header for
    /// the hosts of `url` alone, its cookies (see `ipc::RemoteAddPayload::cookies_file`), its
    /// choice of MP4 for an HLS stream (over the Settings one), whether it is DASH, the tallest
    /// video it wants (over the media quality) and the playlist the page built. The Settings are
    /// not changed.
    fn add_browser_download(&mut self, url: &str, payload: &ipc::RemoteAddPayload) -> Result<usize, String> {
        let mut task = ingest::link_task(&[url])?;
        task.name = payload.file_name().map(PathBuf::from).or(task.name);
        let (headers, auth) = payload.request_headers();
        let mut options = task_options(&self.settings, &task, "", auth.as_deref().unwrap_or_default())?;
        options.referer = payload.referer().or(options.referer);
        options.headers = headers;
        options.hls = payload.hls;
        options.hls_to_mp4 = payload.mp4.unwrap_or(options.hls_to_mp4);
        options.dash = payload.dash;
        options.height = payload.height.filter(|&height| height > 0);
        options.playlist_text = payload.playlist_text().map(str::to_string);
        let saved = Url::parse(url).ok().and_then(|url| ipc::save_cookies(&url, &payload.cookies_file(&url)?));
        if let Some(path) = saved {
            options.cookies_path = Some(path);
            // The cookies the browser sent win over the Settings' browser, for yt-dlp too.
            options.browser_cookies = None;
        }
        if let Some(choice) = &payload.media {
            choose_media(&mut options, choice);
        }
        Ok(queue_task(&mut self.queue, task, options))
    }

    fn open_dialog(&mut self, dialog: Dialog, frame: &eframe::Frame) {
        let picker = rfd::AsyncFileDialog::new().set_parent(frame);
        self.dialog_open = true;
        let picked = move |handle: Option<rfd::FileHandle>| AppEvent::Picked(dialog, handle.map(|h| h.path().to_path_buf()));
        match dialog {
            Dialog::SaveDir | Dialog::CategoryFolder(_) => {
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

    // ---- updates -------------------------------------------------------------------------

    /// Looks for a newer release through the proxy setting; `manual` reports the outcome even
    /// when there is none (see [`AppEvent::UpdateChecked`]).
    fn check_for_update(&self, manual: bool) {
        let proxy = self.settings.list_options().proxy;
        self.spawn_event(async move { AppEvent::UpdateChecked { result: updater::check(proxy.as_deref()).await, manual } });
    }

    /// Downloads, verifies and installs the update offered, once; the app restarts when it is in.
    fn install_update(&mut self) {
        let Some(update) = self.update.clone().filter(|_| !self.updating) else { return };
        self.updating = true;
        let proxy = self.settings.list_options().proxy;
        self.install_task = Some(self.spawn_event(async move {
            AppEvent::UpdateInstalled(updater::install(proxy.as_deref(), &update, updater::Program::Gui).await)
        }));
    }

    // ---- desktop: tray, startup, schedule, when done --------------------------------------

    /// The desktop's part of a frame: hiding a window started hidden, the countdown, the
    /// schedule (checked as each minute begins) and the taskbar's progress. It asks only for the
    /// frames these need.
    fn desktop_frame(&mut self, ctx: &egui::Context) {
        if self.hide_at_start && ctx.cumulative_pass_nr() > 0 {
            self.hide_at_start = false;
            self.hide_window();
        }
        // Brought back from the tray another way (Alt+Tab): its taskbar button comes back too.
        if cfg!(windows) && self.hidden && ctx.input(|i| i.viewport().minimized) == Some(false) {
            self.show_window();
        }
        self.tick_countdown(Instant::now());
        if self.countdown.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if self.settings.schedule.enabled {
            self.run_schedule(desktop::local_now());
            ctx.request_repaint_after(desktop::until_next_minute());
        } else {
            // Turned on again, it starts from then, not from when it was turned off.
            self.schedule_seen = None;
        }
        if self.queue.active_count() == 0 {
            self.batch_failed = false;
        }
        if let Some(taskbar) = &self.taskbar {
            let progress = self.overall_progress();
            if self.taskbar_shown != Some(progress) && taskbar.set_progress(progress) {
                self.taskbar_shown = Some(progress);
            }
        }
    }

    fn tray_action(&mut self, action: TrayAction) {
        match action {
            TrayAction::Show => self.show_window(),
            TrayAction::PauseAll => {
                self.pause_all();
            }
            TrayAction::ResumeAll => self.resume_all(),
            TrayAction::StartQueue => self.settings.auto_run_queue = true,
            TrayAction::StopQueue => self.settings.auto_run_queue = false,
            TrayAction::Quit => self.quit(),
        }
    }

    /// Brings the window back from the tray (or from minimized) to the front.
    fn show_window(&mut self) {
        if let Some(taskbar) = self.taskbar.as_ref().filter(|_| self.hidden) {
            taskbar.show_button(true);
        }
        // A taskbar button put back shows no progress until it is set again.
        self.taskbar_shown = None;
        self.hidden = false;
        self.hide_at_start = false;
        for command in [egui::ViewportCommand::Visible(true), egui::ViewportCommand::Minimized(false), egui::ViewportCommand::Focus] {
            self.ctx.send_viewport_cmd(command);
        }
    }

    /// Hides the window in the tray; downloads go on. On Windows it is minimized and its taskbar
    /// button taken off, as a hidden window there gets no frames, so nothing would run. The
    /// settings are saved, as the system may end the app there without closing it (a shutdown,
    /// restart or logoff).
    fn hide_window(&mut self) {
        self.save_settings();
        self.hidden = true;
        if !cfg!(target_os = "macos") {
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        if cfg!(windows) {
            if let Some(taskbar) = &self.taskbar {
                taskbar.show_button(false);
            }
        } else {
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    /// Closes the app the way closing the window does when it is not kept in the tray.
    fn quit(&mut self) {
        self.quitting = true;
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Stops the queue and pauses every running download; the ones it paused, and those that
    /// were waiting to resume.
    fn pause_all(&mut self) -> Vec<usize> {
        self.settings.auto_run_queue = false;
        let running: Vec<usize> =
            self.queue.items().iter().filter(|item| item.status == QueueItemStatus::Downloading).map(|item| item.id).collect();
        for &id in &running {
            self.pause_job(id);
        }
        let mut paused = std::mem::take(&mut self.resuming);
        paused.extend(running);
        paused
    }

    /// Starts the queue and resumes every paused download, as many at once as the queue runs.
    fn resume_all(&mut self) {
        self.settings.auto_run_queue = true;
        let paused = self.queue.items().iter().filter(|item| item.status == QueueItemStatus::Paused).map(|item| item.id);
        self.resuming.extend(paused);
        self.run_scheduler();
    }

    /// Starts or pauses the queue as the schedule says for the local time `now` (see
    /// `desktop::schedule_due`): its start resumes what its stop paused, also after a restart.
    fn run_schedule(&mut self, now: (u8, u16)) {
        let last = self.schedule_seen;
        // A clock set back (the hour that repeats as summer time ends) takes no minute twice.
        self.schedule_seen = Some(last.filter(|&(day, minute)| day == now.0 && minute > now.1).unwrap_or(now));
        match desktop::schedule_due(&self.settings.schedule, last, now) {
            Some(true) => {
                self.settings.auto_run_queue = true;
                self.resuming.extend(std::mem::take(&mut self.settings.schedule_paused));
                self.run_scheduler();
            }
            Some(false) => {
                let paused = self.pause_all();
                self.settings.schedule_paused.extend(paused);
            }
            None => return,
        }
        self.save_settings();
    }

    /// Nothing runs, and nothing queued will start by itself.
    fn queue_done(&self) -> bool {
        let waiting = self.settings.auto_run_queue && self.queue.items().iter().any(|item| item.status == QueueItemStatus::Queued);
        self.queue.active_count() == 0 && !waiting
    }

    /// Does what the user chose for when the queue is done, once: quits, or counts down to
    /// sleeping or shutting down, the window shown to cancel it.
    fn queue_finished(&mut self) {
        match std::mem::take(&mut self.when_done) {
            WhenDone::Nothing => {}
            WhenDone::Quit => self.quit(),
            action => {
                self.countdown = Some((action, Instant::now() + desktop::COUNTDOWN));
                self.show_window();
            }
        }
    }

    /// Sleeps or shuts down once the countdown is over at `now`, the settings saved first (the
    /// app may not get to save them on its way out). A download started meanwhile stops the
    /// countdown, the choice kept for when the queue is done again.
    fn tick_countdown(&mut self, now: Instant) {
        let Some((action, at)) = self.countdown else { return };
        if !self.queue_done() {
            self.countdown = None;
            self.when_done = action;
            return;
        }
        if now < at {
            return;
        }
        self.countdown = None;
        self.save_settings();
        if let Err(e) = (self.power)(action) {
            self.notice = Some(Err(format!("Could not {}: {}", action.name().to_lowercase(), e)));
        }
    }

    /// Saves the settings now: on the way out, and wherever the app may be ended without one.
    fn save_settings(&mut self) {
        self.settings.clipboard_watch = self.clipboard_enabled.load(Ordering::Relaxed);
        if let Err(e) = self.settings.save() {
            tracing::warn!("Failed to save settings: {}", e);
        }
    }

    /// Registers launch at startup (or removes it): the setting changes, and is saved at once,
    /// only when that worked (so that the next start does not undo it), else a notice says why.
    fn set_launch_at_startup(&mut self, on: bool) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                self.notice = Some(Err(format!("Could not find this program to start it with the system: {}", e)));
                return;
            }
        };
        let (place, value) = desktop::startup_entry(&exe, &desktop::startup_root());
        match (self.writer)(&place, on.then_some(value.as_str())) {
            Ok(()) => {
                self.settings.launch_at_startup = on;
                self.save_settings();
            }
            Err(e) => self.notice = Some(Err(format!("Could not change launch at startup: {}", e))),
        }
    }

    /// Makes `endo-dl:` links start this program (Windows; see `desktop::protocol_entries`).
    fn register_protocol(&self) {
        let Ok(exe) = std::env::current_exe() else { return };
        for (place, value) in desktop::protocol_entries(&exe) {
            if let Err(e) = (self.writer)(&place, Some(&value)) {
                tracing::warn!("Could not register endo-dl: links: {}", e);
                return;
            }
        }
    }

    /// What the taskbar button shows: nothing while no download runs; else the share of their
    /// bytes done, red when one failed since the queue last stood still, yellow when all that
    /// run are pausing or stalled.
    fn overall_progress(&self) -> taskbar::Progress {
        let running: Vec<&QueueItem> = self.queue.items().iter().filter(|item| item.status.is_active()).collect();
        if running.is_empty() {
            return taskbar::Progress::None;
        }
        let (done, total) = running
            .iter()
            .filter(|item| item.total_bytes > 0)
            .fold((0u64, 0u64), |(done, total), item| (done + item.downloaded_bytes.min(item.total_bytes), total + item.total_bytes));
        let share = (done.saturating_mul(1000) / total.max(1)) as u16;
        if self.batch_failed {
            taskbar::Progress::Error(share)
        } else if running.iter().all(|item| item.status == QueueItemStatus::Pausing || self.stalled_for(item.id).is_some()) {
            taskbar::Progress::Paused(share)
        } else {
            taskbar::Progress::Normal(share)
        }
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
        self.desktop_frame(ctx);
        // Kept in the tray, the window only hides. Asked to close, the window first stays open
        // for the live recordings to finish; asked again, it closes at once.
        let close = ctx.input(|i| i.viewport().close_requested());
        if close && self.settings.close_to_tray && self.tray.is_some() && !self.quitting && !self.closing {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window();
        } else if close && !self.closing && stop_recordings(&mut self.queue, &self.jobs) {
            self.closing = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.closing && !self.jobs.values().any(JobView::records) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.run_scheduler();

        ui::render(self, ctx);
        ipc::set_proxy(&self.settings.proxy);

        if let Some(dialog) = self.pending_dialog.take() {
            self.open_dialog(dialog, frame);
        }
        if self.queue.revision() != self.queue_saved {
            self.queue_saved = self.queue.revision();
            if let Some(saver) = &self.queue_saver {
                saver.save(self.queue.clone());
            }
        }
        // Background work wakes the UI when it has news, and animations ask for their own frames;
        // otherwise only the once-a-second clocks (elapsed time, stall timer) need frames.
        if self.queue.active_count() > 0 || self.verifying || self.repair.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    /// Stops every download so it saves its resume state (and kills yt-dlp), waiting briefly,
    /// then saves the queue. Live recordings have finished by now, unless the user closed the
    /// window a second time while they did (see [`stop_recordings`]).
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_settings();
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
        // An update swapping the exes must not be cut off halfway; one still downloading is
        // dropped, which leaves nothing changed.
        // ponytail: an install whose swap starts at the end of the grace can still be cut off;
        // wait for the swap itself if that ever shows.
        tasks.extend(self.install_task.take());
        if !wait_for_tasks(&self.rt, tasks, EXIT_GRACE) {
            tracing::warn!("Some downloads did not stop within {}s", EXIT_GRACE.as_secs());
        }
        // Torrents still seeding stop with the app; their files stay.
        let stopped = self.rt.block_on(async { tokio::time::timeout(EXIT_GRACE, hyperfetch_core::p2p::shutdown()).await });
        if stopped.is_err() {
            tracing::warn!("The BitTorrent session did not stop within {}s", EXIT_GRACE.as_secs());
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
    if task.from_document {
        options.referer = None;
    } else if options.referer.is_none() {
        options.referer = task.referer.clone();
    }
    for part in [&task.folder, &task.name].into_iter().flatten() {
        options.output_path = options.output_path.map(|path| path.join(part));
    }
    // An unnamed torrent is saved in the folder itself (see `DownloadEngine::save_dir`).
    if task.p2p && task.name.is_none() {
        options.output_path = options.output_path.map(|path| path.join(""));
    }
    options.media_name = task.media_name.clone();
    options.metalink_pieces = task.pieces.clone();
    options.archive_lines = task.archive.clone();
    options.bittorrent = task.p2p;
    options.post.sort &= !task.keeps_its_place();
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

/// Queues the downloads a .metalink, .meta4 or .torrent lists; nothing if one is refused. Those
/// of a list the extension's media card sent (see `App::media_lists`) each as it chose, with the
/// browser's cookies.
fn queue_listed(
    queue: &mut DownloadQueue,
    settings: &Settings,
    tasks: Vec<Task>,
    checksum: &str,
    auth: &str,
    media: Option<&(ipc::MediaChoice, Option<PathBuf>)>,
) -> Result<Vec<usize>, String> {
    let options = document_options(settings, &tasks, checksum, auth)?;
    let queued = tasks.into_iter().zip(options).map(|(task, mut options)| {
        if let Some((choice, cookies)) = media {
            choose_media(&mut options, choice);
            if let Some(path) = cookies {
                options.cookies_path = Some(path.clone());
                options.browser_cookies = None;
            }
        }
        queue_task(queue, task, options)
    });
    Ok(queued.collect())
}

/// `options` as the extension's media card chose, over the Settings: the quality (a preset, or
/// the tallest video), the container, subtitles, clips, SponsorBlock, and recording a live
/// stream from its start (which "From now" cannot turn off when the Settings turn it on).
fn choose_media(options: &mut DownloadOptions, choice: &ipc::MediaChoice) {
    use media::MediaQualityPreset::{AudioM4a, AudioMp3, BestVideoAudio, Fhd1080p, Hd720p};
    let preset = match choice.quality.as_deref() {
        None => None,
        Some("best") => Some(BestVideoAudio),
        Some("1080") => Some(Fhd1080p),
        Some("720") => Some(Hd720p),
        Some("audio-m4a") => Some(AudioM4a),
        Some("audio-mp3") => Some(AudioMp3),
        // 2160, 1440, 480, 360 (see `ipc::MediaChoice::check`).
        Some(height) => {
            options.height = height.parse().ok();
            None
        }
    };
    if preset.is_some() {
        (options.media_preset, options.height) = (preset, None);
    }
    options.merge_format = choice.container.clone().or(options.merge_format.take());
    // The card's None ("") and Off are over the Settings too.
    if let Some(subtitles) = &choice.subtitles {
        options.subtitles = Some(subtitles.clone()).filter(|s| !s.is_empty());
    }
    if let Some(mode) = &choice.sponsorblock {
        options.sponsorblock = Some(mode.clone()).filter(|m| m != "off");
    }
    if !choice.sections.is_empty() {
        options.sections = choice.sections.clone();
    }
    options.live_from_start |= choice.live_from_start;
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
/// lists many) lists, fetching through the proxy setting, and the notes the listing left; the
/// `pasted` link `input` is is read with its password (a share's) and sign-in (a feed's).
fn read_listing(settings: &Settings, input: String, pasted: Option<&paste::PastedLink>) -> impl Future<Output = (Result<Vec<Task>, String>, Vec<String>)> + Send + 'static {
    let (notes, noted) = mpsc::channel();
    let password = pasted.and_then(|link| link.secrets.password.clone());
    let auth = pasted.and_then(|link| Some((link.url.clone(), link.secrets.auth_header.clone()?)));
    let list = ingest::ListOptions { notes: Some(notes), password, auth, ..read_options(settings, &input) };
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

/// Clients [`cached_client`] keeps. A browser download brings its page's own headers and cookies,
/// and so a key of its own: past this many the cache starts over rather than keep them all.
// ponytail: emptied when full; evict the least recently used if rebuilding clients ever shows.
const MAX_CLIENTS: usize = 16;

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
    if cache.len() >= MAX_CLIENTS {
        cache.clear();
    }
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
    let mut forward = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(snapshot) => {
                    *lock(&slot) = Some(with_check_note(snapshot));
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
    // The last snapshot carries the post-processing notes: let it through first.
    let _ = tokio::time::timeout(Duration::from_millis(500), &mut forward).await;
    forward.abort();

    let path = result?;
    // A folder's (a torrent's, a gallery's) is the snapshots' total, which this keeps.
    let size = tokio::fs::metadata(&path).await.ok().filter(|m| m.is_file()).map(|m| m.len());
    Ok((path, size))
}

/// `snapshot` with whose checksum the finished file matched, when not the user's ("Checked
/// against the site's SHA-256"), among the notes the rows and the download's page show.
fn with_check_note(mut snapshot: EngineSnapshot) -> EngineSnapshot {
    snapshot.notes.extend(snapshot.checked_against.as_ref().map(|whose| format!("Checked against {}", whose)));
    snapshot
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
            if let Some((shown, text)) = util::clipboard_offer(text) {
                if tx.send(AppEvent::ClipboardLink(shown, text)).is_err() {
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Started again to stop a live recording (a debug build has a console), this only does that.
    use tracing_subscriber::fmt::writer::MakeWriterExt;

    hyperfetch_core::media::serve_ctrl_c();
    hyperfetch_core::media::prepare_macos();
    let launch = desktop::Launch::parse(std::env::args_os().skip(1));
    // Before any thread starts, so that everything reading the history or the download archive
    // finds them in the portable data folder.
    let portable = desktop::portable_dir(launch.data_dir.clone(), std::env::current_exe().ok().as_deref());
    let links = desktop::takes_links(portable.is_some());
    if let Some(dir) = &portable {
        std::env::set_var("ENDO_HISTORY_PATH", dir.join("history.json"));
        std::env::set_var("ENDO_ARCHIVE_PATH", dir.join("download-archive.txt"));
    }
    let data = settings::data_dir();
    let log = Mutex::new(desktop::LogFile::new(data.join("logs").join("endo.log"), desktop::LOG_LIMIT));
    let _ = tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr.and(log)).try_init();
    // One app per data folder, which it locks (two started at once): a second launch shows the
    // running one's window and ends, one started hidden (at startup, by a link) just ends. Not
    // the build an update started, which would find the app it replaces still closing.
    let restarted = std::env::var_os(RESTARTED).is_some();
    std::env::remove_var(RESTARTED);
    let instance = desktop::lock_instance(&data).inspect_err(|e| tracing::warn!("Could not lock the data folder: {}", e));
    let held = matches!(instance, Ok(None));
    if !restarted {
        // The app holding the lock may still be starting its local API.
        let deadline = Instant::now() + Duration::from_secs(3);
        let running = loop {
            let port = ipc::running_instance(&ipc::PORTS, &desktop::data_tag(&data));
            if port.is_some() || !held || Instant::now() >= deadline {
                break port;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        if let Some(port) = running {
            if !launch.minimized {
                ipc::show_instance(port);
            }
            return Ok(());
        }
        if held {
            return Ok(());
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let handle = runtime.handle().clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 720.0]).with_min_inner_size([760.0, 520.0]).with_title(TITLE),
        ..Default::default()
    };
    let restart = Arc::new(Mutex::new(None));
    let restart_for_app = Arc::clone(&restart);
    let minimized = launch.minimized;
    let result = eframe::run_native(TITLE, options, Box::new(move |cc| Ok(Box::new(App::new(cc, handle, restart_for_app, minimized, links)))));
    // Downloads were stopped in `on_exit`; leftover blocking work (e.g. hashing for a verify)
    // must not keep the process alive.
    runtime.shutdown_background();
    // An update was installed: the new build takes over, the data folder unlocked for it.
    if let Some(exe) = lock(&restart).take() {
        drop(instance);
        std::env::set_var(RESTARTED, "1");
        if let Err(e) = updater::restart(&exe) {
            tracing::warn!("Could not start the updated app {}: {}", exe.display(), e);
        }
    }
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperfetch_core::state::DownloadState;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal HTTP/1.1 file server with range support (suffix ranges too), sending about 800 KB/s
    /// per connection; the same body for every path.
    pub(crate) async fn serve(body: Arc<Vec<u8>>) -> std::net::SocketAddr {
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
                        match a.parse::<usize>() {
                            Ok(a) => (a, b.parse::<usize>().map_or(last, |b| b.min(last))),
                            Err(_) => (body.len().saturating_sub(b.parse().unwrap()), last),
                        }
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
        for n in 0..2 * MAX_CLIENTS {
            let browser = DownloadOptions { referer: Some(format!("https://page{n}.example/")), ..plain.clone() };
            cached_client(&cache, &browser, build).unwrap();
        }
        assert!(lock(&cache).len() <= MAX_CLIENTS, "one-off keys do not pile up");

        let failing = DownloadOptions { proxy: Some("http://127.0.0.1:10".into()), ..plain };
        assert!(cached_client(&cache, &failing, |_: &DownloadOptions| Err::<u32, _>("bad proxy".to_string())).is_err());
        assert_eq!(cached_client(&cache, &failing, build), Ok(5 + 2 * MAX_CLIENTS as u32), "a failed build is not kept");
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

    /// Points download history, and with it the settings, queue and logs (see
    /// `settings::app_file`), and the download archive at a folder of this test process, once for
    /// every test, so no test touches the user's files or switches the folder while another runs.
    /// Earlier runs' folders go once they are an hour old.
    pub(crate) fn isolate_history() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let temp = std::env::temp_dir();
            let stale = |entry: &std::fs::DirEntry| {
                let age = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
                entry.file_name().to_string_lossy().starts_with("hf-gui-tests-") && age.is_some_and(|age| age.as_secs() > 3600)
            };
            for entry in std::fs::read_dir(&temp).into_iter().flatten().flatten().filter(stale) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
            let dir = temp.join(format!("hf-gui-tests-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("ENDO_HISTORY_PATH", dir.join("history.json"));
            std::env::set_var("ENDO_ARCHIVE_PATH", dir.join("download-archive.txt"));
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
        // A metalink's piece hashes, for repairing only the damaged pieces.
        let pieces = hyperfetch_core::metalink::Pieces { length: 1 << 20, algo: "sha256".into(), hashes: vec!["cd".repeat(32)] };
        let with_pieces = Task { urls: vec![url.clone()], pieces: Some(pieces.clone()), ..Task::default() };
        assert_eq!(task_options(&settings, &with_pieces, "", "").unwrap().metalink_pieces, Some(pieces));

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
        let archive = vec!["feed-file show/ep1.mp3".to_string()];
        let named = Task { urls: urls.clone(), folder: Some("Show".into()), name: Some("ep1.mp3".into()), archive: archive.clone(), ..Task::default() };
        let options = task_options(&settings, &named, "", "").unwrap();
        assert_eq!(options.output_path, Some(PathBuf::from("dl").join("Show").join("ep1.mp3")));
        assert_eq!(options.archive_lines, archive, "the engine adds them to the download archive once it is downloaded");
        // A playlist entry, which yt-dlp names by its title and id.
        let template = "%(title)s [%(id)s].%(ext)s".to_string();
        let unnamed = Task { urls, folder: Some("Show".into()), media_name: Some(template.clone()), ..Task::default() };
        let mut queue = DownloadQueue::new();
        let [id] = queue_listed(&mut queue, &settings, vec![unnamed], "", "", None).unwrap()[..] else { panic!("one download") };
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
        let ids = queue_listed(&mut queue, &settings, vec![file(1), file(2)], "", "Bearer t", None).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|&id| queue.get_item(id).is_some_and(|item| item.names_file && item.target_path.is_none())));
        assert!(ids.iter().all(|&id| queue.get_item(id).is_some_and(|item| item.options.auth_header.is_none())), "a listed host got it");
        // A document its host would not hand over without a login is left to the engine as the
        // link typed, which the header is for; the form's checksum is for the file it lists.
        let link = Task { urls: vec![Url::parse("https://tracker.example/dl/1.torrent").unwrap()], document_itself: true, ..Task::default() };
        let typed = format!("sha256:{}", "cc".repeat(32));
        let [id] = queue_listed(&mut queue, &settings, vec![link], &typed, "Bearer t", None).unwrap()[..] else { panic!("one download") };
        let options = queue.get_item(id).map(|item| (item.options.auth_header.clone(), item.options.expected_checksum.clone()));
        assert_eq!(options, Some((Some("Bearer t".to_string()), None)));
        queue.remove_item(id);
        let refused = Task { checksum: Some("crc32:1".to_string()), ..file(3) };
        assert!(queue_listed(&mut queue, &settings, vec![file(4), refused], "", "", None).is_err());
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
    /// nothing is read or downloaded. Its settings and history are this test process's.
    fn test_app(rt: &tokio::runtime::Runtime, dir: &std::path::Path) -> App {
        isolate_history();
        let settings = Settings { save_dir: dir.to_string_lossy().into_owned(), ..Settings::default() };
        App::with(egui::Context::default(), rt.handle().clone(), settings, DownloadQueue::new(), None)
    }

    /// The first link of each download queued.
    fn queued(app: &App) -> Vec<String> {
        app.queue.items().iter().map(|item| item.urls[0].to_string()).collect()
    }

    /// A paste with secrets queues each link with what the paste gave it, the form's
    /// Authorization header winning, from the form, the queue input and a dropped .txt alike; the notes name what is used, and no secret reaches a note, a notice, a link or
    /// the queue file. A copied POST adds nothing.
    #[test]
    fn pasted_secrets_reach_their_downloads_and_nothing_else() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        let secrets = ["hunter2", "Ym9iOmh1bnRlcjI=", "s3cret-pw", "cookie-secret", "key-secret"];

        let text = "https://bob:hunter2@h.example/a.zip\nhttps://h.example/b.zip\nPassword: s3cret-pw";
        let (ids, notes) = app.add_pasted(text, "", "", Origin::Form).unwrap().unwrap();
        let options = |app: &App, id: usize| app.queue.get_item(id).unwrap().options.clone();
        assert_eq!(queued(&app), ["https://h.example/a.zip", "https://h.example/b.zip"]);
        assert_eq!(options(&app, ids[0]).auth_header.as_deref(), Some("Basic Ym9iOmh1bnRlcjI="));
        assert_eq!(options(&app, ids[1]).password.as_deref(), Some("s3cret-pw"));
        assert!(notes.iter().all(|note| note.starts_with("Using the ") && note.ends_with("from your paste for h.example")), "{notes:?}");

        let typed = app.add_pasted("https://bob:hunter2@h.example/c.zip", "", "Bearer typed", Origin::Form).unwrap().unwrap();
        assert_eq!(options(&app, typed.0[0]).auth_header.as_deref(), Some("Bearer typed"), "the form wins");

        app.queue_input = "curl 'https://c.example/c.bin' -H 'Cookie: sid=cookie-secret' -H 'X-Api-Key: key-secret'".to_string();
        app.add_queue_input();
        assert_eq!((app.queue_input.as_str(), &app.queue_error), ("", &None), "the queue input clears");
        let headers = options(&app, app.queue.items().last().unwrap().id).secret_headers;
        assert!(headers.contains(&("Cookie".into(), "sid=cookie-secret".into())) && headers.contains(&("X-Api-Key".into(), "key-secret".into())), "{headers:?}");

        app.handle_event(AppEvent::DroppedText(Ok("https://d.example/d.7z\nPassword: s3cret-pw\n".to_string())));
        assert_eq!(options(&app, app.queue.items().last().unwrap().id).password.as_deref(), Some("s3cret-pw"));

        let count = app.queue.items().len();
        assert_eq!(app.add_pasted("curl -d a=1 https://h.example/f.zip", "", "", Origin::Form), Some(Err(paste::SENDS_DATA.to_string())));
        assert_eq!(app.queue.items().len(), count);
        assert_eq!(app.add_pasted("https://h.example/plain.zip", "", "", Origin::Form), None, "no secret: any other input");

        let notice = format!("{:?}", app.notice);
        let saved = serde_json::to_string(&app.queue).unwrap();
        for secret in secrets {
            assert!(!saved.contains(secret) && !notice.contains(secret) && !queued(&app).concat().contains(secret), "{secret}");
            assert!(!notes.concat().contains(secret));
        }
    }

    /// A paste in the queue input: a link to read is read with its pasted sign-in (sent to its
    /// host alone) and goes back on its line should reading it fail; a line the paste leaves that
    /// is an input of its own (a magnet) is read too, and nothing claims "Added 0 download(s)".
    #[test]
    fn pasted_links_to_read_use_their_sign_in_and_go_back_when_they_fail() {
        const META4: &str = r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="a.bin"><url>https://m.example/a.bin</url></file></metalink>"#;
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // A proxy serving the list only to its sign-in ("bob:hunter2"), failing the other link.
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let heads = Arc::clone(&seen);
        let proxy = rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let heads = Arc::clone(&heads);
                    tokio::spawn(async move {
                        let (mut head, mut buf) = (Vec::new(), [0u8; 1024]);
                        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                            match socket.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => head.extend_from_slice(&buf[..n]),
                            }
                        }
                        let head = String::from_utf8_lossy(&head).into_owned();
                        let signed = head.lines().any(|l| l.eq_ignore_ascii_case("authorization: Basic Ym9iOmh1bnRlcjI="));
                        let reply = match (head.split_whitespace().nth(1), signed) {
                            (Some("http://docs.example/list.meta4"), true) => format!("200 OK\r\nContent-Length: {}\r\n\r\n{META4}", META4.len()),
                            (Some("http://docs.example/list.meta4"), false) => "401 Unauthorized\r\nContent-Length: 0\r\n\r\n".to_string(),
                            _ => "500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_string(),
                        };
                        heads.lock().unwrap().push(head);
                        let _ = socket.write_all(format!("HTTP/1.1 {reply}").as_bytes()).await;
                    });
                }
            });
            addr
        });
        let mut app = test_app(&rt, dir.path());
        app.settings.proxy = format!("http://{proxy}");
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=ubuntu.iso";
        app.queue_input = format!("{magnet}\nhttp://bob:hunter2@docs.example/list.meta4\nhttp://docs.example/broken.meta4\nPassword: s3cret");
        app.add_queue_input();
        assert_eq!(app.queue_input, "");
        assert!(matches!(&app.notice, Some(Ok(n)) if n.starts_with("Reading")), "{:?}", app.notice);
        for read in [magnet, "http://docs.example/list.meta4", "http://docs.example/broken.meta4"] {
            assert!(app.reading.contains_key(read), "{read} is not read");
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !app.reading.is_empty() && std::time::Instant::now() < deadline {
            rt.block_on(async { tokio::time::sleep(Duration::from_millis(20)).await });
            while let Ok(event) = app.events_rx.try_recv() {
                app.handle_event(event);
            }
        }
        assert!(queued(&app).contains(&"https://m.example/a.bin".to_string()), "{:?}", queued(&app));
        assert!(app.queue_input.lines().any(|l| l == "http://docs.example/broken.meta4"), "{}", app.queue_input);
        assert!(app.queue_error.as_deref().is_some_and(|e| e.contains("Line 3: ")), "{:?}", app.queue_error);
        let heads = seen.lock().unwrap().clone();
        assert!(heads.iter().any(|h| h.contains("broken.meta4")), "{heads:?}");
        for head in heads.iter().filter(|h| !h.contains("/list.meta4")) {
            assert!(!head.to_ascii_lowercase().contains("authorization"), "{head}");
        }
        let shown = format!("{} {:?} {:?}", app.queue_input, app.queue_error, app.notice);
        assert!(!shown.contains("hunter2") && !shown.contains("s3cret"), "{shown}");
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
        assert_eq!(app.ffmpeg_waiting, [(first, false)], "started as it was asked to last");
        assert!(!app.start_job(first, true));
        assert_eq!(app.ffmpeg_waiting, [(first, true)], "started as it was asked to last");

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

    /// A video that waits for the answer about ffmpeg holds up none of the downloads queued
    /// after it: the queue starts them, as many as may run.
    #[test]
    fn a_video_waiting_for_the_ffmpeg_answer_holds_up_no_other_download() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        app.engines = Arc::new(|_: Vec<Url>, _: DownloadOptions| Err("not downloaded here".to_string()));
        app.settings.max_concurrent = 2;
        let video = app.add_download("https://www.youtube.com/watch?v=jNQXAC9IVRw", "", "").unwrap();
        let files: Vec<usize> = (1..=3).map(|n| app.add_download(&format!("https://a.example/{n}.iso"), "", "").unwrap()).collect();
        app.run_scheduler();
        assert_eq!(app.ffmpeg_waiting, [(video, false)]);
        let running: Vec<usize> = app.queue.items().iter().filter(|item| item.status.is_active()).map(|item| item.id).collect();
        assert_eq!(running, files[..2]);
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

    /// A download the browser sends takes its name, referer, headers, Authorization header and
    /// choice of MP4 for itself alone: the Settings stay as they were. It waits in the queue while
    /// the URL box is in use, and starts when it is empty and nothing shown runs.
    #[test]
    fn a_download_from_the_browser_takes_its_request_alone() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        app.settings.referer = "https://settings.example/".into();
        let settings = (app.settings.referer.clone(), app.settings.cookies_path.clone());
        let remote = |json: serde_json::Value| AppEvent::RemoteAdd(serde_json::from_value(json).unwrap());
        app.url_input = "https://typed.example/a.iso".into();
        app.handle_event(remote(serde_json::json!({
            "url": "https://cdn.example/v/master.m3u8",
            "user_agent": "Mozilla/5.0 Test",
            "referer": "https://page.example/watch",
            "headers": {"Origin": "https://page.example", "Authorization": "Bearer t", "Cookie": "a=1"},
            "filename": "My: Talk.mp4",
            "hls": true,
            "mp4": false,
        })));
        let item = &app.queue.items()[0];
        assert!(!item.status.is_active(), "queued while the URL box is in use");
        assert!(item.options.hls && !item.options.hls_to_mp4 && app.settings.hls_to_mp4, "MP4 is off for this download alone");
        assert_eq!(item.filename, "My_ Talk.mp4");
        assert_eq!(item.options.output_path, Some(dir.path().join("My_ Talk.mp4")));
        assert_eq!(item.options.referer.as_deref(), Some("https://page.example/watch"));
        let header = |name: &str, value: &str| (name.to_string(), value.to_string());
        assert_eq!(item.options.headers, [header("User-Agent", "Mozilla/5.0 Test"), header("Origin", "https://page.example")]);
        assert_eq!(item.options.auth_header.as_deref(), Some("Bearer t"));
        assert_eq!((app.settings.referer.clone(), app.settings.cookies_path.clone()), settings);

        app.url_input.clear();
        app.handle_event(remote(serde_json::json!({"url": "https://cdn.example/b.mp4"})));
        let started = &app.queue.items()[1];
        assert!(started.status.is_active() && app.focused == Some(started.id));
        assert_eq!(started.options.referer.as_deref(), Some("https://settings.example/"), "the Settings referer when the browser sends none");
        assert!(started.options.headers.is_empty() && started.options.auth_header.is_none());
        assert!(started.options.hls_to_mp4, "the Settings' choice of MP4 when the browser sends none");
        assert!(!started.options.dash && started.options.height.is_none() && started.options.playlist_text.is_none());

        // A DASH manifest with a height to keep to, and a playlist the page built, based on the
        // frame's page, which is not read as a document even when it looks like one.
        app.handle_event(remote(serde_json::json!({"url": "https://cdn.example/v/manifest.mpd", "dash": true, "height": 720, "filename": "D.mp4"})));
        let dash = &app.queue.items()[2];
        assert!(dash.options.dash && dash.options.height == Some(720) && dash.options.playlist_text.is_none());
        let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nseg0.ts\n#EXT-X-ENDLIST";
        app.handle_event(remote(serde_json::json!({"url": "https://page.example/list.meta4", "playlist": format!("{playlist}\n"), "hls": true, "height": 0})));
        let built = &app.queue.items()[3];
        assert_eq!(built.options.playlist_text.as_deref(), Some(playlist));
        assert!(built.options.hls && built.options.height.is_none(), "no height of 0");
        assert!(app.reading.is_empty(), "not read as a document");
        assert_eq!(app.queue.items().len(), 4);
    }

    /// What the extension's media card chose goes to that download alone, over the Settings: the
    /// one video of a link that names its playlist too, or the whole playlist, read as that link
    /// without asking, each of its downloads as the card chose. A link that is only a list is read
    /// whatever the card chose; a reading that adds nothing forgets the choice.
    #[test]
    fn a_media_choice_from_the_browser_goes_to_its_downloads() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        app.url_input = "https://typed.example/a.iso".into();
        app.settings.subtitles = "de".into();
        app.settings.sponsorblock = 1;
        let remote = |json: serde_json::Value| AppEvent::RemoteAdd(serde_json::from_value(json).unwrap());
        let video = "https://www.youtube.com/watch?v=jNQXAC9IVRw&list=PLbpi6ZahtOH6Blw3RGYpWkSByi_T7Rygb";
        let media = serde_json::json!({"quality": "1440", "container": "mkv", "sections": [[90, 150]], "sponsorblock": "mark"});
        app.handle_event(remote(serde_json::json!({"url": video, "height": 720, "media": media})));
        let item = &app.queue.items()[0];
        assert_eq!(item.urls[0].as_str(), video);
        assert!(app.reading.is_empty() && app.listings.is_empty(), "the one video, without asking");
        assert_eq!((item.options.height, item.options.sections.as_slice()), (Some(1440), &[(90.0, 150.0)][..]));
        assert_eq!((item.options.merge_format.as_deref(), item.options.sponsorblock.as_deref()), (Some("mkv"), Some("mark")));
        assert_eq!(item.options.subtitles.as_deref(), Some("de"), "the Settings' subtitles when the card sent none");
        assert!(!app.settings.live_from_start && !item.options.live_from_start);

        // The card's None and Off.
        app.handle_event(remote(serde_json::json!({"url": video, "media": {"subtitles": "", "sponsorblock": "off"}})));
        let item = &app.queue.items()[1];
        assert_eq!((item.options.subtitles.as_deref(), item.options.sponsorblock.as_deref()), (None, None));
        let id = item.id;
        app.queue.remove_item(id);

        // A mix: YouTube lists it at the video link alone.
        let media = serde_json::json!({"quality": "audio-mp3", "subtitles": "en", "live_from_start": true, "playlist": true});
        let video = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDdQw4w9WgXcQ";
        app.handle_event(remote(serde_json::json!({"url": video, "height": 720, "media": media})));
        let list = video;
        assert!(app.reading.contains_key(list) && app.listings.is_empty(), "the playlist is read, without asking");
        let entry = |id: &str| Task { urls: vec![Url::parse(&format!("https://www.youtube.com/watch?v={id}")).unwrap()], from_document: true, ..Default::default() };
        let (checksum, auth, notes) = (String::new(), String::new(), Vec::new());
        let result = Ok(vec![entry("Vh4O04Bpovw"), entry("5TIp7oVKHq8")]);
        app.handle_event(AppEvent::Read { origin: Origin::Dropped, input: list.to_string(), checksum, auth, result, notes });
        let listed = &app.queue.items()[1..];
        assert_eq!(listed.len(), 2);
        for item in listed {
            assert_eq!((&item.options.media_preset, item.options.height), (&Some(media::MediaQualityPreset::AudioMp3), None));
            assert!(item.options.subtitles.as_deref() == Some("en") && item.options.live_from_start, "{:?}", item.options);
        }
        assert!(app.media_lists.is_empty());

        // A channel is read with the card's choice, which a reading that adds nothing forgets.
        let channel = "https://www.youtube.com/@NASA";
        app.handle_event(remote(serde_json::json!({"url": channel, "media": {"quality": "best"}})));
        assert!(app.reading.contains_key(channel) && app.media_lists.contains_key(channel));
        let (checksum, auth, notes) = (String::new(), String::new(), Vec::new());
        app.handle_event(AppEvent::Read { origin: Origin::Dropped, input: channel.to_string(), checksum, auth, result: Ok(Vec::new()), notes });
        assert!(app.media_lists.is_empty() && app.queue.items().len() == 3);
    }

    /// The link grabber's batch is queued whole, each link with the page's referer and User-Agent
    /// alone; a list among them is read; nothing the URL box holds is touched.
    #[test]
    fn a_batch_from_the_browser_is_queued() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        app.url_input = "https://typed.example/a.iso".into();
        app.handle_event(AppEvent::RemoteAdd(
            serde_json::from_value(serde_json::json!({
                "urls": ["https://a.example/x.zip", "https://b.example/y.pdf", "https://c.example/list.meta4"],
                "referer": "https://page.example/",
                "user_agent": "Mozilla/5.0 Test",
            }))
            .unwrap(),
        ));
        assert_eq!(queued(&app), ["https://a.example/x.zip", "https://b.example/y.pdf"]);
        for item in app.queue.items() {
            assert!(!item.status.is_active());
            assert_eq!(item.options.referer.as_deref(), Some("https://page.example/"));
            assert_eq!(item.options.headers, [("User-Agent".to_string(), "Mozilla/5.0 Test".to_string())]);
        }
        assert!(app.reading.contains_key("https://c.example/list.meta4"), "the list is read");
        assert_eq!(app.url_input, "https://typed.example/a.iso");
        assert_eq!(app.notice, Some(Ok("Added 2 download(s) from the browser to the queue".to_string())));
    }

    /// A remote document is fetched through the proxy setting: its host does not exist, so only
    /// the proxy can answer.
    #[tokio::test]
    async fn documents_are_read_through_the_proxy_setting() {
        let xml = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="a.bin"><url>https://m.example/a.bin</url></file></metalink>"#;
        let proxy = serve(Arc::new(xml.to_vec())).await;
        let settings = Settings { proxy: format!("http://{}", proxy), ..Settings::default() };
        let (tasks, notes) = read_listing(&settings, "http://documents.invalid/list.meta4".to_string(), None).await;
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
        let (tasks, notes) = read_listing(&settings, "http://www.mediafire.com/folder/pack00000001".to_string(), None).await;
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

    /// A check at launch is quiet unless it finds a newer release, which is offered; one the user
    /// asked for also says when there is none, or why it failed. A failed install keeps the offer;
    /// a finished one closes the window (the existing way) for `main` to start the new build.
    #[test]
    fn updates_are_offered_and_installed_once() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        let checked = |result, manual| AppEvent::UpdateChecked { result, manual };
        app.handle_event(checked(Ok(None), false));
        app.handle_event(checked(Err("offline".to_string()), false));
        assert!(app.update.is_none() && app.notice.is_none(), "a check at launch only logs");
        app.handle_event(checked(Ok(None), true));
        assert_eq!(app.notice, Some(Ok(format!("You have the latest version ({}).", env!("CARGO_PKG_VERSION")))));
        app.handle_event(checked(Err("offline".to_string()), true));
        assert!(matches!(&app.notice, Some(Err(e)) if e.contains("offline")));

        let update = Update { version: "9.0.0".into(), tag: "v9.0.0".into(), page: format!("{}/tag/v9.0.0", updater::RELEASES) };
        app.handle_event(checked(Ok(Some(update.clone())), false));
        assert_eq!(app.update.as_ref(), Some(&update));
        app.install_update();
        assert!(app.updating);
        app.handle_event(AppEvent::UpdateInstalled(Err("the signature does not match".to_string())));
        assert!(!app.updating && app.update.as_ref() == Some(&update), "the offer stays to try again");
        assert!(matches!(&app.notice, Some(Err(e)) if e.contains("signature")));
        assert!(lock(&app.restart).is_none());

        let own = dir.path().join(updater::GUI_EXE);
        app.handle_event(AppEvent::UpdateInstalled(Ok(Installed { own: own.clone(), ..Installed::default() })));
        assert_eq!(lock(&app.restart).as_ref(), Some(&own), "started once the window has closed");
        assert!(app.update.is_none() && !app.updating);
        assert!(app.quitting, "it closes for good, not into the tray");
    }

    /// Download `id` of `app` running, as far as the app can tell (no engine runs on `rt`).
    fn running(app: &mut App, rt: &tokio::runtime::Runtime, id: usize) {
        app.queue.mark_started(id);
        let running = Running { cancel: Arc::default(), task: rt.spawn(async {}), snapshot: Arc::default() };
        app.jobs.insert(id, JobView { running: Some(running), ..JobView::default() });
    }

    fn add(app: &mut App, name: &str) -> usize {
        app.queue.add_item(vec![Url::parse(&format!("https://example.com/{}", name)).unwrap()], DownloadOptions::default())
    }

    fn status(app: &App, id: usize) -> QueueItemStatus {
        app.queue.get_item(id).unwrap().status.clone()
    }

    /// Once the queue is done (its last download ended by itself and nothing queued is left to
    /// start), what was chosen for it happens, once: Sleep and Shut down after 30 s, through the
    /// stand-in for the computer, unless cancelled; Quit closes the app as closing the window
    /// does. A paused download is not done.
    #[test]
    fn when_the_queue_is_done_the_choice_happens_once() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        let ran = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&ran);
        app.power = Box::new(move |action| {
            lock(&log).push(action);
            Ok(())
        });
        let end = |app: &mut App, id, result| app.handle_event(AppEvent::JobFinished { id, result });
        let done = |name: &str| Ok((PathBuf::from(name), Some(1)));

        app.when_done = WhenDone::ShutDown;
        let (a, b) = (add(&mut app, "a.iso"), add(&mut app, "b.iso"));
        running(&mut app, &rt, a);
        end(&mut app, a, done("a.iso"));
        assert!(app.countdown.is_none(), "b.iso waits to start");
        running(&mut app, &rt, b);
        app.pause_job(b);
        end(&mut app, b, Err(hyperfetch_core::engine::CANCELLED.to_string()));
        assert!(app.countdown.is_none(), "paused is not done");
        running(&mut app, &rt, b);
        end(&mut app, b, Err("The server answered 404 Not Found".to_string()));
        let (action, at) = app.countdown.expect("counting down");
        assert_eq!((action, app.when_done), (WhenDone::ShutDown, WhenDone::Nothing), "once");
        app.tick_countdown(at - Duration::from_secs(1));
        assert!(lock(&ran).is_empty() && app.countdown.is_some());
        app.tick_countdown(at);
        assert_eq!(*lock(&ran), [WhenDone::ShutDown]);
        assert!(app.countdown.is_none());

        // Cancel (the banner's button) takes the countdown away.
        app.when_done = WhenDone::Sleep;
        let c = add(&mut app, "c.iso");
        running(&mut app, &rt, c);
        end(&mut app, c, done("c.iso"));
        let (_, at) = app.countdown.take().expect("counting down");
        app.tick_countdown(at);
        assert_eq!(lock(&ran).len(), 1);

        // What the computer refused is told.
        app.power = Box::new(|_| Err(std::io::Error::other("not allowed")));
        app.when_done = WhenDone::Sleep;
        let d = add(&mut app, "d.iso");
        running(&mut app, &rt, d);
        end(&mut app, d, done("d.iso"));
        app.tick_countdown(app.countdown.unwrap().1);
        assert_eq!(app.notice, Some(Err("Could not sleep: not allowed".to_string())));

        let e = add(&mut app, "e.iso");
        running(&mut app, &rt, e);
        app.when_done = WhenDone::Quit;
        end(&mut app, e, done("e.iso"));
        assert!(app.quitting && app.countdown.is_none());
    }

    /// A download started while the countdown runs stops it, the choice kept for when the queue
    /// is done again; the settings are saved before the computer sleeps.
    #[test]
    fn the_countdown_waits_for_a_new_download_and_saves_the_settings_first() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        let slept = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&slept);
        app.power = Box::new(move |action| {
            let saved = std::fs::read_to_string(settings::app_file("gui-settings.json")).unwrap_or_default();
            lock(&log).push((action, saved.contains("countdown-test-agent")));
            Ok(())
        });
        app.settings.user_agent = "countdown-test-agent".to_string();
        app.when_done = WhenDone::Sleep;
        let done = |name: &str| Ok((PathBuf::from(name), Some(1)));
        let a = add(&mut app, "a.iso");
        running(&mut app, &rt, a);
        app.handle_event(AppEvent::JobFinished { id: a, result: done("a.iso") });
        let (_, at) = app.countdown.expect("counting down");
        let b = add(&mut app, "b.iso");
        running(&mut app, &rt, b);
        app.tick_countdown(at);
        assert!(app.countdown.is_none() && lock(&slept).is_empty(), "b runs");
        assert_eq!(app.when_done, WhenDone::Sleep, "the choice is kept");
        app.handle_event(AppEvent::JobFinished { id: b, result: done("b.iso") });
        let (_, at) = app.countdown.expect("counting down again");
        app.tick_countdown(at);
        assert_eq!(*lock(&slept), [(WhenDone::Sleep, true)], "the settings were saved first");
    }

    /// "See what's inside" gives up on a server that never answers, and says so.
    #[test]
    fn a_zip_preview_gives_up_on_a_silent_server() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        // Takes connections and never answers; they close with the runtime.
        let addr = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let mut held = Vec::new();
                while let Ok((socket, _)) = listener.accept().await {
                    held.push(socket);
                }
            });
            addr
        });
        app.preview_zip(Url::parse(&format!("http://{addr}/pack.zip")).unwrap());
        let event = app.events_rx.recv_timeout(Duration::from_secs(20)).expect("an answer within the limit");
        app.handle_event(event);
        let listed = app.zip_preview.as_ref().and_then(|(_, listed)| listed.as_ref());
        let Some(Err(error)) = listed else { panic!("no error") };
        assert_eq!(error, "Gave up listing the ZIP: the server took over 2 seconds");
    }

    /// Pause all stops the queue and pauses what runs; Resume all starts both again, as many at
    /// once as the queue runs, the paused ones first; the queue's switch follows Start and Stop.
    /// The schedule's stop pauses what runs, and its start resumes only that, not a download the
    /// user paused.
    #[test]
    fn the_tray_and_the_schedule_pause_and_resume_the_queue() {
        use QueueItemStatus::{Completed, Downloading, Paused, Pausing, Queued};
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        app.settings.max_concurrent = 1;
        let [a, b, c] = ["a.iso", "b.iso", "c.iso"].map(|name| add(&mut app, name));
        running(&mut app, &rt, a);
        running(&mut app, &rt, b);
        let stopped = |app: &mut App, id| app.handle_event(AppEvent::JobFinished { id, result: Err(hyperfetch_core::engine::CANCELLED.to_string()) });

        app.handle_event(AppEvent::Tray(TrayAction::PauseAll));
        assert!(!app.settings.auto_run_queue);
        assert_eq!([a, b, c].map(|id| status(&app, id)), [Pausing, Pausing, Queued]);
        stopped(&mut app, a);
        stopped(&mut app, b);
        app.handle_event(AppEvent::Tray(TrayAction::ResumeAll));
        assert!(app.settings.auto_run_queue);
        assert_eq!([a, b, c].map(|id| status(&app, id)), [Downloading, Paused, Queued], "one at a time");
        app.handle_event(AppEvent::JobFinished { id: a, result: Ok((PathBuf::from("a.iso"), Some(1))) });
        app.run_scheduler();
        assert_eq!([a, b, c].map(|id| status(&app, id)), [Completed, Downloading, Queued], "the paused one first");
        app.handle_event(AppEvent::Tray(TrayAction::StopQueue));
        assert!(!app.settings.auto_run_queue);
        app.handle_event(AppEvent::Tray(TrayAction::StartQueue));
        assert!(app.settings.auto_run_queue);

        let schedule = settings::Schedule { enabled: true, start: Some("08:00".into()), stop: Some("18:00".into()), ..Default::default() };
        app.settings.schedule = schedule;
        app.schedule_seen = Some((0, 17 * 60 + 58));
        app.run_schedule((0, 17 * 60 + 59));
        assert_eq!(status(&app, b), Downloading);
        app.run_schedule((0, 18 * 60));
        assert!(!app.settings.auto_run_queue);
        assert_eq!(status(&app, b), Pausing);
        stopped(&mut app, b);
        assert_eq!(app.settings.schedule_paused, [b], "kept with the settings");
        assert!(app.start_job(c, false));
        app.pause_job(c);
        stopped(&mut app, c);
        app.run_schedule((1, 8 * 60 + 2));
        assert!(app.settings.auto_run_queue && app.settings.schedule_paused.is_empty());
        assert_eq!([b, c].map(|id| status(&app, id)), [Downloading, Paused], "the user's pause stays");

        app.hide_window();
        assert!(app.hidden);
        app.taskbar_shown = Some(taskbar::Progress::Normal(500));
        app.handle_event(AppEvent::Tray(TrayAction::Show));
        assert!(!app.hidden && !app.quitting);
        assert_eq!(app.taskbar_shown, None, "set again on the button put back");
        app.handle_event(AppEvent::Tray(TrayAction::Quit));
        assert!(app.quitting);
    }

    /// What the schedule paused is resumed at its start after a restart in between, a launch
    /// inside its time starting the queue. Each minute is taken once, also as the clock goes back
    /// an hour; turned off and on again, it starts from then.
    #[test]
    fn the_schedule_outlives_a_restart_and_takes_each_minute_once() {
        use QueueItemStatus::{Downloading, Paused, Pausing};
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        app.settings.schedule = settings::Schedule { enabled: true, start: Some("08:00".into()), stop: Some("18:00".into()), ..Default::default() };
        let [a, b] = ["a.iso", "b.iso"].map(|name| add(&mut app, name));
        running(&mut app, &rt, a);
        app.schedule_seen = Some((0, 17 * 60 + 59));
        app.run_schedule((0, 18 * 60));
        app.handle_event(AppEvent::JobFinished { id: a, result: Err(hyperfetch_core::engine::CANCELLED.to_string()) });
        assert_eq!([a, b].map(|id| status(&app, id)), [Paused, QueueItemStatus::Queued]);

        // Started again the next morning, with the settings and queue it saved.
        let mut app = App::with(egui::Context::default(), rt.handle().clone(), app.settings.clone(), app.queue.clone(), None);
        app.run_schedule((1, 9 * 60));
        assert!(app.settings.auto_run_queue);
        assert_eq!([a, b].map(|id| status(&app, id)), [Downloading, Downloading]);

        app.run_schedule((1, 18 * 60));
        assert_eq!([a, b].map(|id| status(&app, id)), [Pausing, Pausing]);
        for id in [a, b] {
            app.handle_event(AppEvent::JobFinished { id, result: Err(hyperfetch_core::engine::CANCELLED.to_string()) });
        }
        // Summer time ends: 18:00 comes again an hour later, and stops nothing the user resumed.
        app.run_schedule((1, 17 * 60));
        assert_eq!(app.schedule_seen, Some((1, 18 * 60)));
        running(&mut app, &rt, b);
        app.run_schedule((1, 18 * 60));
        assert_eq!(status(&app, b), Downloading);

        // Off, then on again two days later: yesterday's stop time is not taken now.
        app.settings.schedule.enabled = false;
        let ctx = app.ctx.clone();
        app.desktop_frame(&ctx);
        assert_eq!(app.schedule_seen, None);
        app.settings.schedule.enabled = true;
        app.run_schedule((3, 19 * 60));
        assert_eq!(status(&app, b), Downloading);
    }

    /// The taskbar shows nothing while no download runs, else the share of their bytes done:
    /// yellow when all are pausing, red once one failed until the queue stands still.
    #[test]
    fn the_taskbar_shows_the_running_downloads() {
        use taskbar::Progress;
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        let [a, b] = ["a.iso", "b.iso"].map(|name| add(&mut app, name));
        assert_eq!(app.overall_progress(), Progress::None);
        running(&mut app, &rt, a);
        running(&mut app, &rt, b);
        app.queue.apply_snapshot(a, &EngineSnapshot { total_bytes: 1000, downloaded_bytes: 250, ..Default::default() });
        assert_eq!(app.overall_progress(), Progress::Normal(250), "a size not known yet counts for nothing");
        app.queue.apply_snapshot(b, &EngineSnapshot { total_bytes: 3000, downloaded_bytes: 3000, ..Default::default() });
        assert_eq!(app.overall_progress(), Progress::Normal(812));
        app.pause_job(a);
        assert_eq!(app.overall_progress(), Progress::Normal(812));
        app.pause_job(b);
        assert_eq!(app.overall_progress(), Progress::Paused(812));
        app.batch_failed = true;
        assert_eq!(app.overall_progress(), Progress::Error(812));
    }

    /// Launch at startup is written where the system looks, through the writer the app holds,
    /// and the setting changes only when that worked.
    #[test]
    fn launch_at_startup_is_registered_and_removed() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&rt, dir.path());
        let written = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&written);
        app.writer = Box::new(move |place, value| {
            lock(&log).push((place.clone(), value.map(str::to_string)));
            Ok(())
        });
        app.set_launch_at_startup(true);
        assert!(app.settings.launch_at_startup);
        let (place, value) = desktop::startup_entry(&std::env::current_exe().unwrap(), &desktop::startup_root());
        app.set_launch_at_startup(false);
        assert!(!app.settings.launch_at_startup);
        assert_eq!(*lock(&written), [(place.clone(), Some(value)), (place, None)]);

        app.register_protocol();
        assert_eq!(lock(&written).len(), 5, "the endo-dl: key, its URL Protocol value and its command");

        app.writer = Box::new(|_, _| Err(std::io::Error::other("access denied")));
        app.set_launch_at_startup(true);
        assert!(!app.settings.launch_at_startup);
        assert_eq!(app.notice, Some(Err("Could not change launch at startup: access denied".to_string())));
    }
}
