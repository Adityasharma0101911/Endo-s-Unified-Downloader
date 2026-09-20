use std::net::SocketAddr;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use hyperfetch_core::engine::{DownloadEngine, DownloadOptions};

/// Lightweight mock HTTP server supporting HEAD and GET with Range requests
async fn run_mock_http_server(data: Arc<Vec<u8>>) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                res = listener.accept() => {
                    let (mut socket, _) = match res {
                        Ok(conn) => conn,
                        Err(_) => break,
                    };
                    let file_data = Arc::clone(&data);

                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 4096];
                        let n = match socket.read(&mut buf).await {
                            Ok(n) if n > 0 => n,
                            _ => return,
                        };

                        let request = String::from_utf8_lossy(&buf[..n]);
                        let lines: Vec<&str> = request.lines().collect();
                        if lines.is_empty() {
                            return;
                        }

                        let first_line = lines[0];
                        let is_head = first_line.starts_with("HEAD");
                        let is_get = first_line.starts_with("GET");
                        let total_len = file_data.len();

                        // Look for Range header
                        let mut range_header = None;
                        for line in &lines[1..] {
                            if line.to_ascii_lowercase().starts_with("range:") {
                                if let Some(val) = line.split(':').nth(1) {
                                    range_header = Some(val.trim());
                                }
                            }
                        }

                        if is_head {
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                                total_len
                            );
                            let _ = socket.write_all(resp.as_bytes()).await;
                        } else if is_get {
                            if let Some(r_str) = range_header {
                                if let Some(stripped) = r_str.strip_prefix("bytes=") {
                                    let bounds: Vec<&str> = stripped.split('-').collect();
                                    let start: usize = bounds[0].parse().unwrap_or(0);
                                    let end: usize = bounds.get(1).and_then(|s| s.parse().ok()).unwrap_or(total_len - 1);
                                    let end = end.min(total_len - 1);

                                    let slice = &file_data[start..=end];
                                    let resp = format!(
                                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                                        start, end, total_len, slice.len()
                                    );
                                    let _ = socket.write_all(resp.as_bytes()).await;
                                    let _ = socket.write_all(slice).await;
                                }
                            } else {
                                let resp = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                                    total_len
                                );
                                let _ = socket.write_all(resp.as_bytes()).await;
                                let _ = socket.write_all(&file_data).await;
                            }
                        }
                    });
                }
            }
        }
    });

    (addr, shutdown_tx)
}

#[tokio::test]
async fn test_multi_threaded_download_and_verification() {
    // Generate 1MB of pseudo-random repeatable test data
    let mut test_data = vec![0u8; 1024 * 1024];
    for (i, byte) in test_data.iter_mut().enumerate() {
        *byte = ((i * 37) % 256) as u8;
    }
    let expected_hash = blake3::hash(&test_data);
    let shared_data = Arc::new(test_data);

    let (addr, shutdown_tx) = run_mock_http_server(shared_data).await;
    let mirror_url = Url::parse(&format!("http://{}/test_payload.bin", addr)).unwrap();

    let temp = tempdir().unwrap();
    let out_file = temp.path().join("downloaded.bin");

    let options = DownloadOptions {
        num_connections: 4,
        base_chunk_size: 256 * 1024,      // 256KB chunks (4 chunks total)
        min_steal_threshold: 64 * 1024,   // 64KB
        output_path: Some(out_file.clone()),
    };

    let engine = DownloadEngine::new(vec![mirror_url], options);
    let downloaded_path = engine.run(None).await.expect("Download should succeed");

    assert_eq!(downloaded_path, out_file);
    assert!(out_file.exists());

    // Verify downloaded file size and BLAKE3 hash
    let downloaded_bytes = std::fs::read(&out_file).unwrap();
    assert_eq!(downloaded_bytes.len(), 1024 * 1024);
    let actual_hash = blake3::hash(&downloaded_bytes);
    assert_eq!(actual_hash, expected_hash);

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn test_zero_byte_download() {
    let empty_data = Arc::new(Vec::<u8>::new());
    let (addr, shutdown_tx) = run_mock_http_server(empty_data).await;
    let mirror_url = Url::parse(&format!("http://{}/empty.bin", addr)).unwrap();

    let temp = tempdir().unwrap();
    let out_file = temp.path().join("empty_downloaded.bin");

    let options = DownloadOptions {
        num_connections: 4,
        base_chunk_size: 64 * 1024,
        min_steal_threshold: 16 * 1024,
        output_path: Some(out_file.clone()),
    };

    let engine = DownloadEngine::new(vec![mirror_url], options);
    let downloaded_path = engine.run(None).await.expect("0-byte download should succeed");

    assert_eq!(downloaded_path, out_file);
    assert!(out_file.exists());
    let downloaded_bytes = std::fs::read(&out_file).unwrap();
    assert_eq!(downloaded_bytes.len(), 0);

    let _ = shutdown_tx.send(());
}

async fn run_mock_non_range_server(data: Arc<Vec<u8>>) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                res = listener.accept() => {
                    let (mut socket, _) = match res {
                        Ok(conn) => conn,
                        Err(_) => break,
                    };
                    let file_data = Arc::clone(&data);

                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 4096];
                        let n = match socket.read(&mut buf).await {
                            Ok(n) if n > 0 => n,
                            _ => return,
                        };

                        let request = String::from_utf8_lossy(&buf[..n]);
                        let lines: Vec<&str> = request.lines().collect();
                        if lines.is_empty() {
                            return;
                        }

                        let first_line = lines[0];
                        let is_head = first_line.starts_with("HEAD");
                        let total_len = file_data.len();

                        if is_head {
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                total_len
                            );
                            let _ = socket.write_all(resp.as_bytes()).await;
                        } else {
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                total_len
                            );
                            let _ = socket.write_all(resp.as_bytes()).await;
                            let _ = socket.write_all(&file_data).await;
                        }
                    });
                }
            }
        }
    });

    (addr, shutdown_tx)
}

