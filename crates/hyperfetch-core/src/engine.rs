use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use bytes::Bytes;
use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::header::{
    HeaderMap, HeaderName, ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_RANGE, ETAG, LAST_MODIFIED,
    RANGE,
};
use reqwest::{Client, Response, StatusCode};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::chunk::{ChunkManager, ChunkSnapshot};
use crate::history::{redact_url, DownloadHistoryManager, HistoryEntry, HistoryStatus};
use crate::hls::HlsError;
use crate::hosts::{self, HostKey, HostProfile, HostSlot};
use crate::mirror::MirrorRacer;
use crate::range::{compute_gaps, ByteRange};
use crate::resolver::HtmlVideoResolver;
use crate::state::DownloadState;
use crate::storage::{verify_digest, DiskWriter, FileDigest, StorageError, StreamHasher, VerifyError};
use crate::worker::{
    authorize, content_length, retry_after, Auth, Body, FailureKind, HttpWorker, RateLimiter, Seed, WorkerEvent,
    WorkerShared,
};

const CANCELLED: &str = "Download cancelled by user";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a client keeps a connection whose request ended, for the next request to its host.
pub(crate) const POOL_IDLE: Duration = Duration::from_secs(90);
/// Most connections a client keeps that way per host.
pub(crate) const POOL_MAX_IDLE: usize = 64;
/// Longest a range worker waits between body reads (the stall timeout, if shorter): its retry
/// keeps what arrived and starts at once, so a connection gone quiet is best replaced soon.
const BODY_IDLE: Duration = Duration::from_secs(5);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on one mirror's whole probe: HEAD, the ranged GET's tries and the pauses between them.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_CONCURRENCY: usize = 8;
/// Tries of a probe's ranged GET while the server is busy or the connection fails.
const PROBE_ATTEMPTS: u32 = 3;
/// Longest pause between probe tries, whatever Retry-After asks for.
const PROBE_RETRY_CAP: Duration = Duration::from_secs(5);
/// How long the probe still waits for HEAD once the ranged GET has answered.
const HEAD_GRACE: Duration = Duration::from_millis(500);
/// The first mirror's probe fetches this much of the file: a file this small needs no other
/// request, and a larger one has its start on disk before the workers begin.
const PREFETCH: u64 = 1024 * 1024;
/// Longest the probe reads the start of a file larger than `PREFETCH`: the workers wait for it,
/// and together they fetch those bytes faster than one connection.
const PREFETCH_TIME: Duration = Duration::from_millis(250);
/// Shortest interval over which the probe measures its rate: shorter ones see bursts, not a rate.
const MIN_RATE_TICK: Duration = Duration::from_millis(20);
/// Missing bytes that justify one more connection when the probe measured no rate: for less, its
/// handshakes cost more than it saves. Also the most one connection is asked to carry before
/// another one pays off.
const BYTES_PER_CONNECTION: u64 = 1024 * 1024;
/// Fewest missing bytes that justify one more connection, whatever the probe measured.
const MIN_BYTES_PER_CONNECTION: u64 = 64 * 1024;
/// Fetching the playlists and their AES keys may take this long, besides what one request takes
/// that uses every retry the user allows (see [`crate::hls::FetchPolicy::give_up_after`]).
const HLS_PARSE_TIMEOUT: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(120) };
/// Most links one download follows past those it was given (see `DownloadEngine::follow`).
const MAX_FOLLOWS: usize = 3;
/// Longest file name we create, in bytes: leaves room for " (n)" and ".part.hfstate.tmp" under
/// the 255-byte (Linux) and 255 UTF-16 unit (NTFS) limits.
const MAX_NAME_BYTES: usize = 200;
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(150);
/// Bytes a single stream gathers before writing and hashing them on a blocking thread.
const STREAM_BATCH: usize = 1024 * 1024;
const PERSIST_INTERVAL: Duration = Duration::from_secs(2);
/// Time constant of the smoothed speed shown to the user.
const SPEED_TAU_SECS: f64 = 2.0;
const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
/// Largest default chunk while the file is hashed from its start as that is written.
const IN_ORDER_CHUNK: u64 = 8 * 1024 * 1024;
const DEFAULT_MIN_STEAL: u64 = 1024 * 1024;
/// Most connections one download opens, whatever `num_connections` asks for; also the default
/// per-host budget, so that alone never holds a download back.
const MAX_CONNECTIONS: usize = 64;
/// Fewest bytes a steal takes unless the user set `min_steal_threshold`. Whether a steal pays
/// off is decided by time (see `ChunkManager::steal_work`); this only keeps rates measured over
/// a few packets from splitting off slivers.
const MIN_STEAL: u64 = 64 * 1024;

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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DownloadOptions {
    pub num_connections: usize,
    pub base_chunk_size: u64,
    pub min_steal_threshold: u64,
    /// Where to save. A path that ends with a path separator, or is an existing directory, is a
    /// directory (created if missing) that receives the file under its server-provided name.
    /// Any other path is the output file itself. `None` saves into the working directory.
    pub output_path: Option<PathBuf>,
    pub expected_checksum: Option<String>,
    pub cookies_path: Option<PathBuf>,
    /// Never serialized, so saved options (e.g. a persisted queue) don't leak the credential.
    #[serde(skip)]
    pub auth_header: Option<String>,
    pub proxy: Option<String>,
    pub media_preset: Option<crate::media::MediaQualityPreset>,
    /// The quality of a link that turns out to be media only once it answers (a web page one of
    /// yt-dlp's sites takes, a short link to a media site), when `media_preset` is not set: unlike
    /// that, it sends no link to yt-dlp itself.
    pub page_media_preset: Option<crate::media::MediaQualityPreset>,
    pub browser_cookies: Option<crate::media::BrowserCookieSource>,
    /// The yt-dlp to run for media, in place of the one found or installed. Never saved with the
    /// options: a program to run is not read back from a file.
    #[serde(skip)]
    pub ytdlp_path: Option<PathBuf>,
    /// Global download speed cap in bytes/sec across all connections (None = unlimited).
    pub max_speed: Option<u64>,
    /// Failed attempts allowed per chunk before the download fails. Attempts that made progress don't count.
    pub max_retries: u32,
    /// Seconds without receiving a byte before a connection is treated as stalled and retried.
    pub stall_timeout_secs: u64,
    /// Wait for the finished file to reach the disk before reporting it done. Off, the OS writes
    /// it out on its own schedule, as curl, wget and browsers leave it; the flushes that keep
    /// resume state consistent during the download happen either way.
    pub fsync_on_complete: bool,
    /// Connections all downloads in this process may hold to one host at once (0 = no limit).
    /// When downloads sharing a host set different limits, the smallest nonzero one among those
    /// with a request open or waiting for one applies to all of them. The default is the most
    /// connections one download opens, so only several downloads to one host are held back.
    pub max_connections_per_host: usize,
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
            page_media_preset: None,
            browser_cookies: None,
            ytdlp_path: None,
            max_speed: None,
            max_retries: 8,
            stall_timeout_secs: 30,
            fsync_on_complete: false,
            max_connections_per_host: MAX_CONNECTIONS,
        }
    }
}

/// Everything `build_client` depends on: downloads whose options have equal keys can share one
/// client, and with it open connections and TLS sessions.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClientKey {
    /// Credentials change the redirect policy (no HTTPS to HTTP downgrade).
    credentials: bool,
    proxy: Option<String>,
    cookies_path: Option<PathBuf>,
}

impl ClientKey {
    pub fn of(options: &DownloadOptions) -> Self {
        Self {
            credentials: options.auth_header.is_some(),
            proxy: options.proxy.clone(),
            cookies_path: options.cookies_path.clone(),
        }
    }
}

/// The speed limits of downloads that run at once (a batch, a queue): one per `max_speed`, which
/// every download with that limit shares, so together they stay within it (see
/// [`DownloadEngine::sharing_limit`]).
#[derive(Debug, Default)]
pub struct SharedLimits(Mutex<std::collections::HashMap<u64, Arc<RateLimiter>>>);

#[derive(Clone)]
pub struct DownloadEngine {
    options: DownloadOptions,
    urls: Vec<Url>,
    /// A client that failed to build (bad proxy, header or cookies file) is reported by `run()`
    /// instead of silently downloading without the user's settings.
    client: Result<Client, String>,
    /// Added per request, only for the hosts in `urls`; never a client default header.
    auth: Option<Arc<Auth>>,
    /// The speed limit, one for every connection of the download (and every stream of a media
    /// download, see `download_media_stream`), and for other downloads too (see `sharing_limit`).
    limiter: Option<Arc<RateLimiter>>,
    cancel_flag: Arc<AtomicBool>,
    cancel_token: CancellationToken,
}

impl DownloadEngine {
    pub fn new(urls: Vec<Url>, options: DownloadOptions) -> Self {
        let client = build_client(&options);
        Self::with_client_result(urls, options, client)
    }

    /// Like `new`, but downloads through `client`, which must come from `build_client` with options
    /// of the same `ClientKey`: a batch or queue shares one client so later downloads reuse its
    /// connections. Credentials are still added per request, only for the hosts in `urls`.
    pub fn with_client(urls: Vec<Url>, options: DownloadOptions, client: Client) -> Self {
        Self::with_client_result(urls, options, Ok(client))
    }

    fn with_client_result(urls: Vec<Url>, options: DownloadOptions, client: Result<Client, String>) -> Self {
        // A "leaving this site" link stands for its target, as the front ends' ingest takes it:
        // that is what is downloaded, recorded, and given the credentials.
        let urls: Vec<Url> = urls.into_iter().map(|u| crate::resolver::unwrap_redirect(&u).unwrap_or(u)).collect();
        let auth = options.auth_header.as_deref().map(|value| Auth::new(value, &urls));
        let client = match &auth {
            Some(Err(e)) => Err(e.clone()),
            _ => client,
        };
        Self {
            limiter: options.max_speed.filter(|&s| s > 0).map(|s| Arc::new(RateLimiter::new(s))),
            options,
            urls,
            client,
            auth: auth.and_then(Result::ok).map(Arc::new),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            cancel_token: CancellationToken::new(),
        }
    }

    /// Holds the download to the limit `limits` keeps for its `max_speed`, together with the other
    /// downloads there, instead of to a limit of its own. A download without a limit ignores it.
    pub fn sharing_limit(mut self, limits: &SharedLimits) -> Self {
        if let Some(speed) = self.options.max_speed.filter(|&s| s > 0) {
            let shared = Arc::clone(limits.0.lock().entry(speed).or_insert_with(|| Arc::new(RateLimiter::new(speed))));
            self.limiter = Some(shared);
        }
        self
    }

