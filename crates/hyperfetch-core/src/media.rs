use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::EngineSnapshot;

/// Progress update emitted during media downloads
#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub downloaded: u64,
    pub total: u64,
    pub speed: f64,
    pub eta_seconds: Option<u64>,
    pub active_connections: usize,
}

/// Supported media quality presets
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MediaQualityPreset {
    /// Best available video and audio merged into MP4
    #[default]
    BestVideoAudio,
    /// Up to 1080p video with best audio merged into MP4
    Fhd1080p,
    /// Up to 720p video with best audio merged into MP4
    Hd720p,
    /// Extract audio only and convert to MP3
    AudioMp3,
    /// Extract audio only as high quality M4A / AAC
    AudioM4a,
    /// Custom yt-dlp format selector string
    Custom(String),
}

/// Keeps ffmpeg from rewriting a merged mp4 a second time just to move its index (the moov atom)
/// to the front. yt-dlp asks for that pass on every output (`-movflags +faststart`); a later
/// `-movflags` replaces it. Players read the index wherever it is; only a file streamed over the
/// web while it downloads wants it in front.
const NO_FASTSTART: [&str; 2] = ["--postprocessor-args", "Merger+ffmpeg_o:-movflags -faststart"];

impl MediaQualityPreset {
    /// Convert preset to yt-dlp arguments
    pub fn to_args(&self) -> Vec<String> {
        let mp4 = |format: &str| {
            ["-f", format, "--merge-output-format", "mp4", NO_FASTSTART[0], NO_FASTSTART[1]].map(String::from).to_vec()
        };
        match self {
            Self::BestVideoAudio => mp4("bv*+ba/b"),
            Self::Fhd1080p => mp4("bv*[height<=1080]+ba/b[height<=1080]/best"),
            Self::Hd720p => mp4("bv*[height<=720]+ba/b[height<=720]/best"),
            Self::AudioMp3 => vec![
                "-x".to_string(),
                "--audio-format".to_string(),
                "mp3".to_string(),
            ],
            // An AAC source is only copied into the .m4a. The best audio is often Opus, which
            // ffmpeg would have to re-encode: slower, and a second lossy encode.
            Self::AudioM4a => vec![
                "-f".to_string(),
                "ba[acodec^=mp4a]/ba/b".to_string(),
                "-x".to_string(),
                "--audio-format".to_string(),
                "m4a".to_string(),
            ],
            Self::Custom(fmt) => vec![
                "-f".to_string(),
                fmt.clone(),
            ],
        }
    }

    /// Whether yt-dlp converts the download to an audio file.
    fn extracts_audio(&self) -> bool {
        matches!(self, Self::AudioMp3 | Self::AudioM4a)
    }
}

/// Browser cookie sources for bypassing age gates and bot verification
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BrowserCookieSource {
    #[default]
    None,
    Chrome,
    Edge,
    Firefox,
    Brave,
    Opera,
    Vivaldi,
    File(PathBuf),
}

impl BrowserCookieSource {
    pub fn to_args(&self) -> Vec<String> {
        match (self, self.browser()) {
            (Self::File(p), _) => vec!["--cookies".to_string(), p.to_string_lossy().to_string()],
            (_, Some(browser)) => vec!["--cookies-from-browser".to_string(), browser.to_string()],
            _ => vec![],
        }
    }

    /// The browser, as yt-dlp's `--cookies-from-browser` names it.
    fn browser(&self) -> Option<&'static str> {
        match self {
            Self::None | Self::File(_) => None,
            Self::Chrome => Some("chrome"),
            Self::Edge => Some("edge"),
            Self::Firefox => Some("firefox"),
            Self::Brave => Some("brave"),
            Self::Opera => Some("opera"),
            Self::Vivaldi => Some("vivaldi"),
        }
    }
}

/// Options for configuring a media download
#[derive(Debug, Clone)]
pub struct MediaDownloadOptions {
    pub preset: MediaQualityPreset,
    pub cookies: BrowserCookieSource,
    pub proxy: Option<String>,
    pub output_dir: PathBuf,
    pub output_filename: Option<String>,
    pub custom_ytdlp_path: Option<PathBuf>,
    pub concurrent_fragments: usize,
}

impl Default for MediaDownloadOptions {
    fn default() -> Self {
        Self {
            preset: MediaQualityPreset::default(),
            cookies: BrowserCookieSource::default(),
            proxy: None,
            output_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            output_filename: None,
            custom_ytdlp_path: None,
            concurrent_fragments: 8,
        }
    }
}

const CANCELLED: &str = "Download cancelled by user";

/// yt-dlp's GitHub releases. `/latest` redirects to `/tag/<version>`; the files of a release are
/// under `/download/<version>/`.
const RELEASES: &str = "https://github.com/yt-dlp/yt-dlp/releases";

/// Process creation flag that keeps a console program from opening a window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// First yt-dlp release that understands `--js-runtimes`; older builds abort on the unknown flag.
const JS_RUNTIMES_MIN_VERSION: (u32, u32, u32) = (2025, 11, 12);
/// First yt-dlp release that understands `--no-plugin-dirs`.
const NO_PLUGIN_DIRS_MIN_VERSION: (u32, u32, u32) = (2025, 3, 21);

// yt-dlp prints these machine-readable lines for us (see `build_ytdlp_args`).
// Fields: status, downloaded, total, total estimate ('~' sizes), speed, eta, stream file name.
// The file name goes last because it may contain spaces. Missing values print as "NA".
const PROGRESS_TEMPLATE: &str = "download:HFP %(progress.status)s %(progress.downloaded_bytes)s \
    %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s \
    %(progress.filename)s";
const PROGRESS_MARK: &str = "HFP ";
/// Announced size of each video about to download (merged formats sum their streams).
const PLANNED_TEMPLATE: &str = "before_dl:HFTOTAL %(filesize,filesize_approx)s";
const PLANNED_MARK: &str = "HFTOTAL ";
/// Printed after the downloads of a video, right before merging / audio extraction.
const POSTPROCESS_TEMPLATE: &str = "post_process:HFPOST %(id)s";
const POSTPROCESS_MARK: &str = "HFPOST";
/// Final path of each video once every post-processor has run and the file has been moved.
const PATH_TEMPLATE: &str = "after_move:HFPATH %(filepath)s";
const PATH_MARK: &str = "HFPATH ";

/// Turns YouTube's formats into 10 MiB range fragments that `--concurrent-fragments` fetches in
/// parallel; otherwise one connection pulls the 10 MiB pieces one after another. Fragments report
/// progress against an estimated total, which the progress parser reads. A format whose size
/// YouTube does not give (the muxed format 18, often) has no fragments and is dropped altogether,
/// so only our presets get it: they pick adaptive formats, which have sizes, while a custom format
/// selection may name exactly such a format.
const YOUTUBE_DASHY: [&str; 2] = ["--extractor-args", "youtube:formats=dashy"];

/// Number of trailing stderr lines kept to explain a failure without an `ERROR:` line.
const STDERR_TAIL_LINES: usize = 5;

/// Hosts (and their subdomains) that are best handled by the Media Engine.
const MEDIA_DOMAINS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "youtube-nocookie.com",
    "twitch.tv",
    "tiktok.com",
    "twitter.com",
    "x.com",
    "vimeo.com",
    "soundcloud.com",
    "reddit.com",
    "v.redd.it",
    "instagram.com",
    "facebook.com",
    "fb.watch",
    "dailymotion.com",
    "dai.ly",
    "bilibili.com",
];

/// Check if a given URL is a known media site best handled by the Media Engine
pub fn is_supported_media_site(url: &Url) -> bool {
    let host = url.host_str().unwrap_or("").trim_end_matches('.').to_ascii_lowercase();
    // Bare redd.it is Reddit's post shortener. Only its v. subdomain serves video:
    // i.redd.it / preview.redd.it are plain image files the regular engine handles.
    host == "redd.it"
        || MEDIA_DOMAINS.iter().any(|domain| {
            host == *domain || host.strip_suffix(domain).is_some_and(|sub| sub.ends_with('.'))
        })
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn next_to_current_exe(name: &str) -> Option<PathBuf> {
    let candidate = std::env::current_exe().ok()?.parent()?.join(name);
    candidate.is_file().then_some(candidate)
}

/// Searches the absolute PATH entries; a relative entry would depend on the current directory.
fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Per-user application data directory.
fn app_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let data_dir = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share")));
    Some(data_dir?.join("EndosUnifiedDownloader"))
}

/// Per-user directory the managed yt-dlp is installed into (always writable, unlike the
/// application directory under Program Files or /usr/local/bin).
fn managed_bin_dir() -> Option<PathBuf> {
    Some(app_data_dir()?.join("bin"))
}

/// `p` made absolute against the current directory. An empty path (the parent of a bare file
/// name) is the current directory itself.
fn absolute(p: &Path) -> Result<PathBuf, String> {
    let p = if p.as_os_str().is_empty() { Path::new(".") } else { p };
    std::path::absolute(p).map_err(|e| format!("Invalid path {}: {e}", p.display()))
}

/// Empty per-user directory yt-dlp runs in, so nothing in the user's current directory
/// (such as a planted `yt-dlp.conf`) can influence it.
fn ytdlp_work_dir() -> PathBuf {
    app_data_dir().map_or_else(std::env::temp_dir, |dir| dir.join("ytdlp-work"))
}

/// The managed yt-dlp, if one is installed: on Windows the unpacked build of the newest release
/// installed (see [`install_onedir`]), else a single-file build (the only kind elsewhere, and what
/// Windows installs used to be). Blocking.
fn installed_managed_ytdlp() -> Option<PathBuf> {
    let dir = managed_bin_dir()?;
    #[cfg(windows)]
    if let Some(exe) = newest_release(&dir) {
        return Some(exe);
    }
    Some(dir.join(exe_name("yt-dlp"))).filter(|p| p.is_file())
}

/// Discover the path to yt-dlp executable: next to the application, then the managed install,
/// then pip installs, well-known directories and PATH. Call it from a blocking context. Only the
/// search past the managed install is cached for the process lifetime (it stats every PATH entry
/// the first time): the managed install moves when it updates.
pub fn find_ytdlp_path() -> Option<PathBuf> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    next_to_current_exe(&exe_name("yt-dlp"))
        .or_else(installed_managed_ytdlp)
        .or_else(|| CACHE.get_or_init(discover_ytdlp).clone())
}

/// Installs of yt-dlp other than ours (see [`find_ytdlp_path`]).
fn discover_ytdlp() -> Option<PathBuf> {
    let name = exe_name("yt-dlp");

    #[cfg(windows)]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        // pip installs into %LOCALAPPDATA%\Programs\Python\PythonXY\Scripts
        let python_root = PathBuf::from(local_app_data).join("Programs").join("Python");
        if let Ok(entries) = std::fs::read_dir(&python_root) {
            for entry in entries.flatten() {
                let candidate = entry.path().join("Scripts").join(&name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            let local_bin = PathBuf::from(home).join(".local").join("bin").join(&name);
            if local_bin.is_file() {
                return Some(local_bin);
            }
        }
        for dir in ["/usr/local/bin", "/usr/bin", "/opt/homebrew/bin", "/usr/local/share/yt-dlp"] {
            let candidate = PathBuf::from(dir).join(&name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    find_in_path(&name)
}

/// Discover a JavaScript runtime (Node.js, Deno, Bun) for solving YouTube challenges, as a
/// `kind:path` value for `--js-runtimes`. Cached like [`find_ytdlp_path`].
pub fn find_js_runtime() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE.get_or_init(discover_js_runtime).clone()
}

fn discover_js_runtime() -> Option<String> {
    #[cfg(windows)]
    for dir in [r"C:\Program Files\nodejs", r"C:\Program Files (x86)\nodejs"] {
        let node = Path::new(dir).join("node.exe");
        if node.is_file() {
            return Some(format!("node:{}", node.display()));
        }
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var).filter(|dir| dir.is_absolute()) {
            for kind in ["node", "deno", "bun"] {
                let candidate = dir.join(exe_name(kind));
                if candidate.is_file() {
                    return Some(format!("{kind}:{}", candidate.display()));
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            for (kind, sub) in [("node", ".local/bin/node"), ("bun", ".bun/bin/bun"), ("deno", ".deno/bin/deno")] {
                let candidate = PathBuf::from(&home).join(sub);
                if candidate.is_file() {
                    return Some(format!("{kind}:{}", candidate.display()));
                }
            }
        }
        for (kind, path) in [("node", "/usr/bin/node"), ("node", "/usr/local/bin/node"), ("deno", "/usr/local/bin/deno"), ("bun", "/usr/local/bin/bun")] {
            if Path::new(path).is_file() {
                return Some(format!("{kind}:{path}"));
            }
        }
    }

    None
}

/// Discover the path to ffmpeg executable. Cached like [`find_ytdlp_path`].
pub fn find_ffmpeg_path() -> Option<PathBuf> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE.get_or_init(discover_ffmpeg).clone()
}

fn discover_ffmpeg() -> Option<PathBuf> {
    let name = exe_name("ffmpeg");
    if let Some(candidate) = next_to_current_exe(&name) {
        return Some(candidate);
    }
    if let Some(candidate) = managed_bin_dir().map(|dir| dir.join(&name)).filter(|p| p.is_file()) {
        return Some(candidate);
    }
    if let Some(candidate) = find_in_path(&name) {
        return Some(candidate);
    }

    #[cfg(windows)]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        // `winget install Gyan.FFmpeg` links ffmpeg into WinGet\Links (which may not be on
        // PATH for an already-running app) and unpacks it to Packages\Gyan.FFmpeg*\<build>\bin.
        let winget = PathBuf::from(local_app_data).join("Microsoft").join("WinGet");
        let link = winget.join("Links").join(&name);
        if link.is_file() {
            return Some(link);
        }
        let packages = std::fs::read_dir(winget.join("Packages")).into_iter().flatten().flatten();
        for package in packages.filter(|p| p.file_name().to_string_lossy().starts_with("Gyan.FFmpeg")) {
            for build in std::fs::read_dir(package.path()).into_iter().flatten().flatten() {
                let candidate = build.path().join("bin").join(&name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            let local_bin = PathBuf::from(home).join(".local").join("bin").join(&name);
            if local_bin.is_file() {
                return Some(local_bin);
            }
        }
        for dir in ["/usr/local/bin", "/usr/bin", "/opt/homebrew/bin"] {
            let candidate = PathBuf::from(dir).join(&name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

/// The yt-dlp build installed on this platform, as named in the GitHub release. On Windows the
/// unpacked ("onedir") build: the single-file one unpacks its Python runtime to %TEMP% on every
/// launch, about 0.7 s more per run.
fn ytdlp_release_asset() -> &'static str {
    if cfg!(all(windows, target_arch = "aarch64")) {
        "yt-dlp_win_arm64.zip"
    } else if cfg!(all(windows, target_arch = "x86")) {
        "yt-dlp_win_x86.zip"
    } else if cfg!(windows) {
        "yt-dlp_win.zip"
    } else if cfg!(target_os = "macos") {
        "yt-dlp_macos"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "yt-dlp_linux"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "yt-dlp_linux_aarch64"
    } else {
        // Zipimport build; needs a system python3.
        "yt-dlp"
    }
}

/// Look up `asset` in a `sha256sum`-style listing ("<hex>  <name>" per line).
fn expected_sha256<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == asset).then_some(hash)
    })
}

async fn http_get(client: &reqwest::Client, url: &str, timeout: Duration) -> Result<reqwest::Response, String> {
    let resp = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("Failed to download {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url} returned status {}", resp.status()));
    }
    Ok(resp)
}

/// The tag (version) of the newest stable yt-dlp release.
async fn latest_release_tag(client: &reqwest::Client) -> Result<String, String> {
    let resp = client
        .head(format!("{RELEASES}/latest"))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("Failed to check for a yt-dlp update: {e}"))?;
    let tag = resp
        .url()
        .path()
        .rsplit_once("/releases/tag/")
        .map(|(_, tag)| tag.to_string())
        .ok_or_else(|| format!("Unexpected yt-dlp release URL {}", resp.url()))?;
    // The tag goes into URLs and, on Windows, names the install's folder.
    if version_parts(&tag).is_none() {
        return Err(format!("Unexpected yt-dlp release tag {tag}"));
    }
    Ok(tag)
}

/// Download the latest official yt-dlp build into the per-user managed directory, verified
/// against the release's SHA2-256SUMS and moved into place atomically, so a failed or
/// interrupted download never leaves a broken yt-dlp behind. Returns the program to run.
pub async fn download_ytdlp_binary(client: &reqwest::Client) -> Result<PathBuf, String> {
    let tag = latest_release_tag(client).await?;
    install_release(client, &tag).await
}

/// Installs release `tag` for this platform like [`download_ytdlp_binary`]. On Windows a release
/// already unpacked (by a concurrent job or another process) is used as it is.
async fn install_release(client: &reqwest::Client, tag: &str) -> Result<PathBuf, String> {
    let bin_dir = managed_bin_dir().ok_or("Cannot determine a per-user directory to install yt-dlp into")?;
    #[cfg(windows)]
    {
        let exe = release_dir(&bin_dir, tag).join("yt-dlp.exe");
        if tokio::fs::metadata(&exe).await.is_ok_and(|m| m.is_file()) {
            return Ok(exe);
        }
    }
    let asset = ytdlp_release_asset();
    let release = format!("{RELEASES}/download/{tag}");

    let sums_url = format!("{release}/SHA2-256SUMS");
    let sums = http_get(client, &sums_url, Duration::from_secs(30))
        .await?
        .text()
        .await
        .map_err(|e| format!("Failed to read {sums_url}: {e}"))?;
    let expected = expected_sha256(&sums, asset)
        .ok_or_else(|| format!("yt-dlp SHA2-256SUMS has no entry for {asset}"))?
        .to_ascii_lowercase();

    let asset_url = format!("{release}/{asset}");
    let bytes = http_get(client, &asset_url, Duration::from_secs(600))
        .await?
        .bytes()
        .await
        .map_err(|e| format!("Failed to read {asset_url}: {e}"))?;

    let tag = tag.to_string();
    tokio::task::spawn_blocking(move || install_verified(&bin_dir, &tag, &bytes, &expected))
        .await
        .map_err(|e| format!("yt-dlp install task failed: {e}"))?
}

