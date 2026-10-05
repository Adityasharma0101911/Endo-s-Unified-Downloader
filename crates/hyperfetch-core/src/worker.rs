use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt};
use parking_lot::Mutex;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED,
    LOCATION, RANGE, RETRY_AFTER,
};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::chunk::{Chunk, ChunkManager, StealRule, StealTiming};
use crate::hosts::{self, HostKey, HostProfile, HostSlot};
use crate::mirror::{Mirror, MirrorRacer};
use crate::range::ByteRange;
use crate::storage::DiskWriter;

/// How long an idle worker waits before looking for work again.
const IDLE_POLL: Duration = Duration::from_millis(50);
/// Upper bound on a server-provided Retry-After.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// Retry delay for a chunk refused because the server allows fewer connections than we opened.
const THROTTLE_RETRY: Duration = Duration::from_millis(500);
/// Mirror pause after a 429/503 with nothing else in flight and no Retry-After.
const THROTTLE_COOLDOWN: Duration = Duration::from_secs(1);
/// Received bytes collected before one disk write: few blocking writes, little memory per connection.
const WRITE_BATCH: usize = 512 * 1024;
/// Longest a received byte waits in memory, so progress and resume state keep up on slow links.
const WRITE_INTERVAL: Duration = Duration::from_millis(250);
/// Wait for a byte after which an idle worker takes over what is left of a chunk.
const TAKEOVER_SILENCE: Duration = Duration::from_secs(2);
/// ...or this many of the mirror's answer times, if longer.
const TAKEOVER_TTFBS: u32 = 4;

/// Why an attempt failed, which decides how the engine retries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Timeout, reset, early EOF, 5xx: retry the chunk with backoff.
    Transient,
    /// 429/503, with the server's Retry-After if it sent one.
    Throttled(Option<Duration>),
    /// This mirror cannot serve the file (wrong Content-Range, ignored Range).
    BadMirror,
    /// The URL refused the request (401/403/404/410), or a redirect target answered with anything
    /// but the file, throttling or a server error: an expired redirect target, or a mirror that
    /// cannot serve the file.
    Denied,
    /// Retrying cannot help (disk write error, remote file changed): fail the download.
    Fatal,
}

#[derive(Debug)]
pub enum WorkerEvent {
    /// A mirror accepted a request; sent only for responses whose body will be used.
    Ttfb {
        worker_id: usize,
        mirror_id: usize,
        ttfb: Duration,
    },
    Progress {
        worker_id: usize,
        chunk_id: usize,
        mirror_id: usize,
        bytes_received: u64,
        duration: Duration,
    },
    ChunkCompleted {
        worker_id: usize,
        chunk_id: usize,
    },
    ChunkFailed {
        worker_id: usize,
        chunk_id: usize,
        mirror_id: usize,
        kind: FailureKind,
        error: String,
    },
}

type Failure = (FailureKind, String);

/// Received bytes not yet on disk; they belong at `pos..`.
struct Batch {
    pos: u64,
    buf: Vec<u8>,
}

impl Batch {
    /// Offset just past the last received byte.
    fn end(&self) -> u64 {
        self.pos + self.buf.len() as u64
    }
}

/// Download-wide bandwidth cap shared by every connection. Each caller reserves the next time
/// slot for its bytes (the GCRA form of a token bucket), so callers are served in order and
/// waiting is a single timer, not polling.
#[derive(Debug)]
pub struct RateLimiter {
    bytes_per_sec: f64,
    /// Idle credit a caller may use immediately.
    burst: Duration,
    next_free: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec: bytes_per_sec.max(1) as f64,
            burst: Duration::from_millis(100),
            next_free: Mutex::new(Instant::now()),
        }
    }

    /// Waits until `bytes` more may be consumed without exceeding the rate.
    pub async fn acquire(&self, bytes: u64) {
        let ready_at = {
            let now = Instant::now();
            let mut next = self.next_free.lock();
            let floor = now.checked_sub(self.burst).unwrap_or(now);
            *next = (*next).max(floor) + Duration::from_secs_f64(bytes as f64 / self.bytes_per_sec);
            *next
        };
        tokio::time::sleep_until(ready_at.into()).await;
    }
}

/// The user's `Authorization` or `Referer` headers, scoped to the hosts the user named: resolved, scraped and
/// third-party URLs never receive it, and neither does plain HTTP to a host given as HTTPS.
#[derive(Debug, Clone)]
pub struct Auth {
    value: Option<HeaderValue>,
    referer: Option<String>,
    /// A paste's headers (see `DownloadOptions::secret_headers`), scoped as `value` is.
    headers: HeaderMap,
    user_urls: Vec<Url>,
}

impl Auth {
    pub(crate) fn new(value: &str, user_urls: &[Url]) -> Result<Self, String> {
        let mut value = HeaderValue::from_str(value).map_err(|_| "Invalid Authorization header value".to_string())?;
        value.set_sensitive(true);
        Ok(Self { value: Some(value), referer: None, headers: HeaderMap::new(), user_urls: user_urls.to_vec() })
    }

    pub(crate) fn with_referer(mut self, referer: Option<String>) -> Self {
        self.referer = referer;
        self
    }

    pub(crate) fn only_referer(referer: Option<&str>, user_urls: &[Url]) -> Self {
        Self { value: None, referer: referer.map(str::to_string), headers: HeaderMap::new(), user_urls: user_urls.to_vec() }
    }

    /// With `headers` too; a pair that is no valid header is left out.
    pub(crate) fn with_headers(mut self, headers: &[(String, String)]) -> Self {
        for (name, mut value) in headers.iter().filter_map(|(name, value)| crate::engine::request_header(name, value)) {
            value.set_sensitive(true);
            self.headers.insert(name, value);
        }
        self
    }

    pub(crate) fn allows(&self, url: &Url) -> bool {
        url.host_str().is_some()
            && self.user_urls.iter().any(|u| {
                u.host_str() == url.host_str() && (u.scheme() != "https" || url.scheme() == "https")
            })
    }
}

/// Adds the user's `Authorization` header and a paste's headers (replacing those of the same
/// name) to a request for `url` when `auth` covers that URL, the User-Agent `url`'s host takes
/// (see [`crate::resolver::with_agent_for`]), and the appropriate `Referer` header (see
/// [`crate::resolver::with_referer_for`]).
pub(crate) fn authorize(request: RequestBuilder, auth: Option<&Auth>, url: &Url) -> RequestBuilder {
    let request = crate::resolver::with_agent_for(request, url);
    let explicit_referer = auth.and_then(|a| a.referer.as_deref());
    let request = crate::resolver::with_referer_for(request, url, explicit_referer);
    let Some(auth) = auth.filter(|a| a.allows(url)) else { return request };
    let request = request.headers(auth.headers.clone());
    match &auth.value {
        Some(v) => request.header(AUTHORIZATION, v.clone()),
        None => request,
    }
}

/// Sends the request `build` makes for `url`. A client carrying a paste's headers stops at a
/// redirect to another host (see `engine::build_route_client`), as the client would send them
/// along; it is followed here instead, in a request `build` makes for the new link, which
/// [`authorize`] gives them only if it covers that link. Other clients follow redirects
/// themselves, so their answers come back as they are.
pub(crate) async fn send(build: impl Fn(&Url) -> RequestBuilder, url: &Url) -> reqwest::Result<Response> {
    let mut response = build(url).send().await?;
    for _ in 0..10 {
        let moved = matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308);
        let location = response.headers().get(LOCATION).and_then(|l| l.to_str().ok());
        match location.filter(|_| moved).and_then(|l| response.url().join(l).ok()) {
            Some(next) => response = build(&next).send().await?,
            None => break,
        }
    }
    Ok(response)
}

