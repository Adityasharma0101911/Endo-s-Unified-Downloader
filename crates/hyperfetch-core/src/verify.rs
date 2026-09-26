use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use reqwest::{Client, StatusCode};
use url::Url;
use crate::engine::{claim_target, TargetClaim};
use crate::history::{DownloadHistoryManager, HistoryEntry};
use crate::range::{compute_gaps, merge_ranges, ByteRange};
use crate::state::DownloadState;
use crate::storage::DiskWriter;
use crate::worker::carries_validator;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A mirror that sends nothing for this long is treated as stalled.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a pending network operation checks the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(200);
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Failed attempts per mirror (without any progress) before a range is given up.
const ATTEMPTS_PER_MIRROR: usize = 2;

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

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The file that holds the data for `path`: an in-progress download lives at `<path>.part`.
fn resolve_target(path: &Path) -> PathBuf {
    if !path.exists() {
        let part = with_suffix(path, ".part");
        if part.exists() {
            return part;
        }
    }
    path.to_path_buf()
}

/// The final name of an in-progress `.part` file, or None for any other file.
fn final_name_of(path: &Path) -> Option<PathBuf> {
    (path.extension()? == "part").then(|| path.with_extension(""))
}

/// The most recent history entry recorded for exactly this file path.
fn history_entry_for<'a>(history: &'a DownloadHistoryManager, path: &Path) -> Option<&'a HistoryEntry> {
    let wanted = std::path::absolute(path).ok()?;
    history
        .entries()
        .iter()
        .find(|e| std::path::absolute(&e.file_path).is_ok_and(|p| p == wanted))
}

/// Verifies whether a downloaded build file has all chunks completely written.
///
/// Evidence used, strongest first: a checksum (the caller's, or the BLAKE3 hash history recorded
/// for this exact path), the `.hfstate` range bookkeeping, and the expected size. A file with no
/// evidence at all is reported as unverifiable, never as complete.
pub fn verify_build_file(
    file_path: &Path,
    expected_size: Option<u64>,
    expected_checksum: Option<&str>,
) -> Result<BuildVerificationResult, String> {
    verify_with_history(file_path, expected_size, expected_checksum, &DownloadHistoryManager::load())
}

fn verify_with_history(
    file_path: &Path,
    expected_size: Option<u64>,
    expected_checksum: Option<&str>,
    history: &DownloadHistoryManager,
) -> Result<BuildVerificationResult, String> {
    let target = resolve_target(file_path);
    let actual_size = std::fs::metadata(&target)
        .map_err(|e| format!("Cannot read {:?}: {}", target, e))?
        .len();
    let is_part = final_name_of(&target).is_some();
    let entry = history_entry_for(history, &final_name_of(&target).unwrap_or_else(|| target.clone()));

    let state = DownloadState::load_from_path(&DownloadState::state_file_path(&target)).ok().flatten();
    let has_state_file = state.is_some();
    let exp_size = expected_size
        .or(state.as_ref().map(|s| s.file_size))
        .or(entry.map(|e| e.file_size).filter(|&s| s > 0));

    let mut missing = Vec::new();
    if let Some(ref state) = state {
        missing.extend(compute_gaps(state.file_size, &state.completed_ranges));
    } else if is_part {
        // Without state nothing proves which bytes of a preallocated in-progress file were written.
        missing.extend(exp_size.and_then(|exp| ByteRange::from_len(0, exp).ok()));
    }
    if let Some(exp) = exp_size.filter(|&exp| actual_size < exp) {
        missing.extend(ByteRange::new(actual_size, exp - 1).ok());
    }
    let missing_ranges = merge_ranges(missing);
    let oversized = exp_size.is_some_and(|exp| actual_size > exp);

    let checksum = match (expected_checksum, entry.and_then(|e| e.blake3_hash.as_deref())) {
        (Some(c), _) => Some((c.to_string(), "provided checksum")),
        (None, Some(h)) => Some((format!("blake3:{}", h), "BLAKE3 hash from download history")),
        (None, None) => None,
    };
    let sizes_ok = missing_ranges.is_empty() && !oversized;
    let checksum_result = checksum
        .as_ref()
        .filter(|_| sizes_ok)
        .map(|(c, _)| DiskWriter::verify_file_checksum(&target, c));
    let checksum_match = checksum_result.as_ref().map(|r| matches!(r, Ok(true)));

    let is_complete = sizes_ok
        && match checksum_match {
            Some(matched) => matched,
            None => has_state_file || (exp_size.is_some() && !is_part),
        };

    let status_message = if !missing_ranges.is_empty() {
        let missing_bytes: u64 = missing_ranges.iter().map(|r| r.len()).sum();
        format!(
            "Build incomplete: {} missing range(s) totaling {} bytes",
            missing_ranges.len(),
            missing_bytes
        )
    } else if let (true, Some(exp)) = (oversized, exp_size) {
        format!("File is larger than expected ({} > {} bytes); its contents cannot be trusted", actual_size, exp)
    } else if let (Some(result), Some((_, source))) = (&checksum_result, &checksum) {
        match result {
            Ok(true) => format!("Build complete: {} bytes, verified against {}", actual_size, source),
            Ok(false) => format!("Checksum mismatch against {}: the file is corrupt and must be re-downloaded", source),
            Err(e) => format!("Checksum verification against {} failed: {}", source, e),
        }
    } else if has_state_file {
        format!("Build complete: all {} bytes recorded as downloaded (no checksum available to verify contents)", actual_size)
    } else if is_complete {
        format!("File size matches the expected {} bytes (no checksum or download state available to verify contents)", actual_size)
    } else {
        format!(
            "Cannot verify: no download state, expected size or checksum is known for this file ({} bytes on disk)",
            actual_size
        )
    };

    Ok(BuildVerificationResult {
        file_path: target,
        expected_size: exp_size,
        actual_size,
        has_state_file,
        missing_ranges,
        is_complete,
        checksum_match,
        status_message,
    })
}

