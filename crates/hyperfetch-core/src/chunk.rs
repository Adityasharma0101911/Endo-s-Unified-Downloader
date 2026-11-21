use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use crate::autopilot::{CALM, CLIMB};
use crate::range::{ByteRange, RangeError};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Base and cap of the per-chunk exponential retry backoff.
const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// A timed steal leaves a chunk alone unless its worker needs more than this many times a new
/// request's startup to finish it: otherwise the new request barely gets going before it ends.
const STEAL_MIN_STARTUPS: f64 = 2.0;
/// A connection is judged by its speed over windows this long: shorter ones see bursts and TCP's
/// slow start, not a route. Also the least time between two replaced connections of a download
/// (see `ChunkManager::replaces`).
pub(crate) const LAG_WINDOW: Duration = Duration::from_secs(5);
/// A steal also goes by a connection's speed over this much of its last bytes: one fading out
/// shows there at once, where its average since its answer, or over `LAG_WINDOW`, takes long to.
pub(crate) const RECENT: Duration = Duration::from_millis(500);
/// A connection going at under this share of the median of the others makes way for a new one.
const LAG_SHARE: f64 = 0.3;
/// Fewest connections to a mirror, the slow one included, for their median to say anything.
const LAG_PEERS: usize = 4;

/// How a thief may split a chunk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StealRule<'a> {
    /// Fewest bytes a steal hands the thief: a split that would give it less is not made.
    pub min_bytes: u64,
    /// What a new request to each mirror costs, by mirror id. Without it the rest of a chunk is
    /// split in half.
    pub timing: Option<&'a [StealTiming]>,
}

/// What a new request to a mirror costs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StealTiming {
    /// Time the request needs before its first byte.
    pub startup: Duration,
    /// Bytes per second its connection is expected to reach.
    pub rate: f64,
}

/// A steal of at least this many bytes, split in half.
impl From<u64> for StealRule<'_> {
    fn from(min_bytes: u64) -> Self {
        Self { min_bytes, timing: None }
    }
}

