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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
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
    /// Install a managed ffmpeg when a download needs one and none is found.
    pub install_ffmpeg: bool,
    /// Subtitle languages to fetch ("en,es", "all"); None fetches none.
    pub subtitles: Option<String>,
    /// Title, artist, date and URL tags, chapters and cover art inside the file.
    pub embed_metadata: bool,
    /// Record a live stream from its start.
    pub live_from_start: bool,
    /// Wait for a scheduled stream or premiere to start.
    pub wait_for_video: bool,
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
            install_ffmpeg: true,
            subtitles: None,
            embed_metadata: true,
            live_from_start: false,
            wait_for_video: false,
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
/// Printed before each video downloads: whether it is a live stream, and the file it goes to.
const LIVE_TEMPLATE: &str = "before_dl:HFLIVE %(is_live)s %(filename)s";
const LIVE_MARK: &str = "HFLIVE ";
/// Makes the ffmpeg yt-dlp downloads with (every live stream, see [`OutputState`]) report what it
/// has written as `key=value` lines on yt-dlp's output, instead of a status line it redraws with
/// carriage returns, which would never end a line.
const FFMPEG_PROGRESS: [&str; 2] = ["--downloader-args", "ffmpeg:-progress pipe:1 -nostats"];
/// Seconds between yt-dlp's checks whether a scheduled stream has started (`--wait-for-video`):
/// at its start time, but never sooner than a minute apart nor over ten minutes apart, so a late
/// stream is not asked about too often.
const WAIT_FOR_VIDEO: &str = "60-600";
/// How long a live recording that was asked to stop (see [`ProcessTree::interrupt`]) has to stop
/// recording before it is killed and its file kept as it is. Once it has, yt-dlp finishes the
/// file (remuxes, joins, tags it), however long that takes.
const LIVE_STOP_GRACE: Duration = if cfg!(test) { Duration::from_secs(3) } else { Duration::from_secs(30) };
/// Keeps ffmpeg from moving the index of a file it writes tags into to the front (see
/// [`NO_FASTSTART`]).
const NO_FASTSTART_METADATA: [&str; 2] = ["--postprocessor-args", "Metadata+ffmpeg_o:-movflags -faststart"];

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

/// Whether `url` names a playlist, channel, album or other list of videos this module lists
/// through yt-dlp, from its shape alone.
pub fn lists(_url: &Url) -> bool {
    false
}

/// One task per entry of the list at `url`, as `options` say; called only when [`lists`] takes
/// `url`. None when it is one video after all (the link is then downloaded as it is);
/// `Some(Ok)` is never empty.
pub async fn list(
    _http: &reqwest::Client,
    _url: &Url,
    _options: &crate::ingest::ListOptions,
) -> Option<Result<Vec<crate::ingest::Task>, String>> {
    None
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

/// The managed yt-dlp, if one is installed: on Windows the build of the newest release installed
/// in its own folder (see [`install_onedir`]), else a single-file build right in the directory (the
/// only kind elsewhere, and what Windows installs used to be). Blocking.
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

/// The single-file Windows build, as named in the GitHub release: what is installed where the
/// release zip cannot be unpacked (see [`tar_works`]).
#[cfg(windows)]
fn single_file_asset() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "yt-dlp_arm64.exe"
    } else if cfg!(target_arch = "x86") {
        "yt-dlp_x86.exe"
    } else {
        "yt-dlp.exe"
    }
}

/// The yt-dlp build installed on this platform, as named in the GitHub release. On Windows the
/// unpacked ("onedir") build where it can be unpacked: the single-file one (see
/// [`single_file_asset`]) unpacks its Python runtime to %TEMP% on every launch, about 0.7 s more
/// per run.
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
    #[cfg(windows)]
    let asset = if tokio::task::spawn_blocking(|| tar_works(&windows_tar())).await.unwrap_or(false) {
        ytdlp_release_asset()
    } else {
        tracing::warn!("{} does not run: installing the single-file yt-dlp, which starts more slowly", windows_tar().display());
        single_file_asset()
    };
    #[cfg(not(windows))]
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
    tokio::task::spawn_blocking(move || install_verified(&bin_dir, &tag, asset, &bytes, &expected))
        .await
        .map_err(|e| format!("yt-dlp install task failed: {e}"))?
}

/// Check `bytes` (release asset `asset`) against `expected_sha256`, then install them into
/// `bin_dir`: on Windows into the release's own folder (see [`install_onedir`]), elsewhere as one
/// file written next to its target and renamed into place. Returns the program to run. Blocking.
fn install_verified(bin_dir: &Path, tag: &str, asset: &str, bytes: &[u8], expected_sha256: &str) -> Result<PathBuf, String> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected_sha256 {
        return Err(format!(
            "Downloaded yt-dlp failed checksum verification (expected {expected_sha256}, got {actual})"
        ));
    }
    std::fs::create_dir_all(bin_dir).map_err(|e| format!("Failed to create {}: {e}", bin_dir.display()))?;
    #[cfg(windows)]
    return install_onedir(bin_dir, tag, asset, bytes);
    #[cfg(not(windows))]
    {
        let _ = (tag, asset);
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

/// Folder release `tag` is installed in.
#[cfg(windows)]
fn release_dir(bin_dir: &Path, tag: &str) -> PathBuf {
    bin_dir.join(format!("yt-dlp-{tag}"))
}

/// `yt-dlp.exe` of the newest release installed in `bin_dir`.
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

/// Installs release asset `asset` (`bytes`) into the release's own `yt-dlp-<tag>` folder: the zip
/// unpacked, or the single-file build (see [`single_file_asset`]) as `yt-dlp.exe`. The folder is
/// made under a temporary name and appears complete in one rename, which is what switches the
/// managed yt-dlp to this release (see [`newest_release`]); a running yt-dlp keeps running from
/// its own folder, which Windows would not let us replace anyway. Older releases are cleared away
/// afterwards (see [`remove_old_releases`]). Blocking.
#[cfg(windows)]
fn install_onedir(bin_dir: &Path, tag: &str, asset: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let target = release_dir(bin_dir, tag);
    let temp = format!(".yt-dlp-{tag}-{}", unique_suffix());
    let (archive, unpacked) = (bin_dir.join(format!("{temp}.zip")), bin_dir.join(format!("{temp}.tmp")));
    let fail = |e: std::io::Error| format!("Failed to install yt-dlp to {}: {e}", target.display());
    let extracted = std::fs::create_dir(&unpacked).and_then(|()| {
        if !asset.ends_with(".zip") {
            return std::fs::write(unpacked.join("yt-dlp.exe"), bytes);
        }
        let unzipped = std::fs::write(&archive, bytes).and_then(|()| unzip(&archive, &unpacked));
        let _ = std::fs::remove_file(&archive);
        unzipped
    });
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

/// Renames `from` to `to`, trying again for a moment while Windows refuses because a file is
/// open: antivirus scanners open new programs right after they are written (a folder with an open
/// file in it cannot be renamed either), and a program just ended (a killed ffmpeg) holds its
/// files until Windows has closed them. Gives up at once once `to` exists (another process
/// installed the same release). Blocking.
fn rename_patiently(from: &Path, to: &Path) -> std::io::Result<()> {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    let mut pause = Duration::from_millis(100);
    // 100 + 200 + 400 + 800 ms of waiting at most.
    for _ in 0..4 {
        match std::fs::rename(from, to) {
            Err(e)
                if cfg!(windows)
                    && (e.kind() == std::io::ErrorKind::PermissionDenied
                        || e.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
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

/// The tar.exe that ships with Windows (10 1803 and later), named by full path so no other tar on
/// PATH is used.
#[cfg(windows)]
fn windows_tar() -> PathBuf {
    let windows = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    windows.join("System32").join("tar.exe")
}

/// Whether `tar` runs, so [`unzip`] can unpack a release: older and trimmed-down Windows installs
/// have no tar.exe. Blocking.
#[cfg(windows)]
fn tar_works(tar: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(tar)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|status| status.success())
}

/// Extracts `archive` into `into` with [`windows_tar`]. It refuses entries that would land outside
/// `into`. Blocking.
#[cfg(windows)]
fn unzip(archive: &Path, into: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new(windows_tar())
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
        .user_agent(crate::resolver::APP_USER_AGENT);
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| format!("Invalid proxy {proxy}: {e}"))?);
    }
    builder.build().map_err(|e| format!("Failed to build HTTP client: {e}"))
}

/// Serializes installs and updates of the managed yt-dlp across concurrent media jobs.
static INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// When installing yt-dlp last failed, and why (see [`install_managed_ytdlp`]).
static INSTALL_FAILED: parking_lot::Mutex<Option<(tokio::time::Instant, String)>> = parking_lot::const_mutex(None);

/// How long after installing yt-dlp failed a run that may do without it does so rather than try
/// again: each web page downloaded would otherwise wait for another attempt (see
/// [`find_site_media`]).
const INSTALL_RETRY_AFTER: Duration = Duration::from_secs(15 * 60);

/// Installs the managed yt-dlp, unless a concurrent job did. Unless `retry`, a failure less than
/// [`INSTALL_RETRY_AFTER`] ago stands for this attempt, also for the jobs that waited for it.
async fn install_managed_ytdlp(proxy: Option<&str>, retry: bool) -> Result<PathBuf, String> {
    let _guard = INSTALL_LOCK.lock().await;
    // A concurrent job may have installed it while we waited for the lock.
    if let Ok(Some(installed)) = tokio::task::spawn_blocking(installed_managed_ytdlp).await {
        return Ok(installed);
    }
    unless_failed_lately(&INSTALL_FAILED, retry, async { download_ytdlp_binary(&http_client(proxy)?).await }).await
}

/// `install`, whose failure `failed` keeps; unless `retry` is false and the last one it keeps is
/// less than [`INSTALL_RETRY_AFTER`] old, which then stands for it.
async fn unless_failed_lately(
    failed: &parking_lot::Mutex<Option<(tokio::time::Instant, String)>>,
    retry: bool,
    install: impl std::future::Future<Output = Result<PathBuf, String>>,
) -> Result<PathBuf, String> {
    let lately = failed.lock().clone().filter(|(at, _)| !retry && at.elapsed() < INSTALL_RETRY_AFTER);
    if let Some((at, e)) = lately {
        return Err(format!("Installing yt-dlp failed {}s ago: {e}", at.elapsed().as_secs()));
    }
    let installed = install.await;
    *failed.lock() = installed.as_ref().err().map(|e| (tokio::time::Instant::now(), e.clone()));
    installed
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

/// Per-user folder for files that hold secrets while a yt-dlp run reads them: cookie jars (see
/// [`BrowserCookies`]) and what an extraction found, its cookies included (see [`download_with`]).
/// It is under %LOCALAPPDATA%, which only the user may open, or made private to the user
/// elsewhere. A file exists only while its run uses it; what a crashed process left is deleted by
/// the next media download, of this process or a later one.
struct PrivateDir {
    /// `None` without a per-user directory.
    dir: Option<PathBuf>,
    /// Whether `dir` is ready, cleared of what crashed processes left there.
    ready: tokio::sync::OnceCell<bool>,
}

impl PrivateDir {
    fn new(dir: Option<PathBuf>) -> Self {
        Self { dir, ready: tokio::sync::OnceCell::new() }
    }

    /// The folder, prepared the first time (see [`prepare_private_dir`]).
    async fn get(&self) -> Option<&Path> {
        let dir = self.dir.as_deref()?;
        let ready = self.ready.get_or_init(|| async {
            let dir = dir.to_path_buf();
            let prepared = tokio::task::spawn_blocking(move || prepare_private_dir(&dir)).await;
            match prepared.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| e.to_string())) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("No private folder for yt-dlp's cookies and extractions: {e}");
                    false
                }
            }
        });
        ready.await.then_some(dir)
    }

    /// A new file for one run, holding `contents`, or not created yet, for yt-dlp to write.
    async fn file(&self, contents: Option<Vec<u8>>, ext: &'static str) -> Result<PrivateFile, String> {
        let dir = self.get().await.ok_or("No private folder")?.to_path_buf();
        tokio::task::spawn_blocking(move || private_file(&dir, contents.as_deref(), ext))
            .await
            .map_err(|e| format!("Background task failed: {e}"))?
    }
}

/// Creates the private folder and deletes the files of runs no process holds any more (it
/// crashed or was killed). Blocking.
fn prepare_private_dir(dir: &Path) -> std::io::Result<()> {
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

/// A new file `<unique>.<ext>` in `dir`, claimed, readable by the user only: holding `contents`,
/// or not created yet. Blocking.
fn private_file(dir: &Path, contents: Option<&[u8]>, ext: &str) -> Result<PrivateFile, String> {
    use std::io::Write;
    let path = dir.join(format!("{}.{ext}", unique_suffix()));
    let claim = crate::engine::claim_target(&path)?.ok_or_else(|| format!("{} is in use", path.display()))?;
    if let Some(contents) = contents {
        let mut file = std::fs::OpenOptions::new();
        file.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
        if let Err(e) = file.open(&path).and_then(|mut f| f.write_all(contents)) {
            let _ = std::fs::remove_file(&path);
            return Err(format!("Failed to write {}: {e}", path.display()));
        }
    }
    Ok(PrivateFile { path, _claim: claim })
}

/// A file in the [`PrivateDir`], deleted on drop. Claimed for as long as it exists, which tells
/// other processes (see [`prepare_private_dir`]) that it is in use.
struct PrivateFile {
    path: PathBuf,
    _claim: crate::engine::TargetClaim,
}

impl Drop for PrivateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Browser cookies, read from the browser once in a while rather than for every run. yt-dlp takes
/// 0.2-0.5 s to read them (it copies and decrypts the browser's cookie database), so a run that
/// reads a browser also saves what it read: given `--cookies <file>` next to
/// `--cookies-from-browser`, yt-dlp writes its whole cookie jar to that file when it exits. Later
/// runs get the jar through `--cookies`, and yt-dlp writes it back with what the site renewed.
/// After [`JAR_LIFETIME`], or once a run fails with it, the next run reads the browser again.
///
/// The jar is kept in memory. It is on disk only while a yt-dlp run uses it, as a file of that run
/// in the [`PrivateDir`].
struct BrowserCookies {
    files: PrivateDir,
    jars: parking_lot::Mutex<HashMap<&'static str, Jar>>,
}

/// How long a jar read from the browser stands in for it. The browser renews some cookies as the
/// user browses (YouTube rotates its session cookies, and then rejects the old ones), which only
/// a new read picks up.
const JAR_LIFETIME: Duration = Duration::from_secs(15 * 60);

enum Jar {
    /// A run reads the browser and saves the jar.
    Saving,
    /// A jar read from the browser at `read_at`, with what the sites renewed since.
    Saved { jar: Arc<str>, read_at: tokio::time::Instant },
}

static BROWSER_COOKIES: LazyLock<BrowserCookies> =
    LazyLock::new(|| BrowserCookies::new(app_data_dir().map(|dir| dir.join("cookies"))));

impl BrowserCookies {
    /// Cookies whose files go to `dir` (see [`PrivateDir`]).
    fn new(dir: Option<PathBuf>) -> Self {
        Self { files: PrivateDir::new(dir), jars: parking_lot::Mutex::new(HashMap::new()) }
    }

    /// The cookie arguments of one yt-dlp run with `source`. A cookies file goes to yt-dlp as a
    /// copy of the run's own: yt-dlp writes its jar back into the file it reads when it exits,
    /// which would rewrite the user's on every run (dropping lines yt-dlp cannot read), several
    /// runs at once among them. One that cannot be read is left out, as yt-dlp leaves it out.
    async fn for_run(&self, source: &BrowserCookieSource) -> CookieRun<'_> {
        let mut run = CookieRun { cookies: self, args: source.to_args(), file: None, jar: None };
        if let BrowserCookieSource::File(path) = source {
            let copy = match tokio::fs::read(path).await {
                Ok(contents) => self.files.file(Some(contents), "txt").await,
                Err(e) => Err(e.to_string()),
            };
            match copy {
                Ok(file) => {
                    run.args = vec!["--cookies".to_string(), file.path.to_string_lossy().into_owned()];
                    run.file = Some(file);
                }
                Err(e) => {
                    tracing::warn!("yt-dlp runs without the cookies of {}: {e}", path.display());
                    run.args.clear();
                }
            }
            return run;
        }
        let Some(browser) = source.browser() else {
            return run;
        };
        if self.files.get().await.is_none() {
            return run;
        }
        let now = tokio::time::Instant::now();
        let saved = {
            let mut jars = self.jars.lock();
            match jars.get(browser) {
                // Another run reads the browser right now.
                Some(Jar::Saving) => return run,
                Some(Jar::Saved { jar, read_at }) if now.duration_since(*read_at) < JAR_LIFETIME => Some((Arc::clone(jar), *read_at)),
                _ => {
                    jars.insert(browser, Jar::Saving);
                    None
                }
            }
        };
        let contents = saved.as_ref().map(|(jar, _)| jar.as_bytes().to_vec());
        run.jar = Some(match saved {
            Some((jar, read_at)) => JarUse::Uses { browser, jar, read_at },
            None => JarUse::Reads { browser, started: now },
        });
        match self.files.file(contents, "txt").await {
            Ok(file) => {
                let file_args = ["--cookies".to_string(), file.path.to_string_lossy().into_owned()];
                if let Some(JarUse::Reads { .. }) = run.jar {
                    run.args.extend(file_args);
                } else {
                    run.args = file_args.to_vec();
                }
                run.file = Some(file);
            }
            Err(e) => {
                tracing::warn!("Could not keep the browser cookies for later downloads: {e}");
                // It reads the browser, as its arguments say, and has nowhere to save the jar.
                if let Some(JarUse::Uses { .. }) = run.jar {
                    run.jar = None;
                }
            }
        }
        run
    }
}

/// What a yt-dlp run does with the jar of a browser.
enum JarUse {
    /// It reads the browser (it starts at `started`), and yt-dlp saves the jar into its file.
    Reads { browser: &'static str, started: tokio::time::Instant },
    /// It got `jar`, which yt-dlp saves back with what the site renewed.
    Uses { browser: &'static str, jar: Arc<str>, read_at: tokio::time::Instant },
}

/// The cookie arguments of one yt-dlp run, and the file they name.
struct CookieRun<'a> {
    cookies: &'a BrowserCookies,
    args: Vec<String>,
    file: Option<PrivateFile>,
    jar: Option<JarUse>,
}

impl CookieRun<'_> {
    /// Keeps the jar yt-dlp saved: the one it read from the browser, or the jar it got with what
    /// the site renewed. A run that fails with a jar it got drops it instead, so the next run reads
    /// the browser again: the site may reject cookies the browser has renewed since. Only for a
    /// yt-dlp that exited by itself: one that was killed may have written half a jar.
    async fn finish(mut self, succeeded: bool) {
        let Some(jar_use) = self.jar.take() else {
            return;
        };
        let written = match &self.file {
            // yt-dlp starts the file with this header, even when it read no cookies.
            Some(file) => tokio::fs::read_to_string(&file.path).await.ok().filter(|jar| jar.starts_with("# Netscape HTTP Cookie File")),
            None => None,
        };
        let mut jars = self.cookies.jars.lock();
        match jar_use {
            JarUse::Reads { browser, started } => match written {
                Some(jar) => {
                    jars.insert(browser, Jar::Saved { jar: jar.into(), read_at: started });
                }
                // Nothing saved: the next run reads the browser again.
                None => {
                    jars.remove(browser);
                }
            },
            JarUse::Uses { browser, jar, read_at } => {
                // Unless the jar has been replaced meanwhile.
                if matches!(jars.get(browser), Some(Jar::Saved { jar: current, .. }) if Arc::ptr_eq(current, &jar)) {
                    match written {
                        _ if !succeeded => {
                            jars.remove(browser);
                        }
                        Some(renewed) => {
                            jars.insert(browser, Jar::Saved { jar: renewed.into(), read_at });
                        }
                        None => {}
                    }
                }
            }
        }
    }
}

impl Drop for CookieRun<'_> {
    fn drop(&mut self) {
        // Killed before it saved the jar it read: the next run reads the browser again.
        if let Some(JarUse::Reads { browser, .. }) = self.jar.take() {
            self.cookies.jars.lock().remove(browser);
        }
    }
}

/// What a yt-dlp run does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    /// Finds what to download and prints it as JSON (`-J`), for the engine to download
    /// (see [`fast_download`]), or else a Download run of [`Source::Info`] (see [`loads_found`]).
    /// Its formats are plain files and playlists: no `formats=dashy`.
    Extract,
    /// An Extract run that only yt-dlp's own sites may take (`--ies default,-generic`): a link to
    /// none of them fails at once, offline, instead of yt-dlp looking through the page for
    /// anything playable (see [`find_site_media`]).
    Find,
    /// Downloads and post-processes, printing progress for [`OutputState`].
    Download,
    /// Writes only the subtitles of what an extraction found ([`Source::Info`]), for a video the
    /// engine downloaded (see [`subtitle_args`]).
    Subtitles,
}

