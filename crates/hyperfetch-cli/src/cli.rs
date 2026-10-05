//! Command-line arguments and the parsers that validate them.

use std::path::PathBuf;

use clap::{ArgAction, Parser, ValueEnum};
use hyperfetch_core::engine::DownloadOptions;
use hyperfetch_core::media::{BrowserCookieSource, MediaQualityPreset};

#[derive(Parser, Debug)]
#[command(
    name = "Endos-Unified-Downloader-CLI",
    version,
    about = "High-speed multi-connection download accelerator",
    after_help = "Exit status: 0 all downloads finished, 1 a download failed, 2 usage error or \
                  verification failed, 130 interrupted (Ctrl+C), 143 terminated (SIGTERM).\n\
                  Press Ctrl+C once to stop and save resume state (a live recording is finished and \
                  kept); press it again to quit immediately."
)]
pub struct Args {
    /// Mirrors of ONE file (all must serve identical bytes), a magnet link, a local/remote
    /// .metalink, .meta4 or .torrent, or a link that lists many downloads (a Google Drive,
    /// MediaFire or MEGA folder, a Pixeldrain list or Gofile folder, a playlist or channel, a
    /// podcast feed). A paste works too: links with "Password: x", "Key: x" or a token, a
    /// user:pass@ link, or a copied curl/wget/fetch/PowerShell command (each link then is its
    /// own download, with its secrets, which are never shown or saved); "-" reads it from stdin.
    /// Without URLs and without -i an interactive prompt starts.
    #[arg(num_args = 0..)]
    pub urls: Vec<String>,

    /// Batch file: one download per line (space-separated mirrors, # comments); "-" reads stdin
    #[arg(short = 'i', long = "input-file", value_name = "FILE")]
    pub input_file: Option<PathBuf>,

    /// Connections per download (1-64)
    #[arg(short = 's', long = "split", default_value_t = 16, value_parser = clap::value_parser!(u64).range(1..=64))]
    pub connections: u64,

    /// Base chunk size in MiB (1-1024)
    #[arg(short = 'c', long = "chunk-size", default_value_t = 4, value_parser = clap::value_parser!(u64).range(1..=1024))]
    pub chunk_size_mb: u64,

    /// Downloads to run at the same time in batch mode (1-32); above 1, results are printed as
    /// they finish, each naming its input
    #[arg(short = 'j', long = "max-concurrent-downloads", default_value_t = 4, value_parser = clap::value_parser!(u64).range(1..=32))]
    pub jobs: u64,

    /// Connections all running downloads may open to one host together (0 = no limit)
    #[arg(long = "max-connections-per-host", value_name = "N", default_value_t = DownloadOptions::default().max_connections_per_host)]
    pub max_connections_per_host: usize,

    /// Output FILE path (single download only; relative to -d when both are given)
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Directory to save downloads in (created if missing) [default: current directory]
    #[arg(short = 'd', long = "dir", value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// No progress bars or status lines (errors and warnings still go to stderr)
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// More log output (-v info, -vv debug, -vvv trace); RUST_LOG overrides it
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
    pub verbose: u8,

    /// Speed limit in bytes/s for all running downloads (-j) together, e.g. 500K, 2M, 1.5MiB (K/M/G
    /// are powers of 1024; 0 = unlimited)
    #[arg(long = "max-speed", value_name = "RATE", value_parser = parse_speed)]
    pub max_speed: Option<u64>,

    /// Failed attempts per chunk before a download gives up (attempts that made progress are free)
    #[arg(long = "max-retries", default_value_t = 8, value_parser = clap::value_parser!(u32).range(0..=1000))]
    pub max_retries: u32,

    /// Seconds without data before a connection is considered stalled and retried (1-3600)
    #[arg(long = "stall-timeout", value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub stall_timeout: u64,

    /// Wait until each finished file is on the disk before reporting it done. Without it the OS
    /// writes the file out on its own schedule, so a power loss right after a download finishes
    /// can damage the file (--verify detects that)
    #[arg(long = "fsync")]
    pub fsync: bool,

    /// Expected checksum (sha256:HEX, sha512:HEX, sha1:HEX, md5:HEX, blake3:HEX or bare hex); single download or --verify
    #[arg(long = "checksum", value_parser = parse_checksum)]
    pub checksum: Option<String>,

