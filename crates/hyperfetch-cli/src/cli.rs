//! Command-line arguments, the config file that gives them defaults, and the parsers that
//! validate them.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::parser::ValueSource;
use clap::{Arg, ArgAction, ArgMatches, CommandFactory, FromArgMatches, Parser, ValueEnum};
use hyperfetch_core::engine::DownloadOptions;
use hyperfetch_core::media::{BrowserCookieSource, MediaQualityPreset};
use hyperfetch_core::{check_name_pattern, check_pattern, parse_part_ids, AudioSpec, LiveStart, VideoSpec};

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
    #[arg(short = 'o', long = "output", alias = "save-name", value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Directory to save downloads in (created if missing) [default: current directory]
    #[arg(short = 'd', long = "dir", alias = "save-dir", value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// No progress bars or status lines (errors and warnings still go to stderr)
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// More log output (-v info, -vv debug, -vvv trace); RUST_LOG overrides it
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
    pub verbose: u8,

    /// Speed limit in bytes/s for all running downloads (-j) together, e.g. 500K, 2M, 1.5MiB (K/M/G
    /// are powers of 1024; 0 = unlimited)
    #[arg(long = "max-speed", value_name = "RATE", value_parser = parse_bytes)]
    pub max_speed: Option<u64>,

    /// Failed attempts per chunk before a download gives up (attempts that made progress are free)
    #[arg(long = "max-retries", default_value_t = 8, value_parser = clap::value_parser!(u32).range(0..=1000))]
    pub max_retries: u32,

    /// Seconds without data before a connection is considered stalled and retried (1-3600)
    #[arg(long = "stall-timeout", value_name = "SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub stall_timeout: u64,

    /// Skip a download whose file is there already
    #[arg(long = "no-clobber", conflicts_with = "only_if_newer")]
    pub no_clobber: bool,

    /// Download a file that is there already again only when its server has a newer one (implies
    /// --remote-time)
    #[arg(short = 'N', long = "only-if-newer")]
    pub only_if_newer: bool,

    /// Give each finished file the date its server gives it
    #[arg(long = "remote-time")]
    pub remote_time: bool,

    /// Fail a download larger than this, e.g. 10M or 2G (K/M/G are powers of 1024)
    #[arg(long = "max-filesize", value_name = "SIZE", value_parser = parse_bytes)]
    pub max_filesize: Option<u64>,

    /// Do not check a download against the checksum its site publishes
    #[arg(long = "no-remote-checksum")]
    pub no_remote_checksum: bool,

    /// Adapt each download's connections as it goes: one more while that makes it faster (the
    /// default; this undoes a --no-adaptive from the config file)
    #[arg(long = "adaptive", overrides_with = "no_adaptive")]
    pub adaptive: bool,

    /// Keep the connections each download starts with
    #[arg(long = "no-adaptive", overrides_with = "adaptive")]
    pub no_adaptive: bool,

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

    /// Request header "Name: value", repeat it for several; a bare value ("Bearer TOKEN") is an
    /// Authorization header. Sent only to the hosts of the links given, never to the mirrors or
    /// files a .metalink, .torrent or folder lists
    #[arg(long = "header", value_name = "HEADER", value_parser = parse_header, conflicts_with = "repair")]
    pub headers: Vec<(String, String)>,

    /// Basic sign-in sent only to the hosts of the links given; without :PASS a terminal asks for
    /// the password (an Authorization --header wins over it)
    #[arg(short = 'u', long = "user", value_name = "USER[:PASS]", conflicts_with = "repair")]
    pub user: Option<String>,

    /// Cookies, e.g. "a=b; c=d", sent only to the hosts of the links given
    #[arg(long = "cookie", value_name = "STRING", conflicts_with = "repair")]
    pub cookie: Option<String>,

    /// User-Agent sent in place of the browser one
    #[arg(short = 'U', long = "user-agent", value_name = "UA")]
    pub user_agent: Option<String>,

    /// Do not check TLS certificates: anyone on the way can then read and change the download
    #[arg(short = 'k', long = "insecure")]
    pub insecure: bool,

    /// Connect over IPv4 only
    #[arg(short = '4', long = "ipv4", conflicts_with = "ipv6")]
    pub ipv4: bool,

    /// Connect over IPv6 only
    #[arg(short = '6', long = "ipv6")]
    pub ipv6: bool,

    /// Seconds opening a connection may take: DNS, TCP and TLS (1-3600)
    #[arg(long = "connect-timeout", value_name = "SECS", default_value_t = DownloadOptions::default().connect_timeout_secs, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub connect_timeout: u64,

    /// Connect directly, ignoring the proxy variables of the environment
    #[arg(long = "no-proxy", conflicts_with_all = ["proxy", "proxy_pool", "proxies_file"])]
    pub no_proxy: bool,

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

    /// Keep each download's connections on one address of its server, instead of spreading them
    /// over the several a server's name often has
    #[arg(long = "no-spread-addresses")]
    pub no_spread_addresses: bool,

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
    /// when ffmpeg is at hand); for an HLS or DASH link, its subtitles tracks in these languages
    /// (all of them unless given, "none" for none)
    #[arg(long = "subs", alias = "select-subtitle", value_name = "LANGS", value_parser = parse_subtitle_langs)]
    pub subs: Option<String>,

    /// Do not write the title, artist, date and URL tags, chapters and cover art into media files
    #[arg(long = "no-embed-metadata")]
    pub no_embed_metadata: bool,

    /// Record a live stream from its start (where the site keeps it) instead of from now
    #[arg(long = "live-from-start")]
    pub live_from_start: bool,

    /// Start an HLS or DASH recording further back: a time ago (30m, 1h30m, 90s, 01:00:00) or a
    /// clock time (2026-10-10T21:00, "2026-10-10 21:00:00+08:00"; this computer's time zone
    /// unless one is given). Media sites take only --live-from-start
    #[arg(long = "live-from", value_name = "TIME", value_parser = parse_live_from, conflicts_with = "live_from_start")]
    pub live_from: Option<String>,

    /// End a live recording after N seconds of it
    #[arg(long = "record-seconds", alias = "live-record-limit", value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    pub record_seconds: Option<u64>,

    /// End an HLS or DASH recording, or an MPEG-TS stream's, once nothing new has come for SECS
    /// seconds
    #[arg(long = "live-idle-timeout", value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    pub live_idle_timeout: Option<u64>,

    /// Wait for a scheduled stream or premiere to start instead of failing
    #[arg(long = "wait-for-video")]
    pub wait_for_video: bool,

    /// Download only this part of a media file, e.g. 90-150, 1:30-2:30 or 1:02:03-1:05:00;
    /// repeat it for several parts. An HLS or DASH stream keeps the whole segments that overlap it
    #[arg(long = "sections", alias = "custom-range", value_name = "START-END", value_parser = parse_section)]
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

    /// The video of an HLS or DASH stream: comma-separated terms that must all hold: best, worst
    /// or none; 1080p (at most that many lines); avc, hevc, av1 or vp9; sdr, hdr or dv; 30fps
    /// (at most that rate), e.g. "1080p,hevc" [default: best]
    #[arg(long = "video", alias = "select-video", value_name = "SPEC", value_parser = parse_video_spec)]
    pub video: Option<String>,

    /// The audio of an HLS or DASH stream: languages (each its best track, all kept, in that
    /// order), best, all or none; and aac, ac3, eac3, opus or flac, 6ch (at least that many
    /// channels), e.g. "en,ja,eac3" [default: best, the default track's language]
    #[arg(long = "audio", alias = "select-audio", value_name = "SPEC", value_parser = parse_audio_spec)]
    pub audio: Option<String>,

    /// The tracks of an HLS or DASH stream to take, by the ids --list-tracks gives, e.g.
    /// "v0,a1,s0"; they win over --video and --audio
    #[arg(long = "tracks", value_name = "IDS", value_delimiter = ',', value_parser = parse_track_id)]
    pub tracks: Vec<String>,

    /// Print the tracks and parts of each HLS or DASH link, or the channels of a channel list,
    /// and download nothing
    #[arg(long = "list-tracks", alias = "vod-list-parts", conflicts_with_all = ["input_file", "dry_run", "spider", "preview", "stdout", "history", "update", "verify"])]
    pub list_tracks: bool,

    /// Leave these parts of an HLS or DASH stream out, by the ids --list-tracks gives, e.g. 0,2-4
    #[arg(long = "drop-parts", alias = "vod-drop-parts", value_name = "IDS", value_parser = parse_drop_parts)]
    pub drop_parts: Option<String>,

    /// Leave out the segments of an HLS or DASH stream whose links PATTERN matches, in any case:
    /// * stands for any run of characters, else it is found anywhere in the link (e.g. "/ads/");
    /// repeat it for several
    #[arg(long = "ad-pattern", alias = "ad-keyword", value_name = "PATTERN", value_parser = parse_pattern)]
    pub ad_patterns: Vec<String>,

    /// Name HLS, DASH and channel downloads by PATTERN, e.g. "{name} {height}p": {name}, {height},
    /// {width}, {fps}, {codec}, {range}, {audio}, {bitrate}, {channel}, {date}, {time}
    #[arg(long = "name-pattern", alias = "save-pattern", value_name = "PATTERN", value_parser = parse_name_pattern)]
    pub name_pattern: Option<String>,

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

    /// Also the items of a channel, playlist, feed or cloud folder downloaded before (by default only new ones);
    /// every channel of a channel list
    #[arg(long = "all-items")]
    pub all_items: bool,

    /// Record the channels of a channel list (an M3U of stream links) whose name or group PATTERN
    /// matches, in any case (* stands for any run of characters); repeat it for several
    #[arg(long = "channel", value_name = "PATTERN", value_parser = parse_pattern)]
    pub channels: Vec<String>,

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

    /// Print what this program runs with, then exit: its version, the system, the data folder, the
    /// config file, yt-dlp, ffmpeg, ffprobe and gallery-dl (where and which version), the proxy
    /// variables and the free space where downloads go. Nothing goes online
    #[arg(long = "doctor", conflicts_with_all = ["urls", "input_file", "verify", "repair", "history", "update"])]
    pub doctor: bool,

    /// Default options from FILE: options as on the command line, any number a line ("double
    /// quotes" around a value with spaces; # starts a comment line). The command line's win; the
    /// file's --header, --ad-pattern, --channel, --sections and --bind-address come before its own
    /// [default: cli.conf in the data folder, when it is there]
    #[arg(long = "config", value_name = "FILE", value_parser = existing_file)]
    pub config: Option<PathBuf>,

    /// Read no config file
    #[arg(long = "no-config", conflicts_with = "config")]
    pub no_config: bool,

    /// Wait until TIME, then run: HH:MM (the next time the clock shows it) or "YYYY-MM-DD HH:MM".
    /// Ctrl+C while waiting ends the program
    #[arg(long = "start-at", value_name = "TIME", value_parser = parse_start_at, conflicts_with_all = ["history", "update", "verify", "doctor"])]
    pub start_at: Option<String>,

    /// The config file the options were read from, if any (see [`load`]).
    #[arg(skip)]
    pub config_in_use: Option<PathBuf>,

    /// One JSON object per finished item on stdout: {"status": "ok", "failed", "skipped" or
    /// "up-to-date", "url", "path", "size", "checksum", "checked_against", "error"}; with
    /// --list-tracks one per link, with --doctor one. What is printed otherwise goes to stderr
    #[arg(long = "json", conflicts_with_all = ["history", "update", "verify", "preview", "stdout"])]
    pub json: bool,

    /// Probe only: print each link's file name, size, type, whether it resumes, its validators
    /// and the checksum its site publishes. Nothing is written; a bad link exits with 1
    #[arg(long = "spider", conflicts_with_all = ["dry_run", "preview", "stdout", "history", "update", "verify"])]
    pub spider: bool,

    /// Print the request and response headers of each link's probe (> and < lines, on stderr),
    /// with sign-ins and cookies masked
    #[arg(short = 'S', long = "server-response")]
    pub server_response: bool,

    /// List the files in a remote ZIP (name, size, total) without downloading it
    #[arg(long = "preview", conflicts_with_all = ["dry_run", "stdout", "input_file", "history", "update", "verify"])]
    pub preview: bool,

    /// Print what the run would download (name, size if known, path), with folders, playlists,
    /// channels, feeds, metalinks and torrents expanded; nothing is written
    #[arg(long = "dry-run", alias = "skip-download", conflicts_with_all = ["stdout", "history", "update", "verify"])]
    pub dry_run: bool,

    /// Write one direct file link's bytes to stdout as they arrive (progress and messages go to
    /// stderr)
    #[arg(long = "stdout", conflicts_with_all = ["input_file", "output", "checksum", "history", "update", "verify"])]
    pub stdout: bool,

    /// Append what is printed (warnings, results; not progress bars) to FILE too
    #[arg(long = "logfile", value_name = "FILE")]
    pub logfile: Option<PathBuf>,

    /// Print a completion script for this shell, then exit
    #[arg(long = "completions", value_name = "SHELL", value_enum)]
    pub completions: Option<clap_complete::Shell>,

    /// Subcommand (concat, merge, mux, doctor)
    #[command(subcommand)]
    pub command: Option<SubCommand>,

    /// Decryption key(s) in KID:KEY or KEY format (HEX or Base64)
    #[arg(long = "key", value_name = "KEY")]
    pub keys: Vec<String>,

    /// File containing KID:KEY lines
    #[arg(long = "key-text-file", value_name = "FILE")]
    pub key_text_file: Option<PathBuf>,

    /// Decryption engine program (MP4DECRYPT | SHAKA_PACKAGER | FFMPEG)
    #[arg(long = "decryption-engine", value_name = "ENGINE")]
    pub decryption_engine: Option<String>,

    /// Path to MP4 decryption binary
    #[arg(long = "decryption-binary-path", value_name = "PATH")]
    pub decryption_binary_path: Option<PathBuf>,

    /// Custom HLS encryption method (AES_128 | AES_128_ECB | CENC | CHACHA20 | SAMPLE_AES)
    #[arg(long = "custom-hls-method", value_name = "METHOD")]
    pub custom_hls_method: Option<String>,

    /// Custom HLS decryption key (FILE | HEX | BASE64)
    #[arg(long = "custom-hls-key", value_name = "KEY")]
    pub custom_hls_key: Option<String>,

    /// Custom HLS decryption IV (FILE | HEX | BASE64)
    #[arg(long = "custom-hls-iv", value_name = "IV")]
    pub custom_hls_iv: Option<String>,

    /// Custom HLS scope (ALL | VIDEO | AUDIO)
    #[arg(long = "custom-hls-scope", value_name = "SCOPE")]
    pub custom_hls_scope: Option<String>,
}

