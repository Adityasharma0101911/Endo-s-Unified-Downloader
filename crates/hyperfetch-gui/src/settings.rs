use std::net::IpAddr;
use std::path::{Path, PathBuf};

use hyperfetch_core::engine::DownloadOptions;
use hyperfetch_core::history::DownloadHistoryManager;
use hyperfetch_core::ingest::ListOptions;
use hyperfetch_core::media::{is_supported_media_site, BrowserCookieSource, MediaQualityPreset};
use hyperfetch_core::postprocess::PostOptions;
use serde::{Deserialize, Serialize};
use url::Url;

pub const MEDIA_PRESETS: [&str; 5] = [
    "Best Available (Merged MP4)",
    "1080p FHD (Merged MP4)",
    "720p HD (Merged MP4)",
    "Audio Only (MP3)",
    "Audio Only (M4A)",
];

/// What `Settings::sponsorblock` does with a YouTube video's SponsorBlock segments.
pub const SPONSORBLOCK_MODES: [&str; 3] = ["Off", "Remove them", "Mark as chapters"];

/// The containers `Settings::merge_format` merges video and audio into.
pub const MERGE_FORMATS: [&str; 3] = ["MP4", "MKV", "WebM"];

pub const BROWSERS: [&str; 7] = [
    "None",
    "Google Chrome",
    "Microsoft Edge",
    "Mozilla Firefox",
    "Brave Browser",
    "Opera",
    "Vivaldi",
];

/// The environment variable the command line takes the Google API key from too.
pub const GOOGLE_API_KEY_VAR: &str = "ENDO_GOOGLE_API_KEY";

