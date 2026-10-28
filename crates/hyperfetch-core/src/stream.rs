//! What HLS and DASH streams have in common: their tracks and parts, choosing among the tracks by
//! the video and audio specs (see [`choose`]), and reading a stream's manifest for them (see
//! [`list_tracks`]).

use reqwest::Client;
use serde::{Deserialize, Serialize};
use url::Url;
use crate::hls::{FetchPolicy, HlsError, HlsSegment, Origin};
use crate::worker::Auth;

/// Whether a stream is HLS or DASH.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Hls,
    Dash,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Video,
    Audio,
    Subtitles,
}

/// The dynamic range of a video track.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DynamicRange {
    #[default]
    Sdr,
    /// HDR10 and other PQ video.
    Hdr10,
    Hlg,
    DolbyVision,
}

/// What a track is for, as its manifest says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Main,
    Alternate,
    Commentary,
    Dub,
    /// Audio that describes the video, for the blind.
    Description,
    /// Subtitles of everything heard, for the deaf.
    Caption,
    Subtitle,
    /// Subtitles of only what is not in the audio's language.
    Forced,
}

/// A track of a stream: a video (an HLS variant, which may carry its audio too), an audio or a
/// subtitles track. Its id is its kind's letter and its position among the manifest's tracks of
/// that kind (`v0`, `a1`, `s0`): the same manifest listed again gives the same ids.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub kind: Kind,
    /// As the manifest gives them (`avc1.640028,mp4a.40.2`); empty when it does not.
    pub codecs: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    /// Bits per second.
    pub bandwidth: Option<u64>,
    pub range: DynamicRange,
    pub channels: Option<u32>,
    /// A BCP 47 tag (`en`, `pt-BR`).
    pub language: Option<String>,
    pub name: Option<String>,
    pub role: Role,
    pub default: bool,
    /// HLS: the group of an audio or subtitles rendition (`GROUP-ID`), and the audio group a
    /// variant plays with (`AUDIO`). DASH: none.
    pub group: Option<String>,
}

/// A stretch of a stream: a run of HLS segments between `#EXT-X-DISCONTINUITY`s, a DASH `Period`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Part {
    /// Its position, from 0.
    pub id: usize,
    /// Seconds.
    pub duration: f64,
    /// Its video's resolution and codec (`1920x1080 avc`), else its audio codec.
    pub summary: String,
    /// It looks like an ad: tagged as one, or much shorter than the parts around it and unlike
    /// them.
    pub ad: bool,
}

/// What [`list_tracks`] finds in a stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub format: Format,
    pub live: bool,
    /// Seconds, when the manifest tells.
    pub duration: Option<f64>,
    pub tracks: Vec<Track>,
    pub parts: Vec<Part>,
}

/// Which tracks of an HLS or DASH stream a download takes (`DownloadOptions::tracks`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackChoice {
    /// The video spec (see [`VideoSpec::parse`]); empty is `best`.
    pub video: String,
    /// The audio spec (see [`AudioSpec::parse`]); empty is `best`.
    pub audio: String,
    /// Track ids from a listing ([`list_tracks`]): when given, they are the tracks taken, whatever
    /// the specs say.
    pub ids: Vec<String>,
}

const VIDEO_CODECS: [&str; 5] = ["avc", "hevc", "av1", "vp9", "vp8"];
const AUDIO_CODECS: [&str; 10] = ["aac", "mp3", "ac3", "eac3", "ac4", "opus", "flac", "alac", "dts", "vorbis"];

/// The codec family of one codec of a `CODECS` list (`avc1.640028` is `avc`), if it is one of
/// [`VIDEO_CODECS`] or [`AUDIO_CODECS`].
pub(crate) fn family(codec: &str) -> Option<&'static str> {
    let codec = codec.trim().to_ascii_lowercase();
    let fourcc = codec.split('.').next().unwrap_or_default();
    Some(match fourcc {
        "avc1" | "avc3" | "dva1" | "dvav" => "avc",
        "hvc1" | "hev1" | "dvh1" | "dvhe" => "hevc",
        "av01" | "dav1" => "av1",
        "vp09" | "vp9" => "vp9",
        "vp08" | "vp8" => "vp8",
        // MPEG-1/2 layer 3 inside MP4 is `mp4a.69` / `mp4a.6b`.
        "mp4a" if codec.starts_with("mp4a.69") || codec.starts_with("mp4a.6b") => "mp3",
        "mp4a" => "aac",
        "mp3" => "mp3",
        "ac-3" | "ac3" => "ac3",
        "ec-3" | "ec3" | "eac3" => "eac3",
        "ac-4" => "ac4",
        "opus" => "opus",
        "flac" => "flac",
        "alac" => "alac",
        "vorbis" => "vorbis",
        f if f.starts_with("dts") => "dts",
        _ => return None,
    })
}