    /// Requests cancellation; may be called from any clone of the engine, before or during `run()`.
    ///
    /// Callers should keep awaiting `run()` afterwards: it stops every connection, flushes written
    /// data, saves the resume state and returns `Err("Download cancelled by user")`, normally within
    /// about two seconds (longer only while the disk is still flushing written data). Dropping the
    /// `run()` future instead skips that final state save, so up to two seconds of progress would
    /// be downloaded again on resume.
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
            return self.run_media(media_url, None, snapshot_tx).await;
        }

        let resolved = self.resolve_all(&client).await?;
        self.fetch_resolved(client, resolved, snapshot_tx, Route { follows: 0, tried: Vec::new(), scrape: true }).await
    }

    /// Downloads what the resolved mirrors serve: an HLS stream if one is a playlist, else their
    /// file. An answer that lands on a host a resolver takes, on a media site, or on a "leaving
    /// this site" link, is downloaded from there instead (see `follow`). With `route.scrape`, a
    /// web page they answer with is an error when a link shortener or mail scanner showed it
    /// instead of redirecting; any other is looked into (see `look_into_page`): the video it plays
    /// is downloaded in its place, the link it sends the browser on to at once is followed. A page
    /// that leads nowhere is asked of yt-dlp's own sites (see `site_media`), and is downloaded as
    /// it is when none takes it.
    async fn fetch_resolved(
        &self,
        client: Client,
        resolved: Vec<Url>,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
        mut route: Route,
    ) -> Result<PathBuf, String> {
        if let Some(playlist) = resolved.iter().find(|u| u.as_str().contains(".m3u8")) {
            tracing::info!("Detected HLS video stream: {}", playlist);
            let started_at = unix_now();
            let fetch = self.fetch_policy();
            let parsed = self
                .guarded(
                    HLS_PARSE_TIMEOUT.saturating_add(fetch.give_up_after()),
                    "fetching the HLS playlist",
                    crate::hls::parse_hls_playlist(&client, playlist, self.auth.as_deref(), fetch),
                )
                .await?;
            match parsed {
                Ok(segments) => {
                    let base = self.hls_output_path(playlist, &segments);
                    let (path, digest) = self.fetch_hls(&client, playlist, segments, base, snapshot_tx).await?;
                    // Hashed as it was written and, with fsync_on_complete, flushed before it took
                    // its name: nothing is read back or flushed again.
                    return self.finish_external(path, started_at, Some(digest)).await;
                }
                // Fetched, but not a playlist after all: try it as a plain file.
                Err(HlsError::InvalidPlaylist(reason)) => {
                    tracing::warn!("Not an HLS playlist ({}); falling back to a direct download", reason)
                }
                // A real stream; downloading the playlist text instead would only fake a success.
                Err(e) if matches!(e, HlsError::Unsupported(_)) => {
                    return Err(format!("{} (try a media preset to use the media engine)", e))
                }
                Err(e) => return Err(e.to_string()),
            }
        }

        let mut probed = self.probe_all(&client, &resolved).await?;
        let (url, final_url) = (probed.reference.url.clone(), probed.reference.final_url.clone());
        route.tried.push(url.clone());
        // Landing on a "leaving this site" link is landing on its target.
        let lands =
            crate::resolver::lands_elsewhere(&url, &final_url) || crate::resolver::unwrap_redirect(&final_url).is_some();
        if lands && route.goes_on(&final_url, &probed.reference)? {
            // Nothing of the answer is kept: it and the probes still out are given up.
            drop(probed);
            return self.follow(client, final_url, snapshot_tx, route).await;
        }
        if !route.scrape || !HtmlVideoResolver::is_page(&url, &probed.reference.headers) {
            return self.download(client, probed, snapshot_tx).await;
        }
        // Never clicked through, whatever the page holds.
        if let Some(host) = shortener_host(&final_url) {
            return Err(format!(
                "{} showed a page instead of redirecting (a preview or a warning): open the link in your browser",
                host
            ));
        }
        match self.look_into_page(&client, &mut probed).await? {
            Some(Lead::Video(video)) => {
                // Nothing of the page is kept: its answer and the probes still out are given up.
                drop(probed);
                let mut mirrors = Vec::new();
                for mirror in resolved {
                    let mirror = if mirror == url { video.clone() } else { mirror };
                    if !mirrors.contains(&mirror) {
                        mirrors.push(mirror);
                    }
                }
                let route = Route { scrape: false, ..route };
                return Box::pin(self.naming(video).fetch_resolved(client, mirrors, snapshot_tx, route)).await;
            }
            Some(Lead::Refresh(target)) => {
                if route.goes_on(&target, &probed.reference)? {
                    drop(probed);
                    return self.follow(client, target, snapshot_tx, route).await;
                }
            }
            None => {}
        }
        if let Some(found) = self.site_media(&final_url).await? {
            drop(probed);
            return self.naming(final_url.clone()).run_media(final_url, Some(found), snapshot_tx).await;
        }
        self.download(client, probed, snapshot_tx).await
    }

    /// Downloads `target`, where the download's link led (see `fetch_resolved`), in its place: a
    /// media site's link with yt-dlp, any other as its resolver takes it, looked into again if it
    /// answers with a page. A "leaving this site" link is taken as its target, without a request.
    async fn follow(
        &self,
        client: Client,
        mut target: Url,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
        mut route: Route,
    ) -> Result<PathBuf, String> {
        if let Some(inner) = crate::resolver::unwrap_redirect(&target) {
            route.tried.push(std::mem::replace(&mut target, inner));
        }
        tracing::info!("Downloading {} in place of the link that led there", target);
        let engine = self.naming(target.clone());
        if crate::media::is_supported_media_site(&target) {
            return engine.run_media(target, None, snapshot_tx).await;
        }
        let resolved = engine
            .guarded(RESOLVE_TIMEOUT, &format!("resolving {}", target), crate::resolver::SmartResolver::resolve(&client, &target))
            .await?
            .map_err(|e| format!("Failed to resolve {}: {}", target, e))?;
        route.follows += 1;
        route.tried.push(target);
        route.scrape = true;
        Box::pin(engine.fetch_resolved(client, resolved, snapshot_tx, route)).await
    }

    /// This download with `url`, found on the way, among its URLs: its history entry lists it,
    /// as its resume state does, so a repair finds it without the link that led there.
    /// Credentials still go only to the hosts the user named, as they were scoped to when this
    /// engine was built.
    fn naming(&self, url: Url) -> Self {
        let mut engine = self.clone();
        if !engine.urls.contains(&url) {
            engine.urls.push(url);
        }
        engine
    }

    /// What one of yt-dlp's own sites finds at `url`, a web page that leads nowhere by itself (see
    /// `crate::media::find_site_media`). None when none of them takes it, or the one that does
    /// finds nothing there, or yt-dlp cannot be found, installed or run, or takes too long: that
    /// never fails the download. A site that takes it but fails at it does (its video is
    /// private, removed, DRM-protected, ...): the page is not what the link stands for, and
    /// yt-dlp's error says why. Nor is a file-share page, which `check_answer` lets through only
    /// for yt-dlp: any failure to find what it shares fails the download.
    async fn site_media(&self, url: &Url) -> Result<Option<crate::media::Extracted>, String> {
        let options = self.media_options();
        match crate::media::find_site_media(url, &options, Some(Arc::clone(&self.cancel_flag))).await {
            Ok(found) => Ok(Some(found)),
            Err(_) if self.cancel_token.is_cancelled() => Err(CANCELLED.to_string()),
            Err(e) if e == crate::media::DRM_REFUSED => Err(e),
            Err(e) => match crate::resolver::unsupported_share(url) {
                Some(service) => {
                    Err(format!("yt-dlp could not download this {} link, and its page is not the file: {}", service, e))
                }
                None if crate::media::site_failed(&e) => Err(e),
                None => {
                    tracing::info!("No site of yt-dlp's takes {}: {}", url, e);
                    Ok(None)
                }
            },
        }
    }

    /// Where a web page the download's reference answered with leads (see
    /// `HtmlVideoResolver::is_page`): to the video it plays, else to the link it sends the browser
    /// on to at once. It is looked into as the probe brought it (all of it, or all still coming),
    /// else as a new request without Range brings it. A page that could not be read leads
    /// nowhere; an answer still coming is taken from `probed` once read.
    async fn look_into_page(&self, client: &Client, probed: &mut Probed) -> Result<Option<Lead>, String> {
        let reference = &probed.reference;
        let (url, final_url) = (reference.url.clone(), reference.final_url.clone());
        let whole = reference.size == Some(reference.prefetch.len() as u64);
        let prefetch = reference.prefetch.clone();
        let live = probed.live.take();
        let page = async {
            let (response, _slot) = match live {
                _ if whole => return Ok((String::from_utf8_lossy(&prefetch).into_owned(), final_url)),
                Some(Live::Stream { response, slot }) => (response, Some(slot)),
                // Only the start came, or none of it: the page again, all of it, under a slot of
                // its host once the probe's answer gave its own back.
                other => {
                    drop(other);
                    let slot = hosts::acquire(&final_url, self.options.max_connections_per_host).await;
                    let request = authorize(client.get(url.clone()), self.auth.as_deref(), &url).header(ACCEPT_ENCODING, "identity");
                    let response = request.send().await.map_err(|e| e.to_string())?;
                    if !response.status().is_success() {
                        return Err(format!("HTTP {}", response.status()));
                    }
                    (response, Some(slot))
                }
            };
            let page_url = response.url().clone();
            let html = HtmlVideoResolver::read_page(response).await.map_err(|e| e.to_string())?;
            Ok((html, page_url))
        };
        match self.guarded(RESOLVE_TIMEOUT, &format!("reading the page {}", url), page).await {
            Ok(Ok((html, page_url))) => Ok(HtmlVideoResolver::video_in(&html, &page_url)
                .map(Lead::Video)
                .or_else(|| HtmlVideoResolver::meta_refresh(&html, &page_url).map(Lead::Refresh))),
            Err(e) if self.cancel_token.is_cancelled() => Err(e),
            Ok(Err(e)) | Err(e) => {
                tracing::warn!("Could not look into the page {}: {}", url, e);
                Ok(None)
            }
        }
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

    /// What a media download of this one goes by (see `run_media`): its settings, and the quality
    /// asked for, else the one for a link that turns out to be media, else the default.
    fn media_options(&self) -> crate::media::MediaDownloadOptions {
        let (output_dir, output_filename) = match &self.options.output_path {
            Some(p) if is_dir_target(p) => (p.clone(), None),
            Some(p) => (
                p.parent().unwrap_or(Path::new(".")).to_path_buf(),
                p.file_name().map(|n| n.to_string_lossy().to_string()),
            ),
            None => (PathBuf::from("."), None),
        };
        let cookies = if let Some(ref bc) = self.options.browser_cookies {
            bc.clone()
        } else if let Some(ref cp) = self.options.cookies_path {
            crate::media::BrowserCookieSource::File(cp.clone())
        } else {
            crate::media::BrowserCookieSource::None
        };
        let preset = self.options.media_preset.as_ref().or(self.options.page_media_preset.as_ref());
        crate::media::MediaDownloadOptions {
            preset: preset.cloned().unwrap_or_default(),
            cookies,
            proxy: self.options.proxy.clone(),
            output_dir,
            output_filename,
            custom_ytdlp_path: self.options.ytdlp_path.clone(),
            concurrent_fragments: self.options.num_connections.clamp(1, 32),
        }
    }

    /// Downloads `media_url` with yt-dlp, which goes by what it found there moments ago when
    /// that is given (see `site_media`).
    async fn run_media(
        &self,
        media_url: Url,
        extracted: Option<crate::media::Extracted>,
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

        let media_opts = self.media_options();
        {
            let dir = media_opts.output_dir.clone();
            blocking(move || std::fs::create_dir_all(&dir))
                .await?
                .map_err(|e| format!("Failed to create {}: {}", media_opts.output_dir.display(), e))?;
        }

        // yt-dlp finds the streams; this engine downloads them where it can.
        let fetch: &crate::media::StreamFetcher<'_> =
            &|stream, tx, stop| Box::pin(self.download_media_stream(stream, tx, stop));
        let res = crate::media::download_media_with(
            &media_url,
            &media_opts,
            Some(prog_tx),
            Some(Arc::clone(&self.cancel_flag)),
            Some(fetch),
            extracted,
        )
        .await;
        let _ = forwarder.await;
        self.finish_external(res?, started_at, None).await
    }

    /// Downloads one stream of a media download (see `crate::media`) with this download's
    /// settings, but the stream's own client, file and request size. The streams, HLS ones
    /// included, draw on this download's one speed limit together, whatever their sizes (what
    /// yt-dlp downloads itself is not held to it). The stream's key is one of its URLs, so its
    /// resume state and history outlive the stream URL. Cancelling `stop` stops it as `cancel`
    /// stops a download.
    pub(crate) async fn download_media_stream(
        &self,
        stream: crate::media::MediaStream,
        snapshot_tx: broadcast::Sender<EngineSnapshot>,
        stop: CancellationToken,
    ) -> Result<PathBuf, String> {
        let options = DownloadOptions {
            output_path: Some(stream.path.clone()),
            base_chunk_size: stream.chunk_size.unwrap_or(self.options.base_chunk_size),
            // The checksum, the user's credentials and cookies are for the page, not this stream.
            expected_checksum: None,
            cookies_path: None,
            auth_header: None,
            media_preset: None,
            browser_cookies: None,
            fsync_on_complete: false,
            ..self.options.clone()
        };
        let urls = vec![stream.url.clone(), stream.key.clone()];
        let mut engine = DownloadEngine::with_client(urls, options, stream.client.clone());
        engine.limiter = self.limiter.clone();
        let download = engine.fetch_media_stream(&stream, snapshot_tx);
        tokio::pin!(download);
        tokio::select! {
            result = &mut download => result,
            () = stop.cancelled() => {
                engine.cancel();
                download.await
            }
        }
    }

    async fn fetch_media_stream(
        &self,
        stream: &crate::media::MediaStream,
        snapshot_tx: broadcast::Sender<EngineSnapshot>,
    ) -> Result<PathBuf, String> {
        let client = &stream.client;
        if !stream.hls {
            let probed = self.probe_all(client, std::slice::from_ref(&stream.url)).await?;
            return self.download(client.clone(), probed, Some(snapshot_tx)).await;
        }
        // Finished by an earlier attempt whose other streams did not finish: kept for this one,
        // as a stream downloaded over ranges is (see `plan_target`).
        let (base, key) = (stream.path.clone(), stream.key.to_string());
        let history = DownloadHistoryManager::default_history_path();
        if let Some((path, size)) = blocking(move || finished_stream(&base, &key, &history)).await? {
            tracing::info!("{} is already downloaded", path.display());
            emit(&Some(snapshot_tx), || done_snapshot(size, &path));
            return Ok(path);
        }
        let started_at = unix_now();
        let fetch = self.fetch_policy();
        let parsed = self
            .guarded(
                HLS_PARSE_TIMEOUT.saturating_add(fetch.give_up_after()),
                "fetching the HLS playlist",
                crate::hls::parse_hls_playlist(client, &stream.url, None, fetch),
            )
            .await?;
        let segments = parsed.map_err(|e| e.to_string())?;
        let (path, digest) = self.fetch_hls(client, &stream.url, segments, stream.path.clone(), Some(snapshot_tx)).await?;
        // Recorded under the stream's URL and key, as a stream downloaded over ranges is, for the
        // next attempt to find it by its key.
        self.finish_external(path, started_at, Some(digest)).await
    }

    /// Downloads the HLS stream `segments` of `playlist` make, to `base` or the first name after
    /// it that is free or holds this stream's `.part`, and returns the file with the digest taken
    /// while writing it. The claim on the name is held until this returns, whichever way it ends,
    /// and by the stream's writer while that touches the `.part`, even after this future is
    /// dropped.
    async fn fetch_hls(
        &self,
        client: &Client,
        playlist: &Url,
        segments: Vec<crate::hls::HlsSegment>,
        base: PathBuf,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<(PathBuf, FileDigest), String> {
        let (auth, fetch) = (self.auth.as_deref(), self.fetch_policy());
        let cancel_flag = Some(Arc::clone(&self.cancel_flag));
        let (target, claim) = claim_hls_output(client, auth, playlist, &segments, base, fetch, &cancel_flag).await?;
        let claim = Arc::new(claim);
        let target = target.hold(Arc::clone(&claim));
        let options = crate::hls::HlsOptions {
            connections: self.options.num_connections,
            fetch,
            fsync_on_complete: self.options.fsync_on_complete,
            expected_checksum: self.options.expected_checksum.clone(),
            limiter: self.limiter(),
        };
        crate::hls::HlsEngine::download(client, auth, segments, target, &options, snapshot_tx, cancel_flag)
            .await
            .map_err(|e| e.to_string())
    }

    /// Checks the expected checksum of a file another engine (yt-dlp, HLS) finished and records
    /// it in history. `digest`, from an engine that hashed the file as it wrote it (see
    /// [`StreamHasher`]) and flushed it before it took its name as fsync_on_complete asks, spares
    /// reading it back and flushing it again; without one it is read once, off the runtime. A
    /// mismatch is an error; the file is left in place for the user to inspect.
    async fn finish_external(&self, path: PathBuf, started_at: u64, digest: Option<FileDigest>) -> Result<PathBuf, String> {
        let written = digest.map_or(Written::File, Written::Flushed);
        let blake3_hex = self.verify_written(&path, written).await.map_err(|e| e.to_string())?;
        if self.options.expected_checksum.is_some() {
            tracing::info!("Checksum verification passed for {}", path.display());
        }
        self.record_completed(path.clone(), None, blake3_hex, started_at).await;
        Ok(path)
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

    /// Probes all mirrors concurrently; returns the reference probe, the mirrors that serve the
    /// same file and the reference's answer while it is still coming. The mirror that leads is the
    /// first, in URL order, of those whose probes answered (failures aside), the first mirror's
    /// counting from when its answer is in, while its start is read. When that mirror takes ranges
    /// or brought the whole file it is the reference, and the download starts without waiting for
    /// the probes still out, which come with it as `late`: a mirror that answers later, listed
    /// before it or not, only joins if it serves the same file. Otherwise another mirror may have
    /// to lead, so every probe is waited for, and no answer holds its host slot meanwhile: another
    /// probe may need it. A Google Drive answer that is a web page fails its mirror's probe.
    async fn probe_all(&self, client: &Client, urls: &[Url]) -> Result<Probed, String> {
        let (limiter, stall) = (self.limiter(), self.stall_timeout());
        let several_connections = self.options.num_connections > 1;
        let limit = self.options.max_connections_per_host;
        // Whether the first mirror's start is being read, and whether the download went ahead
        // without its answer, which then has no start to read.
        let (reading, started) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        // Owned, as the probes may still be out once this returns.
        let (client, auth, owned) = (client.clone(), self.auth.clone(), urls.to_vec());
        let flags = (Arc::clone(&reading), Arc::clone(&started));
        let mut probes: ProbeStream = futures_util::stream::iter(owned.into_iter().enumerate())
            .map(move |(i, url)| {
                let (client, auth, limiter) = (client.clone(), auth.clone(), limiter.clone());
                let (reading, started) = (Arc::clone(&flags.0), Arc::clone(&flags.1));
                async move {
                    let probe = async {
                        // Only the first mirror fetches the file's start: the download uses one copy.
                        let (mut info, body) = probe_url(&client, auth.as_deref(), &url, i == 0, limit).await?;
                        let live = match body {
                            Some(body) if !started.load(Ordering::Relaxed) => {
                                reading.store(true, Ordering::Relaxed);
                                take_start(&mut info, body, several_connections, limiter.as_deref(), stall).await
                            }
                            _ => None,
                        };
                        // A shortened link that lands on a host a resolver takes is judged there (see
                        // `fetch_resolved`), not as the page it may answer with here. An answer whose
                        // start, when read, shows a file is none, whatever it is labelled.
                        if !crate::resolver::lands_elsewhere(&url, &info.final_url)
                            && !crate::resolver::start_is_no_page(&info.prefetch)
                        {
                            crate::resolver::check_answer(&url, &info.final_url, &info.headers)
                                .map_err(|e| format!("{}: {}", url, e))?;
                        }
                        Ok((info, live))
                    };
                    (i, probe.await)
                }
            })
            .buffer_unordered(PROBE_CONCURRENCY)
            .boxed();
        // Each mirror's probe, in URL order, once it is in.
        let mut done: Vec<Option<Probe>> = urls.iter().map(|_| None).collect();
        let starts = loop {
            let next = tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => return Err(CANCELLED.to_string()),
                next = probes.next() => next,
            };
            let Some((i, probe)) = next else { break false };
            if let Some(slot) = done.get_mut(i) {
                *slot = Some(probe);
            }
            let lead = done.iter().enumerate().find(|&(i, probe)| match probe {
                Some(probe) => probe.is_ok(),
                None => i == 0 && reading.load(Ordering::Relaxed),
            });
            if let Some((_, Some(Ok((info, _))))) = lead {
                if info.accepts_ranges || info.size == Some(info.prefetch.len() as u64) {
                    break true;
                }
            }
            if done.iter().any(Option::is_none) {
                for (_, live) in done.iter_mut().flatten().flatten() {
                    *live = None;
                }
            }
        };
        let late = (starts && done.iter().any(Option::is_none)).then(|| {
            started.store(true, Ordering::Relaxed);
            probes
        });
        // Only the first mirror's answer can still be coming.
        let mut live = None;
        let probes = done
            .into_iter()
            .flatten()
            .map(|probe| {
                probe.map(|(info, answer)| {
                    live = live.take().or(answer);
                    info
                })
            })
            .collect();
        let (reference, mirrors) = select_mirrors(probes)?;
        let live = live.filter(|_| urls.first() == Some(&reference.url));
        Ok(Probed { reference, mirrors, live, late })
    }

    async fn download(
        &self,
        client: Client,
        probed: Probed,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<PathBuf, String> {
        let Probed { reference, mirrors, live, late } = probed;
        // Mirrors answering late only join a download over ranges: otherwise their probes, and
        // the host slots they hold, are given up now.
        let late = late.filter(|_| reference.accepts_ranges && reference.size != Some(reference.prefetch.len() as u64));
        let started_at = unix_now();
        let base = self.output_path_for(&reference.filename);
        // The mirrors, as resolvers made them of the links, among its URLs: a repair of the file
        // asks them, not a page the link was (see `naming`).
        let this = mirrors.iter().fold(self.clone(), |engine, mirror| engine.naming(mirror.url.clone()));
        let known_urls = this.url_strings();

        let plan = {
            let (base, remote, urls) = (base.clone(), reference.clone(), known_urls.clone());
            let checksum = self.options.expected_checksum.clone();
            blocking(move || {
                if let Some(dir) = base.parent().filter(|d| !d.as_os_str().is_empty()) {
                    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {}", dir.display(), e))?;
                }
                let history = DownloadHistoryManager::default_history_path();
                plan_target(&base, &remote, &urls, &history, checksum.as_deref())
            })
            .await?
        };
        let (final_path, resume, claim) = match plan? {
            Plan::AlreadyDone(path) => {
                tracing::info!("{} is already downloaded", path.display());
                emit(&snapshot_tx, || done_snapshot(reference.size.unwrap_or(0), &path));
                return Ok(path);
            }
            Plan::Fetch { final_path, resume, claim } => (final_path, resume, claim),
        };

        let part = part_path(&final_path);
        let state_path = DownloadState::state_file_path(&part);
        let mut state = DownloadState::new(
            file_name_of(&final_path),
            reference.size.unwrap_or(0),
            self.options.base_chunk_size,
            known_urls,
        );
        state.etag = reference.etag.clone();
        state.last_modified = reference.last_modified.clone();
        if let Some(previous) = resume {
            tracing::info!("Resuming {} with {} completed range(s)", part.display(), previous.completed_ranges.len());
            state.completed_ranges = previous.completed_ranges;
        }
        let prefetched = reference.prefetch.clone();
        // The probe brought the whole file: it is written in one go, so there is nothing to resume.
        let whole = reference.size == Some(prefetched.len() as u64);
        // Otherwise record the sources and validators before the first byte arrives.
        if !whole {
            let (state, path) = (state.clone(), state_path.clone());
            blocking(move || state.save_atomic(&path))
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
        let written = match reference.size {
            // A plain write (no sparse file, no preallocation, no flush unless fsync_on_complete
            // asks for one in `finalize`), hashed from memory.
            _ if whole => {
                let (path, expected) = (part.clone(), self.options.expected_checksum.clone());
                let digest = blocking(move || {
                    let written = std::fs::write(&path, &prefetched);
                    if written.is_err() {
                        // Without a state file nothing could resume it.
                        let _ = std::fs::remove_file(&path);
                    }
                    written.map(|()| {
                        let mut hasher = StreamHasher::new(expected.as_deref());
                        hasher.update(&prefetched);
                        hasher.finish()
                    })
                })
                .await?
                .map_err(|e| format!("Failed to write {}: {}", part.display(), e))?;
                Written::Digest(digest)
            }
            Some(size) if reference.accepts_ranges => {
                let probes = Probed { reference, mirrors, live, late };
                let writer =
                    self.fetch_ranges(client, size, probes, state, &part, &state_path, &final_path, &snapshot_tx).await?;
                Written::Writer(writer)
            }
            _ => Written::Digest(self.fetch_stream(&client, &reference, live, &part, &final_path, &snapshot_tx).await?),
        };
        this.finalize(final_path, claim, written, started_at, &snapshot_tx).await
    }

    /// Multi-connection download of a file whose size is known and whose server honours ranges.
    /// The reference's prefetch, the file's first bytes from the probe, counts as downloaded; it
    /// is written wherever the resumed state does not already hold those bytes. The probe's
    /// answer, if still `live`, goes on as the first chunk from where the probe stopped reading,
    /// when those bytes are still missing, its rate watched on until another connection answers
    /// (see `Watched`). The bytes one connection carries in the time another takes to start (the
    /// reference's `per_setup`) justify one connection each, up to the limit and what the
    /// mirrors' hosts take at once. Mirrors whose probes come in `late` join as they do, if they
    /// serve the reference's file. Returns the writer, with what it hashed on the way, for
    /// `finalize`.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_ranges(
        &self,
        client: Client,
        size: u64,
        probed: Probed,
        state: DownloadState,
        part: &Path,
        state_path: &Path,
        final_path: &Path,
        snapshot_tx: &Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<DiskWriter, String> {
        let Probed { reference, mirrors, live, mut late } = probed;
        let prefetch = reference.prefetch.clone();
        let unwritten = compute_gaps(prefetch.len() as u64, &state.completed_ranges);
        let mut have = state.completed_ranges.clone();
        have.extend(ByteRange::from_len(0, prefetch.len() as u64));
        // A connection must carry enough to repay its handshakes: a small file arrives sooner over
        // one connection than over many, unless the server caps each connection's speed.
        let per_connection = reference
            .per_setup
            .map_or(BYTES_PER_CONNECTION, |bytes| bytes.clamp(MIN_BYTES_PER_CONNECTION, BYTES_PER_CONNECTION));
        let remaining: u64 = compute_gaps(size, &have).iter().map(ByteRange::len).sum();
        // Mirrors still to come may bring hosts with room of their own.
        let room = if late.is_some() { usize::MAX } else { host_room(&mirrors, self.options.max_connections_per_host) };
        let max_workers = self.options.num_connections.clamp(1, MAX_CONNECTIONS).min(room).max(1) as u64;
        let num_workers = remaining.div_ceil(per_connection).clamp(1, max_workers) as usize;
        let in_order = crate::storage::hashes_prefix(self.options.expected_checksum.as_deref());
        let chunk_size = effective_chunk_size(remaining, num_workers as u64, self.options.base_chunk_size, in_order);
        let mut manager = ChunkManager::with_resumed_ranges(size, chunk_size, &have).map_err(|e| e.to_string())?;
        manager.set_max_retries(self.options.max_retries);
        // The probe's answer is worker 0's first attempt, its connection's rate watched on while no
        // other connection of the download has an answer.
        let alone = CancellationToken::new();
        let seed = match live {
            Some(Live::Range { response, slot, end, watch }) => mirrors.iter().position(|m| m.url == reference.url).and_then(|mirror_id| {
                let chunk = manager.assign_at(0, mirror_id, prefetch.len() as u64, end)?;
                let (body, url) = (response.bytes_stream().boxed(), reference.final_url.clone());
                let body = match watch {
                    Some(watch) => Watched { body, watch: Some(watch), alone: alone.clone(), url: url.clone() }.boxed(),
                    None => body,
                };
                Some(Seed { chunk, mirror_id, url, body, slot })
            }),
            _ => None,
        };

        let racer = build_racer(&mirrors);

        let writer = {
            let part = part.to_path_buf();
            let (on_disk, checksum) = (state.completed_ranges.clone(), self.options.expected_checksum.clone());
            blocking(move || {
                let writer = DiskWriter::open_or_create(&part, size)?;
                writer.track_digest(&on_disk, checksum.as_deref());
                for gap in unwritten {
                    writer.write_chunk_slice(gap.start, &prefetch[gap.start as usize..=gap.end as usize])?;
                }
                Ok::<_, StorageError>(writer)
            })
            .await?
            .map_err(|e| e.to_string())?
        };
        let mut job = RangeJob {
            chunks: Arc::new(Mutex::new(manager)),
            mirrors: Arc::new(Mutex::new(racer)),
            writer,
            state,
            state_path: state_path.to_path_buf(),
        };

        let mut meter = SpeedMeter::new(job.chunks.lock().total_downloaded());
        emit(snapshot_tx, || job.snapshot(size, &mut meter, final_path));

        let (events_tx, mut events) = mpsc::channel(1024);
        // Workers stop when the user cancels or when this download is over.
        let stop = self.cancel_token.child_token();
        let shared = WorkerShared {
            client,
            auth: self.auth.clone(),
            writer: job.writer.clone(),
            chunks: Arc::clone(&job.chunks),
            mirrors: Arc::clone(&job.mirrors),
            events: events_tx,
            cancel: stop.clone(),
            limiter: self.limiter(),
            file_size: size,
            min_steal: min_steal(&self.options),
            host_limit: self.options.max_connections_per_host,
            stall_timeout: self.stall_timeout(),
            body_idle: self.stall_timeout().min(BODY_IDLE),
        };
        let mut workers = JoinSet::new();
        let seeded = seed.is_some() as usize;
        if let Some(seed) = seed {
            workers.spawn(HttpWorker::new(0, shared.clone()).run(Some(seed)));
        }
        for worker_id in seeded..num_workers {
            workers.spawn(HttpWorker::new(worker_id, shared.clone()).run(None));
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
                probe = next_late(&mut late) => match probe {
                    Some((_, probe)) => job.admit(probe.map(|(info, _)| info), &reference),
                    None => late = None,
                },
                event = events.recv() => match event {
                    Some(event) => {
                        if matches!(event, WorkerEvent::Ttfb { .. }) {
                            alone.cancel();
                        }
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

        // Nothing may keep downloading, or write to or hold the file, once this returns. Every
        // worker writes out what it received and awaits its own disk writes before exiting, so
        // once all have exited no write is in flight. They are not aborted: an aborted worker
        // would leave its blocking write running on its own.
        stop.cancel();
        // Probes still out are given up, and the host slots they hold with them.
        drop((events, late));
        while workers.join_next().await.is_some() {}

        match outcome {
            // No flush here: `finalize` flushes (with fsync_on_complete) while it hashes.
            Ok(()) => Ok(job.writer),
            Err(e) => {
                if let Err(save_err) = job.persist().await {
                    tracing::warn!("Failed to save resume state: {}", save_err);
                }
                Err(e)
            }
        }
    }

    /// Single-connection download for servers without range support or without a known length.
    /// Such a download cannot resume, so every retry starts from byte 0. The probe's answer, if
    /// `live` brought the whole file, is the first try. Returns the digest of the file, taken as
    /// it was written, for `finalize`.
    async fn fetch_stream(
        &self,
        client: &Client,
        remote: &ProbeInfo,
        live: Option<Live>,
        part: &Path,
        final_path: &Path,
        snapshot_tx: &Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<FileDigest, String> {
        let limiter = self.limiter();
        let mut answered = match live {
            Some(Live::Stream { response, slot }) => Some((response, slot)),
            _ => None,
        };
        let mut failures = 0u32;
        let mut bad_responses = 0u32;
        loop {
            let attempt = self.stream_once(client, remote, answered.take(), part, final_path, limiter.as_deref(), snapshot_tx);
            let (kind, error) = match attempt.await {
                Ok(digest) => return Ok(digest),
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

    /// One try at the whole file: `answered`, an answer already in with the host slot its request
    /// holds, or else a new request under a slot of the host the probe was sent on to. The file is
    /// hashed as it is written (see [`InOrderFile`]), from its first byte, as every try starts
    /// there; nothing is flushed here, as `finalize` does that with fsync_on_complete.
    #[allow(clippy::too_many_arguments)]
    async fn stream_once(
        &self,
        client: &Client,
        remote: &ProbeInfo,
        answered: Option<(Response, HostSlot)>,
        part: &Path,
        final_path: &Path,
        limiter: Option<&RateLimiter>,
        snapshot_tx: &Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<FileDigest, (FailureKind, String)> {
        let stall = self.stall_timeout();
        let transient = |msg: String| (FailureKind::Transient, msg);
        let (response, _slot) = match answered {
            Some(answered) => answered,
            None => {
                // Waiting for the host's other requests is no stall.
                let slot = tokio::select! {
                    biased;
                    _ = self.cancel_token.cancelled() => return Err(transient(CANCELLED.to_string())),
                    slot = hosts::acquire(&remote.final_url, self.options.max_connections_per_host) => slot,
                };
                let request = authorize(client.get(remote.url.clone()), self.auth.as_deref(), &remote.url)
                    .header(ACCEPT_ENCODING, "identity")
                    .send();
                let response = tokio::select! {
                    biased;
                    _ = self.cancel_token.cancelled() => return Err(transient(CANCELLED.to_string())),
                    res = tokio::time::timeout(stall, request) => match res {
                        Err(_) => return Err(transient(format!("no response within {}s", stall.as_secs()))),
                        Ok(Err(e)) => return Err(transient(format!("request failed: {}", e))),
                        Ok(Ok(resp)) => resp,
                    },
                };
                (response, slot)
            }
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

        let mut out = InOrderFile::create(part, self.options.expected_checksum.clone())
            .await
            .map_err(|e| (FailureKind::Fatal, format!("Failed to create {}: {}", part.display(), e)))?;
        let disk_error = |e: std::io::Error| (FailureKind::Fatal, format!("Disk write error: {}", e));
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
                    written += bytes.len() as u64;
                    if remote.size.is_some_and(|size| written > size) {
                        return Err((FailureKind::Fatal, "remote file changed: server sent more data than expected".to_string()));
                    }
                    out = out.push(&bytes).await.map_err(disk_error)?;
                }
            }
        }

        let digest = out.finish().await.map_err(disk_error)?;
        match remote.size {
            Some(expected) if written != expected => {
                Err(transient(format!("connection closed after {} of {} bytes", written, expected)))
            }
            _ => Ok(digest),
        }
    }

    /// Verifies the finished `.part` of `final_path`, moves it to its final name and records it in
    /// history. The claim on the name is released only once the file is in place (or discarded).
    async fn finalize(
        &self,
        final_path: PathBuf,
        claim: Claim,
        written: Written,
        started_at: u64,
        snapshot_tx: &Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<PathBuf, String> {
        let part = part_path(&final_path);
        let state_path = DownloadState::state_file_path(&part);
        let blake3_hex = match self.verify_written(&part, written).await {
            Ok(hash) => hash,
            Err(VerifyError::Mismatch(e)) => {
                // Resuming would only reproduce the same bytes, so start from scratch next time.
                let _ = blocking(move || {
                    let _ = std::fs::remove_file(&part);
                    let _ = DownloadState::remove(&state_path);
                    drop(claim);
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
            drop(claim);
            Ok((target, size))
        })
        .await??;
        tracing::info!("Download completed: {} (BLAKE3 {})", target.display(), blake3_hex);
        self.record_completed(target.clone(), Some(size), blake3_hex, started_at).await;

        emit(snapshot_tx, || done_snapshot(size, &target));
        Ok(target)
    }

    /// The BLAKE3 (hex) of the finished file at `path`, once its expected checksum is confirmed.
    /// The digest comes from `written` where it can, else from reading the file; with
    /// fsync_on_complete the file is flushed to disk at the same time (unless its writer already
    /// did, see [`Written::Flushed`]), and both must succeed. A
    /// mismatch found without reading the whole file is confirmed by reading it before it counts:
    /// only the file itself can condemn it.
    async fn verify_written(&self, path: &Path, written: Written) -> Result<String, VerifyError> {
        let expected = self.options.expected_checksum.clone();
        let writer = match &written {
            Written::Writer(writer) => Some(writer.clone()),
            _ => None,
        };
        // Through a handle of its own, as the hash may be reading through the writer's.
        let flush = (self.options.fsync_on_complete && !matches!(written, Written::Flushed(_))).then(|| {
            let (writer, path) = (writer.clone(), path.to_path_buf());
            move || match writer {
                Some(writer) => writer.sync_separately().map_err(|e| e.to_string()),
                None => OpenOptions::new().write(true).open(&path).and_then(|f| f.sync_data()).map_err(|e| e.to_string()),
            }
        });
        let reread = (!matches!(written, Written::File)).then(|| {
            let (path, expected) = (path.to_path_buf(), expected.clone());
            move || {
                tracing::warn!(
                    "The digest of {} taken while writing it does not match; reading it back to be sure",
                    path.display()
                );
                match writer {
                    Some(writer) => checked(&path, writer.full_digest(expected.as_deref()), expected.as_deref()),
                    None => crate::storage::hash_and_verify_file(&path, expected.as_deref()),
                }
            }
        });
        let hash = {
            let path = path.to_path_buf();
            move || match written {
                Written::File => crate::storage::hash_and_verify_file(&path, expected.as_deref()),
                Written::Writer(writer) => checked(&path, writer.digest(expected.as_deref()), expected.as_deref()),
                Written::Digest(digest) | Written::Flushed(digest) => verify_digest(&digest, expected.as_deref()),
            }
        };
        hash_and_flush(hash, flush, reread).await
    }

    /// Records `path` as completed in the download history, which is read only once, under its
    /// lock: it may have been edited (entries removed, other downloads finished) while this
    /// download ran. `size` is looked up if not given.
    async fn record_completed(&self, path: PathBuf, size: Option<u64>, blake3_hex: String, started_at: u64) {
        let urls = self.url_strings();
        let _ = blocking(move || {
            let size = size.unwrap_or_else(|| std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0));
            let mut entry = HistoryEntry::new(file_name_of(&path), absolute(&path), size, urls);
            entry.downloaded_bytes = size;
            entry.status = HistoryStatus::Completed;
            entry.blake3_hash = Some(blake3_hex);
            entry.started_at = started_at;
            entry.completed_at = Some(unix_now());
            let history = DownloadHistoryManager::default_history_path();
            if let Err(e) = DownloadHistoryManager::record(&history, entry) {
                tracing::warn!("Failed to update history file {:?}: {}", history, e);
            }
        })
        .await;
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
            Some(p) if is_dir_target(p) => p.join(filename),
            Some(p) => p.clone(),
            None => PathBuf::from(filename),
        }
    }

    fn hls_output_path(&self, playlist: &Url, segments: &[crate::hls::HlsSegment]) -> PathBuf {
        let ext = crate::hls::container_extension(segments);
        // The playlist URL's extension (.m3u8, .php, ...) says nothing about the media.
        let mut name = PathBuf::from(filename_from_url(playlist).unwrap_or_else(|| "stream".to_string()));
        name.set_extension(ext);
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
        self.limiter.clone()
    }

    fn stall_timeout(&self) -> Duration {
        Duration::from_secs(self.options.stall_timeout_secs.max(1))
    }

    /// How HLS requests wait, retry and share their hosts: as the user set it for every connection.
    fn fetch_policy(&self) -> crate::hls::FetchPolicy {
        crate::hls::FetchPolicy {
            stall_timeout: self.stall_timeout(),
            max_retries: self.options.max_retries,
            host_limit: self.options.max_connections_per_host,
        }
    }
}

/// What a finished download hands `finalize` for the file's digest.
enum Written {
    /// Nothing: the file is read back.
    File,
    /// The writer that wrote it, which hashed most of it on the way (see [`DiskWriter::digest`]).
    Writer(DiskWriter),
    /// Taken while the file was written in order (see [`StreamHasher`]).
    Digest(FileDigest),
    /// A [`Written::Digest`] of a file its writer has already flushed, as fsync_on_complete asks.
    Flushed(FileDigest),
}

/// A file written from its first byte to its last, as a single stream writes it. Pieces gather
/// into batches of `STREAM_BATCH` bytes, each written and then hashed on a blocking thread, so
/// the file's digest is ready once its last byte is written (see [`StreamHasher`]).
struct InOrderFile {
    file: File,
    hasher: StreamHasher,
    batch: Vec<u8>,
}

impl InOrderFile {
    /// Creates `path` (emptying it if it exists); `expected_checksum` says which digests to take.
    async fn create(path: &Path, expected_checksum: Option<String>) -> std::io::Result<Self> {
        let path = path.to_path_buf();
        let file = blocking(move || File::create(&path)).await.map_err(std::io::Error::other)??;
        let hasher = StreamHasher::new(expected_checksum.as_deref());
        Ok(Self { file, hasher, batch: Vec::with_capacity(STREAM_BATCH) })
    }

    /// Appends `bytes`, writing the batch out once it is full.
    async fn push(mut self, bytes: &[u8]) -> std::io::Result<Self> {
        self.batch.extend_from_slice(bytes);
        if self.batch.len() < STREAM_BATCH {
            return Ok(self);
        }
        self.write_batch().await
    }

    async fn write_batch(mut self) -> std::io::Result<Self> {
        blocking(move || {
            std::io::Write::write_all(&mut self.file, &self.batch)?;
            self.hasher.update(&self.batch);
            self.batch.clear();
            Ok(self)
        })
        .await
        .map_err(std::io::Error::other)?
    }

    /// Writes what is left and returns the digest of everything written.
    async fn finish(self) -> std::io::Result<FileDigest> {
        Ok(self.write_batch().await?.hasher.finish())
    }
}

/// Runs `hash` and, if given, `flush` at the same time on blocking threads. A mismatch `hash`
/// reports is checked by `reread`, if given, before it counts. The file passes only if its digest
/// matches and the flush succeeded; one that does not match is a mismatch, flushed or not.
async fn hash_and_flush(
    hash: impl FnOnce() -> Result<String, VerifyError> + Send + 'static,
    flush: Option<impl FnOnce() -> Result<(), String> + Send + 'static>,
    reread: Option<impl FnOnce() -> Result<String, VerifyError> + Send + 'static>,
) -> Result<String, VerifyError> {
    let flushed = async {
        match flush {
            Some(flush) => blocking(flush).await.and_then(|flushed| flushed),
            None => Ok(()),
        }
    };
    let (hashed, flushed) = tokio::join!(blocking(hash), flushed);
    let hashed = match (hashed.map_err(VerifyError::Io)?, reread) {
        (Err(VerifyError::Mismatch(_)), Some(reread)) => blocking(reread).await.map_err(VerifyError::Io)?,
        (hashed, _) => hashed,
    }?;
    flushed.map_err(|e| VerifyError::Io(format!("Failed to flush download to disk: {}", e)))?;
    Ok(hashed)
}

/// Checks `digest`, a digest of the file at `path` (reading it if needed), against `expected`.
fn checked(path: &Path, digest: std::io::Result<FileDigest>, expected: Option<&str>) -> Result<String, VerifyError> {
    let digest = digest.map_err(|e| VerifyError::Io(format!("Failed to read {}: {}", path.display(), e)))?;
    verify_digest(&digest, expected)
}

/// Chooses and claims the HLS output name. Walks `base`, `base (1)`, ... and takes the first name
/// it can claim with no finished file whose `.part` is absent or proven to be this same stream (see
/// [`crate::hls::HlsEngine::prepare`]), so a retry resumes it and never orphans it, while another
/// paused download's `.part` is never touched. A name another running download holds is skipped
/// even when its `.part` is this stream.
async fn claim_hls_output(
    client: &Client,
    auth: Option<&Auth>,
    playlist: &Url,
    segments: &[crate::hls::HlsSegment],
    base: PathBuf,
    fetch: crate::hls::FetchPolicy,
    cancel_flag: &Option<Arc<AtomicBool>>,
) -> Result<(crate::hls::HlsTarget, Claim), String> {
    if let Some(parent) = base.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf) {
        blocking(move || {
            std::fs::create_dir_all(&parent).map_err(|e| format!("Failed to create {}: {}", parent.display(), e))
        })
        .await??;
    }
    let mut n = 0;
    loop {
        let candidate = numbered(&base, n);
        n += 1;
        let path = candidate.clone();
        let claim = blocking(move || Ok::<_, String>(Claim::try_take(&path)?.filter(|_| !path.exists()))).await??;
        let Some(claim) = claim else {
            continue;
        };
        let prepared = crate::hls::HlsEngine::prepare(client, auth, playlist, segments, &candidate, fetch, cancel_flag).await;
        if let Some(target) = prepared.map_err(|e| e.to_string())? {
            return Ok((target, claim));
        }
    }
}

/// The user's Authorization header is deliberately not a default header here: it is added per
/// request, only for the hosts the user named (see [`Auth`]).
/// The HTTP client for downloads with these options; see `ClientKey` for what it depends on.
pub fn build_client(options: &DownloadOptions) -> Result<Client, String> {
    let headers = crate::resolver::SmartResolver::default_anti_qos_headers();

    let mut builder = Client::builder()
        // One TCP connection per worker: over HTTP/2 every "connection" would be a stream
        // multiplexed onto a single TCP connection, defeating multi-connection downloads.
        .http1_only()
        .tcp_nodelay(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(Duration::from_secs(30))
        .pool_max_idle_per_host(POOL_MAX_IDLE)
        .pool_idle_timeout(Some(POOL_IDLE))
        .default_headers(headers);

    if options.auth_header.is_some() {
        // reqwest drops Authorization on a redirect to another host, but keeps it on a
        // same-host redirect from https to http, which would send it in cleartext.
        builder = builder.redirect(reqwest::redirect::Policy::custom(|attempt| {
            let downgrade = attempt.url().scheme() == "http"
                && attempt.previous().last().is_some_and(|prev| prev.scheme() == "https");
            if downgrade {
                attempt.error("refusing an HTTPS to HTTP redirect while sending credentials")
            } else if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }));
    }

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

    /// Adds the mirror a probe that came in after the download started found, if it serves the
    /// file `reference` describes, and keeps its URL for a resume.
    fn admit(&mut self, probe: Result<ProbeInfo, String>, reference: &ProbeInfo) {
        let info = match probe {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!("Mirror probe failed: {}", e);
                return;
            }
        };
        if let Some(reason) = mismatch(&info, reference) {
            tracing::warn!("Dropping mirror {}: {}", info.url, reason);
            return;
        }
        tracing::info!("Mirror {} joins the download", info.url);
        add_mirror(&mut self.mirrors.lock(), &info);
        let url = info.url.to_string();
        if !self.state.mirrors.contains(&url) {
            self.state.mirrors.push(url);
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
    /// Where the probe's answer came from, after redirects.
    final_url: Url,
    /// `None` when the server does not tell (chunked responses).
    size: Option<u64>,
    accepts_ranges: bool,
    filename: String,
    etag: Option<String>,
    last_modified: Option<String>,
    /// The file's first bytes, from the response whose validators these are.
    prefetch: Bytes,
    /// Bytes the probe's connection moves, capped at the rate it was measured or known to be, in
    /// the time a new connection takes to answer. `None` when no cap is known.
    per_setup: Option<u64>,
    /// What a new request to `final_url` waits for its answer, at most: how long the ranged GET's
    /// first try waited, the hops of a redirect included, which chunk requests skip. `None` after
    /// a retry (which may reuse the connection, and so say too little), or when HEAD or a plain
    /// GET had to stand in for it.
    answer_time: Option<Duration>,
    /// The headers of the answer that decided the rest: what it served shows there.
    headers: HeaderMap,
}

impl ProbeInfo {
    fn strong_etag(&self) -> Option<&str> {
        self.etag.as_deref().filter(|e| is_strong(e))
    }

    /// Validator for `If-Range`: weak ETags are not allowed there.
    fn if_range(&self) -> Option<String> {
        self.strong_etag().map(str::to_string).or_else(|| self.last_modified.clone())
    }

    /// Name and validators of `url` from the first of `responses` that has each; without a
    /// Content-Disposition name, the first response's final (post-redirect) URL names the file,
    /// and its headers are kept. Size and range support are left for the caller.
    fn describe(url: &Url, responses: &[&Response]) -> Self {
        let header = |name: HeaderName| {
            responses.iter().find_map(|r| r.headers().get(&name)?.to_str().ok().map(str::to_string))
        };
        let final_url = responses.first().map_or(url, |r| r.url());
        Self {
            url: url.clone(),
            final_url: final_url.clone(),
            size: None,
            accepts_ranges: false,
            filename: extract_filename(responses.iter().map(|r| r.headers()), final_url),
            etag: header(ETAG),
            last_modified: header(LAST_MODIFIED),
            prefetch: Bytes::new(),
            per_setup: None,
            answer_time: None,
            headers: responses.first().map(|r| r.headers().clone()).unwrap_or_default(),
        }
    }
}

/// A probe response whose body is the file, or its start.
struct ProbeBody {
    response: Response,
    /// The host slot the response's request holds.
    slot: HostSlot,
    holds: Holds,
    /// When the answer arrived.
    answered: tokio::time::Instant,
    /// How long the request waited for it, when that was the first try: a retry may reuse the
    /// connection, so its wait says nothing about what a new one needs.
    setup: Option<Duration>,
}

/// What a probe response's body holds for the download.
enum Holds {
    /// The file's first bytes, this many (all of it when that is its size).
    Start(u64),
    /// The whole file, too large to read in advance or of unknown size: what a single stream goes
    /// on with.
    Whole,
}

/// A probe's answer whose body is still coming, with the host slot its request holds.
enum Live {
    /// The rest of a ranged answer: the file's bytes from where the probe stopped reading up to
    /// `end`, and the watch on its connection's rate while that told nothing yet.
    Range { response: Response, slot: HostSlot, end: u64, watch: Option<RateWatch> },
    /// A server ignoring the probe's range sent the whole file, none of it read yet.
    Stream { response: Response, slot: HostSlot },
}

/// A probe's outcome: what it found, and the answer still coming when it is the first mirror's.
type Probe = Result<(ProbeInfo, Option<Live>), String>;

/// Probes as they come in, each with its mirror's place among the URLs probed.
type ProbeStream = futures_util::stream::BoxStream<'static, (usize, Probe)>;

/// What the probes found, for the download.
struct Probed {
    /// The mirror whose answers the download goes by.
    reference: ProbeInfo,
    /// The mirrors serving the reference's file, the reference among them.
    mirrors: Vec<ProbeInfo>,
    /// The reference's probe answer, still coming.
    live: Option<Live>,
    /// The probes still out when the download could start without them.
    late: Option<ProbeStream>,
}

/// Where a web page leads a download (see `DownloadEngine::look_into_page`).
enum Lead {
    /// The video it plays.
    Video(Url),
    /// The link it sends the browser on to at once.
    Refresh(Url),
}

/// How far a download went from the links it was given.
#[derive(Clone, Debug)]
struct Route {
    /// Links followed so far (see `DownloadEngine::follow`).
    follows: usize,
    /// The links whose answers the download had, and those it followed: a link it was given but
    /// never had the answer of (a mirror whose probe lost to the first's) is not among them.
    tried: Vec<Url>,
    /// Whether a web page answered is looked into: not once one led to its video.
    scrape: bool,
}

impl Route {
    /// Whether the download goes on to `target`, where the answer `reference` describes leads: at
    /// most `MAX_FOLLOWS` links past those it was given, and never to one it tried. When it does
    /// not, that answer is judged as `probe_all` leaves out for one that lands elsewhere, and as
    /// if the link had been where it landed, so a page is never saved in place of a file.
    fn goes_on(&self, target: &Url, reference: &ProbeInfo) -> Result<bool, String> {
        if self.follows < MAX_FOLLOWS && !self.tried.contains(target) {
            return Ok(true);
        }
        tracing::warn!("Not following {} to {}: too many links followed, or one tried before", reference.url, target);
        for link in [&reference.url, &reference.final_url] {
            crate::resolver::check_answer(link, &reference.final_url, &reference.headers)
                .map_err(|e| format!("{}: {}", reference.url, e))?;
        }
        Ok(false)
    }
}

/// The host of `url` when a link shortener or a mail link scanner answers there (see
/// `HtmlVideoResolver::SHORTENER_HOSTS`): a page from it is a preview or a warning.
fn shortener_host(url: &Url) -> Option<&str> {
    let host = url.host_str()?;
    let listed = |pattern: &&str| match pattern.split_once('*') {
        Some((head, tail)) => host.len() > head.len() + tail.len() && host.starts_with(head) && host.ends_with(tail),
        None => host == *pattern,
    };
    HtmlVideoResolver::SHORTENER_HOSTS.iter().any(listed).then_some(host)
}

/// Where a `RateWatch` measures a connection's rate from: when its answer arrived.
#[derive(Clone, Copy)]
struct Pace {
    answered: tokio::time::Instant,
    /// What a new connection needs before its first byte: the probe's own wait for an answer.
    setup: Duration,
}

/// Learns size, range support, name and validators. HEAD is only a hint: servers and proxies
/// advertise ranges (and sizes) their GETs do not honour, so a ranged GET always decides range
/// support and size, and supplies the name and validators wherever it has them. HEAD and GET go
/// out together, so probing takes one round trip. A GET still busy or failing after its tries
/// tells nothing, so HEAD's word is taken then (ranges included) and the download itself waits
/// out the busy server; only without HEAD is it an error.
///
/// With `prefetch` the GET asks for the first `PREFETCH` bytes instead of one, and its response
/// comes back too when its body is of use: the start of the file (or all of it, if that small)
/// under the validators found, which must be the response's own, or else the whole file from a
/// server ignoring ranges, for a single stream to take up.
///
/// HEAD and GET each hold a slot of their host's budget under `limit` (see [`crate::hosts`]).
/// The GET waits for its slot before the probe's time runs, since a host busy with other
/// downloads is no dead mirror, and keeps it for as long as its answer is read. HEAD goes out
/// alongside only with a slot free at once: waiting for one while the GET holds another could be
/// waiting for the GET's own. Without one, HEAD goes out after a GET that left no body to read,
/// under the GET's slot, and is left out otherwise.
async fn probe_url(
    client: &Client,
    auth: Option<&Auth>,
    url: &Url,
    prefetch: bool,
    limit: usize,
) -> Result<(ProbeInfo, Option<ProbeBody>), String> {
    let slot = hosts::acquire(url, limit).await;
    tokio::time::timeout(PROBE_TIMEOUT, probe_with(client, auth, url, prefetch, limit, slot))
        .await
        .unwrap_or_else(|_| Err(format!("{}: no answer within {}s", url, PROBE_TIMEOUT.as_secs())))
}

/// `probe_url` once the GET holds `slot`.
async fn probe_with(
    client: &Client,
    auth: Option<&Auth>,
    url: &Url,
    prefetch: bool,
    limit: usize,
    slot: HostSlot,
) -> Result<(ProbeInfo, Option<ProbeBody>), String> {
    let cold = slot.opens_connection();
    let send_head = || async move {
        match authorize(client.head(url.clone()), auth, url).send().await {
            Ok(resp) if resp.status().is_success() => Some(resp),
            Ok(resp) => {
                tracing::debug!("HEAD {} returned {}", url, resp.status());
                None
            }
            Err(e) => {
                tracing::debug!("HEAD {} failed: {}", url, e);
                None
            }
        }
    };
    let head_slot = hosts::try_acquire(url, limit);
    let alongside = head_slot.is_some();
    let head = async {
        let _slot = head_slot?;
        send_head().await
    };
    // The bytes the GET asks for.
    let asked = if prefetch { PREFETCH } else { 1 };
    let range = format!("bytes=0-{}", asked - 1);
    // Also when it answered and, for a first try, how long that took: what a connection needs to start.
    let ranged = async {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let sent_at = tokio::time::Instant::now();
            let sent = authorize(client.get(url.clone()), auth, url)
                .header(RANGE, range.as_str())
                .header(ACCEPT_ENCODING, "identity")
                .send()
                .await;
            let retry_in = match &sent {
                Ok(resp) if is_busy(resp.status()) => Some(retry_after(resp.headers()).unwrap_or_default()),
                Ok(_) => None,
                Err(_) => Some(Duration::ZERO),
            };
            match retry_in {
                Some(after) if attempt < PROBE_ATTEMPTS => {
                    drop(sent);
                    tokio::time::sleep(crate::chunk::backoff_delay(attempt).max(after).min(PROBE_RETRY_CAP)).await;
                }
                _ => {
                    let answered = tokio::time::Instant::now();
                    break (sent, answered, (attempt == 1).then(|| answered - sent_at));
                }
            }
        }
    };
    // Once the GET has answered, a HEAD that is still out only gets a short grace: some servers
    // never answer HEAD, and the GET alone has everything needed. None at all once the GET has
    // everything HEAD could add.
    tokio::pin!(head, ranged);
    let (head, (ranged, answered, setup)) = tokio::select! {
        head = &mut head => (head, ranged.await),
        ranged = &mut ranged => {
            let complete = ranged.0.as_ref().is_ok_and(|r| says_it_all(r.status(), r.headers(), r.url()));
            let head = if complete { None } else { tokio::time::timeout(HEAD_GRACE, head).await.ok().flatten() };
            (head, ranged)
        }
    };
    // Still busy or unreachable after every try: that says nothing about range support.
    let unanswered = ranged.as_ref().map_or(true, |resp| is_busy(resp.status()));
    let usable = ranged.as_ref().is_ok_and(|resp| {
        matches!(resp.status(), StatusCode::PARTIAL_CONTENT | StatusCode::OK | StatusCode::RANGE_NOT_SATISFIABLE)
    });
    // HEAD without a slot of its own goes out now if the GET left nothing to read under its slot:
    // it is all the probe has to go by then.
    let head = match head {
        None if !alongside && !usable => send_head().await,
        head => head,
    };
    // A body is read under the GET's slot, which a redirect moves to the host it came from.
    let body = |response: Response, holds: Holds| {
        let slot = slot_at(slot, response.url(), limit)?;
        Some(ProbeBody { response, slot, holds, answered, setup })
    };
    let head_len = head.as_ref().and_then(|r| content_length(r.headers()));

    let get = match (ranged, &head) {
        (Ok(resp), _) if usable => resp,
        // Go by HEAD. A refused range request means a single stream. An unanswered one is no
        // reason to demote the download to one stream that cannot resume, so range support is as
        // a ranged GET to the host found before, else as HEAD's Accept-Ranges says, and the
        // download retries the busy server as Retry-After and max_retries allow. A HEAD
        // Content-Length of 0 is common for dynamic content: it is no size.
        (_, Some(head)) => {
            let mut info = ProbeInfo::describe(url, &[head]);
            info.size = head_len.filter(|&n| n > 0);
            let ranges = hosts::profile(head.url()).accepts_ranges.unwrap_or_else(|| accepts_bytes(head.headers()));
            info.accepts_ranges = unanswered && info.size.is_some() && ranges;
            return Ok((info, None));
        }
        (Ok(resp), None) if unanswered => return Err(format!("{}: server busy (HTTP {})", url, resp.status())),
        (Err(e), None) => return Err(format!("{}: {}", url, e)),
        // Some servers reject HEAD and any Range header: a plain GET is the last resort.
        (Ok(_), None) => {
            let plain = authorize(client.get(url.clone()), auth, url)
                .header(ACCEPT_ENCODING, "identity")
                .send()
                .await
                .map_err(|e| format!("{}: {}", url, e))?;
            if !plain.status().is_success() {
                return Err(format!("{}: HTTP {}", url, plain.status()));
            }
            let mut info = ProbeInfo::describe(url, &[&plain]);
            info.size = content_length(plain.headers());
            let body = if prefetch { body(plain, whole_file(info.size)) } else { None };
            return Ok((info, body));
        }
    };

    let sources: Vec<&Response> = std::iter::once(&get).chain(head.as_ref()).collect();
    let mut info = ProbeInfo::describe(url, &sources);
    // Chunk requests go straight to the final URL, skipping the hops the probe took to get there:
    // after a redirect this overstates their wait, which errs on the safe side, as a mirror
    // taken for quicker than it is gets its requests taken over and split before they can answer.
    info.answer_time = setup;
    let content_range = get.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok());
    let holds = match get.status() {
        StatusCode::PARTIAL_CONTENT => match content_range.map(ByteRange::parse_content_range) {
            Some(Ok((range, Some(total)))) if range.start == 0 => {
                info.accepts_ranges = true;
                info.size = Some(total);
                Some(Holds::Start(range.len().min(PREFETCH)))
            }
            // `bytes 0-0/*`: the length is unknown, so only a single stream can fetch the file.
            Some(Ok((range, None))) if range.start == 0 => None,
            _ => return Err(format!("{}: invalid Content-Range {:?}", url, content_range)),
        },
        StatusCode::OK => {
            info.size = content_length(get.headers());
            Some(whole_file(info.size))
        }
        // Only an empty file cannot satisfy a range from byte 0.
        _ => {
            info.size = content_range
                .and_then(|h| h.trim().strip_prefix("bytes */"))
                .and_then(|n| n.parse().ok())
                .or(head_len.filter(|&n| n == 0));
            if info.size.is_none() {
                return Err(format!("{}: 416 for {} without a size", url, range));
            }
            None
        }
    };
    // What the host answering the GET was seen to do: honour the range or not, and take this long
    // to answer over a new connection straight to it. An unknown length or an empty file tells
    // neither range support nor its lack, and nor does the whole file for a range that reaches
    // past its end: a server may send that as it is.
    let ranges = match get.status() {
        StatusCode::OK => info.size.is_none_or(|size| size > asked).then_some(false),
        _ => info.accepts_ranges.then_some(true),
    };
    let setup_time = info.answer_time.filter(|_| cold && info.final_url == *url);
    hosts::record(get.url(), HostProfile { accepts_ranges: ranges, setup_time, ..Default::default() });

    let own = |name: HeaderName| get.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let own_validators = own(ETAG) == info.etag && own(LAST_MODIFIED) == info.last_modified;
    // A stream that cannot resume goes by no validator.
    let body = holds
        .filter(|holds| prefetch && (own_validators || matches!(holds, Holds::Whole)))
        .and_then(|holds| body(get, holds));
    Ok((info, body))
}

/// What a 200's body, the whole file, holds: read in advance when that is small enough.
fn whole_file(size: Option<u64>) -> Holds {
    match size {
        Some(size) if size <= PREFETCH => Holds::Start(size),
        _ => Holds::Whole,
    }
}

/// `slot` for a request whose answer came from `url`: a redirect to another host moves the
/// request to that host's budget, if it has room; without room the answer is not read.
fn slot_at(slot: HostSlot, url: &Url, limit: usize) -> Option<HostSlot> {
    if *slot.host() == HostKey::of(url) {
        Some(slot)
    } else {
        hosts::try_acquire(url, limit)
    }
}

/// The racer over the mirrors serving the download. Requests go straight to where each probe was
/// redirected, saving a redirect per request, with the mirror's own URL to fall back to. Each
/// mirror starts from the answer time its probe measured, where it measured one, instead of an
/// assumed one, so the first requests already favour near mirrors.
fn build_racer(mirrors: &[ProbeInfo]) -> MirrorRacer {
    let mut racer = MirrorRacer::new(Vec::new());
    for probe in mirrors {
        add_mirror(&mut racer, probe);
    }
    racer
}

/// Adds the mirror `probe` found to `racer` (see `build_racer`).
fn add_mirror(racer: &mut MirrorRacer, probe: &ProbeInfo) {
    let mirror = racer.add(probe.final_url.clone());
    mirror.fallback = (probe.final_url != probe.url).then(|| probe.url.clone());
    mirror.if_range = probe.if_range();
    if let Some(answer) = probe.answer_time {
        mirror.ttfb_ewma_ms = answer.as_secs_f64() * 1000.0;
    }
}

/// The next probe of those still out, if any are; never while none are.
async fn next_late(late: &mut Option<ProbeStream>) -> Option<(usize, Probe)> {
    match late {
        Some(probes) => probes.next().await,
        None => std::future::pending().await,
    }
}

/// Requests the mirrors' hosts take at once from a download under `limit` (0 for none): per host,
/// the limit, or the connection cap it was seen to enforce if lower.
fn host_room(mirrors: &[ProbeInfo], limit: usize) -> usize {
    let mut seen = std::collections::HashSet::new();
    mirrors
        .iter()
        .filter(|m| seen.insert(HostKey::of(&m.final_url)))
        .map(|m| {
            let cap = hosts::profile(&m.final_url).connection_cap;
            cap.into_iter().chain((limit > 0).then_some(limit)).min().unwrap_or(usize::MAX)
        })
        .fold(0, usize::saturating_add)
}

/// Whether a probe's ranged GET, answered from `final_url`, leaves HEAD nothing to add: a 206
/// with the total size, a validator chunk requests can send as If-Range (a strong ETag or
/// Last-Modified: HEAD may have the date a weak ETag cannot stand in for), and a file name (its
/// own Content-Disposition or a name in the URL's path).
fn says_it_all(status: StatusCode, headers: &HeaderMap, final_url: &Url) -> bool {
    let has_total = headers
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| matches!(ByteRange::parse_content_range(v), Ok((_, Some(_)))));
    let strong_etag = headers.get(ETAG).and_then(|v| v.to_str().ok()).is_some_and(is_strong);
    status == StatusCode::PARTIAL_CONTENT
        && has_total
        && (strong_etag || headers.contains_key(LAST_MODIFIED))
        && (disposition_name(headers).is_some() || filename_from_url(final_url).is_some())
}

/// Whether an ETag is strong: If-Range takes no weak one.
fn is_strong(etag: &str) -> bool {
    !etag.starts_with("W/")
}

/// Answers that mean "not now", not "no ranges" or "no such file".
fn is_busy(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE | StatusCode::BAD_GATEWAY | StatusCode::GATEWAY_TIMEOUT
    )
}

/// Whether `Accept-Ranges` lists the `bytes` unit.
fn accepts_bytes(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT_RANGES)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|unit| unit.trim().eq_ignore_ascii_case("bytes"))
}

/// Watches one connection's rate for a cap on it. Every half setup time (the time a new
/// connection needs before its first byte), or at the first byte after, the rate over the bytes
/// that came since the last look is measured; a look at nothing leaves the window open, as a pause
/// is no rate. Two rates in a row without it climbing mean TCP slow start (which new connections
/// would go through too) is over and the server caps the connection at that rate.
struct RateWatch {
    pace: Pace,
    tick: Duration,
    /// Bytes that came since the answer, and when the last of them came.
    received: u64,
    last_byte_at: tokio::time::Instant,
    /// The window being measured: since when, from how many bytes, and when it is next looked at.
    window_began: tokio::time::Instant,
    window_began_len: u64,
    next_tick: tokio::time::Instant,
    /// The last rate measured, and whether it had stopped climbing.
    last: Option<(f64, bool)>,
    /// The rate, in bytes per second, the connection keeps to, once it kept to it twice in a row.
    capped_at: Option<f64>,
}

impl RateWatch {
    fn new(pace: Pace) -> Self {
        let tick = (pace.setup / 2).max(MIN_RATE_TICK);
        Self {
            pace,
            tick,
            received: 0,
            last_byte_at: pace.answered,
            window_began: pace.answered,
            window_began_len: 0,
            next_tick: pace.answered + tick,
            last: None,
            capped_at: None,
        }
    }

    /// `bytes` came at `now`, after the window a look then due measures.
    fn arrived(&mut self, bytes: u64, now: tokio::time::Instant) {
        self.look(now);
        self.received += bytes;
        self.last_byte_at = now;
    }

    /// Measures the rate over the window up to `now`, if a look is due.
    fn look(&mut self, now: tokio::time::Instant) {
        if now < self.next_tick {
            return;
        }
        self.next_tick = now + self.tick;
        let got = self.received - self.window_began_len;
        if got == 0 {
            return;
        }
        let rate = got as f64 / (now - self.window_began).as_secs_f64();
        let flat = self.last.is_some_and(|(prev, _)| rate < prev * 1.5);
        if flat && self.last.is_some_and(|(_, was_flat)| was_flat) {
            self.capped_at = Some(rate);
        } else if !flat {
            self.capped_at = None;
        }
        self.last = Some((rate, flat));
        (self.window_began, self.window_began_len) = (now, self.received);
    }

    /// What the connection told of its host so far: capped at `capped_at`; or not capped (`None`)
    /// once it came, on average, so fast that in the time a new connection takes to start it
    /// brings all one connection is ever asked to carry (`BYTES_PER_CONNECTION`), since whatever
    /// cap it may have above that rate, no download would be split differently for it. Less tells
    /// nothing: a cap may be just ahead.
    fn verdict(&self) -> Option<Option<f64>> {
        if self.capped_at.is_some() {
            return Some(self.capped_at);
        }
        let secs = (self.last_byte_at - self.pace.answered).as_secs_f64();
        let rate = (secs > 0.0).then(|| self.received as f64 / secs);
        let fast = bytes_per_setup(rate, Some(self.pace.setup)).is_some_and(|bytes| bytes >= BYTES_PER_CONNECTION);
        fast.then_some(None)
    }

    /// Records what the connection told of `url`'s host, if anything; whether it told anything.
    fn record(&self, url: &Url) -> bool {
        let Some(capped_at) = self.verdict() else { return false };
        let seen = HostProfile { capped_per_connection: Some(capped_at.is_some()), connection_rate: capped_at, ..Default::default() };
        hosts::record(url, seen);
        true
    }
}

/// The rest of the probe's answer as the first chunk's worker reads it, its connection's rate
/// watched on (see `RateWatch`) for as long as the download has no other connection with an
/// answer: what it tells of the host is recorded once it tells it, or once watching ends.
struct Watched {
    body: Body,
    watch: Option<RateWatch>,
    /// Cancelled once another connection of the download has its answer.
    alone: CancellationToken,
    url: Url,
}

impl Watched {
    fn stop(&mut self) {
        if let Some(watch) = self.watch.take() {
            watch.record(&self.url);
        }
    }
}

impl futures_util::Stream for Watched {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let item = std::task::ready!(this.body.poll_next_unpin(cx));
        let over = match (&item, this.watch.as_mut()) {
            (Some(Ok(bytes)), Some(watch)) if !this.alone.is_cancelled() => {
                watch.arrived(bytes.len() as u64, tokio::time::Instant::now());
                watch.capped_at.is_some()
            }
            _ => true,
        };
        if over {
            this.stop();
        }
        Poll::Ready(item)
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What `read_prefix` read.
struct Prefix {
    bytes: Bytes,
    /// The connection's rate, as watched while reading.
    watch: Option<RateWatch>,
    /// Whether reading stopped by choice, for a capped connection or at the deadline, while the
    /// body is still coming: the rest of it can go on as the download's first chunk.
    live: bool,
}

/// Reads up to `len` bytes of a probe's body until `deadline`, giving up after `stall` without
/// data. Whatever arrived is kept, even if the body ends early.
///
/// With `watch`, the connection's rate is watched for a cap (see `RateWatch`). Reading stops at
/// one if the rest would take over two setup times: workers fetch it in parallel sooner, with this
/// connection as one of them.
async fn read_prefix(
    response: &mut Response,
    len: u64,
    deadline: Option<tokio::time::Instant>,
    mut watch: Option<RateWatch>,
    stall: Duration,
    limiter: Option<&RateLimiter>,
) -> Prefix {
    let mut kept = Vec::with_capacity(len as usize);
    let mut last_byte_at = tokio::time::Instant::now();
    let mut live = false;
    while (kept.len() as u64) < len {
        let mut wake = last_byte_at + stall;
        if let Some(watch) = &watch {
            wake = wake.min(watch.next_tick);
        }
        if let Some(deadline) = deadline {
            wake = wake.min(deadline);
        }
        match tokio::time::timeout_at(wake, response.chunk()).await {
            Ok(Ok(Some(bytes))) => {
                if let Some(limiter) = limiter {
                    limiter.acquire(bytes.len() as u64).await;
                }
                let take = bytes.len().min(len as usize - kept.len());
                kept.extend_from_slice(&bytes[..take]);
                last_byte_at = tokio::time::Instant::now();
                if let Some(watch) = &mut watch {
                    watch.arrived(take as u64, last_byte_at);
                }
            }
            Ok(_) => break, // the body ended, or failed
            Err(_) => {
                let now = tokio::time::Instant::now();
                if now >= last_byte_at + stall {
                    break; // stalled: nothing more is coming
                }
                if deadline.is_some_and(|d| now >= d) {
                    live = true; // out of time: the rest goes on alongside the workers
                    break;
                }
                if let Some(watch) = &mut watch {
                    watch.look(now);
                }
            }
        }
        if let Some(watch) = &watch {
            let rest = (len - kept.len() as u64) as f64;
            if watch.capped_at.is_some_and(|rate| rest > 2.0 * rate * watch.pace.setup.as_secs_f64()) {
                live = true;
                break;
            }
        }
    }
    Prefix { bytes: kept.into(), watch, live }
}

/// Reads the file's start from the first mirror's probe answer, as much of it as the workers
/// should wait for (see `read_prefix`), and returns the answer while the rest of it is still
/// coming: the download goes on with it (as its first chunk, or as its one stream) instead of
/// asking for those bytes again.
///
/// What its host was seen to do (see [`crate::hosts`]) spares measuring it again: on a host known
/// to cap each connection the workers start at once, at the rate it was capped at, unless the
/// answer brings the whole file in less than two setup times at that rate (as `read_prefix` would
/// judge it): that file is read, to be written as it is. On a host known not to cap, the start is
/// read as if no rate could be measured. Otherwise what the measurement tells is recorded for the
/// next download, and while it tells nothing yet, the answer's connection is watched on as the
/// first chunk (see `Watched`).
async fn take_start(
    info: &mut ProbeInfo,
    body: ProbeBody,
    several_connections: bool,
    limiter: Option<&RateLimiter>,
    stall: Duration,
) -> Option<Live> {
    let ProbeBody { mut response, slot, holds, answered, setup } = body;
    let len = match holds {
        Holds::Start(len) => len,
        Holds::Whole => return Some(Live::Stream { response, slot }),
    };
    let known = hosts::profile(&info.final_url);
    // What a new connection needs before its first byte: as one needed before, since this answer
    // may have come over a connection already open, which needs less.
    let setup = known.setup_time.or(setup);
    // Workers can take over early only where ranges work (without them only the whole body is of
    // use) and there can be several.
    let splits = info.accepts_ranges && several_connections;
    if splits && known.capped_per_connection == Some(true) {
        let per_setup = bytes_per_setup(known.connection_rate, setup);
        let whole_soon = info.size == Some(len) && per_setup.is_some_and(|bytes| len <= bytes.saturating_mul(2));
        if !whole_soon {
            info.per_setup = per_setup;
            return Some(Live::Range { response, slot, end: len.saturating_sub(1), watch: None });
        }
    }
    // Only the start of a larger file keeps workers waiting.
    let deadline = (info.size != Some(len)).then(|| tokio::time::Instant::now() + PREFETCH_TIME);
    // A speed limit would make every connection look capped.
    let watch = setup
        .filter(|_| splits && limiter.is_none() && known.capped_per_connection.is_none())
        .map(|setup| RateWatch::new(Pace { answered, setup }));
    let prefix = read_prefix(&mut response, len, deadline, watch, stall, limiter).await;
    info.prefetch = prefix.bytes;
    info.per_setup = bytes_per_setup(prefix.watch.as_ref().and_then(|watch| watch.capped_at), setup);
    let mut watch = prefix.watch;
    if watch.as_ref().is_some_and(|watch| watch.record(&info.final_url)) {
        watch = None;
    }
    prefix.live.then(|| Live::Range { response, slot, end: len.saturating_sub(1), watch })
}

/// Bytes a connection at `rate` carries in the time `setup` another needs to start.
fn bytes_per_setup(rate: Option<f64>, setup: Option<Duration>) -> Option<u64> {
    Some((rate? * setup?.as_secs_f64()) as u64)
}

/// Picks the reference probe and keeps the mirrors that serve the same file. The first successful
/// probe defines the file; a mirror serving it that takes ranges is the reference if that one does
/// not, since only ranges resume a partial download or split the work, unless the first probe
/// brought the whole file.
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
    let first = ok.first().cloned().ok_or_else(|| format!("Failed to probe file information: {}", errors.join("; ")))?;
    let reference = if first.accepts_ranges || first.size == Some(first.prefetch.len() as u64) {
        &first
    } else {
        ok.iter().find(|m| m.accepts_ranges && mismatch(m, &first).is_none()).unwrap_or(&first)
    }
    .clone();

    // The first probe keeps defining the file even when another mirror leads: a reference
    // without an ETag must not let in a mirror whose ETag the first probe rules out.
    let mirrors = ok
        .into_iter()
        .filter(|m| {
            let mismatch = mismatch(m, &first).or_else(|| mismatch(m, &reference));
            if let Some(reason) = &mismatch {
                tracing::warn!("Dropping mirror {}: {}", m.url, reason);
            }
            mismatch.is_none()
        })
        .collect();
    Ok((reference, mirrors))
}

/// Why mirror `m` cannot serve the download `reference` describes, if it cannot.
fn mismatch(m: &ProbeInfo, reference: &ProbeInfo) -> Option<String> {
    match (m.strong_etag(), reference.strong_etag()) {
        _ if m.size != reference.size => Some(format!("size {:?} differs from {:?}", m.size, reference.size)),
        (Some(a), Some(b)) if a != b => Some(format!("ETag {} differs from {}", a, b)),
        _ if reference.accepts_ranges && !m.accepts_ranges => Some("no range support".to_string()),
        _ => None,
    }
}

/// Where the download goes and whether it can pick up a previous partial download.
#[derive(Debug)]
enum Plan {
    /// This path already holds the exact file.
    AlreadyDone(PathBuf),
    /// Download into `<final_path>.part`, resuming from `resume` if set, while holding `claim`.
    Fetch { final_path: PathBuf, resume: Option<Box<DownloadState>>, claim: Claim },
}

/// Exclusive ownership of an output name for a whole download: an OS lock on
/// `<final>.part.lock`, which the OS also releases if the process dies. Other downloads, in this
/// process or another, skip a claimed name even before its `.part` exists. Dropping the claim
/// deletes the lock file.
#[derive(Debug)]
struct Claim {
    path: PathBuf,
    _lock: File,
}

impl Claim {
    /// Claims `final_path`, or returns `None` while another download holds it. Blocking.
    fn try_take(final_path: &Path) -> Result<Option<Self>, String> {
        let path = lock_path(final_path);
        let fail = |e: std::io::Error| format!("Failed to lock {}: {}", path.display(), e);
        let mut denied = None;
        for _ in 0..3 {
            let lock = match OpenOptions::new().write(true).create(true).truncate(false).open(&path) {
                Ok(lock) => lock,
                // Windows refuses to open a lock file while its holder deletes it, for an instant.
                // Anything longer is a real permission problem.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    denied = Some(e);
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => return Err(fail(e)),
            };
            denied = None;
            match lock.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(e)) => return Err(fail(e)),
            }
            // A finishing holder deletes the lock file. If that happened between our open and our
            // lock, we hold a lock on a file nobody else can see, while another download may hold
            // one on a new file at `path`: only a lock on the very file now at `path` counts.
            if is_file_at(&lock, &path).map_err(fail)? {
                return Ok(Some(Self { path, _lock: lock }));
            }
        }
        denied.map_or(Ok(None), |e| Err(fail(e)))
    }
}

/// Whether `file` is the file now at `path`, not one deleted from there meanwhile.
fn is_file_at(file: &File, path: &Path) -> std::io::Result<bool> {
    match File::open(path) {
        Ok(at_path) => Ok(file_id(&at_path)? == file_id(file)?),
        // Deleted meanwhile, or still being deleted.
        Err(_) => Ok(false),
    }
}

/// Identity of an open file: device and inode.
#[cfg(unix)]
fn file_id(file: &File) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

/// Identity of an open file: volume serial number and file index.
#[cfg(windows)]
fn file_id(file: &File) -> std::io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};

    // SAFETY: a plain C struct, for which all zeroes is a valid value.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is owned by `file` and outlives this synchronous call; `info` is a valid
    // place for the result.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE, &mut info) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((u64::from(info.dwVolumeSerialNumber), index))
}

impl Drop for Claim {
    fn drop(&mut self) {
        // Deleted while still locked, so nobody can take a claim on the file as it goes away.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Exclusive use of a download target: the same claim a running download holds, so tools such as
/// repair or leftover cleanup never touch a `.part` another download (in any process) is writing.
/// Released on drop.
pub struct TargetClaim {
    _claim: Claim,
}

/// Claims `final_path`, or returns `None` while a download holds it. Blocking.
pub fn claim_target(final_path: &Path) -> Result<Option<TargetClaim>, String> {
    Ok(Claim::try_take(final_path)?.map(|claim| TargetClaim { _claim: claim }))
}

/// Deletes the partial files of `final_path` (`.part`, `.part.hfstate`, `.part.hlsstate`, and the
/// `.tmp` files a crash can leave of the latter two) and returns how many existed. Never touches
/// the final file. Fails while a download holds the target. Blocking.
pub fn discard_partial(final_path: &Path) -> Result<usize, String> {
    let Some(_claim) = claim_target(final_path)? else {
        return Err(format!("{} is still being downloaded", final_path.display()));
    };
    let part = part_path(final_path);
    let beside_part = |suffix: &str| {
        let mut path = part.clone().into_os_string();
        path.push(suffix);
        PathBuf::from(path)
    };
    // The data goes first: if a state file then fails to delete, it no longer matches anything.
    let mut removed = 0;
    let states = [".hfstate", ".hlsstate", ".hfstate.tmp", ".hlsstate.tmp"].map(beside_part);
    for path in std::iter::once(part.clone()).chain(states) {
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Failed to delete {}: {}", path.display(), e)),
        }
    }
    Ok(removed)
}

/// Chooses the output path. Walks `name`, `name (1)`, `name (2)`... and takes the first one that
/// is already this exact file, or that it can claim and that holds our resumable (or stale)
/// `.part` or is free. Existing files, claimed names and other downloads' `.part` files are never
/// touched. Our `.part` holding progress that no mirror can resume right now (`remote` takes ranges
/// if any mirror serving the file does) fails the plan and is kept. The history file at
/// `history_path` is read only if an existing file has to be compared with it.
fn plan_target(
    base: &Path,
    remote: &ProbeInfo,
    urls: &[String],
    history_path: &Path,
    checksum: Option<&str>,
) -> Result<Plan, String> {
    let loaded = std::cell::OnceCell::new();
    let history = || loaded.get_or_init(|| DownloadHistoryManager::load_from_path(history_path));
    let mut n = 0;
    loop {
        let candidate = numbered(base, n);
        n += 1;
        if already_downloaded(&candidate, remote, urls, history, checksum) {
            return Ok(Plan::AlreadyDone(candidate));
        }
        // A running download owns this name, whether or not its `.part` exists yet.
        let Some(claim) = Claim::try_take(&candidate)? else {
            continue;
        };
        let part = part_path(&candidate);
        let part_state = DownloadState::state_file_path(&part);

        if candidate.exists() && (part.exists() || !migrate_legacy(&candidate, &part, remote, urls)?) {
            continue;
        }

        if part.exists() {
            match DownloadState::load_from_path(&part_state).ok().flatten() {
                Some(state) if state.mirrors.iter().any(|m| urls.contains(m)) => {
                    let same_file = remote.size.is_none_or(|size| size == state.file_size)
                        && validators_compatible(&state, remote)
                        && std::fs::metadata(&part).map(|m| m.len()).ok() == Some(state.file_size);
                    if same_file && remote.accepts_ranges {
                        return Ok(Plan::Fetch { final_path: candidate, resume: Some(Box::new(state)), claim });
                    }
                    // Downloaded bytes go only once they are proven stale, or the probe brought the
                    // whole file anyway: without range support now, they may still resume later.
                    let whole_file_fetched = remote.size == Some(remote.prefetch.len() as u64);
                    if same_file && !state.completed_ranges.is_empty() && !whole_file_fetched {
                        return Err(format!(
                            "{} does not accept range requests right now, so the partial download {} cannot \
                             resume; it was kept. Try again later, or delete it to download from the start.",
                            remote.url,
                            part.display()
                        ));
                    }
                    tracing::info!("Discarding stale partial download {}", part.display());
                    std::fs::remove_file(&part).map_err(|e| format!("Failed to remove {}: {}", part.display(), e))?;
                    let _ = DownloadState::remove(&part_state);
                }
                // Another download's partial file.
                _ => continue,
            }
        }

        return Ok(Plan::Fetch { final_path: candidate, resume: None, claim });
    }
}

/// An existing file counts as this download only if the checksum says so, or history recorded
/// this exact path completing from one of these URLs (as history saves them, without secrets)
/// with this size, and the server's Last-Modified is not newer than that download. `history` is
/// consulted only in that last case.
fn already_downloaded<'h>(
    path: &Path,
    remote: &ProbeInfo,
    urls: &[String],
    history: impl FnOnce() -> &'h DownloadHistoryManager,
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
    let urls: Vec<String> = urls.iter().map(|u| redact_url(u)).collect();
    history().entries().iter().any(|e| {
        e.status == HistoryStatus::Completed
            && absolute(&e.file_path) == path
            && e.file_size == size
            && e.urls.iter().any(|u| urls.contains(u))
            && modified.is_none_or(|lm| lm <= e.started_at)
    }) && header_matches_extension(&path)
}

/// The file, and its size, that the media stream `key` (see [`crate::media::MediaStream::key`])
/// was finished into next to `base` by an earlier attempt: history, read from `history_path`,
/// recorded it completing from that key in `base`'s folder, at the size the file has now.
/// Blocking.
fn finished_stream(base: &Path, key: &str, history_path: &Path) -> Option<(PathBuf, u64)> {
    let dir = absolute(base).parent()?.to_path_buf();
    let history = DownloadHistoryManager::load_from_path(history_path);
    history
        .entries()
        .iter()
        .filter(|e| e.status == HistoryStatus::Completed && e.urls.iter().any(|u| u == key))
        .filter(|e| absolute(&e.file_path).parent() == Some(dir.as_path()))
        .find_map(|e| {
            let size = std::fs::metadata(&e.file_path).ok().filter(|m| m.is_file())?.len();
            (size == e.file_size).then(|| (e.file_path.clone(), size))
        })
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

/// Fewest bytes a steal takes: the user's `min_steal_threshold`, else `MIN_STEAL`.
fn min_steal(options: &DownloadOptions) -> u64 {
    if options.min_steal_threshold != DEFAULT_MIN_STEAL {
        options.min_steal_threshold
    } else {
        MIN_STEAL
    }
}

/// The chunk size for `file_size` missing bytes over `num_workers` connections, unless the user
/// `configured` one. `in_order`, when the file is hashed from its start as that is written (SHA-256
/// or MD5, see [`DiskWriter::track_digest`]), keeps chunks small: taken in file order, they keep
/// that start growing, so little is left to hash once the download is done.
fn effective_chunk_size(file_size: u64, num_workers: u64, configured: u64, in_order: bool) -> u64 {
    const MB: u64 = 1024 * 1024;
    if configured != DEFAULT_CHUNK_SIZE {
        configured
    } else if in_order {
        effective_chunk_size(file_size, num_workers, configured, false).min(IN_ORDER_CHUNK)
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

/// First of `base`, `base (1)`, ... where neither the file, its `.part` nor a claim exists.
fn free_path(base: &Path) -> PathBuf {
    (0..)
        .map(|n| numbered(base, n))
        .find(|c| !c.exists() && !part_path(c).exists() && !lock_path(c).exists())
        .unwrap_or_else(|| base.to_path_buf())
}

fn part_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    final_path.with_file_name(name)
}

/// `<final>.part.lock`, the file a [`Claim`] locks.
fn lock_path(final_path: &Path) -> PathBuf {
    let mut name = part_path(final_path).into_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// Whether an output path names a directory to save into: it ends with a path separator or is
/// an existing directory.
fn is_dir_target(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().last().is_some_and(|&b| std::path::is_separator(b as char)) || path.is_dir()
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

/// File name from the first Content-Disposition among `headers` that names one, else the last
/// segment of the (final, post-redirect) URL. Raw UTF-8 in the header is accepted, as browsers do.
fn extract_filename<'a>(headers: impl IntoIterator<Item = &'a HeaderMap>, url: &Url) -> String {
    headers
        .into_iter()
        .find_map(disposition_name)
        .or_else(|| filename_from_url(url))
        .unwrap_or_else(|| "downloaded_file.bin".to_string())
}

/// The usable file name in a response's Content-Disposition, if it has one.
fn disposition_name(headers: &HeaderMap) -> Option<String> {
    let value = String::from_utf8_lossy(headers.get(CONTENT_DISPOSITION)?.as_bytes()).into_owned();
    content_disposition_filename(&value)
        .map(|name| sanitize_filename(&name))
        .filter(|name| !name.is_empty())
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

/// Makes a server-provided name safe as a single path component on every OS, at most
/// `MAX_NAME_BYTES` long. Leading dots go too, so a download is never hidden. Empty when nothing
/// usable is left.
fn sanitize_filename(name: &str) -> String {
    finish_name(replace_invalid_chars(name).trim().trim_matches('.').trim())
}

/// Makes a name from a torrent or metalink safe as a single path component on every OS, at most
/// `MAX_NAME_BYTES` long. Unlike a server's file name it keeps leading dots (`.gitignore`,
/// `.config`): only the trailing dots and spaces Windows drops go. Empty when nothing usable is
/// left.
pub(crate) fn sanitize_component(name: &str) -> String {
    finish_name(replace_invalid_chars(name).trim_end_matches(['.', ' ']))
}

/// Replaces control characters and the characters Windows forbids in names with '_'.
fn replace_invalid_chars(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// Prefixes names Windows reserves for devices with '_' and caps the length.
fn finish_name(name: &str) -> String {
    let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let is_reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
            | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    );
    truncate_name(if is_reserved { format!("_{}", name) } else { name.to_string() })
}

/// Cuts `name` to at most `MAX_NAME_BYTES` on a character boundary, keeping a short extension.
fn truncate_name(name: String) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, Some(ext)),
        _ => (name.as_str(), None),
    };
    let budget = MAX_NAME_BYTES - ext.map_or(0, |e| e.len() + 1);
    let cut = (0..=budget).rev().find(|&i| stem.is_char_boundary(i)).unwrap_or(0);
    let stem = stem[..cut].trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    match ext {
        Some(ext) => format!("{}.{}", stem, ext),
        None => stem.to_string(),
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
    use reqwest::header::HeaderValue;
    use tempfile::tempdir;

    fn remote(size: u64) -> ProbeInfo {
        let url = Url::parse("http://example.com/file.bin").unwrap();
        ProbeInfo {
            final_url: url.clone(),
            url,
            size: Some(size),
            accepts_ranges: true,
            filename: "file.bin".to_string(),
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
            prefetch: Bytes::new(),
            per_setup: None,
            answer_time: None,
            headers: HeaderMap::new(),
        }
    }

    fn urls() -> Vec<String> {
        vec!["http://example.com/file.bin".to_string()]
    }

    #[test]
    fn serialized_options_round_trip_without_the_credential() {
        let opts = DownloadOptions {
            auth_header: Some("Bearer secret".into()),
            max_speed: Some(1024),
            media_preset: Some(crate::media::MediaQualityPreset::Custom("bv*".into())),
            ..Default::default()
        };
        let json = serde_json::to_string(&opts).unwrap();
        assert!(!json.contains("secret"));
        let back: DownloadOptions = serde_json::from_str(&json).unwrap();
        assert_eq!((back.auth_header, back.max_speed), (None, Some(1024)));
        assert_eq!(back.media_preset, opts.media_preset);
        // Fields missing from older saved data take their defaults.
        let old: DownloadOptions = serde_json::from_str(r#"{"num_connections":4}"#).unwrap();
        assert_eq!((old.num_connections, old.max_retries), (4, DownloadOptions::default().max_retries));
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
        assert_eq!(extract_filename([&headers], &url), "final name.iso");

        headers.insert(CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"../../etc/passwd\""));
        let name = extract_filename([&headers], &url);
        assert!(!name.contains('/') && !name.contains('\\') && !name.starts_with('.'), "{name}");

        headers.insert(CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"..\""));
        assert_eq!(extract_filename([&headers], &url), "final name.iso");
        assert_eq!(sanitize_filename("con.txt"), "_con.txt");
        // A server's name never hides the download; a torrent's or metalink's dotfile keeps its name.
        assert_eq!(sanitize_filename(".bashrc"), "bashrc");
        assert_eq!(sanitize_component(".bashrc"), ".bashrc");
        assert_eq!(sanitize_component(" a: b. . "), " a_ b");
        // C0 and C1 control characters alike (NEL, DEL and CSI can drive terminals).
        assert_eq!(sanitize_filename("a\u{1b}b\u{7f}c\u{85}d\u{9b}.txt"), "a_b_c_d_.txt");
        assert_eq!(extract_filename([&HeaderMap::new()], &Url::parse("http://h/").unwrap()), "downloaded_file.bin");
        assert_eq!(String::from_utf8_lossy(&percent_decode("100%-%zz%4")), "100%-%zz%4");
    }

    #[test]
    fn test_a_complete_ranged_answer_leaves_head_nothing_to_add() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut map = HeaderMap::new();
            for (k, v) in pairs {
                map.insert(*k, HeaderValue::from_static(v));
            }
            map
        };
        let (named, unnamed) = (Url::parse("http://h/files/tool.zip").unwrap(), Url::parse("http://h/").unwrap());
        let tagged = headers(&[("content-range", "bytes 0-0/1000"), ("etag", "\"v1\"")]);
        assert!(says_it_all(StatusCode::PARTIAL_CONTENT, &tagged, &named));
        // Last-Modified is a validator too, and Content-Disposition a name.
        let dated = headers(&[
            ("content-range", "bytes 0-0/1000"),
            ("last-modified", "Sun, 06 Nov 1994 08:49:37 GMT"),
            ("content-disposition", "attachment; filename=\"a.bin\""),
        ]);
        assert!(says_it_all(StatusCode::PARTIAL_CONTENT, &dated, &unnamed));

        // Anything missing leaves HEAD its grace.
        assert!(!says_it_all(StatusCode::PARTIAL_CONTENT, &tagged, &unnamed), "no name");
        let untagged = headers(&[("content-range", "bytes 0-0/1000")]);
        assert!(!says_it_all(StatusCode::PARTIAL_CONTENT, &untagged, &named), "no validator");
        let weak = headers(&[("content-range", "bytes 0-0/1000"), ("etag", "W/\"v1\"")]);
        assert!(!says_it_all(StatusCode::PARTIAL_CONTENT, &weak, &named), "no validator If-Range takes");
        let no_total = headers(&[("content-range", "bytes 0-0/*"), ("etag", "\"v1\"")]);
        assert!(!says_it_all(StatusCode::PARTIAL_CONTENT, &no_total, &named), "no size");
        assert!(!says_it_all(StatusCode::OK, &tagged, &named), "no ranges");
    }

    #[test]
    fn test_raw_utf8_content_disposition_is_used() {
        let url = Url::parse("http://example.com/dl.cgi").unwrap();
        let mut headers = HeaderMap::new();
        let raw = HeaderValue::from_bytes("attachment; filename=\"café.zip\"".as_bytes()).unwrap();
        headers.insert(CONTENT_DISPOSITION, raw);
        assert_eq!(extract_filename([&headers], &url), "café.zip");
        // The first response that names the file wins; later ones only fill gaps.
        let none = HeaderMap::new();
        assert_eq!(extract_filename([&none, &headers], &url), "café.zip");
    }

    #[test]
    fn test_long_names_are_capped_keeping_the_extension() {
        let url = Url::parse("http://example.com/x").unwrap();
        for long in ["a".repeat(300) + ".bin", "é".repeat(150) + ".tar.gz", "b".repeat(300)] {
            let mut headers = HeaderMap::new();
            let value = format!("attachment; filename=\"{}\"", long);
            headers.insert(CONTENT_DISPOSITION, HeaderValue::from_bytes(value.as_bytes()).unwrap());
            let name = extract_filename([&headers], &url);
            assert!(name.len() <= MAX_NAME_BYTES, "{} bytes", name.len());
            assert_eq!(Path::new(&name).extension(), Path::new(&long).extension(), "{name}");
            assert!(long.starts_with(Path::new(&name).file_stem().unwrap().to_str().unwrap()));
            // Every file the download creates next to it still fits a 255-byte component.
            let state_tmp = format!("{} (99).part.hfstate.tmp", name);
            assert!(state_tmp.len() <= 255, "{} bytes", state_tmp.len());
        }
        assert_eq!(filename_from_url(&Url::parse(&format!("http://h/{}.iso", "c".repeat(400))).unwrap()).unwrap().len(), 200);
    }

    #[tokio::test]
    async fn test_authorization_is_not_a_client_default_header() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let third_party = Url::parse(&format!("http://{}/v.mp4", listener.local_addr().unwrap())).unwrap();
        let (head_tx, head_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = [0u8; 4096];
            let n = socket.read(&mut head).await.unwrap();
            let _ = head_tx.send(String::from_utf8_lossy(&head[..n]).to_ascii_lowercase());
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
        });

        let user_url = Url::parse("https://intranet.example/videos/42").unwrap();
        let options = DownloadOptions { auth_header: Some("Bearer secret".into()), ..Default::default() };
        let engine = DownloadEngine::new(vec![user_url.clone()], options);
        let client = engine.client.clone().unwrap();
        // What resolvers, HLS and anything else using the client send to a host the user never named.
        client.get(third_party.clone()).send().await.unwrap();
        let head = head_rx.await.unwrap();
        assert!(!head.contains("authorization") && !head.contains("secret"), "{head}");

        let auth = engine.auth.as_deref();
        let to_user = authorize(client.get(user_url.clone()), auth, &user_url).build().unwrap();
        assert_eq!(to_user.headers()[reqwest::header::AUTHORIZATION], "Bearer secret");
        let to_other = authorize(client.get(third_party.clone()), auth, &third_party).build().unwrap();
        assert!(!to_other.headers().contains_key(reqwest::header::AUTHORIZATION));
    }

    #[test]
    fn test_client_key_covers_what_the_client_is_built_from() {
        let base = DownloadOptions::default();
        let same = DownloadOptions { num_connections: 3, max_speed: Some(1), ..DownloadOptions::default() };
        assert_eq!(ClientKey::of(&base), ClientKey::of(&same));
        for other in [
            DownloadOptions { auth_header: Some("Bearer x".into()), ..DownloadOptions::default() },
            DownloadOptions { proxy: Some("http://p:8080".into()), ..DownloadOptions::default() },
            DownloadOptions { cookies_path: Some("c.txt".into()), ..DownloadOptions::default() },
        ] {
            assert_ne!(ClientKey::of(&base), ClientKey::of(&other));
        }
    }

    /// A body that sends each `(ms, bytes)` step `ms` after the one before.
    fn paced(steps: Vec<(u64, usize)>) -> Response {
        let chunks = futures_util::stream::unfold(steps.into_iter(), |mut steps| async move {
            let (ms, n) = steps.next()?;
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Some((Ok::<_, std::io::Error>(Bytes::from(vec![7u8; n])), steps))
        });
        Response::from(http::Response::new(reqwest::Body::wrap_stream(chunks)))
    }

    /// Reads all of `body` as a probe whose answer took 40 ms, so its rate ticks every 20 ms.
    async fn read_paced(body: &mut Response, len: u64, paced_read: bool) -> Prefix {
        let pace = Pace { answered: tokio::time::Instant::now(), setup: Duration::from_millis(40) };
        read_prefix(body, len, None, paced_read.then(|| RateWatch::new(pace)), Duration::from_secs(1), None).await
    }

    /// What a read's connection told of its host.
    fn verdict(prefix: &Prefix) -> Option<Option<f64>> {
        prefix.watch.as_ref().and_then(RateWatch::verdict)
    }

    /// The rate a read found its connection capped at.
    fn capped_at(prefix: &Prefix) -> Option<f64> {
        prefix.watch.as_ref().and_then(|watch| watch.capped_at)
    }

    /// What is left of a body, read to its end.
    async fn rest_of(mut body: Response) -> usize {
        let mut rest = 0;
        while let Some(bytes) = body.chunk().await.unwrap() {
            rest += bytes.len();
        }
        rest
    }

    #[tokio::test(start_paused = true)]
    async fn test_prefix_read_hands_a_capped_connection_to_workers() {
        // 8 KiB per 3 ms from the first byte on (~2.7 MB/s), for 1 MiB: the rest would take far
        // longer than two 40 ms setups, so workers take over, each worth ~40 ms of transfer.
        let capped = || paced(std::iter::once((0, 8 * 1024)).chain(std::iter::repeat_n((3, 8 * 1024), 127)).collect());
        let mut body = capped();
        let prefix = read_paced(&mut body, PREFETCH, true).await;
        assert!(prefix.bytes.len() <= 256 * 1024, "{}", prefix.bytes.len());
        let per_setup = bytes_per_setup(capped_at(&prefix), Some(Duration::from_millis(40))).expect("a capped rate is measured");
        assert!((80 * 1024..=140 * 1024).contains(&per_setup), "{per_setup}");
        // The probe's connection is one of them: the rest of its answer is still coming.
        assert!(verdict(&prefix).is_some() && prefix.live);
        assert_eq!(prefix.bytes.len() + rest_of(body).await, PREFETCH as usize, "nothing read was lost");

        // Without ranges only the whole body is of use, and no rate tells anything.
        let prefix = read_paced(&mut capped(), PREFETCH, false).await;
        assert_eq!((prefix.bytes.len() as u64, prefix.live, verdict(&prefix)), (PREFETCH, false, None));
    }

    #[tokio::test(start_paused = true)]
    async fn test_prefix_read_keeps_a_connection_in_slow_start() {
        // Bursts doubling every round trip, the first with the headers and the next just before
        // the first tick: one window looks flat (4+8 KiB, then 16), yet the rate is still
        // climbing and new connections would start as slowly, so the probe keeps the whole file.
        let steps = vec![(0, 4), (19, 8), (20, 16), (20, 32), (20, 64), (20, 128), (20, 4)];
        let mut body = paced(steps.into_iter().map(|(ms, k)| (ms, k * 1024)).collect());
        let prefix = read_paced(&mut body, 256 * 1024, true).await;
        assert_eq!(prefix.bytes.len(), 256 * 1024);
        assert_eq!((capped_at(&prefix), prefix.live), (None, false));
        // All of it came before the rate stopped climbing, but a cap may have been just ahead:
        // ~2 MB/s brings far less than a connection is asked to carry in the 40 ms another needs.
        assert_eq!(verdict(&prefix), None, "too little came too slowly to tell there is no cap");
    }

    #[tokio::test(start_paused = true)]
    async fn test_what_a_connection_tells_of_its_host() {
        let setup = Duration::from_millis(40);
        let url = Url::parse("http://told.engine.invalid/f").unwrap();
        // Too short to measure: two pieces, 20 KiB at ~0.6 MB/s. Nothing is recorded, so the next
        // file from the host, as capped, is measured and split, not kept on one connection.
        let mut info = probed(url.as_str(), 20 * 1024);
        let body = probe_body(&url, paced(vec![(0, 16 * 1024), (32, 4 * 1024)]), 20 * 1024, setup);
        assert!(take_start(&mut info, body, true, None, Duration::from_secs(1)).await.is_none());
        assert_eq!(info.prefetch.len(), 20 * 1024);
        assert_eq!(hosts::profile(&url).capped_per_connection, None);
        let mut info = probed(url.as_str(), PREFETCH);
        let body = probe_body(&url, capped_body(), PREFETCH, setup);
        assert!(matches!(take_start(&mut info, body, true, None, Duration::from_secs(1)).await, Some(Live::Range { .. })));
        assert_eq!(hosts::profile(&url).capped_per_connection, Some(true));

        // So fast that, in the time a new connection needs, it brings all a connection is asked
        // to carry: whatever cap it has, splitting could not pay off.
        let url = Url::parse("http://told-fast.engine.invalid/f").unwrap();
        let mut info = probed(url.as_str(), PREFETCH);
        let body = probe_body(&url, paced(vec![(0, 256 * 1024), (2, 256 * 1024), (2, 256 * 1024), (2, 256 * 1024)]), PREFETCH, setup);
        assert!(take_start(&mut info, body, true, None, Duration::from_secs(1)).await.is_none());
        let seen = hosts::profile(&url);
        assert_eq!((seen.capped_per_connection, seen.connection_rate), (Some(false), None));
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_connection_that_told_nothing_yet_is_watched_on_as_the_first_chunk() {
        // New connections take 200 ms, so the rate is looked at every 100 ms: by the 250 ms the
        // workers wait for the start of a larger file, the capped rate held only once.
        let taken = |url: &Url| {
            hosts::record(url, HostProfile { setup_time: Some(Duration::from_millis(200)), ..Default::default() });
            let info = probed(url.as_str(), 4 * PREFETCH);
            let body = probe_body(url, capped_body(), PREFETCH, Duration::from_millis(200));
            async move {
                let mut info = info;
                match take_start(&mut info, body, true, None, Duration::from_secs(1)).await {
                    Some(Live::Range { response, watch: Some(watch), .. }) => (response, watch),
                    _ => panic!("the answer goes on, still watched"),
                }
            }
        };
        let url = Url::parse("http://watched.engine.invalid/f").unwrap();
        let (response, watch) = taken(&url).await;
        assert_eq!(hosts::profile(&url).capped_per_connection, None, "nothing told yet");
        // Read on as the only connection with an answer, it tells: the next download knows.
        let alone = CancellationToken::new();
        let mut watched = Watched { body: response.bytes_stream().boxed(), watch: Some(watch), alone, url: url.clone() };
        let mut rest = 0;
        while let Some(bytes) = watched.next().await {
            rest += bytes.unwrap().len();
        }
        assert!(rest > 0);
        let seen = hosts::profile(&url);
        assert_eq!(seen.capped_per_connection, Some(true));
        assert!(seen.connection_rate.is_some_and(|rate| (2e6..3.5e6).contains(&rate)), "{seen:?}");

        // Once another connection has its answer, what the rate does may be the others' doing.
        let url = Url::parse("http://watched-shared.engine.invalid/f").unwrap();
        let (response, watch) = taken(&url).await;
        let alone = CancellationToken::new();
        alone.cancel();
        let watched = Watched { body: response.bytes_stream().boxed(), watch: Some(watch), alone, url: url.clone() };
        let rest: Vec<Bytes> = watched.map(Result::unwrap).collect().await;
        assert!(!rest.is_empty());
        assert_eq!(hosts::profile(&url).capped_per_connection, None);
    }

    #[tokio::test(start_paused = true)]
    async fn test_prefix_read_out_of_time_leaves_the_rest_coming() {
        // 8 KiB per 10 ms, and 95 ms to read it: the rest of the answer goes on with the workers.
        let mut body = paced(std::iter::repeat_n((10, 8 * 1024), 32).collect());
        let deadline = tokio::time::Instant::now() + Duration::from_millis(95);
        let prefix = read_prefix(&mut body, 256 * 1024, Some(deadline), None, Duration::from_secs(1), None).await;
        assert!(prefix.live && prefix.watch.is_none());
        assert_eq!(prefix.bytes.len(), 72 * 1024);
        assert_eq!(rest_of(body).await, 184 * 1024);
    }

    /// ~2.7 MB/s from the first byte on, for 1 MiB.
    fn capped_body() -> Response {
        paced(std::iter::once((0, 8 * 1024)).chain(std::iter::repeat_n((3, 8 * 1024), 127)).collect())
    }

    /// A probe of `url` that found a file of `size` taking ranges.
    fn probed(url: &str, size: u64) -> ProbeInfo {
        let mut info = remote(size);
        info.url = Url::parse(url).unwrap();
        info.final_url = info.url.clone();
        info
    }

    /// The first mirror's probe answer as `probe_url` hands it on: `len` bytes of `body` coming
    /// from `url`, whose request waited `setup` for them.
    fn probe_body(url: &Url, body: Response, len: u64, setup: Duration) -> ProbeBody {
        let slot = hosts::try_acquire(url, 0).unwrap();
        ProbeBody { response: body, slot, holds: Holds::Start(len), answered: tokio::time::Instant::now(), setup: Some(setup) }
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_probe_over_an_open_connection_is_paced_by_what_a_new_one_needs() {
        let mut info = probed("http://cold-setup.engine.invalid/f", PREFETCH);
        // The answer came in 10 ms over a connection already open; a new one took 200 ms.
        hosts::record(&info.final_url, HostProfile { setup_time: Some(Duration::from_millis(200)), ..Default::default() });
        let body = probe_body(&info.final_url, capped_body(), PREFETCH, Duration::from_millis(10));
        let live = take_start(&mut info, body, true, None, Duration::from_secs(1)).await;
        // The rest took less than two new connections' setup: no worker takes over.
        assert!(live.is_none());
        assert_eq!(info.prefetch.len() as u64, PREFETCH);
        let per_setup = info.per_setup.expect("a capped rate is measured");
        assert!((400 * 1024..=700 * 1024).contains(&per_setup), "{per_setup}: 200 ms at the capped rate");
        // And the next download from the host knows the cap.
        let seen = hosts::profile(&info.final_url);
        assert_eq!(seen.capped_per_connection, Some(true));
        assert!(seen.connection_rate.is_some_and(|rate| (2e6..3.5e6).contains(&rate)), "{seen:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_host_seen_before_is_not_measured_again() {
        let stall = Duration::from_secs(1);
        // Seen capped: the workers start at once, at the rate seen, the answer going on unread.
        let capped = "http://seen-capped.engine.invalid/f";
        let seen = HostProfile {
            capped_per_connection: Some(true),
            connection_rate: Some(1e6),
            setup_time: Some(Duration::from_millis(100)),
            ..Default::default()
        };
        hosts::record(&Url::parse(capped).unwrap(), seen);
        let mut info = probed(capped, 4 * PREFETCH);
        let body = probe_body(&info.final_url, capped_body(), PREFETCH, Duration::from_millis(10));
        let Some(Live::Range { response, end, .. }) = take_start(&mut info, body, true, None, stall).await else {
            panic!("the answer goes on as the first chunk");
        };
        assert_eq!((info.prefetch.len(), end, info.per_setup), (0, PREFETCH - 1, Some(100_000)));
        assert_eq!(rest_of(response).await, PREFETCH as usize);
        // With a single connection there is nobody to hand over to: the start is read as ever.
        let mut info = probed(capped, 4 * PREFETCH);
        let body = probe_body(&info.final_url, capped_body(), PREFETCH, Duration::from_millis(10));
        take_start(&mut info, body, false, None, stall).await;
        assert!(!info.prefetch.is_empty());
        // Nor for a file the answer brings whole in less than two setups (200 KB at the rate
        // seen): it is read, to be written as it is without resume state or workers. One that
        // takes longer still goes to the workers at once.
        for (size, whole) in [(64 * 1024, true), (200_000, true), (PREFETCH, false)] {
            let mut info = probed(capped, size);
            let body = probe_body(&info.final_url, paced(vec![(0, size as usize)]), size, Duration::from_millis(10));
            let live = take_start(&mut info, body, true, None, stall).await;
            assert_eq!((live.is_none(), info.prefetch.len() as u64 == size), (whole, whole), "{size} bytes");
        }

        // Seen uncapped: the start is read as if no rate could be measured, and nothing is
        // measured to record anew.
        let uncapped = "http://seen-uncapped.engine.invalid/f";
        hosts::record(&Url::parse(uncapped).unwrap(), HostProfile { capped_per_connection: Some(false), ..Default::default() });
        let mut info = probed(uncapped, PREFETCH);
        let body = probe_body(&info.final_url, capped_body(), PREFETCH, Duration::from_millis(10));
        assert!(take_start(&mut info, body, true, None, stall).await.is_none());
        assert_eq!((info.prefetch.len() as u64, info.per_setup), (PREFETCH, None));
        assert_eq!(hosts::profile(&info.final_url).capped_per_connection, Some(false));
    }

    #[tokio::test(start_paused = true)]
    async fn test_prefix_read_gives_up_on_a_stalled_body() {
        let silent = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        let mut body = Response::from(http::Response::new(reqwest::Body::wrap_stream(silent)));
        let read = tokio::time::timeout(Duration::from_secs(5), read_paced(&mut body, 256 * 1024, true)).await;
        let prefix = read.expect("the stall timeout ends the read");
        assert!(prefix.bytes.is_empty() && !prefix.live, "nothing more is coming");
        assert_eq!(verdict(&prefix), None, "a silent connection tells nothing of a cap");

        // Nor does a body that ended early leave anything to go on with.
        let mut short = paced(vec![(0, 8 * 1024)]);
        let prefix = read_paced(&mut short, 256 * 1024, false).await;
        assert_eq!((prefix.bytes.len(), prefix.live), (8 * 1024, false));
    }

    #[test]
    fn test_discard_partial_respects_the_claim_and_spares_the_final_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("file.bin");
        std::fs::write(&target, b"finished").unwrap();
        let part = part_path(&target);
        for p in [part.clone(), DownloadState::state_file_path(&part), dir.path().join("file.bin.part.hlsstate")] {
            std::fs::write(p, b"x").unwrap();
        }

        let running = claim_target(&target).unwrap().expect("free target");
        assert!(discard_partial(&target).is_err(), "a claimed target must not be cleaned");
        assert!(part.exists());
        drop(running);

        assert_eq!(discard_partial(&target).unwrap(), 3);
        assert!(!part.exists() && !lock_path(&target).exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"finished");
        assert_eq!(discard_partial(&target).unwrap(), 0);
    }

    #[test]
    fn test_discard_partial_removes_the_temporary_state_files() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("file.ts");
        // What a crash while saving a state leaves behind, for either engine.
        let tmps = ["file.ts.part.hlsstate.tmp", "file.ts.part.hfstate.tmp"].map(|name| dir.path().join(name));
        for tmp in &tmps {
            std::fs::write(tmp, b"x").unwrap();
        }
        assert_eq!(discard_partial(&target).unwrap(), 2);
        assert!(tmps.iter().all(|tmp| !tmp.exists()));
    }

    #[test]
    fn test_directory_targets() {
        let dir = tempdir().unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        assert!(is_dir_target(dir.path()));
        assert!(is_dir_target(Path::new(&format!("{}{}missing{}", dir.path().display(), sep, sep))));
        assert!(is_dir_target(Path::new("missing/")));
        assert!(!is_dir_target(&dir.path().join("missing")));
        assert!(!is_dir_target(&dir.path().join("file.bin")));
    }

    #[test]
    fn test_hls_output_takes_the_container_extension() {
        let dir = tempdir().unwrap();
        let options = DownloadOptions { output_path: Some(dir.path().to_path_buf()), ..Default::default() };
        let engine = DownloadEngine::new(Vec::new(), options);
        for (playlist, expected) in [
            ("http://h/stream.php?f=main.m3u8", "stream.ts"),
            ("http://h/INDEX.M3U8", "INDEX.ts"),
            ("http://h/live/index.m3u8", "index.ts"),
            ("http://h/video", "video.ts"),
        ] {
            let out = engine.hls_output_path(&Url::parse(playlist).unwrap(), &[]);
            assert_eq!(out, dir.path().join(expected), "{playlist}");
        }
    }

    #[tokio::test]
    async fn test_concurrent_hls_downloads_of_one_stream_use_different_files() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/live/stream.m3u8", listener.local_addr().unwrap())).unwrap();
        // The first request for the second segment is held until `release` fires.
        let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut hold = Some((held_tx, release_rx));
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let path = String::from_utf8_lossy(&req).split_whitespace().nth(1).unwrap_or("/").to_string();
                let body: &[u8] = match path.as_str() {
                    "/live/stream.m3u8" => b"#EXTM3U\n#EXTINF:4,\nseg0.ts\n#EXTINF:4,\nseg1.ts\n#EXT-X-ENDLIST\n",
                    "/live/seg0.ts" => b"AAAA",
                    _ => b"BBBB",
                };
                let gate = if path == "/live/seg1.ts" { hold.take() } else { None };
                tokio::spawn(async move {
                    if let Some((held, release)) = gate {
                        let _ = held.send(());
                        let _ = release.await;
                    }
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let dir = tempdir().unwrap();
        let engine = || {
            let options =
                DownloadOptions { output_path: Some(dir.path().to_path_buf()), num_connections: 1, ..Default::default() };
            DownloadEngine::new(vec![url.clone()], options)
        };
        let first = engine();
        let first = tokio::spawn(async move { first.run(None).await });
        tokio::time::timeout(Duration::from_secs(10), held_rx).await.unwrap().unwrap();
        // The first download's `.part` is now a resumable copy of this very stream.
        assert!(dir.path().join("stream.ts.part.hlsstate").exists());

        let second = tokio::time::timeout(Duration::from_secs(10), engine().run(None)).await.unwrap().unwrap();
        assert_eq!(second, dir.path().join("stream (1).ts"), "a running download's .part must not be reused");
        release_tx.send(()).unwrap();
        let first = tokio::time::timeout(Duration::from_secs(10), first).await.unwrap().unwrap().unwrap();
        assert_eq!(first, dir.path().join("stream.ts"));
        for path in [&first, &second] {
            assert_eq!(std::fs::read(path).unwrap(), b"AAAABBBB");
            assert!(!lock_path(path).exists(), "the claim is released when the download ends");
        }
    }

    #[tokio::test]
    async fn test_hls_claim_outlasts_a_dropped_run_until_its_writer_is_done() {
        use crate::hls::tests::{ok, serve, wait_for, DiskGate, DiskOp};
        let (addr, hits) = serve(|path: &str, _| match path {
            "/stream.m3u8" => ok(format!("#EXTM3U\n{}#EXT-X-ENDLIST\n", "#EXTINF:4,\nseg.ts\n".repeat(10))),
            _ => ok("DATA"),
        })
        .await;
        let dir = tempdir().unwrap();
        let options = DownloadOptions { output_path: Some(dir.path().to_path_buf()), num_connections: 1, ..Default::default() };
        let engine = DownloadEngine::new(vec![Url::parse(&format!("http://{addr}/stream.m3u8")).unwrap()], options);
        let writes = DiskGate::close(dir.path(), DiskOp::Write);
        let run = tokio::spawn(async move { engine.run(None).await });
        // With one connection, the third segment is requested once the first went to the writer,
        // which is stuck on it.
        wait_for("the third segment", || hits.lock().get("/seg.ts").is_some_and(|&n| n >= 3)).await;
        run.abort();
        assert!(run.await.unwrap_err().is_cancelled());

        let target = dir.path().join("stream.ts");
        let claimed = || claim_target(&target).unwrap().is_none();
        assert!(claimed(), "the writer still writes the .part");
        drop(writes);
        wait_for("the writer to release the claim", || !claimed()).await;
        assert!(dir.path().join("stream.ts.part.hlsstate").exists());
    }

    #[tokio::test]
    async fn test_hls_run_authorizes_and_never_saves_playlist_text() {
        use crate::hls::tests::{ok, serve};
        let (addr, _) = serve(|path: &str, _| match path {
            "/private/stream.m3u8" => ok("#EXTM3U\n#EXTINF:4,\nseg.ts\n#EXT-X-ENDLIST\n"),
            "/private/seg.ts" => ok("SEGMENT"),
            "/master.m3u8" => ok("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nexpired.m3u8\n"),
            "/expired.m3u8" => ok("<html>token expired</html>"),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let dir = tempdir().unwrap();
        let engine = |path: &str, auth_header: Option<String>| {
            let options = DownloadOptions { output_path: Some(dir.path().to_path_buf()), auth_header, ..Default::default() };
            DownloadEngine::new(vec![Url::parse(&format!("http://{addr}{path}")).unwrap()], options)
        };

        let path = engine("/private/stream.m3u8", Some("Bearer secret".into())).run(None).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"SEGMENT");

        let err = engine("/master.m3u8", None).run(None).await.unwrap_err();
        assert!(err.contains("not an HLS playlist"), "{err}");
        assert!(!dir.path().join("master.m3u8").exists(), "the playlist text must not pass for the download");
    }

    #[tokio::test]
    async fn test_hls_retry_with_rotated_tokens_keeps_the_name() {
        use crate::hls::tests::{ok, serve};
        let playlist_fetches = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let broken = Arc::new(AtomicBool::new(true));
        let (fetches_srv, broken_srv) = (Arc::clone(&playlist_fetches), Arc::clone(&broken));
        let (addr, _) = serve(move |path: &str, _| match path.split('?').next().unwrap_or(path) {
            "/live/stream.m3u8" => {
                let t = fetches_srv.fetch_add(1, Ordering::SeqCst);
                ok(format!("#EXTM3U\n#EXTINF:4,\nseg0.ts?t={t}\n#EXTINF:4,\nseg1.ts?t={t}\n#EXT-X-ENDLIST\n"))
            }
            "/live/seg0.ts" => ok("AAAA"),
            "/live/seg1.ts" if broken_srv.load(Ordering::SeqCst) => (404, String::new(), Vec::new()),
            "/live/seg1.ts" => ok("BBBB"),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let dir = tempdir().unwrap();
        let url = Url::parse(&format!("http://{addr}/live/stream.m3u8")).unwrap();
        let engine = || {
            let options =
                DownloadOptions { output_path: Some(dir.path().to_path_buf()), num_connections: 1, ..Default::default() };
            DownloadEngine::new(vec![url.clone()], options)
        };

        assert!(engine().run(None).await.is_err());
        broken.store(false, Ordering::SeqCst);
        let path = engine().run(None).await.unwrap();
        assert_eq!(path, dir.path().join("stream.ts"));
        assert_eq!(std::fs::read(&path).unwrap(), b"AAAABBBB");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["stream.ts"], "the first run's .part must be resumed, not orphaned");
    }

    #[tokio::test]
    async fn test_hls_never_takes_over_another_streams_part() {
        use crate::hls::tests::{ok, serve};
        let fetches = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let broken = Arc::new(AtomicBool::new(true));
        let (fetches_srv, broken_srv) = (Arc::clone(&fetches), Arc::clone(&broken));
        // Both qualities open with the same intro, differ only by queries and rotate their tokens.
        let (addr, hits) = serve(move |path: &str, _| {
            let (route, query) = path.split_once('?').unwrap_or((path, ""));
            let param = |name: &str| {
                query.split('&').find_map(|kv| kv.strip_prefix(name)?.strip_prefix('=')).unwrap_or("").to_string()
            };
            match route {
                "/video/index.m3u8" => {
                    let (q, t) = (param("q"), fetches_srv.fetch_add(1, Ordering::SeqCst));
                    let segments: String = (0..3).map(|n| format!("#EXTINF:4,\nseg.ts?q={q}&t={t}&n={n}\n")).collect();
                    ok(format!("#EXTM3U\n{segments}#EXT-X-ENDLIST\n"))
                }
                "/video/seg.ts" => match param("n").as_str() {
                    "0" => ok("INTRO "),
                    "2" if param("q") == "720" && broken_srv.load(Ordering::SeqCst) => (404, String::new(), Vec::new()),
                    n => ok(format!("{}-{n} ", param("q"))),
                },
                _ => (404, String::new(), Vec::new()),
            }
        })
        .await;
        let dir = tempdir().unwrap();
        let engine = |q: &str| {
            let url = Url::parse(&format!("http://{addr}/video/index.m3u8?q={q}")).unwrap();
            let options =
                DownloadOptions { output_path: Some(dir.path().to_path_buf()), num_connections: 1, ..Default::default() };
            DownloadEngine::new(vec![url], options)
        };
        let part = dir.path().join("index.ts.part");

        // The 720p download fails (as if paused) after two segments.
        assert!(engine("720").run(None).await.is_err());
        let paused = std::fs::read(&part).unwrap();
        assert_eq!(paused, b"INTRO 720-1 ");

        // 1080p maps to the same name and starts alike, but must neither splice onto nor wipe it.
        let path = engine("1080").run(None).await.unwrap();
        assert_eq!(path, dir.path().join("index (1).ts"));
        assert_eq!(std::fs::read(&path).unwrap(), b"INTRO 1080-1 1080-2 ");
        assert_eq!(std::fs::read(&part).unwrap(), paused, "the paused .part is untouched");

        // 720p again, with new tokens: it resumes in place.
        broken.store(false, Ordering::SeqCst);
        let path = engine("720").run(None).await.unwrap();
        assert_eq!(path, dir.path().join("index.ts"));
        assert_eq!(std::fs::read(&path).unwrap(), b"INTRO 720-1 720-2 ");
        let fetched = |seg: &str| hits.lock().iter().filter(|(p, _)| p.contains("q=720&") && p.ends_with(seg)).map(|(_, n)| n).sum::<usize>();
        assert_eq!((fetched("n=0"), fetched("n=1")), (2, 2), "checked once each after the first run, not refetched");
    }

    #[tokio::test]
    async fn test_hls_takes_stall_timeout_and_retries_from_the_options() {
        use crate::hls::tests::{ok, serve};
        let busy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let busy_srv = Arc::clone(&busy);
        let (addr, hits) = serve(move |path: &str, _| match path {
            "/stall.m3u8" => ok("#EXTM3U\n#EXTINF:4,\nstall.ts\n#EXT-X-ENDLIST\n"),
            "/busy.m3u8" => ok("#EXTM3U\n#EXTINF:4,\nbusy.ts\n#EXT-X-ENDLIST\n"),
            "/stall.ts" => (0, String::new(), Vec::new()),
            // More failures in a row than HLS used to try.
            "/busy.ts" if busy_srv.fetch_add(1, Ordering::SeqCst) < 6 => (503, String::new(), Vec::new()),
            "/busy.ts" => ok("DATA"),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let dir = tempdir().unwrap();
        let engine = |path: &str, stall_timeout_secs: u64, max_retries: u32| {
            let options = DownloadOptions {
                output_path: Some(dir.path().to_path_buf()),
                num_connections: 1,
                stall_timeout_secs,
                max_retries,
                ..Default::default()
            };
            DownloadEngine::new(vec![Url::parse(&format!("http://{addr}{path}")).unwrap()], options)
        };

        // One attempt that gives up after a second, not five of 30 s each.
        let started = Instant::now();
        let err = tokio::time::timeout(Duration::from_secs(10), engine("/stall.m3u8", 1, 0).run(None)).await.unwrap().unwrap_err();
        assert!(err.contains("after 1 attempt(s)"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());

        let path = engine("/busy.m3u8", 30, 6).run(None).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"DATA");
        assert_eq!(hits.lock()["/busy.ts"], 7);
    }

    #[tokio::test]
    async fn test_hls_is_held_to_the_speed_limit() {
        use crate::hls::tests::{ok, serve};
        const SEGMENT: usize = 48 * 1024;
        let (addr, _) = serve(|path: &str, _| match path {
            "/stream.m3u8" => ok(format!("#EXTM3U\n{}#EXT-X-ENDLIST\n", "#EXTINF:4,\nseg.ts\n".repeat(6))),
            _ => ok(vec![5u8; SEGMENT]),
        })
        .await;
        let dir = tempdir().unwrap();
        let limit = 96 * 1024;
        let options = DownloadOptions { output_path: Some(dir.path().to_path_buf()), max_speed: Some(limit), ..Default::default() };
        let engine = DownloadEngine::new(vec![Url::parse(&format!("http://{addr}/stream.m3u8")).unwrap()], options);

        let started = Instant::now();
        let path = engine.run(None).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), vec![5u8; 6 * SEGMENT]);
        // Three seconds' worth at the limit, less the tenth of a second it lets through at once.
        let least = Duration::from_secs_f64((6 * SEGMENT) as f64 / limit as f64 - 0.5);
        assert!(started.elapsed() >= least, "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn test_downloads_sharing_a_limit_stay_within_it_together() {
        use crate::hls::tests::{ok, serve};
        const SEGMENT: usize = 24 * 1024;
        let (addr, _) = serve(|path: &str, _| match path.strip_suffix(".m3u8") {
            Some(name) => ok(format!("#EXTM3U\n{}#EXT-X-ENDLIST\n", format!("#EXTINF:4,\n{name}.ts\n").repeat(6))),
            None => ok(vec![5u8; SEGMENT]),
        })
        .await;
        let dir = tempdir().unwrap();
        let limit = 96 * 1024;
        let limits = SharedLimits::default();
        let engine = |name: &str, max_speed| {
            let options = DownloadOptions { output_path: Some(dir.path().to_path_buf()), max_speed, ..Default::default() };
            DownloadEngine::new(vec![Url::parse(&format!("http://{addr}/{name}.m3u8")).unwrap()], options).sharing_limit(&limits)
        };

        let (a, b) = (engine("a", Some(limit)), engine("b", Some(limit)));
        let started = Instant::now();
        let (a, b) = tokio::join!(a.run(None), b.run(None));
        assert_eq!((a.unwrap().file_name().unwrap(), b.unwrap().file_name().unwrap()), ("a.ts".as_ref(), "b.ts".as_ref()));
        // Three seconds' worth of both at the limit, where each at a limit of its own takes half.
        let least = Duration::from_secs_f64((2 * 6 * SEGMENT) as f64 / limit as f64 - 0.5);
        assert!(started.elapsed() >= least, "{:?}", started.elapsed());

        // A download without a limit is not held to theirs.
        let started = Instant::now();
        engine("c", None).run(None).await.unwrap();
        assert!(started.elapsed() < least / 2, "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn test_hls_downloads_sharing_a_host_stay_within_its_budget() {
        use crate::hls::tests::{ok, serve_async};
        use std::sync::atomic::AtomicUsize;
        // Requests the server is answering now, and the most it ever answered at once.
        let (answering, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (answering_srv, most_srv) = (Arc::clone(&answering), Arc::clone(&most));
        let (addr, _) = serve_async(move |path: &str, _| {
            let (path, answering, most) = (path.to_string(), Arc::clone(&answering_srv), Arc::clone(&most_srv));
            async move {
                most.fetch_max(answering.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                answering.fetch_sub(1, Ordering::SeqCst);
                match path.strip_suffix(".m3u8") {
                    Some(name) => ok(format!("#EXTM3U\n{}#EXT-X-ENDLIST\n", format!("#EXTINF:4,\n{name}.ts\n").repeat(12))),
                    None => ok("DATA"),
                }
            }
        })
        .await;
        let dir = tempdir().unwrap();
        let engine = |name: &str| {
            let options = DownloadOptions {
                output_path: Some(dir.path().join(format!("{name}.ts"))),
                num_connections: 4,
                max_connections_per_host: 2,
                ..Default::default()
            };
            DownloadEngine::new(vec![Url::parse(&format!("http://{addr}/{name}.m3u8")).unwrap()], options)
        };
        let (a, b) = (engine("a"), engine("b"));

        let (a_done, b_done) = tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(a.run(None), b.run(None)) })
            .await
            .expect("both downloads finish");
        for (done, name) in [(a_done, "a.ts"), (b_done, "b.ts")] {
            assert_eq!(std::fs::read(done.unwrap()).unwrap(), b"DATA".repeat(12), "{name}");
        }
        // Playlists and segments of both, four connections each, within the host's two.
        assert!(most.load(Ordering::SeqCst) <= 2, "{} answered at once", most.load(Ordering::SeqCst));
        assert_eq!(crate::hosts::peak(&Url::parse(&format!("http://{addr}/")).unwrap()), 2);
    }

    #[tokio::test]
    async fn test_a_file_whose_writer_flushed_it_is_not_flushed_again() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("flushed.ts");
        std::fs::write(&path, b"DATA").unwrap();
        let digest = || {
            let mut hasher = StreamHasher::new(None);
            hasher.update(b"DATA");
            hasher.finish()
        };
        // Read-only: a flush, which opens the file for writing, fails.
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions.clone()).unwrap();
        let options = DownloadOptions { fsync_on_complete: true, ..Default::default() };
        let engine = DownloadEngine::new(Vec::new(), options);

        let flushed = engine.finish_external(path.clone(), unix_now(), Some(digest())).await;
        assert_eq!(flushed.unwrap(), path, "a file its engine flushed is not flushed again");
        let err = engine.finish_external(path.clone(), unix_now(), None).await.unwrap_err();
        assert!(err.contains("flush"), "any other is: {err}");
        assert!(engine.verify_written(&path, Written::Digest(digest())).await.is_err(), "so is one with a digest alone");

        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
    }

    #[tokio::test]
    async fn test_hls_playlist_may_take_every_retry_the_options_allow() {
        use crate::hls::tests::serve;
        let (addr, _) = serve(|_, _| (0, String::new(), Vec::new())).await;
        let dir = tempdir().unwrap();
        let options = DownloadOptions {
            output_path: Some(dir.path().to_path_buf()),
            stall_timeout_secs: 1,
            max_retries: 1,
            ..Default::default()
        };
        let engine = DownloadEngine::new(vec![Url::parse(&format!("http://{addr}/stalled.m3u8")).unwrap()], options);
        // Two attempts of a second each outlast HLS_PARSE_TIMEOUT, which is a second in tests: the
        // playlist fetch's own error ends the download, not the time limit.
        let err = engine.run(None).await.unwrap_err();
        assert!(err.contains("after 2 attempt(s)"), "{err}");
    }

    #[test]
    fn test_claim_is_exclusive_and_removed_when_released() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("file.bin");
        let claim = Claim::try_take(&target).unwrap().expect("free name");
        assert!(lock_path(&target).exists());
        assert!(Claim::try_take(&target).unwrap().is_none(), "a claimed name must not be claimed twice");
        drop(claim);
        assert!(!lock_path(&target).exists());
        assert!(Claim::try_take(&target).unwrap().is_some());
    }

    #[test]
    fn test_a_lock_on_a_deleted_lock_file_is_no_claim() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("file.bin");
        let first = Claim::try_take(&target).unwrap().unwrap();
        // A second download opened the lock file just before the first finished and deleted it.
        let late = OpenOptions::new().write(true).open(lock_path(&target)).unwrap();
        drop(first);
        // Where an open handle keeps a deleted name taken, no new claim (and no race) can happen.
        let Ok(Some(third)) = Claim::try_take(&target) else { return };
        late.try_lock().unwrap();
        // Both hold a lock, but only the lock on the file at the path is a claim.
        assert!(!is_file_at(&late, &lock_path(&target)).unwrap(), "a lock on a deleted file must not count");
        assert!(is_file_at(&third._lock, &lock_path(&target)).unwrap());
    }

    #[test]
    fn test_plan_skips_a_name_claimed_by_another_download() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        let history = dir.path().join("h.json");
        // Another download planned this name but has not created its .part yet.
        let other = Claim::try_take(&base).unwrap().unwrap();
        match plan_target(&base, &remote(1000), &urls(), &history, None).unwrap() {
            Plan::Fetch { final_path, resume: None, .. } => assert_eq!(final_path, dir.path().join("file (1).bin")),
            other => panic!("{other:?}"),
        }
        drop(other);

        // Same for a live download of the same URL: its .part is not stale, however it looks.
        std::fs::write(part_path(&base), vec![1u8; 1000]).unwrap();
        let mut state = state_for(1000, "\"v2\"", urls());
        state.completed_ranges.clear();
        state.save_atomic(&DownloadState::state_file_path(&part_path(&base))).unwrap();
        let live = Claim::try_take(&base).unwrap().unwrap();
        assert!(matches!(
            plan_target(&base, &remote(1000), &urls(), &history, None).unwrap(),
            Plan::Fetch { ref final_path, .. } if *final_path == dir.path().join("file (1).bin")
        ));
        assert!(part_path(&base).exists(), "a live download's .part must survive");
        drop(live);
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
    fn test_mirrors_start_from_their_probed_answer_times() {
        let mirror = |host: &str, answer_ms: Option<u64>| {
            let mut m = remote(1000);
            m.url = Url::parse(&format!("http://{host}/file.bin")).unwrap();
            m.final_url = m.url.clone();
            m.answer_time = answer_ms.map(Duration::from_millis);
            m
        };
        // The second mirror answered its probe ten times sooner: the first request goes there.
        let racer = build_racer(&[mirror("far.example", Some(300)), mirror("near.example", Some(30))]);
        assert_eq!(racer.select_best_mirror(), Some(1));
        assert_eq!(racer.get_mirror(1).unwrap().ttfb_ewma_ms, 30.0);
        assert_eq!(racer.get_mirror(0).unwrap().if_range.as_deref(), Some("\"v1\""));
        // Equal answers, or none measured: the first listed mirror leads.
        let even = build_racer(&[mirror("a.example", Some(30)), mirror("b.example", Some(30))]);
        assert_eq!(even.select_best_mirror(), Some(0));
        let unmeasured = build_racer(&[mirror("a.example", None), mirror("b.example", None)]);
        assert_eq!(unmeasured.select_best_mirror(), Some(0));
    }

    #[test]
    fn test_chunks_stay_small_while_a_checksum_hashes_the_file_from_its_start() {
        const MB: u64 = 1024 * 1024;
        assert_eq!(effective_chunk_size(4096 * MB, 16, DEFAULT_CHUNK_SIZE, false), 128 * MB);
        assert_eq!(effective_chunk_size(4096 * MB, 16, DEFAULT_CHUNK_SIZE, true), IN_ORDER_CHUNK);
        // Chunks that small already stay, and so does a size the user chose.
        assert_eq!(effective_chunk_size(20 * MB, 8, DEFAULT_CHUNK_SIZE, true), 4 * MB);
        assert_eq!(effective_chunk_size(4096 * MB, 16, 32 * MB, true), 32 * MB);

        let (hex64, hex32) = ("ab".repeat(32), "ab".repeat(16));
        for (checksum, in_order) in [
            (None, false),
            (Some(format!("blake3:{hex64}")), false),
            (Some(format!("sha256:{hex64}")), true),
            (Some(format!("md5:{hex32}")), true),
            // Either SHA-256 or BLAKE3: SHA-256 is taken too.
            (Some(hex64.clone()), true),
        ] {
            assert_eq!(crate::storage::hashes_prefix(checksum.as_deref()), in_order, "{checksum:?}");
        }
    }

    #[test]
    fn test_steals_take_a_small_floor_unless_the_user_set_one() {
        // Time decides a steal now; the floor no longer grows with the chunk size.
        assert_eq!(min_steal(&DownloadOptions::default()), 64 * 1024);
        let set = DownloadOptions { min_steal_threshold: 8 * 1024 * 1024, ..Default::default() };
        assert_eq!(min_steal(&set), 8 * 1024 * 1024);
    }

    #[test]
    fn test_requests_go_where_the_probe_was_redirected() {
        let mut redirected = remote(1000);
        redirected.final_url = Url::parse("https://cdn.example/file.bin?sig=1").unwrap();
        let racer = build_racer(&[redirected.clone(), remote(1000)]);
        let (first, second) = (racer.get_mirror(0).unwrap(), racer.get_mirror(1).unwrap());
        assert_eq!((&first.url, first.fallback.as_ref()), (&redirected.final_url, Some(&redirected.url)));
        assert_eq!((&second.url, second.fallback.as_ref()), (&remote(1000).url, None), "nothing to fall back to");
    }

    #[test]
    fn test_the_default_host_budget_never_holds_one_download_back() {
        assert!(DownloadOptions::default().max_connections_per_host >= MAX_CONNECTIONS);
    }

    /// A local server of `data` at every path: HEAD, and GETs honouring `Range` whose bodies come
    /// 64 KiB per `pace`. `/moved/<name>` redirects to `/<name>`, the first GET of `/busy/<name>`
    /// and every GET of `/refused/<name>` are refused with 503, GETs of `/untagged/<name>` carry
    /// no ETag, and the first GET of `/stall/<name>` past the file's start goes silent after
    /// 64 KiB.
    async fn file_server(data: Vec<u8>, pace: Duration) -> Url {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = crate::hosts::unseen_listener().await;
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let data = Arc::new(data);
        let seen = Arc::new(Mutex::new(std::collections::HashSet::new()));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (data, seen) = (Arc::clone(&data), Arc::clone(&seen));
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                    let mut words = head.split_whitespace();
                    let (method, path) = (words.next().unwrap_or_default(), words.next().unwrap_or("/").to_string());
                    let (start, end) = head
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes=")?.trim().split_once('-'))
                        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?.min(data.len() - 1))))
                        .unwrap_or((0, data.len() - 1));
                    let stall = path.starts_with("/stall/");
                    let first = method == "get" && (!stall || start > 0) && seen.lock().insert(path.clone());
                    let answer = if let Some(name) = path.strip_prefix("/moved/") {
                        format!("HTTP/1.1 302 Found\r\nLocation: /{name}\r\nContent-Length: 0\r\n")
                    } else if (first && path.starts_with("/busy/")) || (method == "get" && path.starts_with("/refused/")) {
                        "HTTP/1.1 503 Busy\r\nRetry-After: 0\r\nContent-Length: 0\r\n".to_string()
                    } else if method == "head" {
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\n", data.len())
                    } else {
                        let (total, len) = (data.len(), end + 1 - start);
                        let etag = if path.starts_with("/untagged/") { "" } else { "ETag: \"v1\"\r\n" };
                        format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {len}\r\n{etag}")
                    };
                    if socket.write_all(format!("{answer}Connection: close\r\n\r\n").as_bytes()).await.is_err() {
                        return;
                    }
                    if !answer.starts_with("HTTP/1.1 206") {
                        return;
                    }
                    let silent = stall && first;
                    for piece in data[start..=end].chunks(64 * 1024).take(if silent { 1 } else { usize::MAX }) {
                        tokio::time::sleep(pace).await;
                        if socket.write_all(piece).await.is_err() {
                            return;
                        }
                    }
                    if silent {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                });
            }
        });
        base
    }

    #[tokio::test]
    async fn test_only_a_first_try_seeds_a_mirrors_answer_time() {
        let base = file_server(vec![7u8; 4096], Duration::ZERO).await;
        let client = Client::new();
        let probe = |path: &str| {
            let (client, url) = (client.clone(), base.join(path).unwrap());
            async move { probe_url(&client, None, &url, false, 0).await.unwrap().0 }
        };
        // Chunk requests skip the redirect the probe went through: its wait overstates theirs, but
        // unlike an assumed one never makes a slow target look quick. What a new connection to the
        // target's host needs is only learned from a request straight to it.
        let moved = probe("moved/file.bin").await;
        assert_eq!(moved.final_url, base.join("file.bin").unwrap());
        let waited = moved.answer_time.expect("the redirected first try's wait");
        assert_eq!(hosts::profile(&base).setup_time, None);
        let racer = build_racer(&[moved]);
        assert_eq!(racer.get_mirror(0).unwrap().ttfb_ewma_ms, waited.as_secs_f64() * 1000.0);
        assert!(probe("file.bin").await.answer_time.is_some());
        // A retry may have reused the connection of the try before.
        let busy = probe("busy/file.bin").await;
        assert!(busy.accepts_ranges);
        assert_eq!(busy.answer_time, None);
    }

    #[tokio::test]
    async fn test_head_without_a_slot_of_its_own_holds_up_no_probe() {
        let base = file_server(vec![7u8; 4096], Duration::ZERO).await;
        let client = Client::new();
        // One request to the host at a time, which the ranged GET holds: HEAD has no slot. A GET
        // naming the file without a validator would otherwise leave HEAD its grace.
        let started = std::time::Instant::now();
        let (info, _) = probe_url(&client, None, &base.join("untagged/f.bin").unwrap(), false, 1).await.unwrap();
        assert!(started.elapsed() < HEAD_GRACE, "HEAD was waited for: {:?}", started.elapsed());
        assert_eq!((info.size, info.accepts_ranges, info.etag), (Some(4096), true, None));
        // A GET refused on every try leaves nothing to read: HEAD goes out after it, under its
        // slot, and says what the file is.
        let refused = base.join("refused/f.bin").unwrap();
        let (info, body) = probe_url(&client, None, &refused, true, 1).await.expect("HEAD answers for the refused GET");
        assert_eq!((info.size, info.accepts_ranges, info.etag.as_deref()), (Some(4096), true, Some("\"v1\"")));
        assert!(body.is_none());
    }

    #[tokio::test]
    async fn test_probes_still_out_are_given_up_when_the_probe_brought_the_whole_file() {
        use tokio::io::AsyncReadExt;
        let file = file_server(vec![7u8; 4096], Duration::ZERO).await.join("f.bin").unwrap();
        // A second mirror that takes requests but never answers them, and tells when they are
        // given up.
        let listener = crate::hosts::unseen_listener().await;
        let silent = Url::parse(&format!("http://{}/f.bin", listener.local_addr().unwrap())).unwrap();
        let (given_up_tx, mut given_up) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let given_up_tx = given_up_tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while socket.read(&mut buf).await.is_ok_and(|n| n > 0) {}
                    let _ = given_up_tx.send(Instant::now());
                });
            }
        });
        let dir = tempdir().unwrap();
        let options = DownloadOptions { output_path: Some(dir.path().to_path_buf()), ..Default::default() };
        let path = DownloadEngine::new(vec![file, silent], options).run(None).await.unwrap();
        // Nothing runs between the end of the download and this: whatever the mirror noticed
        // before, it noticed while the download wrote and checked the file.
        let finished = Instant::now();
        assert_eq!(std::fs::read(path).unwrap(), vec![7u8; 4096]);
        let given_up = tokio::time::timeout(Duration::from_secs(10), given_up.recv()).await.unwrap().unwrap();
        assert!(given_up < finished, "the second mirror's probe held its host slot until the download was over");
    }

    #[tokio::test]
    async fn test_a_web_page_from_google_drive_fails_its_probe() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Every answer comes through this proxy: a web page, as Drive sends for a private or
        // deleted file.
        let proxy = crate::hosts::unseen_listener().await;
        let options = DownloadOptions { proxy: Some(format!("http://{}", proxy.local_addr().unwrap())), ..Default::default() };
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = proxy.accept().await {
                tokio::spawn(async move {
                    let mut head = [0u8; 4096];
                    let _ = socket.read(&mut head).await;
                    let page = "<html>You can't view or download this file at this time.</html>";
                    let answer = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                        page.len()
                    );
                    let _ = socket.write_all(answer.as_bytes()).await;
                });
            }
        });
        let drive = Url::parse("http://drive.usercontent.google.com/download?id=abc&export=download&confirm=t").unwrap();
        let engine = DownloadEngine::new(vec![drive.clone()], options);
        let client = engine.client.clone().unwrap();
        let err = engine.probe_all(&client, &[drive]).await.err().expect("a web page is no file");
        assert!(err.contains("Google Drive served a web page instead of the file"), "{err}");
        // The same answer from anywhere else is what was asked for.
        let elsewhere = Url::parse("http://files.engine.invalid/download?id=abc").unwrap();
        assert!(engine.probe_all(&client, &[elsewhere]).await.is_ok());
    }

    #[tokio::test]
    async fn test_a_probe_tells_what_its_host_does() {
        let url = file_server(vec![7u8; 4096], Duration::ZERO).await.join("file.bin").unwrap();
        let client = Client::new();
        // Nothing went to the host before: the probe's connection is new, and its wait what a
        // new one needs.
        let (probe, _) = probe_url(&client, None, &url, false, 0).await.unwrap();
        let seen = hosts::profile(&url);
        assert_eq!(seen.accepts_ranges, Some(true));
        assert!(seen.setup_time.is_some() && seen.setup_time == probe.answer_time, "{seen:?}");
        // Right after, a connection may be open for reuse: the next probe's wait tells nothing.
        let cold = Duration::from_secs(7);
        hosts::record(&url, HostProfile { setup_time: Some(cold), ..Default::default() });
        probe_url(&client, None, &url, false, 0).await.unwrap();
        assert_eq!(hosts::profile(&url).setup_time, Some(cold));
    }

    #[test]
    fn test_a_download_opens_no_more_connections_than_its_hosts_take() {
        hosts::record(&Url::parse("http://capped.room.engine.invalid/").unwrap(), HostProfile { connection_cap: Some(3), ..Default::default() });
        let mirrors = [
            probed("http://capped.room.engine.invalid/a", 1000),
            probed("http://open.room.engine.invalid/a", 1000),
            probed("http://capped.room.engine.invalid/b", 1000),
        ];
        assert_eq!(host_room(&mirrors, 8), 3 + 8, "per host, not per mirror");
        assert_eq!(host_room(&mirrors, 2), 2 + 2);
        assert_eq!(host_room(&mirrors[..1], 0), 3);
        assert_eq!(host_room(&mirrors, 0), usize::MAX, "no limit, and no cap seen on one host");
    }

    #[tokio::test]
    async fn test_downloads_sharing_a_host_stay_within_its_budget_through_steals_and_takeovers() {
        const MIB: usize = 1024 * 1024;
        let data: Vec<u8> = (0..9 * MIB).map(|i| (i % 251) as u8).collect();
        let base = file_server(data.clone(), Duration::from_millis(2)).await;
        let dir = tempdir().unwrap();
        let engine = |path: &str, out: &str| {
            let options = DownloadOptions {
                num_connections: 8,
                base_chunk_size: 256 * 1024,
                max_connections_per_host: 4,
                output_path: Some(dir.path().join(out)),
                ..Default::default()
            };
            DownloadEngine::new(vec![base.join(path).unwrap()], options)
        };
        // Steals are on (the default floor), and one connection goes silent to be taken over.
        let (a, b) = (engine("stall/a.bin", "a.bin"), engine("b.bin", "b.bin"));

        let (a_done, b_done) = tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(a.run(None), b.run(None)) })
            .await
            .expect("both downloads finish");
        for (done, out) in [(a_done, "a.bin"), (b_done, "b.bin")] {
            assert_eq!(done.unwrap(), dir.path().join(out));
            assert!(std::fs::read(dir.path().join(out)).unwrap() == data, "{out} differs");
        }
        assert_eq!(crate::hosts::peak(&base), 4, "both downloads' workers share the host's four slots");
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
    fn test_select_mirrors_prefers_a_mirror_that_takes_ranges() {
        let mut edge = remote(1000);
        edge.accepts_ranges = false;
        edge.etag = None;
        let mut bigger = remote(2000);
        bigger.url = Url::parse("http://b.example.com/file.bin").unwrap();
        let mut origin = remote(1000);
        origin.url = Url::parse("http://c.example.com/file.bin").unwrap();

        // The first probe defines the file; the first mirror serving it with ranges leads.
        let probes = vec![Ok(edge.clone()), Ok(bigger.clone()), Ok(origin.clone())];
        let (reference, mirrors) = select_mirrors(probes).unwrap();
        assert_eq!(reference.url, origin.url);
        let hosts: Vec<_> = mirrors.iter().map(|m| m.url.host_str().unwrap().to_string()).collect();
        assert_eq!(hosts, vec!["c.example.com"]);

        // Without such a mirror, or when the first probe brought the whole file, the first leads.
        assert_eq!(select_mirrors(vec![Ok(edge.clone()), Ok(bigger)]).unwrap().0.url, edge.url);
        edge.prefetch = Bytes::from(vec![0u8; 1000]);
        assert_eq!(select_mirrors(vec![Ok(edge.clone()), Ok(origin)]).unwrap().0.url, edge.url);
    }

    #[test]
    fn test_select_mirrors_keeps_the_first_probes_identity_after_a_reference_swap() {
        // A defines the file (ETag "X") but ignores ranges; C is a stale copy (ETag "Y");
        // B serves the file with ranges but sends no ETag, so B leads.
        let mut a = remote(1000);
        a.accepts_ranges = false;
        a.etag = Some("\"X\"".into());
        let mut c = remote(1000);
        c.url = Url::parse("http://c.example.com/file.bin").unwrap();
        c.etag = Some("\"Y\"".into());
        let mut b = remote(1000);
        b.url = Url::parse("http://b.example.com/file.bin").unwrap();
        b.etag = None;

        let (reference, mirrors) = select_mirrors(vec![Ok(a), Ok(c), Ok(b.clone())]).unwrap();
        assert_eq!(reference.url, b.url);
        let hosts: Vec<_> = mirrors.iter().map(|m| m.url.host_str().unwrap().to_string()).collect();
        assert_eq!(hosts, vec!["b.example.com"], "the stale copy must never be mixed in");
    }

    #[test]
    fn test_plan_never_reuses_unrelated_file() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        std::fs::write(&base, vec![7u8; 1000]).unwrap(); // same name AND same size
        let history = dir.path().join("h.json");
        match plan_target(&base, &remote(1000), &urls(), &history, None).unwrap() {
            Plan::Fetch { final_path, resume: None, .. } => assert_eq!(final_path, dir.path().join("file (1).bin")),
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(&base).unwrap(), vec![7u8; 1000]);
        assert!(!lock_path(&base).exists(), "no lock file may be left next to a skipped name");
    }

    #[test]
    fn test_plan_history_requires_exact_path() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        std::fs::write(&base, vec![7u8; 1000]).unwrap();
        let history = dir.path().join("h.json");
        let mut recorded = DownloadHistoryManager::load_from_path(&history);

        // Same name and URL, but recorded in another directory: not this file.
        let elsewhere = dir.path().join("other").join("file.bin");
        recorded.add_or_update(completed_entry(&elsewhere, 1000, urls()));
        assert!(matches!(plan_target(&base, &remote(1000), &urls(), &history, None).unwrap(), Plan::Fetch { .. }));

        recorded.add_or_update(completed_entry(&base, 1000, urls()));
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
    fn test_plan_finds_the_history_of_a_link_with_secrets() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        std::fs::write(&base, vec![7u8; 1000]).unwrap();
        let history = dir.path().join("h.json");
        let signed = vec!["http://example.com/file.bin?X-Amz-Signature=abc".to_string()];
        DownloadHistoryManager::load_from_path(&history).add_or_update(completed_entry(&base, 1000, signed.clone()));
        // History saved the link without its signature; the live link is compared the same way.
        assert!(matches!(
            plan_target(&base, &remote(1000), &signed, &history, None).unwrap(),
            Plan::AlreadyDone(p) if p == base
        ));
    }

    #[test]
    fn test_plan_checksum_decides_for_existing_file() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        std::fs::write(&base, b"hello").unwrap();
        let history = dir.path().join("h.json");
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
        let history = dir.path().join("h.json");

        std::fs::write(&part, vec![1u8; 1000]).unwrap();
        state_for(1000, "\"v1\"", urls()).save_atomic(&part_state).unwrap();
        match plan_target(&base, &remote(1000), &urls(), &history, None).unwrap() {
            Plan::Fetch { final_path, resume: Some(state), .. } => {
                assert_eq!(final_path, base);
                assert_eq!(state.completed_ranges, vec![ByteRange::new(0, 499).unwrap()]);
            }
            other => panic!("{other:?}"),
        }

        // The same file, but the server takes no range requests right now (a CDN cache miss,
        // say): the progress cannot resume, and it is not stale either, so it is kept.
        let mut no_ranges = remote(1000);
        no_ranges.accepts_ranges = false;
        let err = plan_target(&base, &no_ranges, &urls(), &history, None).unwrap_err();
        assert!(err.contains("kept"), "{err}");
        assert!(part.exists() && part_state.exists() && !lock_path(&base).exists());
        // Unless the probe brought the whole file: then nothing is lost.
        let mut whole = no_ranges.clone();
        whole.prefetch = Bytes::from(vec![1u8; 1000]);
        assert!(matches!(
            plan_target(&base, &whole, &urls(), &history, None).unwrap(),
            Plan::Fetch { resume: None, ref final_path, .. } if *final_path == base
        ));
        assert!(!part.exists() && !part_state.exists());
        std::fs::write(&part, vec![1u8; 1000]).unwrap();
        state_for(1000, "\"v1\"", urls()).save_atomic(&part_state).unwrap();

        // The server's ETag changed: the partial data is stale and gets thrown away.
        let mut changed = remote(1000);
        changed.etag = Some("\"v2\"".into());
        assert!(matches!(
            plan_target(&base, &changed, &urls(), &history, None).unwrap(),
            Plan::Fetch { resume: None, ref final_path, .. } if *final_path == base
        ));
        assert!(!part.exists() && !part_state.exists());

        // An interrupted single-stream download recorded no progress: nothing to keep.
        std::fs::write(&part, vec![1u8; 1000]).unwrap();
        let mut nothing = state_for(1000, "\"v1\"", urls());
        nothing.completed_ranges.clear();
        nothing.save_atomic(&part_state).unwrap();
        assert!(matches!(
            plan_target(&base, &no_ranges, &urls(), &history, None).unwrap(),
            Plan::Fetch { resume: None, ref final_path, .. } if *final_path == base
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
        let history = dir.path().join("h.json");

        match plan_target(&base, &remote(1000), &urls(), &history, None).unwrap() {
            Plan::Fetch { final_path, resume: Some(_), .. } => assert_eq!(final_path, base),
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

    #[test]
    fn test_plan_reads_history_only_to_judge_an_existing_file() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("file.bin");
        let history = dir.path().join("h.json");
        DownloadHistoryManager::load_from_path(&history).add_or_update(completed_entry(&base, 1000, urls()));
        let reads = || crate::history::READS.with(std::cell::Cell::get);
        let before = reads();

        assert!(matches!(plan_target(&base, &remote(1000), &urls(), &history, None).unwrap(), Plan::Fetch { .. }));
        assert_eq!(reads(), before, "no file to judge, so no history to read");
        std::fs::write(&base, vec![7u8; 1000]).unwrap();
        assert!(matches!(plan_target(&base, &remote(1000), &urls(), &history, None).unwrap(), Plan::AlreadyDone(_)));
        assert_eq!(reads(), before + 1);
    }

    fn engine_with(options: DownloadOptions) -> DownloadEngine {
        DownloadEngine::new(vec![Url::parse("http://example.com/file.bin").unwrap()], options)
    }

    /// A `.part` of `final_path` holding `data`, written through a writer that hashed it.
    fn written_part(final_path: &Path, data: &[u8]) -> DiskWriter {
        let writer = DiskWriter::open_or_create(part_path(final_path), data.len() as u64).unwrap();
        writer.track_digest(&[], None);
        writer.write_chunk_slice(0, data).unwrap();
        writer
    }

    #[tokio::test]
    async fn test_a_mismatch_found_while_writing_is_confirmed_by_reading_the_file() {
        let dir = tempdir().unwrap();
        let (seen, on_disk) = (vec![1u8; 5000], vec![2u8; 5000]);
        let checksum = Some(blake3::hash(&on_disk).to_hex().to_string());

        // The bytes on disk are not the ones the writer hashed: the checksum decides by the file.
        let target = dir.path().join("kept.bin");
        let writer = written_part(&target, &seen);
        std::io::Write::write_all(&mut File::options().write(true).open(part_path(&target)).unwrap(), &on_disk).unwrap();
        let engine = engine_with(DownloadOptions { expected_checksum: checksum.clone(), ..Default::default() });
        let claim = Claim::try_take(&target).unwrap().unwrap();
        let done = engine.finalize(target.clone(), claim, Written::Writer(writer), unix_now(), &None).await;
        assert_eq!(done.unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), on_disk);

        // A file that really differs is still discarded.
        let target = dir.path().join("discarded.bin");
        let writer = written_part(&target, &seen);
        let claim = Claim::try_take(&target).unwrap().unwrap();
        let err = engine.finalize(target.clone(), claim, Written::Writer(writer), unix_now(), &None).await.unwrap_err();
        assert!(err.contains("Checksum verification failed"), "{err}");
        assert!(!part_path(&target).exists() && !target.exists());
    }

    #[tokio::test]
    async fn test_completion_waits_for_the_disk_only_with_fsync_on_complete() {
        let dir = tempdir().unwrap();
        for fsync in [false, true] {
            let target = dir.path().join(format!("fsync-{fsync}.bin"));
            let writer = written_part(&target, b"abc");
            let engine = engine_with(DownloadOptions { fsync_on_complete: fsync, ..Default::default() });
            let claim = Claim::try_take(&target).unwrap().unwrap();
            let done = engine.finalize(target.clone(), claim, Written::Writer(writer.clone()), unix_now(), &None).await;
            assert_eq!(done.unwrap(), target);
            assert_eq!(writer.sync_count(), usize::from(fsync), "fsync_on_complete: {fsync}");
            assert_eq!(std::fs::read(&target).unwrap(), b"abc");
        }
    }

    #[tokio::test]
    async fn test_a_single_stream_is_hashed_from_its_first_byte_as_it_is_written() {
        use sha2::Digest;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let data: Vec<u8> = (0..3 * PREFETCH as usize + 12_345).map(|i| (i * 31 % 251) as u8).collect();
        // A server ignoring ranges whose first answer breaks off halfway: the stream starts over.
        let listener = crate::hosts::unseen_listener().await;
        let url = Url::parse(&format!("http://{}/whole.bin", listener.local_addr().unwrap())).unwrap();
        let gets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (served, answers) = (Arc::new(data.clone()), Arc::clone(&gets));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (data, gets) = (Arc::clone(&served), Arc::clone(&answers));
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let answer = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len());
                    if socket.write_all(answer.as_bytes()).await.is_err() || head.starts_with(b"HEAD") {
                        return;
                    }
                    let body = match gets.fetch_add(1, Ordering::SeqCst) {
                        0 => &data[..data.len() / 2],
                        _ => &data[..],
                    };
                    let _ = socket.write_all(body).await;
                });
            }
        });
        let dir = tempdir().unwrap();
        let sha256: String = sha2::Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
        let checksum = format!("sha256:{sha256}");
        let options =
            DownloadOptions { output_path: Some(dir.path().to_path_buf()), expected_checksum: Some(checksum), ..Default::default() };
        let path = DownloadEngine::new(vec![url], options).run(None).await.expect("the second try brings the file");

        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(gets.load(Ordering::SeqCst), 2, "the broken answer, then the whole file");
        // The broken try's bytes are not in the digest, or the checksum would call for a re-read.
        let read_back = crate::storage::READ_BACK.lock().unwrap().clone();
        assert!(!read_back.iter().any(|p| p.starts_with(dir.path())), "read back: {read_back:?}");
    }

    #[tokio::test]
    async fn test_flush_and_hash_run_at_the_same_time() {
        // Each waits for the other to have started: run one after the other, both would time out.
        let (flush_started, flush_rx) = std::sync::mpsc::channel();
        let (hash_started, hash_rx) = std::sync::mpsc::channel();
        let hash = move || {
            let _ = hash_started.send(());
            flush_rx
                .recv_timeout(Duration::from_secs(10))
                .map(|()| "digest".to_string())
                .map_err(|_| VerifyError::Io("the flush did not run alongside".into()))
        };
        let flush = move || {
            let _ = flush_started.send(());
            hash_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "the hash did not run alongside".to_string())
        };
        let no_reread = None::<fn() -> Result<String, VerifyError>>;
        assert_eq!(hash_and_flush(hash, Some(flush), no_reread).await.unwrap(), "digest");

        // A failed flush fails the download, so the file is not renamed.
        let digest = || Ok("digest".to_string());
        let (failed, mismatch) = (|| Err("disk gone".to_string()), || Err(VerifyError::Mismatch("differs".into())));
        let err = hash_and_flush(digest, Some(failed), no_reread).await.unwrap_err();
        assert!(matches!(&err, VerifyError::Io(e) if e.contains("disk gone")), "{err}");
        // Also when a mismatch the digest reported is overturned by reading the file back.
        let err = hash_and_flush(mismatch, Some(failed), Some(digest)).await.unwrap_err();
        assert!(matches!(&err, VerifyError::Io(e) if e.contains("disk gone")), "{err}");
        assert_eq!(hash_and_flush(mismatch, Some(|| Ok(())), Some(digest)).await.unwrap(), "digest");
        // A mismatch the file confirms is one, flushed or not: the file is discarded.
        let err = hash_and_flush(mismatch, Some(failed), Some(mismatch)).await.unwrap_err();
        assert!(matches!(err, VerifyError::Mismatch(_)), "{err}");
    }

    #[tokio::test]
    async fn test_bad_proxy_is_reported_not_ignored() {
        let options = DownloadOptions { proxy: Some("::not a proxy::".into()), ..Default::default() };
        let engine = DownloadEngine::new(vec![Url::parse("http://127.0.0.1:9/x.bin").unwrap()], options);
        let err = engine.run(None).await.unwrap_err();
        assert!(err.contains("proxy"), "{err}");
    }

    #[test]
    fn test_a_download_follows_at_most_three_links_and_never_back() {
        let given = Url::parse("http://short.engine.invalid/x").unwrap();
        let answer = probed(given.as_str(), 100);
        let next = Url::parse("http://files.engine.invalid/a.bin").unwrap();
        let after = |follows, tried: &[&Url]| Route { follows, tried: tried.iter().map(|u| (*u).clone()).collect(), scrape: true };
        assert_eq!(after(0, &[&given]).goes_on(&next, &answer), Ok(true));
        assert_eq!(after(MAX_FOLLOWS - 1, &[&given]).goes_on(&next, &answer), Ok(true));
        assert_eq!(after(MAX_FOLLOWS, &[&given]).goes_on(&next, &answer), Ok(false));
        // Never to a link the download tried: one whose answer it had, or one it followed. A
        // link it was only given is not among them (see the integration tests).
        assert_eq!(after(0, &[&given]).goes_on(&given, &answer), Ok(false));
        assert_eq!(after(1, &[&given, &next]).goes_on(&next, &answer), Ok(false));
        // An answer the download does not follow on from is judged as the probe skipped it...
        let html = |mut info: ProbeInfo| {
            info.headers.insert(reqwest::header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
            info
        };
        let drive = html(probed("http://drive.usercontent.google.com/download?id=abc&export=download&confirm=t", 100));
        let err = after(MAX_FOLLOWS, &[]).goes_on(&next, &drive).unwrap_err();
        assert!(err.contains("Google Drive served a web page instead of the file"), "{err}");
        // ... and as if the link had been where it landed.
        let mut landed = html(probed(given.as_str(), 100));
        landed.final_url = drive.url.clone();
        let err = after(MAX_FOLLOWS, &[]).goes_on(&next, &landed).unwrap_err();
        assert!(err.starts_with(&format!("{given}: ")) && err.contains("Google Drive served a web page"), "{err}");
    }

    #[test]
    fn test_a_page_refreshing_at_once_names_its_target() {
        let page = Url::parse("https://t.co/abc").unwrap();
        let target = |meta: &str| HtmlVideoResolver::meta_refresh(&format!("<head>{meta}</head>"), &page);
        let a = Url::parse("https://example.com/a").unwrap();
        // t.co's own answer, and the case, spacing, separator and quoting variants browsers read alike.
        for meta in [
            r#"<noscript><META http-equiv="refresh" content="0;URL=https://example.com/a"></noscript>"#,
            r#"<meta HTTP-EQUIV="Refresh" CONTENT="0; url=https://example.com/a">"#,
            r#"<meta http-equiv="refresh" content="0;URL='https://example.com/a'">"#,
            r#"<meta http-equiv='refresh' content='0, URL="https://example.com/a"'>"#,
            r#"<meta http-equiv="refresh" content="  0 ;  url = https://example.com/a  ">"#,
            r#"<meta http-equiv="refresh" content="0;https://example.com/a">"#,
            r#"<meta http-equiv="refresh" content="0.5; url=https://example.com/a">"#,
        ] {
            assert_eq!(target(meta).as_ref(), Some(&a), "{meta}");
        }
        let relative = target(r#"<meta http-equiv="refresh" content="0; url=/dl/f.zip?x=1&amp;y=2">"#);
        assert_eq!(relative.map(String::from).as_deref(), Some("https://t.co/dl/f.zip?x=1&y=2"));
        // For browsers without JavaScript only, a page asks for it on its own site (as Google's
        // answers do), or sends the browser on to another site (as t.co's do).
        let enable_js = r#"<noscript><meta content="0;url=/httpservice/retry/enablejs?sei=x" http-equiv="refresh"></noscript>"#;
        assert_eq!(target(enable_js), None);
        let both = format!(r#"{enable_js}<noscript><meta http-equiv="refresh" content="0;url=https://example.com/a"></noscript>"#);
        assert_eq!(target(&both).as_ref(), Some(&a));
        let after_noscript = r#"<noscript><style>p{display:none}</style></noscript><meta http-equiv="refresh" content="0; url=/next">"#;
        assert_eq!(target(after_noscript).map(String::from).as_deref(), Some("https://t.co/next"));
        for meta in [
            // A timed page is for reading first; one refreshing without a target only reloads.
            r#"<meta http-equiv="refresh" content="5; url=https://example.com/a">"#,
            r#"<meta http-equiv="refresh" content="0">"#,
            r#"<meta http-equiv="refresh" content="0; url=javascript:alert(1)">"#,
            r#"<meta http-equiv="refresh" content="0; url=ftp://example.com/a">"#,
            r#"<meta http-equiv="content-type" content="0; url=https://example.com/a">"#,
            r#"<meta name="refresh" content="0; url=https://example.com/a">"#,
            r#"<NOSCRIPT><meta http-equiv="refresh" content="0; url=https://t.co/abc?js=0"></NOSCRIPT>"#,
        ] {
            assert_eq!(target(meta), None, "{meta}");
        }
    }

    #[test]
    fn test_link_shorteners_and_mail_scanners_are_known_by_host() {
        let host = |url: &str| shortener_host(&Url::parse(url).unwrap()).map(str::to_string);
        for url in [
            "https://bit.ly/3xYz",
            "https://tinyurl.com/abc",
            "https://lnkd.in/eAbc",
            "https://nam12.safelinks.protection.outlook.com/?url=https%3A%2F%2Fexample.com",
            "https://urldefense.proofpoint.com/v2/url?u=x",
            "https://protect-eu.mimecast.com/s/abc",
            "https://url.uk.m.mimecastprotect.com/s/abc",
        ] {
            let expected = Url::parse(url).unwrap().host_str().map(str::to_string);
            assert_eq!(host(url), expected, "{url}");
        }
        // t.co's page sends the browser on (see `meta_refresh`); lookalikes are no shorteners.
        for url in [
            "https://t.co/abc",
            "https://example.com/bit.ly",
            "https://notbit.ly/x",
            "https://bit.ly.example.com/x",
            "https://safelinks.protection.outlook.com/?url=x",
            "https://protect-.mimecast.com/s/abc",
            "https://mimecast.com/s/abc",
        ] {
            assert_eq!(host(url), None, "{url}");
        }
    }

    #[test]
    fn test_media_quality_is_the_one_asked_for_else_the_one_for_pages() {
        use crate::media::MediaQualityPreset::{AudioMp3, Hd720p};
        let preset = |media_preset, page_media_preset| {
            engine_with(DownloadOptions { media_preset, page_media_preset, ..Default::default() }).media_options().preset
        };
        assert_eq!(preset(None, None), crate::media::MediaQualityPreset::default());
        assert_eq!(preset(None, Some(AudioMp3)), AudioMp3);
        assert_eq!(preset(Some(Hd720p), Some(AudioMp3)), Hd720p);
        // It changes nothing the client is built from, and is kept with the other options.
        let with = DownloadOptions { page_media_preset: Some(AudioMp3), ..Default::default() };
        assert_eq!(ClientKey::of(&with), ClientKey::of(&DownloadOptions::default()));
        let back: DownloadOptions = serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
        assert_eq!(back.page_media_preset, Some(AudioMp3));
        assert_eq!(serde_json::from_str::<DownloadOptions>("{}").unwrap().page_media_preset, None);
    }
}
