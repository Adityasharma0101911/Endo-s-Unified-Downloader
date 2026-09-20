use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use reqwest::header::{ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use reqwest::Client;
use tokio::sync::{broadcast, mpsc};
use url::Url;

use crate::chunk::ChunkManager;
use crate::mirror::MirrorRacer;
use crate::range::ByteRange;
use crate::state::DownloadState;
use crate::storage::DiskWriter;
use crate::worker::{HttpWorker, WorkerEvent};

#[derive(Debug, Clone)]
pub struct EngineSnapshot {
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub speed_bytes_per_sec: f64,
    pub progress_ratio: f64,
    pub active_workers: usize,
    pub mirror_speeds: Vec<(usize, String, f64)>, // (id, host, bytes_per_sec)
}

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub num_connections: usize,
    pub base_chunk_size: u64,
    pub min_steal_threshold: u64,
    pub output_path: Option<PathBuf>,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            num_connections: 8,
            base_chunk_size: 4 * 1024 * 1024,      // 4MB
            min_steal_threshold: 1024 * 1024,     // 1MB
            output_path: None,
        }
    }
}

pub struct DownloadEngine {
    options: DownloadOptions,
    urls: Vec<Url>,
    client: Client,
    cancel_flag: Arc<AtomicBool>,
}

