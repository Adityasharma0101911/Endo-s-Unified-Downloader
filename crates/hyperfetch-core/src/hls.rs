use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use url::Url;
use thiserror::Error;
use crate::engine::EngineSnapshot;
use crate::range::ByteRange;
use crate::storage::{FileDigest, StreamHasher};
use crate::worker::{authorize, Auth};

const MAX_CONNECTIONS: usize = 64;
/// Segments fetched or held ahead of the next one to write, per connection: a slow segment leaves
/// the other connections this much to do before they would sit idle.
const WINDOW_PER_CONNECTION: usize = 2;
/// A segment is requested a second time once it has taken twice as long as a typical one, but
/// never sooner than this: a second request saves too little on a fast one to be worth it.
const HEDGE_MIN_WAIT: Duration = Duration::from_secs(1);
/// Before any segment has been fetched, one is requested a second time after half the stall
/// timeout, but never later than this: a first segment that stalls would otherwise wait out the
/// stall timeout of every attempt.
const FIRST_HEDGE_MAX_WAIT: Duration = Duration::from_secs(5);
/// Master playlists may point at further master playlists; stop following them after this many hops.
const MAX_MASTER_DEPTH: usize = 3;
const MAX_PLAYLIST_BYTES: u64 = if cfg!(test) { 64 * 1024 } else { 16 * 1024 * 1024 };
const MAX_KEY_BYTES: u64 = 1024;
/// Most AES keys fetched at once; more would risk a burst of 429s from the key server.
const KEY_CONCURRENCY: usize = 16;
/// Most of an oversized body read anyway, to tell a huge playlist from an ordinary file.
const PEEK_BYTES: u64 = 1024;
/// Segments queued for the disk besides the one being written. The reorder window holds more
/// while the disk is slow; this only smooths out bursts.
const WRITE_QUEUE: usize = 4;
/// The resume state is saved at most this often while segments are written, and once they stop.
const PERSIST_INTERVAL: Duration = Duration::from_secs(2);
/// What an earlier run wrote is hashed this much at a time, so that a download stopped meanwhile
/// need not wait for all of it.
const PART_HASH_STEP: u64 = if cfg!(test) { 16 * 1024 } else { 64 * 1024 * 1024 };
/// Largest segment or init section held in memory. Real segments are a few MiB.
// ponytail: up to WINDOW_PER_CONNECTION x connections segments, one second request and
// WRITE_QUEUE + 1 segments on their way to the disk are held at once, so peak memory is that many
// times this; spool segments to disk if streams with huge segments must work.
const MAX_SEGMENT_BYTES: u64 = if cfg!(test) { 64 * 1024 } else { 256 * 1024 * 1024 };
/// Most bytes one request fetches when it merges the byte ranges of several segments of one file:
/// enough to spare most of the per-request round trips, small enough to keep every connection busy.
const MAX_MERGED_BYTES: u64 = if MAX_SEGMENT_BYTES < 8 * 1024 * 1024 { MAX_SEGMENT_BYTES } else { 8 * 1024 * 1024 };
/// Most bytes that merged requests hold in memory at once (see [`merge_limit`]).
const MERGE_BUDGET: u64 = 8 * MAX_MERGED_BYTES;
/// Longest media playlist accepted (a day of 2-second segments is about 43,000).
const MAX_SEGMENTS: usize = if cfg!(test) { 1000 } else { 1_000_000 };
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(200);
/// First retry delay; doubles on every further attempt (0.5s, 1s, 2s, 4s, 8s, 8s, ...).
const RETRY_BASE_DELAY: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_millis(500) };
/// Longest pause between two attempts: the pauses of the default 8 retries add up to about 40 s,
/// well inside the time the engine allows for fetching the playlist and its keys.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(8);

/// Patience of every HLS request (playlists, keys, segments), from the user's settings.
#[derive(Clone, Copy, Debug)]
pub struct FetchPolicy {
    /// Longest wait for the response headers, and between two pieces of the body.
    pub stall_timeout: Duration,
    /// Failed attempts allowed per request before it gives up.
    pub max_retries: u32,
}

/// How [`HlsEngine::download`] fetches and stores a stream.
#[derive(Clone, Debug)]
pub struct HlsOptions {
    /// Requests in flight at once (1 to 64).
    pub connections: usize,
    pub fetch: FetchPolicy,
    /// Flush the finished file to disk before it takes its final name.
    pub fsync_on_complete: bool,
    /// The checksum the file will be checked against: it says which digests to take while
    /// writing it.
    pub expected_checksum: Option<String>,
}

#[derive(Error, Debug)]
pub enum HlsError {
    #[error("Network error during HLS transfer: {0}")]
    Network(#[from] reqwest::Error),
    #[error("I/O error during HLS assembly: {0}")]
    Io(#[from] std::io::Error),
    /// The top-level response is not an HLS playlist at all (no `#EXTM3U` header), so it may
    /// be an ordinary file. Never used once a real playlist has been seen.
    #[error("Invalid HLS playlist: {0}")]
    InvalidPlaylist(String),
    /// A playlist or key could not be fetched (network error, HTTP error status, bad key).
    #[error("HLS stream unavailable: {0}")]
    Unavailable(String),
    #[error("No video segments found in playlist")]
    NoSegments,
    /// A real HLS stream this engine cannot download correctly. Falling back to a
    /// plain download of the playlist URL would not help either.
    #[error("Unsupported HLS stream: {0}")]
    Unsupported(String),
    #[error("HLS segment {index} could not be downloaded: {reason}")]
    SegmentFailed { index: usize, reason: String },
    #[error("Download cancelled by user")]
    Cancelled,
}

/// AES-128-CBC parameters for one encrypted resource (`#EXT-X-KEY:METHOD=AES-128`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aes128Key {
    pub key: [u8; 16],
    pub iv: [u8; 16],
}

/// Media initialization section (`#EXT-X-MAP`), e.g. the ftyp/moov header of fMP4 streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitSection {
    pub url: Url,
    pub byte_range: Option<ByteRange>,
    pub encryption: Option<Aes128Key>,
}

#[derive(Debug, Clone)]
pub struct HlsSegment {
    pub index: usize,
    pub url: Url,
    pub duration_secs: f64,
    pub byte_range: Option<ByteRange>,
    pub encryption: Option<Aes128Key>,
    /// Init section that must precede this segment in the output, shared by every
    /// segment it applies to.
    pub init: Option<Arc<InitSection>>,
}

/// File extension matching the container the segments form: fMP4 streams need an
/// init section (`#EXT-X-MAP`), everything else is MPEG-TS.
pub fn container_extension(segments: &[HlsSegment]) -> &'static str {
    if segments.iter().any(|s| s.init.is_some()) { "mp4" } else { "ts" }
}

/// Parses an HLS (.m3u8) playlist. If it is a master playlist, it selects the best
/// variant that carries audio and returns that variant's media segments, with
/// AES-128 keys already fetched. `auth` is added to every request it covers.
pub async fn parse_hls_playlist(
    client: &Client,
    playlist_url: &Url,
    auth: Option<&Auth>,
    fetch: FetchPolicy,
) -> Result<Vec<HlsSegment>, HlsError> {
    let mut url = playlist_url.clone();
    for hop in 0..=MAX_MASTER_DEPTH {
        let (body, final_url) = fetch_with_retry(client, auth, &url, None, MAX_PLAYLIST_BYTES, None, fetch)
            .await
            .map_err(|e| {
                let reason = format!("could not fetch {}: {}", url, e.reason);
                match e.kind {
                    // A large file whose URL merely mentions .m3u8: download it as a plain file.
                    FetchErrorKind::TooLarge { playlist: false } if hop == 0 => HlsError::InvalidPlaylist(reason),
                    // A real playlist, or a variant: saving playlist text as the download would be
                    // a fake success.
                    FetchErrorKind::TooLarge { .. } => HlsError::Unsupported(reason),
                    _ => HlsError::Unavailable(reason),
                }
            })?;
        let text = String::from_utf8_lossy(&body);
        let text = text.trim_start_matches('\u{feff}').trim_start();
        if !text.starts_with("#EXTM3U") {
            // Only the URL the caller named may turn out to be a plain file. A variant that is not
            // a playlist (typically an error page for an expired token) means the stream failed.
            return Err(if hop == 0 {
                HlsError::InvalidPlaylist("Missing #EXTM3U header".to_string())
            } else {
                HlsError::Unavailable(format!("variant {} is not an HLS playlist", url))
            });
        }

        if text.lines().any(|l| l.trim_start().starts_with("#EXT-X-STREAM-INF:")) {
            // Relative URIs resolve against where the playlist actually came from (post-redirect).
            url = select_variant(text, &final_url)?;
            tracing::info!("Selected HLS variant: {}", url);
            continue;
        }
        return parse_media_playlist(client, auth, text, &final_url, fetch).await;
    }
    Err(malformed(format!("master playlists nested more than {} levels deep", MAX_MASTER_DEPTH)))
}

/// Whether `body` starts like an HLS playlist: `#EXTM3U` after an optional BOM and whitespace.
fn is_playlist_start(body: &[u8]) -> bool {
    body.strip_prefix("\u{feff}".as_bytes()).unwrap_or(body).trim_ascii_start().starts_with(b"#EXTM3U")
}

/// A fault inside a real playlist. Never `InvalidPlaylist`: falling back to a plain download
/// would save the playlist text as if it were the video.
fn malformed(reason: impl std::fmt::Display) -> HlsError {
    HlsError::Unsupported(format!("malformed playlist: {}", reason))
}

/// Splits an attribute list (`KEY=value,KEY2="quoted,value"`) into name/value pairs.
fn parse_attributes(list: &str) -> HashMap<&str, &str> {
    let mut attrs = HashMap::new();
    let mut rest = list.trim();
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq].trim();
        rest = &rest[eq + 1..];
        let value = if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"').unwrap_or(quoted.len());
            rest = quoted.get(end + 1..).unwrap_or("");
            &quoted[..end]
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let value = &rest[..end];
            rest = &rest[end..];
            value
        };
        rest = rest.trim_start().strip_prefix(',').unwrap_or(rest).trim_start();
        attrs.insert(name, value.trim());
    }
    attrs
}

/// Parses `<length>[@<offset>]`; without an offset the range continues from `next_start`.
fn parse_byte_range(spec: &str, next_start: u64) -> Result<ByteRange, HlsError> {
    let invalid = || malformed(format!("invalid byte range '{}'", spec));
    let (len, offset) = match spec.split_once('@') {
        Some((len, offset)) => (len, Some(offset.trim().parse::<u64>().map_err(|_| invalid())?)),
        None => (spec, None),
    };
    let len = len.trim().parse::<u64>().map_err(|_| invalid())?;
    ByteRange::from_len(offset.unwrap_or(next_start), len).map_err(|_| invalid())
}

fn parse_iv(hex: &str) -> Option<[u8; 16]> {
    let digits = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X"))?;
    u128::from_str_radix(digits, 16).ok().filter(|_| digits.len() == 32).map(u128::to_be_bytes)
}

const AUDIO_CODEC_PREFIXES: [&str; 8] = ["mp4a", "ac-3", "ec-3", "ac-4", "opus", "flac", "alac", "dts"];

/// Picks the best variant of a master playlist. Variants whose audio is muxed in are
/// preferred; a variant whose audio lives only in a separate `#EXT-X-MEDIA` rendition
/// would come out silent, so that case is an error.
fn select_variant(master: &str, base: &Url) -> Result<Url, HlsError> {
    struct Variant<'a> {
        bandwidth: u64,
        pixels: u64,
        audio_group: Option<&'a str>,
        codecs: Option<&'a str>,
        uri: &'a str,
    }

    // GROUP-ID -> whether every audio rendition of the group is a separate playlist.
    let mut audio_groups: HashMap<&str, bool> = HashMap::new();
    let mut variants = Vec::new();
    let mut pending = None;
    for line in master.lines() {
        let trimmed = line.trim();
        if let Some(list) = trimmed.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = parse_attributes(list);
            if attrs.get("TYPE") == Some(&"AUDIO") {
                if let Some(group) = attrs.get("GROUP-ID") {
                    let separate = attrs.contains_key("URI");
                    audio_groups.entry(*group).and_modify(|s| *s &= separate).or_insert(separate);
                }
            }
        } else if let Some(list) = trimmed.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(parse_attributes(list));
        } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
            if let Some(attrs) = pending.take() {
                let pixels = attrs
                    .get("RESOLUTION")
                    .and_then(|r| r.split_once('x'))
                    .and_then(|(w, h)| Some(w.parse::<u64>().ok()?.saturating_mul(h.parse().ok()?)))
                    .unwrap_or(0);
                variants.push(Variant {
                    bandwidth: attrs.get("BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                    pixels,
                    audio_group: attrs.get("AUDIO").copied(),
                    codecs: attrs.get("CODECS").copied(),
                    uri: trimmed,
                });
            }
        }
    }

    let separate_audio = |v: &Variant| v.audio_group.is_some_and(|g| audio_groups.get(g) == Some(&true));
    let muxed_audio = |v: &Variant| {
        !separate_audio(v)
            && v.codecs.is_none_or(|c| {
                c.split(',').any(|codec| AUDIO_CODEC_PREFIXES.iter().any(|p| codec.trim().starts_with(p)))
            })
    };
    let rank = |v: &&Variant| (v.bandwidth, v.pixels);

    let chosen = match variants.iter().filter(|v| muxed_audio(v)).max_by_key(rank) {
        Some(v) => v.uri,
        None if variants.iter().any(separate_audio) => {
            return Err(HlsError::Unsupported(
                "the audio track is delivered as a separate EXT-X-MEDIA rendition, so the video \
                 variant alone would be silent; download this stream with the media engine \
                 (yt-dlp + ffmpeg) instead"
                    .to_string(),
            ))
        }
        // No variant advertises audio at all: the stream is video-only by design.
        None => variants
            .iter()
            .max_by_key(rank)
            .map(|v| v.uri)
            .ok_or_else(|| malformed("no variant stream URI found"))?,
    };
    base.join(chosen).map_err(|e| malformed(format!("invalid variant URL '{}': {}", chosen, e)))
}

/// Key currently in force while walking a media playlist.
struct ActiveKey {
    key: [u8; 16],
    iv: Option<[u8; 16]>,
}