    /// Netscape cookies.txt file
    #[arg(long = "load-cookies", value_name = "FILE", value_parser = existing_file, conflicts_with = "repair")]
    pub load_cookies: Option<PathBuf>,

    /// Authorization header, e.g. "Authorization: Bearer TOKEN" or just "Bearer TOKEN"
    #[arg(long = "header", value_name = "HEADER", value_parser = parse_auth_header, conflicts_with = "repair")]
    pub auth_header: Option<String>,

    /// Password of a share, a video or an archive (over one given in a paste)
    #[arg(long = "password", value_name = "PASS", conflicts_with = "repair")]
    pub password: Option<String>,

    /// HTTP Referer header, e.g. "https://dood.to/" or the embedding web page URL
    #[arg(long = "referer", value_name = "URL", conflicts_with = "repair")]
    pub referer: Option<String>,

    /// Proxy URL (http://, https://, socks5:// or socks5h://)
    #[arg(long = "proxy", value_name = "URL", value_parser = parse_proxy, conflicts_with = "repair")]
    pub proxy: Option<String>,

    /// Comma-separated list of proxy URLs for multi-egress rotation (e.g. "socks5://127.0.0.1:9050,socks5://127.0.0.1:9051")
    #[arg(long = "proxy-pool", value_name = "PROXIES", conflicts_with = "repair")]
    pub proxy_pool: Option<String>,

    /// Path to file containing proxy URLs (one per line, # for comments) for multi-egress rotation
    #[arg(long = "proxies-file", value_name = "FILE", value_parser = existing_file, conflicts_with = "repair")]
    pub proxies_file: Option<PathBuf>,

    /// Debrid API key: filehost links are unrestricted into direct high-speed ones, and magnet
    /// links are downloaded through the debrid service
    #[arg(long = "debrid-key", value_name = "KEY", env = "ENDO_DEBRID_KEY", hide_env_values = true)]
    pub debrid_key: Option<String>,

    /// Debrid service of --debrid-key
    #[arg(long = "debrid-provider", value_name = "PROVIDER", value_parser = debrid_providers(), ignore_case = true)]
    pub debrid_provider: Option<String>,

    /// Local IP address to connect from; repeat it to spread connections over several networks
    #[arg(long = "bind-address", value_name = "IP")]
    pub bind_address: Vec<std::net::IpAddr>,

    /// Spread connections over every network with internet access (wired, Wi-Fi, phone)
    #[arg(long = "all-networks", conflicts_with = "bind_address")]
    pub all_networks: bool,

    /// Never download magnet links and .torrent files from the BitTorrent swarm
    #[arg(long = "no-p2p")]
    pub no_p2p: bool,

    /// Seed a finished torrent until it has uploaded R times its size; the program waits for that
    /// before exiting [default: 0, no seeding]
    #[arg(long = "seed-ratio", value_name = "R", value_parser = parse_ratio)]
    pub seed_ratio: Option<f64>,

    /// Seed a finished torrent for at most this many minutes (alone: seed that long whatever the ratio)
    #[arg(long = "seed-time", value_name = "MINUTES", value_parser = clap::value_parser!(u32).range(1..))]
    pub seed_time: Option<u32>,

    /// BitTorrent listen port [default: librqbit's range]
    #[arg(long = "bt-port", value_name = "PORT", value_parser = clap::value_parser!(u16).range(1..))]
    pub bt_port: Option<u16>,

    /// Ask the router (UPnP) to forward the BitTorrent port
    #[arg(long = "upnp")]
    pub upnp: bool,

    /// Do not send image galleries (imgur, pixiv, DeviantArt, ...) to gallery-dl
    #[arg(long = "no-gallery-dl")]
    pub no_gallery_dl: bool,

    /// Unpack downloaded archives (zip, 7z, rar, tar) into a folder next to them
    #[arg(long = "extract")]
    pub extract: bool,

    /// With --extract: delete an archive once it is unpacked
    #[arg(long = "delete-archive", requires = "extract")]
    pub delete_archive: bool,

