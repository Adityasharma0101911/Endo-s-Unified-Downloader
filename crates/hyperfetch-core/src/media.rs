use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::Sender;
use url::Url;

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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

impl MediaQualityPreset {
    /// Convert preset to yt-dlp arguments
    pub fn to_args(&self) -> Vec<String> {
        match self {
            Self::BestVideoAudio => vec![
                "-f".to_string(),
                "bv*+ba/b".to_string(),
                "--merge-output-format".to_string(),
                "mp4".to_string(),
            ],
            Self::Fhd1080p => vec![
                "-f".to_string(),
                "bv*[height<=1080]+ba/b[height<=1080]/best".to_string(),
                "--merge-output-format".to_string(),
                "mp4".to_string(),
            ],
            Self::Hd720p => vec![
                "-f".to_string(),
                "bv*[height<=720]+ba/b[height<=720]/best".to_string(),
                "--merge-output-format".to_string(),
                "mp4".to_string(),
            ],
            Self::AudioMp3 => vec![
                "-x".to_string(),
                "--audio-format".to_string(),
                "mp3".to_string(),
            ],
            Self::AudioM4a => vec![
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
}

/// Browser cookie sources for bypassing age gates and bot verification
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
        match self {
            Self::None => vec![],
            Self::Chrome => vec!["--cookies-from-browser".to_string(), "chrome".to_string()],
            Self::Edge => vec!["--cookies-from-browser".to_string(), "edge".to_string()],
            Self::Firefox => vec!["--cookies-from-browser".to_string(), "firefox".to_string()],
            Self::Brave => vec!["--cookies-from-browser".to_string(), "brave".to_string()],
            Self::Opera => vec!["--cookies-from-browser".to_string(), "opera".to_string()],
            Self::Vivaldi => vec!["--cookies-from-browser".to_string(), "vivaldi".to_string()],
            Self::File(p) => vec!["--cookies".to_string(), p.to_string_lossy().to_string()],
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

/// Base URL of the newest stable yt-dlp release; GitHub redirects it to `/releases/tag/<version>`.
const RELEASE_BASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest";

/// First yt-dlp release that understands `--js-runtimes`; older builds abort on the unknown flag.
const JS_RUNTIMES_MIN_VERSION: (u32, u32, u32) = (2025, 11, 12);

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

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Per-user directory the managed yt-dlp is installed into (always writable, unlike the
/// application directory under Program Files or /usr/local/bin).
fn managed_bin_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let data_dir = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share")));
    Some(data_dir?.join("EndosUnifiedDownloader").join("bin"))
}

fn managed_ytdlp_path() -> Option<PathBuf> {
    Some(managed_bin_dir()?.join(exe_name("yt-dlp")))
}

/// Discover the path to yt-dlp executable. The result is cached for the process lifetime;
/// call it from a blocking context (it stats every PATH entry the first time).
pub fn find_ytdlp_path() -> Option<PathBuf> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE.get_or_init(discover_ytdlp).clone()
}

fn discover_ytdlp() -> Option<PathBuf> {
    let name = exe_name("yt-dlp");
    if let Some(candidate) = next_to_current_exe(&name) {
        return Some(candidate);
    }
    if let Some(candidate) = managed_ytdlp_path().filter(|p| p.is_file()) {
        return Some(candidate);
    }

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
        for dir in std::env::split_paths(&path_var) {
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

/// Standalone yt-dlp build for this platform, as named in the GitHub release.
fn ytdlp_release_asset() -> &'static str {
    if cfg!(all(windows, target_arch = "aarch64")) {
        "yt-dlp_arm64.exe"
    } else if cfg!(all(windows, target_arch = "x86")) {
        "yt-dlp_x86.exe"
    } else if cfg!(windows) {
        "yt-dlp.exe"
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

/// Download the latest official yt-dlp standalone build into the per-user managed directory,
/// verified against the release's SHA2-256SUMS and moved into place atomically, so a failed
/// or interrupted download never leaves a truncated executable behind.
pub async fn download_ytdlp_binary(client: &reqwest::Client) -> Result<PathBuf, String> {
    let target = managed_ytdlp_path().ok_or("Cannot determine a per-user directory to install yt-dlp into")?;
    let asset = ytdlp_release_asset();

    let sums_url = format!("{RELEASE_BASE}/download/SHA2-256SUMS");
    let sums = http_get(client, &sums_url, Duration::from_secs(30))
        .await?
        .text()
        .await
        .map_err(|e| format!("Failed to read {sums_url}: {e}"))?;
    let expected = expected_sha256(&sums, asset)
        .ok_or_else(|| format!("yt-dlp SHA2-256SUMS has no entry for {asset}"))?
        .to_ascii_lowercase();

    let asset_url = format!("{RELEASE_BASE}/download/{asset}");
    let bytes = http_get(client, &asset_url, Duration::from_secs(600))
        .await?
        .bytes()
        .await
        .map_err(|e| format!("Failed to read {asset_url}: {e}"))?;

    tokio::task::spawn_blocking(move || install_verified(&target, &bytes, &expected))
        .await
        .map_err(|e| format!("yt-dlp install task failed: {e}"))?
}

/// Check `bytes` against `expected_sha256`, then write them next to `target` and rename into place.
fn install_verified(target: &Path, bytes: &[u8], expected_sha256: &str) -> Result<PathBuf, String> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected_sha256 {
        return Err(format!(
            "Downloaded yt-dlp failed checksum verification (expected {expected_sha256}, got {actual})"
        ));
    }

    let file_name = target.file_name().ok_or("Invalid yt-dlp install path")?.to_string_lossy();
    let tmp = target.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let installed = target
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&tmp, bytes))
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

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
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
    if let Some(target) = managed_ytdlp_path() {
        if tokio::fs::metadata(&target).await.is_ok_and(|m| m.is_file()) {
            return Ok(target);
        }
    }
    download_ytdlp_binary(&http_client(proxy)?).await
}

/// Replace the managed yt-dlp with the latest release if `current` is older.
/// Returns whether a new binary was installed.
async fn update_managed_ytdlp(proxy: Option<&str>, current: Option<&str>) -> Result<bool, String> {
    let client = http_client(proxy)?;
    let resp = client
        .head(RELEASE_BASE)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("Failed to check for a yt-dlp update: {e}"))?;
    let latest = resp
        .url()
        .path()
        .rsplit_once("/releases/tag/")
        .map(|(_, tag)| tag.to_string())
        .ok_or_else(|| format!("Unexpected yt-dlp release URL {}", resp.url()))?;
    if current == Some(latest.as_str()) {
        return Ok(false);
    }

    let _guard = INSTALL_LOCK.lock().await;
    download_ytdlp_binary(&client).await?;
    *VERSION_CACHE.lock() = None;
    tracing::info!("Updated managed yt-dlp from {} to {latest}", current.unwrap_or("an unknown version"));
    Ok(true)
}

/// Last successfully probed `yt-dlp --version`, keyed by binary path.
static VERSION_CACHE: parking_lot::Mutex<Option<(PathBuf, String)>> = parking_lot::const_mutex(None);

async fn ytdlp_version(bin: &Path) -> Option<String> {
    let cached = VERSION_CACHE.lock().clone();
    if let Some((path, version)) = cached {
        if path == bin {
            return Some(version);
        }
    }

    let mut cmd = Command::new(bin);
    cmd.arg("--version").stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    hide_console(&mut cmd);
    let output = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await.ok()?.ok()?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || version.is_empty() {
        return None;
    }
    *VERSION_CACHE.lock() = Some((bin.to_path_buf(), version.clone()));
    Some(version)
}

/// Compare a yt-dlp version ("2025.11.12", nightly "2025.11.12.232810") against `min`.
/// Unparseable versions count as too old.
fn version_at_least(version: &str, min: (u32, u32, u32)) -> bool {
    let mut parts = version.trim().split('.').map(|p| p.parse::<u32>());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(year)), Some(Ok(month)), Some(Ok(day))) => (year, month, day) >= min,
        _ => false,
    }
}