enum FetchError {
    /// This attempt failed; another attempt or mirror may succeed.
    Retry(String),
    /// This mirror serves another version of the file; nothing from this answer was written.
    WrongVersion(String),
    /// Stop the whole repair (cancelled, or the local file cannot be written).
    Fatal(String),
}

/// Awaits `fut`, giving up promptly once the cancel flag is set.
async fn until_cancelled<T>(fut: impl Future<Output = T>, cancel: Option<&AtomicBool>) -> Result<T, FetchError> {
    tokio::pin!(fut);
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(FetchError::Fatal("Repair cancelled by user".to_string()));
        }
        if let Ok(out) = tokio::time::timeout(CANCEL_POLL, &mut fut).await {
            return Ok(out);
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("Background task failed: {}", e))
}

/// The ETag and Last-Modified identifying one version of the file.
#[derive(Clone)]
struct Version {
    etag: Option<String>,
    last_modified: Option<String>,
}

impl Version {
    fn recorded(state: &DownloadState) -> Self {
        Self { etag: state.etag.clone(), last_modified: state.last_modified.clone() }
    }

    fn served(headers: &HeaderMap) -> Self {
        let header = |name| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        Self { etag: header(ETAG), last_modified: header(LAST_MODIFIED) }
    }

    /// Whether `other` may be this version: equal ETags where both have one, else equal
    /// Last-Modified where both have one.
    fn matches(&self, other: &Version) -> bool {
        match (&self.etag, &other.etag) {
            (Some(a), Some(b)) => a == b,
            _ => match (&self.last_modified, &other.last_modified) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            },
        }
    }

    /// Validator for `If-Range`: a strong ETag, else Last-Modified (weak ETags are not allowed there).
    fn if_range(&self) -> Option<&str> {
        self.etag.as_deref().filter(|e| !e.starts_with("W/")).or(self.last_modified.as_deref())
    }
}

/// Fetches bytes `*offset..=end` from `url` into the file, advancing `*offset` past every byte
/// actually written. Only a 206 whose Content-Range starts at `*offset`, ends at or before `end`
/// and reports the file's full size is accepted; the server may return fewer bytes than asked for.
///
/// `seen` is the version this mirror served so far. Mirrors of one download may differ in their
/// validators, so each mirror is held to its own: its first answer must match the version the
/// state recorded, and later requests send its own validator in If-Range.
#[allow(clippy::too_many_arguments)]
async fn fetch_range(
    client: &Client,
    url: &Url,
    seen: &mut Option<Version>,
    offset: &mut u64,
    end: u64,
    repair: &RepairTarget,
    cancel: Option<&AtomicBool>,
    on_bytes: &mut (dyn FnMut(u64) + Send),
) -> Result<(), FetchError> {
    let RepairTarget { state, writer, .. } = repair;
    let total_size = writer.size();
    let validator = seen.as_ref().and_then(Version::if_range).map(str::to_string);
    let mut request = client.get(url.clone()).header(RANGE, format!("bytes={}-{}", *offset, end));
    if let Some(validator) = &validator {
        request = request.header(IF_RANGE, validator.as_str());
    }
    let resp = until_cancelled(request.send(), cancel)
        .await?
        .map_err(|e| FetchError::Retry(format!("request to {} failed: {}", url, e)))?;

    let changed = || FetchError::WrongVersion(format!("the file at {} changed since it was downloaded", url));
    if resp.status() == StatusCode::OK {
        // Under If-Range, a 200 still carrying our validator only means the range was ignored.
        return Err(match &validator {
            Some(v) if !carries_validator(resp.headers(), v) => changed(),
            _ => FetchError::Retry(format!("{} ignored the range request and sent the whole file", url)),
        });
    }
    if resp.status() != StatusCode::PARTIAL_CONTENT {
        return Err(FetchError::Retry(format!(
            "{} answered HTTP {} instead of 206 Partial Content",
            url,
            resp.status()
        )));
    }
    let version = Version::served(resp.headers());
    let same = match seen.as_ref() {
        Some(known) => known.matches(&version),
        None => Version::recorded(state).matches(&version),
    };
    if !same {
        return Err(changed());
    }
    let header = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| FetchError::Retry(format!("{} sent 206 without a Content-Range", url)))?
        .to_string();
    let served = match ByteRange::parse_content_range(&header) {
        Ok((served, Some(total))) if served.start == *offset && served.end <= end && total == total_size => served,
        _ => {
            return Err(FetchError::Retry(format!(
                "{} answered Content-Range '{}' for bytes {}-{} of a {}-byte file",
                url, header, *offset, end, total_size
            )))
        }
    };
    seen.get_or_insert(version);

    let mut stream = resp.bytes_stream();
    while *offset <= served.end {
        let chunk = match until_cancelled(stream.next(), cancel).await? {
            Some(Ok(chunk)) => chunk,
            Some(Err(e)) => return Err(FetchError::Retry(format!("error reading from {}: {}", url, e))),
            None => {
                return Err(FetchError::Retry(format!(
                    "{} ended the response at byte {} of {}",
                    url, *offset, served.end
                )))
            }
        };
        let wanted = served.end + 1 - *offset;
        let data = chunk.slice(..chunk.len().min(usize::try_from(wanted).unwrap_or(usize::MAX)));
        let len = data.len() as u64;
        let (writer, at) = (writer.clone(), *offset);
        blocking(move || writer.write_chunk_slice(at, &data))
            .await
            .map_err(FetchError::Fatal)?
            .map_err(|e| FetchError::Fatal(format!("Failed writing repair data: {}", e)))?;
        *offset += len;
        on_bytes(len);
    }
    Ok(())
}

