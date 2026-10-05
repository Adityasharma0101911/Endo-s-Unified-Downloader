use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};
use url::{form_urlencoded, Url};

/// Only the newest entries are kept so loading and saving stay cheap.
const MAX_ENTRIES: usize = 1000;
/// How long a writer waits for another process (CLI, GUI, parallel download) to finish its save.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

static UNIQUE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A string unique across processes and calls: nanosecond timestamp, pid and a per-process counter.
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:x}-{:x}-{:x}",
        nanos,
        std::process::id(),
        UNIQUE_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// `path` with `suffix` appended to its file name (`history.json` -> `history.json<suffix>`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Stands in for a secret taken out of a saved link.
pub const REDACTED: &str = "REDACTED";

/// Why a saved link cannot be requested again.
pub const REDACTED_LINK: &str = "The link held a secret that was not saved; paste the link again";

/// Query and fragment parameters whose values are secrets (lower case, matched ignoring case and
/// underscores around the name, as in Akamai's `__token__`), besides those [`SECRET_SUFFIXES`]
/// end: the credentials of presigned links (S3 and CloudFront, Google Cloud Storage, Azure SAS
/// `sig`), logins passed in the link (SharePoint and OneDrive `tempauth`, Backblaze B2
/// `Authorization`, JWTs), API keys, CDN tokens (Akamai `hdnts`, `hdnea`) and Google's link
/// signature `usg`.
const SECRET_PARAMS: &[&str] = &[
    "x-amz-credential",
    "awsaccesskeyid",
    "x-goog-credential",
    "googleaccessid",
    "sig",
    "tempauth",
    "apikey",
    "key",
    "authorization",
    "auth",
    "jwt",
    "hdnts",
    "hdnea",
    "usg",
];

/// Endings of parameter names whose values are secrets: tokens (`token`, OAuth `access_token`,
/// GitLab `private_token`, S3 `X-Amz-Security-Token`), keys (`api_key`, `access_key`), secrets,
/// signatures (`X-Amz-Signature`) and passwords.
const SECRET_SUFFIXES: &[&str] = &["token", "_key", "-key", "secret", "signature", "password"];

/// Parameters that are secrets only on some hosts (and their subdomains), as the names are
/// common elsewhere: Discord's attachment signature `hm`, Outlook Safe Links' `data`, which
/// holds the recipient's address, and `sdata`, its signature, and the token of a Slack file link.
const HOST_SECRET_PARAMS: &[(&str, &[&str])] = &[
    ("discordapp.com", &["hm"]),
    ("discordapp.net", &["hm"]),
    ("safelinks.protection.outlook.com", &["data", "sdata"]),
    ("safelinks.protection.office365.us", &["data", "sdata"]),
    ("slack.com", &["t"]),
];