/// Whether a Download run with `options` asks YouTube for its formats in fragments (see
/// [`YOUTUBE_DASHY`]).
fn fragments_youtube(options: &MediaDownloadOptions) -> bool {
    !matches!(options.preset, MediaQualityPreset::Custom(_))
}

/// Whether a Download run can load what an extraction found (`info`, see [`Source::Info`]) rather
/// than find it again from the URL. yt-dlp drops a playlist's entries from a file it loads; and a
/// YouTube video's formats there are whole files (see [`RunKind::Extract`]), each of which the
/// download would pull 10 MiB request after request where it could have fetched fragments in
/// parallel.
fn loads_found(info: &Value, options: &MediaDownloadOptions) -> bool {
    let text = |key| info.get(key).and_then(Value::as_str);
    let video = text("_type").is_none_or(|t| t == "video");
    let youtube = text("extractor_key").or_else(|| text("extractor")).is_some_and(|e| e.eq_ignore_ascii_case("youtube"));
    video && !(youtube && fragments_youtube(options))
}

/// What a yt-dlp run works on.
#[derive(Debug, Clone, Copy)]
enum Source<'a> {
    Url(&'a Url),
    /// What an extraction found (its `-J` output), downloaded as it is (`--load-info-json`).
    Info(&'a Path),
}

/// yt-dlp arguments for one run. Paths in `options` must be absolute: yt-dlp runs in
/// [`ytdlp_work_dir`]. The cookie arguments come from [`BrowserCookies::for_run`]; the proxy is not
/// among them (see [`ytdlp_command`]).
fn build_ytdlp_args(
    source: Source<'_>,
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
        RunKind::Find => &["-J", "--flat-playlist", "--ies", "default,-generic"],
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
            "--print",
            LIVE_TEMPLATE,
            FFMPEG_PROGRESS[0],
            FFMPEG_PROGRESS[1],
            // No --http-chunk-size: a file then streams in one response instead of one request per
            // chunk, each a round trip of idle connection. YouTube asks for 10 MiB requests itself
            // (the format's `http_chunk_size`), which a global chunk size would override.
            "--buffer-size",
            "16M",
        ],
        RunKind::Subtitles => &["--skip-download"],
    };
    args.extend(kind_args.iter().map(|a| a.to_string()));
    if kind == RunKind::Download && fragments_youtube(options) {
        args.extend(YOUTUBE_DASHY.map(String::from));
    }
    // A live stream's formats from its start are other formats: every run that finds them asks
    // for those.
    if options.live_from_start && kind != RunKind::Subtitles {
        args.push("--live-from-start".to_string());
    }
    // A page is asked about for a moment only (see `find_site_media`).
    if options.wait_for_video && matches!(kind, RunKind::Extract | RunKind::Download) {
        args.extend(["--wait-for-video".to_string(), WAIT_FOR_VIDEO.to_string()]);
    }
    if matches!(kind, RunKind::Download | RunKind::Subtitles) {
        args.extend(subtitle_args(options, ffmpeg_dir.is_some()));
    }
    if kind == RunKind::Download {
        args.extend(embed_args(options, ffmpeg_dir.is_some()));
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
    match source {
        Source::Url(url) => args.push(url.to_string()),
        // yt-dlp neither extracts the video again nor picks up the formats' URLs anew, unless
        // their download fails: it then extracts from the video's page.
        Source::Info(file) => args.extend(["--load-info-json".to_string(), file.to_string_lossy().into_owned()]),
    }
    args
}

/// The subtitle languages `options` ask for, as `--sub-langs` takes them, and whether they are
/// `all` of them; None when they ask for none.
fn sub_langs(options: &MediaDownloadOptions) -> Option<(String, bool)> {
    let mut langs: Vec<&str> = options.subtitles.as_deref()?.split(',').map(str::trim).filter(|l| !l.is_empty()).collect();
    for lang in &mut langs {
        if lang.eq_ignore_ascii_case("all") {
            *lang = "all";
        }
    }
    let all = langs.contains(&"all");
    if all {
        // YouTube lists a stream's chat among its subtitles.
        langs.push("-live_chat");
    }
    (!langs.is_empty()).then(|| (langs.join(","), all))
}

/// yt-dlp arguments that write the subtitles `options` ask for next to the video: the site's
/// `.srt`, else its `.vtt`, else its best format, converted to `.srt` when ffmpeg is at hand
/// (yt-dlp converts TTML itself, ffmpeg the rest). A language named gets the site's subtitles, else
/// its automatic captions (a language only those have, `en-orig`, is one to name); `all` gets every
/// language the site has subtitles in, not the automatic captions, often a hundred machine
/// translations. A language the site lacks is left out; one that fails to download, or to convert
/// (its file then stays as the site had it), is a warning, not the failure of the video
/// (`--ignore-errors`, see [`OutputState::only_steps_failed`]).
fn subtitle_args(options: &MediaDownloadOptions, have_ffmpeg: bool) -> Vec<String> {
    let Some((langs, all)) = sub_langs(options) else { return Vec::new() };
    let mut args = vec!["--write-subs".to_string()];
    if !all {
        args.push("--write-auto-subs".to_string());
    }
    args.extend(["--sub-langs".to_string(), langs]);
    args.extend(["--sub-format", "srt/vtt/best"].map(String::from));
    if have_ffmpeg {
        args.extend(["--convert-subs", "srt"].map(String::from));
    }
    args.push("--ignore-errors".to_string());
    args
}

/// yt-dlp arguments that write the title, artist, date, description and URL tags and the chapters
/// into the file, and cover art into an audio file, as `options` ask. ffmpeg writes them: none
/// without it.
fn embed_args(options: &MediaDownloadOptions, have_ffmpeg: bool) -> Vec<String> {
    if !options.embed_metadata || !have_ffmpeg {
        return Vec::new();
    }
    let mut args = vec!["--embed-metadata", "--embed-chapters"];
    if options.preset.extracts_audio() {
        args.push("--embed-thumbnail");
    } else {
        args.extend(NO_FASTSTART_METADATA);
    }
    args.into_iter().map(String::from).collect()
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
    /// The file the video downloading goes to (see [`LIVE_TEMPLATE`]).
    file: String,
    /// The video downloading is a live stream: a recording, with no size to reach.
    live: bool,
    /// The files a live recording writes: where it goes, then the streams it records apart.
    recording: Vec<PathBuf>,
    /// The rate ffmpeg last reported (see [`FFMPEG_PROGRESS`]), in bytes per second.
    ffmpeg_rate: f64,
    /// The downloads are done and yt-dlp is making the file of them (see [`POSTPROCESS_TEMPLATE`]).
    finishing: bool,
    /// An `ERROR:` that no video yt-dlp finished (see [`PATH_TEMPLATE`]) has followed yet.
    error_pending: bool,
    /// A video failed: an error was pending when the next one began.
    video_failed: bool,
}

impl OutputState {
    /// Consume one output line; returns a progress update to forward, if any.
    fn handle_line(&mut self, line: &str, from_stderr: bool) -> Option<ProgressUpdate> {
        if line.starts_with(PROGRESS_MARK) {
            let progress = parse_progress_line(line)?;
            if self.live && !self.recording.iter().any(|file| file.as_os_str() == progress.stream) {
                self.recording.push(PathBuf::from(progress.stream));
            }
            let update = self.tracker.update(&progress);
            return Some(self.shown(update));
        }
        if let Some(size) = line.strip_prefix(PLANNED_MARK) {
            if let Some(bytes) = parse_template_number(size.trim()) {
                self.tracker.plan(bytes as u64);
            }
            return None;
        }
        if line.starts_with(POSTPROCESS_MARK) {
            self.finishing = true;
            let update = self.tracker.post_processing();
            return Some(self.shown(update));
        }
        if let Some(video) = line.strip_prefix(LIVE_MARK) {
            self.video_failed |= self.error_pending;
            let (live, file) = video.split_once(' ').unwrap_or((video, ""));
            self.live = live == "True";
            self.file = file.to_string();
            self.recording = if self.live && !file.is_empty() { vec![PathBuf::from(file)] } else { Vec::new() };
            // A recording's file is the one yt-dlp finishes for it, not an earlier video's.
            if self.live {
                self.final_path = None;
            }
            return None;
        }
        // ffmpeg's progress: its rate, then what it has written. Counted as the video's file, which
        // yt-dlp's last progress line for it names. Only a recording's: ffmpeg downloading the
        // formats of another video one by one writes each to a file of its own, whose last
        // progress line counts it already.
        if let Some(rate) = line.strip_prefix("bitrate=") {
            let kbits = rate.trim().strip_suffix("kbits/s").and_then(parse_template_number);
            self.ffmpeg_rate = kbits.map_or(0.0, |kbits| kbits * 1000.0 / 8.0);
            return None;
        }
        if let Some(size) = line.strip_prefix("total_size=").filter(|_| self.live) {
            let written = parse_template_number(size.trim())? as u64;
            let speed = Some(self.ffmpeg_rate);
            let progress = TemplateProgress { finished: false, downloaded: written, total: None, speed, eta: None, stream: &self.file };
            let update = self.tracker.update(&progress);
            return Some(self.shown(update));
        }
        if let Some(path) = line.strip_prefix(PATH_MARK) {
            // With --no-playlist this is the only file, otherwise the playlist's last one.
            self.final_path = Some(PathBuf::from(path));
            self.error_pending = false;
            return None;
        }
        if !from_stderr || line.trim().is_empty() {
            return None;
        }
        if line.starts_with("ERROR:") {
            self.errors.push(line.to_string());
            self.error_pending = true;
        } else if line.starts_with("WARNING:") {
            tracing::warn!("yt-dlp: {line}");
        }
        if self.stderr_tail.len() == STDERR_TAIL_LINES {
            self.stderr_tail.pop_front();
        }
        self.stderr_tail.push_back(line.to_string());
        None
    }

    /// `update` as it is shown: a live recording has no total (0, unknown) and no end to count
    /// down to.
    fn shown(&self, update: ProgressUpdate) -> ProgressUpdate {
        if self.live {
            ProgressUpdate { total: 0, eta_seconds: None, ..update }
        } else {
            update
        }
    }

    /// Whether each error yt-dlp reported was of a step of a video it went on to finish: with
    /// `--ignore-errors` (see [`subtitle_args`]) it goes past a step that fails once a video has
    /// begun (a subtitle it cannot convert, the cover art, the tags) and exits 1, the video whole.
    /// Not when a video failed, nor when an error came after the last video it finished.
    fn only_steps_failed(&self) -> bool {
        !self.errors.is_empty() && !self.error_pending && !self.video_failed
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

// SAFETY: a job object handle is a process-wide kernel handle, valid on any thread, and `&self`
// only terminates the job.
#[cfg(windows)]
unsafe impl Send for ProcessTree {}
#[cfg(windows)]
unsafe impl Sync for ProcessTree {}

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

    /// Asks the tree to stop as a Ctrl+C at a terminal does, which ends a yt-dlp live recording
    /// with its file finished (see [`run_ytdlp`]): a SIGINT to the process group on Unix, a Ctrl+C
    /// to yt-dlp's console on Windows (see [`ctrl_c_console_of`] and [`ctrl_c_by_helper`]).
    /// Returns whether it was sent.
    async fn interrupt(&self, child: &Child) -> bool {
        #[cfg(unix)]
        {
            let _ = child;
            // SAFETY: signals the process group we created; the leader is not reaped yet.
            self.pgid.is_some_and(|pgid| unsafe { libc::killpg(pgid, libc::SIGINT) } == 0)
        }
        #[cfg(windows)]
        {
            let Some(pid) = child.id() else { return false };
            tokio::task::spawn_blocking(move || ctrl_c_console_of(pid) || ctrl_c_by_helper(pid)).await.unwrap_or(false)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            false
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

/// Sends a Ctrl+C to the console process `pid` runs in, which reaches every process there (yt-dlp
/// and its ffmpeg: each yt-dlp gets a hidden console of its own, see [`hide_console`]). Only a
/// process without a console of its own can attach to it (the GUI; the CLI has a helper do it,
/// see [`serve_ctrl_c`]), and attaching holds for the whole process, so one at a time. The Ctrl+C
/// this process gets while attached is swallowed, and its standard handles, which attaching points
/// at that console, are put back. Blocking.
#[cfg(windows)]
fn ctrl_c_console_of(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, GetStdHandle, SetConsoleCtrlHandler, SetStdHandle,
        CTRL_C_EVENT, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    unsafe extern "system" fn swallow_ctrl_c(event: u32) -> BOOL {
        BOOL::from(event == CTRL_C_EVENT)
    }
    static ATTACHED: parking_lot::Mutex<()> = parking_lot::const_mutex(());
    static SWALLOWING: std::sync::Once = std::sync::Once::new();
    let _one_at_a_time = ATTACHED.lock();
    let std_handles = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    // SAFETY: plain Win32 calls; the handler is a function that lives as long as the process.
    unsafe {
        let saved = std_handles.map(|which| GetStdHandle(which));
        if AttachConsole(pid) == 0 {
            return false;
        }
        SWALLOWING.call_once(|| {
            SetConsoleCtrlHandler(Some(swallow_ctrl_c), 1);
        });
        let sent = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) != 0;
        // Each process on the console, this one too, gets it on a thread of its own.
        std::thread::sleep(Duration::from_millis(500));
        FreeConsole();
        for (which, handle) in std_handles.into_iter().zip(saved) {
            SetStdHandle(which, handle);
        }
        sent
    }
}

/// Names the process to whose console a program started as the Ctrl+C helper sends a Ctrl+C (see
/// [`serve_ctrl_c`]).
#[cfg(windows)]
const CTRL_C_PID: &str = "HYPERFETCH_CTRL_C_PID";

/// This program, which [`serve_ctrl_c`] made a Ctrl+C helper.
#[cfg(windows)]
static CTRL_C_HELPER: OnceLock<PathBuf> = OnceLock::new();

/// For the `main` of a program that downloads, called first. The Ctrl+C that finishes a live
/// recording can only be sent to yt-dlp's console by a process without a console of its own (see
/// [`ctrl_c_console_of`]), which a command-line program is not: it starts itself again without
/// one to send it. Started so, this sends it and exits; otherwise it lets the program do that.
/// Nothing on other systems, where a signal does it.
pub fn serve_ctrl_c() {
    #[cfg(windows)]
    {
        send_ctrl_c_if_asked();
        if let Ok(exe) = std::env::current_exe() {
            let _ = CTRL_C_HELPER.set(exe);
        }
    }
}

/// Started as the Ctrl+C helper (see [`serve_ctrl_c`]): sends the Ctrl+C and exits, with 0 when
/// it was sent.
#[cfg(windows)]
fn send_ctrl_c_if_asked() {
    if let Some(pid) = std::env::var(CTRL_C_PID).ok().and_then(|pid| pid.parse().ok()) {
        std::process::exit(if ctrl_c_console_of(pid) { 0 } else { 1 });
    }
}

/// How this program starts itself as the Ctrl+C helper, when [`serve_ctrl_c`] let it.
#[cfg(all(windows, not(test)))]
fn ctrl_c_helper() -> Option<std::process::Command> {
    CTRL_C_HELPER.get().map(std::process::Command::new)
}

#[cfg(all(windows, test))]
fn ctrl_c_helper() -> Option<std::process::Command> {
    tests::ctrl_c_helper()
}

/// [`ctrl_c_console_of`] `pid`, done by this program started again without a console (see
/// [`serve_ctrl_c`]). Waits for it, 10 s at most. Blocking.
#[cfg(windows)]
fn ctrl_c_by_helper(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let Some(mut helper) = ctrl_c_helper() else { return false };
    helper.env(CTRL_C_PID, pid.to_string()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut helper) = helper.creation_flags(DETACHED_PROCESS).spawn() else { return false };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match helper.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = helper.kill();
                let _ = helper.wait();
                return false;
            }
        }
    }
}