/// Check `bytes` (the release asset) against `expected_sha256`, then install them into `bin_dir`:
/// unpacked on Windows (see [`install_onedir`]), elsewhere as one file written next to its target
/// and renamed into place. Returns the program to run. Blocking.
fn install_verified(bin_dir: &Path, tag: &str, bytes: &[u8], expected_sha256: &str) -> Result<PathBuf, String> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected_sha256 {
        return Err(format!(
            "Downloaded yt-dlp failed checksum verification (expected {expected_sha256}, got {actual})"
        ));
    }
    std::fs::create_dir_all(bin_dir).map_err(|e| format!("Failed to create {}: {e}", bin_dir.display()))?;
    #[cfg(windows)]
    return install_onedir(bin_dir, tag, bytes);
    #[cfg(not(windows))]
    {
        let _ = tag;
        install_file(&bin_dir.join("yt-dlp"), bytes)
    }
}

/// Writes `bytes` next to `target` and renames them into place.
#[cfg(not(windows))]
fn install_file(target: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let file_name = target.file_name().ok_or("Invalid yt-dlp install path")?.to_string_lossy();
    let tmp = target.with_file_name(format!(".{file_name}.{}.tmp", unique_suffix()));
    let installed = std::fs::write(&tmp, bytes)
        .and_then(|_| make_executable(&tmp))
        .and_then(|_| std::fs::rename(&tmp, target));
    if let Err(e) = installed {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Failed to install yt-dlp to {}: {e}", target.display()));
    }
    Ok(target.to_path_buf())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(all(not(unix), not(windows)))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Folder the unpacked build of release `tag` is installed in.
#[cfg(windows)]
fn release_dir(bin_dir: &Path, tag: &str) -> PathBuf {
    bin_dir.join(format!("yt-dlp-{tag}"))
}

/// `yt-dlp.exe` of the newest release unpacked in `bin_dir`.
#[cfg(windows)]
fn newest_release(bin_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(bin_dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let version = version_parts(entry.file_name().to_str()?.strip_prefix("yt-dlp-")?)?;
            let exe = entry.path().join("yt-dlp.exe");
            exe.is_file().then_some((version, exe))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, exe)| exe)
}

/// Unpacks the release zip into its own `yt-dlp-<tag>` folder. The folder is unpacked under a
/// temporary name and appears complete in one rename, which is what switches the managed yt-dlp
/// to this release (see [`newest_release`]); a running yt-dlp keeps running from its own folder,
/// which Windows would not let us replace anyway. Older releases are cleared away afterwards (see
/// [`remove_old_releases`]). Blocking.
#[cfg(windows)]
fn install_onedir(bin_dir: &Path, tag: &str, zip: &[u8]) -> Result<PathBuf, String> {
    let target = release_dir(bin_dir, tag);
    let temp = format!(".yt-dlp-{tag}-{}", unique_suffix());
    let (archive, unpacked) = (bin_dir.join(format!("{temp}.zip")), bin_dir.join(format!("{temp}.tmp")));
    let fail = |e: std::io::Error| format!("Failed to install yt-dlp to {}: {e}", target.display());
    let extracted = std::fs::write(&archive, zip)
        .and_then(|()| std::fs::create_dir(&unpacked))
        .and_then(|()| unzip(&archive, &unpacked));
    let _ = std::fs::remove_file(&archive);
    let complete = extracted.map_err(fail).and_then(|()| {
        if unpacked.join("yt-dlp.exe").is_file() {
            Ok(())
        } else {
            Err(format!("The yt-dlp release {tag} has no yt-dlp.exe"))
        }
    });
    if let Err(e) = complete {
        let _ = std::fs::remove_dir_all(&unpacked);
        return Err(e);
    }
    if let Err(e) = rename_patiently(&unpacked, &target) {
        let _ = std::fs::remove_dir_all(&unpacked);
        // Another process installed the same release meanwhile.
        if !target.join("yt-dlp.exe").is_file() {
            return Err(fail(e));
        }
    }
    remove_old_releases(bin_dir);
    Ok(target.join("yt-dlp.exe"))
}

/// Renames folder `from` to `to`, trying again for a moment while Windows refuses. Antivirus
/// scanners open new programs right after they are written, and a folder with an open file in it
/// cannot be renamed. Gives up at once once `to` exists (another process installed the same
/// release). Blocking.
#[cfg(windows)]
fn rename_patiently(from: &Path, to: &Path) -> std::io::Result<()> {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    let mut pause = Duration::from_millis(100);
    // 100 + 200 + 400 + 800 ms of waiting at most.
    for _ in 0..4 {
        match std::fs::rename(from, to) {
            Err(e)
                if (e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
                    && !to.exists() =>
            {
                std::thread::sleep(pause);
                pause *= 2;
            }
            done => return done,
        }
    }
    std::fs::rename(from, to)
}

/// Extracts `archive` into `into` with the tar.exe that ships with Windows (10 1803 and later),
/// named by full path so no other tar on PATH is used. It refuses entries that would land outside
/// `into`. Blocking.
#[cfg(windows)]
fn unzip(archive: &Path, into: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    let windows = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let output = std::process::Command::new(windows.join("System32").join("tar.exe"))
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(std::io::Error::other(format!("tar exited with {}: {}", output.status, stderr.trim())))
    }
}

/// Deletes what earlier installs left in `bin_dir`, except the two newest releases: a job that
/// found the previous one before this install may still start it. Older releases (the single-file
/// build counts as the oldest) go unless a program runs from them, and temporary files once they
/// are an hour old. A release folder is renamed away before it is deleted, so no half-deleted
/// folder ever passes for a release. Blocking.
#[cfg(windows)]
fn remove_old_releases(bin_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(bin_dir) else { return };
    let stale = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > Duration::from_secs(3600)))
    };
    let mut releases = Vec::new();
    for entry in entries.flatten() {
        let (path, name) = (entry.path(), entry.file_name().to_string_lossy().into_owned());
        if name.eq_ignore_ascii_case("yt-dlp.exe") {
            releases.push((Vec::new(), path));
        } else if let Some(version) = name.strip_prefix("yt-dlp-").and_then(version_parts).filter(|_| path.is_dir()) {
            releases.push((version, path));
        } else if name.starts_with(".yt-dlp-") && stale(&path) {
            let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        }
    }
    releases.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in releases.into_iter().skip(2) {
        if !path.is_dir() {
            if !is_running(&path) {
                let _ = std::fs::remove_file(&path);
            }
        } else if !is_running(&path.join("yt-dlp.exe")) {
            let retired = bin_dir.join(format!(".yt-dlp-retired-{}", unique_suffix()));
            if std::fs::rename(&path, &retired).is_ok() {
                let _ = std::fs::remove_dir_all(&retired);
            }
        }
    }
}

/// Whether a program runs from `exe`: Windows refuses to open a running program for writing.
#[cfg(windows)]
fn is_running(exe: &Path) -> bool {
    exe.is_file() && std::fs::OpenOptions::new().write(true).open(exe).is_err()
}

fn http_client(proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("EndosUnifiedDownloader/", env!("CARGO_PKG_VERSION")));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| format!("Invalid proxy {proxy}: {e}"))?);
    }
    builder.build().map_err(|e| format!("Failed to build HTTP client: {e}"))
}

/// Serializes installs and updates of the managed yt-dlp across concurrent media jobs.
static INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn install_managed_ytdlp(proxy: Option<&str>) -> Result<PathBuf, String> {
    let _guard = INSTALL_LOCK.lock().await;
    // A concurrent job may have installed it while we waited for the lock.
    if let Ok(Some(installed)) = tokio::task::spawn_blocking(installed_managed_ytdlp).await {
        return Ok(installed);
    }
    download_ytdlp_binary(&http_client(proxy)?).await
}

/// Installs the latest release as the managed yt-dlp if `current` is older, and returns the
/// program to run from now on, if it changed.
async fn update_managed_ytdlp(proxy: Option<&str>, current: Option<&str>) -> Result<Option<PathBuf>, String> {
    let client = http_client(proxy)?;
    let latest = latest_release_tag(&client).await?;
    if current == Some(latest.as_str()) {
        return Ok(None);
    }

    let _guard = INSTALL_LOCK.lock().await;
    let installed = install_release(&client, &latest).await?;
    *VERSION_CACHE.lock() = None;
    tracing::info!("Updated managed yt-dlp from {} to {latest}", current.unwrap_or("an unknown version"));
    Ok(Some(installed))
}

/// A name no other call in any process uses: time, process id and a counter.
fn unique_suffix() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed))
}

/// One yt-dlp build: another build at the same path differs in size or modification time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct BinaryId {
    path: PathBuf,
    size: u64,
    /// Seconds and nanoseconds since the Unix epoch.
    modified: (u64, u32),
}

impl BinaryId {
    /// Blocking.
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Self { path: path.to_path_buf(), size: meta.len(), modified: (modified.as_secs(), modified.subsec_nanos()) })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedVersion {
    binary: BinaryId,
    version: String,
}

/// Most yt-dlp builds whose version the cache file remembers.
const VERSION_CACHE_ENTRIES: usize = 8;

fn version_cache_file() -> Option<PathBuf> {
    Some(app_data_dir()?.join("ytdlp-version.json"))
}

/// The version `file` records for exactly this build. Blocking.
fn cached_version(file: &Path, binary: &BinaryId) -> Option<String> {
    let entries: Vec<CachedVersion> = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    entries.into_iter().find(|e| e.binary == *binary).map(|e| e.version)
}

/// Records `version` for `binary` in `file`, in place of what it held for that path. The file is
/// replaced in one rename, so a reader never sees half of it. Blocking.
fn store_version(file: &Path, binary: &BinaryId, version: &str) -> std::io::Result<()> {
    let mut entries: Vec<CachedVersion> =
        std::fs::read(file).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
    entries.retain(|e| e.binary.path != binary.path);
    entries.insert(0, CachedVersion { binary: binary.clone(), version: version.to_string() });
    entries.truncate(VERSION_CACHE_ENTRIES);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", unique_suffix()));
    let tmp = PathBuf::from(tmp);
    let written = serde_json::to_vec(&entries)
        .map_err(std::io::Error::other)
        .and_then(|json| std::fs::write(&tmp, json))
        .and_then(|()| std::fs::rename(&tmp, file));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Last probed `yt-dlp --version`, with the build it came from.
static VERSION_CACHE: parking_lot::Mutex<Option<(BinaryId, String)>> = parking_lot::const_mutex(None);

/// `yt-dlp --version` of `bin`. Asking costs a launch (a second or more on Windows), so the answer
/// is kept in memory and in `cache_file` for as long as the build at `bin` keeps its size and
/// modification time.
async fn ytdlp_version(bin: &Path, work_dir: &Path, cache_file: Option<&Path>) -> Option<String> {
    let binary = {
        let bin = bin.to_path_buf();
        tokio::task::spawn_blocking(move || BinaryId::of(&bin)).await.ok().flatten()
    };
    if let Some(binary) = &binary {
        let cached = VERSION_CACHE.lock().clone();
        if let Some((_, version)) = cached.filter(|(cached, _)| cached == binary) {
            return Some(version);
        }
        if let Some(file) = cache_file {
            let (file, id) = (file.to_path_buf(), binary.clone());
            if let Some(version) = tokio::task::spawn_blocking(move || cached_version(&file, &id)).await.ok().flatten() {
                *VERSION_CACHE.lock() = Some((binary.clone(), version.clone()));
                return Some(version);
            }
        }
    }

    let mut cmd = Command::new(bin);
    cmd.args(["--ignore-config", "--version"])
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    hide_console(&mut cmd);
    let output = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await.ok()?.ok()?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || version.is_empty() {
        return None;
    }
    if let Some(binary) = binary {
        *VERSION_CACHE.lock() = Some((binary.clone(), version.clone()));
        if let Some(file) = cache_file.map(Path::to_path_buf) {
            let stored = version.clone();
            let saved = tokio::task::spawn_blocking(move || store_version(&file, &binary, &stored)).await;
            if let Ok(Err(e)) = saved {
                tracing::debug!("Could not cache the yt-dlp version: {e}");
            }
        }
    }
    Some(version)
}

/// The numbers of a yt-dlp version ("2025.11.12", nightly "2025.11.12.232810"), which compare in
/// release order; `None` unless it has at least year, month and day.
fn version_parts(version: &str) -> Option<Vec<u32>> {
    let parts = version.trim().split('.').map(|p| p.parse().ok()).collect::<Option<Vec<u32>>>()?;
    (parts.len() >= 3).then_some(parts)
}

/// Compare a yt-dlp version against `min`. Unparseable versions count as too old.
fn version_at_least(version: &str, min: (u32, u32, u32)) -> bool {
    version_parts(version).is_some_and(|p| (p[0], p[1], p[2]) >= min)
}

/// Browser cookies, read from the browser once per process. yt-dlp takes 0.2-0.5 s to read them
/// (it copies and decrypts the browser's cookie database), so the first run that reads a browser
/// also saves what it read: given `--cookies <file>` next to `--cookies-from-browser`, yt-dlp writes
/// its whole cookie jar to that file when it exits. Later runs get the jar through `--cookies`.
/// Cookies the browser changes after that reach yt-dlp at the next start.
///
/// The jar is kept in memory. It is on disk only while a yt-dlp run uses it, as a file of that run
/// (see [`CookieRun`]) in a per-user folder: under %LOCALAPPDATA%, which only the user may open, or
/// made private to the user elsewhere.
struct BrowserCookies {
    /// `None` without a per-user directory: every run then reads the browser.
    dir: Option<PathBuf>,
    /// Whether `dir` is ready, cleared of what crashed processes left there.
    ready: tokio::sync::OnceCell<bool>,
    jars: parking_lot::Mutex<HashMap<&'static str, Jar>>,
}

enum Jar {
    /// A run reads the browser and saves the jar.
    Saving,
    Saved(Arc<str>),
}

static BROWSER_COOKIES: LazyLock<BrowserCookies> =
    LazyLock::new(|| BrowserCookies::new(app_data_dir().map(|dir| dir.join("cookies"))));

impl BrowserCookies {
    fn new(dir: Option<PathBuf>) -> Self {
        Self { dir, ready: tokio::sync::OnceCell::new(), jars: parking_lot::Mutex::new(HashMap::new()) }
    }

    /// The cookie arguments of one yt-dlp run with `source`.
    async fn for_run(&self, source: &BrowserCookieSource) -> CookieRun<'_> {
        let mut run = CookieRun { cookies: self, args: source.to_args(), file: None, saving: None };
        let (Some(browser), Some(dir)) = (source.browser(), &self.dir) else {
            return run;
        };
        let ready = self.ready.get_or_init(|| async {
            let dir = dir.clone();
            let prepared = tokio::task::spawn_blocking(move || prepare_cookie_dir(&dir)).await;
            match prepared.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| e.to_string())) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("Browser cookies will be read for every download: {e}");
                    false
                }
            }
        });
        if !*ready.await {
            return run;
        }
        let saved = match self.jars.lock().entry(browser) {
            Entry::Occupied(jar) => match jar.get() {
                Jar::Saved(jar) => Some(Arc::clone(jar)),
                // Another run reads the browser right now.
                Jar::Saving => return run,
            },
            Entry::Vacant(jar) => {
                jar.insert(Jar::Saving);
                run.saving = Some(browser);
                None
            }
        };
        let dir = dir.clone();
        let file = tokio::task::spawn_blocking(move || cookie_file(&dir, saved.as_deref())).await;
        match file.map_err(|e| e.to_string()).and_then(|r| r) {
            Ok((path, claim)) => {
                let file_args = ["--cookies".to_string(), path.to_string_lossy().into_owned()];
                if run.saving.is_some() {
                    run.args.extend(file_args);
                } else {
                    run.args = file_args.to_vec();
                }
                run.file = Some((path, claim));
            }
            Err(e) => tracing::warn!("Could not keep the browser cookies for later downloads: {e}"),
        }
        run
    }
}

/// Creates the cookie folder, private to the user, and deletes the files of runs no process holds
/// any more (it crashed or was killed). Blocking.
fn prepare_cookie_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // A run's file, or the lock of its claim.
        let file = dir.join(name.strip_suffix(".part.lock").unwrap_or(&name));
        if let Ok(Some(_claim)) = crate::engine::claim_target(&file) {
            let _ = std::fs::remove_file(&file);
        }
    }
    Ok(())
}

/// A new file in `dir` for one run, claimed: holding `jar`, readable by the user only, or not
/// created yet, for yt-dlp to save the jar in. Blocking.
fn cookie_file(dir: &Path, jar: Option<&str>) -> Result<(PathBuf, crate::engine::TargetClaim), String> {
    use std::io::Write;
    let path = dir.join(format!("{}.txt", unique_suffix()));
    let claim = crate::engine::claim_target(&path)?.ok_or_else(|| format!("{} is in use", path.display()))?;
    if let Some(jar) = jar {
        let mut file = std::fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
        if let Err(e) = file.open(&path).and_then(|mut f| f.write_all(jar.as_bytes())) {
            let _ = std::fs::remove_file(&path);
            return Err(format!("Failed to write {}: {e}", path.display()));
        }
    }
    Ok((path, claim))
}

/// The cookie arguments of one yt-dlp run, and the file they name.
struct CookieRun<'a> {
    cookies: &'a BrowserCookies,
    args: Vec<String>,
    /// The run's file, deleted on drop. Claimed for as long as it exists, which tells other
    /// processes (see [`prepare_cookie_dir`]) that it is in use.
    file: Option<(PathBuf, crate::engine::TargetClaim)>,
    /// The browser whose cookies the run saves into `file`.
    saving: Option<&'static str>,
}

