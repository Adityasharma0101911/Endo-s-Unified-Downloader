use serde::{Deserialize, Serialize};
use bitvec::prelude::*;
use crate::range::{ByteRange, RangeError};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ChunkError {
    #[error("Range error: {0}")]
    Range(#[from] RangeError),
    #[error("Chunk {0} not found")]
    NotFound(usize),
    #[error("Invalid state transition for chunk {0}: {1}")]
    InvalidTransition(usize, String),
    #[error("No work available to steal (threshold {0} bytes)")]
    NoWorkToSteal(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkStatus {
    Unassigned,
    Assigned {
        worker_id: usize,
        mirror_id: usize,
    },
    Downloading {
        worker_id: usize,
        mirror_id: usize,
        downloaded_bytes: u64,
    },
    Verifying,
    Completed,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: usize,
    pub range: ByteRange,
    pub status: ChunkStatus,
    pub downloaded_bytes: u64,
    pub hash: Option<[u8; 32]>,
}

impl Chunk {
    pub fn new(id: usize, range: ByteRange) -> Self {
        Self {
            id,
            range,
            status: ChunkStatus::Unassigned,
            downloaded_bytes: 0,
            hash: None,
        }
    }

    pub fn is_completed(&self) -> bool {
        matches!(self.status, ChunkStatus::Completed)
    }

    pub fn is_in_flight(&self) -> bool {
        matches!(self.status, ChunkStatus::Assigned { .. } | ChunkStatus::Downloading { .. })
    }

    pub fn remaining_bytes(&self) -> u64 {
        self.range.len().saturating_sub(self.downloaded_bytes)
    }
}

/// Manages chunk partitioning, assignment, bitfield tracking, and work stealing.
#[derive(Debug)]
pub struct ChunkManager {
    total_size: u64,
    base_chunk_size: u64,
    chunks: Vec<Chunk>,
    completed_bitmap: BitVec,
    total_downloaded: u64,
    next_chunk_id: usize,
}

impl ChunkManager {
    /// Creates a new ChunkManager partitioning a file of `total_size` into chunks of `base_chunk_size`.
    pub fn new(total_size: u64, base_chunk_size: u64) -> Result<Self, ChunkError> {
        let base_chunk_size = base_chunk_size.max(64 * 1024); // Minimum 64KB
        let mut chunks = Vec::new();
        let mut offset = 0;
        let mut id = 0;

        while offset < total_size {
            let chunk_len = (total_size - offset).min(base_chunk_size);
            let range = ByteRange::from_len(offset, chunk_len)?;
            chunks.push(Chunk::new(id, range));
            offset += chunk_len;
            id += 1;
        }

        let num_chunks = chunks.len();
        let completed_bitmap = bitvec![0; num_chunks];

        Ok(Self {
            total_size,
            base_chunk_size,
            chunks,
            completed_bitmap,
            total_downloaded: 0,
            next_chunk_id: id,
        })
    }

    /// Creates a ChunkManager from pre-existing completed ranges (resuming a previous download).
    pub fn with_resumed_ranges(
        total_size: u64,
        base_chunk_size: u64,
        completed_ranges: &[ByteRange],
    ) -> Result<Self, ChunkError> {
        let mut manager = Self::new(total_size, base_chunk_size)?;

        for completed_range in completed_ranges {
            for chunk in &mut manager.chunks {
                if completed_range.contains_range(&chunk.range) {
                    chunk.status = ChunkStatus::Completed;
                    chunk.downloaded_bytes = chunk.range.len();
                }
            }
        }

        manager.recalculate_progress();
        Ok(manager)
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn base_chunk_size(&self) -> u64 {
        self.base_chunk_size
    }

    pub fn total_downloaded(&self) -> u64 {
        self.total_downloaded
    }

    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    pub fn is_all_completed(&self) -> bool {
        self.total_downloaded >= self.total_size && self.chunks.iter().all(|c| c.is_completed())
    }

    pub fn progress_ratio(&self) -> f64 {
        if self.total_size == 0 {
            1.0
        } else {
            self.total_downloaded as f64 / self.total_size as f64
        }
    }

    /// Gets an unassigned or failed chunk for a worker and mirror.
    pub fn get_next_work(&mut self, worker_id: usize, mirror_id: usize) -> Option<Chunk> {
        for chunk in &mut self.chunks {
            match chunk.status {
                ChunkStatus::Unassigned => {
                    chunk.status = ChunkStatus::Assigned { worker_id, mirror_id };
                    return Some(chunk.clone());
                }
                ChunkStatus::Failed { retries, .. } if retries < 5 => {
                    chunk.status = ChunkStatus::Assigned { worker_id, mirror_id };
                    return Some(chunk.clone());
                }
                _ => {}
            }
        }
        None
    }

    /// Steals work from the in-flight chunk that has the largest remaining sub-range.
    /// If an eligible chunk with remaining bytes >= `min_steal_threshold` is found,
    /// its remaining range is split at the midpoint.
    /// Returns `(victim_chunk_id, new_stolen_chunk)`.
    pub fn steal_work(
        &mut self,
        thief_worker_id: usize,
        thief_mirror_id: usize,
        min_steal_threshold: u64,
    ) -> Option<(usize, Chunk)> {
        // Find in-flight chunk with maximum remaining bytes
        let mut best_candidate: Option<(usize, u64)> = None;

        for (idx, chunk) in self.chunks.iter().enumerate() {
            if chunk.is_in_flight() {
                let remaining = chunk.remaining_bytes();
                if remaining >= min_steal_threshold {
                    match best_candidate {
                        None => best_candidate = Some((idx, remaining)),
                        Some((_, max_rem)) if remaining > max_rem => {
                            best_candidate = Some((idx, remaining));
                        }
                        _ => {}
                    }
                }
            }
        }

        let (victim_idx, remaining) = best_candidate?;
        let victim = &mut self.chunks[victim_idx];

        // Split the remaining portion: [victim.range.start + victim.downloaded_bytes, victim.range.end]
        let remaining_start = victim.range.start + victim.downloaded_bytes;
        let split_offset = remaining_start + (remaining / 2);

        // Truncate victim range to end right before split_offset
        let victim_new_end = split_offset - 1;
        let old_end = victim.range.end;
        victim.range.end = victim_new_end;

        // Create stolen chunk for [split_offset, old_end]
        let stolen_range = ByteRange::new(split_offset, old_end).ok()?;
        let new_chunk_id = self.next_chunk_id;
        self.next_chunk_id += 1;

        let mut stolen_chunk = Chunk::new(new_chunk_id, stolen_range);
        stolen_chunk.status = ChunkStatus::Assigned {
            worker_id: thief_worker_id,
            mirror_id: thief_mirror_id,
        };

        let victim_id = victim.id;
        self.chunks.push(stolen_chunk.clone());
        self.completed_bitmap.push(false);

        Some((victim_id, stolen_chunk))
    }

    /// Updates progress for a chunk being downloaded.
    pub fn update_chunk_progress(
        &mut self,
        chunk_id: usize,
        bytes_just_received: u64,
        worker_id: usize,
        mirror_id: usize,
    ) -> Result<(), ChunkError> {
        let chunk = self.get_chunk_mut(chunk_id)?;
        chunk.downloaded_bytes = (chunk.downloaded_bytes + bytes_just_received).min(chunk.range.len());
        chunk.status = ChunkStatus::Downloading {
            worker_id,
            mirror_id,
            downloaded_bytes: chunk.downloaded_bytes,
        };
        self.total_downloaded = (self.total_downloaded + bytes_just_received).min(self.total_size);
        Ok(())
    }

    /// Marks a chunk as completed and records its BLAKE3 hash.
    pub fn mark_completed(
        &mut self,
        chunk_id: usize,
        hash: Option<[u8; 32]>,
    ) -> Result<(), ChunkError> {
        let chunk = self.get_chunk_mut(chunk_id)?;
        chunk.status = ChunkStatus::Completed;
        chunk.downloaded_bytes = chunk.range.len();
        chunk.hash = hash;

        if chunk_id < self.completed_bitmap.len() {
            self.completed_bitmap.set(chunk_id, true);
        }

        self.recalculate_progress();
        Ok(())
    }

    /// Marks a chunk as failed and increments retry counter.
    pub fn mark_failed(&mut self, chunk_id: usize, reason: &str) -> Result<(), ChunkError> {
        let chunk = self.get_chunk_mut(chunk_id)?;
        let retries = match chunk.status {
            ChunkStatus::Failed { retries, .. } => retries + 1,
            _ => 1,
        };
        chunk.status = ChunkStatus::Failed {
            reason: reason.to_string(),
            retries,
        };
        Ok(())
    }

    /// Returns all completed byte ranges.
    pub fn completed_ranges(&self) -> Vec<ByteRange> {
        let mut ranges = Vec::new();
        for chunk in &self.chunks {
            if chunk.is_completed() {
                ranges.push(chunk.range);
            }
        }
        ranges.sort();
        ranges
    }

    /// Returns a vector of snapshots for all current chunks for UI rendering.
    pub fn chunk_snapshots(&self) -> Vec<ChunkSnapshot> {
        self.chunks
            .iter()
            .map(|c| {
                let (status_str, worker_id) = match &c.status {
                    ChunkStatus::Unassigned => ("Pending".to_string(), None),
                    ChunkStatus::Assigned { worker_id, .. } => (format!("Worker {}", worker_id), Some(*worker_id)),
                    ChunkStatus::Downloading { worker_id, .. } => (format!("Worker {}", worker_id), Some(*worker_id)),
                    ChunkStatus::Verifying => ("Verifying".to_string(), None),
                    ChunkStatus::Completed => ("Completed".to_string(), None),
                    ChunkStatus::Failed { reason, .. } => (format!("Failed: {}", reason), None),
                };
                ChunkSnapshot {
                    id: c.id,
                    range_start: c.range.start,
                    range_end: c.range.end,
                    downloaded_bytes: c.downloaded_bytes,
                    total_bytes: c.range.len(),
                    status: status_str,
                    worker_id,
                }
            })
            .collect()
    }

    fn get_chunk_mut(&mut self, chunk_id: usize) -> Result<&mut Chunk, ChunkError> {
        self.chunks
            .iter_mut()
            .find(|c| c.id == chunk_id)
            .ok_or(ChunkError::NotFound(chunk_id))
    }

    fn recalculate_progress(&mut self) {
        self.total_downloaded = self
            .chunks
            .iter()
            .map(|c| c.downloaded_bytes)
            .sum::<u64>()
            .min(self.total_size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_partitioning() {
        let manager = ChunkManager::new(10 * 1024 * 1024, 2 * 1024 * 1024).unwrap();
        assert_eq!(manager.chunks().len(), 5);
        assert_eq!(manager.chunks()[0].range, ByteRange::new(0, 2 * 1024 * 1024 - 1).unwrap());
        assert_eq!(
            manager.chunks()[4].range,
            ByteRange::new(8 * 1024 * 1024, 10 * 1024 * 1024 - 1).unwrap()
        );
    }

    #[test]
    fn test_work_stealing() {
        let mut manager = ChunkManager::new(10 * 1024 * 1024, 10 * 1024 * 1024).unwrap();
        assert_eq!(manager.chunks().len(), 1);

        // Worker 0 takes the only chunk
        let work = manager.get_next_work(0, 0).unwrap();
        assert_eq!(work.id, 0);

        // Worker 0 downloaded 2MB out of 10MB
        manager.update_chunk_progress(0, 2 * 1024 * 1024, 0, 0).unwrap();

        // Worker 1 tries to steal work, threshold is 1MB
        let (victim_id, stolen) = manager.steal_work(1, 1, 1024 * 1024).unwrap();
        assert_eq!(victim_id, 0);

        // Original remaining: 8MB (from 2MB to 10MB).
        // Split point: 2MB + (8MB / 2) = 6MB.
        // Victim new range: [0, 6MB - 1]. Stolen range: [6MB, 10MB - 1].
        let victim = &manager.chunks()[0];
        assert_eq!(victim.range.end, 6 * 1024 * 1024 - 1);
        assert_eq!(stolen.range.start, 6 * 1024 * 1024);
        assert_eq!(stolen.range.end, 10 * 1024 * 1024 - 1);
        assert_eq!(stolen.range.len(), 4 * 1024 * 1024);
    }

    #[test]
    fn test_resume_completed() {
        let completed = vec![ByteRange::new(0, 2 * 1024 * 1024 - 1).unwrap()];
        let manager = ChunkManager::with_resumed_ranges(4 * 1024 * 1024, 2 * 1024 * 1024, &completed).unwrap();
        assert_eq!(manager.chunks()[0].is_completed(), true);
        assert_eq!(manager.chunks()[1].is_completed(), false);
        assert_eq!(manager.total_downloaded(), 2 * 1024 * 1024);
        assert_eq!(manager.progress_ratio(), 0.5);
    }
}
