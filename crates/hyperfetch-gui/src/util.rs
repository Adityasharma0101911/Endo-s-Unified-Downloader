use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use hyperfetch_core::history::{DownloadHistoryManager, HistoryEntry};
use hyperfetch_core::state::DownloadState;
use hyperfetch_core::torrent::{is_magnet_uri, parse_magnet_uri};
use hyperfetch_core::verify::BuildVerificationResult;
use url::Url;

pub const BLOB_MESSAGE: &str = "Browser-internal blob: URLs exist only in the browser's memory and cannot be downloaded by external tools. Copy the page URL from the address bar instead (e.g. https://www.youtube.com/watch?v=...).";

/// Locks a mutex, recovering the data if another thread panicked while holding it.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A `blob:` URL, or a YouTube URL ending in a blob UUID pasted without its prefix.
pub fn is_blob_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("blob:")
        || (s.contains("youtube.com")
            && s.rsplit('/').next().is_some_and(|last| last.len() == 36 && last.matches('-').count() == 4))
}

/// Parses the mirrors of one file: whitespace-separated http(s) URLs and magnet links with
/// HTTP web seeds.
pub fn parse_urls(input: &str) -> Result<Vec<Url>, String> {
    let mut urls: Vec<Url> = Vec::new();
    for token in input.split_whitespace() {
        if is_blob_url(token) {
            return Err(BLOB_MESSAGE.to_string());
        }
        let found = if is_magnet_uri(token) {
            let magnet = parse_magnet_uri(token).map_err(|e| format!("Invalid magnet link: {}", e))?;
            if magnet.web_seeds.is_empty() {
                let name = magnet.display_name.map(|n| format!(" for \"{}\"", n)).unwrap_or_default();
                return Err(format!(
                    "The magnet link{} has no HTTP web seeds (ws=). Peer-to-peer BitTorrent transfers are not supported, so it cannot be downloaded.",
                    name
                ));
            }
            magnet.web_seeds
        } else {
            let url = Url::parse(token).map_err(|e| format!("Invalid URL '{}': {}", truncate_chars(token, 80), e))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(format!("Unsupported URL scheme '{}': only http and https can be downloaded", url.scheme()));
            }
            vec![url]
        };
        for url in found {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    if urls.is_empty() {
        return Err("Enter a download URL".to_string());
    }
    Ok(urls)
}

/// Clipboard text worth offering as a download: one downloadable link.
pub fn clipboard_link(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.len() > 8192 || text.contains(char::is_whitespace) {
        return None;
    }
    parse_urls(text).ok().map(|_| text.to_string())
}

/// At most `max` characters, ending in "..." when shortened. Never splits a character.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// `path` without a trailing `.part`: the final name of an in-progress download.
pub fn final_path_of(path: &Path) -> PathBuf {
    match path.extension() {
        Some(ext) if ext == "part" => path.with_extension(""),
        _ => path.to_path_buf(),
    }
}

/// The files an unfinished download of `final_path` leaves behind; never the final file itself.
pub fn leftover_paths(final_path: &Path) -> [PathBuf; 3] {
    let part = with_suffix(final_path, ".part");
    [with_suffix(&part, ".hfstate"), with_suffix(&part, ".hlsstate"), part]
}

/// Deletes the partial file and resume state of `final_path` and returns how many files were
/// removed. The `.part` goes first so a failure leaves the download resumable. Blocking.
pub fn delete_leftovers(final_path: &Path) -> Result<usize, String> {
    let [hfstate, hlsstate, part] = leftover_paths(final_path);
    let mut removed = 0;
    for path in [part, hfstate, hlsstate] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Could not delete {}: {}", path.display(), e)),
        }
    }
    Ok(removed)
}