impl CookieRun<'_> {
    /// Keeps the jar yt-dlp saved, if the run was to save one. Only for a yt-dlp that exited by
    /// itself: one that was killed may have written half a jar.
    async fn finish(mut self) {
        let (Some(browser), Some((path, _))) = (self.saving, &self.file) else {
            return;
        };
        // yt-dlp starts the file with this header, even when it read no cookies.
        let jar = tokio::fs::read_to_string(path).await.ok().filter(|jar| jar.starts_with("# Netscape HTTP Cookie File"));
        if let Some(jar) = jar {
            self.cookies.jars.lock().insert(browser, Jar::Saved(jar.into()));
            self.saving = None;
        }
    }
}

impl Drop for CookieRun<'_> {
    fn drop(&mut self) {
        // Nothing saved: the next run reads the browser again.
        if let Some(browser) = self.saving {
            self.cookies.jars.lock().remove(browser);
        }
        if let Some((path, _claim)) = self.file.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// What a yt-dlp run does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    /// Finds what to download and prints it as JSON (`-J`), for the engine to download
    /// (see [`fast_download`]). Its formats are plain files and playlists: no `formats=dashy`.
    Extract,
    /// Downloads and post-processes, printing progress for [`OutputState`].
    Download,
}

/// yt-dlp arguments for one run. Paths in `options` must be absolute: yt-dlp runs in
/// [`ytdlp_work_dir`]. The cookie arguments come from [`BrowserCookies::for_run`]; the proxy is not
/// among them (see [`ytdlp_command`]).
fn build_ytdlp_args(
    url: &Url,
    options: &MediaDownloadOptions,
    kind: RunKind,
    cookie_args: &[String],
    ffmpeg_dir: Option<&Path>,
    js_runtime: Option<&str>,
    version: Option<&str>,
) -> Vec<String> {
    let supports = |min| version.is_some_and(|v| version_at_least(v, min));
    let mut args: Vec<String> = [
        // Config files (next to the binary, in the working directory, per user, system wide)
        // can run arbitrary commands via --exec and would break our output parsing.
        "--ignore-config",
        "--no-colors",
        // Only affects URLs that name both a video and a playlist (watch?v=..&list=..).
        // A URL that is only a playlist is still downloaded as one.
        "--no-playlist",
    ]
    .map(String::from)
    .to_vec();
    let kind_args: &[&str] = match kind {
        // A playlist comes back as a list of links, not every video extracted: it goes to a
        // Download run anyway.
        RunKind::Extract => &["-J", "--flat-playlist"],
        RunKind::Download => &[
            "--newline",
            "--progress",
            // --print implies --simulate for early stages; ours are all post-download, but be explicit.
            "--no-simulate",
            "--progress-template",
            PROGRESS_TEMPLATE,
            "--print",
            PLANNED_TEMPLATE,
            "--print",
            POSTPROCESS_TEMPLATE,
            "--print",
            PATH_TEMPLATE,
            // No --http-chunk-size: a file then streams in one response instead of one request per
            // chunk, each a round trip of idle connection. YouTube asks for 10 MiB requests itself
            // (the format's `http_chunk_size`), which a global chunk size would override.
            "--buffer-size",
            "16M",
        ],
    };
    args.extend(kind_args.iter().map(|a| a.to_string()));
    if kind == RunKind::Download && !matches!(options.preset, MediaQualityPreset::Custom(_)) {
        args.extend(YOUTUBE_DASHY.map(String::from));
    }

    if supports(NO_PLUGIN_DIRS_MIN_VERSION) {
        args.push("--no-plugin-dirs".to_string());
    }
    if let Some(runtime) = js_runtime.filter(|_| supports(JS_RUNTIMES_MIN_VERSION)) {
        args.extend(["--js-runtimes".to_string(), runtime.to_string()]);
    }
    if let Some(dir) = ffmpeg_dir {
        args.extend(["--ffmpeg-location".to_string(), dir.to_string_lossy().to_string()]);
    }
    args.extend(options.preset.to_args());
    args.extend_from_slice(cookie_args);
    if kind == RunKind::Download && options.concurrent_fragments > 1 {
        args.extend(["--concurrent-fragments".to_string(), options.concurrent_fragments.min(32).to_string()]);
    }

    let file_template = options.output_filename.as_deref().unwrap_or("%(title)s.%(ext)s");
    args.extend(["-o".to_string(), options.output_dir.join(file_template).to_string_lossy().to_string()]);
    args.push(url.to_string());
    args
}

/// One line printed by our `--progress-template`.
#[derive(Debug, PartialEq)]
struct TemplateProgress<'a> {
    finished: bool,
    downloaded: u64,
    /// Exact total, else yt-dlp's estimate (shown as "~ 28.46MiB" in its own output).
    total: Option<u64>,
    speed: Option<f64>,
    eta: Option<u64>,
    /// File name of the stream being downloaded; changes between video, audio and playlist items.
    stream: &'a str,
}

/// Parse a template number; "NA" (missing) and non-finite values yield `None`.
fn parse_template_number(field: &str) -> Option<f64> {
    field.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)
}

fn parse_progress_line(line: &str) -> Option<TemplateProgress<'_>> {
    let mut fields = line.strip_prefix(PROGRESS_MARK)?.splitn(7, ' ');
    let finished = fields.next()? == "finished";
    let mut number = || fields.next().and_then(parse_template_number);
    let (downloaded, total, estimate, speed, eta) = (number(), number(), number(), number(), number());
    let stream = fields.next()?;
    let total = total.or(estimate).map(|v| v as u64);
    let downloaded = downloaded.map(|v| v as u64).or(if finished { total } else { None })?;
    Some(TemplateProgress { finished, downloaded, total, speed, eta: eta.map(|v| v as u64), stream })
}

/// Folds yt-dlp's per-stream progress (video, then audio, then the next playlist item) into one
/// overall figure whose downloaded byte count never goes backwards.
#[derive(Debug, Default)]
struct ProgressTracker {
    /// Sum of the sizes yt-dlp announced before each video.
    planned: u64,
    /// Bytes of streams that have already finished.
    completed: u64,
    stream: String,
    current: u64,
    current_total: u64,
    /// Highest `downloaded` reported so far.
    reported: u64,
}

impl ProgressTracker {
    fn plan(&mut self, bytes: u64) {
        self.planned += bytes;
    }

    fn update(&mut self, progress: &TemplateProgress) -> ProgressUpdate {
        if progress.stream != self.stream {
            self.completed += self.current;
            self.current = 0;
            self.stream = progress.stream.to_string();
        }
        self.current_total = progress.total.unwrap_or(0).max(progress.downloaded);
        self.current = if progress.finished { self.current_total } else { progress.downloaded };
        self.snapshot(progress.speed.unwrap_or(0.0), progress.eta)
    }

    /// All streams of the video are down; pin the bar at 100% while yt-dlp merges / converts.
    fn post_processing(&mut self) -> ProgressUpdate {
        self.planned = self.reported.max(self.completed + self.current);
        self.current_total = self.current;
        self.snapshot(0.0, None)
    }

    fn snapshot(&mut self, speed: f64, eta_seconds: Option<u64>) -> ProgressUpdate {
        let downloaded = (self.completed + self.current).max(self.reported);
        self.reported = downloaded;
        let total = self.planned.max(self.completed + self.current_total).max(downloaded);
        ProgressUpdate { downloaded, total, speed, eta_seconds, active_connections: 1 }
    }
}

/// What we learn from a yt-dlp run's output.
#[derive(Debug, Default)]
struct OutputState {
    tracker: ProgressTracker,
    final_path: Option<PathBuf>,
    errors: Vec<String>,
    stderr_tail: VecDeque<String>,
}

impl OutputState {
    /// Consume one output line; returns a progress update to forward, if any.
    fn handle_line(&mut self, line: &str, from_stderr: bool) -> Option<ProgressUpdate> {
        if line.starts_with(PROGRESS_MARK) {
            return parse_progress_line(line).map(|p| self.tracker.update(&p));
        }
        if let Some(size) = line.strip_prefix(PLANNED_MARK) {
            if let Some(bytes) = parse_template_number(size.trim()) {
                self.tracker.plan(bytes as u64);
            }
            return None;
        }
        if line.starts_with(POSTPROCESS_MARK) {
            return Some(self.tracker.post_processing());
        }
        if let Some(path) = line.strip_prefix(PATH_MARK) {
            // With --no-playlist this is the only file, otherwise the playlist's last one.
            self.final_path = Some(PathBuf::from(path));
            return None;
        }
        if !from_stderr || line.trim().is_empty() {
            return None;
        }
        if line.starts_with("ERROR:") {
            self.errors.push(line.to_string());
        } else if line.starts_with("WARNING:") {
            tracing::warn!("yt-dlp: {line}");
        }
        if self.stderr_tail.len() == STDERR_TAIL_LINES {
            self.stderr_tail.pop_front();
        }
        self.stderr_tail.push_back(line.to_string());
        None
    }

    fn failure_message(&self, status: ExitStatus) -> String {
        if !self.errors.is_empty() {
            self.errors.join("\n")
        } else if !self.stderr_tail.is_empty() {
            format!("yt-dlp exited with {status}:\n{}", Vec::from(self.stderr_tail.clone()).join("\n"))
        } else {
            format!("yt-dlp exited with {status}")
        }
    }
}

/// Turn the bytes `read_until` accumulated into a line (lossy UTF-8, so a stray byte can never
/// stall the reader). Marks the stream closed on EOF or error.
fn take_line(read: std::io::Result<usize>, buf: &mut Vec<u8>, open: &mut bool) -> Option<String> {
    if !matches!(read, Ok(n) if n > 0) {
        *open = false;
    }
    if buf.is_empty() {
        return None;
    }
    let line = String::from_utf8_lossy(buf).trim_end_matches(['\r', '\n']).to_string();
    buf.clear();
    Some(line)
}

fn hide_console(cmd: &mut Command) {
    // So no blank console window pops up.
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    #[cfg(not(windows))]
    let _ = cmd;
}

async fn wait_cancelled(flag: Option<Arc<AtomicBool>>) {
    // ponytail: polls because the engine exposes cancellation only as an AtomicBool;
    // switch to a Notify/CancellationToken if the engine grows one.
    match flag {
        Some(flag) => {
            while !flag.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        None => std::future::pending().await,
    }
}

fn is_cancelled(flag: &Option<Arc<AtomicBool>>) -> bool {
    flag.as_ref().is_some_and(|f| f.load(Ordering::Relaxed))
}

/// yt-dlp's whole process tree: yt-dlp itself, the interpreter a pip/PyInstaller launcher
/// starts as its child, and the ffmpeg processes yt-dlp spawns. Dropping it kills the tree.
struct ProcessTree {
    /// Job object created with KILL_ON_JOB_CLOSE: when its last handle closes (on drop, or when
    /// this process dies for any reason) Windows terminates every process in it.
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
    /// yt-dlp runs as the leader of its own process group; `None` once it has been reaped
    /// (its id may then be reused, so we must not signal it any more).
    #[cfg(unix)]
    pgid: Option<libc::pid_t>,
}

// SAFETY: a job object handle is a process-wide kernel handle, valid on any thread.
#[cfg(windows)]
unsafe impl Send for ProcessTree {}

impl ProcessTree {
    #[cfg(windows)]
    fn attach(child: &Child) -> Self {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // SAFETY: plain Win32 calls with valid pointers; `info` outlives the call and the child
        // handle stays valid while `child` is borrowed.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        let tree = Self { job };
        let attached = !job.is_null() && unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const std::ffi::c_void,
                std::mem::size_of_val(&info) as u32,
            ) != 0
                && child.raw_handle().is_some_and(|h| AssignProcessToJobObject(job, h as HANDLE) != 0)
        };
        if !attached {
            tracing::warn!(
                "Could not put yt-dlp in a job object ({}); cancelling may leave its ffmpeg children running",
                std::io::Error::last_os_error()
            );
        }
        tree
    }

    #[cfg(unix)]
    fn attach(child: &Child) -> Self {
        Self { pgid: child.id().and_then(|id| libc::pid_t::try_from(id).ok()) }
    }

    fn kill(&self) {
        #[cfg(windows)]
        if !self.job.is_null() {
            // SAFETY: `job` is a job object handle we own.
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
        }
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // SAFETY: signals the process group we created; the leader is not reaped yet.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }

    /// The leader has exited and been reaped.
    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }

    async fn kill_and_reap(&mut self, child: &mut Child) {
        self.kill();
        let _ = child.start_kill();
        let _ = child.wait().await;
        self.disarm();
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        #[cfg(windows)]
        if !self.job.is_null() {
            // SAFETY: closing the job handle we own; KILL_ON_JOB_CLOSE ends the tree.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
        }
        #[cfg(unix)]
        self.kill();
    }
}

/// A piped, windowless command whose process tree [`ProcessTree::attach`] can own.
fn tree_command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    hide_console(&mut cmd);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// The yt-dlp invocation, run in `work_dir`. A proxy goes into the child's environment, which
/// yt-dlp and the ffmpeg it starts both honor: on the command line its credentials would be
/// visible to every local user.
fn ytdlp_command(bin: &Path, args: &[String], proxy: Option<&str>, work_dir: &Path) -> Command {
    let mut cmd = tree_command(bin);
    // Otherwise Python encodes piped output in the locale code page (cp1252 on Windows).
    cmd.args(args).current_dir(work_dir).env("PYTHONIOENCODING", "utf-8");
    if let Some(proxy) = proxy {
        // ffmpeg ignores a proxy without a scheme, which reqwest and yt-dlp read as http://.
        let has_scheme = proxy.split_once("://").is_some_and(|(scheme, _)| {
            !scheme.is_empty() && scheme.bytes().all(|b| b.is_ascii_alphanumeric())
        });
        let proxy = if has_scheme { proxy.to_string() } else { format!("http://{proxy}") };
        // Python prefers the lower-case names; set both so an inherited value cannot win.
        for var in ["http_proxy", "https_proxy", "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            cmd.env(var, &proxy);
        }
        // The proxy applies to every host; an inherited exemption list must not change that.
        cmd.env_remove("no_proxy").env_remove("NO_PROXY");
    }
    cmd
}

/// Run yt-dlp once and return the file it produced.
async fn run_ytdlp(
    mut cmd: Command,
    progress_tx: Option<&Sender<ProgressUpdate>>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<PathBuf, String> {
    let mut child = cmd.spawn().map_err(|e| {
        format!("Failed to spawn yt-dlp ({}): {e}", Path::new(cmd.as_std().get_program()).display())
    })?;
    let mut tree = ProcessTree::attach(&child);
    let mut stdout = BufReader::new(child.stdout.take().ok_or("Failed to capture yt-dlp stdout")?);
    let mut stderr = BufReader::new(child.stderr.take().ok_or("Failed to capture yt-dlp stderr")?);

    let cancelled = wait_cancelled(cancel_flag);
    tokio::pin!(cancelled);

    let mut state = OutputState::default();
    let (mut out_buf, mut err_buf) = (Vec::new(), Vec::new());
    let (mut out_open, mut err_open) = (true, true);
    // Drain both pipes until EOF so yt-dlp never blocks on a full pipe and its final
    // ERROR lines are always collected.
    while out_open || err_open {
        let update = tokio::select! {
            _ = &mut cancelled => {
                tree.kill_and_reap(&mut child).await;
                return Err(CANCELLED.to_string());
            }
            read = stdout.read_until(b'\n', &mut out_buf), if out_open => {
                take_line(read, &mut out_buf, &mut out_open).and_then(|line| state.handle_line(&line, false))
            }
            read = stderr.read_until(b'\n', &mut err_buf), if err_open => {
                take_line(read, &mut err_buf, &mut err_open).and_then(|line| state.handle_line(&line, true))
            }
        };
        if let (Some(update), Some(tx)) = (update, progress_tx) {
            // Progress is lossy by nature; never let a slow consumer stall pipe draining.
            let _ = tx.try_send(update);
        }
    }

    let status = tokio::select! {
        status = child.wait() => status.map_err(|e| format!("Failed to wait on yt-dlp: {e}"))?,
        _ = &mut cancelled => {
            tree.kill_and_reap(&mut child).await;
            return Err(CANCELLED.to_string());
        }
    };
    tree.disarm();

    if !status.success() {
        return Err(state.failure_message(status));
    }
    let path = state.final_path.ok_or("yt-dlp finished without reporting an output file")?;
    match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.is_file() => Ok(path),
        _ => Err(format!("yt-dlp reported {} but no such file exists", path.display())),
    }
}

/// Runs a [`tree_command`] to the end and returns its standard output, or the errors it printed
/// when it fails. Cancelling kills its process tree.
async fn run_to_end(mut cmd: Command, cancel_flag: Option<Arc<AtomicBool>>) -> Result<Vec<u8>, String> {
    let program = Path::new(cmd.as_std().get_program()).display().to_string();
    let child = cmd.spawn().map_err(|e| format!("Failed to spawn {program}: {e}"))?;
    let mut tree = ProcessTree::attach(&child);
    let output = tokio::select! {
        biased;
        output = child.wait_with_output() => output.map_err(|e| format!("Failed to wait on {program}: {e}"))?,
        // Dropping the child and its tree kills them.
        _ = wait_cancelled(cancel_flag) => return Err(CANCELLED.to_string()),
    };
    tree.disarm();
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let errors: Vec<&str> = lines.iter().copied().filter(|l| l.starts_with("ERROR:")).collect();
    let detail = if errors.is_empty() { &lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..] } else { &errors[..] };
    Err(format!("{program} exited with {}: {}", output.status, detail.join("\n")))
}

