use std::collections::HashSet;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use reqwest::{Client, StatusCode};
use tokio::sync::broadcast;
use url::Url;
use crate::engine::{claim_target, discard_partial, DownloadEngine, DownloadOptions, EngineSnapshot, TargetClaim};
use crate::history::{is_redacted, DownloadHistoryManager, HistoryEntry, REDACTED_LINK};
use crate::range::{compute_gaps, merge_ranges, ByteRange};
use crate::resolver::with_agent_for;
use crate::state::DownloadState;
use crate::storage::DiskWriter;
use crate::worker::carries_validator;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A mirror that sends nothing for this long is treated as stalled.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a pending network operation checks the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(200);
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Failed attempts per mirror (without any progress) before a range, or the mirror, is given up.
const ATTEMPTS_PER_MIRROR: usize = 2;
const CANCELLED: &str = "Repair cancelled by user";

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
            return Err(FetchError::Fatal(CANCELLED.to_string()));
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
    let mut request = with_agent_for(client.get(url.clone()), url).header(RANGE, format!("bytes={}-{}", *offset, end));
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
                DownloadOptions::default().base_chunk_size,
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
    // The engine resumes a `.part` only for a download from one of the mirrors its state lists.
    for url in urls.iter().map(Url::to_string) {
        if !state.mirrors.contains(&url) {
            state.mirrors.push(url);
        }
    }

    let part = with_suffix(&final_path, ".part");
    let state_path = DownloadState::state_file_path(&part);
    if part == target {
        // Nothing was written yet: the state records only bytes the file already holds.
        state
            .save_atomic(&state_path)
            .map_err(|e| format!("Failed to save repair state: {}", e))?;
    } else {
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
///
/// First every mirror must show it serves the version the state records (by ETag, else
/// Last-Modified); the others are left out, and with none left the repair stops. The prepared
/// `.part` then goes to the download engine, which fetches the gaps from one of them over several
/// connections (`options` gives their number, the speed limit, retries and timeouts; its output,
/// checksum and credentials are not used: the repair connects directly). The engine holds a mirror
/// to its version only through If-Range, so it gets only a mirror that has a validator allowed
/// there and honors it (see [`VersionCheck::mirror`]). Without such a mirror, for a `.part` whose
/// final name is taken, and for media and playlist URLs the engine would hand to other tools, the
/// gaps are fetched here range by range instead, holding each mirror to its version on every
/// answer. Either way a repair never mixes two versions of a file.
pub async fn repair_missing_ranges<F>(
    file_path: &Path,
    total_size: u64,
    missing_ranges: &[ByteRange],
    urls: &[Url],
    options: &DownloadOptions,
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
    // A link history saved without its secret is never requested.
    let usable: Vec<Url> = urls.iter().filter(|u| !is_redacted(u.as_str())).cloned().collect();
    if usable.is_empty() {
        return Err(REDACTED_LINK.to_string());
    }
    let urls = usable.as_slice();
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

    let (repair, final_taken) = {
        let (file_path, missing_ranges, urls) = (file_path.to_path_buf(), missing_ranges.to_vec(), urls.to_vec());
        blocking(move || {
            let repair = prepare_repair(&file_path, total_size, &missing_ranges, &urls)?;
            let final_taken = repair.final_path.exists();
            Ok::<_, String>((repair, final_taken))
        })
        .await??
    };
    let gaps = compute_gaps(total_size, &repair.state.completed_ranges);
    let total_repair_bytes: u64 = gaps.iter().map(|r| r.len()).sum();
    let cancel = cancel_flag.as_deref();

    let mut repaired_bytes: u64 = 0;
    let mut on_bytes = |n: u64| {
        repaired_bytes += n;
        progress_callback(repaired_bytes, total_repair_bytes);
    };
    // The engine would save beside a final file that exists, and sends media and playlist URLs to
    // yt-dlp or the HLS engine.
    let engine_urls = !urls.iter().any(|u| crate::media::is_supported_media_site(u) || u.as_str().contains(".m3u8"));
    if final_taken || !engine_urls {
        return repair_in_place(&client, repair, &gaps, urls, cancel, &mut on_bytes).await;
    }

    let check = VersionCheck {
        client: &client,
        recorded: Version::recorded(&repair.state),
        at: gaps.first().map_or(0, |g| g.start),
        total_size,
        cancel,
    };
    let serving = match check.mirrors(urls).await {
        Ok(serving) => serving,
        Err(e) => {
            blocking(move || drop(repair)).await?;
            return Err(e);
        }
    };
    let Some(mirror) = serving.iter().find(|m| m.engine).map(|m| m.url.clone()) else {
        let urls: Vec<Url> = serving.into_iter().map(|m| m.url).collect();
        return repair_in_place(&client, repair, &gaps, &urls, cancel, &mut on_bytes).await;
    };
    let final_path = repair.final_path.clone();
    // The engine takes the claim itself.
    blocking(move || drop(repair)).await?;
    let recorded_hash = {
        let path = final_path.clone();
        blocking(move || {
            history_entry_for(&DownloadHistoryManager::load(), &path).filter(|e| e.blake3_hash.is_some()).cloned()
        })
        .await?
    };
    let progress = move |downloaded: u64| {
        let done = downloaded.saturating_sub(total_size - total_repair_bytes).min(total_repair_bytes);
        progress_callback(done, total_repair_bytes);
    };
    repair_with_engine(final_path.clone(), mirror.clone(), options, cancel, progress).await?;
    match recorded_hash {
        Some(recorded) => keep_recorded_hash(&check, &mirror, &final_path, recorded).await,
        None => Ok(()),
    }
}

/// A mirror that serves the version a repair expects.
struct Serving {
    url: Url,
    /// The download engine can hold this mirror to that version (see [`VersionCheck::mirror`]).
    engine: bool,
}

/// Asks mirrors for byte `at` of the `total_size`-byte file and holds their answers to the
/// version `recorded`.
struct VersionCheck<'a> {
    client: &'a Client,
    recorded: Version,
    at: u64,
    total_size: u64,
    cancel: Option<&'a AtomicBool>,
}

