use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use crate::range::{ByteRange, RangeError};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Base and cap of the per-chunk exponential retry backoff.
const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// A timed steal leaves a chunk alone unless its worker needs more than this many times a new
/// request's startup to finish it: otherwise the new request barely gets going before it ends.
const STEAL_MIN_STARTUPS: f64 = 2.0;

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
    /// Since when the attempt's worker has been waiting for the server without a byte; `None`
    /// while it is busy (writing, rate limited) rather than waiting.
    waiting: Arc<Mutex<Option<Instant>>>,
    not_before: Option<Instant>,
    assigned_at: Option<Instant>,
    assigned_offset: u64,
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
            waiting: Arc::new(Mutex::new(None)),
            not_before: None,
            assigned_at: None,
            assigned_offset: range.start,
        }
    }

    /// The attempt's worker waits for the server from `since` on (a time in the future while
    /// the rate limit holds it back).
    pub(crate) fn wait_for_server(&self, since: Instant) {
        *self.waiting.lock() = Some(since);
    }

    /// The attempt's worker is busy with what arrived, not waiting for the server.
    pub(crate) fn busy(&self) {
        *self.waiting.lock() = None;
    }

    /// How long the attempt's worker has been waiting for the server without a byte.
    fn silence(&self, now: Instant) -> Duration {
        self.waiting.lock().map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
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
        if self.is_completed() {
            return Some(self.range);
        }
        let pos = self.current_offset.load(Ordering::SeqCst).min(self.range.end.saturating_add(1));
        (pos > self.range.start).then(|| ByteRange { start: self.range.start, end: pos - 1 })
    }

    fn done_bytes(&self) -> u64 {
        self.written_prefix().map_or(0, |r| r.len())
    }

    /// Bytes per second written since assignment; `None` before anything arrived.
    fn rate(&self, now: Instant) -> Option<f64> {
        let received = self.current_offset.load(Ordering::SeqCst).saturating_sub(self.assigned_offset);
        let elapsed = self.assigned_at.map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        (received > 0 && elapsed > 0.0).then(|| received as f64 / elapsed)
    }

    /// How the chunk's worker is expected to go on: seconds until its next byte, and bytes per
    /// second from then on. Once it has a rate since assignment, that rate at once; before, what
    /// a request to its mirror costs (`expected`), less the part of the startup already behind it.
    /// `None` without either.
    fn pace(&self, now: Instant, expected: Option<&StealTiming>) -> Option<(f64, f64)> {
        if let Some(rate) = self.rate(now) {
            return Some((0.0, rate));
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
    completed: usize,
    /// No `Unassigned` chunk exists below this index.
    next_fresh: usize,
    /// Failed chunks waiting to be retried.
    retry_queue: Vec<usize>,
    max_retries: u32,
    fatal: Option<(usize, String)>,
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

    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
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
        chunk.assigned_offset = chunk.current_offset.load(Ordering::SeqCst);
        chunk.wait_for_server(now);
        chunk.clone()
    }

    /// Hands everything left of the in-flight chunk whose worker has waited longest for the server
    /// without a byte to an idle thief, if that wait is at least `min_silence(chunk's mirror)`. The
    /// silent attempt is revoked. What it wrote becomes a completed chunk; the rest keeps the
    /// chunk's id and gets new offsets and a new token, so the revoked worker, which still holds
    /// the old ones, can neither move nor settle what the thief now owns.
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
        let chunk = &mut self.chunks[id];
        chunk.current_offset = Arc::new(AtomicU64::new(pos));
        chunk.end_offset = Arc::new(AtomicU64::new(end));
        chunk.revoked = CancellationToken::new();
        chunk.waiting = Arc::new(Mutex::new(None));
        Some(self.assign(id, thief_worker_id, thief_mirror_id, now))
    }

    /// Splits the in-flight chunk expected to finish last (ties: most bytes left). Its worker is
    /// expected to go on at the rate it has had since assignment, or before it has one, at its
    /// mirror's rate once its mirror's startup is over. A timed steal splits where both sides
    /// finish together, the thief starting after its mirror's startup at its mirror's rate, and
    /// leaves the chunk alone if its worker finishes within `STEAL_MIN_STARTUPS` of those startups
    /// anyway. Without timing the rest is split in half. A split that would hand the thief fewer
    /// than `min_bytes` is not made, and the victim keeps at least one byte. The victim is always
    /// truncated and the thief always gets `[split, old_end]`; if the victim's worker already
    /// wrote past the split, those bytes are identical and simply get written twice.
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
            .filter_map(|c| match c.status {
                ChunkStatus::Assigned { mirror_id, .. } => {
                    let remaining = c.remaining_bytes();
                    (remaining > min_bytes).then(|| (c.id, remaining, c.pace(now, expected(mirror_id))))
                }
                _ => None,
            })
            .max_by(|a, b| eta_secs(a.1, a.2).total_cmp(&eta_secs(b.1, b.2)).then(a.1.cmp(&b.1)))?;

        let victim = &mut self.chunks[victim_id];
        let cur_pos = victim.current_offset.load(Ordering::SeqCst).max(victim.range.start);
        let cur_end = victim.end_offset.load(Ordering::SeqCst);
        let remaining = cur_end.checked_sub(cur_pos)?.saturating_add(1);
        let keep = match (expected(thief_mirror_id).filter(|t| t.rate > 0.0), pace) {
            (Some(thief), Some((wait, victim_rate))) => {
                let (startup, left) = (thief.startup.as_secs_f64(), remaining as f64);
                if eta_secs(remaining, pace) <= STEAL_MIN_STARTUPS * startup {
                    return None;
                }
                // wait + keep / victim_rate == startup + (remaining - keep) / thief.rate
                (((startup - wait) * thief.rate + left) * victim_rate / (victim_rate + thief.rate)) as u64
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
            .map(|c| {
                let (status_str, worker_id) = match &c.status {
                    ChunkStatus::Unassigned => ("Pending".to_string(), None),
                    ChunkStatus::Assigned { worker_id, .. } => (format!("Worker {}", worker_id), Some(*worker_id)),
                    ChunkStatus::Completed => ("Completed".to_string(), None),
                    ChunkStatus::Failed { reason, .. } => (format!("Failed: {}", reason), None),
                };
                ChunkSnapshot {
                    id: c.id,
                    range_start: c.range.start,
                    range_end: c.range.end,
                    downloaded_bytes: c.done_bytes(),
                    total_bytes: c.range.len(),
                    status: status_str,
                    worker_id,
                }
            })
            .collect()
    }

    fn get_chunk_mut(&mut self, chunk_id: usize) -> Result<&mut Chunk, ChunkError> {
        self.chunks.get_mut(chunk_id).ok_or(ChunkError::NotFound(chunk_id))
    }

    /// Moves a chunk's assignment and its worker's wait `by` into the past, as if its worker had
    /// been at it that long.
    #[cfg(test)]
    pub(crate) fn backdate(&mut self, chunk_id: usize, by: Duration) {
        let chunk = &mut self.chunks[chunk_id];
        chunk.assigned_at = chunk.assigned_at.and_then(|t| t.checked_sub(by));
        let waiting = *chunk.waiting.lock();
        *chunk.waiting.lock() = waiting.and_then(|t| t.checked_sub(by));
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
        let work = manager.get_next_work(0, 0).unwrap();
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
        let fast = manager.get_next_work(0, 0).unwrap(); // [0, 6MB)
        let slow = manager.get_next_work(1, 0).unwrap(); // [6MB, 10MB)
        fast.current_offset.store(MB, Ordering::SeqCst);
        slow.current_offset.store(6 * MB + 1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(5));
        // Fast has the most bytes left (5MB) but a high rate; slow has 4MB left at 1 byte per 5ms.
        let (victim, _) = manager.steal_work(2, 0, MB).unwrap();
        assert_eq!(victim, 1);

        // An assigned chunk that has received nothing (e.g. stuck before headers) is stealable too.
        let mut manager = ChunkManager::new(4 * MB, 4 * MB).unwrap();
        manager.get_next_work(0, 0).unwrap();
        assert!(manager.steal_work(1, 0, MB).is_some());
    }

    /// A manager with one chunk of `size` whose worker wrote `written` bytes over the last `secs`.
    fn one_victim(size: u64, written: u64, secs: u64) -> ChunkManager {
        let mut manager = ChunkManager::new(size, size).unwrap();
        manager.get_next_work(0, 0).unwrap().current_offset.store(written, Ordering::SeqCst);
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
        // Assigned to slow mirror 1 and still waiting for its answer: it is expected to start after
        // 0.5 s at 0.5 MB/s. A thief on mirror 0 starts after 0.1 s at 2 MB/s. Both finish after
        // 1.78 s: the victim with 0.64 MB, the thief with 3.36 MB.
        let mut manager = ChunkManager::new(4 * MB, 4 * MB).unwrap();
        manager.get_next_work(0, 1).unwrap();
        let mirrors = [cost(0.1, 2.0 * MB as f64), cost(0.5, 0.5 * MB as f64)];
        let (_, stolen) = manager.steal_work(1, 0, timed(64 * 1024, &mirrors)).unwrap();
        let split = (0.64 * MB as f64) as u64;
        assert!(stolen.range.start.abs_diff(split) < 64 * 1024, "split at {} instead of about {split}", stolen.range.start);
    }

    #[test]
    fn test_thieves_in_a_row_split_the_straggler_not_each_others_fresh_chunks() {
        // 18 MiB left at 2 MiB/s: 9 s. Seven idle workers arrive one after the other, each a new
        // request that starts after 0.3 s and then moves 2 MiB/s, as the straggler does.
        let mut manager = one_victim(20 * MB, 2 * MB, 1);
        let mirror = [cost(0.3, 2.0 * MB as f64)];
        let victims: Vec<usize> =
            (1..=7).map(|thief| manager.steal_work(thief, 0, timed(64 * 1024, &mirror)).unwrap().0).collect();
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

    const SILENT: Duration = Duration::from_secs(2);

    #[test]
    fn test_silent_chunk_is_taken_over_keeping_what_it_wrote() {
        let mut manager = ChunkManager::new(4 * MB, 4 * MB).unwrap();
        let silent = manager.get_next_work(0, 0).unwrap();
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

        taken.current_offset.store(4 * MB, Ordering::SeqCst);
        manager.mark_completed(taken.id).unwrap();
        assert!(manager.is_all_completed());
        assert_eq!(manager.completed_ranges(), vec![ByteRange::new(0, 4 * MB - 1).unwrap()]);
    }

    #[test]
    fn test_silent_chunk_that_wrote_nothing_is_handed_over_whole() {
        // Stuck before its answer arrived, at the very start of the file.
        let mut manager = ChunkManager::new(2 * MB, MB).unwrap();
        let silent = manager.get_next_work(0, 0).unwrap();
        let other = manager.get_next_work(1, 0).unwrap();
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
    fn test_only_a_long_silence_is_taken_over() {
        let mut manager = ChunkManager::new(3 * MB, MB).unwrap();
        let quiet = manager.get_next_work(0, 0).unwrap();
        let quieter = manager.get_next_work(1, 1).unwrap();
        let working = manager.get_next_work(2, 0).unwrap();
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
        manager.get_next_work(0, 0).unwrap();
        manager.get_next_work(1, 0).unwrap();
        manager.backdate(0, Duration::from_secs(3));
        manager.backdate(1, Duration::from_secs(4));
        assert_eq!(manager.take_over_silent(2, 0, |_| SILENT).unwrap().id, 1);
    }

    #[test]
    fn test_a_silent_chunk_that_finished_meanwhile_is_completed_not_handed_over() {
        let mut manager = ChunkManager::new(MB, MB).unwrap();
        let silent = manager.get_next_work(0, 0).unwrap();
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
        let victim = manager.get_next_work(0, 0).unwrap();
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