/// Lets the yt-dlp runs this process starts take the Ctrl+C that ends a live recording (see
/// [`ctrl_c_console_of`]): a program started with Ctrl+C ignored (from some shells, by `start /b`)
/// passes that on to what it starts. Changed only where no console of its own gets a Ctrl+C.
#[cfg(windows)]
fn let_children_take_ctrl_c() {
    use windows_sys::Win32::System::Console::{GetConsoleCP, SetConsoleCtrlHandler};
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: plain Win32 calls; GetConsoleCP fails (0) without a console.
    ONCE.call_once(|| unsafe {
        if GetConsoleCP() == 0 {
            SetConsoleCtrlHandler(None, 0);
        }
    });
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

/// Run yt-dlp once and return the file it produced. Cancelling kills it, but for a live recording,
/// which it is asked to end as a Ctrl+C ends it (one that has ended is left to finish its file),
/// and then gives the file recorded so far (see [`stopped`]).
async fn run_ytdlp(
    mut cmd: Command,
    progress_tx: Option<&Sender<ProgressUpdate>>,
    cancel_flag: Option<Arc<AtomicBool>>,
    ffmpeg: Option<&Path>,
) -> Result<PathBuf, String> {
    #[cfg(windows)]
    let_children_take_ctrl_c();
    let mut child = cmd.spawn().map_err(|e| {
        format!("Failed to spawn yt-dlp ({}): {e}", Path::new(cmd.as_std().get_program()).display())
    })?;
    let mut tree = ProcessTree::attach(&child);
    let mut stdout = BufReader::new(child.stdout.take().ok_or("Failed to capture yt-dlp stdout")?);
    let mut stderr = BufReader::new(child.stderr.take().ok_or("Failed to capture yt-dlp stderr")?);

    let cancelled = wait_cancelled(cancel_flag);
    tokio::pin!(cancelled);
    // A live recording asked to stop, and until when it may take to stop recording.
    let (mut interrupted, mut deadline) = (false, None);

    let mut state = OutputState::default();
    let (mut out_buf, mut err_buf) = (Vec::new(), Vec::new());
    let (mut out_open, mut err_open) = (true, true);
    // Drain both pipes until EOF so yt-dlp never blocks on a full pipe and its final
    // ERROR lines are always collected; then wait for it to exit.
    let status = loop {
        let update = tokio::select! {
            () = &mut cancelled, if !interrupted => {
                // A recording that has ended already is left to finish its file: a Ctrl+C would
                // cut that short.
                if state.live && (state.finishing || tree.interrupt(&child).await) {
                    interrupted = true;
                    deadline = (!state.finishing).then(|| tokio::time::Instant::now() + LIVE_STOP_GRACE);
                    continue;
                }
                tree.kill_and_reap(&mut child).await;
                return stopped(&state, ffmpeg).await;
            }
            () = sleep_until(deadline) => {
                tracing::warn!("yt-dlp did not stop the live recording within {}s; killing it", LIVE_STOP_GRACE.as_secs());
                tree.kill_and_reap(&mut child).await;
                return stopped(&state, ffmpeg).await;
            }
            read = stdout.read_until(b'\n', &mut out_buf), if out_open => {
                take_line(read, &mut out_buf, &mut out_open).and_then(|line| state.handle_line(&line, false))
            }
            read = stderr.read_until(b'\n', &mut err_buf), if err_open => {
                take_line(read, &mut err_buf, &mut err_open).and_then(|line| state.handle_line(&line, true))
            }
            status = child.wait(), if !out_open && !err_open => {
                break status.map_err(|e| format!("Failed to wait on yt-dlp: {e}"))?;
            }
        };
        if state.finishing {
            deadline = None;
        }
        if let (Some(update), Some(tx)) = (update, progress_tx) {
            // Progress is lossy by nature; never let a slow consumer stall pipe draining.
            let _ = tx.try_send(update);
        }
    };
    tree.disarm();

    if interrupted {
        return stopped(&state, ffmpeg).await;
    }
    let failed = !status.success();
    if failed && !state.only_steps_failed() {
        return Err(state.failure_message(status));
    }
    let path = state.final_path.take().ok_or("yt-dlp finished without reporting an output file")?;
    if !is_file(&path).await {
        return Err(if failed { state.failure_message(status) } else { format!("yt-dlp reported {} but no such file exists", path.display()) });
    }
    if failed {
        tracing::warn!("yt-dlp made {}, but: {}", path.display(), state.errors.join("\n"));
    }
    Ok(path)
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok_and(|m| m.is_file())
}

/// What a yt-dlp run that was stopped leaves: nothing (the download is cancelled), unless it was
/// recording a live stream. Then it is the file yt-dlp finished once asked to stop (see
/// [`ProcessTree::interrupt`]), even when a step after that failed; else what it recorded, made
/// playable (see [`keep_recording`] and [`join_recording`]). Nothing recorded yet is cancelled all
/// the same.
async fn stopped(state: &OutputState, ffmpeg: Option<&Path>) -> Result<PathBuf, String> {
    let Some(output) = state.recording.first().filter(|_| state.live) else {
        return Err(CANCELLED.to_string());
    };
    if let Some(path) = &state.final_path {
        if is_file(path).await {
            return Ok(path.clone());
        }
    }
    let mut parts = Vec::new();
    for file in &state.recording {
        let part = crate::engine::part_path(file);
        if is_file(&part).await {
            parts.push(part);
        }
    }
    match parts.as_slice() {
        // Moved to its name by yt-dlp before it was killed, or not started.
        [] if is_file(output).await => Ok(keep_recording(output, ffmpeg).await),
        [] => Err(CANCELLED.to_string()),
        [part] => Ok(keep_recording(part, ffmpeg).await),
        _ => join_recording(&parts, output, ffmpeg).await,
    }
}

/// Makes a stopped live recording's `file` (its `.part`, or the file yt-dlp moved it to) a playable
/// file of its own, never in place of another: MPEG-TS, as ffmpeg records HLS, is named `.ts`,
/// then remuxed to `.mp4` when ffmpeg is at hand (the `.ts` stays if that fails); a file ffmpeg
/// finished otherwise just loses its `.part`. Returns the file: `file` itself when it cannot be
/// moved, which is then the recording, never something to resume and overwrite.
async fn keep_recording(file: &Path, ffmpeg: Option<&Path>) -> PathBuf {
    let kept = |e: String| {
        tracing::warn!("Kept the recording as {}: {e}", file.display());
        file.to_path_buf()
    };
    let named = if file.extension().is_some_and(|ext| ext == "part") { file.with_extension("") } else { file.to_path_buf() };
    if !is_mpeg_ts(file).await {
        return move_to_free(file, &named).await.unwrap_or_else(kept);
    }
    let ts = match move_to_free(file, &named.with_extension("ts")).await {
        Ok(ts) => ts,
        Err(e) => return kept(e),
    };
    let Some(ffmpeg) = ffmpeg else { return ts };
    let mp4 = match free_name(&named.with_extension("mp4")).await {
        Ok(mp4) => mp4,
        Err(e) => {
            tracing::warn!("Kept the recording as {}: {e}", ts.display());
            return ts;
        }
    };
    let temp = merge_temp(&mp4);
    let mut args: Vec<OsString> = FFMPEG_QUIET.map(OsString::from).to_vec();
    args.extend([OsString::from("-i"), file_arg(&ts)]);
    args.extend(COPY_ALL.into_iter().chain(["-f", "mp4"]).map(OsString::from));
    args.push(file_arg(&temp));
    let mut cmd = tree_command(ffmpeg);
    cmd.args(args);
    let remuxed = match run_to_end(cmd, None).await {
        Ok(_) => tokio::fs::rename(&temp, &mp4).await.map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    match remuxed {
        Ok(()) => {
            if let Err(e) = tokio::fs::remove_file(&ts).await {
                tracing::warn!("Failed to delete {}: {e}", ts.display());
            }
            mp4
        }
        Err(e) => {
            tracing::warn!("Kept the recording as {}: remuxing it to MP4 failed: {e}", ts.display());
            let _ = tokio::fs::remove_file(&temp).await;
            ts
        }
    }
}

/// Joins the streams a stopped live recording downloaded apart (its video and audio from their
/// start) into `output`, or a free name after it, as yt-dlp would have. They stay if that fails.
async fn join_recording(parts: &[PathBuf], output: &Path, ffmpeg: Option<&Path>) -> Result<PathBuf, String> {
    let kept = |why: String| {
        let names: Vec<String> = parts.iter().map(|part| part.display().to_string()).collect();
        format!("Could not join the recorded streams ({why}); they are kept as {}", names.join(" and "))
    };
    let ffmpeg = ffmpeg.ok_or_else(|| kept("ffmpeg is missing".to_string()))?;
    let target = free_name(output).await?;
    let temp = merge_temp(&target);
    let mut args: Vec<OsString> = FFMPEG_QUIET.map(OsString::from).to_vec();
    for part in parts {
        args.extend([OsString::from("-i"), file_arg(part)]);
    }
    args.extend(["-c", "copy"].map(OsString::from));
    for i in 0..parts.len() {
        args.extend([OsString::from("-map"), OsString::from(i.to_string())]);
    }
    args.push(file_arg(&temp));
    let mut cmd = tree_command(ffmpeg);
    cmd.args(args);
    let joined = match run_to_end(cmd, None).await {
        Ok(_) => tokio::fs::rename(&temp, &target).await.map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    if let Err(e) = joined {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(kept(e));
    }
    for part in parts {
        if let Err(e) = tokio::fs::remove_file(part).await {
            tracing::warn!("Failed to delete {}: {e}", part.display());
        }
    }
    Ok(target)
}

/// Whether `file` starts as MPEG-TS does: a sync byte every 188 bytes.
async fn is_mpeg_ts(file: &Path) -> bool {
    let mut head = Vec::new();
    let read = async { tokio::fs::File::open(file).await?.take(189).read_to_end(&mut head).await };
    read.await.is_ok() && head.first() == Some(&0x47) && head.get(188).is_none_or(|&b| b == 0x47)
}

/// The first name from `path` on that no file, `.part` or claim has (see
/// `crate::engine::free_path`).
async fn free_name(path: &Path) -> Result<PathBuf, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::engine::free_path(&path)).await.map_err(|e| format!("Background task failed: {e}"))
}

/// Moves `file` to `to`, or the first free name after it, and returns where it went; `file`
/// stays where it is when that is `to`. `to`'s `.part` is taken while it is `file`. A process
/// just killed (see [`ProcessTree::kill_and_reap`]) may hold `file` a moment longer, which is
/// waited out (see [`rename_patiently`]).
async fn move_to_free(file: &Path, to: &Path) -> Result<PathBuf, String> {
    if file == to {
        return Ok(to.to_path_buf());
    }
    let own_part = crate::engine::part_path(to) == file && !tokio::fs::try_exists(to).await.unwrap_or(true);
    let to = if own_part { to.to_path_buf() } else { free_name(to).await? };
    let (from, target) = (file.to_path_buf(), to.clone());
    tokio::task::spawn_blocking(move || rename_patiently(&from, &target))
        .await
        .map_err(|e| format!("Background task failed: {e}"))?
        .map_err(|e| format!("Failed to move {} to {}: {e}", file.display(), to.display()))?;
    Ok(to)
}

/// Runs a [`tree_command`] to the end and returns its standard output, or the errors it printed
/// when it fails. Cancelling kills its process tree and waits for it to exit, so nothing it had
/// open (ffmpeg's output) is held any more once this returns.
async fn run_to_end(mut cmd: Command, cancel_flag: Option<Arc<AtomicBool>>) -> Result<Vec<u8>, String> {
    let program = Path::new(cmd.as_std().get_program()).display().to_string();
    let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn {program}: {e}"))?;
    let mut tree = ProcessTree::attach(&child);
    let (Some(mut out), Some(mut err)) = (child.stdout.take(), child.stderr.take()) else {
        tree.kill_and_reap(&mut child).await;
        return Err(format!("Failed to capture the output of {program}"));
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = tokio::select! {
        biased;
        // Both pipes drain while it runs, so it never blocks on a full one.
        ended = async { tokio::try_join!(child.wait(), out.read_to_end(&mut stdout), err.read_to_end(&mut stderr)) } => {
            ended.map_err(|e| format!("Failed to wait on {program}: {e}"))?.0
        }
        _ = wait_cancelled(cancel_flag) => {
            tree.kill_and_reap(&mut child).await;
            return Err(CANCELLED.to_string());
        }
    };
    tree.disarm();
    let stderr = String::from_utf8_lossy(&stderr);
    if status.success() {
        // yt-dlp's (subtitles it skips, as a download's are, see `OutputState::handle_line`).
        for warning in stderr.lines().filter(|l| l.starts_with("WARNING:")) {
            tracing::warn!("{program}: {warning}");
        }
        return Ok(stdout);
    }
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    // As a yt-dlp download reports them (see `OutputState::failure_message`), with the lines an
    // error goes on over (a geo-blocked video's "You might want to use a VPN ...", see
    // `site_failed`) up to the next message.
    let errors: Vec<&str> = lines
        .iter()
        .scan(false, |in_error, &line| {
            *in_error = line.starts_with("ERROR:") || (*in_error && !line.starts_with("WARNING:") && !line.starts_with('['));
            Some(in_error.then_some(line))
        })
        .flatten()
        .collect();
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    let tail = &lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..];
    Err(format!("{program} exited with {status}: {}", tail.join("\n")))
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

/// How every ffmpeg run here starts: quiet but for errors, never waiting for input, overwriting
/// its (temporary) output.
const FFMPEG_QUIET: [&str; 5] = ["-y", "-nostdin", "-hide_banner", "-loglevel", "error"];
/// Copies all of the first input, as yt-dlp's fixups do: without data streams, and past ones
/// ffmpeg does not know.
const COPY_ALL: [&str; 6] = ["-map", "0", "-dn", "-ignore_unknown", "-c", "copy"];

/// `path` as a `file:` URL, as yt-dlp passes paths to ffmpeg, so no name is taken for a protocol
/// or an option.
fn file_arg(path: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(path);
    url
}

/// ffmpeg arguments that turn `inputs` (the streams' files, in order) into `output` without
/// re-encoding, as `finish` says: several streams as yt-dlp's merger joins them, one as its MPEG-TS
/// / DASH m4a fixups remux it, or copied as it is. `tags` go in too, the chapters read from the
/// ffmetadata file `chapters` (see [`Tags`]). None asks for the second `faststart` pass (see
/// [`NO_FASTSTART`]).
fn ffmpeg_args(
    streams: &[PlannedStream],
    finish: Finish,
    inputs: &[PathBuf],
    tags: &Tags,
    chapters: Option<&Path>,
    output: &Path,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = FFMPEG_QUIET.map(OsString::from).to_vec();
    for input in inputs {
        args.extend([OsString::from("-i"), file_arg(input)]);
    }
    if let Some(chapters) = chapters {
        args.extend(["-f", "ffmetadata", "-i"].map(OsString::from));
        args.push(file_arg(chapters));
    }
    match finish {
        Finish::Merge => {
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
        Finish::Remux => args.extend(COPY_ALL.into_iter().chain(["-f", "mp4"]).map(OsString::from)),
        Finish::Rename => args.extend(COPY_ALL.map(OsString::from)),
    }
    if chapters.is_some() {
        args.extend([OsString::from("-map_chapters"), OsString::from(inputs.len().to_string())]);
    }
    for (name, value) in &tags.metadata {
        args.extend([OsString::from("-metadata"), OsString::from(format!("{name}={value}"))]);
    }
    args.push(file_arg(output));
    args
}

/// The tags and chapters yt-dlp's `--embed-metadata` writes into a file (see [`tags_of`]).
#[derive(Debug, Default, PartialEq)]
struct Tags {
    /// `-metadata` names and values, in yt-dlp's order.
    metadata: Vec<(&'static str, String)>,
    /// The chapters, as an ffmetadata file; None without any.
    chapters: Option<String>,
}

impl Tags {
    fn is_empty(&self) -> bool {
        self.metadata.is_empty() && self.chapters.is_none()
    }
}

/// The tags and chapters yt-dlp's `--embed-metadata --embed-chapters` would write for what it found
/// (`info`), from the fields it takes them from (its `FFmpegMetadataPP`): the title (a track's
/// own, else the video's), upload date, description, page URL, artist (the uploader, unless the
/// site names an artist) and what a site knows of albums, shows and genres, under the names ffmpeg
/// maps to each container's own tags. yt-dlp has filled in each chapter's times already.
fn tags_of(info: &Value) -> Tags {
    // yt-dlp's first field that is set, even when empty; lists are joined.
    let field = |keys: &[&str]| -> Option<String> {
        let value = keys.iter().find_map(|key| info.get(*key).filter(|v| !v.is_null()))?;
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Array(items) => {
                items.iter().map(|item| item.as_str().map_or_else(|| item.to_string(), str::to_string)).collect::<Vec<_>>().join(", ")
            }
            other => other.to_string(),
        };
        // ffmpeg cannot take a NUL on its command line.
        Some(text.replace('\0', "")).filter(|text| !text.is_empty())
    };
    let mut metadata = Vec::new();
    let mut add = |names: &[&'static str], keys: &[&str]| {
        if let Some(value) = field(keys) {
            metadata.extend(names.iter().map(|name| (*name, value.clone())));
        }
    };
    add(&["title"], &["track", "title"]);
    add(&["date"], &["upload_date"]);
    add(&["description", "synopsis"], &["description"]);
    add(&["purl", "comment"], &["webpage_url"]);
    add(&["track"], &["track_number"]);
    add(&["artist"], &["artist", "artists", "creator", "creators", "uploader", "uploader_id"]);
    add(&["composer"], &["composer", "composers"]);
    add(&["genre"], &["genre", "genres", "categories", "tags"]);
    add(&["album"], &["album", "series"]);
    add(&["album_artist"], &["album_artist", "album_artists"]);
    add(&["disc"], &["disc_number"]);
    add(&["show"], &["series"]);
    add(&["season_number"], &["season_number"]);
    add(&["episode_id"], &["episode", "episode_id"]);
    add(&["episode_sort"], &["episode_number"]);

    let escape = |text: &str| {
        text.chars().fold(String::new(), |mut out, c| {
            if matches!(c, '=' | ';' | '#' | '\\' | '\n') {
                out.push('\\');
            }
            out.push(c);
            out
        })
    };
    let listed = info.get("chapters").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let mut chapters = String::from(";FFMETADATA1\n");
    for chapter in listed {
        let time = |key| chapter.get(key).and_then(Value::as_f64);
        let (Some(start), Some(end)) = (time("start_time"), time("end_time")) else { continue };
        // ffmpeg refuses the whole file for a chapter that ends before it starts.
        if start < 0.0 || end < start {
            continue;
        }
        chapters.push_str(&format!("[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\n", (start * 1000.0) as u64, (end * 1000.0) as u64));
        if let Some(title) = chapter.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            chapters.push_str(&format!("title={}\n", escape(title)));
        }
    }
    Tags { metadata, chapters: chapters.contains("[CHAPTER]").then_some(chapters) }
}

/// Where ffmpeg reads the chapters it writes into `output` from, next to it.
fn chapters_file(output: &Path) -> PathBuf {
    let stem = output.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    output.with_file_name(format!("{stem}.hfchapters.txt"))
}

/// The stream of `output` that the file `name` next to it belongs to: a stream of any format
/// (see [`stream_path`]), also under the name the engine numbers when the plain one is taken
/// (`<stream> (1).<ext>`), or one of its partial files. Returns the stream's file name.
fn stream_of(output: &Path, name: &str) -> Option<String> {
    let file = [".part.hfstate", ".part.hlsstate", ".part"].iter().find_map(|partial| name.strip_suffix(partial)).unwrap_or(name);
    let (stem, ext) = file.rsplit_once('.')?;
    let plain = stem
        .strip_suffix(')')
        .and_then(|s| s.rsplit_once(" ("))
        .filter(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map_or(stem, |(s, _)| s);
    let head = plain.strip_suffix(".hf")?;
    let plain = format!("{plain}.{ext}");
    // Whichever `.f` starts the format id, only the very name `stream_path` makes counts.
    head.match_indices(".f")
        .any(|(at, _)| stream_path(output, &head[at + 2..], ext).file_name().and_then(|n| n.to_str()) == Some(plain.as_str()))
        .then(|| file.to_string())
}

/// Where ffmpeg joins the streams of `output` before the result is moved there.
fn merge_temp(output: &Path) -> PathBuf {
    let ext = output.extension().map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
    let stem = output.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    output.with_file_name(format!("{stem}.hfmerge{ext}"))
}

/// Deletes every stream of `output` next to it (see [`stream_of`]), finished or partial, with the
/// history entries the engine made for them, except what a download holds, and what a merge a
/// crash cut short left (see [`merge_temp`] and [`chapters_file`]). The caller holds the claim on
/// `output`, so no job is downloading these streams for it or joining them. Blocking.
fn remove_streams_of(output: &Path) {
    let Some(dir) = output.parent() else { return };
    for temp in [merge_temp(output), chapters_file(output)] {
        match std::fs::remove_file(&temp) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => tracing::warn!("Failed to delete {}: {e}", temp.display()),
            _ => {}
        }
    }
    let stream_name = |path: &Path| stream_of(output, path.file_name()?.to_str()?);
    let mut streams: Vec<String> =
        std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|entry| stream_name(&entry.path())).collect();
    let mut history = crate::history::DownloadHistoryManager::load();
    let entries: Vec<(String, String)> = history
        .entries()
        .iter()
        .filter(|e| e.file_path.parent() == Some(dir))
        .filter_map(|e| Some((e.id.clone(), stream_name(&e.file_path)?)))
        .collect();
    streams.extend(entries.iter().map(|(_, name)| name.clone()));
    streams.sort();
    streams.dedup();
    for name in streams {
        let path = dir.join(&name);
        // A download that holds the stream keeps it, and its history.
        if let Err(e) = crate::engine::discard_partial(&path) {
            tracing::debug!("{e}");
            continue;
        }
        let Ok(Some(_claim)) = crate::engine::claim_target(&path) else { continue };
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!("Failed to delete {}: {e}", path.display());
                continue;
            }
            _ => {}
        }
        for (id, _) in entries.iter().filter(|(_, stream)| *stream == name) {
            history.remove_entry(id);
        }
    }
}

/// [`remove_streams_of`] once something else (yt-dlp) has made `output`: the streams an earlier
/// attempt kept to resume are of no use any more. Skipped while another job holds `output`.
async fn remove_streams_after(output: &Path) {
    let output = output.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(Some(_claim)) = crate::engine::claim_target(&output) {
            remove_streams_of(&output);
        }
    })
    .await;
}

/// Why the engine did not make a media download (see [`fast_download`]).
#[derive(Debug, PartialEq)]
enum FastError {
    /// The engine does not download what yt-dlp found; yt-dlp does.
    Unsupported(String),
    /// Another job is making the same file right now.
    Busy(String),
    /// Downloading or joining the streams failed, or was cancelled.
    Failed(String),
}

/// Downloads what yt-dlp found (`info`, its `-J` output) with the engine instead of yt-dlp: every
/// stream at once, each over as many connections as the engine opens, then joined or remuxed
/// with ffmpeg as yt-dlp would, with the tags and chapters its `--embed-metadata` writes when
/// `options` ask for them (see [`tags_of`]). Until the output is made, the streams stay for the
/// next attempt to resume (see [`MediaStream::key`]); once it exists, or ffmpeg cannot join them,
/// every stream of it goes (see [`remove_streams_of`]).
async fn fast_download(
    info: &Value,
    options: &MediaDownloadOptions,
    ffmpeg: Option<&Path>,
    progress_tx: Option<&Sender<ProgressUpdate>>,
    cancel_flag: &Option<Arc<AtomicBool>>,
    fetch: &StreamFetcher<'_>,
) -> Result<PathBuf, FastError> {
    let plan = plan_fast(info, ffmpeg.is_some()).map_err(FastError::Unsupported)?;
    let output = plan.output.clone();
    if let Some(dir) = output.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| FastError::Failed(format!("Failed to create {}: {e}", dir.display())))?;
    }
    // Two jobs for the same video would write the same files.
    let claim = {
        let output = output.clone();
        tokio::task::spawn_blocking(move || crate::engine::claim_target(&output))
            .await
            .map_err(|e| FastError::Failed(format!("Background task failed: {e}")))?
            .map_err(FastError::Failed)?
    };
    // yt-dlp does not download a file that is already there either.
    if tokio::fs::metadata(&output).await.is_ok_and(|m| m.is_file()) {
        if claim.is_some() {
            let output = output.clone();
            let _ = tokio::task::spawn_blocking(move || remove_streams_of(&output)).await;
        }
        return Ok(output);
    }
    let Some(_claim) = claim else {
        return Err(FastError::Busy(format!("{} is being downloaded already", output.display())));
    };

    if let Some(at) = plan.available_at {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if at > now {
            tracing::info!("Waiting {}s before downloading, as the site requires", at - now);
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(at - now)) => {}
                () = wait_cancelled(cancel_flag.clone()) => return Err(FastError::Failed(CANCELLED.to_string())),
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
            client: stream_client(planned, options.proxy.as_deref()).map_err(FastError::Unsupported)?,
            path: stream_path(&output, &planned.format_id, &planned.ext),
            chunk_size: planned.chunk_size,
        });
    }
    let files = fetch_streams(streams, sizes, fetch, progress_tx, cancel_flag).await.map_err(FastError::Failed)?;
    let tags = if options.embed_metadata { tags_of(info) } else { Tags::default() };
    let made = joined(&plan, &files, ffmpeg, &tags, cancel_flag).await;
    // Cancelled while ffmpeg ran: the next attempt has every stream at hand.
    if made.is_err() && is_cancelled(cancel_flag) {
        return Err(FastError::Failed(CANCELLED.to_string()));
    }
    // In the output now, or what ffmpeg cannot join: of no further use either way.
    let _ = {
        let output = output.clone();
        tokio::task::spawn_blocking(move || remove_streams_of(&output)).await
    };
    made.map_err(FastError::Failed)
}