/// The in-progress file a repair writes into, and the claim that keeps downloads away from it
/// until the repair ends.
struct RepairTarget {
    final_path: PathBuf,
    part: PathBuf,
    state_path: PathBuf,
    state: DownloadState,
    writer: DiskWriter,
    _claim: TargetClaim,
}

/// Sets up `<final>.part` + `<final>.part.hfstate` for repairing `file_path`, recording as complete
/// only bytes known to be good: those the existing state (or, without one, the caller) vouches
/// for, minus `missing_ranges` and anything past the end of the file. A damaged file at its final
/// name is moved to the `.part` first, since a file at its final name must always be complete; an
/// unfinished repair then leaves a download the engine can resume. Fails without touching
/// anything while a download holds the target.
fn prepare_repair(
    file_path: &Path,
    total_size: u64,
    missing_ranges: &[ByteRange],
    urls: &[Url],
) -> Result<RepairTarget, String> {
    let final_of = |target: &Path| final_name_of(target).unwrap_or_else(|| target.to_path_buf());
    let claimed = final_of(&resolve_target(file_path));
    let Some(claim) = claim_target(&claimed)? else {
        return Err(format!(
            "{} is still being downloaded; stop that download before repairing it",
            claimed.display()
        ));
    };
    // Resolved again under the claim: a download that just finished has moved its `.part`.
    let target = resolve_target(file_path);
    let final_path = final_of(&target);
    if final_path != claimed {
        return Err(format!("{} changed while the repair was starting; try again", file_path.display()));
    }
    let on_disk = match std::fs::metadata(&target) {
        Ok(meta) => Some(meta.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Cannot read {:?}: {}", target, e)),
    };
    let len = on_disk.unwrap_or(0);
    if len > total_size {
        return Err(format!(
            "{:?} is larger than the expected {} bytes; refusing to truncate it",
            target, total_size
        ));
    }

    let target_state_path = DownloadState::state_file_path(&target);
    let mut state = match DownloadState::load_from_path(&target_state_path) {
        Ok(Some(state)) if state.file_size == total_size => state,
        Ok(Some(state)) => {
            return Err(format!(
                "Download state records {} bytes but repair was asked for {} bytes",
                state.file_size, total_size
            ))
        }
        _ => {
            // No usable bookkeeping: everything outside the reported gaps is taken as present.
            let mut state = DownloadState::new(
                target.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                total_size,
                crate::engine::DownloadOptions::default().base_chunk_size,
                urls.iter().map(|u| u.to_string()).collect(),
            );
            state.completed_ranges.extend(ByteRange::from_len(0, total_size).ok());
            state
        }
    };
    let mut untrusted = compute_gaps(total_size, &state.completed_ranges);
    untrusted.extend_from_slice(missing_ranges);
    untrusted.extend(ByteRange::new(len, total_size.saturating_sub(1)).ok());
    state.completed_ranges = compute_gaps(total_size, &merge_ranges(untrusted));

    let part = with_suffix(&final_path, ".part");
    let state_path = DownloadState::state_file_path(&part);
    if part != target {
        if part.exists() {
            return Err(format!("{:?} already exists; refusing to overwrite it", part));
        }
        // State first: a crash before the rename leaves only a stray state file behind.
        state
            .save_atomic(&state_path)
            .map_err(|e| format!("Failed to save repair state: {}", e))?;
        if on_disk.is_some() {
            if let Err(e) = std::fs::rename(&target, &part) {
                let _ = DownloadState::remove(&state_path);
                return Err(format!("Failed to move {:?} to {:?} for repair: {}", target, part, e));
            }
        }
        let _ = DownloadState::remove(&target_state_path);
    }

    let writer = DiskWriter::open_or_create(&part, total_size)
        .map_err(|e| format!("Failed to open file for repair: {}", e))?;
    Ok(RepairTarget { final_path, part, state_path, state, writer, _claim: claim })
}

/// Makes the repair's progress durable; a complete `.part` is renamed to its final name. The
/// claim is released only after that.
fn finish_repair(repair: RepairTarget, complete: bool) -> Result<(), String> {
    let RepairTarget { final_path, part, state_path, mut state, writer, _claim } = repair;
    // Record progress only once it is durable.
    writer.sync().map_err(|e| format!("Failed to flush repaired data to disk: {}", e))?;
    drop(writer);

    state.completed_ranges = merge_ranges(std::mem::take(&mut state.completed_ranges));
    if complete && !final_path.exists() {
        std::fs::rename(&part, &final_path)
            .map_err(|e| format!("Repaired {:?} but could not rename it to {:?}: {}", part, final_path, e))?;
        let _ = DownloadState::remove(&state_path);
        return Ok(());
    }
    if complete {
        tracing::warn!("Repaired {:?} but {:?} already exists; leaving the .part in place", part, final_path);
    }
    state
        .save_atomic(&state_path)
        .map_err(|e| format!("Failed to save repair progress: {}", e))
}

/// Selectively downloads and repairs missing chunk ranges of a file.
///
/// The repair always happens in `<final>.part` (a damaged file at its final name is moved there
/// first) and bytes are recorded in its `.hfstate` only once they have actually been written, so
/// an interrupted repair leaves a download the engine resumes. Every gap the state records is
/// fetched, including `missing_ranges`. Only a complete file is renamed back to its final name.
/// A mirror is used only while it serves the version the state records (by ETag, else
/// Last-Modified); with no such mirror left the repair stops instead of mixing two versions.
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
    if let Some(r) = missing_ranges.iter().find(|r| r.end >= total_size) {
        return Err(format!("Missing range {} lies outside the {}-byte file", r, total_size));
    }

    let client = Client::builder()
        .tcp_nodelay(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .default_headers(crate::resolver::SmartResolver::default_anti_qos_headers())
        .build()
        .map_err(|e| format!("Failed to build HTTP client for repair: {}", e))?;

    let mut repair = {
        let (file_path, missing_ranges, urls) = (file_path.to_path_buf(), missing_ranges.to_vec(), urls.to_vec());
        blocking(move || prepare_repair(&file_path, total_size, &missing_ranges, &urls)).await??
    };

    let gaps = compute_gaps(total_size, &repair.state.completed_ranges);
    let total_repair_bytes: u64 = gaps.iter().map(|r| r.len()).sum();
    let mut repaired_bytes: u64 = 0;
    let mut on_bytes = |n: u64| {
        repaired_bytes += n;
        progress_callback(repaired_bytes, total_repair_bytes);
    };
    let cancel = cancel_flag.as_deref();
    let mut outcome = Ok(());
    // Per mirror: the version it served so far, and why it was dropped.
    let mut seen: Vec<Option<Version>> = vec![None; urls.len()];
    let mut dropped: Vec<Option<String>> = vec![None; urls.len()];

    'ranges: for (range_idx, range) in gaps.iter().enumerate() {
        let mut offset = range.start;
        let mut failures = 0;
        while offset <= range.end {
            let usable: Vec<usize> = (0..urls.len()).filter(|&i| dropped[i].is_none()).collect();
            if usable.is_empty() {
                let reasons: Vec<&str> = dropped.iter().flatten().map(String::as_str).collect();
                outcome = Err(format!("Repair stopped: {}", reasons.join("; ")));
                break 'ranges;
            }
            let mirror = usable[(range_idx + failures) % usable.len()];
            let before = offset;
            let result =
                fetch_range(&client, &urls[mirror], &mut seen[mirror], &mut offset, range.end, &repair, cancel, &mut on_bytes)
                    .await;
            if offset > before {
                repair.state.completed_ranges.push(ByteRange { start: before, end: offset - 1 });
                failures = 0;
            }
            match result {
                Ok(()) => {}
                Err(FetchError::Fatal(e)) => {
                    outcome = Err(e);
                    break 'ranges;
                }
                Err(FetchError::WrongVersion(e)) => {
                    tracing::warn!("Repair no longer uses {}: {}", urls[mirror], e);
                    dropped[mirror] = Some(e);
                }
                Err(FetchError::Retry(e)) => {
                    failures += 1;
                    tracing::warn!("Repair of range {} failed: {}", range, e);
                    if failures >= usable.len() * ATTEMPTS_PER_MIRROR {
                        outcome = Err(format!("Could not repair range {}: {}", range, e));
                        break 'ranges;
                    }
                    if let Err(FetchError::Fatal(e) | FetchError::Retry(e)) =
                        until_cancelled(tokio::time::sleep(RETRY_DELAY), cancel).await
                    {
                        outcome = Err(e);
                        break 'ranges;
                    }
                }
            }
        }
    }

    let complete = outcome.is_ok() && compute_gaps(total_size, &repair.state.completed_ranges).is_empty();
    blocking(move || finish_repair(repair, complete)).await??;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{tempdir, NamedTempFile};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn empty_history() -> DownloadHistoryManager {
        let dir = tempdir().unwrap();
        DownloadHistoryManager::load_from_path(&dir.path().join("history.json"))
    }

    #[test]
    fn test_verify_complete_file() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();
        std::fs::write(&path, b"Hello World 12345").unwrap();

        let res = verify_with_history(&path, Some(17), None, &empty_history()).unwrap();
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

        let res = verify_with_history(&path, None, None, &empty_history()).unwrap();
        assert!(!res.is_complete);
        assert_eq!(res.missing_ranges.len(), 1);
        assert_eq!(res.missing_ranges[0].start, 500);
        assert_eq!(res.missing_ranges[0].end, 999);

        let _ = DownloadState::remove(&state_path);
    }

    #[test]
    fn verify_without_any_reference_is_not_complete() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mystery.bin");
        std::fs::write(&path, vec![0u8; 64]).unwrap();

        let res = verify_with_history(&path, None, None, &empty_history()).unwrap();
        assert!(!res.is_complete);
        assert!(res.status_message.starts_with("Cannot verify"), "{}", res.status_message);
    }

    #[test]
    fn truncated_file_reports_missing_tail_even_with_complete_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("build.bin");
        std::fs::write(&path, vec![1u8; 500]).unwrap();
        let mut state = DownloadState::new("build.bin".into(), 1000, 250, vec![]);
        state.completed_ranges.push(ByteRange::new(0, 999).unwrap());
        state.save_atomic(&DownloadState::state_file_path(&path)).unwrap();

        let res = verify_with_history(&path, None, None, &empty_history()).unwrap();
        assert!(!res.is_complete);
        assert_eq!(res.missing_ranges, vec![ByteRange::new(500, 999).unwrap()]);
    }

    #[test]
    fn verify_uses_part_file_and_its_state() {
        let dir = tempdir().unwrap();
        let final_path = dir.path().join("game.zip");
        let part = dir.path().join("game.zip.part");
        std::fs::write(&part, vec![0u8; 100]).unwrap();
        let mut state = DownloadState::new("game.zip".into(), 100, 50, vec![]);
        state.completed_ranges.push(ByteRange::new(0, 49).unwrap());
        state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();

        let res = verify_with_history(&final_path, None, None, &empty_history()).unwrap();
        assert_eq!(res.file_path, part);
        assert!(res.has_state_file);
        assert_eq!(res.missing_ranges, vec![ByteRange::new(50, 99).unwrap()]);

        // Without state nothing in a preallocated .part is trusted.
        DownloadState::remove(&DownloadState::state_file_path(&part)).unwrap();
        let res = verify_with_history(&final_path, Some(100), None, &empty_history()).unwrap();
        assert_eq!(res.missing_ranges, vec![ByteRange::new(0, 99).unwrap()]);
    }

    #[test]
    fn verify_checks_history_blake3_for_exact_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("iso.img");
        std::fs::write(&path, b"good contents").unwrap();
        let mut history = DownloadHistoryManager::load_from_path(&dir.path().join("history.json"));
        let mut entry = HistoryEntry::new("iso.img".into(), path.clone(), 13, vec![]);
        entry.blake3_hash = Some(blake3::hash(b"good contents").to_hex().to_string());
        history.add_or_update(entry);

        let res = verify_with_history(&path, None, None, &history).unwrap();
        assert!(res.is_complete, "{}", res.status_message);
        assert_eq!(res.checksum_match, Some(true));

        std::fs::write(&path, b"bad!contents!").unwrap();
        let res = verify_with_history(&path, None, None, &history).unwrap();
        assert!(!res.is_complete);
        assert_eq!(res.checksum_match, Some(false));
    }

    /// Serves `content` over HTTP; `respond(attempt, request, start, end, content)` builds each raw
    /// response from the lowercased request head and its Range.
    async fn mock_server(
        content: Vec<u8>,
        respond: fn(usize, &str, u64, u64, &[u8]) -> Vec<u8>,
    ) -> Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for attempt in 0.. {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                }
                let req = String::from_utf8_lossy(&req).to_ascii_lowercase();
                let spec = req.split("range: bytes=").nth(1).unwrap().lines().next().unwrap();
                let (s, e) = spec.trim().split_once('-').unwrap();
                let resp = respond(attempt, &req, s.parse().unwrap(), e.parse().unwrap(), &content);
                let _ = sock.write_all(&resp).await;
                let _ = sock.shutdown().await;
            }
        });
        Url::parse(&format!("http://{}/file.bin", addr)).unwrap()
    }

    fn partial(start: u64, end: u64, content: &[u8], body_len: usize) -> Vec<u8> {
        let body = &content[start as usize..=end as usize];
        let mut resp = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            start, end, content.len(), body.len()
        )
        .into_bytes();
        resp.extend_from_slice(&body[..body_len.min(body.len())]);
        resp
    }

    fn full_200(content: &[u8]) -> Vec<u8> {
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            content.len()
        )
        .into_bytes();
        resp.extend_from_slice(content);
        resp
    }

    /// `resp` with `header` added after its status line.
    fn with_header(resp: Vec<u8>, header: &str) -> Vec<u8> {
        let head_end = resp.windows(2).position(|w| w == b"\r\n").unwrap() + 2;
        [&resp[..head_end], header.as_bytes(), b"\r\n", &resp[head_end..]].concat()
    }

    /// 1000-byte file whose bytes 100..=199 are missing; state records the rest as complete.
    fn damaged_file(dir: &Path, content: &[u8]) -> PathBuf {
        let path = dir.join("build.bin");
        let mut data = content.to_vec();
        data[100..200].fill(0);
        std::fs::write(&path, &data).unwrap();
        let mut state = DownloadState::new("build.bin".into(), 1000, 250, vec![]);
        state.completed_ranges = vec![ByteRange::new(0, 99).unwrap(), ByteRange::new(200, 999).unwrap()];
        state.save_atomic(&DownloadState::state_file_path(&path)).unwrap();
        path
    }

    /// `damaged_file` whose state records that it was downloaded as ETag `"v1"`.
    fn damaged_file_v1(dir: &Path, content: &[u8]) -> PathBuf {
        let path = damaged_file(dir, content);
        let state_path = DownloadState::state_file_path(&path);
        let mut state = DownloadState::load_from_path(&state_path).unwrap().unwrap();
        state.etag = Some("\"v1\"".into());
        state.save_atomic(&state_path).unwrap();
        path
    }

    fn content() -> Vec<u8> {
        (0..1000u32).map(|i| (i % 251) as u8 + 1).collect()
    }

    fn part_of(path: &Path) -> PathBuf {
        with_suffix(path, ".part")
    }

    fn gaps_on_disk(part: &Path) -> Vec<ByteRange> {
        let state = DownloadState::load_from_path(&DownloadState::state_file_path(part)).unwrap().unwrap();
        compute_gaps(state.file_size, &state.completed_ranges)
    }

    #[tokio::test]
    async fn repair_rejects_full_200_response() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = damaged_file(dir.path(), &content);
        let url = mock_server(content.clone(), |_, _, _, _, c| full_200(c)).await;

        let gap = ByteRange::new(100, 199).unwrap();
        let res = repair_missing_ranges(&path, 1000, &[gap], &[url], None, |_, _| {}).await;
        assert!(res.is_err());

        // The unfinished repair is left as a resumable .part, never under the final name.
        assert!(!path.exists() && !DownloadState::state_file_path(&path).exists());
        let on_disk = std::fs::read(part_of(&path)).unwrap();
        assert_eq!(&on_disk[200..], &content[200..], "good data was overwritten");
        assert!(on_disk[100..200].iter().all(|&b| b == 0));
        assert_eq!(gaps_on_disk(&part_of(&path)), vec![gap]);
    }

    #[tokio::test]
    async fn repair_records_only_bytes_that_arrived() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = damaged_file(dir.path(), &content);
        // First answer is cut off after 30 bytes, every later one ignores Range.
        let url = mock_server(content.clone(), |attempt, _, s, e, c| {
            if attempt == 0 { partial(s, e, c, 30) } else { full_200(c) }
        })
        .await;

        let res = repair_missing_ranges(&path, 1000, &[ByteRange::new(100, 199).unwrap()], &[url], None, |_, _| {}).await;
        assert!(res.is_err());

        assert_eq!(gaps_on_disk(&part_of(&path)), vec![ByteRange::new(130, 199).unwrap()]);
        assert_eq!(std::fs::read(part_of(&path)).unwrap()[100..130], content[100..130]);
    }

    #[tokio::test]
    async fn failed_repair_of_truncated_final_file_leaves_resumable_part() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = dir.path().join("build.bin");
        std::fs::write(&path, &content[..500]).unwrap();
        let url = mock_server(content.clone(), |_, _, _, _, c| full_200(c)).await;

        let res = verify_with_history(&path, Some(1000), None, &empty_history()).unwrap();
        let repaired = repair_missing_ranges(&path, 1000, &res.missing_ranges, std::slice::from_ref(&url), None, |_, _| {}).await;
        assert!(repaired.is_err());

        // No full-size file with a zero tail may sit under the final name.
        assert!(!path.exists());
        let part = part_of(&path);
        let on_disk = std::fs::read(&part).unwrap();
        assert_eq!(on_disk.len(), 1000, "preallocated like an engine .part");
        assert_eq!(on_disk[..500], content[..500]);
        assert_eq!(gaps_on_disk(&part), vec![ByteRange::new(500, 999).unwrap()]);
        let state = DownloadState::load_from_path(&DownloadState::state_file_path(&part)).unwrap().unwrap();
        assert_eq!(state.mirrors, vec![url.to_string()], "the engine resumes a .part only from its own mirrors");
    }

    #[tokio::test]
    async fn repair_of_truncated_final_file_restores_it() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = dir.path().join("build.bin");
        std::fs::write(&path, &content[..500]).unwrap();
        let url = mock_server(content.clone(), |_, _, s, e, c| partial(s, e, c, usize::MAX)).await;

        let res = verify_with_history(&path, Some(1000), None, &empty_history()).unwrap();
        repair_missing_ranges(&path, 1000, &res.missing_ranges, &[url], None, |_, _| {}).await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), content);
        let part = part_of(&path);
        assert!(!part.exists());
        assert!(!DownloadState::state_file_path(&part).exists());
        assert!(!DownloadState::state_file_path(&path).exists());
    }

    #[tokio::test]
    async fn repair_sends_if_range_and_stops_when_the_file_changes() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = damaged_file_v1(dir.path(), &content);
        // The first answer, capped at 40 bytes, is still build "v1". Then the server has a new
        // build: it honors Range only while If-Range still matches, and without If-Range it would
        // pass the new bytes off as "v1".
        let url = mock_server(content.clone(), |attempt, req, s, e, c| {
            let new_build: Vec<u8> = c.iter().map(|b| b ^ 0xff).collect();
            match attempt {
                0 => with_header(partial(s, e.min(s + 39), c, usize::MAX), "ETag: \"v1\""),
                _ if req.contains("if-range: \"v1\"") => with_header(full_200(&new_build), "ETag: \"v2\""),
                _ => with_header(partial(s, e, &new_build, usize::MAX), "ETag: \"v1\""),
            }
        })
        .await;

        let gap = ByteRange::new(100, 199).unwrap();
        let res = repair_missing_ranges(&path, 1000, &[gap], &[url], None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("changed")), "{:?}", res);

        assert!(!path.exists());
        let on_disk = std::fs::read(part_of(&path)).unwrap();
        assert_eq!(on_disk[100..140], content[100..140]);
        assert!(on_disk[140..200].iter().all(|&b| b == 0), "no byte of the new build may be written");
        assert_eq!(gaps_on_disk(&part_of(&path)), vec![ByteRange::new(140, 199).unwrap()]);
    }

    /// 1000-byte file missing bytes 100..=199 and 500..=599, downloaded as ETag "v1" from a
    /// mirror whose Last-Modified was `MON`.
    fn damaged_twice(dir: &Path, content: &[u8]) -> PathBuf {
        let path = dir.join("build.bin");
        let mut data = content.to_vec();
        data[100..200].fill(0);
        data[500..600].fill(0);
        std::fs::write(&path, &data).unwrap();
        let mut state = DownloadState::new("build.bin".into(), 1000, 250, vec![]);
        state.completed_ranges =
            vec![ByteRange::new(0, 99).unwrap(), ByteRange::new(200, 499).unwrap(), ByteRange::new(600, 999).unwrap()];
        state.etag = Some("\"v1\"".into());
        state.last_modified = Some(MON.into());
        state.save_atomic(&DownloadState::state_file_path(&path)).unwrap();
        path
    }

    const MON: &str = "Mon, 06 Nov 2023 08:49:37 GMT";

    #[tokio::test]
    async fn repair_skips_a_mirror_whose_validators_differ_instead_of_aborting() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = damaged_twice(dir.path(), &content);
        // Both serve the same bytes, as the download accepted, but only `a` has the recorded
        // validators; `b` has no ETag and a later Last-Modified.
        let a = mock_server(content.clone(), |_, _, s, e, c| {
            with_header(with_header(partial(s, e, c, usize::MAX), "ETag: \"v1\""), &format!("Last-Modified: {MON}"))
        })
        .await;
        let b = mock_server(content.clone(), |_, _, s, e, c| {
            with_header(partial(s, e, c, usize::MAX), "Last-Modified: Tue, 07 Nov 2023 08:49:37 GMT")
        })
        .await;

        let gaps = [ByteRange::new(100, 199).unwrap(), ByteRange::new(500, 599).unwrap()];
        repair_missing_ranges(&path, 1000, &gaps, &[a, b], None, |_, _| {}).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), content);
    }

    #[tokio::test]
    async fn repair_takes_a_200_with_our_validator_as_an_ignored_range() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = damaged_file_v1(dir.path(), &content);
        // `a` sends 40 bytes, then ignores Range for the unchanged file; `b` honors it.
        let a = mock_server(content.clone(), |attempt, _, s, e, c| match attempt {
            0 => with_header(partial(s, e.min(s + 39), c, usize::MAX), "ETag: \"v1\""),
            _ => with_header(full_200(c), "ETag: \"v1\""),
        })
        .await;
        let b = mock_server(content.clone(), |_, _, s, e, c| with_header(partial(s, e, c, usize::MAX), "ETag: \"v1\"")).await;

        repair_missing_ranges(&path, 1000, &[ByteRange::new(100, 199).unwrap()], &[a, b], None, |_, _| {}).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), content);
    }

    #[tokio::test]
    async fn repair_aborts_when_partial_response_has_new_etag() {
        let dir = tempdir().unwrap();
        let path = damaged_file_v1(dir.path(), &content());
        // A server that ignores If-Range but reports the new build's ETag.
        let new_build: Vec<u8> = content().iter().map(|b| b ^ 0xff).collect();
        let url = mock_server(new_build, |_, _, s, e, c| with_header(partial(s, e, c, usize::MAX), "ETag: \"v2\"")).await;

        let gap = ByteRange::new(100, 199).unwrap();
        let res = repair_missing_ranges(&path, 1000, &[gap], &[url], None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("changed")), "{:?}", res);
        assert!(std::fs::read(part_of(&path)).unwrap()[100..200].iter().all(|&b| b == 0));
        assert_eq!(gaps_on_disk(&part_of(&path)), vec![gap]);
    }

    #[tokio::test]
    async fn repair_resumes_short_ranges_and_finalizes_part_file() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = damaged_file(dir.path(), &content);
        let part = dir.path().join("build.bin.part");
        std::fs::rename(&path, &part).unwrap();
        std::fs::rename(DownloadState::state_file_path(&path), DownloadState::state_file_path(&part)).unwrap();
        // The server caps every answer at 40 bytes.
        let url = mock_server(content.clone(), |_, _, s, e, c| partial(s, e.min(s + 39), c, usize::MAX)).await;

        let res = verify_with_history(&path, None, None, &empty_history()).unwrap();
        repair_missing_ranges(&path, 1000, &res.missing_ranges, &[url], None, |_, _| {}).await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), content);
        assert!(!part.exists());
        assert!(!DownloadState::state_file_path(&part).exists());
    }

    #[tokio::test]
    async fn repair_honors_cancel_while_mirror_stalls() {
        let dir = tempdir().unwrap();
        let path = damaged_file(dir.path(), &content());
        // Accepts the connection and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/f", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move {
            let _held = listener.accept().await;
            std::future::pending::<()>().await;
        });

        let cancel = Arc::new(AtomicBool::new(false));
        let (flag, target) = (Arc::clone(&cancel), path.clone());
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let claimed_by_repair = claim_target(&target).unwrap().is_none();
            flag.store(true, Ordering::Relaxed);
            claimed_by_repair
        });
        let started = std::time::Instant::now();
        // Spawned, like the GUI does, which also proves the repair future is Send.
        let repaired = path.clone();
        let res = tokio::spawn(async move {
            repair_missing_ranges(&repaired, 1000, &[ByteRange::new(100, 199).unwrap()], &[url], Some(cancel), |_, _| {}).await
        })
        .await
        .unwrap();
        assert_eq!(res, Err("Repair cancelled by user".to_string()));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(canceller.await.unwrap(), "a running repair must hold the claim");
        assert!(claim_target(&path).unwrap().is_some(), "a cancelled repair must release the claim");
    }

    #[tokio::test]
    async fn repair_refuses_a_target_a_download_holds() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = damaged_file(dir.path(), &content);
        let state_path = DownloadState::state_file_path(&path);
        let (data, state) = (std::fs::read(&path).unwrap(), std::fs::read(&state_path).unwrap());
        let url = mock_server(content.clone(), |_, _, s, e, c| partial(s, e, c, usize::MAX)).await;
        let gap = ByteRange::new(100, 199).unwrap();

        let download = claim_target(&path).unwrap().expect("free target");
        let res = repair_missing_ranges(&path, 1000, &[gap], std::slice::from_ref(&url), None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("still being downloaded")), "{:?}", res);
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(std::fs::read(&state_path).unwrap(), state);
        assert!(!part_of(&path).exists());
        drop(download);

        repair_missing_ranges(&path, 1000, &[gap], &[url], None, |_, _| {}).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), content);
        assert!(claim_target(&path).unwrap().is_some(), "a finished repair must release the claim");
    }

    #[tokio::test]
    async fn failed_repair_releases_the_claim() {
        let dir = tempdir().unwrap();
        let path = damaged_file(dir.path(), &content());
        let url = mock_server(content(), |_, _, _, _, c| full_200(c)).await;
        let gap = ByteRange::new(100, 199).unwrap();
        assert!(repair_missing_ranges(&path, 1000, &[gap], &[url], None, |_, _| {}).await.is_err());
        assert!(claim_target(&path).unwrap().is_some());

        // Also when the repair fails before it starts: the .part it would write is not its own.
        std::fs::write(dir.path().join("fresh.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.path().join("fresh.bin.part"), b"other").unwrap();
        let fresh = dir.path().join("fresh.bin");
        let res = repair_missing_ranges(&fresh, 1000, &[gap], &[Url::parse("http://127.0.0.1:9/").unwrap()], None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("refusing to overwrite")), "{:?}", res);
        assert!(claim_target(&fresh).unwrap().is_some());
    }
}
