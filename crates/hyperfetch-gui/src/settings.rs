use std::path::PathBuf;

use hyperfetch_core::engine::DownloadOptions;
use hyperfetch_core::history::DownloadHistoryManager;
use hyperfetch_core::media::{is_supported_media_site, BrowserCookieSource, MediaQualityPreset};
use serde::{Deserialize, Serialize};
use url::Url;

pub const MEDIA_PRESETS: [&str; 5] = [
    "Best Available (Merged MP4)",
    "1080p FHD (Merged MP4)",
    "720p HD (Merged MP4)",
    "Audio Only (MP3)",
    "Audio Only (M4A)",
];

pub const BROWSERS: [&str; 7] = [
    "None",
    "Google Chrome",
    "Microsoft Edge",
    "Mozilla Firefox",
    "Brave Browser",
    "Opera",
    "Vivaldi",
];

/// Options the user sets once and that are kept across launches. The checksum and the
/// Authorization header belong to a single download and are never saved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub save_dir: String,
    pub connections: usize,
    pub cookies_path: String,
    pub proxy: String,
    pub media_preset: usize,
    pub browser_cookies: usize,
    /// Speed cap in KB/s or MB/s (see `max_speed_in_mb`); 0 means unlimited.
    pub max_speed: f64,
    pub max_speed_in_mb: bool,
    pub max_retries: u32,
    pub stall_timeout_secs: u64,
    pub clipboard_watch: bool,
    pub auto_run_queue: bool,
    pub max_concurrent: usize,
}

impl Default for Settings {
    fn default() -> Self {
        let engine = DownloadOptions::default();
        Self {
            save_dir: default_download_dir().to_string_lossy().into_owned(),
            connections: 16,
            cookies_path: String::new(),
            proxy: String::new(),
            media_preset: 0,
            browser_cookies: 0,
            max_speed: 0.0,
            max_speed_in_mb: true,
            max_retries: engine.max_retries,
            stall_timeout_secs: engine.stall_timeout_secs,
            clipboard_watch: true,
            auto_run_queue: true,
            max_concurrent: 2,
        }
    }
}

impl Settings {
    /// Stored next to the download history.
    fn path() -> PathBuf {
        DownloadHistoryManager::default_history_path().with_file_name("gui-settings.json")
    }

    /// Saved settings, or the defaults when there are none or they cannot be read.
    pub fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    /// Engine options for downloading `urls` with these settings plus the per-download
    /// checksum and Authorization header.
    pub fn download_options(&self, urls: &[Url], checksum: &str, auth: &str) -> Result<DownloadOptions, String> {
        let checksum = non_empty(checksum);
        if let Some(c) = &checksum {
            hyperfetch_core::storage::validate_checksum(c)?;
        }
        let save_dir = self.save_dir.trim();
        if save_dir.is_empty() {
            return Err("Choose a folder to save downloads to".to_string());
        }
        // A preset sends every non-file URL to yt-dlp, so it is only set for known media sites.
        let media_preset = urls.iter().any(is_supported_media_site).then_some(match self.media_preset {
            1 => MediaQualityPreset::Fhd1080p,
            2 => MediaQualityPreset::Hd720p,
            3 => MediaQualityPreset::AudioMp3,
            4 => MediaQualityPreset::AudioM4a,
            _ => MediaQualityPreset::BestVideoAudio,
        });
        let browser_cookies = match self.browser_cookies {
            1 => Some(BrowserCookieSource::Chrome),
            2 => Some(BrowserCookieSource::Edge),
            3 => Some(BrowserCookieSource::Firefox),
            4 => Some(BrowserCookieSource::Brave),
            5 => Some(BrowserCookieSource::Opera),
            6 => Some(BrowserCookieSource::Vivaldi),
            _ => None,
        };
        Ok(DownloadOptions {
            num_connections: self.connections.clamp(1, 64),
            output_path: Some(PathBuf::from(save_dir)),
            expected_checksum: checksum,
            cookies_path: non_empty(&self.cookies_path).map(PathBuf::from),
            auth_header: non_empty(auth),
            proxy: non_empty(&self.proxy),
            media_preset,
            browser_cookies,
            max_speed: speed_limit_bytes(self.max_speed, self.max_speed_in_mb),
            max_retries: self.max_retries,
            stall_timeout_secs: self.stall_timeout_secs.max(1),
            ..Default::default()
        })
    }
}

fn non_empty(s: &str) -> Option<String> {
    Some(s.trim()).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Bytes per second for a limit given in KB/s or MB/s; `None` (unlimited) for zero or less.
pub fn speed_limit_bytes(value: f64, in_mb: bool) -> Option<u64> {
    let unit = if in_mb { 1024.0 * 1024.0 } else { 1024.0 };
    (value > 0.0).then(|| ((value * unit) as u64).max(1))
}

/// The user's Downloads folder: `%USERPROFILE%\Downloads` on Windows, the XDG download
/// directory (from `user-dirs.dirs`) on Linux, `~/Downloads` otherwise.
pub fn default_download_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile).join("Downloads");
    }
    #[cfg(unix)]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        return std::fs::read_to_string(config.join("user-dirs.dirs"))
            .ok()
            .and_then(|contents| xdg_download_dir(&contents, &home))
            .unwrap_or_else(|| home.join("Downloads"));
    }
    PathBuf::from(".")
}