async fn parse_media_playlist(
    client: &Client,
    auth: Option<&Auth>,
    text: &str,
    base: &Url,
    fetch: FetchPolicy,
) -> Result<Vec<HlsSegment>, HlsError> {
    let is_vod = text.lines().map(str::trim).any(|l| l == "#EXT-X-ENDLIST" || l == "#EXT-X-PLAYLIST-TYPE:VOD");
    if !is_vod {
        return Err(HlsError::Unsupported(
            "live playlist (no #EXT-X-ENDLIST): only the segments currently listed exist, so the \
             recording would be silently truncated; use the media engine to record live streams"
                .to_string(),
        ));
    }

    let join = |uri: &str| base.join(uri).map_err(|e| malformed(format!("invalid URL '{}': {}", uri, e)));

    // Playlists that rotate keys name hundreds of them: they are fetched several at once, ahead of
    // the walk below, which waits only for the key it has reached. A key that could not be fetched
    // ends the walk where it is used, so errors come in playlist order, and the fetches still in
    // flight then, never needed, are dropped.
    let mut prefetched = futures_util::stream::iter(key_urls(text, base))
        .map(|url| async move {
            let key = fetch_key(client, auth, &url, fetch).await;
            (url, key)
        })
        .buffer_unordered(KEY_CONCURRENCY);
    let mut key_cache: HashMap<Url, Result<[u8; 16], String>> = HashMap::new();

    let mut segments = Vec::new();
    let mut media_sequence: u64 = 0;
    let mut duration = 2.0;
    let mut byte_range = None;
    let mut next_range_start = 0;
    let mut key: Option<ActiveKey> = None;
    let mut init = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(info) = trimmed.strip_prefix("#EXTINF:") {
            duration = info.split(',').next().and_then(|d| d.trim().parse().ok()).unwrap_or(2.0);
        } else if let Some(seq) = trimmed.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = seq.trim().parse().map_err(|_| malformed(format!("invalid media sequence '{}'", seq)))?;
        } else if let Some(spec) = trimmed.strip_prefix("#EXT-X-BYTERANGE:") {
            let range = parse_byte_range(spec, next_range_start)?;
            next_range_start = range.end.saturating_add(1);
            byte_range = Some(range);
        } else if let Some(list) = trimmed.strip_prefix("#EXT-X-KEY:") {
            let attrs = parse_attributes(list);
            match attrs.get("METHOD").copied() {
                Some("NONE") => key = None,
                Some("AES-128") => {
                    if attrs.get("KEYFORMAT").is_some_and(|f| *f != "identity") {
                        return Err(HlsError::Unsupported("DRM-protected stream (non-identity KEYFORMAT)".to_string()));
                    }
                    let uri = attrs.get("URI").ok_or_else(|| malformed("#EXT-X-KEY without URI"))?;
                    let key_url = join(uri)?;
                    let key_bytes = loop {
                        if let Some(k) = key_cache.get(&key_url) {
                            break k.clone();
                        }
                        match prefetched.next().await {
                            Some((url, k)) => {
                                key_cache.insert(url, k);
                            }
                            // Not among the keys fetched ahead: fetch it now.
                            None => {
                                let k = fetch_key(client, auth, &key_url, fetch).await;
                                key_cache.insert(key_url, k.clone());
                                break k;
                            }
                        }
                    }
                    .map_err(HlsError::Unavailable)?;
                    let iv = match attrs.get("IV") {
                        Some(iv) => Some(parse_iv(iv).ok_or_else(|| malformed(format!("invalid IV '{}'", iv)))?),
                        None => None,
                    };
                    key = Some(ActiveKey { key: key_bytes, iv });
                }
                other => {
                    return Err(HlsError::Unsupported(format!(
                        "encryption method {} is not supported (only AES-128)",
                        other.unwrap_or("<missing>")
                    )))
                }
            }
        } else if let Some(list) = trimmed.strip_prefix("#EXT-X-MAP:") {
            let attrs = parse_attributes(list);
            let uri = attrs.get("URI").ok_or_else(|| malformed("#EXT-X-MAP without URI"))?;
            let encryption = match &key {
                None => None,
                Some(ActiveKey { key, iv: Some(iv) }) => Some(Aes128Key { key: *key, iv: *iv }),
                Some(_) => return Err(malformed("encrypted #EXT-X-MAP requires an explicit IV")),
            };
            init = Some(Arc::new(InitSection {
                url: join(uri)?,
                byte_range: attrs.get("BYTERANGE").map(|r| parse_byte_range(r, 0)).transpose()?,
                encryption,
            }));
        } else if !trimmed.starts_with('#') {
            let index = segments.len();
            if index == MAX_SEGMENTS {
                return Err(HlsError::Unsupported(format!("playlist has more than {} segments", MAX_SEGMENTS)));
            }
            // Parsing a huge playlist takes a while; let other tasks (and timeouts) run.
            if index % 1024 == 1023 {
                tokio::task::yield_now().await;
            }
            let sequence = media_sequence.saturating_add(index as u64);
            segments.push(HlsSegment {
                index,
                url: join(trimmed)?,
                duration_secs: duration,
                byte_range: byte_range.take(),
                encryption: key.as_ref().map(|k| Aes128Key {
                    key: k.key,
                    iv: k.iv.unwrap_or((sequence as u128).to_be_bytes()),
                }),
                init: init.clone(),
            });
        }
    }

    if segments.is_empty() {
        return Err(HlsError::NoSegments);
    }
    Ok(segments)
}

/// The URLs of the AES-128 keys `text` names, each once, in playlist order. Lines the walk in
/// [`parse_media_playlist`] rejects are skipped here and left for it to report.
fn key_urls(text: &str, base: &Url) -> Vec<Url> {
    let mut seen = HashSet::new();
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("#EXT-X-KEY:"))
        .filter_map(|list| {
            let attrs = parse_attributes(list);
            let identity = attrs.get("KEYFORMAT").is_none_or(|f| *f == "identity");
            if attrs.get("METHOD") != Some(&"AES-128") || !identity {
                return None;
            }
            base.join(attrs.get("URI")?).ok()
        })
        .filter(|url| seen.insert(url.clone()))
        .collect()
}

/// Fetches a 16-byte AES key; the error is the reason it could not.
async fn fetch_key(client: &Client, auth: Option<&Auth>, url: &Url, fetch: FetchPolicy) -> Result<[u8; 16], String> {
    let (bytes, _) = fetch_with_retry(client, auth, url, None, MAX_KEY_BYTES, None, fetch)
        .await
        .map_err(|e| format!("could not fetch AES key {}: {}", url, e.reason))?;
    <[u8; 16]>::try_from(bytes.as_slice())
        .map_err(|_| format!("AES-128 key at {} is {} bytes, expected 16", url, bytes.len()))
}

#[derive(Debug, PartialEq, Eq)]
enum FetchErrorKind {
    /// Worth retrying: network trouble, 5xx, 408, 429, a short body.
    Transient,
    Fatal,
    /// The body is larger than the caller's limit; `playlist` if it starts like an HLS playlist.
    TooLarge { playlist: bool },
}

struct FetchError {
    kind: FetchErrorKind,
    reason: String,
}

impl FetchError {
    fn retryable(reason: impl ToString) -> Self {
        Self { kind: FetchErrorKind::Transient, reason: reason.to_string() }
    }

    fn fatal(reason: impl ToString) -> Self {
        Self { kind: FetchErrorKind::Fatal, reason: reason.to_string() }
    }

    /// `head` is the start of the body, as far as it was read.
    fn too_large(max_bytes: u64, head: &[u8]) -> Self {
        Self {
            kind: FetchErrorKind::TooLarge { playlist: is_playlist_start(head) },
            reason: format!("response larger than the {} byte limit", max_bytes),
        }
    }
}

/// Body bytes of one segment's requests, counted in a download's progress as they arrive. Dropped
/// before [`Tally::keep`], as a fetch that failed or lost the race to a second request is, it takes
/// them back out.
struct Tally<'a> {
    total: &'a AtomicU64,
    own: AtomicU64,
}

impl<'a> Tally<'a> {
    fn new(total: &'a AtomicU64) -> Self {
        Self { total, own: AtomicU64::new(0) }
    }

    fn add(&self, bytes: u64) {
        self.total.fetch_add(bytes, Ordering::Relaxed);
        self.own.fetch_add(bytes, Ordering::Relaxed);
    }

    fn take_back(&self, bytes: u64) {
        self.total.fetch_sub(bytes, Ordering::Relaxed);
        self.own.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// Leaves the bytes counted: they are the segment's.
    fn keep(self) {
        self.own.store(0, Ordering::Relaxed);
    }
}

impl Drop for Tally<'_> {
    fn drop(&mut self) {
        self.total.fetch_sub(*self.own.get_mut(), Ordering::Relaxed);
    }
}

/// One GET with header and idle timeouts of `stall`, reading at most `max_bytes` of body. Body
/// bytes are added to `progress` as they arrive and taken back out if the attempt fails.
async fn fetch_once(
    client: &Client,
    auth: Option<&Auth>,
    url: &Url,
    range: Option<ByteRange>,
    max_bytes: u64,
    progress: Option<&Tally<'_>>,
    stall: Duration,
) -> Result<(Vec<u8>, Url), FetchError> {
    let mut req = authorize(client.get(url.clone()), auth, url);
    if let Some(r) = range {
        req = req.header(reqwest::header::RANGE, r.to_http_header());
    }
    let mut resp = tokio::time::timeout(stall, req.send())
        .await
        .map_err(|_| FetchError::retryable("timed out waiting for a response"))?
        .map_err(FetchError::retryable)?;

    let status = resp.status();
    if !status.is_success() {
        let transient = status.is_server_error()
            || status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::TOO_MANY_REQUESTS;
        let reason = format!("HTTP {}", status);
        return Err(if transient { FetchError::retryable(reason) } else { FetchError::fatal(reason) });
    }
    if let Some(r) = range {
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(FetchError::fatal(format!(
                "server ignored the byte range request ({}), answered HTTP {}",
                r.to_http_header(),
                status
            )));
        }
    }

    // Of a body declared too large only the start is read, to tell what it is.
    let declared_too_large = resp.content_length().is_some_and(|len| len > max_bytes);
    let read_limit = if declared_too_large { PEEK_BYTES.min(max_bytes) } else { max_bytes };

    let final_url = resp.url().clone();
    let mut body = Vec::new();
    let result = loop {
        match tokio::time::timeout(stall, resp.chunk()).await {
            Err(_) => break Err(FetchError::retryable("connection stalled")),
            Ok(Err(e)) => break Err(FetchError::retryable(e)),
            Ok(Ok(None)) if declared_too_large => break Err(FetchError::too_large(max_bytes, &body)),
            Ok(Ok(None)) => break Ok(()),
            Ok(Ok(Some(chunk))) => {
                if body.len() as u64 + chunk.len() as u64 > read_limit {
                    let head: Vec<u8> = body.iter().chain(chunk.iter()).take(PEEK_BYTES as usize).copied().collect();
                    break Err(FetchError::too_large(max_bytes, &head));
                }
                if let Some(progress) = progress {
                    progress.add(chunk.len() as u64);
                }
                body.extend_from_slice(&chunk);
            }
        }
    };
    let result = result.and_then(|()| match range {
        Some(r) if body.len() as u64 != r.len() => Err(FetchError::retryable(format!(
            "expected {} bytes for range, got {}",
            r.len(),
            body.len()
        ))),
        _ => Ok(()),
    });
    if let Err(e) = result {
        if let Some(progress) = progress {
            progress.take_back(body.len() as u64);
        }
        return Err(e);
    }
    Ok((body, final_url))
}

/// `fetch_once` with exponential backoff, as often as `fetch` allows; client errors other than
/// 408/429 are not retried.
async fn fetch_with_retry(
    client: &Client,
    auth: Option<&Auth>,
    url: &Url,
    range: Option<ByteRange>,
    max_bytes: u64,
    progress: Option<&Tally<'_>>,
    fetch: FetchPolicy,
) -> Result<(Vec<u8>, Url), FetchError> {
    let mut delay = RETRY_BASE_DELAY;
    let mut failures = 0;
    loop {
        match fetch_once(client, auth, url, range, max_bytes, progress, fetch.stall_timeout).await {
            Ok(fetched) => return Ok(fetched),
            Err(e) if e.kind == FetchErrorKind::Transient && failures < fetch.max_retries => {
                failures += 1;
                tracing::warn!("HLS fetch of {} failed (attempt {}): {}; retrying", url, failures, e.reason);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_MAX_DELAY);
            }
            Err(mut e) => {
                e.reason = format!("{} (after {} attempt(s))", e.reason, failures + 1);
                return Err(e);
            }
        }
    }
}

async fn decrypt(data: Vec<u8>, key: &Option<Aes128Key>) -> Result<Vec<u8>, String> {
    let Some(key) = key.clone() else { return Ok(data) };
    tokio::task::spawn_blocking(move || {
        let mut data = data;
        let plain_len = cbc::Decryptor::<aes::Aes128>::new(&key.key.into(), &key.iv.into())
            .decrypt_padded_mut::<Pkcs7>(&mut data)
            .map_err(|_| "AES-128 decryption failed (wrong key or corrupt data)".to_string())?
            .len();
        data.truncate(plain_len);
        Ok(data)
    })
    .await
    .map_err(|e| format!("decryption task failed: {}", e))?
}

/// Downloads and decrypts one segment, prefixed by its init section when that changes. Its body
/// bytes count in `tally` as they arrive.
async fn fetch_segment(
    client: &Client,
    auth: Option<&Auth>,
    segment: &HlsSegment,
    with_init: bool,
    tally: &Tally<'_>,
    fetch: FetchPolicy,
) -> Result<Vec<u8>, HlsError> {
    let failed = |reason: String| HlsError::SegmentFailed { index: segment.index, reason };
    let limit = |range: Option<ByteRange>| range.map_or(MAX_SEGMENT_BYTES, |r| r.len().min(MAX_SEGMENT_BYTES));
    let mut init_data = None;
    if let Some(init) = segment.init.as_ref().filter(|_| with_init) {
        let (data, _) =
            fetch_with_retry(client, auth, &init.url, init.byte_range, limit(init.byte_range), Some(tally), fetch)
                .await
                .map_err(|e| failed(format!("init section {}: {}", init.url, e.reason)))?;
        init_data = Some(decrypt(data, &init.encryption).await.map_err(|r| failed(format!("init section: {}", r)))?);
    }
    let (data, _) =
        fetch_with_retry(client, auth, &segment.url, segment.byte_range, limit(segment.byte_range), Some(tally), fetch)
            .await
            .map_err(|e| failed(format!("{}: {}", segment.url, e.reason)))?;
    let data = decrypt(data, &segment.encryption).await.map_err(failed)?;
    Ok(match init_data {
        Some(mut out) => {
            out.extend_from_slice(&data);
            out
        }
        None => data,
    })
}