    /// Move each downloaded file into a category folder (Video, Music, Pictures, Documents,
    /// Archives, Programs) of the save directory
    #[arg(long = "sort")]
    pub sort: bool,

    /// Command run after each download, without a shell; {path}, {dir}, {name} and {url} in its
    /// arguments are replaced (also in env ENDO_PATH, ENDO_DIR, ENDO_NAME, ENDO_URL)
    #[arg(long = "exec", value_name = "CMD")]
    pub exec: Option<String>,

    /// Do not mark downloaded files as from the internet (Windows' Zone.Identifier, macOS' quarantine)
    #[arg(long = "no-mark-of-the-web")]
    pub no_mark_of_the_web: bool,

    /// VirusTotal API key: each downloaded file's SHA-256 is looked up (the file is never uploaded)
    #[arg(long = "virustotal-key", value_name = "KEY", env = "VIRUSTOTAL_API_KEY", hide_env_values = true)]
    pub virustotal_key: Option<String>,

    /// Media quality: best, 1080p, 720p, mp3, m4a, or any other yt-dlp format selector
    /// (e.g. "bestvideo[height<=480]+bestaudio"). Also sends non-file page URLs to yt-dlp.
    #[arg(long = "media-preset", value_name = "PRESET", value_parser = parse_media_preset)]
    pub media_preset: Option<MediaQualityPreset>,

    /// Browser to read cookies from for media sites
    #[arg(long = "cookies-from-browser", value_enum, value_name = "BROWSER", conflicts_with = "repair")]
    pub cookies_from_browser: Option<Browser>,

    /// Connections (parallel fragments) for yt-dlp media downloads: URLs of supported media sites,
    /// or every URL when --media-preset is given (1-32)
    #[arg(long = "concurrent-fragments", default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..=32))]
    pub concurrent_fragments: u64,

    /// Subtitle languages to save next to media files, e.g. "en" or "en,es" or "all" (as .srt
    /// when ffmpeg is at hand)
    #[arg(long = "subs", value_name = "LANGS", value_parser = parse_subtitle_langs)]
    pub subs: Option<String>,

    /// Do not write the title, artist, date and URL tags, chapters and cover art into media files
    #[arg(long = "no-embed-metadata")]
    pub no_embed_metadata: bool,

    /// Record a live stream from its start (where the site keeps it) instead of from now
    #[arg(long = "live-from-start")]
    pub live_from_start: bool,

    /// Wait for a scheduled stream or premiere to start instead of failing
    #[arg(long = "wait-for-video")]
    pub wait_for_video: bool,

    /// Download only this part of a media file, e.g. 90-150, 1:30-2:30 or 1:02:03-1:05:00;
    /// repeat it for several parts
    #[arg(long = "sections", value_name = "START-END", value_parser = parse_section)]
    pub sections: Vec<(f64, f64)>,

    /// Remove a YouTube video's sponsor, self-promotion and subscribe reminder segments, or mark
    /// SponsorBlock's segments as chapters
    #[arg(long = "sponsorblock", value_name = "MODE", value_parser = ["remove", "mark"])]
    pub sponsorblock: Option<String>,

    /// The container a media file's video and audio are merged into
    #[arg(long = "merge-format", value_name = "FORMAT", value_parser = ["mp4", "mkv", "webm"])]
    pub merge_format: Option<String>,

    /// Remux an HLS (m3u8) stream saved as MPEG-TS into an MP4 once downloaded, without
    /// re-encoding (needs ffmpeg; without it the .ts is kept)
    #[arg(long = "hls-mp4")]
    pub hls_mp4: bool,

    #[cfg_attr(not(target_os = "macos"), doc = "Install ffmpeg (the checked build yt-dlp's makers publish, about 200 MB) when media needs")]
    #[cfg_attr(target_os = "macos", doc = "Install ffmpeg (Martin Riedl's checked static build, about 70 MB) when media needs")]
    /// it and none is found. Without this or --no-install-ffmpeg, a terminal asks first
    #[arg(long = "install-ffmpeg", conflicts_with = "no_install_ffmpeg")]
    pub install_ffmpeg: bool,

    /// Never install ffmpeg, nor ask: media that needs it falls back to what works without it
    #[arg(long = "no-install-ffmpeg")]
    pub no_install_ffmpeg: bool,

