//! Runs downloads with progress output and graceful Ctrl+C / SIGTERM handling.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::future::{BoxFuture, OptionFuture, Shared};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use hyperfetch_core::engine::{build_client, ClientKey, DownloadEngine, DownloadOptions, EngineSnapshot, SharedLimits};
use hyperfetch_core::history::{DownloadHistoryManager, HistoryEntry, HistoryStatus};
use hyperfetch_core::media::is_supported_media_site;
use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use tokio::sync::{broadcast, mpsc, watch};
use url::Url;

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
const EXIT_SIGINT: i32 = 130;
#[cfg(unix)]
const EXIT_SIGTERM: i32 = 143;

/// How long a cancelled download may take to stop and save its resume state.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// No new bytes for this long shows the download as stalled.
const STALL_AFTER: Duration = Duration::from_secs(5);
/// Interval of plain progress lines when stderr is not a terminal (journald, cron, pipes).
const PLAIN_INTERVAL: Duration = Duration::from_secs(10);

/// Process-wide shutdown request, set by the first Ctrl+C / SIGTERM.
#[derive(Clone)]
pub struct Shutdown(watch::Sender<Option<i32>>);

impl Shutdown {
    /// Starts the one signal listener of the process. The first signal asks running work to stop
    /// (or exits right away when nothing is running, e.g. at a prompt); a second one exits at once.
    pub fn install() -> Self {
        let (tx, _) = watch::channel(None);
        let listener = tx.clone();
        let (sig_tx, mut sig_rx) = mpsc::unbounded_channel();
        forward_signals(sig_tx);
        tokio::spawn(async move {
            let Some(code) = sig_rx.recv().await else { return };
            if listener.receiver_count() == 0 {
                std::process::exit(code);
            }
            listener.send_replace(Some(code));
            stderr_line("\nStopping: saving resume state... (press Ctrl+C again to quit immediately)");
            let code = sig_rx.recv().await.unwrap_or(code);
            std::process::exit(code);
        });
        Self(tx)
    }

    /// A receiver that counts as running work until it is dropped.
    pub fn subscribe(&self) -> watch::Receiver<Option<i32>> {
        self.0.subscribe()
    }

    /// The exit code of the signal that requested shutdown, if any.
    pub fn requested(&self) -> Option<i32> {
        *self.0.borrow()
    }
}

fn forward_signals(tx: mpsc::UnboundedSender<i32>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        for (kind, code) in [(SignalKind::interrupt(), EXIT_SIGINT), (SignalKind::terminate(), EXIT_SIGTERM)] {
            match signal(kind) {
                Ok(mut stream) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        while stream.recv().await.is_some() && tx.send(code).is_ok() {}
                    });
                }
                Err(e) => tracing::warn!("Cannot listen for signal {}: {}", code - 128, e),
            }
        }
    }
    #[cfg(not(unix))]
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() && tx.send(EXIT_SIGINT).is_ok() {}
    });
}