/// State shared by all workers of one download.
#[derive(Clone)]
pub struct WorkerShared {
    pub client: Client,
    /// One per route (proxy and/or local address) the workers take turns on; empty for `client`.
    pub clients: Arc<Vec<Client>>,
    /// Routes of `clients` that could not connect, dropped for the rest of the download.
    pub dropped_routes: Arc<Mutex<Vec<usize>>>,
    /// The routes and the origins (`scheme://host:port`) each has reached in this download.
    pub reached: Arc<Mutex<Vec<(usize, String)>>>,
    pub auth: Option<Arc<Auth>>,
    pub writer: DiskWriter,
    pub chunks: Arc<Mutex<ChunkManager>>,
    pub mirrors: Arc<Mutex<MirrorRacer>>,
    pub events: mpsc::Sender<WorkerEvent>,
    /// Stops the workers: the user cancelled, or the engine is done with them.
    pub cancel: CancellationToken,
    pub limiter: Option<Arc<RateLimiter>>,
    pub file_size: u64,
    /// Fewest bytes a steal takes.
    pub min_steal: u64,
    /// Most connections this download opens to one host, together with every other download
    /// there (see [`crate::hosts`]); 0 for no limit of its own.
    pub host_limit: usize,
    /// Max wait for response headers.
    pub stall_timeout: Duration,
    /// Max wait between body reads once the answer has started. Shorter than `stall_timeout`:
    /// a retry keeps what arrived and starts at once, so a quiet connection is best replaced.
    pub body_idle: Duration,
}

impl WorkerShared {
    /// The worker's own route (workers take turns on them), or once that is dropped the next one
    /// still in use: an index into `clients`, whose request goes out on `client` when that is empty.
    pub fn route_for_worker(&self, worker_id: usize) -> usize {
        let routes = self.clients.len().max(1);
        let dropped = self.dropped_routes.lock();
        (0..routes).map(|i| (worker_id + i) % routes).find(|r| !dropped.contains(r)).unwrap_or(0)
    }

    /// Notes that `route` reached the server of `url` (it answered).
    fn reached(&self, route: usize, url: &Url) {
        let origin = url.origin().ascii_serialization();
        let mut reached = self.reached.lock();
        if !reached.iter().any(|(r, o)| *r == route && *o == origin) {
            reached.push((route, origin));
        }
    }

    /// Stops using `route`, whose connection to the server of `url` could not be made (an
    /// unplugged network, a dead proxy), for the rest of the download; never the last route
    /// left. Only when another route has reached that server: else the server is what fails.
    fn drop_route(&self, route: usize, url: &Url) {
        let origin = url.origin().ascii_serialization();
        if !self.reached.lock().iter().any(|(r, o)| *r != route && *o == origin) {
            return;
        }
        let mut dropped = self.dropped_routes.lock();
        if self.clients.len() > dropped.len() + 1 && !dropped.contains(&route) {
            dropped.push(route);
            tracing::warn!("Route {} cannot connect; dropping it, {} left", route, self.clients.len() - dropped.len());
        }
    }
}

/// The body of an answer, as it comes.
pub type Body = BoxStream<'static, reqwest::Result<Bytes>>;

/// An answer already bringing a chunk's bytes, from the chunk's start: the probe's, which its
/// worker streams as its first attempt instead of asking for those bytes again.
pub struct Seed {
    /// The chunk, assigned to the worker.
    pub chunk: Chunk,
    pub mirror_id: usize,
    /// Where the answer came from.
    pub url: Url,
    pub body: Body,
    /// The host slot the answer's request holds.
    pub slot: HostSlot,
}

pub struct HttpWorker {
    pub worker_id: usize,
    shared: WorkerShared,
}

impl HttpWorker {
    pub fn new(worker_id: usize, shared: WorkerShared) -> Self {
        Self { worker_id, shared }
    }

    /// Takes chunks (or takes over or steals from slow ones) until cancelled or the engine goes away.
    /// Each request holds a slot of its host's connection budget, taken before any chunk: waiting
    /// for one is neither a stall nor a failure, and leaves nothing for others to steal. A `seed`,
    /// whose chunk must be assigned to this worker, comes first, like any other attempt but for
    /// the request.
    pub async fn run(self, seed: Option<Seed>) {
        if let Some(Seed { chunk, mirror_id, url, body, slot }) = seed {
            self.shared.mirrors.lock().acquire_mirror(mirror_id);
            let start = chunk.current_offset.load(Ordering::SeqCst);
            let outcome = self.receive(&chunk, mirror_id, body, start).await;
            if !self.conclude(&chunk, mirror_id, &url, outcome, slot).await {
                return;
            }
        }
        let s = &self.shared;
        loop {
            if s.cancel.is_cancelled() {
                return;
            }
            let slot = tokio::select! {
                biased;
                _ = s.cancel.cancelled() => return,
                slot = self.take_slot() => slot,
            };
            let job = slot.and_then(|(mirror_id, slot)| Some((self.next_job(mirror_id, &slot)?, mirror_id, slot)));
            let Some(((chunk, url, if_range, redirected), mirror_id, slot)) = job else {
                tokio::select! {
                    _ = s.cancel.cancelled() => return,
                    _ = tokio::time::sleep(IDLE_POLL) => continue,
                }
            };

            let outcome = self.download_chunk(&chunk, mirror_id, &url, if_range, redirected, &slot).await;
            if !self.conclude(&chunk, mirror_id, &url, outcome, slot).await {
                return;
            }
        }
    }

    /// Settles an attempt at `chunk` (a request to `url` under `slot`) and reports it; `false`
    /// once the worker is to stop.
    async fn conclude(&self, chunk: &Chunk, mirror_id: usize, url: &Url, outcome: Result<(), Failure>, slot: HostSlot) -> bool {
        let s = &self.shared;
        if s.cancel.is_cancelled() {
            s.mirrors.lock().release_mirror(mirror_id);
            return false;
        }
        // Settled before looking for more work, so nobody (including this worker) mistakes the
        // chunk for a live one to steal from. The event wakes the engine so it notices completion
        // or a fatal error.
        let event = self.settle(chunk, mirror_id, url, outcome, &slot);
        drop(slot);
        match event {
            Some(event) => s.events.send(event).await.is_ok(),
            None => true,
        }
    }

    /// A slot on the host of a mirror that can take another connection: the best such mirror
    /// whose host has one free, else the best one's, once it has. `None` while no mirror can.
    async fn take_slot(&self) -> Option<(usize, HostSlot)> {
        let s = &self.shared;
        let route_id = s.route_for_worker(self.worker_id);
        let ranked: Vec<(usize, Url)> = {
            let racer = s.mirrors.lock();
            racer.ranked().into_iter().filter_map(|id| Some((id, racer.get_mirror(id)?.url.clone()))).collect()
        };
        for (mirror_id, url) in &ranked {
            if let Some(slot) = hosts::try_acquire_routed(url, s.host_limit, route_id) {
                return Some((*mirror_id, slot));
            }
        }
        let (mirror_id, url) = ranked.into_iter().next()?;
        Some((mirror_id, hosts::acquire_routed(&url, s.host_limit, route_id).await))
    }

    /// A chunk for the mirror `slot` was taken for, if it can still take a connection there:
    /// fresh work, else what is left of a chunk gone silent, else part of a slow chunk, timed by
    /// what a new request to each mirror costs. Also where to send the request, with its If-Range
    /// validator and whether that URL is the mirror's redirect target. Lock order: mirrors, then
    /// chunks.
    fn next_job(&self, mirror_id: usize, slot: &HostSlot) -> Option<(Chunk, Url, Option<String>, bool)> {
        let s = &self.shared;
        let route_id = s.route_for_worker(self.worker_id);
        let mut racer = s.mirrors.lock();
        // Meanwhile the mirror may have filled up, cooled down or gone back to its own URL.
        let mirror = racer
            .get_mirror(mirror_id)
            .filter(|m| m.score(Instant::now()) >= 0.0 && HostKey::routed(&m.url, route_id) == *slot.host())?;
        let (url, if_range, redirected) = (mirror.url.clone(), mirror.if_range.clone(), mirror.fallback.is_some());
        let chunk = {
            let mut mgr = s.chunks.lock();
            mgr.get_next_work(self.worker_id, mirror_id)
                .or_else(|| {
                    let taken = mgr.take_over_silent(self.worker_id, mirror_id, |m| takeover_after(racer.get_mirror(m)));
                    if let Some(chunk) = &taken {
                        tracing::info!("Chunk {} went silent; worker {} takes over the rest", chunk.id, self.worker_id);
                    }
                    taken
                })
                .or_else(|| {
                    let costs: Vec<StealTiming> =
                        racer.mirrors().iter().map(|m| StealTiming { startup: m.ttfb(), rate: m.speed_ewma }).collect();
                    let steal = StealRule { min_bytes: s.min_steal, timing: Some(&costs) };
                    mgr.steal_work(self.worker_id, mirror_id, steal).map(|(_, c)| c)
                })?
        };
        racer.acquire_mirror(mirror_id);
        Some((chunk, url, if_range, redirected))
    }