/// The codec families a `CODECS` list names, in its order.
pub(crate) fn families(codecs: &str) -> impl Iterator<Item = &'static str> + '_ {
    codecs.split(',').filter_map(family)
}

pub(crate) fn is_audio_codec(codec: &str) -> bool {
    family(codec).is_some_and(|f| AUDIO_CODECS.contains(&f))
}

fn video_family(codecs: &str) -> Option<&'static str> {
    families(codecs).find(|f| VIDEO_CODECS.contains(f))
}

fn audio_family(codecs: &str) -> Option<&'static str> {
    families(codecs).find(|f| AUDIO_CODECS.contains(f))
}

/// The dynamic range `codecs` (Dolby Vision's own) and an HLS `VIDEO-RANGE` tell.
pub(crate) fn dynamic_range(codecs: &str, video_range: Option<&str>) -> DynamicRange {
    let dolby = codecs.split(',').any(|c| ["dvh1", "dvhe", "dva1", "dvav", "dav1"].contains(&c.trim().split('.').next().unwrap_or_default()));
    match video_range {
        _ if dolby => DynamicRange::DolbyVision,
        Some("PQ") => DynamicRange::Hdr10,
        Some("HLG") => DynamicRange::Hlg,
        _ => DynamicRange::Sdr,
    }
}

impl Track {
    /// One line for a listing: `v2 video 1920x1080 30fps avc hdr10 5000 kbps`, `a0 audio en aac 2ch
    /// "English" default`.
    pub fn describe(&self) -> String {
        let mut words = vec![self.id.clone(), format!("{:?}", self.kind).to_ascii_lowercase()];
        match (self.width, self.height) {
            (Some(w), Some(h)) => words.push(format!("{w}x{h}")),
            (None, Some(h)) => words.push(format!("{h}p")),
            _ => {}
        }
        if let Some(fps) = self.frame_rate {
            words.push(format!("{}fps", (fps * 100.0).round() / 100.0));
        }
        words.extend(self.language.clone());
        words.extend(families(&self.codecs).map(str::to_string));
        if self.range != DynamicRange::Sdr {
            words.push(format!("{:?}", self.range).to_ascii_lowercase());
        }
        words.extend(self.channels.map(|c| format!("{c}ch")));
        words.extend(self.bandwidth.map(|b| format!("{} kbps", b / 1000)));
        words.extend(self.name.as_ref().map(|n| format!("{n:?}")));
        if !matches!(self.role, Role::Main | Role::Alternate | Role::Subtitle) {
            words.push(format!("{:?}", self.role).to_ascii_lowercase());
        }
        if self.default {
            words.push("default".to_string());
        }
        words.join(" ")
    }
}

/// What tracks are there, for an error that says none fits.
fn listing<'a>(tracks: impl IntoIterator<Item = &'a Track>) -> String {
    tracks.into_iter().map(Track::describe).collect::<Vec<_>>().join("; ")
}

/// The video spec: comma-separated terms, all of which must hold. `best` (the default), `worst`
/// or `none`; `<N>p`, at most N lines; a codec (`avc`/`h264`, `hevc`/`h265`, `av1`, `vp9`); `sdr`,
/// `hdr` (HDR10/PQ or HLG) or `dv` (Dolby Vision); `<N>fps`, at most N frames a second.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoSpec {
    pub none: bool,
    pub worst: bool,
    pub max_height: Option<u32>,
    pub codec: Option<&'static str>,
    /// `sdr`, `hdr` or `dv`.
    pub range: Option<&'static str>,
    pub max_fps: Option<f64>,
}