/// Resolves once shutdown is requested (never, if the listener is gone).
pub async fn stop_requested(stop: &mut watch::Receiver<Option<i32>>) {
    if stop.wait_for(Option::is_some).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Terminal output shared by all downloads: progress bars, messages and log lines.
#[derive(Clone)]
pub struct Ui {
    multi: MultiProgress,
    quiet: bool,
    /// stderr is not a terminal: print a progress line now and then instead of bars.
    plain: bool,
    held: Arc<Mutex<Held>>,
}

/// Output held back while a question waits for its answer (see [`Ui::hiding_bars`]), each piece
/// with whether it goes to stderr.
type Held = Option<Vec<(bool, Vec<u8>)>>;

impl Ui {
    pub fn new(quiet: bool) -> Self {
        let multi = if quiet {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        } else {
            MultiProgress::new()
        };
        Self { multi, quiet, plain: !quiet && !std::io::stderr().is_terminal(), held: Arc::default() }
    }

    /// -q was given: no progress or status output.
    pub fn quiet(&self) -> bool {
        self.quiet
    }

    /// Prints to stdout without tearing the progress bars.
    pub fn print(&self, line: &str) {
        let _ = self.write(false, format!("{}\n", printable(line)).as_bytes());
    }

    /// Prints to stderr without tearing the progress bars.
    pub fn error(&self, line: &str) {
        let _ = self.write(true, format!("{}\n", printable(line)).as_bytes());
    }

    /// Writes `bytes` to stderr, else stdout, without tearing the progress bars; while a question
    /// waits for its answer, once it is answered.
    fn write(&self, stderr: bool, bytes: &[u8]) -> std::io::Result<()> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        match held.as_mut() {
            Some(held) => {
                held.push((stderr, bytes.to_vec()));
                Ok(())
            }
            None => self.multi.suspend(|| write_to(stderr, bytes)),
        }
    }

    /// Awaits `question`, a prompt at the terminal, with the progress bars off it, so that they
    /// draw over neither the question nor the answer typed, and what the downloads print
    /// meanwhile (a finished file, a log line) held back until it is answered, so that it does
    /// not land in the question's line or scroll it away.
    pub async fn hiding_bars<T>(&self, question: impl Future<Output = T>) -> T {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Some(Vec::new());
        let _ = self.multi.clear();
        self.multi.set_draw_target(ProgressDrawTarget::hidden());
        let answer = question.await;
        // Written with the lock held, so that nothing printed now goes ahead of it.
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        for (stderr, bytes) in held.take().unwrap_or_default() {
            let _ = write_to(stderr, &bytes);
        }
        drop(held);
        if !self.quiet {
            self.multi.set_draw_target(ProgressDrawTarget::stderr());
        }
        answer
    }

    /// A writer for log output that keeps the progress bars intact.
    pub fn log_writer(&self) -> LogWriter {
        LogWriter(self.clone())
    }

    /// A byte progress bar of `len` bytes (unknown when None), placed above `below` when given.
    pub fn byte_bar(&self, label: &str, len: Option<u64>, below: Option<&ProgressBar>) -> ProgressBar {
        if self.quiet {
            return ProgressBar::hidden();
        }
        let bar = match len {
            Some(len) => ProgressBar::new(len).with_style(style(SIZED)),
            None => ProgressBar::no_length().with_style(style(UNSIZED)),
        }
        .with_prefix(truncate(label, 28));
        let bar = match below {
            Some(below) => self.multi.insert_before(below, bar),
            None => self.multi.add(bar),
        };
        bar.enable_steady_tick(Duration::from_millis(120));
        bar
    }
}

#[derive(Clone)]
pub struct LogWriter(Ui);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(true, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// Writes `bytes` to stderr, else stdout.
fn write_to(stderr: bool, bytes: &[u8]) -> std::io::Result<()> {
    if stderr {
        std::io::stderr().write_all(bytes)
    } else {
        std::io::stdout().write_all(bytes)
    }
}

const SIZED: &str = "{spinner:.green} {prefix:28.bold} [{bar:30.cyan/blue}] {bytes:>10}/{total_bytes:<10} {msg}";
/// With no size to reach (a live recording, a server that sends none): what came in, and for how long.
const UNSIZED: &str = "{spinner:.green} {prefix:28.bold} {bytes:>10} in {elapsed_precise} {msg}";
const OVERALL: &str = "  {prefix:28.bold} [{bar:30.green/white}] {pos}/{len} files {msg}";

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=>-")
}

/// `s` with control characters other than newline and tab shown as '?', so names and messages
/// from untrusted sources (torrents, servers, history) cannot drive the terminal.
pub fn printable(s: &str) -> String {
    s.chars().map(|c| if c.is_control() && c != '\n' && c != '\t' { '?' } else { c }).collect()
}

/// Prints a line to stdout. Write errors are ignored: a reader that went away (`| head`) must
/// not abort the process and the downloads still running in it.
pub fn stdout_line(line: &str) {
    let _ = writeln!(std::io::stdout(), "{}", printable(line));
}

/// Prints a line to stderr, ignoring write errors like [`stdout_line`].
pub fn stderr_line(line: &str) {
    let _ = writeln!(std::io::stderr(), "{}", printable(line));
}

/// Shortens `s` to at most `max` characters, marking the cut with "...".
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(3)).collect();
    format!("{}...", kept)
}

fn compact_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{}s", secs),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// The text after the bar: engine speed, open connections and ETA, or a stall warning.
fn status_text(speed: f64, connections: usize, remaining: Option<u64>, stalled: Option<Duration>) -> String {
    if let Some(stalled) = stalled {
        return format!("STALLED: no data for {}, {} conn", compact_duration(stalled.as_secs()), connections);
    }
    let mut text = format!("{}/s, {} conn", HumanBytes(speed as u64), connections);
    if let Some(remaining) = remaining.filter(|_| speed >= 1.0) {
        text.push_str(&format!(", ETA {}", compact_duration((remaining as f64 / speed).ceil() as u64)));
    }
    text
}

