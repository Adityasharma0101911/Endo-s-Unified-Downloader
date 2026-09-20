use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use crate::range::ByteRange;

#[derive(Error, Debug)]
pub enum StateError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Bincode(#[from] bincode::Error),
    #[error("State file corrupted: {0}")]
    Corrupted(String),
}

/// Persistent download state representation for zero-scan crash resilience.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadState {
    pub file_name: String,
    pub file_size: u64,
    pub base_chunk_size: u64,
    pub completed_ranges: Vec<ByteRange>,
    pub mirrors: Vec<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub blake3_root: Option<[u8; 32]>,
}

impl DownloadState {
    pub fn new(
        file_name: String,
        file_size: u64,
        base_chunk_size: u64,
        mirrors: Vec<String>,
    ) -> Self {
        Self {
            file_name,
            file_size,
            base_chunk_size,
            completed_ranges: Vec::new(),
            mirrors,
            etag: None,
            last_modified: None,
            blake3_root: None,
        }
    }

    /// Derives the state file path for a given target file (e.g. `file.zip.hfstate`).
    pub fn state_file_path(target_path: &Path) -> PathBuf {
        let mut state_name = target_path.file_name().unwrap_or_default().to_os_string();
        state_name.push(".hfstate");
        target_path.with_file_name(state_name)
    }

    /// Loads the state file if it exists.
    pub fn load_from_path(state_path: &Path) -> Result<Option<Self>, StateError> {
        if !state_path.exists() {
            return Ok(None);
        }

        let mut file = File::open(state_path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        let state: Self = bincode::deserialize(&buffer)
            .map_err(|e| StateError::Corrupted(e.to_string()))?;

        Ok(Some(state))
    }

    /// Atomically persists the state to disk (via .tmp file rename).
    pub fn save_atomic(&self, state_path: &Path) -> Result<(), StateError> {
        let mut tmp_path = state_path.to_path_buf();
        tmp_path.set_extension("hfstate.tmp");

        let serialized = bincode::serialize(self)?;

        {
            let mut file = File::create(&tmp_path)?;
            file.write_all(&serialized)?;
            file.flush()?;
            file.sync_all()?;
        }

        // Atomic replace
        fs::rename(&tmp_path, state_path)?;
        Ok(())
    }

    /// Deletes the state file when the download is fully completed.
    pub fn remove(state_path: &Path) -> std::io::Result<()> {
        if state_path.exists() {
            fs::remove_file(state_path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_atomic_state_save_and_load() {
        let temp = NamedTempFile::new().unwrap();
        let target_path = temp.path().to_path_buf();
        let state_path = DownloadState::state_file_path(&target_path);

        let mut state = DownloadState::new(
            "test_file.bin".to_string(),
            10_000_000,
            1024 * 1024,
            vec!["https://example.com/test_file.bin".to_string()],
        );
        state.completed_ranges.push(ByteRange::new(0, 1024 * 1024 - 1).unwrap());

        state.save_atomic(&state_path).unwrap();
        assert!(state_path.exists());

        let loaded = DownloadState::load_from_path(&state_path).unwrap().unwrap();
        assert_eq!(loaded.file_name, "test_file.bin");
        assert_eq!(loaded.file_size, 10_000_000);
        assert_eq!(loaded.completed_ranges.len(), 1);
        assert_eq!(loaded.completed_ranges[0], ByteRange::new(0, 1024 * 1024 - 1).unwrap());

        DownloadState::remove(&state_path).unwrap();
        assert!(!state_path.exists());
    }
}