    /// Applies an attempt's outcome to mirror, host and chunk bookkeeping and returns the event to
    /// send. A chunk another worker took over is no longer this worker's to complete or retry:
    /// only a disk error still counts, and the silence against the mirror.
    fn settle(
        &self,
        chunk: &Chunk,
        mirror_id: usize,
        url: &Url,
        outcome: Result<(), Failure>,
        slot: &HostSlot,
    ) -> Option<WorkerEvent> {
        let s = &self.shared;
        let mut racer = s.mirrors.lock();
        racer.release_mirror(mirror_id);
        let mut chunks = s.chunks.lock();
        let (worker_id, chunk_id) = (self.worker_id, chunk.id);
        let revoked = chunk.revoked.is_cancelled();
        match outcome {
            Ok(()) if revoked => None,
            Ok(()) => {
                if let Err(e) = chunks.mark_completed(chunk_id) {
                    chunks.abort(chunk_id, &e.to_string());
                }
                Some(WorkerEvent::ChunkCompleted { worker_id, chunk_id })
            }
            Err((kind, error)) => {
                if !revoked {
                    if matches!(kind, FailureKind::Throttled(_)) {
                        slot.throttled();
                    }
                    record_failure(&mut racer, &mut chunks, chunk_id, mirror_id, url, kind, &error);
                } else if kind == FailureKind::Fatal {
                    chunks.abort(chunk_id, &error);
                } else if let Some(mirror) = racer.get_mirror_mut(mirror_id) {
                    mirror.record_failure();
                }
                Some(WorkerEvent::ChunkFailed { worker_id, chunk_id, mirror_id, kind, error })
            }
        }
    }

    /// Streams the chunk's remaining range from `url` (the mirror's redirect target if
    /// `redirected`) into the file. Bytes are written in batches on the blocking pool, never on
    /// the async runtime. Only this worker advances `chunk.current_offset`, and only once a batch
    /// is on disk.
    async fn download_chunk(
        &self,
        chunk: &Chunk,
        mirror_id: usize,
        url: &Url,
        if_range: Option<String>,
        redirected: bool,
        slot: &HostSlot,
    ) -> Result<(), Failure> {
        let s = &self.shared;
        let start = chunk.current_offset.load(Ordering::SeqCst);
        let end = chunk.end_offset.load(Ordering::SeqCst);
        if start > end {
            return Ok(()); // stolen away entirely before we started
        }

        let route = s.route_for_worker(self.worker_id);
        let client = s.clients.get(route).unwrap_or(&s.client);
        let request = |url: &Url| {
            let mut request = authorize(client.get(url.clone()), s.auth.as_deref(), url)
                .header(RANGE, format!("bytes={}-{}", start, end))
                .header(ACCEPT_ENCODING, "identity");
            if crate::hosts::HostKey::of(url).is_bitrate_throttled() {
                request = request.header(reqwest::header::CONNECTION, "close");
            }
            if let Some(validator) = &if_range {
                request = request.header(IF_RANGE, validator.as_str());
            }
            request
        };

        let sent_at = Instant::now();
        let response = tokio::select! {
            biased;
            _ = s.cancel.cancelled() => return Err(cancelled()),
            _ = chunk.revoked.cancelled() => return Err(taken_over()),
            res = tokio::time::timeout(s.stall_timeout, send(request, url)) => match res {
                Err(_) => return Err((FailureKind::Transient, format!("no response within {}s", s.stall_timeout.as_secs()))),
                Ok(Err(e)) => {
                    // A name that did not resolve says nothing of the route.
                    if e.is_connect() && !format!("{e:?}").contains("dns error") {
                        s.drop_route(route, url);
                    }
                    let err_str = e.to_string();
                    if err_str.contains("rate limited") || err_str.contains("too many") {
                        return Err((FailureKind::Throttled(Some(std::time::Duration::from_secs(5))), format!("request throttled: {}", e)));
                    }
                    return Err((FailureKind::Transient, format!("request failed: {}", e)));
                }
                Ok(Ok(resp)) => {
                    s.reached(route, url);
                    resp
                }
            },
        };
        check_response(response.status(), response.headers(), start, end, s.file_size, if_range.as_deref())
            .map_err(|failure| if redirected { at_redirect_target(response.status(), failure) } else { failure })?;
        slot.accepted();
        let ttfb = sent_at.elapsed();
        // The host answering honours ranges (a 200 from the file's start says nothing) and, on a
        // new connection straight to it, takes this long to answer: the next download from it
        // starts from that.
        let accepts_ranges = (response.status() == StatusCode::PARTIAL_CONTENT).then_some(true);
        let setup_time = (response.url() == url && slot.opens_connection()).then_some(ttfb);
        hosts::record(response.url(), HostProfile { accepts_ranges, setup_time, ..Default::default() });
        let _ = s.events.try_send(WorkerEvent::Ttfb { worker_id: self.worker_id, mirror_id, ttfb });
        self.receive(chunk, mirror_id, response.bytes_stream(), start).await
    }

    /// Streams `body`, the chunk's bytes from `start`, into the file. The attempt ends once
    /// `body_idle` passes without a byte: what arrived is kept for the retry. It ends at once when
    /// another worker takes the chunk over; what arrived is then left to that worker.
    async fn receive(
        &self,
        chunk: &Chunk,
        mirror_id: usize,
        body: impl Stream<Item = reqwest::Result<Bytes>>,
        start: u64,
    ) -> Result<(), Failure> {
        let s = &self.shared;
        // The wait before is the answer's: the chunk's rate, and its silence, count from here.
        chunk.answered(Instant::now());
        let mut stream = std::pin::pin!(body);
        let mut batch = Batch { pos: start, buf: Vec::with_capacity(WRITE_BATCH) };
        // Due `WRITE_INTERVAL` after the oldest byte in `batch` arrived.
        let flush_due = tokio::time::sleep(WRITE_INTERVAL);
        tokio::pin!(flush_due);
        // Due `body_idle` after the last byte arrived (or waiting on the rate limit ended).
        let idle = tokio::time::sleep(s.body_idle);
        tokio::pin!(idle);
        let mut pending: u64 = 0;
        let mut last_report = Instant::now();
        let result = loop {
            // Only time spent waiting on the server counts as the chunk's silence, not writing or
            // rate limiting what arrived.
            chunk.wait_for_server(Instant::now());
            let item = tokio::select! {
                biased;
                _ = s.cancel.cancelled() => break Err(cancelled()),
                _ = chunk.revoked.cancelled() => break Err(taken_over()),
                // Also while the server pauses: received bytes must not wait for the next ones.
                _ = &mut flush_due, if !batch.buf.is_empty() => {
                    if let Err(e) = self.flush(chunk, &mut batch).await {
                        break Err(e);
                    }
                    // A thief may have left this chunk nothing beyond what just went to disk.
                    if batch.pos > chunk.end_offset.load(Ordering::SeqCst) {
                        break Ok(());
                    }
                    continue;
                }
                item = stream.next() => item,
                _ = &mut idle => break Err((
                    FailureKind::Transient,
                    format!("stalled: no data for {:?} at offset {}", s.body_idle, batch.end()),
                )),
            };
            let bytes = match item {
                None => break Ok(()),
                Some(Err(e)) => break Err((FailureKind::Transient, format!("read error at offset {}: {}", batch.end(), e))),
                Some(Ok(bytes)) => bytes,
            };
            chunk.busy();
            if let Some(limiter) = &s.limiter {
                tokio::select! {
                    biased;
                    _ = s.cancel.cancelled() => break Err(cancelled()),
                    _ = chunk.revoked.cancelled() => break Err(taken_over()),
                    _ = limiter.acquire(bytes.len() as u64) => {}
                }
            }
            idle.as_mut().reset(tokio::time::Instant::now() + s.body_idle);

            if batch.buf.len() + bytes.len() > WRITE_BATCH {
                if let Err(e) = self.flush(chunk, &mut batch).await {
                    break Err(e);
                }
            }
            // Re-read the end: a thief may have taken the tail of this chunk.
            let end = chunk.end_offset.load(Ordering::SeqCst);
            let received = batch.end();
            if received > end {
                break Ok(());
            }
            let take = (bytes.len() as u64).min(end - received + 1) as usize;
            if batch.buf.is_empty() {
                flush_due.as_mut().reset(tokio::time::Instant::now() + WRITE_INTERVAL);
            }
            batch.buf.extend_from_slice(&bytes[..take]);
            pending += take as u64;

            if pending >= 1024 * 1024 || last_report.elapsed() >= Duration::from_millis(100) {
                self.report_progress(chunk.id, mirror_id, &mut pending, &mut last_report);
            }
            if take < bytes.len() {
                break Ok(());
            }
        };
        // What arrived is kept even when the attempt failed or was cancelled: a retry resumes
        // after it. A disk error outranks the network outcome.
        let flushed = self.flush(chunk, &mut batch).await;
        self.report_progress(chunk.id, mirror_id, &mut pending, &mut last_report);
        flushed.and(result)?;

        let end = chunk.end_offset.load(Ordering::SeqCst);
        if batch.pos <= end {
            return Err((
                FailureKind::Transient,
                format!("connection closed at offset {}, expected data up to {}", batch.pos, end),
            ));
        }
        Ok(())
    }

