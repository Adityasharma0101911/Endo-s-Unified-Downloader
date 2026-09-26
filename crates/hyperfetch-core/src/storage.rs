use std::fs::{File, OpenOptions};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use blake3::hazmat::{merge_subtrees_non_root, merge_subtrees_root, ChainingValue, HasherExt, Mode};
use parking_lot::{Condvar, Mutex};
use thiserror::Error;
use sha2::{Digest as Sha2Digest, Sha256};
use md5::Md5;
use crate::range::ByteRange;

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Write out of bounds: offset {0} + len {1} > file size {2}")]
    OutOfBounds(u64, usize, u64),
    #[error("Preallocation failed: {0}")]
    PreallocationFailed(String),
    #[error("Chunk verification failed for range {0}: expected {1}, got {2}")]
    HashMismatch(ByteRange, String, String),
}

/// Read size for hashing passes.
const HASH_BUF_SIZE: usize = 4 * 1024 * 1024;
/// Read size for whole-file hashing: enough work per block to spread BLAKE3 across every core,
/// small enough to stay in cache between being read and being hashed (on a 4 GiB file, 8 MiB
/// beat both 4 and 16 MiB).
const FILE_HASH_BLOCK: usize = 8 * 1024 * 1024;
/// Smallest block a writer hashes on its own. A power of two of BLAKE3 chunks, so each block is a
/// subtree of the file's hash tree and the blocks' hashes merge into the file's.
const MIN_HASH_BLOCK: u64 = 256 * 1024;
/// Most blocks per file, which keeps their hashes to a few MiB: larger files get larger blocks.
const MAX_HASH_BLOCKS: u64 = 1 << 16;
/// Most the prefix hasher reads at once: it shares the file handle with the writers.
const FOLLOW_STEP: u64 = 1024 * 1024;

/// Preallocated output file shared by all workers. Writes are positional (`pwrite` /
/// `WriteFile` with an offset), so concurrent writers never share a cursor or a mapping.
///
/// Once told to with [`DiskWriter::track_digest`], the writer also hashes what it writes: each
/// aligned block is hashed with BLAKE3 as soon as it has been written in full, so
/// [`DiskWriter::digest`] reads back only the blocks it never saw whole (and SHA-256/MD5, see
/// `track_digest`).
#[derive(Clone)]
pub struct DiskWriter {
    path: PathBuf,
    size: u64,
    inner: Arc<Inner>,
}

/// Shared by a writer's clones. Once the last one is gone nothing writes any more, so the prefix
/// hasher stops.
struct Inner {
    file: Arc<File>,
    hashes: Arc<Hashes>,
    #[cfg(test)]
    syncs: AtomicUsize,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.hashes.stop();
    }
}

impl DiskWriter {
    /// Opens or creates the file and sets its length to `total_size`. On Linux and macOS the space
    /// is reserved, so a full disk is reported here; on Windows the file is sparse and a full disk
    /// surfaces as a write error.
    pub fn open_or_create(path: impl AsRef<Path>, total_size: u64) -> Result<Self, StorageError> {
        let block = total_size.div_ceil(MAX_HASH_BLOCKS).next_power_of_two().max(MIN_HASH_BLOCK);
        Self::open(path.as_ref(), total_size, block)
    }

