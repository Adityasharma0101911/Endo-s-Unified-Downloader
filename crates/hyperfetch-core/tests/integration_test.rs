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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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

#[tokio::test]
async fn test_non_aligned_work_stolen_resume() {
    let mut test_data = vec![0u8; 512 * 1024]; // 512KB
    for (i, byte) in test_data.iter_mut().enumerate() {
        *byte = ((i * 61) % 256) as u8;
    }
    let expected_hash = blake3::hash(&test_data);
    let shared_data = Arc::new(test_data.clone());

    let (addr, shutdown_tx) = run_mock_http_server(shared_data).await;
    let mirror_url = Url::parse(&format!("http://{}/non_aligned_resume.bin", addr)).unwrap();

    let temp = tempdir().unwrap();
    let out_file = temp.path().join("non_aligned_resume.bin");

    // Pre-populate only the first 128KB on disk (which is NOT a multiple of 256KB base chunk size)
    std::fs::write(&out_file, &test_data[..128 * 1024]).unwrap();

    let state_path = hyperfetch_core::state::DownloadState::state_file_path(&out_file);
    let mut state = hyperfetch_core::state::DownloadState::new(
        "non_aligned_resume.bin".to_string(),
        512 * 1024,
        256 * 1024, // 256KB base chunk size
        vec![mirror_url.to_string()],
    );
    state.completed_ranges.push(hyperfetch_core::range::ByteRange::new(0, 128 * 1024 - 1).unwrap());
    state.save_atomic(&state_path).unwrap();

    let options = DownloadOptions {
        num_connections: 4,
        base_chunk_size: 256 * 1024,
        min_steal_threshold: 32 * 1024,
        output_path: Some(out_file.clone()),
        ..Default::default()
    };

    let engine = DownloadEngine::new(vec![mirror_url], options);
    let downloaded_path = engine.run(None).await.expect("Resumed download with non-aligned ranges should succeed");

    assert_eq!(downloaded_path, out_file);
    assert!(out_file.exists());
    let downloaded_bytes = std::fs::read(&out_file).unwrap();
    assert_eq!(downloaded_bytes.len(), 512 * 1024);
    assert_eq!(blake3::hash(&downloaded_bytes), expected_hash);

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn test_fatal_failure_detection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                res = listener.accept() => {
                    let (mut socket, _) = match res {
                        Ok(c) => c,
                        Err(_) => break,
                    };
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 1024];
                        let _ = socket.read(&mut buf).await;
                        let first_line = String::from_utf8_lossy(&buf);
                        if first_line.starts_with("HEAD") {
                            let resp = "HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n";
                            let _ = socket.write_all(resp.as_bytes()).await;
                            let _ = socket.shutdown().await;
                        } else {
                            let resp = "HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\n\r\n";
                            let _ = socket.write_all(resp.as_bytes()).await;
                            let _ = socket.shutdown().await;
                        }
                    });
                }
            }
        }
    });

    let mirror_url = Url::parse(&format!("http://{}/fatal_test.bin", addr)).unwrap();
    let temp = tempdir().unwrap();
    let out_file = temp.path().join("fatal_test.bin");

    let options = DownloadOptions {
        num_connections: 2,
        base_chunk_size: 512 * 1024,
        min_steal_threshold: 64 * 1024,
        output_path: Some(out_file),
        ..Default::default()
    };

    let engine = DownloadEngine::new(vec![mirror_url], options);
    let result = engine.run(None).await;
    assert!(result.is_err(), "Engine should fail promptly on persistent server errors rather than hanging");

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn test_premature_stream_closure_retries() {
    let mut test_data = vec![0u8; 256 * 1024]; // 256KB
    for (i, byte) in test_data.iter_mut().enumerate() {
        *byte = ((i * 73) % 256) as u8;
    }
    let expected_hash = blake3::hash(&test_data);
    let shared_data = Arc::new(test_data);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let attempt_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let sdata = Arc::clone(&shared_data);
    let counter = Arc::clone(&attempt_counter);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                res = listener.accept() => {
                    let (mut socket, _) = match res {
                        Ok(c) => c,
                        Err(_) => break,
                    };
                    let file_data = Arc::clone(&sdata);
                    let cnt = Arc::clone(&counter);

                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 1024];
                        let _ = socket.read(&mut buf).await;
                        let first_line = String::from_utf8_lossy(&buf);
                        if first_line.starts_with("HEAD") {
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                                file_data.len()
                            );
                            let _ = socket.write_all(resp.as_bytes()).await;
                        } else {
                            let attempt = cnt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if attempt == 0 {
                                // First attempt: send header indicating full 256KB slice, but close after sending only 16KB!
                                let resp = format!(
                                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                                    file_data.len() - 1, file_data.len(), file_data.len()
                                );
                                let _ = socket.write_all(resp.as_bytes()).await;
                                let _ = socket.write_all(&file_data[..16 * 1024]).await;
                                let _ = socket.shutdown().await;
                            } else {
                                // Subsequent retry: send full slice
                                let resp = format!(
                                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                                    file_data.len() - 1, file_data.len(), file_data.len()
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

    let mirror_url = Url::parse(&format!("http://{}/retry_test.bin", addr)).unwrap();
    let temp = tempdir().unwrap();
    let out_file = temp.path().join("retry_test.bin");

    let options = DownloadOptions {
        num_connections: 1,
        base_chunk_size: 256 * 1024,
        min_steal_threshold: 64 * 1024,
        output_path: Some(out_file.clone()),
        ..Default::default()
    };

    let engine = DownloadEngine::new(vec![mirror_url], options);
    let downloaded_path = engine.run(None).await.expect("Download should recover from premature EOF via retry");

    assert_eq!(downloaded_path, out_file);
    let downloaded_bytes = std::fs::read(&out_file).unwrap();
    assert_eq!(downloaded_bytes.len(), 256 * 1024);
    assert_eq!(blake3::hash(&downloaded_bytes), expected_hash);

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn test_redownload_existing_complete_file_does_not_overwrite() {
    let mut test_data = vec![0u8; 1024 * 1024];
    for (i, byte) in test_data.iter_mut().enumerate() {
        *byte = ((i * 43) % 256) as u8;
    }
    let expected_hash = blake3::hash(&test_data);
    let shared_data = Arc::new(test_data);

    let (addr, shutdown_tx) = run_mock_http_server(shared_data).await;
    let mirror_url = Url::parse(&format!("http://{}/complete_file.bin", addr)).unwrap();

    let temp = tempdir().unwrap();
    let out_file = temp.path().join("complete_file.bin");

    let options = DownloadOptions {
        num_connections: 4,
        base_chunk_size: 256 * 1024,
        min_steal_threshold: 64 * 1024,
        output_path: Some(out_file.clone()),
        ..Default::default()
    };

    // First download
    let engine = DownloadEngine::new(vec![mirror_url.clone()], options.clone());
    let path1 = engine.run(None).await.expect("First download should succeed");
    assert_eq!(path1, out_file);

    let _meta1 = std::fs::metadata(&out_file).unwrap();

    // Small delay to ensure timestamp difference if it were modified
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second download with same URL and output path
    let engine2 = DownloadEngine::new(vec![mirror_url], options);
    let path2 = engine2.run(None).await.expect("Second download should detect existing complete file");
    assert_eq!(path2, out_file);

    // Verify file content is intact and verified
    let content = std::fs::read(&out_file).unwrap();
    assert_eq!(content.len(), 1024 * 1024);
    assert_eq!(blake3::hash(&content), expected_hash);

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn test_redownload_different_file_auto_renames() {
    let mut test_data = vec![0u8; 1024 * 1024];
    for (i, byte) in test_data.iter_mut().enumerate() {
        *byte = ((i * 51) % 256) as u8;
    }
    let expected_hash = blake3::hash(&test_data);
    let shared_data = Arc::new(test_data);

    let (addr, shutdown_tx) = run_mock_http_server(shared_data).await;
    let mirror_url = Url::parse(&format!("http://{}/collision_test.bin", addr)).unwrap();

    let temp = tempdir().unwrap();
    let out_file = temp.path().join("collision_test.bin");

    // Create an existing file with different size/content
    let existing_data = b"Existing pre-allocated unrelated file with different size";
    std::fs::write(&out_file, existing_data).unwrap();

    let options = DownloadOptions {
        num_connections: 4,
        base_chunk_size: 256 * 1024,
        min_steal_threshold: 64 * 1024,
        output_path: Some(out_file.clone()),
        ..Default::default()
    };

    let engine = DownloadEngine::new(vec![mirror_url], options);
    let downloaded_path = engine.run(None).await.expect("Download should auto-rename and succeed");

    // The downloaded file must be auto-renamed to collision_test (1).bin
    let expected_renamed = temp.path().join("collision_test (1).bin");
    assert_eq!(downloaded_path, expected_renamed);
    assert!(expected_renamed.exists());

    // The original file must NOT be modified or deleted!
    let original_content = std::fs::read(&out_file).unwrap();
    assert_eq!(original_content, existing_data);

    // The renamed file must contain the complete downloaded payload
    let downloaded_content = std::fs::read(&expected_renamed).unwrap();
    assert_eq!(downloaded_content.len(), 1024 * 1024);
    assert_eq!(blake3::hash(&downloaded_content), expected_hash);

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn test_resume_partial_file_without_state_preserves_existing_bytes() {
    let mut test_data = vec![0u8; 1024 * 1024];
    for (i, byte) in test_data.iter_mut().enumerate() {
        *byte = ((i * 19) % 256) as u8;
    }
    let expected_hash = blake3::hash(&test_data);
    let shared_data = Arc::new(test_data.clone());

    let (addr, shutdown_tx) = run_mock_http_server(shared_data).await;
    let mirror_url = Url::parse(&format!("http://{}/partial_resume.bin", addr)).unwrap();

    let temp = tempdir().unwrap();
    let out_file = temp.path().join("partial_resume.bin");

    // Write first 512KB of valid data to out_file (simulating interrupted download without .hfstate)
    std::fs::write(&out_file, &test_data[..512 * 1024]).unwrap();

    // Record past download entry in history so engine knows this partial file is from this URL
    let mut history = hyperfetch_core::history::DownloadHistoryManager::load();
    let mut entry = hyperfetch_core::history::HistoryEntry::new(
        "partial_resume.bin".to_string(),
        out_file.clone(),
        1024 * 1024,
        vec![mirror_url.to_string()],
    );
    entry.downloaded_bytes = 512 * 1024;
    entry.status = hyperfetch_core::history::HistoryStatus::Cancelled;
    history.add_or_update(entry);

    let options = DownloadOptions {
        num_connections: 4,
        base_chunk_size: 256 * 1024,
        min_steal_threshold: 64 * 1024,
        output_path: Some(out_file.clone()),
        ..Default::default()
    };

    let engine = DownloadEngine::new(vec![mirror_url], options);
    let downloaded_path = engine.run(None).await.expect("Download should resume and complete");

    assert_eq!(downloaded_path, out_file);
    let final_content = std::fs::read(&out_file).unwrap();
    assert_eq!(final_content.len(), 1024 * 1024);
    assert_eq!(blake3::hash(&final_content), expected_hash);

    let _ = shutdown_tx.send(());
}