    /// Only the newest N items of a channel, playlist or podcast feed
    #[arg(long = "latest", value_name = "N", value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
    pub latest: Option<usize>,

    /// Also the items of a channel, playlist, feed or cloud folder downloaded before (by default only new ones)
    #[arg(long = "all-items")]
    pub all_items: bool,

    /// For a video link that also names a playlist (watch?v=X&list=Y), download the whole playlist
    #[arg(long = "yes-playlist")]
    pub yes_playlist: bool,

    /// Google API key to list a whole Google Drive folder through the Drive API, with sizes and
    /// checksums; without it the public folder page is read, which may not show every file
    #[arg(long = "google-api-key", value_name = "KEY", env = "ENDO_GOOGLE_API_KEY", hide_env_values = true)]
    pub google_api_key: Option<String>,

    /// Show download history and exit
    #[arg(long = "history", conflicts_with_all = ["urls", "input_file", "verify"])]
    pub history: bool,

    /// Check that a downloaded (or partially downloaded) file is complete and intact
    #[arg(long = "verify", value_name = "FILE", conflicts_with = "input_file")]
    pub verify: Option<PathBuf>,

    /// With --verify: re-download missing ranges over up to -s connections (URLs from the command
    /// line, the file's resume state or history for that exact path). The repair connects
    /// directly, so it cannot be combined with --proxy, --header or cookies
    #[arg(long = "repair", requires = "verify")]
    pub repair: bool,

    /// Install the newest signed release from GitHub over this program (Windows and macOS; with
    /// --proxy if given): on Windows the GUI and browser extension next to it too, on macOS the
    /// whole app bundle; then exit
    #[arg(long = "update", conflicts_with_all = ["urls", "input_file", "verify", "repair", "history"])]
    pub update: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Browser {
    Chrome,
    Edge,
    Firefox,
    Brave,
    Opera,
    Vivaldi,
}

impl From<Browser> for BrowserCookieSource {
    fn from(browser: Browser) -> Self {
        match browser {
            Browser::Chrome => Self::Chrome,
            Browser::Edge => Self::Edge,
            Browser::Firefox => Self::Firefox,
            Browser::Brave => Self::Brave,
            Browser::Opera => Self::Opera,
            Browser::Vivaldi => Self::Vivaldi,
        }
    }
}

/// Parses "500K", "2M", "1.5MiB", "750kb/s" or plain bytes. K/M/G are binary multiples.
pub fn parse_speed(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let lower = lower.strip_suffix("/s").unwrap_or(&lower);
    let split = lower.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(lower.len());
    let (number, unit) = lower.split_at(split);
    let multiplier: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("unknown unit '{}' (use K, M or G, e.g. 500K or 2M)", other)),
    };
    let value: f64 = number.parse().map_err(|_| format!("'{}' is not a speed (e.g. 500K, 2M, 1.5MiB)", s))?;
    let bytes = value * multiplier;
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return Err(format!("'{}' is too large", s));
    }
    Ok(bytes.round() as u64)
}

/// Accepts curl-style "Authorization: VALUE" or a bare "VALUE" and returns the header value.
/// The engine can only send an Authorization header, so any other header name is rejected.
pub fn parse_auth_header(s: &str) -> Result<String, String> {
    let s = s.trim();
    let value = match s.split_once(':') {
        // A name is a single token ("Bearer abc:def" is a value that contains a colon).
        Some((name, value)) if !name.is_empty() && !name.contains(char::is_whitespace) => {
            if !name.eq_ignore_ascii_case("authorization") {
                return Err(format!(
                    "only the Authorization header is supported, not '{}' (use --load-cookies for cookies)",
                    name
                ));
            }
            value.trim()
        }
        _ => s,
    };
    if value.is_empty() {
        return Err("the Authorization header value is empty".to_string());
    }
    if value.chars().any(|c| c.is_control()) {
        return Err("the header value contains control characters".to_string());
    }
    Ok(value.to_string())
}

fn parse_proxy(s: &str) -> Result<String, String> {
    let url = url::Url::parse(s).map_err(|e| format!("invalid proxy URL '{}': {}", s, e))?;
    match url.scheme() {
        "http" | "https" | "socks5" | "socks5h" => Ok(s.to_string()),
        other => Err(format!("unsupported proxy scheme '{}' (use http, https, socks5 or socks5h)", other)),
    }
}

