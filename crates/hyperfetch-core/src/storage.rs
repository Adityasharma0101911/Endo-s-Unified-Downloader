use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use memmap2::MmapMut;
use thiserror::Error;
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

/// A zero-copy, thread-safe memory-mapped storage buffer that permits
/// concurrent writes to non-overlapping byte ranges.
pub struct ConcurrentMmap {
    ptr: *mut u8,
    len: usize,
    _mmap: Option<MmapMut>,
}

unsafe impl Send for ConcurrentMmap {}
unsafe impl Sync for ConcurrentMmap {}

impl ConcurrentMmap {
    pub fn new(mmap: MmapMut) -> Self {
        let len = mmap.len();
        let ptr = mmap.as_ptr() as *mut u8;
        Self {
            ptr,
            len,
            _mmap: Some(mmap),
        }
    }

    pub fn empty() -> Self {
        Self {
            ptr: std::ptr::NonNull::dangling().as_ptr(),
            len: 0,
            _mmap: None,
        }
    }

    /// Writes data directly at the specified offset.
    /// Safety: Callers must guarantee that concurrent calls write to disjoint byte ranges.
    pub unsafe fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        let data_len = data.len();
        if data_len == 0 {
            return Ok(());
        }
        let offset = offset as usize;
        if offset.checked_add(data_len).map_or(true, |end| end > self.len) {
            return Err(StorageError::OutOfBounds(offset as u64, data_len, self.len as u64));
        }

        std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(offset), data_len);
        Ok(())
    }

    /// Reads a slice of data from the mmap.
    pub fn read_range(&self, range: &ByteRange) -> Result<&[u8], StorageError> {
        if self.len == 0 || range.len() == 0 {
            return Ok(&[]);
        }
        let start = range.start as usize;
        let end = range.end as usize;
        if end >= self.len {
            return Err(StorageError::OutOfBounds(range.start, range.len() as usize, self.len as u64));
        }
        unsafe {
            let slice = std::slice::from_raw_parts(self.ptr.add(start), range.len() as usize);
            Ok(slice)
        }
    }

    /// Flushes dirty pages to disk.
    pub fn flush(&self) -> std::io::Result<()> {
        if let Some(ref mmap) = self._mmap {
            mmap.flush()
        } else {
            Ok(())
        }
    }
}

/// High-performance DiskWriter managing OS-level preallocation,
/// memory-mapped zero-copy writes, and BLAKE3 verification.
#[derive(Clone)]
pub struct DiskWriter {
    path: PathBuf,
    size: u64,
    storage: Arc<ConcurrentMmap>,
}

impl DiskWriter {
    /// Opens or creates a file, performs OS-native preallocation, and memory-maps it.
    pub fn open_or_create(path: impl AsRef<Path>, total_size: u64) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        let storage = if total_size > 0 {
            preallocate_file(&file, total_size)?;
            let mmap = unsafe { MmapMut::map_mut(&file)? };
            Arc::new(ConcurrentMmap::new(mmap))
        } else {
            file.set_len(0)?;
            Arc::new(ConcurrentMmap::empty())
        };

        Ok(Self {
            path,
            size: total_size,
            storage,
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes a slice to the file at `offset`.
    /// Safe because the engine assigns non-overlapping ranges to each worker.
    pub fn write_chunk_slice(&self, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        unsafe { self.storage.write_at(offset, data) }
    }

    /// Computes BLAKE3 hash for a specific range.
    pub fn compute_chunk_hash(&self, range: &ByteRange) -> Result<[u8; 32], StorageError> {
        let data = self.storage.read_range(range)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(data);
        Ok(*hasher.finalize().as_bytes())
    }

    /// Computes BLAKE3 root hash for the entire file.
    pub fn compute_file_hash(&self) -> Result<[u8; 32], StorageError> {
        if self.size == 0 {
            return Ok(*blake3::hash(&[]).as_bytes());
        }
        let full_range = ByteRange::from_len(0, self.size).map_err(StorageError::from)?;
        self.compute_chunk_hash(&full_range)
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

    /// Flushes dirty pages to persistent storage.
    pub fn sync(&self) -> Result<(), StorageError> {
        self.storage.flush()?;
        Ok(())
    }
}

impl From<crate::range::RangeError> for StorageError {
    fn from(_err: crate::range::RangeError) -> Self {
        StorageError::OutOfBounds(0, 0, 0)
    }
}

/// OS-native preallocation without zero-fill stalls
#[cfg(windows)]
fn preallocate_file(file: &File, size: u64) -> Result<(), StorageError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        SetFileInformationByHandle, FileAllocationInfo, FILE_ALLOCATION_INFO,
    };

    // First ensure file length is set
    file.set_len(size)?;

    unsafe {
        let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        let mut alloc_info = FILE_ALLOCATION_INFO {
            AllocationSize: size as i64,
        };
        let res = SetFileInformationByHandle(
            handle,
            FileAllocationInfo,
            &mut alloc_info as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<FILE_ALLOCATION_INFO>() as u32,
        );
        if res == 0 {
            // Non-fatal, set_len already succeeded
            tracing::warn!("SetFileInformationByHandle (FileAllocationInfo) returned 0, fallback to set_len");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn preallocate_file(file: &File, size: u64) -> Result<(), StorageError> {
    use std::os::unix::io::AsRawFd;

    file.set_len(size)?;
    let fd = file.as_raw_fd();

    #[cfg(target_os = "linux")]
    unsafe {
        let ret = libc::posix_fallocate(fd, 0, size as libc::off_t);
        if ret != 0 {
            tracing::warn!("posix_fallocate failed with code {}, fallback to set_len", ret);
        }
    }

    #[cfg(target_os = "macos")]
    unsafe {
        use std::mem;
        let mut fst: libc::fstore_t = mem::zeroed();
        fst.fst_flags = libc::F_ALLOCATECONTIG;
        fst.fst_posmode = libc::F_PEOFPOSMODE;
        fst.fst_offset = 0;
        fst.fst_length = size as libc::off_t;
        fst.fst_bytesalloc = 0;

        let ret = libc::fcntl(fd, libc::F_PREALLOCATE, &fst);
        if ret == -1 {
            // Try non-contiguous allocation
            fst.fst_flags = libc::F_ALLOCATEALL;
            libc::fcntl(fd, libc::F_PREALLOCATE, &fst);
        }
    }

    Ok(())
}

#[cfg(not(any(windows, unix)))]
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
}