impl VideoSpec {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut out = Self::default();
        for term in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match term.to_ascii_lowercase().as_str() {
                "best" => {}
                "worst" => out.worst = true,
                "none" => out.none = true,
                "avc" | "h264" => out.codec = Some("avc"),
                "hevc" | "h265" => out.codec = Some("hevc"),
                "av1" => out.codec = Some("av1"),
                "vp9" => out.codec = Some("vp9"),
                "sdr" => out.range = Some("sdr"),
                "hdr" => out.range = Some("hdr"),
                "dv" => out.range = Some("dv"),
                other => match (other.strip_suffix("fps"), other.strip_suffix('p')) {
                    (Some(fps), _) if fps.parse::<f64>().is_ok_and(|f| f.is_finite() && f > 0.0) => out.max_fps = fps.parse().ok(),
                    (_, Some(lines)) if lines.parse::<u32>().is_ok_and(|n| n > 0) => out.max_height = lines.parse().ok(),
                    _ => {
                        return Err(format!(
                            "Unknown video term \"{term}\" (best, worst, none, 1080p, avc, hevc, av1, vp9, sdr, hdr, dv, 30fps)"
                        ))
                    }
                },
            }
        }
        Ok(out)
    }

    fn fits(&self, track: &Track) -> bool {
        let range = |range: &str| match range {
            "sdr" => track.range == DynamicRange::Sdr,
            "hdr" => matches!(track.range, DynamicRange::Hdr10 | DynamicRange::Hlg),
            _ => track.range == DynamicRange::DolbyVision,
        };
        self.max_height.is_none_or(|max| track.height.is_none_or(|h| h <= max))
            && self.codec.is_none_or(|codec| video_family(&track.codecs) == Some(codec))
            && self.range.is_none_or(range)
            && self.max_fps.is_none_or(|max| track.frame_rate.is_none_or(|fps| fps <= max + 0.01))
    }
}

/// The audio spec: comma-separated language tags (each gets its best track, all of them kept in
/// that order; `en` takes `en-US` too), or `best` (the default: the default track's language,
/// else the first track's), `all` (the best track of every language) or `none`; plus constraints
/// every language's track must meet: a codec (`aac`, `ac3`, `eac3`, `opus`, `flac`) and `<N>ch`,
/// at least N channels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioSpec {
    pub none: bool,
    pub all: bool,
    pub languages: Vec<String>,
    pub codec: Option<&'static str>,
    pub min_channels: Option<u32>,
}

/// Whether `tag` reads as a BCP 47 language tag: a 2 or 3 letter language, then subtags.
fn is_language(tag: &str) -> bool {
    let mut parts = tag.split(['-', '_']);
    let primary = parts.next().unwrap_or_default();
    (2..=3).contains(&primary.len())
        && primary.bytes().all(|b| b.is_ascii_alphabetic())
        && parts.all(|p| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

impl AudioSpec {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut out = Self::default();
        for term in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match term.to_ascii_lowercase().as_str() {
                "best" => {}
                "all" => out.all = true,
                "none" => out.none = true,
                codec @ ("aac" | "ac3" | "eac3" | "opus" | "flac") => out.codec = AUDIO_CODECS.iter().copied().find(|c| *c == codec),
                other => match other.strip_suffix("ch").map(str::parse::<u32>) {
                    Some(Ok(n)) if n > 0 => out.min_channels = Some(n),
                    _ if is_language(term) => out.languages.push(term.to_string()),
                    _ => return Err(format!("Unknown audio term \"{term}\" (a language such as en or pt-BR, best, all, none, aac, ac3, eac3, opus, flac, 6ch)")),
                },
            }
        }
        Ok(out)
    }

    fn fits(&self, track: &Track) -> bool {
        self.codec.is_none_or(|codec| families(&track.codecs).any(|f| f == codec))
            && self.min_channels.is_none_or(|min| track.channels.is_some_and(|c| c >= min))
    }
}

/// Whether the track language `lang` is the language `asked` for: `en` is `en-US` and `eng` too;
/// `pt-BR` only `pt-BR` and what is under it.
fn same_language(asked: &str, lang: &str) -> bool {
    let lower = |s: &str| s.trim().to_ascii_lowercase().replace('_', "-");
    let (asked, lang) = (lower(asked), lower(lang));
    if asked.contains('-') {
        return lang == asked || lang.starts_with(&format!("{asked}-"));
    }
    let primary = lang.split('-').next().unwrap_or_default();
    let iso = crate::media::mp4_language;
    primary == asked || (iso(&asked) != "und" && iso(&asked) == iso(primary))
}

/// The tracks of `info` that `choice` takes, by their positions in `info.tracks`: the video, then
/// the audio tracks in the order asked for; with ids, those tracks, videos first. A spec that no
/// track fits is an error that lists what the stream has (never something else in its place),
/// except that `best` always finds a track. A stream without video takes none, whatever the video
/// spec, and one whose only audio is inside its video takes that, whatever the audio spec.
/// Subtitles are chosen by the subtitle languages, not here.
pub fn choose(info: &StreamInfo, choice: &TrackChoice) -> Result<Vec<usize>, String> {
    let tracks = &info.tracks;
    if !choice.ids.is_empty() {
        let mut chosen = Vec::new();
        for id in choice.ids.iter().map(|id| id.trim()).filter(|id| !id.is_empty()) {
            let at = tracks.iter().position(|t| t.id == id).ok_or_else(|| format!("The stream has no track {id}. It has: {}", listing(tracks)))?;
            if !chosen.contains(&at) {
                chosen.push(at);
            }
        }
        chosen.sort_by_key(|&at| tracks[at].kind as u8);
        return Ok(chosen);
    }
    let video = pick_video(info, &VideoSpec::parse(&choice.video)?, &choice.video)?;
    let audio = pick_audio(info, video.map(|v| &tracks[v]), &AudioSpec::parse(&choice.audio)?, &choice.audio)?;
    Ok(video.into_iter().chain(audio).collect())
}

