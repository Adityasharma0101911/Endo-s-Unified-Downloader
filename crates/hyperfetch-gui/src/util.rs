#[cfg(windows)]
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use hyperfetch_core::history::{is_redacted, redact_url, DownloadHistoryManager, HistoryEntry, REDACTED_LINK};
use hyperfetch_core::ingest;
use hyperfetch_core::queue::QueueItem;
use hyperfetch_core::state::DownloadState;
use hyperfetch_core::verify::BuildVerificationResult;
use url::Url;

/// Locks a mutex, recovering the data if another thread panicked while holding it.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clipboard text worth offering as a download: one downloadable link (not a .metalink or
/// .torrent, which lists several).
pub fn clipboard_link(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.len() > 8192 || text.contains(char::is_whitespace) || ingest::names_document(text) {
        return None;
    }
    ingest::link_task(&[text]).ok().map(|_| text.to_string())
}

/// The folder to create before `item` starts, since the engine takes a missing folder for a file
/// name: its output path, unless that is the file itself (a download a metalink, torrent or
/// magnet named; the engine creates its folders).
pub fn folder_to_create(item: &QueueItem) -> Option<PathBuf> {
    item.options.output_path.clone().filter(|_| !item.names_file)
}

/// `path` without a trailing `.part`: the final name of an in-progress download.
pub fn final_path_of(path: &Path) -> PathBuf {
    match path.extension() {
        Some(ext) if ext == "part" => path.with_extension(""),
        _ => path.to_path_buf(),
    }
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

/// What the history search box matches against saved links: a pasted link in the form history
/// saves it, without secrets, lower case.
pub fn history_search_key(input: &str) -> String {
    redact_url(input.trim()).to_lowercase()
}

/// The links to download a history entry again, for the link box, or why they cannot be used:
/// links saved without their secret are left out.
pub fn redownload_input(urls: &[String]) -> Result<String, String> {
    let usable: Vec<&str> = urls.iter().map(String::as_str).filter(|u| !is_redacted(u)).collect();
    if usable.is_empty() && !urls.is_empty() {
        return Err(REDACTED_LINK.to_string());
    }
    Ok(usable.join(" "))
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
/// its final path (see [`repair_mirrors`]). Blocking.
pub fn repair_urls_for(target: &Path) -> Result<Vec<Url>, String> {
    let from_state = DownloadState::load_from_path(&DownloadState::state_file_path(target))
        .ok()
        .flatten()
        .map(|state| state.mirrors);
    repair_mirrors(
        &from_state
            .or_else(|| history_urls_for(DownloadHistoryManager::load().entries(), &final_path_of(target)))
            .unwrap_or_default(),
    )
}

/// The links among `urls` a repair can request: links saved without their secret are left out,
/// and when that leaves none, the error says to give the link again.
pub fn repair_mirrors(urls: &[String]) -> Result<Vec<Url>, String> {
    let usable: Vec<Url> = urls.iter().filter(|u| !is_redacted(u)).filter_map(|u| Url::parse(u).ok()).collect();
    if usable.is_empty() && urls.iter().any(|u| is_redacted(u)) {
        return Err(REDACTED_LINK.to_string());
    }
    Ok(usable)
}

/// Starts `command` without waiting for it and returns its process id. A thread waits for it to
/// exit, so no zombie process is left behind.
fn launch(mut command: Command) -> std::io::Result<u32> {
    let mut child = command.spawn()?;
    let pid = child.id();
    // Without the thread the child is merely not reaped until the app exits.
    let _ = std::thread::Builder::new().name("reap-child".to_string()).spawn(move || child.wait());
    Ok(pid)
}

/// `explorer.exe` in the Windows directory. Never looked up by bare name: that searches the
/// app's own folder (often the download folder) first.
#[cfg(windows)]
fn explorer_path(system_root: Option<OsString>) -> PathBuf {
    system_root
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("explorer.exe")
}

/// Explorer with `arg` passed verbatim (it parses its own command line).
#[cfg(windows)]
fn explorer(arg: OsString) -> Command {
    use std::os::windows::process::CommandExt;
    let mut command = Command::new(explorer_path(std::env::var_os("SystemRoot")));
    command.raw_arg(arg);
    command
}

/// Opens a file or folder with its default application. Arguments go straight to the
/// program, never through a shell.
pub fn open_path(path: &Path) -> std::io::Result<()> {
    let path = std::path::absolute(path)?;
    #[cfg(windows)]
    let command = {
        // Windows paths cannot contain '"', so quoting keeps commas and spaces inside the one argument.
        let mut arg = OsString::from("\"");
        arg.push(&path);
        arg.push("\"");
        explorer(arg)
    };
    #[cfg(target_os = "macos")]
    let command = {
        let mut command = Command::new("open");
        command.arg(&path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let command = {
        let mut command = Command::new("xdg-open");
        command.arg(&path);
        command
    };
    launch(command).map(|_| ())
}

/// Shows `path` selected in the system file manager (its folder where selecting isn't supported).
pub fn reveal_in_folder(path: &Path) -> std::io::Result<()> {
    let path = std::path::absolute(path)?;
    #[cfg(windows)]
    let command = {
        let mut arg = OsString::from("/select,\"");
        arg.push(&path);
        arg.push("\"");
        explorer(arg)
    };
    #[cfg(target_os = "macos")]
    let command = {
        let mut command = Command::new("open");
        command.arg("-R").arg(&path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let command = {
        let mut command = Command::new("xdg-open");
        command.arg(path.parent().unwrap_or(path.as_path()));
        command
    };
    launch(command).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_link_only_takes_single_links() {
        let long = format!("https://ja.wikipedia.org/wiki/{}", "東京都の区市町村".repeat(4));
        assert_eq!(clipboard_link(&format!("  {}\n", long)), Some(long));
        assert_eq!(clipboard_link("see https://a.com/x"), None);
        assert_eq!(clipboard_link("https://a.com/x https://b.com/x"), None);
        assert_eq!(clipboard_link("hello"), None);
        assert_eq!(clipboard_link("file:///etc/passwd"), None);
        assert_eq!(clipboard_link("blob:https://www.youtube.com/x"), None);
        let hash = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";
        let seeded = format!("magnet:?xt=urn:btih:{}&dn=f.iso&ws=https%3A%2F%2Fmirror.example%2Ff.iso", hash);
        assert!(clipboard_link(&seeded).is_some());
        assert!(clipboard_link(&format!("magnet:?xt=urn:btih:{}", hash)).is_none());
        // A document lists downloads rather than being one.
        assert_eq!(clipboard_link("https://a.com/list.meta4"), None);
        assert_eq!(clipboard_link("https://a.com/x.torrent"), None);
    }

    #[test]
    fn only_a_folder_to_save_into_is_created_before_a_download() {
        let dir = Path::new("dl");
        let item = |output: PathBuf, named: bool, target: Option<PathBuf>| {
            let mut queue = hyperfetch_core::DownloadQueue::new();
            let options = hyperfetch_core::engine::DownloadOptions { output_path: Some(output), ..Default::default() };
            let urls = vec![Url::parse("https://a.example/x.iso").unwrap()];
            let id = if named { queue.add_named_item(urls, options) } else { queue.add_item(urls, options) };
            let mut item = queue.get_item(id).unwrap().clone();
            item.target_path = target;
            item
        };
        // Saved under the server's name: the folder, whether or not the engine reported the file yet.
        assert_eq!(folder_to_create(&item(dir.into(), false, None)), Some(dir.into()));
        assert_eq!(folder_to_create(&item(dir.into(), false, Some(dir.join("x.iso")))), Some(dir.into()));
        // Named by its input: the path is the file, whose folders the engine creates, before and
        // after the engine reported where it went.
        let file = dir.join("sub").join("x.iso");
        assert_eq!(folder_to_create(&item(file.clone(), true, None)), None);
        assert_eq!(folder_to_create(&item(file, true, Some(dir.join("sub").join("x (1).iso")))), None);
    }

    #[cfg(windows)]
    #[test]
    fn explorer_is_launched_from_the_windows_directory() {
        assert_eq!(explorer_path(None), PathBuf::from(r"C:\Windows\explorer.exe"));
        assert_eq!(explorer_path(Some(r"D:\WinNT".into())), PathBuf::from(r"D:\WinNT\explorer.exe"));
        assert_eq!(explorer_path(Some("Windows".into())), PathBuf::from(r"C:\Windows\explorer.exe"), "never relative");
        assert!(explorer_path(std::env::var_os("SystemRoot")).is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn launched_programs_are_reaped() {
        let pid = launch(Command::new("true")).unwrap();
        let proc_entry = PathBuf::from(format!("/proc/{}", pid));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        // A zombie keeps its /proc entry until it is waited for.
        while proc_entry.exists() {
            assert!(std::time::Instant::now() < deadline, "the child was never reaped");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn launch_reports_missing_programs() {
        assert!(launch(Command::new("endo-no-such-program-4f1c")).is_err());
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

    #[test]
    fn a_pasted_link_with_a_secret_finds_its_history_entry() {
        let live = "https://files.example/Report.pdf?X-Amz-Signature=abc";
        let entry = HistoryEntry::new("Report.pdf".into(), PathBuf::from("/d/Report.pdf"), 1, vec![live.into()]);
        let key = history_search_key(&format!("  {live} "));
        assert!(entry.urls[0].to_lowercase().contains(&key), "{key}");
        assert_eq!(history_search_key(" Report "), "report");
    }

    #[test]
    fn repair_leaves_out_links_saved_without_their_secret() {
        let signed = HistoryEntry::new("a.bin".into(), PathBuf::from("/d/a.bin"), 1, vec!["https://h/a.bin?X-Amz-Signature=abc".into()]);
        assert_eq!(repair_mirrors(&signed.urls), Err(REDACTED_LINK.to_string()));
        let mirrors = vec!["https://h/a.bin?token=REDACTED".to_string(), "https://m/a.bin".to_string()];
        assert_eq!(repair_mirrors(&mirrors), Ok(vec![Url::parse("https://m/a.bin").unwrap()]));
        assert_eq!(repair_mirrors(&[]), Ok(Vec::new()), "nothing recorded is no redacted link");
    }

    #[test]
    fn redownload_leaves_out_links_saved_without_their_secret() {
        let entry = HistoryEntry::new("a.bin".into(), PathBuf::from("/d/a.bin"), 1, vec!["https://h/a.bin?token=abc".into()]);
        assert_eq!(redownload_input(&entry.urls), Err(REDACTED_LINK.to_string()));
        let mirrors = vec!["https://h/a.bin?token=REDACTED".to_string(), "https://m/a.bin".to_string()];
        assert_eq!(redownload_input(&mirrors).as_deref(), Ok("https://m/a.bin"));
        assert_eq!(redownload_input(&["https://m/a.bin?x=1".to_string()]).as_deref(), Ok("https://m/a.bin?x=1"));
    }
}
