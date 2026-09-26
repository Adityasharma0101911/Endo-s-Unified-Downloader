//! Command-line arguments and the parsers that validate them.

use std::path::PathBuf;

use clap::{ArgAction, Parser, ValueEnum};
use hyperfetch_core::media::{BrowserCookieSource, MediaQualityPreset};

#[derive(Parser, Debug)]
#[command(
    name = "Endos-Unified-Downloader-CLI",
    version,
    about = "High-speed multi-connection download accelerator",
    after_help = "Exit status: 0 all downloads finished, 1 a download failed, 2 usage error or \
                  verification failed, 130 interrupted (Ctrl+C), 143 terminated (SIGTERM).\n\
                  Press Ctrl+C once to stop and save resume state; press it again to quit immediately."
)]
pub struct Args {
    /// Mirrors of ONE file (all must serve identical bytes), a magnet link with web seeds, or a
    /// local/remote .metalink, .meta4 or .torrent. Without URLs and without -i an interactive
    /// prompt starts.
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

    /// Downloads to run at the same time in batch mode (1-32)
    #[arg(short = 'j', long = "max-concurrent-downloads", default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..=32))]
    pub jobs: u64,

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

    /// Speed limit per download in bytes/s, e.g. 500K, 2M, 1.5MiB (K/M/G are powers of 1024; 0 = unlimited)
    #[arg(long = "max-speed", value_name = "RATE", value_parser = parse_speed)]
    pub max_speed: Option<u64>,

    /// Failed attempts per chunk before a download gives up (attempts that made progress are free)
    #[arg(long = "max-retries", default_value_t = 8, value_parser = clap::value_parser!(u32).range(0..=1000))]
    pub max_retries: u32,

    /// Seconds without data before a connection is considered stalled and retried (1-3600)
    #[arg(long = "stall-timeout", value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub stall_timeout: u64,

    /// Expected checksum (sha256:HEX, md5:HEX, blake3:HEX or bare hex); single download or --verify
    #[arg(long = "checksum", value_parser = parse_checksum)]
    pub checksum: Option<String>,

    /// Netscape cookies.txt file
    #[arg(long = "load-cookies", value_name = "FILE", value_parser = existing_file)]
    pub load_cookies: Option<PathBuf>,

    /// Authorization header, e.g. "Authorization: Bearer TOKEN" or just "Bearer TOKEN"
    #[arg(long = "header", value_name = "HEADER", value_parser = parse_auth_header)]
    pub auth_header: Option<String>,

    /// Proxy URL (http://, https://, socks5:// or socks5h://)
    #[arg(long = "proxy", value_name = "URL", value_parser = parse_proxy)]
    pub proxy: Option<String>,

    /// Media quality: best, 1080p, 720p, mp3, m4a, or any other yt-dlp format selector
    /// (e.g. "bestvideo[height<=480]+bestaudio"). Also sends non-file page URLs to yt-dlp.
    #[arg(long = "media-preset", value_name = "PRESET", value_parser = parse_media_preset)]
    pub media_preset: Option<MediaQualityPreset>,

    /// Browser to read cookies from for media sites
    #[arg(long = "cookies-from-browser", value_enum, value_name = "BROWSER")]
    pub cookies_from_browser: Option<Browser>,

    /// Connections (parallel fragments) for yt-dlp media downloads: URLs of supported media sites,
    /// or every URL when --media-preset is given (1-32)
    #[arg(long = "concurrent-fragments", default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..=32))]
    pub concurrent_fragments: u64,

    /// Show download history and exit
    #[arg(long = "history", conflicts_with_all = ["urls", "input_file", "verify"])]
    pub history: bool,

    /// Check that a downloaded (or partially downloaded) file is complete and intact
    #[arg(long = "verify", value_name = "FILE", conflicts_with = "input_file")]
    pub verify: Option<PathBuf>,

    /// With --verify: re-download missing ranges (URLs from the command line, the file's resume
    /// state or history for that exact path)
    #[arg(long = "repair", requires = "verify")]
    pub repair: bool,
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
        let args = parse(&["-vv", "--max-speed", "2M", "--header", "Authorization: Bearer t", "u"]).unwrap();
        assert_eq!((args.verbose, args.max_speed, args.auth_header.as_deref()), (2, Some(2 << 20), Some("Bearer t")));
    }
}