    /// `open_or_create`, hashing blocks of `block` bytes (a power of two, at least 1 KiB).
    fn open(path: &Path, total_size: u64, block: u64) -> Result<Self, StorageError> {
        let path = path.to_path_buf();

        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        preallocate_file(&file, total_size)?;

        Ok(Self {
            path,
            size: total_size,
            inner: Arc::new(Inner {
                file: Arc::new(file),
                hashes: Arc::new(Hashes::new(total_size, block)),
                #[cfg(test)]
                syncs: AtomicUsize::new(0),
            }),
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `data` at `offset`. Safe to call concurrently from many threads.
    pub fn write_chunk_slice(&self, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        if offset.checked_add(data.len() as u64).is_none_or(|end| end > self.size) {
            return Err(StorageError::OutOfBounds(offset, data.len(), self.size));
        }
        write_all_at(&self.inner.file, data, offset)?;
        self.inner.hashes.wrote(&self.inner.file, offset, data);
        Ok(())
    }

    /// Starts hashing what is written from now on, for [`DiskWriter::digest`]; a writer that is
    /// never asked for a digest (a repair) does without. Counts `on_disk`, bytes an earlier run
    /// wrote, as written, so blocks they complete are hashed too. When `expected_checksum` needs
    /// SHA-256 or MD5, also starts hashing the file's written prefix with it in the background as
    /// the prefix grows, so `digest` reads only the rest. That hashing stops once the last clone
    /// of this writer is dropped.
    pub fn track_digest(&self, on_disk: &[ByteRange], expected_checksum: Option<&str>) {
        let (sha256, md5) = needs(expected_checksum).unwrap_or_default();
        let hashes = &self.inner.hashes;
        let mut state = hashes.state.lock();
        state.tracking = true;
        for range in on_disk.iter().filter(|r| r.start < self.size) {
            add(&mut state.written, range.start..range.end.saturating_add(1).min(self.size));
        }
        hashes.changed.notify_all();
        if (sha256 || md5) && state.follower.is_none() && !state.stopped {
            let (hashes, file) = (Arc::clone(hashes), Arc::clone(&self.inner.file));
            let prefix = PrefixHash::new(sha256, md5);
            // Without the thread, `digest` hashes the whole file at the end, as it would anyway.
            state.follower = std::thread::Builder::new()
                .name("prefix-hash".into())
                .spawn(move || follow(&hashes, &file, prefix))
                .ok();
        }
    }

    /// The file's digest, once writing is done: BLAKE3 merged from the blocks hashed as they were
    /// written, reading back (on every core) only the blocks never written whole through this
    /// writer, plus SHA-256/MD5 when `expected_checksum` needs them, reading only what the prefix
    /// hasher had not reached. Reads through this writer's handle; blocking.
    pub fn digest(&self, expected_checksum: Option<&str>) -> std::io::Result<FileDigest> {
        let (sha256, md5) = needs(expected_checksum).unwrap_or_default();
        let (hashes, file) = (&*self.inner.hashes, &*self.inner.file);
        let mut prefix = hashes
            .stop_follower()
            .filter(|p| p.has(sha256, md5))
            .unwrap_or_else(|| PrefixHash::new(sha256, md5));
        let (blake3, tail) = std::thread::scope(|s| {
            let tail = (sha256 || md5).then(|| s.spawn(|| prefix.read_to(file, self.size)));
            let blake3 = hashes.root(file);
            (blake3, tail.map(joined))
        });
        tail.transpose()?;
        Ok(FileDigest::new(blake3?, prefix))
    }

    /// The file's digest from reading all of it back through this writer's handle, ignoring
    /// what was hashed while writing. Blocking.
    pub fn full_digest(&self, expected_checksum: Option<&str>) -> std::io::Result<FileDigest> {
        let (sha256, md5) = needs(expected_checksum).unwrap_or_default();
        self.read_digest(sha256, md5)
    }

    fn read_digest(&self, sha256: bool, md5: bool) -> std::io::Result<FileDigest> {
        FileDigest::of_file(&self.inner.file, self.size, FILE_HASH_BLOCK, sha256, md5)
    }

    /// Computes BLAKE3 hash for a specific range.
    pub fn compute_chunk_hash(&self, range: &ByteRange) -> Result<[u8; 32], StorageError> {
        if range.end >= self.size {
            return Err(StorageError::OutOfBounds(range.start, range.len() as usize, self.size));
        }
        let mut hasher = blake3::Hasher::new();
        read_blocks(&self.inner.file, range.start, range.len(), |block| {
            hasher.update(block);
        })?;
        Ok(*hasher.finalize().as_bytes())
    }

    /// Computes BLAKE3 root hash for the entire file.
    pub fn compute_file_hash(&self) -> Result<[u8; 32], StorageError> {
        Ok(self.read_digest(false, false)?.blake3)
    }

    /// Computes SHA-256 hash for the entire file as a lowercase hex string.
    pub fn compute_sha256(&self) -> Result<String, StorageError> {
        Ok(self.read_digest(true, false)?.sha256.unwrap_or_default())
    }

    /// Computes MD5 hash for the entire file as a lowercase hex string.
    pub fn compute_md5(&self) -> Result<String, StorageError> {
        Ok(self.read_digest(false, true)?.md5.unwrap_or_default())
    }

    /// Verifies the file against an expected checksum ("sha256:...", "md5:...", "blake3:...", or raw hex).
    /// `Ok(false)` means the hash does not match; `Err` means the file could not be read or the
    /// algorithm prefix is unknown.
    pub fn verify_checksum(&self, expected: &str) -> Result<bool, String> {
        let checksum = Checksum::parse(expected)?;
        let digest = self
            .read_digest(checksum.needs_sha256(), checksum.needs_md5())
            .map_err(|e| format!("Failed to read {}: {}", self.path.display(), e))?;
        Ok(checksum.matches(&digest))
    }

    /// Verifies an existing file against an expected checksum. Opens the file read-only and never modifies it.
    pub fn verify_file_checksum(path: &Path, expected: &str) -> Result<bool, String> {
        let checksum = Checksum::parse(expected)?;
        let digests = FileDigest::of(path, checksum.needs_sha256(), checksum.needs_md5())
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        Ok(checksum.matches(&digests))
    }

    /// Verifies a chunk against an expected hash.
    pub fn verify_chunk(&self, range: &ByteRange, expected_hash: &[u8; 32]) -> Result<bool, StorageError> {
        let actual = self.compute_chunk_hash(range)?;
        if actual == *expected_hash {
            Ok(true)
        } else {
            Err(StorageError::HashMismatch(
                *range,
                hex::encode(expected_hash),
                hex::encode(&actual),
            ))
        }
    }

    /// Flushes written data to persistent storage.
    pub fn sync(&self) -> Result<(), StorageError> {
        #[cfg(test)]
        self.inner.syncs.fetch_add(1, Ordering::Relaxed);
        self.inner.file.sync_data()?;
        Ok(())
    }

    /// Flushes written data to persistent storage through a handle of its own, so that reads
    /// through this writer (such as [`DiskWriter::digest`]) go on meanwhile: Windows serves one
    /// handle's requests, the flush included, one at a time.
    pub fn sync_separately(&self) -> Result<(), StorageError> {
        #[cfg(test)]
        self.inner.syncs.fetch_add(1, Ordering::Relaxed);
        OpenOptions::new().write(true).open(&self.path)?.sync_data()?;
        Ok(())
    }

    /// How often this writer (any clone) was synced.
    #[cfg(test)]
    pub(crate) fn sync_count(&self) -> usize {
        self.inner.syncs.load(Ordering::Relaxed)
    }
}

/// What a writer knows of the bytes written through it, and the hashes it took of them.
struct Hashes {
    size: u64,
    /// Bytes per block: a power of two, at least one BLAKE3 chunk.
    block: u64,
    state: Mutex<HashState>,
    /// Signalled when the written prefix grows, and when the prefix hasher is to stop.
    changed: Condvar,
    /// Bytes read back to hash.
    #[cfg(test)]
    read: AtomicUsize,
}

struct HashState {
    /// Writes are recorded and hashed (see `DiskWriter::track_digest`).
    tracking: bool,
    /// Written byte ranges: sorted, disjoint and not touching.
    written: Vec<Range<u64>>,
    blocks: Vec<Block>,
    /// SHA-256/MD5 of the written prefix, taken in the background (see `DiskWriter::track_digest`).
    follower: Option<JoinHandle<PrefixHash>>,
    /// How far the prefix hasher has read, or is reading.
    followed: u64,
    /// Bytes it may have read were written again since, so its hashes are void.
    refollow: bool,
    /// The prefix hasher is to stop.
    stopped: bool,
}

#[derive(Clone, Copy, Default)]
struct Block {
    /// Counts the writes into the block, so a hash of older contents is never kept.
    version: u32,
    /// The block's chaining value, or the file's hash when it is the only block; `None` until
    /// the block is hashed.
    hash: Option<ChainingValue>,
}

impl Hashes {
    fn new(size: u64, block: u64) -> Self {
        let state = HashState {
            tracking: false,
            written: Vec::new(),
            blocks: vec![Block::default(); size.div_ceil(block) as usize],
            follower: None,
            followed: 0,
            refollow: false,
            stopped: false,
        };
        Self {
            size,
            block,
            state: Mutex::new(state),
            changed: Condvar::new(),
            #[cfg(test)]
            read: AtomicUsize::new(0),
        }
    }

    /// The bytes of block `i`.
    fn span(&self, i: usize) -> Range<u64> {
        let start = i as u64 * self.block;
        start..(start + self.block).min(self.size)
    }

    /// Hash of block `i`, which holds `data`.
    fn hash_block(&self, i: usize, data: &[u8]) -> ChainingValue {
        if self.size <= self.block {
            return *blake3::hash(data).as_bytes();
        }
        blake3::Hasher::new().set_input_offset(i as u64 * self.block).update(data).finalize_non_root()
    }

    /// Records that `data` was written at `offset`, then hashes each block this leaves written in
    /// full: from `data` where it holds the whole block, else read back from `file` (just
    /// written, so from the page cache). A block that cannot be read back is left for `root`.
    fn wrote(&self, file: &File, offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let end = offset + data.len() as u64;
        let (first, last) = ((offset / self.block) as usize, ((end - 1) / self.block) as usize);
        let mut complete = Vec::new();
        {
            let mut state = self.state.lock();
            let HashState { tracking, written, blocks, followed, refollow, .. } = &mut *state;
            if !*tracking {
                return;
            }
            *refollow |= offset < *followed;
            let prefix = written_prefix(written);
            add(written, offset..end);
            for (i, block) in blocks.iter_mut().enumerate().take(last + 1).skip(first) {
                block.version = block.version.wrapping_add(1);
                block.hash = None;
                if covers(written, &self.span(i)) {
                    complete.push((i, block.version));
                }
            }
            if written_prefix(written) > prefix {
                self.changed.notify_all();
            }
        }
        for (i, version) in complete {
            let span = self.span(i);
            let hash = if offset <= span.start && span.end <= end {
                let at = (span.start - offset) as usize;
                self.hash_block(i, &data[at..at + (span.end - span.start) as usize])
            } else {
                let mut buf = vec![0; (span.end - span.start) as usize];
                #[cfg(test)]
                self.read.fetch_add(buf.len(), Ordering::Relaxed);
                if read_exact_at(file, &mut buf, span.start).is_err() {
                    continue;
                }
                self.hash_block(i, &buf)
            };
            // A later write into the block hashes it again; this hash would be stale.
            let mut state = self.state.lock();
            if state.blocks[i].version == version {
                state.blocks[i].hash = Some(hash);
            }
        }
    }

    /// The file's BLAKE3 hash: the blocks' hashes merged, once the blocks never hashed have been
    /// read back from `file` and hashed, spread over every core.
    fn root(&self, file: &File) -> std::io::Result<[u8; 32]> {
        let mut hashes: Vec<Option<ChainingValue>> = self.state.lock().blocks.iter().map(|b| b.hash).collect();
        let missing: Vec<usize> = (0..hashes.len()).filter(|&i| hashes[i].is_none()).collect();
        let next = AtomicUsize::new(0);
        let threads = std::thread::available_parallelism().map_or(1, usize::from).min(missing.len());
        let found = std::thread::scope(|s| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| {
                        let (mut found, mut buf) = (Vec::new(), Vec::new());
                        while let Some(&i) = missing.get(next.fetch_add(1, Ordering::Relaxed)) {
                            let span = self.span(i);
                            buf.resize((span.end - span.start) as usize, 0);
                            read_exact_at(file, &mut buf, span.start)?;
                            #[cfg(test)]
                            self.read.fetch_add(buf.len(), Ordering::Relaxed);
                            found.push((i, self.hash_block(i, &buf)));
                        }
                        Ok::<_, std::io::Error>(found)
                    })
                })
                .collect();
            workers.into_iter().map(joined).collect::<std::io::Result<Vec<_>>>()
        })?;
        for (i, hash) in found.into_iter().flatten() {
            hashes[i] = Some(hash);
        }
        Ok(merge(hashes.into_iter().flatten().collect()))
    }

    /// Tells the prefix hasher to stop.
    fn stop(&self) {
        self.state.lock().stopped = true;
        self.changed.notify_all();
    }

    /// Stops the prefix hasher and returns what it hashed, unless that was written again since.
    fn stop_follower(&self) -> Option<PrefixHash> {
        let follower = {
            let mut state = self.state.lock();
            state.stopped = true;
            state.follower.take()
        };
        self.changed.notify_all();
        let prefix = follower.and_then(|f| f.join().ok());
        prefix.filter(|_| !self.state.lock().refollow)
    }
}