/// The debrid services --debrid-provider names, in any case ("Real-Debrid" as older versions
/// took it).
fn debrid_providers() -> impl clap::builder::TypedValueParser<Value = String> {
    use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser};
    PossibleValuesParser::new([
        PossibleValue::new("realdebrid").alias("real-debrid"),
        PossibleValue::new("alldebrid"),
        PossibleValue::new("torbox"),
        PossibleValue::new("premiumize"),
    ])
    .map(|name| name.to_ascii_lowercase().replace('-', ""))
}

/// A seeding ratio: a number, 0 or more.
fn parse_ratio(s: &str) -> Result<f64, String> {
    s.trim().parse::<f64>().ok().filter(|r| r.is_finite() && *r >= 0.0).ok_or_else(|| format!("'{}' is not a ratio (e.g. 1 or 2.5)", s))
}

fn parse_checksum(s: &str) -> Result<String, String> {
    hyperfetch_core::storage::validate_checksum(s)?;
    Ok(s.to_string())
}

fn existing_file(s: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(s);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("'{}' is not a readable file", s))
    }
}

/// Subtitle languages as yt-dlp's --sub-langs takes them: a comma-separated list without spaces.
fn parse_subtitle_langs(s: &str) -> Result<String, String> {
    let s = s.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return Err(format!("'{}' is not a list of languages (e.g. en,es or all)", s));
    }
    Ok(s.to_string())
}

/// A part of a media file as START-END, each a time in seconds, M:SS or H:MM:SS (seconds may
/// have decimals), START before END: (start, end) in seconds.
fn parse_section(s: &str) -> Result<(f64, f64), String> {
    let invalid = || format!("'{}' is not a part of a video (e.g. 90-150, 1:30-2:30 or 1:02:03-1:05:00)", s);
    let seconds = |time: &str| -> Option<f64> {
        let fields: Vec<&str> = time.trim().split(':').collect();
        if fields.len() > 3 {
            return None;
        }
        let mut total = 0.0;
        for (i, field) in fields.iter().enumerate() {
            // Digits only: no sign, exponent, "inf" or "NaN".
            if !field.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                return None;
            }
            let value: f64 = field.parse().ok()?;
            // Minutes and seconds after the first field stay under 60.
            if i > 0 && value >= 60.0 {
                return None;
            }
            total = total * 60.0 + value;
        }
        Some(total)
    };
    let (start, end) = s.split_once('-').ok_or_else(invalid)?;
    match (seconds(start), seconds(end)) {
        (Some(start), Some(end)) if start < end => Ok((start, end)),
        _ => Err(invalid()),
    }
}

