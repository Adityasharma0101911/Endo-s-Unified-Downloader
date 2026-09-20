use std::path::PathBuf;
use url::Url;

use crate::engine::DownloadOptions;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueItemStatus {
    Queued,
    Resolving,
    Downloading,
    Paused,
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub id: usize,
    pub urls: Vec<Url>,
    pub destination: PathBuf,
    pub filename: String,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub speed_bytes_per_sec: f64,
    pub progress_ratio: f64,
    pub status: QueueItemStatus,
    pub options: DownloadOptions,
}

#[derive(Default)]
pub struct DownloadQueue {
    items: Vec<QueueItem>,
    next_id: usize,
}

impl DownloadQueue {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
        }
    }

    pub fn add_item(&mut self, urls: Vec<Url>, destination: PathBuf, options: DownloadOptions) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let filename = urls.first()
            .and_then(|u| u.path_segments())
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
            .unwrap_or("download.bin")
            .to_string();

        self.items.push(QueueItem {
            id,
            urls,
            destination,
            filename,
            total_bytes: 0,
            downloaded_bytes: 0,
            speed_bytes_per_sec: 0.0,
            progress_ratio: 0.0,
            status: QueueItemStatus::Queued,
            options,
        });
        id
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn items_mut(&mut self) -> &mut [QueueItem] {
        &mut self.items
    }

    pub fn get_item(&self, id: usize) -> Option<&QueueItem> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn get_item_mut(&mut self, id: usize) -> Option<&mut QueueItem> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    pub fn next_queued_item_mut(&mut self) -> Option<&mut QueueItem> {
        self.items.iter_mut().find(|i| i.status == QueueItemStatus::Queued)
    }

    pub fn remove_item(&mut self, id: usize) {
        self.items.retain(|item| item.id != id);
    }

    pub fn retain_items<F>(&mut self, f: F)
    where
        F: FnMut(&QueueItem) -> bool,
    {
        self.items.retain(f);
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}