impl Args {
    /// The Authorization header of --header (or -u), if any.
    pub fn auth_header(&self) -> Option<String> {
        self.headers.iter().rev().find(|(name, _)| name == AUTHORIZATION).map(|(_, value)| value.clone())
    }

    /// The other headers of --header.
    pub fn extra_headers(&self) -> Vec<(String, String)> {
        self.headers.iter().filter(|(name, _)| name != AUTHORIZATION).cloned().collect()
    }
}

const AUTHORIZATION: &str = "Authorization";

/// The Basic Authorization value of `-u USER[:PASS]`, the password asked of `ask` when it is
/// missing; without `ask` (no terminal) that is an error.
pub fn basic_auth(user: &str, ask: Option<fn(&str) -> Option<String>>) -> Result<String, String> {
    use base64::Engine as _;
    let pair = match user.contains(':') {
        true => user.to_string(),
        false => {
            let missing = || format!("-u {user} has no password: give it as -u {user}:PASS, or run at a terminal to be asked for it");
            format!("{user}:{}", ask.and_then(|ask| ask(user)).ok_or_else(missing)?)
        }
    };
    Ok(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(pair)))
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

/// Parses a speed or size: "500K", "2M", "1.5MiB", "750kb/s" or plain bytes. K/M/G are binary
/// multiples.
pub fn parse_bytes(s: &str) -> Result<u64, String> {
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
    let value: f64 = number.parse().map_err(|_| format!("'{}' is not a number of bytes (e.g. 500K, 2M, 1.5MiB)", s))?;
    let bytes = value * multiplier;
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return Err(format!("'{}' is too large", s));
    }
    Ok(bytes.round() as u64)
}

