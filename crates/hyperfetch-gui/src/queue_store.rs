use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyperfetch_core::queue::DownloadQueue;

use crate::settings::{app_file, write_atomic};

/// Changes made within this long of the first unsaved one are written together.
const SAVE_DELAY: Duration = Duration::from_secs(1);

pub fn path() -> PathBuf {
    app_file("gui-queue.json")
}

/// The saved queue, settled for a restart, plus a message for the user when it could not be
/// used. A missing file is an empty queue; a damaged one is moved aside (never overwritten)
/// and the queue starts empty.
pub fn load(path: &Path) -> (DownloadQueue, Option<String>) {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (DownloadQueue::new(), None),
        Err(e) => return (DownloadQueue::new(), Some(format!("Could not read the saved queue {}: {}", path.display(), e))),
    };
    match serde_json::from_slice::<DownloadQueue>(&bytes) {
        Ok(mut queue) => {
            queue.settle_for_restart();
            (queue, None)
        }
        Err(e) => {
            let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let mut aside = path.as_os_str().to_owned();
            aside.push(format!(".corrupt-{}", secs));
            let aside = PathBuf::from(aside);
            let message = match std::fs::rename(path, &aside) {
                Ok(()) => format!("The saved queue was damaged ({}); it was moved to {}", e, aside.display()),
                Err(moved) => format!("The saved queue was damaged ({}) and could not be moved aside: {}", e, moved),
            };
            (DownloadQueue::new(), Some(message))
        }
    }
}

/// Writes `queue` as it will look after a restart (see `DownloadQueue::settle_for_restart`).
pub fn save(path: &Path, mut queue: DownloadQueue) -> std::io::Result<()> {
    queue.settle_for_restart();
    let json = serde_json::to_vec_pretty(&queue).map_err(std::io::Error::other)?;
    write_atomic(path, &json)
}

/// Saves the queue on its own thread, at most once per `SAVE_DELAY` and always the latest.
pub struct Saver {
    tx: mpsc::Sender<DownloadQueue>,
    thread: JoinHandle<()>,
}

impl Saver {
    pub fn spawn(path: PathBuf) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<DownloadQueue>();
        let thread = std::thread::Builder::new().name("queue-saver".to_string()).spawn(move || {
            while let Ok(mut queue) = rx.recv() {
                // Also ends at once when the sender is gone, so the last queue is written on exit.
                let deadline = Instant::now() + SAVE_DELAY;
                while let Ok(newer) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    queue = newer;
                }
                if let Err(e) = save(&path, queue) {
                    tracing::warn!("Failed to save the queue to {}: {}", path.display(), e);
                }
            }
        })?;
        Ok(Self { tx, thread })
    }

    pub fn save(&self, queue: DownloadQueue) {
        let _ = self.tx.send(queue);
    }

    /// Saves `queue` and waits until it is written.
    pub fn finish(self, queue: DownloadQueue) {
        self.save(queue);
        drop(self.tx);
        let _ = self.thread.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperfetch_core::engine::DownloadOptions;
    use hyperfetch_core::queue::QueueItemStatus;
    use url::Url;

    fn add(queue: &mut DownloadQueue, url: &str, auth: Option<&str>) -> usize {
        let options = DownloadOptions { auth_header: auth.map(str::to_string), ..Default::default() };
        queue.add_item(vec![Url::parse(url).unwrap()], options)
    }

    #[test]
    fn saved_queue_comes_back_ready_to_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("gui-queue.json");
        let mut queue = DownloadQueue::new();
        let running = add(&mut queue, "https://e.com/big.iso", None);
        let queued = add(&mut queue, "https://e.com/next.iso", None);
        let done = add(&mut queue, "https://e.com/done.iso", None);
        let private = add(&mut queue, "https://e.com/private.iso", Some("Bearer secret"));
        queue.mark_started(running);
        queue.mark_started(done);
        queue.finish(done, Ok((PathBuf::from("/dl/done.iso"), Some(7))));
        queue.mark_started(private);

        save(&path, queue.clone()).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("secret"), "the credential is never written");
        assert_eq!(queue.get_item(running).unwrap().status, QueueItemStatus::Downloading, "the live queue is untouched");

        let (restored, message) = load(&path);
        assert_eq!(message, None);
        let status = |id| restored.get_item(id).unwrap().status.clone();
        assert_eq!(status(running), QueueItemStatus::Paused);
        assert_eq!(status(queued), QueueItemStatus::Queued);
        assert_eq!(status(done), QueueItemStatus::Completed);
        assert_eq!(status(private), QueueItemStatus::AuthRequired);
        assert_eq!(restored.get_item(done).unwrap().total_bytes, 7);
        assert_eq!(restored.next_to_start(8), Some(queued));
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "no temp file is left behind");
    }

    #[test]
    fn missing_or_damaged_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui-queue.json");
        let (queue, message) = load(&path);
        assert!(queue.items().is_empty() && message.is_none());

        std::fs::write(&path, b"{\"items\": [").unwrap();
        let (queue, message) = load(&path);
        assert!(queue.items().is_empty());
        assert!(message.unwrap().contains("moved to"));
        assert!(!path.exists(), "the damaged file is not left to be overwritten");
        let aside: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(aside.len(), 1);
        assert!(aside[0].to_string_lossy().starts_with("gui-queue.json.corrupt-"));
        assert_eq!(std::fs::read(dir.path().join(&aside[0])).unwrap(), b"{\"items\": [");
    }

    #[test]
    fn saver_writes_the_latest_queue_on_finish() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui-queue.json");
        let saver = Saver::spawn(path.clone()).unwrap();
        let mut queue = DownloadQueue::new();
        add(&mut queue, "https://e.com/a", None);
        saver.save(queue.clone());
        add(&mut queue, "https://e.com/b", None);
        // Finishing does not wait for the save delay and writes the last queue given.
        let started = Instant::now();
        saver.finish(queue);
        assert!(started.elapsed() < SAVE_DELAY);
        assert_eq!(load(&path).0.items().len(), 2);
    }
}
