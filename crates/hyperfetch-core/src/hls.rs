use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::broadcast;
use url::Url;
use thiserror::Error;
use crate::engine::EngineSnapshot;
use crate::range::ByteRange;
use crate::worker::{authorize, Auth};

const MAX_CONNECTIONS: usize = 64;
/// Master playlists may point at further master playlists; stop following them after this many hops.
const MAX_MASTER_DEPTH: usize = 3;
const MAX_ATTEMPTS: u32 = 5;
const MAX_PLAYLIST_BYTES: u64 = if cfg!(test) { 64 * 1024 } else { 16 * 1024 * 1024 };
const MAX_KEY_BYTES: u64 = 1024;
/// Most of an oversized body read anyway, to tell a huge playlist from an ordinary file.
const PEEK_BYTES: u64 = 1024;
/// Largest segment or init section held in memory. Real segments are a few MiB.
// ponytail: up to num_connections segments are buffered at once, so peak memory is
// num_connections x this; spool segments to disk if streams with huge segments must work.
const MAX_SEGMENT_BYTES: u64 = if cfg!(test) { 64 * 1024 } else { 256 * 1024 * 1024 };
/// Longest media playlist accepted (a day of 2-second segments is about 43,000).
const MAX_SEGMENTS: usize = if cfg!(test) { 1000 } else { 1_000_000 };
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(200);
/// Time allowed for response headers to arrive, and for any gap between body chunks.
const STALL_TIMEOUT: Duration = if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(30) };
/// First retry delay; doubles on every further attempt (0.5s, 1s, 2s, 4s).
const RETRY_BASE_DELAY: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_millis(500) };

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
) -> Result<Vec<HlsSegment>, HlsError> {
    let mut url = playlist_url.clone();
    for hop in 0..=MAX_MASTER_DEPTH {
        let (body, final_url) = fetch_with_retry(client, auth, &url, None, MAX_PLAYLIST_BYTES, &AtomicU64::new(0))
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
        if !is_playlist_start(&body) {
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
        return parse_media_playlist(client, auth, text, &final_url).await;
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

    let mut segments = Vec::new();
    let mut media_sequence: u64 = 0;
    let mut duration = 2.0;
    let mut byte_range = None;
    let mut next_range_start = 0;
    let mut key: Option<ActiveKey> = None;
    let mut key_cache: HashMap<Url, [u8; 16]> = HashMap::new();
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
                    let key_bytes = match key_cache.get(&key_url) {
                        Some(k) => *k,
                        None => {
                            let k = fetch_key(client, auth, &key_url).await?;
                            key_cache.insert(key_url, k);
                            k
                        }
                    };
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

async fn fetch_key(client: &Client, auth: Option<&Auth>, url: &Url) -> Result<[u8; 16], HlsError> {
    let (bytes, _) = fetch_with_retry(client, auth, url, None, MAX_KEY_BYTES, &AtomicU64::new(0))
        .await
        .map_err(|e| HlsError::Unavailable(format!("could not fetch AES key {}: {}", url, e.reason)))?;
    <[u8; 16]>::try_from(bytes.as_slice()).map_err(|_| {
        HlsError::Unavailable(format!("AES-128 key at {} is {} bytes, expected 16", url, bytes.len()))
    })
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

/// One GET with header and idle timeouts, reading at most `max_bytes` of body. Body bytes
/// are added to `progress` as they arrive and taken back out if the attempt fails.
async fn fetch_once(
    client: &Client,
    auth: Option<&Auth>,
    url: &Url,
    range: Option<ByteRange>,
    max_bytes: u64,
    progress: &AtomicU64,
) -> Result<(Vec<u8>, Url), FetchError> {
    let mut req = authorize(client.get(url.clone()), auth, url);
    if let Some(r) = range {
        req = req.header(reqwest::header::RANGE, r.to_http_header());
    }
    let mut resp = tokio::time::timeout(STALL_TIMEOUT, req.send())
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
        match tokio::time::timeout(STALL_TIMEOUT, resp.chunk()).await {
            Err(_) => break Err(FetchError::retryable("connection stalled")),
            Ok(Err(e)) => break Err(FetchError::retryable(e)),
            Ok(Ok(None)) if declared_too_large => break Err(FetchError::too_large(max_bytes, &body)),
            Ok(Ok(None)) => break Ok(()),
            Ok(Ok(Some(chunk))) => {
                if body.len() as u64 + chunk.len() as u64 > read_limit {
                    let head: Vec<u8> = body.iter().chain(chunk.iter()).take(PEEK_BYTES as usize).copied().collect();
                    break Err(FetchError::too_large(max_bytes, &head));
                }
                progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
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
        progress.fetch_sub(body.len() as u64, Ordering::Relaxed);
        return Err(e);
    }
    Ok((body, final_url))
}

/// `fetch_once` with exponential backoff; client errors other than 408/429 are not retried.
async fn fetch_with_retry(
    client: &Client,
    auth: Option<&Auth>,
    url: &Url,
    range: Option<ByteRange>,
    max_bytes: u64,
    progress: &AtomicU64,
) -> Result<(Vec<u8>, Url), FetchError> {
    let mut delay = RETRY_BASE_DELAY;
    let mut attempt = 1;
    loop {
        match fetch_once(client, auth, url, range, max_bytes, progress).await {
            Ok(fetched) => return Ok(fetched),
            Err(e) if e.kind == FetchErrorKind::Transient && attempt < MAX_ATTEMPTS => {
                tracing::warn!("HLS fetch of {} failed (attempt {}): {}; retrying", url, attempt, e.reason);
                tokio::time::sleep(delay).await;
                delay *= 2;
                attempt += 1;
            }
            Err(mut e) => {
                e.reason = format!("{} (after {} attempt(s))", e.reason, attempt);
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

/// Downloads and decrypts one segment, prefixed by its init section when that changes.
async fn fetch_segment(
    client: &Client,
    auth: Option<&Auth>,
    segment: HlsSegment,
    with_init: bool,
    progress: &AtomicU64,
) -> Result<Vec<u8>, HlsError> {
    let failed = |reason: String| HlsError::SegmentFailed { index: segment.index, reason };
    let limit = |range: Option<ByteRange>| range.map_or(MAX_SEGMENT_BYTES, |r| r.len().min(MAX_SEGMENT_BYTES));
    let mut init_data = None;
    if let Some(init) = segment.init.as_ref().filter(|_| with_init) {
        let (data, _) = fetch_with_retry(client, auth, &init.url, init.byte_range, limit(init.byte_range), progress)
            .await
            .map_err(|e| failed(format!("init section {}: {}", init.url, e.reason)))?;
        init_data = Some(decrypt(data, &init.encryption).await.map_err(|r| failed(format!("init section: {}", r)))?);
    }
    let (data, _) = fetch_with_retry(client, auth, &segment.url, segment.byte_range, limit(segment.byte_range), progress)
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

/// Whether the `count` segments written to `part_path` (`bytes` long) are these: the first and the
/// last of them, fetched again, must equal the bytes that start and end the `.part`.
async fn part_matches(
    client: &Client,
    auth: Option<&Auth>,
    segments: &[HlsSegment],
    part_path: &Path,
    count: usize,
    bytes: u64,
    cancel_flag: &Option<Arc<AtomicBool>>,
) -> Result<bool, HlsError> {
    for index in std::iter::once(0).chain((count > 1).then_some(count - 1)) {
        let uncounted = AtomicU64::new(0);
        let fetch = fetch_segment(client, auth, segments[index].clone(), writes_init(segments, index), &uncounted);
        let data = tokio::select! {
            data = fetch => data?,
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

/// Where an HLS download goes and how much of its `.part` it keeps, from [`HlsEngine::prepare`].
pub struct HlsTarget {
    path: PathBuf,
    fingerprints: Fingerprints,
    resume_from: usize,
    resume_bytes: u64,
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
                return Ok(Some(HlsTarget { path, fingerprints, resume_from: 0, resume_bytes: 0 }))
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
            && part_matches(client, auth, segments, &part_path, count, bytes, cancel_flag).await?
        {
            (count, bytes)
        } else {
            tracing::info!("{} holds another stream; leaving it alone", part_path.display());
            return Ok(None);
        };
        Ok(Some(HlsTarget { path, fingerprints, resume_from, resume_bytes }))
    }

    /// Downloads `segments` with at most `num_connections` requests in flight and writes them in
    /// order to the `.part` of `target` (from [`HlsEngine::prepare`] for these segments), renamed
    /// to the target path once complete. A failed or cancelled run keeps the `.part` file and
    /// resumes from it next time. `auth` is added to every request it covers.
    pub async fn download(
        client: &Client,
        auth: Option<&Auth>,
        segments: Vec<HlsSegment>,
        target: HlsTarget,
        num_connections: usize,
        snapshot_tx: Option<broadcast::Sender<EngineSnapshot>>,
        cancel_flag: Option<Arc<AtomicBool>>,
    ) -> Result<PathBuf, HlsError> {
        let num_connections = num_connections.clamp(1, MAX_CONNECTIONS);
        let total_segments = segments.len();
        if total_segments == 0 {
            return Err(HlsError::NoSegments);
        }
        let is_cancelled = || cancel_flag.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));

        let HlsTarget { path: target_file, fingerprints, resume_from, resume_bytes: mut written_bytes } = target;
        if let Some(parent) = target_file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let part_path = with_suffix(&target_file, ".part");
        let state_path = with_suffix(&part_path, ".hlsstate");
        let mut out_file = tokio::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&part_path).await?;
        out_file.set_len(written_bytes).await?;
        out_file.seek(SeekFrom::End(0)).await?;

        tracing::info!(
            "Starting HLS ingestion: {} segments ({} already done) across {} streams -> {}",
            total_segments,
            resume_from,
            num_connections,
            target_file.display()
        );

        // An init section is written before the first segment that uses it and again whenever it changes.
        let with_init: Vec<bool> = (0..total_segments).map(|i| writes_init(&segments, i)).collect();
        let received = AtomicU64::new(written_bytes);
        // `buffered` keeps at most `num_connections` fetches in flight and yields them in order,
        // so no more than that many segments are ever held in memory. Dropping it aborts them.
        let mut pipeline = futures_util::stream::iter(
            segments
                .into_iter()
                .zip(with_init)
                .skip(resume_from)
                .map(|(segment, with_init)| fetch_segment(client, auth, segment, with_init, &received)),
        )
        .buffered(num_connections);

        let mut written = resume_from;
        let start_time = Instant::now();
        let start_bytes = written_bytes;
        let mut ticker = tokio::time::interval(SNAPSHOT_INTERVAL);
        loop {
            tokio::select! {
                next = pipeline.next() => match next {
                    None => break,
                    Some(data) => {
                        let data = data?;
                        out_file.write_all(&data).await?;
                        out_file.flush().await?;
                        written += 1;
                        written_bytes += data.len() as u64;
                        let state =
                            format!("{} {} {} {}", fingerprints.exact, fingerprints.stream, written, written_bytes);
                        tokio::fs::write(&state_path, state).await?;
                    }
                },
                _ = ticker.tick() => {
                    if is_cancelled() {
                        return Err(HlsError::Cancelled);
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
        }
        drop(pipeline);

        out_file.sync_all().await?;
        drop(out_file);
        tokio::fs::rename(&part_path, &target_file).await?;
        let _ = tokio::fs::remove_file(&state_path).await;

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
        Ok(target_file)
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
    use std::net::SocketAddr;

    /// Response of the mock server: status, extra header lines, body.
    /// Status 0 means "never answer" (a stalled connection).
    pub(crate) type Reply = (u16, String, Vec<u8>);

    /// Minimal HTTP/1.1 server that also counts requests per path (query included). Paths under
    /// `/private/` answer 401 unless the request carries `Authorization: Bearer secret`.
    pub(crate) async fn serve(
        handler: impl Fn(&str, Option<ByteRange>) -> Reply + Send + Sync + 'static,
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
                        handler(&path, range)
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

    /// Prepares `out` for `segments` of `playlist`, which must be free or hold this stream, and
    /// downloads them there.
    async fn download_to(
        client: &Client,
        auth: Option<&Auth>,
        playlist: &Url,
        segments: Vec<HlsSegment>,
        out: &Path,
        num_connections: usize,
    ) -> Result<PathBuf, HlsError> {
        let target = HlsEngine::prepare(client, auth, playlist, &segments, out, &None).await?.expect("usable name");
        HlsEngine::download(client, auth, segments, target, num_connections, None, None).await
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
        let segments = parse_hls_playlist(&Client::new(), &url, None).await.unwrap();
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
        assert!(matches!(parse_hls_playlist(&client, &live, None).await, Err(HlsError::Unsupported(_))));

        let start = Url::parse(&format!("http://{}/start/master.m3u8", addr)).unwrap();
        let segments = parse_hls_playlist(&client, &start, None).await.unwrap();
        assert_eq!(segments[0].url.path(), "/cdn/abc/v/seg0.ts");
    }

    #[tokio::test]
    async fn test_master_recursion_is_depth_limited() {
        let (addr, hits) = serve(|_, _| ok("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nself.m3u8\n")).await;
        let url = Url::parse(&format!("http://{}/self.m3u8", addr)).unwrap();
        assert!(matches!(parse_hls_playlist(&Client::new(), &url, None).await, Err(HlsError::Unsupported(_))));
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
        let segments = parse_hls_playlist(&client, &url, None).await.unwrap();
        assert_eq!(container_extension(&segments), "mp4");

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("video.mp4");
        let (tx, mut rx) = broadcast::channel(256);
        let target = HlsEngine::prepare(&client, None, &url, &segments, &out, &None).await.unwrap().unwrap();
        // num_connections == 0 must be treated as 1, not panic.
        let path = HlsEngine::download(&client, None, segments, target, 0, Some(tx), None).await.unwrap();

        let expected = [init, seg0, seg1, seg2].concat();
        assert_eq!(path, out);
        assert_eq!(std::fs::read(&out).unwrap(), expected);
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

        let segments = parse_hls_playlist(&client, &url, None).await.unwrap();
        let err = download_to(&client, None, &url, segments, &out, 1).await.unwrap_err();
        assert!(matches!(err, HlsError::SegmentFailed { index: 1, .. }), "{err}");
        assert_eq!(std::fs::read(&out).unwrap(), b"previous download", "a failed run must not touch the final name");
        assert_eq!(hits.lock()["/b.ts"], 1, "404 is not retried");
        assert_eq!(std::fs::read(with_suffix(&out, ".part")).unwrap(), b"AAAA");

        broken.store(false, Ordering::SeqCst);
        let segments = parse_hls_playlist(&client, &url, None).await.unwrap();
        download_to(&client, None, &url, segments, &out, 4).await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"AAAABBBBCCCC");
        assert_eq!(hits.lock()["/a.ts"], 1, "segments already written are not fetched again");
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
        let segments = parse_hls_playlist(&client, &url, None).await.unwrap();
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
            let segments = parse_hls_playlist(&client, &url, None).await.unwrap();
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
            async move { parse_hls_playlist(&client, &url, None).await }
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
        assert!(matches!(parse_hls_playlist(&client, &url, None).await, Err(HlsError::Unavailable(_))));

        let segments = parse_hls_playlist(&client, &url, Some(&auth)).await.unwrap();
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
                let segments = parse_hls_playlist(&client, &url, None).await.unwrap();
                let target = HlsEngine::prepare(&client, None, &url, &segments, &out, &None).await.unwrap();
                (target, segments)
            }
        };
        let fetches = |v: &str| hits.lock().iter().filter(|(p, _)| p.contains(&format!("v={v}&"))).map(|(_, n)| n).sum::<usize>();

        let pairs = [("/one/index.m3u8", "/two/index.m3u8"), ("/video/index.m3u8?q=720", "/video/index.m3u8?q=1080")];
        for (i, (paused, other)) in pairs.into_iter().enumerate() {
            let out = dir.path().join(format!("{i}.ts"));
            let (target, segments) = prepare(paused, out.clone()).await;
            let err = HlsEngine::download(&client, None, segments, target.unwrap(), 1, None, None).await.unwrap_err();
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
        HlsEngine::download(&client, None, segments, target.unwrap(), 1, None, None).await.unwrap();
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

        let segments = parse_hls_playlist(&client, &url(1), None).await.unwrap();
        assert!(download_to(&client, None, &url(1), segments, &out, 1).await.is_err());

        broken.store(false, Ordering::SeqCst);
        let segments = parse_hls_playlist(&client, &url(2), None).await.unwrap();
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
        let segments = parse_hls_playlist(&client, &url, None).await.unwrap();
        let (a, b) = (segments[0].init.as_ref().unwrap(), segments[1].init.as_ref().unwrap());
        assert!(Arc::ptr_eq(a, b), "segments must share one init section, not copies");

        let url = Url::parse(&format!("http://{}/many.m3u8", addr)).unwrap();
        assert!(matches!(parse_hls_playlist(&client, &url, None).await, Err(HlsError::Unsupported(_))));
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
        let segments = parse_hls_playlist(&client, &url, None).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&cancel);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            setter.store(true, Ordering::SeqCst);
        });
        let target = HlsEngine::prepare(&client, None, &url, &segments, &dir.path().join("x.ts"), &None).await.unwrap();
        let started = Instant::now();
        let result = HlsEngine::download(&client, None, segments, target.unwrap(), 2, None, Some(cancel)).await;
        assert!(matches!(result, Err(HlsError::Cancelled)));
        // Well under the stall timeout: the cancel must not wait for the fetch to give up.
        assert!(started.elapsed() < STALL_TIMEOUT);
    }
}
