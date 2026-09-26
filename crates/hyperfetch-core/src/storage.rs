use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// Preallocated output file shared by all workers. Writes are positional (`pwrite` /
/// `WriteFile` with an offset), so concurrent writers never share a cursor or a mapping.
#[derive(Clone)]
pub struct DiskWriter {
    path: PathBuf,
    size: u64,
    file: Arc<File>,
}

impl DiskWriter {
    /// Opens or creates the file and sets its length to `total_size`. On Linux and macOS the space
    /// is reserved, so a full disk is reported here; on Windows the file is sparse and a full disk
    /// surfaces as a write error.
    pub fn open_or_create(path: impl AsRef<Path>, total_size: u64) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();

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
            file: Arc::new(file),
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
        write_all_at(&self.file, data, offset)?;
        Ok(())
    }

    /// Computes BLAKE3 hash for a specific range.
    pub fn compute_chunk_hash(&self, range: &ByteRange) -> Result<[u8; 32], StorageError> {
        if range.end >= self.size {
            return Err(StorageError::OutOfBounds(range.start, range.len() as usize, self.size));
        }
        let mut hasher = blake3::Hasher::new();
        read_blocks(&self.file, range.start, range.len(), |block| {
            hasher.update(block);
        })?;
        Ok(*hasher.finalize().as_bytes())
    }

    /// Computes BLAKE3 root hash for the entire file.
    pub fn compute_file_hash(&self) -> Result<[u8; 32], StorageError> {
        Ok(Digests::of(&self.file, self.size, false, false)?.blake3)
    }

    /// Computes SHA-256 hash for the entire file as a lowercase hex string.
    pub fn compute_sha256(&self) -> Result<String, StorageError> {
        Ok(Digests::of(&self.file, self.size, true, false)?.sha256.unwrap_or_default())
    }

    /// Computes MD5 hash for the entire file as a lowercase hex string.
    pub fn compute_md5(&self) -> Result<String, StorageError> {
        Ok(Digests::of(&self.file, self.size, false, true)?.md5.unwrap_or_default())
    }

    /// Verifies the file against an expected checksum ("sha256:...", "md5:...", "blake3:...", or raw hex).
    /// `Ok(false)` means the hash does not match; `Err` means the file could not be read or the
    /// algorithm prefix is unknown.
    pub fn verify_checksum(&self, expected: &str) -> Result<bool, String> {
        let checksum = Checksum::parse(expected)?;
        let digests = Digests::of(&self.file, self.size, checksum.needs_sha256(), checksum.needs_md5())
            .map_err(|e| e.to_string())?;
        Ok(checksum.matches(&digests))
    }

    /// Verifies an existing file against an expected checksum. Opens the file read-only and never modifies it.
    pub fn verify_file_checksum(path: &Path, expected: &str) -> Result<bool, String> {
        let checksum = Checksum::parse(expected)?;
        let (file, len) = open_read_only(path)?;
        let digests = Digests::of(&file, len, checksum.needs_sha256(), checksum.needs_md5())
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
        self.file.sync_data()?;
        Ok(())
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

/// Hashes a finished file in a single read pass: always BLAKE3 (returned as hex), plus whatever
/// `expected_checksum` needs. Opens the file read-only; call it from a blocking context.
pub fn hash_and_verify_file(path: &Path, expected_checksum: Option<&str>) -> Result<String, VerifyError> {
    let checksum = expected_checksum.map(Checksum::parse).transpose().map_err(VerifyError::Io)?;
    let (file, len) = open_read_only(path).map_err(VerifyError::Io)?;
    let digests = Digests::of(
        &file,
        len,
        checksum.as_ref().is_some_and(Checksum::needs_sha256),
        checksum.as_ref().is_some_and(Checksum::needs_md5),
    )
    .map_err(|e| VerifyError::Io(format!("Failed to read {}: {}", path.display(), e)))?;

    match checksum {
        Some(c) if !c.matches(&digests) => Err(VerifyError::Mismatch(format!(
            "Checksum verification failed: expected {} {}, got {}",
            c.algo.name(),
            c.hex,
            c.actual(&digests),
        ))),
        _ => Ok(digests.blake3_hex),
    }
}

fn open_read_only(path: &Path) -> Result<(File, u64), String> {
    let file = File::open(path).map_err(|e| format!("Failed to open {}: {}", path.display(), e))?;
    let len = file
        .metadata()
        .map_err(|e| format!("Failed to read file metadata: {}", e))?
        .len();
    Ok((file, len))
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

    fn actual(&self, d: &Digests) -> String {
        let sha256 = d.sha256.as_deref().unwrap_or_default();
        match self.algo {
            Algo::Sha256 => sha256.to_string(),
            Algo::Md5 => d.md5.clone().unwrap_or_default(),
            Algo::Blake3 => d.blake3_hex.clone(),
            Algo::Sha256OrBlake3 => format!("SHA-256 {} / BLAKE3 {}", sha256, d.blake3_hex),
        }
    }

    fn matches(&self, d: &Digests) -> bool {
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

struct Digests {
    blake3: [u8; 32],
    blake3_hex: String,
    sha256: Option<String>,
    md5: Option<String>,
}

impl Digests {
    /// Reads `[0, len)` once, feeding every requested hasher.
    fn of(file: &File, len: u64, sha256: bool, md5: bool) -> Result<Self, StorageError> {
        let mut blake = blake3::Hasher::new();
        let mut sha = sha256.then(Sha256::new);
        let mut md = md5.then(Md5::new);
        read_blocks(file, 0, len, |block| {
            blake.update(block);
            if let Some(h) = sha.as_mut() {
                h.update(block);
            }
            if let Some(h) = md.as_mut() {
                h.update(block);
            }
        })?;
        let blake3 = *blake.finalize().as_bytes();
        Ok(Self {
            blake3,
            blake3_hex: hex::encode(&blake3),
            sha256: sha.map(|h| hex::encode(&h.finalize())),
            md5: md.map(|h| hex::encode(&h.finalize())),
        })
    }
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
}