/// One stream of a media download, which the engine fetches itself (see [`fast_download`]).
pub(crate) struct MediaStream {
    /// Where the stream is served; good for this extraction only.
    pub url: Url,
    /// What the stream is, the same in every extraction: `hyperfetch-media:/<extractor>/<video
    /// id>/<format id>`. The engine's resume state and history know the download by it as well as
    /// by `url`, so a later attempt resumes it from a fresh URL.
    pub key: Url,
    /// An HLS playlist rather than a file.
    pub hls: bool,
    /// Sends the format's headers, and its cookies only to the hosts they belong to.
    pub client: reqwest::Client,
    /// Where it goes (see [`stream_path`]).
    pub path: PathBuf,
    /// Size of the requests the site asks for (the format's `http_chunk_size`).
    pub chunk_size: Option<u64>,
}

/// Downloads a [`MediaStream`] to its path, or a free name next to it, and returns where it went.
/// It reports progress on the sender, and stops, keeping what it can resume, once the token is
/// cancelled.
pub(crate) type StreamFetcher<'a> = dyn Fn(MediaStream, broadcast::Sender<EngineSnapshot>, CancellationToken) -> BoxFuture<'a, Result<PathBuf, String>>
    + Send
    + Sync
    + 'a;

/// Protocols the engine downloads itself; anything else (DASH fragments, RTMP, ...) is yt-dlp's.
const ENGINE_PROTOCOLS: &[&str] = &["http", "https", "m3u8", "m3u8_native"];

/// Format fields that make yt-dlp download a format in a way the engine does not copy: request
/// bodies, browser impersonation, extra URL parameters, keys and playlists the extractor supplies,
/// one discontinuity of a playlist, fragment lists, parts of a video.
const YTDLP_ONLY_FIELDS: &[&str] = &[
    "fragments",
    "request_data",
    "impersonate",
    "extra_param_to_segment_url",
    "extra_param_to_key_url",
    "hls_aes",
    "hls_media_playlist_data",
    "format_index",
    "is_from_start",
    "section_start",
    "section_end",
];

/// Longest stream file name (see [`stream_path`]), as the engine caps names.
const MAX_STREAM_NAME: usize = 200;

/// How often the progress of streams downloading at once is reported.
const STREAMS_PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// How the downloaded streams become the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finish {
    /// One stream that is the output as it is.
    Rename,
    /// One stream in the wrong container, which yt-dlp would fix up (MPEG-TS from HLS in an .mp4,
    /// or a DASH .m4a).
    Remux,
    /// Several streams, joined as yt-dlp's merger joins them.
    Merge,
}

/// A stream of a [`FastPlan`].
#[derive(Debug)]
struct PlannedStream {
    url: Url,
    key: Url,
    hls: bool,
    format_id: String,
    ext: String,
    headers: Vec<(String, String)>,
    /// `Set-Cookie` values for `url` (see [`format_cookies`]).
    cookies: Vec<String>,
    chunk_size: Option<u64>,
    /// The size yt-dlp announced, exact or estimated.
    size: Option<u64>,
    /// Whether it has audio / video; `None` when yt-dlp does not know.
    audio: Option<bool>,
    video: Option<bool>,
}

/// What yt-dlp found (`-J`), when the engine can download it.
#[derive(Debug)]
struct FastPlan {
    /// The file yt-dlp would have made.
    output: PathBuf,
    streams: Vec<PlannedStream>,
    finish: Finish,
    /// When the site lets the download start (it shows ads first), in Unix seconds.
    available_at: Option<u64>,
}

/// Whether a JSON field is set: not missing, null or false. 0 counts (a `format_index`).
fn is_set(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null) | Some(Value::Bool(false)))
}

/// What yt-dlp found (`-J` output), as a download the engine can do, or why it cannot. The
/// streams are downloaded as yt-dlp's own downloaders would (see [`YTDLP_ONLY_FIELDS`]), and the
/// finish needs ffmpeg unless it is a rename.
fn plan_fast(info: &Value, have_ffmpeg: bool) -> Result<FastPlan, String> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    if text(info, "_type").is_some_and(|t| t != "video") {
        return Err("not a single video".into());
    }
    let live = matches!(text(info, "live_status").as_deref(), Some("is_live" | "is_upcoming" | "post_live"));
    if live || is_set(info.get("is_live")) {
        return Err("a live stream".into());
    }
    if is_set(info.get("stretched_ratio")) && info.get("stretched_ratio").and_then(Value::as_f64) != Some(1.0) {
        return Err("a video yt-dlp fixes the aspect ratio of".into());
    }
    let downloads = info.get("requested_downloads").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let [download] = downloads else {
        return Err(format!("{} files to download", downloads.len()));
    };
    let output = PathBuf::from(text(download, "filename").ok_or("no output file name")?);
    if !output.is_absolute() || output.file_name().is_none() {
        return Err(format!("output {}", output.display()));
    }
    let extractor = text(info, "extractor_key").or_else(|| text(info, "extractor")).ok_or("no extractor")?;
    let video_id = text(info, "id").ok_or("no video id")?;
    let formats: Vec<&Value> = match info.get("requested_formats").and_then(Value::as_array) {
        Some(formats) => formats.iter().collect(),
        None => vec![info],
    };

    let mut streams = Vec::new();
    for format in &formats {
        let protocol = text(format, "protocol").unwrap_or_default();
        if !ENGINE_PROTOCOLS.contains(&protocol.as_str()) {
            return Err(format!("a {protocol} stream"));
        }
        if is_set(format.get("has_drm")) {
            return Err("DRM".into());
        }
        if let Some(field) = YTDLP_ONLY_FIELDS.iter().find(|f| is_set(format.get(**f))) {
            return Err(format!("a stream with {field}"));
        }
        let options = format.get("downloader_options").and_then(Value::as_object);
        if let Some(option) = options.and_then(|o| o.keys().find(|k| *k != "http_chunk_size")) {
            return Err(format!("a stream with the downloader option {option}"));
        }
        let url = text(format, "url").ok_or("a stream without a URL")?;
        let url = Url::parse(&url).map_err(|e| format!("stream URL {url}: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("a {} URL", url.scheme()));
        }
        let format_id = text(format, "format_id").ok_or("a stream without a format id")?;
        let ext = text(format, "ext").ok_or("a stream without an extension")?;
        let mut key = Url::parse("hyperfetch-media:/").map_err(|e| e.to_string())?;
        key.path_segments_mut()
            .map_err(|()| "no stream key")?
            .clear()
            .extend([extractor.as_str(), video_id.as_str(), format_id.as_str()]);
        let headers = format.get("http_headers").and_then(Value::as_object).map_or_else(Vec::new, |headers| {
            headers.iter().filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string()))).collect()
        });
        let codec = |key: &str| text(format, key).map(|codec| codec != "none");
        streams.push(PlannedStream {
            hls: protocol.starts_with("m3u8"),
            cookies: text(format, "cookies").map_or_else(Vec::new, |c| format_cookies(&c)),
            chunk_size: options.and_then(|o| o.get("http_chunk_size")).and_then(Value::as_u64).filter(|&n| n > 0),
            size: ["filesize", "filesize_approx"]
                .iter()
                .find_map(|key| format.get(*key).and_then(Value::as_f64))
                .map(|size| size as u64),
            audio: codec("acodec"),
            video: codec("vcodec"),
            url,
            key,
            format_id,
            ext,
            headers,
        });
    }

    let dash_m4a = |stream: &PlannedStream| {
        stream.ext == "m4a" && formats.first().and_then(|f| text(f, "container")).as_deref() == Some("m4a_dash")
    };
    let finish = match streams.as_slice() {
        [_, _, ..] => Finish::Merge,
        [one] if (one.hls && matches!(one.ext.as_str(), "mp4" | "m4a")) || dash_m4a(one) => Finish::Remux,
        _ => Finish::Rename,
    };
    if finish != Finish::Rename && !have_ffmpeg {
        return Err("ffmpeg is missing".into());
    }
    let available_at = formats.iter().filter_map(|f| f.get("available_at")?.as_f64()).map(|t| t as u64).max();
    Ok(FastPlan { output, streams, finish, available_at })
}

/// The cookies yt-dlp lists for a format (`name=value; Domain=..; Path=..; Secure; Expires=..;
/// name2=..`, values quoted as Python's `http.cookies` quotes them) as `Set-Cookie` values. A
/// value a `Set-Cookie` cannot carry (one with `;`) is left out.
fn format_cookies(list: &str) -> Vec<String> {
    let mut cookies: Vec<Option<String>> = Vec::new();
    for part in list.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        match name.to_ascii_lowercase().as_str() {
            "domain" | "path" | "secure" => {
                if let Some(Some(cookie)) = cookies.last_mut() {
                    cookie.push_str("; ");
                    cookie.push_str(part);
                }
            }
            // The client lives for one download.
            "expires" | "version" => {}
            _ => {
                let value = unquote_cookie_value(value);
                cookies.push((!value.contains(';')).then(|| format!("{name}={value}")));
            }
        }
    }
    cookies.into_iter().flatten().collect()
}

/// Undoes Python's cookie value quoting: `"..."` with `\"`, `\\` and octal `\ooo` escapes.
fn unquote_cookie_value(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_string();
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let rest = chars.as_str();
        // `\000` to `\377`, as Python's `_unquote` reads them.
        let octal = rest.get(..3).filter(|d| d.starts_with(['0', '1', '2', '3']) && d.bytes().all(|b| (b'0'..=b'7').contains(&b)));
        match octal.and_then(|d| u32::from_str_radix(d, 8).ok()).and_then(char::from_u32) {
            Some(decoded) => {
                out.push(decoded);
                chars = rest[3..].chars();
            }
            None => out.extend(chars.next()),
        }
    }
    out
}

/// The client for a stream: its format's headers, and its cookies in a jar, which sends them only
/// where they belong (never to a host a redirect leads to). Otherwise set up like
/// [`crate::engine::build_client`]'s, with the user's proxy.
fn stream_client(stream: &PlannedStream, proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &stream.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| format!("header {name}: {e}"))?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|e| format!("header {name}: {e}"))?;
        headers.insert(name, value);
    }
    let jar = reqwest::cookie::Jar::default();
    for cookie in &stream.cookies {
        jar.add_cookie_str(cookie, &stream.url);
    }
    let mut builder = reqwest::Client::builder()
        .http1_only()
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(15))
        .tcp_keepalive(Duration::from_secs(30))
        .default_headers(headers)
        .cookie_provider(Arc::new(jar));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| format!("Invalid proxy URL {proxy}: {e}"))?);
    }
    builder.build().map_err(|e| format!("Failed to build HTTP client: {e}"))
}

/// Where a stream of `output` downloads: next to it, named apart from yt-dlp's own
/// `<name>.f<format>.<ext>` files, so neither ever takes the other's partial file for its own.
fn stream_path(output: &Path, format_id: &str, ext: &str) -> PathBuf {
    let tail = crate::engine::sanitize_component(&format!(".f{format_id}.hf.{ext}"));
    let stem = output.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    // The whole tail stays: two streams may differ only there.
    let mut cut = MAX_STREAM_NAME.saturating_sub(tail.len()).min(stem.len());
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    output.with_file_name(format!("{}{tail}", &stem[..cut]))
}

/// Combined progress of streams downloading at once. The byte count never goes backwards.
#[derive(Debug)]
struct StreamsProgress {
    /// Per stream: bytes downloaded, size (yt-dlp's until the engine knows it), speed, connections.
    streams: Vec<(u64, u64, f64, usize)>,
    reported: u64,
}

impl StreamsProgress {
    fn new(sizes: impl IntoIterator<Item = u64>) -> Self {
        Self { streams: sizes.into_iter().map(|size| (0, size, 0.0, 0)).collect(), reported: 0 }
    }

    fn record(&mut self, stream: usize, snapshot: &EngineSnapshot) {
        if let Some(s) = self.streams.get_mut(stream) {
            s.0 = snapshot.downloaded_bytes;
            if snapshot.total_bytes > 0 {
                s.1 = snapshot.total_bytes;
            }
            (s.2, s.3) = (snapshot.speed_bytes_per_sec, snapshot.active_workers);
        }
    }

    fn update(&mut self) -> ProgressUpdate {
        let downloaded = self.streams.iter().map(|s| s.0).sum::<u64>().max(self.reported);
        self.reported = downloaded;
        let total = self.streams.iter().map(|s| s.1.max(s.0)).sum::<u64>().max(downloaded);
        let speed: f64 = self.streams.iter().map(|s| s.2).sum();
        let eta_seconds = (speed > 0.0).then(|| ((total - downloaded) as f64 / speed) as u64);
        let active_connections = self.streams.iter().map(|s| s.3).sum::<usize>().max(1);
        ProgressUpdate { downloaded, total, speed, eta_seconds, active_connections }
    }

    /// Everything is down; the bar stays full while ffmpeg runs.
    fn done(&mut self) -> ProgressUpdate {
        for s in &mut self.streams {
            s.1 = s.1.max(s.0);
            s.0 = s.1;
            s.2 = 0.0;
        }
        let update = self.update();
        ProgressUpdate { total: update.downloaded, eta_seconds: None, ..update }
    }
}

/// The last snapshot waiting in `rx`, if any.
fn latest(rx: &mut broadcast::Receiver<EngineSnapshot>) -> Option<EngineSnapshot> {
    let mut latest = None;
    loop {
        match rx.try_recv() {
            Ok(snapshot) => latest = Some(snapshot),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => return latest,
        }
    }
}

/// Downloads every stream at once with `fetch` and reports their combined progress. The first
/// failure stops the others, as cancelling does. Returns each stream's file, in order.
async fn fetch_streams(
    streams: Vec<MediaStream>,
    sizes: Vec<u64>,
    fetch: &StreamFetcher<'_>,
    progress_tx: Option<&Sender<ProgressUpdate>>,
    cancel_flag: &Option<Arc<AtomicBool>>,
) -> Result<Vec<PathBuf>, String> {
    let stop = CancellationToken::new();
    let failure = parking_lot::Mutex::new(None);
    let mut receivers = Vec::new();
    let downloads: Vec<_> = streams
        .into_iter()
        .map(|stream| {
            let (tx, rx) = broadcast::channel(16);
            receivers.push(rx);
            let (download, stop, failure) = (fetch(stream, tx, stop.clone()), stop.clone(), &failure);
            async move {
                let result = download.await;
                if let Err(e) = &result {
                    // The others fail too once stopped; this is why.
                    if !stop.is_cancelled() {
                        *failure.lock() = Some(e.clone());
                        stop.cancel();
                    }
                }
                result
            }
        })
        .collect();
    let all = futures_util::future::join_all(downloads);
    let cancelled = wait_cancelled(cancel_flag.clone());
    tokio::pin!(all, cancelled);
    let mut progress = StreamsProgress::new(sizes);
    let mut read_snapshots = |progress: &mut StreamsProgress| {
        for (i, rx) in receivers.iter_mut().enumerate() {
            if let Some(snapshot) = latest(rx) {
                progress.record(i, &snapshot);
            }
        }
    };
    let mut tick = tokio::time::interval(STREAMS_PROGRESS_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let results = loop {
        tokio::select! {
            results = &mut all => break results,
            () = &mut cancelled, if !stop.is_cancelled() => stop.cancel(),
            _ = tick.tick() => {
                read_snapshots(&mut progress);
                if let Some(tx) = progress_tx {
                    let _ = tx.try_send(progress.update());
                }
            }
        }
    };
    // The final sizes, reported after the last tick.
    read_snapshots(&mut progress);
    if is_cancelled(cancel_flag) {
        return Err(CANCELLED.to_string());
    }
    if let Some(failure) = failure.lock().take() {
        return Err(failure);
    }
    let files = results.into_iter().collect::<Result<Vec<_>, _>>()?;
    if let Some(tx) = progress_tx {
        let _ = tx.try_send(progress.done());
    }
    Ok(files)
}

/// ffmpeg arguments that turn `inputs` (the streams' files, in order) into `output` without
/// re-encoding: several streams as yt-dlp's merger joins them, one as its MPEG-TS / DASH m4a
/// fixups remux it. Neither asks for the second `faststart` pass (see [`NO_FASTSTART`]). Paths go
/// in as `file:` URLs, as yt-dlp passes them, so no name is taken for a protocol or an option.
fn ffmpeg_args(streams: &[PlannedStream], inputs: &[PathBuf], output: &Path) -> Vec<OsString> {
    let file = |path: &Path| {
        let mut url = OsString::from("file:");
        url.push(path);
        url
    };
    let mut args: Vec<OsString> = ["-y", "-nostdin", "-hide_banner", "-loglevel", "error"].map(OsString::from).to_vec();
    for input in inputs {
        args.extend([OsString::from("-i"), file(input)]);
    }
    if let [_] = streams {
        args.extend(["-map", "0", "-dn", "-ignore_unknown", "-c", "copy", "-f", "mp4"].map(OsString::from));
    } else {
        args.extend(["-c", "copy"].map(OsString::from));
        // yt-dlp's order. A stream yt-dlp cannot tell has audio or video is mapped if it has.
        for (i, stream) in streams.iter().enumerate() {
            for (kind, has) in [("a", stream.audio), ("v", stream.video)] {
                if has != Some(false) {
                    let optional = if has.is_none() { "?" } else { "" };
                    args.extend([OsString::from("-map"), OsString::from(format!("{i}:{kind}:0{optional}"))]);
                }
            }
        }
    }
    args.push(file(output));
    args
}

/// Deletes what is left of streams: finished files, partial ones, and the history entries the
/// engine made for them. Blocking.
fn remove_streams(paths: &[PathBuf]) {
    let mut history = crate::history::DownloadHistoryManager::load();
    for path in paths {
        if let Err(e) = crate::engine::discard_partial(path) {
            tracing::warn!("{e}");
        }
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!("Failed to delete {}: {e}", path.display());
            }
            _ => {}
        }
        let path = std::path::absolute(path).unwrap_or_else(|_| path.clone());
        let ids: Vec<String> = history.entries().iter().filter(|e| e.file_path == path).map(|e| e.id.clone()).collect();
        for id in ids {
            history.remove_entry(&id);
        }
    }
}