/// Options the user sets once and that are kept across launches. The checksum and the
/// Authorization header belong to a single download and are never saved, nor is the Google API
/// key, a credential.
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
    /// Wait for each finished file to reach the disk before reporting it done.
    pub fsync_on_complete: bool,
    /// Connections all running downloads may hold to one host together (0 = no limit).
    pub max_connections_per_host: usize,
    pub clipboard_watch: bool,
    pub auto_run_queue: bool,
    pub max_concurrent: usize,
    /// The user's Google API key, for listing whole Google Drive folders (empty = none). Never
    /// saved: [`Settings::load`] takes it from [`GOOGLE_API_KEY_VAR`], and one an older version
    /// saved is read and left out of the file the next time the settings are saved.
    #[serde(skip_serializing)]
    pub google_api_key: String,
    /// Leave out the items of a channel, playlist, feed or cloud folder downloaded before.
    pub only_new: bool,
    /// Only the newest this many items of a channel, playlist or feed (0 = all).
    pub latest: usize,
    /// Install a managed ffmpeg (about 200 MB) when a media download needs one and none is found:
    /// None until the user answers (see `App::start_job`). Saved under a key of its own, as the
    /// `install_ffmpeg: true` older versions saved was their default, never an answer.
    #[serde(rename = "install_ffmpeg_answer")]
    pub install_ffmpeg: Option<bool>,
    /// Subtitle languages for media downloads, e.g. "en,es" or "all" (empty = none).
    pub subtitles: String,
    /// Title, artist, date and URL tags, chapters and cover art inside media files.
    pub embed_metadata: bool,
    /// Record live streams from their start.
    pub live_from_start: bool,
    /// Wait for a scheduled stream or premiere to begin instead of failing.
    pub wait_for_video: bool,
    /// A YouTube video's SponsorBlock segments: 0 left alone, 1 removed, 2 marked as chapters
    /// (see [`SPONSORBLOCK_MODES`]).
    pub sponsorblock: usize,
    /// The container video and audio are merged into: 0 MP4, 1 MKV, 2 WebM (see [`MERGE_FORMATS`]).
    pub merge_format: usize,
    /// Remux HLS streams saved as MPEG-TS into MP4 (see `DownloadOptions::hls_to_mp4`). On by
    /// default, also for settings an older version saved.
    pub hls_to_mp4: bool,
    /// Optional HTTP Referer header for downloads requiring anti-hotlinking bypass.
    pub referer: String,
    /// Comma-separated list of proxy URLs for multi-egress rotation across workers.
    pub proxy_pool: String,
    /// Debrid API key for automatic high-speed CDN unrestricting. Saved here, never with the queue
    /// or in the history (see [`Settings::fill_keys`]).
    pub debrid_api_key: String,
    /// Debrid provider: "realdebrid", "alldebrid", "torbox" or "premiumize".
    pub debrid_provider: String,
    /// Look for a newer release at launch (see `hyperfetch_core::updater::check`). On by default,
    /// also for settings an older version saved.
    pub check_updates: bool,
    /// Spread connections over several networks (see [`Settings::bind_addresses`]).
    pub multi_network: bool,
    /// The local IP addresses chosen for that; none means every usable network.
    pub bind_addresses: Vec<String>,
    /// Download magnet links and .torrent files without web seeds from the BitTorrent swarm.
    pub p2p: bool,
    /// Seed a finished torrent until it has uploaded this many times its size (0 = don't seed).
    pub seed_ratio: f64,
    /// ... or for this many minutes, whichever comes first (0 = no time limit).
    pub seed_minutes: u32,
    /// BitTorrent listen port (0 = librqbit's default range).
    pub bt_port: u16,
    /// Ask the router (UPnP) to forward the BitTorrent port.
    pub bt_upnp: bool,
    /// Magnet links go through debrid when a debrid key is set.
    pub debrid_magnets: bool,
    /// Image galleries go to gallery-dl.
    pub gallery_dl: bool,
    /// Unpack archives after download.
    pub auto_extract: bool,
    /// Delete an archive once it is unpacked.
    pub delete_archives: bool,
    /// Move single files into category folders of the save folder.
    pub sort_downloads: bool,
    /// Command run after each download (empty = none).
    pub run_after: String,
    /// Mark downloaded files as from the internet (Windows).
    pub mark_of_the_web: bool,
    /// VirusTotal API key. Saved here, never with the queue or in the history.
    pub virustotal_api_key: String,
    /// Look each downloaded file's SHA-256 up on VirusTotal (the hash only, never the file).
    pub virustotal_check: bool,
    /// 0 follows the system, 1 dark, 2 light.
    pub theme: usize,
    /// Instant transitions and nothing moving by itself; None follows the system (see
    /// [`Settings::reduces_motion`]).
    pub reduce_motion: Option<bool>,
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
            fsync_on_complete: engine.fsync_on_complete,
            max_connections_per_host: engine.max_connections_per_host,
            clipboard_watch: true,
            auto_run_queue: true,
            max_concurrent: 4,
            google_api_key: String::new(),
            only_new: true,
            latest: 0,
            install_ffmpeg: None,
            subtitles: String::new(),
            embed_metadata: engine.embed_metadata,
            live_from_start: engine.live_from_start,
            wait_for_video: engine.wait_for_video,
            sponsorblock: 0,
            merge_format: 0,
            hls_to_mp4: true,
            referer: String::new(),
            proxy_pool: String::new(),
            debrid_api_key: String::new(),
            debrid_provider: String::new(),
            check_updates: true,
            multi_network: false,
            bind_addresses: Vec::new(),
            p2p: true,
            seed_ratio: 1.0,
            seed_minutes: 60,
            bt_port: 0,
            bt_upnp: false,
            debrid_magnets: engine.debrid_magnets,
            gallery_dl: engine.gallery_dl,
            auto_extract: false,
            delete_archives: false,
            sort_downloads: false,
            run_after: String::new(),
            mark_of_the_web: engine.post.mark_of_the_web,
            virustotal_api_key: String::new(),
            virustotal_check: false,
            theme: 0,
            reduce_motion: None,
        }
    }
}

/// A file of the GUI's own, stored next to the download history.
pub fn app_file(name: &str) -> PathBuf {
    DownloadHistoryManager::default_history_path().with_file_name(name)
}

/// Replaces `path` with `bytes` in one step: a crash leaves either the old file or the new one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let written = std::fs::File::create(&tmp).and_then(|mut file| {
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

impl Settings {
    fn path() -> PathBuf {
        app_file("gui-settings.json")
    }

