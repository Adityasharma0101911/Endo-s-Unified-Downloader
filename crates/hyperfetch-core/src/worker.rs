use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, IF_RANGE, RANGE, RETRY_AFTER};
use reqwest::{Client, StatusCode};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::chunk::{Chunk, ChunkManager};
use crate::mirror::MirrorRacer;
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

/// Why an attempt failed, which decides how the engine retries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Timeout, reset, early EOF, 5xx: retry the chunk with backoff.
    Transient,
    /// 429/503, with the server's Retry-After if it sent one.
    Throttled(Option<Duration>),
    /// This mirror cannot serve the file (401/403/404/410, wrong Content-Range, ignored Range).
    BadMirror,
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

/// State shared by all workers of one download.
#[derive(Clone)]
pub struct WorkerShared {
    pub client: Client,
    pub writer: DiskWriter,
    pub chunks: Arc<Mutex<ChunkManager>>,
    pub mirrors: Arc<Mutex<MirrorRacer>>,
    pub events: mpsc::Sender<WorkerEvent>,
    pub cancel: CancellationToken,
    pub limiter: Option<Arc<RateLimiter>>,
    pub file_size: u64,
    pub min_steal: u64,
    /// Max wait for response headers and between body reads.
    pub stall_timeout: Duration,
}

pub struct HttpWorker {
    pub worker_id: usize,
    shared: WorkerShared,
}

impl HttpWorker {
    pub fn new(worker_id: usize, shared: WorkerShared) -> Self {
        Self { worker_id, shared }
    }

    /// Takes chunks (or steals halves of slow ones) until cancelled or the engine goes away.
    pub async fn run(self) {
        let s = &self.shared;
        loop {
            if s.cancel.is_cancelled() {
                return;
            }
            let Some((chunk, mirror_id, url, if_range)) = self.next_job() else {
                tokio::select! {
                    _ = s.cancel.cancelled() => return,
                    _ = tokio::time::sleep(IDLE_POLL) => continue,
                }
            };

            let outcome = self.download_chunk(&chunk, mirror_id, url, if_range).await;
            // Settle the chunk before looking for more work, so nobody (including this worker)
            // mistakes it for a live chunk to steal from.
            let event = {
                let mut racer = s.mirrors.lock();
                racer.release_mirror(mirror_id);
                if s.cancel.is_cancelled() {
                    return;
                }
                let mut chunks = s.chunks.lock();
                match outcome {
                    Ok(()) => {
                        if let Err(e) = chunks.mark_completed(chunk.id) {
                            chunks.abort(chunk.id, &e.to_string());
                        }
                        WorkerEvent::ChunkCompleted { worker_id: self.worker_id, chunk_id: chunk.id }
                    }
                    Err((kind, error)) => {
                        record_failure(&mut racer, &mut chunks, chunk.id, mirror_id, kind, &error);
                        WorkerEvent::ChunkFailed { worker_id: self.worker_id, chunk_id: chunk.id, mirror_id, kind, error }
                    }
                }
            };
            // Wakes the engine so it notices completion or a fatal error.
            if s.events.send(event).await.is_err() {
                return;
            }
        }
    }

    /// Picks the best available mirror, then a chunk for it. Lock order: mirrors, then chunks.
    fn next_job(&self) -> Option<(Chunk, usize, Url, Option<String>)> {
        let s = &self.shared;
        let mut racer = s.mirrors.lock();
        let mirror_id = racer.select_best_mirror()?;
        let (url, if_range) = racer.get_mirror(mirror_id).map(|m| (m.url.clone(), m.if_range.clone()))?;
        let chunk = {
            let mut mgr = s.chunks.lock();
            mgr.get_next_work(self.worker_id, mirror_id)
                .or_else(|| mgr.steal_work(self.worker_id, mirror_id, s.min_steal).map(|(_, c)| c))?
        };
        racer.acquire_mirror(mirror_id);
        Some((chunk, mirror_id, url, if_range))
    }

