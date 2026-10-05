//! What happens to a download once it is on disk: Mark-of-the-Web, archive extraction, sorting
//! into category folders, a VirusTotal lookup and the user's command, in that order.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::history::DownloadHistoryManager;

/// The post-processing a download asks for (see `DownloadOptions::post`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PostOptions {
    /// Unpack an archive into a folder next to it.
    pub extract: bool,
    /// Delete the archive once it is unpacked.
    pub delete_archive: bool,
    /// Move a single file into a category folder (Video, Music, ...) of the save folder.
    pub sort: bool,
    /// The command run after the download. Only the settings or the command line set it, never
    /// anything the browser sends.
    pub run_after: Option<String>,
    /// Mark the downloaded files as from the internet (Windows' Zone.Identifier).
    pub mark_of_the_web: bool,
    /// The VirusTotal API key to look the file's hash up with. Never saved with the options.
    #[serde(skip)]
    pub virustotal_key: Option<String>,
}

impl Default for PostOptions {
    fn default() -> Self {
        Self { extract: false, delete_archive: false, sort: false, run_after: None, mark_of_the_web: true, virustotal_key: None }
    }
}

/// Where the download is after post-processing (moved or unpacked) and what the user should know.
#[derive(Debug, Clone, PartialEq)]
pub struct PostOutcome {
    pub path: PathBuf,
    pub notes: Vec<String>,
}

/// VirusTotal's file reports; the file's SHA-256 goes on the end.
const VIRUSTOTAL: &str = "https://www.virustotal.com/api/v3/files/";

/// How long the command after a download may run before it is stopped.
const RUN_AFTER_LIMIT: Duration = Duration::from_secs(600);

/// Archive extensions tar unpacks, longest first so `.tar.gz` wins over a bare `.gz`.
const ARCHIVES: &[&str] = &[".tar.gz", ".tar.xz", ".tar.bz2", ".tgz", ".txz", ".tbz2", ".tar", ".zip", ".7z", ".rar"];