/// Whether a parameter named `name` holds a secret (see [`SECRET_PARAMS`]); `scoped` lists the
/// names that do on the link's host.
fn secret_param(name: &str, scoped: &[&str]) -> bool {
    let name = name.to_ascii_lowercase();
    let name = name.trim_matches('_');
    SECRET_PARAMS.iter().chain(scoped).any(|s| name == *s) || SECRET_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// `path` of a Telegram Bot API link with the bot's token (`/bot<id>:<token>/...` or
/// `/file/bot<id>:<token>/...`, which logs in as the bot) replaced by [`REDACTED`], or None if
/// it holds none. Sets `marked` when a token already is [`REDACTED`].
fn scrub_bot_token(path: &str, marked: &mut bool) -> Option<String> {
    let mut changed = false;
    let segments: Vec<Cow<'_, str>> = path
        .split('/')
        .map(|segment| match segment.strip_prefix("bot").and_then(|s| s.split_once(':')) {
            Some((id, token)) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && !token.is_empty() => {
                if token == REDACTED {
                    *marked = true;
                    Cow::Borrowed(segment)
                } else {
                    changed = true;
                    Cow::Owned(format!("bot{id}:{REDACTED}"))
                }
            }
            _ => Cow::Borrowed(segment),
        })
        .collect();
    changed.then(|| segments.join("/"))
}

/// `url` without its secrets, for saving: the `user:password@` part is dropped, and the values of
/// secret parameters (see `SECRET_PARAMS`) in the query and fragment, and a Telegram bot's token
/// in the path, become [`REDACTED`], also in links carried inside a parameter (a redirect's target). Everything else is kept as written, so
/// a link without secrets comes back unchanged, as does a string that is not a URL.
pub fn redact_url(url: &str) -> String {
    scrub(url).0.into_owned()
}

/// A secret was taken out of this saved link (see [`redact_url`]), so it cannot be requested.
pub fn is_redacted(url: &str) -> bool {
    scrub(url).1
}

/// `text` (an error message, say) with every link in it passed through [`redact_url`].
pub fn redact_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let scheme = rest[..at].chars().rev().take_while(|c| c.is_ascii_alphanumeric() || "+-.".contains(*c)).count();
        let start = at - scheme;
        let len = rest[at..].find(|c: char| c.is_whitespace() || "\"'<>()[]{}".contains(c)).unwrap_or(rest.len() - at);
        // Punctuation that ends a sentence is not part of the link.
        let link = rest[start..at + len].trim_end_matches(['.', ',', ':', ';']);
        let end = (start + link.len()).max(at + 3);
        out.push_str(&rest[..start]);
        out.push_str(&redact_url(&rest[start..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// `input` without its secrets (borrowed when it has none), and whether a secret parameter
/// already holds [`REDACTED`].
fn scrub(input: &str) -> (Cow<'_, str>, bool) {
    let unchanged = (Cow::Borrowed(input), false);
    if !input.contains(['@', '?', '#']) && !input.contains("/bot") {
        return unchanged;
    }
    let Ok(mut url) = Url::parse(input) else {
        return unchanged;
    };
    let mut changed = false;
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_password(None);
        let _ = url.set_username("");
        changed = true;
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let scoped: Vec<&str> = HOST_SECRET_PARAMS
        .iter()
        .filter(|(domain, _)| host.strip_suffix(domain).is_some_and(|sub| sub.is_empty() || sub.ends_with('.')))
        .flat_map(|(_, names)| names.iter().copied())
        .collect();
    let mut marked = false;
    if host == "api.telegram.org" {
        if let Some(path) = scrub_bot_token(url.path(), &mut marked) {
            url.set_path(&path);
            changed = true;
        }
    }
    if let Some(query) = url.query().and_then(|q| scrub_pairs(q, &scoped, &mut marked)) {
        url.set_query(Some(&query));
        changed = true;
    }
    if let Some(fragment) = url.fragment().and_then(|f| scrub_pairs(f, &scoped, &mut marked)) {
        url.set_fragment(Some(&fragment));
        changed = true;
    }
    (if changed { Cow::Owned(url.into()) } else { Cow::Borrowed(input) }, marked)
}

/// The `name=value` pairs of a query or fragment with secret values replaced and links inside
/// values scrubbed, or None if nothing changed. Sets `marked` when a secret already holds
/// [`REDACTED`].
fn scrub_pairs(pairs: &str, scoped: &[&str], marked: &mut bool) -> Option<String> {
    let mut changed = false;
    let out: Vec<Cow<'_, str>> = pairs
        .split('&')
        .map(|pair| {
            let Some((name, value)) = form_urlencoded::parse(pair.as_bytes()).next() else {
                return Cow::Borrowed(pair);
            };
            let raw_name = pair.split_once('=').map_or(pair, |(n, _)| n);
            let secret = secret_param(&name, scoped);
            if secret && value == REDACTED {
                *marked = true;
            } else if secret && !value.is_empty() {
                changed = true;
                return Cow::Owned(format!("{raw_name}={REDACTED}"));
            } else if value.contains("://") {
                let (inner, inner_marked) = scrub(&value);
                *marked |= inner_marked;
                if let Cow::Owned(inner) = inner {
                    changed = true;
                    let encoded: String = form_urlencoded::byte_serialize(inner.as_bytes()).collect();
                    return Cow::Owned(format!("{raw_name}={encoded}"));
                }
            }
            Cow::Borrowed(pair)
        })
        .collect();
    changed.then(|| out.join("&"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryStatus {
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub file_name: String,
    pub file_path: PathBuf,
    #[serde(default)]
    pub file_size: u64,
    #[serde(default)]
    pub downloaded_bytes: u64,
    #[serde(default)]
    pub urls: Vec<String>,
    pub status: HistoryStatus,
    #[serde(default)]
    pub blake3_hash: Option<String>,
    #[serde(default)]
    pub sha256_hash: Option<String>,
    /// Unix seconds when the download started.
    #[serde(default)]
    pub started_at: u64,
    #[serde(default)]
    pub completed_at: Option<u64>,
}

impl HistoryEntry {
    /// Creates an entry with a unique id and `urls` without their secrets (see [`redact_url`]).
    /// `started_at` defaults to now; callers that build the entry after the download finished
    /// must set it to the time the download began.
    pub fn new(
        file_name: String,
        file_path: PathBuf,
        file_size: u64,
        urls: Vec<String>,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Self {
            id: unique_suffix(),
            file_name,
            file_path,
            file_size,
            downloaded_bytes: 0,
            urls: urls.iter().map(|u| redact_url(u)).collect(),
            status: HistoryStatus::Completed,
            blake3_hash: None,
            sha256_hash: None,
            started_at: now,
            completed_at: None,
        }
    }

    /// Takes the secrets out of the links and of a failure message.
    fn redact(&mut self) {
        for url in &mut self.urls {
            if let Cow::Owned(redacted) = scrub(url).0 {
                *url = redacted;
            }
        }
        if let HistoryStatus::Failed(reason) = &mut self.status {
            *reason = redact_text(reason);
        }
    }

    fn recency(&self) -> u64 {
        self.completed_at.unwrap_or(self.started_at)
    }
}

/// Newest first, one entry per file path, at most `MAX_ENTRIES`.
fn normalize(entries: &mut Vec<HistoryEntry>) {
    entries.sort_by_key(|e| std::cmp::Reverse(e.recency()));
    let mut seen_paths = HashSet::new();
    entries.retain(|e| seen_paths.insert(e.file_path.clone()));
    entries.truncate(MAX_ENTRIES);
}

#[cfg(test)]
thread_local! {
    /// History file reads made on this thread.
    pub(crate) static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// History files flushed to disk on this thread.
    static SYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Reads the history file. A file that cannot be parsed reads as an empty list; with `backup` it
/// is also moved aside to `<path>.corrupt-<suffix>` (never overwritten). Only a caller holding the
/// history lock may back up, or it could move away a valid file another process just wrote.
fn read_entries(path: &Path, backup: bool) -> io::Result<Vec<HistoryEntry>> {
    #[cfg(test)]
    READS.with(|n| n.set(n.get() + 1));
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice::<Vec<HistoryEntry>>(&bytes) {
        // Entries saved before links were redacted lose their secrets at the next save.
        Ok(mut entries) => {
            entries.iter_mut().for_each(HistoryEntry::redact);
            Ok(entries)
        }
        Err(parse_err) if backup => {
            let backup = with_suffix(path, &format!(".corrupt-{}", unique_suffix()));
            fs::rename(path, &backup)?;
            tracing::warn!(
                "History file {:?} could not be parsed ({}); moved it to {:?} and started a new history",
                path,
                parse_err,
                backup
            );
            Ok(Vec::new())
        }
        Err(parse_err) => {
            tracing::warn!("History file {:?} could not be parsed ({}); showing an empty history", path, parse_err);
            Ok(Vec::new())
        }
    }
}

/// Atomically replaces the history file via a uniquely named temp file, which reaches the disk
/// before it takes the file's name: otherwise a power cut in the seconds after could leave a
/// history the OS had not written out yet, which reads as corrupt and loses every entry, with the
/// hashes `--verify` checks finished files against. That costs a few milliseconds a save, also
/// when downloads do not wait for the disk themselves (`fsync_on_complete`).
fn write_entries(path: &Path, entries: &[HistoryEntry]) -> io::Result<()> {
    let json = serde_json::to_vec(entries).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    let tmp_path = with_suffix(path, &format!(".{}.tmp", unique_suffix()));
    let result = (|| {
        let mut file = File::create(&tmp_path)?;
        file.write_all(&json)?;
        #[cfg(test)]
        SYNCS.with(|n| n.set(n.get() + 1));
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// Takes the cross-process lock of the file `path` (an OS advisory lock on `<path>.lock`, released
/// when the returned file is dropped or the process dies), as the history and the download
/// archive are written under.
pub(crate) fn lock_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(with_suffix(path, ".lock"))?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(ErrorKind::TimedOut, format!("timed out waiting for the lock on {}", path.display())));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// A change made through a manager, kept until it has been written to the history file.
#[derive(Debug, Clone)]
enum Change {
    Upsert(HistoryEntry),
    Remove(String),
    /// Removes the entries of this file.
    RemoveFile(PathBuf),
    Clear,
}

impl Change {
    /// Applies the change; returns whether it altered `entries`.
    fn apply(&self, entries: &mut Vec<HistoryEntry>) -> bool {
        let before = entries.len();
        match self {
            Change::Upsert(entry) => {
                entries.retain(|e| e.id != entry.id && e.file_path != entry.file_path);
                // Also a failure message set after `HistoryEntry::new`.
                let mut entry = entry.clone();
                entry.redact();
                entries.insert(0, entry);
                return true;
            }
            Change::Remove(id) => entries.retain(|e| e.id != *id),
            Change::RemoveFile(path) => entries.retain(|e| e.file_path != *path),
            Change::Clear => entries.clear(),
        }
        entries.len() != before
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadHistoryManager {
    entries: Vec<HistoryEntry>,
    #[serde(skip)]
    custom_path: Option<PathBuf>,
    /// Changes not yet written to disk. Saving replays only these onto the current file, so it
    /// never brings back entries another manager or process removed.
    #[serde(skip)]
    pending: Vec<Change>,
}

impl DownloadHistoryManager {
    pub fn default_history_path() -> PathBuf {
        if let Some(p) = std::env::var_os("ENDO_HISTORY_PATH") {
            return PathBuf::from(p);
        }
        // Unit tests never touch the user's history, and never switch files mid-run.
        if cfg!(test) {
            return std::env::temp_dir().join(format!("hf-unit-history-{}.json", std::process::id()));
        }

        #[cfg(windows)]
        if let Ok(app_data) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(app_data).join("EndosUnifiedDownloader").join("history.json");
        }

        // Application Support, with everything else of the app's (see media::app_data_dir).
        #[cfg(target_os = "macos")]
        if let Some(dir) = crate::media::app_data_dir() {
            return dir.join("history.json");
        }

        #[cfg(not(windows))]
        {
            if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
                return PathBuf::from(xdg_data).join("endos-downloader").join("history.json");
            }
            if let Ok(home) = std::env::var("HOME") {
                let xdg_fallback = PathBuf::from(&home).join(".local").join("share").join("endos-downloader").join("history.json");
                if xdg_fallback.exists() {
                    return xdg_fallback;
                }
                return PathBuf::from(home).join(".hyperfetch").join("history.json");
            }
        }

        if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
            return PathBuf::from(home).join(".hyperfetch").join("history.json");
        }

        PathBuf::from("history.json")
    }

    pub fn load() -> Self {
        let path = Self::default_history_path();
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Self {
        let entries = read_entries(path, false).unwrap_or_else(|e| {
            tracing::warn!("Failed to read history file at {:?}: {}", path, e);
            Vec::new()
        });
        Self {
            entries,
            custom_path: Some(path.to_path_buf()),
            pending: Vec::new(),
        }
    }

    fn path(&self) -> PathBuf {
        self.custom_path.clone().unwrap_or_else(Self::default_history_path)
    }

    /// Applies `change` in memory and writes it, with any earlier unsaved changes, to the history
    /// file. If the file cannot be updated the change stays pending for the next write.
    fn modify(&mut self, change: Change) -> bool {
        let changed = change.apply(&mut self.entries);
        normalize(&mut self.entries);
        self.pending.push(change);
        if let Err(e) = self.save() {
            tracing::warn!("Failed to update history file {:?}: {}", self.path(), e);
        }
        changed
    }

    /// Writes this manager's unsaved changes to the history file and refreshes it from the result.
    /// Under the history lock the file is reloaded and only those changes are replayed onto it, so
    /// updates made by other managers or processes since this one loaded are never lost or undone.
    pub fn save(&mut self) -> Result<(), std::io::Error> {
        let path = self.path();
        let _lock = lock_file(&path)?;
        let mut entries = read_entries(&path, true)?;
        for change in &self.pending {
            change.apply(&mut entries);
        }
        normalize(&mut entries);
        write_entries(&path, &entries)?;
        self.pending.clear();
        self.entries = entries;
        Ok(())
    }

    /// Adds `entry` to the history file at `path`, replacing any entry with the same id or the
    /// same file path, without loading the history first: the file is read once, under the lock,
    /// as [`DownloadHistoryManager::save`] does.
    pub fn record(path: &Path, entry: HistoryEntry) -> io::Result<()> {
        Self { entries: Vec::new(), custom_path: Some(path.to_path_buf()), pending: vec![Change::Upsert(entry)] }.save()
    }

    /// [`DownloadHistoryManager::record`] for a file made from `replaced`, which it took the place
    /// of (an HLS download's `.ts`, remuxed into an MP4): the entries of `replaced`, such as one a
    /// stopped attempt left, go in the same write, as they name a file that is no longer there.
    pub fn record_replacing(path: &Path, entry: HistoryEntry, replaced: &Path) -> io::Result<()> {
        let pending = vec![Change::RemoveFile(replaced.to_path_buf()), Change::Upsert(entry)];
        Self { entries: Vec::new(), custom_path: Some(path.to_path_buf()), pending }.save()
    }

    /// Adds `entry`, replacing any entry with the same id or the same file path.
    pub fn add_or_update(&mut self, entry: HistoryEntry) {
        self.modify(Change::Upsert(entry));
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Removes the entry with this id; returns whether this manager had it.
    pub fn remove_entry(&mut self, id: &str) -> bool {
        self.modify(Change::Remove(id.to_string()))
    }

    pub fn clear(&mut self) {
        self.modify(Change::Clear);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn entry(name: &str, dir: &Path) -> HistoryEntry {
        HistoryEntry::new(
            name.to_string(),
            dir.join(name),
            10,
            vec![format!("https://example.com/{}", name)],
        )
    }

    #[test]
    fn test_history_manager_lifecycle() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("test_history.json");

        let mut manager = DownloadHistoryManager::load_from_path(&history_path);
        assert_eq!(manager.entries().len(), 0);

        let mut entry1 = HistoryEntry::new(
            "test_build.7z".to_string(),
            PathBuf::from("C:\\Downloads\\test_build.7z"),
            1024 * 1024 * 100,
            vec!["https://example.com/test_build.7z".to_string()],
        );
        entry1.downloaded_bytes = 1024 * 1024 * 100;
        entry1.status = HistoryStatus::Completed;
        entry1.blake3_hash = Some("abcdef1234567890".to_string());

        manager.add_or_update(entry1.clone());
        assert_eq!(manager.entries().len(), 1);
        assert_eq!(manager.entries()[0].file_name, "test_build.7z");

        // Reload from disk
        let reloaded = DownloadHistoryManager::load_from_path(&history_path);
        assert_eq!(reloaded.entries().len(), 1);
        assert_eq!(reloaded.entries()[0].id, entry1.id);
        assert_eq!(reloaded.entries()[0].blake3_hash, Some("abcdef1234567890".to_string()));

        // Remove
        let removed = manager.remove_entry(&entry1.id);
        assert!(removed);
        assert_eq!(manager.entries().len(), 0);
        assert_eq!(DownloadHistoryManager::load_from_path(&history_path).entries().len(), 0);
    }

    #[test]
    fn corrupt_history_is_backed_up_not_wiped() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        std::fs::write(&history_path, b"[{\"id\": \"truncated").unwrap();

        // A plain load holds no lock, so it must leave the file where it is.
        let mut manager = DownloadHistoryManager::load_from_path(&history_path);
        assert!(manager.entries().is_empty());
        assert_eq!(std::fs::read(&history_path).unwrap(), b"[{\"id\": \"truncated");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        manager.add_or_update(entry("a.bin", dir.path()));

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains("history.json.corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"[{\"id\": \"truncated");
        assert_eq!(DownloadHistoryManager::load_from_path(&history_path).entries().len(), 1);
    }

    #[test]
    fn concurrent_managers_lose_no_updates() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");

        // Both managers load before either writes, like the GUI and a CLI run side by side.
        let handles: Vec<_> = ["gui", "cli"]
            .into_iter()
            .map(|who| {
                let mut manager = DownloadHistoryManager::load_from_path(&history_path);
                let dir = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        manager.add_or_update(entry(&format!("{}-{}.bin", who, i), &dir));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let reloaded = DownloadHistoryManager::load_from_path(&history_path);
        assert_eq!(reloaded.entries().len(), 50);
    }

    #[test]
    fn stale_manager_does_not_drop_other_writers_entries() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let mut gui = DownloadHistoryManager::load_from_path(&history_path);
        gui.add_or_update(entry("old.bin", dir.path()));
        let old_id = gui.entries()[0].id.clone();

        let mut cli = DownloadHistoryManager::load_from_path(&history_path);
        cli.add_or_update(entry("new.bin", dir.path()));

        assert!(gui.remove_entry(&old_id));
        let names: Vec<_> = gui.entries().iter().map(|e| e.file_name.as_str()).collect();
        assert_eq!(names, ["new.bin"]);
    }

    fn names_on_disk(path: &Path) -> Vec<String> {
        let mut names: Vec<_> = DownloadHistoryManager::load_from_path(path)
            .entries()
            .iter()
            .map(|e| e.file_name.clone())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn save_does_not_resurrect_entries_removed_elsewhere() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let mut gui = DownloadHistoryManager::load_from_path(&history_path);
        gui.add_or_update(entry("x.bin", dir.path()));
        gui.add_or_update(entry("y.bin", dir.path()));
        let x_id = gui.entries().iter().find(|e| e.file_name == "x.bin").unwrap().id.clone();

        let mut cli = DownloadHistoryManager::load_from_path(&history_path);
        assert!(gui.remove_entry(&x_id));
        cli.save().unwrap();
        assert_eq!(names_on_disk(&history_path), ["y.bin"]);
        assert_eq!(cli.entries().len(), 1);

        gui.clear();
        cli.save().unwrap();
        assert!(names_on_disk(&history_path).is_empty());
    }

    #[test]
    fn unsaved_change_is_written_later_without_undoing_others() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let mut gui = DownloadHistoryManager::load_from_path(&history_path);
        gui.add_or_update(entry("x.bin", dir.path()));
        let x_id = gui.entries()[0].id.clone();
        let mut cli = DownloadHistoryManager::load_from_path(&history_path);

        // A directory in place of the lock file makes every write fail.
        let lock_path = with_suffix(&history_path, ".lock");
        std::fs::remove_file(&lock_path).unwrap();
        std::fs::create_dir(&lock_path).unwrap();
        cli.add_or_update(entry("offline.bin", dir.path()));
        assert_eq!(cli.entries().len(), 2, "the change is kept in memory");
        std::fs::remove_dir(&lock_path).unwrap();

        assert!(gui.remove_entry(&x_id));
        cli.save().unwrap();
        assert_eq!(names_on_disk(&history_path), ["offline.bin"]);
    }

    #[test]
    fn record_reads_the_history_once_and_keeps_other_entries() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let mut other = DownloadHistoryManager::load_from_path(&history_path);
        other.add_or_update(entry("old.bin", dir.path()));
        let mut replaced = entry("same.bin", dir.path());
        other.add_or_update(replaced.clone());

        replaced.file_size = 99;
        let reads = READS.with(std::cell::Cell::get);
        DownloadHistoryManager::record(&history_path, replaced).unwrap();
        assert_eq!(READS.with(std::cell::Cell::get) - reads, 1, "one read, under the lock");

        assert_eq!(names_on_disk(&history_path), ["old.bin", "same.bin"]);
        let on_disk = DownloadHistoryManager::load_from_path(&history_path);
        assert_eq!(on_disk.entries().iter().find(|e| e.file_name == "same.bin").unwrap().file_size, 99);
    }

    #[test]
    fn every_save_reaches_the_disk_before_it_replaces_the_history() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let syncs = || SYNCS.with(std::cell::Cell::get);
        let before = syncs();
        // Also that of a download finishing without fsync_on_complete: a power cut must not wipe
        // the history, and every finished file's hash with it, for the sake of the newest entry.
        DownloadHistoryManager::record(&history_path, entry("fast.bin", dir.path())).unwrap();
        assert_eq!(syncs(), before + 1);
        DownloadHistoryManager::load_from_path(&history_path).clear();
        assert_eq!(syncs(), before + 2);

        DownloadHistoryManager::record(&history_path, entry("fast.bin", dir.path())).unwrap();
        assert_eq!(names_on_disk(&history_path), ["fast.bin"]);
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|f| !f.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn redownload_of_same_path_replaces_entry_and_ids_are_unique() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let mut manager = DownloadHistoryManager::load_from_path(&history_path);

        let first = entry("same.bin", dir.path());
        let mut second = entry("same.bin", dir.path());
        assert_ne!(first.id, second.id);
        second.file_size = 99;

        manager.add_or_update(first);
        manager.add_or_update(second.clone());
        assert_eq!(manager.entries().len(), 1);
        assert_eq!(manager.entries()[0].id, second.id);
        assert_eq!(manager.entries()[0].file_size, 99);
    }

    #[test]
    fn normalize_keeps_newest_bounded_entries() {
        let dir = tempdir().unwrap();
        let mut entries: Vec<_> = (0..MAX_ENTRIES as u64 + 5)
            .map(|i| {
                let mut e = entry(&format!("{}.bin", i), dir.path());
                e.completed_at = Some(i);
                e
            })
            .collect();
        normalize(&mut entries);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0].completed_at, Some(MAX_ENTRIES as u64 + 4));
        assert_eq!(entries.last().unwrap().completed_at, Some(5));
    }

    #[test]
    fn redact_takes_out_credentials_and_secret_parameters_only() {
        assert_eq!(redact_url("https://me:pw@files.example/a.zip"), "https://files.example/a.zip");
        assert_eq!(redact_url("ftp://me@files.example/a.zip"), "ftp://files.example/a.zip");
        let s3 = "https://b.s3.amazonaws.com/k.bin?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIA%2F2026&X-Amz-Date=20260927T000000Z&X-Amz-Security-Token=t&X-Amz-Signature=abc";
        assert_eq!(
            redact_url(s3),
            "https://b.s3.amazonaws.com/k.bin?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=REDACTED&X-Amz-Date=20260927T000000Z&X-Amz-Security-Token=REDACTED&X-Amz-Signature=REDACTED"
        );
        assert_eq!(redact_url("https://a.blob.core.windows.net/c/f?sv=2022&SIG=x%2By"), "https://a.blob.core.windows.net/c/f?sv=2022&SIG=REDACTED");
        assert_eq!(
            redact_url("https://moodle.example/webservice/pluginfile.php/1/f.pdf?Token=abc&forcedownload=1"),
            "https://moodle.example/webservice/pluginfile.php/1/f.pdf?Token=REDACTED&forcedownload=1"
        );
        assert_eq!(redact_url("https://app.example/cb#access_token=abc&state=1"), "https://app.example/cb#access_token=REDACTED&state=1");

        // Links without secrets, and strings that are not links, come back exactly as given.
        for same in ["https://e.com/a%20b?x=1&token", "HTTPS://E.com/a?q=1", "not a url?token=abc", "hyperfetch-media:/yt/id/137", ""] {
            assert_eq!(redact_url(same), same);
        }
        let once = redact_url(s3);
        assert_eq!(redact_url(&once), once, "redacting again changes nothing");
    }

    #[test]
    fn redact_knows_host_specific_secrets_and_links_inside_links() {
        let discord = "https://cdn.discordapp.com/attachments/1/2/f.png?ex=65&is=64&hm=abc&";
        assert_eq!(redact_url(discord), "https://cdn.discordapp.com/attachments/1/2/f.png?ex=65&is=64&hm=REDACTED&");
        assert_eq!(redact_url("https://other.example/f?hm=abc&data=1"), "https://other.example/f?hm=abc&data=1");

        let safelink = "https://nam12.safelinks.protection.outlook.com/?url=https%3A%2F%2Fx.example%2Ff.zip%3Ftoken%3Dabc&data=05%7C02%7Cme%40corp.example%7C&sdata=xyz&reserved=0";
        let redacted = redact_url(safelink);
        assert!(!redacted.contains("corp.example") && !redacted.contains("xyz") && !redacted.contains("abc"), "{redacted}");
        assert!(redacted.ends_with("&data=REDACTED&sdata=REDACTED&reserved=0"), "{redacted}");
        let inner = url::form_urlencoded::parse(Url::parse(&redacted).unwrap().query().unwrap().as_bytes()).next().unwrap().1.into_owned();
        assert_eq!(inner, "https://x.example/f.zip?token=REDACTED");
        assert!(is_redacted(&redacted), "a secret inside the carried link counts too");
    }

    #[test]
    fn redact_knows_secrets_by_how_their_names_end_and_bot_tokens() {
        for (live, saved) in [
            (
                "https://gitlab.com/api/v4/projects/1/packages/generic/app/1.0/app.zip?private_token=glpat-XXXX",
                "https://gitlab.com/api/v4/projects/1/packages/generic/app/1.0/app.zip?private_token=REDACTED",
            ),
            ("https://f002.backblazeb2.com/file/b/a.zip?Authorization=4_002abc", "https://f002.backblazeb2.com/file/b/a.zip?Authorization=REDACTED"),
            ("https://h.example/a?auth_token=1&access_key=2&jwt=3&auth=4&v=5", "https://h.example/a?auth_token=REDACTED&access_key=REDACTED&jwt=REDACTED&auth=REDACTED&v=5"),
            ("https://cdn.example/v.mp4?hdnts=exp%3D1~hmac%3Dab&__token__=st%3D1", "https://cdn.example/v.mp4?hdnts=REDACTED&__token__=REDACTED"),
            ("https://h.example/a?X-API-Key=k&db_password=p&client_secret=s", "https://h.example/a?X-API-Key=REDACTED&db_password=REDACTED&client_secret=REDACTED"),
            ("https://www.google.com/url?q=https%3A%2F%2Fx.example%2F&usg=AOvVaw1", "https://www.google.com/url?q=https%3A%2F%2Fx.example%2F&usg=REDACTED"),
            ("https://files.slack.com/files-pri/T0-F0/download/f.zip?t=xoxe-123", "https://files.slack.com/files-pri/T0-F0/download/f.zip?t=REDACTED"),
            ("https://api.telegram.org/file/bot123456:AAHdqTcv/documents/file_1.pdf", "https://api.telegram.org/file/bot123456:REDACTED/documents/file_1.pdf"),
        ] {
            assert_eq!(redact_url(live), saved, "{live}");
            assert!(is_redacted(saved), "{saved}");
        }
        // Names that only look alike, and `t` or bot paths elsewhere, are kept.
        for same in [
            "https://h.example/a?keyword=x&monkey=1&t=2",
            "https://example.com/file/bot123456:AAH/a.pdf",
            "https://api.telegram.org/file/botfather/a.pdf",
        ] {
            assert_eq!(redact_url(same), same);
        }
    }

    #[test]
    fn only_a_saved_link_counts_as_redacted() {
        let live = "https://h.example/f?token=abc";
        assert!(!is_redacted(live));
        assert!(is_redacted(&redact_url(live)));
        assert!(!is_redacted("https://h.example/f?note=REDACTED"), "not a secret parameter");
        assert!(!is_redacted("https://me:pw@h.example/f"), "a dropped login leaves nothing to find");
    }

    #[test]
    fn redact_text_scrubs_every_link_in_a_message() {
        assert_eq!(redact_text("https://h/f?token=abc: HTTP 403"), "https://h/f?token=REDACTED: HTTP 403");
        assert_eq!(
            redact_text("error sending request for url (https://u:p@h/x?sig=1&a=2), then ftp://me:pw@f/y."),
            "error sending request for url (https://h/x?sig=REDACTED&a=2), then ftp://f/y."
        );
        for same in ["connection reset", "odd :// here", "://", "é://x?token=1"] {
            assert_eq!(redact_text(same), same);
        }
    }

    #[test]
    fn history_keeps_no_secrets_and_still_finds_the_entry() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let live = "https://me:pw@h.example/f.bin?X-Amz-Signature=abc&v=1";
        let mut failed = HistoryEntry::new("f.bin".into(), dir.path().join("f.bin"), 1, vec![live.to_string()]);
        assert_eq!(failed.urls, ["https://h.example/f.bin?X-Amz-Signature=REDACTED&v=1"]);
        // The failure message is set after `new`, as a download does.
        failed.status = HistoryStatus::Failed(format!("{live}: HTTP 403"));
        DownloadHistoryManager::record(&history_path, failed).unwrap();

        let on_disk = std::fs::read_to_string(&history_path).unwrap();
        assert!(!on_disk.contains("abc") && !on_disk.contains("pw"), "{on_disk}");
        let loaded = DownloadHistoryManager::load_from_path(&history_path);
        assert_eq!(loaded.entries()[0].urls, [redact_url(live)], "the live link finds its entry by its redacted form");
    }

    #[test]
    fn old_entries_lose_their_secrets_at_the_next_save() {
        let dir = tempdir().unwrap();
        let history_path = dir.path().join("history.json");
        let mut old = entry("old.bin", dir.path());
        old.urls = vec!["https://h.example/old.bin?token=abc".into()];
        old.status = HistoryStatus::Failed("https://h.example/old.bin?token=abc: HTTP 500".into());
        std::fs::write(&history_path, serde_json::to_vec(&[old]).unwrap()).unwrap();

        let mut manager = DownloadHistoryManager::load_from_path(&history_path);
        assert_eq!(manager.entries()[0].urls, ["https://h.example/old.bin?token=REDACTED"]);
        assert_eq!(manager.entries()[0].status, HistoryStatus::Failed("https://h.example/old.bin?token=REDACTED: HTTP 500".into()));
        assert!(std::fs::read_to_string(&history_path).unwrap().contains("abc"), "a plain load writes nothing");

        manager.add_or_update(entry("new.bin", dir.path()));
        assert!(!std::fs::read_to_string(&history_path).unwrap().contains("abc"));
        assert_eq!(names_on_disk(&history_path), ["new.bin", "old.bin"]);
    }
}
