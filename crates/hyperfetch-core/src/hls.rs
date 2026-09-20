use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use reqwest::Client;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, Semaphore};
use url::Url;
use thiserror::Error;
use crate::engine::EngineSnapshot;

#[derive(Error, Debug)]
pub enum HlsError {
    #[error("Network error during HLS transfer: {0}")]
    Network(#[from] reqwest::Error),
    #[error("I/O error during HLS assembly: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid HLS playlist: {0}")]
    InvalidPlaylist(String),
    #[error("No video segments found in playlist")]
    NoSegments,
}

#[derive(Debug, Clone)]
pub struct HlsSegment {
    pub index: usize,
    pub url: Url,
    pub duration_secs: f64,
}

/// Parses an HLS (.m3u8) playlist. If it is a master playlist, it automatically
/// selects the highest bandwidth/resolution variant and fetches its media segments.
pub async fn parse_hls_playlist(client: &Client, playlist_url: &Url) -> Result<Vec<HlsSegment>, HlsError> {
    let resp = client.get(playlist_url.clone()).send().await?;
    if !resp.status().is_success() {
        return Err(HlsError::InvalidPlaylist(format!("HTTP status {}", resp.status())));
    }

    let text = resp.text().await?;
    if !text.starts_with("#EXTM3U") {
        return Err(HlsError::InvalidPlaylist("Missing #EXTM3U header".to_string()));
    }

    // Check if this is a Master Playlist with multiple quality streams
    if text.contains("#EXT-X-STREAM-INF") {
        let best_variant_url = parse_best_variant_url(&text, playlist_url)?;
        tracing::info!("Selected highest quality HLS variant: {}", best_variant_url);
        // Recursively fetch media playlist
        return Box::pin(parse_hls_playlist(client, &best_variant_url)).await;
    }

    // It is a media playlist containing segments
    let mut segments = Vec::new();
    let mut current_duration = 2.0;
    let mut segment_index = 0;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with("#EXTINF:") {
            let info = &trimmed[8..];
            let dur_str = info.split(',').next().unwrap_or("2.0");
            current_duration = dur_str.parse::<f64>().unwrap_or(2.0);
        } else if !trimmed.starts_with('#') {
            // This is a segment URI
            let segment_url = playlist_url.join(trimmed).map_err(|e| {
                HlsError::InvalidPlaylist(format!("Invalid segment URL '{}': {}", trimmed, e))
            })?;

            segments.push(HlsSegment {
                index: segment_index,
                url: segment_url,
                duration_secs: current_duration,
            });
            segment_index += 1;
        }
    }

    if segments.is_empty() {
        return Err(HlsError::NoSegments);
    }

    Ok(segments)
}

fn parse_best_variant_url(master_text: &str, base_url: &Url) -> Result<Url, HlsError> {
    let mut best_bandwidth: u64 = 0;
    let mut best_uri = None;
    let mut next_line_is_uri = false;
    let mut current_bandwidth = 0;

    for line in master_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with("#EXT-X-STREAM-INF:") {
            next_line_is_uri = true;
            current_bandwidth = extract_bandwidth(trimmed);
        } else if next_line_is_uri {
            next_line_is_uri = false;
            if current_bandwidth >= best_bandwidth || best_uri.is_none() {
                best_bandwidth = current_bandwidth;
                best_uri = Some(trimmed.to_string());
            }
        }
    }

    let uri_str = best_uri.ok_or_else(|| HlsError::InvalidPlaylist("No variant stream URI found".to_string()))?;
    base_url.join(&uri_str).map_err(|e| HlsError::InvalidPlaylist(e.to_string()))
}

fn extract_bandwidth(line: &str) -> u64 {
    if let Some(idx) = line.find("BANDWIDTH=") {
        let sub = &line[idx + 10..];
        let num_str: String = sub.chars().take_while(|c| c.is_ascii_digit()).collect();
        return num_str.parse().unwrap_or(0);
    }
    0
}

/// High-speed parallel HLS segment downloader and in-order stream stitcher.
pub struct HlsEngine;