/// Accepts curl-style "Name: VALUE", or a bare "VALUE" for Authorization, as (name, value); the
/// Authorization header is named "Authorization" whatever its case. The headers a download sets
/// itself, or that would change what the server sends, are refused, as `run()` refuses them.
pub fn parse_header(s: &str) -> Result<(String, String), String> {
    const OWN: [&str; 11] = [
        "host",
        "range",
        "content-length",
        "transfer-encoding",
        "connection",
        "accept-encoding",
        "te",
        "upgrade",
        "expect",
        "keep-alive",
        "proxy-connection",
    ];
    let s = s.trim();
    let (name, value) = match s.split_once(':') {
        // A name is a single token ("Bearer abc:def" is a value that contains a colon).
        Some((name, value)) if !name.is_empty() && !name.contains(char::is_whitespace) => (name, value.trim()),
        _ => (AUTHORIZATION, s),
    };
    let name = if name.eq_ignore_ascii_case(AUTHORIZATION) { AUTHORIZATION } else { name };
    if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
        return Err(format!("'{}' is not a header name", name));
    }
    if OWN.iter().any(|own| name.eq_ignore_ascii_case(own)) || name.get(..3).is_some_and(|start| start.eq_ignore_ascii_case("if-")) {
        return Err(format!("the download sets the {} header itself, or it would change what the server sends", name));
    }
    if value.is_empty() {
        return Err(format!("the {} header value is empty", name));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err("the header value contains control characters".to_string());
    }
    Ok((name.to_string(), value.to_string()))
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

fn parse_video_spec(s: &str) -> Result<String, String> {
    VideoSpec::parse(s)?;
    Ok(s.trim().to_string())
}

fn parse_audio_spec(s: &str) -> Result<String, String> {
    AudioSpec::parse(s)?;
    Ok(s.trim().to_string())
}

/// A track id as --list-tracks gives it: its kind's letter (v, a or s) and its number.
fn parse_track_id(s: &str) -> Result<String, String> {
    let id = s.trim().to_ascii_lowercase();
    let shaped = id.len() > 1 && id.starts_with(['v', 'a', 's']) && id[1..].bytes().all(|b| b.is_ascii_digit());
    shaped.then_some(id).ok_or_else(|| format!("'{}' is not a track id (v0, a1, s0, as --list-tracks lists them)", s))
}

/// Part ids as `parse_part_ids` takes them, kept as given (the engine gets them from [`part_ids`]).
fn parse_drop_parts(s: &str) -> Result<String, String> {
    match parse_part_ids(s)?.is_empty() {
        true => Err(format!("'{}' names no part (e.g. 0,2-4)", s)),
        false => Ok(s.to_string()),
    }
}