/// Makes the plan's output from the streams' `files`, with `tags`: the one file itself, or what
/// ffmpeg writes to a temporary name next to it. A file that is the output as it is goes through
/// ffmpeg only for its tags, when ffmpeg is at hand, and is kept without them if that fails.
async fn joined(
    plan: &FastPlan,
    files: &[PathBuf],
    ffmpeg: Option<&Path>,
    tags: &Tags,
    cancel_flag: &Option<Arc<AtomicBool>>,
) -> Result<PathBuf, String> {
    if plan.finish != Finish::Rename {
        return ffmpeg_output(plan, files, ffmpeg.ok_or("ffmpeg is missing")?, tags, cancel_flag).await;
    }
    let [file] = files else {
        return Err(format!("{} streams for one file", files.len()));
    };
    if let Some(ffmpeg) = ffmpeg.filter(|_| !tags.is_empty()) {
        match ffmpeg_output(plan, files, ffmpeg, tags, cancel_flag).await {
            Err(e) if !is_cancelled(cancel_flag) => tracing::warn!("Keeping {} without its tags: {e}", plan.output.display()),
            made => return made,
        }
    }
    let output = &plan.output;
    tokio::fs::rename(file, output)
        .await
        .map(|()| output.clone())
        .map_err(|e| format!("Failed to move {} to {}: {e}", file.display(), output.display()))
}

/// What ffmpeg makes of `files` and `tags` as the plan says (see [`ffmpeg_args`]), written to a
/// temporary name, then moved to the plan's output.
async fn ffmpeg_output(
    plan: &FastPlan,
    files: &[PathBuf],
    ffmpeg: &Path,
    tags: &Tags,
    cancel_flag: &Option<Arc<AtomicBool>>,
) -> Result<PathBuf, String> {
    let output = &plan.output;
    let temp = merge_temp(output);
    let chapters = match &tags.chapters {
        Some(text) => {
            let file = chapters_file(output);
            match tokio::fs::write(&file, text).await {
                Ok(()) => Some(file),
                Err(e) => {
                    tracing::warn!("Leaving out the chapters of {}: {}: {e}", output.display(), file.display());
                    None
                }
            }
        }
        None => None,
    };
    let mut cmd = tree_command(ffmpeg);
    cmd.args(ffmpeg_args(&plan.streams, plan.finish, files, tags, chapters.as_deref(), &temp));
    let made = match run_to_end(cmd, cancel_flag.clone()).await {
        Ok(_) => tokio::fs::rename(&temp, output)
            .await
            .map(|()| output.clone())
            .map_err(|e| format!("Failed to move {} to {}: {e}", temp.display(), output.display())),
        Err(e) => Err(e),
    };
    if let Some(chapters) = chapters {
        let _ = tokio::fs::remove_file(chapters).await;
    }
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
    download_media_with(url, options, progress_tx, cancel_flag, None, None).await
}

/// What one of yt-dlp's own sites found at a link (see [`find_site_media`]): its `-J` output,
/// which the download of it goes by instead of finding it again.
pub(crate) struct Extracted(Vec<u8>);

/// [`download_media`]; with `fetch`, yt-dlp first only finds the formats and `fetch` downloads
/// them (see [`fast_download`]). What that cannot do goes to yt-dlp as before. `extracted`, what
/// yt-dlp found at `url` moments ago with these options, stands in for finding it again.
pub(crate) async fn download_media_with(
    url: &Url,
    options: &MediaDownloadOptions,
    progress_tx: Option<Sender<ProgressUpdate>>,
    cancel_flag: Option<Arc<AtomicBool>>,
    fetch: Option<&StreamFetcher<'_>>,
    extracted: Option<Extracted>,
) -> Result<PathBuf, String> {
    let (options, tools) = prepare(options, &cancel_flag, true).await?;
    if tools.ffmpeg.is_none() {
        tracing::warn!(
            "ffmpeg not found: yt-dlp cannot merge separate video and audio streams, so it will fall back \
             to a lower-quality pre-merged format, and audio extraction presets will fail. Install ffmpeg \
             (e.g. `winget install Gyan.FFmpeg` or your package manager) or place it next to the application."
        );
    }
    download_with(url, &options, tools, progress_tx, cancel_flag, fetch, extracted).await.map_err(drm_refused)
}

/// What one of yt-dlp's own sites finds at `url` (see [`RunKind::Find`]), for a download with
/// `options` to go by. Fails when none of them takes the link, when the one that does finds
/// nothing there (an empty list, as a news site's section page gives), when yt-dlp cannot be
/// found, installed or run, or takes longer than [`FIND_TIMEOUT`], installing it included, or
/// when the extraction fails; [`site_failed`] tells the last apart.
pub(crate) async fn find_site_media(
    url: &Url,
    options: &MediaDownloadOptions,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<Extracted, String> {
    find_prepared(url, prepare(options, &cancel_flag, false), cancel_flag.clone()).await
}

/// [`find_site_media`] with the options and tools `prepared` gives, all within [`FIND_TIMEOUT`].
async fn find_prepared(
    url: &Url,
    prepared: impl std::future::Future<Output = Result<(MediaDownloadOptions, Tools<'static>), String>>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<Extracted, String> {
    let find = async {
        let (options, tools) = prepared.await?;
        find_with(url, &options, &tools, cancel_flag).await
    };
    // Dropped, the run's process tree is killed.
    match tokio::time::timeout(FIND_TIMEOUT, find).await {
        Ok(Ok(Extracted(json))) if lists_nothing(&json) => Err(NOTHING_FOUND.to_string()),
        Ok(found) => found.map_err(drm_refused),
        Err(_) => Err(format!("yt-dlp took over {}s", FIND_TIMEOUT.as_secs())),
    }
}

/// How long [`find_site_media`] waits for yt-dlp: a site on a slow or stalling host is left
/// alone rather than hold up the download of the page for as long as yt-dlp's retries last.
const FIND_TIMEOUT: Duration = if cfg!(test) { Duration::from_secs(3) } else { Duration::from_secs(45) };

/// How yt-dlp says that none of the sites it may use takes a link.
const NO_SITE: &str = "No suitable extractor";

/// How [`find_site_media`] says that the site that takes a link found nothing there.
const NOTHING_FOUND: &str = "yt-dlp found nothing to download there";

/// Whether yt-dlp's `-J` output is a list with nothing in it.
fn lists_nothing(json: &[u8]) -> bool {
    serde_json::from_slice::<Value>(json)
        .is_ok_and(|info| info["_type"] == "playlist" && info["entries"].as_array().is_none_or(Vec::is_empty))
}

/// How yt-dlp's sites say a page has no media, in their own words ("No video formats found!",
/// "There is no video.", "This article does not have a video.", "No media found"), matched in
/// lower case.
const NO_MEDIA: &[&str] = &["no video", "no media", "not a video", "not have a video", "not have any video", "not contain a video"];

/// What yt-dlp adds to an error its site did not expect: one that could not find on the page what
/// it looks for there ("Unable to extract media id", a KeyError), as Spiegel's, ABC News' and NBC
/// News' article pages without a video give. The errors a site expects, which say it failed (an
/// HTTP error, a private, removed or geo-blocked video, a login, a rate limit, cookies), come
/// without it.
const UNEXPECTED: &str = "please report this issue";

/// What yt-dlp adds, on a line of its own, to every error of a site that finds the video
/// geo-blocked, whatever the site's words: NetEase Music's are "No media links found; possibly
/// due to geo restriction".
const GEO_BLOCKED: &str = "you might want to use a vpn";

/// Whether `error`, from [`find_site_media`], is that of a site of yt-dlp's that took the link
/// and failed at it (a private, removed or geo-blocked video, a login it needs, an HTTP error, the
/// cookies it was given): an `ERROR:` of yt-dlp's other than the one for a link no site takes,
/// or for a page without media (see [`NO_MEDIA`] and [`UNEXPECTED`]; [`GEO_BLOCKED`] is never
/// that). A site broken on a page that has media says the same (El País on an article with a
/// video, the Guardian on a podcast's page): the page is then kept, as it was before yt-dlp was
/// asked, until yt-dlp is fixed. Not installing, starting or waiting for yt-dlp, which says
/// nothing of the link.
pub(crate) fn site_failed(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    let no_media = lower.contains(UNEXPECTED) || NO_MEDIA.iter().any(|words| lower.contains(words));
    error == DRM_REFUSED
        || (error.starts_with("ERROR:")
            && !error.contains(NO_SITE)
            && !error.contains("Unsupported URL")
            && (lower.contains(GEO_BLOCKED) || !no_media))
}

/// [`find_site_media`] with `tools`. Paths in `options` must be absolute.
async fn find_with(
    url: &Url,
    options: &MediaDownloadOptions,
    tools: &Tools<'_>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<Extracted, String> {
    let ffmpeg_dir = tools.ffmpeg.as_deref().and_then(Path::parent);
    let version = ytdlp_version(&tools.ytdlp, &tools.work_dir, tools.version_cache.as_deref()).await;
    let cookies = tools.cookies.for_run(&options.cookies).await;
    let args = build_ytdlp_args(Source::Url(url), options, RunKind::Find, &cookies.args, ffmpeg_dir, tools.js_runtime.as_deref(), version.as_deref());
    let found = run_to_end(ytdlp_command(&tools.ytdlp, &args, options.proxy.as_deref(), &tools.work_dir), cancel_flag.clone()).await;
    if !is_cancelled(&cancel_flag) {
        // A link no site takes says nothing of the cookies: none was sent.
        cookies.finish(found.as_ref().err().is_none_or(|e| e.contains(NO_SITE))).await;
    }
    found.map(Extracted)
}

/// What a media download fails with when yt-dlp finds the video DRM-protected (see
/// [`drm_refused`]).
pub(crate) const DRM_REFUSED: &str = "DRM-protected: not supported";

/// A failure as the user reads it: yt-dlp finding a video DRM-protected means it is not to be
/// downloaded at all, which it says in words of its own ("This video is DRM protected", "This
/// format is DRM protected; ...").
fn drm_refused(error: String) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains(" drm protected") || lower.contains(" drm-protected") {
        DRM_REFUSED.to_string()
    } else {
        error
    }
}

/// `options` with every path absolute, as yt-dlp needs them (it runs in [`ytdlp_work_dir`]), and
/// the programs a media download with them works with: yt-dlp is installed if none is found,
/// unless installing it failed lately and `retry_install` is false (see [`install_managed_ytdlp`]).
async fn prepare(
    options: &MediaDownloadOptions,
    cancel_flag: &Option<Arc<AtomicBool>>,
    retry_install: bool,
) -> Result<(MediaDownloadOptions, Tools<'static>), String> {
    // First-time discovery stats every PATH entry (possibly on slow network drives).
    let (found_ytdlp, ffmpeg, js_runtime) =
        tokio::task::spawn_blocking(|| (find_ytdlp_path(), find_ffmpeg_path(), find_js_runtime()))
            .await
            .map_err(|e| format!("Tool discovery failed: {e}"))?;

    let mut options = options.clone();
    options.output_dir = absolute(&options.output_dir)?;
    if let BrowserCookieSource::File(path) = &mut options.cookies {
        *path = absolute(path)?;
    }
    let work_dir = ytdlp_work_dir();
    tokio::fs::create_dir_all(&work_dir)
        .await
        .map_err(|e| format!("Failed to create {}: {e}", work_dir.display()))?;

    let ytdlp = match (&options.custom_ytdlp_path, found_ytdlp) {
        (Some(path), _) => absolute(path)?,
        (None, Some(path)) => path,
        (None, None) => {
            // A task of its own: a run that stops waiting for it (a page check out of time, a
            // cancelled download) leaves it to finish for the next one.
            let proxy = options.proxy.clone();
            let install = tokio::spawn(async move { install_managed_ytdlp(proxy.as_deref(), retry_install).await });
            tokio::select! {
                installed = install => installed.map_err(|e| format!("Installing yt-dlp failed: {e}"))??,
                _ = wait_cancelled(cancel_flag.clone()) => return Err(CANCELLED.to_string()),
            }
        }
    };
    let managed = managed_bin_dir().is_some_and(|dir| ytdlp.starts_with(dir));
    let tools = Tools {
        ytdlp,
        managed,
        ffmpeg,
        js_runtime,
        work_dir,
        version_cache: version_cache_file(),
        cookies: &BROWSER_COOKIES,
    };
    Ok((options, tools))
}

/// The programs and places a media download works with.
struct Tools<'a> {
    ytdlp: PathBuf,
    /// Whether `ytdlp` is ours to update (see [`update_managed_ytdlp`]).
    managed: bool,
    ffmpeg: Option<PathBuf>,
    js_runtime: Option<String>,
    /// See [`ytdlp_work_dir`].
    work_dir: PathBuf,
    /// See [`ytdlp_version`].
    version_cache: Option<PathBuf>,
    cookies: &'a BrowserCookies,
}