impl HlsEngine {
    pub async fn download(
        client: &Client,
        segments: Vec<HlsSegment>,
        output_path: &Path,
        num_connections: usize,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
    ) -> Result<PathBuf, HlsError> {
        let total_segments = segments.len();
        let target_file = if output_path.extension().is_none() {
            output_path.with_extension("mp4")
        } else {
            output_path.to_path_buf()
        };

        if let Some(parent) = target_file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut out_file = File::create(&target_file).await?;

        tracing::info!(
            "Starting HLS parallel ingestion: {} segments across {} streams -> {}",
            total_segments,
            num_connections,
            target_file.display()
        );

        let semaphore = Arc::new(Semaphore::new(num_connections.max(1).min(64)));
        let completed_buffer = Arc::new(Mutex::new(BTreeMap::<usize, Vec<u8>>::new()));
        let total_bytes_downloaded = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let (notify_tx, mut notify_rx) = tokio::sync::mpsc::channel::<usize>(total_segments);

        // Spawn worker download tasks
        for segment in segments {
            let sem = Arc::clone(&semaphore);
            let client = client.clone();
            let buf = Arc::clone(&completed_buffer);
            let bytes_counter = Arc::clone(&total_bytes_downloaded);
            let tx = notify_tx.clone();

            tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();

                // Retry loop for transient network glitches
                for _attempt in 0..3 {
                    if let Ok(resp) = client.get(segment.url.clone()).send().await {
                        if resp.status().is_success() {
                            if let Ok(bytes) = resp.bytes().await {
                                let len = bytes.len() as u64;
                                bytes_counter.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
                                buf.lock().insert(segment.index, bytes.to_vec());
                                let _ = tx.send(segment.index).await;
                                return;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }

                // If all retries fail, insert empty segment to maintain ordering
                buf.lock().insert(segment.index, Vec::new());
                let _ = tx.send(segment.index).await;
            });
        }

        // Drop the master sender so notify_rx will terminate if all workers fail
        drop(notify_tx);

        // In-order streaming file writer
        let mut next_index = 0;
        let start_time = Instant::now();
        let mut last_snapshot = Instant::now();

        while next_index < total_segments {
            // Check if next segment is ready in memory
            let segment_data = {
                let mut guard = completed_buffer.lock();
                guard.remove(&next_index)
            };

            if let Some(data) = segment_data {
                if !data.is_empty() {
                    out_file.write_all(&data).await?;
                }
                next_index += 1;

                // Broadcast progress
                let now = Instant::now();
                if now.duration_since(last_snapshot) >= Duration::from_millis(150) {
                    let total_bytes = total_bytes_downloaded.load(std::sync::atomic::Ordering::Relaxed);
                    let est_total_bytes = (total_bytes * total_segments as u64) / (next_index as u64).max(1);
                    let elapsed = now.duration_since(start_time).as_secs_f64();
                    let speed = if elapsed > 0.0 { total_bytes as f64 / elapsed } else { 0.0 };
                    let mut chunks = Vec::new();
                    let display_count = total_segments.min(64);
                    for i in 0..display_count {
                        let seg_start_idx = (i * total_segments) / display_count;
                        let seg_end_idx = ((i + 1) * total_segments) / display_count;
                        let status = if seg_end_idx <= next_index {
                            "Completed".to_string()
                        } else if seg_start_idx <= next_index + num_connections {
                            "Downloading".to_string()
                        } else {
                            "Pending".to_string()
                        };

                        let range_start = if total_segments > 0 {
                            (seg_start_idx as u64 * est_total_bytes) / total_segments as u64
                        } else {
                            0
                        };
                        let range_end = if total_segments > 0 {
                            (seg_end_idx as u64 * est_total_bytes) / total_segments as u64
                        } else {
                            1
                        };
                        let chunk_total = range_end.saturating_sub(range_start).max(1);
                        let downloaded = if seg_end_idx <= next_index {
                            chunk_total
                        } else {
                            0
                        };

                        chunks.push(crate::chunk::ChunkSnapshot {
                            id: i,
                            range_start,
                            range_end,
                            downloaded_bytes: downloaded,
                            total_bytes: chunk_total,
                            status,
                            worker_id: Some(i % num_connections),
                        });
                    }

                    let snapshot = EngineSnapshot {
                        total_bytes: est_total_bytes,
                        downloaded_bytes: total_bytes,
                        speed_bytes_per_sec: speed,
                        progress_ratio: next_index as f64 / total_segments as f64,
                        active_workers: num_connections,
                        mirror_speeds: Vec::new(),
                        chunks,
                    };

                    if let Some(ref tx) = snapshot_tx {
                        let _ = tx.send(snapshot);
                    }
                    last_snapshot = now;
                }
            } else {
                // Wait for notification from worker
                let _ = notify_rx.recv().await;
            }
        }

        out_file.flush().await?;
        out_file.sync_all().await?;

        tracing::info!("HLS download and stitching completed: {}", target_file.display());
        Ok(target_file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_best_variant() {
        let master = r#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
360p.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720
720p.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080
1080p.m3u8
"#;
        let base = Url::parse("https://cdn.example.com/hls/master.m3u8").unwrap();
        let best = parse_best_variant_url(master, &base).unwrap();
        assert_eq!(best, Url::parse("https://cdn.example.com/hls/1080p.m3u8").unwrap());
    }
}
