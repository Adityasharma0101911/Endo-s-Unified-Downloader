//! Runs downloads with progress output and graceful Ctrl+C / SIGTERM handling.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use hyperfetch_core::engine::{DownloadEngine, DownloadOptions, EngineSnapshot};
use hyperfetch_core::history::{DownloadHistoryManager, HistoryEntry, HistoryStatus};
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
            eprintln!("\nStopping: saving resume state... (press Ctrl+C again to quit immediately)");
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
}

impl Ui {
    pub fn new(quiet: bool) -> Self {
        let multi = if quiet {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        } else {
            MultiProgress::new()
        };
        Self { multi, quiet, plain: !quiet && !std::io::stderr().is_terminal() }
    }

    /// Prints to stdout without tearing the progress bars.
    pub fn print(&self, line: &str) {
        self.multi.suspend(|| println!("{}", line));
    }

    /// Prints to stderr without tearing the progress bars.
    pub fn error(&self, line: &str) {
        self.multi.suspend(|| eprintln!("{}", line));
    }

    /// A writer for log output that keeps the progress bars intact.
    pub fn log_writer(&self) -> LogWriter {
        LogWriter(self.multi.clone())
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
pub struct LogWriter(MultiProgress);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.suspend(|| std::io::stderr().write_all(buf))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

const SIZED: &str = "{spinner:.green} {prefix:28.bold} [{bar:30.cyan/blue}] {bytes:>10}/{total_bytes:<10} {msg}";
const UNSIZED: &str = "{spinner:.green} {prefix:28.bold} {bytes:>10} {msg}";
const OVERALL: &str = "  {prefix:28.bold} [{bar:30.green/white}] {pos}/{len} files {msg}";

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=>-")
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

/// One download to run.
pub struct Job {
    pub label: String,
    pub urls: Vec<Url>,
    pub options: DownloadOptions,
}

enum Outcome {
    Done,
    Failed,
    Interrupted,
}

/// Runs `jobs` with at most `concurrency` at a time and returns how many failed. After a
/// shutdown request no new job starts and running ones stop with their state saved.
pub async fn run_jobs(jobs: Vec<Job>, concurrency: usize, ui: &Ui, shutdown: &Shutdown) -> usize {
    let overall = (jobs.len() > 1 && !ui.quiet).then(|| {
        let bar = ui.multi.add(ProgressBar::new(jobs.len() as u64).with_style(style(OVERALL)).with_prefix("Total"));
        bar.tick();
        bar
    });
    let mut outcomes = futures_util::stream::iter(jobs)
        .map(|job| run_job(job, ui, shutdown, overall.as_ref()))
        .buffer_unordered(concurrency.max(1));
    let mut failed = 0;
    while let Some(outcome) = outcomes.next().await {
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
    drop(outcomes);
    if let Some(bar) = overall {
        bar.abandon();
    }
    failed
}

async fn run_job(job: Job, ui: &Ui, shutdown: &Shutdown, overall: Option<&ProgressBar>) -> Outcome {
    let mut stop = shutdown.subscribe();
    if stop.borrow().is_some() {
        return Outcome::Interrupted;
    }
    let started_at = unix_now();
    let urls: Vec<String> = job.urls.iter().map(Url::to_string).collect();
    let engine = DownloadEngine::new(job.urls, job.options);
    let mut view = TaskView::new(ui, &job.label, overall);

    let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(64);
    let run = engine.run(Some(tx));
    tokio::pin!(run);
    let mut last: Option<EngineSnapshot> = None;
    let mut snapshots_open = true;
    let mut deadline: Option<tokio::time::Instant> = None;
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
            // Keep awaiting run() after cancel: it stops the workers and saves the resume state.
            _ = stop_requested(&mut stop), if deadline.is_none() => {
                engine.cancel();
                view.bar.set_message("stopping, saving resume state...");
                deadline = Some(tokio::time::Instant::now() + STOP_TIMEOUT);
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
                ui.print(&format!("[OK] {}", path.display()));
            }
            Outcome::Done
        }
        Err(err) => {
            let interrupted = shutdown.requested().is_some();
            if interrupted {
                ui.error(&format!("[STOPPED] {}: run the same command again to resume ({})", view.label, err));
            } else {
                ui.error(&format!("[FAILED] {}: {}", view.label, err));
            }
            if let Some(snapshot) = last.filter(|s| s.target_path.is_some()) {
                let status = if interrupted { HistoryStatus::Cancelled } else { HistoryStatus::Failed(err) };
                record_unfinished(snapshot, urls, status, started_at).await;
            }
            if interrupted { Outcome::Interrupted } else { Outcome::Failed }
        }
    }
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
    let recorded = tokio::task::spawn_blocking(move || DownloadHistoryManager::load().add_or_update(entry)).await;
    if let Err(e) = recorded {
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
    last_bytes: u64,
    last_progress: Instant,
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
            last_bytes: 0,
            last_progress: Instant::now(),
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
        if s.downloaded_bytes != self.last_bytes {
            self.last_bytes = s.downloaded_bytes;
            self.last_progress = now;
        }

        let finished = s.total_bytes > 0 && s.downloaded_bytes >= s.total_bytes;
        let idle = now.duration_since(self.last_progress);
        let stalled = (!finished && idle >= STALL_AFTER).then_some(idle);
        let remaining = (s.total_bytes > 0).then(|| s.total_bytes.saturating_sub(s.downloaded_bytes));
        let status = status_text(s.speed_bytes_per_sec, s.active_workers, remaining, stalled);

        if self.last_plain.is_some_and(|t| now.duration_since(t) >= PLAIN_INTERVAL) {
            let done = match remaining {
                Some(_) => format!(
                    "{:.1}% of {}",
                    s.downloaded_bytes as f64 * 100.0 / s.total_bytes as f64,
                    HumanBytes(s.total_bytes)
                ),
                None => HumanBytes(s.downloaded_bytes).to_string(),
            };
            self.ui.error(&format!("[{}] {}, {}", self.label, done, status));
            self.last_plain = Some(now);
        }
        self.bar.set_message(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn truncation_is_char_safe() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghijkl", 8), "abcde...");
        assert_eq!(truncate("ääääääääää", 5), "ää...");
    }
}
