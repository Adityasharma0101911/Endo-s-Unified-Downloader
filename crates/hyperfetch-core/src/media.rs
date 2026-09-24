use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaQualityPreset {
    /// Best available video and audio merged into MP4
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

impl Default for MediaQualityPreset {
    fn default() -> Self {
        Self::BestVideoAudio
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserCookieSource {
    None,
    Chrome,
    Edge,
    Firefox,
    Brave,
    Opera,
    Vivaldi,
    File(PathBuf),
}

impl Default for BrowserCookieSource {
    fn default() -> Self {
        Self::None
    }
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

/// Check if a given URL is a known media site best handled by the Media Engine
pub fn is_supported_media_site(url: &Url) -> bool {
    let host = url.host_str().unwrap_or("").to_lowercase();
    host.ends_with("youtube.com")
        || host == "youtu.be"
        || host.ends_with("youtube-nocookie.com")
        || host.ends_with("twitch.tv")
        || host.ends_with("tiktok.com")
        || host.ends_with("twitter.com")
        || host == "x.com"
        || host.ends_with("vimeo.com")
        || host.ends_with("soundcloud.com")
        || host.ends_with("reddit.com")
        || host.ends_with("instagram.com")
        || host.ends_with("facebook.com")
        || host.ends_with("fb.watch")
        || host.ends_with("dailymotion.com")
        || host.ends_with("bilibili.com")
}

/// Discover the path to yt-dlp executable
pub fn find_ytdlp_path() -> Option<PathBuf> {
    // 1. Check next to current executable
    if let Ok(mut current_exe) = std::env::current_exe() {
        current_exe.pop();
        let candidate = if cfg!(windows) {
            current_exe.join("yt-dlp.exe")
        } else {
            current_exe.join("yt-dlp")
        };
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // 2. Check local app data bin directory on Windows
    #[cfg(windows)]
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let candidate = PathBuf::from(&local_app_data).join("EndosUnifiedDownloader").join("bin").join("yt-dlp.exe");
        if candidate.is_file() {
            return Some(candidate);
        }

        // Python Scripts directory
        let python_candidate = PathBuf::from(&local_app_data).join("Programs").join("Python");
        if python_candidate.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&python_candidate) {
                for entry in entries.flatten() {
                    let script_ytdlp = entry.path().join("Scripts").join("yt-dlp.exe");
                    if script_ytdlp.is_file() {
                        return Some(script_ytdlp);
                    }
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME") {
            let local_bin = PathBuf::from(&home).join(".local").join("bin").join("yt-dlp");
            if local_bin.is_file() {
                return Some(local_bin);
            }
        }
        for dir in ["/usr/local/bin", "/usr/bin", "/opt/homebrew/bin", "/usr/local/share/yt-dlp"] {
            let candidate = PathBuf::from(dir).join("yt-dlp");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 3. Check system PATH
    let binary_name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
    if let Ok(path_var) = std::env::var("PATH") {
        let separator = if cfg!(windows) { ';' } else { ':' };
        for dir in path_var.split(separator) {
            let candidate = PathBuf::from(dir).join(binary_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

/// Discover the path to a JavaScript runtime (Node.js, Deno, Bun) for solving YouTube challenges
pub fn find_js_runtime() -> Option<String> {
    // 1. Check standard Node.js install location on Windows
    #[cfg(windows)]
    {
        let standard_node = PathBuf::from(r"C:\Program Files\nodejs\node.exe");
        if standard_node.is_file() {
            return Some(format!("node:{}", standard_node.to_string_lossy()));
        }

        let standard_node_x86 = PathBuf::from(r"C:\Program Files (x86)\nodejs\node.exe");
        if standard_node_x86.is_file() {
            return Some(format!("node:{}", standard_node_x86.to_string_lossy()));
        }
    }

    // 2. Check PATH for node, deno, or bun
    if let Ok(path_var) = std::env::var("PATH") {
        let separator = if cfg!(windows) { ';' } else { ':' };
        let node_bin = if cfg!(windows) { "node.exe" } else { "node" };
        let deno_bin = if cfg!(windows) { "deno.exe" } else { "deno" };
        let bun_bin = if cfg!(windows) { "bun.exe" } else { "bun" };

        for dir in path_var.split(separator) {
            let path_dir = PathBuf::from(dir);
            let node_path = path_dir.join(node_bin);
            if node_path.is_file() {
                return Some(format!("node:{}", node_path.to_string_lossy()));
            }
            let deno_path = path_dir.join(deno_bin);
            if deno_path.is_file() {
                return Some(format!("deno:{}", deno_path.to_string_lossy()));
            }
            let bun_path = path_dir.join(bun_bin);
            if bun_path.is_file() {
                return Some(format!("bun:{}", bun_path.to_string_lossy()));
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME") {
            for sub in [".local/bin/node", ".bun/bin/bun", ".deno/bin/deno"] {
                let p = PathBuf::from(&home).join(sub);
                if p.is_file() {
                    let kind = if sub.contains("bun") { "bun" } else if sub.contains("deno") { "deno" } else { "node" };
                    return Some(format!("{}:{}", kind, p.to_string_lossy()));
                }
            }
        }
        for (kind, path) in [("node", "/usr/bin/node"), ("node", "/usr/local/bin/node"), ("deno", "/usr/local/bin/deno"), ("bun", "/usr/local/bin/bun")] {
            let p = PathBuf::from(path);
            if p.is_file() {
                return Some(format!("{}:{}", kind, p.to_string_lossy()));
            }
        }
    }

    None
}

/// Discover the path to ffmpeg executable
pub fn find_ffmpeg_path() -> Option<PathBuf> {
    let binary_name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };

    // 1. Next to current executable
    if let Ok(mut current_exe) = std::env::current_exe() {
        current_exe.pop();
        let candidate = current_exe.join(binary_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // 2. Check system PATH
    if let Ok(path_var) = std::env::var("PATH") {
        let separator = if cfg!(windows) { ';' } else { ':' };
        for dir in path_var.split(separator) {
            let candidate = PathBuf::from(dir).join(binary_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 3. Check Windows 11 Tools or standard directories
    #[cfg(windows)]
    {
        let tools_dir = PathBuf::from(r"C:\Windows 11 Tools\ffmpeg");
        if tools_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&tools_dir) {
                for entry in entries.flatten() {
                    let bin_candidate = entry.path().join("bin").join("ffmpeg.exe");
                    if bin_candidate.is_file() {
                        return Some(bin_candidate);
                    }
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME") {
            let local_bin = PathBuf::from(&home).join(".local").join("bin").join("ffmpeg");
            if local_bin.is_file() {
                return Some(local_bin);
            }
        }
        for dir in ["/usr/local/bin", "/usr/bin", "/opt/homebrew/bin"] {
            let candidate = PathBuf::from(dir).join("ffmpeg");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

/// Download the official yt-dlp standalone binary if not present on system
pub async fn download_ytdlp_binary(client: &reqwest::Client) -> Result<PathBuf, String> {
    #[cfg(windows)]
    let url = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
    #[cfg(not(windows))]
    let url = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp";

    let dest_dir = if let Ok(mut current_exe) = std::env::current_exe() {
        current_exe.pop();
        current_exe
    } else {
        PathBuf::from(".")
    };

    let target_file = if cfg!(windows) {
        dest_dir.join("yt-dlp.exe")
    } else {
        dest_dir.join("yt-dlp")
    };

    let resp = client.get(url).send().await.map_err(|e| format!("Failed to download yt-dlp: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("yt-dlp download failed with status {}", resp.status()));
    }

    let bytes = resp.bytes().await.map_err(|e| format!("Failed to read yt-dlp bytes: {}", e))?;
    std::fs::write(&target_file, &bytes).map_err(|e| format!("Failed to write yt-dlp binary: {}", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&target_file).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&target_file, perms).map_err(|e| e.to_string())?;
    }

    Ok(target_file)
}

/// Parse a line of yt-dlp progress output
/// Returns: (percentage, total_bytes, speed_bytes_per_sec, eta_seconds)
pub fn parse_ytdlp_progress_line(line: &str) -> Option<(f64, u64, u64, u64)> {
    let line = line.trim();
    if !line.starts_with("[download]") {
        return None;
    }

    let rest = line.strip_prefix("[download]")?.trim();
    // Example: "45.2% of 28.46MiB at 3.50MiB/s ETA 00:04"
    // Example: "100% of 28.46MiB in 00:00:08 at 3.50MiB/s"
    let pct_idx = rest.find('%')?;
    let pct_str = rest[..pct_idx].trim();
    let pct: f64 = pct_str.parse().ok()?;

    let after_pct = rest[pct_idx + 1..].trim();
    let after_of = after_pct.strip_prefix("of")?.trim();

    // Find the next space to get total size string
    let size_end = after_of.find(' ')?;
    let size_str = &after_of[..size_end];
    let total_bytes = parse_size_to_bytes(size_str)?;

    let mut speed_bps: u64 = 0;
    let mut eta_secs: u64 = 0;

    // Parse speed: "at 3.50MiB/s"
    if let Some(at_idx) = after_of.find("at ") {
        let after_at = after_of[at_idx + 3..].trim();
        let speed_end = after_at.find(' ').unwrap_or(after_at.len());
        let speed_str = &after_at[..speed_end];
        speed_bps = parse_speed_to_bps(speed_str);
    }

    // Parse ETA: "ETA 00:04"
    if let Some(eta_idx) = after_of.find("ETA ") {
        let after_eta = after_of[eta_idx + 4..].trim();
        let eta_end = after_eta.find(' ').unwrap_or(after_eta.len());
        let eta_str = &after_eta[..eta_end];
        eta_secs = parse_eta_to_secs(eta_str);
    }

    Some((pct, total_bytes, speed_bps, eta_secs))
}

fn parse_size_to_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.ends_with("GiB") {
        let num: f64 = s.strip_suffix("GiB")?.trim().parse().ok()?;
        Some((num * 1024.0 * 1024.0 * 1024.0) as u64)
    } else if s.ends_with("MiB") {
        let num: f64 = s.strip_suffix("MiB")?.trim().parse().ok()?;
        Some((num * 1024.0 * 1024.0) as u64)
    } else if s.ends_with("KiB") {
        let num: f64 = s.strip_suffix("KiB")?.trim().parse().ok()?;
        Some((num * 1024.0) as u64)
    } else if s.ends_with('B') {
        let num: f64 = s.strip_suffix('B')?.trim().parse().ok()?;
        Some(num as u64)
    } else {
        None
    }
}

fn parse_speed_to_bps(s: &str) -> u64 {
    let s = s.trim();
    if let Some(stripped) = s.strip_suffix("/s") {
        parse_size_to_bytes(stripped).unwrap_or(0)
    } else {
        0
    }
}

fn parse_eta_to_secs(s: &str) -> u64 {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        2 => {
            let mins: u64 = parts[0].parse().unwrap_or(0);
            let secs: u64 = parts[1].parse().unwrap_or(0);
            mins * 60 + secs
        }
        3 => {
            let hrs: u64 = parts[0].parse().unwrap_or(0);
            let mins: u64 = parts[1].parse().unwrap_or(0);
            let secs: u64 = parts[2].parse().unwrap_or(0);
            hrs * 3600 + mins * 60 + secs
        }
        _ => 0,
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
    // 1. Locate or download yt-dlp
    let ytdlp_bin = if let Some(ref p) = options.custom_ytdlp_path {
        p.clone()
    } else if let Some(p) = find_ytdlp_path() {
        p
    } else {
        // Automatically download yt-dlp
        let client = reqwest::Client::new();
        download_ytdlp_binary(&client).await?
    };

    // 2. Build command arguments
    let mut args = Vec::new();
    args.push("--newline".to_string());
    args.push("--progress".to_string());
    args.push("--no-colors".to_string());

    // JS runtime resolution (solves YouTube n-sig / player challenges)
    if let Some(js_runtime) = find_js_runtime() {
        args.push("--js-runtimes".to_string());
        args.push(js_runtime);
    }

    // FFmpeg location
    if let Some(ffmpeg_bin) = find_ffmpeg_path() {
        if let Some(parent) = ffmpeg_bin.parent() {
            args.push("--ffmpeg-location".to_string());
            args.push(parent.to_string_lossy().to_string());
        }
    }

    // Quality preset arguments
    args.extend(options.preset.to_args());

    // Cookies arguments
    args.extend(options.cookies.to_args());

    // Proxy argument
    if let Some(ref proxy) = options.proxy {
        args.push("--proxy".to_string());
        args.push(proxy.clone());
    }

    // High throughput buffers and concurrent fragments
    if options.concurrent_fragments > 1 {
        args.push("--concurrent-fragments".to_string());
        args.push(options.concurrent_fragments.clamp(1, 32).to_string());
    }
    args.push("--buffer-size".to_string());
    args.push("16M".to_string());
    args.push("--http-chunk-size".to_string());
    args.push("10M".to_string());

    // Output template
    let output_template = if let Some(ref name) = options.output_filename {
        options.output_dir.join(name).to_string_lossy().to_string()
    } else {
        options.output_dir.join("%(title)s.%(ext)s").to_string_lossy().to_string()
    };
    args.push("-o".to_string());
    args.push(output_template);

    // Target URL
    args.push(url.to_string());

    // 3. Spawn child process
    let mut cmd = Command::new(&ytdlp_bin);
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW (0x08000000) so no blank console window pops up
        cmd.creation_flags(0x08000000);
    }

    let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn yt-dlp: {}", e))?;

    let stdout = child.stdout.take().ok_or("Failed to capture yt-dlp stdout")?;
    let stderr = child.stderr.take().ok_or("Failed to capture yt-dlp stderr")?;

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut last_destination: Option<PathBuf> = None;
    let mut error_lines: Vec<String> = Vec::new();

    loop {
        if let Some(ref cf) = cancel_flag {
            if cf.load(Ordering::Relaxed) {
                let _ = child.kill().await;
                return Err("Download canceled by user".to_string());
            }
        }

        tokio::select! {
            _ = async {
                if let Some(ref cf) = cancel_flag {
                    while !cf.load(Ordering::Relaxed) {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                let _ = child.kill().await;
                return Err("Download canceled by user".to_string());
            }
            line_res = stdout_reader.next_line() => {
                match line_res {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        // Detect destination file
                        if let Some(dest) = trimmed.strip_prefix("[download] Destination:") {
                            let p = PathBuf::from(dest.trim());
                            last_destination = Some(p);
                        } else if let Some(dest) = trimmed.strip_prefix("[Merger] Merging formats into \"") {
                            if let Some(end) = dest.find('\"') {
                                let p = PathBuf::from(&dest[..end]);
                                last_destination = Some(p);
                            }
                        } else if trimmed.contains("has already been downloaded") {
                            if let Some(dest) = trimmed.strip_prefix("[download]") {
                                if let Some(end) = dest.find(" has already been downloaded") {
                                    let p = PathBuf::from(dest[..end].trim());
                                    last_destination = Some(p);
                                }
                            }
                        }

                        // Parse progress
                        if let Some((pct, total_bytes, speed, eta_secs)) = parse_ytdlp_progress_line(trimmed) {
                            if let Some(ref tx) = progress_tx {
                                let downloaded = ((total_bytes as f64) * (pct / 100.0)) as u64;
                                let _ = tx.send(ProgressUpdate {
                                    downloaded,
                                    total: total_bytes,
                                    speed: speed as f64,
                                    eta_seconds: Some(eta_secs),
                                    active_connections: 1,
                                }).await;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            err_res = stderr_reader.next_line() => {
                if let Ok(Some(err_line)) = err_res {
                    let trimmed = err_line.trim().to_string();
                    if trimmed.starts_with("ERROR:") {
                        error_lines.push(trimmed);
                    }
                }
            }
        }
    }

    let status = child.wait().await.map_err(|e| format!("Failed to wait on yt-dlp: {}", e))?;

    if !status.success() {
        let err_msg = if !error_lines.is_empty() {
            error_lines.join("\n")
        } else {
            format!("yt-dlp process exited with status {}", status)
        };
        return Err(err_msg);
    }

    // Return the downloaded file path
    if let Some(dest) = last_destination {
        if dest.is_file() {
            return Ok(dest);
        }
        // If relative, join with output_dir
        let full = options.output_dir.join(&dest);
        if full.is_file() {
            return Ok(full);
        }
    }

    Ok(options.output_dir.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ytdlp_progress_line() {
        let line1 = "[download]  45.2% of   28.46MiB at    3.50MiB/s ETA 00:04";
        let res1 = parse_ytdlp_progress_line(line1);
        assert!(res1.is_some());
        let (pct, total, speed, eta) = res1.unwrap();
        assert!((pct - 45.2).abs() < 0.001);
        assert_eq!(total, (28.46 * 1024.0 * 1024.0) as u64);
        assert_eq!(speed, (3.50 * 1024.0 * 1024.0) as u64);
        assert_eq!(eta, 4);

        let line2 = "[download] 100.0% of   1.50GiB at   25.00MiB/s ETA 00:00";
        let res2 = parse_ytdlp_progress_line(line2);
        assert!(res2.is_some());
        let (pct2, total2, speed2, eta2) = res2.unwrap();
        assert!((pct2 - 100.0).abs() < 0.001);
        assert_eq!(total2, (1.50 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(speed2, (25.00 * 1024.0 * 1024.0) as u64);
        assert_eq!(eta2, 0);
    }

    #[test]
    fn test_is_supported_media_site() {
        let yt = Url::parse("https://www.youtube.com/watch?v=X-pwWiF7FFI").unwrap();
        assert!(is_supported_media_site(&yt));

        let twitch = Url::parse("https://www.twitch.tv/videos/12345678").unwrap();
        assert!(is_supported_media_site(&twitch));

        let direct = Url::parse("https://example.com/file.zip").unwrap();
        assert!(!is_supported_media_site(&direct));
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
}