    /// Writes the batch at its offset on the blocking pool, then advances the chunk's offset.
    /// Bytes past the chunk's current end belong to whoever stole that tail and are dropped, as
    /// is everything once the chunk has been taken over.
    async fn flush(&self, chunk: &Chunk, batch: &mut Batch) -> Result<(), Failure> {
        chunk.busy();
        if chunk.revoked.is_cancelled() {
            batch.buf.clear();
            return Ok(());
        }
        let end = chunk.end_offset.load(Ordering::SeqCst);
        let keep = (end + 1).saturating_sub(batch.pos).min(batch.buf.len() as u64) as usize;
        batch.buf.truncate(keep);
        if batch.buf.is_empty() {
            return Ok(());
        }
        let (writer, pos, data) = (self.shared.writer.clone(), batch.pos, std::mem::take(&mut batch.buf));
        let (mut data, written) = tokio::task::spawn_blocking(move || {
            let written = writer.write_chunk_slice(pos, &data);
            (data, written)
        })
        .await
        .map_err(|e| (FailureKind::Fatal, format!("Disk write task failed: {}", e)))?;
        written.map_err(|e| (FailureKind::Fatal, format!("Disk write error: {}", e)))?;
        batch.pos += data.len() as u64;
        chunk.current_offset.store(batch.pos, Ordering::SeqCst);
        data.clear();
        batch.buf = data;
        Ok(())
    }

    /// Mirror speed statistics only; dropped rather than stalling the data path when the engine is busy.
    fn report_progress(&self, chunk_id: usize, mirror_id: usize, pending: &mut u64, last_report: &mut Instant) {
        if *pending == 0 {
            return;
        }
        let _ = self.shared.events.try_send(WorkerEvent::Progress {
            worker_id: self.worker_id,
            chunk_id,
            mirror_id,
            bytes_received: *pending,
            duration: last_report.elapsed(),
        });
        *pending = 0;
        *last_report = Instant::now();
    }
}

/// Applies a failed attempt (a request to `url`) to mirror and chunk bookkeeping. This is the retry
/// policy: transient errors cost the chunk a retry (unless it made progress) and back off;
/// bad mirrors are dropped after repeated bad answers, though a redirect target that refuses
/// first sends the mirror back to its own URL; throttling while other connections are being
/// served lowers the connection cap instead of costing retries.
/// `mirror.in_flight` must already exclude the failed connection.
fn record_failure(
    racer: &mut MirrorRacer,
    chunks: &mut ChunkManager,
    chunk_id: usize,
    mirror_id: usize,
    url: &Url,
    kind: FailureKind,
    error: &str,
) {
    let Some(mirror) = racer.get_mirror_mut(mirror_id) else {
        chunks.abort(chunk_id, &format!("unknown mirror {}", mirror_id));
        return;
    };
    let (delay, counts) = match kind {
        FailureKind::Fatal => {
            chunks.abort(chunk_id, error);
            return;
        }
        FailureKind::Transient => {
            mirror.record_failure();
            (Duration::ZERO, true)
        }
        // Another connection already sent the mirror back to its own URL.
        FailureKind::Denied if *url != mirror.url => (Duration::ZERO, false),
        FailureKind::Denied if mirror.fallback.is_some() => {
            if let Some(own) = mirror.fallback.take() {
                tracing::info!("{} refused ({}); going back to {}", mirror.url, error, own);
                mirror.url = own;
            }
            (Duration::ZERO, false)
        }
        FailureKind::BadMirror | FailureKind::Denied => {
            if mirror.record_bad_response() {
                tracing::warn!("Disabling mirror {}: {}", mirror.url, error);
            }
            if racer.all_inactive() {
                chunks.abort(chunk_id, &format!("every mirror failed; last error: {}", error));
                return;
            }
            (Duration::ZERO, false)
        }
        FailureKind::Throttled(after) if mirror.in_flight > 0 => {
            // The server limits connections; the ones it accepts can still finish the job.
            let others = mirror.in_flight;
            mirror.record_throttled(others);
            (after.unwrap_or(THROTTLE_RETRY), false)
        }
        FailureKind::Throttled(after) => {
            // Refused with nothing else running: the server is busy. Pause it and count the
            // attempt, so a server that never recovers still ends the download.
            let pause = after.unwrap_or(THROTTLE_COOLDOWN);
            mirror.record_throttled(1);
            mirror.cool_down(Instant::now() + pause);
            (pause, true)
        }
    };
    if let Err(e) = chunks.mark_failed(chunk_id, error, delay, counts) {
        chunks.abort(chunk_id, &e.to_string());
    }
}

fn cancelled() -> Failure {
    (FailureKind::Transient, "cancelled".to_string())
}

fn taken_over() -> Failure {
    (FailureKind::Transient, "no data for too long; another connection took over".to_string())
}

/// How long a chunk's worker may wait for a byte from its mirror before an idle worker takes over
/// what is left: `TAKEOVER_SILENCE`, or `TAKEOVER_TTFBS` of the mirror's answer times if longer,
/// since a server slow to answer is not a dead one.
fn takeover_after(mirror: Option<&Mirror>) -> Duration {
    mirror.map_or(TAKEOVER_SILENCE, |m| TAKEOVER_SILENCE.max(m.ttfb().saturating_mul(TAKEOVER_TTFBS)))
}

/// Classifies the response to `Range: bytes=start-end` (with `If-Range: if_range` when set) for
/// a file of `file_size` bytes.
fn check_response(
    status: StatusCode,
    headers: &HeaderMap,
    start: u64,
    end: u64,
    file_size: u64,
    if_range: Option<&str>,
) -> Result<(), Failure> {
    match status {
        StatusCode::PARTIAL_CONTENT => {
            let header = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok());
            match header.map(ByteRange::parse_content_range) {
                // A server honouring If-Range answers for another version with a 200. One that
                // ignores it may send part of that version as a 206, which only its validator
                // tells apart.
                Some(Ok((range, Some(total)))) if range.start == start && total == file_size => {
                    match if_range.filter(|v| names_another_version(headers, v)) {
                        Some(_) => Err((
                            FailureKind::Fatal,
                            "remote file changed: server ignored If-Range and sent part of a different version".to_string(),
                        )),
                        None => Ok(()),
                    }
                }
                _ => Err((
                    FailureKind::BadMirror,
                    format!(
                        "Content-Range {:?} does not match the request for bytes {}-{} of {}",
                        header.unwrap_or("(missing)"),
                        start,
                        end,
                        file_size
                    ),
                )),
            }
        }
        // A full 200 is only usable from offset 0. Under If-Range it is the same file with the
        // Range ignored only if it carries the validator we sent; otherwise the file may have
        // changed, which matters unless we asked for the whole file anyway.
        StatusCode::OK => {
            let same_file = if_range.is_none_or(|v| carries_validator(headers, v));
            if start == 0 && (same_file || end + 1 == file_size) {
                match content_length(headers) {
                    Some(len) if len != file_size => Err((
                        FailureKind::BadMirror,
                        format!("200 response is {} bytes, expected {}", len, file_size),
                    )),
                    _ => Ok(()),
                }
            } else if same_file {
                Err((FailureKind::BadMirror, format!("server ignored Range and sent 200 for offset {}", start)))
            } else {
                Err((
                    FailureKind::Fatal,
                    "remote file changed: server ignored If-Range and sent a different version".to_string(),
                ))
            }
        }
        StatusCode::RANGE_NOT_SATISFIABLE => Err((
            FailureKind::Fatal,
            format!("remote file changed: 416 Range Not Satisfiable for bytes {}-{}", start, end),
        )),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE => {
            Err((FailureKind::Denied, format!("HTTP {}", status)))
        }
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
            Err((FailureKind::Throttled(retry_after(headers)), format!("HTTP {}", status)))
        }
        _ => Err((FailureKind::Transient, format!("HTTP {}", status))),
    }
}

