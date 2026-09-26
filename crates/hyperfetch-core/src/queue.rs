use std::path::PathBuf;
use url::Url;

use crate::engine::{DownloadOptions, EngineSnapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueItemStatus {
    /// Waiting for the scheduler or for the user to start it.
    Queued,
    /// An engine is running for the item (resolving, probing or transferring).
    Downloading,
    /// Cancellation was requested; the engine is still saving its resume state.
    Pausing,
    /// Stopped; the partial file and its resume state are kept on disk.
    Paused,
    Completed,
    Failed(String),
}

impl QueueItemStatus {
    /// An engine task is running for the item, so it must not be started again or removed.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Downloading | Self::Pausing)
    }
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub id: usize,
    /// Mirrors of one file.
    pub urls: Vec<Url>,
    /// Display name: the URL's last path segment until the engine reports the real target.
    pub filename: String,
    /// Captured when the item was added; every (re)start uses exactly these.
    pub options: DownloadOptions,
    pub status: QueueItemStatus,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub speed_bytes_per_sec: f64,
    pub progress_ratio: f64,
    /// Final path reported by the engine, once known.
    pub target_path: Option<PathBuf>,
}

impl QueueItem {
    /// Another item that must not run at the same time: it shares a mirror URL or the target file.
    fn conflicts_with(&self, other: &QueueItem) -> bool {
        self.urls.iter().any(|u| other.urls.contains(u))
            || self.target_path.is_some() && self.target_path == other.target_path
    }
}

#[derive(Debug)]
pub struct DownloadQueue {
    items: Vec<QueueItem>,
    next_id: usize,
}

