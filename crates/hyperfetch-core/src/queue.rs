use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::engine::{DownloadOptions, EngineSnapshot, CANCELLED};

/// Small downloads that run beside a batch's or queue's places (see [`may_start`]).
pub const SMALL_AT_ONCE: usize = 16;

/// Whether a batch or queue that runs `at_once` downloads at once (the CLI's `-j`, the app's
/// setting) may start another while `running` run, `small` of them known small (see
/// `EngineSnapshot::small`): up to `SMALL_AT_ONCE` of those take no place, as handshakes, not
/// bandwidth, are what they wait on. They still keep to the hosts' connection limits and the speed
/// limit, as every download does. One at a time stays one at a time: it is asked for to keep the
/// order, or to spare a server.
pub fn may_start(running: usize, small: usize, at_once: usize) -> bool {
    let beside = if at_once > 1 { small.min(SMALL_AT_ONCE) } else { 0 };
    running.saturating_sub(beside) < at_once.max(1)
}
use crate::history::{is_redacted, redact_text, redact_url, REDACTED_LINK};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Restored after a restart without its Authorization header, which is never saved; it
    /// runs again only once the header is provided (see [`DownloadQueue::provide_auth`]).
    AuthRequired,
}

impl QueueItemStatus {
    /// An engine task is running for the item, so it must not be started again or removed.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Downloading | Self::Pausing)
    }
}

/// Which waiting downloads start first: High before Normal, Normal before Low, and within one the
/// queue's order. It decides only when they start, not how they share the speed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Priority {
    High,
    #[default]
    Normal,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: usize,
    /// Mirrors of one file.
    #[serde(with = "url_list")]
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
    /// `options.output_path` is the file itself, named by the input (a .metalink, .torrent or
    /// magnet), rather than the folder that receives it under the server's name. The engine may
    /// still save it under a numbered name, which `target_path` then reports.
    #[serde(default)]
    pub names_file: bool,
    /// What post-processing told the user once the download finished (see `EngineSnapshot::notes`).
    #[serde(default)]
    pub notes: Vec<String>,
    /// When it starts among the waiting downloads; Normal in a queue saved before priorities.
    #[serde(default)]
    pub priority: Priority,
    /// Running, it is known small (see `EngineSnapshot::small`).
    #[serde(skip)]
    pub small: bool,
}

impl QueueItem {
    /// Another item that must not run at the same time: it shares a mirror URL or the target file.
    fn conflicts_with(&self, other: &QueueItem) -> bool {
        self.urls.iter().any(|u| other.urls.contains(u))
            || self.target_path.is_some() && self.target_path == other.target_path
    }

    /// The engine reported `path` as the item's target file.
    pub fn targets(&self, path: &Path) -> bool {
        self.target_path.as_deref().is_some_and(|target| {
            target == path
                || matches!((std::path::absolute(target), std::path::absolute(path)), (Ok(a), Ok(b)) if a == b)
        })
    }

    /// Every byte has arrived and the engine is verifying the file and moving it into place, which
    /// sends no progress: not a stall.
    pub fn is_finishing(&self) -> bool {
        self.status == QueueItemStatus::Downloading && self.total_bytes > 0 && self.downloaded_bytes >= self.total_bytes
    }
}