    /// Streams the chunk's remaining range into the file. Only this worker advances
    /// `chunk.current_offset`, and it does so after each write lands.
    async fn download_chunk(
        &self,
        chunk: &Chunk,
        mirror_id: usize,
        url: Url,
        if_range: Option<String>,
    ) -> Result<(), Failure> {
        let s = &self.shared;
        let start = chunk.current_offset.load(Ordering::SeqCst);
        let end = chunk.end_offset.load(Ordering::SeqCst);
        if start > end {
            return Ok(()); // stolen away entirely before we started
        }

        let mut request = s
            .client
            .get(url)
            .header(RANGE, format!("bytes={}-{}", start, end))
            .header(ACCEPT_ENCODING, "identity");
        if let Some(validator) = &if_range {
            request = request.header(IF_RANGE, validator.as_str());
        }

        let sent_at = Instant::now();
        let response = tokio::select! {
            biased;
            _ = s.cancel.cancelled() => return Err(cancelled()),
            res = tokio::time::timeout(s.stall_timeout, request.send()) => match res {
                Err(_) => return Err((FailureKind::Transient, format!("no response within {}s", s.stall_timeout.as_secs()))),
                Ok(Err(e)) => return Err((FailureKind::Transient, format!("request failed: {}", e))),
                Ok(Ok(resp)) => resp,
            },
        };
        check_response(response.status(), response.headers(), start, end, s.file_size, if_range.is_some())?;
        let _ = s.events.try_send(WorkerEvent::Ttfb {
            worker_id: self.worker_id,
            mirror_id,
            ttfb: sent_at.elapsed(),
        });

        let mut stream = response.bytes_stream();
        let mut pos = start;
        let mut pending: u64 = 0;
        let mut last_report = Instant::now();
        let result = loop {
            let item = tokio::select! {
                biased;
                _ = s.cancel.cancelled() => break Err(cancelled()),
                item = tokio::time::timeout(s.stall_timeout, stream.next()) => item,
            };
            let bytes = match item {
                Err(_) => break Err((
                    FailureKind::Transient,
                    format!("stalled: no data for {}s at offset {}", s.stall_timeout.as_secs(), pos),
                )),
                Ok(None) => break Ok(()),
                Ok(Some(Err(e))) => break Err((FailureKind::Transient, format!("read error at offset {}: {}", pos, e))),
                Ok(Some(Ok(bytes))) => bytes,
            };
            if let Some(limiter) = &s.limiter {
                tokio::select! {
                    biased;
                    _ = s.cancel.cancelled() => break Err(cancelled()),
                    _ = limiter.acquire(bytes.len() as u64) => {}
                }
            }

            // Re-read the end: a thief may have taken the tail of this chunk.
            let end = chunk.end_offset.load(Ordering::SeqCst);
            if pos > end {
                break Ok(());
            }
            let take = (bytes.len() as u64).min(end - pos + 1) as usize;
            if let Err(e) = s.writer.write_chunk_slice(pos, &bytes[..take]) {
                break Err((FailureKind::Fatal, format!("Disk write error: {}", e)));
            }
            pos += take as u64;
            chunk.current_offset.store(pos, Ordering::SeqCst);
            pending += take as u64;

            if pending >= 1024 * 1024 || last_report.elapsed() >= Duration::from_millis(100) {
                self.report_progress(chunk.id, mirror_id, &mut pending, &mut last_report);
            }
            if take < bytes.len() {
                break Ok(());
            }
        };
        self.report_progress(chunk.id, mirror_id, &mut pending, &mut last_report);
        result?;

        let end = chunk.end_offset.load(Ordering::SeqCst);
        if pos <= end {
            return Err((
                FailureKind::Transient,
                format!("connection closed at offset {}, expected data up to {}", pos, end),
            ));
        }
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

/// Applies a failed attempt to mirror and chunk bookkeeping. This is the retry policy:
/// transient errors cost the chunk a retry (unless it made progress) and back off;
/// bad mirrors are dropped after repeated bad answers; throttling while other connections are
/// being served lowers the connection cap instead of costing retries.
/// `mirror.in_flight` must already exclude the failed connection.
fn record_failure(
    racer: &mut MirrorRacer,
    chunks: &mut ChunkManager,
    chunk_id: usize,
    mirror_id: usize,
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
        FailureKind::BadMirror => {
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

/// Classifies the response to `Range: bytes=start-end` for a file of `file_size` bytes.
fn check_response(
    status: StatusCode,
    headers: &HeaderMap,
    start: u64,
    end: u64,
    file_size: u64,
    sent_if_range: bool,
) -> Result<(), Failure> {
    match status {
        StatusCode::PARTIAL_CONTENT => {
            let header = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok());
            match header.map(ByteRange::parse_content_range) {
                Some(Ok((range, Some(total)))) if range.start == start && total == file_size => Ok(()),
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
        // A full 200 is only usable from offset 0. Under If-Range it means the validator no
        // longer matches, unless we asked for the whole file anyway.
        StatusCode::OK if start == 0 && (!sent_if_range || end + 1 == file_size) => {
            match content_length(headers) {
                Some(len) if len != file_size => Err((
                    FailureKind::BadMirror,
                    format!("200 response is {} bytes, expected {}", len, file_size),
                )),
                _ => Ok(()),
            }
        }
        StatusCode::OK if sent_if_range => Err((
            FailureKind::Fatal,
            "remote file changed: server ignored If-Range and sent a different version".to_string(),
        )),
        StatusCode::OK => Err((FailureKind::BadMirror, format!("server ignored Range and sent 200 for offset {}", start))),
        StatusCode::RANGE_NOT_SATISFIABLE => Err((
            FailureKind::Fatal,
            format!("remote file changed: 416 Range Not Satisfiable for bytes {}-{}", start, end),
        )),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE => {
            Err((FailureKind::BadMirror, format!("HTTP {}", status)))
        }
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
            Err((FailureKind::Throttled(retry_after(headers)), format!("HTTP {}", status)))
        }
        _ => Err((FailureKind::Transient, format!("HTTP {}", status))),
    }
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

    fn check(status: u16, headers: &[(&'static str, &'static str)], start: u64, end: u64, size: u64, if_range: bool) -> Option<FailureKind> {
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
        assert_eq!(check(206, &cr, 100, 199, 1000, true), None);
        // Wrong start, or a total from a different file.
        assert_eq!(check(206, &cr, 50, 199, 1000, true), Some(FailureKind::BadMirror));
        assert_eq!(check(206, &[("content-range", "bytes 100-199/2000")], 100, 199, 1000, false), Some(FailureKind::BadMirror));
        assert_eq!(check(206, &[], 0, 9, 10, false), Some(FailureKind::BadMirror));

        let full = [("content-length", "1000")];
        assert_eq!(check(200, &full, 0, 499, 1000, false), None);
        assert_eq!(check(200, &full, 0, 999, 1000, true), None);
        assert_eq!(check(200, &full, 0, 499, 1000, true), Some(FailureKind::Fatal));
        assert_eq!(check(200, &full, 500, 999, 1000, true), Some(FailureKind::Fatal));
        assert_eq!(check(200, &full, 500, 999, 1000, false), Some(FailureKind::BadMirror));
        assert_eq!(check(200, &[("content-length", "512")], 0, 999, 1000, false), Some(FailureKind::BadMirror));

        assert_eq!(check(416, &[], 0, 9, 10, true), Some(FailureKind::Fatal));
        for status in [401, 403, 404, 410] {
            assert_eq!(check(status, &[], 0, 9, 10, false), Some(FailureKind::BadMirror));
        }
        assert_eq!(
            check(503, &[("retry-after", "3")], 0, 9, 10, false),
            Some(FailureKind::Throttled(Some(Duration::from_secs(3))))
        );
        assert_eq!(check(429, &[], 0, 9, 10, false), Some(FailureKind::Throttled(None)));
        assert_eq!(check(502, &[], 0, 9, 10, false), Some(FailureKind::Transient));
    }

    fn setup(mirrors: usize) -> (MirrorRacer, ChunkManager) {
        let urls = (0..mirrors).map(|i| Url::parse(&format!("http://m{}.example/f", i)).unwrap()).collect();
        let mut chunks = ChunkManager::new(4 * 1024 * 1024, 1024 * 1024).unwrap();
        chunks.set_max_retries(1);
        (MirrorRacer::new(urls), chunks)
    }

    #[test]
    fn test_throttling_with_other_connections_costs_no_retries() {
        let (mut racer, mut chunks) = setup(1);
        for _ in 0..3 {
            racer.acquire_mirror(0); // three connections being served
        }
        for _ in 0..5 {
            let chunk = chunks.get_next_work(0, 0).map(|c| c.id).unwrap_or(0);
            record_failure(&mut racer, &mut chunks, chunk, 0, FailureKind::Throttled(None), "HTTP 503");
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
        record_failure(&mut racer, &mut chunks, 0, 0, busy, "HTTP 503");
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
                record_failure(&mut racer, &mut chunks, chunk, mirror, FailureKind::BadMirror, "HTTP 404");
            }
        }
        assert!(racer.all_inactive());
        let (_, reason) = chunks.has_fatal_failure().unwrap();
        assert!(reason.contains("every mirror failed") && reason.contains("404"), "{reason}");
        assert!(chunks.chunks().iter().all(|c| c.retries == 0), "a dead mirror is not the chunk's fault");
    }

    #[test]
    fn test_fatal_failure_aborts() {
        let (mut racer, mut chunks) = setup(1);
        chunks.get_next_work(0, 0).unwrap();
        record_failure(&mut racer, &mut chunks, 0, 0, FailureKind::Fatal, "Disk write error: no space");
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
}
