use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, RANGE};
use reqwest::{Client, StatusCode};
use tokio::sync::mpsc;
use url::Url;

use crate::chunk::Chunk;
use crate::storage::DiskWriter;

#[derive(Debug)]
pub enum WorkerEvent {
    Ttfb {
        worker_id: usize,
        mirror_id: usize,
        ttfb: Duration,
    },
    Progress {
        worker_id: usize,
        chunk_id: usize,
        mirror_id: usize,
        bytes_received: u64,
        duration: Duration,
    },
    ChunkCompleted {
        worker_id: usize,
        chunk_id: usize,
        hash: Option<[u8; 32]>,
    },
    ChunkFailed {
        worker_id: usize,
        chunk_id: usize,
        mirror_id: usize,
        error: String,
    },
}

pub struct HttpWorker {
    pub worker_id: usize,
    client: Client,
    writer: DiskWriter,
    event_tx: mpsc::Sender<WorkerEvent>,
}

impl HttpWorker {
    pub fn new(
        worker_id: usize,
        client: Client,
        writer: DiskWriter,
        event_tx: mpsc::Sender<WorkerEvent>,
    ) -> Self {
        Self {
            worker_id,
            client,
            writer,
            event_tx,
        }
    }

    /// Downloads a specific chunk from the given mirror URL.
    /// Cooperative cancellation: if `cancel_flag` is set, terminates early.
    pub async fn download_chunk(
        &self,
        chunk: Chunk,
        mirror_id: usize,
        mirror_url: Url,
        cancel_flag: Arc<AtomicBool>,
    ) {
        let chunk_id = chunk.id;
        let worker_id = self.worker_id;

        // Ensure atomic offsets are initialized to current chunk range
        chunk.current_offset.store(chunk.range.start, Ordering::SeqCst);
        chunk.end_offset.store(chunk.range.end, Ordering::SeqCst);

        let request_start = Instant::now();

        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_str(&chunk.range.to_http_header()).unwrap());
        headers.insert(reqwest::header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));

        let response = match self
            .client
            .get(mirror_url)
            .headers(headers)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                let _ = self.event_tx.send(WorkerEvent::ChunkFailed {
                    worker_id,
                    chunk_id,
                    mirror_id,
                    error: format!("Network request failed: {}", err),
                }).await;
                return;
            }
        };

        let ttfb = request_start.elapsed();
        let _ = self.event_tx.send(WorkerEvent::Ttfb {
            worker_id,
            mirror_id,
            ttfb,
        }).await;

        let status = response.status();
        if status != StatusCode::PARTIAL_CONTENT && status != StatusCode::OK {
            let _ = self.event_tx.send(WorkerEvent::ChunkFailed {
                worker_id,
                chunk_id,
                mirror_id,
                error: format!("Unexpected HTTP status: {}", status),
            }).await;
            return;
        }

        // If server returned 200 OK for a chunk starting at non-zero, server does not support ranges!
        if status == StatusCode::OK && chunk.range.start != 0 {
            let _ = self.event_tx.send(WorkerEvent::ChunkFailed {
                worker_id,
                chunk_id,
                mirror_id,
                error: format!("Server returned 200 OK for partial range starting at {}", chunk.range.start),
            }).await;
            return;
        }

        let mut stream = response.bytes_stream();
        let mut current_offset = chunk.range.start;
        let mut last_progress_time = Instant::now();
        let mut pending_bytes: u64 = 0;

        while let Some(item) = stream.next().await {
            if cancel_flag.load(Ordering::Relaxed) {
                // Cooperative cancellation (e.g. download paused or cancelled)
                return;
            }

            let current_end = chunk.end_offset.load(Ordering::SeqCst);
            if current_offset > current_end {
                break;
            }

            match item {
                Ok(bytes) => {
                    let len = bytes.len() as u64;
                    if len == 0 {
                        continue;
                    }

                    // Re-read current_end with SeqCst in case it was truncated during network await
                    let current_end = chunk.end_offset.load(Ordering::SeqCst);
                    if current_offset > current_end {
                        break;
                    }

                    // Check bounds against dynamic chunk range end (can be truncated by work stealing)
                    if current_offset + len - 1 > current_end {
                        let valid_len = (current_end + 1).saturating_sub(current_offset) as usize;
                        if valid_len > 0 {
                            if let Err(e) = self.writer.write_chunk_slice(current_offset, &bytes[..valid_len]) {
                                let _ = self.event_tx.send(WorkerEvent::ChunkFailed {
                                    worker_id,
                                    chunk_id,
                                    mirror_id,
                                    error: format!("Disk write error: {}", e),
                                }).await;
                                return;
                            }
                            current_offset += valid_len as u64;
                            chunk.current_offset.store(current_offset, Ordering::SeqCst);
                            pending_bytes += valid_len as u64;
                        }
                        break;
                    }

                    if let Err(e) = self.writer.write_chunk_slice(current_offset, &bytes) {
                        let _ = self.event_tx.send(WorkerEvent::ChunkFailed {
                            worker_id,
                            chunk_id,
                            mirror_id,
                            error: format!("Disk write error: {}", e),
                        }).await;
                        return;
                    }

                    current_offset += len;
                    chunk.current_offset.store(current_offset, Ordering::SeqCst);
                    pending_bytes += len;

                    // Send batched progress (every 1MB or 100ms) to maximize throughput
                    let now = Instant::now();
                    if pending_bytes >= 1024 * 1024 || now.duration_since(last_progress_time) >= Duration::from_millis(100) {
                        let elapsed = now.duration_since(last_progress_time);
                        last_progress_time = now;
                        let bytes_to_report = pending_bytes;
                        pending_bytes = 0;

                        let _ = self.event_tx.send(WorkerEvent::Progress {
                            worker_id,
                            chunk_id,
                            mirror_id,
                            bytes_received: bytes_to_report,
                            duration: elapsed,
                        }).await;
                    }
                }
                Err(err) => {
                    let _ = self.event_tx.send(WorkerEvent::ChunkFailed {
                        worker_id,
                        chunk_id,
                        mirror_id,
                        error: format!("Stream read error: {}", err),
                    }).await;
                    return;
                }
            }
        }

        // Flush any remaining batched progress
        if pending_bytes > 0 {
            let elapsed = last_progress_time.elapsed();
            let _ = self.event_tx.send(WorkerEvent::Progress {
                worker_id,
                chunk_id,
                mirror_id,
                bytes_received: pending_bytes,
                duration: elapsed,
            }).await;
        }

        // Verify that the chunk was completely downloaded
        let final_end = chunk.end_offset.load(Ordering::SeqCst);
        if current_offset <= final_end {
            // Premature termination: connection closed before chunk was fully received
            let _ = self.event_tx.send(WorkerEvent::ChunkFailed {
                worker_id,
                chunk_id,
                mirror_id,
                error: format!(
                    "Connection closed prematurely: received up to offset {}, expected up to {}",
                    current_offset, final_end
                ),
            }).await;
            return;
        }

        // Compute BLAKE3 chunk hash on completion of actual downloaded range
        let actual_range = crate::range::ByteRange::new(chunk.range.start, final_end).unwrap_or(chunk.range);
        let hash = self.writer.compute_chunk_hash(&actual_range).ok();

        let _ = self.event_tx.send(WorkerEvent::ChunkCompleted {
            worker_id,
            chunk_id,
            hash,
        }).await;
    }
}
