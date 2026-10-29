//! Local loopback HTTP virtual input server for FFmpeg (`--ffmpeg-concat-mode LOCAL_HTTP`).
//! Provides a seekable byte stream of concatenated segment files over HTTP to avoid command-line
//! length limits and "too many open files" errors without creating intermediate disk files.

use std::fs;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Virtual concatenation server over loopback HTTP.
pub struct ConcatInputServer {
    url: String,
    total_length: u64,
    cancel_token: CancellationToken,
}

impl ConcatInputServer {
    /// Start the virtual concat server on loopback interface with an ephemeral port.
    pub async fn start(files: &[PathBuf]) -> Result<Self, String> {
        if files.is_empty() {
            return Err("cannot start concat server with zero files".to_string());
        }

        // Compute cumulative file boundaries
        let mut ends = Vec::with_capacity(files.len());
        let mut cum_len = 0u64;
        let mut full_paths = Vec::with_capacity(files.len());

        for f in files {
            let metadata = fs::metadata(f).map_err(|e| format!("cannot stat segment '{}': {e}", f.display()))?;
            cum_len = cum_len.checked_add(metadata.len()).ok_or("total length overflow")?;
            ends.push(cum_len);
            full_paths.push(f.clone());
        }

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("failed to bind local TCP listener: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;

        let cancel_token = CancellationToken::new();
        let cancel_clone = cancel_token.clone();

        let state = Arc::new(ServerState {
            files: full_paths,
            ends,
            total_length: cum_len,
        });

        let url = format!("http://127.0.0.1:{}/stream", addr.port());

        // Spawn background listener task
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _)) => {
                                let st = state.clone();
                                let token = cancel_clone.clone();
                                tokio::spawn(async move {
                                    handle_client(stream, st, token).await;
                                });
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        Ok(Self {
            url,
            total_length: cum_len,
            cancel_token,
        })
    }

    /// The local loopback URL to pass to FFmpeg as input (e.g. `http://127.0.0.1:54321/stream`).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Total combined length of all files.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Stop the server.
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }
}

impl Drop for ConcatInputServer {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

struct ServerState {
    files: Vec<PathBuf>,
    ends: Vec<u64>,
    total_length: u64,
}

async fn handle_client(mut stream: TcpStream, state: Arc<ServerState>, token: CancellationToken) {
    let mut header_buf = [0u8; 4096];
    let n = match stream.read(&mut header_buf).await {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let req_text = String::from_utf8_lossy(&header_buf[..n]);
    let first_line = req_text.lines().next().unwrap_or("");
    let is_head = first_line.starts_with("HEAD");

    // Parse Range: bytes=start-end
    let mut range_start = 0u64;
    let mut range_end = state.total_length.saturating_sub(1);
    let mut is_range = false;

    for line in req_text.lines() {
        if line.to_ascii_lowercase().starts_with("range:") {
            if let Some((_, r_val)) = line.split_once('=') {
                is_range = true;
                if let Some((s_str, e_str)) = r_val.split_once('-') {
                    if let Ok(s) = s_str.trim().parse::<u64>() {
                        range_start = s;
                    }
                    if let Ok(e) = e_str.trim().parse::<u64>() {
                        if e < state.total_length {
                            range_end = e;
                        }
                    }
                }
            }
        }
    }

    if range_start > range_end || range_start >= state.total_length {
        let resp = format!(
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nConnection: close\r\n\r\n",
            state.total_length
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        return;
    }

    let content_len = (range_end - range_start) + 1;
    let status_code = if is_range { "206 Partial Content" } else { "200 OK" };
    let headers = format!(
        "HTTP/1.1 {}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nContent-Type: video/MP2T\r\nConnection: close\r\n\r\n",
        status_code, content_len, range_start, range_end, state.total_length
    );

    if stream.write_all(headers.as_bytes()).await.is_err() || is_head {
        return;
    }

    // Stream bytes across files
    let mut current_offset = range_start;
    let mut bytes_remaining = content_len;

    // Binary search for starting file index
    let mut file_idx = match state.ends.binary_search(&current_offset) {
        Ok(idx) => idx + 1,
        Err(idx) => idx,
    };

    let mut buf = vec![0u8; 64 * 1024];

    while bytes_remaining > 0 && file_idx < state.files.len() {
        if token.is_cancelled() {
            break;
        }

        let file_path = &state.files[file_idx];
        let file_end = state.ends[file_idx];
        let file_start = if file_idx == 0 { 0 } else { state.ends[file_idx - 1] };
        let file_len = file_end - file_start;

        let offset_in_file = current_offset - file_start;
        let bytes_from_this_file = (file_len - offset_in_file).min(bytes_remaining);

        if let Ok(mut f) = tokio::fs::File::open(file_path).await {
            if f.seek(SeekFrom::Start(offset_in_file)).await.is_ok() {
                let mut read_for_file = 0u64;
                while read_for_file < bytes_from_this_file {
                    if token.is_cancelled() {
                        return;
                    }
                    let to_read = (bytes_from_this_file - read_for_file).min(buf.len() as u64) as usize;
                    match f.read(&mut buf[..to_read]).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                            read_for_file += n as u64;
                            current_offset += n as u64;
                            bytes_remaining -= n as u64;
                        }
                        Err(_) => return,
                    }
                }
            }
        } else {
            return;
        }

        file_idx += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use std::io::Write;

    #[tokio::test]
    async fn test_concat_server_basic() {
        let mut f1 = NamedTempFile::new().unwrap();
        f1.write_all(b"Hello ").unwrap();
        f1.flush().unwrap();

        let mut f2 = NamedTempFile::new().unwrap();
        f2.write_all(b"World!").unwrap();
        f2.flush().unwrap();

        let files = vec![f1.path().to_path_buf(), f2.path().to_path_buf()];
        let server = ConcatInputServer::start(&files).await.unwrap();

        assert_eq!(server.total_length(), 12);

        // Fetch via reqwest
        let client = reqwest::Client::new();
        let resp = client.get(server.url()).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(&bytes[..], b"Hello World!");

        // Test Range request
        let resp_range = client.get(server.url()).header("Range", "bytes=6-11").send().await.unwrap();
        assert_eq!(resp_range.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        let range_bytes = resp_range.bytes().await.unwrap();
        assert_eq!(&range_bytes[..], b"World!");

        server.stop();
    }
}