/// How far a download is, for a plain progress line: the share of its `total`, or with no size to
/// reach (0: a live recording, a server that sends none) what came in and in how long.
fn done_text(downloaded: u64, total: u64, elapsed: Duration) -> String {
    if total > 0 {
        format!("{:.1}% of {}", downloaded as f64 * 100.0 / total as f64, HumanBytes(total))
    } else {
        format!("{} in {}", HumanBytes(downloaded), compact_duration(elapsed.as_secs()))
    }
}

/// One download to run.
pub struct Job {
    pub label: String,
    pub urls: Vec<Url>,
    pub options: DownloadOptions,
    /// The line of the input file (-i) that listed it.
    pub line: Option<usize>,
}

impl Job {
    /// Whether it goes to yt-dlp, which may need ffmpeg (see `DownloadOptions::install_ffmpeg`).
    pub fn needs_ffmpeg(&self) -> bool {
        self.options.media_preset.is_some() || self.urls.iter().any(is_supported_media_site)
    }
}

/// The user's answer whether ffmpeg may be installed, asked once for every download waiting for it.
pub type FfmpegAnswer = Shared<BoxFuture<'static, bool>>;

enum Outcome {
    Done,
    Failed,
    Interrupted,
}

/// One HTTP client per [`ClientKey`] among a batch's downloads, so later downloads reuse the
/// connections and TLS sessions earlier ones opened, and the speed limit they share (-j downloads
/// at once stay within --max-speed together).
struct Clients(HashMap<ClientKey, Result<reqwest::Client, String>>, SharedLimits);

impl Clients {
    /// Builds, off the runtime, the client of every key among `jobs`.
    async fn for_jobs(jobs: &[Job]) -> Self {
        let mut wanted: HashMap<ClientKey, DownloadOptions> = HashMap::new();
        for job in jobs {
            wanted.entry(ClientKey::of(&job.options)).or_insert_with(|| job.options.clone());
        }
        let keys: Vec<ClientKey> = wanted.keys().cloned().collect();
        let built = tokio::task::spawn_blocking(move || {
            wanted.into_iter().map(|(key, options)| (key, build_client(&options))).collect()
        })
        .await;
        let clients = match built {
            Ok(clients) => clients,
            Err(e) => keys.into_iter().map(|key| (key, Err(format!("cannot create HTTP client: {}", e)))).collect(),
        };
        Self(clients, SharedLimits::default())
    }

    /// An engine for these URLs and options on the shared client of their key and the shared
    /// speed limit, or why that client could not be built (a bad proxy, an unreadable cookies file).
    fn engine(&self, urls: Vec<Url>, options: DownloadOptions) -> Result<DownloadEngine, String> {
        match self.0.get(&ClientKey::of(&options)) {
            Some(Ok(client)) => Ok(DownloadEngine::with_client(urls, options, client.clone()).sharing_limit(&self.1)),
            Some(Err(e)) => Err(e.clone()),
            None => Err("no HTTP client was built for this download".to_string()),
        }
    }
}