    /// Saved settings, or the defaults when there are none or they cannot be read, with the
    /// Google API key from [`GOOGLE_API_KEY_VAR`] when that is set.
    pub fn load() -> Self {
        let mut settings: Self =
            std::fs::read(Self::path()).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        if let Some(key) = std::env::var(GOOGLE_API_KEY_VAR).ok().as_deref().and_then(non_empty) {
            settings.google_api_key = key;
        }
        settings
    }

    pub fn save(&self) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        write_atomic(&Self::path(), &json)
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
        let quality = match self.media_preset {
            1 => MediaQualityPreset::Fhd1080p,
            2 => MediaQualityPreset::Hd720p,
            3 => MediaQualityPreset::AudioMp3,
            4 => MediaQualityPreset::AudioM4a,
            _ => MediaQualityPreset::BestVideoAudio,
        };
        // A preset sends every non-file URL to yt-dlp, so it is only set for known media sites;
        // any other link that turns out to be media takes the quality all the same.
        let media_preset = urls.iter().any(is_supported_media_site).then(|| quality.clone());
        Ok(DownloadOptions {
            output_path: Some(PathBuf::from(save_dir)),
            expected_checksum: checksum,
            cookies_path: non_empty(&self.cookies_path).map(PathBuf::from),
            auth_header: non_empty(auth),
            referer: non_empty(&self.referer),
            proxy: non_empty(&self.proxy),
            media_preset,
            page_media_preset: Some(quality),
            browser_cookies: self.browser_cookies(),
            install_ffmpeg: self.install_ffmpeg == Some(true),
            subtitles: non_empty(&self.subtitles),
            embed_metadata: self.embed_metadata,
            live_from_start: self.live_from_start,
            wait_for_video: self.wait_for_video,
            sponsorblock: match self.sponsorblock {
                1 => Some("remove".to_string()),
                2 => Some("mark".to_string()),
                _ => None,
            },
            // MP4 is what video and audio are merged into without a choice, so it sets none.
            merge_format: match self.merge_format {
                1 => Some("mkv".to_string()),
                2 => Some("webm".to_string()),
                _ => None,
            },
            hls_to_mp4: self.hls_to_mp4,
            post: PostOptions {
                extract: self.auto_extract,
                delete_archive: self.delete_archives,
                sort: self.sort_downloads,
                run_after: non_empty(&self.run_after),
                mark_of_the_web: self.mark_of_the_web,
                // Never saved with the queue: see `fill_keys`.
                virustotal_key: None,
            },
            ..self.tuning()
        })
    }

    /// Puts the keys a queued download is never saved with (debrid, VirusTotal) into its
    /// `options`, from these settings, as it starts; and the local addresses it connects from,
    /// as the networks of when it was queued may be gone.
    pub fn fill_keys(&self, options: &mut DownloadOptions) {
        options.bind_addresses = self.bind_addresses();
        options.debrid_api_key = non_empty(&self.debrid_api_key);
        options.debrid_provider = non_empty(&self.debrid_provider);
        options.post.virustotal_key = non_empty(&self.virustotal_api_key).filter(|_| self.virustotal_check);
    }

    /// The local addresses downloads connect from when `multi_network` is on: the chosen ones,
    /// else every usable network's. None (the system's choice) when it is off.
    pub fn bind_addresses(&self) -> Vec<IpAddr> {
        if !self.multi_network {
            return Vec::new();
        }
        let chosen: Vec<IpAddr> = self.bind_addresses.iter().filter_map(|a| a.trim().parse().ok()).collect();
        if chosen.is_empty() {
            return hyperfetch_core::netif::usable().into_iter().map(|i| i.address).collect();
        }
        chosen
    }

    /// How a link that lists many downloads (a folder, feed, playlist or channel) is read with
    /// these settings: through the proxy, with the browser's cookies, else the cookies file.
    pub fn list_options(&self) -> ListOptions {
        let cookies_file = || non_empty(&self.cookies_path).map(|path| BrowserCookieSource::File(PathBuf::from(path)));
        ListOptions {
            google_api_key: non_empty(&self.google_api_key),
            whole_playlist: false,
            latest: Some(self.latest).filter(|&n| n > 0),
            only_new: self.only_new,
            cookies: self.browser_cookies().or_else(cookies_file).unwrap_or_default(),
            proxy: non_empty(&self.proxy),
            notes: None,
            p2p: self.p2p,
            debrid_key: non_empty(&self.debrid_api_key),
            debrid_provider: non_empty(&self.debrid_provider),
            debrid_magnets: self.debrid_magnets,
            password: None,
            auth: None,
        }
    }

    fn browser_cookies(&self) -> Option<BrowserCookieSource> {
        match self.browser_cookies {
            1 => Some(BrowserCookieSource::Chrome),
            2 => Some(BrowserCookieSource::Edge),
            3 => Some(BrowserCookieSource::Firefox),
            4 => Some(BrowserCookieSource::Brave),
            5 => Some(BrowserCookieSource::Opera),
            6 => Some(BrowserCookieSource::Vivaldi),
            _ => None,
        }
    }

    /// Whether to reduce motion: as set, else as the system's animation setting says.
    pub fn reduces_motion(&self) -> bool {
        self.reduce_motion.unwrap_or_else(system_reduces_motion)
    }

    /// The engine settings shared by every download and repair: connections, speed limit,
    /// retries, timeouts, disk flushing and the per-host connection budget.
    pub fn tuning(&self) -> DownloadOptions {
        let proxy_pool = self.proxy_pool
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let debrid_api_key = (!self.debrid_api_key.trim().is_empty()).then(|| self.debrid_api_key.trim().to_string());
        let debrid_provider = (!self.debrid_provider.trim().is_empty()).then(|| self.debrid_provider.trim().to_string());
        DownloadOptions {
            num_connections: self.connections.clamp(1, 64),
            max_speed: speed_limit_bytes(self.max_speed, self.max_speed_in_mb),
            max_retries: self.max_retries,
            stall_timeout_secs: self.stall_timeout_secs.max(1),
            fsync_on_complete: self.fsync_on_complete,
            max_connections_per_host: self.max_connections_per_host,
            proxy_pool,
            debrid_api_key,
            debrid_provider,
            bind_addresses: self.bind_addresses(),
            seed_ratio: self.seed_ratio.max(0.0),
            seed_minutes: self.seed_minutes,
            bt_port: Some(self.bt_port).filter(|&port| port > 0),
            bt_upnp: self.bt_upnp,
            debrid_magnets: self.debrid_magnets,
            gallery_dl: self.gallery_dl,
            ..Default::default()
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    Some(s.trim()).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Whether Windows' "Animation effects" (Show animations in Windows) is off; asked each time,
/// so that switching it applies at once. Elsewhere animations stay on.
#[cfg(windows)]
fn system_reduces_motion() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION};
    let mut on = 1;
    // SAFETY: this action writes one BOOL to the pointer given, which points at one.
    let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, &mut on as *mut i32 as *mut std::ffi::c_void, 0) };
    ok != 0 && on == 0
}

