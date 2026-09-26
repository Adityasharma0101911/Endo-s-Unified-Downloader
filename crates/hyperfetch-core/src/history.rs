use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};

/// Only the newest entries are kept so loading and saving stay cheap.
const MAX_ENTRIES: usize = 1000;
/// How long a writer waits for another process (CLI, GUI, parallel download) to finish its save.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A string unique across processes and calls: nanosecond timestamp, pid and a per-process counter.
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:x}-{:x}-{:x}",
        nanos,
        std::process::id(),
        UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// `path` with `suffix` appended to its file name (`history.json` -> `history.json<suffix>`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

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
    #[serde(default)]
    pub file_size: u64,
    #[serde(default)]
    pub downloaded_bytes: u64,
    #[serde(default)]
    pub urls: Vec<String>,
    pub status: HistoryStatus,
    #[serde(default)]
    pub blake3_hash: Option<String>,
    #[serde(default)]
    pub sha256_hash: Option<String>,
    /// Unix seconds when the download started.
    #[serde(default)]
    pub started_at: u64,
    #[serde(default)]
    pub completed_at: Option<u64>,
}

impl HistoryEntry {
    /// Creates an entry with a unique id. `started_at` defaults to now; callers that build the
    /// entry after the download finished must set it to the time the download began.
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

        Self {
            id: unique_suffix(),
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

    fn recency(&self) -> u64 {
        self.completed_at.unwrap_or(self.started_at)
    }
}

/// Newest first, one entry per file path, at most `MAX_ENTRIES`.
fn normalize(entries: &mut Vec<HistoryEntry>) {
    entries.sort_by_key(|e| std::cmp::Reverse(e.recency()));
    let mut seen_paths = HashSet::new();
    entries.retain(|e| seen_paths.insert(e.file_path.clone()));
    entries.truncate(MAX_ENTRIES);
}

/// Reads the history file. A file that cannot be parsed reads as an empty list; with `backup` it
/// is also moved aside to `<path>.corrupt-<suffix>` (never overwritten). Only a caller holding the
/// history lock may back up, or it could move away a valid file another process just wrote.
fn read_entries(path: &Path, backup: bool) -> io::Result<Vec<HistoryEntry>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&bytes) {
        Ok(entries) => Ok(entries),
        Err(parse_err) if backup => {
            let backup = with_suffix(path, &format!(".corrupt-{}", unique_suffix()));
            fs::rename(path, &backup)?;
            tracing::warn!(
                "History file {:?} could not be parsed ({}); moved it to {:?} and started a new history",
                path,
                parse_err,
                backup
            );
            Ok(Vec::new())
        }
        Err(parse_err) => {
            tracing::warn!("History file {:?} could not be parsed ({}); showing an empty history", path, parse_err);
            Ok(Vec::new())
        }
    }
}