/// Downloads what yt-dlp found (`info`, its `-J` output) with the engine instead of yt-dlp: every
/// stream at once, each over as many connections as the engine opens, then joined or remuxed
/// with ffmpeg as yt-dlp would. `Err` leaves the download to yt-dlp. Unless the download was
/// cancelled (it then resumes next time), the streams' files are deleted by then.
async fn fast_download(
    info: &Value,
    options: &MediaDownloadOptions,
    ffmpeg: Option<&Path>,
    progress_tx: Option<&Sender<ProgressUpdate>>,
    cancel_flag: &Option<Arc<AtomicBool>>,
    fetch: &StreamFetcher<'_>,
) -> Result<PathBuf, String> {
    let plan = plan_fast(info, ffmpeg.is_some())?;
    let output = plan.output.clone();
    // yt-dlp does not download a file that is already there either.
    if tokio::fs::metadata(&output).await.is_ok_and(|m| m.is_file()) {
        return Ok(output);
    }
    if let Some(dir) = output.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
    }
    // Two jobs for the same video would write the same files.
    let claim = {
        let output = output.clone();
        tokio::task::spawn_blocking(move || crate::engine::claim_target(&output))
            .await
            .map_err(|e| format!("Background task failed: {e}"))??
    };
    let Some(_claim) = claim else {
        return Err(format!("{} is being downloaded already", output.display()));
    };

    if let Some(at) = plan.available_at {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if at > now {
            tracing::info!("Waiting {}s before downloading, as the site requires", at - now);
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(at - now)) => {}
                () = wait_cancelled(cancel_flag.clone()) => return Err(CANCELLED.to_string()),
            }
        }
    }

    let sizes: Vec<u64> = plan.streams.iter().map(|s| s.size.unwrap_or(0)).collect();
    let mut streams = Vec::new();
    for planned in &plan.streams {
        streams.push(MediaStream {
            url: planned.url.clone(),
            key: planned.key.clone(),
            hls: planned.hls,
            client: stream_client(planned, options.proxy.as_deref())?,
            path: stream_path(&output, &planned.format_id, &planned.ext),
            chunk_size: planned.chunk_size,
        });
    }
    let mut leftovers: Vec<PathBuf> = streams.iter().map(|s| s.path.clone()).collect();
    let finished = match fetch_streams(streams, sizes, fetch, progress_tx, cancel_flag).await {
        Ok(files) => {
            leftovers.extend(files.iter().cloned());
            joined(&plan, &files, ffmpeg, cancel_flag).await
        }
        Err(e) => Err(e),
    };
    // A cancelled download keeps its streams, to resume them.
    if finished.is_err() && is_cancelled(cancel_flag) {
        return Err(CANCELLED.to_string());
    }
    let _ = tokio::task::spawn_blocking(move || remove_streams(&leftovers)).await;
    finished
}

/// Makes the plan's output from the streams' `files`: the one file itself, or what ffmpeg writes
/// to a temporary name next to it.
async fn joined(
    plan: &FastPlan,
    files: &[PathBuf],
    ffmpeg: Option<&Path>,
    cancel_flag: &Option<Arc<AtomicBool>>,
) -> Result<PathBuf, String> {
    let output = &plan.output;
    let rename = |from: PathBuf| async move {
        tokio::fs::rename(&from, output)
            .await
            .map(|()| output.clone())
            .map_err(|e| format!("Failed to move {} to {}: {e}", from.display(), output.display()))
    };
    if plan.finish == Finish::Rename {
        let [file] = files else {
            return Err(format!("{} streams for one file", files.len()));
        };
        return rename(file.clone()).await;
    }
    let ffmpeg = ffmpeg.ok_or("ffmpeg is missing")?;
    let ext = output.extension().map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
    let stem = output.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let temp = output.with_file_name(format!("{stem}.hfmerge{ext}"));
    let mut cmd = tree_command(ffmpeg);
    cmd.args(ffmpeg_args(&plan.streams, files, &temp));
    let made = match run_to_end(cmd, cancel_flag.clone()).await {
        Ok(_) => rename(temp.clone()).await,
        Err(e) => Err(e),
    };
    if made.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    made
}