/// Reclassifies `failure`, `check_response`'s verdict on a `status` answer from a mirror's
/// redirect target. A signed target that expired may answer with any client error (Google Cloud
/// Storage sends 400) or an error page that is not the file (a 200, maybe after a redirect of its
/// own), so all of these are Denied there: the mirror goes back to its own URL, whose fresh
/// redirect decides, before anything counts against the mirror or ends the download. Throttling
/// and server errors say nothing about the target's signature and keep their kind.
fn at_redirect_target(status: StatusCode, (kind, error): Failure) -> Failure {
    match kind {
        FailureKind::Throttled(_) => (kind, error),
        _ if status.is_server_error() => (kind, error),
        _ => (FailureKind::Denied, error),
    }
}

/// Whether a response carries the `If-Range` validator we sent: our strong ETag (always quoted),
/// or else our Last-Modified date.
pub(crate) fn carries_validator(headers: &HeaderMap, validator: &str) -> bool {
    let name = if validator.starts_with('"') { ETAG } else { LAST_MODIFIED };
    headers.get(name).and_then(|v| v.to_str().ok()).is_some_and(|v| v.trim() == validator)
}

/// Whether a response names a version other than the `If-Range` validator we sent: it carries
/// that validator's header (ETag, or Last-Modified for a date) with another value.
fn names_another_version(headers: &HeaderMap, validator: &str) -> bool {
    let name = if validator.starts_with('"') { ETAG } else { LAST_MODIFIED };
    headers.get(name).and_then(|v| v.to_str().ok()).is_some_and(|v| v.trim() != validator)
}

