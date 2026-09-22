use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use reqwest::header::{ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use reqwest::Client;
use tokio::sync::{broadcast, mpsc};
use url::Url;

use crate::chunk::{ChunkManager, ChunkSnapshot};
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
    pub chunks: Vec<ChunkSnapshot>,
    pub target_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub num_connections: usize,
    pub base_chunk_size: u64,
    pub min_steal_threshold: u64,
    pub output_path: Option<PathBuf>,
    pub expected_checksum: Option<String>,
    pub cookies_path: Option<PathBuf>,
    pub auth_header: Option<String>,
    pub proxy: Option<String>,
    pub media_preset: Option<crate::media::MediaQualityPreset>,
    pub browser_cookies: Option<crate::media::BrowserCookieSource>,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            num_connections: 8,
            base_chunk_size: 4 * 1024 * 1024,      // 4MB
            min_steal_threshold: 1024 * 1024,     // 1MB
            output_path: None,
            expected_checksum: None,
            cookies_path: None,
            auth_header: None,
            proxy: None,
            media_preset: None,
            browser_cookies: None,
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
        let mut builder = Client::builder()
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_secs(10))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_max_idle_per_host(64)
            .pool_idle_timeout(Some(Duration::from_secs(90)));

        let mut headers = crate::resolver::SmartResolver::default_anti_qos_headers();
        if let Some(ref auth) = options.auth_header {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(auth) {
                headers.insert(reqwest::header::AUTHORIZATION, val);
            }
        }
        builder = builder.default_headers(headers);

        if let Some(ref proxy_url) = options.proxy {
            if let Ok(proxy) = reqwest::Proxy::all(proxy_url) {
                builder = builder.proxy(proxy);
            } else {
                tracing::warn!("Failed to parse proxy URL: {}", proxy_url);
            }
        }

        if let Some(ref cookies_path) = options.cookies_path {
            if let Ok(content) = std::fs::read_to_string(cookies_path) {
                let jar = Arc::new(reqwest::cookie::Jar::default());
                crate::resolver::parse_netscape_cookies(&content, &jar);
                builder = builder.cookie_provider(jar);
            } else {
                tracing::warn!("Failed to read cookies file at: {:?}", cookies_path);
            }
        }

        let client = builder.build().unwrap_or_default();

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

                    let filename = extract_filename(resp.headers(), url);

                    if let Some(len) = content_len {
                        return Ok((len, accepts_ranges, filename));
                    }
                }
            }

            // Fallback to GET with Range: bytes=0-0
            if let Ok(resp) = self.client
                .get(url.clone())
                .header(RANGE, "bytes=0-0")
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .send()
                .await
            {
                let status = resp.status();
                let accepts_ranges = status == reqwest::StatusCode::PARTIAL_CONTENT;
                let filename = extract_filename(resp.headers(), url);

                if let Some(cr) = resp.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
                    if let Ok((_, Some(total))) = ByteRange::parse_content_range(cr) {
                        return Ok((total, accepts_ranges, filename));
                    }
                }

                if status == reqwest::StatusCode::OK {
                    if let Some(cl) = resp.headers().get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
                        return Ok((cl, false, filename));
                    }
                }
            }

            // Fallback to standard GET without Range (handles servers that block HEAD or reject Range 0-0)
            if let Ok(resp) = self.client.get(url.clone()).send().await {
                if resp.status().is_success() {
                    let accepts_ranges = resp.headers()
                        .get(ACCEPT_RANGES)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.eq_ignore_ascii_case("bytes"))
                        .unwrap_or(false);
                    let filename = extract_filename(resp.headers(), url);

                    if let Some(cl) = resp.headers().get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
                        return Ok((cl, accepts_ranges, filename));
                    }
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
        // Helper to check if a URL is a direct file or archive that should never be sent to the media engine
        let is_direct_file_or_archive = |u: &Url| -> bool {
            let path = u.path().to_ascii_lowercase();
            let is_archive_host = u.host_str().map_or(false, |h| h.ends_with("archive.org"));
            is_archive_host
                || path.ends_with(".7z")
                || path.ends_with(".zip")
                || path.ends_with(".rar")
                || path.ends_with(".tar")
                || path.ends_with(".gz")
                || path.ends_with(".bz2")
                || path.ends_with(".xz")
                || path.ends_with(".iso")
                || path.ends_with(".bin")
                || path.ends_with(".exe")
                || path.ends_with(".msi")
                || path.ends_with(".dmg")
                || path.ends_with(".pkg")
                || path.ends_with(".deb")
                || path.ends_with(".rpm")
                || path.ends_with(".apk")
                || path.ends_with(".pdf")
                || path.ends_with(".torrent")
        };

        // Check if any URL is a supported media site (YouTube, Twitch, TikTok, etc.)
        // Non-media-site URLs are only routed to media engine if a media preset was explicitly provided
        // AND the target is not a direct archive/binary file or Archive.org resource.
        let media_target = self.urls.iter().find(|u| crate::media::is_supported_media_site(u)).cloned().or_else(|| {
            if self.options.media_preset.is_some() {
                self.urls.iter().find(|u| !is_direct_file_or_archive(u)).cloned()
            } else {
                None
            }
        });

        if let Some(media_url) = media_target {
            tracing::info!("Routing download to Media Engine: {}", media_url);
            let (prog_tx, mut prog_rx) = tokio::sync::mpsc::channel::<crate::media::ProgressUpdate>(64);
            let snapshot_tx_clone = snapshot_tx.clone();

            // Forward progress updates to snapshot_tx
            let forwarder = tokio::spawn(async move {
                while let Some(update) = prog_rx.recv().await {
                    if let Some(ref tx) = snapshot_tx_clone {
                        let ratio = if update.total > 0 {
                            (update.downloaded as f64 / update.total as f64).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        let _ = tx.send(EngineSnapshot {
                            total_bytes: update.total,
                            downloaded_bytes: update.downloaded,
                            speed_bytes_per_sec: update.speed,
                            progress_ratio: ratio,
                            active_workers: update.active_connections,
                            mirror_speeds: vec![],
                            chunks: vec![],
                            target_path: None,
                        });
                    }
                }
            });

            let output_dir = match &self.options.output_path {
                Some(p) if p.is_dir() => p.clone(),
                Some(p) => p.parent().unwrap_or(std::path::Path::new(".")).to_path_buf(),
                None => PathBuf::from("."),
            };

            let output_filename = match &self.options.output_path {
                Some(p) if !p.is_dir() => p.file_name().map(|n| n.to_string_lossy().to_string()),
                _ => None,
            };

            let cookie_source = if let Some(ref bc) = self.options.browser_cookies {
                bc.clone()
            } else if let Some(ref cp) = self.options.cookies_path {
                crate::media::BrowserCookieSource::File(cp.clone())
            } else {
                crate::media::BrowserCookieSource::None
            };

            let media_opts = crate::media::MediaDownloadOptions {
                preset: self.options.media_preset.clone().unwrap_or_default(),
                cookies: cookie_source,
                proxy: self.options.proxy.clone(),
                output_dir,
                output_filename,
                custom_ytdlp_path: None,
                concurrent_fragments: self.options.num_connections.clamp(1, 32),
            };

            let res = crate::media::download_media(
                &media_url,
                &media_opts,
                Some(prog_tx),
                Some(Arc::clone(&self.cancel_flag)),
            ).await;

            let _ = forwarder.await;

            let final_path = res?;

            // Checksum verification if requested
            if let Some(ref expected) = self.options.expected_checksum {
                match crate::storage::DiskWriter::verify_file_checksum(&final_path, expected) {
                    Ok(true) => tracing::info!("Checksum verification passed: {}", expected),
                    Ok(false) => {
                        let err = format!("Checksum verification failed: hash mismatch (expected {})", expected);
                        tracing::error!("{}", err);
                        return Err(err);
                    }
                    Err(e) => {
                        tracing::error!("Checksum verification failed: {}", e);
                        return Err(format!("Checksum verification failed: {}", e));
                    }
                }
            }

            return Ok(final_path);
        }

        // Automatically resolve multi-cluster mirrors (e.g. Archive.org workable_servers)
        let mut resolved_urls = Vec::new();
        for url in &self.urls {
            let mirrors = crate::resolver::SmartResolver::resolve_mirrors(&self.client, url).await;
            resolved_urls.extend(mirrors);
        }

        // Check if any resolved URL is an HLS streaming playlist (.m3u8)
        for url in &resolved_urls {
            if url.path().ends_with(".m3u8") || url.as_str().contains(".m3u8") {
                tracing::info!("Detected HLS video stream: {}", url);
                match crate::hls::parse_hls_playlist(&self.client, url).await {
                    Ok(segments) => {
                        let base_filename = extract_filename(&reqwest::header::HeaderMap::new(), url);
                        let mut target_name = PathBuf::from(base_filename);
                        if target_name.extension().map_or(true, |ext| ext == "m3u8") {
                            target_name.set_extension("mp4");
                        }

                        let out_path = match &self.options.output_path {
                            Some(p) if p.is_dir() => p.join(&target_name),
                            Some(p) => p.clone(),
                            None => target_name,
                        };

                        return crate::hls::HlsEngine::download(
                            &self.client,
                            segments,
                            &out_path,
                            self.options.num_connections,
                            snapshot_tx,
                            Some(Arc::clone(&self.cancel_flag)),
                        ).await.map_err(|e| e.to_string());
                    }
                    Err(e) => {
                        tracing::warn!("HLS playlist parsing failed, falling back to direct download: {}", e);
                    }
                }
            }
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

        if file_size == 0 {
            let disk_writer = DiskWriter::open_or_create(&output_path, 0)
                .map_err(|e| e.to_string())?;
            disk_writer.sync().map_err(|e| e.to_string())?;
            let final_hash = disk_writer.compute_file_hash().map_err(|e| e.to_string())?;
            tracing::info!("Download completed! (0-byte file) BLAKE3: {}", hex_encode(&final_hash));
            let _ = DownloadState::remove(&state_path);
            return Ok(output_path);
        }

        // Determine worker concurrency
        let num_workers = if accepts_ranges {
            self.options.num_connections.clamp(1, 64)
        } else {
            1 // Single connection if server doesn't support ranges
        };

        let effective_chunk_size = if accepts_ranges {
            if self.options.base_chunk_size != 4 * 1024 * 1024 {
                // User explicitly configured custom chunk size
                self.options.base_chunk_size
            } else if file_size >= 1024 * 1024 * 1024 { // >= 1GB (e.g. 17.6GB)
                // For multi-gigabyte files, scale chunks up to 64MB - 256MB so connections
                // remain in continuous high-speed streaming mode (similar to aria2c stream segments).
                let dynamic_size = file_size / (num_workers as u64 * 2);
                dynamic_size.clamp(64 * 1024 * 1024, 256 * 1024 * 1024)
            } else if file_size >= 100 * 1024 * 1024 { // >= 100MB
                let dynamic_size = file_size / (num_workers as u64 * 2);
                dynamic_size.clamp(16 * 1024 * 1024, 64 * 1024 * 1024)
            } else if file_size >= 16 * 1024 * 1024 { // >= 16MB
                let dynamic_size = file_size / num_workers as u64;
                dynamic_size.clamp(4 * 1024 * 1024, 16 * 1024 * 1024)
            } else {
                (file_size / num_workers as u64).max(64 * 1024)
            }
        } else {
            file_size
        };

        let effective_min_steal = if self.options.min_steal_threshold != 1024 * 1024 {
            self.options.min_steal_threshold
        } else {
            (effective_chunk_size / 4).max(1024 * 1024)
        };

        // Check for resume state
        let resumed_state = DownloadState::load_from_path(&state_path).ok().flatten();
        let chunk_manager = if let Some(ref state) = resumed_state {
            tracing::info!("Found existing .hfstate with {} completed ranges", state.completed_ranges.len());
            ChunkManager::with_resumed_ranges(
                file_size,
                effective_chunk_size,
                &state.completed_ranges,
            ).map_err(|e| e.to_string())?
        } else {
            ChunkManager::new(file_size, effective_chunk_size)
                .map_err(|e| e.to_string())?
        };

        let chunk_manager = Arc::new(Mutex::new(chunk_manager));
        let mirror_racer = Arc::new(Mutex::new(MirrorRacer::new(resolved_urls.clone())));
        let disk_writer = DiskWriter::open_or_create(&output_path, file_size)
            .map_err(|e| e.to_string())?;

        let (event_tx, mut event_rx) = mpsc::channel::<WorkerEvent>(1024);

        for worker_id in 0..num_workers {
            let chunk_mgr = Arc::clone(&chunk_manager);
            let racer = Arc::clone(&mirror_racer);
            let writer = disk_writer.clone();
            let tx = event_tx.clone();
            let cancel = Arc::clone(&self.cancel_flag);
            let client = self.client.clone();
            let min_steal = effective_min_steal;

            tokio::spawn(async move {
                let worker = HttpWorker::new(worker_id, client, writer, tx);

                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }

                    if chunk_mgr.lock().has_fatal_failure().is_some() {
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
                        // Check if download is completely finished or fatally failed
                        let should_break = {
                            let mgr = chunk_mgr.lock();
                            mgr.is_all_completed() || mgr.has_fatal_failure().is_some()
                        };
                        if should_break {
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

                let chunks = chunk_manager.lock().chunk_snapshots();

                let snapshot = EngineSnapshot {
                    total_bytes: file_size,
                    downloaded_bytes: current_downloaded,
                    speed_bytes_per_sec: speed,
                    progress_ratio: if file_size == 0 { 1.0 } else { current_downloaded as f64 / file_size as f64 },
                    active_workers: num_workers,
                    mirror_speeds,
                    chunks,
                    target_path: Some(output_path.clone()),
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
                state.completed_ranges = crate::range::merge_ranges(completed);
                let _ = state.save_atomic(&state_path);
                last_state_save = now;
            }

            if chunk_manager.lock().has_fatal_failure().is_some() {
                break;
            }

            if chunk_manager.lock().is_all_completed() {
                break;
            }
        }

        // Verify that the download actually finished
        let is_completed = chunk_manager.lock().is_all_completed();
        if !is_completed {
            if self.cancel_flag.load(Ordering::Relaxed) {
                // Flush dirty pages and save completed ranges so user can resume anytime!
                let _ = disk_writer.sync();
                let completed = chunk_manager.lock().completed_ranges();
                let mut state = DownloadState::new(
                    output_path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                    file_size,
                    effective_chunk_size,
                    self.urls.iter().map(|u| u.to_string()).collect(),
                );
                state.completed_ranges = crate::range::merge_ranges(completed);
                let _ = state.save_atomic(&state_path);
                return Err("Download cancelled by user".to_string());
            }
            if let Some((failed_id, reason)) = chunk_manager.lock().has_fatal_failure() {
                return Err(format!("Download failed: chunk {} failed after max retries: {}", failed_id, reason));
            }
            return Err("Download aborted: workers terminated before all chunks completed".to_string());
        }

        // Finalize
        disk_writer.sync().map_err(|e| e.to_string())?;

        // Compute final BLAKE3 hash
        let final_hash = disk_writer.compute_file_hash().map_err(|e| e.to_string())?;
        tracing::info!("Download completed! BLAKE3: {}", hex_encode(&final_hash));

        // Post-download checksum verification
        if let Some(ref expected) = self.options.expected_checksum {
            match disk_writer.verify_checksum(expected) {
                Ok(true) => {
                    tracing::info!("Checksum verification succeeded for {:?}", output_path);
                }
                Ok(false) => {
                    let err = format!("Checksum verification failed: hash mismatch (expected {})", expected);
                    tracing::error!("{}", err);
                    return Err(err);
                }
                Err(e) => {
                    tracing::error!("Checksum verification failed: {}", e);
                    return Err(format!("Checksum verification failed: {}", e));
                }
            }
        }

        // Clean up state file on success
        let _ = DownloadState::remove(&state_path);

        Ok(output_path)
    }
}

fn extract_filename(headers: &reqwest::header::HeaderMap, url: &Url) -> String {
    if let Some(cd) = headers.get(CONTENT_DISPOSITION).and_then(|v| v.to_str().ok()) {
        // First check filename*= (RFC 5987 / 6266)
        if let Some(idx) = cd.find("filename*=") {
            let sub = &cd[idx + 10..];
            let raw = sub.trim_matches('"').split(';').next().unwrap_or("").trim();
            // Format: UTF-8''encoded_name
            let name = if let Some(pos) = raw.to_ascii_lowercase().find("utf-8''") {
                percent_decode_str(&raw[pos + 7..])
            } else {
                raw.to_string()
            };
            let sanitized = sanitize_filename(&name);
            if !sanitized.is_empty() {
                return sanitized;
            }
        }

        // Then check filename=
        if let Some(idx) = cd.find("filename=") {
            let sub = &cd[idx + 9..];
            let name = sub.trim_matches('"').split(';').next().unwrap_or("").trim();
            let sanitized = sanitize_filename(name);
            if !sanitized.is_empty() {
                return sanitized;
            }
        }
    }

    if let Some(mut segments) = url.path_segments() {
        if let Some(last) = segments.next_back() {
            let decoded = percent_decode_str(last);
            let sanitized = sanitize_filename(&decoded);
            if !sanitized.is_empty() {
                return sanitized;
            }
        }
    }

    "downloaded_file.bin".to_string()
}

fn percent_decode_str(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(c1), Some(c2)) = (h1, h2) {
                let hex_str = [c1, c2];
                if let Ok(s) = std::str::from_utf8(&hex_str) {
                    if let Ok(val) = u8::from_str_radix(s, 16) {
                        bytes.push(val);
                        continue;
                    }
                }
                bytes.push(b'%');
                bytes.push(c1);
                bytes.push(c2);
            } else {
                bytes.push(b'%');
                if let Some(c1) = h1 { bytes.push(c1); }
            }
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn sanitize_filename(name: &str) -> String {
    // Replace illegal Windows characters: < > : " / \ | ? *
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 32 => '_',
            c => c,
        })
        .collect();

    let trimmed = cleaned.trim().trim_matches('.').to_string();
    let upper = trimmed.to_ascii_uppercase();
    let is_reserved = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
            | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    );

    if is_reserved {
        format!("{}_file", trimmed)
    } else {
        trimmed
    }
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}
