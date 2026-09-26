use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT_ENCODING, ACCEPT_RANGES, AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_RANGE,
    ETAG, LAST_MODIFIED, RANGE,
};
use reqwest::{Client, StatusCode};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::chunk::{ChunkManager, ChunkSnapshot};
use crate::history::{DownloadHistoryManager, HistoryEntry, HistoryStatus};
use crate::hls::HlsError;
use crate::mirror::MirrorRacer;
use crate::range::ByteRange;
use crate::state::DownloadState;
use crate::storage::{DiskWriter, VerifyError};
use crate::worker::{content_length, retry_after, FailureKind, HttpWorker, RateLimiter, WorkerEvent, WorkerShared};

const CANCELLED: &str = "Download cancelled by user";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const PROBE_CONCURRENCY: usize = 8;
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(150);
const PERSIST_INTERVAL: Duration = Duration::from_secs(2);
/// Time constant of the smoothed speed shown to the user.
const SPEED_TAU_SECS: f64 = 2.0;
const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
const DEFAULT_MIN_STEAL: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct EngineSnapshot {
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub speed_bytes_per_sec: f64,
    pub progress_ratio: f64,
    pub active_workers: usize,
    pub mirror_speeds: Vec<(usize, String, f64)>, // (id, host, bytes_per_sec)
    pub chunks: Vec<ChunkSnapshot>,
    pub target_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub num_connections: usize,
    pub base_chunk_size: u64,
    pub min_steal_threshold: u64,
    pub output_path: Option<PathBuf>,
    pub expected_checksum: Option<String>,
    pub cookies_path: Option<PathBuf>,
    pub auth_header: Option<String>,
    pub proxy: Option<String>,
    pub media_preset: Option<crate::media::MediaQualityPreset>,
    pub browser_cookies: Option<crate::media::BrowserCookieSource>,
    /// Global download speed cap in bytes/sec across all connections (None = unlimited).
    pub max_speed: Option<u64>,
    /// Failed attempts allowed per chunk before the download fails. Attempts that made progress don't count.
    pub max_retries: u32,
    /// Seconds without receiving a byte before a connection is treated as stalled and retried.
    pub stall_timeout_secs: u64,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            num_connections: 8,
            base_chunk_size: DEFAULT_CHUNK_SIZE,
            min_steal_threshold: DEFAULT_MIN_STEAL,
            output_path: None,
            expected_checksum: None,
            cookies_path: None,
            auth_header: None,
            proxy: None,
            media_preset: None,
            browser_cookies: None,
            max_speed: None,
            max_retries: 8,
            stall_timeout_secs: 30,
        }
    }
}

#[derive(Clone)]
pub struct DownloadEngine {
    options: DownloadOptions,
    urls: Vec<Url>,
    /// A client that failed to build (bad proxy, header or cookies file) is reported by `run()`
    /// instead of silently downloading without the user's settings.
    client: Result<Client, String>,
    cancel_flag: Arc<AtomicBool>,
    cancel_token: CancellationToken,
}

impl DownloadEngine {
    pub fn new(urls: Vec<Url>, options: DownloadOptions) -> Self {
        let client = build_client(&options);
        Self {
            options,
            urls,
            client,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            cancel_token: CancellationToken::new(),
        }
    }