#[tokio::test]
async fn test_non_range_server_download() {
    let mut test_data = vec![0u8; 512 * 1024]; // 512KB
    for (i, byte) in test_data.iter_mut().enumerate() {
        *byte = ((i * 19) % 256) as u8;
    }
    let expected_hash = blake3::hash(&test_data);
    let shared_data = Arc::new(test_data);

    let (addr, shutdown_tx) = run_mock_non_range_server(shared_data).await;
    let mirror_url = Url::parse(&format!("http://{}/non_range.bin", addr)).unwrap();

    let temp = tempdir().unwrap();
    let out_file = temp.path().join("non_range_downloaded.bin");

    let options = DownloadOptions {
        num_connections: 8,
        base_chunk_size: 64 * 1024,
        min_steal_threshold: 16 * 1024,
        output_path: Some(out_file.clone()),
    };

    let engine = DownloadEngine::new(vec![mirror_url], options);
    let downloaded_path = engine.run(None).await.expect("Non-range download should succeed");

    assert_eq!(downloaded_path, out_file);
    assert!(out_file.exists());
    let downloaded_bytes = std::fs::read(&out_file).unwrap();
    assert_eq!(downloaded_bytes.len(), 512 * 1024);
    assert_eq!(blake3::hash(&downloaded_bytes), expected_hash);

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn test_resume_download_with_hfstate() {
    let mut test_data = vec![0u8; 512 * 1024]; // 512KB
    for (i, byte) in test_data.iter_mut().enumerate() {
        *byte = ((i * 41) % 256) as u8;
    }
    let expected_hash = blake3::hash(&test_data);
    let shared_data = Arc::new(test_data.clone());

    let (addr, shutdown_tx) = run_mock_http_server(shared_data).await;
    let mirror_url = Url::parse(&format!("http://{}/resume_test.bin", addr)).unwrap();

    let temp = tempdir().unwrap();
    let out_file = temp.path().join("resume_test.bin");

    // Pre-populate the first 256KB on disk
    std::fs::write(&out_file, &test_data[..256 * 1024]).unwrap();

    // Create a .hfstate recording that [0, 256KB - 1] was completed
    let state_path = hyperfetch_core::state::DownloadState::state_file_path(&out_file);
    let mut state = hyperfetch_core::state::DownloadState::new(
        "resume_test.bin".to_string(),
        512 * 1024,
        64 * 1024,
        vec![mirror_url.to_string()],
    );
    state.completed_ranges.push(hyperfetch_core::range::ByteRange::new(0, 256 * 1024 - 1).unwrap());
    state.save_atomic(&state_path).unwrap();

    let options = DownloadOptions {
        num_connections: 4,
        base_chunk_size: 64 * 1024,
        min_steal_threshold: 16 * 1024,
        output_path: Some(out_file.clone()),
    };

    let engine = DownloadEngine::new(vec![mirror_url], options);
    let downloaded_path = engine.run(None).await.expect("Resumed download should succeed");

    assert_eq!(downloaded_path, out_file);
    assert!(out_file.exists());
    let downloaded_bytes = std::fs::read(&out_file).unwrap();
    assert_eq!(downloaded_bytes.len(), 512 * 1024);
    assert_eq!(blake3::hash(&downloaded_bytes), expected_hash);

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn test_dynamic_work_stealing_integration() {
    let mut test_data = vec![0u8; 1024 * 1024]; // 1MB
    for (i, byte) in test_data.iter_mut().enumerate() {
        *byte = ((i * 53) % 256) as u8;
    }
    let expected_hash = blake3::hash(&test_data);
    let shared_data = Arc::new(test_data);

    let (addr, shutdown_tx) = run_mock_http_server(shared_data).await;
    let mirror_url = Url::parse(&format!("http://{}/stealing_test.bin", addr)).unwrap();

    let temp = tempdir().unwrap();
    let out_file = temp.path().join("stealing_test.bin");

    // Only 1 initial chunk of 1MB, with 4 workers and low steal threshold of 32KB
    // Workers 1..3 MUST steal work from Worker 0!
    let options = DownloadOptions {
        num_connections: 4,
        base_chunk_size: 1024 * 1024,   // 1 single initial chunk
        min_steal_threshold: 32 * 1024, // 32KB threshold enables stealing
        output_path: Some(out_file.clone()),
    };

    let engine = DownloadEngine::new(vec![mirror_url], options);
    let downloaded_path = engine.run(None).await.expect("Work stealing download should succeed");

    assert_eq!(downloaded_path, out_file);
    assert!(out_file.exists());
    let downloaded_bytes = std::fs::read(&out_file).unwrap();
    assert_eq!(downloaded_bytes.len(), 1024 * 1024);
    assert_eq!(blake3::hash(&downloaded_bytes), expected_hash);

    let _ = shutdown_tx.send(());
}