fn pick_video(info: &StreamInfo, spec: &VideoSpec, text: &str) -> Result<Option<usize>, String> {
    let tracks = &info.tracks;
    let videos: Vec<usize> = (0..tracks.len()).filter(|&at| tracks[at].kind == Kind::Video).collect();
    if spec.none || videos.is_empty() {
        return Ok(None);
    }
    let fits: Vec<usize> = videos.iter().copied().filter(|&at| spec.fits(&tracks[at])).collect();
    if fits.is_empty() {
        return Err(format!("No video track fits \"{text}\". The stream has: {}", listing(videos.iter().map(|&at| &tracks[at]))));
    }
    // An HLS variant that plays with audio beats a silent one, however good.
    let audible = |v: &Track| {
        info.format == Format::Dash
            || v.codecs.is_empty()
            || audio_family(&v.codecs).is_some()
            || tracks.iter().any(|a| a.kind == Kind::Audio && a.group.is_some() && a.group == v.group)
    };
    let heard: Vec<usize> = fits.iter().copied().filter(|&at| audible(&tracks[at])).collect();
    let pool = if heard.is_empty() { fits } else { heard };
    let rank = |at: &usize| (tracks[*at].height.unwrap_or(0), tracks[*at].bandwidth.unwrap_or(0));
    Ok(if spec.worst { pool.into_iter().min_by_key(rank) } else { pool.into_iter().max_by_key(rank) })
}

fn pick_audio(info: &StreamInfo, video: Option<&Track>, spec: &AudioSpec, text: &str) -> Result<Vec<usize>, String> {
    let tracks = &info.tracks;
    // An HLS variant plays with the renditions of its own audio group only.
    let pairs = |a: &Track| match (info.format, video) {
        (Format::Hls, Some(v)) => a.group.is_some() && a.group == v.group,
        _ => true,
    };
    let audios: Vec<usize> = (0..tracks.len()).filter(|&at| tracks[at].kind == Kind::Audio && pairs(&tracks[at])).collect();
    if spec.none || audios.is_empty() {
        return Ok(Vec::new());
    }
    let has = || listing(audios.iter().map(|&at| &tracks[at]));
    let fits: Vec<usize> = audios.iter().copied().filter(|&at| spec.fits(&tracks[at])).collect();
    if fits.is_empty() {
        return Err(format!("No audio track fits \"{text}\". The stream has: {}", has()));
    }
    let rank = |at: &usize| {
        let t = &tracks[*at];
        (!matches!(t.role, Role::Commentary | Role::Description), t.bandwidth.unwrap_or(0), t.channels.unwrap_or(0), t.default)
    };
    let best_of = |lang: Option<&str>| -> Option<usize> {
        let speaks = |t: &Track| match (lang, &t.language) {
            (Some(asked), Some(own)) => same_language(asked, own),
            (None, None) => true,
            _ => false,
        };
        fits.iter().copied().filter(|&at| speaks(&tracks[at])).max_by_key(rank)
    };
    let mut chosen = Vec::new();
    if spec.all {
        let mut seen = Vec::new();
        for &at in &fits {
            let lang = tracks[at].language.as_ref().map(|l| l.to_ascii_lowercase());
            if !seen.contains(&lang) {
                chosen.extend(best_of(tracks[at].language.as_deref()));
                seen.push(lang);
            }
        }
    } else if spec.languages.is_empty() {
        let lead = fits.iter().copied().find(|&at| tracks[at].default).unwrap_or(fits[0]);
        chosen.extend(best_of(tracks[lead].language.as_deref()));
    } else {
        for lang in &spec.languages {
            let at = best_of(Some(lang.as_str())).ok_or_else(|| format!("No audio track in \"{lang}\" fits \"{text}\". The stream has: {}", has()))?;
            chosen.push(at);
        }
    }
    let mut unique = Vec::new();
    for at in chosen {
        if !unique.contains(&at) {
            unique.push(at);
        }
    }
    Ok(unique)
}