    /// Requests cancellation; may be called from any clone of the engine, before or during `run()`.
    ///
    /// Callers should keep awaiting `run()` afterwards: it stops every connection, flushes written
    /// data, saves the resume state and returns `Err("Download cancelled by user")`, normally within
    /// about two seconds. Dropping the `run()` future instead skips that final state save, so up to
    /// two seconds of progress would be downloaded again on resume.
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
        self.cancel_token.cancel();
    }

    /// Downloads the engine's URLs and returns the path of the finished file.
    pub async fn run(
        &self,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<PathBuf, String> {
        let client = self.client.clone()?;
        if let Some(expected) = &self.options.expected_checksum {
            crate::storage::validate_checksum(expected)?;
        }
        if self.cancel_token.is_cancelled() {
            return Err(CANCELLED.to_string());
        }

        if let Some(media_url) = self.media_target() {
            return self.run_media(media_url, snapshot_tx).await;
        }

        let resolved = self.resolve_all(&client).await?;

        if let Some(playlist) = resolved.iter().find(|u| u.as_str().contains(".m3u8")) {
            tracing::info!("Detected HLS video stream: {}", playlist);
            let parsed = self
                .guarded(PROBE_TIMEOUT, "fetching the HLS playlist", crate::hls::parse_hls_playlist(&client, playlist))
                .await?;
            match parsed {
                Ok(segments) => {
                    // Same name as last time when its `.part` is this stream, so a retry resumes.
                    let base = self.hls_output_path(playlist, &segments);
                    let out_path = (0..)
                        .map(|n| numbered(&base, n))
                        .find(|c| {
                            !c.exists()
                                && (!part_path(c).exists() || crate::hls::has_resumable_part(c, &segments))
                        })
                        .unwrap_or(base);
                    return crate::hls::HlsEngine::download(
                        &client,
                        segments,
                        &out_path,
                        self.options.num_connections,
                        snapshot_tx,
                        Some(Arc::clone(&self.cancel_flag)),
                    )
                    .await
                    .map_err(|e| e.to_string());
                }
                // Not a usable playlist after all: try it as a plain file.
                Err(e @ (HlsError::InvalidPlaylist(_) | HlsError::NoSegments)) => {
                    tracing::warn!("HLS playlist parsing failed, falling back to direct download: {}", e)
                }
                // A real stream this engine can't handle; downloading the playlist text would not help.
                Err(e) => return Err(format!("{} (try a media preset to use the media engine)", e)),
            }
        }

        let (reference, mirrors) = self.probe_all(&client, &resolved).await?;
        self.download(client, reference, mirrors, snapshot_tx).await
    }

    /// The first URL that should go to yt-dlp: a known media site, or with an explicit media
    /// preset any URL that is not an obvious direct file.
    fn media_target(&self) -> Option<Url> {
        let is_direct_file_or_archive = |u: &Url| -> bool {
            const DIRECT_EXTENSIONS: &[&str] = &[
                ".7z", ".zip", ".rar", ".tar", ".gz", ".bz2", ".xz", ".iso", ".bin", ".exe", ".msi", ".dmg",
                ".pkg", ".deb", ".rpm", ".apk", ".pdf", ".torrent",
            ];
            let path = u.path().to_ascii_lowercase();
            u.host_str().is_some_and(|h| h.ends_with("archive.org"))
                || DIRECT_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
        };
        self.urls.iter().find(|u| crate::media::is_supported_media_site(u)).cloned().or_else(|| {
            if self.options.media_preset.is_some() {
                self.urls.iter().find(|u| !is_direct_file_or_archive(u)).cloned()
            } else {
                None
            }
        })
    }

    async fn run_media(
        &self,
        media_url: Url,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<PathBuf, String> {
        tracing::info!("Routing download to Media Engine: {}", media_url);
        let started_at = unix_now();
        let (prog_tx, mut prog_rx) = mpsc::channel::<crate::media::ProgressUpdate>(64);

        let forwarder = tokio::spawn(async move {
            while let Some(update) = prog_rx.recv().await {
                if let Some(ref tx) = snapshot_tx {
                    let ratio = if update.total > 0 {
                        (update.downloaded as f64 / update.total as f64).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let _ = tx.send(EngineSnapshot {
                        total_bytes: update.total,
                        downloaded_bytes: update.downloaded,
                        speed_bytes_per_sec: update.speed,
                        progress_ratio: ratio,
                        active_workers: update.active_connections,
                        mirror_speeds: vec![],
                        chunks: vec![],
                        target_path: None,
                    });
                }
            }
        });

        let output_dir = match &self.options.output_path {
            Some(p) if p.is_dir() => p.clone(),
            Some(p) => p.parent().unwrap_or(Path::new(".")).to_path_buf(),
            None => PathBuf::from("."),
        };
        let output_filename = match &self.options.output_path {
            Some(p) if !p.is_dir() => p.file_name().map(|n| n.to_string_lossy().to_string()),
            _ => None,
        };
        let cookie_source = if let Some(ref bc) = self.options.browser_cookies {
            bc.clone()
        } else if let Some(ref cp) = self.options.cookies_path {
            crate::media::BrowserCookieSource::File(cp.clone())
        } else {
            crate::media::BrowserCookieSource::None
        };

        let media_opts = crate::media::MediaDownloadOptions {
            preset: self.options.media_preset.clone().unwrap_or_default(),
            cookies: cookie_source,
            proxy: self.options.proxy.clone(),
            output_dir,
            output_filename,
            custom_ytdlp_path: None,
            concurrent_fragments: self.options.num_connections.clamp(1, 32),
        };

        let res = crate::media::download_media(
            &media_url,
            &media_opts,
            Some(prog_tx),
            Some(Arc::clone(&self.cancel_flag)),
        )
        .await;
        let _ = forwarder.await;
        let final_path = res?;

        if let Some(expected) = self.options.expected_checksum.clone() {
            let path = final_path.clone();
            match blocking(move || DiskWriter::verify_file_checksum(&path, &expected)).await? {
                Ok(true) => tracing::info!("Checksum verification passed for {}", final_path.display()),
                Ok(false) => {
                    let expected = self.options.expected_checksum.as_deref().unwrap_or_default();
                    return Err(format!("Checksum verification failed: hash mismatch (expected {})", expected));
                }
                Err(e) => return Err(format!("Checksum verification failed: {}", e)),
            }
        }

        let urls = self.url_strings();
        let path = final_path.clone();
        let _ = blocking(move || {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "media_download".to_string());
            let mut entry = HistoryEntry::new(name, absolute(&path), size, urls);
            entry.downloaded_bytes = size;
            entry.status = HistoryStatus::Completed;
            entry.started_at = started_at;
            entry.completed_at = Some(unix_now());
            DownloadHistoryManager::load().add_or_update(entry);
        })
        .await;

        Ok(final_path)
    }

    /// Resolves every URL into direct mirrors. A resolver error fails the download instead of
    /// downloading a landing page.
    async fn resolve_all(&self, client: &Client) -> Result<Vec<Url>, String> {
        if self.urls.is_empty() {
            return Err("No URLs to download".to_string());
        }
        let mut resolved: Vec<Url> = Vec::new();
        for url in &self.urls {
            let mirrors = self
                .guarded(RESOLVE_TIMEOUT, &format!("resolving {}", url), crate::resolver::SmartResolver::resolve(client, url))
                .await?
                .map_err(|e| format!("Failed to resolve {}: {}", url, e))?;
            for mirror in mirrors {
                if !resolved.contains(&mirror) {
                    resolved.push(mirror);
                }
            }
        }
        Ok(resolved)
    }

    /// Probes all mirrors concurrently; returns the reference probe and the mirrors that serve the same file.
    async fn probe_all(&self, client: &Client, urls: &[Url]) -> Result<(ProbeInfo, Vec<ProbeInfo>), String> {
        // Owned values keep the future `Send` (a borrowing closure here is not general enough).
        let probes = futures_util::stream::iter(urls.to_vec())
            .map(|url| {
                let client = client.clone();
                async move {
                    tokio::time::timeout(PROBE_TIMEOUT, probe_url(&client, &url))
                        .await
                        .unwrap_or_else(|_| Err(format!("{}: no answer within {}s", url, PROBE_TIMEOUT.as_secs())))
                }
            })
            .buffered(PROBE_CONCURRENCY)
            .collect::<Vec<_>>();
        let probes = tokio::select! {
            biased;
            _ = self.cancel_token.cancelled() => return Err(CANCELLED.to_string()),
            probes = probes => probes,
        };
        select_mirrors(probes)
    }

    async fn download(
        &self,
        client: Client,
        reference: ProbeInfo,
        mirrors: Vec<ProbeInfo>,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<PathBuf, String> {
        let started_at = unix_now();
        let base = self.output_path_for(&reference.filename);
        let mut known_urls = self.url_strings();
        for mirror in &mirrors {
            let url = mirror.url.to_string();
            if !known_urls.contains(&url) {
                known_urls.push(url);
            }
        }

        let plan = {
            let (base, remote, urls) = (base.clone(), reference.clone(), known_urls.clone());
            let checksum = self.options.expected_checksum.clone();
            blocking(move || {
                let history = DownloadHistoryManager::load();
                plan_target(&base, &remote, &urls, &history, checksum.as_deref())
            })
            .await?
        };
        let (final_path, resume) = match plan? {
            Plan::AlreadyDone(path) => {
                tracing::info!("{} is already downloaded", path.display());
                emit(&snapshot_tx, || done_snapshot(reference.size.unwrap_or(0), &path));
                return Ok(path);
            }
            Plan::Fetch { final_path, resume } => (final_path, resume),
        };

        let part = part_path(&final_path);
        let state_path = DownloadState::state_file_path(&part);
        let num_workers = self.options.num_connections.clamp(1, 64);
        let chunk_size = effective_chunk_size(reference.size.unwrap_or(0), num_workers as u64, self.options.base_chunk_size);

        let mut state = DownloadState::new(
            file_name_of(&final_path),
            reference.size.unwrap_or(0),
            chunk_size,
            known_urls,
        );
        state.etag = reference.etag.clone();
        state.last_modified = reference.last_modified.clone();
        if let Some(previous) = resume {
            tracing::info!("Resuming {} with {} completed range(s)", part.display(), previous.completed_ranges.len());
            state.completed_ranges = previous.completed_ranges;
        }
        // Claim the .part and record the validators before the first byte arrives.
        {
            let (state, path) = (state.clone(), state_path.clone());
            blocking(move || -> std::io::Result<()> {
                if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                    std::fs::create_dir_all(dir)?;
                }
                state.save_atomic(&path).map_err(std::io::Error::other)
            })
            .await?
            .map_err(|e| format!("Failed to save download state: {}", e))?;
        }

        tracing::info!(
            "Starting download: {} ({:?} bytes, {} mirror(s), ranges: {})",
            final_path.display(),
            reference.size,
            mirrors.len(),
            reference.accepts_ranges
        );
        match reference.size {
            Some(size) if reference.accepts_ranges => {
                self.fetch_ranges(client, size, chunk_size, num_workers, &mirrors, state, &part, &state_path, &final_path, &snapshot_tx)
                    .await?
            }
            _ => self.fetch_stream(&client, &reference, &part, &final_path, &snapshot_tx).await?,
        }
        self.finalize(part, final_path, state_path, started_at, &snapshot_tx).await
    }

    /// Multi-connection download of a file whose size is known and whose server honours ranges.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_ranges(
        &self,
        client: Client,
        size: u64,
        chunk_size: u64,
        num_workers: usize,
        mirrors: &[ProbeInfo],
        state: DownloadState,
        part: &Path,
        state_path: &Path,
        final_path: &Path,
        snapshot_tx: &Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<(), String> {
        let min_steal = if self.options.min_steal_threshold != DEFAULT_MIN_STEAL {
            self.options.min_steal_threshold
        } else {
            (chunk_size / 4).max(DEFAULT_MIN_STEAL)
        };
        let mut manager = ChunkManager::with_resumed_ranges(size, chunk_size, &state.completed_ranges)
            .map_err(|e| e.to_string())?;
        manager.set_max_retries(self.options.max_retries);

        let mut racer = MirrorRacer::new(mirrors.iter().map(|m| m.url.clone()).collect());
        for (mirror, probe) in racer.mirrors_mut().iter_mut().zip(mirrors) {
            mirror.if_range = probe.if_range();
        }

        let writer = {
            let part = part.to_path_buf();
            blocking(move || DiskWriter::open_or_create(&part, size)).await?.map_err(|e| e.to_string())?
        };
        let job = RangeJob {
            chunks: Arc::new(Mutex::new(manager)),
            mirrors: Arc::new(Mutex::new(racer)),
            writer,
            state,
            state_path: state_path.to_path_buf(),
        };

        let mut meter = SpeedMeter::new(job.chunks.lock().total_downloaded());
        emit(snapshot_tx, || job.snapshot(size, &mut meter, final_path));

        let (events_tx, mut events) = mpsc::channel(1024);
        let shared = WorkerShared {
            client,
            writer: job.writer.clone(),
            chunks: Arc::clone(&job.chunks),
            mirrors: Arc::clone(&job.mirrors),
            events: events_tx,
            cancel: self.cancel_token.clone(),
            limiter: self.limiter(),
            file_size: size,
            min_steal,
            stall_timeout: self.stall_timeout(),
        };
        let mut workers = JoinSet::new();
        for worker_id in 0..num_workers {
            workers.spawn(HttpWorker::new(worker_id, shared.clone()).run());
        }
        drop(shared);

        let mut tick = tokio::time::interval(SNAPSHOT_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_persist = Instant::now();
        let outcome = loop {
            if job.chunks.lock().is_all_completed() {
                break Ok(());
            }
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => break Err(CANCELLED.to_string()),
                event = events.recv() => match event {
                    Some(event) => {
                        if let Err(e) = job.handle(event) {
                            break Err(e);
                        }
                    }
                    None => break Err("Download aborted: all workers stopped before the download completed".to_string()),
                },
                _ = tick.tick() => {
                    emit(snapshot_tx, || job.snapshot(size, &mut meter, final_path));
                    if last_persist.elapsed() >= PERSIST_INTERVAL {
                        if let Err(e) = job.persist().await {
                            tracing::warn!("Failed to save resume state: {}", e);
                        }
                        last_persist = Instant::now();
                    }
                }
            }
        };

        // Nothing may keep downloading, or hold the file open, once this returns.
        workers.shutdown().await;
        drop(events);

        match outcome {
            Ok(()) => {
                let writer = job.writer.clone();
                blocking(move || writer.sync())
                    .await?
                    .map_err(|e| format!("Failed to flush download to disk: {}", e))
            }
            Err(e) => {
                if let Err(save_err) = job.persist().await {
                    tracing::warn!("Failed to save resume state: {}", save_err);
                }
                Err(e)
            }
        }
    }

    /// Single-connection download for servers without range support or without a known length.
    /// Such a download cannot resume, so every retry starts from byte 0.
    async fn fetch_stream(
        &self,
        client: &Client,
        remote: &ProbeInfo,
        part: &Path,
        final_path: &Path,
        snapshot_tx: &Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<(), String> {
        let limiter = self.limiter();
        let mut failures = 0u32;
        let mut bad_responses = 0u32;
        loop {
            let (kind, error) = match self.stream_once(client, remote, part, final_path, limiter.as_deref(), snapshot_tx).await {
                Ok(()) => return Ok(()),
                Err(failure) => failure,
            };
            if self.cancel_token.is_cancelled() {
                return Err(CANCELLED.to_string());
            }
            if kind == FailureKind::Fatal {
                return Err(error);
            }
            bad_responses = if kind == FailureKind::BadMirror { bad_responses + 1 } else { 0 };
            if bad_responses >= 2 {
                return Err(format!("Download failed: every mirror failed; last error: {}", error));
            }
            failures += 1;
            if failures > self.options.max_retries {
                return Err(format!("Download failed after {} attempts: {}", failures, error));
            }
            let mut delay = crate::chunk::backoff_delay(failures);
            if let FailureKind::Throttled(Some(after)) = kind {
                delay = delay.max(after);
            }
            tracing::warn!("Download attempt {} failed ({}); restarting in {:?}", failures, error, delay);
            tokio::select! {
                _ = self.cancel_token.cancelled() => return Err(CANCELLED.to_string()),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    async fn stream_once(
        &self,
        client: &Client,
        remote: &ProbeInfo,
        part: &Path,
        final_path: &Path,
        limiter: Option<&RateLimiter>,
        snapshot_tx: &Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<(), (FailureKind, String)> {
        let stall = self.stall_timeout();
        let transient = |msg: String| (FailureKind::Transient, msg);
        let request = client.get(remote.url.clone()).header(ACCEPT_ENCODING, "identity").send();
        let response = tokio::select! {
            biased;
            _ = self.cancel_token.cancelled() => return Err(transient(CANCELLED.to_string())),
            res = tokio::time::timeout(stall, request) => match res {
                Err(_) => return Err(transient(format!("no response within {}s", stall.as_secs()))),
                Ok(Err(e)) => return Err(transient(format!("request failed: {}", e))),
                Ok(Ok(resp)) => resp,
            },
        };
        let status = response.status();
        if !status.is_success() {
            let kind = match status {
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    FailureKind::Throttled(retry_after(response.headers()))
                }
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE => {
                    FailureKind::BadMirror
                }
                _ => FailureKind::Transient,
            };
            return Err((kind, format!("HTTP {}", status)));
        }
        if let (Some(expected), Some(len)) = (remote.size, content_length(response.headers())) {
            if len != expected {
                return Err((FailureKind::Fatal, format!("remote file changed: server now sends {} bytes, expected {}", len, expected)));
            }
        }

        let file = tokio::fs::File::create(part)
            .await
            .map_err(|e| (FailureKind::Fatal, format!("Failed to create {}: {}", part.display(), e)))?;
        let mut out = tokio::io::BufWriter::with_capacity(1 << 20, file);
        let mut stream = response.bytes_stream();
        let mut written: u64 = 0;
        let mut meter = SpeedMeter::new(0);
        let mut tick = tokio::time::interval(SNAPSHOT_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let idle = tokio::time::sleep(stall);
        tokio::pin!(idle);

        loop {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => return Err(transient(CANCELLED.to_string())),
                _ = &mut idle => {
                    return Err(transient(format!("stalled: no data for {}s at offset {}", stall.as_secs(), written)));
                }
                _ = tick.tick() => {
                    emit(snapshot_tx, || stream_snapshot(remote.size, written, meter.update(written), final_path));
                }
                item = stream.next() => {
                    let bytes = match item {
                        None => break,
                        Some(Err(e)) => return Err(transient(format!("read error at offset {}: {}", written, e))),
                        Some(Ok(bytes)) => bytes,
                    };
                    if let Some(limiter) = limiter {
                        tokio::select! {
                            biased;
                            _ = self.cancel_token.cancelled() => return Err(transient(CANCELLED.to_string())),
                            _ = limiter.acquire(bytes.len() as u64) => {}
                        }
                    }
                    idle.as_mut().reset(tokio::time::Instant::now() + stall);
                    out.write_all(&bytes)
                        .await
                        .map_err(|e| (FailureKind::Fatal, format!("Disk write error: {}", e)))?;
                    written += bytes.len() as u64;
                    if remote.size.is_some_and(|size| written > size) {
                        return Err((FailureKind::Fatal, "remote file changed: server sent more data than expected".to_string()));
                    }
                }
            }
        }

        out.flush().await.map_err(|e| (FailureKind::Fatal, format!("Disk write error: {}", e)))?;
        out.get_ref()
            .sync_all()
            .await
            .map_err(|e| (FailureKind::Fatal, format!("Failed to flush download to disk: {}", e)))?;
        match remote.size {
            Some(expected) if written != expected => {
                Err(transient(format!("connection closed after {} of {} bytes", written, expected)))
            }
            _ => Ok(()),
        }
    }

    /// Verifies the finished `.part`, moves it to its final name and records it in history.
    async fn finalize(
        &self,
        part: PathBuf,
        final_path: PathBuf,
        state_path: PathBuf,
        started_at: u64,
        snapshot_tx: &Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<PathBuf, String> {
        let expected = self.options.expected_checksum.clone();
        let hashed = {
            let part = part.clone();
            blocking(move || crate::storage::hash_and_verify_file(&part, expected.as_deref())).await?
        };
        let blake3_hex = match hashed {
            Ok(hash) => hash,
            Err(VerifyError::Mismatch(e)) => {
                // Resuming would only reproduce the same bytes, so start from scratch next time.
                let _ = blocking(move || {
                    let _ = std::fs::remove_file(&part);
                    let _ = DownloadState::remove(&state_path);
                })
                .await;
                return Err(e);
            }
            Err(VerifyError::Io(e)) => return Err(e),
        };

        let (target, size) = blocking(move || -> Result<(PathBuf, u64), String> {
            // Someone may have created the final name while we were downloading.
            let target = if final_path.exists() { free_path(&final_path) } else { final_path };
            let size = std::fs::metadata(&part).map(|m| m.len()).map_err(|e| e.to_string())?;
            std::fs::rename(&part, &target)
                .map_err(|e| format!("Failed to move {} to {}: {}", part.display(), target.display(), e))?;
            if let Err(e) = DownloadState::remove(&state_path) {
                tracing::warn!("Failed to remove {}: {}", state_path.display(), e);
            }
            Ok((target, size))
        })
        .await??;
        tracing::info!("Download completed: {} (BLAKE3 {})", target.display(), blake3_hex);

        let mut entry = HistoryEntry::new(file_name_of(&target), absolute(&target), size, self.url_strings());
        entry.downloaded_bytes = size;
        entry.status = HistoryStatus::Completed;
        entry.blake3_hash = Some(blake3_hex);
        entry.started_at = started_at;
        entry.completed_at = Some(unix_now());
        // Re-read rather than reuse the planning snapshot: the history may have been edited
        // (entries removed, other downloads finished) while this one ran.
        let _ = blocking(move || DownloadHistoryManager::load().add_or_update(entry)).await;

        emit(snapshot_tx, || done_snapshot(size, &target));
        Ok(target)
    }

    /// Runs `fut` with a timeout, returning early if the download is cancelled.
    async fn guarded<T>(&self, limit: Duration, what: &str, fut: impl Future<Output = T>) -> Result<T, String> {
        tokio::select! {
            biased;
            _ = self.cancel_token.cancelled() => Err(CANCELLED.to_string()),
            res = tokio::time::timeout(limit, fut) => {
                res.map_err(|_| format!("Timed out after {}s {}", limit.as_secs(), what))
            }
        }
    }

    fn output_path_for(&self, filename: &str) -> PathBuf {
        match &self.options.output_path {
            Some(p) if p.is_dir() => p.join(filename),
            Some(p) => p.clone(),
            None => PathBuf::from(filename),
        }
    }

    fn hls_output_path(&self, playlist: &Url, segments: &[crate::hls::HlsSegment]) -> PathBuf {
        let ext = crate::hls::container_extension(segments);
        let mut name = PathBuf::from(filename_from_url(playlist).unwrap_or_else(|| "stream".to_string()));
        if name.extension().is_none_or(|e| e == "m3u8") {
            name.set_extension(ext);
        }
        let mut out = self.output_path_for(&name.to_string_lossy());
        if out.extension().is_none() {
            out.set_extension(ext);
        }
        out
    }

    fn url_strings(&self) -> Vec<String> {
        self.urls.iter().map(Url::to_string).collect()
    }

    fn limiter(&self) -> Option<Arc<RateLimiter>> {
        self.options.max_speed.filter(|&s| s > 0).map(|s| Arc::new(RateLimiter::new(s)))
    }

    fn stall_timeout(&self) -> Duration {
        Duration::from_secs(self.options.stall_timeout_secs.max(1))
    }
}

fn build_client(options: &DownloadOptions) -> Result<Client, String> {
    let mut headers = crate::resolver::SmartResolver::default_anti_qos_headers();
    if let Some(auth) = &options.auth_header {
        let value = HeaderValue::from_str(auth).map_err(|_| "Invalid Authorization header value".to_string())?;
        headers.insert(AUTHORIZATION, value);
    }

    let mut builder = Client::builder()
        // One TCP connection per worker: over HTTP/2 every "connection" would be a stream
        // multiplexed onto a single TCP connection, defeating multi-connection downloads.
        .http1_only()
        .tcp_nodelay(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .default_headers(headers);

    if let Some(proxy_url) = &options.proxy {
        let proxy = reqwest::Proxy::all(proxy_url).map_err(|e| format!("Invalid proxy URL {}: {}", proxy_url, e))?;
        builder = builder.proxy(proxy);
    }

    if let Some(cookies_path) = &options.cookies_path {
        let content = std::fs::read_to_string(cookies_path)
            .map_err(|e| format!("Failed to read cookies file {}: {}", cookies_path.display(), e))?;
        let jar = Arc::new(reqwest::cookie::Jar::default());
        crate::resolver::parse_netscape_cookies(&content, &jar);
        builder = builder.cookie_provider(jar);
    }

    builder.build().map_err(|e| format!("Failed to build HTTP client: {}", e))
}

/// Shared state of one multi-connection download.
struct RangeJob {
    chunks: Arc<Mutex<ChunkManager>>,
    mirrors: Arc<Mutex<MirrorRacer>>,
    writer: DiskWriter,
    /// Template for saves; `completed_ranges` is replaced each time.
    state: DownloadState,
    state_path: PathBuf,
}

impl RangeJob {
    /// Records mirror statistics; `Err` ends the download. Workers settle chunk outcomes
    /// themselves, so this only has to notice a fatal failure.
    fn handle(&self, event: WorkerEvent) -> Result<(), String> {
        match event {
            WorkerEvent::Ttfb { mirror_id, ttfb, .. } => {
                if let Some(m) = self.mirrors.lock().get_mirror_mut(mirror_id) {
                    m.record_ttfb(ttfb);
                    m.record_success();
                }
            }
            WorkerEvent::Progress { mirror_id, bytes_received, duration, .. } => {
                if let Some(m) = self.mirrors.lock().get_mirror_mut(mirror_id) {
                    m.record_progress(bytes_received, duration);
                }
            }
            WorkerEvent::ChunkCompleted { .. } => {}
            WorkerEvent::ChunkFailed { chunk_id, mirror_id, kind, error, .. } => {
                tracing::warn!("Chunk {} failed on mirror {} ({:?}): {}", chunk_id, mirror_id, kind, error);
            }
        }
        match self.chunks.lock().has_fatal_failure() {
            Some((_, reason)) => Err(format!("Download failed: {}", reason)),
            None => Ok(()),
        }
    }

    fn snapshot(&self, size: u64, meter: &mut SpeedMeter, target: &Path) -> EngineSnapshot {
        let (downloaded, chunks) = {
            let m = self.chunks.lock();
            (m.total_downloaded(), m.chunk_snapshots())
        };
        let (mirror_speeds, active_workers) = {
            let r = self.mirrors.lock();
            let speeds = r
                .mirrors()
                .iter()
                .map(|m| (m.id, m.url.host_str().unwrap_or("unknown").to_string(), m.speed_ewma))
                .collect();
            (speeds, r.in_flight())
        };
        EngineSnapshot {
            total_bytes: size,
            downloaded_bytes: downloaded,
            speed_bytes_per_sec: meter.update(downloaded),
            progress_ratio: if size == 0 { 1.0 } else { downloaded as f64 / size as f64 },
            active_workers,
            mirror_speeds,
            chunks,
            target_path: Some(target.to_path_buf()),
        }
    }

    /// Snapshots the written ranges, makes those bytes durable, then records exactly those ranges.
    async fn persist(&self) -> Result<(), String> {
        let mut state = self.state.clone();
        state.completed_ranges = self.chunks.lock().completed_ranges();
        let writer = self.writer.clone();
        let path = self.state_path.clone();
        blocking(move || {
            writer.sync().map_err(|e| e.to_string())?;
            state.save_atomic(&path).map_err(|e| e.to_string())
        })
        .await?
    }
}

/// Exponentially smoothed download speed that decays to zero during stalls and never goes negative.
struct SpeedMeter {
    last_bytes: u64,
    last_at: Instant,
    speed: f64,
}

impl SpeedMeter {
    fn new(bytes: u64) -> Self {
        Self { last_bytes: bytes, last_at: Instant::now(), speed: 0.0 }
    }

    fn update(&mut self, bytes: u64) -> f64 {
        self.update_at(bytes, Instant::now())
    }

    fn update_at(&mut self, bytes: u64, now: Instant) -> f64 {
        let dt = now.duration_since(self.last_at).as_secs_f64();
        if dt > 0.0 {
            let instant = bytes.saturating_sub(self.last_bytes) as f64 / dt;
            let alpha = 1.0 - (-dt / SPEED_TAU_SECS).exp();
            self.speed = (self.speed + alpha * (instant - self.speed)).max(0.0);
            self.last_bytes = bytes;
            self.last_at = now;
        }
        self.speed
    }
}

/// What a probe learned about one mirror.
#[derive(Debug, Clone)]
struct ProbeInfo {
    url: Url,
    /// `None` when the server does not tell (chunked responses).
    size: Option<u64>,
    accepts_ranges: bool,
    filename: String,
    etag: Option<String>,
    last_modified: Option<String>,
}

impl ProbeInfo {
    fn strong_etag(&self) -> Option<&str> {
        self.etag.as_deref().filter(|e| !e.starts_with("W/"))
    }

    /// Validator for `If-Range`: weak ETags are not allowed there.
    fn if_range(&self) -> Option<String> {
        self.strong_etag().map(str::to_string).or_else(|| self.last_modified.clone())
    }

    fn absorb(&mut self, headers: &HeaderMap, final_url: &Url) {
        self.filename = extract_filename(headers, final_url);
        let header = |name| headers.get(name).and_then(|v: &HeaderValue| v.to_str().ok()).map(str::to_string);
        self.etag = header(ETAG);
        self.last_modified = header(LAST_MODIFIED);
    }
}

/// Learns size, range support, name and validators with HEAD, confirming range support (and a
/// size HEAD would not give) with a one-byte ranged GET.
async fn probe_url(client: &Client, url: &Url) -> Result<ProbeInfo, String> {
    let mut info = ProbeInfo {
        url: url.clone(),
        size: None,
        accepts_ranges: false,
        filename: extract_filename(&HeaderMap::new(), url),
        etag: None,
        last_modified: None,
    };

    let mut head_len = None;
    match client.head(url.clone()).send().await {
        Ok(resp) if resp.status().is_success() => {
            info.absorb(resp.headers(), resp.url());
            head_len = content_length(resp.headers());
            // A HEAD Content-Length of 0 is common for dynamic content; confirm it below.
            info.size = head_len.filter(|&n| n > 0);
            info.accepts_ranges = accepts_bytes(resp.headers());
            if info.size.is_some() && info.accepts_ranges {
                return Ok(info);
            }
        }
        Ok(resp) => tracing::debug!("HEAD {} returned {}", url, resp.status()),
        Err(e) => tracing::debug!("HEAD {} failed: {}", url, e),
    }

    let resp = client
        .get(url.clone())
        .header(RANGE, "bytes=0-0")
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await;
    let resp = match resp {
        Ok(resp) => resp,
        Err(_) if head_len.is_some() => return Ok(info),
        Err(e) => return Err(format!("{}: {}", url, e)),
    };
    if head_len.is_none() {
        info.absorb(resp.headers(), resp.url());
    }
    let content_range = resp.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok());
    match resp.status() {
        StatusCode::PARTIAL_CONTENT => match content_range.map(ByteRange::parse_content_range) {
            Some(Ok((range, total))) if range.start == 0 => {
                info.accepts_ranges = true;
                info.size = total.or(info.size);
            }
            _ => return Err(format!("{}: invalid Content-Range {:?}", url, content_range)),
        },
        StatusCode::OK => {
            info.accepts_ranges = false;
            info.size = content_length(resp.headers());
        }
        // Only an empty file cannot satisfy bytes=0-0.
        StatusCode::RANGE_NOT_SATISFIABLE => {
            info.size = content_range
                .and_then(|h| h.trim().strip_prefix("bytes */"))
                .and_then(|n| n.parse().ok())
                .or(head_len.filter(|&n| n == 0));
            if info.size.is_none() {
                return Err(format!("{}: 416 for bytes=0-0 without a size", url));
            }
        }
        _ if head_len.is_some() => {}
        status => return Err(format!("{}: HTTP {}", url, status)),
    }
    Ok(info)
}

/// Picks the first successful probe as the reference and keeps the mirrors that serve the same file.
fn select_mirrors(probes: Vec<Result<ProbeInfo, String>>) -> Result<(ProbeInfo, Vec<ProbeInfo>), String> {
    let mut ok = Vec::new();
    let mut errors = Vec::new();
    for probe in probes {
        match probe {
            Ok(info) => ok.push(info),
            Err(e) => {
                tracing::warn!("Mirror probe failed: {}", e);
                errors.push(e);
            }
        }
    }
    let reference = ok
        .first()
        .cloned()
        .ok_or_else(|| format!("Failed to probe file information: {}", errors.join("; ")))?;

    let mirrors = ok
        .into_iter()
        .filter(|m| {
            let mismatch = match (m.strong_etag(), reference.strong_etag()) {
                _ if m.size != reference.size => Some(format!("size {:?} differs from {:?}", m.size, reference.size)),
                (Some(a), Some(b)) if a != b => Some(format!("ETag {} differs from {}", a, b)),
                _ if reference.accepts_ranges && !m.accepts_ranges => Some("no range support".to_string()),
                _ => None,
            };
            if let Some(reason) = &mismatch {
                tracing::warn!("Dropping mirror {}: {}", m.url, reason);
            }
            mismatch.is_none()
        })
        .collect();
    Ok((reference, mirrors))
}

fn accepts_bytes(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT_RANGES)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|unit| unit.trim().eq_ignore_ascii_case("bytes"))
}

/// Where the download goes and whether it can pick up a previous partial download.
#[derive(Debug)]
enum Plan {
    /// This path already holds the exact file.
    AlreadyDone(PathBuf),
    /// Download into `<final_path>.part`, resuming from `resume` if set.
    Fetch { final_path: PathBuf, resume: Option<DownloadState> },
}

/// Chooses the output path. Walks `name`, `name (1)`, `name (2)`... and takes the first one that
/// is already this exact file, holds our resumable (or stale) `.part`, or is free. Existing files
/// and other downloads' `.part` files are never touched.
fn plan_target(
    base: &Path,
    remote: &ProbeInfo,
    urls: &[String],
    history: &DownloadHistoryManager,
    checksum: Option<&str>,
) -> Result<Plan, String> {
    let mut n = 0;
    loop {
        let candidate = numbered(base, n);
        n += 1;
        let part = part_path(&candidate);
        let part_state = DownloadState::state_file_path(&part);

        if candidate.exists() {
            if already_downloaded(&candidate, remote, urls, history, checksum) {
                return Ok(Plan::AlreadyDone(candidate));
            }
            if part.exists() || !migrate_legacy(&candidate, &part, remote, urls)? {
                continue;
            }
        }

        if part.exists() {
            match DownloadState::load_from_path(&part_state).ok().flatten() {
                Some(state) if state.mirrors.iter().any(|m| urls.contains(m)) => {
                    if can_resume(&state, remote, &part) {
                        return Ok(Plan::Fetch { final_path: candidate, resume: Some(state) });
                    }
                    tracing::info!("Discarding stale partial download {}", part.display());
                    std::fs::remove_file(&part).map_err(|e| format!("Failed to remove {}: {}", part.display(), e))?;
                    let _ = DownloadState::remove(&part_state);
                }
                // Another download's partial file.
                _ => continue,
            }
        }

        return Ok(Plan::Fetch { final_path: candidate, resume: None });
    }
}

/// An existing file counts as this download only if the checksum says so, or history recorded
/// this exact path completing from one of these URLs with this size, and the server's
/// Last-Modified is not newer than that download.
fn already_downloaded(
    path: &Path,
    remote: &ProbeInfo,
    urls: &[String],
    history: &DownloadHistoryManager,
    checksum: Option<&str>,
) -> bool {
    if !path.is_file() {
        return false;
    }
    if let Some(expected) = checksum {
        return DiskWriter::verify_file_checksum(path, expected).unwrap_or(false);
    }
    let Some(size) = remote.size else {
        return false;
    };
    if std::fs::metadata(path).map(|m| m.len()).ok() != Some(size) {
        return false;
    }
    let path = absolute(path);
    let modified = remote.last_modified.as_deref().and_then(parse_http_date);
    history.entries().iter().any(|e| {
        e.status == HistoryStatus::Completed
            && absolute(&e.file_path) == path
            && e.file_size == size
            && e.urls.iter().any(|u| urls.contains(u))
            && modified.is_none_or(|lm| lm <= e.started_at)
    }) && header_matches_extension(&path)
}

/// Rejects archives whose magic bytes are wrong (e.g. overwritten or zero-filled since they
/// completed). Other types pass: many valid formats, such as ISO images, start with zeros.
fn header_matches_extension(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let magics: &[&[u8]] = match ext.as_deref() {
        Some("zip") => &[b"PK\x03\x04", b"PK\x05\x06", b"PK\x07\x08"],
        Some("7z") => &[b"7z\xbc\xaf\x27\x1c"],
        _ => return true,
    };
    let mut head = [0u8; 6];
    let n = std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    magics.iter().any(|m| head[..n].starts_with(m))
}

/// Moves an in-progress download from the old layout (`<final>` + `<final>.hfstate`) to the
/// `.part` names. Returns whether it did.
fn migrate_legacy(candidate: &Path, part: &Path, remote: &ProbeInfo, urls: &[String]) -> Result<bool, String> {
    let legacy_state = DownloadState::state_file_path(candidate);
    let Some(state) = DownloadState::load_from_path(&legacy_state).ok().flatten() else {
        return Ok(false);
    };
    let on_disk = std::fs::metadata(candidate).map(|m| m.len()).ok();
    let ours = remote.size == Some(state.file_size)
        && on_disk == Some(state.file_size)
        && state.mirrors.iter().any(|m| urls.contains(m))
        && validators_compatible(&state, remote);
    if !ours {
        return Ok(false);
    }
    tracing::info!("Migrating in-progress download {} to {}", candidate.display(), part.display());
    let fail = |e: std::io::Error| format!("Failed to migrate {}: {}", candidate.display(), e);
    std::fs::rename(candidate, part).map_err(fail)?;
    std::fs::rename(&legacy_state, DownloadState::state_file_path(part)).map_err(fail)?;
    Ok(true)
}

fn can_resume(state: &DownloadState, remote: &ProbeInfo, part: &Path) -> bool {
    remote.accepts_ranges
        && remote.size == Some(state.file_size)
        && validators_compatible(state, remote)
        && std::fs::metadata(part).map(|m| m.len()).ok() == Some(state.file_size)
}

/// If both sides have an ETag they must match; otherwise, if both have Last-Modified, those must.
fn validators_compatible(state: &DownloadState, remote: &ProbeInfo) -> bool {
    match (state.etag.as_deref(), remote.etag.as_deref()) {
        (Some(a), Some(b)) => a == b,
        _ => match (state.last_modified.as_deref(), remote.last_modified.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        },
    }
}

fn effective_chunk_size(file_size: u64, num_workers: u64, configured: u64) -> u64 {
    const MB: u64 = 1024 * 1024;
    if configured != DEFAULT_CHUNK_SIZE {
        configured
    } else if file_size >= 1024 * MB {
        // Multi-GB files: long-lived 64-256MB streams per connection.
        (file_size / (num_workers * 2)).clamp(64 * MB, 256 * MB)
    } else if file_size >= 100 * MB {
        (file_size / (num_workers * 2)).clamp(16 * MB, 64 * MB)
    } else if file_size >= 16 * MB {
        (file_size / num_workers).clamp(4 * MB, 16 * MB)
    } else {
        (file_size / num_workers).max(64 * 1024)
    }
}

/// `base` for `n == 0`, else `stem (n).ext`.
fn numbered(base: &Path, n: usize) -> PathBuf {
    if n == 0 {
        return base.to_path_buf();
    }
    let stem = base.file_stem().map_or_else(|| "file".into(), |s| s.to_string_lossy());
    let name = match base.extension() {
        Some(ext) => format!("{} ({}).{}", stem, n, ext.to_string_lossy()),
        None => format!("{} ({})", stem, n),
    };
    base.with_file_name(name)
}

/// First of `base`, `base (1)`, ... where neither the file nor its `.part` exists.
fn free_path(base: &Path) -> PathBuf {
    (0..)
        .map(|n| numbered(base, n))
        .find(|c| !c.exists() && !part_path(c).exists())
        .unwrap_or_else(|| base.to_path_buf())
}

fn part_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    final_path.with_file_name(name)
}

fn file_name_of(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().to_string()
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("Background task failed: {}", e))
}

fn emit(tx: &Option<broadcast::Sender<EngineSnapshot>>, make: impl FnOnce() -> EngineSnapshot) {
    if let Some(tx) = tx {
        let _ = tx.send(make());
    }
}

fn done_snapshot(size: u64, path: &Path) -> EngineSnapshot {
    EngineSnapshot {
        total_bytes: size,
        downloaded_bytes: size,
        speed_bytes_per_sec: 0.0,
        progress_ratio: 1.0,
        active_workers: 0,
        mirror_speeds: Vec::new(),
        chunks: Vec::new(),
        target_path: Some(path.to_path_buf()),
    }
}

fn stream_snapshot(size: Option<u64>, written: u64, speed: f64, path: &Path) -> EngineSnapshot {
    EngineSnapshot {
        total_bytes: size.unwrap_or(0),
        downloaded_bytes: written,
        speed_bytes_per_sec: speed,
        progress_ratio: size.filter(|&s| s > 0).map_or(0.0, |s| (written as f64 / s as f64).min(1.0)),
        active_workers: 1,
        mirror_speeds: Vec::new(),
        chunks: Vec::new(),
        target_path: Some(path.to_path_buf()),
    }
}

/// File name from Content-Disposition, else the last segment of the (final, post-redirect) URL.
fn extract_filename(headers: &HeaderMap, url: &Url) -> String {
    headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(content_disposition_filename)
        .map(|name| sanitize_filename(&name))
        .filter(|name| !name.is_empty())
        .or_else(|| filename_from_url(url))
        .unwrap_or_else(|| "downloaded_file.bin".to_string())
}

fn filename_from_url(url: &Url) -> Option<String> {
    let last = url.path_segments()?.next_back()?;
    let name = sanitize_filename(&String::from_utf8_lossy(&percent_decode(last)));
    (!name.is_empty()).then_some(name)
}

/// `filename*` (RFC 5987) wins over `filename`; quoted values may contain `;`.
fn content_disposition_filename(header: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for (name, value) in disposition_params(header) {
        if name.eq_ignore_ascii_case("filename*") {
            extended = extended.or_else(|| decode_ext_value(&value));
        } else if name.eq_ignore_ascii_case("filename") {
            plain = plain.or(Some(value));
        }
    }
    extended.or(plain)
}

/// Splits `type; a=b; c="d;e"` into `(name, unquoted value)` pairs.
fn disposition_params(header: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    let Some((_, mut rest)) = header.split_once(';') else {
        return params;
    };
    loop {
        rest = rest.trim_start_matches(|c: char| c == ';' || c.is_whitespace());
        if rest.is_empty() {
            return params;
        }
        let Some(eq) = rest.find('=') else {
            return params;
        };
        if let Some(semi) = rest[..eq].find(';') {
            rest = &rest[semi..]; // a parameter without a value
            continue;
        }
        let name = rest[..eq].trim().to_string();
        let after = rest[eq + 1..].trim_start();
        let (value, remaining) = match after.strip_prefix('"') {
            Some(quoted) => {
                let mut value = String::new();
                let mut end = quoted.len();
                let mut chars = quoted.char_indices();
                while let Some((i, c)) = chars.next() {
                    match c {
                        '\\' => value.extend(chars.next().map(|(_, escaped)| escaped)),
                        '"' => {
                            end = i + 1;
                            break;
                        }
                        c => value.push(c),
                    }
                }
                (value, &quoted[end..])
            }
            None => {
                let (value, remaining) = after.split_once(';').unwrap_or((after, ""));
                (value.trim().to_string(), remaining)
            }
        };
        params.push((name, value));
        rest = remaining;
    }
}

/// Decodes an RFC 5987 `charset'lang'percent-encoded` value (UTF-8 or ISO-8859-1).
fn decode_ext_value(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?;
    let _language = parts.next()?;
    let bytes = percent_decode(parts.next()?);
    if charset.eq_ignore_ascii_case("utf-8") {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    } else if charset.eq_ignore_ascii_case("iso-8859-1") {
        Some(bytes.iter().map(|&b| b as char).collect())
    } else {
        None
    }
}

fn percent_decode(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(value)) => {
                out.push(value);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Makes a server-provided name safe as a single path component on every OS.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 32 => '_',
            c => c,
        })
        .collect();

    let trimmed = cleaned.trim().trim_matches('.').trim().to_string();
    let stem = trimmed.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let is_reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
            | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    );
    if is_reserved {
        format!("_{}", trimmed)
    } else {
        trimmed
    }
}