/// The part ids of --drop-parts, which clap has checked.
pub fn part_ids(drop_parts: Option<&str>) -> Vec<usize> {
    drop_parts.and_then(|ids| parse_part_ids(ids).ok()).unwrap_or_default()
}

fn parse_pattern(s: &str) -> Result<String, String> {
    check_pattern(s)?;
    Ok(s.to_string())
}

fn parse_name_pattern(s: &str) -> Result<String, String> {
    check_name_pattern(s)?;
    Ok(s.to_string())
}

fn parse_live_from(s: &str) -> Result<String, String> {
    LiveStart::parse(s)?;
    Ok(s.trim().to_string())
}

fn parse_start_at(s: &str) -> Result<String, String> {
    start_time(s, now())?;
    Ok(s.trim().to_string())
}

/// Seconds since 1970, now.
pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// When a run given `--start-at TIME` starts, in seconds since 1970, `now` being now: at `HH:MM`
/// the next time this computer's clock shows it (today, else tomorrow), at `YYYY-MM-DD HH:MM`
/// (seconds, a `T` and a time zone may follow, as `LiveStart::parse` takes them) then, which may
/// not be past.
pub fn start_time(text: &str, now: f64) -> Result<f64, String> {
    let text = text.trim();
    let bad = || format!("'{}' is not a time to start at (e.g. 21:30 or \"2026-10-10 21:30\")", text);
    let at = |text: &str| match LiveStart::parse(text) {
        Ok(LiveStart::At(at)) => Ok(at),
        _ => Err(bad()),
    };
    let clock = text.split_once(':').is_some_and(|(h, m)| [h, m].iter().all(|n| (1..=2).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit())));
    if clock {
        let day = |epoch: f64| hyperfetch_core::stream::clock_text(epoch)[..10].to_string();
        let today = at(&format!("{} {}", day(now), text))?;
        if today > now {
            return Ok(today);
        }
        // Tomorrow's date from today's noon: a day of 23 or 25 hours (a clock change) leaves it so.
        let noon = at(&format!("{} 12:00", day(now)))?;
        return at(&format!("{} {}", day(noon + 86_400.0), text));
    }
    // Not a time ago, which `LiveStart::parse` also takes.
    if text.get(4..5) != Some("-") {
        return Err(bad());
    }
    let start = at(text)?;
    match start > now {
        true => Ok(start),
        false => Err(format!("{} has passed: give a time still to come", text)),
    }
}

/// Options a config file may not hold: the run's links and inputs, the config file's own, the
/// actions that are not downloads and what makes a run wait, list or probe in place of one.
const NOT_IN_CONFIG: [&str; 17] = [
    "urls", "input_file", "config", "no_config", "history", "update", "verify", "repair", "doctor", "completions", "start_at", "list_tracks",
    "json", "dry_run", "spider", "preview", "stdout",
];

/// Options that do the same job another way, so either one on the command line leaves the other
/// out of a config file: a proxy and a pool of them, a cookie string and cookies from a file or a
/// browser.
const ONE_OF: [(&str, &str); 4] = [("proxy", "proxy_pool"), ("proxy", "proxies_file"), ("cookie", "load_cookies"), ("cookie", "cookies_from_browser")];

/// The run's options: the command line `argv` after the options of a config file (see
/// [`config_args`]): `--config FILE`, else `default` when that file is there; none with
/// `--no-config` or `--completions`. A command line clap refuses (or `--help`) exits as
/// `Args::parse` does; Err is what to print about a config file that is refused.
pub fn load(argv: Vec<OsString>, default: Option<PathBuf>) -> Result<Args, String> {
    let (given, missing) = match Args::command().try_get_matches_from(&argv) {
        Ok(given) => (given, None),
        // What an option needs (--delete-archive, --extract) may be in the config file: the merged parse checks it.
        Err(e) if e.kind() == ErrorKind::MissingRequiredArgument => {
            (Args::command().ignore_errors(true).try_get_matches_from(&argv).unwrap_or_else(|e| e.exit()), Some(e))
        }
        Err(e) => e.exit(),
    };
    let path = match given.get_one::<PathBuf>("config") {
        Some(path) => Some(path.clone()),
        None if given.get_flag("no_config") || given.contains_id("completions") => None,
        None => default.filter(|path| path.is_file()),
    };
    let Some(path) = path else {
        if let Some(e) = missing {
            e.exit();
        }
        return Args::from_arg_matches(&given).map_err(|e| e.to_string());
    };
    let refused = |e: String| format!("error: the config file {}: {}", path.display(), e);
    let bytes = std::fs::read(&path).map_err(|e| refused(e.to_string()))?;
    let text = hyperfetch_core::ingest::decode_text(&bytes).map_err(refused)?;
    let options = config_args(&text, &given).map_err(refused)?;
    // Before the command line's own, which then win.
    let mut merged = argv;
    let at = merged.len().min(1);
    merged.splice(at..at, options.into_iter().map(OsString::from));
    let matches = Args::command().try_get_matches_from(merged).map_err(|e| {
        format!("{}With the options of the config file {} (--no-config leaves it out).", e, path.display())
    })?;
    let mut args = Args::from_arg_matches(&matches).map_err(|e| e.to_string())?;
    args.config_in_use = Some(path);
    Ok(args)
}