impl VersionCheck<'_> {
    /// Runs `attempt` until it gives anything but a transient failure, at most
    /// `ATTEMPTS_PER_MIRROR` times.
    async fn retrying<T, Fut>(&self, mut attempt: impl FnMut() -> Fut) -> Result<T, FetchError>
    where
        Fut: Future<Output = Result<T, FetchError>>,
    {
        let mut tries = 1;
        loop {
            match attempt().await {
                Err(FetchError::Retry(_)) if tries < ATTEMPTS_PER_MIRROR => {
                    tries += 1;
                    until_cancelled(tokio::time::sleep(RETRY_DELAY), self.cancel).await?;
                }
                result => return result,
            }
        }
    }

    /// The version `url` serves, once its answer shows it is byte `at` of the file in the
    /// recorded version.
    async fn serves(&self, url: &Url) -> Result<Version, FetchError> {
        let request = with_agent_for(self.client.get(url.clone()), url).header(RANGE, format!("bytes={}-{}", self.at, self.at));
        let resp = until_cancelled(request.send(), self.cancel)
            .await?
            .map_err(|e| FetchError::Retry(format!("request to {} failed: {}", url, e)))?;
        if resp.status() == StatusCode::OK {
            return Err(FetchError::Retry(format!("{} ignored the range request and sent the whole file", url)));
        }
        if resp.status() != StatusCode::PARTIAL_CONTENT {
            return Err(FetchError::Retry(format!("{} answered HTTP {} instead of 206 Partial Content", url, resp.status())));
        }
        let served = Version::served(resp.headers());
        if !self.recorded.matches(&served) {
            return Err(FetchError::WrongVersion(format!("the file at {} changed since it was downloaded", url)));
        }
        let header = resp.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok());
        match header.map(ByteRange::parse_content_range) {
            Some(Ok((range, Some(total)))) if range.start == self.at && total == self.total_size => Ok(served),
            _ => Err(FetchError::Retry(format!(
                "{} answered Content-Range {:?} for byte {} of a {}-byte file",
                url,
                header.unwrap_or("(missing)"),
                self.at,
                self.total_size
            ))),
        }
    }

    /// Whether `url` answers a range request even under an If-Range that does not match: asks for
    /// byte `at` with a validator of the kind of `validator` (ETag or date) that the file does not
    /// have. A server honoring If-Range sends the whole file instead; that body is not read.
    async fn ignores_if_range(&self, url: &Url, validator: &str) -> Result<bool, FetchError> {
        let other = if validator.starts_with('"') { "\"endo-if-range-check\"" } else { "Thu, 01 Jan 1970 00:00:00 GMT" };
        let request = with_agent_for(self.client.get(url.clone()), url)
            .header(RANGE, format!("bytes={}-{}", self.at, self.at))
            .header(IF_RANGE, other);
        let resp = until_cancelled(request.send(), self.cancel)
            .await?
            .map_err(|e| FetchError::Retry(format!("request to {} failed: {}", url, e)))?;
        match resp.status() {
            StatusCode::OK => Ok(false),
            StatusCode::PARTIAL_CONTENT => Ok(true),
            status => Err(FetchError::Retry(format!("{} answered HTTP {} to a range request", url, status))),
        }
    }

    /// Checks that `url` serves the recorded version, trying again after transient failures.
    ///
    /// The engine compares a mirror's answers with nothing but the If-Range it sends, built from
    /// the validator its probe finds: a strong ETag, else Last-Modified. It can hold a mirror to
    /// its version only if that validator exists and the mirror honors it; one that names no
    /// version at all gives no path anything to check.
    async fn mirror(&self, url: &Url) -> Result<Serving, FetchError> {
        let served = self.retrying(|| self.serves(url)).await?;
        let engine = match served.if_range() {
            Some(validator) => match self.retrying(|| self.ignores_if_range(url, validator)).await {
                Ok(ignores) => !ignores,
                Err(FetchError::Fatal(e)) => return Err(FetchError::Fatal(e)),
                Err(FetchError::Retry(e) | FetchError::WrongVersion(e)) => {
                    tracing::warn!("Could not tell whether {} honors If-Range: {}", url, e);
                    false
                }
            },
            None => served.etag.is_none() && served.last_modified.is_none(),
        };
        Ok(Serving { url: url.clone(), engine })
    }

    /// The mirrors among `urls` that serve the recorded version and the file's size. A mirror
    /// serving another version is left out at once. With none left, the error says why each was.
    async fn mirrors(&self, urls: &[Url]) -> Result<Vec<Serving>, String> {
        let mut serving = Vec::new();
        let mut reasons = Vec::new();
        for checked in futures_util::future::join_all(urls.iter().map(|url| self.mirror(url))).await {
            match checked {
                Ok(mirror) => serving.push(mirror),
                Err(FetchError::Fatal(e)) => return Err(e),
                Err(FetchError::Retry(reason) | FetchError::WrongVersion(reason)) => {
                    tracing::warn!("Not repairing from this mirror: {}", reason);
                    reasons.push(reason);
                }
            }
        }
        if serving.is_empty() {
            return Err(format!("Repair stopped: {}", reasons.join("; ")));
        }
        Ok(serving)
    }
}