/// Atomically replaces the history file via a uniquely named temp file.
fn write_entries(path: &Path, entries: &[HistoryEntry]) -> io::Result<()> {
    let json = serde_json::to_vec(entries).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    let tmp_path = with_suffix(path, &format!(".{}.tmp", unique_suffix()));
    let result = (|| {
        let mut file = File::create(&tmp_path)?;
        file.write_all(&json)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// Takes the cross-process history lock (an OS advisory lock on `<path>.lock`, released when the
/// returned file is dropped or the process dies).
fn lock_history(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(with_suffix(path, ".lock"))?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(ErrorKind::TimedOut, "timed out waiting for the history lock"));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// A change made through a manager, kept until it has been written to the history file.
#[derive(Debug, Clone)]
enum Change {
    Upsert(HistoryEntry),
    Remove(String),
    Clear,
}

impl Change {
    /// Applies the change; returns whether it altered `entries`.
    fn apply(&self, entries: &mut Vec<HistoryEntry>) -> bool {
        let before = entries.len();
        match self {
            Change::Upsert(entry) => {
                entries.retain(|e| e.id != entry.id && e.file_path != entry.file_path);
                entries.insert(0, entry.clone());
                return true;
            }
            Change::Remove(id) => entries.retain(|e| e.id != *id),
            Change::Clear => entries.clear(),
        }
        entries.len() != before
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadHistoryManager {
    entries: Vec<HistoryEntry>,
    #[serde(skip)]
    custom_path: Option<PathBuf>,
    /// Changes not yet written to disk. Saving replays only these onto the current file, so it
    /// never brings back entries another manager or process removed.
    #[serde(skip)]
    pending: Vec<Change>,
}

impl DownloadHistoryManager {
    pub fn default_history_path() -> PathBuf {
        if let Some(p) = std::env::var_os("ENDO_HISTORY_PATH") {
            return PathBuf::from(p);
        }

        #[cfg(windows)]
        if let Ok(app_data) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(app_data).join("EndosUnifiedDownloader").join("history.json");
        }

        #[cfg(not(windows))]
        {
            if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
                return PathBuf::from(xdg_data).join("endos-downloader").join("history.json");
            }
            if let Ok(home) = std::env::var("HOME") {
                let xdg_fallback = PathBuf::from(&home).join(".local").join("share").join("endos-downloader").join("history.json");
                if xdg_fallback.exists() {
                    return xdg_fallback;
                }
                return PathBuf::from(home).join(".hyperfetch").join("history.json");
            }
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
        let entries = read_entries(path, false).unwrap_or_else(|e| {
            tracing::warn!("Failed to read history file at {:?}: {}", path, e);
            Vec::new()
        });
        Self {
            entries,
            custom_path: Some(path.to_path_buf()),
            pending: Vec::new(),
        }
    }

    fn path(&self) -> PathBuf {
        self.custom_path.clone().unwrap_or_else(Self::default_history_path)
    }

    /// Applies `change` in memory and writes it, with any earlier unsaved changes, to the history
    /// file. If the file cannot be updated the change stays pending for the next write.
    fn modify(&mut self, change: Change) -> bool {
        let changed = change.apply(&mut self.entries);
        normalize(&mut self.entries);
        self.pending.push(change);
        if let Err(e) = self.save() {
            tracing::warn!("Failed to update history file {:?}: {}", self.path(), e);
        }
        changed
    }

    /// Writes this manager's unsaved changes to the history file and refreshes it from the result.
    /// Under the history lock the file is reloaded and only those changes are replayed onto it, so
    /// updates made by other managers or processes since this one loaded are never lost or undone.
    pub fn save(&mut self) -> Result<(), std::io::Error> {
        let path = self.path();
        let _lock = lock_history(&path)?;
        let mut entries = read_entries(&path, true)?;
        for change in &self.pending {
            change.apply(&mut entries);
        }
        normalize(&mut entries);
        write_entries(&path, &entries)?;
        self.pending.clear();
        self.entries = entries;
        Ok(())
    }

    /// Adds `entry`, replacing any entry with the same id or the same file path.
    pub fn add_or_update(&mut self, entry: HistoryEntry) {
        self.modify(Change::Upsert(entry));
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Removes the entry with this id; returns whether this manager had it.
    pub fn remove_entry(&mut self, id: &str) -> bool {
        self.modify(Change::Remove(id.to_string()))
    }

    pub fn clear(&mut self) {
        self.modify(Change::Clear);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn entry(name: &str, dir: &Path) -> HistoryEntry {
        HistoryEntry::new(
            name.to_string(),
            dir.join(name),
            10,
            vec![format!("https://example.com/{}", name)],
        )
    }

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
        assert_eq!(DownloadHistoryManager::load_from_path(&history_path).entries().len(), 0);
    }

    #[test]
    fn corrupt_history_is_backed_up_not_wiped() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        std::fs::write(&history_path, b"[{\"id\": \"truncated").unwrap();

        // A plain load holds no lock, so it must leave the file where it is.
        let mut manager = DownloadHistoryManager::load_from_path(&history_path);
        assert!(manager.entries().is_empty());
        assert_eq!(std::fs::read(&history_path).unwrap(), b"[{\"id\": \"truncated");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        manager.add_or_update(entry("a.bin", dir.path()));

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains("history.json.corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"[{\"id\": \"truncated");
        assert_eq!(DownloadHistoryManager::load_from_path(&history_path).entries().len(), 1);
    }

    #[test]
    fn concurrent_managers_lose_no_updates() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");

        // Both managers load before either writes, like the GUI and a CLI run side by side.
        let handles: Vec<_> = ["gui", "cli"]
            .into_iter()
            .map(|who| {
                let mut manager = DownloadHistoryManager::load_from_path(&history_path);
                let dir = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        manager.add_or_update(entry(&format!("{}-{}.bin", who, i), &dir));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let reloaded = DownloadHistoryManager::load_from_path(&history_path);
        assert_eq!(reloaded.entries().len(), 50);
    }

    #[test]
    fn stale_manager_does_not_drop_other_writers_entries() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let mut gui = DownloadHistoryManager::load_from_path(&history_path);
        gui.add_or_update(entry("old.bin", dir.path()));
        let old_id = gui.entries()[0].id.clone();

        let mut cli = DownloadHistoryManager::load_from_path(&history_path);
        cli.add_or_update(entry("new.bin", dir.path()));

        assert!(gui.remove_entry(&old_id));
        let names: Vec<_> = gui.entries().iter().map(|e| e.file_name.as_str()).collect();
        assert_eq!(names, ["new.bin"]);
    }

    fn names_on_disk(path: &Path) -> Vec<String> {
        let mut names: Vec<_> = DownloadHistoryManager::load_from_path(path)
            .entries()
            .iter()
            .map(|e| e.file_name.clone())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn save_does_not_resurrect_entries_removed_elsewhere() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let mut gui = DownloadHistoryManager::load_from_path(&history_path);
        gui.add_or_update(entry("x.bin", dir.path()));
        gui.add_or_update(entry("y.bin", dir.path()));
        let x_id = gui.entries().iter().find(|e| e.file_name == "x.bin").unwrap().id.clone();

        let mut cli = DownloadHistoryManager::load_from_path(&history_path);
        assert!(gui.remove_entry(&x_id));
        cli.save().unwrap();
        assert_eq!(names_on_disk(&history_path), ["y.bin"]);
        assert_eq!(cli.entries().len(), 1);

        gui.clear();
        cli.save().unwrap();
        assert!(names_on_disk(&history_path).is_empty());
    }

    #[test]
    fn unsaved_change_is_written_later_without_undoing_others() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let mut gui = DownloadHistoryManager::load_from_path(&history_path);
        gui.add_or_update(entry("x.bin", dir.path()));
        let x_id = gui.entries()[0].id.clone();
        let mut cli = DownloadHistoryManager::load_from_path(&history_path);

        // A directory in place of the lock file makes every write fail.
        let lock_path = with_suffix(&history_path, ".lock");
        std::fs::remove_file(&lock_path).unwrap();
        std::fs::create_dir(&lock_path).unwrap();
        cli.add_or_update(entry("offline.bin", dir.path()));
        assert_eq!(cli.entries().len(), 2, "the change is kept in memory");
        std::fs::remove_dir(&lock_path).unwrap();

        assert!(gui.remove_entry(&x_id));
        cli.save().unwrap();
        assert_eq!(names_on_disk(&history_path), ["offline.bin"]);
    }

    #[test]
    fn redownload_of_same_path_replaces_entry_and_ids_are_unique() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let mut manager = DownloadHistoryManager::load_from_path(&history_path);

        let first = entry("same.bin", dir.path());
        let mut second = entry("same.bin", dir.path());
        assert_ne!(first.id, second.id);
        second.file_size = 99;

        manager.add_or_update(first);
        manager.add_or_update(second.clone());
        assert_eq!(manager.entries().len(), 1);
        assert_eq!(manager.entries()[0].id, second.id);
        assert_eq!(manager.entries()[0].file_size, 99);
    }

    #[test]
    fn normalize_keeps_newest_bounded_entries() {
        let dir = tempdir().unwrap();
        let mut entries: Vec<_> = (0..MAX_ENTRIES as u64 + 5)
            .map(|i| {
                let mut e = entry(&format!("{}.bin", i), dir.path());
                e.completed_at = Some(i);
                e
            })
            .collect();
        normalize(&mut entries);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0].completed_at, Some(MAX_ENTRIES as u64 + 4));
        assert_eq!(entries.last().unwrap().completed_at, Some(5));
    }
}