/// Runs `jobs` in order with at most `concurrency` at a time and returns how many failed. While
/// `ffmpeg`, the answer whether ffmpeg may be installed, is not known, the downloads that need it
/// (see [`Job::needs_ffmpeg`]) wait for it and the others go ahead of them; every download that
/// starts once it is known goes by it. After a shutdown request no new job starts and running
/// ones stop with their state saved. Running several at once, results arrive in completion
/// order, so each result line names its input.
pub async fn run_jobs(jobs: Vec<Job>, concurrency: usize, ui: &Ui, shutdown: &Shutdown, ffmpeg: Option<FfmpegAnswer>) -> usize {
    let overall = (jobs.len() > 1 && !ui.quiet).then(|| {
        let bar = ui.multi.add(ProgressBar::new(jobs.len() as u64).with_style(style(OVERALL)).with_prefix("Total"));
        bar.tick();
        bar
    });
    let named = concurrency > 1 && jobs.len() > 1;
    let clients = Clients::for_jobs(&jobs).await;
    let mut stop = shutdown.subscribe();
    let (mut waiting, mut running) = (VecDeque::from(jobs), FuturesUnordered::new());
    let mut failed = 0;
    loop {
        while running.len() < concurrency.max(1) {
            // None when nothing is asked; Some(None) while it is not answered.
            let answer = ffmpeg.as_ref().map(|answer| answer.peek().copied());
            let next = waiting.iter().position(|job| answer != Some(None) || !job.needs_ffmpeg());
            let Some(mut job) = next.and_then(|at| waiting.remove(at)) else { break };
            if let Some(Some(install)) = answer {
                job.options.install_ffmpeg = install;
            }
            let input = named.then(|| input_name(job.line, &job.urls)).flatten();
            running.push(run_job(job, input, &clients, ui, shutdown, overall.as_ref()));
        }
        let unanswered = ffmpeg.clone().filter(|answer| answer.peek().is_none() && !waiting.is_empty());
        tokio::select! {
            Some(outcome) = running.next() => {
                match outcome {
                    Outcome::Done => {}
                    Outcome::Failed => failed += 1,
                    Outcome::Interrupted => continue,
                }
                if let Some(bar) = &overall {
                    bar.inc(1);
                    if failed > 0 {
                        bar.set_message(format!("({} failed)", failed));
                    }
                }
            }
            Some(_) = OptionFuture::from(unanswered) => {}
            () = stop_requested(&mut stop), if !waiting.is_empty() => waiting.clear(),
            else => break,
        }
    }
    drop(running);
    if let Some(bar) = overall {
        bar.abandon();
    }
    failed
}

/// "line 3 https://host/path/file": the input-file line that listed the job, if one did, and its
/// first URL without the user name, password, query and fragment, which may carry credentials.
/// None when there is neither.
fn input_name(line: Option<usize>, urls: &[Url]) -> Option<String> {
    let url = urls.first().map(|url| {
        let mut shown = url.clone();
        // These fail only for URLs that cannot carry credentials.
        let _ = shown.set_username("");
        let _ = shown.set_password(None);
        shown.set_query(None);
        shown.set_fragment(None);
        truncate(shown.as_str(), 100)
    });
    match (line, url) {
        (Some(line), Some(url)) => Some(format!("line {} {}", line, url)),
        (Some(line), None) => Some(format!("line {}", line)),
        (None, url) => url,
    }
}

/// The line reporting a finished download; `input` names it when results arrive out of order.
fn done_line(input: Option<&str>, path: &std::path::Path) -> String {
    match input {
        Some(input) => format!("[OK] {} -> {}", input, path.display()),
        None => format!("[OK] {}", path.display()),
    }
}