/// Whether Reduce motion is on in the Accessibility settings of macOS, read once (asking takes a
/// program run); off when it cannot be read.
#[cfg(target_os = "macos")]
fn system_reduces_motion() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let read = std::process::Command::new("/usr/bin/defaults").args(["read", "com.apple.universalaccess", "reduceMotion"]).output();
        read.is_ok_and(|out| out.status.success() && out.stdout.trim_ascii() == b"1")
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn system_reduces_motion() -> bool {
    false
}

/// Bytes per second for a limit given in KB/s or MB/s; `None` (unlimited) for zero or less.
pub fn speed_limit_bytes(value: f64, in_mb: bool) -> Option<u64> {
    let unit = if in_mb { 1024.0 * 1024.0 } else { 1024.0 };
    (value > 0.0).then(|| ((value * unit) as u64).max(1))
}

/// The user's Downloads folder: the Downloads known folder on Windows (wherever it was moved),
/// the XDG download directory (from `user-dirs.dirs`) on Linux, `~/Downloads` otherwise.
pub fn default_download_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(downloads) = known_downloads_dir() {
        return downloads;
    }
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

/// The current location of the Downloads known folder, which the user may have moved.
#[cfg(windows)]
fn known_downloads_dir() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath};

    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: valid pointers to the folder id and the out-parameter; no access token.
    let result = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, 0, std::ptr::null_mut(), &mut raw) };
    let path = (result >= 0 && !raw.is_null()).then(|| {
        // SAFETY: on success `raw` is a NUL-terminated UTF-16 string that stays valid until freed below.
        let wide = unsafe {
            let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
            std::slice::from_raw_parts(raw, len)
        };
        PathBuf::from(std::ffi::OsString::from_wide(wide))
    });
    // SAFETY: the buffer is freed exactly once, as documented, also when the call failed (null is allowed).
    unsafe { CoTaskMemFree(raw as *const std::ffi::c_void) };
    path.filter(|p| p.is_absolute())
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

    #[cfg(windows)]
    #[test]
    fn windows_default_is_the_downloads_known_folder() {
        let known = known_downloads_dir().expect("every Windows profile has a Downloads known folder");
        assert!(known.is_absolute());
        assert_eq!(default_download_dir(), known);
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
            fsync_on_complete: true,
            max_connections_per_host: 0,
            ..Settings::default()
        };
        let file = [Url::parse("https://example.com/a.iso").unwrap()];
        let opts = settings.download_options(&file, "sha256:abababababababababababababababababababababababababababababababab", "Bearer t").unwrap();
        assert_eq!(opts.num_connections, 64);
        assert_eq!(opts.output_path, Some(PathBuf::from("dl")));
        assert_eq!(opts.proxy.as_deref(), Some("socks5://127.0.0.1:1080"));
        assert_eq!(opts.cookies_path, Some(PathBuf::from("c.txt")));
        assert_eq!(opts.expected_checksum.as_deref(), Some("sha256:abababababababababababababababababababababababababababababababab"));
        assert_eq!(opts.auth_header.as_deref(), Some("Bearer t"));
        assert_eq!(opts.max_speed, Some(2 * 1024 * 1024));
        assert_eq!((opts.max_retries, opts.stall_timeout_secs), (3, 1));
        assert_eq!(opts.browser_cookies, Some(BrowserCookieSource::Edge));
        assert_eq!((opts.fsync_on_complete, opts.max_connections_per_host), (true, 0));
        assert_eq!(opts.media_preset, None, "a preset would send a plain file URL to yt-dlp");
        // A link that turns out to be media still gets the chosen quality.
        assert_eq!(opts.page_media_preset, Some(MediaQualityPreset::AudioMp3));

        let video = [Url::parse("https://www.youtube.com/watch?v=x").unwrap()];
        let opts = settings.download_options(&video, "", "").unwrap();
        assert_eq!(opts.media_preset, Some(MediaQualityPreset::AudioMp3));
        assert_eq!(opts.page_media_preset, Some(MediaQualityPreset::AudioMp3));
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

    /// Listing and media settings saved by an older version take their defaults, and reach the
    /// listers and the engine.
    #[test]
    fn listing_and_media_settings() {
        // The install_ffmpeg older versions saved was their default, not the user's answer.
        let saved: Settings = serde_json::from_str(r#"{"proxy": "http://p:8080", "cookies_path": "c.txt", "install_ffmpeg": true}"#).unwrap();
        assert!(saved.only_new && saved.install_ffmpeg.is_none() && saved.embed_metadata && !saved.live_from_start);
        assert_eq!((saved.latest, saved.google_api_key.as_str(), saved.subtitles.as_str()), (0, "", ""));
        let list = saved.list_options();
        assert!(list.only_new && !list.whole_playlist);
        assert_eq!((list.google_api_key, list.latest), (None, None));
        assert_eq!((list.cookies, list.proxy.as_deref()), (BrowserCookieSource::File("c.txt".into()), Some("http://p:8080")));
        let file = [Url::parse("https://example.com/a.iso").unwrap()];
        let opts = saved.download_options(&file, "", "").unwrap();
        assert!(!opts.install_ffmpeg && opts.embed_metadata && !opts.live_from_start && !opts.wait_for_video);
        assert!(saved.hls_to_mp4 && opts.hls_to_mp4, "HLS is remuxed to MP4 unless turned off");
        assert_eq!(opts.subtitles, None);
        assert_eq!((saved.sponsorblock, saved.merge_format), (0, 0));
        assert_eq!((opts.sponsorblock, opts.merge_format), (None, None), "no SponsorBlock, and MP4 as without a choice");
        let agreed = Settings { install_ffmpeg: Some(true), ..saved.clone() };
        assert!(agreed.download_options(&file, "", "").unwrap().install_ffmpeg);

        let chosen = Settings {
            google_api_key: " AIzaKey ".into(),
            only_new: false,
            latest: 5,
            browser_cookies: 3,
            install_ffmpeg: Some(false),
            subtitles: "en,es".into(),
            embed_metadata: false,
            live_from_start: true,
            wait_for_video: true,
            hls_to_mp4: false,
            sponsorblock: 2,
            merge_format: 1,
            ..saved
        };
        let list = chosen.list_options();
        assert_eq!((list.google_api_key.as_deref(), list.latest, list.only_new), (Some("AIzaKey"), Some(5), false));
        assert_eq!(list.cookies, BrowserCookieSource::Firefox);
        let opts = chosen.download_options(&file, "", "").unwrap();
        assert!(!opts.install_ffmpeg && !opts.embed_metadata && opts.live_from_start && opts.wait_for_video);
        assert!(!opts.hls_to_mp4);
        assert_eq!(opts.subtitles.as_deref(), Some("en,es"));
        assert_eq!((opts.sponsorblock.as_deref(), opts.merge_format.as_deref()), (Some("mark"), Some("mkv")));
        let other = Settings { sponsorblock: 1, merge_format: 2, ..chosen.clone() }.download_options(&file, "", "").unwrap();
        assert_eq!((other.sponsorblock.as_deref(), other.merge_format.as_deref()), (Some("remove"), Some("webm")));
        let json = serde_json::to_string(&chosen).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), Settings { google_api_key: String::new(), ..chosen });
    }

    /// The Google API key is a credential: it is never written to the settings file, and one an
    /// older version wrote there is read, so the next save leaves it out.
    #[test]
    fn the_google_api_key_is_not_saved() {
        let settings = Settings { google_api_key: "AIzaSecret".into(), ..Settings::default() };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("AIzaSecret") && !json.contains("google_api_key"), "{json}");
        let older: Settings = serde_json::from_str(r#"{"google_api_key": "AIzaOld"}"#).unwrap();
        assert_eq!(older.google_api_key, "AIzaOld");
        assert!(!serde_json::to_string(&older).unwrap().contains("AIzaOld"));
    }

    /// Reduce motion follows the system until set, also in settings an older version saved.
    #[test]
    fn reduce_motion_follows_the_system_until_set() {
        let saved: Settings = serde_json::from_str(r#"{"theme": 1}"#).unwrap();
        assert_eq!(saved.reduce_motion, None);
        assert_eq!(saved.reduces_motion(), system_reduces_motion());
        for on in [false, true] {
            let chosen = Settings { reduce_motion: Some(on), ..Settings::default() };
            assert_eq!(chosen.reduces_motion(), on);
            assert_eq!(serde_json::from_str::<Settings>(&serde_json::to_string(&chosen).unwrap()).unwrap(), chosen);
        }
    }

    #[test]
    fn new_defaults_leave_saved_choices_alone() {
        let defaults = Settings::default();
        assert_eq!((defaults.max_concurrent, defaults.fsync_on_complete, defaults.max_connections_per_host), (4, false, 64));
        assert!(defaults.check_updates);
        // Saved by an older version: the user's own limit stays, the new settings take their defaults.
        let saved: Settings = serde_json::from_str(r#"{"max_concurrent": 2}"#).unwrap();
        assert_eq!((saved.max_concurrent, saved.fsync_on_complete, saved.max_connections_per_host), (2, false, 64));
        assert!(saved.check_updates, "updates are checked unless turned off");
        let chosen = Settings { fsync_on_complete: true, max_connections_per_host: 8, check_updates: false, ..defaults };
        let json = serde_json::to_string(&chosen).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), chosen);
    }
}