impl DownloadEngine {
    pub fn new(urls: Vec<Url>, options: DownloadOptions) -> Self {
        let client = Client::builder()
            .tcp_nodelay(true)
            .pool_max_idle_per_host(16)
            .build()
            .unwrap_or_default();

        Self {
            options,
            urls,
            client,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
    }

    /// Probes mirrors to determine file size, range support, and proposed file name.
    pub async fn probe_mirrors(&self, urls: &[Url]) -> Result<(u64, bool, String), String> {
        for url in urls {
            // First try HEAD
            if let Ok(resp) = self.client.head(url.clone()).send().await {
                let status = resp.status();
                if status.is_success() {
                    let accepts_ranges = resp.headers()
                        .get(ACCEPT_RANGES)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.eq_ignore_ascii_case("bytes"))
                        .unwrap_or(false);

                    let content_len = resp.headers()
                        .get(CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());

                    let filename = extract_filename(&resp.headers(), url);

                    if let Some(len) = content_len {
                        return Ok((len, accepts_ranges, filename));
                    }
                }
            }

            // Fallback to GET with Range: bytes=0-0
            if let Ok(resp) = self.client
                .get(url.clone())
                .header(RANGE, "bytes=0-0")
                .send()
                .await
            {
                let status = resp.status();
                let accepts_ranges = status == reqwest::StatusCode::PARTIAL_CONTENT;
                let filename = extract_filename(&resp.headers(), url);

                if let Some(cr) = resp.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
                    if let Ok((_, Some(total))) = ByteRange::parse_content_range(cr) {
                        return Ok((total, accepts_ranges, filename));
                    }
                }

                if let Some(cl) = resp.headers().get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
                    return Ok((cl, accepts_ranges, filename));
                }
            }
        }

        Err("Failed to probe file information from provided mirror URLs".to_string())
    }

    /// Starts the high-speed multi-connection download pipeline with work stealing.
    pub async fn run(
        &self,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<PathBuf, String> {
        // Automatically resolve multi-cluster mirrors (e.g. Archive.org workable_servers)
        let mut resolved_urls = Vec::new();
        for url in &self.urls {
            let mirrors = crate::resolver::SmartResolver::resolve_mirrors(&self.client, url).await;
            resolved_urls.extend(mirrors);
        }

        let (file_size, accepts_ranges, filename) = self.probe_mirrors(&resolved_urls).await?;

        let output_path = match &self.options.output_path {
            Some(p) if p.is_dir() => p.join(&filename),
            Some(p) => p.clone(),
            None => PathBuf::from(&filename),
        };
        let state_path = DownloadState::state_file_path(&output_path);

        tracing::info!(
            "Starting download: {} ({} bytes) -> {:?}",
            output_path.display(),
            file_size,
            output_path
        );

        // Check for resume state
        let resumed_state = DownloadState::load_from_path(&state_path).ok().flatten();
        let chunk_manager = if let Some(ref state) = resumed_state {
            tracing::info!("Found existing .hfstate with {} completed ranges", state.completed_ranges.len());
            ChunkManager::with_resumed_ranges(
                file_size,
                self.options.base_chunk_size,
                &state.completed_ranges,
            ).map_err(|e| e.to_string())?
        } else {
            ChunkManager::new(file_size, self.options.base_chunk_size)
                .map_err(|e| e.to_string())?
        };

        let chunk_manager = Arc::new(Mutex::new(chunk_manager));
        let mirror_racer = Arc::new(Mutex::new(MirrorRacer::new(resolved_urls.clone())));
        let disk_writer = DiskWriter::open_or_create(&output_path, file_size)
            .map_err(|e| e.to_string())?;

        let (event_tx, mut event_rx) = mpsc::channel::<WorkerEvent>(1024);

        // Spawn workers
        let num_workers = if accepts_ranges {
            self.options.num_connections.min(32)
        } else {
            1 // Single connection if server doesn't support ranges
        };

        for worker_id in 0..num_workers {
            let chunk_mgr = Arc::clone(&chunk_manager);
            let racer = Arc::clone(&mirror_racer);
            let writer = disk_writer.clone();
            let tx = event_tx.clone();
            let cancel = Arc::clone(&self.cancel_flag);
            let client = self.client.clone();
            let min_steal = self.options.min_steal_threshold;

            tokio::spawn(async move {
                let worker = HttpWorker::new(worker_id, client, writer, tx);

                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }

                    // 1. Try to get unassigned work
                    let (work, mirror_id, mirror_url) = {
                        let mut mgr = chunk_mgr.lock();
                        let mut r = racer.lock();

                        let mirror_id = r.select_best_mirror().unwrap_or(0);
                        let mirror_url = r.get_mirror(mirror_id).map(|m| m.url.clone()).unwrap();

                        let work = mgr.get_next_work(worker_id, mirror_id);
                        if let Some(chunk) = work {
                            r.acquire_mirror(mirror_id);
                            (Some(chunk), mirror_id, mirror_url)
                        } else {
                            // 2. No unassigned work, attempt to steal work
                            if let Some((_victim_id, stolen)) = mgr.steal_work(worker_id, mirror_id, min_steal) {
                                r.acquire_mirror(mirror_id);
                                (Some(stolen), mirror_id, mirror_url)
                            } else {
                                (None, mirror_id, mirror_url)
                            }
                        }
                    };

                    if let Some(chunk) = work {
                        worker.download_chunk(chunk, mirror_id, mirror_url, Arc::clone(&cancel)).await;
                        racer.lock().release_mirror(mirror_id);
                    } else {
                        // Check if download is completely finished
                        let all_done = chunk_mgr.lock().is_all_completed();
                        if all_done {
                            break;
                        }
                        // Sleep briefly before polling again
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            });
        }

        // Drop the original sender so the channel closes when all workers finish
        drop(event_tx);

        let mut last_snapshot_time = Instant::now();
        let mut last_snapshot_downloaded = chunk_manager.lock().total_downloaded();
        let mut last_state_save = Instant::now();

        // Event processing loop
        while let Some(event) = event_rx.recv().await {
            match event {
                WorkerEvent::Ttfb { mirror_id, ttfb, .. } => {
                    mirror_racer.lock().get_mirror_mut(mirror_id).map(|m| m.record_ttfb(ttfb));
                }
                WorkerEvent::Progress { worker_id, chunk_id, mirror_id, bytes_received, duration } => {
                    let _ = chunk_manager.lock().update_chunk_progress(chunk_id, bytes_received, worker_id, mirror_id);
                    mirror_racer.lock().get_mirror_mut(mirror_id).map(|m| {
                        m.record_progress(bytes_received, duration);
                        m.record_success();
                    });
                }
                WorkerEvent::ChunkCompleted { chunk_id, hash, .. } => {
                    let _ = chunk_manager.lock().mark_completed(chunk_id, hash);
                }
                WorkerEvent::ChunkFailed { chunk_id, mirror_id, error, .. } => {
                    tracing::warn!("Chunk {} failed on mirror {}: {}", chunk_id, mirror_id, error);
                    let _ = chunk_manager.lock().mark_failed(chunk_id, &error);
                    mirror_racer.lock().get_mirror_mut(mirror_id).map(|m| m.record_failure(Instant::now()));
                }
            }

            // Periodic snapshot & UI broadcast (every 100ms)
            let now = Instant::now();
            if now.duration_since(last_snapshot_time) >= Duration::from_millis(100) {
                let current_downloaded = chunk_manager.lock().total_downloaded();
                let dt = now.duration_since(last_snapshot_time).as_secs_f64();
                let speed = if dt > 0.0 {
                    (current_downloaded.saturating_sub(last_snapshot_downloaded) as f64) / dt
                } else {
                    0.0
                };

                let mirror_speeds = mirror_racer.lock().mirrors().iter().map(|m| {
                    (m.id, m.url.host_str().unwrap_or("unknown").to_string(), m.speed_ewma)
                }).collect();

                let snapshot = EngineSnapshot {
                    total_bytes: file_size,
                    downloaded_bytes: current_downloaded,
                    speed_bytes_per_sec: speed,
                    progress_ratio: if file_size == 0 { 1.0 } else { current_downloaded as f64 / file_size as f64 },
                    active_workers: num_workers,
                    mirror_speeds,
                };

                if let Some(ref tx) = snapshot_tx {
                    let _ = tx.send(snapshot);
                }

                last_snapshot_time = now;
                last_snapshot_downloaded = current_downloaded;
            }

            // Periodic state persistence (every 1 second)
            if now.duration_since(last_state_save) >= Duration::from_secs(1) {
                let completed = chunk_manager.lock().completed_ranges();
                let mut state = DownloadState::new(
                    output_path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                    file_size,
                    self.options.base_chunk_size,
                    self.urls.iter().map(|u| u.to_string()).collect(),
                );
                state.completed_ranges = completed;
                let _ = state.save_atomic(&state_path);
                last_state_save = now;
            }

            if chunk_manager.lock().is_all_completed() {
                break;
            }
        }

        // Finalize
        disk_writer.sync().map_err(|e| e.to_string())?;

        // Compute final BLAKE3 hash
        let final_hash = disk_writer.compute_file_hash().map_err(|e| e.to_string())?;
        tracing::info!("Download completed! BLAKE3: {}", hex_encode(&final_hash));

        // Clean up state file on success
        let _ = DownloadState::remove(&state_path);

        Ok(output_path)
    }
}

fn extract_filename(headers: &reqwest::header::HeaderMap, url: &Url) -> String {
    if let Some(cd) = headers.get(CONTENT_DISPOSITION).and_then(|v| v.to_str().ok()) {
        if let Some(idx) = cd.find("filename=") {
            let sub = &cd[idx + 9..];
            let name = sub.trim_matches('"').split(';').next().unwrap_or("").trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }

    if let Some(mut segments) = url.path_segments() {
        if let Some(last) = segments.next_back() {
            if !last.is_empty() {
                return last.to_string();
            }
        }
    }

    "downloaded_file.bin".to_string()
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}