/// Marks as ads the parts that are much shorter than the parts around them (less than half of
/// each) and unlike them (`settings`, per part, differ).
pub(crate) fn mark_ads(parts: &mut [Part], settings: &[String]) {
    let lengths: Vec<f64> = parts.iter().map(|p| p.duration).collect();
    for (at, part) in parts.iter_mut().enumerate() {
        let neighbours: Vec<usize> = [at.checked_sub(1), Some(at + 1).filter(|&n| n < lengths.len())].into_iter().flatten().collect();
        let odd = |n: &usize| lengths[at] * 2.0 < lengths[*n] && settings.get(at) != settings.get(*n);
        part.ad |= !neighbours.is_empty() && neighbours.iter().all(odd);
    }
}

/// The parts of an HLS media playlist's `segments` (see [`Part`]), each described by `summary`.
fn hls_parts(segments: &[HlsSegment], summary: &str) -> Vec<Part> {
    let (mut parts, mut settings): (Vec<Part>, Vec<String>) = (Vec::new(), Vec::new());
    for segment in segments {
        if parts.is_empty() || segment.discontinuity {
            parts.push(Part { id: parts.len(), duration: 0.0, summary: summary.to_string(), ad: false });
            // Where its segments and init section come from.
            let folder = segment.url.join(".").map_or_else(|_| segment.url.to_string(), |u| u.to_string());
            settings.push(format!("{folder} {:?}", segment.init.as_ref().map(|i| i.url.as_str())));
        }
        if let Some(part) = parts.last_mut() {
            part.duration += segment.duration_secs;
        }
    }
    mark_ads(&mut parts, &settings);
    parts
}

/// A track's resolution and codec, or its audio codec, for a [`Part`]'s summary.
pub(crate) fn summary_of(track: &Track) -> String {
    let size = track.height.map(|h| track.width.map_or(format!("{h}p"), |w| format!("{w}x{h}")));
    size.into_iter().chain(families(&track.codecs).next().map(str::to_string)).collect::<Vec<_>>().join(" ")
}

/// Where the segments of a track of an [`Opened`] stream come from.
enum Source {
    /// Read already: an HLS media playlist given as the link.
    Read(Vec<HlsSegment>),
    /// An HLS media playlist (a variant or a rendition).
    Playlist(Url),
    /// Inside its variants' own segments: an HLS audio rendition without a playlist of its own.
    InVideo,
    /// A DASH representation, followed across the periods.
    Dash(crate::dash::Rendition),
}

/// A stream's manifest, read: what it lists, and where each track's segments come from.
pub(crate) struct Opened {
    pub info: StreamInfo,
    /// Per track of `info.tracks`.
    sources: Vec<Source>,
    /// An HLS master playlist's WebVTT subtitle renditions, saved with a download of it.
    pub subtitles: Vec<crate::hls::Subtitle>,
}

/// Reads the HLS playlist or DASH manifest at `url`, or `text` in its place (a playlist a page
/// built itself, whose URIs resolve against `url`). [`HlsError::InvalidPlaylist`] when it is
/// neither (a plain file, maybe); [`HlsError::Live`] for a live HLS media playlist. A live DASH
/// manifest is read, with `info.live` set.
pub(crate) async fn open(
    client: &Client,
    url: &Url,
    text: Option<&str>,
    auth: Option<&Auth>,
    fetch: FetchPolicy,
) -> Result<Opened, HlsError> {
    let (body, base) = match text {
        Some(text) => (text.as_bytes().to_vec(), url.clone()),
        None => crate::hls::fetch_manifest(client, auth, url, fetch, true).await?,
    };
    let body = String::from_utf8_lossy(&body);
    let body = body.trim_start_matches('\u{feff}').trim_start();
    if body.starts_with("#EXTM3U") {
        if !crate::hls::is_master(body) {
            let segments = crate::hls::parse_media_playlist(client, auth, body, &base, fetch).await?;
            let info = StreamInfo {
                format: Format::Hls,
                live: false,
                duration: Some(segments.iter().map(|s| s.duration_secs).sum()),
                parts: hls_parts(&segments, ""),
                tracks: vec![Track { id: "v0".to_string(), ..Default::default() }],
            };
            return Ok(Opened { info, sources: vec![Source::Read(segments)], subtitles: Vec::new() });
        }
        let (tracks, sources): (Vec<Track>, Vec<Source>) = crate::hls::master_tracks(body, &base)
            .into_iter()
            .map(|(track, url)| (track, url.map_or(Source::InVideo, Source::Playlist)))
            .unzip();
        if !sources.iter().any(|s| matches!(s, Source::Playlist(_))) {
            return Err(HlsError::Unsupported("malformed playlist: no variant stream URI found".to_string()));
        }
        let info = StreamInfo { format: Format::Hls, live: false, duration: None, tracks, parts: Vec::new() };
        return Ok(Opened { info, sources, subtitles: crate::hls::subtitle_renditions(body, &base) });
    }
    if crate::dash::is_mpd(body.as_bytes()) {
        let mpd = crate::dash::parse(body, &base)?;
        let (tracks, sources) = mpd.tracks.into_iter().map(|(track, rendition)| (track, Source::Dash(rendition))).unzip();
        let info = StreamInfo { format: Format::Dash, live: mpd.live, duration: mpd.duration, tracks, parts: mpd.parts };
        return Ok(Opened { info, sources, subtitles: Vec::new() });
    }
    Err(HlsError::InvalidPlaylist("neither an HLS playlist nor a DASH manifest".to_string()))
}