/// Hashes `file`'s written prefix into `prefix` as the prefix grows, until the whole file is
/// hashed or the hasher is stopped; returns what it hashed.
fn follow(hashes: &Hashes, file: &File, mut prefix: PrefixHash) -> PrefixHash {
    let mut buf = Vec::new();
    loop {
        let end = {
            let mut state = hashes.state.lock();
            loop {
                if state.stopped || prefix.len >= hashes.size {
                    return prefix;
                }
                let end = written_prefix(&state.written).min(prefix.len + FOLLOW_STEP);
                if end > prefix.len {
                    // A write below this from now on may be missed by the read, so it voids the hash.
                    state.followed = end;
                    break end;
                }
                hashes.changed.wait(&mut state);
            }
        };
        buf.resize((end - prefix.len) as usize, 0);
        // `DiskWriter::digest` reads on from here, and reports the error if it persists.
        if read_exact_at(file, &mut buf, prefix.len).is_err() {
            return prefix;
        }
        prefix.update(&buf);
    }
}

/// End of the written prefix of the file `written` describes.
fn written_prefix(written: &[Range<u64>]) -> u64 {
    written.first().filter(|r| r.start == 0).map_or(0, |r| r.end)
}

/// Adds `range` to `set` (sorted, disjoint and not touching), merging whatever it touches.
fn add(set: &mut Vec<Range<u64>>, mut range: Range<u64>) {
    let first = set.partition_point(|r| r.end < range.start);
    let last = set.partition_point(|r| r.start <= range.end);
    if first < last {
        range.start = range.start.min(set[first].start);
        range.end = range.end.max(set[last - 1].end);
    }
    set.splice(first..last, [range]);
}

/// Whether `set` (sorted and disjoint) covers all of `range`.
fn covers(set: &[Range<u64>], range: &Range<u64>) -> bool {
    let i = set.partition_point(|r| r.start <= range.start);
    i > 0 && set[i - 1].end >= range.end
}

/// Merges the hashes of a file's blocks, in order, into the file's hash. Blocks of one
/// power-of-two size form the same tree as the file's chunks (BLAKE3 paper, section 2.1):
/// neighbours pair up level by level, and an odd one out moves up a level as it is.
fn merge(mut level: Vec<ChainingValue>) -> [u8; 32] {
    match level.len() {
        0 => *blake3::hash(&[]).as_bytes(),
        // The only block's hash is already the file's (see `Hashes::hash_block`).
        1 => level[0],
        _ => {
            while level.len() > 2 {
                level = level
                    .chunks(2)
                    .map(|pair| pair.get(1).map_or(pair[0], |right| merge_subtrees_non_root(&pair[0], right, Mode::Hash)))
                    .collect();
            }
            *merge_subtrees_root(&level[0], &level[1], Mode::Hash).as_bytes()
        }
    }
}

/// A scoped thread's result; its panic, should it have panicked, carries on in this thread.
fn joined<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
    handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// SHA-256 and/or MD5 of the first `len` bytes of a file.
struct PrefixHash {
    sha256: Option<Sha256>,
    md5: Option<Md5>,
    len: u64,
}

impl PrefixHash {
    fn new(sha256: bool, md5: bool) -> Self {
        Self { sha256: sha256.then(Sha256::new), md5: md5.then(Md5::new), len: 0 }
    }