/// `Retry-After` in delta-seconds form, capped.
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let secs: u64 = headers.get(RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

pub(crate) fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers.get(CONTENT_LENGTH)?.to_str().ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    const V1: Option<&str> = Some("\"v1\"");

    fn check(status: u16, headers: &[(&'static str, &'static str)], start: u64, end: u64, size: u64, if_range: Option<&str>) -> Option<FailureKind> {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(*k, HeaderValue::from_static(v));
        }
        check_response(StatusCode::from_u16(status).unwrap(), &map, start, end, size, if_range)
            .err()
            .map(|(k, _)| k)
    }

    #[test]
    fn test_check_response_classification() {
        let cr = [("content-range", "bytes 100-199/1000")];
        assert_eq!(check(206, &cr, 100, 199, 1000, V1), None);
        // Wrong start, or a total from a different file.
        assert_eq!(check(206, &cr, 50, 199, 1000, V1), Some(FailureKind::BadMirror));
        assert_eq!(check(206, &[("content-range", "bytes 100-199/2000")], 100, 199, 1000, None), Some(FailureKind::BadMirror));
        assert_eq!(check(206, &[], 0, 9, 10, None), Some(FailureKind::BadMirror));

        let full = [("content-length", "1000")];
        assert_eq!(check(200, &full, 0, 499, 1000, None), None);
        assert_eq!(check(200, &full, 0, 999, 1000, V1), None);
        assert_eq!(check(200, &full, 0, 499, 1000, V1), Some(FailureKind::Fatal));
        assert_eq!(check(200, &full, 500, 999, 1000, V1), Some(FailureKind::Fatal));
        assert_eq!(check(200, &full, 500, 999, 1000, None), Some(FailureKind::BadMirror));
        assert_eq!(check(200, &[("content-length", "512")], 0, 999, 1000, None), Some(FailureKind::BadMirror));

        assert_eq!(check(416, &[], 0, 9, 10, V1), Some(FailureKind::Fatal));
        for status in [401, 403, 404, 410] {
            assert_eq!(check(status, &[], 0, 9, 10, None), Some(FailureKind::Denied));
        }
        assert_eq!(
            check(503, &[("retry-after", "3")], 0, 9, 10, None),
            Some(FailureKind::Throttled(Some(Duration::from_secs(3))))
        );
        assert_eq!(check(429, &[], 0, 9, 10, None), Some(FailureKind::Throttled(None)));
        assert_eq!(check(502, &[], 0, 9, 10, None), Some(FailureKind::Transient));
    }

    #[test]
    fn test_a_redirect_target_answering_anything_but_the_file_sends_the_mirror_home() {
        // The answer to bytes 500-999 of a 1000-byte file, asked of a redirect target with If-Range.
        let at_target = |status: u16, headers: &[(&'static str, &'static str)]| {
            let mut map = HeaderMap::new();
            for (k, v) in headers {
                map.insert(*k, HeaderValue::from_static(v));
            }
            let status = StatusCode::from_u16(status).unwrap();
            check_response(status, &map, 500, 999, 1000, V1).err().map(|failure| at_redirect_target(status, failure).0)
        };
        assert_eq!(at_target(206, &[("content-range", "bytes 500-999/1000")]), None);
        // An expired signature: a client error of any kind, an error page, a wrong range.
        for status in [400, 401, 403, 404, 410, 416] {
            assert_eq!(at_target(status, &[]), Some(FailureKind::Denied), "HTTP {status}");
        }
        assert_eq!(at_target(200, &[("content-length", "41"), ("content-type", "text/html")]), Some(FailureKind::Denied));
        assert_eq!(at_target(206, &[("content-range", "bytes 500-999/77")]), Some(FailureKind::Denied));
        // Neither throttling nor a server error says the signature expired.
        assert_eq!(at_target(429, &[]), Some(FailureKind::Throttled(None)));
        assert_eq!(at_target(503, &[]), Some(FailureKind::Throttled(None)));
        assert_eq!(at_target(502, &[]), Some(FailureKind::Transient));
    }

    #[test]
    fn test_200_to_if_range_with_our_validator_is_an_ignored_range_not_a_new_file() {
        let same = [("content-length", "1000"), ("etag", "\"v1\"")];
        let other = [("content-length", "1000"), ("etag", "\"v2\"")];
        // The server ignored Range once; the file is unchanged, so the chunk is just retried.
        assert_eq!(check(200, &same, 500, 999, 1000, V1), Some(FailureKind::BadMirror));
        // From offset 0 the body is usable as is.
        assert_eq!(check(200, &same, 0, 499, 1000, V1), None);
        assert_eq!(check(200, &other, 500, 999, 1000, V1), Some(FailureKind::Fatal));
        assert_eq!(check(200, &other, 0, 499, 1000, V1), Some(FailureKind::Fatal));

        let date = "Sun, 06 Nov 1994 08:49:37 GMT";
        let dated = [("content-length", "1000"), ("last-modified", "Sun, 06 Nov 1994 08:49:37 GMT")];
        assert_eq!(check(200, &dated, 500, 999, 1000, Some(date)), Some(FailureKind::BadMirror));
        let newer = [("content-length", "1000"), ("last-modified", "Mon, 07 Nov 1994 08:49:37 GMT")];
        assert_eq!(check(200, &newer, 500, 999, 1000, Some(date)), Some(FailureKind::Fatal));
    }

    #[test]
    fn test_206_naming_another_version_than_if_range_is_a_changed_file() {
        let range = ("content-range", "bytes 500-999/1000");
        assert_eq!(check(206, &[range, ("etag", "\"v1\"")], 500, 999, 1000, V1), None);
        // A server that ignores If-Range, serving part of a new build.
        assert_eq!(check(206, &[range, ("etag", "\"v2\"")], 500, 999, 1000, V1), Some(FailureKind::Fatal));
        // Nothing to go by: no validator in the answer, or none sent.
        assert_eq!(check(206, &[range], 500, 999, 1000, V1), None);
        assert_eq!(check(206, &[range, ("etag", "\"v2\"")], 500, 999, 1000, None), None);

        let date = "Sun, 06 Nov 1994 08:49:37 GMT";
        let dated = ("last-modified", "Sun, 06 Nov 1994 08:49:37 GMT");
        let newer = ("last-modified", "Mon, 07 Nov 1994 08:49:37 GMT");
        assert_eq!(check(206, &[range, dated, ("etag", "\"any\"")], 500, 999, 1000, Some(date)), None);
        assert_eq!(check(206, &[range, newer], 500, 999, 1000, Some(date)), Some(FailureKind::Fatal));
    }

    #[test]
    fn test_auth_is_scoped_to_user_hosts() {
        let user = [Url::parse("https://files.example.com/a.bin").unwrap(), Url::parse("http://plain.example/b").unwrap()];
        let auth = Auth::new("Bearer secret", &user).unwrap();
        let allows = |u: &str| auth.allows(&Url::parse(u).unwrap());
        assert!(allows("https://files.example.com/other/path?x=1"));
        assert!(allows("https://FILES.example.com:443/a.bin"));
        assert!(allows("http://plain.example/c"));
        assert!(allows("https://plain.example/c"), "an upgrade to HTTPS is fine");
        assert!(!allows("http://files.example.com/a.bin"), "never downgrade to cleartext");
        assert!(!allows("https://cdn.example.com/a.bin"));
        assert!(!allows("https://evil.example/files.example.com"));
        assert!(Auth::new("bad\nvalue", &user).is_err());
    }

    /// A worker for a one-chunk download of `size` bytes from `url` into `path`.
    fn test_worker(url: &Url, path: &std::path::Path, size: u64) -> (HttpWorker, Chunk, mpsc::Receiver<WorkerEvent>) {
        let mut chunks = ChunkManager::new(size, size).unwrap();
        let chunk = chunks.get_next_work(0, 0).unwrap();
        let (events, rx) = mpsc::channel(64);
        let shared = WorkerShared {
            client: Client::new(),
            clients: Arc::new(Vec::new()),
            dropped_routes: Default::default(),
            reached: Default::default(),
            auth: None,
            writer: DiskWriter::open_or_create(path, size).unwrap(),
            chunks: Arc::new(Mutex::new(chunks)),
            mirrors: Arc::new(Mutex::new(MirrorRacer::new(vec![url.clone()]))),
            events,
            cancel: CancellationToken::new(),
            limiter: None,
            file_size: size,
            min_steal: size,
            host_limit: 0,
            stall_timeout: Duration::from_secs(10),
            body_idle: Duration::from_secs(5),
        };
        (HttpWorker::new(0, shared), chunk, rx)
    }

    /// A connection slot on `url`'s host, from no budget.
    fn slot(url: &Url) -> HostSlot {
        hosts::try_acquire(url, 0).unwrap()
    }

    /// A body that sends each `(ms, len)` step `ms` after the one before, then goes silent.
    fn silent_after(steps: Vec<(u64, usize)>) -> Body {
        let steps = futures_util::stream::iter(steps).then(|(ms, len)| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(Bytes::from(vec![9u8; len]))
        });
        steps.chain(futures_util::stream::pending()).boxed()
    }

    #[tokio::test]
    async fn test_a_seeded_worker_streams_the_answer_it_was_given() {
        const SIZE: usize = 256 * 1024;
        let data: Vec<u8> = (0..SIZE).map(|i| (i % 253) as u8).collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.part");
        // Nothing answers there: the bytes can only come from the answer the worker was given.
        let url = Url::parse("http://seeded.worker.invalid/f").unwrap();
        let (worker, chunk, mut events) = test_worker(&url, &path, SIZE as u64);
        let shared = worker.shared.clone();
        let response = reqwest::Response::from(http::Response::new(reqwest::Body::from(data.clone())));
        let slot = hosts::try_acquire(&url, 1).unwrap();
        assert!(hosts::try_acquire(&url, 1).is_none(), "the answer's request holds the host's only slot");
        let seed = Seed { chunk, mirror_id: 0, url: url.clone(), body: response.bytes_stream().boxed(), slot };
        let task = tokio::spawn(HttpWorker::new(0, shared.clone()).run(Some(seed)));

        let completed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Some(WorkerEvent::ChunkCompleted { chunk_id, .. }) => break chunk_id,
                    Some(_) => continue,
                    None => panic!("the worker stopped"),
                }
            }
        });
        assert_eq!(completed.await.expect("the chunk completes"), 0);
        assert!(shared.chunks.lock().is_all_completed());
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert!(hosts::try_acquire(&url, 1).is_some(), "the slot went back once the attempt was settled");
        assert_eq!(shared.mirrors.lock().in_flight(), 0);
        shared.cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_body_gone_quiet_ends_the_attempt_after_the_idle_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let url = Url::parse("http://127.0.0.1:9/f").unwrap();
        for (stall, idle) in [(30, 5), (3, 3)] {
            let (mut worker, chunk, _events) = test_worker(&url, &dir.path().join(format!("{stall}.part")), 1 << 20);
            worker.shared.stall_timeout = Duration::from_secs(stall);
            worker.shared.body_idle = Duration::from_secs(stall).min(Duration::from_secs(5));
            let started = tokio::time::Instant::now();
            // A pause just under the idle timeout is fine; one as long ends the attempt.
            let body = silent_after(vec![(0, 64 * 1024), (idle * 1000 - 100, 64 * 1024)]);
            let err = worker.receive(&chunk, 0, body, 0).await.unwrap_err();

            assert_eq!(err.0, FailureKind::Transient, "{}", err.1);
            assert!(err.1.contains(&format!("no data for {idle}s")), "{}", err.1);
            let waited = started.elapsed();
            let expected = Duration::from_millis(idle * 2000 - 100);
            assert!(waited >= expected && waited < expected + Duration::from_millis(50), "{waited:?}");
            assert_eq!(chunk.current_offset.load(Ordering::SeqCst), 128 * 1024, "what arrived is kept");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_an_attempt_whose_tail_was_stolen_ends_once_its_part_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let url = Url::parse("http://127.0.0.1:9/f").unwrap();
        let (worker, chunk, _events) = test_worker(&url, &dir.path().join("f.part"), 1 << 20);
        let (end, offset) = (Arc::clone(&chunk.end_offset), Arc::clone(&chunk.current_offset));
        let started = tokio::time::Instant::now();
        let task = tokio::spawn(async move { worker.receive(&chunk, 0, silent_after(vec![(0, 64 * 1024)]), 0).await });

        // 64 KiB arrived, then the server went quiet and a thief took everything after it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        end.store(64 * 1024 - 1, Ordering::SeqCst);
        task.await.unwrap().unwrap();
        assert!(started.elapsed() <= WRITE_INTERVAL, "done once written, not after the idle timeout: {:?}", started.elapsed());
        assert_eq!(offset.load(Ordering::SeqCst), 64 * 1024);
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_taken_over_attempt_stops_at_once_and_writes_nothing_more() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.part");
        let url = Url::parse("http://127.0.0.1:9/f").unwrap();
        let (worker, chunk, _events) = test_worker(&url, &path, 1 << 20);
        let (revoked, offset) = (chunk.revoked.clone(), Arc::clone(&chunk.current_offset));
        let started = tokio::time::Instant::now();
        let task = tokio::spawn(async move { worker.receive(&chunk, 0, silent_after(vec![(0, 64 * 1024)]), 0).await });

        // 64 KiB arrived and waits to be written when another worker takes the chunk over.
        tokio::time::sleep(Duration::from_millis(100)).await;
        revoked.cancel();
        let err = task.await.unwrap().unwrap_err();
        assert!(err.1.contains("took over"), "{}", err.1);
        assert!(started.elapsed() < WRITE_INTERVAL, "{:?}", started.elapsed());
        assert_eq!(offset.load(Ordering::SeqCst), 0, "the new owner fetches those bytes");
        assert!(std::fs::read(&path).unwrap().iter().all(|&b| b == 0));
    }

    #[test]
    fn test_a_taken_over_chunk_is_left_to_its_new_owner() {
        use crate::chunk::ChunkStatus;
        let dir = tempfile::tempdir().unwrap();
        let url = Url::parse("http://127.0.0.1:9/f").unwrap();
        let (worker, old, _events) = test_worker(&url, &dir.path().join("f.part"), 1 << 20);
        // Its answer came, then nothing more.
        old.wait_for_server(Instant::now());
        let taken = {
            let mut chunks = worker.shared.chunks.lock();
            chunks.backdate(0, Duration::from_secs(3));
            chunks.take_over_silent(1, 0, |_| Duration::from_secs(2)).unwrap()
        };
        let thiefs = || worker.shared.chunks.lock().chunks()[0].status == ChunkStatus::Assigned { worker_id: 1, mirror_id: 0 };
        let held = slot(&url);

        // However the silent worker's attempt ends, the chunk stays the thief's.
        assert!(worker.settle(&old, 0, &url, Ok(()), &held).is_none());
        assert!(thiefs());
        assert!(matches!(worker.settle(&old, 0, &url, Err(taken_over()), &held), Some(WorkerEvent::ChunkFailed { .. })));
        assert!(thiefs(), "nor is it queued for a retry");
        assert_eq!(worker.shared.mirrors.lock().get_mirror(0).unwrap().failures, 1, "the silence counts against the mirror");

        let thief = HttpWorker::new(1, worker.shared.clone());
        taken.current_offset.store(1 << 20, Ordering::SeqCst);
        assert!(matches!(thief.settle(&taken, 0, &url, Ok(()), &held), Some(WorkerEvent::ChunkCompleted { .. })));
        assert!(worker.shared.chunks.lock().is_all_completed());
        // A disk error in the silent attempt still ends the download.
        worker.settle(&old, 0, &url, Err((FailureKind::Fatal, "Disk write error: no space".into())), &held);
        assert!(worker.shared.chunks.lock().has_fatal_failure().is_some());
    }

    #[test]
    fn test_a_refusal_holds_every_download_to_the_connections_the_host_served() {
        let dir = tempfile::tempdir().unwrap();
        let url = Url::parse("http://refusing.worker.example/f").unwrap();
        let (worker, chunk, _events) = test_worker(&url, &dir.path().join("f.part"), 1 << 20);
        let mut open: Vec<HostSlot> = (0..4).map(|_| slot(&url)).collect();
        let refused = FailureKind::Throttled(None);
        worker.settle(&chunk, 0, &url, Err((refused, "HTTP 503".into())), &open[3]);
        open.pop();

        assert_eq!(hosts::profile(&url).connection_cap, Some(3));
        assert!(hosts::try_acquire(&url, 0).is_none(), "three are open");
        open.pop();
        assert!(hosts::try_acquire(&url, 0).is_some());
    }

    #[test]
    fn test_a_slot_is_only_good_for_its_own_host() {
        let dir = tempfile::tempdir().unwrap();
        let url = Url::parse("http://127.0.0.1:9/f").unwrap();
        let (worker, ..) = test_worker(&url, &dir.path().join("f.part"), 1 << 20);
        *worker.shared.chunks.lock() = ChunkManager::new(1 << 20, 1 << 19).unwrap();
        // Say the mirror went back to its own URL on another host after the slot was taken.
        let elsewhere = Url::parse("http://127.0.0.1:10/f").unwrap();
        assert!(worker.next_job(0, &slot(&elsewhere)).is_none());
        assert!(worker.next_job(0, &slot(&url)).is_some());
    }

    #[test]
    fn test_steals_are_timed_by_what_a_request_to_the_thiefs_mirror_costs() {
        const MB: u64 = 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let url = Url::parse("http://127.0.0.1:9/f").unwrap();
        let (mut worker, chunk, _events) = test_worker(&url, &dir.path().join("f.part"), 100 * MB);
        worker.shared.min_steal = 64 * 1024;
        // Worker 0 wrote 10 MB in the 10 s since its answer: 90 MB left at 1 MB/s.
        chunk.answered(Instant::now());
        chunk.current_offset.store(10 * MB, Ordering::SeqCst);
        worker.shared.chunks.lock().backdate(0, Duration::from_secs(10));
        {
            let mut racer = worker.shared.mirrors.lock();
            let mirror = racer.get_mirror_mut(0).unwrap();
            (mirror.ttfb_ewma_ms, mirror.speed_ewma) = (4000.0, 3.0 * MB as f64);
        }
        let thief = HttpWorker::new(1, worker.shared.clone());
        let (stolen, ..) = thief.next_job(0, &slot(&url)).unwrap();
        // The mirror's 4 s answers and 3 MB/s connections: both sides finish after 25.5 s.
        let split = 10 * MB + (25.5 * MB as f64) as u64;
        assert!(stolen.range.start.abs_diff(split) < MB, "split at {} instead of about {split}", stolen.range.start);
    }

    #[tokio::test]
    async fn test_a_slow_answer_gets_the_full_stall_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const SIZE: usize = 64 * 1024;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/f", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = [0u8; 4096];
            let _ = socket.read(&mut head).await.unwrap();
            // Headers come well after the body idle timeout, within the stall timeout.
            tokio::time::sleep(Duration::from_millis(600)).await;
            let header = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\n\r\n",
                SIZE - 1, SIZE, SIZE
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(&[5u8; SIZE]).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let (mut worker, chunk, _events) = test_worker(&url, &dir.path().join("f.part"), SIZE as u64);
        worker.shared.stall_timeout = Duration::from_secs(10);
        worker.shared.body_idle = Duration::from_millis(100);
        worker.download_chunk(&chunk, 0, &url, None, false, &slot(&url)).await.unwrap();
        assert_eq!(chunk.current_offset.load(Ordering::SeqCst), SIZE as u64);
    }

    #[tokio::test]
    async fn test_an_answered_range_tells_what_its_host_does() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const SIZE: usize = 16 * 1024;
        let listener = hosts::unseen_listener().await;
        let url = Url::parse(&format!("http://{}/f", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut head = [0u8; 4096];
                let _ = socket.read(&mut head).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    SIZE - 1, SIZE, SIZE
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&[5u8; SIZE]).await;
            }
        });
        let dir = tempfile::tempdir().unwrap();

        // Nothing went to the host before: the connection is new, and its wait what one needs.
        let (worker, chunk, _events) = test_worker(&url, &dir.path().join("a.part"), SIZE as u64);
        worker.download_chunk(&chunk, 0, &url, None, false, &slot(&url)).await.unwrap();
        let seen = hosts::profile(&url);
        assert_eq!(seen.accepts_ranges, Some(true));
        assert!(seen.setup_time.is_some_and(|t| t >= Duration::from_millis(50)), "{seen:?}");
        // Right after a request to the host ended, the next may reuse its connection.
        let cold = Duration::from_secs(7);
        hosts::record(&url, HostProfile { setup_time: Some(cold), ..Default::default() });
        let (worker, chunk, _events) = test_worker(&url, &dir.path().join("b.part"), SIZE as u64);
        worker.download_chunk(&chunk, 0, &url, None, false, &slot(&url)).await.unwrap();
        assert_eq!(hosts::profile(&url).setup_time, Some(cold));
    }

    #[tokio::test]
    async fn test_received_bytes_reach_disk_while_the_server_pauses() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const SIZE: usize = 256 * 1024;
        let data: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/f", listener.local_addr().unwrap())).unwrap();
        let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let body = data.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = [0u8; 4096];
            let _ = socket.read(&mut head).await.unwrap();
            let header = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\n\r\n",
                SIZE - 1, SIZE, SIZE
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(&body[..64 * 1024]).await.unwrap();
            let _ = sent_tx.send(());
            let _ = go_rx.await;
            socket.write_all(&body[64 * 1024..]).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.part");
        let (worker, chunk, _events) = test_worker(&url, &path, SIZE as u64);
        let offset = Arc::clone(&chunk.current_offset);
        let task = tokio::spawn(async move { worker.download_chunk(&chunk, 0, &url, None, false, &slot(&url)).await });

        sent_rx.await.unwrap();
        // 64 KiB arrived, far less than a batch, and the server went quiet: within about
        // WRITE_INTERVAL those bytes are on disk, and only then counted as written.
        let deadline = Instant::now() + Duration::from_secs(5);
        while offset.load(Ordering::SeqCst) < 64 * 1024 {
            assert!(Instant::now() < deadline, "received bytes still only in memory");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let written = offset.load(Ordering::SeqCst) as usize;
        assert_eq!(written, 64 * 1024);
        assert_eq!(std::fs::read(&path).unwrap()[..written], data[..written]);

        go_tx.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(offset.load(Ordering::SeqCst), SIZE as u64);
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }

    #[tokio::test]
    async fn test_failed_attempt_keeps_the_bytes_it_received() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const SIZE: usize = 256 * 1024;
        let data: Vec<u8> = (0..SIZE).map(|i| (i % 13) as u8).collect();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/f", listener.local_addr().unwrap())).unwrap();
        let body = data.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = [0u8; 4096];
            let _ = socket.read(&mut head).await.unwrap();
            let header = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\n\r\n",
                SIZE - 1, SIZE, SIZE
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(&body[..100_000]).await.unwrap();
            // Connection drops mid-body.
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.part");
        let (worker, chunk, _events) = test_worker(&url, &path, SIZE as u64);
        let offset = Arc::clone(&chunk.current_offset);
        let err = worker.download_chunk(&chunk, 0, &url, None, false, &slot(&url)).await.unwrap_err();

        assert_eq!(err.0, FailureKind::Transient, "{}", err.1);
        assert_eq!(offset.load(Ordering::SeqCst), 100_000);
        assert_eq!(std::fs::read(&path).unwrap()[..100_000], data[..100_000]);
    }

    fn setup(mirrors: usize) -> (MirrorRacer, ChunkManager) {
        let urls = (0..mirrors).map(|i| Url::parse(&format!("http://m{}.example/f", i)).unwrap()).collect();
        let mut chunks = ChunkManager::new(4 * 1024 * 1024, 1024 * 1024).unwrap();
        chunks.set_max_retries(1);
        (MirrorRacer::new(urls), chunks)
    }

    /// A failed request to the mirror's current URL.
    fn fail(racer: &mut MirrorRacer, chunks: &mut ChunkManager, chunk: usize, mirror: usize, kind: FailureKind, error: &str) {
        let url = racer.get_mirror(mirror).unwrap().url.clone();
        record_failure(racer, chunks, chunk, mirror, &url, kind, error);
    }

    #[test]
    fn test_throttling_with_other_connections_costs_no_retries() {
        let (mut racer, mut chunks) = setup(1);
        for _ in 0..3 {
            racer.acquire_mirror(0); // three connections being served
        }
        for _ in 0..5 {
            let chunk = chunks.get_next_work(0, 0).map(|c| c.id).unwrap_or(0);
            fail(&mut racer, &mut chunks, chunk, 0, FailureKind::Throttled(None), "HTTP 503");
            assert!(chunks.chunks().iter().all(|c| c.retries == 0));
        }
        assert!(chunks.has_fatal_failure().is_none());
        assert_eq!(racer.get_mirror(0).unwrap().max_connections, 3);
        assert!(racer.get_mirror(0).unwrap().cooldown_until.is_none());
    }

    #[test]
    fn test_throttling_with_nothing_in_flight_pauses_and_counts() {
        let (mut racer, mut chunks) = setup(1);
        chunks.get_next_work(0, 0).unwrap();
        let busy = FailureKind::Throttled(Some(Duration::from_secs(2)));
        fail(&mut racer, &mut chunks, 0, 0, busy, "HTTP 503");
        assert_eq!(chunks.chunks()[0].retries, 1);
        assert_eq!(racer.select_best_mirror(), None, "mirror pauses for Retry-After");
        assert!(chunks.get_next_work(0, 0).is_none_or(|c| c.id != 0), "chunk waits for Retry-After");
    }

    #[test]
    fn test_bad_mirrors_are_dropped_until_none_left() {
        let (mut racer, mut chunks) = setup(2);
        for mirror in 0..2 {
            for _ in 0..2 {
                let chunk = chunks.get_next_work(0, mirror).unwrap().id;
                fail(&mut racer, &mut chunks, chunk, mirror, FailureKind::BadMirror, "HTTP 404");
            }
        }
        assert!(racer.all_inactive());
        let (_, reason) = chunks.has_fatal_failure().unwrap();
        assert!(reason.contains("every mirror failed") && reason.contains("404"), "{reason}");
        assert!(chunks.chunks().iter().all(|c| c.retries == 0), "a dead mirror is not the chunk's fault");
    }

    #[test]
    fn test_refused_redirect_target_sends_the_mirror_back_to_its_own_url_once() {
        let target = Url::parse("https://cdn.example/f.bin?sig=expired").unwrap();
        let own = Url::parse("https://example.com/releases/f.bin").unwrap();
        let mut racer = MirrorRacer::new(vec![target.clone()]);
        racer.get_mirror_mut(0).unwrap().fallback = Some(own.clone());
        let (_, mut chunks) = setup(1);

        // Every connection finds the signed target expired at once: none of that counts.
        for _ in 0..3 {
            let chunk = chunks.get_next_work(0, 0).unwrap().id;
            record_failure(&mut racer, &mut chunks, chunk, 0, &target, FailureKind::Denied, "HTTP 403");
        }
        let mirror = racer.get_mirror(0).unwrap();
        assert_eq!((&mirror.url, &mirror.fallback), (&own, &None));
        assert_eq!((mirror.bad_responses, mirror.failures, mirror.is_active), (0, 0, true));
        assert!(chunks.has_fatal_failure().is_none());
        assert!(chunks.chunks().iter().all(|c| c.retries == 0));

        // The mirror's own URL refusing is a bad mirror, as always.
        for _ in 0..2 {
            let chunk = chunks.get_next_work(0, 0).unwrap().id;
            record_failure(&mut racer, &mut chunks, chunk, 0, &own, FailureKind::Denied, "HTTP 404");
        }
        assert!(racer.all_inactive());
        assert!(chunks.has_fatal_failure().unwrap().1.contains("every mirror failed"));
    }

    #[test]
    fn test_fatal_failure_aborts() {
        let (mut racer, mut chunks) = setup(1);
        chunks.get_next_work(0, 0).unwrap();
        fail(&mut racer, &mut chunks, 0, 0, FailureKind::Fatal, "Disk write error: no space");
        assert_eq!(chunks.has_fatal_failure().unwrap().1, "Disk write error: no space");
    }

    #[test]
    fn test_retry_after_is_capped() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("86400"));
        assert_eq!(retry_after(&headers), Some(MAX_RETRY_AFTER));
        headers.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"));
        assert_eq!(retry_after(&headers), None);
    }

    #[tokio::test]
    async fn test_rate_limiter_holds_rate_across_callers() {
        let limiter = Arc::new(RateLimiter::new(200_000));
        let started = Instant::now();
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let l = Arc::clone(&limiter);
                tokio::spawn(async move {
                    for _ in 0..10 {
                        l.acquire(5_000).await;
                    }
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        // 200 KB at 200 KB/s, minus at most 100 ms of burst credit.
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(850), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(2000), "{elapsed:?}");
    }

    /// A route is dropped for a failed connection only once another route reached that server:
    /// a server that refuses everyone says nothing of the route.
    #[test]
    fn a_route_is_dropped_only_when_another_reaches_that_server() {
        let dir = tempfile::tempdir().unwrap();
        let (url, other) = (Url::parse("http://a.example/f").unwrap(), Url::parse("http://b.example/f").unwrap());
        let (worker, _, _) = test_worker(&url, &dir.path().join("f"), 10);
        let s = WorkerShared { clients: Arc::new(vec![Client::new(), Client::new()]), ..worker.shared.clone() };
        s.drop_route(0, &url);
        s.reached(1, &other);
        s.drop_route(0, &url);
        assert!(s.dropped_routes.lock().is_empty());
        s.reached(1, &url);
        s.drop_route(0, &url);
        assert_eq!(*s.dropped_routes.lock(), [0]);
    }
}