/// Seconds left at the current speed; `None` while stalled (under 1 KiB/s) or when the size is unknown.
pub fn eta_secs(total: u64, downloaded: u64, speed: f64) -> Option<u64> {
    (speed >= 1024.0 && total > downloaded).then(|| ((total - downloaded) as f64 / speed).ceil() as u64)
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

pub fn format_duration(seconds: u64) -> String {
    let hrs = seconds / 3600;
    let mins = (seconds % 3600) / 60;
    let secs = seconds % 60;

    if hrs > 0 {
        format!("{:02}:{:02}:{:02}", hrs, mins, secs)
    } else {
        format!("{:02}:{:02}", mins, secs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Verified,
    /// Missing ranges that a repair can fetch.
    Incomplete,
    /// Every byte is present but the checksum does not match.
    Mismatch,
    /// No evidence either way (or an oversized file): nothing to repair.
    Unverified,
}

pub fn verdict(result: &BuildVerificationResult) -> Verdict {
    if result.is_complete {
        Verdict::Verified
    } else if !result.missing_ranges.is_empty() {
        Verdict::Incomplete
    } else if result.checksum_match == Some(false) {
        Verdict::Mismatch
    } else {
        Verdict::Unverified
    }
}

/// URLs recorded in history for exactly `final_path` (never matched by file name alone).
pub fn history_urls_for(entries: &[HistoryEntry], final_path: &Path) -> Option<Vec<String>> {
    let wanted = std::path::absolute(final_path).ok()?;
    entries
        .iter()
        .find(|e| std::path::absolute(&e.file_path).is_ok_and(|p| p == wanted))
        .map(|e| e.urls.clone())
}

/// Mirrors to repair `target` from: its own resume state, else the history entry for exactly
/// its final path. Blocking.
pub fn repair_urls_for(target: &Path) -> Vec<Url> {
    let from_state = DownloadState::load_from_path(&DownloadState::state_file_path(target))
        .ok()
        .flatten()
        .map(|state| state.mirrors);
    from_state
        .or_else(|| history_urls_for(DownloadHistoryManager::load().entries(), &final_path_of(target)))
        .unwrap_or_default()
        .iter()
        .filter_map(|u| Url::parse(u).ok())
        .collect()
}

/// Opens a file or folder with its default application. Arguments go straight to the
/// program, never through a shell.
pub fn open_path(path: &Path) -> std::io::Result<()> {
    let path = std::path::absolute(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Windows paths cannot contain '"', so quoting keeps commas and spaces inside the one argument.
        let mut arg = OsString::from("\"");
        arg.push(&path);
        arg.push("\"");
        Command::new("explorer").raw_arg(arg).spawn().map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(&path).spawn().map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(&path).spawn().map(|_| ())
    }
}

/// Shows `path` selected in the system file manager (its folder where selecting isn't supported).
pub fn reveal_in_folder(path: &Path) -> std::io::Result<()> {
    let path = std::path::absolute(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut arg = OsString::from("/select,\"");
        arg.push(&path);
        arg.push("\"");
        Command::new("explorer").raw_arg(arg).spawn().map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg("-R").arg(&path).spawn().map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(path.parent().unwrap_or(path.as_path())).spawn().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_urls_accepts_mirrors_and_rejects_bad_input() {
        let urls = parse_urls("  https://a.com/f.iso\thttp://b.com/f.iso https://a.com/f.iso ").unwrap();
        assert_eq!(urls.len(), 2, "duplicates are dropped");
        assert!(parse_urls("   ").is_err());
        assert!(parse_urls("ftp://a.com/f").unwrap_err().contains("scheme"));
        assert!(parse_urls("not a url").unwrap_err().contains("Invalid URL"));
        assert_eq!(parse_urls("blob:https://www.youtube.com/x").unwrap_err(), BLOB_MESSAGE);
        assert!(parse_urls("https://www.youtube.com/0b9f5e2c-1d3a-4c5e-9f7a-123456789abc").is_err());
    }

    #[test]
    fn magnets_need_web_seeds() {
        let hash = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";
        let err = parse_urls(&format!("magnet:?xt=urn:btih:{}&dn=Ubuntu", hash)).unwrap_err();
        assert!(err.contains("\"Ubuntu\"") && err.contains("web seeds"), "{}", err);
        let seeded = format!("magnet:?xt=urn:btih:{}&dn=f.iso&ws=https%3A%2F%2Fmirror.example%2Ff.iso", hash);
        assert_eq!(parse_urls(&seeded).unwrap(), vec![Url::parse("https://mirror.example/f.iso").unwrap()]);
        assert!(clipboard_link(&seeded).is_some());
        assert!(clipboard_link(&format!("magnet:?xt=urn:btih:{}", hash)).is_none());
    }

    #[test]
    fn clipboard_link_only_takes_single_links() {
        let long = format!("https://ja.wikipedia.org/wiki/{}", "東京都の区市町村".repeat(4));
        assert_eq!(clipboard_link(&format!("  {}\n", long)), Some(long));
        assert_eq!(clipboard_link("see https://a.com/x"), None);
        assert_eq!(clipboard_link("https://a.com/x https://b.com/x"), None);
        assert_eq!(clipboard_link("hello"), None);
        assert_eq!(clipboard_link("file:///etc/passwd"), None);
    }

    #[test]
    fn truncate_chars_never_splits_characters() {
        let url = format!("https://ja.wikipedia.org/wiki/{}", "東京都の区市町村".repeat(5));
        let short = truncate_chars(&url, 55);
        assert_eq!(short.chars().count(), 55);
        assert!(short.ends_with("..."));
        assert_eq!(truncate_chars("héllo", 5), "héllo");
        assert_eq!(truncate_chars("héllo!", 5), "hé...");
    }

    #[test]
    fn delete_leftovers_only_touches_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("setup.exe");
        let keep = [final_path.clone(), dir.path().join("setup (1).exe"), dir.path().join("setup.exe.hfstate")];
        let [hfstate, hlsstate, part] = leftover_paths(&final_path);
        for p in keep.iter().chain([&part, &hfstate, &hlsstate]) {
            std::fs::write(p, b"x").unwrap();
        }
        assert_eq!(delete_leftovers(&final_path), Ok(3));
        assert!(keep.iter().all(|p| p.exists()));
        assert!(!part.exists() && !hfstate.exists() && !hlsstate.exists());
        assert_eq!(delete_leftovers(&final_path), Ok(0), "nothing left to delete");
        assert_eq!(part.file_name().unwrap(), "setup.exe.part");
        assert_eq!(hfstate.file_name().unwrap(), "setup.exe.part.hfstate");
        assert_eq!(hlsstate.file_name().unwrap(), "setup.exe.part.hlsstate");
    }

    #[test]
    fn final_path_strips_only_part() {
        assert_eq!(final_path_of(Path::new("d/a.iso.part")), PathBuf::from("d/a.iso"));
        assert_eq!(final_path_of(Path::new("d/a.iso")), PathBuf::from("d/a.iso"));
    }

    #[test]
    fn eta_clears_when_stalled_or_unknown() {
        assert_eq!(eta_secs(10_000, 0, 2048.0), Some(5));
        assert_eq!(eta_secs(10_000, 0, 1000.0), None);
        assert_eq!(eta_secs(0, 500, 4096.0), None);
        assert_eq!(eta_secs(100, 100, 4096.0), None);
    }

    fn result(is_complete: bool, missing: bool, checksum_match: Option<bool>) -> BuildVerificationResult {
        BuildVerificationResult {
            file_path: PathBuf::from("f"),
            expected_size: Some(10),
            actual_size: 10,
            has_state_file: false,
            missing_ranges: if missing { vec![hyperfetch_core::ByteRange { start: 0, end: 4 }] } else { vec![] },
            is_complete,
            checksum_match,
            status_message: String::new(),
        }
    }

    #[test]
    fn verdict_only_offers_repair_for_missing_ranges() {
        assert_eq!(verdict(&result(true, false, Some(true))), Verdict::Verified);
        assert_eq!(verdict(&result(false, true, None)), Verdict::Incomplete);
        assert_eq!(verdict(&result(false, false, Some(false))), Verdict::Mismatch);
        assert_eq!(verdict(&result(false, false, None)), Verdict::Unverified);
    }

    #[test]
    fn history_urls_match_the_exact_path_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut other = HistoryEntry::new("setup.exe".into(), dir.path().join("vendor-a").join("setup.exe"), 1, vec!["https://a/setup.exe".into()]);
        other.id = "a".into();
        let mine = HistoryEntry::new("setup.exe".into(), dir.path().join("setup.exe"), 1, vec!["https://b/setup.exe".into()]);
        let entries = [other, mine];
        assert_eq!(history_urls_for(&entries, &dir.path().join("setup.exe")), Some(vec!["https://b/setup.exe".to_string()]));
        assert_eq!(history_urls_for(&entries, &dir.path().join("elsewhere").join("setup.exe")), None);
    }
}
