use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryStatus {
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub file_name: String,
    pub file_path: PathBuf,
    pub file_size: u64,
    pub downloaded_bytes: u64,
    pub urls: Vec<String>,
    pub status: HistoryStatus,
    pub blake3_hash: Option<String>,
    pub sha256_hash: Option<String>,
    pub started_at: u64,
    pub completed_at: Option<u64>,
}

impl HistoryEntry {
    pub fn new(
        file_name: String,
        file_path: PathBuf,
        file_size: u64,
        urls: Vec<String>,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let id = format!("{}_{}", now, file_name);

        Self {
            id,
            file_name,
            file_path,
            file_size,
            downloaded_bytes: 0,
            urls,
            status: HistoryStatus::Completed,
            blake3_hash: None,
            sha256_hash: None,
            started_at: now,
            completed_at: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadHistoryManager {
    entries: Vec<HistoryEntry>,
    #[serde(skip)]
    custom_path: Option<PathBuf>,
}

impl DownloadHistoryManager {
    pub fn default_history_path() -> PathBuf {
        #[cfg(windows)]
        if let Ok(app_data) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(app_data).join("EndosUnifiedDownloader").join("history.json");
        }

        if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
            return PathBuf::from(home).join(".hyperfetch").join("history.json");
        }

        PathBuf::from("history.json")
    }

    pub fn load() -> Self {
        let path = Self::default_history_path();
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Self {
        if !path.exists() {
            return Self {
                entries: Vec::new(),
                custom_path: Some(path.to_path_buf()),
            };
        }

        match File::open(path) {
            Ok(mut file) => {
                let mut content = String::new();
                if file.read_to_string(&mut content).is_ok() {
                    if let Ok(entries) = serde_json::from_str::<Vec<HistoryEntry>>(&content) {
                        return Self {
                            entries,
                            custom_path: Some(path.to_path_buf()),
                        };
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to open history file at {:?}: {}", path, e);
            }
        }

        Self {
            entries: Vec::new(),
            custom_path: Some(path.to_path_buf()),
        }
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = self.custom_path.clone().unwrap_or_else(Self::default_history_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(&self.entries)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let tmp_path = path.with_extension("tmp");
        {
            let mut file = File::create(&tmp_path)?;
            file.write_all(json.as_bytes())?;
            file.flush()?;
            file.sync_all()?;
        }

        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    pub fn add_or_update(&mut self, entry: HistoryEntry) {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.id == entry.id) {
            *existing = entry;
        } else {
            self.entries.insert(0, entry); // Most recent first
        }
        let _ = self.save();
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    pub fn remove_entry(&mut self, id: &str) -> bool {
        let original_len = self.entries.len();
        self.entries.retain(|e| e.id != id);
        if self.entries.len() != original_len {
            let _ = self.save();
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        let _ = self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_history_manager_lifecycle() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("test_history.json");

        let mut manager = DownloadHistoryManager::load_from_path(&history_path);
        assert_eq!(manager.entries().len(), 0);

        let mut entry1 = HistoryEntry::new(
            "test_build.7z".to_string(),
            PathBuf::from("C:\\Downloads\\test_build.7z"),
            1024 * 1024 * 100,
            vec!["https://example.com/test_build.7z".to_string()],
        );
        entry1.downloaded_bytes = 1024 * 1024 * 100;
        entry1.status = HistoryStatus::Completed;
        entry1.blake3_hash = Some("abcdef1234567890".to_string());

        manager.add_or_update(entry1.clone());
        assert_eq!(manager.entries().len(), 1);
        assert_eq!(manager.entries()[0].file_name, "test_build.7z");

        // Reload from disk
        let reloaded = DownloadHistoryManager::load_from_path(&history_path);
        assert_eq!(reloaded.entries().len(), 1);
        assert_eq!(reloaded.entries()[0].id, entry1.id);
        assert_eq!(reloaded.entries()[0].blake3_hash, Some("abcdef1234567890".to_string()));

        // Remove
        let removed = manager.remove_entry(&entry1.id);
        assert!(removed);
        assert_eq!(manager.entries().len(), 0);
    }
}