    /// Whether it computes every digest asked for.
    fn has(&self, sha256: bool, md5: bool) -> bool {
        (!sha256 || self.sha256.is_some()) && (!md5 || self.md5.is_some())
    }

    fn update(&mut self, data: &[u8]) {
        if let Some(h) = self.sha256.as_mut() {
            h.update(data);
        }
        if let Some(h) = self.md5.as_mut() {
            h.update(data);
        }
        self.len += data.len() as u64;
    }

    /// Hashes on up to `end` of `file`. Blocking.
    fn read_to(&mut self, file: &File, end: u64) -> std::io::Result<()> {
        read_ahead(file, self.len..end, FILE_HASH_BLOCK, |data| self.update(data))
    }
}

/// Why a finished file failed verification.
#[derive(Error, Debug)]
pub enum VerifyError {
    /// The file could not be read.
    #[error("{0}")]
    Io(String),
    /// The file was read but does not match the expected checksum.
    #[error("{0}")]
    Mismatch(String),
}

/// Checks that `expected` is a well-formed digest of a supported algorithm, so input that could
/// never match fails before downloading.
pub fn validate_checksum(expected: &str) -> Result<(), String> {
    Checksum::parse(expected).map(|_| ())
}

/// Whether `expected_checksum` needs SHA-256 and MD5 computed.
fn needs(expected_checksum: Option<&str>) -> Result<(bool, bool), String> {
    let checksum = expected_checksum.map(Checksum::parse).transpose()?;
    Ok(checksum.map_or((false, false), |c| (c.needs_sha256(), c.needs_md5())))
}

/// Hashes a finished file: always BLAKE3 (returned as hex), plus whatever `expected_checksum`
/// needs, all at once (see [`FileDigest::of`]). Opens the file read-only; call it from a blocking
/// context.
pub fn hash_and_verify_file(path: &Path, expected_checksum: Option<&str>) -> Result<String, VerifyError> {
    let (sha256, md5) = needs(expected_checksum).map_err(VerifyError::Io)?;
    let digest = FileDigest::of(path, sha256, md5)
        .map_err(|e| VerifyError::Io(format!("Failed to read {}: {}", path.display(), e)))?;
    verify_digest(&digest, expected_checksum)
}

/// Checks a finished file's `digest` against `expected_checksum`; returns its BLAKE3 as hex. A
/// digest without the algorithm the checksum names does not match it.
pub fn verify_digest(digest: &FileDigest, expected_checksum: Option<&str>) -> Result<String, VerifyError> {
    let checksum = expected_checksum.map(Checksum::parse).transpose().map_err(VerifyError::Io)?;
    match checksum {
        Some(c) if !c.matches(digest) => Err(VerifyError::Mismatch(format!(
            "Checksum verification failed: expected {} {}, got {}",
            c.algo.name(),
            c.hex,
            c.actual(digest),
        ))),
        _ => Ok(digest.blake3_hex.clone()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Algo {
    Sha256,
    Md5,
    Blake3,
    /// A bare 64-digit digest: SHA-256 and BLAKE3 digests look alike, so either may match.
    Sha256OrBlake3,
}

impl Algo {
    fn name(self) -> &'static str {
        match self {
            Algo::Sha256 => "SHA-256",
            Algo::Md5 => "MD5",
            Algo::Blake3 => "BLAKE3",
            Algo::Sha256OrBlake3 => "SHA-256 or BLAKE3",
        }
    }

    fn hex_len(self) -> usize {
        match self {
            Algo::Md5 => 32,
            Algo::Sha256 | Algo::Blake3 | Algo::Sha256OrBlake3 => 64,
        }
    }
}

struct Checksum {
    algo: Algo,
    hex: String,
}

impl Checksum {
    /// Accepts `sha256:`, `md5:` or `blake3:` followed by the digest, or a bare digest of 32 (MD5)
    /// or 64 (SHA-256 or BLAKE3) hex digits. Rejects anything no supported algorithm can produce.
    fn parse(expected: &str) -> Result<Self, String> {
        let trimmed = expected.trim();
        let (algo, hex) = match trimmed.split_once(':') {
            Some((prefix, hash)) => {
                let algo = match prefix.trim().to_ascii_lowercase().as_str() {
                    "sha256" | "sha-256" => Algo::Sha256,
                    "md5" => Algo::Md5,
                    "blake3" => Algo::Blake3,
                    other => return Err(format!("Unsupported checksum algorithm: {}", other)),
                };
                (Some(algo), hash.trim())
            }
            None => (None, trimmed),
        };
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "Checksum {:?} is not a hex digest; give only the hash (e.g. sha256:<64 hex digits>), not a whole checksum-file line",
                hex
            ));
        }
        let algo = match algo {
            Some(algo) => algo,
            None => match hex.len() {
                32 => Algo::Md5,
                64 => Algo::Sha256OrBlake3,
                n => {
                    return Err(format!(
                        "Unsupported checksum of {} hex digits: use MD5 (32) or SHA-256/BLAKE3 (64), optionally prefixed with md5:, sha256: or blake3:",
                        n
                    ))
                }
            },
        };
        if hex.len() != algo.hex_len() {
            return Err(format!("A {} checksum has {} hex digits, not {}", algo.name(), algo.hex_len(), hex.len()));
        }
        Ok(Self { algo, hex: hex.to_ascii_lowercase() })
    }

    fn needs_sha256(&self) -> bool {
        matches!(self.algo, Algo::Sha256 | Algo::Sha256OrBlake3)
    }

    fn needs_md5(&self) -> bool {
        self.algo == Algo::Md5
    }

    fn actual(&self, d: &FileDigest) -> String {
        let sha256 = d.sha256.as_deref().unwrap_or_default();
        match self.algo {
            Algo::Sha256 => sha256.to_string(),
            Algo::Md5 => d.md5.clone().unwrap_or_default(),
            Algo::Blake3 => d.blake3_hex.clone(),
            Algo::Sha256OrBlake3 => format!("SHA-256 {} / BLAKE3 {}", sha256, d.blake3_hex),
        }
    }

    fn matches(&self, d: &FileDigest) -> bool {
        let hex = Some(self.hex.as_str());
        let blake3 = Some(d.blake3_hex.as_str());
        match self.algo {
            Algo::Sha256 => d.sha256.as_deref() == hex,
            Algo::Md5 => d.md5.as_deref() == hex,
            Algo::Blake3 => blake3 == hex,
            Algo::Sha256OrBlake3 => d.sha256.as_deref() == hex || blake3 == hex,
        }
    }
}