fn build_ytdlp_args(
    url: &Url,
    options: &MediaDownloadOptions,
    ffmpeg_dir: Option<&Path>,
    js_runtime: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "--newline",
        "--progress",
        "--no-colors",
        // --print implies --simulate for early stages; ours are all post-download, but be explicit.
        "--no-simulate",
        // Only affects URLs that name both a video and a playlist (watch?v=..&list=..).
        // A URL that is only a playlist is still downloaded as one.
        "--no-playlist",
        "--progress-template",
        PROGRESS_TEMPLATE,
        "--print",
        PLANNED_TEMPLATE,
        "--print",
        POSTPROCESS_TEMPLATE,
        "--print",
        PATH_TEMPLATE,
        "--buffer-size",
        "16M",
        "--http-chunk-size",
        "10M",
    ]
    .map(String::from)
    .to_vec();

    if let Some(runtime) = js_runtime {
        args.extend(["--js-runtimes".to_string(), runtime.to_string()]);
    }
    if let Some(dir) = ffmpeg_dir {
        args.extend(["--ffmpeg-location".to_string(), dir.to_string_lossy().to_string()]);
    }
    args.extend(options.preset.to_args());
    args.extend(options.cookies.to_args());
    if let Some(ref proxy) = options.proxy {
        args.extend(["--proxy".to_string(), proxy.clone()]);
    }
    if options.concurrent_fragments > 1 {
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
    // CREATE_NO_WINDOW so no blank console window pops up
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
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

/// Run yt-dlp once and return the file it produced.
async fn run_ytdlp(
    bin: &Path,
    args: &[String],
    progress_tx: Option<&Sender<ProgressUpdate>>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<PathBuf, String> {
    let mut cmd = tree_command(bin);
    // Otherwise Python encodes piped output in the locale code page (cp1252 on Windows).
    cmd.args(args).env("PYTHONIOENCODING", "utf-8");

    let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn yt-dlp ({}): {e}", bin.display()))?;
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

/// Download a media URL using yt-dlp with automated JS challenge solving,
/// quality presets, browser cookies, and real-time progress reporting.
pub async fn download_media(
    url: &Url,
    options: &MediaDownloadOptions,
    progress_tx: Option<Sender<ProgressUpdate>>,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> Result<PathBuf, String> {
    // First-time discovery stats every PATH entry (possibly on slow network drives).
    let (found_ytdlp, ffmpeg, js_runtime) =
        tokio::task::spawn_blocking(|| (find_ytdlp_path(), find_ffmpeg_path(), find_js_runtime()))
            .await
            .map_err(|e| format!("Tool discovery failed: {e}"))?;

    let ytdlp_bin = match (&options.custom_ytdlp_path, found_ytdlp) {
        (Some(path), _) => path.clone(),
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
    let managed = managed_ytdlp_path().is_some_and(|m| m == ytdlp_bin);

    let mut updated = false;
    loop {
        let version = ytdlp_version(&ytdlp_bin).await;
        let supports_js = version.as_deref().is_some_and(|v| version_at_least(v, JS_RUNTIMES_MIN_VERSION));
        let args = build_ytdlp_args(url, options, ffmpeg_dir, js_runtime.as_deref().filter(|_| supports_js));

        let err = match run_ytdlp(&ytdlp_bin, &args, progress_tx.as_ref(), cancel_flag.clone()).await {
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
            Ok(true) => updated = true,
            Ok(false) => return Err(err),
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

        let args = build_ytdlp_args(&url, &options, None, None);
        assert!(!args.contains(&"--js-runtimes".to_string()));
        assert!(args.contains(&"--no-playlist".to_string()));
        assert!(args.contains(&PATH_TEMPLATE.to_string()));
        assert!(args.contains(&PROGRESS_TEMPLATE.to_string()));
        assert_eq!(args.last(), Some(&url.to_string()));

        let args = build_ytdlp_args(&url, &options, None, Some("node:/usr/bin/node"));
        let at = args.iter().position(|a| a == "--js-runtimes").expect("flag present");
        assert_eq!(args[at + 1], "node:/usr/bin/node");
    }

    #[test]
    fn finds_asset_checksum() {
        let sums = "aaa  yt-dlp\nbbb  yt-dlp.exe\r\nccc *yt-dlp_linux\n";
        assert_eq!(expected_sha256(sums, "yt-dlp.exe"), Some("bbb"));
        assert_eq!(expected_sha256(sums, "yt-dlp_linux"), Some("ccc"));
        assert_eq!(expected_sha256(sums, "yt-dlp_macos"), None);
    }

    #[test]
    fn install_verified_rejects_bad_checksum_and_installs_good_one() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("bin").join("yt-dlp");
        let body = b"binary";
        let good = format!("{:x}", Sha256::digest(body));

        assert!(install_verified(&target, body, &"0".repeat(64)).is_err());
        assert!(!target.exists());

        assert_eq!(install_verified(&target, body, &good).unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), body);
        // No temp file left behind.
        assert_eq!(std::fs::read_dir(dir.path().join("bin")).unwrap().count(), 1);
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
}