/// The folders [`sort`] moves files into, by extension.
const CATEGORIES: &[(&str, &[&str])] = &[
    ("Video", &["mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "m4v", "ts", "mpg", "mpeg", "3gp"]),
    ("Music", &["mp3", "m4a", "flac", "wav", "ogg", "opus", "aac", "wma", "aiff", "alac"]),
    ("Pictures", &["jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "heic", "avif", "tif", "tiff"]),
    ("Documents", &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "txt", "rtf", "epub", "mobi", "csv", "md"]),
    ("Archives", &["zip", "7z", "rar", "tar", "gz", "tgz", "xz", "bz2", "zst", "iso"]),
    ("Programs", &["exe", "msi", "msix", "appx", "dmg", "pkg", "deb", "rpm", "apk", "appimage"]),
];

/// Post-processes the download at `path` (a file or a folder) of `source` as `post` says; the
/// VirusTotal lookup goes through `proxy`, as the download did. An archive is opened with
/// `password` (`DownloadOptions::password`) if given.
pub async fn after_download(path: &Path, post: &PostOptions, source: &Url, proxy: Option<&str>, password: Option<&str>) -> PostOutcome {
    let key = post.virustotal_key.clone().filter(|key| !key.trim().is_empty());
    let on_disk = {
        let (path, post, source, hash) = (path.to_path_buf(), post.clone(), source.clone(), key.is_some());
        let password = password.filter(|p| !p.is_empty()).map(str::to_string);
        tokio::task::spawn_blocking(move || on_disk(path, &post, &source, hash, password.as_deref())).await
    };
    let Ok((path, mut notes, sha256)) = on_disk else {
        return PostOutcome { path: path.to_path_buf(), notes: vec!["Post-processing stopped unexpectedly".into()] };
    };
    if let (Some(key), Some(sha256)) = (key, sha256) {
        notes.push(virustotal(VIRUSTOTAL, &sha256, &key, proxy).await);
    }
    if let Some(command) = post.run_after.as_deref().filter(|c| !c.trim().is_empty()) {
        notes.extend(run_after(command, &path, source).await);
    }
    PostOutcome { path, notes }
}

/// The blocking steps of [`after_download`]: marks, unpacks and sorts the download, and hashes
/// it first when `hash` (so an archive unpacked and deleted still gets looked up). Returns where
/// the download is now, the notes and the SHA-256.
fn on_disk(mut path: PathBuf, post: &PostOptions, source: &Url, hash: bool, password: Option<&str>) -> (PathBuf, Vec<String>, Option<String>) {
    let mut notes = Vec::new();
    let sha256 = (hash && path.is_file())
        .then(|| sha256_of(&path).map_err(|e| notes.push(format!("VirusTotal: could not read the file: {e}"))).ok())
        .flatten();
    if post.mark_of_the_web {
        mark_from_internet(&path, source);
    }
    if post.extract && path.is_file() {
        match extract(&path, password) {
            Ok(Some(folder)) => {
                if post.mark_of_the_web {
                    mark_from_internet(&folder, source);
                }
                if post.delete_archive {
                    if let Err(e) = std::fs::remove_file(&path) {
                        notes.push(format!("Could not delete {} after unpacking it: {e}", file_name(&path)));
                    }
                }
                path = folder;
            }
            Ok(None) => {}
            Err(note) => notes.push(note),
        }
    }
    if post.sort && path.is_file() {
        match sort(&path) {
            Ok(Some(moved)) => {
                moved_in_history(&path, &moved);
                path = moved;
            }
            Ok(None) => {}
            Err(e) => notes.push(format!("Could not sort {} into its folder: {e}", file_name(&path))),
        }
    }
    (path, notes, sha256)
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// Calls `f` with `path` and its metadata, and when it is a folder with every file and folder in
/// it, all the way down. Links are left out, and not followed.
fn each_entry(path: &Path, f: &mut impl FnMut(&Path, &std::fs::Metadata)) {
    let Ok(m) = std::fs::symlink_metadata(path) else { return };
    if m.is_symlink() {
        return;
    }
    f(path, &m);
    if m.is_dir() {
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            each_entry(&entry.path(), f);
        }
    }
}

/// Calls `f` with `path` if it is a file, else with every file in the folder `path`, all the way
/// down (see [`each_entry`]).
fn each_file(path: &Path, f: &mut impl FnMut(&Path)) {
    each_entry(path, &mut |entry, m| {
        if m.is_file() {
            f(entry)
        }
    });
}

/// The size of `path`: a file's, or that of every file in a folder, all the way down. Blocking.
pub(crate) fn size_of(path: &Path) -> u64 {
    let mut size = 0;
    each_file(path, &mut |file| size += std::fs::metadata(file).map_or(0, |m| m.len()));
    size
}

/// Marks `path` (a file, or every file of a folder) as downloaded from the internet: Windows'
/// Zone.Identifier stream, which SmartScreen and Office read, with `source` minus its secrets
/// and its fragment (a MEGA folder's key); macOS' quarantine (see [`quarantine_value`]), which
/// Gatekeeper reads, on the folders too: it reads an app's from its bundle folder. Best effort:
/// FAT drives have no streams. Does nothing elsewhere.
pub(crate) fn mark_from_internet(path: &Path, source: &Url) {
    #[cfg(target_os = "macos")]
    {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let value = quarantine_value(now);
        each_entry(path, &mut |entry, _| {
            let _ = set_xattr(entry, "com.apple.quarantine", value.as_bytes());
        });
    }
    if !cfg!(windows) {
        return;
    }
    let mut zone = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if matches!(source.scheme(), "http" | "https") {
        let mut host_url = source.clone();
        host_url.set_fragment(None);
        zone += &format!("HostUrl={}\r\n", crate::history::redact_url(host_url.as_str()));
    }
    each_file(path, &mut |file| {
        let mut stream = file.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let _ = std::fs::write(stream, &zone);
    });
}

/// The `com.apple.quarantine` value of a file this app downloaded at `now` (Unix seconds), as
/// browsers write it: flags 0081 (downloaded, not yet opened), the time in hex, the app's name and
/// no event id.
#[cfg(any(target_os = "macos", test))]
fn quarantine_value(now: u64) -> String {
    format!("0081;{now:x};Endo's Unified Downloader;")
}

/// Sets the extended attribute `name` of `file` to `value`, not following a link. Blocking.
#[cfg(target_os = "macos")]
fn set_xattr(file: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(file.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    let name = std::ffi::CString::new(name).map_err(std::io::Error::other)?;
    // SAFETY: both strings are NUL-terminated and outlive the call; `value` is read for its length.
    let set = unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, libc::XATTR_NOFOLLOW) };
    if set == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `name` without its archive extension (see [`ARCHIVES`]), or None if it is no archive.
fn archive_stem(name: &str) -> Option<&str> {
    let lower = name.to_ascii_lowercase();
    ARCHIVES.iter().find(|ext| lower.ends_with(*ext)).map(|ext| &name[..name.len() - ext.len()]).filter(|s| !s.is_empty())
}

/// Whether `name` (lower case) is one part of a split archive: `x.part2.rar`, `x.7z.001`,
/// `x.zip.002`, `x.r00` or `x.z01`.
fn split_part(name: &str) -> bool {
    let Some((rest, ext)) = name.rsplit_once('.') else { return false };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (ext == "rar" && rest.rsplit_once(".part").is_some_and(|(_, n)| digits(n)))
        || (digits(ext) && [".7z", ".zip", ".rar"].iter().any(|a| rest.ends_with(a)))
        || (ext.len() == 3 && matches!(ext.as_bytes()[0], b'r' | b'z') && digits(&ext[1..]))
}

/// Unpacks the archive `file` with the system tar into a new folder next to it, named after it
/// (numbered if taken), opened with `password` if given: an encrypted .7z or .rar, which tar
/// cannot open, with 7-Zip when it is installed. Every entry is checked first: one that would
/// land outside the folder refuses the whole archive, as does a link among what it unpacked (a
/// later download into a folder of that name would write through it). Ok(None) if `file` is no
/// archive; Err is the note saying why it stayed packed (split, password-protected or the
/// password wrong, unsafe or unreadable), which never holds the password.
fn extract(file: &Path, password: Option<&str>) -> Result<Option<PathBuf>, String> {
    let name = file_name(file);
    let lower = name.to_ascii_lowercase();
    if split_part(&lower) {
        return Err(format!("{name} is one part of a split archive, so it was left packed"));
    }
    let Some(stem) = archive_stem(&name) else { return Ok(None) };
    let seven_or_rar = lower.ends_with(".7z") || lower.ends_with(".rar");
    // Only these are ever encrypted (GNU tar would refuse `--passphrase` for a .tar.gz).
    let password = password.filter(|_| seven_or_rar || lower.ends_with(".zip"));
    let seven = password.filter(|_| seven_or_rar).and_then(|_| seven_zip());
    // Linux's GNU tar reads tar archives alone; bsdtar (Windows' and macOS' tar) the rest too.
    let tar = || match cfg!(target_os = "linux") {
        true => crate::media::find_in_path("bsdtar"),
        false => Some(crate::media::system_tar()),
    };
    let tool = match seven.clone().or_else(tar) {
        Some(tool) => tool,
        None if [".zip", ".7z", ".rar"].iter().any(|ext| lower.ends_with(ext)) => {
            return Err(format!("{name} was left packed: unpacking it needs bsdtar (libarchive-tools)"));
        }
        None => crate::media::system_tar(),
    };
    let failed = |error: &str| {
        let lower = error.to_ascii_lowercase();
        match password {
            _ if !["passphrase", "password", "encrypt"].iter().any(|w| lower.contains(w)) => {
                // Should a tool ever echo the password.
                format!("Could not unpack {name}: {}", password.map_or(error.to_string(), |p| error.replace(p, "••••")).trim())
            }
            None => format!("{name} is password-protected, so it was left packed"),
            Some(_) if seven.is_none() && seven_or_rar => {
                format!("{name} is password-protected and was left packed: opening it needs 7-Zip ({SEVEN_ZIP_FROM})")
            }
            Some(_) => format!("{name} was left packed: the password did not open it"),
        }
    };
    // Never through a shell. On Windows 7-Zip reads it on its input when it asks, off its command
    // line, which other programs can read. ponytail: tar takes it only among its arguments, and so
    // does 7-Zip elsewhere (it would ask the terminal); a password holding `"` does not reach 7-Zip
    // whole there (it reads its command line its own way) and "did not open it".
    let typed = password.filter(|_| seven.is_some() && cfg!(windows));
    let pass: Vec<String> = match (password, &seven) {
        (Some(p), Some(_)) if typed.is_none() => vec![format!("-p{p}")],
        (Some(p), None) => vec!["--passphrase".into(), p.into()],
        _ => Vec::new(),
    };
    let mut listing = crate::media::quiet_command(&tool);
    match seven {
        Some(_) => listing.args(["l", "-slt", "-sccUTF-8"]).args(&pass).arg("--").arg(file),
        None => listing.arg("-tf").arg(file).args(&pass),
    };
    let listing = run_typing(&mut listing, typed).map_err(|e| failed(&e.to_string()))?;
    if !listing.status.success() {
        return Err(failed(&String::from_utf8_lossy(&listing.stderr)));
    }
    let listed = String::from_utf8_lossy(&listing.stdout);
    let entries: Vec<String> = match seven {
        // Its entries follow the archive's own lines, under a line of dashes; `\` on Windows.
        Some(_) => listed
            .split_once("\n----------")
            .map_or("", |(_, entries)| entries)
            .lines()
            .filter_map(|line| line.trim_end_matches('\r').strip_prefix("Path = "))
            .map(|path| path.replace('\\', "/"))
            .collect(),
        None => listed.lines().map(str::to_string).collect(),
    };
    if let Some(entry) = entries.iter().find(|e| crate::updater::escapes(e)) {
        return Err(format!("{name} was left packed: its entry \"{entry}\" would land outside its folder"));
    }
    let mut n = 0;
    let folder = loop {
        let folder = file.with_file_name(if n == 0 { stem.to_string() } else { format!("{stem} ({n})") });
        match std::fs::create_dir(&folder) {
            Ok(()) => break folder,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(failed(&e.to_string())),
        }
    };
    let unpacked = match seven {
        Some(_) => {
            let mut into = OsString::from("-o");
            into.push(&folder);
            let mut unpacking = crate::media::quiet_command(&tool);
            unpacking.args(["x", "-y", "-sccUTF-8"]).arg(into).args(&pass).arg("--").arg(file);
            run_typing(&mut unpacking, typed).and_then(|out| match out.status.success() {
                true => Ok(()),
                false => Err(std::io::Error::other(String::from_utf8_lossy(&out.stderr).into_owned())),
            })
        }
        // bsdtar takes the passphrase among the members too.
        None => crate::media::unpack(&tool, file, &folder, &pass),
    };
    let unpacked = unpacked.map_err(|e| failed(&e.to_string()));
    let unpacked = unpacked.and_then(|()| match has_links(&folder) {
        true => Err(format!("{name} was left packed: it holds links, which could lead outside its folder")),
        false => Ok(Some(folder.clone())),
    });
    if unpacked.is_err() {
        // Removes links themselves, never what they point to.
        let _ = std::fs::remove_dir_all(&folder);
    }
    unpacked
}

/// Runs `command` to its end with `line` typed on its input (it reads it only if it asks), else
/// with no input. Blocking.
fn run_typing(command: &mut std::process::Command, line: Option<&str>) -> std::io::Result<std::process::Output> {
    use std::io::Write;
    let input = if line.is_some() { Stdio::piped() } else { Stdio::null() };
    let mut child = command.stdin(input).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    if let (Some(mut stdin), Some(line)) = (child.stdin.take(), line) {
        // It may have finished without asking.
        let _ = writeln!(stdin, "{line}");
    }
    child.wait_with_output()
}

/// Where to get 7-Zip, as the user is told.
const SEVEN_ZIP_FROM: &str = if cfg!(target_os = "macos") { "brew install sevenzip" } else { "7-zip.org" };

/// 7-Zip's console program: on PATH (Homebrew's sevenzip is `7zz`, p7zip's `7z`), else where its
/// installer puts it on Windows.
fn seven_zip() -> Option<PathBuf> {
    let exe = format!("7z{}", std::env::consts::EXE_SUFFIX);
    let on_path = || crate::media::find_in_path("7zz").or_else(|| crate::media::find_in_path(&exe));
    on_path().or_else(|| {
        ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(|dir| PathBuf::from(dir).join("7-Zip").join(&exe))
            .find(|exe| exe.is_file())
    })
}

/// Whether the folder `dir` holds a symbolic link or junction, anywhere down.
pub(crate) fn has_links(dir: &Path) -> bool {
    std::fs::read_dir(dir).into_iter().flatten().flatten().any(|entry| match entry.file_type() {
        Ok(kind) if kind.is_symlink() => true,
        Ok(kind) if kind.is_dir() => has_links(&entry.path()),
        _ => false,
    })
}

/// Moves `file` into the category folder of its extension (see [`CATEGORIES`]) next to it,
/// numbered if the name is taken. Ok(None) if no category fits. The front ends turn sorting
/// off for a download in a folder of its own (see `ingest::Task::keeps_its_place`).
fn sort(file: &Path) -> std::io::Result<Option<PathBuf>> {
    let ext = file.extension().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    let Some((category, _)) = CATEGORIES.iter().find(|(_, exts)| exts.contains(&ext.as_str())) else { return Ok(None) };
    let (Some(parent), Some(name)) = (file.parent(), file.file_name()) else { return Ok(None) };
    let dir = parent.join(category);
    std::fs::create_dir_all(&dir)?;
    let base = dir.join(name);
    // ponytail: checks then renames, so a file appearing in between is replaced; claim the name
    // with a hard link if two downloads ever race for one.
    let target = (0..).map(|n| crate::engine::numbered(&base, n)).find(|p| !p.exists()).unwrap_or(base);
    std::fs::rename(file, &target)?;
    Ok(Some(target))
}

/// Points the history entry of `from` at `to`, where [`sort`] moved it. Best effort.
fn moved_in_history(from: &Path, to: &Path) {
    let (from, to) = (std::path::absolute(from).unwrap_or(from.into()), std::path::absolute(to).unwrap_or(to.into()));
    let history = DownloadHistoryManager::default_history_path();
    let loaded = DownloadHistoryManager::load_from_path(&history);
    let Some(mut entry) = loaded.entries().iter().rev().find(|e| e.file_path == from).cloned() else { return };
    entry.file_name = file_name(&to);
    entry.file_path = to;
    if let Err(e) = DownloadHistoryManager::record_replacing(&history, entry, &from) {
        tracing::warn!("Failed to update history file {:?}: {}", history, e);
    }
}

/// The SHA-256 of `file` in lower-case hex. Blocking.
fn sha256_of(file: &Path) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut std::fs::File::open(file)?, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Looks the file with this SHA-256 up at `api` (see [`VIRUSTOTAL`]) with `key`, through `proxy`,
/// and says what VirusTotal knows. Only the hash is sent, never the file. Errors become the note
/// too; the key never appears in one.
async fn virustotal(api: &str, sha256: &str, key: &str, proxy: Option<&str>) -> String {
    let Ok(mut key) = reqwest::header::HeaderValue::from_str(key.trim()) else {
        return "VirusTotal lookup failed: the API key is not valid".into();
    };
    key.set_sensitive(true);
    // Its error names the proxy, password and all: left out.
    let Ok(client) = crate::media::http_client(proxy) else {
        return "VirusTotal lookup failed: the proxy setting is not valid".into();
    };
    let request = client.get(format!("{api}{sha256}")).header("x-apikey", key).timeout(Duration::from_secs(30));
    let report = match request.send().await {
        Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => return "Not known to VirusTotal".into(),
        Ok(r) if !r.status().is_success() => return format!("VirusTotal lookup failed: HTTP {}", r.status()),
        Ok(r) => r.bytes().await,
        Err(e) => Err(e),
    };
    let report: serde_json::Value = match report {
        Ok(body) => serde_json::from_slice(&body).unwrap_or_default(),
        Err(e) => return format!("VirusTotal lookup failed: {}", e.without_url()),
    };
    let stats = report["data"]["attributes"]["last_analysis_stats"].as_object();
    let Some(stats) = stats else { return "VirusTotal lookup failed: unexpected answer".into() };
    let total: u64 = stats.values().filter_map(|v| v.as_u64()).sum();
    let flagged = stats.get("malicious").and_then(|v| v.as_u64()).unwrap_or(0);
    format!("VirusTotal: {flagged} of {total} engines flag this file")
}

/// `command` split once into words at whitespace; double or single quotes keep a word together
/// (and are dropped). Backslashes are plain characters, as in Windows paths.
fn split_command(command: &str) -> Vec<String> {
    let (mut words, mut word, mut quote) = (Vec::new(), None::<String>, None);
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.get_or_insert_default().push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                word.get_or_insert_default();
            }
            None if c.is_whitespace() => words.extend(word.take()),
            None => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// `word` with each placeholder of `values` (`{path}` and so on) replaced in one pass, so a value
/// that holds a placeholder's name stays as it is.
fn fill(word: &str, values: &[(&str, &str)]) -> String {
    let (mut out, mut rest) = (String::new(), word);
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        match values.iter().find(|(name, _)| rest.starts_with(name)) {
            Some((name, value)) => {
                out.push_str(value);
                rest = &rest[name.len()..];
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out + rest
}

/// Runs the user's `command` for the download at `path` of `source`: split into a program and its
/// arguments (see [`split_command`]), `{path}`, `{dir}`, `{name}` and `{url}` replaced inside
/// each, and started without a shell, so nothing in a file name is ever run. The same values go
/// in `ENDO_PATH`, `ENDO_DIR`, `ENDO_NAME` and `ENDO_URL`. Stopped after [`RUN_AFTER_LIMIT`].
/// A bare program name is looked up on PATH alone: Windows would look in the app's own folder
/// first, which may be the save folder (a portable copy in Downloads) a download could put it in.
/// Returns a note when it could not run or failed.
async fn run_after(command: &str, path: &Path, source: &Url) -> Option<String> {
    let path_text = path.to_string_lossy();
    let dir = path.parent().unwrap_or(Path::new("")).to_string_lossy();
    let name = file_name(path);
    let values = [("{path}", &*path_text), ("{dir}", &*dir), ("{name}", name.as_str()), ("{url}", source.as_str())];
    let mut words = split_command(command).into_iter().map(|word| fill(&word, &values));
    let mut program = PathBuf::from(words.next()?);
    if program.is_relative() && program.components().count() == 1 {
        let bare = program.to_string_lossy().into_owned();
        match crate::media::find_in_path(&bare).or_else(|| crate::media::find_in_path(&crate::media::exe_name(&bare))) {
            Some(found) => program = found,
            None => return Some(format!("Could not start the command after the download: {bare} is not on PATH")),
        }
    }
    let mut command = crate::media::quiet_command(&program);
    command.args(words).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    for ((_, value), env) in values.iter().zip(["ENDO_PATH", "ENDO_DIR", "ENDO_NAME", "ENDO_URL"]) {
        command.env(env, value);
    }
    let mut child = match tokio::process::Command::from(command).kill_on_drop(true).spawn() {
        Ok(child) => child,
        Err(e) => return Some(format!("Could not start the command after the download: {e}")),
    };
    match tokio::time::timeout(RUN_AFTER_LIMIT, child.wait()).await {
        Ok(Ok(status)) if status.success() => None,
        Ok(Ok(status)) => Some(format!("The command after the download failed ({status})")),
        Ok(Err(e)) => Some(format!("The command after the download failed: {e}")),
        Err(_) => Some("The command after the download was stopped after 10 minutes".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn only(post: PostOptions) -> PostOptions {
        PostOptions { mark_of_the_web: false, ..post }
    }

    /// A tar holding one file `name` with `data`, written by hand so the name can be anything.
    fn tar_with(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(b"0000644");
        header[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
        header[136..147].copy_from_slice(b"00000000000");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let mut out = header.to_vec();
        out.extend(data);
        out.resize(out.len().div_ceil(512) * 512 + 1024, 0);
        out
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn mark_of_the_web_names_the_source_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let source = url("https://user:pw@example.com/a.bin?token=hush");
        let done = after_download(&file, &PostOptions::default(), &source, None, None).await;
        assert_eq!(done, PostOutcome { path: file.clone(), notes: vec![] });
        let zone = std::fs::read_to_string(format!("{}:Zone.Identifier", file.display())).unwrap();
        assert!(zone.starts_with("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://example.com/a.bin?token="), "{zone}");
        assert!(!zone.contains("pw") && !zone.contains("hush"), "{zone}");
        // A MEGA folder's link keeps its key in the fragment.
        mark_from_internet(&file, &url("https://mega.nz/folder/abc#FOLDERKEY/file/xyz"));
        let zone = std::fs::read_to_string(format!("{}:Zone.Identifier", file.display())).unwrap();
        assert!(zone.ends_with("HostUrl=https://mega.nz/folder/abc\r\n"), "{zone}");
    }

    #[test]
    fn the_quarantine_is_a_browser_downloads() {
        assert_eq!(quarantine_value(0x6a00_0001), "0081;6a000001;Endo's Unified Downloader;");
    }

    /// On macOS each file downloaded, also those of a folder, is quarantined as a browser's are.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn downloads_are_quarantined_on_macos() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let (file, folder) = (dir.path().join("a.bin"), dir.path().join("f"));
        std::fs::write(&file, b"x").unwrap();
        let app = folder.join("X.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(folder.join("inner.txt"), b"y").unwrap();
        let done = after_download(&file, &PostOptions::default(), &url("https://example.com/a.bin"), None, None).await;
        assert_eq!(done, PostOutcome { path: file.clone(), notes: vec![] });
        mark_from_internet(&folder, &url("https://example.com/f.zip"));
        // Gatekeeper reads an app's quarantine from its bundle folder.
        for marked in [file, folder.join("inner.txt"), folder.clone(), app.clone(), app.join("Contents")] {
            let (path, name) = (std::ffi::CString::new(marked.as_os_str().as_bytes()).unwrap(), c"com.apple.quarantine");
            let mut value = [0u8; 256];
            // SAFETY: NUL-terminated strings and a buffer of the length given.
            let n = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), value.as_mut_ptr().cast(), value.len(), 0, 0) };
            assert!(n > 0, "{} is not quarantined", marked.display());
            let value = String::from_utf8_lossy(&value[..n as usize]).into_owned();
            assert!(value.starts_with("0081;") && value.ends_with(";Endo's Unified Downloader;"), "{value}");
        }
    }

    #[test]
    fn links_in_a_folder_are_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a").join("b")).unwrap();
        std::fs::write(dir.path().join("a").join("b").join("f"), b"x").unwrap();
        assert!(!has_links(dir.path()));
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(std::env::temp_dir(), dir.path().join("a").join("b").join("l"));
        #[cfg(not(windows))]
        let linked = std::os::unix::fs::symlink("/etc", dir.path().join("a").join("b").join("l"));
        // Windows makes links only with Developer Mode (or as administrator).
        if linked.is_ok() {
            assert!(has_links(dir.path()));
        }
    }

    #[tokio::test]
    async fn archives_with_escaping_entries_stay_packed() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("in");
        std::fs::create_dir(&inner).unwrap();
        let archive = inner.join("evil.tar");
        std::fs::write(&archive, tar_with("../evil.txt", b"gotcha")).unwrap();
        let post = only(PostOptions { extract: true, delete_archive: true, ..Default::default() });
        let done = after_download(&archive, &post, &url("https://example.com/evil.tar"), None, None).await;
        assert_eq!(done.path, archive);
        assert!(done.notes[0].contains("would land outside its folder"), "{:?}", done.notes);
        assert!(archive.is_file() && !inner.join("evil").exists() && !dir.path().join("evil.txt").exists());
    }

    #[tokio::test]
    async fn archives_unpack_into_a_new_folder_and_can_go() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("inner.txt"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("pack")).unwrap(); // taken, so the folder is numbered
        let name = if cfg!(windows) { "pack.zip" } else { "pack.tar.gz" };
        let archive = dir.path().join(name);
        let made = crate::media::quiet_command(&crate::media::system_tar())
            .arg("-a")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg("inner.txt")
            .status()
            .unwrap();
        assert!(made.success());
        let post = PostOptions { extract: true, delete_archive: true, ..Default::default() };
        // A password the archive does not need changes nothing.
        let done = after_download(&archive, &post, &url("https://example.com/pack"), None, Some("unused")).await;
        let folder = dir.path().join("pack (1)");
        assert_eq!(done, PostOutcome { path: folder.clone(), notes: vec![] });
        assert_eq!(std::fs::read(folder.join("inner.txt")).unwrap(), b"hello");
        assert!(!archive.exists());
        if cfg!(windows) {
            assert!(std::fs::metadata(format!("{}:Zone.Identifier", folder.join("inner.txt").display())).is_ok());
        }
    }

    /// An encrypted archive opens with its password only: a wrong one, or none, leaves it packed
    /// and says which, never with the password. A zip goes to tar; a .7z with its names encrypted
    /// too to 7-Zip (each made here when its tool is installed).
    #[tokio::test]
    async fn encrypted_archives_open_with_their_password() {
        use crate::media::quiet_command;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("inner.txt"), b"hello").unwrap();
        let mut made = Vec::new();
        // bsdtar (Windows' and macOS' tar) writes a zip encrypted the traditional way.
        let tar = if cfg!(target_os = "linux") { crate::media::find_in_path("bsdtar") } else { Some(crate::media::system_tar()) };
        if let Some(tar) = tar {
            let zip = dir.path().join("locked.zip");
            let options = ["--options", "zip:encryption=traditional", "--passphrase", "s3cret", "-C"];
            assert!(quiet_command(&tar).args(["-a", "-cf"]).arg(&zip).args(options).arg(&src).arg("inner.txt").status().unwrap().success());
            made.push(zip);
        }
        match seven_zip() {
            Some(seven) => {
                let archive = dir.path().join("locked.7z");
                let mut made_7z = quiet_command(&seven);
                made_7z.args(["a", "-ps3cret", "-mhe=on", "--"]).arg(&archive).arg(src.join("inner.txt")).stdout(Stdio::null());
                assert!(made_7z.status().unwrap().success());
                made.push(archive);
            }
            None => eprintln!("7-Zip is not installed: its part of the test is skipped"),
        }
        let source = url("https://example.com/locked");
        for archive in made {
            let name = file_name(&archive);
            let post = PostOptions { extract: true, mark_of_the_web: false, ..Default::default() };
            let wrong = after_download(&archive, &post, &source, None, Some("n0pe")).await;
            assert_eq!(wrong, PostOutcome { path: archive.clone(), notes: vec![format!("{name} was left packed: the password did not open it")] });
            let none = after_download(&archive, &post, &source, None, None).await;
            assert_eq!(none, PostOutcome { path: archive.clone(), notes: vec![format!("{name} is password-protected, so it was left packed")] });
            assert!(!dir.path().join("locked").exists(), "{name}: a failed unpacking leaves no folder");
            let right = after_download(&archive, &post, &source, None, Some("s3cret")).await;
            assert!(right.notes.is_empty(), "{name}: {:?}", right.notes);
            assert_eq!(std::fs::read(right.path.join("inner.txt")).unwrap(), b"hello", "{name}");
            std::fs::remove_dir_all(&right.path).unwrap();
        }
    }

    #[test]
    fn split_and_odd_archives_are_told_apart() {
        for name in ["a.part2.rar", "a.7z.001", "a.zip.002", "a.r00", "a.z01"] {
            assert!(split_part(name), "{name}");
        }
        for name in ["a.rar", "a.part.rar", "a.7z", "a.mp4", "r00"] {
            assert!(!split_part(name), "{name}");
        }
        assert_eq!(archive_stem("Show.S01.TAR.GZ"), Some("Show.S01"));
        assert_eq!(archive_stem("a.mp4"), None);
        assert_eq!(archive_stem(".zip"), None);
    }

    #[tokio::test]
    async fn sorting_moves_files_into_numbered_category_folders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Video")).unwrap();
        std::fs::write(dir.path().join("Video").join("clip.mp4"), b"old").unwrap();
        let clip = dir.path().join("clip.mp4");
        std::fs::write(&clip, b"new").unwrap();
        let post = only(PostOptions { sort: true, ..Default::default() });
        let source = url("https://example.com/clip.mp4");
        let done = after_download(&clip, &post, &source, None, None).await;
        assert_eq!(done.path, dir.path().join("Video").join("clip (1).mp4"));
        assert_eq!(std::fs::read(&done.path).unwrap(), b"new");
        assert_eq!(std::fs::read(dir.path().join("Video").join("clip.mp4")).unwrap(), b"old");

        let book = dir.path().join("Book.PDF");
        std::fs::write(&book, b"pdf").unwrap();
        assert_eq!(after_download(&book, &post, &source, None, None).await.path, dir.path().join("Documents").join("Book.PDF"));
        let odd = dir.path().join("thing.xyz");
        std::fs::write(&odd, b"?").unwrap();
        assert_eq!(after_download(&odd, &post, &source, None, None).await.path, odd);
        let folder = dir.path().join("Torrent");
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(after_download(&folder, &post, &source, None, None).await.path, folder);
    }

    #[test]
    fn commands_split_once_and_fill_placeholders_in_place() {
        assert_eq!(
            split_command(r#"C:\Tools\scan.exe --in "{path}" 'a b'  x"y z"w "#),
            [r"C:\Tools\scan.exe", "--in", "{path}", "a b", "xy zw"]
        );
        assert_eq!(split_command(r#""" a"#), ["", "a"]);
        let values = [("{path}", "C:/d/{name} & del.txt"), ("{name}", "N")];
        assert_eq!(fill("--file={path};{name}{x}", &values), "--file=C:/d/{name} & del.txt;N{x}");
    }

    #[tokio::test]
    async fn commands_run_without_a_shell() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("my file.txt");
        std::fs::write(&file, b"data").unwrap();
        let tar = crate::media::system_tar();
        let source = url("https://example.com/my%20file.txt");
        let ok = format!(r#""{}" -cf "{{dir}}/out.tar" -C "{{dir}}" "{{name}}""#, tar.display());
        assert_eq!(run_after(&ok, &file, &source).await, None);
        assert!(dir.path().join("out.tar").is_file());

        let sneaky = format!(r#""{}" -cf "{{dir}}/two.tar" -C "{{dir}}" "{{name}}" & echo pwned > "{{dir}}/pwned.txt""#, tar.display());
        let note = run_after(&sneaky, &file, &source).await.unwrap();
        assert!(note.starts_with("The command after the download failed"), "{note}");
        assert!(!dir.path().join("pwned.txt").exists());

        let missing = run_after("no-such-program-endo", &file, &source).await.unwrap();
        assert!(missing.starts_with("Could not start"), "{missing}");
        // A bare name is found on PATH (cmd.exe in System32 on Windows).
        assert_eq!(run_after(if cfg!(windows) { "cmd /c exit 0" } else { "true" }, &file, &source).await, None);
    }

    /// Answers one request with `status` and `body`; the request comes back from the handle.
    async fn answer_once(status: &str, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}/api/v3/files/", listener.local_addr().unwrap());
        let status = status.to_string();
        let served = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let n = socket.read(&mut request).await.unwrap();
            let reply = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            socket.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request[..n]).into_owned()
        });
        (api, served)
    }

    #[tokio::test]
    async fn virustotal_reports_the_hash_lookup() {
        let body = r#"{"data":{"attributes":{"last_analysis_stats":{"malicious":2,"suspicious":1,"undetected":60,"harmless":0}}}}"#;
        let (api, served) = answer_once("200 OK", body).await;
        assert_eq!(virustotal(&api, "abc123", "sekret", None).await, "VirusTotal: 2 of 63 engines flag this file");
        let request = served.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /api/v3/files/abc123 ") && request.contains("x-apikey: sekret"), "{request}");

        let (api, _served) = answer_once("404 Not Found", "{}").await;
        assert_eq!(virustotal(&api, "abc123", "sekret", None).await, "Not known to VirusTotal");
        let (api, _served) = answer_once("401 Unauthorized", "{}").await;
        let note = virustotal(&api, "abc123", "sekret", None).await;
        assert!(note.contains("401") && !note.contains("sekret"), "{note}");
    }

    #[test]
    fn hashes_files_as_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        std::fs::write(&file, b"abc").unwrap();
        assert_eq!(sha256_of(&file).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