/// Keeps the entry history recorded for `final_path` when it was downloaded, `recorded`, where the
/// engine recorded another hash for the repaired file. The hashes differ when bytes the repair
/// kept are damaged; the old one must then stay, so verify still shows it. They also differ when
/// the file changed on `mirror` just before the engine started: the engine then discarded the
/// `.part` and downloaded the new version whole, which the old hash would condemn. That new
/// version keeps its own hash and the repair reports the change. When the mirror cannot tell, the
/// old hash stays.
async fn keep_recorded_hash(
    check: &VersionCheck<'_>,
    mirror: &Url,
    final_path: &Path,
    recorded: HistoryEntry,
) -> Result<(), String> {
    let saved = {
        let path = final_path.to_path_buf();
        blocking(move || history_entry_for(&DownloadHistoryManager::load(), &path).and_then(|e| e.blake3_hash.clone()))
            .await?
    };
    if saved == recorded.blake3_hash {
        return Ok(());
    }
    if let Err(FetchError::WrongVersion(e)) = check.retrying(|| check.serves(mirror)).await {
        return Err(format!("{}; {} now holds the new version, downloaded in full", e, final_path.display()));
    }
    blocking(move || DownloadHistoryManager::load().add_or_update(recorded)).await
}

/// The names in the directory of `path`, or None when it cannot be read.
fn names_beside(path: &Path) -> Option<HashSet<OsString>> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::read_dir(dir).ok()?.map(|entry| entry.map(|e| e.file_name())).collect::<Result<_, _>>().ok()
}

/// Resumes the prepared `<final_path>.part` with the download engine from `mirror`, which verifies
/// the result, renames it to `final_path` and records it in history. `progress` gets the engine's
/// downloaded byte count.
///
/// The claim is free while the engine starts; if another download takes the name meanwhile, the
/// engine picks another name, which is stopped as soon as it shows. What the engine started there
/// is removed; a `.part` it resumed there is another download's saved progress and stays.
async fn repair_with_engine(
    final_path: PathBuf,
    mirror: Url,
    options: &DownloadOptions,
    cancel: Option<&AtomicBool>,
    progress: impl Fn(u64),
) -> Result<(), String> {
    let options = DownloadOptions {
        output_path: Some(final_path.clone()),
        expected_checksum: None,
        cookies_path: None,
        auth_header: None,
        proxy: None,
        media_preset: None,
        browser_cookies: None,
        ..options.clone()
    };
    let names_before = {
        let path = final_path.clone();
        blocking(move || names_beside(&path)).await?
    };
    // Building the client reads the TLS roots.
    let engine = blocking(move || DownloadEngine::new(vec![mirror], options)).await?;

    let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(16);
    let run = engine.run(Some(tx));
    tokio::pin!(run);
    let mut snapshots_open = true;
    let mut poll = tokio::time::interval(CANCEL_POLL);
    // Where the engine went instead of `final_path`, if it did.
    let mut elsewhere: Option<PathBuf> = None;
    let result = loop {
        tokio::select! {
            result = &mut run => break result,
            snapshot = rx.recv(), if snapshots_open => match snapshot {
                Ok(snapshot) => match snapshot.target_path {
                    Some(target) if target != final_path => {
                        if elsewhere.is_none() {
                            engine.cancel();
                        }
                        elsewhere = Some(target);
                    }
                    _ => progress(snapshot.downloaded_bytes),
                },
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => snapshots_open = false,
            },
            _ = poll.tick() => {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    engine.cancel();
                }
            }
        }
    };

    match (result, elsewhere) {
        (Ok(path), _) if path == final_path => Ok(()),
        (Ok(path), _) => Err(format!(
            "another download took {} while the repair was starting; a complete copy was saved as {}",
            final_path.display(),
            path.display()
        )),
        (Err(_), Some(other)) => {
            let part = with_suffix(&other, ".part");
            let started_here = part
                .file_name()
                .is_some_and(|name| names_before.as_ref().is_some_and(|before| !before.contains(name)));
            if started_here {
                if let Err(e) = blocking(move || discard_partial(&other)).await? {
                    tracing::warn!("Could not remove what the repair started under another name: {}", e);
                }
            }
            Err(format!("{} is still being downloaded; stop that download before repairing it", final_path.display()))
        }
        (Err(_), None) if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) => Err(CANCELLED.to_string()),
        (Err(e), None) => Err(e),
    }
}