/// [`download_media_with`] with `tools`. Paths in `options` must be absolute.
async fn download_with(
    url: &Url,
    options: &MediaDownloadOptions,
    mut tools: Tools<'_>,
    progress_tx: Option<Sender<ProgressUpdate>>,
    cancel_flag: Option<Arc<AtomicBool>>,
    fetch: Option<&StreamFetcher<'_>>,
    mut extracted: Option<Extracted>,
) -> Result<PathBuf, String> {
    // Clears what crashed runs left in the private folder, whatever this download uses.
    tools.cookies.files.get().await;
    let ffmpeg_dir = tools.ffmpeg.as_deref().and_then(Path::parent).map(Path::to_path_buf);
    // Audio presets convert with ffmpeg as yt-dlp's post-processor does; those stay with yt-dlp.
    let fast = fetch.filter(|_| !options.preset.extracts_audio());
    let mut updated = false;
    loop {
        let version = ytdlp_version(&tools.ytdlp, &tools.work_dir, tools.version_cache.as_deref()).await;
        let command = |source, kind, cookies: &CookieRun| {
            let args = build_ytdlp_args(source, options, kind, &cookies.args, ffmpeg_dir.as_deref(), tools.js_runtime.as_deref(), version.as_deref());
            ytdlp_command(&tools.ytdlp, &args, options.proxy.as_deref(), &tools.work_dir)
        };
        let result = 'attempt: {
            // What yt-dlp found, for it to download when the engine does not.
            let mut found = None;
            // What yt-dlp found moments ago, else, for the engine to download, what it finds now.
            let json = match extracted.take() {
                Some(Extracted(json)) => Some(json),
                None if fast.is_some() => {
                    let cookies = tools.cookies.for_run(&options.cookies).await;
                    let run = run_to_end(command(Source::Url(url), RunKind::Extract, &cookies), cancel_flag.clone()).await;
                    if !is_cancelled(&cancel_flag) {
                        cookies.finish(run.is_ok()).await;
                    }
                    // The download would only extract the video again and fail the same way.
                    match run {
                        Ok(json) => Some(json),
                        Err(e) => break 'attempt Err(e),
                    }
                }
                None => None,
            };
            if let Some(json) = json {
                match serde_json::from_slice::<Value>(&json) {
                    Err(e) => tracing::info!("Leaving {url} to yt-dlp: yt-dlp -J: {e}"),
                    Ok(info) => {
                        let made = match fast {
                            Some(fetch) => {
                                fast_download(&info, options, tools.ffmpeg.as_deref(), progress_tx.as_ref(), &cancel_flag, fetch).await
                            }
                            None => Err(FastError::Unsupported("yt-dlp converts the audio itself".to_string())),
                        };
                        match made {
                            Ok(path) => {
                                // yt-dlp writes the subtitles next to it, of what it found.
                                if sub_langs(options).is_some() {
                                    let wrote = match tools.cookies.files.file(Some(json), "json").await {
                                        Ok(file) => {
                                            let cookies = tools.cookies.for_run(&options.cookies).await;
                                            let command = command(Source::Info(&file.path), RunKind::Subtitles, &cookies);
                                            let run = run_to_end(command, cancel_flag.clone()).await;
                                            if !is_cancelled(&cancel_flag) {
                                                cookies.finish(run.is_ok()).await;
                                            }
                                            run.map(drop)
                                        }
                                        Err(e) => Err(e),
                                    };
                                    // The video is whole whatever becomes of them (one yt-dlp could
                                    // not convert stays as the site had it).
                                    if let Err(e) = wrote {
                                        tracing::warn!("Subtitles of {}: {e}", path.display());
                                    }
                                }
                                return Ok(path);
                            }
                            Err(_) if is_cancelled(&cancel_flag) => return Err(CANCELLED.to_string()),
                            // yt-dlp would write the very file the other job is making.
                            Err(FastError::Busy(e)) => return Err(e),
                            // Found moments ago: yt-dlp downloads it as it is, where it can.
                            Err(FastError::Unsupported(why)) => {
                                tracing::info!("Leaving {url} to yt-dlp: {why}");
                                if loads_found(&info, options) {
                                    found = tools.cookies.files.file(Some(json), "json").await.inspect_err(|e| tracing::debug!("{e}")).ok();
                                }
                            }
                            // Maybe long after the extraction: the formats' URLs may have expired.
                            Err(FastError::Failed(e)) => tracing::info!("Leaving {url} to yt-dlp: {e}"),
                        }
                    }
                }
            }
            let source = found.as_ref().map_or(Source::Url(url), |file| Source::Info(&file.path));
            let cookies = tools.cookies.for_run(&options.cookies).await;
            let result =
                run_ytdlp(command(source, RunKind::Download, &cookies), progress_tx.as_ref(), cancel_flag.clone(), tools.ffmpeg.as_deref()).await;
            if !is_cancelled(&cancel_flag) {
                cookies.finish(result.is_ok()).await;
            }
            result
        };
        let err = match result {
            Ok(path) => {
                if fast.is_some() {
                    remove_streams_after(&path).await;
                }
                return Ok(path);
            }
            Err(err) => err,
        };
        if updated || !tools.managed || is_cancelled(&cancel_flag) {
            return Err(err);
        }

        // Sites (YouTube above all) break old yt-dlp releases often: retry once on the latest.
        let update = tokio::select! {
            update = update_managed_ytdlp(options.proxy.as_deref(), version.as_deref()) => update,
            _ = wait_cancelled(cancel_flag.clone()) => return Err(CANCELLED.to_string()),
        };
        match update {
            Ok(Some(installed)) => {
                tools.ytdlp = installed;
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
        let args = build_ytdlp_args(Source::Url(&url), &options, RunKind::Download, &[], None, None, Some("2026.08.19"));
        let at = args.iter().position(|a| a == "--extractor-args").expect("extractor arguments");
        assert_eq!(args[at + 1], "youtube:formats=dashy");
        let at = args.iter().position(|a| a == "--concurrent-fragments").expect("parallel fragments");
        assert_eq!(args[at + 1], "8");
        // dashy drops the formats YouTube gives no size for (format 18, often), which a custom
        // selection may name; our presets pick adaptive formats.
        for preset in [MediaQualityPreset::Hd720p, MediaQualityPreset::AudioMp3, MediaQualityPreset::AudioM4a] {
            let options = MediaDownloadOptions { preset, ..options.clone() };
            assert!(build_ytdlp_args(Source::Url(&url), &options, RunKind::Download, &[], None, None, None).contains(&YOUTUBE_DASHY[1].to_string()));
        }
        let custom = MediaDownloadOptions { preset: MediaQualityPreset::Custom("18".into()), ..options };
        let args = build_ytdlp_args(Source::Url(&url), &custom, RunKind::Download, &[], None, None, Some("2026.08.19"));
        assert!(!args.iter().any(|a| a == "--extractor-args"), "{args:?}");
    }

    #[test]
    fn an_extraction_only_finds_the_formats() {
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        let options = MediaDownloadOptions { output_dir: std::env::temp_dir(), concurrent_fragments: 8, ..Default::default() };
        let cookies = ["--cookies".to_string(), "jar.txt".to_string()];
        let args = build_ytdlp_args(Source::Url(&url), &options, RunKind::Extract, &cookies, None, Some("node"), Some("2026.08.19"));
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
        let args = build_ytdlp_args(Source::Url(&url), &options, RunKind::Download, &[], None, node, Some("2025.10.22"));
        assert!(!args.contains(&"--js-runtimes".to_string()));
        assert!(args.contains(&"--no-playlist".to_string()));
        assert!(args.contains(&PATH_TEMPLATE.to_string()));
        assert!(args.contains(&PROGRESS_TEMPLATE.to_string()));
        assert_eq!(args.last(), Some(&url.to_string()));

        let args = build_ytdlp_args(Source::Url(&url), &options, RunKind::Download, &[], None, node, Some("2025.11.12"));
        let at = args.iter().position(|a| a == "--js-runtimes").expect("flag present");
        assert_eq!(args[at + 1], "node:/usr/bin/node");
    }

    #[test]
    fn args_leave_request_sizes_to_the_site() {
        // A global chunk size splits every progressive file into round trips and overrides the
        // 10 MiB YouTube asks for in each format.
        let url = Url::parse("https://vimeo.com/123").unwrap();
        let options = MediaDownloadOptions { output_dir: PathBuf::from("out"), ..Default::default() };
        let args = build_ytdlp_args(Source::Url(&url), &options, RunKind::Download, &[], None, None, Some("2026.08.19"));
        assert!(!args.iter().any(|a| a == "--http-chunk-size"), "{args:?}");
    }

    #[test]
    fn args_never_load_config_files_or_plugins() {
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        let options = MediaDownloadOptions { output_dir: PathBuf::from("out"), ..Default::default() };
        for version in [None, Some("2024.12.23"), Some("2026.08.19")] {
            let args = build_ytdlp_args(Source::Url(&url), &options, RunKind::Download, &[], None, None, version);
            assert_eq!(args[0], "--ignore-config", "{version:?}");
        }
        let has_no_plugin_dirs = |version| {
            build_ytdlp_args(Source::Url(&url), &options, RunKind::Download, &[], None, None, version).contains(&"--no-plugin-dirs".to_string())
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
        let args = build_ytdlp_args(Source::Url(&url), &options, RunKind::Download, &[], None, None, None);
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

        assert!(install_verified(&bin, "2026.08.19", ytdlp_release_asset(), body, &"0".repeat(64)).is_err());
        assert!(!target.exists());

        assert_eq!(install_verified(&bin, "2026.08.19", ytdlp_release_asset(), body, &good).unwrap(), target);
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

        assert!(install_verified(&bin, "2026.08.19", "yt-dlp_win.zip", &zip, &"0".repeat(64)).is_err());
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

        let exe = install_verified(&bin, "2026.08.19", "yt-dlp_win.zip", &zip, &good).unwrap();
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
        assert_eq!(install_verified(&bin, "2026.08.19", "yt-dlp_win.zip", &zip, &good).unwrap(), exe);
        assert_eq!(left(), [".yt-dlp-2026.08.19-2.zip", "yt-dlp-2026.07.01", "yt-dlp-2026.08.19"]);
    }

    #[cfg(windows)]
    #[test]
    fn without_tar_the_single_file_build_is_installed_as_a_release() {
        assert!(tar_works(&windows_tar()));
        let dir = tempfile::tempdir().unwrap();
        assert!(!tar_works(&dir.path().join("tar.exe")), "no tar.exe, nothing to unpack the zip with");
        #[cfg(target_arch = "x86_64")]
        assert_eq!(single_file_asset(), "yt-dlp.exe");
        // The Windows builds a release lists (trimmed from 2026.09's SHA2-256SUMS).
        let sums = "30b4  yt-dlp_win.zip\n6667  yt-dlp.exe\na8f9  yt-dlp_x86.exe\n05b4  yt-dlp_arm64.exe\n";
        assert!(expected_sha256(sums, single_file_asset()).is_some(), "the release lists it");

        let bin = dir.path().join("bin");
        let build = b"single-file build";
        let good = format!("{:x}", Sha256::digest(build));
        assert!(install_verified(&bin, "2026.08.19", single_file_asset(), build, &"0".repeat(64)).is_err());
        assert!(!release_dir(&bin, "2026.08.19").exists());

        // An older release unpacked while tar.exe still worked: the new one supersedes it.
        std::fs::create_dir_all(release_dir(&bin, "2026.07.01")).unwrap();
        std::fs::write(release_dir(&bin, "2026.07.01").join("yt-dlp.exe"), b"previous build").unwrap();
        let exe = install_verified(&bin, "2026.08.19", single_file_asset(), build, &good).unwrap();
        assert_eq!(exe, release_dir(&bin, "2026.08.19").join("yt-dlp.exe"));
        assert_eq!(std::fs::read(&exe).unwrap(), build);
        assert_eq!(newest_release(&bin), Some(exe));
        let mut left: Vec<String> = std::fs::read_dir(&bin).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["yt-dlp-2026.07.01", "yt-dlp-2026.08.19"], "no temporary files");
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
        first.finish(true).await;
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

        // Each browser has its own jar.
        let firefox = cookies.for_run(&BrowserCookieSource::Firefox).await;
        assert_eq!(firefox.args[..2], ["--cookies-from-browser", "firefox"]);
        drop(firefox);
        assert_eq!(cookies.for_run(&BrowserCookieSource::None).await.args, Vec::<String>::new());
        // Nothing is left on disk, claim locks included.
        assert_eq!(names_in(&folder), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_cookies_file_goes_to_yt_dlp_as_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("cookies");
        let cookies = BrowserCookies::new(Some(folder.clone()));
        let mine = dir.path().join("mine.txt");
        let kept = "# Netscape HTTP Cookie File\n# my note, which yt-dlp would drop\n";
        std::fs::write(&mine, kept).unwrap();

        // Each run gets a copy of its own, which yt-dlp may write as it exits.
        let run = cookies.for_run(&BrowserCookieSource::File(mine.clone())).await;
        let other = cookies.for_run(&BrowserCookieSource::File(mine.clone())).await;
        assert_eq!(run.args[0], "--cookies");
        let copy = PathBuf::from(&run.args[1]);
        assert!(copy.starts_with(&folder) && copy.as_os_str() != other.args[1].as_str(), "{copy:?}");
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), kept);
        std::fs::write(&copy, "# This file is generated by yt-dlp.  Do not edit.\n").unwrap();
        run.finish(true).await;
        drop(other);
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), kept);
        // A file that cannot be read is left out.
        let gone = cookies.for_run(&BrowserCookieSource::File(dir.path().join("gone.txt"))).await;
        assert_eq!(gone.args, Vec::<String>::new());
        drop(gone);
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
        run.finish(false).await;
        // It was killed as it wrote the jar (a killed run is not finished).
        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert!(saves(&run));
        std::fs::write(&run.args[3], &JAR[..20]).unwrap();
        drop(run);
        // It wrote something else.
        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert!(saves(&run));
        std::fs::write(&run.args[3], "ERROR").unwrap();
        run.finish(true).await;

        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert!(saves(&run));
        std::fs::write(&run.args[3], JAR).unwrap();
        run.finish(true).await;
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

    /// Whether `pid` exits (and is reaped) within `within`.
    fn process_exits(pid: u32, within: Duration) -> bool {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
            use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
            unsafe {
                let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
                if handle.is_null() {
                    return true;
                }
                let exited = WaitForSingleObject(handle, u32::try_from(within.as_millis()).unwrap_or(u32::MAX)) == WAIT_OBJECT_0;
                CloseHandle(handle);
                exited
            }
        }
        #[cfg(unix)]
        {
            let deadline = std::time::Instant::now() + within;
            loop {
                if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    #[tokio::test]
    async fn cancel_kills_whole_process_tree() {
        let (mut child, mut tree, grandchild) = spawn_tree_with_grandchild().await;
        tree.kill_and_reap(&mut child).await;
        assert!(process_exits(grandchild, Duration::from_secs(10)), "grandchild {grandchild} survived cancel");
    }

    #[tokio::test]
    async fn dropping_the_download_kills_whole_process_tree() {
        let (child, tree, grandchild) = spawn_tree_with_grandchild().await;
        drop((child, tree));
        assert!(process_exits(grandchild, Duration::from_secs(10)), "grandchild {grandchild} survived drop");
    }

    #[tokio::test]
    async fn a_cancelled_run_has_ended_when_it_returns() {
        // Standing in for ffmpeg writing a merge: whatever it holds open must be free to delete
        // once the cancelled run returns.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        #[cfg(windows)]
        let cmd = {
            let mut cmd = tree_command(Path::new("powershell"));
            cmd.args(["-NoProfile", "-Command", &format!("$PID | Out-File -Encoding ascii '{}'; Start-Sleep 120", pid_file.display())]);
            cmd
        };
        #[cfg(unix)]
        let cmd = {
            let mut cmd = tree_command(Path::new("sh"));
            cmd.args(["-c", &format!("echo $$ > '{}'; exec sleep 120", pid_file.display())]);
            cmd
        };
        let flag = Arc::new(AtomicBool::new(false));
        let run = tokio::spawn(run_to_end(cmd, Some(Arc::clone(&flag))));
        let started = async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse::<u32>().ok()) {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        let pid = tokio::time::timeout(Duration::from_secs(30), started).await.expect("the command started");
        flag.store(true, Ordering::Relaxed);
        assert_eq!(run.await.unwrap(), Err(CANCELLED.to_string()));
        assert!(process_exits(pid, Duration::ZERO), "{pid} was still running when the cancelled run returned");
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
        let path = download_media_with(&url, &options, None, None, Some(fetch), None).await.unwrap();
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
        let tagged = |plan: &FastPlan, tags: &Tags, chapters: Option<&Path>| {
            let args = ffmpeg_args(&plan.streams, plan.finish, &inputs[..plan.streams.len()], tags, chapters, &out.join("t.mp4"));
            args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
        };
        let args = |plan: &FastPlan| tagged(plan, &Tags::default(), None);
        let (video, audio, temp) = (file(&inputs[0]), file(&inputs[1]), file(&out.join("t.mp4")));
        assert_eq!(
            args(&plan),
            format!("-y -nostdin -hide_banner -loglevel error -i {video} -i {audio} -c copy -map 0:v:0 -map 1:a:0 {temp}")
        );
        // The tags after the streams, the chapters from an input of their own.
        let chapters = out.join("t.hfchapters.txt");
        let tags = Tags { metadata: vec![("title", "A = b".to_string()), ("artist", "Me".to_string())], chapters: Some(String::new()) };
        assert_eq!(
            tagged(&plan, &tags, Some(&chapters)),
            format!(
                "-y -nostdin -hide_banner -loglevel error -i {video} -i {audio} -f ffmetadata -i {} -c copy -map 0:v:0 -map 1:a:0 \
                 -map_chapters 2 -metadata title=A = b -metadata artist=Me {temp}",
                file(&chapters)
            )
        );
        // Streams yt-dlp cannot tell have audio (or video) are mapped only if they have.
        plan.streams[0].audio = None;
        assert!(args(&plan).contains("-map 0:a:0? -map 0:v:0 -map 1:a:0"), "{}", args(&plan));
        plan.streams.truncate(1);
        plan.finish = Finish::Remux;
        assert!(args(&plan).ends_with(&format!("-i {video} -map 0 -dn -ignore_unknown -c copy -f mp4 {temp}")));
        assert!(!args(&plan).contains("movflags"));
        // A file that is the output as it is, copied for its tags into the same format.
        plan.finish = Finish::Rename;
        assert!(tagged(&plan, &tags, None).ends_with(&format!("-i {video} -map 0 -dn -ignore_unknown -c copy -metadata title=A = b -metadata artist=Me {temp}")));
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
    async fn a_failed_stream_stops_the_others_and_keeps_what_they_fetched() {
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
        assert_eq!(result, Err(FastError::Failed("HTTP 403".to_string())));
        assert!(stopped.load(Ordering::Relaxed));
        // For the next attempt to resume; yt-dlp's own files have other names.
        assert_eq!(names_in(&out), ["clip.f137.hf.mp4.part"]);
    }

    #[tokio::test]
    async fn streams_ffmpeg_cannot_join_are_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let fetch: &StreamFetcher<'_> = &|stream, _tx, _stop| {
            Box::pin(async move {
                std::fs::write(&stream.path, b"not media").unwrap();
                Ok(stream.path)
            })
        };
        let ffmpeg = dir.path().join("no-ffmpeg-here");
        let result = fast_download(&mp4_merge_info(&out), &MediaDownloadOptions::default(), Some(&ffmpeg), None, &None, fetch).await;
        assert!(matches!(&result, Err(FastError::Failed(e)) if e.contains("no-ffmpeg-here")), "{result:?}");
        assert_eq!(names_in(&out), Vec::<String>::new());
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
        assert_eq!(result, Err(FastError::Failed(CANCELLED.to_string())));
        assert_eq!(names_in(&out), ["clip.f137.hf.mp4.part", "clip.f140.hf.m4a.part"]);
    }

    #[tokio::test]
    async fn a_file_another_job_is_making_is_left_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let _other_job = crate::engine::claim_target(&dir.path().join("clip.mp4")).unwrap().unwrap();
        let fetch: &StreamFetcher<'_> = &|_, _, _| panic!("nothing to fetch");
        let result = fast_download(&mp4_merge_info(dir.path()), &MediaDownloadOptions::default(), Some(Path::new("never-run")), None, &None, fetch).await;
        assert!(matches!(&result, Err(FastError::Busy(e)) if e.contains("being downloaded already")), "{result:?}");
    }

    #[test]
    fn stream_files_are_told_by_their_names() {
        let dir = std::env::temp_dir();
        let output = dir.join("My.fine clip.mp4");
        let stream = |name: &str| stream_of(&output, name);
        for (name, of) in [
            ("My.fine clip.f137.hf.mp4", "My.fine clip.f137.hf.mp4"),
            ("My.fine clip.f137.hf.mp4.part", "My.fine clip.f137.hf.mp4"),
            ("My.fine clip.f137.hf.mp4.part.hfstate", "My.fine clip.f137.hf.mp4"),
            ("My.fine clip.fhls-480.hf.mp4.part.hlsstate", "My.fine clip.fhls-480.hf.mp4"),
            ("My.fine clip.f140.hf (2).m4a", "My.fine clip.f140.hf (2).m4a"),
            ("My.fine clip.f140.hf (2).m4a.part", "My.fine clip.f140.hf (2).m4a"),
        ] {
            assert_eq!(stream(name).as_deref(), Some(of), "{name}");
        }
        for name in [
            "My.fine clip.mp4",
            "My.fine clip.f137.mp4.part",
            "My.fine clip.hfmerge.mp4",
            "My.fine clip.f137.hf.mp4.part.lock",
            "Other.f137.hf.mp4",
            "My.fine.f137.hf.mp4",
            "My.fine clip.f137.hf ().mp4",
        ] {
            assert_eq!(stream(name), None, "{name}");
        }
        // A long title is cut differently for each format; each is still told apart.
        let long = dir.join(format!("{}.mp4", "\u{e9}".repeat(150)));
        for (format, ext) in [("137", "mp4"), ("hls-1080p-audio", "m4a")] {
            let name = stream_path(&long, format, ext).file_name().unwrap().to_str().unwrap().to_string();
            assert_eq!(stream_of(&long, &format!("{name}.part")), Some(name));
        }
    }

    #[tokio::test]
    async fn an_existing_output_is_not_downloaded_again_and_its_streams_go() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("clip.mp4");
        std::fs::write(&output, b"done").unwrap();
        // What earlier attempts left: a partial stream of another format, a finished stream the
        // engine recorded in history and one under a numbered name, an HLS stream's state, the
        // merge a crash cut short, and the history entry of a stream already deleted.
        for name in [
            "clip.f136.hf.mp4.part",
            "clip.f136.hf.mp4.part.hfstate",
            "clip.f140.hf.m4a",
            "clip.f137.hf (1).mp4",
            "clip.fhls-1.hf.mp4.part.hlsstate",
            "clip.hfmerge.mp4",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let mut history = crate::history::DownloadHistoryManager::load();
        for name in ["clip.f140.hf.m4a", "clip.f22.hf.mp4", "other.f137.hf.mp4"] {
            let urls = vec![format!("hyperfetch-media:/Youtube/jNQXAC9IVRw/{name}")];
            history.add_or_update(crate::history::HistoryEntry::new(name.to_string(), dir.path().join(name), 1, urls));
        }
        // A stream a download holds, yt-dlp's own partial file, another video's stream.
        let held = dir.path().join("clip.f251.hf.webm");
        let _held = crate::engine::claim_target(&held).unwrap().unwrap();
        for name in ["clip.f251.hf.webm.part", "clip.f137.mp4.part", "other.f137.hf.mp4"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }

        let fetch: &StreamFetcher<'_> = &|_, _, _| panic!("nothing to fetch");
        let result = fast_download(&mp4_merge_info(dir.path()), &MediaDownloadOptions::default(), Some(Path::new("never-run")), None, &None, fetch).await;
        assert_eq!(result, Ok(output));
        assert_eq!(
            names_in(dir.path()),
            ["clip.f137.mp4.part", "clip.f251.hf.webm.part", "clip.f251.hf.webm.part.lock", "clip.mp4", "other.f137.hf.mp4"]
        );
        let recorded: Vec<String> = crate::history::DownloadHistoryManager::load()
            .entries()
            .iter()
            .filter(|e| e.file_path.starts_with(dir.path()))
            .map(|e| e.file_name.clone())
            .collect();
        assert_eq!(recorded, ["other.f137.hf.mp4"]);
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
        assert_eq!(done, Ok(path.clone()), "the partial download resumed rather than set aside");
        assert!(std::fs::read(&path).unwrap() == body, "the resumed file holds the stream's bytes");
        remove_streams_of(&dir.path().join("clip.mp4"));
        assert_eq!(names_in(dir.path()), Vec::<String>::new());
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

    #[tokio::test]
    async fn a_finished_hls_stream_is_not_downloaded_again() {
        let (addr, hits) = crate::hls::tests::serve(|path, _| match path.split('?').next().unwrap_or(path) {
            "/v/index.m3u8" => (200, String::new(), b"#EXTM3U\n#EXTINF:2,\na.ts\n#EXTINF:2,\nb.ts\n#EXT-X-ENDLIST\n".to_vec()),
            "/v/a.ts" => (200, String::new(), vec![1; 1000]),
            "/v/b.ts" => (200, String::new(), vec![2; 500]),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.fhls-audio.hf.mp4");
        let key = Url::parse("hyperfetch-media:/Vimeo/x/hls-audio").unwrap();
        let engine = crate::engine::DownloadEngine::new(Vec::new(), crate::engine::DownloadOptions::default());
        // Each attempt extracts the stream anew, under a URL of its own.
        let fetch = |token: &str| {
            let planned = planned_stream(Url::parse(&format!("http://{addr}/v/index.m3u8?t={token}")).unwrap(), key.clone(), true);
            let stream = MediaStream {
                url: planned.url.clone(),
                key: planned.key.clone(),
                hls: true,
                client: stream_client(&planned, None).unwrap(),
                path: path.clone(),
                chunk_size: None,
            };
            let (tx, rx) = broadcast::channel(16);
            (engine.download_media_stream(stream, tx, CancellationToken::new()), rx)
        };
        let (first, _rx) = fetch("1");
        assert_eq!(first.await, Ok(path.clone()));
        let requests = hits.lock().values().sum::<usize>();

        // The video's other stream failed, or was stopped, and the user tries again: this stream
        // is taken as it is, and its size reported, not fetched again into "clip... (1).mp4".
        let (again, mut rx) = fetch("2");
        assert_eq!(again.await, Ok(path.clone()));
        assert_eq!(hits.lock().values().sum::<usize>(), requests, "{:?}", hits.lock());
        assert_eq!(rx.try_recv().map(|s| (s.downloaded_bytes, s.total_bytes)).ok(), Some((1500, 1500)));
        assert_eq!(names_in(dir.path()), ["clip.fhls-audio.hf.mp4"]);

        // Once it is joined into the output, it goes with its history entry, and a later attempt
        // fetches it anew.
        remove_streams_of(&dir.path().join("clip.mp4"));
        let (after, _rx) = fetch("3");
        assert_eq!(after.await, Ok(path.clone()));
        assert!(hits.lock().values().sum::<usize>() > requests);
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

    /// A stand-in for yt-dlp that works in `dir`. `-J` prints `dir/info.json`, or fails as an
    /// extractor does while `dir/extract-error` exists; a download writes `output` and reports it.
    /// Each run adds a line to `dir/runs.txt`: `extract`, or `download url` / `download info`, the
    /// latter for `--load-info-json`, whose file it copies to `dir/loaded.json`. A run for the
    /// subtitles only (`--skip-download`) writes `output`'s English ones and adds `subtitles info`.
    fn fake_ytdlp(dir: &Path, output: &Path) -> PathBuf {
        let (dir_s, out_s, out_dir) = (dir.display(), output.display(), output.parent().unwrap().display());
        let subs = output.with_extension("en.srt");
        let subs_s = subs.display();
        #[cfg(windows)]
        let (bin, script) = (
            dir.join("yt-dlp.cmd"),
            format!(
                "@echo off\r\n\
                 if \"%~2\"==\"--version\" (echo 2026.08.19& exit /b 0)\r\n\
                 if \"%~4\"==\"-J\" goto extract\r\n\
                 set kind=download\r\n\
                 if \"%~4\"==\"--skip-download\" set kind=subtitles\r\n\
                 set source=url\r\n\
                 :scan\r\n\
                 if \"%~1\"==\"\" goto download\r\n\
                 if \"%~1\"==\"--load-info-json\" (set source=info& copy /y \"%~2\" \"{dir_s}\\loaded.json\" >nul)\r\n\
                 shift\r\n\
                 goto scan\r\n\
                 :download\r\n\
                 if \"%kind%\"==\"subtitles\" goto subtitles\r\n\
                 >>\"{dir_s}\\runs.txt\" echo download %source%\r\n\
                 if not exist \"{out_dir}\" mkdir \"{out_dir}\"\r\n\
                 >\"{out_s}\" echo video\r\n\
                 echo HFPATH {out_s}\r\n\
                 exit /b 0\r\n\
                 :subtitles\r\n\
                 >>\"{dir_s}\\runs.txt\" echo subtitles %source%\r\n\
                 >\"{subs_s}\" echo 1\r\n\
                 exit /b 0\r\n\
                 :extract\r\n\
                 >>\"{dir_s}\\runs.txt\" echo extract\r\n\
                 if exist \"{dir_s}\\extract-error\" (>&2 echo ERROR: [youtube] abc: Video unavailable& exit /b 1)\r\n\
                 type \"{dir_s}\\info.json\"\r\n\
                 exit /b 0\r\n"
            ),
        );
        #[cfg(not(windows))]
        let (bin, script) = (
            dir.join("yt-dlp"),
            format!(
                "#!/bin/sh\n\
                 [ \"$2\" = --version ] && {{ echo 2026.08.19; exit 0; }}\n\
                 if [ \"$4\" = -J ]; then\n\
                 echo extract >> '{dir_s}/runs.txt'\n\
                 [ -e '{dir_s}/extract-error' ] && {{ echo 'ERROR: [youtube] abc: Video unavailable' >&2; exit 1; }}\n\
                 cat '{dir_s}/info.json'; exit 0\n\
                 fi\n\
                 kind=download\n\
                 [ \"$4\" = --skip-download ] && kind=subtitles\n\
                 source=url\n\
                 while [ $# -gt 0 ]; do\n\
                 [ \"$1\" = --load-info-json ] && {{ source=info; cp \"$2\" '{dir_s}/loaded.json'; }}\n\
                 shift\n\
                 done\n\
                 if [ $kind = subtitles ]; then\n\
                 echo \"subtitles $source\" >> '{dir_s}/runs.txt'\n\
                 echo 1 > '{subs_s}'; exit 0\n\
                 fi\n\
                 echo \"download $source\" >> '{dir_s}/runs.txt'\n\
                 mkdir -p '{out_dir}'\n\
                 echo video > '{out_s}'\n\
                 echo 'HFPATH {out_s}'\n"
            ),
        );
        std::fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        make_executable(&bin).unwrap();
        bin
    }

    /// The runs `fake_ytdlp` saw in `dir`.
    fn runs_in(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("runs.txt")).unwrap_or_default().lines().map(|l| l.trim().to_string()).collect()
    }

    /// A media download with `fake_ytdlp`: what `info` describes goes to `out/clip.mp4`, private
    /// files to `private`.
    async fn fake_download(
        dir: &Path,
        info: &Value,
        cookies: &BrowserCookies,
        fetch: &StreamFetcher<'_>,
    ) -> Result<PathBuf, String> {
        fake_download_with(dir, info, cookies, fetch, MediaDownloadOptions::default()).await
    }

    /// [`fake_download`] with `options`, but for the folder.
    async fn fake_download_with(
        dir: &Path,
        info: &Value,
        cookies: &BrowserCookies,
        fetch: &StreamFetcher<'_>,
        options: MediaDownloadOptions,
    ) -> Result<PathBuf, String> {
        let out = dir.join("out");
        std::fs::write(dir.join("info.json"), serde_json::to_vec(info).unwrap()).unwrap();
        let tools = Tools {
            ytdlp: fake_ytdlp(dir, &out.join("clip.mp4")),
            managed: false,
            ffmpeg: Some(dir.join("no-ffmpeg-here")),
            js_runtime: None,
            work_dir: dir.to_path_buf(),
            version_cache: None,
            cookies,
        };
        let options = MediaDownloadOptions { output_dir: out, ..options };
        let url = Url::parse("https://www.youtube.com/watch?v=jNQXAC9IVRw").unwrap();
        download_with(&url, &options, tools, None, None, Some(fetch), None).await
    }

    #[tokio::test]
    async fn yt_dlp_never_writes_a_file_another_job_is_making() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = BrowserCookies::new(Some(dir.path().join("private")));
        std::fs::create_dir(dir.path().join("out")).unwrap();
        let _other_job = crate::engine::claim_target(&dir.path().join("out").join("clip.mp4")).unwrap().unwrap();
        let fetch: &StreamFetcher<'_> = &|_, _, _| panic!("nothing to fetch");
        let result = fake_download(dir.path(), &mp4_merge_info(&dir.path().join("out")), &cookies, fetch).await;
        assert!(result.as_ref().is_err_and(|e| e.contains("being downloaded already")), "{result:?}");
        assert_eq!(runs_in(dir.path()), ["extract"]);
    }

    #[tokio::test]
    async fn after_a_failed_attempt_yt_dlp_downloads_and_the_kept_streams_go() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = BrowserCookies::new(Some(dir.path().join("private")));
        let fetch: &StreamFetcher<'_> = &|stream, _tx, _stop| {
            Box::pin(async move {
                std::fs::write(with_part(&stream.path), b"partial").unwrap();
                Err("HTTP 403".to_string())
            })
        };
        let out = dir.path().join("out");
        let path = fake_download(dir.path(), &mp4_merge_info(&out), &cookies, fetch).await.unwrap();
        assert_eq!(path, out.join("clip.mp4"));
        // The attempt may have taken long enough for the formats' URLs to expire: yt-dlp finds
        // them again.
        assert_eq!(runs_in(dir.path()), ["extract", "download url"]);
        assert_eq!(names_in(&out), ["clip.mp4"]);
    }

    /// yt-dlp's `-J` for a video of one DASH format (fragments) on a site other than YouTube, which
    /// the engine leaves to yt-dlp.
    fn dash_info(out: &Path) -> Value {
        let mut info = merge_info(out, Value::Null, Value::Null);
        info["requested_formats"] = Value::Null;
        for (key, value) in format_info("137", "mp4", "avc1", "none", 1000).as_object().unwrap() {
            info[key] = value.clone();
        }
        info["protocol"] = "http_dash_segments".into();
        (info["extractor"], info["extractor_key"]) = ("vimeo".into(), "Vimeo".into());
        info
    }

    #[test]
    fn only_a_download_that_would_find_the_same_formats_loads_what_was_found() {
        let options = MediaDownloadOptions::default();
        let custom = MediaDownloadOptions { preset: MediaQualityPreset::Custom("18".to_string()), ..Default::default() };
        let out = std::env::temp_dir();
        assert!(loads_found(&dash_info(&out), &options));
        // Found again from the URL, YouTube's formats come in fragments.
        assert!(!loads_found(&youtube_info(&out), &options));
        let mut named = youtube_info(&out);
        named["extractor_key"] = Value::Null;
        assert!(!loads_found(&named, &options));
        // A format selection of its own gets whole files anyway.
        assert!(loads_found(&youtube_info(&out), &custom));
        let playlist = serde_json::json!({"_type": "playlist", "id": "PL1", "extractor_key": "Vimeo"});
        assert!(!loads_found(&playlist, &options));
    }

    #[tokio::test]
    async fn streams_a_cancelled_attempt_kept_go_once_yt_dlp_makes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = BrowserCookies::new(Some(dir.path().join("private")));
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        for name in ["clip.f136.hf.mp4.part", "clip.f136.hf.mp4.part.hfstate", "clip.f140.hf.m4a"] {
            std::fs::write(out.join(name), b"x").unwrap();
        }
        let fetch: &StreamFetcher<'_> = &|_, _, _| panic!("yt-dlp downloads DASH fragments");
        let path = fake_download(dir.path(), &dash_info(&out), &cookies, fetch).await.unwrap();
        assert_eq!(path, out.join("clip.mp4"));
        assert_eq!(runs_in(dir.path()), ["extract", "download info"]);
        assert_eq!(names_in(&out), ["clip.mp4"]);
    }

    #[test]
    fn a_download_of_what_an_extraction_found_names_no_url() {
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        let options = MediaDownloadOptions { output_dir: std::env::temp_dir(), ..Default::default() };
        let found = std::env::temp_dir().join("found.json");
        let args = build_ytdlp_args(Source::Info(&found), &options, RunKind::Download, &[], None, None, Some("2026.08.19"));
        assert_eq!(args[args.len() - 2..], ["--load-info-json".to_string(), found.to_string_lossy().into_owned()]);
        assert!(!args.contains(&url.to_string()), "{args:?}");
        assert!(args.contains(&PATH_TEMPLATE.to_string()), "the download still reports its file");
    }

    #[tokio::test]
    async fn what_the_engine_leaves_to_yt_dlp_is_not_extracted_again() {
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        let cookies = BrowserCookies::new(Some(private.clone()));
        let fetch: &StreamFetcher<'_> = &|_, _, _| panic!("yt-dlp downloads DASH fragments");
        let info = dash_info(&dir.path().join("out"));
        fake_download(dir.path(), &info, &cookies, fetch).await.unwrap();
        assert_eq!(runs_in(dir.path()), ["extract", "download info"]);
        assert_eq!(std::fs::read(dir.path().join("loaded.json")).unwrap(), std::fs::read(dir.path().join("info.json")).unwrap());
        // It holds the cookies the extraction used: gone with the run.
        assert_eq!(names_in(&private), Vec::<String>::new());

        // yt-dlp drops a playlist's entries from a file it loads: it gets the URL again.
        std::fs::remove_file(dir.path().join("runs.txt")).unwrap();
        let playlist = serde_json::json!({"_type": "playlist", "id": "PL1", "entries": [{"_type": "url", "url": "https://youtu.be/a"}]});
        fake_download(dir.path(), &playlist, &cookies, fetch).await.unwrap();
        assert_eq!(runs_in(dir.path()), ["extract", "download url"]);

        // Nor does it load a YouTube video: found again, its formats come in fragments.
        std::fs::remove_file(dir.path().join("runs.txt")).unwrap();
        let mut youtube = dash_info(&dir.path().join("out"));
        (youtube["extractor"], youtube["extractor_key"]) = ("youtube".into(), "Youtube".into());
        fake_download(dir.path(), &youtube, &cookies, fetch).await.unwrap();
        assert_eq!(runs_in(dir.path()), ["extract", "download url"]);
    }

    #[tokio::test]
    async fn an_extraction_error_is_the_download_error() {
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        // What a run of a crashed process left: its jar and its claim's lock.
        std::fs::create_dir(&private).unwrap();
        std::fs::write(private.join("crashed.txt"), JAR).unwrap();
        std::fs::write(private.join("crashed.txt.part.lock"), b"").unwrap();
        std::fs::write(dir.path().join("extract-error"), b"").unwrap();
        let cookies = BrowserCookies::new(Some(private.clone()));
        let fetch: &StreamFetcher<'_> = &|_, _, _| panic!("nothing was found");
        let result = fake_download(dir.path(), &Value::Null, &cookies, fetch).await;
        assert_eq!(result, Err("ERROR: [youtube] abc: Video unavailable".to_string()));
        assert_eq!(runs_in(dir.path()), ["extract"], "no second extraction to fail the same way");
        // Swept at the first media download, though this one uses no browser cookies.
        assert_eq!(names_in(&private), Vec::<String>::new());
    }

    /// Whether `run` reads the browser (and saves what it read).
    fn reads_browser(run: &CookieRun) -> bool {
        run.args.len() == 4 && run.args[..3] == ["--cookies-from-browser", "chrome", "--cookies"]
    }

    /// Saves `jar` as the jar a run read from Chrome.
    async fn save_jar(cookies: &BrowserCookies, jar: &str) {
        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert!(reads_browser(&run), "{:?}", run.args);
        std::fs::write(&run.args[3], jar).unwrap();
        run.finish(true).await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_browser_is_read_again_once_its_jar_is_old() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = BrowserCookies::new(Some(dir.path().to_path_buf()));
        save_jar(&cookies, JAR).await;
        tokio::time::advance(JAR_LIFETIME - Duration::from_secs(1)).await;
        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert_eq!(run.args[0], "--cookies");
        // What yt-dlp writes back is no newer than the browser read it came from.
        run.finish(true).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert!(reads_browser(&run), "{:?}", run.args);
    }

    #[tokio::test]
    async fn a_jar_keeps_what_the_site_renews_and_goes_when_a_run_fails_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = BrowserCookies::new(Some(dir.path().to_path_buf()));
        save_jar(&cookies, JAR).await;
        // The site renewed a cookie, which yt-dlp saved back.
        let renewed = JAR.replace("secret", "renewed");
        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert_eq!(std::fs::read_to_string(&run.args[1]).unwrap(), JAR);
        std::fs::write(&run.args[1], &renewed).unwrap();
        run.finish(true).await;

        let run = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert_eq!(std::fs::read_to_string(&run.args[1]).unwrap(), renewed);
        // A run that got the jar before a new read replaced it leaves the new jar alone.
        let late = cookies.for_run(&BrowserCookieSource::Chrome).await;
        // "Sign in to confirm you're not a bot": the site no longer takes these cookies.
        run.finish(false).await;
        let again = cookies.for_run(&BrowserCookieSource::Chrome).await;
        assert!(reads_browser(&again), "{:?}", again.args);
        std::fs::write(&again.args[3], JAR).unwrap();
        again.finish(true).await;
        late.finish(false).await;
        assert_eq!(cookies.for_run(&BrowserCookieSource::Chrome).await.args[0], "--cookies");
    }

    #[test]
    fn a_find_asks_only_yt_dlps_own_sites() {
        let options = MediaDownloadOptions { output_dir: std::env::temp_dir(), ..Default::default() };
        let url = Url::parse("https://rumble.com/v4abc-clip.html").unwrap();
        let args = |kind| build_ytdlp_args(Source::Url(&url), &options, kind, &[], None, None, Some("2026.08.19"));
        let find = args(RunKind::Find);
        let ies = find.iter().position(|a| a == "--ies").expect("a find names the extractors it may use");
        assert_eq!(find[ies + 1], "default,-generic");
        // It is an extraction as any other, which a download can go by.
        let extract = args(RunKind::Extract);
        assert_eq!([&find[..ies], &find[ies + 2..]].concat(), extract);
        for kind in [RunKind::Extract, RunKind::Find, RunKind::Download] {
            let args = args(kind);
            assert!(kind == RunKind::Find || !args.contains(&"--ies".to_string()), "{kind:?}: {args:?}");
            assert!(!args.contains(&"--allow-unplayable-formats".to_string()), "{kind:?}: {args:?}");
        }
    }

    #[tokio::test]
    async fn what_a_find_found_is_downloaded_without_looking_again() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = BrowserCookies::new(Some(dir.path().join("private")));
        let out = dir.path().join("out");
        std::fs::write(dir.path().join("info.json"), serde_json::to_vec(&dash_info(&out)).unwrap()).unwrap();
        let tools = || Tools {
            ytdlp: fake_ytdlp(dir.path(), &out.join("clip.mp4")),
            managed: false,
            ffmpeg: Some(dir.path().join("no-ffmpeg-here")),
            js_runtime: None,
            work_dir: dir.path().to_path_buf(),
            version_cache: None,
            cookies: &cookies,
        };
        let fetch: &StreamFetcher<'_> = &|_, _, _| panic!("yt-dlp downloads DASH fragments");
        let url = Url::parse("https://vimeo.com/123").unwrap();
        // Video, which the engine may fetch, and audio, which yt-dlp converts itself: without what
        // was found, the latter would go to yt-dlp's download straight from the link.
        for preset in [MediaQualityPreset::BestVideoAudio, MediaQualityPreset::AudioMp3] {
            let _ = std::fs::remove_file(dir.path().join("runs.txt"));
            let options = MediaDownloadOptions { preset, output_dir: out.clone(), ..Default::default() };
            let found = find_with(&url, &options, &tools(), None).await.unwrap();
            let path = download_with(&url, &options, tools(), None, None, Some(fetch), Some(found)).await.unwrap();
            assert_eq!(path, out.join("clip.mp4"));
            assert_eq!(runs_in(dir.path()), ["extract", "download info"], "{:?}", options.preset);
            assert_eq!(std::fs::read(dir.path().join("loaded.json")).unwrap(), std::fs::read(dir.path().join("info.json")).unwrap());
        }

        // A find that fails is the error yt-dlp printed, and nothing is downloaded.
        std::fs::remove_file(dir.path().join("runs.txt")).unwrap();
        std::fs::write(dir.path().join("extract-error"), b"").unwrap();
        let options = MediaDownloadOptions { output_dir: out.clone(), ..Default::default() };
        let failed = find_with(&url, &options, &tools(), None).await.err();
        assert_eq!(failed.as_deref(), Some("ERROR: [youtube] abc: Video unavailable"));
        assert_eq!(runs_in(dir.path()), ["extract"]);
    }

    #[test]
    fn drm_failures_read_as_not_supported() {
        for error in [
            "ERROR: [SomeSite] 123: This video is DRM protected",
            "ERROR: [x] 1: The video is DRM-protected",
            "ERROR: [hlsnative] This format is DRM protected; Try selecting another format",
        ] {
            assert_eq!(drm_refused(error.to_string()), DRM_REFUSED);
        }
        // What only happens to hold the words keeps its own cause.
        for other in [
            "ERROR: [youtube] abc: Video unavailable",
            "ERROR: [vimeo] 12drm3: This video is password protected. Use the --video-password option",
            "ERROR: No suitable extractor found for URL https://x.example/drm/protected-clip",
            "ERROR: No suitable extractor found for URL https://x.example/drm-protected-clip",
        ] {
            assert_eq!(drm_refused(other.to_string()), other);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_install_is_not_tried_again_for_a_while_unless_asked() {
        let failed = parking_lot::Mutex::new(None);
        let tries = std::cell::Cell::new(0);
        let install = |works: bool| {
            let tries = &tries;
            async move {
                tries.set(tries.get() + 1);
                if works { Ok(PathBuf::from("yt-dlp")) } else { Err("offline".to_string()) }
            }
        };
        assert_eq!(unless_failed_lately(&failed, false, install(false)).await, Err("offline".to_string()));
        // The page checks right after it go without yt-dlp, the jobs that waited for it too.
        tokio::time::advance(INSTALL_RETRY_AFTER / 2).await;
        let err = unless_failed_lately(&failed, false, install(true)).await.unwrap_err();
        assert!(err.starts_with("Installing yt-dlp failed") && err.ends_with("offline"), "{err}");
        assert_eq!(tries.get(), 1);
        // A media download tries at once.
        assert_eq!(unless_failed_lately(&failed, true, install(false)).await, Err("offline".to_string()));
        assert_eq!(tries.get(), 2);
        // Later, so do page checks; a success forgets the failure.
        tokio::time::advance(INSTALL_RETRY_AFTER).await;
        assert_eq!(unless_failed_lately(&failed, false, install(true)).await, Ok(PathBuf::from("yt-dlp")));
        assert!(failed.lock().is_none());
    }

    #[tokio::test]
    async fn a_site_that_takes_too_long_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let (bin, script) = (dir.path().join("yt-dlp.cmd"), "@echo off\r\nif \"%~2\"==\"--version\" exit /b 1\r\nping -n 60 127.0.0.1 >nul\r\n");
        #[cfg(not(windows))]
        let (bin, script) = (dir.path().join("yt-dlp"), "#!/bin/sh\n[ \"$2\" = --version ] && exit 1\nsleep 60\n");
        std::fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        make_executable(&bin).unwrap();
        let options = MediaDownloadOptions { output_dir: dir.path().to_path_buf(), custom_ytdlp_path: Some(bin), ..Default::default() };
        let url = Url::parse("https://slow.example/watch/1").unwrap();

        let started = std::time::Instant::now();
        let err = find_site_media(&url, &options, None).await.err().expect("nothing was found");
        assert_eq!(err, format!("yt-dlp took over {}s", FIND_TIMEOUT.as_secs()));
        assert!(started.elapsed() < FIND_TIMEOUT + Duration::from_secs(20), "{:?}", started.elapsed());
        assert!(!site_failed(&err));
    }

    /// As yt-dlp 2026.08.19 answers a NetEase Music song from outside China when it cannot get
    /// past the block: the site's words say "no media", yt-dlp's lines after them say why.
    #[tokio::test]
    async fn a_geo_blocked_site_fails_the_link_whatever_its_words() {
        let dir = tempfile::tempdir().unwrap();
        let error = [
            "ERROR: [netease:song] 32102397: No media links found; possibly due to geo restriction",
            "This video is available in China.",
            "You might want to use a VPN or a proxy server (with --proxy) to workaround.",
        ];
        let warning = "WARNING: [netease:song] Video is geo restricted. Retrying extraction with fake IP 36.164.47.216 (CN) as X-Forwarded-For.";
        #[cfg(windows)]
        let (bin, script) = (
            dir.path().join("yt-dlp.cmd"),
            format!("@echo off\r\nif \"%~2\"==\"--version\" exit /b 1\r\n{}exit /b 1\r\n", [warning].iter().chain(&error).map(|l| format!(">&2 echo {l}\r\n")).collect::<String>()),
        );
        #[cfg(not(windows))]
        let (bin, script) = (
            dir.path().join("yt-dlp"),
            format!("#!/bin/sh\n[ \"$2\" = --version ] && exit 1\n{}exit 1\n", [warning].iter().chain(&error).map(|l| format!("echo '{l}' >&2\n")).collect::<String>()),
        );
        std::fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        make_executable(&bin).unwrap();
        let options = MediaDownloadOptions { output_dir: dir.path().to_path_buf(), custom_ytdlp_path: Some(bin), ..Default::default() };
        let url = Url::parse("https://music.163.com/song?id=32102397").unwrap();

        let err = find_site_media(&url, &options, None).await.err().expect("the site failed");
        assert_eq!(err, error.join("\n"));
        assert!(site_failed(&err));
    }

    /// Installing yt-dlp for a page check is waited for no longer than the check itself.
    #[tokio::test(start_paused = true)]
    async fn installing_yt_dlp_counts_toward_the_wait() {
        let url = Url::parse("https://slow.example/watch/1").unwrap();
        let installing = async {
            tokio::time::sleep(FIND_TIMEOUT * 10).await;
            Err::<(MediaDownloadOptions, Tools<'static>), _>("installed at last".to_string())
        };
        let err = find_prepared(&url, installing, None).await.err();
        assert_eq!(err, Some(format!("yt-dlp took over {}s", FIND_TIMEOUT.as_secs())));
    }

    #[tokio::test]
    async fn a_site_that_finds_nothing_leaves_the_page_alone() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let ytdlp = fake_ytdlp(dir.path(), &out.join("clip.mp4"));
        let options = MediaDownloadOptions { output_dir: out, custom_ytdlp_path: Some(ytdlp), ..Default::default() };
        let url = Url::parse("https://www.bbc.co.uk/news/technology").unwrap();
        // As yt-dlp's BBC site answers a section page: a list with nothing in it.
        for empty in [r#"{"_type": "playlist", "id": "technology", "entries": []}"#, r#"{"_type": "playlist", "id": "technology"}"#] {
            std::fs::write(dir.path().join("info.json"), empty).unwrap();
            let err = find_site_media(&url, &options, None).await.err().expect("nothing was found");
            assert_eq!(err, NOTHING_FOUND);
            assert!(!site_failed(&err));
        }
        let listed = r#"{"_type": "playlist", "id": "technology", "entries": [{"_type": "url", "url": "https://www.bbc.co.uk/news/av/1"}]}"#;
        std::fs::write(dir.path().join("info.json"), listed).unwrap();
        assert!(find_site_media(&url, &options, None).await.is_ok());
    }

    #[test]
    fn only_a_site_that_took_the_link_fails_it() {
        // As yt-dlp 2026.08.19 answered news article pages, and as its sites word a login, a
        // private, removed or geo-blocked video and a rate limit.
        let report = "; please report this issue on  https://github.com/yt-dlp/yt-dlp/issues?q= , filling out the appropriate \
                      issue template. Confirm you are on the latest version using  yt-dlp -U";
        for failed in [
            "ERROR: [Rumble] v000: Unable to download webpage: HTTP Error 404: Not Found",
            "ERROR: [NYTimesArticle] ai-government-regulation: Unable to download webpage: HTTP Error 403: Forbidden (caused by <HTTPError 403: Forbidden>)",
            "ERROR: [Patreon] 123: You do not have access to this post",
            "ERROR: [vimeo] 123: This video is only available for registered users. Use --cookies-from-browser or --cookies for the authentication.",
            "ERROR: [youtube] abc: Private video. Sign in if you've been granted access to this video",
            "ERROR: [youtube] abc: Video unavailable. This video has been removed by the uploader",
            "ERROR: [BBC] p0abc: This video is not available from your location due to geo restriction",
            "ERROR: [netease:song] 32102397: No media links found; possibly due to geo restriction\nThis video is available in China.\n\
             You might want to use a VPN or a proxy server (with --proxy) to workaround.",
            "ERROR: [twitch:vod] 123: Unable to download JSON metadata: HTTP Error 429: Too Many Requests",
            "ERROR: could not find firefox cookies database in 'C:/profile'",
            DRM_REFUSED,
        ] {
            assert!(site_failed(failed), "{failed}");
        }
        for nothing_there in [
            format!("ERROR: [Spiegel] 0861c578-806a-4762-b87d-6a88e987bafa: Unable to extract media id{report}"),
            format!("ERROR: [abc.net.au] 107201770: Unable to extract video urls{report}"),
            format!("ERROR: [ElPais] los-alonso-no-quieren-ser-llamados-buitres: Unable to extract URL prefix{report}"),
            format!("ERROR: rcna600052: An extractor error has occurred. (caused by KeyError('video')){report}"),
            format!("ERROR: [TheGuardianPodcast] protests-erupt: No video formats found!{report}"),
            "ERROR: [CNN] story: This article does not have a video.".to_string(),
            "ERROR: [twitter] 123: No video could be found in this tweet".to_string(),
        ] {
            assert!(!site_failed(&nothing_there), "{nothing_there}");
        }
        for none_took_it in [
            "ERROR: No suitable extractor found for URL https://example.com/a",
            "ERROR: Unsupported URL: https://example.com/a",
            NOTHING_FOUND,
            "Installing yt-dlp failed 3s ago: offline",
            "Failed to spawn C:\\yt-dlp.exe: The system cannot find the file specified. (os error 2)",
            "C:\\yt-dlp.exe exited with exit code: 1: Traceback (most recent call last):",
            "yt-dlp took over 45s",
        ] {
            assert!(!site_failed(none_took_it), "{none_took_it}");
        }
    }

    // ---- Subtitles, tags and live streams ----

    /// The arguments of a `kind` run with `options`, ffmpeg at hand or not.
    fn args_of(options: &MediaDownloadOptions, kind: RunKind, ffmpeg: bool) -> Vec<String> {
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        let ffmpeg_dir = std::env::temp_dir();
        build_ytdlp_args(Source::Url(&url), options, kind, &[], ffmpeg.then_some(ffmpeg_dir.as_path()), None, Some("2026.08.19"))
    }

    /// Whether `args` hold `wanted` in a row.
    fn holds(args: &[String], wanted: &[&str]) -> bool {
        args.windows(wanted.len()).any(|w| w.iter().zip(wanted).all(|(a, b)| a == b))
    }

    #[test]
    fn subtitles_are_asked_of_the_runs_that_write_files() {
        let named = MediaDownloadOptions { subtitles: Some(" en, es ,".to_string()), ..Default::default() };
        let wanted = ["--write-subs", "--write-auto-subs", "--sub-langs", "en,es", "--sub-format", "srt/vtt/best"];
        for kind in [RunKind::Download, RunKind::Subtitles] {
            // Converted to SRT by ffmpeg; without it, as the site has them.
            let args = args_of(&named, kind, true);
            assert!(holds(&args, &wanted) && holds(&args, &["--convert-subs", "srt", "--ignore-errors"]), "{args:?}");
            let args = args_of(&named, kind, false);
            assert!(holds(&args, &[&wanted[..], &["--ignore-errors"]].concat()), "{args:?}");
            assert!(!args.contains(&"--convert-subs".to_string()), "{args:?}");
        }
        // Every language the site has subtitles in, none of its machine translations nor a chat.
        let all = MediaDownloadOptions { subtitles: Some("ALL".to_string()), ..Default::default() };
        let args = args_of(&all, RunKind::Download, true);
        assert!(holds(&args, &["--write-subs", "--sub-langs", "all,-live_chat"]), "{args:?}");
        assert!(!args.contains(&"--write-auto-subs".to_string()), "{args:?}");
        // Finding what to download writes nothing; asked for none, no run writes any.
        for kind in [RunKind::Extract, RunKind::Find] {
            assert!(!args_of(&named, kind, true).contains(&"--write-subs".to_string()));
        }
        for none in [None, Some(""), Some(" , ")] {
            let options = MediaDownloadOptions { subtitles: none.map(str::to_string), ..Default::default() };
            assert!(!args_of(&options, RunKind::Download, true).contains(&"--write-subs".to_string()), "{none:?}");
        }
        // A subtitles run downloads nothing else and reports no progress.
        let only = args_of(&named, RunKind::Subtitles, true);
        assert!(only.contains(&"--skip-download".to_string()) && !only.contains(&"--embed-metadata".to_string()), "{only:?}");
        assert!(!only.contains(&PROGRESS_TEMPLATE.to_string()), "{only:?}");
    }

    /// With --ignore-errors yt-dlp exits 1 for a step that failed once a video began; the video it
    /// went on to finish is whole. A video that failed, or an error after the last finished one,
    /// fails the run.
    #[test]
    fn a_failed_step_of_a_finished_video_is_not_its_failure() {
        let out = std::env::temp_dir();
        let state = |lines: &[String]| {
            let mut state = OutputState::default();
            for line in lines {
                state.handle_line(line, line.starts_with("ERROR:"));
            }
            state
        };
        let video = |name: &str| format!("HFLIVE False {}", out.join(name).display());
        let made = |name: &str| format!("HFPATH {}", out.join(name).display());
        let error = "ERROR: Conversion failed!".to_string();
        // A subtitle it could not convert (before the download), the cover art (after it).
        for at in [1, 2] {
            let mut lines = vec![video("a.mp4"), "HFPOST a".to_string(), made("a.mp4")];
            lines.insert(at, error.clone());
            assert!(state(&lines).only_steps_failed(), "{lines:?}");
        }
        for lines in [
            // Nothing failed.
            vec![video("a.mp4"), made("a.mp4")],
            // The video, an earlier one of a playlist, or its extraction failed.
            vec![video("a.mp4"), error.clone()],
            vec![video("a.mp4"), error.clone(), video("b.mp4"), made("b.mp4")],
            vec![error.clone(), video("b.mp4"), made("b.mp4")],
            // After the last video.
            vec![video("a.mp4"), made("a.mp4"), error.clone()],
        ] {
            assert!(!state(&lines).only_steps_failed(), "{lines:?}");
        }
    }

    #[tokio::test]
    async fn a_video_yt_dlp_made_is_whole_whatever_step_failed_after() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("clip.mp4");
        let out_s = output.display();
        #[cfg(windows)]
        let (bin, script) = (
            dir.path().join("yt-dlp.cmd"),
            format!(
                "@echo off\r\necho HFLIVE False {out_s}\r\n>&2 echo ERROR: Conversion failed!\r\necho HFPOST x\r\n\
                 if \"%1\"==\"make\" echo video> \"{out_s}\"\r\necho HFPATH {out_s}\r\nexit /b 1\r\n"
            ),
        );
        #[cfg(not(windows))]
        let (bin, script) = (
            dir.path().join("yt-dlp"),
            format!(
                "#!/bin/sh\necho 'HFLIVE False {out_s}'\necho 'ERROR: Conversion failed!' >&2\necho HFPOST x\n\
                 [ \"$1\" = make ] && echo video > '{out_s}'\necho 'HFPATH {out_s}'\nexit 1\n"
            ),
        );
        std::fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        make_executable(&bin).unwrap();
        // The file it reported is missing: the step's error is the failure.
        assert_eq!(run_ytdlp(tree_command(&bin), None, None, None).await, Err("ERROR: Conversion failed!".to_string()));
        let mut make = tree_command(&bin);
        make.arg("make");
        assert_eq!(run_ytdlp(make, None, None, None).await, Ok(output));
    }

    #[test]
    fn tags_are_embedded_by_ffmpeg_as_asked() {
        let video = args_of(&MediaDownloadOptions::default(), RunKind::Download, true);
        assert!(holds(&video, &["--embed-metadata", "--embed-chapters", NO_FASTSTART_METADATA[0], NO_FASTSTART_METADATA[1]]), "{video:?}");
        assert!(!video.contains(&"--embed-thumbnail".to_string()), "{video:?}");
        let audio = MediaDownloadOptions { preset: MediaQualityPreset::AudioMp3, ..Default::default() };
        assert!(holds(&args_of(&audio, RunKind::Download, true), &["--embed-metadata", "--embed-chapters", "--embed-thumbnail"]));
        // ffmpeg writes them: none without it, none when not asked for, none but in a download.
        let off = MediaDownloadOptions { embed_metadata: false, ..Default::default() };
        for args in [
            args_of(&MediaDownloadOptions::default(), RunKind::Download, false),
            args_of(&off, RunKind::Download, true),
            args_of(&MediaDownloadOptions::default(), RunKind::Extract, true),
        ] {
            assert!(!args.iter().any(|a| a.starts_with("--embed")), "{args:?}");
        }
    }

    #[test]
    fn live_streams_are_asked_for_as_the_options_say() {
        let live = MediaDownloadOptions { live_from_start: true, wait_for_video: true, ..Default::default() };
        for kind in [RunKind::Extract, RunKind::Find, RunKind::Download] {
            assert!(args_of(&live, kind, true).contains(&"--live-from-start".to_string()), "{kind:?}");
        }
        for kind in [RunKind::Extract, RunKind::Download] {
            assert!(holds(&args_of(&live, kind, true), &["--wait-for-video", WAIT_FOR_VIDEO]), "{kind:?}");
        }
        // A page is asked about for a moment only; subtitles are written of a video found already.
        assert!(!args_of(&live, RunKind::Find, true).contains(&"--wait-for-video".to_string()));
        let subtitles = args_of(&MediaDownloadOptions { subtitles: Some("en".to_string()), ..live }, RunKind::Subtitles, true);
        assert!(!subtitles.iter().any(|a| a == "--wait-for-video" || a == "--live-from-start"), "{subtitles:?}");
        let plain = args_of(&MediaDownloadOptions::default(), RunKind::Download, true);
        assert!(!plain.contains(&"--live-from-start".to_string()) && !plain.contains(&"--wait-for-video".to_string()), "{plain:?}");
        // A download says whether it records, and ffmpeg's progress comes as lines.
        assert!(holds(&plain, &["--print", LIVE_TEMPLATE]) && holds(&plain, &FFMPEG_PROGRESS), "{plain:?}");
        assert!(!args_of(&MediaDownloadOptions::default(), RunKind::Extract, true).contains(&LIVE_TEMPLATE.to_string()));
    }

    #[test]
    fn a_live_recording_reports_what_it_wrote_and_no_total() {
        let out = std::env::temp_dir();
        let (output, video) = (out.join("clip.mp4"), out.join("clip.f299.mp4"));
        let mut state = OutputState::default();
        assert!(state.handle_line(&format!("HFLIVE True {}", output.display()), false).is_none());
        assert!(state.live);
        assert_eq!(state.recording, std::slice::from_ref(&output));
        // ffmpeg's own lines as `-progress` writes them: its rate, then what it has written.
        assert!(state.handle_line("bitrate= 800.0kbits/s", false).is_none());
        let update = state.handle_line("total_size=5000", false).expect("progress");
        assert_eq!((update.downloaded, update.total, update.speed, update.eta_seconds), (5000, 0, 100_000.0, None));
        assert!(state.handle_line("bitrate=N/A", false).is_none());
        assert_eq!(state.handle_line("total_size=N/A", false).map(|u| u.downloaded), None);
        // yt-dlp's last line for the file, once ffmpeg is done, counts the same bytes again.
        let line = format!("HFP finished 6000 6000 NA 1000.0 NA {}", output.display());
        assert_eq!(state.handle_line(&line, false).map(|u| (u.downloaded, u.total)), Some((6000, 0)));
        // yt-dlp's own downloads of a recording from its start: every stream is one of its files.
        let line = format!("HFP downloading 7000 NA 90000 2000.0 40 {}", video.display());
        let update = state.handle_line(&line, false).expect("progress");
        assert_eq!((update.total, update.eta_seconds), (0, None));
        assert_eq!(state.recording, [output.clone(), video.clone()]);
        // The next video is not live: its progress has an end again.
        assert!(state.handle_line(&format!("HFLIVE False {}", out.join("next.mp4").display()), false).is_none());
        assert!(!state.live && state.recording.is_empty());
        let line = format!("HFP downloading 10 100 NA 5.0 18 {}", out.join("next.mp4").display());
        let update = state.handle_line(&line, false).expect("progress");
        assert_eq!(update.eta_seconds, Some(18));
        assert!(update.total > 0 && state.recording.is_empty());
    }

    /// ffmpeg downloading a video's formats one by one (an HLS stream yt-dlp cannot decrypt
    /// itself): each is counted once, by yt-dlp's line for its file.
    #[test]
    fn ffmpegs_progress_of_a_video_that_is_not_live_is_not_counted_twice() {
        let out = std::env::temp_dir();
        let mut state = OutputState::default();
        state.handle_line(&format!("HFLIVE False {}", out.join("clip.mp4").display()), false);
        state.handle_line("HFTOTAL 3000", false);
        for (format, size) in [("f1", 2000), ("f2", 1000)] {
            assert!(state.handle_line("bitrate= 800.0kbits/s", false).is_none());
            assert!(state.handle_line(&format!("total_size={size}"), false).is_none());
            let line = format!("HFP finished {size} {size} NA NA NA {}", out.join(format!("clip.{format}.mp4")).display());
            state.handle_line(&line, false).expect("progress");
        }
        let update = state.handle_line("HFPOST x", false).expect("progress");
        assert_eq!((update.downloaded, update.total), (3000, 3000));
    }

    /// `yt-dlp -J` (trimmed) for a YouTube video with chapters, as yt-dlp 2026.08.19 answered.
    fn chaptered_info() -> Value {
        serde_json::json!({
            "id": "wSSmNUl9Snw", "title": "The Computer Hack That Saved Apollo 14",
            "uploader": "Scott Manley", "uploader_id": "@scottmanley", "creators": null,
            "upload_date": "20170831", "webpage_url": "https://www.youtube.com/watch?v=wSSmNUl9Snw",
            "duration": 682, "extractor_key": "Youtube", "_type": "video",
            "description": "Apollo 14 almost never made it to the lunar surface thanks to a hardware failure which caused a short circuit in the abort switch.",
            "categories": ["Science & Technology"], "tags": ["apollo 14", "computer hack", "troubleshooting"],
            "chapters": [
                {"start_time": 0, "title": "The Apollo 14 crisis", "end_time": 36},
                {"start_time": 36, "title": "Mission control reacts", "end_time": 105},
                {"start_time": 105, "title": "Understanding the computer", "end_time": 229},
                {"start_time": 229, "title": "The first software hack", "end_time": 305},
                {"start_time": 305, "title": "Risks of the initial plan", "end_time": 362},
                {"start_time": 362, "title": "A new procedural approach", "end_time": 451},
                {"start_time": 451, "title": "Executing the new commands", "end_time": 525},
                {"start_time": 525, "title": "Restoring computer control", "end_time": 637},
                {"start_time": 637, "title": "Conclusion and legacy", "end_time": 682},
            ],
        })
    }

    #[test]
    fn a_videos_tags_are_the_ones_yt_dlp_embeds() {
        let tags = tags_of(&chaptered_info());
        let description = chaptered_info()["description"].as_str().unwrap().to_string();
        let url = "https://www.youtube.com/watch?v=wSSmNUl9Snw".to_string();
        assert_eq!(
            tags.metadata,
            [
                ("title", "The Computer Hack That Saved Apollo 14".to_string()),
                ("date", "20170831".to_string()),
                ("description", description.clone()),
                ("synopsis", description),
                ("purl", url.clone()),
                ("comment", url),
                ("artist", "Scott Manley".to_string()),
                ("genre", "Science & Technology".to_string()),
            ]
        );
        let chapters = tags.chapters.expect("chapters");
        assert!(chapters.starts_with(";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=36000\ntitle=The Apollo 14 crisis\n"), "{chapters}");
        assert!(chapters.ends_with("START=637000\nEND=682000\ntitle=Conclusion and legacy\n"), "{chapters}");
        assert_eq!(chapters.matches("[CHAPTER]").count(), 9);

        // A track's own title, number and artists, a show's episode; ffmetadata's special
        // characters escaped; chapters ffmpeg would refuse left out, an untitled one kept.
        let track = serde_json::json!({
            "title": "Video", "track": "Song\u{0}", "track_number": 7, "artists": ["A", "B"], "uploader": "Channel",
            "genres": ["Rock", "Pop"], "series": "Show", "episode_number": 3, "description": "",
            "chapters": [
                {"start_time": 0, "end_time": 10, "title": "One = two; #3 \\ back\nslash"},
                {"start_time": 12, "end_time": 11, "title": "Backwards"},
                {"start_time": 20},
                {"start_time": 20, "end_time": 30.5},
            ],
        });
        let tags = tags_of(&track);
        let expected = [
            ("title", "Song"),
            ("track", "7"),
            ("artist", "A, B"),
            ("genre", "Rock, Pop"),
            ("album", "Show"),
            ("show", "Show"),
            ("episode_sort", "3"),
        ];
        assert_eq!(tags.metadata, expected.map(|(name, value)| (name, value.to_string())));
        assert_eq!(
            tags.chapters.as_deref(),
            Some(
                ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=10000\ntitle=One \\= two\\; \\#3 \\\\ back\\\nslash\n\
                 [CHAPTER]\nTIMEBASE=1/1000\nSTART=20000\nEND=30500\n"
            )
        );
        assert!(tags_of(&serde_json::json!({"id": "x"})).is_empty());
    }

    /// What ffmpeg reads of `file`: its streams, tags and chapters.
    fn probe(ffmpeg: &Path, file: &Path) -> String {
        let out = std::process::Command::new(ffmpeg).args(["-hide_banner", "-i"]).arg(file).output().unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    #[tokio::test]
    async fn the_engines_file_gets_the_tags_and_chapters() {
        let Some(ffmpeg) = find_ffmpeg_path() else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (src, out) = (dir.path().join("src"), dir.path().join("out"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let video = src.join("v.mp4");
        lavfi(&ffmpeg, "testsrc=duration=2:size=64x48:rate=5", &[], &video);
        lavfi(&ffmpeg, "sine=duration=2", &[], &src.join("a.m4a"));
        let mut info = chaptered_info();
        info["duration"] = 2.into();
        info["chapters"] = serde_json::json!([{"start_time": 0, "end_time": 1, "title": "One = one"}, {"start_time": 1, "end_time": 2, "title": "Two"}]);
        let tags = tags_of(&info);

        // Joined from its streams.
        let mut plan = plan_fast(&mp4_merge_info(&out), true).unwrap();
        let files = [video.clone(), src.join("a.m4a")];
        let path = joined(&plan, &files, Some(&ffmpeg), &tags, &None).await.unwrap();
        let read = probe(&ffmpeg, &path);
        assert!(read.contains("The Computer Hack That Saved Apollo 14") && read.contains("Scott Manley"), "{read}");
        assert!(read.contains("Chapter #0:1: start 1.000000, end 2.000000") && read.contains("One = one"), "{read}");
        assert_eq!(media_streams(&path), (true, true));
        assert_eq!(names_in(&out), ["clip.mp4"], "the chapters file stays behind");

        // A file that is the output as it is: copied with them into its place.
        std::fs::remove_file(&path).unwrap();
        plan.streams.truncate(1);
        plan.finish = Finish::Rename;
        let path = joined(&plan, &files[..1], Some(&ffmpeg), &tags, &None).await.unwrap();
        assert!(probe(&ffmpeg, &path).contains("Scott Manley"));
        assert_eq!(names_in(&out), ["clip.mp4"]);
        // ffmpeg failing, it is kept as it is.
        std::fs::remove_file(&path).unwrap();
        let bytes = std::fs::read(&video).unwrap();
        let path = joined(&plan, &files[..1], Some(&dir.path().join("no-ffmpeg-here")), &tags, &None).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(names_in(&out), ["clip.mp4"]);
    }

    /// yt-dlp's `-J` for a video of one plain file the engine downloads itself, with nothing to join.
    fn one_file_info(out: &Path) -> Value {
        let mut info = merge_info(out, Value::Null, Value::Null);
        info["requested_formats"] = Value::Null;
        for (key, value) in format_info("18", "mp4", "avc1.42001E", "mp4a.40.2", 5).as_object().unwrap() {
            info[key] = value.clone();
        }
        info
    }

    #[tokio::test]
    async fn yt_dlp_writes_the_subtitles_of_the_video_the_engine_downloaded() {
        let dir = tempfile::tempdir().unwrap();
        let cookies = BrowserCookies::new(Some(dir.path().join("private")));
        let fetch: &StreamFetcher<'_> = &|stream, _tx, _stop| {
            Box::pin(async move {
                std::fs::write(&stream.path, b"video").unwrap();
                Ok(stream.path)
            })
        };
        let out = dir.path().join("out");
        let options = MediaDownloadOptions { subtitles: Some("en".to_string()), ..Default::default() };
        let path = fake_download_with(dir.path(), &one_file_info(&out), &cookies, fetch, options).await.unwrap();
        assert_eq!(path, out.join("clip.mp4"));
        // From what was found: nothing is extracted again.
        assert_eq!(runs_in(dir.path()), ["extract", "subtitles info"]);
        assert_eq!(names_in(&out), ["clip.en.srt", "clip.mp4"]);
        assert_eq!(std::fs::read(&path).unwrap(), b"video");

        // Asked for none, yt-dlp is not run for them.
        let again = tempfile::tempdir().unwrap();
        let out = again.path().join("out");
        fake_download(again.path(), &one_file_info(&out), &cookies, fetch).await.unwrap();
        assert_eq!(runs_in(again.path()), ["extract"]);
    }

    /// Makes `file` an MPEG-TS recording, as ffmpeg records a live HLS stream, a real one when
    /// `ffmpeg` is at hand.
    fn ts_recording(ffmpeg: Option<&Path>, file: &Path) {
        match ffmpeg {
            Some(ffmpeg) => {
                lavfi(ffmpeg, "testsrc=duration=1:size=64x48:rate=5", &["-f", "mpegts"], file);
            }
            None => {
                let mut ts = vec![0u8; 376];
                (ts[0], ts[188]) = (0x47, 0x47);
                std::fs::write(file, ts).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn stopping_a_live_recording_keeps_what_it_recorded() {
        let ffmpeg = find_ffmpeg_path();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("clip.mp4");
        ts_recording(ffmpeg.as_deref(), &with_part(&output));
        // Records until it is stopped, reporting as yt-dlp and its ffmpeg do.
        let out_s = output.display();
        #[cfg(windows)]
        let (bin, script) = (
            dir.path().join("yt-dlp.cmd"),
            format!("@echo off\r\necho HFLIVE True {out_s}\r\necho bitrate= 800.0kbits/s\r\necho total_size=1000\r\nping -n 30 127.0.0.1 >nul\r\n"),
        );
        #[cfg(not(windows))]
        let (bin, script) = (
            dir.path().join("yt-dlp"),
            format!("#!/bin/sh\necho 'HFLIVE True {out_s}'\necho 'bitrate= 800.0kbits/s'\necho 'total_size=1000'\nsleep 30\n"),
        );
        std::fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        make_executable(&bin).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = async {
            let first = rx.recv().await;
            cancel.store(true, Ordering::Relaxed);
            first
        };
        let started = std::time::Instant::now();
        let (result, first) = tokio::join!(run_ytdlp(tree_command(&bin), Some(&tx), Some(Arc::clone(&cancel)), ffmpeg.as_deref()), stop);
        assert!(started.elapsed() < Duration::from_secs(25), "{:?}", started.elapsed());
        let first = first.expect("progress");
        assert_eq!((first.downloaded, first.total), (1000, 0));
        // Named for what it is, then remuxed to MP4 where ffmpeg is at hand.
        let kept = if ffmpeg.is_some() { dir.path().join("clip.mp4") } else { dir.path().join("clip.ts") };
        assert_eq!(result, Ok(kept.clone()));
        if ffmpeg.is_some() {
            assert_eq!(media_streams(&kept), (true, false));
        }
        let name = kept.file_name().unwrap().to_string_lossy().into_owned();
        let bin_name = bin.file_name().unwrap().to_string_lossy().into_owned();
        let mut left = names_in(dir.path());
        left.retain(|n| *n != bin_name);
        assert_eq!(left, [name]);
    }

    /// A recording that the ffmpeg killed with yt-dlp still holds is moved once Windows lets go of
    /// it, and kept where it is if it never does: that file is the recording, not a download to
    /// resume, which would overwrite it.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_recording_a_killed_ffmpeg_still_holds_is_kept() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("clip.mp4.part");
        ts_recording(None, &part);
        // As ffmpeg holds its output: others may read and write it, not move it.
        let hold = |time| {
            let file = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ_WRITE).open(&part).unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(time);
                drop(file);
            })
        };
        let held = hold(Duration::from_millis(300));
        assert_eq!(keep_recording(&part, None).await, dir.path().join("clip.ts"));
        held.join().unwrap();

        std::fs::rename(dir.path().join("clip.ts"), &part).unwrap();
        let held = hold(Duration::from_secs(3));
        assert_eq!(keep_recording(&part, None).await, part);
        held.join().unwrap();
        assert_eq!(names_in(dir.path()), ["clip.mp4.part"]);
    }

    /// A stop while yt-dlp finishes a live stream that ended of itself waits for the file: no
    /// Ctrl+C cuts its remux short, nor is it killed.
    #[tokio::test]
    async fn a_recording_that_has_ended_is_left_to_finish_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("clip.mp4");
        let out_s = output.display();
        #[cfg(windows)]
        let (bin, script) = (
            dir.path().join("yt-dlp.cmd"),
            format!(
                "@echo off\r\necho HFLIVE True {out_s}\r\necho total_size=1000\r\necho HFPOST x\r\n\
                 ping -n 3 127.0.0.1 >nul\r\necho finished> \"{out_s}\"\r\necho HFPATH {out_s}\r\n"
            ),
        );
        #[cfg(not(windows))]
        let (bin, script) = (
            dir.path().join("yt-dlp"),
            format!("#!/bin/sh\necho 'HFLIVE True {out_s}'\necho total_size=1000\necho HFPOST x\nsleep 2\necho finished > '{out_s}'\necho 'HFPATH {out_s}'\n"),
        );
        std::fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        make_executable(&bin).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = async {
            // Its size, then the bar pinned while it finishes.
            for _ in 0..2 {
                rx.recv().await.expect("progress");
            }
            cancel.store(true, Ordering::Relaxed);
        };
        let (result, ()) = tokio::join!(run_ytdlp(tree_command(&bin), Some(&tx), Some(Arc::clone(&cancel)), None), stop);
        assert_eq!(result, Ok(output.clone()));
        assert!(std::fs::read_to_string(&output).unwrap().starts_with("finished"));
    }

    #[tokio::test]
    async fn a_stopped_recording_is_its_streams_joined_or_its_file_named() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path();
        let live = |lines: &[String]| {
            let mut state = OutputState::default();
            for line in lines {
                state.handle_line(line, false);
            }
            state
        };
        // Nothing to keep: the download is cancelled.
        let not_live = live(&[format!("HFLIVE False {}", out.join("clip.mp4").display())]);
        assert_eq!(stopped(&not_live, None).await, Err(CANCELLED.to_string()));
        let started = live(&[format!("HFLIVE True {}", out.join("clip.mp4").display())]);
        assert_eq!(stopped(&started, None).await, Err(CANCELLED.to_string()));
        // yt-dlp finished it once asked to stop, under the name of its last step (an audio
        // preset's), whether a step after that failed or not.
        std::fs::write(out.join("clip.mp3"), b"done").unwrap();
        let ended = live(&[
            format!("HFLIVE True {}", out.join("clip.m4a").display()),
            "HFPOST x".to_string(),
            format!("HFPATH {}", out.join("clip.mp3").display()),
        ]);
        assert_eq!(stopped(&ended, None).await, Ok(out.join("clip.mp3")));
        // What an earlier video of the run became is not the recording.
        let after = live(&[
            format!("HFPATH {}", out.join("clip.mp3").display()),
            format!("HFLIVE True {}", out.join("clip.mp4").display()),
        ]);
        assert_eq!(stopped(&after, None).await, Err(CANCELLED.to_string()));
        std::fs::remove_file(out.join("clip.mp3")).unwrap();
        // A file of another format just loses its `.part`, under its own name.
        std::fs::write(out.join("talk.webm.part"), b"webm").unwrap();
        let webm = live(&[format!("HFLIVE True {}", out.join("talk.webm").display())]);
        assert_eq!(stopped(&webm, None).await, Ok(out.join("talk.webm")));
        assert_eq!(names_in(out), ["talk.webm"]);
        std::fs::remove_file(out.join("talk.webm")).unwrap();

        // Recorded from its start: its streams, downloaded apart, are joined into one file.
        let Some(ffmpeg) = find_ffmpeg_path() else {
            eprintln!("skipped joining: ffmpeg not found");
            return;
        };
        let (video, audio) = (out.join("clip.f299.mp4"), out.join("clip.f140.m4a"));
        lavfi(&ffmpeg, "testsrc=duration=1:size=64x48:rate=5", &["-f", "mp4"], &with_part(&video));
        lavfi(&ffmpeg, "sine=duration=1", &["-f", "mp4"], &with_part(&audio));
        let from_start = live(&[
            format!("HFLIVE True {}", out.join("clip.mp4").display()),
            format!("HFP downloading 100 NA NA 10.0 NA {}", video.display()),
            format!("HFP downloading 100 NA NA 10.0 NA {}", audio.display()),
        ]);
        let path = stopped(&from_start, Some(&ffmpeg)).await.unwrap();
        assert_eq!(path, out.join("clip.mp4"));
        assert_eq!(media_streams(&path), (true, true));
        assert_eq!(names_in(out), ["clip.mp4"]);
    }

    /// A stand-in for yt-dlp recording a live stream into `output` until it gets a SIGINT, which
    /// runs `on_stop` (sh, no single quotes; empty ignores it).
    #[cfg(unix)]
    fn recording_ytdlp(dir: &Path, output: &Path, on_stop: &str) -> PathBuf {
        let bin = dir.join("yt-dlp");
        let script = format!(
            "#!/bin/sh\ntrap '{on_stop}' INT\necho 'HFLIVE True {}'\necho 'total_size=1000'\nwhile :; do sleep 1; done\n",
            output.display()
        );
        std::fs::write(&bin, script).unwrap();
        make_executable(&bin).unwrap();
        bin
    }

    /// Runs yt-dlp as `cmd` and stops it once it reports progress; returns what the run gave and
    /// how long it took.
    async fn stop_recording(cmd: Command) -> (Result<PathBuf, String>, Duration) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = async {
            rx.recv().await.expect("progress");
            cancel.store(true, Ordering::Relaxed);
        };
        let started = std::time::Instant::now();
        let (result, ()) = tokio::join!(run_ytdlp(cmd, Some(&tx), Some(Arc::clone(&cancel)), None), stop);
        (result, started.elapsed())
    }

    /// Asked to stop, yt-dlp finishes the recording itself, taking as long as that takes once it
    /// has stopped recording; one that goes on recording is killed and its file kept.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stopped_recording_is_finished_by_yt_dlp_unless_it_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("clip.mp4");
        let (out_s, part_s) = (output.display(), with_part(&output).display().to_string());
        std::fs::write(with_part(&output), b"recording").unwrap();
        let finish = format!("echo HFPOST x; sleep 5; echo finished > \"{out_s}\"; rm -f \"{part_s}\"; echo \"HFPATH {out_s}\"; exit 0");
        let (result, took) = stop_recording(tree_command(&recording_ytdlp(dir.path(), &output, &finish))).await;
        assert_eq!(result, Ok(output.clone()));
        assert_eq!(std::fs::read(&output).unwrap(), b"finished\n");
        assert!(took >= Duration::from_secs(5), "killed while finishing, after {took:?}");

        std::fs::remove_file(&output).unwrap();
        std::fs::write(with_part(&output), b"recording").unwrap();
        let (result, took) = stop_recording(tree_command(&recording_ytdlp(dir.path(), &output, ""))).await;
        assert_eq!(result, Ok(output.clone()));
        assert_eq!(std::fs::read(&output).unwrap(), b"recording");
        assert!(took >= LIVE_STOP_GRACE && took < LIVE_STOP_GRACE * 3, "{took:?}");
    }

    /// Where the yt-dlp this test binary stands in for records (see [`records_as_yt_dlp_does`]).
    #[cfg(windows)]
    const FAKE_RECORDING: &str = "HYPERFETCH_TEST_RECORDING";
    /// Makes that yt-dlp go on recording when asked to stop.
    #[cfg(windows)]
    const FAKE_GOES_ON: &str = "HYPERFETCH_TEST_GOES_ON";
    /// Keeps a process from having the Ctrl+C helper send its Ctrl+C (see [`ctrl_c_helper`]).
    #[cfg(windows)]
    const NO_CTRL_C_HELPER: &str = "HYPERFETCH_TEST_NO_CTRL_C_HELPER";
    /// Runs [`a_stopped_recording_is_finished_by_yt_dlp_on_windows`] as the GUI does.
    #[cfg(windows)]
    const AS_THE_GUI: &str = "HYPERFETCH_TEST_AS_THE_GUI";

    /// This test binary, running only the test `name`.
    #[cfg(windows)]
    fn this_test(name: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", name, "--nocapture"]);
        cmd
    }

    /// The Ctrl+C helper of this test binary (see `super::ctrl_c_helper`): the test below.
    #[cfg(windows)]
    pub(super) fn ctrl_c_helper() -> Option<std::process::Command> {
        std::env::var_os(NO_CTRL_C_HELPER).is_none().then(|| this_test("media::tests::sends_a_ctrl_c_as_the_helper"))
    }

    /// Started as the Ctrl+C helper, sends it (see [`serve_ctrl_c`]); else does nothing.
    #[cfg(windows)]
    #[test]
    fn sends_a_ctrl_c_as_the_helper() {
        send_ctrl_c_if_asked();
    }

    /// Started as yt-dlp (see [`FAKE_RECORDING`]), records a live stream until a Ctrl+C, then
    /// finishes the file as yt-dlp does, which takes it 5 s; else does nothing. It takes the Ctrl+C
    /// as Python does: only when this process does not ignore it.
    #[cfg(windows)]
    #[test]
    fn records_as_yt_dlp_does() {
        use std::io::Write;
        use windows_sys::Win32::Foundation::BOOL;
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        static STOPPED: AtomicBool = AtomicBool::new(false);
        unsafe extern "system" fn on_ctrl_c(_event: u32) -> BOOL {
            STOPPED.store(true, Ordering::SeqCst);
            1
        }
        let Some(output) = std::env::var_os(FAKE_RECORDING).map(PathBuf::from) else { return };
        // SAFETY: the handler is a function that lives as long as the process.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl_c), 1) };
        let mut out = std::io::stdout();
        writeln!(out, "HFLIVE True {}\ntotal_size=1000", output.display()).unwrap();
        out.flush().unwrap();
        while !STOPPED.load(Ordering::SeqCst) || std::env::var_os(FAKE_GOES_ON).is_some() {
            std::thread::sleep(Duration::from_millis(50));
        }
        writeln!(out, "HFPOST x").unwrap();
        out.flush().unwrap();
        std::thread::sleep(Duration::from_secs(5));
        std::fs::write(&output, b"finished").unwrap();
        let _ = std::fs::remove_file(with_part(&output));
        writeln!(out, "HFPATH {}", output.display()).unwrap();
        out.flush().unwrap();
        std::process::exit(0);
    }

    /// yt-dlp recording into `output` (see [`records_as_yt_dlp_does`]).
    #[cfg(windows)]
    fn recording_ytdlp(output: &Path, goes_on: bool) -> Command {
        let mut cmd = tree_command(&std::env::current_exe().unwrap());
        cmd.args(["--exact", "media::tests::records_as_yt_dlp_does", "--nocapture"]).env(FAKE_RECORDING, output);
        if goes_on {
            cmd.env(FAKE_GOES_ON, "1");
        }
        cmd
    }

    /// Asked to stop, yt-dlp finishes the recording itself; one that goes on recording is killed
    /// once [`LIVE_STOP_GRACE`] is over, and its file kept.
    #[cfg(windows)]
    async fn stop_and_finish_recording(dir: &Path, goes_on: bool) {
        let output = dir.join(if goes_on { "going on.mp4" } else { "clip.mp4" });
        std::fs::write(with_part(&output), b"recording").unwrap();
        let (result, took) = stop_recording(recording_ytdlp(&output, goes_on)).await;
        assert_eq!(result, Ok(output.clone()));
        if goes_on {
            assert_eq!(std::fs::read(&output).unwrap(), b"recording");
            assert!(took >= LIVE_STOP_GRACE && took < LIVE_STOP_GRACE * 4, "{took:?}");
        } else {
            assert_eq!(std::fs::read(&output).unwrap(), b"finished");
            assert!(took >= Duration::from_secs(5) && took < LIVE_STOP_GRACE * 4, "{took:?}");
        }
    }

    /// As the CLI stops a recording: from a process with a console of its own, which has the
    /// helper send the Ctrl+C; and as the GUI does, from one without, which sends it itself (its
    /// own copy of it swallowed, its standard handles put back), and whose yt-dlp takes it even
    /// though the GUI was started with Ctrl+C ignored.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_stopped_recording_is_finished_by_yt_dlp_on_windows() {
        use windows_sys::Win32::System::Console::{GetConsoleCP, GetStdHandle, SetConsoleCtrlHandler, STD_OUTPUT_HANDLE};
        let dir = tempfile::tempdir().unwrap();
        if std::env::var_os(AS_THE_GUI).is_some() {
            // SAFETY: plain Win32 calls.
            assert_eq!(unsafe { GetConsoleCP() }, 0, "the GUI has no console");
            let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
            stop_and_finish_recording(dir.path(), false).await;
            assert_eq!(unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }, stdout);
            return;
        }
        // yt-dlp takes a Ctrl+C this test process may have been started ignoring, as the CLI's
        // yt-dlp does in a terminal.
        // SAFETY: plain Win32 call.
        unsafe { SetConsoleCtrlHandler(None, 0) };
        stop_and_finish_recording(dir.path(), false).await;
        stop_and_finish_recording(dir.path(), true).await;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let mut gui = tokio::process::Command::from(this_test("media::tests::a_stopped_recording_is_finished_by_yt_dlp_on_windows"));
        gui.env(AS_THE_GUI, "1").env(NO_CTRL_C_HELPER, "1").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // Without a console, and with Ctrl+C ignored.
        let status = gui.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).status().await.unwrap();
        assert!(status.success(), "{status}");
    }
}
