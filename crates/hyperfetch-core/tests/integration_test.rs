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
