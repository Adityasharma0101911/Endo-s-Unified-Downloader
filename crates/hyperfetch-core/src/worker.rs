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
        let range = chunk.range;
        let worker_id = self.worker_id;

        let request_start = Instant::now();

        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_str(&range.to_http_header()).unwrap());

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

        let mut stream = response.bytes_stream();
        let mut current_offset = range.start;
        let mut last_progress_time = Instant::now();

        while let Some(item) = stream.next().await {
            if cancel_flag.load(Ordering::Relaxed) {
                // Cooperative cancellation (e.g. download paused or range stolen)
                return;
            }

            match item {
                Ok(bytes) => {
                    let len = bytes.len() as u64;
                    if len == 0 {
                        continue;
                    }

                    // Check bounds against chunk range
                    if current_offset + len - 1 > range.end {
                        let valid_len = (range.end + 1).saturating_sub(current_offset) as usize;
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
                    let now = Instant::now();
                    let elapsed = now.duration_since(last_progress_time);
                    last_progress_time = now;

                    let _ = self.event_tx.send(WorkerEvent::Progress {
                        worker_id,
                        chunk_id,
                        mirror_id,
                        bytes_received: len,
                        duration: elapsed,
                    }).await;
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

        // Compute BLAKE3 chunk hash on completion
        let hash = self.writer.compute_chunk_hash(&range).ok();

        let _ = self.event_tx.send(WorkerEvent::ChunkCompleted {
            worker_id,
            chunk_id,
            hash,
        }).await;
    }
}