impl Opened {
    /// The segments of track `at`; None for one inside its video's segments.
    pub async fn segments(&self, client: &Client, auth: Option<&Auth>, fetch: FetchPolicy, at: usize) -> Result<Option<Vec<HlsSegment>>, HlsError> {
        match &self.sources[at] {
            Source::Read(segments) => Ok(Some(segments.clone())),
            Source::Playlist(url) => crate::hls::media_segments(client, auth, url, fetch).await.map(Some),
            Source::InVideo => Ok(None),
            Source::Dash(rendition) => crate::dash::segments(client, auth, fetch, rendition).await.map(Some),
        }
    }

    /// The file extension of track `at`, whose segments are `segments`: a DASH representation's
    /// by its type; an HLS track's `mp4` (`m4a` for audio) when they have an init section, else
    /// that of packed audio (`.aac`, ...) its segments name, else `ts`.
    pub fn extension(&self, at: usize, segments: &[HlsSegment]) -> &'static str {
        if let Source::Dash(rendition) = &self.sources[at] {
            return rendition.ext;
        }
        if segments.iter().any(|s| s.init.is_some()) {
            return if self.info.tracks[at].kind == Kind::Audio { "m4a" } else { "mp4" };
        }
        let named = segments.first().and_then(|s| s.url.path().rsplit_once('.')).map(|(_, ext)| ext.to_ascii_lowercase());
        ["aac", "mp3", "ac3", "ec3"].into_iter().find(|ext| named.as_deref() == Some(*ext)).unwrap_or("ts")
    }

    /// [`Self::info`], with the length and parts of an HLS master playlist's stream, read from
    /// the playlist of the first track `choice` takes (else its first variant's); live if that is.
    pub async fn describe(self, client: &Client, auth: Option<&Auth>, fetch: FetchPolicy, choice: &TrackChoice) -> Result<StreamInfo, HlsError> {
        let mut info = self.info;
        if info.format != Format::Hls || !info.parts.is_empty() {
            return Ok(info);
        }
        let listed = |at: &usize| matches!(self.sources[*at], Source::Playlist(_));
        let chosen = choose(&info, choice).unwrap_or_default().into_iter().find(listed);
        let Some(at) = chosen.or_else(|| (0..self.sources.len()).find(listed)) else { return Ok(info) };
        let Source::Playlist(url) = &self.sources[at] else { return Ok(info) };
        match crate::hls::media_segments(client, auth, url, fetch).await {
            Ok(segments) => {
                info.duration = Some(segments.iter().map(|s| s.duration_secs).sum());
                info.parts = hls_parts(&segments, &summary_of(&info.tracks[at]));
            }
            Err(HlsError::Live) => info.live = true,
            Err(e) => return Err(e),
        }
        Ok(info)
    }
}

/// The segments of `origin`'s track, its manifest read again (see `hls::Refresh`); with no track,
/// the stream's only one as the default spec chooses it.
pub(crate) async fn relist(client: &Client, auth: Option<&Auth>, origin: &Origin, fetch: FetchPolicy) -> Result<Vec<HlsSegment>, HlsError> {
    let opened = open(client, &origin.manifest, None, auth, fetch).await?;
    let at = match &origin.track {
        Some(id) => opened.info.tracks.iter().position(|t| &t.id == id),
        None => {
            let chosen = choose(&opened.info, &TrackChoice::default()).map_err(HlsError::NoMatch)?;
            let own: Vec<usize> = chosen.into_iter().filter(|&at| !matches!(opened.sources[at], Source::InVideo)).collect();
            if own.len() > 1 {
                return Err(HlsError::Unsupported("its video and audio are apart: several tracks to download".to_string()));
            }
            own.first().copied()
        }
    };
    let at = at.ok_or_else(|| HlsError::Unavailable(format!("{} no longer lists the track", origin.manifest)))?;
    opened.segments(client, auth, fetch, at).await?.ok_or(HlsError::NoSegments)
}