/// Digests of a whole file: BLAKE3 always, SHA-256 and MD5 when a checksum needed them. Check it
/// with [`verify_digest`].
#[derive(Clone, Debug)]
pub struct FileDigest {
    blake3: [u8; 32],
    blake3_hex: String,
    sha256: Option<String>,
    md5: Option<String>,
}

impl FileDigest {
    fn new(blake3: [u8; 32], rest: PrefixHash) -> Self {
        Self {
            blake3,
            blake3_hex: hex::encode(&blake3),
            sha256: rest.sha256.map(|h| hex::encode(&h.finalize())),
            md5: rest.md5.map(|h| hex::encode(&h.finalize())),
        }
    }

    /// The BLAKE3 hash as lowercase hex, as history records it.
    pub fn blake3_hex(&self) -> &str {
        &self.blake3_hex
    }

    /// Hashes the file at `path` without writing to it (see [`FileDigest::of_file`]). Blocking.
    fn of(path: &Path, sha256: bool, md5: bool) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Self::of_file(&file, len, FILE_HASH_BLOCK, sha256, md5)
    }

    /// Hashes the first `len` bytes of `file` in one sequential pass of `block`-sized reads, the
    /// next block read on another thread while this one is hashed: BLAKE3 on every core and the
    /// requested SHA-256/MD5 on one more thread, so the pass takes about as long as the slowest of
    /// reading, BLAKE3 and SHA-256/MD5. A read error, or a file shorter than `len`, is an `Err`.
    fn of_file(file: &File, len: u64, block: usize, sha256: bool, md5: bool) -> std::io::Result<Self> {
        let mut blake = blake3::Hasher::new();
        let mut rest = PrefixHash::new(sha256, md5);
        read_ahead(file, 0..len, block, |data| {
            std::thread::scope(|s| {
                if sha256 || md5 {
                    s.spawn(|| rest.update(data));
                }
                blake.update_rayon(data);
            })
        })?;
        Ok(Self::new(*blake.finalize().as_bytes(), rest))
    }
}

/// Digest of a file written from start to end, taken as it is written so that nothing has to be
/// read back: BLAKE3, plus SHA-256/MD5 when the expected checksum needs them. For in-order
/// writers (a single stream, HLS), which hand [`StreamHasher::finish`] to the engine instead of
/// the finished file being read back.
pub struct StreamHasher {
    blake3: blake3::Hasher,
    rest: PrefixHash,
}

impl StreamHasher {
    /// A hasher for a download whose expected checksum is `expected_checksum`. One that does not
    /// parse needs no SHA-256/MD5 (the download reports it when verifying).
    pub fn new(expected_checksum: Option<&str>) -> Self {
        let (sha256, md5) = needs(expected_checksum).unwrap_or_default();
        Self { blake3: blake3::Hasher::new(), rest: PrefixHash::new(sha256, md5) }
    }

    /// Hashes the file's next bytes. CPU-bound, about 1 GB/s: large pieces belong on a blocking
    /// thread, like the write they follow.
    pub fn update(&mut self, data: &[u8]) {
        self.blake3.update(data);
        self.rest.update(data);
    }

    /// Bytes hashed so far.
    pub fn position(&self) -> u64 {
        self.rest.len
    }

    /// Hashes `[self.position(), end)` of `file`, such as the part a resumed download already
    /// has. Blocking.
    pub fn update_from_file(&mut self, file: &File, end: u64) -> std::io::Result<()> {
        let Self { blake3, rest } = self;
        read_ahead(file, rest.len..end, FILE_HASH_BLOCK, |data| {
            blake3.update_rayon(data);
            rest.update(data);
        })
    }

    pub fn finish(self) -> FileDigest {
        FileDigest::new(*self.blake3.finalize().as_bytes(), self.rest)
    }
}