/// The options of a config file's `text`, as arguments to go before those of the command line
/// `given` (see [`load`]): options as on a command line, any number a line ("double quotes"
/// around a value with spaces), `#` starting a comment line. An option the command line (or the
/// environment) gives too, or one that cannot go with one it gives or does its job ([`ONE_OF`], an
/// Authorization --header against -u or another), is left out, unless it can be repeated
/// (--header, --ad-pattern, --channel, --sections, --bind-address): then the file's come first.
/// Links, inputs, unknown options, bad values, a bare `--` and [`NOT_IN_CONFIG`] are an error that
/// names the line.
pub fn config_args(text: &str, given: &ArgMatches) -> Result<Vec<String>, String> {
    let command = Args::command();
    let set = |matches: &ArgMatches, sources: &[ValueSource]| -> Vec<clap::Id> {
        let source = |arg: &Arg| matches.value_source(arg.get_id().as_str());
        command.get_arguments().filter(|arg| source(arg).is_some_and(|s| sources.contains(&s))).map(|arg| arg.get_id().clone()).collect()
    };
    let typed = set(given, &[ValueSource::CommandLine, ValueSource::EnvVariable]);
    let arg = |id: &clap::Id| command.get_arguments().find(|arg| arg.get_id() == id);
    let against = |a: &clap::Id, b: &clap::Id| {
        let conflicts = |x: &clap::Id, y: &clap::Id| arg(x).is_some_and(|x| command.get_arg_conflicts_with(x).iter().any(|c| c.get_id() == y));
        let one_of = |x: &clap::Id, y: &clap::Id| ONE_OF.contains(&(x.as_str(), y.as_str()));
        conflicts(a, b) || conflicts(b, a) || one_of(a, b) || one_of(b, a)
    };
    let authorizes = |matches: &ArgMatches| {
        let typed = matches.value_source("headers") == Some(ValueSource::CommandLine);
        typed && matches.get_many::<(String, String)>("headers").into_iter().flatten().any(|(name, _)| name == "Authorization")
    };
    let signs_in = given.value_source("user") == Some(ValueSource::CommandLine) || authorizes(given);
    let mut out = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |e: String| format!("line {}: {}", number + 1, e);
        for option in options_of(split_words(line).map_err(at)?) {
            if option[0] == "--" {
                return Err(at("a bare -- cannot be in a config file: it would make the command line's options links".to_string()));
            }
            let argv = || std::iter::once("cli".to_string()).chain(option.iter().cloned());
            // A stray word is often a 'single-quoted' value or a # comment after an option: say so.
            let hint = || {
                option.iter().find_map(|word| match word {
                    w if w.starts_with('#') => Some("a # comment goes on a line of its own"),
                    w if w.starts_with('\'') || w.ends_with('\'') => Some("put a value with spaces in \"double quotes\": 'single' ones are not taken"),
                    _ => None,
                })
            };
            let refuse = |e: String| at(hint().map_or(e, str::to_string));
            let not_here = || refuse(format!("{} cannot be in a config file: links, inputs and actions go on the command line", option[0]));
            let found = match command.clone().try_get_matches_from(argv()) {
                Ok(found) => found,
                // An option that needs another (--delete-archive, --extract) may have it on another line.
                Err(e) if e.kind() == ErrorKind::MissingRequiredArgument => {
                    command.clone().ignore_errors(true).try_get_matches_from(argv()).map_err(|e| at(e.to_string()))?
                }
                Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => return Err(not_here()),
                Err(e) => return Err(refuse(e.to_string().lines().next().unwrap_or_default().trim_start_matches("error: ").to_string())),
            };
            let ids = set(&found, &[ValueSource::CommandLine]);
            if ids.iter().any(|id| NOT_IN_CONFIG.contains(&id.as_str())) {
                return Err(not_here());
            }
            // --tracks picks the stream's tracks: the command line's replace the file's.
            let repeatable = |id: &clap::Id| id != "tracks" && arg(id).is_some_and(|arg| matches!(arg.get_action(), ArgAction::Append));
            let overridden = ids.iter().any(|id| typed.iter().any(|t| (t == id && !repeatable(id)) || against(t, id)))
                || (signs_in && authorizes(&found));
            if !overridden {
                out.extend(option);
            }
        }
    }
    Ok(out)
}