/// What the downloads still to come have left (see [`DownloadQueue::remaining`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Remaining {
    /// Bytes still to come of those whose size is known.
    pub bytes: u64,
    /// Those whose size is not known yet: no Content-Length, a stream before it tells, a
    /// download not started yet.
    pub unknown: usize,
    /// How fast the running ones come in together: each one's speed, smoothed as for its own
    /// time left.
    pub speed: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadQueue {
    items: Vec<QueueItem>,
    next_id: usize,
    /// Bumped by every change but progress, so a front-end knows when to save the queue.
    #[serde(skip)]
    revision: u64,
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
            revision: 0,
        }
    }

    /// Changes whenever the queue may have changed, but for a running item's progress alone (see
    /// [`DownloadQueue::apply_snapshot`]).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn add_item(&mut self, urls: Vec<Url>, options: DownloadOptions) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.revision += 1;
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
            names_file: false,
            notes: Vec::new(),
            priority: Priority::Normal,
            small: false,
        });
        id
    }

    /// Adds a download saved as `options.output_path` itself (see [`QueueItem::names_file`]),
    /// shown under that name until the engine reports its target.
    pub fn add_named_item(&mut self, urls: Vec<Url>, options: DownloadOptions) -> usize {
        let id = self.add_item(urls, options);
        if let Some(item) = self.items.last_mut() {
            item.names_file = true;
            if let Some(name) = item.options.output_path.as_deref().and_then(Path::file_name) {
                item.filename = name.to_string_lossy().into_owned();
            }
        }
        id
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn get_item(&self, id: usize) -> Option<&QueueItem> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn get_item_mut(&mut self, id: usize) -> Option<&mut QueueItem> {
        self.revision += 1;
        self.items.iter_mut().find(|i| i.id == id)
    }

    pub fn active_count(&self) -> usize {
        self.items.iter().filter(|i| i.status.is_active()).count()
    }

    /// What the downloads still to come have left: those running and, when `waiting` (the queue
    /// starts them by itself), those queued; not paused or stopped ones.
    pub fn remaining(&self, waiting: bool) -> Remaining {
        let mut left = Remaining::default();
        let to_come = |i: &&QueueItem| i.status == QueueItemStatus::Downloading || waiting && i.status == QueueItemStatus::Queued;
        for item in self.items.iter().filter(to_come) {
            match item.total_bytes {
                0 => left.unknown += 1,
                total => left.bytes += total.saturating_sub(item.downloaded_bytes),
            }
            left.speed += item.speed_bytes_per_sec;
        }
        left
    }

    /// Whether the queue, running `max_concurrent` downloads at once, may start another (see
    /// [`may_start`]).
    pub fn may_start(&self, max_concurrent: usize) -> bool {
        let small = self.items.iter().filter(|i| i.status.is_active() && i.small).count();
        may_start(self.active_count(), small, max_concurrent)
    }

    /// The queued item to start while the queue may start one (see [`may_start`]): the first of
    /// the highest priority, but for those `skip` holds back (ones that cannot start yet, which
    /// the others go past).
    pub fn next_to_start(&self, max_concurrent: usize, skip: impl Fn(usize) -> bool) -> Option<usize> {
        if !self.may_start(max_concurrent) {
            return None;
        }
        let first = |priority: Priority| {
            self.items.iter().find(|i| {
                i.priority == priority && i.status == QueueItemStatus::Queued && !skip(i.id) && self.active_conflict(i.id).is_none()
            })
        };
        [Priority::High, Priority::Normal, Priority::Low].into_iter().find_map(first).map(|i| i.id)
    }

    /// Moves item `id` to place `index` of the list (the last one past its end): the order the
    /// waiting downloads start in, within a priority. Returns false if it is missing.
    pub fn move_item(&mut self, id: usize, index: usize) -> bool {
        let Some(from) = self.items.iter().position(|i| i.id == id) else { return false };
        let item = self.items.remove(from);
        self.items.insert(index.min(self.items.len()), item);
        self.revision += 1;
        true
    }

    /// Puts waiting item `id` first among the waiting ones, as high as the highest priority among
    /// them, so that it starts next; what runs goes on. Returns false if it is not waiting.
    pub fn download_next(&mut self, id: usize) -> bool {
        let waiting = |i: &&QueueItem| i.status == QueueItemStatus::Queued;
        if !self.get_item(id).is_some_and(|i| waiting(&i)) {
            return false;
        }
        let first = self.items.iter().position(|i| waiting(&i)).unwrap_or(0);
        let top = self.items.iter().filter(waiting).map(|i| i.priority).min().unwrap_or_default();
        self.move_item(id, first);
        if let Some(item) = self.get_item_mut(id) {
            item.priority = item.priority.min(top);
        }
        true
    }

    /// Another active item downloading the same file as `id` (same mirror URL or target path).
    pub fn active_conflict(&self, id: usize) -> Option<usize> {
        let item = self.get_item(id)?;
        self.items
            .iter()
            .find(|other| other.id != id && other.status.is_active() && item.conflicts_with(other))
            .map(|other| other.id)
    }

    /// An active item whose engine reported `path` as its target file.
    pub fn active_on_target(&self, path: &Path) -> Option<usize> {
        self.items.iter().find(|i| i.status.is_active() && i.targets(path)).map(|i| i.id)
    }

    /// Marks the item as running and clears its speed. Returns false if it is missing or already
    /// active, or if every link it has was saved without its secret (see
    /// [`DownloadQueue::settle_for_restart`]): it then fails, saying so.
    pub fn mark_started(&mut self, id: usize) -> bool {
        match self.get_item_mut(id) {
            Some(item) if !item.status.is_active() => {
                let before = item.urls.len();
                item.urls.retain(|u| !is_redacted(u.as_str()));
                if item.urls.is_empty() && before > 0 {
                    item.status = QueueItemStatus::Failed(REDACTED_LINK.to_string());
                    return false;
                }
                item.status = QueueItemStatus::Downloading;
                item.speed_bytes_per_sec = 0.0;
                item.small = false;
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

    /// Takes a running item's progress. Only a new target is a change of the queue (see
    /// [`DownloadQueue::revision`]): progress comes several times a second, a resume reads its
    /// own from the resume state, and the queue saved on exit has it.
    pub fn apply_snapshot(&mut self, id: usize, snapshot: &EngineSnapshot) {
        let Some(item) = self.items.iter_mut().find(|i| i.id == id) else { return };
        item.small = snapshot.small;
        if snapshot.from_probe() {
            return;
        }
        item.total_bytes = snapshot.total_bytes;
        item.downloaded_bytes = snapshot.downloaded_bytes;
        // Nothing is transferred any more once every byte has arrived.
        item.speed_bytes_per_sec = if item.is_finishing() { 0.0 } else { snapshot.speed_bytes_per_sec };
        // NaN (0/0 from an empty stream) would make the saved queue unreadable JSON.
        item.progress_ratio =
            if snapshot.progress_ratio.is_nan() { 0.0 } else { snapshot.progress_ratio.clamp(0.0, 1.0) };
        if let Some(target) = snapshot.target_path.as_ref().filter(|&target| item.target_path.as_ref() != Some(target)) {
            if let Some(name) = target.file_name() {
                item.filename = name.to_string_lossy().into_owned();
            }
            item.target_path = Some(target.clone());
            self.revision += 1;
        }
        if !snapshot.notes.is_empty() {
            item.notes = snapshot.notes.clone();
            self.revision += 1;
        }
    }

    /// Settles a finished engine run. A run that ends while pausing is `Paused` when the pause
    /// stopped it ([`CANCELLED`]); one that completed anyway is `Completed`, and one that failed
    /// otherwise `Failed` (a live recording whose streams could not be joined says where they
    /// are kept). `size` is the finished file's size when known.
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
            Err(e) if was_pausing && e == CANCELLED => item.status = QueueItemStatus::Paused,
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
            item.notes.clear();
        }
    }

    /// Gives an item its Authorization header; one waiting for it becomes `Paused`, so Resume
    /// continues from its partial file. Returns false if it is missing or running.
    pub fn provide_auth(&mut self, id: usize, header: String) -> bool {
        match self.get_item_mut(id) {
            Some(item) if !item.status.is_active() => {
                item.options.auth_header = Some(header);
                if item.status == QueueItemStatus::AuthRequired {
                    item.status = QueueItemStatus::Paused;
                }
                true
            }
            _ => false,
        }
    }

    /// The state to resume from after a restart, when no engine runs: running items are
    /// `Paused` (their resume state is on disk), and unfinished items that had an
    /// Authorization header, which is never saved, wait for it as `AuthRequired`. Finished
    /// items (completed or failed) keep their links and error without secrets (see
    /// [`crate::history::redact_url`]); unfinished ones keep them whole to resume.
    pub fn settle_for_restart(&mut self) {
        self.revision += 1;
        for item in &mut self.items {
            item.speed_bytes_per_sec = 0.0;
            if item.status.is_active() {
                item.status = QueueItemStatus::Paused;
            }
            if item.options.auth_header.is_some() && item.status != QueueItemStatus::Completed {
                item.status = QueueItemStatus::AuthRequired;
            }
            match &mut item.status {
                QueueItemStatus::Completed => {}
                QueueItemStatus::Failed(error) => *error = redact_text(error),
                _ => continue,
            }
            for url in &mut item.urls {
                if let Ok(redacted) = Url::parse(&redact_url(url.as_str())) {
                    *url = redacted;
                }
            }
        }
    }

    /// Removes an item unless its engine is still running.
    pub fn remove_item(&mut self, id: usize) -> bool {
        self.revision += 1;
        let before = self.items.len();
        self.items.retain(|item| item.id != id || item.status.is_active());
        self.items.len() != before
    }

    pub fn clear_completed(&mut self) {
        self.revision += 1;
        self.items.retain(|i| i.status != QueueItemStatus::Completed);
    }

    /// Removes every item whose engine is not running.
    pub fn clear_inactive(&mut self) {
        self.revision += 1;
        self.items.retain(|i| i.status.is_active());
    }
}

/// URLs as strings (the `url` crate is built without serde).
mod url_list {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};
    use url::Url;

    pub fn serialize<S: Serializer>(urls: &[Url], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(urls.iter().map(Url::as_str))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Url>, D::Error> {
        Vec::<String>::deserialize(deserializer)?
            .iter()
            .map(|u| Url::parse(u).map_err(D::Error::custom))
            .collect()
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
            ..Default::default()
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

    /// A queue saved before clips, SponsorBlock and the merge format existed loads with none of
    /// them; one saved with them keeps them.
    #[test]
    fn a_queue_saved_before_the_media_options_loads() {
        let mut q = DownloadQueue::new();
        let media = DownloadOptions {
            sections: vec![(90.0, 150.5)],
            sponsorblock: Some("remove".into()),
            merge_format: Some("mkv".into()),
            ..Default::default()
        };
        q.add_item(vec![url("https://e.com/v")], media);
        let json = serde_json::to_string(&q).unwrap();
        let back: DownloadQueue = serde_json::from_str(&json).unwrap();
        let options = &back.items()[0].options;
        assert_eq!((&options.sections[..], options.sponsorblock.as_deref(), options.merge_format.as_deref()), (&[(90.0, 150.5)][..], Some("remove"), Some("mkv")));

        let old = json.replace(r#""sections":[[90.0,150.5]],"sponsorblock":"remove","merge_format":"mkv","#, "");
        assert!(!old.contains("sections") && !old.contains("merge_format"), "{old}");
        let back: DownloadQueue = serde_json::from_str(&old).unwrap();
        let options = &back.items()[0].options;
        assert!(options.sections.is_empty() && options.sponsorblock.is_none() && options.merge_format.is_none());
    }

    /// A download its input named shows that name at once, but its target stays unknown until
    /// the engine reports one: until then no other download's file counts as its own, to clean
    /// up or to conflict with.
    #[test]
    fn a_named_item_is_shown_by_name_but_targets_nothing_yet() {
        let mut q = DownloadQueue::new();
        let named = |dir: &str| DownloadOptions { output_path: Some(PathBuf::from(dir).join("video.mkv")), ..Default::default() };
        let running = q.add_named_item(vec![url("https://a.example/1")], named("/dl"));
        let added = q.add_named_item(vec![url("https://b.example/2")], named("/dl"));
        q.mark_started(running);
        let item = q.get_item(added).unwrap();
        assert_eq!((item.filename.as_str(), item.names_file, item.target_path.as_ref()), ("video.mkv", true, None));
        assert_eq!(q.active_conflict(added), None, "the same name is not the same file");
        let plain = q.add_item(vec![url("https://e.com/x")], named("/dl"));
        assert!(!q.get_item(plain).unwrap().names_file);

        // Queues saved before the flag existed load as folders to save into.
        let json = serde_json::to_string(&q).unwrap().replace(",\"names_file\":true", "").replace(",\"names_file\":false", "");
        assert!(!json.contains("names_file"));
        let back: DownloadQueue = serde_json::from_str(&json).unwrap();
        assert!(back.items().iter().all(|item| !item.names_file));
    }

    #[test]
    fn scheduler_respects_concurrency_and_order() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b", "https://e.com/c"]);
        assert_eq!(q.next_to_start(2, |_| false), Some(ids[0]));
        assert!(q.mark_started(ids[0]));
        assert!(!q.mark_started(ids[0]), "an active item cannot be started twice");
        assert_eq!(q.next_to_start(2, |_| false), Some(ids[1]));
        q.mark_started(ids[1]);
        assert_eq!(q.active_count(), 2);
        assert_eq!(q.next_to_start(2, |_| false), None);
        assert!(q.mark_pausing(ids[0]));
        assert_eq!(q.next_to_start(2, |_| false), None, "a pausing item still occupies a slot");
        q.finish(ids[0], Err(CANCELLED.into()));
        assert_eq!(q.get_item(ids[0]).unwrap().status, QueueItemStatus::Paused);
        assert_eq!(q.next_to_start(2, |_| false), Some(ids[2]), "paused items are not restarted automatically");
        let (q, ids) = queue_of(&["https://e.com/a", "https://e.com/b"]);
        assert_eq!(q.next_to_start(2, |id| id == ids[0]), Some(ids[1]), "one held back holds none after it");
    }

    /// A waiting High download starts before any waiting Normal one, and Normal before Low;
    /// within a priority the queue's order decides, which moving an item changes.
    #[test]
    fn waiting_downloads_start_by_priority_then_order() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b", "https://e.com/c", "https://e.com/d"]);
        q.get_item_mut(ids[0]).unwrap().priority = Priority::Low;
        q.get_item_mut(ids[3]).unwrap().priority = Priority::High;
        let mut started = Vec::new();
        while let Some(id) = q.next_to_start(9, |_| false) {
            q.mark_started(id);
            started.push(id);
        }
        assert_eq!(started, [ids[3], ids[1], ids[2], ids[0]]);

        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b", "https://e.com/c"]);
        let before = q.revision();
        assert!(q.move_item(ids[2], 0));
        assert_ne!(q.revision(), before, "the new order is saved");
        assert_eq!(q.next_to_start(9, |_| false), Some(ids[2]));
        assert!(q.move_item(ids[2], 99), "past the end is the last place");
        let order: Vec<usize> = q.items().iter().map(|i| i.id).collect();
        assert_eq!(order, [ids[0], ids[1], ids[2]]);
        assert!(!q.move_item(42, 0));
        assert_eq!(q.next_to_start(9, |id| id == ids[0]), Some(ids[1]), "one held back holds none after it");
    }

    /// Download next puts a waiting download first among the waiting ones (a running one keeps
    /// its place) and as high as the highest priority waiting, so that it starts next; it stops
    /// nothing, and does nothing to a download that is not waiting.
    #[test]
    fn download_next_starts_a_waiting_download_next() {
        let (mut q, ids) = queue_of(&["https://e.com/run", "https://e.com/b", "https://e.com/c", "https://e.com/d"]);
        q.mark_started(ids[0]);
        q.get_item_mut(ids[1]).unwrap().priority = Priority::High;
        q.get_item_mut(ids[3]).unwrap().priority = Priority::Low;
        assert!(q.download_next(ids[3]));
        let order: Vec<usize> = q.items().iter().map(|i| i.id).collect();
        assert_eq!(order, [ids[0], ids[3], ids[1], ids[2]]);
        assert_eq!(q.get_item(ids[3]).unwrap().priority, Priority::High);
        assert_eq!(q.get_item(ids[0]).unwrap().status, QueueItemStatus::Downloading, "what runs goes on");
        assert_eq!(q.next_to_start(9, |_| false), Some(ids[3]));
        assert!(!q.download_next(ids[0]), "it runs already");
        assert!(!q.download_next(42));
    }

    /// The order and priorities are saved with the queue; a queue saved before priorities loads
    /// with every download Normal, in its order.
    #[test]
    fn order_and_priorities_survive_a_restart() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b", "https://e.com/c"]);
        q.move_item(ids[2], 0);
        q.get_item_mut(ids[1]).unwrap().priority = Priority::High;
        q.get_item_mut(ids[0]).unwrap().priority = Priority::Low;
        let json = serde_json::to_string(&q).unwrap();
        let back: DownloadQueue = serde_json::from_str(&json).unwrap();
        let saved = |q: &DownloadQueue| q.items().iter().map(|i| (i.id, i.priority)).collect::<Vec<_>>();
        assert_eq!(saved(&back), [(ids[2], Priority::Normal), (ids[0], Priority::Low), (ids[1], Priority::High)]);

        let old = json.replace(",\"priority\":\"High\"", "").replace(",\"priority\":\"Low\"", "").replace(",\"priority\":\"Normal\"", "");
        assert!(!old.contains("priority"), "{old}");
        let back: DownloadQueue = serde_json::from_str(&old).unwrap();
        assert_eq!(saved(&back), [(ids[2], Priority::Normal), (ids[0], Priority::Normal), (ids[1], Priority::Normal)]);
    }

    /// A download its probe found small takes no place of the queue's, up to `SMALL_AT_ONCE` of
    /// them, from when its snapshot says so until it runs again; but for a queue of one at a time.
    #[test]
    fn small_files_run_beside_the_queues_places() {
        assert!(may_start(3, 2, 2) && !may_start(4, 2, 2) && !may_start(40, 20, 4) && may_start(19, 20, 4));
        assert!(!may_start(1, 1, 1), "one at a time");
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b", "https://e.com/c", "https://e.com/d"]);
        q.mark_started(ids[0]);
        q.mark_started(ids[1]);
        assert_eq!(q.next_to_start(2, |_| false), None);
        q.apply_snapshot(ids[0], &EngineSnapshot { total_bytes: 1000, downloaded_bytes: 400, ..Default::default() });
        q.apply_snapshot(ids[0], &EngineSnapshot { total_bytes: 1000, small: true, ..Default::default() });
        assert_eq!(q.get_item(ids[0]).unwrap().downloaded_bytes, 400, "the probe's word tells nothing of the progress");
        assert!(!q.may_start(1));
        assert_eq!(q.next_to_start(2, |_| false), Some(ids[2]));
        q.mark_started(ids[2]);
        assert_eq!(q.next_to_start(2, |_| false), None, "until it is known small, the third takes the place");
        q.finish(ids[0], Err(CANCELLED.into()));
        q.mark_started(ids[0]);
        assert!(!q.may_start(3), "started again, it takes a place until its snapshot tells");
    }

    /// What is left to download: running and queued downloads (queued ones only when the queue
    /// starts them by itself), not paused or stopped ones; those of no known size are counted
    /// apart, not as nothing left.
    #[test]
    fn what_is_left_counts_unknown_sizes_apart() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b", "https://e.com/c", "https://e.com/d", "https://e.com/e"]);
        for &id in &ids[..4] {
            q.mark_started(id);
        }
        q.apply_snapshot(ids[0], &snapshot("/dl/a"));
        q.apply_snapshot(ids[1], &EngineSnapshot { total_bytes: 0, downloaded_bytes: 7, speed_bytes_per_sec: 5.0, ..snapshot("/dl/b") });
        q.apply_snapshot(ids[2], &snapshot("/dl/c"));
        q.mark_pausing(ids[2]);
        q.finish(ids[2], Err(CANCELLED.into()));
        q.apply_snapshot(ids[3], &snapshot("/dl/d"));
        q.finish(ids[3], Err("HTTP 404".into()));
        assert_eq!(q.remaining(true), Remaining { bytes: 60, unknown: 2, speed: 15.0 }, "the one not started has no size yet");
        assert_eq!(q.remaining(false), Remaining { bytes: 60, unknown: 1, speed: 15.0 });
        q.finish(ids[0], Ok((PathBuf::from("/dl/a"), Some(100))));
        q.mark_pausing(ids[1]);
        assert_eq!(q.remaining(false), Remaining::default(), "a pausing one is stopping");
    }

    #[test]
    fn same_url_or_target_is_a_conflict() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/a", "https://e.com/other"]);
        q.mark_started(ids[0]);
        assert_eq!(q.active_conflict(ids[1]), Some(ids[0]));
        assert_eq!(q.active_conflict(ids[2]), None);
        assert_eq!(q.next_to_start(5, |_| false), Some(ids[2]), "a conflicting item waits");

        q.apply_snapshot(ids[0], &snapshot("/dl/x.bin"));
        q.apply_snapshot(ids[2], &snapshot("/dl/x.bin"));
        assert_eq!(q.active_conflict(ids[2]), Some(ids[0]), "same target file");
    }

    #[test]
    fn active_item_on_a_target_is_found_by_path() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b"]);
        let target = std::env::temp_dir().join("x.bin");
        q.mark_started(ids[0]);
        assert_eq!(q.active_on_target(&target), None, "the target is not known yet");
        q.apply_snapshot(ids[0], &snapshot(target.to_str().unwrap()));
        assert_eq!(q.active_on_target(&target), Some(ids[0]));
        q.apply_snapshot(ids[1], &snapshot("relative.bin"));
        let spelled_out = std::env::current_dir().unwrap().join("relative.bin");
        assert!(q.get_item(ids[1]).unwrap().targets(&spelled_out), "same file, other spelling");
        assert_eq!(q.active_on_target(&std::env::temp_dir().join("y.bin")), None);
        assert!(q.get_item(ids[0]).unwrap().targets(&target));
        assert!(!q.get_item(ids[1]).unwrap().targets(&target));
        assert_eq!(q.active_on_target(&spelled_out), None, "only active items count");
        q.mark_pausing(ids[0]);
        q.finish(ids[0], Err(CANCELLED.into()));
        assert_eq!(q.active_on_target(&target), None, "a paused item is not active");
    }

    #[test]
    fn all_bytes_received_is_finishing_with_no_speed() {
        let (mut q, ids) = queue_of(&["https://e.com/a"]);
        q.mark_started(ids[0]);
        q.apply_snapshot(ids[0], &snapshot("/dl/a"));
        assert!(!q.get_item(ids[0]).unwrap().is_finishing());
        q.apply_snapshot(ids[0], &EngineSnapshot { downloaded_bytes: 100, progress_ratio: 1.0, ..snapshot("/dl/a") });
        let item = q.get_item(ids[0]).unwrap();
        assert!(item.is_finishing());
        assert_eq!(item.speed_bytes_per_sec, 0.0);
        q.finish(ids[0], Ok((PathBuf::from("/dl/a"), Some(100))));
        assert!(!q.get_item(ids[0]).unwrap().is_finishing(), "completed is not finishing");
        // An unknown size never counts as finished.
        let unknown = q.add_item(vec![url("https://e.com/s")], DownloadOptions::default());
        q.mark_started(unknown);
        q.apply_snapshot(unknown, &EngineSnapshot { total_bytes: 0, ..snapshot("/dl/s") });
        assert!(!q.get_item(unknown).unwrap().is_finishing());
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

    /// A pause that stops the download pauses it; one that ends it with another error (a live
    /// recording whose streams could not be joined, which says where they are kept) is that
    /// failure, shown with its message.
    #[test]
    fn only_the_stop_a_pause_asks_for_pauses_a_download() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b"]);
        for id in &ids {
            q.mark_started(*id);
            q.mark_pausing(*id);
        }
        q.finish(ids[0], Err(CANCELLED.into()));
        assert_eq!(q.get_item(ids[0]).unwrap().status, QueueItemStatus::Paused);
        let kept = "Could not join the recorded streams (ffmpeg is missing); what it recorded is kept as /dl/a.f1.mp4.part and /dl/a.f2.m4a.part";
        q.finish(ids[1], Err(kept.into()));
        assert_eq!(q.get_item(ids[1]).unwrap().status, QueueItemStatus::Failed(kept.into()));
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
        q.finish(ids[0], Err(CANCELLED.into()));
        assert!(q.remove_item(ids[0]));
        assert!(q.items().is_empty());
    }

    #[test]
    fn reset_progress_clears_counters() {
        let (mut q, ids) = queue_of(&["https://e.com/a"]);
        q.mark_started(ids[0]);
        q.apply_snapshot(ids[0], &snapshot("/dl/a"));
        q.mark_pausing(ids[0]);
        q.finish(ids[0], Err(CANCELLED.into()));
        q.reset_progress(ids[0]);
        let item = q.get_item(ids[0]).unwrap();
        assert_eq!((item.downloaded_bytes, item.total_bytes, item.progress_ratio), (0, 0, 0.0));
        assert_eq!(item.target_path.as_deref(), Some(std::path::Path::new("/dl/a")), "the target is kept");
    }

    /// Progress alone does not change the queue's revision, so a front end does not save the
    /// whole queue several times a second; the engine naming the target does.
    #[test]
    fn only_a_new_target_of_a_running_item_is_a_change_to_save() {
        let (mut q, ids) = queue_of(&["https://e.com/a"]);
        q.mark_started(ids[0]);
        let started = q.revision();
        q.apply_snapshot(ids[0], &EngineSnapshot { target_path: None, ..snapshot("/dl/a") });
        assert_eq!(q.revision(), started, "progress alone");
        assert_eq!(q.get_item(ids[0]).unwrap().downloaded_bytes, 40);
        q.apply_snapshot(ids[0], &snapshot("/dl/a"));
        let named = q.revision();
        assert_ne!(named, started);
        q.apply_snapshot(ids[0], &EngineSnapshot { downloaded_bytes: 90, ..snapshot("/dl/a") });
        assert_eq!(q.revision(), named, "the same target");
        q.apply_snapshot(ids[0], &snapshot("/dl/a (1)"));
        assert_ne!(q.revision(), named);
        assert_eq!(q.get_item(ids[0]).unwrap().filename, "a (1)");
        q.finish(ids[0], Ok((PathBuf::from("/dl/a (1)"), Some(100))));
        assert_ne!(q.revision(), named, "finishing is saved");
    }

    #[test]
    fn restart_pauses_running_items_and_holds_those_that_need_auth() {
        let (mut q, ids) = queue_of(&["https://e.com/a", "https://e.com/b", "https://e.com/c", "https://e.com/d"]);
        let authed = q.add_item(vec![url("https://e.com/private")], DownloadOptions { auth_header: Some("Bearer t".into()), ..Default::default() });
        let authed_done = q.add_item(vec![url("https://e.com/p2")], DownloadOptions { auth_header: Some("Bearer t".into()), ..Default::default() });
        q.mark_started(ids[0]);
        q.apply_snapshot(ids[0], &snapshot("/dl/a"));
        q.mark_started(ids[1]);
        q.mark_pausing(ids[1]);
        q.mark_started(ids[2]);
        q.finish(ids[2], Err("HTTP 404".into()));
        q.mark_started(authed);
        q.mark_started(authed_done);
        q.finish(authed_done, Ok((PathBuf::from("/dl/p2"), Some(1))));

        let before = q.revision();
        q.settle_for_restart();
        assert_ne!(q.revision(), before);
        let status = |id| q.get_item(id).unwrap().status.clone();
        assert_eq!(status(ids[0]), QueueItemStatus::Paused);
        assert_eq!(status(ids[1]), QueueItemStatus::Paused);
        assert_eq!(status(ids[2]), QueueItemStatus::Failed("HTTP 404".into()));
        assert_eq!(status(ids[3]), QueueItemStatus::Queued);
        assert_eq!(status(authed), QueueItemStatus::AuthRequired);
        assert_eq!(status(authed_done), QueueItemStatus::Completed);
        assert_eq!(q.get_item(ids[0]).unwrap().speed_bytes_per_sec, 0.0);
        assert_eq!(q.next_to_start(5, |_| false), Some(ids[3]), "an item waiting for its header is never auto-started");

        assert!(q.provide_auth(authed, "Bearer new".into()));
        let item = q.get_item(authed).unwrap();
        assert_eq!((item.status.clone(), item.options.auth_header.as_deref()), (QueueItemStatus::Paused, Some("Bearer new")));
    }

    #[test]
    fn queue_round_trips_through_json_without_credentials() {
        let (mut q, ids) = queue_of(&["https://e.com/a%20b?x=1"]);
        let authed = q.add_item(vec![url("https://e.com/p")], DownloadOptions { auth_header: Some("Bearer secret".into()), ..Default::default() });
        q.mark_started(ids[0]);
        q.apply_snapshot(ids[0], &EngineSnapshot { progress_ratio: f64::NAN, ..snapshot("/dl/a b") });
        q.mark_pausing(ids[0]);
        q.finish(ids[0], Err(CANCELLED.into()));

        let json = serde_json::to_string(&q).unwrap();
        assert!(!json.contains("secret"));
        let back: DownloadQueue = serde_json::from_str(&json).unwrap();
        let item = back.get_item(ids[0]).unwrap();
        assert_eq!(item.urls, vec![url("https://e.com/a%20b?x=1")]);
        assert_eq!((item.status.clone(), item.downloaded_bytes, item.progress_ratio), (QueueItemStatus::Paused, 40, 0.0));
        assert_eq!(item.target_path.as_deref(), Some(std::path::Path::new("/dl/a b")));
        assert_eq!(back.get_item(authed).unwrap().options.auth_header, None);
        let mut back = back;
        assert!(back.add_item(vec![url("https://e.com/n")], DownloadOptions::default()) > authed, "ids keep counting");
        assert!(serde_json::from_str::<DownloadQueue>(&json.replace("https://e.com/p", "not a url")).is_err());
    }

    #[test]
    fn finished_items_are_saved_without_secrets_and_ask_for_the_link_again() {
        let (mut q, ids) = queue_of(&[
            "https://me:pw@e.com/done.bin?token=t1",
            "https://e.com/failed.bin?sig=s2",
            "https://e.com/paused.bin?sig=s3",
            "https://e.com/queued.bin?sig=s4",
        ]);
        let mirrored = q.add_item(vec![url("https://e.com/m.bin?sig=s5"), url("https://f.com/m.bin")], DownloadOptions::default());
        for id in [ids[0], ids[1], ids[2], mirrored] {
            q.mark_started(id);
        }
        q.finish(ids[0], Ok((PathBuf::from("/dl/done.bin"), Some(1))));
        q.finish(ids[1], Err("https://e.com/failed.bin?sig=s2: HTTP 403".into()));
        q.finish(mirrored, Err("HTTP 500".into()));

        let json = serde_json::to_string(&{
            let mut saved = q.clone();
            saved.settle_for_restart();
            saved
        })
        .unwrap();
        for secret in ["pw", "t1", "s2", "s5"] {
            assert!(!json.contains(secret), "{secret} in {json}");
        }
        let mut back: DownloadQueue = serde_json::from_str(&json).unwrap();
        let item = |q: &DownloadQueue, id| q.get_item(id).unwrap().clone();
        assert_eq!(item(&back, ids[0]).urls, vec![url("https://e.com/done.bin?token=REDACTED")]);
        assert_eq!(item(&back, ids[1]).status, QueueItemStatus::Failed("https://e.com/failed.bin?sig=REDACTED: HTTP 403".into()));
        // Unfinished items keep their links whole, to resume.
        assert_eq!(item(&back, ids[2]).urls, vec![url("https://e.com/paused.bin?sig=s3")]);
        assert_eq!(item(&back, ids[3]).urls, vec![url("https://e.com/queued.bin?sig=s4")]);

        // A retry never requests a redacted link: it fails, saying so, or keeps the other mirrors.
        assert!(!back.mark_started(ids[1]));
        assert_eq!(item(&back, ids[1]).status, QueueItemStatus::Failed(REDACTED_LINK.to_string()));
        assert!(back.mark_started(mirrored));
        assert_eq!(item(&back, mirrored).urls, vec![url("https://f.com/m.bin")]);
        assert!(back.mark_started(ids[2]));
        // The live queue is untouched until it is saved.
        assert_eq!(item(&q, ids[1]).urls, vec![url("https://e.com/failed.bin?sig=s2")]);
        assert!(q.mark_started(ids[1]));
    }
}