/// What the HLS or DASH stream at `url` holds: its tracks, parts, length and whether it is live,
/// read with the client, headers, cookies, proxy and sign-in a download of it with `options`
/// would have. Downloads no media.
pub async fn list_tracks(url: &Url, options: &crate::engine::DownloadOptions) -> Result<StreamInfo, String> {
    crate::engine::DownloadEngine::new(vec![url.clone()], options.clone()).stream_info().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, kind: Kind, codecs: &str) -> Track {
        Track { id: id.to_string(), kind, codecs: codecs.to_string(), ..Default::default() }
    }

    fn video(id: &str, height: u32, codecs: &str, bandwidth: u64) -> Track {
        Track { height: Some(height), width: Some(height * 16 / 9), bandwidth: Some(bandwidth), ..track(id, Kind::Video, codecs) }
    }

    fn audio(id: &str, lang: &str, codecs: &str, channels: u32, bandwidth: u64) -> Track {
        Track { language: Some(lang.to_string()), channels: Some(channels), bandwidth: Some(bandwidth), ..track(id, Kind::Audio, codecs) }
    }

    fn dash(tracks: Vec<Track>) -> StreamInfo {
        StreamInfo { format: Format::Dash, live: false, duration: None, tracks, parts: Vec::new() }
    }

    fn ids(info: &StreamInfo, video: &str, audio: &str) -> Result<Vec<String>, String> {
        let choice = TrackChoice { video: video.into(), audio: audio.into(), ids: Vec::new() };
        choose(info, &choice).map(|chosen| chosen.into_iter().map(|at| info.tracks[at].id.clone()).collect())
    }

    /// The specs parse into their terms; an unknown term is an error that names it.
    #[test]
    fn test_specs_parse_and_refuse_unknown_terms() {
        let spec = VideoSpec::parse(" 1080P, h265 ,HDR,60fps").unwrap();
        assert_eq!((spec.max_height, spec.codec, spec.range, spec.max_fps), (Some(1080), Some("hevc"), Some("hdr"), Some(60.0)));
        assert!(VideoSpec::parse("").unwrap() == VideoSpec::default() && VideoSpec::parse("none").unwrap().none);
        assert!(VideoSpec::parse("1080i").unwrap_err().contains("\"1080i\""));
        let spec = AudioSpec::parse("en,ja,pt-BR,eac3,6ch").unwrap();
        assert_eq!((spec.languages, spec.codec, spec.min_channels), (vec!["en".to_string(), "ja".into(), "pt-BR".into()], Some("eac3"), Some(6)));
        assert!(AudioSpec::parse("all").unwrap().all && AudioSpec::parse("none").unwrap().none);
        assert!(AudioSpec::parse("english!").is_err() && AudioSpec::parse("0ch").is_err());
    }

    /// The best video under the limits wins; the worst goes the other way; nothing fitting is an
    /// error listing the stream's videos, never another video.
    #[test]
    fn test_video_spec_picks_within_its_limits() {
        let info = dash(vec![
            video("v0", 2160, "hvc1.2.4.L153", 16_000_000),
            Track { range: DynamicRange::Hdr10, ..video("v1", 1080, "hvc1.2.4.L123", 6_000_000) },
            video("v2", 1080, "avc1.640028", 5_000_000),
            Track { frame_rate: Some(60.0), ..video("v3", 720, "avc1.64001f", 3_000_000) },
            audio("a0", "en", "mp4a.40.2", 2, 128_000),
        ]);
        assert_eq!(ids(&info, "", "").unwrap(), ["v0", "a0"]);
        assert_eq!(ids(&info, "1080p", "").unwrap(), ["v1", "a0"]);
        assert_eq!(ids(&info, "1080p,sdr", "").unwrap(), ["v2", "a0"]);
        assert_eq!(ids(&info, "avc,30fps", "").unwrap(), ["v2", "a0"]);
        assert_eq!(ids(&info, "worst", "none").unwrap(), ["v3"]);
        assert_eq!(ids(&info, "none", "").unwrap(), ["a0"]);
        let err = ids(&info, "480p", "").unwrap_err();
        assert!(err.contains("No video track fits \"480p\"") && err.contains("v3 video 1280x720 60fps avc"), "{err}");
        assert!(ids(&info, "av1", "").is_err() && ids(&info, "dv", "").is_err());
    }

    /// Languages each get their best track, in the order asked, `en` taking `en-US` and `eng`;
    /// `best` is the default track's language; `all` one track per language; constraints hold for
    /// every language, and a language the stream lacks is an error that lists its audio.
    #[test]
    fn test_audio_spec_picks_languages_in_order() {
        let info = dash(vec![
            video("v0", 1080, "avc1.640028", 5_000_000),
            audio("a0", "en-US", "mp4a.40.2", 2, 128_000),
            audio("a1", "en-US", "ec-3", 6, 384_000),
            Track { role: Role::Description, ..audio("a2", "en", "mp4a.40.2", 2, 500_000) },
            Track { default: true, ..audio("a3", "ja", "mp4a.40.2", 2, 96_000) },
            audio("a4", "jpn", "mp4a.40.2", 2, 192_000),
            audio("a5", "pt-BR", "mp4a.40.2", 2, 128_000),
        ]);
        assert_eq!(ids(&info, "", "").unwrap(), ["v0", "a4"], "the default track's language, its best track");
        assert_eq!(ids(&info, "", "en,ja").unwrap(), ["v0", "a1", "a4"], "no description track while there are others");
        assert_eq!(ids(&info, "", "ja,en,aac").unwrap(), ["v0", "a4", "a0"]);
        assert_eq!(ids(&info, "", "eng,6ch").unwrap(), ["v0", "a1"]);
        assert_eq!(ids(&info, "", "pt").unwrap(), ["v0", "a5"]);
        assert_eq!(ids(&info, "", "all").unwrap(), ["v0", "a1", "a4", "a5"]);
        assert_eq!(ids(&info, "", "none").unwrap(), ["v0"]);
        let err = ids(&info, "", "en,de").unwrap_err();
        assert!(err.contains("\"de\"") && err.contains("a5 audio pt-BR aac 2ch 128 kbps"), "{err}");
        assert!(ids(&info, "", "best,opus").is_err() && ids(&info, "", "pt-PT").is_err());
    }

    /// Ids win over the specs and come videos first; an unknown id is an error.
    #[test]
    fn test_ids_win_over_specs() {
        let info = dash(vec![video("v0", 1080, "avc1", 1), video("v1", 720, "avc1", 1), audio("a0", "en", "mp4a.40.2", 2, 1)]);
        let choice = TrackChoice { video: "none".into(), audio: "none".into(), ids: vec!["a0".into(), " v1".into(), "a0".into()] };
        assert_eq!(choose(&info, &choice).unwrap(), [1, 2]);
        let choice = TrackChoice { ids: vec!["v9".into()], ..Default::default() };
        assert!(choose(&info, &choice).unwrap_err().contains("no track v9"));
    }

    /// An HLS variant takes the renditions of its own audio group; one with its audio inside wins
    /// over a better silent one, and takes no rendition; a stream with no audio track of its own
    /// takes none, whatever the spec.
    #[test]
    fn test_hls_variants_pair_with_their_audio_group() {
        let hls = |tracks| StreamInfo { format: Format::Hls, ..dash(tracks) };
        let grouped = |t: Track, g: &str| Track { group: Some(g.to_string()), ..t };
        let info = hls(vec![
            grouped(video("v0", 720, "avc1.64001f,mp4a.40.2", 2_000_000), "low"),
            grouped(video("v1", 1080, "avc1.640028,ec-3", 6_000_000), "high"),
            video("v2", 2160, "avc1.640033", 9_000_000),
            grouped(audio("a0", "en", "mp4a.40.2", 2, 0), "low"),
            grouped(audio("a1", "en", "ec-3", 6, 0), "high"),
            grouped(audio("a2", "fr", "ec-3", 6, 0), "high"),
        ]);
        assert_eq!(ids(&info, "", "").unwrap(), ["v1", "a1"], "the silent 2160p loses");
        assert!(ids(&info, "720p", "fr").unwrap_err().contains("\"fr\""));
        assert_eq!(ids(&info, "720p", "en").unwrap(), ["v0", "a0"]);
        assert_eq!(ids(&info, "none", "fr").unwrap(), ["a2"]);
        let muxed = hls(vec![video("v0", 720, "avc1.64001f,mp4a.40.2", 1), video("v1", 1080, "avc1.640028,mp4a.40.2", 2)]);
        assert_eq!(ids(&muxed, "", "en,6ch").unwrap(), ["v1"]);
    }

    /// A part much shorter than its neighbours and unlike them looks like an ad; one alike, or
    /// alone, does not.
    #[test]
    fn test_short_odd_parts_look_like_ads() {
        let part = |id, duration| Part { id, duration, summary: String::new(), ad: false };
        let mut parts = vec![part(0, 600.0), part(1, 30.0), part(2, 600.0), part(3, 20.0)];
        let settings: Vec<String> = ["main", "ad", "main", "main"].map(String::from).to_vec();
        mark_ads(&mut parts, &settings);
        assert_eq!(parts.iter().map(|p| p.ad).collect::<Vec<_>>(), [false, true, false, false]);
        let mut alone = vec![part(0, 5.0)];
        mark_ads(&mut alone, &["x".to_string()]);
        assert!(!alone[0].ad);
    }
}