/// The words of a config file's line: split at spaces, except within "double quotes" (which go).
fn split_words(line: &str) -> Result<Vec<String>, String> {
    let (mut words, mut word, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
    for c in line.chars() {
        match c {
            '"' => (quoted, started) = (!quoted, true),
            c if c.is_whitespace() && !quoted => {
                if std::mem::take(&mut started) {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err("a \" is not closed".to_string());
    }
    words.extend(started.then_some(word));
    Ok(words)
}

/// `words` as options: each word that starts with `-` begins one, which the words up to the next
/// are the values of. Words before the first are one of their own (links).
fn options_of(words: Vec<String>) -> Vec<Vec<String>> {
    let mut options: Vec<Vec<String>> = Vec::new();
    for word in words {
        match options.last_mut() {
            Some(option) if !(word.starts_with('-') && word.len() > 1) => option.push(word),
            _ => options.push(vec![word]),
        }
    }
    options
}

/// The tool commands: concat, merge, mux and doctor.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum SubCommand {
    /// Concatenate file bytes without FFmpeg
    Concat {
        #[arg(short = 'i', long = "input", value_name = "FILE|DIR", num_args = 0..)]
        inputs: Vec<PathBuf>,
        #[arg(long = "input-dir", value_name = "DIR")]
        input_dir: Option<PathBuf>,
        #[arg(long = "pattern", default_value = "*.ts")]
        pattern: String,
        #[arg(short = 'o', long = "output", value_name = "FILE")]
        output: PathBuf,
    },
    /// Use FFmpeg to merge segments from files or directory
    Merge {
        #[arg(short = 'i', long = "input", value_name = "FILE|DIR", num_args = 0..)]
        inputs: Vec<PathBuf>,
        #[arg(long = "input-dir", value_name = "DIR")]
        input_dir: Option<PathBuf>,
        #[arg(long = "pattern", default_value = "*.ts")]
        pattern: String,
        #[arg(short = 'o', long = "output", value_name = "FILE")]
        output: PathBuf,
        #[arg(long = "ffmpeg-concat-mode", value_enum, default_value = "local-http")]
        mode: ConcatModeArg,
        #[arg(long = "ffmpeg-binary-path", value_name = "PATH")]
        ffmpeg_binary_path: Option<PathBuf>,
    },
    /// Mux video, audio and subtitles into container
    Mux {
        #[arg(short = 'i', long = "input", value_name = "FILE|DIR|OPTIONS", num_args = 1..)]
        inputs: Vec<String>,
        #[arg(short = 'o', long = "output", value_name = "FILE")]
        output: PathBuf,
        #[arg(long = "title", value_name = "TITLE")]
        title: Option<String>,
        #[arg(long = "muxer", default_value = "ffmpeg")]
        muxer: String,
        /// Add a file of your own: path=FILE (\: for a colon in it), lang=CODE, name=NAME, e.g.
        /// "path=sub.srt:lang=eng:name=English"
        #[arg(long = "mux-import", value_name = "OPTIONS")]
        mux_imports: Vec<String>,
        #[arg(long = "ffmpeg-binary-path", value_name = "PATH")]
        ffmpeg_binary_path: Option<PathBuf>,
    },
    /// Check tool versions and system environment
    Doctor {
        #[arg(long = "json")]
        json: bool,
        #[arg(long = "ffmpeg-binary-path", value_name = "PATH")]
        ffmpeg_binary_path: Option<PathBuf>,
        #[arg(long = "mkvmerge-binary-path", value_name = "PATH")]
        mkvmerge_binary_path: Option<PathBuf>,
        #[arg(long = "decryption-binary-path", value_name = "PATH")]
        decryption_binary_path: Option<PathBuf>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ConcatModeArg {
    #[default]
    LocalHttp,
    Demuxer,
    Protocol,
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
        assert_eq!(parse_bytes("1048576"), Ok(1_048_576));
        assert_eq!(parse_bytes("500K"), Ok(512_000));
        assert_eq!(parse_bytes("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_bytes("1.5MiB"), Ok(1_572_864));
        assert_eq!(parse_bytes("750kb/s"), Ok(768_000));
        assert_eq!(parse_bytes("1g"), Ok(1024 * 1024 * 1024));
        assert_eq!(parse_bytes("0"), Ok(0));
        assert!(parse_bytes("fast").is_err());
        assert!(parse_bytes("5T").is_err());
        assert!(parse_bytes("").is_err());
        assert!(parse_bytes("1.2.3M").is_err());
    }

    #[test]
    fn header_forms() {
        let header = |s: &str| parse_header(s).map(|(name, value)| format!("{name}={value}"));
        assert_eq!(header("Authorization: Bearer x").as_deref(), Ok("Authorization=Bearer x"));
        assert_eq!(header("authorization:Basic abc=").as_deref(), Ok("Authorization=Basic abc="));
        assert_eq!(header("Bearer x").as_deref(), Ok("Authorization=Bearer x"));
        assert_eq!(header("Bearer a:b").as_deref(), Ok("Authorization=Bearer a:b"));
        assert_eq!(header("Cookie: a=b").as_deref(), Ok("Cookie=a=b"));
        assert_eq!(header("X-Api-Key:  k:1 ").as_deref(), Ok("X-Api-Key=k:1"));
        assert!(header("Authorization:   ").is_err());
        assert!(header("Bearer x\r\nX-Evil: 1").is_err());
        assert!(header("X-A: b\nc").is_err());
        assert!(header("X(A): b").unwrap_err().contains("not a header name"));
        let changing = ["Accept-Encoding: gzip", "TE: trailers", "Upgrade: h2c", "Expect: 100-continue", "Keep-Alive: 5", "Proxy-Connection: close"];
        for own in ["Host: h", "range: bytes=0-", "Content-Length: 1", "Transfer-Encoding: chunked", "Connection: close"].iter().chain(&changing) {
            assert!(header(own).unwrap_err().contains("sets the"), "{own}");
        }
        assert!(header("If-None-Match: \"v1\"").unwrap_err().contains("change what the server sends"));
        assert!(header("if-range: x").is_err());
        assert_eq!(header("Ifx: 1").as_deref(), Ok("Ifx=1"), "only If-*");
    }

    #[test]
    fn a_user_without_a_password_is_asked_only_where_someone_answers() {
        assert_eq!(basic_auth("bob:pw", None).as_deref(), Ok("Basic Ym9iOnB3"));
        assert_eq!(basic_auth("bob:", None).as_deref(), Ok("Basic Ym9iOg=="), "an empty password is one");
        assert_eq!(basic_auth("bob", Some(|user| Some(format!("{user}-pw")))).as_deref(), Ok("Basic Ym9iOmJvYi1wdw=="));
        assert!(basic_auth("bob", None).unwrap_err().contains("-u bob:PASS"));
        assert!(basic_auth("bob", Some(|_| None)).is_err(), "no answer");
    }

    #[test]
    fn request_and_output_flags() {
        let parse = |args: &[&str]| Args::try_parse_from(std::iter::once("cli").chain(args.iter().copied()));
        let args = parse(&["u"]).unwrap();
        assert!(!args.json && !args.spider && !args.server_response && !args.preview && !args.dry_run && !args.stdout);
        assert!(!args.insecure && !args.ipv4 && !args.ipv6 && !args.no_proxy && !args.no_clobber && !args.no_adaptive);
        assert_eq!((args.connect_timeout, args.max_filesize, args.record_seconds), (15, None, None));
        let args = parse(&[
            "-S", "-k", "-4", "-N", "-U", "agent/1", "-u", "bob:pw", "--cookie", "a=b; c=d", "--header", "X-A: 1", "--header",
            "Bearer t", "--max-filesize", "10M", "--connect-timeout", "5", "--record-seconds", "60", "--no-remote-checksum",
            "--remote-time", "--logfile", "log.txt", "--json", "u",
        ])
        .unwrap();
        assert!(args.server_response && args.insecure && args.ipv4 && args.only_if_newer && args.no_remote_checksum && args.remote_time);
        assert_eq!((args.user_agent.as_deref(), args.user.as_deref(), args.cookie.as_deref()), (Some("agent/1"), Some("bob:pw"), Some("a=b; c=d")));
        assert_eq!((args.auth_header().as_deref(), args.extra_headers()), (Some("Bearer t"), vec![("X-A".to_string(), "1".to_string())]));
        assert_eq!((args.max_filesize, args.connect_timeout, args.record_seconds), (Some(10 << 20), 5, Some(60)));
        assert!(parse(&["-4", "-6", "u"]).is_err());
        assert!(parse(&["--no-clobber", "-N", "u"]).is_err());
        assert!(parse(&["--no-proxy", "--proxy", "http://p:1", "u"]).is_err());
        assert!(parse(&["--connect-timeout", "0", "u"]).is_err());
        assert!(parse(&["--record-seconds", "0", "u"]).is_err());
        assert!(parse(&["--header", "Host: x", "u"]).is_err());
        assert!(parse(&["--verify", "f", "--repair", "-u", "bob:pw"]).is_err());
        assert!(parse(&["--verify", "f", "--repair", "--cookie", "a=b"]).is_err());
        // --stdout checks no checksum: the bytes are gone by the time it could.
        assert!(parse(&["--stdout", "--checksum", &"ab".repeat(32), "u"]).is_err());
        // The last of --adaptive and --no-adaptive wins.
        assert!(!parse(&["--no-adaptive", "--adaptive", "u"]).unwrap().no_adaptive);
        assert!(parse(&["--adaptive", "--no-adaptive", "u"]).unwrap().no_adaptive);
        for exclusive in [["--spider", "--dry-run"], ["--preview", "--json"], ["--stdout", "--json"], ["--stdout", "--spider"], ["--json", "--history"]] {
            assert!(parse(&[exclusive[0], exclusive[1], "u"]).is_err(), "{exclusive:?}");
        }
        assert!(parse(&["--stdout", "-o", "f", "u"]).is_err());
        assert!(parse(&["--completions", "powershell"]).unwrap().completions.is_some());
        assert!(parse(&["--completions", "tcsh"]).is_err());
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
        assert_eq!((args.verbose, args.max_speed, args.auth_header().as_deref()), (2, Some(2 << 20), Some("Bearer t")));
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

    /// The stream flags take what the engine takes, checked as they are parsed.
    #[test]
    fn stream_flags_are_checked_as_they_are_parsed() {
        let parse = |args: &[&str]| Args::try_parse_from(std::iter::once("cli").chain(args.iter().copied()));
        let args = parse(&["u"]).unwrap();
        assert!(args.video.is_none() && args.audio.is_none() && args.tracks.is_empty() && !args.list_tracks && args.drop_parts.is_none());
        assert!(args.ad_patterns.is_empty() && args.channels.is_empty() && args.name_pattern.is_none() && args.live_from.is_none());
        let args = parse(&[
            "--video", " 1080p,hevc ", "--audio", "en,ja,eac3", "--tracks", "v0,A1", "--tracks", "s0", "--drop-parts", "0,2-4",
            "--ad-pattern", "/ads/", "--ad-pattern", "*-ad-*.ts", "--name-pattern", "{name} {height}p", "--live-from", "1h30m",
            "--live-idle-timeout", "20", "--channel", "news", "--channel", "*sport*", "u",
        ])
        .unwrap();
        assert_eq!((args.video.as_deref(), args.audio.as_deref()), (Some("1080p,hevc"), Some("en,ja,eac3")));
        assert_eq!((args.tracks, part_ids(args.drop_parts.as_deref())), (vec!["v0".to_string(), "a1".into(), "s0".into()], vec![0, 2, 3, 4]));
        assert_eq!((args.ad_patterns, args.channels), (vec!["/ads/".to_string(), "*-ad-*.ts".into()], vec!["news".to_string(), "*sport*".into()]));
        assert_eq!((args.name_pattern.as_deref(), args.live_from.as_deref(), args.live_idle_timeout), (Some("{name} {height}p"), Some("1h30m"), Some(20)));
        assert!(parse(&["--list-tracks", "u"]).unwrap().list_tracks);
        assert!(parse(&["--live-from", "2026-10-10 21:00:00+08:00", "u"]).is_ok());
        for bad in [
            &["--video", "8k"][..], &["--audio", "en,loud"], &["--tracks", "x1"], &["--tracks", "v"], &["--tracks", "v0,"], &["--drop-parts", "3-1"],
            &["--drop-parts", ","], &["--ad-pattern", " "], &["--channel", ""], &["--name-pattern", "{title}"], &["--name-pattern", "{name"],
            &["--live-from", "yesterday"], &["--live-from", "30m", "--live-from-start"], &["--live-idle-timeout", "0"],
            &["--list-tracks", "--dry-run"], &["--list-tracks", "-i", "list.txt"],
        ] {
            assert!(parse(&[bad, &["u"]].concat()).is_err(), "{bad:?}");
        }
        let e = parse(&["--video", "1080p,8k", "u"]).unwrap_err().to_string();
        assert!(e.contains("8k"), "the error names the bad term: {e}");
    }

    /// --doctor, --config and --start-at are checked as they are parsed.
    #[test]
    fn run_flags() {
        let parse = |args: &[&str]| Args::try_parse_from(std::iter::once("cli").chain(args.iter().copied()));
        assert!(parse(&["--doctor"]).unwrap().doctor);
        assert!(parse(&["--doctor", "--json", "-d", "out"]).is_ok());
        for bad in [&["--doctor", "u"][..], &["--doctor", "--update"], &["--doctor", "--history"], &["--config", "no-such-file.conf"], &["--start-at", "2020-01-01 10:00", "u"]] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let file = std::env::current_exe().unwrap();
        assert!(parse(&["--config", file.to_str().unwrap(), "--no-config"]).is_err());
        assert!(parse(&["--start-at", "21:30", "u"]).unwrap().start_at.is_some());
        assert!(parse(&["--start-at", "21:30", "--doctor"]).is_err());
    }

    /// `HH:MM` is the next time the clock shows it: today if it is still to come, else tomorrow;
    /// a date and time must be still to come.
    #[test]
    fn a_start_time_is_the_next_one_to_come() {
        let now = 1_800_000_000.0;
        let clock = |epoch: f64| hyperfetch_core::stream::clock_text(epoch)[11..16].to_string();
        // Minutes from now, wherever this computer's clock is.
        let soon = start_time(&clock(now + 600.0), now).unwrap();
        assert!(soon > now && soon <= now + 600.0, "{}", soon - now);
        let passed = start_time(&clock(now - 600.0), now).unwrap();
        let tomorrow = passed - now;
        // A day less 10 minutes, give or take an hour of a clock change and the minute cut off.
        assert!((86_400.0 - 600.0 - 3_700.0..=86_400.0 + 3_600.0).contains(&tomorrow), "{tomorrow}");
        // 2027-01-15T08:00:00Z is `now`.
        assert_eq!(start_time("2027-01-15T10:30:00Z", now), Ok(now + 9_000.0));
        assert!(start_time("2020-01-01 10:00", now).unwrap_err().contains("has passed"));
        for bad in ["25:00", "9", "30m", "10:5x", "2027-13-01 10:00", "tomorrow", ""] {
            assert!(start_time(bad, now).is_err(), "{bad}");
        }
        assert!(start_time(" 7:05 ", now).is_ok(), "one-digit hours");
    }

    /// The matches of a command line `argv` (after the program's name).
    fn given(argv: &[&str]) -> ArgMatches {
        Args::command().try_get_matches_from(std::iter::once("cli").chain(argv.iter().copied())).unwrap()
    }

    /// A config file gives defaults: the command line's own win, a repeatable option's come after
    /// the file's, one that cannot go with the command line's is left out; links, inputs and
    /// actions, unknown options and bad values are errors that name the line.
    #[test]
    fn a_config_file_gives_defaults_the_command_line_wins_over() {
        let text = "# defaults\n\n--split 4 --header \"X-A: 1\"\n  --ipv4\n--subs en\n--delete-archive\n--extract\n--name-pattern \"{name} {height}p\"\n";
        let options = config_args(text, &given(&["u"])).unwrap();
        let expected = ["--split", "4", "--header", "X-A: 1", "--ipv4", "--subs", "en", "--delete-archive", "--extract", "--name-pattern", "{name} {height}p"];
        assert_eq!(options, expected);
        let options = config_args(text, &given(&["-s", "8", "--header", "X-B: 2", "--ipv6", "--subs", "fr", "u"])).unwrap();
        assert_eq!(options, ["--header", "X-A: 1", "--delete-archive", "--extract", "--name-pattern", "{name} {height}p"]);
        for (text, line) in [
            ("--hls-mp4\nhttps://a.example/f", "line 2"),
            ("--hls-mp4 -i list.txt", "line 1"),
            ("\n\n--history", "line 3"),
            ("--config other.conf", "line 1"),
            ("--doctor", "line 1"),
            ("--help", "line 1"),
            ("--no-such-option", "line 1"),
            ("# ok\n--video 8k", "line 2"),
            ("--header \"X-A: 1", "line 1"),
            ("--split 4\n--", "line 2"),
        ] {
            let e = config_args(text, &given(&["u"])).unwrap_err();
            assert!(e.starts_with(line), "{text:?}: {e}");
        }
    }

    /// What makes a run wait, list or probe in place of a download stays on the command line, and a
    /// 'single-quoted' value or a # comment after an option is named as such.
    #[test]
    fn a_config_file_holds_no_run_mode_and_its_mistakes_are_named() {
        for text in ["--start-at 21:30", "--list-tracks", "--json", "--dry-run", "--spider", "--preview", "--stdout"] {
            let e = config_args(text, &given(&["u"])).unwrap_err();
            assert!(e.starts_with("line 1") && e.contains("cannot be in a config file"), "{text:?}: {e}");
        }
        let e = config_args("--header 'X-A: 1'", &given(&["u"])).unwrap_err();
        assert!(e.starts_with("line 1") && e.contains("double quotes"), "{e}");
        let e = config_args("--split 4 # four", &given(&["u"])).unwrap_err();
        assert!(e.starts_with("line 1") && e.contains("comment"), "{e}");
    }

    /// The command line's sign-in, proxy, cookies and --tracks leave out the file's that do the same
    /// job: an Authorization header against -u, a proxy pool against a proxy, a cookie string
    /// against cookies from a file or a browser.
    #[test]
    fn the_command_line_s_sign_in_proxy_cookies_and_tracks_win() {
        let auth = "--header \"Authorization: Bearer FILE\"\n--header \"X-A: 1\"\n";
        assert_eq!(config_args(auth, &given(&["-u", "bob:pw", "u"])).unwrap(), ["--header", "X-A: 1"]);
        assert_eq!(config_args(auth, &given(&["--header", "Bearer CLI", "u"])).unwrap(), ["--header", "X-A: 1"]);
        assert_eq!(config_args(auth, &given(&["--header", "X-B: 2", "u"])).unwrap().len(), 4, "another header leaves it in");
        for (text, typed) in [
            ("--proxy-pool socks5://a:1080", "--proxy=http://corp:3128"),
            ("--proxies-file Cargo.toml", "--proxy=http://corp:3128"),
            ("--proxy http://corp:3128", "--proxy-pool=socks5://a:1080"),
            ("--proxy http://corp:3128", "--proxies-file=Cargo.toml"),
            ("--cookie sid=old", "--load-cookies=Cargo.toml"),
            ("--cookie sid=old", "--cookies-from-browser=firefox"),
            ("--load-cookies Cargo.toml", "--cookie=sid=new"),
            ("--cookies-from-browser firefox", "--cookie=sid=new"),
            ("--tracks v0", "--tracks=v1,a0"),
        ] {
            assert_eq!(config_args(text, &given(&[typed, "u"])).unwrap(), Vec::<String>::new(), "{text} against {typed}");
            assert!(!config_args(text, &given(&["u"])).unwrap().is_empty(), "{text} alone");
        }
    }

    /// The config file is --config's, else the default one when it is there; --no-config reads
    /// none. Its options come before the command line's.
    #[test]
    fn the_config_file_is_read_before_the_command_line() {
        let dir = std::env::temp_dir().join(format!("hf-cli-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (default, other) = (dir.join("cli.conf"), dir.join("other.conf"));
        std::fs::write(&default, "--split 4\n--header \"X-A: 1\"\n").unwrap();
        std::fs::write(&other, "\u{feff}--hls-mp4\r\n").unwrap();
        let argv = |args: &[&str]| std::iter::once("cli").chain(args.iter().copied()).map(OsString::from).collect::<Vec<_>>();
        let args = load(argv(&["--header", "X-B: 2", "u"]), Some(default.clone())).unwrap();
        assert_eq!((args.connections, args.headers.len(), args.config_in_use.as_deref()), (4, 2, Some(default.as_path())));
        assert_eq!(args.headers[0], ("X-A".to_string(), "1".to_string()), "the file's first");
        let args = load(argv(&["-s", "2", "u"]), Some(default.clone())).unwrap();
        assert_eq!(args.connections, 2);
        let args = load(argv(&["--no-config", "u"]), Some(default.clone())).unwrap();
        assert_eq!((args.connections, args.config_in_use), (16, None));
        let args = load(argv(&["--config", other.to_str().unwrap(), "u"]), Some(default.clone())).unwrap();
        assert!(args.hls_mp4 && args.connections == 16);
        let args = load(argv(&["u"]), Some(dir.join("missing.conf"))).unwrap();
        assert_eq!(args.config_in_use, None, "a default that is not there is no error");
        std::fs::write(&default, "--extract\nhttps://a.example/f\n").unwrap();
        let e = load(argv(&["u"]), Some(default.clone())).unwrap_err();
        assert!(e.contains("line 2") && e.contains(&default.display().to_string()), "{e}");
        // What clap refuses only once both are put together names the file too.
        std::fs::write(&default, "--delete-archive\n").unwrap();
        let e = load(argv(&["u"]), Some(default.clone())).unwrap_err();
        assert!(e.contains("--extract") && e.contains("--no-config"), "{e}");
        // What a command-line option needs may be in the file.
        std::fs::write(&default, "--extract\n").unwrap();
        let args = load(argv(&["--delete-archive", "u"]), Some(default)).unwrap();
        assert!(args.delete_archive && args.extract);
        let _ = std::fs::remove_dir_all(&dir);
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
