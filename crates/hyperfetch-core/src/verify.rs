use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use reqwest::Client;
use url::Url;
use crate::range::ByteRange;
use crate::state::DownloadState;
use crate::storage::DiskWriter;

#[derive(Debug, Clone)]
pub struct BuildVerificationResult {
    pub file_path: PathBuf,
    pub expected_size: Option<u64>,
    pub actual_size: u64,
    pub has_state_file: bool,
    pub missing_ranges: Vec<ByteRange>,
    pub is_complete: bool,
    pub checksum_match: Option<bool>,
    pub status_message: String,
}

/// Verifies whether a downloaded build file has all chunks completely written.
pub fn verify_build_file(
    file_path: &Path,
    expected_size: Option<u64>,
    expected_checksum: Option<&str>,
) -> Result<BuildVerificationResult, String> {
    if !file_path.exists() {
        return Err(format!("File does not exist: {:?}", file_path));
    }

    let metadata = std::fs::metadata(file_path).map_err(|e| e.to_string())?;
    let actual_size = metadata.len();

    let state_path = DownloadState::state_file_path(file_path);
    let state_opt = DownloadState::load_from_path(&state_path).ok().flatten();

    let mut missing_ranges = Vec::new();
    let has_state_file = state_opt.is_some();
    let exp_size = expected_size.or_else(|| state_opt.as_ref().map(|s| s.file_size));

    if let Some(ref state) = state_opt {
        let gaps = crate::range::compute_gaps(state.file_size, &state.completed_ranges);
        missing_ranges.extend(gaps);
    } else if let Some(expected) = exp_size {
        if actual_size < expected {
            if let Ok(missing) = ByteRange::new(actual_size, expected - 1) {
                missing_ranges.push(missing);
            }
        }
    }

    let is_complete = missing_ranges.is_empty() && (exp_size.is_none() || exp_size == Some(actual_size));

    // Optional Checksum Verification
    let checksum_match = if is_complete {
        if let Some(expected_hash) = expected_checksum {
            match DiskWriter::verify_file_checksum(file_path, expected_hash) {
                Ok(true) => Some(true),
                Ok(false) => Some(false),
                Err(_) => None,
            }
        } else {
            None
        }
    } else {
        None
    };

    let status_message = if is_complete {
        if let Some(true) = checksum_match {
            format!("Build 100% complete and checksum verified ({})", actual_size)
        } else if let Some(false) = checksum_match {
            "Build complete but checksum mismatch detected".to_string()
        } else {
            format!("Build 100% complete ({} bytes, all chunks verified)", actual_size)
        }
    } else {
        let missing_bytes: u64 = missing_ranges.iter().map(|r| r.len()).sum();
        format!(
            "Build incomplete: {} missing range(s) totaling {} bytes",
            missing_ranges.len(),
            missing_bytes
        )
    };

    Ok(BuildVerificationResult {
        file_path: file_path.to_path_buf(),
        expected_size: exp_size,
        actual_size,
        has_state_file,
        missing_ranges,
        is_complete,
        checksum_match,
        status_message,
    })
}

/// Selectively downloads and repairs missing chunk ranges directly into the file on disk.
pub async fn repair_missing_ranges<F>(
    file_path: &Path,
    total_size: u64,
    missing_ranges: &[ByteRange],
    urls: &[Url],
    cancel_flag: Option<Arc<AtomicBool>>,
    progress_callback: F,
) -> Result<(), String>
where
    F: Fn(u64, u64) + Send + Sync + 'static,
{
    if missing_ranges.is_empty() {
        return Ok(());
    }

    if urls.is_empty() {
        return Err("No mirror URLs provided for chunk repair".to_string());
    }

    let client = Client::builder()
        .tcp_nodelay(true)
        .default_headers(crate::resolver::SmartResolver::default_anti_qos_headers())
        .build()
        .unwrap_or_default();

    let disk_writer = DiskWriter::open_or_create(file_path, total_size)
        .map_err(|e| format!("Failed to open file for repair: {}", e))?;

    let total_repair_bytes: u64 = missing_ranges.iter().map(|r| r.len()).sum();
    let mut repaired_bytes: u64 = 0;

    let progress_cb = Arc::new(progress_callback);

    for (range_idx, range) in missing_ranges.iter().enumerate() {
        if let Some(ref flag) = cancel_flag {
            if flag.load(Ordering::Relaxed) {
                return Err("Repair cancelled by user".to_string());
            }
        }

        let mirror_url = &urls[range_idx % urls.len()];
        let range_header = format!("bytes={}-{}", range.start, range.end);

        let resp = client
            .get(mirror_url.clone())
            .header(reqwest::header::RANGE, range_header)
            .send()
            .await
            .map_err(|e| format!("Repair request failed for range {}: {}", range, e))?;

        if !resp.status().is_success() {
            return Err(format!(
                "Server rejected range request {} with HTTP {}",
                range,
                resp.status()
            ));
        }

        let mut stream = resp.bytes_stream();
        use futures_util::StreamExt;

        let mut write_offset = range.start;
        while let Some(chunk_res) = stream.next().await {
            if let Some(ref flag) = cancel_flag {
                if flag.load(Ordering::Relaxed) {
                    return Err("Repair cancelled by user".to_string());
                }
            }

            let data = chunk_res.map_err(|e| format!("Error streaming repair chunk: {}", e))?;
            disk_writer
                .write_chunk_slice(write_offset, &data)
                .map_err(|e| format!("Failed writing repair slice: {}", e))?;

            write_offset += data.len() as u64;
            repaired_bytes += data.len() as u64;
            progress_cb(repaired_bytes, total_repair_bytes);
        }
    }

    disk_writer.sync().map_err(|e| format!("Failed to flush repaired data to disk: {}", e))?;

    // Update .hfstate if present
    let state_path = DownloadState::state_file_path(file_path);
    if let Ok(Some(mut state)) = DownloadState::load_from_path(&state_path) {
        state.completed_ranges.extend_from_slice(missing_ranges);
        state.completed_ranges = crate::range::merge_ranges(state.completed_ranges);

        let remaining = crate::range::compute_gaps(state.file_size, &state.completed_ranges);
        if remaining.is_empty() {
            let _ = DownloadState::remove(&state_path);
        } else {
            let _ = state.save_atomic(&state_path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_verify_complete_file() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();
        std::fs::write(&path, b"Hello World 12345").unwrap();

        let res = verify_build_file(&path, Some(17), None).unwrap();
        assert!(res.is_complete);
        assert_eq!(res.actual_size, 17);
        assert_eq!(res.missing_ranges.len(), 0);
    }

    #[test]
    fn test_verify_missing_gap_from_state() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();
        std::fs::write(&path, vec![0u8; 1000]).unwrap();

        let state_path = DownloadState::state_file_path(&path);
        let mut state = DownloadState::new(
            "test.bin".to_string(),
            1000,
            250,
            vec!["https://example.com/test.bin".to_string()],
        );
        // Only range 0-499 completed, 500-999 missing
        state.completed_ranges.push(ByteRange::new(0, 499).unwrap());
        state.save_atomic(&state_path).unwrap();

        let res = verify_build_file(&path, None, None).unwrap();
        assert!(!res.is_complete);
        assert_eq!(res.missing_ranges.len(), 1);
        assert_eq!(res.missing_ranges[0].start, 500);
        assert_eq!(res.missing_ranges[0].end, 999);

        let _ = DownloadState::remove(&state_path);
    }
}