pub fn parse_media_preset(s: &str) -> Result<MediaQualityPreset, String> {
    let s = s.trim();
    Ok(match s.to_ascii_lowercase().as_str() {
        "" => return Err("the media preset is empty".to_string()),
        "best" => MediaQualityPreset::BestVideoAudio,
        "1080p" | "1080" | "fhd" => MediaQualityPreset::Fhd1080p,
        "720p" | "720" | "hd" => MediaQualityPreset::Hd720p,
        "mp3" => MediaQualityPreset::AudioMp3,
        "m4a" | "aac" => MediaQualityPreset::AudioM4a,
        _ => MediaQualityPreset::Custom(s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_definition_is_valid() {
        Args::command().debug_assert();
    }

    #[test]
    fn speed_units() {
        assert_eq!(parse_speed("1048576"), Ok(1_048_576));
        assert_eq!(parse_speed("500K"), Ok(512_000));
        assert_eq!(parse_speed("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_speed("1.5MiB"), Ok(1_572_864));
        assert_eq!(parse_speed("750kb/s"), Ok(768_000));
        assert_eq!(parse_speed("1g"), Ok(1024 * 1024 * 1024));
        assert_eq!(parse_speed("0"), Ok(0));
        assert!(parse_speed("fast").is_err());
        assert!(parse_speed("5T").is_err());
        assert!(parse_speed("").is_err());
        assert!(parse_speed("1.2.3M").is_err());
    }

    #[test]
    fn auth_header_forms() {
        assert_eq!(parse_auth_header("Authorization: Bearer x").as_deref(), Ok("Bearer x"));
        assert_eq!(parse_auth_header("authorization:Basic abc=").as_deref(), Ok("Basic abc="));
        assert_eq!(parse_auth_header("Bearer x").as_deref(), Ok("Bearer x"));
        assert_eq!(parse_auth_header("Bearer a:b").as_deref(), Ok("Bearer a:b"));
        assert!(parse_auth_header("Cookie: a=b").unwrap_err().contains("only the Authorization"));
        assert!(parse_auth_header("Authorization:   ").is_err());
        assert!(parse_auth_header("Bearer x\r\nX-Evil: 1").is_err());
    }

    #[test]
    fn media_presets_and_custom_formats() {
        assert_eq!(parse_media_preset("1080P"), Ok(MediaQualityPreset::Fhd1080p));
        assert_eq!(parse_media_preset("aac"), Ok(MediaQualityPreset::AudioM4a));
        assert_eq!(
            parse_media_preset("bestaudio[ext=m4a]"),
            Ok(MediaQualityPreset::Custom("bestaudio[ext=m4a]".to_string()))
        );
        assert!(parse_media_preset(" ").is_err());
    }

    #[test]
    fn flag_validation() {
        let parse = |args: &[&str]| Args::try_parse_from(std::iter::once("cli").chain(args.iter().copied()));
        assert!(parse(&["--repair"]).is_err());
        assert!(parse(&["--verify", "f", "--repair"]).is_ok());
        assert!(parse(&["-s", "0", "u"]).is_err());
        assert!(parse(&["-s", "65", "u"]).is_err());
        assert!(parse(&["-c", "0", "u"]).is_err());
        assert!(parse(&["--cookies-from-browser", "netscape", "u"]).is_err());
        assert!(parse(&["--proxy", "ftp://x", "u"]).is_err());
        assert!(parse(&["--checksum", "crc32:abcd", "u"]).is_err());
        assert!(parse(&["--history", "u"]).is_err());
        assert!(parse(&["--update"]).unwrap().update);
        assert!(parse(&["--update", "--proxy", "socks5h://127.0.0.1:9050"]).is_ok());
        for other in [&["u"][..], &["-i", "list.txt"], &["--verify", "f"], &["--verify", "f", "--repair"], &["--history"]] {
            assert!(parse(&[&["--update"][..], other].concat()).is_err(), "{other:?}");
        }
        // The repair cannot honor these, so it must not silently connect without them.
        assert!(parse(&["--verify", "f", "--repair", "--proxy", "socks5h://127.0.0.1:9050"]).is_err());
        assert!(parse(&["--verify", "f", "--repair", "--header", "Bearer t"]).is_err());
        assert!(parse(&["--verify", "f", "--repair", "--cookies-from-browser", "firefox"]).is_err());
        let args = parse(&["-vv", "--max-speed", "2M", "--header", "Authorization: Bearer t", "u"]).unwrap();
        assert_eq!((args.verbose, args.max_speed, args.auth_header.as_deref()), (2, Some(2 << 20), Some("Bearer t")));
        assert_eq!(parse(&["--password", "p w", "u"]).unwrap().password.as_deref(), Some("p w"));
        assert!(parse(&["--verify", "f", "--repair", "--password", "p"]).is_err());
    }

    #[test]
    fn listing_and_media_flags() {
        let parse = |args: &[&str]| Args::try_parse_from(std::iter::once("cli").chain(args.iter().copied()));
        let args = parse(&["u"]).unwrap();
        assert!(!args.yes_playlist && !args.all_items && !args.install_ffmpeg && !args.no_install_ffmpeg && !args.no_embed_metadata);
        assert_eq!((args.latest, args.subs), (None, None));
        assert!(parse(&["--install-ffmpeg", "u"]).unwrap().install_ffmpeg);
        assert!(parse(&["--install-ffmpeg", "--no-install-ffmpeg", "u"]).is_err());
        let args = parse(&["--latest", "5", "--subs", " en,es ", "--yes-playlist", "--google-api-key", "k", "u"]).unwrap();
        assert_eq!((args.latest, args.subs.as_deref(), args.google_api_key.as_deref()), (Some(5), Some("en,es"), Some("k")));
        assert!(parse(&["--latest", "0", "u"]).is_err());
        assert!(parse(&["--subs", " ", "u"]).is_err());
        assert!(parse(&["--subs", "en, es", "u"]).is_err());
    }

    #[test]
    fn sections_sponsorblock_and_merge_format() {
        assert_eq!(parse_section("90-150"), Ok((90.0, 150.0)));
        assert_eq!(parse_section("1:30-2:30"), Ok((90.0, 150.0)));
        assert_eq!(parse_section("1:02:03-1:05:00"), Ok((3723.0, 3900.0)));
        assert_eq!(parse_section(" 0:05.5 - 75 "), Ok((5.5, 75.0)));
        assert_eq!(parse_section("0-100:00"), Ok((0.0, 6000.0)), "the first field may be any size");
        for bad in ["150-90", "90-90", "90", "-5-10", "a-b", "1:60-2:00", "1:2:3:4-5", "1e3-2e3", "inf-NaN", "+1-2", "90-", "1::2-3", ""] {
            assert!(parse_section(bad).is_err(), "{bad}");
        }

        let parse = |args: &[&str]| Args::try_parse_from(std::iter::once("cli").chain(args.iter().copied()));
        let args = parse(&["u"]).unwrap();
        assert!(args.sections.is_empty() && args.sponsorblock.is_none() && args.merge_format.is_none());
        let args = parse(&["--sections", "90-150", "--sections", "1:02:03-1:05:00", "--sponsorblock", "mark", "--merge-format", "webm", "u"]).unwrap();
        assert_eq!(args.sections, [(90.0, 150.0), (3723.0, 3900.0)]);
        assert_eq!((args.sponsorblock.as_deref(), args.merge_format.as_deref()), (Some("mark"), Some("webm")));
        assert!(parse(&["--sponsorblock", "skip", "u"]).is_err());
        assert!(parse(&["--merge-format", "avi", "u"]).is_err());
        assert!(parse(&["--sections", "2:00-1:00", "u"]).is_err());
    }

    #[test]
    fn network_torrent_and_post_flags() {
        let parse = |args: &[&str]| Args::try_parse_from(std::iter::once("cli").chain(args.iter().copied()));
        let args = parse(&["--debrid-provider", "real-debrid", "--seed-ratio", "1.5", "--bind-address", "10.0.0.2", "--bind-address", "::1", "u"]).unwrap();
        assert_eq!((args.debrid_provider.as_deref(), args.seed_ratio, args.bind_address.len()), (Some("realdebrid"), Some(1.5), 2));
        for (name, provider) in [("Real-Debrid", "realdebrid"), ("AllDebrid", "alldebrid"), ("TORBOX", "torbox")] {
            assert_eq!(parse(&["--debrid-provider", name, "u"]).unwrap().debrid_provider.as_deref(), Some(provider));
        }
        assert!(parse(&["--debrid-provider", "megadebrid", "u"]).is_err());
        assert!(parse(&["--seed-ratio", "-1", "u"]).is_err());
        assert!(parse(&["--seed-time", "0", "u"]).is_err());
        assert!(parse(&["--delete-archive", "u"]).is_err(), "only with --extract");
        assert!(parse(&["--all-networks", "--bind-address", "10.0.0.2", "u"]).is_err());
        assert!(parse(&["--bind-address", "not-an-ip", "u"]).is_err());
    }

    #[test]
    fn batch_and_disk_defaults() {
        let parse = |args: &[&str]| Args::try_parse_from(std::iter::once("cli").chain(args.iter().copied()));
        let args = parse(&["u"]).unwrap();
        assert_eq!((args.jobs, args.fsync, args.max_connections_per_host), (4, false, 64));
        let args = parse(&["-j", "1", "--fsync", "--max-connections-per-host", "0", "u"]).unwrap();
        assert_eq!((args.jobs, args.fsync, args.max_connections_per_host), (1, true, 0));
        assert!(parse(&["--max-connections-per-host", "-1", "u"]).is_err());
    }
}