/// Streams `range` of `file` through `f` in `block`-sized pieces, in order, reading the next
/// piece on another thread while `f` runs. Two buffers take turns, so at most two blocks are in
/// memory.
fn read_ahead(file: &File, range: Range<u64>, block: usize, mut f: impl FnMut(&[u8])) -> std::io::Result<()> {
    // No slot: the reader hands over a block only when `f` is ready for it.
    let (full_tx, full_rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(0);
    let (empty_tx, empty_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::scope(|s| {
        s.spawn(move || {
            let mut pos = range.start;
            while pos < range.end {
                let n = (range.end - pos).min(block as u64) as usize;
                let mut buf = empty_rx.try_recv().unwrap_or_default();
                buf.resize(n, 0);
                let read = read_exact_at(file, &mut buf, pos).map(|()| buf);
                let failed = read.is_err();
                // A closed channel means the consumer stopped early.
                if full_tx.send(read).is_err() || failed {
                    return;
                }
                pos += n as u64;
            }
        });
        // Returning early drops `full_rx`, which stops the reader; the scope then joins it.
        for data in full_rx {
            let data = data?;
            f(&data);
            let _ = empty_tx.send(data);
        }
        Ok(())
    })
}

/// Streams `[offset, offset + len)` of `file` through `f` in `HASH_BUF_SIZE` blocks.
fn read_blocks(file: &File, offset: u64, len: u64, mut f: impl FnMut(&[u8])) -> std::io::Result<()> {
    let mut buf = vec![0u8; HASH_BUF_SIZE.min(len as usize)];
    let mut pos = offset;
    let end = offset + len;
    while pos < end {
        let n = (end - pos).min(buf.len() as u64) as usize;
        read_exact_at(file, &mut buf[..n], pos)?;
        f(&buf[..n]);
        pos += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_at(file: &File, data: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, data, offset)
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut data: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !data.is_empty() {
        match file.seek_write(data, offset) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                data = &data[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl From<crate::range::RangeError> for StorageError {
    fn from(_err: crate::range::RangeError) -> Self {
        StorageError::OutOfBounds(0, 0, 0)
    }
}

/// Marks the file sparse, then sets its length. On a non-sparse NTFS file a write beyond the
/// valid data length first zero-fills everything up to its offset, inside that write: the first
/// far-offset chunk write would write most of the file twice and stall every other writer.
/// Sparse files skip that; disk space is allocated as data arrives.
#[cfg(windows)]
fn preallocate_file(file: &File, size: u64) -> Result<(), StorageError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let mut returned = 0u32;
    // SAFETY: the handle is owned by `file` and outlives this synchronous call. No buffers are
    // passed (no input buffer means "make sparse"); `returned` is required without OVERLAPPED.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        // FAT32/exFAT have no sparse files; writes past the valid data length zero-fill there.
        tracing::debug!("FSCTL_SET_SPARSE failed: {}", std::io::Error::last_os_error());
    }
    file.set_len(size)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn preallocate_file(file: &File, size: u64) -> Result<(), StorageError> {
    use std::os::unix::io::AsRawFd;

    file.set_len(size)?;
    if size == 0 {
        return Ok(());
    }
    let len = libc::off_t::try_from(size)
        .map_err(|_| StorageError::PreallocationFailed(format!("{} bytes exceeds off_t", size)))?;

    // Call fallocate(2) directly: glibc's posix_fallocate emulates it by writing every block on
    // filesystems without support, which takes minutes for multi-GB files.
    loop {
        // SAFETY: plain syscall on a valid, owned file descriptor.
        let ret = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, len) };
        if ret == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            // Not supported by this filesystem: the sparse file from set_len has to do.
            Some(libc::EOPNOTSUPP) | Some(libc::EINVAL) | Some(libc::ENOSYS) => return Ok(()),
            _ => {
                return Err(StorageError::PreallocationFailed(format!(
                    "cannot reserve {} bytes: {}",
                    size, err
                )))
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn preallocate_file(file: &File, size: u64) -> Result<(), StorageError> {
    use std::os::unix::io::AsRawFd;

    file.set_len(size)?;
    if size == 0 {
        return Ok(());
    }
    let fd = file.as_raw_fd();
    // SAFETY: fstore_t is plain data and fd is a valid, owned descriptor. Failure is non-fatal:
    // set_len already reserved the logical size.
    unsafe {
        let mut fst: libc::fstore_t = std::mem::zeroed();
        fst.fst_flags = libc::F_ALLOCATECONTIG;
        fst.fst_posmode = libc::F_PEOFPOSMODE;
        fst.fst_offset = 0;
        fst.fst_length = size as libc::off_t;
        if libc::fcntl(fd, libc::F_PREALLOCATE, &fst) == -1 {
            fst.fst_flags = libc::F_ALLOCATEALL;
            libc::fcntl(fd, libc::F_PREALLOCATE, &fst);
        }
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn preallocate_file(file: &File, size: u64) -> Result<(), StorageError> {
    file.set_len(size)?;
    Ok(())
}

// Simple hex helper to avoid extra dependency if not present
mod hex {
    pub fn encode(data: &[u8]) -> String {
        data.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_disk_writer_write_and_hash() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();
        let total_size = 1024;

        let writer = DiskWriter::open_or_create(&path, total_size).unwrap();
        assert_eq!(writer.size(), total_size);

        // Write first half
        let data1 = vec![0xAB; 512];
        writer.write_chunk_slice(0, &data1).unwrap();

        // Write second half
        let data2 = vec![0xCD; 512];
        writer.write_chunk_slice(512, &data2).unwrap();

        writer.sync().unwrap();

        // Compute BLAKE3 hashes
        let range1 = ByteRange::new(0, 511).unwrap();
        let hash1 = writer.compute_chunk_hash(&range1).unwrap();

        let mut expected_hasher = blake3::Hasher::new();
        expected_hasher.update(&data1);
        let expected_hash1 = *expected_hasher.finalize().as_bytes();

        assert_eq!(hash1, expected_hash1);

        let whole = [data1, data2].concat();
        assert_eq!(writer.compute_file_hash().unwrap(), *blake3::hash(&whole).as_bytes());
        assert!(matches!(writer.write_chunk_slice(1000, &[0u8; 25]), Err(StorageError::OutOfBounds(..))));
    }

    #[test]
    fn test_disk_writer_zero_byte() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();

        let writer = DiskWriter::open_or_create(&path, 0).unwrap();
        assert_eq!(writer.size(), 0);

        writer.sync().unwrap();
        let hash = writer.compute_file_hash().unwrap();
        let expected = *blake3::hash(&[]).as_bytes();
        assert_eq!(hash, expected);
    }

    #[test]
    fn test_disk_writer_checksums() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();
        let writer = DiskWriter::open_or_create(&path, 5).unwrap();
        writer.write_chunk_slice(0, b"hello").unwrap();
        writer.sync().unwrap();

        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let sha256 = writer.compute_sha256().unwrap();
        assert_eq!(sha256, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");

        // md5("hello") = 5d41402abc4b2a76b9719d911017c592
        let md5 = writer.compute_md5().unwrap();
        assert_eq!(md5, "5d41402abc4b2a76b9719d911017c592");

        assert!(writer.verify_checksum("sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824").unwrap());
        assert!(writer.verify_checksum("md5:5d41402abc4b2a76b9719d911017c592").unwrap());
        assert!(writer.verify_checksum("5d41402abc4b2a76b9719d911017c592").unwrap()); // auto-detect md5
        assert!(writer.verify_checksum("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824").unwrap()); // auto-detect sha256
        // A mismatch is Ok(false); an unknown algorithm or a malformed digest is an error.
        assert!(!writer.verify_checksum("md5:00000000000000000000000000000000").unwrap());
        assert!(writer.verify_checksum("md5:wronghash").is_err());
        assert!(writer.verify_checksum("crc32:3610a686").is_err());
    }

    #[test]
    fn test_hash_and_verify_file_single_pass() {
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), b"hello").unwrap();
        let blake = hash_and_verify_file(temp.path(), None).unwrap();
        assert_eq!(blake, blake3::hash(b"hello").to_hex().to_string());

        let sha = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(hash_and_verify_file(temp.path(), Some(sha)).unwrap(), blake);
        let err = hash_and_verify_file(temp.path(), Some("md5:00000000000000000000000000000000")).unwrap_err();
        assert!(matches!(&err, VerifyError::Mismatch(m) if m.contains("5d41402abc4b2a76b9719d911017c592")), "{err}");
        assert!(matches!(hash_and_verify_file(&temp.path().with_extension("missing"), None), Err(VerifyError::Io(_))));
        assert!(validate_checksum("crc32:3610a686").is_err());
    }

    #[test]
    fn test_parallel_hashes_equal_single_threaded_ones() {
        // Big enough for BLAKE3 to split across threads.
        let data: Vec<u8> = (0..9 * 1024 * 1024 + 4099u32).map(|i| (i ^ (i >> 11)).wrapping_mul(31) as u8).collect();
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), &data).unwrap();
        let blake = blake3::hash(&data).to_hex().to_string();
        let sha = hex::encode(&Sha256::digest(&data));
        let md5 = hex::encode(&Md5::digest(&data));

        assert_eq!(hash_and_verify_file(temp.path(), None).unwrap(), blake);
        assert_eq!(hash_and_verify_file(temp.path(), Some(&format!("sha256:{sha}"))).unwrap(), blake);
        assert_eq!(hash_and_verify_file(temp.path(), Some(&format!("md5:{md5}"))).unwrap(), blake);
        assert_eq!(hash_and_verify_file(temp.path(), Some(&blake)).unwrap(), blake);
        let err = hash_and_verify_file(temp.path(), Some(&format!("sha256:{}", "0".repeat(64)))).unwrap_err();
        assert!(matches!(&err, VerifyError::Mismatch(m) if m.contains(&sha)), "{err}");
        std::fs::write(temp.path(), &data[..100]).unwrap();
        assert_eq!(hash_and_verify_file(temp.path(), None).unwrap(), blake3::hash(&data[..100]).to_hex().to_string());

        // Many blocks, a partial last one, and buffers handed back and forth between the threads.
        std::fs::write(temp.path(), &data).unwrap();
        let file = File::open(temp.path()).unwrap();
        let d = FileDigest::of_file(&file, data.len() as u64, 1024 * 1024 + 17, true, true).unwrap();
        assert_eq!((d.blake3_hex, d.sha256, d.md5), (blake, Some(sha), Some(md5)));
    }

    #[test]
    fn test_file_shorter_than_its_length_is_a_read_error_not_a_crash() {
        // As when another program truncates the file while it is hashed (which a memory-mapped
        // hash would answer with SIGBUS): the missing bytes are a read error.
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), vec![5u8; 3 * 4096 + 1]).unwrap();
        let file = File::open(temp.path()).unwrap();
        for block in [4096, FILE_HASH_BLOCK] {
            let err = FileDigest::of_file(&file, 8 * 4096, block, true, false).expect_err("truncated file hashed");
            assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        }
    }

    #[test]
    fn test_checksums_that_can_never_match_are_rejected_up_front() {
        let sums_line = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  hello.txt";
        let sha512 = "ab".repeat(64);
        for bad in [
            "da39a3ee5e6b4b0d3255bfef95601890afd80709", // SHA-1
            sha512.as_str(),
            sums_line,
            "md5:wronghash",
            "md5:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            "sha256:5d41402abc4b2a76b9719d911017c592",
            "blake3:",
            "",
        ] {
            assert!(validate_checksum(bad).is_err(), "{bad:?} was accepted");
        }
        for good in [
            "5d41402abc4b2a76b9719d911017c592",
            "SHA256:2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824",
            " blake3:ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f ",
        ] {
            assert!(validate_checksum(good).is_ok(), "{good:?} was rejected");
        }
    }

    #[test]
    fn test_bare_64_hex_matches_sha256_or_blake3() {
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), b"hello").unwrap();
        let blake = blake3::hash(b"hello").to_hex().to_string();
        let sha = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        // The app logs and records bare BLAKE3 digests; pasting one back must verify.
        assert_eq!(DiskWriter::verify_file_checksum(temp.path(), &blake), Ok(true));
        assert_eq!(hash_and_verify_file(temp.path(), Some(&blake)).unwrap(), blake);
        assert_eq!(DiskWriter::verify_file_checksum(temp.path(), sha), Ok(true));
        assert_eq!(DiskWriter::verify_file_checksum(temp.path(), &"0".repeat(64)), Ok(false));
        // An explicit prefix still means exactly that algorithm.
        assert_eq!(DiskWriter::verify_file_checksum(temp.path(), &format!("sha256:{blake}")), Ok(false));
    }

    /// Without the sparse flag NTFS zero-fills up to the offset of the first far write, inside
    /// that write, stalling every other writer of the file.
    #[cfg(windows)]
    #[test]
    fn test_preallocated_file_is_sparse_on_windows() {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
        const SIZE: u64 = 4 << 30;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.part");
        let writer = DiskWriter::open_or_create(&path, SIZE).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), SIZE);
        assert_ne!(meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE, 0, "the .part must be sparse");

        writer.write_chunk_slice(SIZE - 4096, &[7u8; 4096]).unwrap();
        writer.write_chunk_slice(0, &[1u8; 4096]).unwrap();
        writer.sync().unwrap();
        let mut tail = [0u8; 4096];
        read_exact_at(&File::open(&path).unwrap(), &mut tail, SIZE - 4096).unwrap();
        assert_eq!(tail, [7u8; 4096]);
    }

    #[test]
    fn test_verify_file_checksum_is_read_only() {
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), b"hello").unwrap();
        let mut perms = std::fs::metadata(temp.path()).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(temp.path(), perms.clone()).unwrap();

        // Opening read-write (the old implementation) fails on a read-only file.
        let res = DiskWriter::verify_file_checksum(temp.path(), "md5:5d41402abc4b2a76b9719d911017c592");

        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(temp.path(), perms).unwrap();
        assert_eq!(res, Ok(true));
        assert_eq!(std::fs::read(temp.path()).unwrap(), b"hello");
    }

    #[test]
    fn test_concurrent_positional_writes() {
        let temp = NamedTempFile::new().unwrap();
        let writer = DiskWriter::open_or_create(temp.path(), 8 * 4096).unwrap();
        std::thread::scope(|s| {
            for i in 0..8u8 {
                let w = writer.clone();
                s.spawn(move || w.write_chunk_slice(i as u64 * 4096, &[i; 4096]).unwrap());
            }
        });
        writer.sync().unwrap();
        let data = std::fs::read(temp.path()).unwrap();
        for i in 0..8usize {
            assert!(data[i * 4096..(i + 1) * 4096].iter().all(|&b| b == i as u8));
        }
    }

    /// xorshift64*: reproducible sizes, contents and orders.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    #[test]
    fn test_hashes_taken_while_writing_equal_the_file_hash() {
        let dir = tempfile::tempdir().unwrap();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..160u64 {
            // 1-8 KiB blocks, so that small files span many; sizes around block multiples too.
            let block = 1024u64 << rng.below(4);
            let size = match round % 5 {
                0 => rng.below(2 * block + 2),
                1 => block * rng.below(33),
                _ => rng.below(40 * block),
            };
            let data: Vec<u8> = (0..size).map(|_| rng.next() as u8).collect();
            let bytes = |r: Range<u64>| data[r.start as usize..r.end as usize].to_vec();

            // Pieces of random length; an earlier run wrote some of them, the rest of its file is junk.
            let mut pieces = Vec::new();
            while pieces.last().map_or(0, |p: &Range<u64>| p.end) < size {
                let start = pieces.last().map_or(0, |p: &Range<u64>| p.end);
                pieces.push(start..(start + 1 + rng.below(3 * block)).min(size));
            }
            let mut on_disk = vec![0xA5u8; size as usize];
            let mut earlier = Vec::new();
            pieces.retain(|p| {
                let resumed = rng.below(4) == 0;
                if resumed {
                    on_disk[p.start as usize..p.end as usize].copy_from_slice(&bytes(p.clone()));
                    earlier.push(ByteRange::new(p.start, p.end - 1).unwrap());
                }
                !resumed
            });
            let path = dir.path().join(format!("{round}.part"));
            std::fs::write(&path, &on_disk).unwrap();

            // One thread writes each unit in order: a piece, perhaps after other bytes in its place
            // (a retry after a bad write) or followed by part of it again (a steal).
            let mut units: Vec<Vec<(u64, Vec<u8>)>> = pieces
                .iter()
                .map(|p| match rng.below(4) {
                    0 => vec![(p.start, vec![0x5A; (p.end - p.start) as usize]), (p.start, bytes(p.clone()))],
                    1 => {
                        let from = p.start + rng.below(p.end - p.start);
                        let to = from + 1 + rng.below(p.end - from);
                        vec![(p.start, bytes(p.clone())), (from, bytes(from..to))]
                    }
                    _ => vec![(p.start, bytes(p.clone()))],
                })
                .collect();
            for i in (1..units.len()).rev() {
                units.swap(i, rng.below(i as u64 + 1) as usize);
            }

            // Without being told about the earlier run, the writer reads its blocks back at the end.
            let told: &[ByteRange] = if round % 3 == 0 { &[] } else { &earlier };
            let checksum = match round % 4 {
                0 => Some(format!("sha256:{}", hex::encode(&Sha256::digest(&data)))),
                1 => Some(format!("md5:{}", hex::encode(&Md5::digest(&data)))),
                _ => None,
            };
            let writer = DiskWriter::open(&path, size, block).unwrap();
            writer.track_digest(told, checksum.as_deref());
            let threads = 1 + rng.below(4) as usize;
            std::thread::scope(|s| {
                for t in 0..threads {
                    let (writer, units) = (&writer, &units);
                    s.spawn(move || {
                        for (offset, piece) in units.iter().skip(t).step_by(threads).flatten() {
                            writer.write_chunk_slice(*offset, piece).unwrap();
                        }
                    });
                }
            });

            let what = format!("round {round}: {size} bytes in {block}-byte blocks, {threads} thread(s)");
            let digest = writer.digest(checksum.as_deref()).unwrap();
            assert_eq!(digest.blake3, *blake3::hash(&data).as_bytes(), "{what}");
            assert!(verify_digest(&digest, checksum.as_deref()).is_ok(), "{what}: {checksum:?}");
        }
    }

    #[test]
    fn test_what_was_hashed_while_writing_is_not_read_again() {
        let data: Vec<u8> = (0..300 * 1024u32).map(|i| (i % 251) as u8).collect();
        let checksum = format!("sha256:{}", hex::encode(&Sha256::digest(&data)));
        let half = data.len() / 2;
        let temp = NamedTempFile::new().unwrap();
        let writer = DiskWriter::open_or_create(temp.path(), data.len() as u64).unwrap();
        writer.track_digest(&[], Some(&checksum));
        // The back half first: there is no prefix to follow until the front arrives.
        writer.write_chunk_slice(half as u64, &data[half..]).unwrap();
        writer.write_chunk_slice(0, &data[..half]).unwrap();
        let started = std::time::Instant::now();
        while !writer.inner.hashes.state.lock().follower.as_ref().is_some_and(|f| f.is_finished()) {
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "the prefix was never hashed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Change the file behind the writer's back: its digest does not notice, a full read does.
        let other = File::options().write(true).open(temp.path()).unwrap();
        write_all_at(&other, &vec![0u8; data.len()], 0).unwrap();
        let digest = writer.digest(Some(&checksum)).unwrap();
        assert!(verify_digest(&digest, Some(&checksum)).is_ok());
        assert_eq!(digest.blake3, *blake3::hash(&data).as_bytes());
        let reread = writer.full_digest(Some(&checksum)).unwrap();
        assert!(matches!(verify_digest(&reread, Some(&checksum)), Err(VerifyError::Mismatch(_))));
    }

    /// Bytes with no short period, so that blocks in the wrong place change the hash.
    fn pattern(len: u64) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8).collect()
    }

    #[test]
    fn test_a_writer_never_asked_for_a_digest_hashes_nothing() {
        // As a repair writes: no digest follows, so nothing is recorded, read back or hashed.
        let data = pattern(600 * 1024);
        let temp = NamedTempFile::new().unwrap();
        let writer = DiskWriter::open_or_create(temp.path(), data.len() as u64).unwrap();
        writer.write_chunk_slice(100_000, &data[100_000..]).unwrap();
        writer.write_chunk_slice(0, &data[..100_000]).unwrap();
        {
            let state = writer.inner.hashes.state.lock();
            assert!(state.written.is_empty() && state.blocks.iter().all(|b| b.hash.is_none()));
        }
        assert_eq!(writer.inner.hashes.read.load(Ordering::Relaxed), 0);
        // Its digest reads the file instead.
        assert_eq!(writer.digest(None).unwrap().blake3, *blake3::hash(&data).as_bytes());
    }

    #[test]
    fn test_prefix_hashing_stops_with_the_last_clone_of_the_writer() {
        let temp = NamedTempFile::new().unwrap();
        let writer = DiskWriter::open_or_create(temp.path(), 4096).unwrap();
        writer.track_digest(&[], Some("md5:5d41402abc4b2a76b9719d911017c592"));
        // No prefix yet, so the hasher waits for one.
        writer.write_chunk_slice(1024, &[1; 1024]).unwrap();
        let hashes = Arc::downgrade(&writer.inner.hashes);
        drop(writer);

        let started = std::time::Instant::now();
        while hashes.upgrade().is_some() {
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "the prefix hasher outlived its download");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn test_stream_hasher_equals_reading_the_file() {
        let data: Vec<u8> = (0..1024 * 1024 + 777u32).map(|i| (i ^ (i >> 9)).wrapping_mul(7) as u8).collect();
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), &data).unwrap();
        let blake3 = blake3::hash(&data).to_hex().to_string();
        let sha256 = format!("sha256:{}", hex::encode(&Sha256::digest(&data)));
        let md5 = format!("md5:{}", hex::encode(&Md5::digest(&data)));

        for checksum in [None, Some(sha256.as_str()), Some(md5.as_str())] {
            let mut hasher = StreamHasher::new(checksum);
            for piece in data.chunks(64 * 1024 + 3) {
                hasher.update(piece);
            }
            assert_eq!(verify_digest(&hasher.finish(), checksum).unwrap(), blake3, "{checksum:?}");

            // A resumed download hashes what it already has, then the rest as it arrives.
            let mut resumed = StreamHasher::new(checksum);
            resumed.update_from_file(&File::open(temp.path()).unwrap(), 300_001).unwrap();
            assert_eq!(resumed.position(), 300_001);
            resumed.update(&data[300_001..]);
            assert_eq!(verify_digest(&resumed.finish(), checksum).unwrap(), blake3, "{checksum:?}");
        }
        let wrong = format!("md5:{}", "0".repeat(32));
        assert!(matches!(verify_digest(&StreamHasher::new(Some(&wrong)).finish(), Some(&wrong)), Err(VerifyError::Mismatch(_))));
    }
}