/// Download a media URL using yt-dlp with automated JS challenge solving,
/// quality presets, browser cookies, and real-time progress reporting.
pub async fn download_media(
    url: &Url,
    options: &MediaDownloadOptions,
    progress_tx: Option<Sender<ProgressUpdate>>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<PathBuf, String> {
    download_media_with(url, options, progress_tx, cancel_flag, None).await
}

/// [`download_media`]; with `fetch`, yt-dlp first only finds the formats and `fetch` downloads
/// them (see [`fast_download`]). What that cannot do goes to yt-dlp as before.
pub(crate) async fn download_media_with(
    url: &Url,
    options: &MediaDownloadOptions,
    progress_tx: Option<Sender<ProgressUpdate>>,
    cancel_flag: Option<Arc<AtomicBool>>,
    fetch: Option<&StreamFetcher<'_>>,
) -> Result<PathBuf, String> {
    // First-time discovery stats every PATH entry (possibly on slow network drives).
    let (found_ytdlp, ffmpeg, js_runtime) =
        tokio::task::spawn_blocking(|| (find_ytdlp_path(), find_ffmpeg_path(), find_js_runtime()))
            .await
            .map_err(|e| format!("Tool discovery failed: {e}"))?;

    // yt-dlp runs in its own working directory, so every path it gets must be absolute.
    let mut options = options.clone();
    options.output_dir = absolute(&options.output_dir)?;
    if let BrowserCookieSource::File(path) = &mut options.cookies {
        *path = absolute(path)?;
    }
    let work_dir = ytdlp_work_dir();
    tokio::fs::create_dir_all(&work_dir)
        .await
        .map_err(|e| format!("Failed to create {}: {e}", work_dir.display()))?;

    let mut ytdlp_bin = match (&options.custom_ytdlp_path, found_ytdlp) {
        (Some(path), _) => absolute(path)?,
        (None, Some(path)) => path,
        (None, None) => tokio::select! {
            installed = install_managed_ytdlp(options.proxy.as_deref()) => installed?,
            _ = wait_cancelled(cancel_flag.clone()) => return Err(CANCELLED.to_string()),
        },
    };

    if ffmpeg.is_none() {
        tracing::warn!(
            "ffmpeg not found: yt-dlp cannot merge separate video and audio streams, so it will fall back \
             to a lower-quality pre-merged format, and audio extraction presets will fail. Install ffmpeg \
             (e.g. `winget install Gyan.FFmpeg` or your package manager) or place it next to the application."
        );
    }
    let ffmpeg_dir = ffmpeg.as_deref().and_then(Path::parent);
    let managed = managed_bin_dir().is_some_and(|dir| ytdlp_bin.starts_with(dir));

    // Audio presets convert with ffmpeg as yt-dlp's post-processor does; those stay with yt-dlp.
    if let Some(fetch) = fetch.filter(|_| !options.preset.extracts_audio()) {
        let version = ytdlp_version(&ytdlp_bin, &work_dir, version_cache_file().as_deref()).await;
        let cookies = BROWSER_COOKIES.for_run(&options.cookies).await;
        let args = build_ytdlp_args(url, &options, RunKind::Extract, &cookies.args, ffmpeg_dir, js_runtime.as_deref(), version.as_deref());
        let extracted = run_to_end(ytdlp_command(&ytdlp_bin, &args, options.proxy.as_deref(), &work_dir), cancel_flag.clone()).await;
        if !is_cancelled(&cancel_flag) {
            cookies.finish().await;
        }
        let downloaded = match extracted.and_then(|json| serde_json::from_slice(&json).map_err(|e| format!("yt-dlp -J: {e}"))) {
            Ok(info) => fast_download(&info, &options, ffmpeg.as_deref(), progress_tx.as_ref(), &cancel_flag, fetch).await,
            Err(e) => Err(e),
        };
        match downloaded {
            Ok(path) => return Ok(path),
            Err(_) if is_cancelled(&cancel_flag) => return Err(CANCELLED.to_string()),
            Err(e) => tracing::info!("Leaving {url} to yt-dlp: {e}"),
        }
    }

    let mut updated = false;
    loop {
        let version = ytdlp_version(&ytdlp_bin, &work_dir, version_cache_file().as_deref()).await;
        let cookies = BROWSER_COOKIES.for_run(&options.cookies).await;
        let args = build_ytdlp_args(url, &options, RunKind::Download, &cookies.args, ffmpeg_dir, js_runtime.as_deref(), version.as_deref());
        let cmd = ytdlp_command(&ytdlp_bin, &args, options.proxy.as_deref(), &work_dir);

        let result = run_ytdlp(cmd, progress_tx.as_ref(), cancel_flag.clone()).await;
        if !is_cancelled(&cancel_flag) {
            cookies.finish().await;
        }
        let err = match result {
            Ok(path) => return Ok(path),
            Err(err) => err,
        };
        if updated || !managed || is_cancelled(&cancel_flag) {
            return Err(err);
        }

        // Sites (YouTube above all) break old yt-dlp releases often: retry once on the latest.
        let update = tokio::select! {
            update = update_managed_ytdlp(options.proxy.as_deref(), version.as_deref()) => update,
            _ = wait_cancelled(cancel_flag.clone()) => return Err(CANCELLED.to_string()),
        };
        match update {
            Ok(Some(installed)) => {
                ytdlp_bin = installed;
                updated = true;
            }
            Ok(None) => return Err(err),
            Err(update_err) => {
                tracing::warn!("Could not update yt-dlp: {update_err}");
                return Err(err);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(status: &str, downloaded: &str, total: &str, estimate: &str, stream: &str) -> String {
        format!("{PROGRESS_MARK}{status} {downloaded} {total} {estimate} 1048576.5 3 {stream}")
    }

    #[test]
    fn progress_template_matches_parser() {
        // The template must emit exactly the fields the parser expects, in order.
        let fields: Vec<&str> = PROGRESS_TEMPLATE.split_whitespace().collect();
        assert_eq!(fields[0], format!("download:{}", PROGRESS_MARK.trim()));
        assert_eq!(fields.len(), 8);
        assert_eq!(fields[7], "%(progress.filename)s");
        assert!(PLANNED_TEMPLATE.contains(PLANNED_MARK));
        assert!(POSTPROCESS_TEMPLATE.contains(POSTPROCESS_MARK));
        assert!(PATH_TEMPLATE.contains(PATH_MARK));
    }

    #[test]
    fn parses_progress_line_with_exact_size() {
        let l = line("downloading", "1024", "4096", "NA", "C:\\My Videos\\Clip name.f137.mp4");
        let p = parse_progress_line(&l).expect("valid line");
        assert_eq!(
            p,
            TemplateProgress {
                finished: false,
                downloaded: 1024,
                total: Some(4096),
                speed: Some(1048576.5),
                eta: Some(3),
                stream: "C:\\My Videos\\Clip name.f137.mp4",
            }
        );
    }

    #[test]
    fn parses_estimated_size_of_fragmented_download() {
        // yt-dlp's human output shows these as "of ~ 28.46MiB"; the template gives the estimate.
        let l = line("downloading", "500", "NA", "29842964.8", "a.mp4");
        let p = parse_progress_line(&l).expect("valid line");
        assert_eq!(p.total, Some(29_842_964));
        assert_eq!(p.downloaded, 500);
    }

    #[test]
    fn parses_na_fields() {
        let l = format!("{PROGRESS_MARK}downloading 10 NA NA NA NA a.mp4");
        let p = parse_progress_line(&l).expect("valid line");
        assert_eq!((p.total, p.speed, p.eta), (None, None, None));

        let finished = format!("{PROGRESS_MARK}finished NA 77 NA NA NA a.mp4");
        assert_eq!(parse_progress_line(&finished).map(|p| p.downloaded), Some(77));

        assert!(parse_progress_line(&format!("{PROGRESS_MARK}downloading NA NA NA NA NA a.mp4")).is_none());
        assert!(parse_progress_line("[download]  45.2% of 28.46MiB").is_none());
        assert!(parse_progress_line(&format!("{PROGRESS_MARK}downloading 1 2")).is_none());
    }

    #[test]
    fn progress_is_monotonic_across_video_audio_and_merge() {
        let mut state = OutputState::default();
        let mut seen = Vec::new();
        let mut feed = |state: &mut OutputState, l: &str| {
            if let Some(u) = state.handle_line(l, false) {
                seen.push((u.downloaded, u.total));
            }
        };
        feed(&mut state, &format!("{PLANNED_MARK}1100"));
        feed(&mut state, &line("downloading", "400", "1000", "NA", "v.f137.mp4"));
        feed(&mut state, &line("finished", "1000", "1000", "NA", "v.f137.mp4"));
        feed(&mut state, &line("downloading", "50", "100", "NA", "v.f140.m4a"));
        // A retried stream restarting from zero must not move the bar backwards.
        feed(&mut state, &line("downloading", "0", "100", "NA", "v.f140.m4a"));
        feed(&mut state, &line("finished", "100", "100", "NA", "v.f140.m4a"));
        feed(&mut state, &format!("{POSTPROCESS_MARK} abc"));

        assert_eq!(
            seen,
            vec![(400, 1100), (1000, 1100), (1050, 1100), (1050, 1100), (1100, 1100), (1100, 1100)]
        );
    }

    #[test]
    fn args_fetch_youtube_fragments_in_parallel() {
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        let options = MediaDownloadOptions { output_dir: PathBuf::from("out"), concurrent_fragments: 8, ..Default::default() };
        let args = build_ytdlp_args(&url, &options, RunKind::Download, &[], None, None, Some("2026.08.19"));
        let at = args.iter().position(|a| a == "--extractor-args").expect("extractor arguments");
        assert_eq!(args[at + 1], "youtube:formats=dashy");
        let at = args.iter().position(|a| a == "--concurrent-fragments").expect("parallel fragments");
        assert_eq!(args[at + 1], "8");
        // dashy drops the formats YouTube gives no size for (format 18, often), which a custom
        // selection may name; our presets pick adaptive formats.
        for preset in [MediaQualityPreset::Hd720p, MediaQualityPreset::AudioMp3, MediaQualityPreset::AudioM4a] {
            let options = MediaDownloadOptions { preset, ..options.clone() };
            assert!(build_ytdlp_args(&url, &options, RunKind::Download, &[], None, None, None).contains(&YOUTUBE_DASHY[1].to_string()));
        }
        let custom = MediaDownloadOptions { preset: MediaQualityPreset::Custom("18".into()), ..options };
        let args = build_ytdlp_args(&url, &custom, RunKind::Download, &[], None, None, Some("2026.08.19"));
        assert!(!args.iter().any(|a| a == "--extractor-args"), "{args:?}");
    }

    #[test]
    fn an_extraction_only_finds_the_formats() {
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        let options = MediaDownloadOptions { output_dir: std::env::temp_dir(), concurrent_fragments: 8, ..Default::default() };
        let cookies = ["--cookies".to_string(), "jar.txt".to_string()];
        let args = build_ytdlp_args(&url, &options, RunKind::Extract, &cookies, None, Some("node"), Some("2026.08.19"));
        for flag in ["--ignore-config", "--no-playlist", "-J", "--flat-playlist", "--cookies", "--js-runtimes", "-o"] {
            assert!(args.iter().any(|a| a == flag), "{flag} missing from {args:?}");
        }
        // The formats stay plain files for the engine (fragments are yt-dlp's), and nothing prints
        // around the JSON.
        for flag in ["--extractor-args", "--concurrent-fragments", "--print", "--progress-template", "--no-simulate"] {
            assert!(!args.iter().any(|a| a == flag), "{flag} in {args:?}");
        }
        assert_eq!(args.last(), Some(&url.to_string()));
    }

    #[test]
    fn progress_of_parallel_fragments_is_read() {
        // Lines of a real dashy run with -N 8 (yt-dlp 2026.08.19), shortened: fragments report no
        // exact total, only an estimate that moves (and overshoots) as they arrive.
        let (video, audio) = ("C:\\out\\aqz-KE-bpKQ.f396.mp4", "C:\\out\\aqz-KE-bpKQ.f251.webm");
        let lines = [
            format!("{PLANNED_MARK}25501018"),
            format!("{PROGRESS_MARK}downloading 1024 NA 9626096.0 1786.04 NA {video}"),
            format!("{PROGRESS_MARK}downloading 145408 NA 21245952.0 2786.04 NA {video}"),
            format!("{PROGRESS_MARK}downloading 9006328 NA 27638752.0 7689306.1 2 {video}"),
            format!("{PROGRESS_MARK}finished 15298808 15298808 NA 7689306.1 NA {video}"),
            format!("{PROGRESS_MARK}downloading 2096128 NA 11249762.0 5049125.2 1 {audio}"),
            format!("{PROGRESS_MARK}finished 10202210 10202210 NA 5049125.2 NA {audio}"),
            format!("{POSTPROCESS_MARK} aqz-KE-bpKQ"),
        ];
        let mut state = OutputState::default();
        let seen: Vec<(u64, u64)> =
            lines.iter().filter_map(|l| state.handle_line(l, false)).map(|u| (u.downloaded, u.total)).collect();
        assert_eq!(seen.len(), 7, "every progress line counts");
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "never backwards: {seen:?}");
        assert_eq!(seen[2], (9006328, 27638752), "the estimate stands in for the unknown total");
        assert_eq!(seen[4], (15298808 + 2096128, 15298808 + 11249762), "audio adds to the finished video");
        assert_eq!(seen[5], (25501018, 25501018), "done once both streams are");
        assert_eq!(seen[6], (25501018, 25501018));
    }

    #[test]
    fn post_processing_pins_overestimated_total_to_done() {
        let mut tracker = ProgressTracker::default();
        tracker.plan(5000);
        tracker.update(&TemplateProgress { finished: true, downloaded: 3000, total: Some(3000), speed: None, eta: None, stream: "a" });
        let u = tracker.post_processing();
        assert_eq!((u.downloaded, u.total), (3000, 3000));
    }

    #[test]
    fn output_state_collects_path_and_errors() {
        let mut state = OutputState::default();
        assert!(state.handle_line(&format!("{PATH_MARK}/tmp/My Song.mp3"), false).is_none());
        state.handle_line("WARNING: something odd", true);
        state.handle_line("ERROR: [youtube] abc: Sign in to confirm you're not a bot", true);
        assert_eq!(state.final_path, Some(PathBuf::from("/tmp/My Song.mp3")));
        assert_eq!(state.errors, vec!["ERROR: [youtube] abc: Sign in to confirm you're not a bot"]);
        assert_eq!(state.stderr_tail.len(), 2);
    }

    #[test]
    fn take_line_decodes_invalid_utf8_lossily() {
        let mut buf = b"Caf\xe9\r\n".to_vec();
        let mut open = true;
        assert_eq!(take_line(Ok(buf.len()), &mut buf, &mut open), Some("Caf\u{fffd}".to_string()));
        assert!(open && buf.is_empty());

        let mut partial = b"tail".to_vec();
        assert_eq!(take_line(Ok(0), &mut partial, &mut open), Some("tail".to_string()));
        assert!(!open);
    }

    #[test]
    fn version_cache_is_keyed_by_path_size_and_modification_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache").join("ytdlp-version.json");
        let id = |path: &str, size, modified| BinaryId { path: PathBuf::from(path), size, modified };
        let managed = id("/bin/yt-dlp", 100, (1_700_000_000, 5));

        assert_eq!(cached_version(&file, &managed), None, "no cache file yet");
        store_version(&file, &managed, "2026.08.19").unwrap();
        store_version(&file, &id("/usr/bin/yt-dlp", 7, (1, 0)), "2025.01.01").unwrap();
        assert_eq!(cached_version(&file, &managed).as_deref(), Some("2026.08.19"));
        // A build swapped in at the same path is asked again.
        assert_eq!(cached_version(&file, &id("/bin/yt-dlp", 101, (1_700_000_000, 5))), None);
        assert_eq!(cached_version(&file, &id("/bin/yt-dlp", 100, (1_700_000_000, 6))), None);

        // Recording a path again replaces its entry and keeps the others.
        let updated = id("/bin/yt-dlp", 120, (1_800_000_000, 0));
        store_version(&file, &updated, "2026.09.01").unwrap();
        assert_eq!(cached_version(&file, &updated).as_deref(), Some("2026.09.01"));
        assert_eq!(cached_version(&file, &managed), None);
        assert_eq!(cached_version(&file, &id("/usr/bin/yt-dlp", 7, (1, 0))).as_deref(), Some("2025.01.01"));
        for n in 0..VERSION_CACHE_ENTRIES + 3 {
            store_version(&file, &id(&format!("/other/{n}"), 1, (1, 0)), "1").unwrap();
        }
        let kept: Vec<CachedVersion> = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(kept.len(), VERSION_CACHE_ENTRIES);
        assert_eq!(std::fs::read_dir(file.parent().unwrap()).unwrap().count(), 1, "no temp file left behind");
    }

    #[tokio::test]
    async fn cached_version_spares_the_launch() {
        let dir = tempfile::tempdir().unwrap();
        // Not a program: asking it for its version fails.
        let bin = dir.path().join(exe_name("yt-dlp"));
        std::fs::write(&bin, b"not a program").unwrap();
        let cache = dir.path().join("ytdlp-version.json");
        store_version(&cache, &BinaryId::of(&bin).unwrap(), "2026.08.19").unwrap();

        assert_eq!(ytdlp_version(&bin, dir.path(), Some(&cache)).await.as_deref(), Some("2026.08.19"));
        // Another build at the same path is asked, even though this process has seen the path.
        std::fs::write(&bin, b"not a program either").unwrap();
        assert_eq!(ytdlp_version(&bin, dir.path(), Some(&cache)).await, None);
    }

    #[test]
    fn version_gate_for_js_runtimes() {
        assert!(version_at_least("2025.11.12", JS_RUNTIMES_MIN_VERSION));
        assert!(version_at_least("2026.08.19.232810", JS_RUNTIMES_MIN_VERSION));
        assert!(!version_at_least("2025.10.22", JS_RUNTIMES_MIN_VERSION));
        assert!(!version_at_least("garbage", JS_RUNTIMES_MIN_VERSION));
    }

    #[test]
    fn args_gate_js_runtime_and_request_machine_output() {
        let url = Url::parse("https://www.youtube.com/watch?v=abc&list=PL123").unwrap();
        let options = MediaDownloadOptions { output_dir: PathBuf::from("out"), ..Default::default() };

        let node = Some("node:/usr/bin/node");
        let args = build_ytdlp_args(&url, &options, RunKind::Download, &[], None, node, Some("2025.10.22"));
        assert!(!args.contains(&"--js-runtimes".to_string()));
        assert!(args.contains(&"--no-playlist".to_string()));
        assert!(args.contains(&PATH_TEMPLATE.to_string()));
        assert!(args.contains(&PROGRESS_TEMPLATE.to_string()));
        assert_eq!(args.last(), Some(&url.to_string()));

        let args = build_ytdlp_args(&url, &options, RunKind::Download, &[], None, node, Some("2025.11.12"));
        let at = args.iter().position(|a| a == "--js-runtimes").expect("flag present");
        assert_eq!(args[at + 1], "node:/usr/bin/node");
    }

    #[test]
    fn args_leave_request_sizes_to_the_site() {
        // A global chunk size splits every progressive file into round trips and overrides the
        // 10 MiB YouTube asks for in each format.
        let url = Url::parse("https://vimeo.com/123").unwrap();
        let options = MediaDownloadOptions { output_dir: PathBuf::from("out"), ..Default::default() };
        let args = build_ytdlp_args(&url, &options, RunKind::Download, &[], None, None, Some("2026.08.19"));
        assert!(!args.iter().any(|a| a == "--http-chunk-size"), "{args:?}");
    }

    #[test]
    fn args_never_load_config_files_or_plugins() {
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        let options = MediaDownloadOptions { output_dir: PathBuf::from("out"), ..Default::default() };
        for version in [None, Some("2024.12.23"), Some("2026.08.19")] {
            let args = build_ytdlp_args(&url, &options, RunKind::Download, &[], None, None, version);
            assert_eq!(args[0], "--ignore-config", "{version:?}");
        }
        let has_no_plugin_dirs = |version| {
            build_ytdlp_args(&url, &options, RunKind::Download, &[], None, None, version).contains(&"--no-plugin-dirs".to_string())
        };
        assert!(has_no_plugin_dirs(Some("2025.03.21")));
        assert!(!has_no_plugin_dirs(Some("2025.02.19")), "older builds abort on the unknown flag");
        assert!(!has_no_plugin_dirs(None));
    }

    #[test]
    fn proxy_goes_only_into_the_environment_with_a_scheme() {
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        let secret = "http://alice:S3cret@proxy:3128";
        let options = MediaDownloadOptions {
            output_dir: PathBuf::from("out"),
            proxy: Some(secret.to_string()),
            ..Default::default()
        };
        let args = build_ytdlp_args(&url, &options, RunKind::Download, &[], None, None, None);
        assert!(!args.iter().any(|a| a == "--proxy" || a.contains("S3cret")), "{args:?}");

        let work_dir = std::env::temp_dir();
        let env_of = |proxy: Option<&str>| {
            let cmd = ytdlp_command(Path::new("yt-dlp"), &args, proxy, &work_dir);
            assert_eq!(cmd.as_std().get_current_dir(), Some(work_dir.as_path()));
            cmd.as_std()
                .get_envs()
                .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
                .collect::<Vec<_>>()
        };
        // Without a scheme ffmpeg (started by yt-dlp for live streams) would bypass the proxy.
        for (given, expected) in [
            ("127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("user:pw@proxy:3128", "http://user:pw@proxy:3128"),
            ("socks5://proxy:1080", "socks5://proxy:1080"),
            (secret, secret),
        ] {
            let env = env_of(Some(given));
            let value = |name: &str| env.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
            for var in ["http_proxy", "https_proxy", "all_proxy"] {
                assert_eq!(value(var), Some(Some(expected.to_string())), "{given} {var}");
            }
            assert_eq!(value("no_proxy"), Some(None), "inherited NO_PROXY is removed");
        }
        assert!(!env_of(None).iter().any(|(k, _)| k.to_ascii_lowercase().contains("proxy")));
    }

    #[test]
    fn empty_directory_is_the_current_one() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(absolute(Path::new("")).unwrap(), absolute(Path::new(".")).unwrap());
        assert!(absolute(Path::new("")).unwrap().starts_with(&cwd));
        assert_eq!(absolute(Path::new("clip.mp4")).unwrap(), cwd.join("clip.mp4"));
    }

    #[test]
    fn finds_asset_checksum() {
        let sums = "aaa  yt-dlp\nbbb  yt-dlp.exe\r\nccc *yt-dlp_linux\n";
        assert_eq!(expected_sha256(sums, "yt-dlp.exe"), Some("bbb"));
        assert_eq!(expected_sha256(sums, "yt-dlp_linux"), Some("ccc"));
        assert_eq!(expected_sha256(sums, "yt-dlp_macos"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn install_verified_rejects_bad_checksum_and_installs_good_one() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let target = bin.join("yt-dlp");
        let body = b"binary";
        let good = format!("{:x}", Sha256::digest(body));

        assert!(install_verified(&bin, "2026.08.19", body, &"0".repeat(64)).is_err());
        assert!(!target.exists());

        assert_eq!(install_verified(&bin, "2026.08.19", body, &good).unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), body);
        // No temp file left behind.
        assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 1);
    }

    /// A zip laid out like yt-dlp_win.zip: yt-dlp.exe next to its `_internal` folder.
    #[cfg(windows)]
    fn release_zip(dir: &Path, exe: &[u8]) -> Vec<u8> {
        let src = dir.join("release-src");
        std::fs::create_dir_all(src.join("_internal")).unwrap();
        std::fs::write(src.join("yt-dlp.exe"), exe).unwrap();
        std::fs::write(src.join("_internal").join("python.dll"), b"runtime").unwrap();
        let zip = dir.join("release.zip");
        let tar = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32").join("tar.exe");
        let status = std::process::Command::new(tar)
            .args(["-a", "-cf"])
            .arg(&zip)
            .arg("-C")
            .arg(&src)
            .args(["yt-dlp.exe", "_internal"])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(zip).unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn install_verified_unpacks_each_release_into_its_own_folder() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let zip = release_zip(dir.path(), b"new build");
        let good = format!("{:x}", Sha256::digest(&zip));

        assert!(install_verified(&bin, "2026.08.19", &zip, &"0".repeat(64)).is_err());
        assert!(!release_dir(&bin, "2026.08.19").exists());

        // What earlier installs left: the previous release, an older one still running and one
        // nothing runs from, the single-file build, and temporary files of an install that
        // crashed hours ago (deleted) or may still be running (kept).
        let previous = release_dir(&bin, "2026.07.01");
        let running = release_dir(&bin, "2026.06.01");
        let idle = release_dir(&bin, "2026.05.01");
        for (release, build) in [(&previous, b"previous build"), (&idle, b"old build 1234")] {
            std::fs::create_dir_all(release).unwrap();
            std::fs::write(release.join("yt-dlp.exe"), build).unwrap();
        }
        std::fs::create_dir_all(&running).unwrap();
        let system32 = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32");
        std::fs::copy(system32.join("PING.EXE"), running.join("yt-dlp.exe")).unwrap();
        let mut child = std::process::Command::new(running.join("yt-dlp.exe"))
            .args(["-n", "60", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        std::fs::write(bin.join("yt-dlp.exe"), b"single-file build").unwrap();
        let (crashed, fresh) = (bin.join(".yt-dlp-2026.05.01-1.zip"), bin.join(".yt-dlp-2026.08.19-2.zip"));
        for leftover in [&crashed, &fresh] {
            std::fs::write(leftover, b"partial").unwrap();
        }
        let hours_ago = SystemTime::now() - Duration::from_secs(2 * 3600);
        std::fs::File::options().write(true).open(&crashed).unwrap().set_modified(hours_ago).unwrap();

        let exe = install_verified(&bin, "2026.08.19", &zip, &good).unwrap();
        assert_eq!(exe, release_dir(&bin, "2026.08.19").join("yt-dlp.exe"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"new build");
        assert!(release_dir(&bin, "2026.08.19").join("_internal").join("python.dll").is_file());
        assert_eq!(newest_release(&bin), Some(exe.clone()));
        assert!(running.join("yt-dlp.exe").is_file(), "a release a program runs from stays whole");
        let left = || {
            let mut names: Vec<String> =
                std::fs::read_dir(&bin).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        };
        assert_eq!(left(), [".yt-dlp-2026.08.19-2.zip", "yt-dlp-2026.06.01", "yt-dlp-2026.07.01", "yt-dlp-2026.08.19"]);

        child.kill().unwrap();
        child.wait().unwrap();
        // The same release again (as when another process installed it first) is kept as it is,
        // and the old release nothing runs from any more goes.
        assert_eq!(install_verified(&bin, "2026.08.19", &zip, &good).unwrap(), exe);
        assert_eq!(left(), [".yt-dlp-2026.08.19-2.zip", "yt-dlp-2026.07.01", "yt-dlp-2026.08.19"]);
    }

    #[cfg(windows)]
    #[test]
    fn a_release_folder_is_renamed_once_the_scanner_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let (unpacked, target) = (dir.path().join(".yt-dlp-2026.08.19-1.tmp"), dir.path().join("yt-dlp-2026.08.19"));
        std::fs::create_dir(&unpacked).unwrap();
        let exe = unpacked.join("yt-dlp.exe");
        std::fs::write(&exe, b"build").unwrap();
        // What an antivirus scanner does to a program it has not seen yet.
        let scanning = std::fs::File::open(&exe).unwrap();
        assert!(std::fs::rename(&unpacked, &target).is_err(), "a folder with an open file cannot be renamed");
        let scanner = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            drop(scanning);
        });
        rename_patiently(&unpacked, &target).unwrap();
        scanner.join().unwrap();
        assert_eq!(std::fs::read(target.join("yt-dlp.exe")).unwrap(), b"build");

        // Another process installed the release meanwhile: no waiting for a rename that cannot work.
        std::fs::create_dir(&unpacked).unwrap();
        std::fs::write(unpacked.join("yt-dlp.exe"), b"build").unwrap();
        let started = std::time::Instant::now();
        assert!(rename_patiently(&unpacked, &target).is_err());
        assert!(started.elapsed() < Duration::from_millis(100), "{:?}", started.elapsed());
    }

    #[cfg(windows)]
    #[test]
    fn newest_release_compares_versions_as_numbers() {
        let dir = tempfile::tempdir().unwrap();
        for (name, exe) in [("yt-dlp-2026.9.30", true), ("yt-dlp-2026.10.01", true), ("yt-dlp-2026.12.01", false), ("yt-dlp-nightly", true)] {
            let folder = dir.path().join(name);
            std::fs::create_dir_all(&folder).unwrap();
            if exe {
                std::fs::write(folder.join("yt-dlp.exe"), b"x").unwrap();
            }
        }
        // 10 > 9, and a folder without yt-dlp.exe is no release.
        assert_eq!(newest_release(dir.path()), Some(dir.path().join("yt-dlp-2026.10.01").join("yt-dlp.exe")));
    }

    #[test]
    fn test_is_supported_media_site() {
        let yes = [
            "https://www.youtube.com/watch?v=X-pwWiF7FFI",
            "https://youtu.be/X-pwWiF7FFI",
            "https://www.twitch.tv/videos/12345678",
            "https://x.com/user/status/1",
            "https://redd.it/abc123",
            "https://v.redd.it/abc123",
            "https://dai.ly/x8abc",
            "https://m.youtube.com./watch?v=1",
        ];
        for u in yes {
            assert!(is_supported_media_site(&Url::parse(u).unwrap()), "{u}");
        }
        let no = [
            "https://example.com/file.zip",
            "https://notyoutube.com/watch",
            "https://i.redd.it/picture.jpg",
            "https://box.com/file",
            "https://ly/",
        ];
        for u in no {
            assert!(!is_supported_media_site(&Url::parse(u).unwrap()), "{u}");
        }
    }

    #[test]
    fn test_preset_to_args() {
        let best = MediaQualityPreset::BestVideoAudio;
        let args = best.to_args();
        assert!(args.contains(&"-f".to_string()));
        assert!(args.contains(&"mp4".to_string()));

        let mp3 = MediaQualityPreset::AudioMp3;
        let mp3_args = mp3.to_args();
        assert!(mp3_args.contains(&"mp3".to_string()));
    }

    #[test]
    fn mp4_merges_skip_the_faststart_rewrite() {
        for preset in [MediaQualityPreset::BestVideoAudio, MediaQualityPreset::Fhd1080p, MediaQualityPreset::Hd720p] {
            let args = preset.to_args();
            let at = args.iter().position(|a| a == "--postprocessor-args").expect("merger arguments");
            // yt-dlp lower-cases the key and splits the value like a shell.
            assert_eq!(args[at + 1], "Merger+ffmpeg_o:-movflags -faststart", "{preset:?}");
            assert_eq!(args[args.iter().position(|a| a == "--merge-output-format").unwrap() + 1], "mp4");
        }
        assert!(!MediaQualityPreset::AudioMp3.to_args().contains(&"--postprocessor-args".to_string()));
    }

    /// yt-dlp puts `-movflags +faststart` before the merger's own output arguments; ffmpeg must
    /// then keep the index where it wrote it, with no second pass. Needs ffmpeg.
    #[test]
    fn a_later_movflags_turns_faststart_off() {
        let Some(ffmpeg) = find_ffmpeg_path() else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let ffmpeg_run = |args: &[&str]| {
            let out = std::process::Command::new(&ffmpeg).args(["-hide_banner", "-y"]).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stderr).into_owned()
        };
        let clip_arg = clip.to_str().unwrap();
        ffmpeg_run(&["-loglevel", "error", "-f", "lavfi", "-i", "testsrc=duration=1:size=64x48:rate=5", clip_arg]);
        let merged = dir.path().join("merged.mp4");
        let (_, value) = NO_FASTSTART[1].split_once(':').unwrap();
        let mut args = vec!["-loglevel", "info", "-i", clip_arg, "-c", "copy", "-movflags", "+faststart"];
        args.extend(value.split(' '));
        args.push(merged.to_str().unwrap());
        let log = ffmpeg_run(&args);
        assert!(!log.contains("second pass"), "{log}");
        let bytes = std::fs::read(&merged).unwrap();
        let at = |atom: &[u8]| bytes.windows(4).position(|w| w == atom).unwrap();
        assert!(at(b"mdat") < at(b"moov"), "the index stays after the media data");
    }

    #[test]
    fn m4a_preset_prefers_an_aac_source() {
        // yt-dlp copies AAC into the .m4a and re-encodes anything else.
        assert_eq!(
            MediaQualityPreset::AudioM4a.to_args(),
            ["-f", "ba[acodec^=mp4a]/ba/b", "-x", "--audio-format", "m4a"]
        );
    }

    /// A cookie jar as yt-dlp saves it.
    const JAR: &str = "# Netscape HTTP Cookie File\n# This file is generated by yt-dlp.  Do not edit.\n\n\
                       .youtube.com\tTRUE\t/\tTRUE\t0\tSID\tsecret\n";

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn browser_cookies_are_read_once_and_handed_on() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("cookies");
        let cookies = BrowserCookies::new(Some(folder.clone()));

        // The first run reads the browser and has yt-dlp save what it read.
        let first = cookies.for_run(&BrowserCookieSource::Edge).await;
        assert_eq!(first.args[..3], ["--cookies-from-browser", "edge", "--cookies"]);
        let saved_to = PathBuf::from(&first.args[3]);
        assert!(saved_to.starts_with(&folder) && !saved_to.exists(), "{saved_to:?}");
        // Runs that start before it is saved read the browser themselves.
        let meanwhile = cookies.for_run(&BrowserCookieSource::Edge).await;
        assert_eq!(meanwhile.args, ["--cookies-from-browser", "edge"]);
        drop(meanwhile);
        // What yt-dlp does as it exits.
        std::fs::write(&saved_to, JAR).unwrap();
        first.finish().await;
        assert!(!saved_to.exists());

        // Later runs get a copy of their own, gone after the run.
        let later = cookies.for_run(&BrowserCookieSource::Edge).await;
        assert_eq!(later.args.len(), 2);
        assert_eq!(later.args[0], "--cookies");
        let copy = PathBuf::from(&later.args[1]);
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), JAR);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&folder).unwrap().permissions().mode() & 0o777, 0o700);
        }
        drop(later);
        assert!(!copy.exists());

        // Each browser has its own jar; a cookie file is passed on as it is.
        let firefox = cookies.for_run(&BrowserCookieSource::Firefox).await;
        assert_eq!(firefox.args[..2], ["--cookies-from-browser", "firefox"]);
        drop(firefox);
        let file = PathBuf::from("/home/me/cookies.txt");
        assert_eq!(cookies.for_run(&BrowserCookieSource::File(file.clone())).await.args, BrowserCookieSource::File(file).to_args());
        assert_eq!(cookies.for_run(&BrowserCookieSource::None).await.args, Vec::<String>::new());
        // Nothing is left on disk, claim locks included.
        assert_eq!(names_in(&folder), Vec::<String>::new());
    }

    #[tokio::test]
    async fn browser_cookies_are_read_again_until_a_jar_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = BrowserCookies::new(Some(dir.path().to_path_buf()));
        let saves = |run: &CookieRun| run.args.len() == 4 && run.args[..2] == ["--cookies-from-browser", "chrome"];

        // yt-dlp failed before saving anything.
        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert!(saves(&run));
        run.finish().await;
        // It was killed as it wrote the jar (a killed run is not finished).
        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert!(saves(&run));
        std::fs::write(&run.args[3], &JAR[..20]).unwrap();
        drop(run);
        // It wrote something else.
        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert!(saves(&run));
        std::fs::write(&run.args[3], "ERROR").unwrap();
        run.finish().await;

        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert!(saves(&run));
        std::fs::write(&run.args[3], JAR).unwrap();
        run.finish().await;
        assert_eq!(cookies.for_run(&BrowserCookieSource::Chrome).await.args[0], "--cookies");
        assert_eq!(names_in(dir.path()), Vec::<String>::new());
    }

    #[tokio::test]
    async fn cookie_files_no_process_holds_are_deleted() {
        let dir = tempfile::tempdir().unwrap();
        // Left by a run that crashed: its file and its claim's lock.
        std::fs::write(dir.path().join("crashed.txt"), JAR).unwrap();
        std::fs::write(dir.path().join("crashed.txt.part.lock"), b"").unwrap();
        // A run in another process, which holds its file.
        let live = dir.path().join("live.txt");
        let claim = crate::engine::claim_target(&live).unwrap().unwrap();
        std::fs::write(&live, JAR).unwrap();

        let cookies = BrowserCookies::new(Some(dir.path().to_path_buf()));
        drop(cookies.for_run(&BrowserCookieSource::Brave).await);
        assert_eq!(names_in(dir.path()), ["live.txt", "live.txt.part.lock"]);
        drop(claim);
    }

    /// Spawn a shell that starts a long-running grandchild (standing in for yt-dlp's ffmpeg)
    /// and prints its pid.
    async fn spawn_tree_with_grandchild() -> (Child, ProcessTree, u32) {
        #[cfg(windows)]
        let mut cmd = {
            let mut cmd = tree_command(Path::new("powershell"));
            cmd.args([
                "-NoProfile",
                "-Command",
                "$p = Start-Process ping -ArgumentList '-n','120','127.0.0.1' -PassThru -WindowStyle Hidden; $p.Id; Start-Sleep 120",
            ]);
            cmd
        };
        #[cfg(unix)]
        let mut cmd = {
            let mut cmd = tree_command(Path::new("sh"));
            cmd.args(["-c", "sleep 120 & echo $!; wait"]);
            cmd
        };
        let mut child = cmd.spawn().unwrap();
        let tree = ProcessTree::attach(&child);
        let mut first = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut first).await.unwrap();
        (child, tree, first.trim().parse().unwrap())
    }

    /// Wait up to 10 s for `pid` to exit.
    fn process_exits(pid: u32) -> bool {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
            use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
            unsafe {
                let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
                if handle.is_null() {
                    return true;
                }
                let exited = WaitForSingleObject(handle, 10_000) == WAIT_OBJECT_0;
                CloseHandle(handle);
                exited
            }
        }
        #[cfg(unix)]
        {
            for _ in 0..100 {
                if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            false
        }
    }

    #[tokio::test]
    async fn cancel_kills_whole_process_tree() {
        let (mut child, mut tree, grandchild) = spawn_tree_with_grandchild().await;
        tree.kill_and_reap(&mut child).await;
        assert!(process_exits(grandchild), "grandchild {grandchild} survived cancel");
    }

    #[tokio::test]
    async fn dropping_the_download_kills_whole_process_tree() {
        let (child, tree, grandchild) = spawn_tree_with_grandchild().await;
        drop((child, tree));
        assert!(process_exits(grandchild), "grandchild {grandchild} survived drop");
    }

    /// End-to-end against the real site; needs yt-dlp, ffmpeg and network access.
    #[tokio::test]
    #[ignore = "downloads from YouTube over the internet"]
    async fn downloads_audio_and_returns_the_converted_file() {
        let dir = tempfile::tempdir().unwrap();
        let options = MediaDownloadOptions {
            preset: MediaQualityPreset::AudioM4a,
            output_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let url = Url::parse("https://www.youtube.com/watch?v=jNQXAC9IVRw").unwrap();
        let path = download_media(&url, &options, None, None).await.unwrap();
        assert!(path.is_file());
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("m4a"));
    }

    /// End-to-end against the real site, the engine downloading both streams yt-dlp finds (HLS
    /// cannot go this way in a test build: it caps segments at 64 KiB). Needs yt-dlp, ffmpeg and
    /// network access.
    #[tokio::test]
    #[ignore = "downloads from YouTube over the internet"]
    async fn engine_downloads_the_streams_ytdlp_finds() {
        // Shows why a stream would be left to yt-dlp.
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let dir = tempfile::tempdir().unwrap();
        let engine = crate::engine::DownloadEngine::new(Vec::new(), crate::engine::DownloadOptions::default());
        let fetched = std::sync::atomic::AtomicUsize::new(0);
        let fetch: &StreamFetcher<'_> = &|stream, tx, stop| {
            let (engine, fetched) = (&engine, &fetched);
            Box::pin(async move {
                let result = engine.download_media_stream(stream, tx, stop).await;
                if result.is_ok() {
                    fetched.fetch_add(1, Ordering::Relaxed);
                }
                result
            })
        };
        let options = MediaDownloadOptions { output_dir: dir.path().to_path_buf(), ..Default::default() };
        let url = Url::parse("https://www.youtube.com/watch?v=jNQXAC9IVRw").unwrap();
        let started = std::time::Instant::now();
        let path = download_media_with(&url, &options, None, None, Some(fetch)).await.unwrap();
        eprintln!("{} in {:?}", path.display(), started.elapsed());
        assert_eq!(fetched.load(Ordering::Relaxed), 2, "video and audio");
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mp4"));
        assert_eq!(media_streams(&path), (true, true));
        assert_eq!(names_in(dir.path()), [path.file_name().unwrap().to_string_lossy()]);
    }

    /// Whether ffmpeg finds video and audio in `file`.
    fn media_streams(file: &Path) -> (bool, bool) {
        let ffmpeg = find_ffmpeg_path().expect("ffmpeg");
        let out = std::process::Command::new(ffmpeg).args(["-hide_banner", "-i"]).arg(file).output().unwrap();
        let info = String::from_utf8_lossy(&out.stderr);
        (info.contains("Video:"), info.contains("Audio:"))
    }

    /// One format of `yt-dlp -J` output (trimmed), from YouTube.
    fn format_info(id: &str, ext: &str, vcodec: &str, acodec: &str, size: u64) -> Value {
        serde_json::json!({
            "format_id": id, "ext": ext, "vcodec": vcodec, "acodec": acodec, "filesize": size,
            "protocol": "https", "has_drm": false, "container": format!("{ext}_dash"),
            "url": format!("https://rr3---sn-ab5l6nrl.googlevideo.com/videoplayback?itag={id}&expire=1790476086"),
            "downloader_options": {"http_chunk_size": 10485760},
            "http_headers": {"User-Agent": "Mozilla/5.0", "Accept": "text/html", "Sec-Fetch-Mode": "navigate"},
        })
    }

    /// What `yt-dlp -J` prints (trimmed) for a YouTube video downloaded as `bv*+ba` into `out`.
    fn merge_info(out: &Path, video: Value, audio: Value) -> Value {
        serde_json::json!({
            "_type": "video", "id": "jNQXAC9IVRw", "extractor": "youtube", "extractor_key": "Youtube",
            "live_status": "not_live", "is_live": false, "ext": "mp4", "protocol": "https+https",
            "requested_formats": [video, audio],
            "requested_downloads": [{"ext": "mp4", "filename": out.join("clip.mp4"), "_filename": out.join("clip.mp4")}],
        })
    }

    fn youtube_info(out: &Path) -> Value {
        let mut video = format_info("395", "mp4", "av01.0.00M.08", "none", 223779);
        video["available_at"] = 1700000000.into();
        merge_info(out, video, format_info("251", "webm", "none", "opus", 252182))
    }

    #[test]
    fn plan_fast_fetches_the_streams_of_a_merge() {
        let out = std::env::temp_dir();
        let plan = plan_fast(&youtube_info(&out), true).unwrap();
        assert_eq!(plan.output, out.join("clip.mp4"));
        assert_eq!(plan.finish, Finish::Merge);
        assert_eq!(plan.available_at, Some(1700000000));
        let [video, audio] = plan.streams.as_slice() else { panic!("{:?}", plan.streams) };
        assert_eq!(video.key.as_str(), "hyperfetch-media:/Youtube/jNQXAC9IVRw/395");
        assert_eq!(video.url.query(), Some("itag=395&expire=1790476086"));
        assert_eq!((video.video, video.audio, audio.video, audio.audio), (Some(true), Some(false), Some(false), Some(true)));
        assert_eq!((video.chunk_size, audio.size, video.hls), (Some(10485760), Some(252182), false));
        assert!(video.headers.contains(&("Sec-Fetch-Mode".to_string(), "navigate".to_string())));
        // Without ffmpeg only yt-dlp merges (it picks a format that needs none).
        assert!(plan_fast(&youtube_info(&out), false).is_err());
    }

    #[test]
    fn plan_fast_leaves_to_ytdlp_what_the_engine_would_download_differently() {
        let out = std::env::temp_dir();
        let with = |edit: &dyn Fn(&mut Value)| {
            let mut info = youtube_info(&out);
            edit(&mut info);
            plan_fast(&info, true)
        };
        // Why each is left to yt-dlp, and what makes it so.
        type Edit = fn(&mut Value);
        let rejected: [(&str, Edit); 13] = [
            ("not a single video", |i| i["_type"] = "playlist".into()),
            ("a live stream", |i| i["live_status"] = "is_live".into()),
            ("a live stream", |i| i["live_status"] = "post_live".into()),
            ("DRM", |i| i["requested_formats"][1]["has_drm"] = "maybe".into()),
            ("http_dash_segments", |i| i["requested_formats"][0]["protocol"] = "http_dash_segments".into()),
            ("format_index", |i| i["requested_formats"][0]["format_index"] = 0.into()),
            ("request_data", |i| i["requested_formats"][0]["request_data"] = "a=b".into()),
            ("impersonate", |i| i["requested_formats"][1]["impersonate"] = true.into()),
            ("ffmpeg_args", |i| i["requested_formats"][0]["downloader_options"]["ffmpeg_args"] = serde_json::json!([])),
            ("2 files", |i| i["requested_downloads"] = serde_json::json!([{}, {}])),
            ("output clip.mp4", |i| i["requested_downloads"][0]["filename"] = "clip.mp4".into()),
            ("aspect ratio", |i| i["stretched_ratio"] = 1.5.into()),
            ("a ftp URL", |i| i["requested_formats"][0]["url"] = "ftp://example.com/a.mp4".into()),
        ];
        for (why, edit) in rejected {
            let reason = with(&edit).unwrap_err();
            assert!(reason.contains(why), "{reason} is not {why}");
        }
        // Values that change nothing for the download.
        let harmless = with(&|i| {
            i["stretched_ratio"] = 1.into();
            i["requested_formats"][0]["is_from_start"] = false.into();
            i["requested_formats"][0]["has_drm"] = Value::Null;
        });
        assert!(harmless.is_ok(), "{harmless:?}");
    }

    #[test]
    fn plan_fast_remuxes_what_ytdlp_fixes_up() {
        let out = std::env::temp_dir();
        let single = |protocol: &str, ext: &str, container: Option<&str>| {
            serde_json::json!({
                "id": "x8j6ogo", "extractor_key": "Dailymotion", "format_id": "hls-480", "protocol": protocol,
                "ext": ext, "container": container, "url": "https://vod3.cf.dmcdn.net/video/x_1.m3u8",
                "requested_downloads": [{"filename": out.join(format!("clip.{ext}"))}],
            })
        };
        let finish = |info: Value, ffmpeg| plan_fast(&info, ffmpeg).map(|plan| plan.finish);
        // MPEG-TS from HLS in an .mp4 / .m4a, and DASH .m4a, which yt-dlp remuxes.
        assert_eq!(finish(single("m3u8_native", "mp4", None), true), Ok(Finish::Remux));
        assert_eq!(finish(single("m3u8", "m4a", None), true), Ok(Finish::Remux));
        assert_eq!(finish(single("https", "m4a", Some("m4a_dash")), true), Ok(Finish::Remux));
        assert!(finish(single("m3u8_native", "mp4", None), false).is_err(), "remuxing needs ffmpeg");
        // Files that are the output as they are.
        assert_eq!(finish(single("https", "mp4", Some("mp4_dash")), false), Ok(Finish::Rename));
        assert_eq!(finish(single("m3u8_native", "ts", None), false), Ok(Finish::Rename));
        assert!(plan_fast(&single("m3u8_native", "mp4", None), true).unwrap().streams[0].hls);
    }

    #[test]
    fn format_cookies_are_read_as_ytdlp_lists_them() {
        // Joined as yt-dlp's `_calc_headers` joins them; Python quotes values that are not plain
        // tokens, escaping `"`, `\` and some characters (`;` among them) in octal.
        let list = r#"SID=abc123; Domain=.youtube.com; Path=/; Secure; Expires=1790476086; PREF="f6=4&tz=Europe.London"; Domain=.youtube.com; Path=/; odd="a\"b\\c\073d"; Path=/; spaced="x\054y z"; Path=/"#;
        assert_eq!(
            format_cookies(list),
            [
                "SID=abc123; Domain=.youtube.com; Path=/; Secure",
                "PREF=f6=4&tz=Europe.London; Domain=.youtube.com; Path=/",
                "spaced=x,y z; Path=/",
            ]
        );
        assert_eq!(unquote_cookie_value(r#""a\"b\\c\073d""#), "a\"b\\c;d");
        assert_eq!(unquote_cookie_value("plain"), "plain");
    }

    #[test]
    fn stream_files_are_named_apart_from_ytdlp_and_each_other() {
        let dir = std::env::temp_dir();
        let output = dir.join("Me at the zoo.mp4");
        assert_eq!(stream_path(&output, "395", "mp4"), dir.join("Me at the zoo.f395.hf.mp4"));
        assert_eq!(stream_path(&output, "hls-480/a:b", "mp4"), dir.join("Me at the zoo.fhls-480_a_b.hf.mp4"));
        // A long title is cut, never the part that tells the streams apart.
        let long = dir.join(format!("{}.mp4", "é".repeat(150)));
        let (video, audio) = (stream_path(&long, "137", "mp4"), stream_path(&long, "140", "mp4"));
        assert_ne!(video, audio);
        for path in [&video, &audio] {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(name.len() <= MAX_STREAM_NAME && name.ends_with(".hf.mp4"), "{name}");
        }
    }

    #[test]
    fn ffmpeg_joins_the_streams_as_ytdlp_does_without_faststart() {
        let out = std::env::temp_dir();
        let mut plan = plan_fast(&youtube_info(&out), true).unwrap();
        let inputs = [out.join("v.mp4"), out.join("a.webm")];
        let file = |p: &Path| format!("file:{}", p.display());
        let args = |plan: &FastPlan| {
            let args = ffmpeg_args(&plan.streams, &inputs[..plan.streams.len()], &out.join("t.mp4"));
            args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
        };
        let (video, audio, temp) = (file(&inputs[0]), file(&inputs[1]), file(&out.join("t.mp4")));
        assert_eq!(
            args(&plan),
            format!("-y -nostdin -hide_banner -loglevel error -i {video} -i {audio} -c copy -map 0:v:0 -map 1:a:0 {temp}")
        );
        // Streams yt-dlp cannot tell have audio (or video) are mapped only if they have.
        plan.streams[0].audio = None;
        assert!(args(&plan).contains("-map 0:a:0? -map 0:v:0 -map 1:a:0"), "{}", args(&plan));
        plan.streams.truncate(1);
        assert!(args(&plan).ends_with(&format!("-i {video} -map 0 -dn -ignore_unknown -c copy -f mp4 {temp}")));
        assert!(!args(&plan).contains("movflags"));
    }

    fn snapshot(downloaded: u64, total: u64, speed: f64) -> EngineSnapshot {
        EngineSnapshot {
            total_bytes: total,
            downloaded_bytes: downloaded,
            speed_bytes_per_sec: speed,
            progress_ratio: 0.0,
            active_workers: 4,
            mirror_speeds: vec![],
            chunks: vec![],
            target_path: None,
        }
    }

    #[test]
    fn progress_of_streams_at_once_adds_up_and_never_goes_back() {
        let mut progress = StreamsProgress::new([1000, 200]);
        // Until the engine knows the sizes, yt-dlp's count.
        let start = progress.update();
        assert_eq!((start.downloaded, start.total), (0, 1200));
        progress.record(0, &snapshot(300, 1100, 50.0));
        progress.record(1, &snapshot(100, 0, 25.0));
        let update = progress.update();
        assert_eq!((update.downloaded, update.total, update.speed, update.active_connections), (400, 1300, 75.0, 8));
        assert_eq!(update.eta_seconds, Some(12));
        // A stream starting over (a server without ranges) does not move the bar back.
        progress.record(1, &snapshot(0, 200, 25.0));
        assert_eq!(progress.update().downloaded, 400);
        let done = progress.done();
        assert_eq!((done.downloaded, done.total, done.eta_seconds), (1300, 1300, None));
    }

    fn with_part(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(".part");
        PathBuf::from(name)
    }

    /// yt-dlp's `-J` for a merge of `137.mp4` (video) and `140.m4a` (audio) into `out/clip.mp4`.
    fn mp4_merge_info(out: &Path) -> Value {
        merge_info(out, format_info("137", "mp4", "avc1.4d401e", "none", 0), format_info("140", "m4a", "none", "mp4a.40.2", 0))
    }

    #[tokio::test]
    async fn fast_download_merges_the_streams_and_leaves_only_the_output() {
        let Some(ffmpeg) = find_ffmpeg_path() else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (src, out) = (dir.path().join("src"), dir.path().join("out"));
        std::fs::create_dir(&src).unwrap();
        for (name, input) in [("137.mp4", "testsrc=duration=1:size=64x48:rate=5"), ("140.m4a", "sine=duration=1")] {
            let made = std::process::Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", input])
                .arg(src.join(name))
                .output()
                .unwrap();
            assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        }
        // Stands in for the engine: copies the stream's file and records it in history.
        let fetch: &StreamFetcher<'_> = &|stream, tx, _stop| {
            let id = stream.key.path_segments().unwrap().next_back().unwrap().to_string();
            let from = src.join(format!("{id}.{}", stream.path.extension().unwrap().to_string_lossy()));
            Box::pin(async move {
                let size = std::fs::copy(&from, &stream.path).unwrap();
                let _ = tx.send(snapshot(size, size, 0.0));
                let urls = vec![stream.url.to_string(), stream.key.to_string()];
                let entry = crate::history::HistoryEntry::new(id, stream.path.clone(), size, urls);
                crate::history::DownloadHistoryManager::load().add_or_update(entry);
                Ok(stream.path)
            })
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);
        let path =
            fast_download(&mp4_merge_info(&out), &MediaDownloadOptions::default(), Some(&ffmpeg), Some(&progress_tx), &None, fetch)
                .await
                .unwrap();
        assert_eq!(path, out.join("clip.mp4"));
        assert_eq!(media_streams(&path), (true, true));
        let bytes = std::fs::read(&path).unwrap();
        let at = |atom: &[u8]| bytes.windows(4).position(|w| w == atom).unwrap();
        assert!(at(b"mdat") < at(b"moov"), "no second pass to move the index to the front");
        assert_eq!(names_in(&out), ["clip.mp4"]);
        let history = crate::history::DownloadHistoryManager::load();
        assert!(!history.entries().iter().any(|e| e.file_path.starts_with(&out)), "stream entries are gone");
        let mut last = None;
        while let Ok(update) = progress_rx.try_recv() {
            last = Some(update);
        }
        let last = last.expect("progress");
        assert!(last.total > 0 && last.downloaded == last.total, "{last:?}");
    }

    #[tokio::test]
    async fn a_failed_stream_stops_the_others_and_leaves_the_download_to_ytdlp() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let stopped = AtomicBool::new(false);
        let fetch: &StreamFetcher<'_> = &|stream, _tx, stop| {
            let stopped = &stopped;
            Box::pin(async move {
                if stream.key.as_str().ends_with("/140") {
                    return Err("HTTP 403".to_string());
                }
                // The video, half done when it is told to stop.
                std::fs::write(with_part(&stream.path), b"half").unwrap();
                stop.cancelled().await;
                stopped.store(true, Ordering::Relaxed);
                Err(CANCELLED.to_string())
            })
        };
        let ffmpeg = Path::new("never-run");
        let result = fast_download(&mp4_merge_info(&out), &MediaDownloadOptions::default(), Some(ffmpeg), None, &None, fetch).await;
        assert_eq!(result, Err("HTTP 403".to_string()));
        assert!(stopped.load(Ordering::Relaxed));
        assert_eq!(names_in(&out), Vec::<String>::new(), "yt-dlp starts afresh");
    }

    #[tokio::test]
    async fn a_cancelled_download_keeps_its_streams_to_resume() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let cancel = Arc::new(AtomicBool::new(false));
        let fetch: &StreamFetcher<'_> = &|stream, _tx, stop| {
            let cancel = Arc::clone(&cancel);
            Box::pin(async move {
                std::fs::write(with_part(&stream.path), b"partial").unwrap();
                cancel.store(true, Ordering::Relaxed);
                stop.cancelled().await;
                Err(CANCELLED.to_string())
            })
        };
        let ffmpeg = Path::new("never-run");
        let cancel_flag = Some(Arc::clone(&cancel));
        let result = fast_download(&mp4_merge_info(&out), &MediaDownloadOptions::default(), Some(ffmpeg), None, &cancel_flag, fetch).await;
        assert_eq!(result, Err(CANCELLED.to_string()));
        assert_eq!(names_in(&out), ["clip.f137.hf.mp4.part", "clip.f140.hf.m4a.part"]);
    }

    #[tokio::test]
    async fn an_existing_output_is_not_downloaded_again() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("clip.mp4"), b"done").unwrap();
        let fetch: &StreamFetcher<'_> = &|_, _, _| panic!("nothing to fetch");
        let result = fast_download(&mp4_merge_info(dir.path()), &MediaDownloadOptions::default(), Some(Path::new("never-run")), None, &None, fetch).await;
        assert_eq!(result, Ok(dir.path().join("clip.mp4")));
    }

    #[tokio::test(start_paused = true)]
    async fn the_download_starts_when_the_site_allows_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut info = merge_info(dir.path(), format_info("18", "mp4", "avc1", "mp4a", 0), Value::Null);
        info["requested_formats"] = Value::Null;
        for (key, value) in format_info("18", "mp4", "avc1", "mp4a", 0).as_object().unwrap() {
            info[key] = value.clone();
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        info["available_at"] = (now + 30).into();
        let started = tokio::time::Instant::now();
        let waited = parking_lot::Mutex::new(None);
        let fetch: &StreamFetcher<'_> = &|stream, _, _| {
            *waited.lock() = Some(started.elapsed());
            Box::pin(async move {
                std::fs::write(&stream.path, b"video").unwrap();
                Ok(stream.path)
            })
        };
        let path = fast_download(&info, &MediaDownloadOptions::default(), None, None, &None, fetch).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"video");
        let waited = waited.lock().expect("fetched");
        assert!(waited >= Duration::from_secs(29), "{waited:?}");
    }

    /// Serves `body` at any path as a file that takes ranges, only to requests with the format's
    /// header and cookie (403 otherwise). Counts the body bytes it sends.
    async fn serve_media(body: Vec<u8>) -> (std::net::SocketAddr, Arc<AtomicU64>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (body, sent) = (Arc::new(body), Arc::new(AtomicU64::new(0)));
        let counted = Arc::clone(&sent);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (body, sent) = (Arc::clone(&body), Arc::clone(&sent));
                tokio::spawn(async move {
                    let last = body.len() - 1;
                    loop {
                        let mut request = Vec::new();
                        let mut buf = [0u8; 4096];
                        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                            match socket.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => request.extend_from_slice(&buf[..n]),
                            }
                        }
                        let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                        let allowed = request.contains("\r\nx-format: yes\r\n") && request.contains("\r\ncookie: session=abc\r\n");
                        let range = request.lines().find_map(|l| l.strip_prefix("range: bytes=")).and_then(|r| {
                            let (start, end) = r.split_once('-')?;
                            Some((start.parse::<usize>().ok()?, end.parse::<usize>().map_or(last, |e| e.min(last))))
                        });
                        let (status, extra, part) = match range {
                            _ if !allowed => ("403 Forbidden", String::new(), &body[..0]),
                            Some((start, end)) => {
                                ("206 Partial Content", format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len()), &body[start..=end])
                            }
                            None => ("200 OK", String::new(), &body[..]),
                        };
                        let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n{extra}\r\n", part.len());
                        if socket.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                        if !request.starts_with("head ") {
                            if socket.write_all(part).await.is_err() {
                                return;
                            }
                            sent.fetch_add(part.len() as u64, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        (addr, counted)
    }

    fn planned_stream(url: Url, key: Url, hls: bool) -> PlannedStream {
        PlannedStream {
            url,
            key,
            hls,
            format_id: "137".into(),
            ext: "mp4".into(),
            headers: vec![("X-Format".into(), "yes".into())],
            cookies: format_cookies("session=abc; Path=/"),
            chunk_size: None,
            size: None,
            audio: None,
            video: None,
        }
    }

    #[tokio::test]
    async fn a_media_stream_downloads_with_its_format_headers_and_resumes_from_a_new_url() {
        let body: Vec<u8> = (0..3u32 << 20).map(|i| (i * 7 % 251) as u8).collect();
        let (addr, sent) = serve_media(body.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.f137.hf.mp4");
        let key = Url::parse("hyperfetch-media:/Youtube/abc/137").unwrap();
        // What an attempt at another URL of the same stream left: 2 of its 3 MiB.
        let part = with_part(&path);
        let mut partial = body.clone();
        partial[2 << 20..].fill(0);
        std::fs::write(&part, &partial).unwrap();
        let old_url = format!("http://{addr}/videoplayback?sig=old");
        let mut state = crate::state::DownloadState::new("clip.f137.hf.mp4".into(), body.len() as u64, 1 << 20, vec![old_url, key.to_string()]);
        state.completed_ranges.push(crate::range::ByteRange::new(0, (2 << 20) - 1).unwrap());
        state.save_atomic(&crate::state::DownloadState::state_file_path(&part)).unwrap();

        let planned = planned_stream(Url::parse(&format!("http://{addr}/videoplayback?sig=new")).unwrap(), key, false);
        let stream = MediaStream {
            url: planned.url.clone(),
            key: planned.key.clone(),
            hls: false,
            client: stream_client(&planned, None).unwrap(),
            path: path.clone(),
            chunk_size: None,
        };
        let engine = crate::engine::DownloadEngine::new(Vec::new(), crate::engine::DownloadOptions::default());
        let (tx, _rx) = broadcast::channel(16);
        let done = engine.download_media_stream(stream, tx, CancellationToken::new()).await;
        remove_streams(std::slice::from_ref(&path));
        assert_eq!(done, Ok(path.clone()), "the partial download resumed rather than set aside");
        // The probe's first MiB and the missing one, not what was there.
        assert!(sent.load(Ordering::Relaxed) <= 2 << 20, "{} bytes sent", sent.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn a_media_stream_from_an_hls_playlist() {
        let (addr, _) = crate::hls::tests::serve(|path, _| match path {
            "/v/index.m3u8" => (200, String::new(), b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\na.ts\n#EXTINF:2,\nb.ts\n#EXT-X-ENDLIST\n".to_vec()),
            "/v/a.ts" => (200, String::new(), vec![1; 1000]),
            "/v/b.ts" => (200, String::new(), vec![2; 500]),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.fhls-480.hf.mp4");
        let url = Url::parse(&format!("http://{addr}/v/index.m3u8")).unwrap();
        let planned = planned_stream(url, Url::parse("hyperfetch-media:/Dailymotion/x/hls-480").unwrap(), true);
        let stream = MediaStream {
            url: planned.url.clone(),
            key: planned.key.clone(),
            hls: true,
            client: stream_client(&planned, None).unwrap(),
            path: path.clone(),
            chunk_size: None,
        };
        let engine = crate::engine::DownloadEngine::new(Vec::new(), crate::engine::DownloadOptions::default());
        let (tx, _rx) = broadcast::channel(16);
        let done = engine.download_media_stream(stream, tx, CancellationToken::new()).await.unwrap();
        assert_eq!(done, path);
        assert_eq!(std::fs::read(&path).unwrap(), [vec![1; 1000], vec![2; 500]].concat());
    }

    /// Makes `file` with ffmpeg from a lavfi source.
    fn lavfi(ffmpeg: &Path, source: &str, extra: &[&str], file: &Path) -> Vec<u8> {
        let made = std::process::Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", source])
            .args(extra)
            .arg(file)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        std::fs::read(file).unwrap()
    }

    #[tokio::test]
    async fn the_streams_share_the_speed_limit_whatever_ytdlp_knows_of_their_sizes() {
        let Some(ffmpeg) = find_ffmpeg_path() else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // About 50 KB each; every frame a key frame keeps the video that large.
        let video = lavfi(&ffmpeg, "testsrc=duration=2:size=320x240:rate=10", &["-g", "1"], &dir.path().join("v.mp4"));
        let audio = lavfi(&ffmpeg, "sine=duration=5", &[], &dir.path().join("a.m4a"));
        assert!(video.len().min(audio.len()) > 20_000, "{} and {} bytes", video.len(), audio.len());
        let out = dir.path().join("out");
        let mut info = mp4_merge_info(&out);
        for (i, body) in [video.clone(), audio.clone()].into_iter().enumerate() {
            let (addr, _) = serve_media(body).await;
            let format = &mut info["requested_formats"][i];
            format["url"] = format!("http://{addr}/videoplayback?stream={i}").into();
            format["http_headers"] = serde_json::json!({"X-Format": "yes"});
            format["cookies"] = "session=abc; Path=/".into();
        }
        // yt-dlp knows the size of the video only.
        info["requested_formats"][0]["filesize"] = video.len().into();
        info["requested_formats"][1]["filesize"] = Value::Null;

        let limit = 48 * 1024;
        let options = crate::engine::DownloadOptions { max_speed: Some(limit), ..Default::default() };
        let engine = crate::engine::DownloadEngine::new(Vec::new(), options);
        // What `DownloadEngine::run_media` passes.
        let fetch: &StreamFetcher<'_> = &|stream, tx, stop| Box::pin(engine.download_media_stream(stream, tx, stop));
        let media_options = MediaDownloadOptions::default();
        let started = tokio::time::Instant::now();
        let download = fast_download(&info, &media_options, Some(&ffmpeg), None, &None, fetch);
        let path = tokio::time::timeout(Duration::from_secs(60), download)
            .await
            .expect("a stream of unknown size is held to the limit, not all but stopped")
            .unwrap();
        let took = started.elapsed();
        assert_eq!(media_streams(&path), (true, true));
        // Both streams at the whole limit each would take half as long; the limiter lets 0.1 s
        // of idle credit go at once.
        let together = Duration::from_secs_f64((video.len() + audio.len()) as f64 / limit as f64);
        assert!(took + Duration::from_millis(200) >= together, "{took:?} for what takes {together:?} at the limit");
    }
}