/// Downloads what `request` covers. Its body bytes count in `progress` as they arrive, and stay
/// counted only if it succeeds. Segments merged into one request are tried together once: a server
/// may cut a long range short or refuse it, so if that fails, each of them is fetched on its own,
/// with every retry `fetch` allows.
async fn fetch_request(
    client: &Client,
    auth: Option<&Auth>,
    request: &Request,
    progress: &AtomicU64,
    fetch: FetchPolicy,
) -> Result<Vec<u8>, HlsError> {
    let first = &request.segment;
    if !request.merged.is_empty() {
        let tally = Tally::new(progress);
        let once = FetchPolicy { max_retries: 0, ..fetch };
        match fetch_segment(client, auth, first, request.with_init, &tally, once).await {
            Ok(data) => {
                tally.keep();
                return Ok(data);
            }
            Err(e) => tracing::warn!("{}; fetching those {} segments one at a time", e, request.merged.len()),
        }
    }
    let tally = Tally::new(progress);
    let data = if request.merged.is_empty() {
        fetch_segment(client, auth, first, request.with_init, &tally, fetch).await?
    } else {
        let mut data = Vec::new();
        for (i, range) in request.merged.iter().enumerate() {
            let segment = HlsSegment { index: first.index + i, byte_range: Some(*range), ..first.clone() };
            data.extend(fetch_segment(client, auth, &segment, request.with_init && i == 0, &tally, fetch).await?);
        }
        data
    };
    tally.keep();
    Ok(data)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Identifies a segment list across runs. `exact` hashes the full segment URLs; `stream` hashes
/// the playlist URL and the segment URLs, all without their queries. CDN tokens in the query often
/// change on every playlist fetch, yet the query can also be all that tells two streams apart, so
/// a `stream`-only match is trusted only once the stream's bytes confirm it (see
/// [`HlsEngine::prepare`]).
struct Fingerprints {
    exact: String,
    stream: String,
}

impl Fingerprints {
    fn of(playlist: &Url, segments: &[HlsSegment]) -> Self {
        let (mut exact, mut stream) = (blake3::Hasher::new(), blake3::Hasher::new());
        stream.update(playlist[..url::Position::AfterPath].as_bytes());
        stream.update(b"\n");
        for s in segments {
            exact.update(s.url.as_str().as_bytes());
            stream.update(s.url[..url::Position::AfterPath].as_bytes());
            for hasher in [&mut exact, &mut stream] {
                if let Some(r) = s.byte_range {
                    hasher.update(r.to_http_header().as_bytes());
                }
                hasher.update(b"\n");
            }
        }
        Self { exact: exact.finalize().to_hex().to_string(), stream: stream.finalize().to_hex().to_string() }
    }
}

/// Reads `<exact fingerprint> <stream fingerprint> <segments written> <bytes written>` from the
/// resume file, or `None` if there is none or it is not one.
async fn read_state(state_path: &Path) -> Option<(String, String, usize, u64)> {
    let state = tokio::fs::read_to_string(state_path).await.ok()?;
    let mut fields = state.split_whitespace();
    let (exact, stream) = (fields.next()?.to_string(), fields.next()?.to_string());
    Some((exact, stream, fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

/// Whether segment `i` is written with its init section: the first segment that has one does,
/// and so does every segment where it changes.
fn writes_init(segments: &[HlsSegment], i: usize) -> bool {
    segments[i].init.is_some() && (i == 0 || segments[i - 1].init != segments[i].init)
}

/// One request of a download: a segment (with its init section if [`writes_init`]), or several
/// consecutive segments whose byte ranges it fetches together.
struct Request {
    /// The (first) segment, with the byte range of all of them.
    segment: HlsSegment,
    with_init: bool,
    /// The byte ranges of the segments fetched together; empty for a single segment.
    merged: Vec<ByteRange>,
}

impl Request {
    /// How many segments it fetches.
    fn segments(&self) -> usize {
        self.merged.len().max(1)
    }
}

/// Most bytes one request may fetch for several segments with `connections` connections: the
/// requests held in memory at once (the window, the writer's queue and the one it writes, and a
/// second request) share [`MERGE_BUDGET`].
fn merge_limit(connections: usize) -> u64 {
    let held = WINDOW_PER_CONNECTION * connections + WRITE_QUEUE + 2;
    (MERGE_BUDGET / held as u64).min(MAX_MERGED_BYTES)
}

/// The requests that fetch `segments[from..]`: one per segment, except that consecutive byte
/// ranges of one file, each starting where the one before ends, are fetched together, up to `limit`
/// bytes at a time. Encrypted segments are never merged, as each is decrypted on its own with its
/// own IV, and neither is a segment its init section must precede.
fn plan_requests(segments: Vec<HlsSegment>, from: usize, limit: u64) -> Vec<Request> {
    let with_init: Vec<bool> = (0..segments.len()).map(|i| writes_init(&segments, i)).collect();
    let mut requests: Vec<Request> = Vec::new();
    for (segment, with_init) in segments.into_iter().zip(with_init).skip(from) {
        let merged =
            requests.last().filter(|_| !with_init).and_then(|last| merged_range(&last.segment, &segment, limit));
        match (merged, requests.last_mut()) {
            (Some(range), Some(last)) => {
                if last.merged.is_empty() {
                    last.merged.extend(last.segment.byte_range);
                }
                last.merged.extend(segment.byte_range);
                last.segment.byte_range = Some(range);
            }
            _ => requests.push(Request { segment, with_init, merged: Vec::new() }),
        }
    }
    requests
}

/// The byte range of `request` extended by that of `next`, if one request of at most `limit` bytes
/// may fetch both.
fn merged_range(request: &HlsSegment, next: &HlsSegment, limit: u64) -> Option<ByteRange> {
    let (range, more) = (request.byte_range?, next.byte_range?);
    let adjoining = range.end.checked_add(1) == Some(more.start) && request.url == next.url;
    let plain = request.encryption.is_none() && next.encryption.is_none();
    if !adjoining || !plain || range.len() + more.len() > limit {
        return None;
    }
    ByteRange::new(range.start, more.end).ok()
}

/// Whether the `count` segments written to `part_path` (`bytes` long) are these: the first and the
/// last of them, fetched again, must equal the bytes that start and end the `.part`.
#[allow(clippy::too_many_arguments)]
async fn part_matches(
    client: &Client,
    auth: Option<&Auth>,
    segments: &[HlsSegment],
    part_path: &Path,
    count: usize,
    bytes: u64,
    fetch: FetchPolicy,
    cancel_flag: &Option<Arc<AtomicBool>>,
) -> Result<bool, HlsError> {
    for index in std::iter::once(0).chain((count > 1).then_some(count - 1)) {
        let uncounted = AtomicU64::new(0);
        let tally = Tally::new(&uncounted);
        let segment = fetch_segment(client, auth, &segments[index], writes_init(segments, index), &tally, fetch);
        let data = tokio::select! {
            data = segment => data?,
            _ = cancelled(cancel_flag) => return Err(HlsError::Cancelled),
        };
        let len = data.len() as u64;
        let offset = if index == 0 { (len <= bytes).then_some(0) } else { bytes.checked_sub(len) };
        match offset {
            Some(offset) if part_holds(part_path, offset, &data).await? => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Whether `part_path` holds `expected` at `offset`.
async fn part_holds(part_path: &Path, offset: u64, expected: &[u8]) -> std::io::Result<bool> {
    let mut file = tokio::fs::File::open(part_path).await?;
    file.seek(SeekFrom::Start(offset)).await?;
    let mut actual = vec![0; expected.len()];
    file.read_exact(&mut actual).await?;
    Ok(actual == expected)
}

/// Completes once `cancel_flag` is set; never without one.
async fn cancelled(cancel_flag: &Option<Arc<AtomicBool>>) {
    let mut ticker = tokio::time::interval(SNAPSHOT_INTERVAL);
    loop {
        ticker.tick().await;
        if cancel_flag.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
            return;
        }
    }
}

/// A unit of an [`InOrder`] download that was started and not yet handed out.
enum Slot<Fut> {
    /// Requested at `since`, by `requests` requests at once; `hedge` starts the second one, and
    /// dropping it unused drops the first (see [`race`]).
    Fetching { since: tokio::time::Instant, requests: usize, hedge: Option<oneshot::Sender<Fut>> },
    /// Its data, or why it could not be fetched.
    Done(Result<Vec<u8>, HlsError>),
}

type Job<'a> = Pin<Box<dyn Future<Output = (usize, Result<Vec<u8>, HlsError>)> + Send + 'a>>;

/// Fetches units `0..count` with `fetch`, at most `connections` requests at once, and hands them
/// out in order. The units fetched or waiting to be handed out span at most `window` from the next
/// one on, which bounds memory: a slow unit does not idle the other connections, which go on with
/// the units after it until the window is full. Once a connection would sit idle while the next
/// unit is still being fetched, and that fetch has taken twice as long as a typical one (at least
/// [`HEDGE_MIN_WAIT`]; half the stall timeout, at most [`FIRST_HEDGE_MAX_WAIT`], while no unit has
/// been fetched yet), the unit is requested a second time, never a third: the first of the two to
/// succeed is used and the other is dropped, which aborts it. A unit that fails ends the download
/// once every unit before it has been handed out; the units after it are dropped.
struct InOrder<'a, F, Fut> {
    fetch: F,
    /// Units to hand out: all of them, or up to the first one known to have failed.
    count: usize,
    connections: usize,
    window: usize,
    /// How long the next unit may take before it is requested again, while no typical fetch time
    /// is known.
    first_hedge: Duration,
    /// The next unit to hand out; `slots` holds it and the units after it that were started.
    head: usize,
    slots: VecDeque<Slot<Fut>>,
    jobs: FuturesUnordered<Job<'a>>,
    /// Requests in flight: a unit requested twice counts twice.
    requests: usize,
    /// Time the units fetched by a single request took, and how many they were.
    fetch_time: Duration,
    fetched: u32,
}

impl<'a, F, Fut> InOrder<'a, F, Fut>
where
    F: Fn(usize) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, HlsError>> + Send + 'a,
{
    /// `stall` is the stall timeout of a request.
    fn new(count: usize, connections: usize, window: usize, stall: Duration, fetch: F) -> Self {
        Self {
            fetch,
            count,
            connections,
            window,
            first_hedge: (stall / 2).clamp(HEDGE_MIN_WAIT, FIRST_HEDGE_MAX_WAIT),
            head: 0,
            slots: VecDeque::new(),
            jobs: FuturesUnordered::new(),
            requests: 0,
            fetch_time: Duration::ZERO,
            fetched: 0,
        }
    }

    /// Whether every unit has been handed out.
    fn is_done(&self) -> bool {
        self.head == self.count
    }

    /// Whether the next unit has arrived, or failed.
    fn head_ready(&self) -> bool {
        matches!(self.slots.front(), Some(Slot::Done(_)))
    }

    /// The next unit and its data, or the error that ends the download, once it has arrived.
    fn pop(&mut self) -> Option<Result<(usize, Vec<u8>), HlsError>> {
        match self.slots.pop_front()? {
            Slot::Done(result) => {
                self.head += 1;
                Some(result.map(|data| (self.head - 1, data)))
            }
            fetching => {
                self.slots.push_front(fetching);
                None
            }
        }
    }

    /// Waits until a unit has been fetched (or failed), or the next one is due to be requested
    /// again.
    async fn progress(&mut self) {
        self.launch();
        let hedge_at = self.hedge_at();
        let lag = async move {
            match hedge_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            finished = self.jobs.next() => match finished {
                Some((unit, result)) => self.finish(unit, result),
                // Nothing in flight: every unit started waits to be handed out.
                None => std::future::pending().await,
            },
            () = lag => self.hedge(),
        }
    }

    /// Starts the units after the last one started, as far as connections and the window allow.
    fn launch(&mut self) {
        while self.requests < self.connections && self.slots.len() < self.window {
            let unit = self.head + self.slots.len();
            if unit == self.count {
                break;
            }
            let (hedge, second) = oneshot::channel();
            self.jobs.push(Box::pin(race(unit, (self.fetch)(unit), second)));
            let since = tokio::time::Instant::now();
            self.slots.push_back(Slot::Fetching { since, requests: 1, hedge: Some(hedge) });
            self.requests += 1;
        }
    }

    /// When the next unit is due to be requested again, if it may be: it is still being fetched
    /// by one request, and a connection is free for another (so nothing else can be started).
    fn hedge_at(&self) -> Option<tokio::time::Instant> {
        let Some(Slot::Fetching { since, hedge: Some(_), .. }) = self.slots.front() else { return None };
        if self.requests >= self.connections {
            return None;
        }
        let patience = match self.fetched {
            0 => self.first_hedge,
            fetched => (self.fetch_time / fetched * 2).max(HEDGE_MIN_WAIT),
        };
        Some(*since + patience)
    }

    /// Requests the next unit a second time.
    fn hedge(&mut self) {
        let Some(Slot::Fetching { requests, hedge, .. }) = self.slots.front_mut() else { return };
        let Some(hedge) = hedge.take() else { return };
        tracing::info!("HLS segment {} is slow; requesting it a second time", self.head);
        // Refused only when the first request has just finished: its answer is used.
        if hedge.send((self.fetch)(self.head)).is_ok() {
            *requests += 1;
            self.requests += 1;
        }
    }

    /// Records how `unit` ended. A failure makes it the last unit: those after it are dropped, and
    /// the ones still being fetched stop.
    fn finish(&mut self, unit: usize, result: Result<Vec<u8>, HlsError>) {
        // Units dropped after a failure end here too.
        let Some(at) = unit.checked_sub(self.head).filter(|&at| at < self.slots.len()) else { return };
        let Slot::Fetching { since, requests, .. } = &self.slots[at] else { return };
        let (since, requests) = (*since, *requests);
        self.requests -= requests;
        if result.is_err() {
            self.count = unit + 1;
            for dropped in self.slots.drain(at + 1..) {
                if let Slot::Fetching { requests, .. } = dropped {
                    self.requests -= requests;
                }
            }
        } else if requests == 1 {
            self.fetch_time += since.elapsed();
            self.fetched += 1;
        }
        self.slots[at] = Slot::Done(result);
    }
}

/// Fetches `unit` with `first` until it succeeds or fails, or until a second request for it arrives
/// on `hedge`: then the first of the two to succeed is used, and the other dropped. When both fail,
/// the first request's error is returned. If `hedge` is dropped unused, the unit is no longer
/// wanted: `first` is dropped as well.
async fn race<Fut>(unit: usize, first: Fut, hedge: oneshot::Receiver<Fut>) -> (usize, Result<Vec<u8>, HlsError>)
where
    Fut: Future<Output = Result<Vec<u8>, HlsError>>,
{
    tokio::pin!(first);
    let second = tokio::select! {
        biased;
        result = &mut first => return (unit, result),
        second = hedge => match second {
            Ok(second) => second,
            Err(_) => return (unit, Err(HlsError::Cancelled)),
        },
    };
    tokio::pin!(second);
    let result = tokio::select! {
        biased;
        result = &mut first => match result {
            Ok(data) => Ok(data),
            Err(e) => second.await.map_err(|_| e),
        },
        result = &mut second => match result {
            Ok(data) => Ok(data),
            Err(_) => first.await,
        },
    };
    (unit, result)
}

/// Where a download goes, and the stream the `.part` beside it holds, which its `.hlsstate` names
/// along with how many of the stream's segments (bytes) the `.part` holds.
struct ResumeState {
    target: PathBuf,
    fingerprints: Fingerprints,
}

impl ResumeState {
    fn part(&self) -> PathBuf {
        with_suffix(&self.target, ".part")
    }

    fn path(&self) -> PathBuf {
        with_suffix(&self.target, ".part.hlsstate")
    }

    /// Records that `part` holds `held` (segments, bytes). Its data reaches the disk first and the
    /// state file is replaced in one step, so the state never claims more than the `.part` holds.
    /// Blocking.
    fn save(&self, part: &std::fs::File, (segments, bytes): (usize, u64)) -> std::io::Result<()> {
        use std::io::Write;
        let path = self.path();
        if bytes > 0 {
            #[cfg(test)]
            tests::DiskGate::pass(&path, tests::DiskOp::Flush);
            part.sync_data()?;
        }
        let tmp = with_suffix(&path, ".tmp");
        let mut file = std::fs::File::create(&tmp)?;
        let Fingerprints { exact, stream } = &self.fingerprints;
        write!(file, "{} {} {} {}", exact, stream, segments, bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path)
    }
}

/// Hashes the first `len` bytes of `part`, which an earlier run wrote, with `hasher`, a step at a
/// time so that it can stop once `received` closes: returns whether it got through. Blocking.
fn hash_part(
    hasher: &mut StreamHasher,
    part: &std::fs::File,
    len: u64,
    received: &mpsc::Receiver<(usize, Vec<u8>)>,
) -> std::io::Result<bool> {
    while hasher.position() < len {
        if received.is_closed() {
            return Ok(false);
        }
        hasher.update_from_file(part, (hasher.position() + PART_HASH_STEP).min(len))?;
    }
    Ok(true)
}

/// Appends the data it receives, with how many segments each piece holds, to `part`, which holds
/// `held` (segments, bytes) of the stream `state` names. The state is saved at most every
/// [`PERSIST_INTERVAL`], by a thread of its own: writing goes on while the `.part` is flushed for
/// it. With all `total` segments written, flushes the file to disk if `fsync`, gives it its final
/// name and returns its digest, taken with `hasher` as it was written (after what an earlier run
/// wrote), so it never has to be read back. If `received` closes before that (the download failed,
/// was cancelled or dropped), stops hashing what an earlier run wrote, writes what it was given,
/// saves the state and returns `None`. Blocking: runs on a thread of its own, so the fetches never
/// wait for the disk unless its queue is full.
fn write_segments(
    mut part: std::fs::File,
    state: &ResumeState,
    mut held: (usize, u64),
    total: usize,
    fsync: bool,
    mut hasher: StreamHasher,
    mut received: mpsc::Receiver<(usize, Vec<u8>)>,
) -> std::io::Result<Option<FileDigest>> {
    use std::io::{Seek, Write};
    use std::sync::mpsc::TrySendError;
    let hashed = hash_part(&mut hasher, &part, held.1, &received)?;
    part.seek(SeekFrom::Start(held.1))?;
    let flusher = part.try_clone()?;
    let mut saved = held.0;
    std::thread::scope(|scope| -> std::io::Result<()> {
        // A save is handed over only while the saver waits for one, so one runs at a time.
        let (to_saver, saves) = std::sync::mpsc::sync_channel(0);
        let saver = scope.spawn(move || saves.into_iter().try_for_each(|held| state.save(&flusher, held)));
        let mut saved_at = Instant::now();
        while held.0 < total {
            let Some((segments, data)) = received.blocking_recv() else { break };
            #[cfg(test)]
            tests::DiskGate::pass(&state.path(), tests::DiskOp::Write);
            part.write_all(&data)?;
            if hashed {
                hasher.update(&data);
            }
            held = (held.0 + segments, held.1 + data.len() as u64);
            if saved_at.elapsed() >= PERSIST_INTERVAL {
                match to_saver.try_send(held) {
                    Ok(()) => (saved, saved_at) = (held.0, Instant::now()),
                    // Still saving: the next piece tries again.
                    Err(TrySendError::Full(_)) => {}
                    // The saver failed; its error ends the download.
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
        }
        drop(to_saver);
        saver.join().map_err(|_| std::io::Error::other("the HLS state saver panicked"))?
    })?;
    if hashed && held.0 == total && !received.is_closed() {
        if fsync {
            part.sync_data()?;
        }
        drop(part);
        std::fs::rename(state.part(), &state.target)?;
        // Without its `.part` the state names nothing.
        let _ = std::fs::remove_file(state.path());
        return Ok(Some(hasher.finish()));
    }
    if held.0 > saved {
        state.save(&part, held)?;
    }
    Ok(None)
}

/// Where an HLS download goes and how much of its `.part` it keeps, from [`HlsEngine::prepare`].
pub struct HlsTarget {
    path: PathBuf,
    fingerprints: Fingerprints,
    resume_from: usize,
    resume_bytes: u64,
    claim: Option<Box<dyn Send>>,
}

impl HlsTarget {
    /// Keeps `claim`, the caller's claim on the target, until the download is done with its
    /// `.part`: that can be after the download's future was dropped, as its writer still writes
    /// what it was given and saves the state.
    pub fn hold(mut self, claim: impl Send + 'static) -> Self {
        self.claim = Some(Box::new(claim));
        self
    }
}

/// High-speed parallel HLS segment downloader and in-order stream stitcher.
pub struct HlsEngine;

impl HlsEngine {
    /// Decides how a download of `segments` (from `playlist`) to `output_path` starts. A `.part`
    /// of exactly this stream is resumed. One whose playlist and segment URLs match only up to
    /// their queries (rotated CDN tokens, or another stream told apart only by them) is resumed
    /// only if its first and last written segments, fetched again, equal its bytes. `None` means
    /// the `.part` belongs to another download: it stays untouched and needs another name.
    pub async fn prepare(
        client: &Client,
        auth: Option<&Auth>,
        playlist: &Url,
        segments: &[HlsSegment],
        output_path: &Path,
        fetch: FetchPolicy,
        cancel_flag: &Option<Arc<AtomicBool>>,
    ) -> Result<Option<HlsTarget>, HlsError> {
        let path = if output_path.extension().is_none() {
            output_path.with_extension(container_extension(segments))
        } else {
            output_path.to_path_buf()
        };
        let part_path = with_suffix(&path, ".part");
        let fingerprints = Fingerprints::of(playlist, segments);
        let part_len = match tokio::fs::metadata(&part_path).await {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(HlsTarget { path, fingerprints, resume_from: 0, resume_bytes: 0, claim: None }))
            }
            Err(e) => return Err(e.into()),
        };
        let Some((exact, stream, count, bytes)) = read_state(&with_suffix(&part_path, ".hlsstate")).await else {
            return Ok(None);
        };
        let resumable = (1..=segments.len()).contains(&count) && bytes <= part_len;
        let (resume_from, resume_bytes) = if exact == fingerprints.exact {
            // Surely this stream: resume it, or restart it in place if its state is unusable.
            if resumable { (count, bytes) } else { (0, 0) }
        } else if stream == fingerprints.stream
            && resumable
            && part_matches(client, auth, segments, &part_path, count, bytes, fetch, cancel_flag).await?
        {
            (count, bytes)
        } else {
            tracing::info!("{} holds another stream; leaving it alone", part_path.display());
            return Ok(None);
        };
        Ok(Some(HlsTarget { path, fingerprints, resume_from, resume_bytes, claim: None }))
    }

    /// Downloads `segments` with at most `options.connections` requests in flight (see
    /// [`InOrder`]) and writes them in order (see [`write_segments`]) to the `.part` of `target`
    /// (from [`HlsEngine::prepare`] for these segments), renamed to the target path once complete.
    /// Returns that path and the file's digest, taken while writing it. A failed or cancelled run
    /// keeps the `.part` file, with its state saved, and resumes from it next time; so does a run
    /// whose future is dropped, and its writer holds the target's claim (see [`HlsTarget::hold`])
    /// until it has saved it. `auth` is added to every request it covers.
    pub async fn download(
        client: &Client,
        auth: Option<&Auth>,
        segments: Vec<HlsSegment>,
        target: HlsTarget,
        options: &HlsOptions,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
        cancel_flag: Option<Arc<AtomicBool>>,
    ) -> Result<(PathBuf, FileDigest), HlsError> {
        let num_connections = options.connections.clamp(1, MAX_CONNECTIONS);
        let total_segments = segments.len();
        if total_segments == 0 {
            return Err(HlsError::NoSegments);
        }
        let is_cancelled = || cancel_flag.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));

        let HlsTarget { path: target_file, fingerprints, resume_from, resume_bytes: mut written_bytes, claim } = target;
        let part_path = with_suffix(&target_file, ".part");
        let state = ResumeState { target: target_file.clone(), fingerprints };
        let held = (resume_from, written_bytes);
        // Whatever touches the `.part` holds the claim, even if this future is dropped meanwhile.
        let (part, state, claim) = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
            if let Some(dir) = state.target.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let part =
                std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(state.part())?;
            part.set_len(held.1)?;
            // A new `.part` names its stream from the start: without a state it would pass for
            // another download's and be left behind.
            if held.0 == 0 {
                state.save(&part, held)?;
            }
            Ok((part, state, claim))
        })
        .await
        .map_err(std::io::Error::other)??;
        // Written on a thread of its own, fed by a short queue: the fetches go on while it writes.
        let (to_disk, queued) = mpsc::channel(WRITE_QUEUE);
        let (fsync, hasher) = (options.fsync_on_complete, StreamHasher::new(options.expected_checksum.as_deref()));
        let writer = tokio::task::spawn_blocking(move || {
            let _claim = claim;
            write_segments(part, &state, held, total_segments, fsync, hasher, queued)
        });

        tracing::info!(
            "Starting HLS ingestion: {} segments ({} already done) across {} streams -> {}",
            total_segments,
            resume_from,
            num_connections,
            target_file.display()
        );

        let requests = plan_requests(segments, resume_from, merge_limit(num_connections));
        let received = AtomicU64::new(written_bytes);
        // Dropping it aborts the fetches in flight.
        let window = WINDOW_PER_CONNECTION * num_connections;
        let mut window = InOrder::new(requests.len(), num_connections, window, options.fetch.stall_timeout, |i| {
            fetch_request(client, auth, &requests[i], &received, options.fetch)
        });

        // Segments handed to the writer.
        let mut written = resume_from;
        let start_time = Instant::now();
        let start_bytes = written_bytes;
        let mut ticker = tokio::time::interval(SNAPSHOT_INTERVAL);
        // Why the download stopped early; `None` once every segment is with the writer, or if the
        // writer failed, which its own error explains.
        let stopped = loop {
            if window.is_done() {
                break None;
            }
            tokio::select! {
                room = to_disk.reserve(), if window.head_ready() => {
                    let Ok(room) = room else { break None };
                    match window.pop() {
                        Some(Ok((i, data))) => {
                            written += requests[i].segments();
                            written_bytes += data.len() as u64;
                            room.send((requests[i].segments(), data));
                        }
                        Some(Err(e)) => break Some(e),
                        None => {}
                    }
                }
                () = window.progress() => {}
                _ = ticker.tick() => {
                    if is_cancelled() {
                        break Some(HlsError::Cancelled);
                    }
                    if let Some(tx) = &snapshot_tx {
                        let _ = tx.send(progress_snapshot(
                            written,
                            written_bytes,
                            received.load(Ordering::Relaxed),
                            total_segments,
                            num_connections,
                            (received.load(Ordering::Relaxed).saturating_sub(start_bytes)) as f64
                                / start_time.elapsed().as_secs_f64().max(0.001),
                            &target_file,
                        ));
                    }
                }
            }
        };
        // Stops the fetches, then lets the writer finish. Its queue stays open until then only if
        // every segment went to it: closed early, it tells the writer that the download stopped,
        // so it writes what it was given, saves the state and closes the file.
        drop(window);
        let queue = stopped.is_none().then_some(to_disk);
        let wrote = writer.await.map_err(std::io::Error::other).and_then(|wrote| wrote);
        drop(queue);
        if let Some(e) = stopped {
            if let Err(save) = wrote {
                tracing::warn!("Failed to save the HLS resume state of {}: {}", part_path.display(), save);
            }
            return Err(e);
        }
        // With every segment written the writer has named the file.
        let digest = wrote?.ok_or_else(|| std::io::Error::other("the HLS writer stopped before the last segment"))?;

        if let Some(tx) = &snapshot_tx {
            let _ = tx.send(progress_snapshot(
                total_segments,
                written_bytes,
                written_bytes,
                total_segments,
                num_connections,
                0.0,
                &target_file,
            ));
        }
        tracing::info!("HLS download and stitching completed: {}", target_file.display());
        Ok((target_file, digest))
    }
}

/// Builds a progress snapshot, shown as up to 64 blocks of segments. The total size
/// is extrapolated from the average size of the segments already written.
fn progress_snapshot(
    written: usize,
    written_bytes: u64,
    received_bytes: u64,
    total_segments: usize,
    num_connections: usize,
    speed_bytes_per_sec: f64,
    target: &Path,
) -> EngineSnapshot {
    let est_total_bytes = if written > 0 {
        (written_bytes as u128 * total_segments as u128 / written as u128) as u64
    } else {
        0
    }
    .max(received_bytes);
    let total = total_segments.max(1);
    let display_count = total.min(64);
    let chunks = (0..display_count)
        .map(|i| {
            let seg_start = i * total / display_count;
            let seg_end = (i + 1) * total / display_count;
            let status = if seg_end <= written {
                "Completed"
            } else if seg_start < written + num_connections {
                "Downloading"
            } else {
                "Pending"
            };
            let range_start = (seg_start as u128 * est_total_bytes as u128 / total as u128) as u64;
            let range_end = (seg_end as u128 * est_total_bytes as u128 / total as u128) as u64;
            let chunk_total = range_end.saturating_sub(range_start).max(1);
            crate::chunk::ChunkSnapshot {
                id: i,
                range_start,
                range_end,
                downloaded_bytes: if seg_end <= written { chunk_total } else { 0 },
                total_bytes: chunk_total,
                status: status.to_string(),
                worker_id: Some(i % num_connections.max(1)),
            }
        })
        .collect();

    EngineSnapshot {
        total_bytes: est_total_bytes,
        downloaded_bytes: received_bytes,
        speed_bytes_per_sec,
        progress_ratio: written as f64 / total as f64,
        active_workers: num_connections,
        mirror_speeds: Vec::new(),
        chunks,
        target_path: Some(target.to_path_buf()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;
    use parking_lot::Mutex;
    use std::future::Future;
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::Semaphore;

    /// Response of the mock server: status, extra header lines, body.
    /// Status 0 means "never answer" (a stalled connection).
    pub(crate) type Reply = (u16, String, Vec<u8>);

    /// Minimal HTTP/1.1 server that also counts requests per path (query included). Paths under
    /// `/private/` answer 401 unless the request carries `Authorization: Bearer secret`.
    pub(crate) async fn serve(
        handler: impl Fn(&str, Option<ByteRange>) -> Reply + Send + Sync + 'static,
    ) -> (SocketAddr, Arc<Mutex<HashMap<String, usize>>>) {
        serve_async(move |path, range| std::future::ready(handler(path, range))).await
    }

    /// [`serve`] with a handler that may take its time to answer.
    pub(crate) async fn serve_async<F: Future<Output = Reply> + Send + 'static>(
        handler: impl Fn(&str, Option<ByteRange>) -> F + Send + Sync + 'static,
    ) -> (SocketAddr, Arc<Mutex<HashMap<String, usize>>>) {
        let handler = Arc::new(handler);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(HashMap::new()));
        let hits_srv = Arc::clone(&hits);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                let hits = Arc::clone(&hits_srv);
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let req = String::from_utf8_lossy(&req).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let range = req.lines().find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        let (start, end) = name.eq_ignore_ascii_case("range").then_some(())
                            .and(value.trim().strip_prefix("bytes="))?
                            .split_once('-')?;
                        ByteRange::new(start.parse().ok()?, end.parse().ok()?).ok()
                    });
                    *hits.lock().entry(path.clone()).or_insert(0) += 1;
                    let authorized = req.lines().any(|l| l.eq_ignore_ascii_case("authorization: Bearer secret"));
                    let (status, headers, body) = if path.starts_with("/private/") && !authorized {
                        (401, String::new(), Vec::new())
                    } else {
                        handler(&path, range).await
                    };
                    if status == 0 {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        return;
                    }
                    let head = format!(
                        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                        status,
                        body.len(),
                        headers
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (addr, hits)
    }

    pub(crate) fn ok(body: impl Into<Vec<u8>>) -> Reply {
        (200, String::new(), body.into())
    }

    /// What a [`DiskGate`] holds back.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum DiskOp {
        /// Writing a piece of the stream to the `.part`.
        Write,
        /// Flushing the `.part` to disk for a state save.
        Flush,
    }

    type Gates = Vec<(PathBuf, DiskOp, Arc<Mutex<std::sync::mpsc::Receiver<()>>>)>;
    static DISK_GATES: Mutex<Gates> = parking_lot::const_mutex(Vec::new());

    /// Stands in for a stuck disk: every `op` on the `.part` of a download under `dir` waits until
    /// the gate is dropped.
    pub(crate) struct DiskGate {
        dir: PathBuf,
        op: DiskOp,
        // Nothing is ever sent: waiting ends when it is dropped.
        _open: std::sync::mpsc::Sender<()>,
    }

    impl DiskGate {
        pub(crate) fn close(dir: &Path, op: DiskOp) -> Self {
            let (open, waiting) = std::sync::mpsc::channel();
            DISK_GATES.lock().push((dir.to_path_buf(), op, Arc::new(Mutex::new(waiting))));
            Self { dir: dir.to_path_buf(), op, _open: open }
        }

        /// Runs before `op` on the files of the download whose state file is `state`.
        pub(crate) fn pass(state: &Path, op: DiskOp) {
            let gates = DISK_GATES.lock();
            let gate = gates.iter().find(|(dir, gated, _)| *gated == op && state.starts_with(dir)).map(|(.., g)| Arc::clone(g));
            drop(gates);
            if let Some(gate) = gate {
                let _ = gate.lock().recv();
            }
        }
    }

    impl Drop for DiskGate {
        fn drop(&mut self) {
            DISK_GATES.lock().retain(|(dir, op, _)| (dir, *op) != (&self.dir, self.op));
        }
    }

    /// Waits up to ten seconds for `done`.
    pub(crate) async fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The segments and bytes the state of `out` says its `.part` holds.
    fn saved(out: &Path) -> Option<(usize, u64)> {
        let state = std::fs::read_to_string(with_suffix(out, ".part.hlsstate")).ok()?;
        let fields: Vec<&str> = state.split_whitespace().collect();
        Some((fields.get(2)?.parse().ok()?, fields.get(3)?.parse().ok()?))
    }

    fn part_len(out: &Path) -> Option<u64> {
        std::fs::metadata(with_suffix(out, ".part")).map(|m| m.len()).ok()
    }

    /// A media playlist of `count` segments `s0.ts`, `s1.ts`, ...
    fn numbered_playlist(count: usize) -> Reply {
        ok(format!("#EXTM3U\n{}#EXT-X-ENDLIST\n", (0..count).map(|n| format!("#EXTINF:4,\ns{n}.ts\n")).collect::<String>()))
    }

    /// The body of `/s<n>.ts`: its number, four times.
    fn numbered_segment(path: &str) -> Reply {
        ok(path.trim_start_matches("/s").trim_end_matches(".ts").repeat(4))
    }

    /// A short stall timeout keeps tests that stall quick.
    const FETCH: FetchPolicy = FetchPolicy { stall_timeout: Duration::from_millis(500), max_retries: 4 };

    fn options(connections: usize) -> HlsOptions {
        HlsOptions { connections, fetch: FETCH, fsync_on_complete: false, expected_checksum: None }
    }

    /// Prepares `out` for `segments` of `playlist`, which must be free or hold this stream, and
    /// downloads them there, checking the digest it returns against the file.
    async fn download_to(
        client: &Client,
        auth: Option<&Auth>,
        playlist: &Url,
        segments: Vec<HlsSegment>,
        out: &Path,
        num_connections: usize,
    ) -> Result<PathBuf, HlsError> {
        let target = HlsEngine::prepare(client, auth, playlist, &segments, out, FETCH, &None).await?.expect("usable name");
        let (path, digest) = HlsEngine::download(client, auth, segments, target, &options(num_connections), None, None).await?;
        assert_digest(&digest, &path);
        Ok(path)
    }

    /// `digest` must be the digest of the file at `path`.
    fn assert_digest(digest: &FileDigest, path: &Path) {
        let file = std::fs::read(path).unwrap();
        assert_eq!(digest.blake3_hex(), blake3::hash(&file).to_hex().as_str(), "digest of {}", path.display());
    }

    fn encrypt(plain: &[u8], key: [u8; 16], iv: [u8; 16]) -> Vec<u8> {
        let mut buf = plain.to_vec();
        buf.resize(plain.len() + 16 - plain.len() % 16, 0);
        cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .to_vec()
    }

    #[test]
    fn test_parse_best_variant() {
        let master = r#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
360p.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720
720p.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080
1080p.m3u8
"#;
        let base = Url::parse("https://cdn.example.com/hls/master.m3u8").unwrap();
        let best = select_variant(master, &base).unwrap();
        assert_eq!(best, Url::parse("https://cdn.example.com/hls/1080p.m3u8").unwrap());
    }

    #[test]
    fn test_variant_attributes_and_audio_preference() {
        // AVERAGE-BANDWIDTH must not be mistaken for BANDWIDTH, and quoted CODECS contain commas.
        let master = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="en",URI="audio/en.m3u8"
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=9000000,BANDWIDTH=1000000,CODECS="avc1.4d401f,mp4a.40.2"
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS="avc1.4d401f,mp4a.40.2"
mid.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=8000000,CODECS="avc1.640028,mp4a.40.2",AUDIO="aud"
high-silent.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=9000000,CODECS="avc1.640028"
video-only.m3u8
"#;
        let base = Url::parse("https://cdn.example.com/hls/master.m3u8").unwrap();
        assert_eq!(select_variant(master, &base).unwrap().as_str(), "https://cdn.example.com/hls/mid.m3u8");

        let demuxed = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="en",URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=8000000,CODECS="avc1.640028,mp4a.40.2",AUDIO="aud"
video.m3u8
"#;
        assert!(matches!(select_variant(demuxed, &base), Err(HlsError::Unsupported(_))));

        // An audio group whose rendition has no URI means the audio is muxed into the variant.
        let muxed_group = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="en",DEFAULT=YES
#EXT-X-STREAM-INF:BANDWIDTH=8000000,AUDIO="aud"
video.m3u8
"#;
        assert_eq!(select_variant(muxed_group, &base).unwrap().as_str(), "https://cdn.example.com/hls/video.m3u8");
    }

    #[test]
    fn test_parse_iv_and_attributes() {
        assert_eq!(parse_iv("0x00000000000000000000000000000001").unwrap()[15], 1);
        assert!(parse_iv("0x01").is_none());
        let attrs = parse_attributes(r#"METHOD=AES-128,URI="k.bin?a=1,b=2",IV=0x0A"#);
        assert_eq!(attrs["URI"], "k.bin?a=1,b=2");
        assert_eq!(attrs["IV"], "0x0A");
    }

    #[tokio::test]
    async fn test_parse_byterange_playlist() {
        let playlist = "#EXTM3U\n#EXT-X-VERSION:4\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n#EXT-X-BYTERANGE:1000@0\nmedia.ts\n#EXTINF:10.0,\n#EXT-X-BYTERANGE:2000\nmedia.ts\n#EXT-X-ENDLIST\n";
        let (addr, _) = serve(move |_, _| ok(playlist)).await;
        let url = Url::parse(&format!("http://{}/playlist.m3u8", addr)).unwrap();
        let segments = parse_hls_playlist(&Client::new(), &url, None, FETCH).await.unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].byte_range, Some(ByteRange::new(0, 999).unwrap()));
        assert_eq!(segments[1].byte_range, Some(ByteRange::new(1000, 2999).unwrap()));
    }

    #[tokio::test]
    async fn test_live_playlist_rejected_and_bom_redirect_handled() {
        let (addr, _) = serve(|path: &str, _| match path {
            "/live.m3u8" => ok("#EXTM3U\n#EXTINF:4,\nlive0.ts\n"),
            "/start/master.m3u8" => (302, "Location: /cdn/abc/master.m3u8\r\n".to_string(), Vec::new()),
            "/cdn/abc/master.m3u8" => ok("\u{feff}  \n#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv/media.m3u8\n"),
            "/cdn/abc/v/media.m3u8" => ok("#EXTM3U\n#EXTINF:4,\nseg0.ts\n#EXT-X-ENDLIST\n"),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        let live = Url::parse(&format!("http://{}/live.m3u8", addr)).unwrap();
        assert!(matches!(parse_hls_playlist(&client, &live, None, FETCH).await, Err(HlsError::Unsupported(_))));

        let start = Url::parse(&format!("http://{}/start/master.m3u8", addr)).unwrap();
        let segments = parse_hls_playlist(&client, &start, None, FETCH).await.unwrap();
        assert_eq!(segments[0].url.path(), "/cdn/abc/v/seg0.ts");
    }

    #[tokio::test]
    async fn test_keys_are_fetched_in_parallel_up_to_the_cap() {
        const KEYS: usize = KEY_CONCURRENCY + 4;
        let rotating: String =
            (0..KEYS).map(|n| format!("#EXT-X-KEY:METHOD=AES-128,URI=\"k{n}.bin\"\n#EXTINF:4,\ns{n}.ts\n")).collect();
        let rotating = format!("#EXTM3U\n{rotating}#EXT-X-ENDLIST\n");
        let (open, most, arrived) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let counters = (Arc::clone(&open), Arc::clone(&most), Arc::clone(&arrived));
        let (addr, hits) = serve_async(move |path: &str, _| {
            let key: Option<u8> = path.strip_prefix("/k").and_then(|k| k.strip_suffix(".bin")?.parse().ok());
            let reply = match path {
                "/rotating.m3u8" => ok(rotating.clone()),
                // A DRM key comes first: its error is the one reported, although the key after it is gone.
                "/drm-first.m3u8" => ok("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,KEYFORMAT=\"com.apple.streamingkeydelivery\",URI=\"skd://k\"\n\
                    #EXTINF:4,\na.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"gone.bin\"\n#EXTINF:4,\nb.ts\n#EXT-X-ENDLIST\n"),
                _ => (404, String::new(), Vec::new()),
            };
            let (open, most, arrived) = (Arc::clone(&counters.0), Arc::clone(&counters.1), Arc::clone(&counters.2));
            async move {
                let Some(key) = key else { return reply };
                most.fetch_max(open.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                arrived.fetch_add(1, Ordering::SeqCst);
                // No key is answered before as many requests as the cap allows have arrived (or two
                // seconds have passed), so all of those must be open at once.
                let deadline = Instant::now() + Duration::from_secs(2);
                while arrived.load(Ordering::SeqCst) < KEY_CONCURRENCY && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                open.fetch_sub(1, Ordering::SeqCst);
                ok(vec![key; 16])
            }
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{addr}/rotating.m3u8")).unwrap();
        // Patient: a key request given up on and sent again would count twice.
        let patient = FetchPolicy { stall_timeout: Duration::from_secs(10), ..FETCH };
        let segments = parse_hls_playlist(&client, &url, None, patient).await.unwrap();
        assert_eq!(most.load(Ordering::SeqCst), KEY_CONCURRENCY, "at most {KEY_CONCURRENCY} key requests at once, and that many");
        for (n, segment) in segments.iter().enumerate() {
            assert_eq!(segment.encryption.as_ref().unwrap().key, [n as u8; 16], "segment {n} has its own key");
        }

        let url = Url::parse(&format!("http://{addr}/drm-first.m3u8")).unwrap();
        assert!(matches!(parse_hls_playlist(&client, &url, None, FETCH).await, Err(HlsError::Unsupported(_))));
        assert!(!hits.lock().contains_key("/gone.bin"), "no key is fetched for a playlist rejected before it");
    }

    #[tokio::test]
    async fn test_a_missing_key_fails_the_parse_without_waiting_for_the_others() {
        let (addr, _) = serve(|path: &str, _| match path {
            "/media.m3u8" => {
                let keys: String = (0..3).map(|n| format!("#EXT-X-KEY:METHOD=AES-128,URI=\"k{n}.bin\"\n#EXTINF:4,\ns{n}.ts\n")).collect();
                ok(format!("#EXTM3U\n{keys}#EXT-X-ENDLIST\n"))
            }
            "/k0.bin" => (404, String::new(), Vec::new()),
            // The other keys never come.
            _ => (0, String::new(), Vec::new()),
        })
        .await;
        let url = Url::parse(&format!("http://{addr}/media.m3u8")).unwrap();
        let patient = FetchPolicy { stall_timeout: Duration::from_secs(10), max_retries: 0 };
        let started = Instant::now();
        let err = parse_hls_playlist(&Client::new(), &url, None, patient).await.unwrap_err();
        assert!(matches!(&err, HlsError::Unavailable(reason) if reason.contains("k0.bin")), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn test_master_recursion_is_depth_limited() {
        let (addr, hits) = serve(|_, _| ok("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nself.m3u8\n")).await;
        let url = Url::parse(&format!("http://{}/self.m3u8", addr)).unwrap();
        assert!(matches!(parse_hls_playlist(&Client::new(), &url, None, FETCH).await, Err(HlsError::Unsupported(_))));
        assert_eq!(hits.lock()["/self.m3u8"], MAX_MASTER_DEPTH + 1);
    }

    #[tokio::test]
    async fn test_download_aes128_with_init_map() {
        let key = [7u8; 16];
        let explicit_iv = [9u8; 16];
        let init = b"INIT-SECTION".to_vec();
        let seg0 = b"segment zero plaintext".to_vec();
        let seg1 = b"segment one, a little longer than one block".to_vec();
        let seg2 = b"segment two".to_vec();
        // Segments 0 and 1 use the media sequence number (5, 6) as IV, segment 2 an explicit IV.
        let enc0 = encrypt(&seg0, key, 5u128.to_be_bytes());
        let enc1 = encrypt(&seg1, key, 6u128.to_be_bytes());
        let enc2 = encrypt(&seg2, key, explicit_iv);
        let media = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:5\n#EXT-X-MAP:URI=\"init.mp4\"\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\ns0.m4s\n#EXTINF:4,\ns1.m4s\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x09090909090909090909090909090909\n#EXTINF:4,\ns2.m4s\n#EXT-X-ENDLIST\n";
        let flaky_calls = Arc::new(AtomicU64::new(0));
        let flaky = Arc::clone(&flaky_calls);
        let init_c = init.clone();
        let (addr, hits) = serve(move |path: &str, _| match path {
            "/master.m3u8" => ok("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"avc1.64001f,mp4a.40.2\"\nmedia.m3u8\n"),
            "/media.m3u8" => ok(media),
            "/key.bin" => ok(key.to_vec()),
            "/init.mp4" => ok(init_c.clone()),
            "/s0.m4s" => ok(enc0.clone()),
            // First request stalls (idle timeout), second is a 503, third succeeds.
            "/s1.m4s" => match flaky.fetch_add(1, Ordering::SeqCst) {
                0 => (0, String::new(), Vec::new()),
                1 => (503, String::new(), Vec::new()),
                _ => ok(enc1.clone()),
            },
            "/s2.m4s" => ok(enc2.clone()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;

        let client = Client::new();
        let url = Url::parse(&format!("http://{}/master.m3u8", addr)).unwrap();
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        assert_eq!(container_extension(&segments), "mp4");

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("video.mp4");
        let (tx, mut rx) = broadcast::channel(256);
        let target = HlsEngine::prepare(&client, None, &url, &segments, &out, FETCH, &None).await.unwrap().unwrap();
        // num_connections == 0 must be treated as 1, not panic.
        let (path, digest) = HlsEngine::download(&client, None, segments, target, &options(0), Some(tx), None).await.unwrap();

        let expected = [init, seg0, seg1, seg2].concat();
        assert_eq!(path, out);
        assert_eq!(std::fs::read(&out).unwrap(), expected);
        assert_digest(&digest, &out);
        assert!(!with_suffix(&out, ".part").exists());
        assert!(!with_suffix(&out, ".part.hlsstate").exists());
        let hits = hits.lock();
        assert_eq!(hits["/key.bin"], 1, "the key must be fetched once");
        assert_eq!(hits["/init.mp4"], 1, "an unchanged init section is written once");
        assert_eq!(hits["/s1.m4s"], 3);

        let mut last = None;
        while let Ok(s) = rx.try_recv() {
            assert!(s.progress_ratio <= 1.0);
            last = Some(s);
        }
        let last = last.unwrap();
        assert_eq!(last.progress_ratio, 1.0);
        assert_eq!(last.downloaded_bytes, expected.len() as u64);
    }

    #[tokio::test]
    async fn test_failed_segment_fails_download_then_resumes() {
        let broken = Arc::new(AtomicBool::new(true));
        let broken_srv = Arc::clone(&broken);
        let (addr, hits) = serve(move |path: &str, _| match path {
            "/media.m3u8" => ok("#EXTM3U\n#EXTINF:4,\na.ts\n#EXTINF:4,\nb.ts\n#EXTINF:4,\nc.ts\n#EXT-X-ENDLIST\n"),
            "/a.ts" => ok("AAAA"),
            "/b.ts" if broken_srv.load(Ordering::SeqCst) => (404, String::new(), Vec::new()),
            "/b.ts" => ok("BBBB"),
            "/c.ts" => ok("CCCC"),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{}/media.m3u8", addr)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("clip.ts");
        std::fs::write(&out, "previous download").unwrap();

        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        let err = download_to(&client, None, &url, segments, &out, 1).await.unwrap_err();
        assert!(matches!(err, HlsError::SegmentFailed { index: 1, .. }), "{err}");
        assert_eq!(std::fs::read(&out).unwrap(), b"previous download", "a failed run must not touch the final name");
        assert_eq!(hits.lock()["/b.ts"], 1, "404 is not retried");
        assert_eq!(std::fs::read(with_suffix(&out, ".part")).unwrap(), b"AAAA");

        broken.store(false, Ordering::SeqCst);
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        download_to(&client, None, &url, segments, &out, 4).await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"AAAABBBBCCCC");
        assert_eq!(hits.lock()["/a.ts"], 1, "segments already written are not fetched again");
    }

    #[tokio::test]
    async fn test_segments_before_a_failed_one_are_written() {
        // The last segment fails at once, the others answer a little later.
        let (addr, _) = serve_async(|path: &str, _| {
            let path = path.to_string();
            async move {
                match path.as_str() {
                    "/media.m3u8" => ok(format!("#EXTM3U\n{}#EXT-X-ENDLIST\n", (0..6).map(|n| format!("#EXTINF:4,\ns{n}.ts\n")).collect::<String>())),
                    "/s5.ts" => (404, String::new(), Vec::new()),
                    p => {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        ok(p.trim_start_matches("/s").trim_end_matches(".ts").repeat(4))
                    }
                }
            }
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{addr}/media.m3u8")).unwrap();
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("partial.ts");
        let err = download_to(&client, None, &url, segments, &out, 6).await.unwrap_err();
        assert!(matches!(err, HlsError::SegmentFailed { index: 5, .. }), "{err}");
        assert_eq!(std::fs::read(with_suffix(&out, ".part")).unwrap(), b"00001111222233334444");
        let state = std::fs::read_to_string(with_suffix(&out, ".part.hlsstate")).unwrap();
        assert!(state.ends_with(" 5 20"), "the segments before the failed one are kept: {state}");
    }

    #[tokio::test]
    async fn test_ranged_segment_rejects_full_body() {
        let (addr, _) = serve(|path: &str, range: Option<ByteRange>| match (path, range) {
            ("/media.m3u8", _) => ok("#EXTM3U\n#EXTINF:4,\n#EXT-X-BYTERANGE:4@0\nall.ts\n#EXT-X-ENDLIST\n"),
            ("/honest.ts", Some(r)) => (206, String::new(), b"0123456789"[r.start as usize..=r.end as usize].to_vec()),
            ("/all.ts", _) => ok("0123456789"),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{}/media.m3u8", addr)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        let err = download_to(&client, None, &url, segments.clone(), &dir.path().join("a.ts"), 2).await.unwrap_err();
        assert!(err.to_string().contains("ignored the byte range"), "{err}");

        let mut honest = segments;
        honest[0].url.set_path("/honest.ts");
        let path = download_to(&client, None, &url, honest, &dir.path().join("b.ts"), 2).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"0123");
    }

    #[tokio::test]
    async fn test_segment_bodies_are_size_limited() {
        let big = vec![b'x'; MAX_SEGMENT_BYTES as usize + 1];
        let (addr, hits) = serve(move |path: &str, range: Option<ByteRange>| match (path, range) {
            ("/big.m3u8", _) => ok("#EXTM3U\n#EXTINF:4,\nbig.ts\n#EXT-X-ENDLIST\n"),
            ("/big.ts", _) => ok(big.clone()),
            ("/ranged.m3u8", _) => ok("#EXTM3U\n#EXTINF:4,\n#EXT-X-BYTERANGE:4@0\nover.ts\n#EXT-X-ENDLIST\n"),
            // Answers the range with more bytes than were asked for.
            ("/over.ts", Some(_)) => (206, String::new(), b"0123456789".to_vec()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        let dir = tempfile::tempdir().unwrap();
        for (playlist, segment) in [("/big.m3u8", "/big.ts"), ("/ranged.m3u8", "/over.ts")] {
            let url = Url::parse(&format!("http://{}{}", addr, playlist)).unwrap();
            let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
            let out = dir.path().join(&segment[1..]);
            let err = download_to(&client, None, &url, segments, &out, 1).await.unwrap_err();
            assert!(matches!(err, HlsError::SegmentFailed { index: 0, .. }), "{err}");
            assert!(err.to_string().contains("byte limit"), "{err}");
            assert_eq!(hits.lock()[segment], 1, "an oversized body is not retried");
        }
    }

    #[tokio::test]
    async fn test_only_a_non_playlist_url_is_invalid_playlist() {
        let oversized = |head: &str| {
            let mut body = head.as_bytes().to_vec();
            body.resize(MAX_PLAYLIST_BYTES as usize + 1, b'\n');
            ok(body)
        };
        let (addr, _) = serve(move |path: &str, _| match path {
            "/page.m3u8" => ok("<html>not a playlist</html>"),
            "/channels.m3u8.zip" => oversized("PK\u{3}\u{4}"),
            "/huge.m3u8" => oversized("\u{feff}\n#EXTM3U\n#EXT-X-TARGETDURATION:4\n"),
            "/huge-variant.m3u8" => ok("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nchannels.m3u8.zip\n"),
            "/keyless.m3u8" => ok("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"gone.key\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n"),
            "/master.m3u8" => ok("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nexpired.m3u8\n"),
            "/expired.m3u8" => ok("<html>token expired</html>"),
            "/mapless.m3u8" => ok("#EXTM3U\n#EXT-X-MAP:BYTERANGE=\"4@0\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n"),
            "/uri-less.m3u8" => ok("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n"),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        let parse = |path: &str| {
            let url = Url::parse(&format!("http://{}{}", addr, path)).unwrap();
            let client = client.clone();
            async move { parse_hls_playlist(&client, &url, None, FETCH).await }
        };
        assert!(matches!(parse("/missing.m3u8").await, Err(HlsError::Unavailable(_))));
        assert!(matches!(parse("/keyless.m3u8").await, Err(HlsError::Unavailable(_))));
        assert!(matches!(parse("/page.m3u8").await, Err(HlsError::InvalidPlaylist(_))));
        // A large ordinary file whose URL mentions .m3u8 falls back to a plain download, but a
        // real playlist too large to read, or an oversized variant, does not.
        assert!(matches!(parse("/channels.m3u8.zip").await, Err(HlsError::InvalidPlaylist(_))));
        assert!(matches!(parse("/huge.m3u8").await, Err(HlsError::Unsupported(_))));
        assert!(matches!(parse("/huge-variant.m3u8").await, Err(HlsError::Unsupported(_))));
        // Inside a real stream nothing may look like "not a playlist", or the engine would save
        // the playlist text as the download.
        assert!(matches!(parse("/master.m3u8").await, Err(HlsError::Unavailable(_))));
        assert!(matches!(parse("/mapless.m3u8").await, Err(HlsError::Unsupported(_))));
        assert!(matches!(parse("/uri-less.m3u8").await, Err(HlsError::Unsupported(_))));
    }

    #[tokio::test]
    async fn test_authorization_reaches_playlist_key_and_segments() {
        let key = [3u8; 16];
        let (addr, _) = serve(move |path: &str, _| match path {
            "/private/master.m3u8" => ok("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmedia.m3u8\n"),
            "/private/media.m3u8" => ok("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\",IV=0x00000000000000000000000000000001\n\
                #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\ns.m4s\n#EXT-X-ENDLIST\n"),
            "/private/k.bin" => ok(key.to_vec()),
            "/private/init.mp4" => ok(encrypt(b"INIT", key, 1u128.to_be_bytes())),
            "/private/s.m4s" => ok(encrypt(b"DATA", key, 1u128.to_be_bytes())),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{}/private/master.m3u8", addr)).unwrap();
        let auth = Auth::new("Bearer secret", std::slice::from_ref(&url)).unwrap();
        assert!(matches!(parse_hls_playlist(&client, &url, None, FETCH).await, Err(HlsError::Unavailable(_))));

        let segments = parse_hls_playlist(&client, &url, Some(&auth), FETCH).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = download_to(&client, None, &url, segments.clone(), &dir.path().join("public.mp4"), 1).await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
        let out = dir.path().join("private.mp4");
        download_to(&client, Some(&auth), &url, segments, &out, 1).await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"INITDATA");
    }

    #[tokio::test]
    async fn test_resume_never_splices_or_overwrites_another_stream() {
        fn media(v: &str) -> Reply {
            let segments: String = (0..3).map(|n| format!("#EXTINF:4,\n/seg.ts?v={v}&n={n}\n")).collect();
            ok(format!("#EXTM3U\n{segments}#EXT-X-ENDLIST\n"))
        }
        let broken = Arc::new(AtomicBool::new(true));
        let broken_srv = Arc::clone(&broken);
        // Every stream opens with the same intro and is told apart by queries only.
        let (addr, hits) = serve(move |path: &str, _| match path {
            "/one/index.m3u8" => media("one"),
            "/two/index.m3u8" => media("two"),
            "/video/index.m3u8?q=720" => media("720"),
            "/video/index.m3u8?q=1080" => media("1080"),
            p if p.ends_with("&n=0") => ok("INTRO "),
            p if p.ends_with("&n=2") && broken_srv.load(Ordering::SeqCst) => (404, String::new(), Vec::new()),
            p => ok(format!("{p} ")),
        })
        .await;
        let client = Client::new();
        let dir = tempfile::tempdir().unwrap();
        let playlist = |path: &str| Url::parse(&format!("http://{addr}{path}")).unwrap();
        let prepare = |path: &'static str, out: PathBuf| {
            let client = client.clone();
            async move {
                let url = playlist(path);
                let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
                let target = HlsEngine::prepare(&client, None, &url, &segments, &out, FETCH, &None).await.unwrap();
                (target, segments)
            }
        };
        let fetches = |v: &str| hits.lock().iter().filter(|(p, _)| p.contains(&format!("v={v}&"))).map(|(_, n)| n).sum::<usize>();

        let pairs = [("/one/index.m3u8", "/two/index.m3u8"), ("/video/index.m3u8?q=720", "/video/index.m3u8?q=1080")];
        for (i, (paused, other)) in pairs.into_iter().enumerate() {
            let out = dir.path().join(format!("{i}.ts"));
            let (target, segments) = prepare(paused, out.clone()).await;
            let err = HlsEngine::download(&client, None, segments, target.unwrap(), &options(1), None, None).await.unwrap_err();
            assert!(matches!(err, HlsError::SegmentFailed { index: 2, .. }), "{err}");
            let part = std::fs::read(with_suffix(&out, ".part")).unwrap();
            let state = std::fs::read(with_suffix(&out, ".part.hlsstate")).unwrap();

            // Another stream whose URLs differ only by queries and which starts the same: the
            // other playlist, or else the last written segment, tells them apart.
            let (target, _) = prepare(other, out.clone()).await;
            assert!(target.is_none(), "{other} must not take over the .part of {paused}");
            assert_eq!(std::fs::read(with_suffix(&out, ".part")).unwrap(), part, "the paused .part is untouched");
            assert_eq!(std::fs::read(with_suffix(&out, ".part.hlsstate")).unwrap(), state);
        }
        assert_eq!(fetches("two"), 0, "another playlist is rejected without fetching anything");
        assert_eq!(fetches("1080"), 2, "the first and the last written segment are checked");

        // An exact match resumes without checking any segment again.
        broken.store(false, Ordering::SeqCst);
        let out = dir.path().join("1.ts");
        let before = fetches("720");
        let (target, segments) = prepare("/video/index.m3u8?q=720", out.clone()).await;
        HlsEngine::download(&client, None, segments, target.unwrap(), &options(1), None, None).await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"INTRO /seg.ts?v=720&n=1 /seg.ts?v=720&n=2 ");
        assert_eq!(fetches("720"), before + 1, "only the missing segment is fetched");
    }

    #[tokio::test]
    async fn test_rotated_token_resumes_the_same_stream() {
        let playlist_fetches = Arc::new(AtomicU64::new(0));
        let broken = Arc::new(AtomicBool::new(true));
        let (fetches_srv, broken_srv) = (Arc::clone(&playlist_fetches), Arc::clone(&broken));
        // Every playlist fetch hands out a new token; the segments do not care.
        let (addr, hits) = serve(move |path: &str, _| match path.split('?').next().unwrap_or(path) {
            "/index.m3u8" => {
                let t = fetches_srv.fetch_add(1, Ordering::SeqCst);
                ok(format!("#EXTM3U\n#EXTINF:4,\na.ts?t={t}\n#EXTINF:4,\nb.ts?t={t}\n#EXTINF:4,\nc.ts?t={t}\n#EXT-X-ENDLIST\n"))
            }
            "/a.ts" => ok("AAAA"),
            "/b.ts" => ok("BBBB"),
            "/c.ts" if broken_srv.load(Ordering::SeqCst) => (404, String::new(), Vec::new()),
            "/c.ts" => ok("CCCC"),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        // The playlist URL's own token rotates as well.
        let url = |session: u32| Url::parse(&format!("http://{addr}/index.m3u8?session={session}")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("index.ts");

        let segments = parse_hls_playlist(&client, &url(1), None, FETCH).await.unwrap();
        assert!(download_to(&client, None, &url(1), segments, &out, 1).await.is_err());

        broken.store(false, Ordering::SeqCst);
        let segments = parse_hls_playlist(&client, &url(2), None, FETCH).await.unwrap();
        // download_to panics unless the .part is accepted as this stream.
        download_to(&client, None, &url(2), segments, &out, 1).await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"AAAABBBBCCCC");
        let fetches = |seg: &str| hits.lock().iter().filter(|(p, _)| p.starts_with(seg)).map(|(_, n)| n).sum::<usize>();
        assert_eq!(
            (fetches("/a.ts"), fetches("/b.ts")),
            (2, 2),
            "resumed after one check of the first and the last written segment, not restarted"
        );
    }

    #[tokio::test]
    async fn test_init_section_is_shared_and_segment_count_bounded() {
        let many = format!("#EXTM3U\n{}#EXT-X-ENDLIST\n", "#EXTINF:1,\ns.ts\n".repeat(MAX_SEGMENTS + 1));
        let (addr, _) = serve(move |path: &str, _| match path {
            "/fmp4.m3u8" => ok("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\na.m4s\n#EXTINF:4,\nb.m4s\n#EXT-X-ENDLIST\n"),
            "/many.m3u8" => ok(many.clone()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{}/fmp4.m3u8", addr)).unwrap();
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        let (a, b) = (segments[0].init.as_ref().unwrap(), segments[1].init.as_ref().unwrap());
        assert!(Arc::ptr_eq(a, b), "segments must share one init section, not copies");

        let url = Url::parse(&format!("http://{}/many.m3u8", addr)).unwrap();
        assert!(matches!(parse_hls_playlist(&client, &url, None, FETCH).await, Err(HlsError::Unsupported(_))));
    }

    #[tokio::test]
    async fn test_cancel_is_prompt_while_segment_stalls() {
        let (addr, _) = serve(|path: &str, _| match path {
            "/media.m3u8" => ok("#EXTM3U\n#EXTINF:4,\nstall.ts\n#EXT-X-ENDLIST\n"),
            _ => (0, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{}/media.m3u8", addr)).unwrap();
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&cancel);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            setter.store(true, Ordering::SeqCst);
        });
        let target = HlsEngine::prepare(&client, None, &url, &segments, &dir.path().join("x.ts"), FETCH, &None).await.unwrap();
        let started = Instant::now();
        let result = HlsEngine::download(&client, None, segments, target.unwrap(), &options(2), None, Some(cancel)).await;
        assert!(matches!(result, Err(HlsError::Cancelled)));
        // Well under the stall timeout: the cancel must not wait for the fetch to give up.
        assert!(started.elapsed() < FETCH.stall_timeout, "{:?}", started.elapsed());
    }

    /// What a fake fetch saw: requests per unit, requests open at once (and the most ever), how
    /// many units were handed out, and how far past those a unit was ever requested.
    #[derive(Default)]
    struct Requests {
        per_unit: Mutex<HashMap<usize, usize>>,
        open: AtomicUsize,
        most_open: AtomicUsize,
        handed_out: AtomicUsize,
        most_ahead: AtomicUsize,
    }

    type FakeFetch = Pin<Box<dyn Future<Output = Result<Vec<u8>, HlsError>> + Send>>;

    /// A fetch whose `n`-th request (from 0) of `unit` takes `plan(unit, n).0` milliseconds and
    /// answers `[unit]`, or fails if `plan(unit, n).1` is false.
    fn fake(requests: &Arc<Requests>, plan: fn(usize, usize) -> (u64, bool)) -> impl Fn(usize) -> FakeFetch {
        struct Open(Arc<Requests>);
        impl Drop for Open {
            fn drop(&mut self) {
                self.0.open.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let requests = Arc::clone(requests);
        move |unit| {
            let n = {
                let mut per_unit = requests.per_unit.lock();
                let count = per_unit.entry(unit).or_insert(0);
                *count += 1;
                *count - 1
            };
            let ahead = unit.saturating_sub(requests.handed_out.load(Ordering::SeqCst));
            requests.most_ahead.fetch_max(ahead, Ordering::SeqCst);
            let requests = Arc::clone(&requests);
            let (millis, succeeds) = plan(unit, n);
            Box::pin(async move {
                requests.most_open.fetch_max(requests.open.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                let _open = Open(Arc::clone(&requests));
                tokio::time::sleep(Duration::from_millis(millis)).await;
                match succeeds {
                    true => Ok(vec![unit as u8]),
                    false => Err(HlsError::SegmentFailed { index: unit, reason: format!("request {n} failed") }),
                }
            })
        }
    }

    /// Runs `window` to the end as `download` does, returning what it handed out, in order.
    async fn drain(window: &mut InOrder<'_, impl Fn(usize) -> FakeFetch, FakeFetch>, requests: &Requests) -> Result<Vec<u8>, HlsError> {
        let mut out = Vec::new();
        loop {
            while let Some(next) = window.pop() {
                let (unit, data) = next?;
                assert_eq!(unit, requests.handed_out.fetch_add(1, Ordering::SeqCst), "units come out in order");
                out.extend(data);
            }
            if window.is_done() {
                return Ok(out);
            }
            window.progress().await;
        }
    }

    fn requests_of(requests: &Requests, unit: usize) -> usize {
        requests.per_unit.lock().get(&unit).copied().unwrap_or(0)
    }

    /// The stall timeout of the fake fetches: no unit is requested again before 5 s unless a
    /// typical fetch time is known.
    const STALL: Duration = Duration::from_secs(30);

    #[tokio::test(start_paused = true)]
    async fn test_window_goes_past_a_stalled_unit_and_requests_it_once_more() {
        let requests = Arc::new(Requests::default());
        // The first request of unit 0 stalls; everything else takes a second.
        let fetch = fake(&requests, |unit, n| (if (unit, n) == (0, 0) { 100_000 } else { 1_000 }, true));
        let started = tokio::time::Instant::now();
        let out = drain(&mut InOrder::new(12, 3, 6, STALL, fetch), &requests).await.unwrap();

        assert_eq!(out, (0..12).collect::<Vec<u8>>(), "handed out in order");
        assert_eq!(requests_of(&requests, 0), 2, "the stalled unit is requested once more");
        assert!((1..12).all(|unit| requests_of(&requests, unit) == 1), "{:?}", requests.per_unit.lock());
        // Two seconds (twice the typical fetch) after it started, not a hundred.
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert_eq!(requests.most_open.load(Ordering::SeqCst), 3, "never more requests than connections");
        assert_eq!(requests.most_ahead.load(Ordering::SeqCst), 5, "the others fill the window, and no more");
    }

    #[tokio::test(start_paused = true)]
    async fn test_steady_units_are_requested_once() {
        let requests = Arc::new(Requests::default());
        // Between 1 and 1.4 s each, the tail included: no unit lags.
        let fetch = fake(&requests, |unit, _| (1_000 + (unit * 37 % 5) as u64 * 100, true));
        let out = drain(&mut InOrder::new(30, 4, 8, STALL, fetch), &requests).await.unwrap();
        assert_eq!(out, (0..30).collect::<Vec<u8>>());
        assert!((0..30).all(|unit| requests_of(&requests, unit) == 1), "{:?}", requests.per_unit.lock());
    }

    #[tokio::test(start_paused = true)]
    async fn test_either_request_may_answer_and_there_is_no_third() {
        // Unit 0's first request fails after 5 s, while its second one (from 3 s on) is still
        // running: that one's answer is used.
        let requests = Arc::new(Requests::default());
        let fetch = fake(&requests, |unit, n| match (unit, n) {
            (0, 0) => (5_000, false),
            (0, _) => (10_000, true),
            _ => (1_000, true),
        });
        let started = tokio::time::Instant::now();
        let out = drain(&mut InOrder::new(4, 2, 4, STALL, fetch), &requests).await.unwrap();
        assert_eq!(out, [0, 1, 2, 3]);
        assert_eq!(requests_of(&requests, 0), 2);
        let elapsed = started.elapsed();
        assert!((13_000..13_100).contains(&elapsed.as_millis()), "second request from 3 s on, answered 10 s later: {elapsed:?}");

        // Both requests fail: the first one's error ends the download, and no third is made.
        let requests = Arc::new(Requests::default());
        let fetch = fake(&requests, |unit, _| if unit == 0 { (5_000, false) } else { (1_000, true) });
        let err = drain(&mut InOrder::new(4, 2, 4, STALL, fetch), &requests).await.unwrap_err();
        assert!(err.to_string().contains("request 0 failed"), "{err}");
        assert_eq!(requests_of(&requests, 0), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_failed_unit_ends_the_download_only_after_the_units_before_it() {
        // The last unit fails at once, the others arrive later: they are all handed out first.
        let requests = Arc::new(Requests::default());
        let fetch = fake(&requests, |unit, _| if unit == 5 { (0, false) } else { (300, true) });
        let err = drain(&mut InOrder::new(6, 6, 12, STALL, fetch), &requests).await.unwrap_err();
        assert!(matches!(err, HlsError::SegmentFailed { index: 5, .. }), "{err}");
        assert_eq!(requests.handed_out.load(Ordering::SeqCst), 5);

        // Unit 3 fails first, unit 1 later: unit 1's error is the one reported, nothing after unit
        // 3 is requested, and unit 2, which was still being fetched, is dropped once unit 1 fails.
        let requests = Arc::new(Requests::default());
        let fetch = fake(&requests, |unit, _| match unit {
            1 => (200, false),
            2 => (10_000, true),
            3 => (0, false),
            _ => (300, true),
        });
        let started = tokio::time::Instant::now();
        let mut window = InOrder::new(8, 4, 8, STALL, fetch);
        let err = drain(&mut window, &requests).await.unwrap_err();
        assert!(matches!(err, HlsError::SegmentFailed { index: 1, .. }), "{err}");
        let elapsed = started.elapsed().as_millis();
        assert!((300..310).contains(&elapsed), "as soon as unit 0 is in: {elapsed} ms");
        assert_eq!(requests.handed_out.load(Ordering::SeqCst), 1);
        assert!((0..4).all(|unit| requests_of(&requests, unit) == 1), "{:?}", requests.per_unit.lock());
        assert_eq!(requests.per_unit.lock().len(), 4, "no unit after a failed one is requested");
        assert_eq!(requests.open.load(Ordering::SeqCst), 0, "unit 2 is no longer fetched");
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_first_unit_that_stalls_is_requested_again_before_any_arrived() {
        // A single unit and a free connection: no typical fetch time is known yet.
        for (stall, answered) in [(STALL, 6_000), (Duration::from_secs(4), 3_000)] {
            let requests = Arc::new(Requests::default());
            let fetch = fake(&requests, |_, n| (if n == 0 { 100_000 } else { 1_000 }, true));
            let started = tokio::time::Instant::now();
            let out = drain(&mut InOrder::new(1, 2, 2, stall, fetch), &requests).await.unwrap();
            assert_eq!(out, [0]);
            assert_eq!(requests_of(&requests, 0), 2);
            // Half the stall timeout, at most 5 s, and the second request's second.
            let elapsed = started.elapsed().as_millis();
            assert!((answered..answered + 10).contains(&elapsed), "stall timeout {stall:?}: {elapsed} ms");
        }
    }

    #[test]
    fn test_tally_keeps_only_what_a_finished_fetch_received() {
        let total = AtomicU64::new(100);
        let lost = Tally::new(&total);
        lost.add(40);
        lost.take_back(10);
        assert_eq!(total.load(Ordering::SeqCst), 130);
        // A fetch dropped midway (it failed, or lost the race to a second request) counts for nothing.
        drop(lost);
        assert_eq!(total.load(Ordering::SeqCst), 100);
        let kept = Tally::new(&total);
        kept.add(25);
        kept.keep();
        assert_eq!(total.load(Ordering::SeqCst), 125);
    }

    #[tokio::test]
    async fn test_stalled_segment_is_requested_again_before_the_stall_timeout() {
        let first = Arc::new(AtomicBool::new(true));
        let (addr, hits) = serve(move |path: &str, _| match path {
            "/media.m3u8" => ok(format!("#EXTM3U\n{}#EXT-X-ENDLIST\n", (0..6).map(|n| format!("#EXTINF:4,\ns{n}.ts\n")).collect::<String>())),
            // The first request of the first segment never gets an answer.
            "/s0.ts" if first.swap(false, Ordering::SeqCst) => (0, String::new(), Vec::new()),
            p => ok(p.trim_start_matches("/s").trim_end_matches(".ts").repeat(3)),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{addr}/media.m3u8")).unwrap();
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hedged.ts");
        // Waiting out the stall would take 20 s, and fail: no retries.
        let patient = HlsOptions { fetch: FetchPolicy { stall_timeout: Duration::from_secs(20), max_retries: 0 }, ..options(3) };
        let target = HlsEngine::prepare(&client, None, &url, &segments, &out, patient.fetch, &None).await.unwrap().unwrap();
        let started = Instant::now();
        HlsEngine::download(&client, None, segments, target, &patient, None, None).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert_eq!(std::fs::read(&out).unwrap(), b"000111222333444555");
        let hits = hits.lock();
        assert_eq!(hits["/s0.ts"], 2);
        assert!((1..6).all(|n| hits[&format!("/s{n}.ts")] == 1), "{hits:?}");
    }

    #[tokio::test]
    async fn test_state_is_saved_every_two_seconds_and_when_stopped() {
        // The first requests of segments 5 and 7 are answered only once the test lets them.
        let gates = Arc::new([Semaphore::new(0), Semaphore::new(0)]);
        let held = Arc::new([AtomicBool::new(true), AtomicBool::new(true)]);
        let gates_srv = Arc::clone(&gates);
        let (addr, hits) = serve_async(move |path: &str, _| {
            let (path, gates, held) = (path.to_string(), Arc::clone(&gates_srv), Arc::clone(&held));
            async move {
                let gate = match path.as_str() {
                    "/media.m3u8" => return numbered_playlist(10),
                    "/s5.ts" => Some(0),
                    "/s7.ts" => Some(1),
                    _ => None,
                };
                if let Some(gate) = gate.filter(|&g| held[g].swap(false, Ordering::SeqCst)) {
                    let _ = gates[gate].acquire().await;
                }
                numbered_segment(&path)
            }
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{addr}/media.m3u8")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("paced.ts");
        let state = with_suffix(&out, ".part.hlsstate");
        let saved = || saved(&out);
        let part_len = || part_len(&out);

        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        let target = HlsEngine::prepare(&client, None, &url, &segments, &out, FETCH, &None).await.unwrap().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        // Patient enough to wait for the held segments.
        let patient = HlsOptions { fetch: FetchPolicy { stall_timeout: Duration::from_secs(30), ..FETCH }, ..options(1) };
        let download = {
            let (client, cancel) = (client.clone(), Arc::clone(&cancel));
            tokio::spawn(async move { HlsEngine::download(&client, None, segments, target, &patient, None, Some(cancel)).await })
        };

        // Segments 0 to 4 are written at once, but the state is not saved for each of them: it
        // still says what it said when the .part was created.
        wait_for("segments 0-4", || part_len() == Some(20)).await;
        assert_eq!(saved(), Some((0, 0)));
        // The first segment written two seconds after that save saves it again, the next does not.
        // That save waits for the .part to reach the disk, and the writer goes on meanwhile.
        let flush = DiskGate::close(dir.path(), DiskOp::Flush);
        tokio::time::sleep(Duration::from_millis(2200)).await;
        gates[0].add_permits(1);
        wait_for("segment 6", || part_len() == Some(28)).await;
        assert_eq!(saved(), Some((0, 0)), "the save of segment 5 is still flushing");
        drop(flush);
        wait_for("the save of segment 5", || saved() == Some((6, 24))).await;

        // Cancelled while segment 7 is held back: the state records every segment written.
        cancel.store(true, Ordering::SeqCst);
        assert!(matches!(download.await.unwrap(), Err(HlsError::Cancelled)));
        assert_eq!(saved(), Some((7, 28)));
        assert!(!with_suffix(&state, ".tmp").exists());

        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        download_to(&client, None, &url, segments, &out, 1).await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"0000111122223333444455556666777788889999");
        let hits = hits.lock();
        assert!((0..10).all(|n| hits[&format!("/s{n}.ts")] == if n == 7 { 2 } else { 1 }), "{hits:?}");
    }

    #[tokio::test]
    async fn test_fetches_go_on_while_the_disk_is_stuck() {
        let (addr, hits) = serve(|path: &str, _| match path {
            "/media.m3u8" => numbered_playlist(20),
            p => numbered_segment(p),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{addr}/media.m3u8")).unwrap();
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("stuck.ts");
        let target = HlsEngine::prepare(&client, None, &url, &segments, &out, FETCH, &None).await.unwrap().unwrap();
        let writes = DiskGate::close(dir.path(), DiskOp::Write);
        let download = {
            let client = client.clone();
            tokio::spawn(async move { HlsEngine::download(&client, None, segments, target, &options(2), None, None).await })
        };

        // The writer is stuck on segment 0, four more wait in its queue, and the window of two
        // connections fetches the four after those, but no more.
        let requested = |n: usize| hits.lock().contains_key(&format!("/s{n}.ts"));
        wait_for("segment 8 to be requested", || requested(8)).await;
        assert_eq!(part_len(&out), Some(0), "nothing is written yet");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!requested(9), "what is held in memory stays bounded");

        drop(writes);
        let (path, digest) = download.await.unwrap().unwrap();
        let expected: String = (0..20).map(|n| n.to_string().repeat(4)).collect();
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
        assert_digest(&digest, &path);
    }

    #[tokio::test]
    async fn test_a_dropped_download_holds_its_claim_until_its_writer_saved_what_it_was_given() {
        let (addr, hits) = serve(|path: &str, _| match path {
            "/media.m3u8" => numbered_playlist(10),
            p => numbered_segment(p),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{addr}/media.m3u8")).unwrap();
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("dropped.ts");
        struct Claim(Arc<AtomicBool>);
        impl Drop for Claim {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let target = HlsEngine::prepare(&client, None, &url, &segments, &out, FETCH, &None).await.unwrap().unwrap();
        let target = target.hold(Claim(Arc::clone(&released)));
        let writes = DiskGate::close(dir.path(), DiskOp::Write);
        let download = {
            let (client, segments) = (client.clone(), segments.clone());
            tokio::spawn(async move { HlsEngine::download(&client, None, segments, target, &options(1), None, None).await })
        };

        // The writer is stuck on segment 0 with 1-4 in its queue; segments 5 and 6 are fetched.
        wait_for("segment 6 to be requested", || hits.lock().contains_key("/s6.ts")).await;
        download.abort();
        assert!(download.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!released.load(Ordering::SeqCst), "the writer still has segments to write");

        drop(writes);
        wait_for("the writer to let go of the claim", || released.load(Ordering::SeqCst)).await;
        assert_eq!(saved(&out), Some((5, 20)), "the state records every segment the writer was given");
        assert_eq!(part_len(&out), Some(20));

        download_to(&client, None, &url, segments, &out, 1).await.unwrap();
        let hits = hits.lock();
        assert!((0..5).all(|n| hits[&format!("/s{n}.ts")] == 1), "{hits:?}");
    }

    #[tokio::test]
    async fn test_stopping_a_resumed_download_does_not_wait_for_its_part_to_be_hashed() {
        let (addr, _) = serve(|_, _| (0, String::new(), Vec::new())).await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{addr}/media.m3u8")).unwrap();
        // Two segments; an earlier run wrote the first, a huge one.
        let segments: Vec<HlsSegment> = (0..2)
            .map(|index| HlsSegment {
                index,
                url: url.join(&format!("s{index}.ts")).unwrap(),
                duration_secs: 4.0,
                byte_range: None,
                encryption: None,
                init: None,
            })
            .collect();
        const WRITTEN: u64 = 512 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("resumed.ts");
        std::fs::File::create(with_suffix(&out, ".part")).unwrap().set_len(WRITTEN).unwrap();
        let state = with_suffix(&out, ".part.hlsstate");
        std::fs::write(&state, "as the earlier run left it").unwrap();
        let fingerprints = Fingerprints::of(&url, &segments);
        let target = HlsTarget { path: out.clone(), fingerprints, resume_from: 1, resume_bytes: WRITTEN, claim: None };
        // Hashing all of it with SHA-256 as well takes seconds.
        let options = HlsOptions { expected_checksum: Some(format!("sha256:{}", "0".repeat(64))), ..options(1) };

        let started = Instant::now();
        let cancelled = Some(Arc::new(AtomicBool::new(true)));
        let result = HlsEngine::download(&client, None, segments, target, &options, None, cancelled).await;
        assert!(matches!(result, Err(HlsError::Cancelled)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "as the earlier run left it", "nothing new to record");
    }

    #[tokio::test]
    async fn test_digest_covers_a_resumed_part_and_the_checksum() {
        use sha2::Digest;
        let broken = Arc::new(AtomicBool::new(true));
        let broken_srv = Arc::clone(&broken);
        let (addr, _) = serve(move |path: &str, _| match path {
            "/media.m3u8" => ok("#EXTM3U\n#EXTINF:4,\na.ts\n#EXTINF:4,\nb.ts\n#EXTINF:4,\nc.ts\n#EXT-X-ENDLIST\n"),
            "/c.ts" if broken_srv.load(Ordering::SeqCst) => (404, String::new(), Vec::new()),
            p => ok(p.repeat(3000)),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{addr}/media.m3u8")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("hashed.ts");
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        assert!(download_to(&client, None, &url, segments.clone(), &out, 1).await.is_err());

        // The rest, after a and b from the first run, with a SHA-256 checksum to take as well.
        broken.store(false, Ordering::SeqCst);
        let expected = ["/a.ts", "/b.ts", "/c.ts"].map(|p| p.repeat(3000)).concat();
        let checksum = format!("sha256:{:x}", sha2::Sha256::digest(expected.as_bytes()));
        let options = HlsOptions { expected_checksum: Some(checksum.clone()), ..options(2) };
        let target = HlsEngine::prepare(&client, None, &url, &segments, &out, FETCH, &None).await.unwrap().unwrap();
        assert_eq!(target.resume_from, 2);
        let (path, digest) = HlsEngine::download(&client, None, segments, target, &options, None, None).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
        let blake3 = blake3::hash(expected.as_bytes()).to_hex().to_string();
        assert_eq!(crate::storage::verify_digest(&digest, Some(&checksum)).unwrap(), blake3);
        let wrong = format!("sha256:{}", "0".repeat(64));
        assert!(crate::storage::verify_digest(&digest, Some(&wrong)).is_err());
    }

    #[test]
    fn test_merged_requests_shrink_as_connections_grow() {
        const SEGMENT: u64 = 4 * 1024;
        let segments: Vec<HlsSegment> = (0..200)
            .map(|index| HlsSegment {
                index,
                url: Url::parse("http://h/all.mp4").unwrap(),
                duration_secs: 4.0,
                byte_range: Some(ByteRange::from_len(index as u64 * SEGMENT, SEGMENT).unwrap()),
                encryption: None,
                init: None,
            })
            .collect();
        let largest = |connections| plan_requests(segments.clone(), 0, merge_limit(connections)).iter().map(Request::segments).max();
        // One connection merges as much as ever; more share the memory, down to one segment a request.
        assert_eq!(largest(1), Some((MAX_MERGED_BYTES / SEGMENT) as usize));
        assert_eq!(largest(8), Some(5));
        assert_eq!(largest(MAX_CONNECTIONS), Some(1));
        for connections in 1..=MAX_CONNECTIONS {
            let held = (WINDOW_PER_CONNECTION * connections + WRITE_QUEUE + 2) as u64;
            assert!(merge_limit(connections) * held <= MERGE_BUDGET, "{connections} connections");
        }
    }

    #[test]
    fn test_plan_merges_adjoining_plain_ranges_of_one_file() {
        let init = |name: &str| Arc::new(InitSection { url: Url::parse(&format!("http://h/{name}")).unwrap(), byte_range: None, encryption: None });
        let (x, y) = (init("x.mp4"), init("y.mp4"));
        let segment = |file: &str, range: Option<(u64, u64)>, encrypted: bool, init: Option<&Arc<InitSection>>| HlsSegment {
            index: 0,
            url: Url::parse(&format!("http://h/{file}")).unwrap(),
            duration_secs: 4.0,
            byte_range: range.map(|(start, len)| ByteRange::from_len(start, len).unwrap()),
            encryption: encrypted.then_some(Aes128Key { key: [1; 16], iv: [2; 16] }),
            init: init.cloned(),
        };
        const K: u64 = 1024;
        let mut segments = vec![
            segment("a.mp4", Some((0, 10)), false, None),
            segment("a.mp4", Some((10, 10)), false, None),
            segment("a.mp4", Some((20, 10)), false, None),
            segment("a.mp4", Some((31, 9)), false, None),  // a gap
            segment("b.mp4", Some((40, 10)), false, None), // another file
            segment("b.mp4", Some((50, 10)), true, None),  // encrypted
            segment("b.mp4", Some((60, 10)), true, None),
            segment("c.ts", None, false, None),            // no byte ranges
            segment("c.ts", None, false, None),
            segment("d.mp4", Some((0, 10)), false, Some(&x)),
            segment("d.mp4", Some((10, 10)), false, Some(&x)),
            segment("d.mp4", Some((20, 10)), false, Some(&y)), // its init section comes first
            segment("e.mp4", Some((0, 30 * K)), false, Some(&y)),
            segment("e.mp4", Some((30 * K, 30 * K)), false, Some(&y)),
            segment("e.mp4", Some((60 * K, 30 * K)), false, Some(&y)), // over MAX_MERGED_BYTES together
        ];
        for (index, segment) in segments.iter_mut().enumerate() {
            segment.index = index;
        }
        // (first segment, segments, byte range, with init section) of each request.
        let plan = |from: usize| {
            plan_requests(segments.clone(), from, MAX_MERGED_BYTES)
                .iter()
                .map(|r| (r.segment.index, r.segments(), r.segment.byte_range.map(|b| (b.start, b.end)), r.with_init))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            plan(0),
            [
                (0, 3, Some((0, 29)), false),
                (3, 1, Some((31, 39)), false),
                (4, 1, Some((40, 49)), false),
                (5, 1, Some((50, 59)), false),
                (6, 1, Some((60, 69)), false),
                (7, 1, None, false),
                (8, 1, None, false),
                (9, 2, Some((0, 19)), true),
                (11, 1, Some((20, 29)), true),
                (12, 2, Some((0, 60 * K - 1)), false),
                (14, 1, Some((60 * K, 90 * K - 1)), false),
            ]
        );
        // Resumed after one segment: the rest of the first file's run is still one request.
        assert_eq!(plan(1)[0], (1, 2, Some((10, 29)), false));
    }

    #[tokio::test]
    async fn test_byte_ranges_of_one_file_are_fetched_together_and_resume_by_segment() {
        const SEGMENT: usize = 10 * 1024;
        let file: Vec<u8> = (0..20 * SEGMENT).map(|i| (i % 251) as u8).collect();
        let playlist: String = (0..20).map(|n| format!("#EXTINF:4,\n#EXT-X-BYTERANGE:{SEGMENT}@{}\nall.mp4\n", n * SEGMENT)).collect();
        let playlist = format!("#EXTM3U\n{playlist}#EXT-X-ENDLIST\n");
        // Fails the request for the third group of segments until it is repaired.
        let broken = Arc::new(AtomicBool::new(true));
        let (file_srv, broken_srv) = (file.clone(), Arc::clone(&broken));
        let (addr, hits) = serve(move |path: &str, range: Option<ByteRange>| match (path, range) {
            ("/media.m3u8", _) => ok(playlist.clone()),
            ("/all.mp4", Some(r)) if r.start == 12 * SEGMENT as u64 && broken_srv.load(Ordering::SeqCst) => {
                (404, String::new(), Vec::new())
            }
            ("/all.mp4", Some(r)) => (206, String::new(), file_srv[r.start as usize..=r.end as usize].to_vec()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{addr}/media.m3u8")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("ranged.mp4");
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();

        // Six segments (60 KiB) per request: the first two requests are written, the third fails,
        // and so does its first segment on its own.
        let err = download_to(&client, None, &url, segments.clone(), &out, 1).await.unwrap_err();
        assert!(matches!(err, HlsError::SegmentFailed { index: 12, .. }), "{err}");
        let state = std::fs::read_to_string(with_suffix(&out, ".part.hlsstate")).unwrap();
        assert!(state.ends_with(&format!(" 12 {}", 12 * SEGMENT)), "{state}");

        broken.store(false, Ordering::SeqCst);
        download_to(&client, None, &url, segments, &out, 2).await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), file);
        assert_eq!(hits.lock()["/all.mp4"], 3 + 1 + 2, "segments 12-16 and 17-19 on resume (five at a time with two connections), not 8 requests");
    }

    #[tokio::test]
    async fn test_ranges_a_server_will_not_serve_together_are_fetched_one_at_a_time() {
        const SEGMENT: usize = 10 * 1024;
        let file: Vec<u8> = (0..8 * SEGMENT).map(|i| (i % 251) as u8).collect();
        let playlist = |name: &str| {
            let segments: String =
                (0..8).map(|n| format!("#EXTINF:4,\n#EXT-X-BYTERANGE:{SEGMENT}@{}\n{name}\n", n * SEGMENT)).collect();
            ok(format!("#EXTM3U\n{segments}#EXT-X-ENDLIST\n"))
        };
        let file_srv = file.clone();
        let (addr, hits) = serve(move |path: &str, range: Option<ByteRange>| match (path, range) {
            ("/capped.m3u8", _) => playlist("capped.mp4"),
            ("/refused.m3u8", _) => playlist("refused.mp4"),
            // Longer ranges are cut short to one segment's worth, or refused.
            ("/capped.mp4", Some(r)) => {
                let end = (r.end as usize).min(r.start as usize + SEGMENT - 1);
                (206, String::new(), file_srv[r.start as usize..=end].to_vec())
            }
            ("/refused.mp4", Some(r)) if r.len() > SEGMENT as u64 => (416, String::new(), Vec::new()),
            ("/refused.mp4", Some(r)) => (206, String::new(), file_srv[r.start as usize..=r.end as usize].to_vec()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let client = Client::new();
        let dir = tempfile::tempdir().unwrap();
        for name in ["capped", "refused"] {
            let url = Url::parse(&format!("http://{addr}/{name}.m3u8")).unwrap();
            let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
            let out = dir.path().join(format!("{name}.mp4"));
            download_to(&client, None, &url, segments, &out, 1).await.unwrap();
            assert_eq!(std::fs::read(&out).unwrap(), file, "{name}");
            // Segments 0-5 and 6-7 are each tried together once, then one at a time.
            assert_eq!(hits.lock()[&format!("/{name}.mp4")], 2 + 8, "{name}");
        }
    }

    #[tokio::test]
    async fn test_retries_follow_the_policy() {
        let failures = Arc::new(AtomicUsize::new(0));
        let failures_srv = Arc::clone(&failures);
        let (addr, _) = serve(move |path: &str, _| match path {
            "/media.m3u8" => ok("#EXTM3U\n#EXTINF:4,\nbusy.ts\n#EXT-X-ENDLIST\n"),
            // Busy twice for every client that starts over.
            _ if failures_srv.fetch_add(1, Ordering::SeqCst) % 3 < 2 => (503, String::new(), Vec::new()),
            _ => ok("DATA"),
        })
        .await;
        let client = Client::new();
        let url = Url::parse(&format!("http://{addr}/media.m3u8")).unwrap();
        let segments = parse_hls_playlist(&client, &url, None, FETCH).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        for (max_retries, succeeds) in [(1, false), (2, true)] {
            failures.store(0, Ordering::SeqCst);
            // Only the 503s count as failures, however slow the machine.
            let fetch = FetchPolicy { max_retries, stall_timeout: Duration::from_secs(10) };
            let options = HlsOptions { fetch, ..options(1) };
            let out = dir.path().join(format!("{max_retries}.ts"));
            let target = HlsEngine::prepare(&client, None, &url, &segments, &out, FETCH, &None).await.unwrap().unwrap();
            let result = HlsEngine::download(&client, None, segments.clone(), target, &options, None, None).await;
            match result {
                Ok(_) => assert!(succeeds, "{max_retries} retries must not be enough"),
                Err(e) => assert!(!succeeds && e.to_string().contains("after 2 attempt(s)"), "{e}"),
            }
        }
    }
}