/// Parses an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) into Unix seconds.
fn parse_http_date(value: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut parts = value.split_whitespace();
    let _weekday = parts.next()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(month_name))? as i64 + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (hms.next()??, hms.next()??, hms.next()??);
    if parts.next()? != "GMT" || !(1..=31).contains(&day) || h > 23 || m > 59 || s > 60 {
        return None;
    }
    // Days from 1970-01-01 to the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + m * 60 + s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn remote(size: u64) -> ProbeInfo {
        ProbeInfo {
            url: Url::parse("http://example.com/file.bin").unwrap(),
            size: Some(size),
            accepts_ranges: true,
            filename: "file.bin".to_string(),
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
        }
    }

    fn urls() -> Vec<String> {
        vec!["http://example.com/file.bin".to_string()]
    }

    #[test]
    fn archive_header_must_match_extension() {
        let dir = tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let p = dir.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        assert!(header_matches_extension(&write("ok.zip", b"PK\x03\x04rest")));
        assert!(header_matches_extension(&write("ok.7z", b"7z\xbc\xaf\x27\x1crest")));
        assert!(!header_matches_extension(&write("zeroed.zip", &[0u8; 64])));
        assert!(!header_matches_extension(&write("short.7z", b"7z")));
        // Formats without a magic check pass, including ones that legitimately start with zeros.
        assert!(header_matches_extension(&write("disk.iso", &[0u8; 64])));
    }

    fn state_for(size: u64, etag: &str, mirrors: Vec<String>) -> DownloadState {
        let mut state = DownloadState::new("file.bin".into(), size, 64 * 1024, mirrors);
        state.etag = Some(etag.to_string());
        state.completed_ranges.push(ByteRange::new(0, size / 2 - 1).unwrap());
        state
    }

    fn completed_entry(path: &Path, size: u64, urls: Vec<String>) -> HistoryEntry {
        let mut entry = HistoryEntry::new(file_name_of(path), absolute(path), size, urls);
        entry.status = HistoryStatus::Completed;
        entry
    }

    #[test]
    fn test_content_disposition_parsing() {
        let cd = |h: &str| content_disposition_filename(h);
        assert_eq!(cd("attachment; filename=\"a;b.zip\"; size=3").as_deref(), Some("a;b.zip"));
        assert_eq!(cd("attachment; filename=plain.bin").as_deref(), Some("plain.bin"));
        assert_eq!(
            cd("attachment; filename=\"fallback.txt\"; filename*=UTF-8''na%C3%AFve%20file.txt").as_deref(),
            Some("naïve file.txt")
        );
        assert_eq!(cd("attachment; filename*=iso-8859-1'en'caf%E9.txt").as_deref(), Some("café.txt"));
        assert_eq!(cd("attachment; filename=\"say \\\"hi\\\".txt\"").as_deref(), Some("say \"hi\".txt"));
        assert_eq!(cd("attachment; foo; filename=x.bin").as_deref(), Some("x.bin"));
        assert_eq!(cd("inline"), None);
    }

    #[test]
    fn test_extract_filename_is_safe() {
        let url = Url::parse("http://example.com/dir/final%20name.iso?x=1").unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(extract_filename(&headers, &url), "final name.iso");

        headers.insert(CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"../../etc/passwd\""));
        let name = extract_filename(&headers, &url);
        assert!(!name.contains('/') && !name.contains('\\') && !name.starts_with('.'), "{name}");

        headers.insert(CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"..\""));
        assert_eq!(extract_filename(&headers, &url), "final name.iso");
        assert_eq!(sanitize_filename("con.txt"), "_con.txt");
        assert_eq!(extract_filename(&HeaderMap::new(), &Url::parse("http://h/").unwrap()), "downloaded_file.bin");
        assert_eq!(String::from_utf8_lossy(&percent_decode("100%-%zz%4")), "100%-%zz%4");
    }

    #[test]
    fn test_numbered_names() {
        let base = Path::new("dir").join("a.bin");
        assert_eq!(numbered(&base, 0), base);
        assert_eq!(numbered(&base, 2), Path::new("dir").join("a (2).bin"));
        assert_eq!(numbered(Path::new("noext"), 1), PathBuf::from("noext (1)"));
        assert_eq!(part_path(&base), Path::new("dir").join("a.bin.part"));
    }

    #[test]
    fn test_parse_http_date() {
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("Tue, 29 Feb 2000 12:00:00 GMT"), Some(951_825_600));
        assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), None);
        assert_eq!(parse_http_date("garbage"), None);
    }

    #[test]
    fn test_speed_meter_smooths_and_decays() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter { last_bytes: 0, last_at: t0, speed: 0.0 };
        let mut speed = 0.0;
        for i in 1..=100u64 {
            speed = meter.update_at(i * 100_000, t0 + Duration::from_millis(100 * i)); // 1 MB/s
        }
        assert!((speed - 1_000_000.0).abs() < 50_000.0, "{speed}");
        // A stall decays towards zero; progress going backwards never yields a negative speed.
        let stalled = meter.update_at(100 * 100_000, t0 + Duration::from_secs(12));
        assert!((0.0..400_000.0).contains(&stalled), "{stalled}");
        assert!(meter.update_at(0, t0 + Duration::from_secs(13)) >= 0.0);
    }

    #[test]
    fn test_select_mirrors_drops_different_files() {
        let mut other_size = remote(2000);
        other_size.url = Url::parse("http://b.example.com/file.bin").unwrap();
        let mut other_etag = remote(1000);
        other_etag.url = Url::parse("http://c.example.com/file.bin").unwrap();
        other_etag.etag = Some("\"v2\"".into());
        let mut weak_etag = remote(1000);
        weak_etag.url = Url::parse("http://d.example.com/file.bin").unwrap();
        weak_etag.etag = Some("W/\"whatever\"".into());
        let mut no_ranges = remote(1000);
        no_ranges.url = Url::parse("http://e.example.com/file.bin").unwrap();
        no_ranges.accepts_ranges = false;

        let (reference, mirrors) = select_mirrors(vec![
            Err("a is down".into()),
            Ok(remote(1000)),
            Ok(other_size),
            Ok(other_etag),
            Ok(weak_etag),
            Ok(no_ranges),
        ])
        .unwrap();
        assert_eq!(reference.url.as_str(), "http://example.com/file.bin");
        let hosts: Vec<_> = mirrors.iter().map(|m| m.url.host_str().unwrap().to_string()).collect();
        assert_eq!(hosts, vec!["example.com", "d.example.com"]);
        assert!(select_mirrors(vec![Err("x".into())]).unwrap_err().contains("x"));
    }

    #[test]
    fn test_plan_never_reuses_unrelated_file() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        std::fs::write(&base, vec![7u8; 1000]).unwrap(); // same name AND same size
        let history = DownloadHistoryManager::load_from_path(&dir.path().join("h.json"));
        match plan_target(&base, &remote(1000), &urls(), &history, None).unwrap() {
            Plan::Fetch { final_path, resume: None } => assert_eq!(final_path, dir.path().join("file (1).bin")),
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(&base).unwrap(), vec![7u8; 1000]);
    }

    #[test]
    fn test_plan_history_requires_exact_path() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        std::fs::write(&base, vec![7u8; 1000]).unwrap();
        let mut history = DownloadHistoryManager::load_from_path(&dir.path().join("h.json"));

        // Same name and URL, but recorded in another directory: not this file.
        let elsewhere = dir.path().join("other").join("file.bin");
        history.add_or_update(completed_entry(&elsewhere, 1000, urls()));
        assert!(matches!(plan_target(&base, &remote(1000), &urls(), &history, None).unwrap(), Plan::Fetch { .. }));

        history.add_or_update(completed_entry(&base, 1000, urls()));
        assert!(matches!(
            plan_target(&base, &remote(1000), &urls(), &history, None).unwrap(),
            Plan::AlreadyDone(p) if p == base
        ));
        // Different size on the server now, or modified after we fetched it: download again.
        assert!(matches!(plan_target(&base, &remote(1001), &urls(), &history, None).unwrap(), Plan::Fetch { .. }));
        let mut newer = remote(1000);
        newer.last_modified = Some("Fri, 01 Jan 2100 00:00:00 GMT".into());
        assert!(matches!(plan_target(&base, &newer, &urls(), &history, None).unwrap(), Plan::Fetch { .. }));
    }

    #[test]
    fn test_plan_checksum_decides_for_existing_file() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        std::fs::write(&base, b"hello").unwrap();
        let history = DownloadHistoryManager::load_from_path(&dir.path().join("h.json"));
        let good = Some("md5:5d41402abc4b2a76b9719d911017c592");
        let bad = Some("md5:00000000000000000000000000000000");
        assert!(matches!(plan_target(&base, &remote(5), &urls(), &history, good).unwrap(), Plan::AlreadyDone(_)));
        assert!(matches!(plan_target(&base, &remote(5), &urls(), &history, bad).unwrap(), Plan::Fetch { .. }));
    }

    #[test]
    fn test_plan_resumes_or_discards_part() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        let part = part_path(&base);
        let part_state = DownloadState::state_file_path(&part);
        let history = DownloadHistoryManager::load_from_path(&dir.path().join("h.json"));

        std::fs::write(&part, vec![1u8; 1000]).unwrap();
        state_for(1000, "\"v1\"", urls()).save_atomic(&part_state).unwrap();
        match plan_target(&base, &remote(1000), &urls(), &history, None).unwrap() {
            Plan::Fetch { final_path, resume: Some(state) } => {
                assert_eq!(final_path, base);
                assert_eq!(state.completed_ranges, vec![ByteRange::new(0, 499).unwrap()]);
            }
            other => panic!("{other:?}"),
        }

        // The server's ETag changed: the partial data is stale and gets thrown away.
        let mut changed = remote(1000);
        changed.etag = Some("\"v2\"".into());
        assert!(matches!(
            plan_target(&base, &changed, &urls(), &history, None).unwrap(),
            Plan::Fetch { resume: None, ref final_path } if *final_path == base
        ));
        assert!(!part.exists() && !part_state.exists());

        // A .part belonging to some other download is left alone and skipped.
        std::fs::write(&part, vec![1u8; 1000]).unwrap();
        state_for(1000, "\"v1\"", vec!["http://other.example/x".into()]).save_atomic(&part_state).unwrap();
        assert!(matches!(
            plan_target(&base, &remote(1000), &urls(), &history, None).unwrap(),
            Plan::Fetch { ref final_path, .. } if *final_path == dir.path().join("file (1).bin")
        ));
        assert!(part.exists());

        // So is a .part without any state.
        std::fs::remove_file(&part_state).unwrap();
        assert!(matches!(
            plan_target(&base, &remote(1000), &urls(), &history, None).unwrap(),
            Plan::Fetch { ref final_path, .. } if *final_path == dir.path().join("file (1).bin")
        ));
    }

    #[test]
    fn test_plan_migrates_legacy_in_progress_download() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        std::fs::write(&base, vec![3u8; 1000]).unwrap();
        state_for(1000, "\"v1\"", urls()).save_atomic(&DownloadState::state_file_path(&base)).unwrap();
        let history = DownloadHistoryManager::load_from_path(&dir.path().join("h.json"));

        match plan_target(&base, &remote(1000), &urls(), &history, None).unwrap() {
            Plan::Fetch { final_path, resume: Some(_) } => assert_eq!(final_path, base),
            other => panic!("{other:?}"),
        }
        assert!(!base.exists());
        assert_eq!(std::fs::read(part_path(&base)).unwrap(), vec![3u8; 1000]);
        assert!(DownloadState::state_file_path(&part_path(&base)).exists());
    }

    #[test]
    fn test_validators_compatible() {
        let state = state_for(1000, "\"v1\"", urls());
        let mut r = remote(1000);
        assert!(validators_compatible(&state, &r));
        r.etag = Some("\"v2\"".into());
        assert!(!validators_compatible(&state, &r));
        r.etag = None;
        r.last_modified = Some("Sun, 06 Nov 1994 08:49:37 GMT".into());
        assert!(validators_compatible(&state, &r), "nothing comparable");
    }

    #[tokio::test]
    async fn test_bad_proxy_is_reported_not_ignored() {
        let options = DownloadOptions { proxy: Some("::not a proxy::".into()), ..Default::default() };
        let engine = DownloadEngine::new(vec![Url::parse("http://127.0.0.1:9/x.bin").unwrap()], options);
        let err = engine.run(None).await.unwrap_err();
        assert!(err.contains("proxy"), "{err}");
    }
}