impl Default for DownloadQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl DownloadQueue {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
        }
    }

    pub fn add_item(&mut self, urls: Vec<Url>, options: DownloadOptions) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let filename = urls
            .first()
            .and_then(|u| u.path_segments())
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
            .map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned())
            .unwrap_or_else(|| "download".to_string());

        self.items.push(QueueItem {
            id,
            urls,
            filename,
            options,
            status: QueueItemStatus::Queued,
            total_bytes: 0,
            downloaded_bytes: 0,
            speed_bytes_per_sec: 0.0,
            progress_ratio: 0.0,
            target_path: None,
        });
        id
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn get_item(&self, id: usize) -> Option<&QueueItem> {
        self.items.iter().find(|i| i.id == id)
    }

    fn get_item_mut(&mut self, id: usize) -> Option<&mut QueueItem> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    pub fn active_count(&self) -> usize {
        self.items.iter().filter(|i| i.status.is_active()).count()
    }

    /// The first queued item to start while fewer than `max_concurrent` items are active.
    pub fn next_to_start(&self, max_concurrent: usize) -> Option<usize> {
        if self.active_count() >= max_concurrent {
            return None;
        }
        self.items
            .iter()
            .find(|i| i.status == QueueItemStatus::Queued && self.active_conflict(i.id).is_none())
            .map(|i| i.id)
    }

    /// Another active item downloading the same file as `id` (same mirror URL or target path).
    pub fn active_conflict(&self, id: usize) -> Option<usize> {
        let item = self.get_item(id)?;
        self.items
            .iter()
            .find(|other| other.id != id && other.status.is_active() && item.conflicts_with(other))
            .map(|other| other.id)
    }

    /// Marks the item as running and clears its speed. Returns false if it is missing or already active.
    pub fn mark_started(&mut self, id: usize) -> bool {
        match self.get_item_mut(id) {
            Some(item) if !item.status.is_active() => {
                item.status = QueueItemStatus::Downloading;
                item.speed_bytes_per_sec = 0.0;
                true
            }
            _ => false,
        }
    }

    /// Records that cancellation was requested for a running item.
    pub fn mark_pausing(&mut self, id: usize) -> bool {
        match self.get_item_mut(id) {
            Some(item) if item.status == QueueItemStatus::Downloading => {
                item.status = QueueItemStatus::Pausing;
                true
            }
            _ => false,
        }
    }

    pub fn apply_snapshot(&mut self, id: usize, snapshot: &EngineSnapshot) {
        let Some(item) = self.get_item_mut(id) else { return };
        item.total_bytes = snapshot.total_bytes;
        item.downloaded_bytes = snapshot.downloaded_bytes;
        item.speed_bytes_per_sec = snapshot.speed_bytes_per_sec;
        item.progress_ratio = snapshot.progress_ratio.clamp(0.0, 1.0);
        if let Some(target) = &snapshot.target_path {
            if let Some(name) = target.file_name() {
                item.filename = name.to_string_lossy().into_owned();
            }
            item.target_path = Some(target.clone());
        }
    }

    /// Settles a finished engine run. A run that ends while pausing is `Paused` unless it
    /// completed anyway; `size` is the finished file's size when known.
    pub fn finish(&mut self, id: usize, result: Result<(PathBuf, Option<u64>), String>) {
        let Some(item) = self.get_item_mut(id) else { return };
        let was_pausing = item.status == QueueItemStatus::Pausing;
        item.speed_bytes_per_sec = 0.0;
        match result {
            Ok((path, size)) => {
                if let Some(size) = size {
                    item.total_bytes = size;
                    item.downloaded_bytes = size;
                }
                if let Some(name) = path.file_name() {
                    item.filename = name.to_string_lossy().into_owned();
                }
                item.target_path = Some(path);
                item.progress_ratio = 1.0;
                item.status = QueueItemStatus::Completed;
            }
            Err(_) if was_pausing => item.status = QueueItemStatus::Paused,
            Err(e) => item.status = QueueItemStatus::Failed(e),
        }
    }

    /// Forgets progress before a fresh restart (the partial files are deleted by the caller).
    pub fn reset_progress(&mut self, id: usize) {
        if let Some(item) = self.get_item_mut(id) {
            item.total_bytes = 0;
            item.downloaded_bytes = 0;
            item.speed_bytes_per_sec = 0.0;
            item.progress_ratio = 0.0;
        }
    }

    /// Removes an item unless its engine is still running.
    pub fn remove_item(&mut self, id: usize) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item.id != id || item.status.is_active());
        self.items.len() != before
    }

    pub fn clear_completed(&mut self) {
        self.items.retain(|i| i.status != QueueItemStatus::Completed);
    }

    /// Removes every item whose engine is not running.
    pub fn clear_inactive(&mut self) {
        self.items.retain(|i| i.status.is_active());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn queue_of(urls: &[&str]) -> (DownloadQueue, Vec<usize>) {
        let mut q = DownloadQueue::new();
        let ids = urls.iter().map(|u| q.add_item(vec![url(u)], DownloadOptions::default())).collect();
        (q, ids)
    }

    fn snapshot(target: &str) -> EngineSnapshot {
        EngineSnapshot {
            total_bytes: 100,
            downloaded_bytes: 40,
            speed_bytes_per_sec: 10.0,
            progress_ratio: 0.4,
            active_workers: 2,
            mirror_speeds: Vec::new(),
            chunks: Vec::new(),
            target_path: Some(PathBuf::from(target)),
        }
    }

    #[test]
    fn add_item_uses_decoded_last_segment_and_keeps_options() {
        let mut q = DownloadQueue::new();
        let opts = DownloadOptions { max_speed: Some(1024), proxy: Some("http://p:1".into()), ..Default::default() };
        let a = q.add_item(vec![url("https://e.com/dir/my%20file.iso")], opts);
        let b = q.add_item(vec![url("https://e.com/")], DownloadOptions::default());
        assert_ne!(a, b);
        let item = q.get_item(a).unwrap();
        assert_eq!(item.filename, "my file.iso");
        assert_eq!(item.options.max_speed, Some(1024));
        assert_eq!(item.options.proxy.as_deref(), Some("http://p:1"));
        assert_eq!(item.status, QueueItemStatus::Queued);
        assert_eq!(q.get_item(b).unwrap().filename, "download");
    }

    #[test]
    fn scheduler_respects_concurrency_and_order() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b", "https://e.com/c"]);
        assert_eq!(q.next_to_start(2), Some(ids[0]));
        assert!(q.mark_started(ids[0]));
        assert!(!q.mark_started(ids[0]), "an active item cannot be started twice");
        assert_eq!(q.next_to_start(2), Some(ids[1]));
        q.mark_started(ids[1]);
        assert_eq!(q.active_count(), 2);
        assert_eq!(q.next_to_start(2), None);
        assert!(q.mark_pausing(ids[0]));
        assert_eq!(q.next_to_start(2), None, "a pausing item still occupies a slot");
        q.finish(ids[0], Err("Download cancelled by user".into()));
        assert_eq!(q.get_item(ids[0]).unwrap().status, QueueItemStatus::Paused);
        assert_eq!(q.next_to_start(2), Some(ids[2]), "paused items are not restarted automatically");
    }

    #[test]
    fn same_url_or_target_is_a_conflict() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/a", "https://e.com/other"]);
        q.mark_started(ids[0]);
        assert_eq!(q.active_conflict(ids[1]), Some(ids[0]));
        assert_eq!(q.active_conflict(ids[2]), None);
        assert_eq!(q.next_to_start(5), Some(ids[2]), "a conflicting item waits");

        q.apply_snapshot(ids[0], &snapshot("/dl/x.bin"));
        q.apply_snapshot(ids[2], &snapshot("/dl/x.bin"));
        assert_eq!(q.active_conflict(ids[2]), Some(ids[0]), "same target file");
    }

    #[test]
    fn snapshot_and_finish_update_progress_and_status() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b", "https://e.com/c"]);
        q.mark_started(ids[0]);
        q.apply_snapshot(ids[0], &snapshot("/dl/real name.bin"));
        let item = q.get_item(ids[0]).unwrap();
        assert_eq!((item.downloaded_bytes, item.total_bytes), (40, 100));
        assert_eq!(item.filename, "real name.bin");
        assert_eq!(item.target_path.as_deref(), Some(std::path::Path::new("/dl/real name.bin")));

        q.finish(ids[0], Ok((PathBuf::from("/dl/real name.bin"), Some(100))));
        let item = q.get_item(ids[0]).unwrap();
        assert_eq!(item.status, QueueItemStatus::Completed);
        assert_eq!((item.progress_ratio, item.speed_bytes_per_sec), (1.0, 0.0));

        q.mark_started(ids[1]);
        q.finish(ids[1], Err("HTTP 404".into()));
        assert_eq!(q.get_item(ids[1]).unwrap().status, QueueItemStatus::Failed("HTTP 404".into()));

        // Completing while a pause was requested still counts as completed.
        q.mark_started(ids[2]);
        q.mark_pausing(ids[2]);
        q.finish(ids[2], Ok((PathBuf::from("/dl/c"), None)));
        assert_eq!(q.get_item(ids[2]).unwrap().status, QueueItemStatus::Completed);
    }

    #[test]
    fn active_items_survive_remove_and_clear() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b", "https://e.com/c"]);
        q.mark_started(ids[0]);
        q.mark_started(ids[1]);
        q.finish(ids[1], Ok((PathBuf::from("/dl/b"), Some(1))));

        assert!(!q.remove_item(ids[0]));
        q.clear_completed();
        assert!(q.get_item(ids[1]).is_none());
        assert!(q.get_item(ids[2]).is_some());

        q.clear_inactive();
        assert_eq!(q.items().len(), 1);
        assert_eq!(q.items()[0].id, ids[0]);

        q.mark_pausing(ids[0]);
        q.finish(ids[0], Err("cancelled".into()));
        assert!(q.remove_item(ids[0]));
        assert!(q.items().is_empty());
    }

    #[test]
    fn reset_progress_clears_counters() {
        let (mut q, ids) = queue_of(&["https://e.com/a"]);
        q.mark_started(ids[0]);
        q.apply_snapshot(ids[0], &snapshot("/dl/a"));
        q.mark_pausing(ids[0]);
        q.finish(ids[0], Err("cancelled".into()));
        q.reset_progress(ids[0]);
        let item = q.get_item(ids[0]).unwrap();
        assert_eq!((item.downloaded_bytes, item.total_bytes, item.progress_ratio), (0, 0, 0.0));
        assert_eq!(item.target_path.as_deref(), Some(std::path::Path::new("/dl/a")), "the target is kept");
    }
}