#[derive(Error, Debug)]
pub enum ChunkError {
    #[error("Range error: {0}")]
    Range(#[from] RangeError),
    #[error("Chunk {0} not found")]
    NotFound(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkStatus {
    Unassigned,
    /// A worker owns the chunk and is (or is about to be) streaming it.
    Assigned {
        worker_id: usize,
        mirror_id: usize,
    },
    Completed,
    /// Waiting to be retried. `retries` counts consecutive attempts that made no progress.
    Failed {
        reason: String,
        retries: u32,
    },
    /// Its bytes went to another chunk's request (see `ChunkManager::merge_pending`): nothing is
    /// left to do for it.
    Merged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkSnapshot {
    pub id: usize,
    pub range_start: u64,
    pub range_end: u64,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub status: String,
    pub worker_id: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub id: usize,
    pub range: ByteRange,
    pub status: ChunkStatus,
    pub retries: u32,
    /// Next byte the worker will write; `[range.start, current_offset)` is on disk.
    /// Only the worker downloading the chunk advances it.
    pub current_offset: Arc<AtomicU64>,
    /// Inclusive end, lowered by work stealing while the chunk is in flight.
    pub end_offset: Arc<AtomicU64>,
    /// Cancelled when another worker takes over what is left: the attempt's worker then stops at
    /// once and settles nothing, since the chunk is no longer its own.
    pub(crate) revoked: CancellationToken,
    /// What the current attempt's worker tells of it, shared with that worker.
    attempt: Arc<Mutex<Attempt>>,
    not_before: Option<Instant>,
    assigned_at: Option<Instant>,
    /// Another worker asked for it again while its request had no answer (see
    /// `ChunkManager::hedge`): once per chunk.
    hedged: bool,
}

/// A second request for what is left of a chunk whose request has no answer yet (see
/// [`ChunkManager::hedge`]): the first answer keeps the chunk.
#[derive(Debug)]
pub(crate) struct Hedge {
    pub(crate) chunk_id: usize,
    /// The range asked for again.
    pub(crate) start: u64,
    pub(crate) end: u64,
    /// The attempt that has no answer.
    attempt: Arc<Mutex<Attempt>>,
}

/// How an attempt at a chunk goes, as its worker tells it.
#[derive(Debug, Default)]
struct Attempt {
    /// When its answer came, and the chunk's offset then: its rate counts from there, as the wait
    /// for the answer moved no bytes.
    answered: Option<(Instant, u64)>,
    /// Since when its worker has been waiting for the server without a byte; `None` while it is
    /// busy (writing, rate limited) rather than waiting, and until the answer came: a server slow
    /// to answer is no silent one, and the wait for an answer has the stall timeout of its own.
    waiting: Option<Instant>,
    /// Next byte its worker has yet to receive (0 until it tells): it writes what arrived in
    /// batches, so the bytes it holds would otherwise look left to fetch.
    received: u64,
    /// Bytes per second over its worker's last full `LAG_WINDOW`, and over the fastest of them.
    window_rate: Option<f64>,
    fastest: f64,
    /// Bytes per second over its worker's last `RECENT`.
    recent: Option<f64>,
    /// Its fastest `RECENT` window (one more than `CLIMB` faster than the fastest before), and the
    /// windows since that were not: fewer than `CALM`, its speed still climbs (see
    /// [`Chunk::climbing`]).
    lately_best: f64,
    calm: u32,
    /// The last byte its request asked for (0 until told): the answer brings the bytes up to it
    /// whatever the chunk's end became since (see [`ChunkManager::strand_nothing`]).
    asked: u64,
}

impl Chunk {
    pub fn new(id: usize, range: ByteRange) -> Self {
        Self {
            id,
            range,
            status: ChunkStatus::Unassigned,
            retries: 0,
            current_offset: Arc::new(AtomicU64::new(range.start)),
            end_offset: Arc::new(AtomicU64::new(range.end)),
            revoked: CancellationToken::new(),
            attempt: Arc::default(),
            not_before: None,
            assigned_at: None,
            hedged: false,
        }
    }

    /// The attempt's answer came at `at`: from then on its worker waits for the server's bytes, and
    /// the chunk is its own. False when the request it was cut from took it back first (see
    /// [`ChunkManager::run_on`]): the answer is thrown away.
    pub(crate) fn answered(&self, at: Instant) -> bool {
        let from = self.current_offset.load(Ordering::SeqCst);
        let mut attempt = self.attempt.lock();
        if self.revoked.is_cancelled() {
            return false;
        }
        attempt.answered = Some((at, from));
        attempt.waiting = Some(at);
        true
    }

    /// Whether the attempt's answer came (see [`Chunk::answered`]).
    pub(crate) fn has_answer(&self) -> bool {
        self.attempt.lock().answered.is_some()
    }

    /// The attempt's request asked for the bytes up to `end`.
    pub(crate) fn asked(&self, end: u64) {
        self.attempt.lock().asked = end;
    }

    /// The attempt's worker waits for the server from `since` on.
    pub(crate) fn wait_for_server(&self, since: Instant) {
        self.attempt.lock().waiting = Some(since);
    }

    /// The attempt's worker is busy with what arrived, not waiting for the server.
    pub(crate) fn busy(&self) {
        self.attempt.lock().waiting = None;
    }

    /// The attempt's worker holds the chunk's bytes before `to`, on disk or about to be.
    pub(crate) fn received(&self, to: u64) {
        self.attempt.lock().received = to;
    }

    /// The attempt went at `rate` bytes per second over its worker's last `LAG_WINDOW`.
    pub(crate) fn went_at(&self, rate: f64) {
        let mut attempt = self.attempt.lock();
        attempt.window_rate = Some(rate);
        attempt.fastest = attempt.fastest.max(rate);
    }

    /// The attempt went at `rate` bytes per second over its worker's last `RECENT`.
    pub(crate) fn went_lately(&self, rate: f64) {
        let mut attempt = self.attempt.lock();
        attempt.recent = Some(rate);
        match rate > (1.0 + CLIMB) * attempt.lately_best {
            true => (attempt.lately_best, attempt.calm) = (rate, 0),
            false => attempt.calm += 1,
        }
    }

    /// Whether the attempt's speed still climbs, as a new connection's does for a while after its
    /// answer, on a line where others have their share for seconds: its fastest `RECENT` window
    /// passed in none of the `CALM` after it yet, as `autopilot` reads a speed. Such a connection
    /// is no crawling one, however little it brought since its answer.
    fn climbing(&self) -> bool {
        self.attempt.lock().calm < CALM
    }

    /// Bytes still to arrive before the (possibly stolen-from) end: what the worker holds to
    /// write is no more left to fetch than what is on disk.
    fn unreceived(&self) -> u64 {
        let held = self.attempt.lock().received;
        let pos = self.current_offset.load(Ordering::SeqCst).max(self.range.start).max(held);
        self.end_offset.load(Ordering::SeqCst).saturating_add(1).saturating_sub(pos)
    }

    /// How long the attempt's worker has been waiting for the server without a byte.
    fn silence(&self, now: Instant) -> Duration {
        self.attempt.lock().waiting.map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
    }

    pub fn is_completed(&self) -> bool {
        matches!(self.status, ChunkStatus::Completed)
    }

    pub fn is_in_flight(&self) -> bool {
        matches!(self.status, ChunkStatus::Assigned { .. })
    }

    /// Bytes still to be written before the (possibly stolen-from) end.
    pub fn remaining_bytes(&self) -> u64 {
        let cur_pos = self.current_offset.load(Ordering::SeqCst).max(self.range.start);
        let cur_end = self.end_offset.load(Ordering::SeqCst);
        if cur_end >= cur_pos {
            cur_end - cur_pos + 1
        } else {
            0
        }
    }

    /// The contiguous prefix already on disk, if any.
    fn written_prefix(&self) -> Option<ByteRange> {
        match self.status {
            ChunkStatus::Completed => return Some(self.range),
            ChunkStatus::Merged => return None,
            _ => {}
        }
        let pos = self.current_offset.load(Ordering::SeqCst).min(self.range.end.saturating_add(1));
        (pos > self.range.start).then(|| ByteRange { start: self.range.start, end: pos - 1 })
    }

    fn done_bytes(&self) -> u64 {
        self.written_prefix().map_or(0, |r| r.len())
    }

    /// Bytes per second written since the attempt's answer came: the wait for the answer is no
    /// transfer, and counted in, it would make an attempt that just answered after a long wait
    /// look nearly stalled. `None` before the answer, and until a first batch of what it brings is
    /// on disk, which a worker writes once it holds a few hundred KiB or a quarter of a second of
    /// data: the least a rate is measured over.
    fn rate(&self, now: Instant) -> Option<f64> {
        let (at, from) = self.attempt.lock().answered?;
        let received = self.current_offset.load(Ordering::SeqCst).saturating_sub(from);
        let elapsed = now.saturating_duration_since(at).as_secs_f64();
        (received > 0 && elapsed > 0.0).then(|| received as f64 / elapsed)
    }

    /// How the chunk's worker is expected to go on: seconds until its next byte, and bytes per
    /// second from then on. Once it has a rate since its answer, that rate at once, or its last
    /// `LAG_WINDOW`'s or `RECENT`'s if slower: a connection that slowed down near its end, as a
    /// tail left to a faltering route does, would take long to drag its average down, and a tail
    /// it keeps at that average may never come; before, what a request to
    /// its mirror costs (`expected`), less the part of the startup already behind it. `None`
    /// without either.
    fn pace(&self, now: Instant, expected: Option<&StealTiming>) -> Option<(f64, f64)> {
        if let Some(rate) = self.rate(now) {
            let attempt = self.attempt.lock();
            return Some((0.0, [attempt.window_rate, attempt.recent].into_iter().flatten().fold(rate, f64::min)));
        }
        let expected = expected.filter(|t| t.rate > 0.0)?;
        let waited = self.assigned_at.map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        Some((expected.startup.saturating_sub(waited).as_secs_f64(), expected.rate))
    }
}

/// Seconds until a worker going on at `pace` has written `remaining` bytes (infinite without a pace).
fn eta_secs(remaining: u64, pace: Option<(f64, f64)>) -> f64 {
    pace.map_or(f64::INFINITY, |(wait, rate)| wait + remaining as f64 / rate)
}

/// Manages chunk partitioning, assignment, retries and work stealing.
/// Invariant: `chunks[i].id == i`.
#[derive(Debug)]
pub struct ChunkManager {
    total_size: u64,
    base_chunk_size: u64,
    chunks: Vec<Chunk>,
    /// Chunks with nothing left to do: completed, or merged into another.
    completed: usize,
    /// No `Unassigned` chunk exists below this index.
    next_fresh: usize,
    /// Failed chunks waiting to be retried.
    retry_queue: Vec<usize>,
    max_retries: u32,
    fatal: Option<(usize, String)>,
    /// When a slow connection last made way for a new one (see `replaces`).
    replaced_at: Option<Instant>,
    /// The first stream, whose answer runs to the file's end, and its attempt (see `halve`).
    stream: Option<(usize, Arc<Mutex<Attempt>>)>,
    /// What a new request was seen to take to answer at least: twice the longest wait of one
    /// dropped without an answer, as its piece was taken back (see `take_after`). Every later cut
    /// and steal leaves the request it splits at least that much of its own.
    setup: Duration,
    /// How long the first stream took from its answer to show its speed, when it was first halved
    /// (see `halve`): a new connection to the host climbs as long before it goes at its mirror's
    /// speed (see `take_over_slow`).
    climb: Duration,
    /// The piece cut off the first stream for one more connection the Autopilot judges, the
    /// stream's until the verdict (see [`ChunkManager::on_trial`]).
    trial: Option<usize>,
    /// The first stream reached that piece before the verdict and took it back (see
    /// [`ChunkManager::overtaken`]).
    overtaken: bool,
}

impl ChunkManager {
    /// Creates a new ChunkManager partitioning a file of `total_size` into chunks of `base_chunk_size`.
    pub fn new(total_size: u64, base_chunk_size: u64) -> Result<Self, ChunkError> {
        Self::with_resumed_ranges(total_size, base_chunk_size, &[])
    }

    /// Creates a ChunkManager from pre-existing completed ranges (resuming a previous download).
    /// Ranges may be unsorted, overlapping or extend past the end of the file.
    pub fn with_resumed_ranges(
        total_size: u64,
        base_chunk_size: u64,
        completed_ranges: &[ByteRange],
    ) -> Result<Self, ChunkError> {
        let base_chunk_size = base_chunk_size.max(64 * 1024);
        let in_file: Vec<ByteRange> = match total_size.checked_sub(1) {
            Some(last) => {
                let whole = ByteRange::new(0, last)?;
                completed_ranges.iter().filter_map(|r| r.intersection(&whole)).collect()
            }
            None => Vec::new(),
        };
        let merged_completed = crate::range::merge_ranges(in_file);
        let gaps = crate::range::compute_gaps(total_size, &merged_completed);

        let mut chunks = Vec::new();
        for completed_range in &merged_completed {
            let mut chunk = Chunk::new(0, *completed_range);
            chunk.status = ChunkStatus::Completed;
            chunks.push(chunk);
        }
        for gap in &gaps {
            let mut offset = gap.start;
            while offset <= gap.end {
                let chunk_len = (gap.end - offset + 1).min(base_chunk_size);
                chunks.push(Chunk::new(0, ByteRange::from_len(offset, chunk_len)?));
                offset += chunk_len;
            }
        }

        chunks.sort_by_key(|c| c.range.start);
        for (idx, chunk) in chunks.iter_mut().enumerate() {
            chunk.id = idx;
        }

        Ok(Self {
            total_size,
            base_chunk_size,
            completed: merged_completed.len(),
            chunks,
            next_fresh: 0,
            retry_queue: Vec::new(),
            max_retries: 8,
            fatal: None,
            replaced_at: None,
            stream: None,
            setup: Duration::ZERO,
            climb: Duration::ZERO,
            trial: None,
            overtaken: false,
        })
    }

    /// Failed attempts without progress a chunk may accumulate before the download fails.
    pub fn set_max_retries(&mut self, max_retries: u32) {
        self.max_retries = max_retries;
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn base_chunk_size(&self) -> u64 {
        self.base_chunk_size
    }

    /// Bytes on disk: completed chunks plus the written prefix of every other chunk.
    pub fn total_downloaded(&self) -> u64 {
        self.chunks.iter().map(Chunk::done_bytes).sum::<u64>().min(self.total_size)
    }

    /// Bytes in, on disk or held by their workers to be written: a steadier count than
    /// `total_downloaded`, which moves a batch at a time.
    pub(crate) fn total_received(&self) -> u64 {
        self.chunks.iter().map(Self::has).sum::<u64>().min(self.total_size)
    }

    /// What the first stream (see [`ChunkManager::halve`]) has in, counted as
    /// [`ChunkManager::total_received`] counts each chunk; all there is before it was halved.
    pub(crate) fn first_received(&self) -> u64 {
        self.stream.as_ref().map_or_else(|| self.total_received(), |(id, _)| Self::has(&self.chunks[*id]))
    }

    fn has(c: &Chunk) -> u64 {
        let received = match c.is_in_flight() {
            true => c.attempt.lock().received.saturating_sub(c.range.start).min(c.range.len()),
            false => 0,
        };
        c.done_bytes().max(received)
    }

    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// Chunks in flight whose request was answered.
    pub(crate) fn answered(&self) -> usize {
        self.chunks.iter().filter(|c| c.is_in_flight() && c.attempt.lock().answered.is_some()).count()
    }

    pub fn is_all_completed(&self) -> bool {
        self.completed == self.chunks.len()
    }

    pub fn progress_ratio(&self) -> f64 {
        if self.total_size == 0 {
            1.0
        } else {
            self.total_downloaded() as f64 / self.total_size as f64
        }
    }

    /// Hands out a failed chunk whose backoff has elapsed, else the next unassigned chunk.
    pub fn get_next_work(&mut self, worker_id: usize, mirror_id: usize) -> Option<Chunk> {
        if self.fatal.is_some() {
            return None;
        }
        let now = Instant::now();
        let ready = self
            .retry_queue
            .iter()
            .position(|&id| self.chunks[id].not_before.is_none_or(|t| t <= now));
        if let Some(pos) = ready {
            let id = self.retry_queue.remove(pos);
            return Some(self.assign(id, worker_id, mirror_id, now));
        }
        while self.next_fresh < self.chunks.len() {
            let id = self.next_fresh;
            self.next_fresh += 1;
            if self.chunks[id].status == ChunkStatus::Unassigned {
                return Some(self.assign(id, worker_id, mirror_id, now));
            }
        }
        None
    }

    /// Hands `worker_id` the unassigned chunk that starts at `start`, cut to end at `end` if it
    /// goes further (the rest becomes a chunk of its own): for bytes an answer already on its way
    /// brings, such as the probe's, whose range ends at `end`. `None` without such a chunk, as
    /// when those bytes are on disk.
    pub fn assign_at(&mut self, worker_id: usize, mirror_id: usize, start: u64, end: u64) -> Option<Chunk> {
        if self.fatal.is_some() || end < start {
            return None;
        }
        let id = self.chunks.iter().position(|c| c.status == ChunkStatus::Unassigned && c.range.start == start)?;
        let chunk_end = self.chunks[id].range.end;
        if chunk_end > end {
            let rest = Chunk::new(self.chunks.len(), ByteRange::new(end + 1, chunk_end).ok()?);
            let chunk = &mut self.chunks[id];
            chunk.range.end = end;
            chunk.end_offset.store(end, Ordering::SeqCst);
            self.chunks.push(rest);
        }
        Some(self.assign(id, worker_id, mirror_id, Instant::now()))
    }

    fn assign(&mut self, id: usize, worker_id: usize, mirror_id: usize, now: Instant) -> Chunk {
        let chunk = &mut self.chunks[id];
        chunk.status = ChunkStatus::Assigned { worker_id, mirror_id };
        chunk.not_before = None;
        chunk.assigned_at = Some(now);
        // Whatever an earlier worker told of its attempt stays with it.
        chunk.attempt = Arc::default();
        chunk.clone()
    }

    /// Hands everything left of the in-flight chunk whose worker has waited longest for the server
    /// without a byte, once its answer came, to an idle thief, if that wait is at least
    /// `min_silence(chunk's mirror)`. A chunk still waiting for its answer is never taken: the
    /// thief's request would wait as long, and be taken over in turn. The silent attempt is
    /// revoked. What it wrote becomes a completed chunk; the rest keeps the chunk's id and gets new
    /// offsets and a new token, so the revoked worker, which still holds the old ones, can neither
    /// move nor settle what the thief now owns.
    pub fn take_over_silent(
        &mut self,
        thief_worker_id: usize,
        thief_mirror_id: usize,
        min_silence: impl Fn(usize) -> Duration,
    ) -> Option<Chunk> {
        if self.fatal.is_some() {
            return None;
        }
        let now = Instant::now();
        let (id, _) = self
            .chunks
            .iter()
            .filter_map(|c| match c.status {
                ChunkStatus::Assigned { mirror_id, .. } if c.remaining_bytes() > 0 => {
                    let silent = c.silence(now);
                    (silent >= min_silence(mirror_id)).then_some((c.id, silent))
                }
                _ => None,
            })
            .max_by_key(|&(_, silent)| silent)?;
        self.take_over(id, thief_worker_id, thief_mirror_id, now)
    }

    /// Hands everything left of the in-flight chunk to an idle thief whose request, at its
    /// mirror's `timing`, would bring it `STEAL_MIN_STARTUPS` times sooner than the chunk's worker
    /// at its pace: a tail too short to split (see [`ChunkManager::steal_work`]), left to a
    /// connection fading out, which may never bring it, as [`ChunkManager::take_over_silent`].
    /// Not one whose speed still climbs (see [`Chunk::climbing`]), as one just answered or just
    /// taken over does: little since its answer is no crawl, and its thief's would climb as well.
    /// The thief's request is reckoned at its mirror's speed only after that climb, as long as the
    /// first stream's took (see `ChunkManager::climb`).
    pub(crate) fn take_over_slow(&mut self, thief_worker_id: usize, thief_mirror_id: usize, timing: &[StealTiming]) -> Option<Chunk> {
        let thief = timing.get(thief_mirror_id).filter(|t| t.rate > 0.0)?;
        let now = Instant::now();
        let (id, _) = self
            .chunks
            .iter()
            .filter(|c| c.is_in_flight() && self.fatal.is_none() && !c.climbing())
            .filter_map(|c| {
                let (left, pace) = (c.unreceived(), c.pace(now, None));
                let theirs = eta_secs(left, pace);
                let ours = (thief.startup + self.climb).as_secs_f64() + left as f64 / thief.rate;
                (left > 0 && pace.is_some() && theirs > STEAL_MIN_STARTUPS * ours).then_some((c.id, theirs))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        self.take_over(id, thief_worker_id, thief_mirror_id, now)
    }

    /// Revokes chunk `id`'s attempt and hands what is left of it to the thief: what it wrote
    /// becomes a completed chunk.
    fn take_over(&mut self, id: usize, thief_worker_id: usize, thief_mirror_id: usize, now: Instant) -> Option<Chunk> {
        let chunk = &mut self.chunks[id];
        chunk.revoked.cancel();
        // Read after revoking: whatever the old worker records from now on is its own business.
        let pos = chunk.current_offset.load(Ordering::SeqCst).max(chunk.range.start);
        let end = chunk.end_offset.load(Ordering::SeqCst);
        if pos > end {
            // Its last byte got written meanwhile, and the revoked worker will not settle it.
            chunk.status = ChunkStatus::Completed;
            self.completed += 1;
            return None;
        }
        if pos > chunk.range.start {
            let written = ByteRange { start: chunk.range.start, end: pos - 1 };
            chunk.range.start = pos;
            let prefix_id = self.chunks.len();
            let mut done = Chunk::new(prefix_id, written);
            done.status = ChunkStatus::Completed;
            self.chunks.push(done);
            self.completed += 1;
        }
        Some(self.hand_over(id, pos, end, thief_worker_id, thief_mirror_id, now))
    }

    /// Assigns `[pos, end]` of chunk `id`, whose attempt was just revoked, to the thief, with new
    /// offsets and a new token: the revoked worker, which still holds the old ones, can neither
    /// move nor settle what the thief now owns.
    fn hand_over(&mut self, id: usize, pos: u64, end: u64, thief_worker_id: usize, thief_mirror_id: usize, now: Instant) -> Chunk {
        let chunk = &mut self.chunks[id];
        chunk.current_offset = Arc::new(AtomicU64::new(pos));
        chunk.end_offset = Arc::new(AtomicU64::new(end));
        chunk.revoked = CancellationToken::new();
        self.assign(id, thief_worker_id, thief_mirror_id, now)
    }

    /// What is left of the in-flight chunk whose request has waited longest for its answer, for an
    /// idle worker to ask for again: once at most per chunk, after `after(chunk's mirror)` without
    /// an answer, and only while `answered_since(that mirror, when the request went out)`, as a
    /// server answering other requests meanwhile lost this one rather than being slow. The chunk
    /// stays with its worker until the second answer comes first (see [`ChunkManager::claim`]).
    pub(crate) fn hedge(
        &mut self,
        after: impl Fn(usize) -> Duration,
        answered_since: impl Fn(usize, Instant) -> bool,
    ) -> Option<Hedge> {
        if self.fatal.is_some() {
            return None;
        }
        let now = Instant::now();
        let chunk = self
            .chunks
            .iter_mut()
            .filter(|c| !c.hedged && c.remaining_bytes() > 0 && c.attempt.lock().answered.is_none())
            .filter(|c| match (&c.status, c.assigned_at) {
                (ChunkStatus::Assigned { mirror_id, .. }, Some(sent)) => {
                    now.saturating_duration_since(sent) >= after(*mirror_id) && answered_since(*mirror_id, sent)
                }
                _ => false,
            })
            .min_by_key(|c| c.assigned_at)?;
        chunk.hedged = true;
        let start = chunk.current_offset.load(Ordering::SeqCst).max(chunk.range.start);
        Some(Hedge { chunk_id: chunk.id, start, end: chunk.end_offset.load(Ordering::SeqCst), attempt: Arc::clone(&chunk.attempt) })
    }

    /// Whether `hedge`'s chunk is still with the attempt it was asked for again for, with no
    /// answer and nothing written.
    pub(crate) fn hedging(&self, hedge: &Hedge) -> bool {
        self.chunks.get(hedge.chunk_id).is_some_and(|c| {
            c.is_in_flight()
                && Arc::ptr_eq(&c.attempt, &hedge.attempt)
                && !c.revoked.is_cancelled()
                && c.attempt.lock().answered.is_none()
                && c.current_offset.load(Ordering::SeqCst).max(c.range.start) == hedge.start
        })
    }

    /// The second request for `hedge`'s chunk was answered: unless the first was meanwhile (or
    /// the chunk is no longer that attempt's), it revokes that attempt and hands the chunk, up to
    /// its end now, to the thief.
    pub(crate) fn claim(&mut self, hedge: &Hedge, thief_worker_id: usize, thief_mirror_id: usize) -> Option<Chunk> {
        if self.fatal.is_some() || !self.hedging(hedge) {
            return None;
        }
        let chunk = &self.chunks[hedge.chunk_id];
        chunk.revoked.cancel();
        let end = chunk.end_offset.load(Ordering::SeqCst);
        Some(self.hand_over(hedge.chunk_id, hedge.start, end, thief_worker_id, thief_mirror_id, Instant::now()))
    }

    /// Splits the in-flight chunk expected to finish last (ties: most bytes left). Its worker is
    /// expected to go on at the rate it has had since its answer (or over its last window, if
    /// slower), or before it has one, at its mirror's rate once its mirror's startup is over. A timed steal splits where both sides
    /// finish together, the thief starting after its mirror's startup at its mirror's rate, and
    /// leaves the chunk alone if its worker finishes within `STEAL_MIN_STARTUPS` of those startups
    /// anyway. Without timing the rest is split in half. Bytes the victim's worker received but
    /// has yet to write count as its own: the split comes after them. A split that would hand the
    /// thief fewer than `min_bytes` is not made, and the victim keeps at least one byte. The victim
    /// is always truncated and the thief always gets `[split, old_end]`; if the victim's worker
    /// already wrote past the split, those bytes are identical and simply get written twice.
    /// Returns `(victim_chunk_id, new_stolen_chunk)`.
    pub fn steal_work<'a>(
        &mut self,
        thief_worker_id: usize,
        thief_mirror_id: usize,
        rule: impl Into<StealRule<'a>>,
    ) -> Option<(usize, Chunk)> {
        if self.fatal.is_some() {
            return None;
        }
        let rule = rule.into();
        let min_bytes = rule.min_bytes.max(1);
        let expected = |mirror_id: usize| rule.timing.and_then(|t| t.get(mirror_id));
        let now = Instant::now();
        let (victim_id, _, pace) = self
            .chunks
            .iter()
            // Not one whose request has no answer yet: the thief's would wait as long, and the
            // piece go back to the request it was cut from (see `ChunkManager::run_on`).
            .filter_map(|c| match c.status {
                ChunkStatus::Assigned { mirror_id, .. } if c.has_answer() => {
                    let remaining = c.unreceived();
                    (remaining > min_bytes).then(|| (c.id, remaining, c.pace(now, expected(mirror_id))))
                }
                _ => None,
            })
            .max_by(|a, b| eta_secs(a.1, a.2).total_cmp(&eta_secs(b.1, b.2)).then(a.1.cmp(&b.1)))?;

        let floor = self.setup.as_secs_f64();
        let victim = &mut self.chunks[victim_id];
        let cur_end = victim.end_offset.load(Ordering::SeqCst);
        let remaining = Some(victim.unreceived()).filter(|&n| n > 0)?;
        let keep = match (expected(thief_mirror_id).filter(|t| t.rate > 0.0), pace) {
            (Some(thief), Some((wait, victim_rate))) => {
                let (startup, left) = (thief.startup.as_secs_f64().max(floor), remaining as f64);
                if eta_secs(remaining, pace) <= STEAL_MIN_STARTUPS * startup {
                    return None;
                }
                // wait + keep / victim_rate == startup + (remaining - keep) / thief.rate
                (((startup - wait) * thief.rate + left) * victim_rate / (victim_rate + thief.rate)) as u64
            }
            // Untimed, it halves, but the victim keeps what it brings while a new request waits
            // as long as one was seen to.
            (_, Some((_, victim_rate))) if floor > 0.0 => {
                if eta_secs(remaining, pace) <= STEAL_MIN_STARTUPS * floor {
                    return None;
                }
                (remaining / 2).max((victim_rate * floor) as u64)
            }
            _ => remaining / 2,
        };
        let stolen = remaining.checked_sub(keep.max(1)).filter(|&n| n >= min_bytes)?;

        let split_offset = cur_end + 1 - stolen;
        victim.end_offset.store(split_offset - 1, Ordering::SeqCst);
        victim.range.end = split_offset - 1;

        let stolen_id = self.chunks.len();
        let stolen = Chunk::new(stolen_id, ByteRange::new(split_offset, cur_end).ok()?);
        self.chunks.push(stolen);
        Some((victim_id, self.assign(stolen_id, thief_worker_id, thief_mirror_id, now)))
    }

    /// Whether the attempt at `chunk_id`, which went at `rate` over its worker's last
    /// `LAG_WINDOW`, makes way for a new connection: a slow route tends to stay slow, and a new
    /// connection often gets a faster one. While at least `LAG_PEERS` connections to `mirror_id`
    /// run, at under `LAG_SHARE` of the median of the others (over their last window, or since their
    /// answer before they had one); with fewer, at under `LAG_SHARE` of its own fastest window and
    /// of every other's, as a connection slowing down before it stalls goes. Only with enough left
    /// to take that pace a window, as stealing sees to a short rest; and at most once per window in
    /// a download, so connections do not churn.
    pub(crate) fn replaces(&mut self, chunk_id: usize, mirror_id: usize, rate: f64, now: Instant) -> bool {
        if self.replaced_at.is_some_and(|at| now.saturating_duration_since(at) < LAG_WINDOW) {
            return false;
        }
        let mut others: Vec<f64> = self
            .chunks
            .iter()
            .filter(|c| c.id != chunk_id && matches!(c.status, ChunkStatus::Assigned { mirror_id: m, .. } if m == mirror_id))
            .filter_map(|c| {
                let window_rate = c.attempt.lock().window_rate;
                window_rate.or_else(|| c.rate(now))
            })
            .collect();
        others.sort_by(f64::total_cmp);
        let pace = if others.len() + 1 >= LAG_PEERS {
            others[others.len() / 2]
        } else {
            let fastest = self.chunks.get(chunk_id).map_or(0.0, |c| c.attempt.lock().fastest);
            others.first().map_or(fastest, |&slowest| slowest.min(fastest))
        };
        let left = self.chunks.get(chunk_id).map_or(0, Chunk::unreceived);
        if rate >= LAG_SHARE * pace || (left as f64) < pace * LAG_WINDOW.as_secs_f64() {
            return false;
        }
        self.replaced_at = Some(now);
        true
    }

    /// Work for `newcomers` more requests: the pieces no worker has first, then the far half of
    /// what each request under way has yet to receive, those with the most left first. Halves
    /// that end together: the newcomer's is short by what the request brings in `setup`, the time
    /// the newcomer's takes to answer, at its pace (that of the fastest request under way before
    /// it has one), and by no less than a request was seen to take (see `ChunkManager::setup`); none
    /// under `min_bytes`, which would not pay a request of its own. As a steal,
    /// it truncates each chunk, whose request keeps asking for that half until the newcomer's is
    /// answered (see [`ChunkManager::run_on`]). The first chunk ever halved is kept as the first
    /// stream (see [`ChunkManager::rejoin`]). How many requests have work.
    pub(crate) fn halve(&mut self, newcomers: usize, min_bytes: u64, setup: Duration) -> usize {
        let (now, setup) = (Instant::now(), setup.max(self.setup));
        let pace = |c: &Chunk| c.pace(now, None).map(|(_, rate)| rate);
        let fastest = self.chunks.iter().filter(|c| c.is_in_flight()).filter_map(pace).fold(0.0, f64::max);
        let given = |c: &Chunk| c.unreceived().saturating_sub((pace(c).unwrap_or(fastest) * setup.as_secs_f64()) as u64) / 2;
        let pending = self.chunks.iter().filter(|c| matches!(c.status, ChunkStatus::Unassigned | ChunkStatus::Failed { .. })).count();
        let mut halved: Vec<(usize, u64)> = self
            .chunks
            .iter()
            .filter(|c| c.is_in_flight() && !c.revoked.is_cancelled())
            .map(|c| (c.id, given(c)))
            .filter(|&(_, given)| given >= min_bytes.max(1))
            .collect();
        halved.sort_by_key(|&(id, _)| std::cmp::Reverse(self.chunks[id].unreceived()));
        halved.truncate(newcomers.saturating_sub(pending));
        for &(id, given) in &halved {
            let chunk = &mut self.chunks[id];
            let end = chunk.end_offset.load(Ordering::SeqCst);
            let at = end + 1 - given;
            chunk.end_offset.store(at - 1, Ordering::SeqCst);
            chunk.range.end = at - 1;
            if self.stream.is_none() {
                self.climb = chunk.attempt.lock().answered.map_or(Duration::ZERO, |(at, _)| now.saturating_duration_since(at));
                self.stream = Some((id, Arc::clone(&chunk.attempt)));
            }
            let piece = Chunk::new(self.chunks.len(), ByteRange { start: at, end });
            self.chunks.push(piece);
        }
        pending.min(newcomers) + halved.len()
    }

    /// Gives the first stream (see [`ChunkManager::halve`]) back the pieces no worker has right
    /// after it, for a host that limits how often it is asked: its answer runs to the file's end,
    /// so they come over the request already open instead of new ones. Only those it reaches at
    /// its pace within `wait`, the host's rest: past them, a new request once the host takes one
    /// again brings a piece sooner. Only while that attempt goes on and has yet to reach its end;
    /// one that reaches the old end first fails there, and is retried as any connection cut short.
    pub(crate) fn rejoin(&mut self, wait: Duration) {
        let Some((id, attempt)) = &self.stream else { return };
        let (id, stream) = (*id, &self.chunks[*id]);
        if !stream.is_in_flight() || !Arc::ptr_eq(&stream.attempt, attempt) || stream.revoked.is_cancelled() || stream.unreceived() == 0 {
            return;
        }
        let Some((_, rate)) = stream.pace(Instant::now(), None) else { return };
        let mut end = stream.end_offset.load(Ordering::SeqCst);
        let reach = (end + 1 - stream.unreceived()).saturating_add((rate * wait.as_secs_f64()) as u64);
        while let Some(next) = self.chunks.iter().position(|c| {
            c.range.start == end + 1 && c.range.start <= reach && matches!(c.status, ChunkStatus::Unassigned | ChunkStatus::Failed { .. })
        }) {
            end = self.chunks[next].range.end;
            self.chunks[next].status = ChunkStatus::Merged;
            self.completed += 1;
            self.retry_queue.retain(|&queued| queued != next);
        }
        let stream = &mut self.chunks[id];
        stream.range.end = end;
        stream.end_offset.store(end, Ordering::SeqCst);
    }

    /// Gives each request under way the pieces no worker has right after its chunk's end, as far
    /// as the request asked: when a refusal ended the download's widening, or the width came back
    /// to one, what no connection is to take comes over an answer already on its way (the first
    /// stream's runs to the file's end) instead of a new request after the host's rest. A piece
    /// further than any request asked is left for one. With `wait`, the host's rest after a
    /// refusal at another time (a piece cut off a request and refused, say), only the pieces it
    /// reaches at its pace within that rest, as [`ChunkManager::rejoin`] gives the first stream:
    /// past them, a new request once the host takes one again brings a piece sooner.
    pub(crate) fn strand_nothing(&mut self, wait: Option<Duration>) {
        let now = Instant::now();
        for id in 0..self.chunks.len() {
            let chunk = &self.chunks[id];
            if !chunk.is_in_flight() || chunk.revoked.is_cancelled() || chunk.unreceived() == 0 {
                continue;
            }
            let end = chunk.end_offset.load(Ordering::SeqCst);
            let reach = match wait {
                None => u64::MAX,
                Some(wait) => match chunk.pace(now, None) {
                    Some((_, rate)) => (end + 1 - chunk.unreceived()).saturating_add((rate * wait.as_secs_f64()) as u64),
                    None => continue,
                },
            };
            self.take_after(id, reach, true, false);
        }
    }

    /// Chunk `id`'s request, at its chunk's end, goes on over the piece right after it that it
    /// asked for, unless another request for that piece was answered: a piece cut off a request
    /// under way changes hands when the newcomer's answer comes, and one whose newcomer was refused,
    /// failed, or has no answer yet comes over the request already streaming instead (the
    /// newcomer's is dropped, its answer thrown away, see [`Chunk::answered`]). So no piece is left
    /// without a request that asked for it, as one refused after its stream ended would be, to
    /// wait out the host's rest. While the download `widening`, over one no worker has taken yet
    /// too, which the newcomers would; else that waits its turn as any other. Only for the worker
    /// whose attempt the chunk still is (`mine`, as it holds it). Whether it goes on.
    pub(crate) fn run_on(&mut self, mine: &Chunk, widening: bool) -> bool {
        let Some(chunk) = self.chunks.get(mine.id).filter(|c| c.is_in_flight() && Arc::ptr_eq(&c.attempt, &mine.attempt) && !c.revoked.is_cancelled())
        else {
            return false;
        };
        let end = chunk.end_offset.load(Ordering::SeqCst);
        self.take_after(mine.id, end + 1, widening, true) > end
    }

    /// Merges into chunk `id` the pieces right after its end that start by `reach`, as far as its
    /// request asked, that failed, with `fresh` that no worker has taken yet, and with `back` whose
    /// request has no answer yet (that request is dropped, see [`ChunkManager::run_on`]), or that
    /// is on trial, as the first stream reached it before the verdict, if the stream brings it
    /// sooner than its newcomer (see [`ChunkManager::outpaces`]). Its end then.
    fn take_after(&mut self, id: usize, reach: u64, fresh: bool, back: bool) -> u64 {
        let asked = self.chunks[id].attempt.lock().asked;
        let mut end = self.chunks[id].end_offset.load(Ordering::SeqCst);
        let now = Instant::now();
        let first = self.stream.as_ref().is_some_and(|(stream, _)| *stream == id);
        let trial = self.trial.filter(|&trial| back && first && self.outpaces(id, trial, now));
        // How long the request of a piece taken back had waited for its answer.
        let mut waited = Duration::ZERO;
        while let Some(next) = self.chunks.iter().position(|c| {
            c.range.start == end + 1
                && c.range.end <= asked
                && c.range.start <= reach
                && match c.status {
                    ChunkStatus::Failed { .. } => true,
                    ChunkStatus::Unassigned => fresh,
                    // Revoked under the attempt's lock, which its answer takes too: one or the other.
                    ChunkStatus::Assigned { .. } if back => {
                        let attempt = c.attempt.lock();
                        let unanswered = attempt.answered.is_none() && c.current_offset.load(Ordering::SeqCst) <= c.range.start;
                        (unanswered || trial == Some(c.id)) && {
                            c.revoked.cancel();
                            if unanswered {
                                waited = waited.max(c.assigned_at.map_or(Duration::ZERO, |sent| now.saturating_duration_since(sent)));
                            }
                            true
                        }
                    }
                    _ => false,
                }
        }) {
            end = self.chunks[next].range.end;
            self.chunks[next].status = ChunkStatus::Merged;
            self.completed += 1;
            self.retry_queue.retain(|&queued| queued != next);
        }
        if let Some(trial) = trial.filter(|&trial| self.chunks[trial].status == ChunkStatus::Merged) {
            (self.trial, self.overtaken) = (None, true);
            tracing::info!("Chunk {trial}: the first stream reached the piece of the connection beside it first, and goes on over it");
        }
        // A take-back is a measure: a new request takes longer than that to answer, so every later
        // cut and steal leaves the request it splits more of its own (see `ChunkManager::setup`).
        if !waited.is_zero() {
            self.setup = (2 * waited).max(2 * self.setup);
        }
        let chunk = &mut self.chunks[id];
        chunk.range.end = end;
        chunk.end_offset.store(end, Ordering::SeqCst);
        end
    }

    /// The piece the last [`ChunkManager::halve`] cut off the first stream goes to one more
    /// connection the Autopilot judges: it stays the stream's until the verdict, the stream's
    /// request asking for it still (see [`ChunkManager::verdict`]). A stream that reaches it first
    /// takes it back (see [`ChunkManager::run_on`]) where it brings it sooner (see
    /// [`ChunkManager::outpaces`]), a "no" (see [`ChunkManager::overtaken`]).
    pub(crate) fn on_trial(&mut self) {
        let Some((stream, _)) = &self.stream else { return };
        let end = self.chunks[*stream].end_offset.load(Ordering::SeqCst);
        self.trial = self.chunks.last().filter(|c| c.range.start == end + 1 && c.status == ChunkStatus::Unassigned).map(|c| c.id);
    }

    /// Whether the first stream took the piece on trial back since this was last asked.
    pub(crate) fn overtaken(&mut self) -> bool {
        std::mem::take(&mut self.overtaken)
    }

    /// The Autopilot's verdict on the connection on trial (see [`ChunkManager::on_trial`]): where
    /// more `pays`, its piece is its own from now on. Where not, it is dropped and the first stream
    /// reads on over its piece, if that brings it sooner (see [`ChunkManager::outpaces`]): no new
    /// request, and no connection given up that was up to speed. What it brought is written again,
    /// the same bytes. Whether it was dropped.
    pub(crate) fn verdict(&mut self, pays: bool) -> bool {
        let (Some(trial), Some((stream, attempt))) = (self.trial.take(), &self.stream) else { return false };
        let (stream, s, t) = (*stream, &self.chunks[*stream], &self.chunks[trial]);
        let runs = s.is_in_flight() && Arc::ptr_eq(&s.attempt, attempt) && !s.revoked.is_cancelled() && s.attempt.lock().asked >= t.range.end;
        let next = t.is_in_flight() && t.range.start == s.end_offset.load(Ordering::SeqCst) + 1;
        if pays || !runs || !next || !self.outpaces(stream, trial, Instant::now()) {
            return false;
        }
        t.revoked.cancel();
        let end = t.range.end;
        self.chunks[trial].status = ChunkStatus::Merged;
        self.completed += 1;
        let s = &mut self.chunks[stream];
        s.range.end = end;
        s.end_offset.store(end, Ordering::SeqCst);
        true
    }

    /// Whether chunk `stream`, reading on over chunk `trial` right after it from where it is,
    /// brings all that sooner than `trial`'s worker brings its piece: with `S` left of its own, the
    /// piece's `W` in and `R` to come, at its pace `r_s` and the newcomer's `r_n`, and the line
    /// theirs together once it is alone, `(S + W + R) / (r_s + r_n) < R / r_n`. Not where the
    /// newcomer keeps pace, as on a line both share alike, whose piece would come twice for
    /// nothing; a newcomer with no pace yet brought too little to count.
    fn outpaces(&self, stream: usize, trial: usize, now: Instant) -> bool {
        let (s, t) = (&self.chunks[stream], &self.chunks[trial]);
        let Some((_, mine)) = s.pace(now, None) else { return false };
        let theirs = t.pace(now, None).map_or(0.0, |(_, rate)| rate);
        let (left, had, rest) = (s.unreceived() as f64, Self::has(t) as f64, t.unreceived() as f64);
        (left + had + rest) * theirs < rest * (mine + theirs)
    }

    /// Hands back what the attempts of workers `from` and above have yet to receive, as fresh
    /// chunks for the others: a download whose connections were cut goes on without waiting for
    /// those attempts. Each keeps one byte, so it ends at its next, as a victim of a steal does.
    pub(crate) fn give_back(&mut self, from: usize) {
        for id in 0..self.chunks.len() {
            let chunk = &mut self.chunks[id];
            let remaining = chunk.unreceived();
            if !matches!(chunk.status, ChunkStatus::Assigned { worker_id, .. } if worker_id >= from) || remaining < 2 {
                continue;
            }
            let end = chunk.end_offset.load(Ordering::SeqCst);
            let split = end + 2 - remaining;
            chunk.end_offset.store(split - 1, Ordering::SeqCst);
            chunk.range.end = split - 1;
            let rest = Chunk::new(self.chunks.len(), ByteRange { start: split, end });
            self.chunks.push(rest);
        }
    }

    /// Cuts what no worker has (chunks waiting for a first or another attempt) into about
    /// `pieces` requests, for a host that limits how often it is asked: one request per chunk would
    /// cost it far more than the connections kept need. Each run of such chunks side by side
    /// becomes one request, or several of equal size while that gives fewer than `pieces` in all.
    /// Chunks in flight or done, and what is on disk, are left as they are. When one of them waits
    /// to be retried (a refusal's Retry-After, which the host rests anyway), every piece waits as
    /// long as the longest such wait and keeps the most retries any of them had. The pieces get
    /// the lowest ids of those chunks and are retried in the file's order, so its start is asked
    /// for first; the other chunks are merged.
    pub(crate) fn merge_pending(&mut self, pieces: usize) {
        let mut pending: Vec<usize> =
            self.chunks.iter().filter(|c| matches!(c.status, ChunkStatus::Unassigned | ChunkStatus::Failed { .. })).map(|c| c.id).collect();
        pending.sort_by_key(|&id| self.chunks[id].range.start);
        let mut runs: Vec<Vec<usize>> = Vec::new();
        for &id in &pending {
            match runs.last_mut() {
                Some(run) if self.chunks[run[run.len() - 1]].range.end + 1 == self.chunks[id].range.start => run.push(id),
                _ => runs.push(vec![id]),
            }
        }
        let left: u64 = pending.iter().map(|&id| self.chunks[id].range.len()).sum();
        let share = left.div_ceil(pieces.max(1) as u64).max(1);
        // Each run's first byte, length and pieces.
        let cuts: Vec<_> = runs
            .iter()
            .map(|run| {
                let start = self.chunks[run[0]].range.start;
                let len = self.chunks[run[run.len() - 1]].range.end - start + 1;
                (start, len, len.div_ceil(share).clamp(1, run.len() as u64))
            })
            .collect();
        if cuts.iter().map(|cut| cut.2).sum::<u64>() == pending.len() as u64 {
            return;
        }
        let not_before = pending.iter().filter_map(|&id| self.chunks[id].not_before).max();
        let retries = pending.iter().map(|&id| self.chunks[id].retries).max().unwrap_or(0);
        let failed = pending.iter().find_map(|&id| match &self.chunks[id].status {
            ChunkStatus::Failed { reason, .. } => Some(reason.clone()),
            _ => None,
        });
        let mut ids = pending.clone();
        ids.sort_unstable();
        let mut ids = ids.into_iter();
        self.retry_queue.retain(|id| !pending.contains(id));
        for (start, len, n64) in cuts {
            for i in 0..n64 {
                let id = ids.next().expect("a run has no more pieces than chunks");
                let mut piece = Chunk::new(id, ByteRange { start: start + len * i / n64, end: start + len * (i + 1) / n64 - 1 });
                piece.retries = retries;
                match &failed {
                    Some(reason) => {
                        piece.status = ChunkStatus::Failed { reason: reason.clone(), retries };
                        piece.not_before = not_before;
                        self.retry_queue.push(id);
                    }
                    None => self.next_fresh = self.next_fresh.min(id),
                }
                self.chunks[id] = piece;
            }
        }
        for id in ids {
            self.chunks[id].status = ChunkStatus::Merged;
            self.completed += 1;
        }
    }

    /// Marks a chunk as completed.
    pub fn mark_completed(&mut self, chunk_id: usize) -> Result<(), ChunkError> {
        let chunk = self.get_chunk_mut(chunk_id)?;
        if !chunk.is_completed() {
            chunk.status = ChunkStatus::Completed;
            self.completed += 1;
        }
        Ok(())
    }

    /// Records a failed attempt. The prefix the worker already wrote becomes a new completed
    /// chunk and the retry covers only the rest. An attempt that wrote something resets the
    /// retry budget; one that wrote nothing counts against it only if `counts` is set, and then
    /// also backs off exponentially. The chunk is retried no sooner than `min_delay`.
    pub fn mark_failed(
        &mut self,
        chunk_id: usize,
        reason: &str,
        min_delay: Duration,
        counts: bool,
    ) -> Result<(), ChunkError> {
        let chunk = self.get_chunk_mut(chunk_id)?;
        if chunk.is_completed() {
            return Ok(());
        }
        let pos = chunk
            .current_offset
            .load(Ordering::SeqCst)
            .clamp(chunk.range.start, chunk.range.end.saturating_add(1));
        if pos > chunk.range.end {
            return self.mark_completed(chunk_id);
        }

        let made_progress = pos > chunk.range.start;
        if made_progress {
            let prefix = ByteRange::new(chunk.range.start, pos - 1)?;
            chunk.range.start = pos;
            chunk.retries = 0;
            let prefix_id = self.chunks.len();
            let mut done = Chunk::new(prefix_id, prefix);
            done.status = ChunkStatus::Completed;
            self.chunks.push(done);
            self.completed += 1;
        }

        let chunk = &mut self.chunks[chunk_id];
        let mut delay = min_delay;
        if counts && !made_progress {
            chunk.retries += 1;
            delay = delay.max(backoff_delay(chunk.retries));
        }
        chunk.not_before = Some(Instant::now() + delay);
        chunk.status = ChunkStatus::Failed {
            reason: reason.to_string(),
            retries: chunk.retries,
        };
        if chunk.retries > self.max_retries && self.fatal.is_none() {
            let message = format!(
                "chunk {} failed {} times in a row without progress: {}",
                chunk_id, chunk.retries, reason
            );
            self.fatal = Some((chunk_id, message));
        }
        self.retry_queue.push(chunk_id);
        Ok(())
    }

    /// Fails the whole download because of `chunk_id` (e.g. the remote file changed).
    pub fn abort(&mut self, chunk_id: usize, reason: &str) {
        if self.fatal.is_none() {
            self.fatal = Some((chunk_id, reason.to_string()));
        }
    }

    /// The chunk that ended the download and why: exhausted retries or `abort`.
    pub fn has_fatal_failure(&self) -> Option<(usize, String)> {
        self.fatal.clone()
    }

    /// Byte ranges on disk, merged: completed chunks plus the written prefix of in-flight ones.
    pub fn completed_ranges(&self) -> Vec<ByteRange> {
        crate::range::merge_ranges(self.chunks.iter().filter_map(Chunk::written_prefix).collect())
    }

    /// Returns a vector of snapshots for all current chunks for UI rendering.
    pub fn chunk_snapshots(&self) -> Vec<ChunkSnapshot> {
        self.chunks
            .iter()
            .filter_map(|c| {
                let (status_str, worker_id) = match &c.status {
                    ChunkStatus::Unassigned => ("Pending".to_string(), None),
                    ChunkStatus::Assigned { worker_id, .. } => (format!("Worker {}", worker_id), Some(*worker_id)),
                    ChunkStatus::Completed => ("Completed".to_string(), None),
                    ChunkStatus::Failed { reason, .. } => (format!("Failed: {}", reason), None),
                    ChunkStatus::Merged => return None,
                };
                Some(ChunkSnapshot {
                    id: c.id,
                    range_start: c.range.start,
                    range_end: c.range.end,
                    downloaded_bytes: c.done_bytes(),
                    total_bytes: c.range.len(),
                    status: status_str,
                    worker_id,
                })
            })
            .collect()
    }

    fn get_chunk_mut(&mut self, chunk_id: usize) -> Result<&mut Chunk, ChunkError> {
        self.chunks.get_mut(chunk_id).ok_or(ChunkError::NotFound(chunk_id))
    }

    /// Moves a chunk's assignment, its answer and its worker's wait `by` into the past, as if its
    /// worker had been at it that long.
    #[cfg(test)]
    pub(crate) fn backdate(&mut self, chunk_id: usize, by: Duration) {
        let chunk = &mut self.chunks[chunk_id];
        chunk.assigned_at = chunk.assigned_at.and_then(|t| t.checked_sub(by));
        let mut attempt = chunk.attempt.lock();
        attempt.answered = attempt.answered.and_then(|(at, from)| Some((at.checked_sub(by)?, from)));
        attempt.waiting = attempt.waiting.and_then(|t| t.checked_sub(by));
    }
}

/// Exponential backoff with equal jitter: `[d/2, d]` for `d = base * 2^(attempt-1)`, capped.
pub(crate) fn backoff_delay(attempt: u32) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let full = BACKOFF_BASE
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(16))
        .min(BACKOFF_CAP);
    // RandomState is freshly keyed per call, which is plenty for jitter.
    let random = std::collections::hash_map::RandomState::new().build_hasher().finish();
    let fraction = (random >> 11) as f64 / (1u64 << 53) as f64;
    full / 2 + (full / 2).mul_f64(fraction)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn test_chunk_partitioning() {
        let manager = ChunkManager::new(10 * MB, 2 * MB).unwrap();
        assert_eq!(manager.chunks().len(), 5);
        assert_eq!(manager.chunks()[0].range, ByteRange::new(0, 2 * MB - 1).unwrap());
        assert_eq!(manager.chunks()[4].range, ByteRange::new(8 * MB, 10 * MB - 1).unwrap());
        assert!(ChunkManager::new(0, MB).unwrap().is_all_completed());
    }

    #[test]
    fn test_work_stealing() {
        let mut manager = ChunkManager::new(10 * MB, 10 * MB).unwrap();
        assert_eq!(manager.chunks().len(), 1);

        // Worker 0 takes the only chunk and writes 2MB of it.
        let work = answering(&mut manager, 0, 0);
        assert_eq!(work.id, 0);
        work.current_offset.store(2 * MB, Ordering::SeqCst);

        // Worker 1 steals half of what is left: split at 2MB + 8MB / 2 = 6MB.
        let (victim_id, stolen) = manager.steal_work(1, 1, MB).unwrap();
        assert_eq!(victim_id, 0);
        assert_eq!(manager.chunks()[0].range.end, 6 * MB - 1);
        assert_eq!(work.end_offset.load(Ordering::SeqCst), 6 * MB - 1, "worker sees the truncation");
        assert_eq!(stolen.id, 1);
        assert_eq!(stolen.range, ByteRange::new(6 * MB, 10 * MB - 1).unwrap());
    }

    #[test]
    fn test_steal_prefers_longest_eta() {
        let mut manager = ChunkManager::new(10 * MB, 6 * MB).unwrap();
        let fast = answering(&mut manager, 0, 0); // [0, 6MB)
        let slow = answering(&mut manager, 1, 0); // [6MB, 10MB)
        fast.current_offset.store(MB, Ordering::SeqCst);
        slow.current_offset.store(6 * MB + 1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(5));
        // Fast has the most bytes left (5MB) but a high rate; slow has 4MB left at 1 byte per 5ms.
        let (victim, _) = manager.steal_work(2, 0, MB).unwrap();
        assert_eq!(victim, 1);

        // One whose request has no answer yet (stuck before headers, say) is not: the thief's would
        // wait as long, and the piece go back to it (see `run_on`). Once answered, it is.
        let mut manager = ChunkManager::new(4 * MB, 4 * MB).unwrap();
        let waiting = manager.get_next_work(0, 0).unwrap();
        assert!(manager.steal_work(1, 0, MB).is_none());
        waiting.answered(Instant::now());
        assert!(manager.steal_work(1, 0, MB).is_some());
    }

    /// The next chunk for `worker_id` on mirror `mirror_id`, whose answer has just come.
    fn answering(manager: &mut ChunkManager, worker_id: usize, mirror_id: usize) -> Chunk {
        let chunk = manager.get_next_work(worker_id, mirror_id).unwrap();
        chunk.answered(Instant::now());
        chunk
    }

    /// A manager with one chunk of `size` whose worker wrote `written` bytes over the `secs` since
    /// its answer came.
    fn one_victim(size: u64, written: u64, secs: u64) -> ChunkManager {
        let mut manager = ChunkManager::new(size, size).unwrap();
        answering(&mut manager, 0, 0).current_offset.store(written, Ordering::SeqCst);
        manager.backdate(0, Duration::from_secs(secs));
        manager
    }

    /// What a request to a mirror costs: `startup_secs` before its first byte, then `rate`.
    fn cost(startup_secs: f64, rate: f64) -> StealTiming {
        StealTiming { startup: Duration::from_secs_f64(startup_secs), rate }
    }

    fn timed(min_bytes: u64, mirrors: &[StealTiming]) -> StealRule<'_> {
        StealRule { min_bytes, timing: Some(mirrors) }
    }

    #[test]
    fn test_timed_steal_splits_where_both_sides_finish_together() {
        // 90 MB left at 1 MB/s; the thief starts after 4 s, then moves 3 MB/s. Both finish after
        // 25.5 s: the victim with 25.5 MB, the thief with 64.5 MB in 4 + 21.5 s.
        let mut manager = one_victim(100 * MB, 10 * MB, 10);
        let (victim, stolen) = manager.steal_work(1, 0, timed(64 * 1024, &[cost(4.0, 3.0 * MB as f64)])).unwrap();
        let split = 10 * MB + (25.5 * MB as f64) as u64;
        assert_eq!(victim, 0);
        assert!(stolen.range.start.abs_diff(split) < MB, "split at {} instead of about {split}", stolen.range.start);
        assert_eq!(stolen.range.end, 100 * MB - 1);
        assert_eq!(manager.chunks()[0].end_offset.load(Ordering::SeqCst), stolen.range.start - 1);
    }

    #[test]
    fn test_a_steal_whose_fair_share_is_under_the_floor_is_not_made() {
        // A thief far slower than the victim would finish its share after the victim anyway:
        // topping it up to the floor would only make the download end later.
        let mut manager = one_victim(100 * MB, 10 * MB, 10);
        assert!(manager.steal_work(1, 0, timed(MB, &[cost(0.1, 1000.0)])).is_none());
        assert_eq!(manager.chunks()[0].end_offset.load(Ordering::SeqCst), 100 * MB - 1, "the victim keeps it all");

        // 9 MB left at 1 MB/s, a thief as fast after 0.5 s: its fair share is 4.25 MB, so a
        // user's 8 MB floor steals nothing rather than hand a new connection 8 MB.
        let mut manager = one_victim(19 * MB, 10 * MB, 10);
        let even = [cost(0.5, MB as f64)];
        assert!(manager.steal_work(1, 0, timed(8 * MB, &even)).is_none());
        let (_, stolen) = manager.steal_work(1, 0, timed(4 * MB, &even)).unwrap();
        assert!(stolen.range.len().abs_diff(17 * MB / 4) < 64 * 1024, "{}", stolen.range.len());
    }

    #[test]
    fn test_timed_steal_leaves_a_chunk_that_ends_within_two_startups() {
        // 5 MB left at 1 MB/s: 5 s.
        let mut manager = one_victim(15 * MB, 10 * MB, 10);
        assert!(manager.steal_work(1, 0, timed(64 * 1024, &[cost(3.0, MB as f64)])).is_none(), "5 s is under two 3 s startups");
        assert!(manager.steal_work(1, 0, timed(64 * 1024, &[cost(2.0, MB as f64)])).is_some(), "5 s is over two 2 s startups");
    }

    #[test]
    fn test_a_chunk_without_a_rate_is_timed_by_what_a_request_to_its_mirror_costs() {
        // Assigned to slow mirror 1 and answered, nothing in yet: it is expected to go on after
        // 0.5 s at 0.5 MB/s. A thief on mirror 0 starts after 0.1 s at 2 MB/s. Both finish after
        // 1.78 s: the victim with 0.64 MB, the thief with 3.36 MB.
        let mut manager = ChunkManager::new(4 * MB, 4 * MB).unwrap();
        answering(&mut manager, 0, 1);
        let mirrors = [cost(0.1, 2.0 * MB as f64), cost(0.5, 0.5 * MB as f64)];
        let (_, stolen) = manager.steal_work(1, 0, timed(64 * 1024, &mirrors)).unwrap();
        let split = (0.64 * MB as f64) as u64;
        assert!(stolen.range.start.abs_diff(split) < 64 * 1024, "split at {} instead of about {split}", stolen.range.start);
    }

    #[test]
    fn test_a_chunk_is_timed_from_its_answer_not_its_request() {
        // The answer took 2 s to come, then brought 0.5 MB in 0.1 s: the worker goes on at 5 MB/s,
        // not the 0.24 MB/s the time since its request would make of it.
        let mut manager = ChunkManager::new(100 * MB, 100 * MB).unwrap();
        let chunk = manager.get_next_work(0, 0).unwrap();
        manager.backdate(0, Duration::from_secs(2));
        chunk.answered(Instant::now());
        let mirror = [cost(2.0, 5.0 * MB as f64)];
        let pace = |manager: &ChunkManager| manager.chunks()[0].pace(Instant::now(), Some(&mirror[0]));
        assert_eq!(pace(&manager), Some((0.0, 5.0 * MB as f64)), "nothing on disk yet: the mirror's rate");
        chunk.current_offset.store(MB / 2, Ordering::SeqCst);
        manager.backdate(0, Duration::from_millis(100));
        let (_, rate) = pace(&manager).unwrap();
        assert!((rate / (5.0 * MB as f64) - 1.0).abs() < 0.1, "{rate}");

        // A thief as fast after its own 2 s: both finish together once the victim keeps 5 MB more
        // than half of the 99.5 MB left, rather than a sliver it would end long before the thief.
        let (_, stolen) = manager.steal_work(1, 0, timed(64 * 1024, &mirror)).unwrap();
        let fair = (99.5 - 10.0) / 2.0 * MB as f64;
        assert!((stolen.range.len() as f64 - fair).abs() < 2.0 * MB as f64, "stole {} of 99.5 MB", stolen.range.len());
    }

    #[test]
    fn test_thieves_in_a_row_split_the_straggler_not_each_others_fresh_chunks() {
        // 18 MiB left at 2 MiB/s: 9 s. Seven idle workers arrive one after the other, each a new
        // request that starts after 0.3 s and then moves 2 MiB/s, as the straggler does.
        let mut manager = one_victim(20 * MB, 2 * MB, 1);
        let mirror = [cost(0.3, 2.0 * MB as f64)];
        // Each thief's request is answered at once, nothing in yet.
        let victims: Vec<usize> = (1..=7)
            .map(|thief| {
                let (victim, stolen) = manager.steal_work(thief, 0, timed(64 * 1024, &mirror)).unwrap();
                stolen.answered(Instant::now());
                victim
            })
            .collect();
        // The second thief finds the straggler, not the first thief's chunk that has yet to start,
        // still the one to finish last.
        assert_eq!(victims[..2], [0, 0], "{victims:?}");

        // All eight pieces finish together, as soon as eight connections can: after about 1.39 s.
        let now = Instant::now();
        let finish: Vec<f64> =
            manager.chunks().iter().map(|c| eta_secs(c.remaining_bytes(), c.pace(now, Some(&mirror[0])))).collect();
        assert_eq!(finish.len(), 8);
        assert!(finish.iter().all(|&secs| (1.3..1.5).contains(&secs)), "{finish:?}");
    }

    #[test]
    fn test_bytes_received_but_not_yet_written_are_the_victims() {
        let mirror = [cost(0.3, 2.0 * MB as f64)];
        // 2 MiB/s, and all but half a MiB received, if not yet written: done in 0.25 s, sooner
        // than a thief could start twice.
        let mut manager = one_victim(20 * MB, 2 * MB, 1);
        manager.chunks()[0].received(20 * MB - MB / 2);
        assert!(manager.steal_work(1, 0, timed(64 * 1024, &mirror)).is_none());
        // 4 MiB still to come: the split is timed on those, after what the victim holds.
        manager.chunks()[0].received(16 * MB);
        let (_, stolen) = manager.steal_work(1, 0, timed(64 * 1024, &mirror)).unwrap();
        assert!(stolen.range.start > 18 * MB, "{:?}", stolen.range);
    }

    #[test]
    fn test_a_connection_far_slower_than_the_others_on_its_mirror_makes_way() {
        let mut manager = ChunkManager::new(100 * MB, 20 * MB).unwrap();
        let [slow, a, b, c, elsewhere] = [0, 0, 0, 0, 1].map(|mirror| manager.get_next_work(0, mirror).unwrap());
        let t0 = Instant::now();
        for peer in [&a, &b, &elsewhere] {
            peer.went_at(2e6);
        }
        // Two others on its mirror are too few to judge by, whatever another mirror does.
        assert!(!manager.replaces(slow.id, 0, 1e5, t0));
        // A third, yet to finish a window, goes by its rate since its answer: 4 MiB in 2 s.
        c.answered(t0);
        c.current_offset.store(c.range.start + 4 * MB, Ordering::SeqCst);
        manager.backdate(c.id, Duration::from_secs(2));
        assert!(!manager.replaces(slow.id, 0, 0.7e6, t0), "over 0.3 of their median");
        assert!(manager.replaces(slow.id, 0, 0.5e6, t0));
        assert!(!manager.replaces(slow.id, 0, 1e5, t0 + LAG_WINDOW / 2), "once per window");
        // Less left than the median brings in a window (10 MB): stealing sees to that rest.
        slow.received(slow.range.end + 1 - 5 * MB);
        assert!(!manager.replaces(slow.id, 0, 1e5, t0 + LAG_WINDOW));
        slow.received(slow.range.start);
        assert!(manager.replaces(slow.id, 0, 1e5, t0 + LAG_WINDOW));
    }

    /// With too few connections for a median, one that falls under 0.3 of its own fastest window
    /// and of every other's is slowing down before a stall, and makes way; one whose share fell as
    /// connections joined it, or all of them slowing together, does not.
    #[test]
    fn test_a_connection_slowing_down_before_a_stall_makes_way() {
        let mut manager = ChunkManager::new(100 * MB, 20 * MB).unwrap();
        let [decaying, other] = [0, 0].map(|mirror| manager.get_next_work(0, mirror).unwrap());
        let t0 = Instant::now();
        decaying.went_at(2e6);
        other.went_at(2e6);
        assert!(!manager.replaces(decaying.id, 0, 1.2e6, t0), "shared with one more");
        other.went_at(0.5e6);
        assert!(!manager.replaces(decaying.id, 0, 0.5e6, t0), "both slowed down: the line or the server");
        other.went_at(2e6);
        assert!(manager.replaces(decaying.id, 0, 0.5e6, t0));
        // Alone too, once a window went faster.
        let mut alone = ChunkManager::new(100 * MB, 50 * MB).unwrap();
        let only = alone.get_next_work(0, 0).unwrap();
        assert!(!alone.replaces(only.id, 0, 1e4, t0), "no window to go by yet");
        only.went_at(2e6);
        assert!(alone.replaces(only.id, 0, 0.5e6, t0));
    }

    /// Cut connections hand back what they have yet to receive, but a byte each, as fresh work;
    /// those below the cut keep theirs.
    #[test]
    fn test_cut_connections_hand_their_rest_back() {
        let mut manager = ChunkManager::new(3 * MB, MB).unwrap();
        let [kept, cut, received] = [0, 1, 2].map(|worker| manager.get_next_work(worker, 0).unwrap());
        received.received(MB * 2 + 1000);
        assert_eq!((manager.total_received(), manager.total_downloaded()), (1000, 0), "held, not yet written");
        manager.give_back(1);
        let ends: Vec<u64> = [&kept, &cut, &received].iter().map(|c| c.end_offset.load(Ordering::SeqCst)).collect();
        assert_eq!(ends, [MB - 1, MB, 2 * MB + 1000]);
        let rests: Vec<ByteRange> = (0..2).map(|_| manager.get_next_work(0, 0).unwrap().range).collect();
        assert_eq!(rests, [ByteRange::new(MB + 1, 2 * MB - 1).unwrap(), ByteRange::new(2 * MB + 1001, 3 * MB - 1).unwrap()]);
        assert!(manager.get_next_work(0, 0).is_none());
    }

    /// A step gives each request under way one newcomer, for the far half of what it has yet to
    /// receive, those with the most left first; pieces no worker has go first. Never a half under
    /// the least a connection is to carry.
    #[test]
    fn test_a_widening_halves_the_requests_under_way() {
        let mut manager = ChunkManager::new(10 * MB, 10 * MB).unwrap();
        let stream = manager.get_next_work(0, 0).unwrap();
        stream.received(MB);
        assert_eq!(manager.halve(1, 64 * 1024, Duration::ZERO), 1);
        assert_eq!(stream.end_offset.load(Ordering::SeqCst), MB + 9 * MB / 2 - 1);
        let far = manager.get_next_work(1, 0).unwrap();
        assert_eq!(far.range, ByteRange::new(MB + 9 * MB / 2, 10 * MB - 1).unwrap());
        far.received(far.range.start + MB);
        // The stream has 4.5 MB left, the far piece 3.5 MB: two newcomers at most, whatever is asked.
        assert_eq!(manager.halve(6, 64 * 1024, Duration::ZERO), 2);
        let halves: Vec<ByteRange> = (2..4).map(|worker| manager.get_next_work(worker, 0).unwrap().range).collect();
        let (stream_cut, far_cut) = (MB + 9 * MB / 4, far.range.start + MB + 7 * MB / 4);
        assert_eq!(halves, [ByteRange::new(stream_cut, MB + 9 * MB / 2 - 1).unwrap(), ByteRange::new(far_cut, 10 * MB - 1).unwrap()]);
        assert_eq!(stream.end_offset.load(Ordering::SeqCst), stream_cut - 1);
        // A piece no worker has is work already: it is handed out before anything is cut.
        manager.mark_failed(3, "cut", Duration::ZERO, false).unwrap();
        assert_eq!(manager.halve(1, 64 * 1024, Duration::ZERO), 1);
        assert_eq!(manager.chunks().len(), 4, "nothing was cut");
        // Halves of 4 MB at least: of the stream's 9 MB, one.
        let mut manager = ChunkManager::new(10 * MB, 10 * MB).unwrap();
        let stream = manager.get_next_work(0, 0).unwrap();
        stream.received(MB);
        assert_eq!(manager.halve(1, 4 * MB, Duration::ZERO), 1);
        manager.get_next_work(1, 0).unwrap();
        assert_eq!(manager.halve(1, 4 * MB, Duration::ZERO), 0, "2.25 MB halves do not pay");
        // At 1 MB/s, a newcomer 2 s to answer: its half is short by the 2 MB the stream brings
        // meanwhile, so both end together.
        let mut manager = ChunkManager::new(10 * MB, 10 * MB).unwrap();
        let stream = manager.get_next_work(0, 0).unwrap();
        stream.answered(Instant::now() - Duration::from_secs(1));
        stream.current_offset.store(MB, Ordering::SeqCst);
        assert_eq!(manager.halve(1, 64 * 1024, Duration::from_secs(2)), 1);
        // About: its pace is a hair under 1 MB/s by the time it is read.
        let end = stream.end_offset.load(Ordering::SeqCst);
        assert!(end.abs_diff(MB + 11 * MB / 2 - 1) < 16 * 1024, "{end}: the stream's 5.5 MB take as long as 2 s and 3.5 MB");
        // Then at 4.5 MB/s, the stream with 2 MB left and the newcomer with 3.5 MB: what either
        // has left goes in less than another's 2 s to answer, nothing to give it.
        manager.get_next_work(1, 0).unwrap();
        stream.current_offset.store(MB + 7 * MB / 2, Ordering::SeqCst);
        assert_eq!(manager.halve(2, 64 * 1024, Duration::from_secs(2)), 0);
    }

    /// Once its host refuses a request, the first stream takes back the pieces right after it that
    /// no worker has, a refused one too, and its request brings them; a piece a worker has, and
    /// what lies past it, stay. Only those it reaches within the host's rest, at its pace. Not a
    /// stream retried over a new request, whose answer ends where it asked.
    #[test]
    fn test_the_first_stream_takes_back_what_a_refusing_host_left() {
        let at = |i: u64| MB + 9 * MB * i / 4;
        let refused = |wait: Duration| {
            let mut manager = ChunkManager::new(10 * MB, 10 * MB).unwrap();
            let stream = manager.get_next_work(0, 0).unwrap();
            // 1 MB in its first second.
            stream.answered(Instant::now() - Duration::from_secs(1));
            stream.current_offset.store(MB, Ordering::SeqCst);
            manager.halve(1, 64 * 1024, Duration::ZERO);
            let half = manager.get_next_work(1, 0).unwrap();
            half.answered(Instant::now());
            manager.halve(2, 64 * 1024, Duration::ZERO);
            let [refused, far] = [2, 3].map(|worker| manager.get_next_work(worker, 0).unwrap());
            manager.mark_failed(refused.id, "HTTP 429", Duration::from_secs(3), false).unwrap();
            manager.rejoin(wait);
            (manager, stream, [half, far])
        };
        // In a 2 s rest it reaches 3 MB, short of the refused piece after its own.
        let (_, stream, _) = refused(Duration::from_secs(2));
        assert_eq!(stream.end_offset.load(Ordering::SeqCst), at(1) - 1);
        let (mut manager, stream, others) = refused(Duration::from_secs(10));
        assert_eq!(stream.end_offset.load(Ordering::SeqCst), at(2) - 1);
        assert!(manager.get_next_work(4, 0).is_none(), "nothing is left to ask for");
        for id in [stream.id, others[0].id, others[1].id] {
            manager.mark_completed(id).unwrap();
        }
        assert!(manager.is_all_completed());
        assert_eq!(manager.completed_ranges(), [ByteRange::new(0, 10 * MB - 1).unwrap()]);

        let mut manager = ChunkManager::new(10 * MB, 10 * MB).unwrap();
        let first = manager.get_next_work(0, 0).unwrap();
        first.received(MB);
        manager.halve(1, 64 * 1024, Duration::ZERO);
        first.current_offset.store(MB, Ordering::SeqCst);
        manager.mark_failed(first.id, "cut", Duration::ZERO, true).unwrap();
        let retried = manager.get_next_work(0, 0).unwrap();
        assert_eq!(retried.id, first.id);
        manager.rejoin(Duration::from_secs(10));
        assert_eq!(retried.end_offset.load(Ordering::SeqCst), at(2) - 1);
    }

    /// A piece cut off a request under way and refused goes back to that request, which asked for
    /// it already: at once when a refusal ended a widening, else only where the request reaches it
    /// at its pace within the host's rest.
    #[test]
    fn test_a_refused_piece_goes_back_to_the_request_that_asked_for_it() {
        let cut = || {
            let mut manager = ChunkManager::new(10 * MB, 10 * MB).unwrap();
            let stream = manager.get_next_work(0, 0).unwrap();
            stream.asked(10 * MB - 1);
            // 1 MB in its first second.
            stream.answered(Instant::now() - Duration::from_secs(1));
            stream.current_offset.store(MB, Ordering::SeqCst);
            let (_, stolen) = manager.steal_work(1, 1, MB).unwrap();
            manager.mark_failed(stolen.id, "HTTP 429", Duration::from_secs(3), false).unwrap();
            (manager, stream)
        };
        let (mut manager, stream) = cut();
        manager.strand_nothing(Some(Duration::from_secs(3)));
        assert_eq!(stream.end_offset.load(Ordering::SeqCst), MB + 9 * MB / 2 - 1, "out of reach in 3 s");
        manager.strand_nothing(Some(Duration::from_secs(10)));
        assert_eq!(stream.end_offset.load(Ordering::SeqCst), 10 * MB - 1);
        assert!(manager.get_next_work(2, 0).is_none(), "nothing is left to ask for");
        let (mut manager, stream) = cut();
        manager.strand_nothing(None);
        assert_eq!(stream.end_offset.load(Ordering::SeqCst), 10 * MB - 1);
        manager.mark_completed(stream.id).unwrap();
        assert!(manager.is_all_completed());
    }

    /// A piece cut off a request under way changes hands when the newcomer's answer comes: the
    /// request at its end goes on over it while it has none, the newcomer's then dropped (its
    /// answer thrown away), or once it was refused; not once it was answered, nor past what the
    /// request asked for, nor over a piece no worker has taken yet unless the download widens.
    #[test]
    fn test_a_piece_changes_hands_when_its_request_is_answered() {
        let cut = || {
            let mut manager = ChunkManager::new(10 * MB, 10 * MB).unwrap();
            let stream = manager.get_next_work(0, 0).unwrap();
            stream.asked(10 * MB - 1);
            stream.answered(Instant::now());
            manager.halve(1, 64 * 1024, Duration::ZERO);
            let newcomer = manager.get_next_work(1, 0).unwrap();
            newcomer.asked(10 * MB - 1);
            (manager, stream, newcomer)
        };
        // No answer yet: the stream takes the piece back, and the newcomer's answer is thrown away.
        let (mut manager, stream, newcomer) = cut();
        assert!(manager.run_on(&stream, false));
        assert_eq!(stream.end_offset.load(Ordering::SeqCst), 10 * MB - 1);
        assert!(newcomer.revoked.is_cancelled() && !newcomer.answered(Instant::now()));
        manager.mark_completed(stream.id).unwrap();
        assert!(manager.is_all_completed());
        // Answered first: the piece is the newcomer's.
        let (mut manager, stream, newcomer) = cut();
        assert!(newcomer.answered(Instant::now()));
        assert!(!manager.run_on(&stream, true));
        assert_eq!(stream.end_offset.load(Ordering::SeqCst), 5 * MB - 1);
        // Refused: the piece waits out the rest unless the stream that asked for it reaches it.
        let (mut manager, stream, newcomer) = cut();
        manager.mark_failed(newcomer.id, "HTTP 429", Duration::from_secs(6), false).unwrap();
        assert!(manager.run_on(&stream, false));
        assert!(manager.get_next_work(2, 0).is_none(), "nothing is left to ask for");
        // A request that asked for less does not take it.
        let (mut manager, stream, _) = cut();
        stream.asked(5 * MB - 1);
        assert!(!manager.run_on(&stream, true));
        // One no worker has taken yet waits its turn, unless the download widens.
        let mut manager = ChunkManager::new(10 * MB, 10 * MB).unwrap();
        let stream = manager.get_next_work(0, 0).unwrap();
        stream.asked(10 * MB - 1);
        manager.halve(1, 64 * 1024, Duration::ZERO);
        assert!(!manager.run_on(&stream, false) && manager.run_on(&stream, true));
    }

    /// A worker whose attempt was handed over (here its request asked for again, and the second
    /// answered first) runs on over nothing: the chunk is the new owner's, as `rejoin` holds.
    #[test]
    fn test_a_worker_whose_attempt_was_handed_over_runs_on_over_nothing() {
        let mut manager = ChunkManager::new(10 * MB, 5 * MB).unwrap();
        let old = manager.get_next_work(0, 0).unwrap();
        old.asked(10 * MB - 1);
        let hedge = manager.hedge(|_| Duration::ZERO, |_, _| true).unwrap();
        let thief = manager.claim(&hedge, 1, 0).unwrap();
        thief.asked(10 * MB - 1);
        assert!(!manager.run_on(&old, true));
        assert_eq!(manager.chunks()[1].status, ChunkStatus::Unassigned, "the next piece is nobody's yet");
        assert!(manager.run_on(&thief, true), "the new owner's request goes on over it");
    }

    /// A piece taken back from a newcomer with no answer (after 1 s) is a measure: a new request
    /// takes at least twice that. The next cut leaves the stream what it brings meanwhile, and no
    /// steal splits a request done within twice that, nor one whose request has no answer.
    #[test]
    fn test_a_take_back_tells_how_long_a_new_request_takes() {
        let mut manager = ChunkManager::new(20 * MB, 20 * MB).unwrap();
        let stream = manager.get_next_work(0, 0).unwrap();
        stream.asked(20 * MB - 1);
        stream.answered(Instant::now());
        manager.halve(1, 64 * 1024, Duration::ZERO);
        let newcomer = manager.get_next_work(1, 0).unwrap();
        manager.backdate(newcomer.id, Duration::from_secs(1));
        // 1 MB/s: 10 MB in its first 10 s, at the piece before the newcomer's answer.
        manager.backdate(stream.id, Duration::from_secs(10));
        stream.current_offset.store(10 * MB, Ordering::SeqCst);
        assert!(manager.run_on(&stream, false));
        assert!(newcomer.revoked.is_cancelled());
        assert!(manager.setup >= Duration::from_secs(2), "{:?}", manager.setup);
        // 10 MB left: the newcomer's half is short by the 2 MB the stream brings meanwhile.
        assert_eq!(manager.halve(1, 64 * 1024, Duration::ZERO), 1);
        let second = manager.get_next_work(2, 0).unwrap();
        let given = second.range.len();
        assert!((3_900_000..=4 * MB).contains(&given), "{given}");
        // 3 MB left at 1.3 MB/s, done within twice 2 s: not split, nor the newcomer with no answer.
        stream.current_offset.store(13 * MB, Ordering::SeqCst);
        assert!(manager.steal_work(3, 0, 64 * 1024).is_none());
    }

    /// However splits, steals, take-overs (of silent and of slow chunks), second requests, merges,
    /// hand-backs, answers, refusals, take-backs, pieces on trial and verdicts on them, finishes
    /// and failures follow one another, the chunks tile the file (no gap, no overlap), every piece
    /// not done is under a request under way that asked for it or waits to be asked for, and the
    /// file completes.
    #[test]
    fn test_the_chunks_tile_the_file_whatever_comes_in_whatever_order() {
        let size = 4 * MB;
        for seed in 1..=3000u64 {
            let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let mut roll = move |n: usize| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % n as u64) as usize
            };
            let mut manager = ChunkManager::new(size, size).unwrap();
            // The attempts under way, as their workers hold them, and the next worker's id.
            let (mut live, mut workers): (Vec<Chunk>, usize) = (Vec::new(), 0);
            let ask = |chunk: Chunk, live: &mut Vec<Chunk>| {
                chunk.asked(chunk.end_offset.load(Ordering::SeqCst));
                live.push(chunk);
            };
            let take = |manager: &mut ChunkManager, live: &mut Vec<Chunk>, workers: &mut usize| {
                *workers += 1;
                if let Some(chunk) = manager.get_next_work(*workers, 0) {
                    ask(chunk, live);
                }
            };
            take(&mut manager, &mut live, &mut workers);
            for step in 0..60 {
                // A worker whose attempt was revoked (its piece taken back, or taken over) stops
                // at once and settles nothing.
                live.retain(|c| !c.revoked.is_cancelled());
                let pick = |roll: &mut dyn FnMut(usize) -> usize, live: &[Chunk], answered: bool| {
                    let some: Vec<usize> = (0..live.len()).filter(|&i| live[i].has_answer() == answered).collect();
                    (!some.is_empty()).then(|| some[roll(some.len())])
                };
                match roll(15) {
                    0 => {
                        manager.halve(1 + roll(3), 64 * 1024, Duration::from_millis(roll(3) as u64 * 100));
                    }
                    1 => take(&mut manager, &mut live, &mut workers),
                    2 => {
                        if let Some(i) = pick(&mut roll, &live, false) {
                            if !live[i].answered(Instant::now()) {
                                live.remove(i);
                            }
                        }
                    }
                    3 | 4 => {
                        if let Some(i) = pick(&mut roll, &live, true) {
                            let (id, pos, end) = (live[i].id, live[i].current_offset.load(Ordering::SeqCst), live[i].end_offset.load(Ordering::SeqCst));
                            if pos <= end {
                                let to = pos + 1 + roll((end - pos + 1) as usize) as u64;
                                live[i].received(to);
                                live[i].current_offset.store(to, Ordering::SeqCst);
                                live[i].went_lately(roll(3) as f64);
                            } else if !manager.run_on(&live[i], roll(2) == 0) {
                                manager.mark_completed(id).unwrap();
                                live.remove(i);
                            }
                        }
                    }
                    5 => {
                        if let Some(i) = pick(&mut roll, &live, false) {
                            manager.mark_failed(live.remove(i).id, "HTTP 429", Duration::ZERO, false).unwrap();
                            manager.strand_nothing([None, Some(Duration::from_secs(1))][roll(2)]);
                        }
                    }
                    6 => {
                        if let Some(i) = pick(&mut roll, &live, true) {
                            manager.mark_failed(live.remove(i).id, "cut", Duration::ZERO, false).unwrap();
                        }
                    }
                    7 => {
                        workers += 1;
                        if let Some((_, stolen)) = manager.steal_work(workers, 0, 64 * 1024) {
                            ask(stolen, &mut live);
                        }
                    }
                    8 => {
                        workers += 1;
                        if let Some(taken) = manager.take_over_silent(workers, 0, |_| Duration::ZERO) {
                            ask(taken, &mut live);
                        }
                    }
                    9 => {
                        workers += 1;
                        let hedge = manager.hedge(|_| Duration::ZERO, |_, _| true);
                        if let Some(claimed) = hedge.and_then(|hedge| manager.claim(&hedge, workers, 0)) {
                            ask(claimed, &mut live);
                        }
                    }
                    10 => manager.merge_pending(1 + roll(3)),
                    11 => manager.on_trial(),
                    12 => {
                        manager.verdict(roll(2) == 0);
                        manager.overtaken();
                    }
                    13 => {
                        workers += 1;
                        if let Some(taken) = manager.take_over_slow(workers, 0, &[StealTiming { startup: Duration::ZERO, rate: 1e15 }]) {
                            ask(taken, &mut live);
                        }
                    }
                    _ => manager.give_back(roll(workers + 1)),
                }
                let mut tiles: Vec<ByteRange> = manager.chunks().iter().filter(|c| c.status != ChunkStatus::Merged).map(|c| c.range).collect();
                tiles.sort_by_key(|r| r.start);
                let ends = tiles.iter().try_fold(0, |next, r| (r.start == next).then_some(r.end + 1));
                assert_eq!(ends, Some(size), "seed {seed}, step {step}: {tiles:?}");
                for c in manager.chunks().iter().filter(|c| c.is_in_flight()) {
                    let held = live.iter().any(|l| l.id == c.id && Arc::ptr_eq(&l.attempt, &c.attempt) && !l.revoked.is_cancelled());
                    assert!(held && c.attempt.lock().asked >= c.range.end, "seed {seed}, step {step}: chunk {} has no request that asked for it", c.id);
                }
            }
            // Every attempt goes to its end, and what is left is asked for.
            for _ in 0..1000 {
                if manager.is_all_completed() {
                    break;
                }
                match live.pop() {
                    Some(chunk) if chunk.revoked.is_cancelled() => {}
                    Some(chunk) => {
                        chunk.answered(Instant::now());
                        chunk.current_offset.store(chunk.end_offset.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        if manager.run_on(&chunk, true) {
                            live.push(chunk);
                        } else {
                            manager.mark_completed(chunk.id).unwrap();
                        }
                    }
                    None => take(&mut manager, &mut live, &mut workers),
                }
            }
            assert_eq!(manager.completed_ranges(), [ByteRange::new(0, size - 1).unwrap()], "seed {seed}");
        }
    }

    /// What no worker has is merged into as many requests as the connections kept: with one, the
    /// chunks side by side after a refused one are a single request, which waits out the refusal's
    /// Retry-After; chunks running or done, and what is on disk, stay. A later widening splits the
    /// piece as any running chunk, and the pieces finish the file. With two kept, a fresh file
    /// is two halves.
    #[test]
    fn test_pending_chunks_merge_into_as_many_requests_as_connections_kept() {
        let mut manager = ChunkManager::new(10 * MB, MB).unwrap();
        let [running, done, later, refused] = [0, 1, 2, 3].map(|worker| manager.get_next_work(worker, 0).unwrap());
        manager.mark_completed(done.id).unwrap();
        refused.current_offset.store(3 * MB + 1000, Ordering::SeqCst);
        manager.mark_failed(refused.id, "HTTP 429", Duration::from_millis(100), false).unwrap();
        let on_disk = manager.completed_ranges();
        manager.merge_pending(1);
        assert_eq!(manager.completed_ranges(), on_disk, "what is on disk stays");
        assert_eq!(manager.chunk_snapshots().len(), 5, "two running, one done, what the refused one wrote, and one piece");
        assert!(manager.get_next_work(4, 0).is_none(), "the piece waits out the Retry-After");
        std::thread::sleep(Duration::from_millis(150));
        let rest = manager.get_next_work(4, 0).unwrap();
        assert_eq!((rest.id, rest.range), (refused.id, ByteRange::new(3 * MB + 1000, 10 * MB - 1).unwrap()));
        assert!(manager.get_next_work(5, 0).is_none());
        rest.answered(Instant::now());
        let (victim, stolen) = manager.steal_work(5, 0, MB).unwrap();
        assert_eq!(victim, rest.id);
        for id in [running.id, later.id, rest.id, stolen.id] {
            manager.mark_completed(id).unwrap();
        }
        assert!(manager.is_all_completed());
        assert_eq!(manager.completed_ranges(), [ByteRange::new(0, 10 * MB - 1).unwrap()]);

        let mut fresh = ChunkManager::new(10 * MB, MB).unwrap();
        fresh.merge_pending(2);
        let halves: Vec<Chunk> = (0..2).map(|worker| fresh.get_next_work(worker, 0).unwrap()).collect();
        let ranges: Vec<ByteRange> = halves.iter().map(|c| c.range).collect();
        assert_eq!(ranges, [ByteRange::new(0, 5 * MB - 1).unwrap(), ByteRange::new(5 * MB, 10 * MB - 1).unwrap()]);
        assert!(fresh.get_next_work(2, 0).is_none());
        halves.iter().for_each(|c| fresh.mark_completed(c.id).unwrap());
        assert!(fresh.is_all_completed());

        // Two runs on either side of a running chunk, one refused: both wait out its Retry-After,
        // and go in the file's order.
        let mut split = ChunkManager::new(4 * MB, MB).unwrap();
        let [first, _running] = [0, 1].map(|worker| split.get_next_work(worker, 0).unwrap());
        split.mark_failed(first.id, "HTTP 429", Duration::from_millis(100), false).unwrap();
        split.merge_pending(1);
        assert!(split.get_next_work(2, 0).is_none(), "both wait");
        std::thread::sleep(Duration::from_millis(150));
        let order: Vec<u64> = (2..4).map(|worker| split.get_next_work(worker, 0).unwrap().range.start).collect();
        assert_eq!(order, [0, 2 * MB]);
        assert!(split.get_next_work(4, 0).is_none());
    }

    const SILENT: Duration = Duration::from_secs(2);

    #[test]
    fn test_silent_chunk_is_taken_over_keeping_what_it_wrote() {
        let mut manager = ChunkManager::new(4 * MB, 4 * MB).unwrap();
        let silent = answering(&mut manager, 0, 0);
        silent.current_offset.store(MB, Ordering::SeqCst);
        manager.backdate(0, Duration::from_secs(3));

        let taken = manager.take_over_silent(1, 2, |_| SILENT).unwrap();
        assert!(silent.revoked.is_cancelled() && !taken.revoked.is_cancelled());
        assert_eq!((taken.id, taken.range), (0, ByteRange::new(MB, 4 * MB - 1).unwrap()));
        assert_eq!(taken.status, ChunkStatus::Assigned { worker_id: 1, mirror_id: 2 });
        assert_eq!(taken.current_offset.load(Ordering::SeqCst), MB);
        assert_eq!(manager.completed_ranges(), vec![ByteRange::new(0, MB - 1).unwrap()]);

        // The revoked worker's late progress no longer counts: the rest is the thief's.
        silent.current_offset.store(2 * MB, Ordering::SeqCst);
        assert_eq!(manager.total_downloaded(), MB);
        assert!(manager.take_over_silent(3, 0, |_| SILENT).is_none(), "the thief's attempt has just begun");
        manager.backdate(0, Duration::from_secs(3));
        assert!(manager.take_over_silent(3, 0, |_| SILENT).is_none(), "and waits for its answer");

        taken.current_offset.store(4 * MB, Ordering::SeqCst);
        manager.mark_completed(taken.id).unwrap();
        assert!(manager.is_all_completed());
        assert_eq!(manager.completed_ranges(), vec![ByteRange::new(0, 4 * MB - 1).unwrap()]);
    }

    #[test]
    fn test_silent_chunk_that_wrote_nothing_is_handed_over_whole() {
        // Its answer came, and then not a byte, at the very start of the file.
        let mut manager = ChunkManager::new(2 * MB, MB).unwrap();
        let silent = answering(&mut manager, 0, 0);
        let other = answering(&mut manager, 1, 0);
        manager.backdate(0, Duration::from_secs(3));

        let taken = manager.take_over_silent(2, 0, |_| SILENT).unwrap();
        assert!(silent.revoked.is_cancelled() && !other.revoked.is_cancelled());
        assert_eq!((taken.id, taken.range), (0, ByteRange::new(0, MB - 1).unwrap()));
        assert_eq!(manager.chunks().len(), 2, "nothing was written, so nothing is split off");
        assert!(manager.completed_ranges().is_empty());

        for chunk in [&taken, &other] {
            chunk.current_offset.store(chunk.range.end + 1, Ordering::SeqCst);
            manager.mark_completed(chunk.id).unwrap();
        }
        assert!(manager.is_all_completed());
    }

    #[test]
    fn test_a_chunk_waiting_for_its_answer_is_not_taken_over() {
        // A server that takes long to answer: a new request would wait as long, and be taken
        // over in turn, again and again. The wait for an answer has the stall timeout instead.
        let mut manager = ChunkManager::new(2 * MB, MB).unwrap();
        let waiting = manager.get_next_work(0, 0).unwrap();
        manager.backdate(0, Duration::from_secs(60));
        assert!(manager.take_over_silent(1, 0, |_| SILENT).is_none());
        assert!(!waiting.revoked.is_cancelled());

        // A retry waits for an answer of its own, however long the attempt before it waited for
        // bytes.
        waiting.answered(Instant::now());
        manager.backdate(0, Duration::from_secs(3));
        manager.mark_failed(0, "stalled", Duration::ZERO, false).unwrap();
        let retry = manager.get_next_work(1, 0).unwrap();
        assert_eq!(retry.id, 0);
        manager.backdate(0, Duration::from_secs(3));
        assert!(manager.take_over_silent(2, 0, |_| SILENT).is_none());
        assert!(!retry.revoked.is_cancelled());
    }

    #[test]
    fn test_a_request_without_an_answer_is_asked_for_again_once() {
        let mut manager = ChunkManager::new(8 * MB, 4 * MB).unwrap();
        let waiting = manager.get_next_work(0, 0).unwrap();
        answering(&mut manager, 1, 0);
        let after = |_| Duration::from_secs(3);
        // Not before its time, nor while its mirror has answered nothing since it was sent.
        assert!(manager.hedge(after, |_, _| true).is_none());
        manager.backdate(0, Duration::from_secs(3));
        assert!(manager.hedge(after, |_, _| false).is_none());
        let hedge = manager.hedge(after, |_, _| true).unwrap();
        assert_eq!((hedge.chunk_id, hedge.start, hedge.end), (0, 0, 4 * MB - 1));
        assert!(manager.hedge(after, |_, _| true).is_none(), "once per chunk");
        // The second request answered first: the chunk is its worker's, the first attempt revoked.
        let taken = manager.claim(&hedge, 2, 0).unwrap();
        assert!(waiting.revoked.is_cancelled() && !taken.revoked.is_cancelled());
        assert_eq!(taken.status, ChunkStatus::Assigned { worker_id: 2, mirror_id: 0 });
        assert!(manager.claim(&hedge, 3, 0).is_none(), "taken once");

        // The first answered first: the second is dropped.
        let mut manager = ChunkManager::new(4 * MB, 4 * MB).unwrap();
        let waiting = manager.get_next_work(0, 0).unwrap();
        manager.backdate(0, Duration::from_secs(3));
        let hedge = manager.hedge(after, |_, _| true).unwrap();
        waiting.answered(Instant::now());
        assert!(!manager.hedging(&hedge));
        assert!(manager.claim(&hedge, 1, 0).is_none());
        assert!(!waiting.revoked.is_cancelled());
    }

    #[test]
    fn test_a_connection_slowed_near_its_end_is_timed_by_its_last_window() {
        // 64 MiB less 300 KB in 10 s, but its last window went at 10 KB/s: the 300 KB left take
        // 30 s, not the 0.05 s its average says, and a thief that starts after 0.1 s at 5 MiB/s
        // takes nearly all of them.
        let mut manager = one_victim(64 * MB, 64 * MB - 300_000, 10);
        let thief = [cost(0.1, 5.0 * MB as f64)];
        assert!(manager.steal_work(1, 0, timed(64 * 1024, &thief)).is_none(), "by its average it is about done");
        manager.chunks()[0].went_at(10_000.0);
        let (_, stolen) = manager.steal_work(1, 0, timed(64 * 1024, &thief)).unwrap();
        assert!(stolen.range.len() > 290_000, "{}", stolen.range.len());
        // As by its last half second, before a whole window: one fading out shows there first.
        let mut manager = one_victim(64 * MB, 64 * MB - 300_000, 10);
        manager.chunks()[0].went_lately(10_000.0);
        let (_, stolen) = manager.steal_work(1, 0, timed(64 * 1024, &thief)).unwrap();
        assert!(stolen.range.len() > 290_000, "{}", stolen.range.len());
    }

    /// A tail too short to split, left to a connection that crawls, goes whole to a new request
    /// that brings it well before; one going about as fast as a new request stays, and so does one
    /// whose speed may still climb: its fastest half second passed by none of the two after it.
    #[test]
    fn test_a_crawling_tail_is_taken_over() {
        let thief = [cost(0.1, 5.0 * MB as f64)];
        // 50 KB left: at its 25 KB/s of late they take 2 s, a new request 0.11 s.
        let mut manager = one_victim(64 * MB, 64 * MB - 50_000, 10);
        let crawling = manager.chunks()[0].clone();
        crawling.went_lately(25_000.0);
        assert!(manager.steal_work(1, 0, timed(64 * 1024, &thief)).is_none(), "too short to split");
        crawling.went_lately(25_000.0);
        assert!(manager.take_over_slow(1, 0, &thief).is_none(), "its speed may still climb");
        crawling.went_lately(26_000.0);
        let taken = manager.take_over_slow(1, 0, &thief).unwrap();
        assert!(crawling.revoked.is_cancelled());
        assert_eq!(taken.range, ByteRange::new(64 * MB - 50_000, 64 * MB - 1).unwrap());
        assert_eq!(taken.status, ChunkStatus::Assigned { worker_id: 1, mirror_id: 0 });
        // At 1 MB/s of late they take 0.05 s.
        let mut manager = one_victim(64 * MB, 64 * MB - 50_000, 10);
        for _ in 0..3 {
            manager.chunks()[0].went_lately(1e6);
        }
        assert!(manager.take_over_slow(1, 0, &thief).is_none());
    }

    /// One more connection judged beside the first stream (see `ChunkManager::on_trial`): its
    /// piece stays the stream's until the verdict. Given 2 % of a full line, it is outpaced: the
    /// stream that reaches its piece first takes it back, its few bytes written again, a "no"; a
    /// "no" before that drops it at once, the stream reading on. Not one that keeps pace, as on a
    /// line both share alike, nor once more pay.
    #[test]
    fn test_a_piece_on_trial_is_the_first_streams_until_the_verdict() {
        // 64 MB; the stream halved at 20 MB for one more, then 30 MB in 3 s; the new connection's
        // answer came 1 s ago, and it brought `brought` since.
        let judged = |brought: u64| {
            let mut manager = ChunkManager::new(64 * MB, 64 * MB).unwrap();
            let stream = answering(&mut manager, 0, 0);
            stream.asked(64 * MB - 1);
            stream.current_offset.store(20 * MB, Ordering::SeqCst);
            assert_eq!(manager.halve(1, 64 * 1024, Duration::ZERO), 1);
            manager.on_trial();
            let newcomer = manager.get_next_work(1, 0).unwrap();
            assert_eq!(newcomer.range, ByteRange::new(42 * MB, 64 * MB - 1).unwrap());
            newcomer.asked(64 * MB - 1);
            newcomer.answered(Instant::now());
            manager.backdate(1, Duration::from_secs(1));
            newcomer.current_offset.store(42 * MB + brought, Ordering::SeqCst);
            stream.current_offset.store(30 * MB, Ordering::SeqCst);
            manager.backdate(0, Duration::from_secs(3));
            (manager, stream, newcomer)
        };
        let (mut manager, stream, newcomer) = judged(240_000);
        stream.current_offset.store(42 * MB, Ordering::SeqCst);
        assert!(manager.run_on(&stream, false), "the stream goes on over the piece");
        assert!(newcomer.revoked.is_cancelled() && manager.overtaken() && !manager.overtaken());
        assert_eq!((stream.end_offset.load(Ordering::SeqCst), &manager.chunks()[1].status), (64 * MB - 1, &ChunkStatus::Merged));
        assert!(!manager.verdict(false), "nothing left on trial");

        let (mut manager, stream, newcomer) = judged(240_000);
        assert!(manager.verdict(false), "dropped at the verdict");
        assert!(newcomer.revoked.is_cancelled() && !manager.overtaken());
        assert_eq!(stream.end_offset.load(Ordering::SeqCst), 64 * MB - 1);

        // At 10 MB/s each, it keeps pace: a "no" leaves both going; and a stream that ends its own,
        // the newcomer with 2 MB of its 22 MB left, ends there.
        let (mut manager, _, newcomer) = judged(10 * MB);
        assert!(!manager.verdict(false) && !newcomer.revoked.is_cancelled(), "both go on");
        let (mut manager, stream, newcomer) = judged(10 * MB);
        stream.current_offset.store(42 * MB, Ordering::SeqCst);
        newcomer.current_offset.store(62 * MB, Ordering::SeqCst);
        manager.backdate(0, Duration::from_secs(1));
        manager.backdate(1, Duration::from_secs(1));
        assert!(!manager.run_on(&stream, false) && !newcomer.revoked.is_cancelled() && !manager.overtaken(), "it ends at its own");

        let (mut manager, stream, newcomer) = judged(240_000);
        assert!(!manager.verdict(true));
        stream.current_offset.store(42 * MB, Ordering::SeqCst);
        assert!(!manager.run_on(&stream, false) && !newcomer.revoked.is_cancelled(), "once more pay, the piece is its own");
    }

    #[test]
    fn test_only_a_long_silence_is_taken_over() {
        let mut manager = ChunkManager::new(3 * MB, MB).unwrap();
        let quiet = answering(&mut manager, 0, 0);
        let quieter = answering(&mut manager, 1, 1);
        let working = answering(&mut manager, 2, 0);
        manager.backdate(0, Duration::from_secs(3));
        manager.backdate(1, Duration::from_secs(5));
        manager.backdate(2, Duration::from_secs(9));
        // Worker 2 is writing what arrived, not waiting for its server.
        working.busy();

        assert!(manager.take_over_silent(3, 0, |_| Duration::from_secs(6)).is_none());
        // A mirror slow to answer gets longer: mirror 1's chunk is left alone.
        let limit = |mirror: usize| if mirror == 1 { Duration::from_secs(10) } else { SILENT };
        assert_eq!(manager.take_over_silent(3, 0, limit).unwrap().id, quiet.id);
        assert!(!quieter.revoked.is_cancelled() && !working.revoked.is_cancelled());
        // Of several silent chunks, the one silent longest goes first.
        let mut manager = ChunkManager::new(2 * MB, MB).unwrap();
        answering(&mut manager, 0, 0);
        answering(&mut manager, 1, 0);
        manager.backdate(0, Duration::from_secs(3));
        manager.backdate(1, Duration::from_secs(4));
        assert_eq!(manager.take_over_silent(2, 0, |_| SILENT).unwrap().id, 1);
    }

    #[test]
    fn test_a_silent_chunk_that_finished_meanwhile_is_completed_not_handed_over() {
        let mut manager = ChunkManager::new(MB, MB).unwrap();
        let silent = answering(&mut manager, 0, 0);
        manager.backdate(0, Duration::from_secs(3));
        // The worker's last batch lands just as its chunk is picked to be taken over.
        let lands = |_| {
            silent.current_offset.store(MB, Ordering::SeqCst);
            SILENT
        };
        assert!(manager.take_over_silent(1, 0, lands).is_none(), "nothing is left to hand over");
        assert!(silent.revoked.is_cancelled());
        assert!(manager.is_all_completed(), "the revoked worker will not complete it");
    }

    #[test]
    fn test_an_answer_on_its_way_takes_the_chunk_it_brings() {
        // The probe read [0, 100) of its answer for [0, 1 MB); a previous run left [3 MB, 4 MB).
        let done = [ByteRange::new(0, 99).unwrap(), ByteRange::new(3 * MB, 4 * MB - 1).unwrap()];
        let mut manager = ChunkManager::with_resumed_ranges(4 * MB, 2 * MB, &done).unwrap();
        assert!(manager.assign_at(0, 0, 50, MB - 1).is_none(), "those bytes are on disk");
        let live = manager.assign_at(0, 0, 100, MB - 1).unwrap();
        assert_eq!((live.range, live.status.clone()), (ByteRange::new(100, MB - 1).unwrap(), ChunkStatus::Assigned { worker_id: 0, mirror_id: 0 }));
        assert_eq!(live.end_offset.load(Ordering::SeqCst), MB - 1);
        assert!(manager.assign_at(1, 0, 100, MB - 1).is_none(), "already taken");

        // The rest of the chunk it was cut from goes to the other workers, and a thief can still
        // split what the answer has left to bring.
        let rest: Vec<Chunk> = std::iter::from_fn(|| manager.get_next_work(1, 0)).collect();
        let mut ranges: Vec<ByteRange> = rest.iter().map(|c| c.range).collect();
        ranges.sort_by_key(|r| r.start);
        assert_eq!(ranges, [ByteRange::new(MB, 2 * MB + 99).unwrap(), ByteRange::new(2 * MB + 100, 3 * MB - 1).unwrap()]);
        for chunk in &rest {
            chunk.current_offset.store(chunk.range.end + 1, Ordering::SeqCst);
            manager.mark_completed(chunk.id).unwrap();
        }
        live.answered(Instant::now());
        let (victim, stolen) = manager.steal_work(2, 0, 64 * 1024).unwrap();
        assert_eq!((victim, stolen.range.end), (live.id, MB - 1));
        assert_eq!(live.end_offset.load(Ordering::SeqCst), stolen.range.start - 1);

        // An answer that ends before the chunk would leaves the chunk whole when it does not.
        let mut manager = ChunkManager::new(MB, 256 * 1024).unwrap();
        assert_eq!(manager.assign_at(0, 0, 0, MB - 1).unwrap().range, ByteRange::new(0, 256 * 1024 - 1).unwrap());
    }

    #[test]
    fn test_steal_respects_threshold() {
        let mut manager = ChunkManager::new(MB, MB).unwrap();
        let work = manager.get_next_work(0, 0).unwrap();
        work.current_offset.store(MB - 1000, Ordering::SeqCst);
        assert!(manager.steal_work(1, 0, 64 * 1024).is_none());
    }

    #[test]
    fn test_victim_past_split_still_tiles_file() {
        let mut manager = ChunkManager::new(4 * MB, 4 * MB).unwrap();
        let victim = answering(&mut manager, 0, 0);
        let (_, stolen) = manager.steal_work(1, 0, MB).unwrap();
        // The victim's worker raced past the split point before seeing the truncation.
        victim.current_offset.store(3 * MB, Ordering::SeqCst);
        manager.mark_completed(victim.id).unwrap();
        stolen.current_offset.store(4 * MB, Ordering::SeqCst);
        manager.mark_completed(stolen.id).unwrap();
        assert!(manager.is_all_completed());
        assert_eq!(manager.completed_ranges(), vec![ByteRange::new(0, 4 * MB - 1).unwrap()]);
        assert_eq!(manager.total_downloaded(), 4 * MB);
    }

    #[test]
    fn test_resume_completed() {
        let completed = vec![ByteRange::new(0, 2 * MB - 1).unwrap()];
        let manager = ChunkManager::with_resumed_ranges(4 * MB, 2 * MB, &completed).unwrap();
        assert!(manager.chunks()[0].is_completed());
        assert!(!manager.chunks()[1].is_completed());
        assert_eq!(manager.total_downloaded(), 2 * MB);
        assert_eq!(manager.progress_ratio(), 0.5);
    }

    #[test]
    fn test_resume_non_aligned_work_stolen_ranges() {
        let completed = vec![ByteRange::new(0, 512 * 1024 - 1).unwrap()];
        let manager = ChunkManager::with_resumed_ranges(2 * MB, MB, &completed).unwrap();

        assert!(manager.chunks()[0].is_completed());
        assert_eq!(manager.chunks()[0].range, ByteRange::new(0, 512 * 1024 - 1).unwrap());
        assert!(!manager.chunks()[1].is_completed());
        assert_eq!(manager.chunks()[1].range, ByteRange::new(512 * 1024, 1536 * 1024 - 1).unwrap());
        assert!(!manager.chunks()[2].is_completed());
        assert_eq!(manager.chunks()[2].range, ByteRange::new(1536 * 1024, 2 * MB - 1).unwrap());
        assert_eq!(manager.total_downloaded(), 512 * 1024);
        assert_eq!(manager.progress_ratio(), 0.25);
    }

    #[test]
    fn test_resume_clamps_unsorted_overlapping_and_past_eof_ranges() {
        let completed = vec![
            ByteRange::new(900, 5000).unwrap(),
            ByteRange::new(0, 99).unwrap(),
            ByteRange::new(50, 199).unwrap(),
        ];
        let manager = ChunkManager::with_resumed_ranges(1000, 64 * 1024, &completed).unwrap();
        assert_eq!(
            manager.completed_ranges(),
            vec![ByteRange::new(0, 199).unwrap(), ByteRange::new(900, 999).unwrap()]
        );
        assert_eq!(manager.total_downloaded(), 300);
        for (i, c) in manager.chunks().iter().enumerate() {
            assert_eq!(c.id, i);
        }
    }

    #[test]
    fn test_mark_failed_keeps_progress() {
        let mut manager = ChunkManager::new(2 * MB, MB).unwrap();
        let work = manager.get_next_work(0, 0).unwrap();
        work.current_offset.store(512 * 1024, Ordering::SeqCst);

        manager.mark_failed(0, "connection drop", Duration::ZERO, true).unwrap();
        assert_eq!(manager.total_downloaded(), 512 * 1024);
        assert_eq!(manager.chunks()[0].range, ByteRange::new(512 * 1024, MB - 1).unwrap());
        assert_eq!(manager.chunks()[0].retries, 0, "an attempt with progress does not count");
        let prefix = &manager.chunks()[2];
        assert_eq!(prefix.id, 2);
        assert!(prefix.is_completed());
        assert_eq!(prefix.range, ByteRange::new(0, 512 * 1024 - 1).unwrap());

        // The retry starts where the failed attempt stopped.
        let retry = manager.get_next_work(1, 0).unwrap();
        assert_eq!(retry.id, 0);
        assert_eq!(retry.range.start, 512 * 1024);
        assert_eq!(retry.current_offset.load(Ordering::SeqCst), 512 * 1024);

        retry.current_offset.store(MB, Ordering::SeqCst);
        manager.mark_completed(0).unwrap();
        let rest = manager.get_next_work(1, 0).unwrap();
        rest.current_offset.store(2 * MB, Ordering::SeqCst);
        manager.mark_completed(rest.id).unwrap();
        assert!(manager.is_all_completed());
        assert_eq!(manager.completed_ranges(), vec![ByteRange::new(0, 2 * MB - 1).unwrap()]);
    }

    #[test]
    fn test_failed_chunk_backs_off_and_exhausts_retries() {
        let mut manager = ChunkManager::new(2 * MB, MB).unwrap();
        manager.set_max_retries(1);
        manager.get_next_work(0, 0).unwrap();
        manager.mark_failed(0, "timeout", Duration::ZERO, true).unwrap();
        assert!(manager.has_fatal_failure().is_none());

        // Chunk 0 is backing off, so the next unassigned chunk is handed out instead.
        assert_eq!(manager.get_next_work(0, 0).unwrap().id, 1);
        assert!(manager.get_next_work(0, 0).is_none());

        manager.mark_failed(1, "throttled", Duration::ZERO, false).unwrap();
        assert_eq!(manager.chunks()[1].retries, 0, "uncounted failures keep the budget");

        manager.chunks[0].not_before = None;
        assert_eq!(manager.get_next_work(0, 0).unwrap().id, 0);
        manager.mark_failed(0, "timeout again", Duration::ZERO, true).unwrap();
        let (id, reason) = manager.has_fatal_failure().unwrap();
        assert_eq!(id, 0);
        assert!(reason.contains("2 times") && reason.contains("timeout again"), "{reason}");
        assert!(manager.get_next_work(0, 0).is_none());
    }

    #[test]
    fn test_completed_ranges_include_in_flight_prefix() {
        let mut manager = ChunkManager::new(2 * MB, MB).unwrap();
        let a = manager.get_next_work(0, 0).unwrap();
        let b = manager.get_next_work(1, 0).unwrap();
        a.current_offset.store(1000, Ordering::SeqCst);
        b.current_offset.store(MB + 10, Ordering::SeqCst);
        assert_eq!(
            manager.completed_ranges(),
            vec![ByteRange::new(0, 999).unwrap(), ByteRange::new(MB, MB + 9).unwrap()]
        );
        assert_eq!(manager.total_downloaded(), 1010);
    }

    #[test]
    fn test_backoff_grows_and_is_capped() {
        for attempt in 1..=3 {
            let full = BACKOFF_BASE * (1 << (attempt - 1));
            let d = backoff_delay(attempt);
            assert!(d >= full / 2 && d <= full, "attempt {attempt}: {d:?}");
        }
        assert!(backoff_delay(40) <= BACKOFF_CAP);
    }

    #[test]
    fn test_concurrent_workers_tile_file() {
        use parking_lot::Mutex;
        let size = 8 * MB + 12345;
        let manager = Arc::new(Mutex::new(ChunkManager::new(size, MB).unwrap()));
        std::thread::scope(|s| {
            for worker in 0..8 {
                let manager = Arc::clone(&manager);
                s.spawn(move || loop {
                    let work = {
                        let mut m = manager.lock();
                        if m.is_all_completed() {
                            return;
                        }
                        m.get_next_work(worker, 0).or_else(|| m.steal_work(worker, 0, 64 * 1024).map(|(_, c)| c))
                    };
                    let Some(chunk) = work else {
                        std::thread::yield_now();
                        continue;
                    };
                    // "Download" in 16KB steps, honouring truncation like the real worker.
                    let mut pos = chunk.current_offset.load(Ordering::SeqCst);
                    while pos <= chunk.end_offset.load(Ordering::SeqCst) {
                        pos = (pos + 16 * 1024).min(chunk.end_offset.load(Ordering::SeqCst) + 1);
                        chunk.current_offset.store(pos, Ordering::SeqCst);
                    }
                    manager.lock().mark_completed(chunk.id).unwrap();
                });
            }
        });
        let m = manager.lock();
        assert!(m.is_all_completed());
        assert_eq!(m.completed_ranges(), vec![ByteRange::new(0, size - 1).unwrap()]);
    }
}