/// `input` is what result lines call the download (see [`input_name`]); None uses its file name.
async fn run_job(
    job: Job,
    input: Option<String>,
    clients: &Clients,
    ui: &Ui,
    shutdown: &Shutdown,
    overall: Option<&ProgressBar>,
) -> Outcome {
    let mut stop = shutdown.subscribe();
    if stop.borrow().is_some() {
        return Outcome::Interrupted;
    }
    let started_at = unix_now();
    let urls: Vec<String> = job.urls.iter().map(Url::to_string).collect();
    let engine = match clients.engine(job.urls, job.options) {
        Ok(engine) => engine,
        Err(err) => {
            ui.error(&format!("[FAILED] {}: {}", input.as_deref().unwrap_or(&job.label), err));
            return Outcome::Failed;
        }
    };
    let mut view = TaskView::new(ui, &job.label, overall);

    let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(64);
    let run = engine.run(Some(tx));
    tokio::pin!(run);
    let mut last: Option<EngineSnapshot> = None;
    let mut snapshots_open = true;
    let (mut stopping, mut deadline) = (false, None);
    let result = loop {
        tokio::select! {
            res = &mut run => break res,
            snapshot = rx.recv(), if snapshots_open => match snapshot {
                Ok(snapshot) => {
                    view.update(&snapshot);
                    last = Some(snapshot);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => snapshots_open = false,
            },
            // Keep awaiting run() after cancel: it stops the workers and saves the resume state,
            // or finishes a recording.
            _ = stop_requested(&mut stop), if !stopping => {
                stopping = true;
                engine.cancel();
                let recording = is_recording(last.as_ref());
                view.bar.set_message(if recording { "stopping, finishing the recording..." } else { "stopping, saving resume state..." });
                deadline = (!recording).then(|| tokio::time::Instant::now() + STOP_TIMEOUT);
            }
            _ = sleep_until(deadline) => {
                break Err(format!(
                    "did not stop within {}s; progress since the last state save will be downloaded again",
                    STOP_TIMEOUT.as_secs()
                ));
            }
        }
    };
    view.bar.finish_and_clear();

    match result {
        Ok(path) => {
            if !ui.quiet {
                ui.print(&done_line(input.as_deref(), &path));
            }
            Outcome::Done
        }
        Err(err) => {
            let interrupted = shutdown.requested().is_some();
            let subject = input.as_deref().unwrap_or(&view.label);
            if interrupted {
                ui.error(&format!("[STOPPED] {}: run the same command again to resume ({})", subject, err));
            } else {
                ui.error(&format!("[FAILED] {}: {}", subject, err));
            }
            if let Some(snapshot) = last.filter(|s| s.target_path.is_some()) {
                let status = if interrupted { HistoryStatus::Cancelled } else { HistoryStatus::Failed(err) };
                record_unfinished(snapshot, urls, status, started_at).await;
            }
            if interrupted { Outcome::Interrupted } else { Outcome::Failed }
        }
    }
}

/// A live recording that has something already (see [`EngineSnapshot::is_recording`]): stopping
/// finishes its file, which takes as long as the file is big, so it gets no [`STOP_TIMEOUT`] (a
/// second Ctrl+C still quits at once). Any other download without a size stops as all others do.
fn is_recording(last: Option<&EngineSnapshot>) -> bool {
    last.is_some_and(EngineSnapshot::is_recording)
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Records a failed or stopped download under its final path, so --history shows it and
/// --verify --repair finds its URLs.
async fn record_unfinished(snapshot: EngineSnapshot, urls: Vec<String>, status: HistoryStatus, started_at: u64) {
    let Some(target) = snapshot.target_path else { return };
    let path = std::path::absolute(&target).unwrap_or(target);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut entry = HistoryEntry::new(name, path, snapshot.total_bytes, urls);
    entry.downloaded_bytes = snapshot.downloaded_bytes;
    entry.status = status;
    entry.started_at = started_at;
    // Read once, under the history lock.
    let history = DownloadHistoryManager::default_history_path();
    let recorded = tokio::task::spawn_blocking(move || DownloadHistoryManager::record(&history, entry)).await;
    if let Err(e) = recorded.map_err(|e| e.to_string()).and_then(|saved| saved.map_err(|e| e.to_string())) {
        tracing::warn!("Could not record the download in history: {}", e);
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Progress display of one download.
struct TaskView {
    bar: ProgressBar,
    label: String,
    sized: bool,
    named: bool,
    stall: StallClock,
    /// When the last plain progress line was printed; None when plain lines are off.
    last_plain: Option<Instant>,
    ui: Ui,
}

impl TaskView {
    fn new(ui: &Ui, label: &str, overall: Option<&ProgressBar>) -> Self {
        Self {
            bar: ui.byte_bar(label, None, overall),
            label: label.to_string(),
            sized: false,
            named: false,
            stall: StallClock::default(),
            last_plain: ui.plain.then(Instant::now),
            ui: ui.clone(),
        }
    }

    fn update(&mut self, s: &EngineSnapshot) {
        let now = Instant::now();
        if !self.named {
            if let Some(name) = s.target_path.as_ref().and_then(|p| p.file_name()) {
                self.label = name.to_string_lossy().into_owned();
                self.bar.set_prefix(truncate(&self.label, 28));
                self.named = true;
            }
        }
        if s.total_bytes > 0 {
            if !self.sized {
                self.bar.set_style(style(SIZED));
                self.sized = true;
            }
            self.bar.set_length(s.total_bytes);
        }
        self.bar.set_position(s.downloaded_bytes);
        let idle = self.stall.idle(s.downloaded_bytes, now);

        let finished = s.total_bytes > 0 && s.downloaded_bytes >= s.total_bytes;
        let stalled = (!finished && idle >= STALL_AFTER).then_some(idle);
        let remaining = (s.total_bytes > 0).then(|| s.total_bytes.saturating_sub(s.downloaded_bytes));
        let status = status_text(s.speed_bytes_per_sec, s.active_workers, remaining, stalled);

        if self.last_plain.is_some_and(|t| now.duration_since(t) >= PLAIN_INTERVAL) {
            let done = done_text(s.downloaded_bytes, s.total_bytes, self.bar.elapsed());
            self.ui.error(&format!("[{}] {}, {}", self.label, done, status));
            self.last_plain = Some(now);
        }
        self.bar.set_message(status);
    }
}

/// How long a download has gone without new bytes. The clock starts at the first snapshot, not
/// when the view was created, so a slow probe or setup does not show as a stall.
#[derive(Default)]
struct StallClock {
    last_bytes: u64,
    last_progress: Option<Instant>,
}

impl StallClock {
    fn idle(&mut self, downloaded: u64, now: Instant) -> Duration {
        let since = match self.last_progress {
            Some(since) if downloaded == self.last_bytes => since,
            _ => now,
        };
        self.last_bytes = downloaded;
        self.last_progress = Some(since);
        now.duration_since(since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The service gives a stopping live recording time to finish its file, which has no
    /// [`STOP_TIMEOUT`] (see [`is_recording`]): systemd kills everything once TimeoutStopSec is up.
    #[test]
    fn the_service_lets_a_stopped_recording_finish() {
        let unit = include_str!("../../../endos-downloader.service");
        let timeout = unit.lines().find_map(|line| line.strip_prefix("TimeoutStopSec=")).unwrap();
        let seconds = match timeout.strip_suffix("min") {
            Some(minutes) => minutes.parse::<u64>().unwrap() * 60,
            None => timeout.trim_end_matches('s').parse().unwrap(),
        };
        assert!(seconds >= 10 * 60, "{timeout}");
    }

    /// Serves `body` at every path over keep-alive HTTP/1.1 (HEAD, and GET with or without a
    /// Range) and counts the connections it accepts.
    async fn keep_alive_server(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let body = body.clone();
                tokio::spawn(async move {
                    let mut pending = Vec::new();
                    let mut buf = [0u8; 4096];
                    loop {
                        let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") else {
                            match socket.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => pending.extend_from_slice(&buf[..n]),
                            }
                            continue;
                        };
                        let head = String::from_utf8_lossy(&pending[..end]).to_ascii_lowercase();
                        pending.drain(..end + 4);
                        let last = body.len() - 1;
                        let range = head.lines().find_map(|l| l.strip_prefix("range: bytes=")).and_then(|r| {
                            let (a, b) = r.trim().split_once('-')?;
                            Some((a.parse::<usize>().ok()?, b.parse::<usize>().map_or(last, |b| b.min(last))))
                        });
                        let (start, end) = range.unwrap_or((0, last));
                        let status = match range {
                            Some(_) => format!("206 Partial Content\r\nContent-Range: bytes {}-{}/{}", start, end, body.len()),
                            None => "200 OK".to_string(),
                        };
                        let mut response = format!(
                            "HTTP/1.1 {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                            status,
                            end + 1 - start
                        )
                        .into_bytes();
                        if !head.starts_with("head ") {
                            response.extend_from_slice(&body[start..=end]);
                        }
                        if socket.write_all(&response).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (format!("http://{}", addr), accepted)
    }

    /// Later downloads of a batch reuse the connections of earlier ones instead of each opening
    /// its own (each download here needs two: the probe's HEAD and GET).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_shares_one_client() {
        let dir = std::env::temp_dir().join(format!("hf-cli-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ENDO_HISTORY_PATH", dir.join("history.json"));
        let body: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let (server, accepted) = keep_alive_server(body.clone()).await;
        let job = |name: &str| Job {
            label: name.to_string(),
            urls: vec![Url::parse(&format!("{}/{}.bin", server, name)).unwrap()],
            options: DownloadOptions { output_path: Some(dir.join("")), ..Default::default() },
            line: None,
        };
        let mut jobs: Vec<Job> = ["a", "b", "c"].iter().map(|name| job(name)).collect();
        // A client that cannot be built fails only the downloads that need it.
        let mut no_cookies = job("d");
        no_cookies.options.cookies_path = Some(dir.join("missing-cookies.txt"));
        jobs.insert(1, no_cookies);
        let failed = run_jobs(jobs, 1, &Ui::new(true), &Shutdown::install(), None).await;
        assert_eq!(failed, 1);
        assert!(!dir.join("d.bin").exists());
        for name in ["a", "b", "c"] {
            assert_eq!(std::fs::read(dir.join(format!("{}.bin", name))).unwrap(), body);
        }
        let accepted = accepted.load(Ordering::SeqCst);
        assert!(accepted < 6, "{} connections for 3 downloads: the later ones did not reuse any", accepted);
    }

    /// Downloads running at once stay within --max-speed together, not each on its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_shares_the_speed_limit() {
        let dir = std::env::temp_dir().join(format!("hf-cli-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ENDO_HISTORY_PATH", dir.join("history.json"));
        let body: Vec<u8> = (0..96 * 1024u32).map(|i| (i % 251) as u8).collect();
        let (server, _) = keep_alive_server(body.clone()).await;
        let limit = 64 * 1024;
        let jobs = ["a", "b"].map(|name| Job {
            label: name.to_string(),
            urls: vec![Url::parse(&format!("{}/{}.bin", server, name)).unwrap()],
            options: DownloadOptions { output_path: Some(dir.join("")), max_speed: Some(limit), ..Default::default() },
            line: None,
        });
        let started = Instant::now();
        assert_eq!(run_jobs(Vec::from(jobs), 2, &Ui::new(true), &Shutdown::install(), None).await, 0);
        // Three seconds' worth of both at the limit, where each at a limit of its own takes half;
        // less the tenth of a second the limit lets through at once.
        let least = Duration::from_secs_f64(2.0 * body.len() as f64 / limit as f64 - 0.5);
        assert!(started.elapsed() >= least, "{:?} for both at {} B/s", started.elapsed(), limit);
        for name in ["a", "b"] {
            assert_eq!(std::fs::read(dir.join(format!("{}.bin", name))).unwrap(), body);
        }
    }

    /// A download that needs ffmpeg waits for the answer whether it may be installed, and one
    /// that does not goes ahead of it; once answered, it starts too.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_downloads_that_need_ffmpeg_wait_for_the_answer() {
        use futures_util::FutureExt;
        let dir = std::env::temp_dir().join(format!("hf-cli-ffmpeg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let body: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let (server, _) = keep_alive_server(body.clone()).await;
        let jobs = || {
            // It fails as it starts, with no cookies file to read.
            let video = Job {
                label: "video".to_string(),
                urls: vec![Url::parse("https://www.youtube.com/watch?v=jNQXAC9IVRw").unwrap()],
                options: DownloadOptions { cookies_path: Some(dir.join("missing-cookies.txt")), ..Default::default() },
                line: None,
            };
            let file = Job {
                label: "file".to_string(),
                urls: vec![Url::parse(&format!("{server}/file.bin")).unwrap()],
                options: DownloadOptions { output_path: Some(dir.join("")), ..Default::default() },
                line: None,
            };
            assert!(video.needs_ffmpeg() && !file.needs_ffmpeg());
            vec![video, file]
        };
        let unanswered: FfmpegAnswer = std::future::pending().boxed().shared();
        let (ui, shutdown) = (Ui::new(true), Shutdown::install());
        let run = run_jobs(jobs(), 1, &ui, &shutdown, Some(unanswered));
        let downloaded = async {
            while !dir.join("file.bin").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let raced = async {
            tokio::select! {
                _ = run => panic!("the run ended while the video waits for the answer"),
                () = downloaded => {}
            }
        };
        tokio::time::timeout(Duration::from_secs(30), raced).await.expect("the file downloads while the video waits");
        assert_eq!(std::fs::read(dir.join("file.bin")).unwrap(), body);
        std::fs::remove_file(dir.join("file.bin")).unwrap();
        let answered: FfmpegAnswer = async { false }.boxed().shared();
        assert_eq!(run_jobs(jobs(), 1, &Ui::new(true), &Shutdown::install(), Some(answered)).await, 1, "the video started, and failed");
    }

    /// What the downloads print while a question waits for its answer (a finished file, a log
    /// line) is held back, so that it neither lands in the question's line nor scrolls it away,
    /// and written in its order once the question is answered.
    #[tokio::test]
    async fn output_waits_for_the_answer_to_a_question() {
        let ui = Ui::new(true);
        let held = || ui.held.lock().unwrap().clone();
        let during = ui
            .hiding_bars(async {
                ui.print("[OK] a.iso");
                ui.log_writer().write_all(b"WARN a log line\n").unwrap();
                ui.error("[FAILED] b.iso: gone");
                held()
            })
            .await;
        let line = |stderr: bool, text: &str| (stderr, text.as_bytes().to_vec());
        assert_eq!(during, Some(vec![line(false, "[OK] a.iso\n"), line(true, "WARN a log line\n"), line(true, "[FAILED] b.iso: gone\n")]));
        assert_eq!(held(), None, "written once answered");
    }

    #[test]
    fn templates_parse() {
        for template in [SIZED, UNSIZED, OVERALL] {
            assert!(ProgressStyle::with_template(template).is_ok(), "{}", template);
        }
    }

    #[test]
    fn status_shows_speed_connections_eta_and_stalls() {
        assert_eq!(status_text(2048.0, 8, Some(4096), None), "2.00 KiB/s, 8 conn, ETA 2s");
        assert_eq!(status_text(0.0, 3, Some(100), None), "0 B/s, 3 conn");
        assert_eq!(status_text(1.0, 1, Some(7200), None), "1 B/s, 1 conn, ETA 2h00m");
        assert_eq!(status_text(50.0, 4, None, None), "50 B/s, 4 conn");
        assert_eq!(
            status_text(9.0, 2, Some(1), Some(Duration::from_secs(75))),
            "STALLED: no data for 1m15s, 2 conn"
        );
    }

    #[test]
    fn a_download_with_no_size_shows_what_came_in_and_for_how_long() {
        // A live recording: no share of a size, but its size and how long it has run.
        assert_eq!(done_text(3 * 1024 * 1024, 0, Duration::from_secs(75)), "3.00 MiB in 1m15s");
        assert_eq!(done_text(250, 1000, Duration::from_secs(75)), "25.0% of 1000 B");
    }

    #[test]
    fn only_a_recording_may_take_its_time_to_stop() {
        let snapshot = |total_bytes, downloaded_bytes| EngineSnapshot {
            total_bytes,
            downloaded_bytes,
            speed_bytes_per_sec: 0.0,
            progress_ratio: 0.0,
            active_workers: 1,
            mirror_speeds: vec![],
            chunks: vec![],
            target_path: None,
        };
        assert!(is_recording(Some(&snapshot(0, 4096))));
        // A server that sends no size: the engine's own download of a file.
        let unsized_file = EngineSnapshot { target_path: Some("file.bin".into()), ..snapshot(0, 4096) };
        for not_one in [Some(snapshot(8192, 4096)), Some(snapshot(0, 0)), Some(unsized_file), None] {
            assert!(!is_recording(not_one.as_ref()), "{not_one:?}");
        }
    }

    #[test]
    fn stall_clock_starts_at_the_first_snapshot() {
        let mut clock = StallClock::default();
        let start = Instant::now() + Duration::from_secs(60);
        assert_eq!(clock.idle(0, start), Duration::ZERO);
        assert_eq!(clock.idle(0, start + Duration::from_secs(6)), Duration::from_secs(6));
        assert_eq!(clock.idle(10, start + Duration::from_secs(7)), Duration::ZERO);
        assert_eq!(clock.idle(10, start + Duration::from_secs(9)), Duration::from_secs(2));
    }

    #[test]
    fn control_characters_are_not_printed() {
        assert_eq!(printable("\u{1b}[31mRED\u{1b}]52;c;x\u{7}\u{9b}.bin"), "?[31mRED?]52;c;x??.bin");
        assert_eq!(printable("\nStopping\tnow"), "\nStopping\tnow");
    }

    #[test]
    fn parallel_results_name_their_input() {
        let urls = [Url::parse("https://cdn.example/dl/a.iso?token=secret#part").unwrap()];
        assert_eq!(input_name(Some(3), &urls).as_deref(), Some("line 3 https://cdn.example/dl/a.iso"));
        assert_eq!(input_name(None, &urls).as_deref(), Some("https://cdn.example/dl/a.iso"));
        let login = [Url::parse("https://alice:s3cret@files.example/a.iso").unwrap()];
        assert_eq!(input_name(Some(1), &login).as_deref(), Some("line 1 https://files.example/a.iso"));
        let user = [Url::parse("https://alice@files.example/a.iso").unwrap()];
        assert_eq!(input_name(None, &user).as_deref(), Some("https://files.example/a.iso"));
        assert_eq!(input_name(Some(7), &[]).as_deref(), Some("line 7"));
        assert_eq!(input_name(None, &[]), None);
        let path = std::path::Path::new("out").join("a.iso");
        let named = done_line(Some("line 3 https://cdn.example/dl/a.iso"), &path);
        assert_eq!(named, format!("[OK] line 3 https://cdn.example/dl/a.iso -> {}", path.display()));
        assert_eq!(done_line(None, &path), format!("[OK] {}", path.display()));
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghijkl", 8), "abcde...");
        assert_eq!(truncate("ääääääääää", 5), "ää...");
    }
}