/// `XDG_DOWNLOAD_DIR` from the contents of `user-dirs.dirs` (values are `"$HOME/..."` or absolute).
#[cfg(any(unix, test))]
fn xdg_download_dir(contents: &str, home: &std::path::Path) -> Option<PathBuf> {
    let value = contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("XDG_DOWNLOAD_DIR="))?
        .trim()
        .trim_matches('"');
    if let Some(rest) = value.strip_prefix("$HOME") {
        return Some(home.join(rest.trim_start_matches('/')));
    }
    Some(PathBuf::from(value)).filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_download_dir_expands_home_and_ignores_comments() {
        let home = std::path::Path::new("/home/u");
        let file = "# XDG_DOWNLOAD_DIR=\"$HOME/nope\"\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\n  XDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n";
        assert_eq!(xdg_download_dir(file, home), Some(home.join("Téléchargements")));
        assert_eq!(xdg_download_dir("XDG_DOWNLOAD_DIR=\"$HOME/\"", home), Some(home.join("")));
        assert_eq!(xdg_download_dir("XDG_DOWNLOAD_DIR=\"relative/dir\"", home), None);
        assert_eq!(xdg_download_dir("XDG_MUSIC_DIR=\"$HOME/Music\"", home), None);
        let absolute = if cfg!(windows) { "C:\\dl" } else { "/mnt/dl" };
        assert_eq!(xdg_download_dir(&format!("XDG_DOWNLOAD_DIR=\"{}\"", absolute), home), Some(PathBuf::from(absolute)));
    }

    #[test]
    fn speed_limit_converts_units() {
        assert_eq!(speed_limit_bytes(0.0, true), None);
        assert_eq!(speed_limit_bytes(-3.0, false), None);
        assert_eq!(speed_limit_bytes(512.0, false), Some(512 * 1024));
        assert_eq!(speed_limit_bytes(1.5, true), Some(3 * 512 * 1024));
        assert_eq!(speed_limit_bytes(0.0001, false), Some(1));
    }

    #[test]
    fn download_options_capture_every_setting() {
        let settings = Settings {
            save_dir: "dl".into(),
            connections: 200,
            proxy: " socks5://127.0.0.1:1080 ".into(),
            cookies_path: "c.txt".into(),
            max_speed: 2.0,
            max_speed_in_mb: true,
            max_retries: 3,
            stall_timeout_secs: 0,
            browser_cookies: 2,
            media_preset: 3,
            ..Settings::default()
        };
        let file = [Url::parse("https://example.com/a.iso").unwrap()];
        let opts = settings.download_options(&file, "sha256:abcd", "Bearer t").unwrap();
        assert_eq!(opts.num_connections, 64);
        assert_eq!(opts.output_path, Some(PathBuf::from("dl")));
        assert_eq!(opts.proxy.as_deref(), Some("socks5://127.0.0.1:1080"));
        assert_eq!(opts.cookies_path, Some(PathBuf::from("c.txt")));
        assert_eq!(opts.expected_checksum.as_deref(), Some("sha256:abcd"));
        assert_eq!(opts.auth_header.as_deref(), Some("Bearer t"));
        assert_eq!(opts.max_speed, Some(2 * 1024 * 1024));
        assert_eq!((opts.max_retries, opts.stall_timeout_secs), (3, 1));
        assert_eq!(opts.browser_cookies, Some(BrowserCookieSource::Edge));
        assert_eq!(opts.media_preset, None, "a preset would send a plain file URL to yt-dlp");

        let video = [Url::parse("https://www.youtube.com/watch?v=x").unwrap()];
        let opts = settings.download_options(&video, "", "").unwrap();
        assert_eq!(opts.media_preset, Some(MediaQualityPreset::AudioMp3));
        assert_eq!((opts.expected_checksum, opts.auth_header), (None, None));

        assert!(settings.download_options(&file, "crc32:1234", "").is_err());
        let no_dir = Settings { save_dir: "  ".into(), ..Settings::default() };
        assert!(no_dir.download_options(&file, "", "").is_err());
    }

    #[test]
    fn settings_round_trip_and_tolerate_missing_fields() {
        let settings = Settings { proxy: "http://p:8080".into(), max_concurrent: 4, ..Settings::default() };
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), settings);
        let partial: Settings = serde_json::from_str(r#"{"connections": 4}"#).unwrap();
        assert_eq!(partial.connections, 4);
        assert_eq!(partial.max_retries, Settings::default().max_retries);
    }
}