/// Fetches `gaps` range by range from `urls` into the prepared `.part`, holding each mirror to the
/// version it first served (see [`fetch_range`]), then records the progress; a complete `.part`
/// whose final name is free gets that name.
async fn repair_in_place(
    client: &Client,
    mut repair: RepairTarget,
    gaps: &[ByteRange],
    urls: &[Url],
    cancel: Option<&AtomicBool>,
    on_bytes: &mut (dyn FnMut(u64) + Send),
) -> Result<(), String> {
    let total_size = repair.writer.size();
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
                fetch_range(client, &urls[mirror], &mut seen[mirror], &mut offset, range.end, &repair, cancel, on_bytes).await;
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
    use std::sync::atomic::AtomicUsize;
    use tempfile::{tempdir, NamedTempFile};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

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

    /// The lowercased head of the request on `sock`, None if the client went away first.
    async fn read_head(sock: &mut tokio::net::TcpStream) -> Option<String> {
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            match sock.read(&mut buf).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => req.extend_from_slice(&buf[..n]),
            }
        }
        Some(String::from_utf8_lossy(&req).to_ascii_lowercase())
    }

    /// The bytes a request head asks for from a `len`-byte file: its Range, cut at the end of the
    /// file, or all of it.
    fn requested(req: &str, len: u64) -> (u64, u64) {
        let range = req.lines().find_map(|l| l.strip_prefix("range: bytes=")).and_then(|spec| {
            let (s, e) = spec.trim().split_once('-')?;
            Some((s.parse().ok()?, e.parse::<u64>().map_or(len - 1, |e| e.min(len - 1))))
        });
        range.unwrap_or((0, len - 1))
    }

    const NO_HEAD: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    /// Serves `content` over HTTP, one connection at a time; `respond(n, request, start, end, content)`
    /// builds the raw response to the n-th GET from its lowercased head and the bytes it asks for.
    /// HEAD is refused, so the engine's probe goes by its ranged GET.
    async fn mock_server(
        content: Vec<u8>,
        respond: fn(usize, &str, u64, u64, &[u8]) -> Vec<u8>,
    ) -> Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut gets = 0;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let Some(req) = read_head(&mut sock).await else { continue };
                if req.starts_with("head ") {
                    let _ = sock.write_all(NO_HEAD).await;
                    continue;
                }
                let (s, e) = requested(&req, content.len() as u64);
                let resp = respond(gets, &req, s, e, &content);
                gets += 1;
                let _ = sock.write_all(&resp).await;
                let _ = sock.shutdown().await;
            }
        });
        Url::parse(&format!("http://{}/file.bin", addr)).unwrap()
    }

    /// How [`engine_server`] answers.
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        /// Every body at 16 KiB per this pause.
        Paced(Duration),
        /// The file is "v1" until the first request with If-Range "v1" (the engine's workers),
        /// and the new build "v2" from then on.
        NewBuildOnIfRange,
        /// Requests with If-Range "v1" (the engine's workers) get their headers, then nothing.
        StallIfRange,
        /// The first request from byte 0 (the engine's probe) waits for `Served::release`.
        HoldProbe,
        /// The file is the new build "v2" from the `get`-th GET on (counting from 0). The ETags
        /// are `weak`, and with `ignores_if_range` ranges are answered whatever If-Range says.
        NewBuildFrom { get: usize, weak: bool, ignores_if_range: bool },
    }

    #[derive(Default)]
    struct Served {
        gets: AtomicUsize,
        /// Ranged answers to requests with If-Range (the engine's workers) being sent.
        busy: AtomicUsize,
        /// The most of those sent at once.
        most_busy: AtomicUsize,
        probe: Notify,
        release: Notify,
    }

    /// Serves `content` as ETag "v1" to many connections at once, HEAD refused. An If-Range that
    /// does not match gets the whole file, as from any server honoring it.
    async fn engine_server(content: Vec<u8>, mode: Mode) -> (Url, Arc<Served>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = Arc::new(Served::default());
        let new_build: Vec<u8> = content.iter().map(|b| b ^ 0xff).collect();
        let (builds, stats) = (Arc::new([content, new_build]), Arc::clone(&served));
        let (held, changed) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (builds, served, held, changed) =
                    (Arc::clone(&builds), Arc::clone(&stats), Arc::clone(&held), Arc::clone(&changed));
                tokio::spawn(async move {
                    let Some(req) = read_head(&mut sock).await else { return };
                    if req.starts_with("head ") {
                        let _ = sock.write_all(NO_HEAD).await;
                        return;
                    }
                    let get = served.gets.fetch_add(1, Ordering::SeqCst);
                    let (start, end) = requested(&req, builds[0].len() as u64);
                    if mode == Mode::HoldProbe && start == 0 && !held.swap(true, Ordering::SeqCst) {
                        served.probe.notify_one();
                        served.release.notified().await;
                    }
                    let if_range = req.lines().find_map(|l| l.strip_prefix("if-range:")).map(str::trim);
                    if mode == Mode::NewBuildOnIfRange && if_range == Some("\"v1\"") {
                        changed.store(true, Ordering::SeqCst);
                    }
                    let (weak, ignores_if_range, new) = match mode {
                        Mode::NewBuildFrom { get: from, weak, ignores_if_range } => (weak, ignores_if_range, get >= from),
                        _ => (false, false, changed.load(Ordering::SeqCst)),
                    };
                    let content = &builds[usize::from(new)];
                    let etag = format!("{}\"v{}\"", if weak { "W/" } else { "" }, 1 + usize::from(new));
                    let etag_header = format!("ETag: {}", etag);
                    if if_range.is_some_and(|v| v != etag.to_ascii_lowercase()) && !ignores_if_range {
                        let _ = sock.write_all(&with_header(full_200(content), &etag_header)).await;
                        return;
                    }
                    let body = &content[start as usize..=end as usize];
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\n{}\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        etag_header,
                        start,
                        end,
                        content.len(),
                        body.len()
                    );
                    let worker = if_range.is_some();
                    if worker {
                        let now = served.busy.fetch_add(1, Ordering::SeqCst) + 1;
                        served.most_busy.fetch_max(now, Ordering::SeqCst);
                    }
                    if sock.write_all(head.as_bytes()).await.is_ok() {
                        if mode == Mode::StallIfRange && worker {
                            std::future::pending::<()>().await;
                        }
                        for piece in body.chunks(16 * 1024) {
                            if sock.write_all(piece).await.is_err() {
                                break;
                            }
                            if let Mode::Paced(pause) = mode {
                                tokio::time::sleep(pause).await;
                            }
                        }
                    }
                    if worker {
                        served.busy.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        });
        (Url::parse(&format!("http://{}/big.bin", addr)).unwrap(), served)
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
        let res = repair_missing_ranges(&path, 1000, &[gap], &[url], &DownloadOptions::default(), None, |_, _| {}).await;
        assert!(res.is_err());

        // The unfinished repair is left as a resumable .part, never under the final name.
        assert!(!path.exists() && !DownloadState::state_file_path(&path).exists());
        let on_disk = std::fs::read(part_of(&path)).unwrap();
        assert_eq!(&on_disk[200..], &content[200..], "good data was overwritten");
        assert!(on_disk[100..200].iter().all(|&b| b == 0));
        assert_eq!(gaps_on_disk(&part_of(&path)), vec![gap]);
    }

    /// `damaged_file` recorded as ETag `etag`, moved to `build.bin.part` beside a finished
    /// `build.bin` with other contents, so its repair cannot give it that name. Returns the `.part`.
    fn damaged_part_beside_final(dir: &Path, content: &[u8], etag: Option<&str>) -> PathBuf {
        let path = damaged_file(dir, content);
        let part = part_of(&path);
        let mut state = DownloadState::load_from_path(&DownloadState::state_file_path(&path)).unwrap().unwrap();
        state.etag = etag.map(str::to_string);
        state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();
        DownloadState::remove(&DownloadState::state_file_path(&path)).unwrap();
        std::fs::rename(&path, &part).unwrap();
        std::fs::write(&path, b"a newer finished download").unwrap();
        part
    }

    // Was `repair_records_only_bytes_that_arrived`: through the engine, a server that sends the
    // whole file once it ignores ranges now completes the repair with that file. The range by range
    // path this checks is now the one taken beside a finished file.
    #[tokio::test]
    async fn in_place_repair_records_only_bytes_that_arrived() {
        let dir = tempdir().unwrap();
        let content = content();
        let part = damaged_part_beside_final(dir.path(), &content, None);
        // First answer is cut off after 30 bytes, every later one ignores Range.
        let url = mock_server(content.clone(), |attempt, _, s, e, c| {
            if attempt == 0 { partial(s, e, c, 30) } else { full_200(c) }
        })
        .await;

        let res = repair_missing_ranges(&part, 1000, &[ByteRange::new(100, 199).unwrap()], &[url], &DownloadOptions::default(), None, |_, _| {}).await;
        assert!(res.is_err());

        assert_eq!(gaps_on_disk(&part), vec![ByteRange::new(130, 199).unwrap()]);
        assert_eq!(std::fs::read(&part).unwrap()[100..130], content[100..130]);
        assert_eq!(std::fs::read(dir.path().join("build.bin")).unwrap(), b"a newer finished download");
    }

    #[tokio::test]
    async fn a_part_beside_a_finished_file_is_repaired_in_place() {
        let dir = tempdir().unwrap();
        let content = content();
        let part = damaged_part_beside_final(dir.path(), &content, None);
        let url = mock_server(content.clone(), |_, _, s, e, c| partial(s, e, c, usize::MAX)).await;

        let res = verify_with_history(&part, None, None, &empty_history()).unwrap();
        repair_missing_ranges(&part, 1000, &res.missing_ranges, &[url], &DownloadOptions::default(), None, |_, _| {}).await.unwrap();

        assert_eq!(std::fs::read(&part).unwrap(), content);
        assert!(gaps_on_disk(&part).is_empty());
        assert_eq!(std::fs::read(dir.path().join("build.bin")).unwrap(), b"a newer finished download");
    }

    #[tokio::test]
    async fn failed_repair_of_truncated_final_file_leaves_resumable_part() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = dir.path().join("build.bin");
        std::fs::write(&path, &content[..500]).unwrap();
        let url = mock_server(content.clone(), |_, _, _, _, c| full_200(c)).await;

        let res = verify_with_history(&path, Some(1000), None, &empty_history()).unwrap();
        let repaired = repair_missing_ranges(&path, 1000, &res.missing_ranges, std::slice::from_ref(&url), &DownloadOptions::default(), None, |_, _| {}).await;
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
        repair_missing_ranges(&path, 1000, &res.missing_ranges, &[url], &DownloadOptions::default(), None, |_, _| {}).await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), content);
        let part = part_of(&path);
        assert!(!part.exists());
        assert!(!DownloadState::state_file_path(&part).exists());
        assert!(!DownloadState::state_file_path(&path).exists());
    }

    // Was `repair_sends_if_range_and_stops_when_the_file_changes` on a file at its final name, which
    // now goes through the engine (see `engine_repair_stops_when_the_file_changes`).
    #[tokio::test]
    async fn in_place_repair_sends_if_range_and_stops_when_the_file_changes() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = damaged_part_beside_final(dir.path(), &content, Some("\"v1\""));
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
        let res = repair_missing_ranges(&path, 1000, &[gap], &[url], &DownloadOptions::default(), None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("changed")), "{:?}", res);

        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk[100..140], content[100..140]);
        assert!(on_disk[140..200].iter().all(|&b| b == 0), "no byte of the new build may be written");
        assert_eq!(gaps_on_disk(&path), vec![ByteRange::new(140, 199).unwrap()]);
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
        repair_missing_ranges(&path, 1000, &gaps, &[a, b], &DownloadOptions::default(), None, |_, _| {}).await.unwrap();
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

        repair_missing_ranges(&path, 1000, &[ByteRange::new(100, 199).unwrap()], &[a, b], &DownloadOptions::default(), None, |_, _| {}).await.unwrap();
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
        let res = repair_missing_ranges(&path, 1000, &[gap], &[url], &DownloadOptions::default(), None, |_, _| {}).await;
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
        repair_missing_ranges(&path, 1000, &res.missing_ranges, &[url], &DownloadOptions::default(), None, |_, _| {}).await.unwrap();

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
            repair_missing_ranges(&repaired, 1000, &[ByteRange::new(100, 199).unwrap()], &[url], &DownloadOptions::default(), Some(cancel), |_, _| {}).await
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
        let res = repair_missing_ranges(&path, 1000, &[gap], std::slice::from_ref(&url), &DownloadOptions::default(), None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("still being downloaded")), "{:?}", res);
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(std::fs::read(&state_path).unwrap(), state);
        assert!(!part_of(&path).exists());
        drop(download);

        repair_missing_ranges(&path, 1000, &[gap], &[url], &DownloadOptions::default(), None, |_, _| {}).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), content);
        assert!(claim_target(&path).unwrap().is_some(), "a finished repair must release the claim");
    }

    #[tokio::test]
    async fn failed_repair_releases_the_claim() {
        let dir = tempdir().unwrap();
        let path = damaged_file(dir.path(), &content());
        let url = mock_server(content(), |_, _, _, _, c| full_200(c)).await;
        let gap = ByteRange::new(100, 199).unwrap();
        assert!(repair_missing_ranges(&path, 1000, &[gap], &[url], &DownloadOptions::default(), None, |_, _| {}).await.is_err());
        assert!(claim_target(&path).unwrap().is_some());

        // Also when the repair fails before it starts: the .part it would write is not its own.
        std::fs::write(dir.path().join("fresh.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.path().join("fresh.bin.part"), b"other").unwrap();
        let fresh = dir.path().join("fresh.bin");
        let res = repair_missing_ranges(&fresh, 1000, &[gap], &[Url::parse("http://127.0.0.1:9/").unwrap()], &DownloadOptions::default(), None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("refusing to overwrite")), "{:?}", res);
        assert!(claim_target(&fresh).unwrap().is_some());
    }

    #[tokio::test]
    async fn repair_never_requests_a_link_saved_without_its_secret() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = damaged_file(dir.path(), &content);
        let data = std::fs::read(&path).unwrap();
        let gap = ByteRange::new(100, 199).unwrap();
        let redacted = Url::parse("http://127.0.0.1:9/file.bin?token=REDACTED").unwrap();
        let res = repair_missing_ranges(&path, 1000, &[gap], std::slice::from_ref(&redacted), &DownloadOptions::default(), None, |_, _| {}).await;
        assert_eq!(res, Err(REDACTED_LINK.to_string()));
        assert_eq!(std::fs::read(&path).unwrap(), data);

        let url = mock_server(content.clone(), |_, _, s, e, c| partial(s, e, c, usize::MAX)).await;
        repair_missing_ranges(&path, 1000, &[gap], &[redacted, url], &DownloadOptions::default(), None, |_, _| {}).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), content);
    }

    const MIB: usize = 1024 * 1024;

    /// A 5 MiB file downloaded as ETag "v1" of which only the first MiB arrived (see `big_gap`).
    fn damaged_big(dir: &Path) -> (PathBuf, Vec<u8>) {
        let content: Vec<u8> = (0..5 * MIB).map(|i| (i % 251) as u8 ^ (i >> 12) as u8).collect();
        let path = dir.join("big.bin");
        let mut data = content.clone();
        data[MIB..].fill(0);
        std::fs::write(&path, &data).unwrap();
        let mut state = DownloadState::new("big.bin".into(), content.len() as u64, MIB as u64, vec![]);
        state.completed_ranges = vec![ByteRange::from_len(0, MIB as u64).unwrap()];
        state.etag = Some("\"v1\"".into());
        state.save_atomic(&DownloadState::state_file_path(&path)).unwrap();
        (path, content)
    }

    fn big_gap() -> ByteRange {
        ByteRange::new(MIB as u64, 5 * MIB as u64 - 1).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repair_fetches_the_gaps_over_several_connections() {
        let dir = tempdir().unwrap();
        let (path, content) = damaged_big(dir.path());
        let (url, served) = engine_server(content.clone(), Mode::Paced(Duration::from_millis(5))).await;
        let options = DownloadOptions { num_connections: 4, ..DownloadOptions::default() };

        repair_missing_ranges(&path, content.len() as u64, &[big_gap()], &[url], &options, None, |_, _| {}).await.unwrap();

        assert!(std::fs::read(&path).unwrap() == content, "repaired file differs");
        assert!(!part_of(&path).exists() && !DownloadState::state_file_path(&part_of(&path)).exists());
        let most = served.most_busy.load(Ordering::SeqCst);
        assert!(most >= 2, "the missing 4 MiB came over {} connection(s) at a time", most);
    }

    /// `damaged_big` at `path` after a repair that stopped: still a `.part` missing `big_gap()`
    /// that holds no byte of another build, and verify does not pass it.
    fn assert_repair_stopped(path: &Path, content: &[u8]) {
        assert!(!path.exists());
        let on_disk = std::fs::read(part_of(path)).unwrap();
        assert!(on_disk[..MIB] == content[..MIB]);
        assert!(on_disk[MIB..].iter().all(|&b| b == 0), "no byte of the new build may be written");
        assert_eq!(gaps_on_disk(&part_of(path)), vec![big_gap()]);
        let res = verify_build_file(path, None, None).unwrap();
        assert!(!res.is_complete, "{}", res.status_message);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn engine_repair_stops_when_the_file_changes() {
        let dir = tempdir().unwrap();
        let (path, content) = damaged_big(dir.path());
        let (url, _) = engine_server(content.clone(), Mode::NewBuildOnIfRange).await;

        let res = repair_missing_ranges(&path, content.len() as u64, &[big_gap()], &[url], &DownloadOptions::default(), None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("changed")), "{:?}", res);
        assert_repair_stopped(&path, &content);
    }

    /// The engine checks a ranged answer only through the If-Range it sent, so a server ignoring
    /// If-Range could hand it the ranges of a new build. Such a server is repaired from range by
    /// range, every answer checked.
    #[tokio::test(flavor = "multi_thread")]
    async fn repair_never_mixes_versions_from_a_server_ignoring_if_range() {
        let dir = tempdir().unwrap();
        let (path, content) = damaged_big(dir.path());
        // The new build appears after the repair's two checks.
        let mode = Mode::NewBuildFrom { get: 2, weak: false, ignores_if_range: true };
        let (url, _) = engine_server(content.clone(), mode).await;

        let res = repair_missing_ranges(&path, content.len() as u64, &[big_gap()], &[url], &DownloadOptions::default(), None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("changed")), "{:?}", res);
        assert_repair_stopped(&path, &content);
    }

    /// A weak ETag may not go into If-Range, so the engine would send none and could not tell a
    /// new build from the old one at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn repair_never_mixes_versions_behind_a_weak_etag() {
        let dir = tempdir().unwrap();
        let (path, content) = damaged_big(dir.path());
        let state_path = DownloadState::state_file_path(&path);
        let mut state = DownloadState::load_from_path(&state_path).unwrap().unwrap();
        state.etag = Some("W/\"v1\"".into());
        state.save_atomic(&state_path).unwrap();
        // The new build appears after the repair's check.
        let mode = Mode::NewBuildFrom { get: 1, weak: true, ignores_if_range: false };
        let (url, _) = engine_server(content.clone(), mode).await;

        let res = repair_missing_ranges(&path, content.len() as u64, &[big_gap()], &[url], &DownloadOptions::default(), None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("changed")), "{:?}", res);
        assert_repair_stopped(&path, &content);
    }

    /// The file changes on the server after the repair's checks but before the engine starts,
    /// which then downloads the new build whole instead of resuming. The hash history recorded for
    /// the old build must not condemn that file, and the repair says what happened.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_build_the_engine_downloads_whole_keeps_its_own_hash() {
        let dir = tempdir().unwrap();
        let (path, content) = damaged_big(dir.path());
        let mode = Mode::NewBuildFrom { get: 2, weak: false, ignores_if_range: false };
        let (url, _) = engine_server(content.clone(), mode).await;
        let size = content.len() as u64;
        let mut entry = HistoryEntry::new("big.bin".into(), std::path::absolute(&path).unwrap(), size, vec![url.to_string()]);
        entry.blake3_hash = Some(blake3::hash(&content).to_hex().to_string());
        DownloadHistoryManager::load().add_or_update(entry);

        let res = repair_missing_ranges(&path, size, &[big_gap()], &[url], &DownloadOptions::default(), None, |_, _| {}).await;
        assert!(res.as_ref().is_err_and(|e| e.contains("changed") && e.contains("new version")), "{:?}", res);
        let new_build: Vec<u8> = content.iter().map(|b| b ^ 0xff).collect();
        assert!(std::fs::read(&path).unwrap() == new_build, "the engine saved something else");
        let res = verify_build_file(&path, None, None).unwrap();
        assert!(res.is_complete && res.checksum_match == Some(true), "{}", res.status_message);
    }

    /// Checked only against each other, a mirror serving another version would lead the engine
    /// when listed first, and the repair would fetch that version instead.
    #[tokio::test]
    async fn repair_leaves_out_a_mirror_serving_another_version() {
        let dir = tempdir().unwrap();
        let content = content();
        let path = damaged_twice(dir.path(), &content);
        let other_build: Vec<u8> = content.iter().map(|b| b ^ 0xff).collect();
        let other = mock_server(other_build, |_, _, s, e, c| {
            with_header(partial(s, e, c, usize::MAX), "Last-Modified: Tue, 07 Nov 2023 08:49:37 GMT")
        })
        .await;
        let good = mock_server(content.clone(), |_, _, s, e, c| {
            with_header(with_header(partial(s, e, c, usize::MAX), "ETag: \"v1\""), &format!("Last-Modified: {MON}"))
        })
        .await;

        let gaps = [ByteRange::new(100, 199).unwrap(), ByteRange::new(500, 599).unwrap()];
        repair_missing_ranges(&path, 1000, &gaps, &[other, good], &DownloadOptions::default(), None, |_, _| {}).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), content);
    }

    /// The engine takes the name's claim itself; if a download takes it first, the engine would
    /// fetch the file again under another name. That is stopped and cleaned up.
    #[tokio::test(flavor = "multi_thread")]
    async fn repair_never_continues_under_another_name() {
        let dir = tempdir().unwrap();
        let (path, content) = damaged_big(dir.path());
        let (url, served) = engine_server(content.clone(), Mode::HoldProbe).await;
        let repair = {
            let (path, total) = (path.clone(), content.len() as u64);
            tokio::spawn(async move {
                repair_missing_ranges(&path, total, &[big_gap()], &[url], &DownloadOptions::default(), None, |_, _| {}).await
            })
        };
        tokio::time::timeout(Duration::from_secs(20), served.probe.notified()).await.expect("the engine never probed");
        let download = claim_target(&path).unwrap().expect("the repair left the claim to the engine");
        served.release.notify_one();

        let res = repair.await.unwrap();
        assert!(res.as_ref().is_err_and(|e| e.contains("still being downloaded")), "{:?}", res);
        let mut left: Vec<String> =
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["big.bin.part", "big.bin.part.hfstate", "big.bin.part.lock"]);
        assert_eq!(gaps_on_disk(&part_of(&path)), vec![big_gap()]);
        drop(download);
    }

    /// Under another name the engine may resume a `.part` that another download left there, here
    /// a paused download of the same file. Stopping the engine must leave that progress alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn repair_keeps_the_progress_it_found_under_another_name() {
        let dir = tempdir().unwrap();
        let (path, content) = damaged_big(dir.path());
        let (url, served) = engine_server(content.clone(), Mode::HoldProbe).await;
        let paused = dir.path().join("big (1).bin.part");
        let mut data = content.clone();
        data[2 * MIB..].fill(0);
        std::fs::write(&paused, &data).unwrap();
        let mut state = DownloadState::new("big (1).bin".into(), content.len() as u64, MIB as u64, vec![url.to_string()]);
        state.completed_ranges = vec![ByteRange::from_len(0, 2 * MIB as u64).unwrap()];
        state.etag = Some("\"v1\"".into());
        state.save_atomic(&DownloadState::state_file_path(&paused)).unwrap();

        let repair = {
            let (path, total) = (path.clone(), content.len() as u64);
            tokio::spawn(async move {
                repair_missing_ranges(&path, total, &[big_gap()], &[url], &DownloadOptions::default(), None, |_, _| {}).await
            })
        };
        tokio::time::timeout(Duration::from_secs(20), served.probe.notified()).await.expect("the engine never probed");
        let download = claim_target(&path).unwrap().expect("the repair left the claim to the engine");
        served.release.notify_one();

        let res = repair.await.unwrap();
        assert!(res.as_ref().is_err_and(|e| e.contains("still being downloaded")), "{:?}", res);
        assert!(std::fs::read(&paused).unwrap()[..2 * MIB] == content[..2 * MIB]);
        let gaps = gaps_on_disk(&paused);
        assert!(gaps.iter().all(|g| g.start >= 2 * MIB as u64), "the paused download lost its progress: {:?}", gaps);
        assert_eq!(gaps_on_disk(&part_of(&path)), vec![big_gap()]);
        drop(download);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn engine_repair_honors_cancel() {
        let dir = tempdir().unwrap();
        let (path, content) = damaged_big(dir.path());
        let (url, served) = engine_server(content.clone(), Mode::StallIfRange).await;
        let cancel = Arc::new(AtomicBool::new(false));
        let repair = {
            let (path, total, flag) = (path.clone(), content.len() as u64, Arc::clone(&cancel));
            tokio::spawn(async move {
                repair_missing_ranges(&path, total, &[big_gap()], &[url], &DownloadOptions::default(), Some(flag), |_, _| {}).await
            })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while served.busy.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "the engine never started fetching");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(claim_target(&path).unwrap().is_none(), "the engine holds the claim while it runs");
        let stopping = std::time::Instant::now();
        cancel.store(true, Ordering::Relaxed);

        assert_eq!(repair.await.unwrap(), Err(CANCELLED.to_string()));
        assert!(stopping.elapsed() < Duration::from_secs(10));
        assert!(!path.exists());
        assert_eq!(gaps_on_disk(&part_of(&path)), vec![big_gap()], "the .part stays resumable");
        assert!(claim_target(&path).unwrap().is_some(), "a cancelled repair releases the claim");
    }

    /// The engine records the repaired file's hash in history. A hash recorded when the file was
    /// downloaded stays instead: damage in bytes the repair kept must still show.
    #[tokio::test(flavor = "multi_thread")]
    async fn repair_keeps_the_hash_history_recorded() {
        let dir = tempdir().unwrap();
        let (_, content) = damaged_big(dir.path());
        let (url, _) = engine_server(content.clone(), Mode::Paced(Duration::ZERO)).await;
        // Cut off after 3 MiB, and damaged past the first MiB, which the engine fetches anyway.
        let path = dir.path().join("cut.bin");
        let mut truncated = content[..3 * MIB].to_vec();
        truncated[2 * MIB] ^= 0xff;
        std::fs::write(&path, &truncated).unwrap();
        let original = blake3::hash(&content).to_hex().to_string();
        let size = content.len() as u64;
        let mut entry = HistoryEntry::new("cut.bin".into(), std::path::absolute(&path).unwrap(), size, vec![url.to_string()]);
        entry.blake3_hash = Some(original.clone());
        DownloadHistoryManager::load().add_or_update(entry);

        let res = verify_build_file(&path, None, None).unwrap();
        assert_eq!(res.missing_ranges, vec![ByteRange::new(3 * MIB as u64, size - 1).unwrap()]);
        repair_missing_ranges(&path, size, &res.missing_ranges, &[url], &DownloadOptions::default(), None, |_, _| {}).await.unwrap();

        let recorded = history_entry_for(&DownloadHistoryManager::load(), &path).and_then(|e| e.blake3_hash.clone());
        assert_eq!(recorded, Some(original));
        let res = verify_build_file(&path, None, None).unwrap();
        assert_eq!(res.checksum_match, Some(false), "{}", res.status_message);
    }
}
