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

/// The subtitles tracks of `info` in the languages `langs` asks for (`DownloadOptions::subtitles`:
/// `all`, `none` or a comma-separated list such as `en,pt-BR`; None takes them all, as an HLS
/// download always did), each language and name once, at most [`crate::hls::MAX_SUBTITLES`].
pub fn choose_subtitles(info: &StreamInfo, langs: Option<&str>) -> Vec<usize> {
    let asked: Vec<&str> = langs.map_or(vec!["all"], |langs| langs.split(',').map(str::trim).filter(|l| !l.is_empty()).collect());
    let all = asked.iter().any(|lang| lang.eq_ignore_ascii_case("all"));
    let mut seen = Vec::new();
    let mut chosen = Vec::new();
    for (at, track) in info.tracks.iter().enumerate().filter(|(_, t)| t.kind == Kind::Subtitles) {
        let wanted = all || asked.iter().any(|asked| track.language.as_deref().is_some_and(|lang| same_language(asked, lang)));
        if wanted && !seen.contains(&(&track.language, &track.name)) && chosen.len() < crate::hls::MAX_SUBTITLES {
            seen.push((&track.language, &track.name));
            chosen.push(at);
        }
    }
    chosen
}

/// Whether `text` (a segment link, a channel's name) holds what `pattern` describes, ignoring
/// case: `*` stands for any run of characters, anything else for itself, anywhere in `text`
/// (`ad*.ts` matches `https://cdn/ad_01.ts?x`). A blank pattern matches nothing.
pub fn pattern_matches(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.trim().to_lowercase(), text.to_lowercase());
    if pattern.is_empty() {
        return false;
    }
    let mut rest = text.as_str();
    for piece in pattern.split('*').filter(|piece| !piece.is_empty()) {
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    true
}

/// Checks a pattern the user gives (`DownloadOptions::ad_patterns`, `ingest::ListOptions::channels`,
/// see [`pattern_matches`]): it may not be blank.
pub fn check_pattern(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("A pattern may not be empty (e.g. ad*.ts, or *news*)".to_string());
    }
    Ok(())
}

/// Highest part id [`parse_part_ids`] takes.
const MAX_PART_ID: usize = 100_000;

/// The part ids `text` gives (`0,2-4`, see `DownloadOptions::drop_parts`), each once, in order.
pub fn parse_part_ids(text: &str) -> Result<Vec<usize>, String> {
    let mut ids = Vec::new();
    for term in text.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let bad = || format!("\"{term}\" is not a part number or range of them (e.g. 0,2-4)");
        let (from, to) = term.split_once('-').unwrap_or((term, term));
        let number = |n: &str| n.trim().parse::<usize>().map_err(|_| bad());
        let (from, to) = (number(from)?, number(to)?);
        if from > to || to > MAX_PART_ID {
            return Err(bad());
        }
        ids.extend(from..=to);
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// What a download leaves out of an HLS or DASH stream: the segments outside every section (by
/// when they play, from the track's start: one that overlaps a section is kept whole), those of
/// the parts dropped, and those whose links an ad pattern matches (see `DownloadOptions::sections`,
/// `drop_parts` and `ad_patterns`).
#[derive(Default)]
pub(crate) struct Cuts<'a> {
    pub sections: &'a [(f64, f64)],
    pub parts: &'a [usize],
    pub ads: &'a [String],
}

impl Cuts<'_> {
    /// `segments` of one track, each in the part `parts` gives it (see [`Opened::parts_of`]),
    /// without what is cut, numbered again; what follows a gap starts a new run (a discontinuity).
    pub fn apply(&self, segments: Vec<HlsSegment>, parts: &[usize]) -> Vec<HlsSegment> {
        let (mut out, mut at, mut gap) = (Vec::with_capacity(segments.len()), 0.0, false);
        for (n, segment) in segments.into_iter().enumerate() {
            let (from, to) = (at, at + segment.duration_secs);
            at = to;
            let kept = (self.sections.is_empty() || self.sections.iter().any(|&(start, end)| from < end && to > start))
                && !parts.get(n).is_some_and(|part| self.parts.contains(part))
                && !self.ads.iter().any(|pattern| pattern_matches(pattern, segment.url.as_str()));
            if !kept {
                gap = true;
                continue;
            }
            let discontinuity = (std::mem::take(&mut gap) && !out.is_empty()) || segment.discontinuity;
            out.push(HlsSegment { index: out.len(), discontinuity, ..segment });
        }
        out
    }
}

/// The variables a stream's file-name pattern (`DownloadOptions::name_pattern`) takes, each in
/// braces: `{name}` is the name the download would get without one.
pub const NAME_VARIABLES: [&str; 11] = ["name", "height", "width", "fps", "codec", "range", "audio", "bitrate", "channel", "date", "time"];

/// Checks a file-name pattern: not blank, and every `{...}` in it one of [`NAME_VARIABLES`].
pub fn check_name_pattern(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("The name pattern is empty (e.g. {name} {height}p)".to_string());
    }
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        let close = rest[open..].find('}').ok_or("The name pattern has a { without its }")? + open;
        let name = &rest[open + 1..close];
        if !NAME_VARIABLES.contains(&name) {
            return Err(format!("Unknown name variable {{{name}}}: the variables are {{{}}}", NAME_VARIABLES.join("}, {")));
        }
        rest = &rest[close + 1..];
    }
    Ok(())
}

/// `pattern` with each `{variable}` (see [`NAME_VARIABLES`]) replaced by what `value` gives for
/// it, empty when nothing; then brackets left empty go, a run of separators doubled by an empty
/// value (`" -  - "`) becomes one, and the ends lose theirs. Not yet made safe as a file name.
pub(crate) fn fill_name(pattern: &str, value: impl Fn(&str) -> Option<String>) -> String {
    let mut filled = String::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        filled.push_str(&rest[..open]);
        match rest[open..].find('}').map(|close| close + open) {
            Some(close) if NAME_VARIABLES.contains(&&rest[open + 1..close]) => {
                filled.push_str(&value(&rest[open + 1..close]).unwrap_or_default());
                rest = &rest[close + 1..];
            }
            _ => {
                filled.push('{');
                rest = &rest[open + 1..];
            }
        }
    }
    filled.push_str(rest);
    for empty in ["()", "[]", "( )", "[ ]"] {
        filled = filled.replace(empty, "");
    }
    const SEPARATORS: [char; 4] = [' ', '-', '_', '.'];
    let mut out = String::with_capacity(filled.len());
    let mut chars = filled.chars().peekable();
    while let Some(c) = chars.next() {
        if !SEPARATORS.contains(&c) {
            out.push(c);
            continue;
        }
        let mut run = String::from(c);
        while let Some(next) = chars.next_if(|next| SEPARATORS.contains(next)) {
            run.push(next);
        }
        // `" -  - "` is `" - "`: spaces around one of each separator.
        let mut core: Vec<char> = run.chars().filter(|c| *c != ' ').collect();
        core.dedup();
        if run.starts_with(' ') || core.is_empty() {
            out.push(' ');
        }
        out.extend(&core);
        if run.ends_with(' ') && !core.is_empty() {
            out.push(' ');
        }
    }
    out.trim_matches(&SEPARATORS[..]).to_string()
}

/// What a track variable of a file-name pattern (see [`NAME_VARIABLES`]) is for the `chosen`
/// tracks of a download, its video first: None when they do not tell, and for the others.
pub(crate) fn track_value(variable: &str, chosen: &[&Track]) -> Option<String> {
    let video = chosen.iter().find(|t| t.kind == Kind::Video);
    Some(match variable {
        "height" => video?.height?.to_string(),
        "width" => video?.width?.to_string(),
        "fps" => ((video?.frame_rate? * 100.0).round() / 100.0).to_string(),
        "codec" => video_family(&video?.codecs)?.to_string(),
        "range" => match video?.range {
            DynamicRange::Sdr => "sdr",
            DynamicRange::Hdr10 | DynamicRange::Hlg => "hdr",
            DynamicRange::DolbyVision => "dv",
        }
        .to_string(),
        "audio" => chosen.iter().filter(|t| t.kind == Kind::Audio).filter_map(|t| t.language.clone()).collect::<Vec<_>>().join("+"),
        "bitrate" => (video.or(chosen.first())?.bandwidth? / 1000).to_string(),
        _ => return None,
    })
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
}

/// Reads the HLS playlist or DASH manifest at `url`, or `text` in its place (a playlist a page
/// built itself, whose URIs resolve against `url`). [`HlsError::InvalidPlaylist`] when it is
/// neither (a plain file, maybe, or a list of channels: see `ingest::parse_channels`);
/// [`HlsError::Live`] for a live HLS media playlist. A live DASH manifest is read, with
/// `info.live` set.
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
        // Its entries are streams of their own, which would be fetched as segments: some never end.
        if crate::ingest::parse_channels(body).is_some() {
            return Err(HlsError::InvalidPlaylist("a list of channels, not a stream".to_string()));
        }
        if !crate::hls::is_master(body) {
            let segments = crate::hls::parse_media_playlist(client, auth, body, &base, fetch).await?;
            let info = StreamInfo {
                format: Format::Hls,
                live: false,
                duration: Some(segments.iter().map(|s| s.duration_secs).sum()),
                parts: hls_parts(&segments, ""),
                tracks: vec![Track { id: "v0".to_string(), ..Default::default() }],
            };
            return Ok(Opened { info, sources: vec![Source::Read(segments)] });
        }
        let (tracks, sources): (Vec<Track>, Vec<Source>) = crate::hls::master_tracks(body, &base)
            .into_iter()
            .map(|(track, url)| (track, url.map_or(Source::InVideo, Source::Playlist)))
            .unzip();
        if !sources.iter().any(|s| matches!(s, Source::Playlist(_))) {
            return Err(HlsError::Unsupported("malformed playlist: no variant stream URI found".to_string()));
        }
        let info = StreamInfo { format: Format::Hls, live: false, duration: None, tracks, parts: Vec::new() };
        return Ok(Opened { info, sources });
    }
    if crate::dash::is_mpd(body.as_bytes()) {
        let mpd = crate::dash::parse(body, &base)?;
        let (tracks, sources) = mpd.tracks.into_iter().map(|(track, rendition)| (track, Source::Dash(rendition))).unzip();
        let info = StreamInfo { format: Format::Dash, live: mpd.live, duration: mpd.duration, tracks, parts: mpd.parts };
        return Ok(Opened { info, sources });
    }
    Err(HlsError::InvalidPlaylist("neither an HLS playlist nor a DASH manifest".to_string()))
}

impl Opened {
    /// The segments of track `at`; None for one inside its video's segments. An HLS subtitles
    /// rendition whose link is no playlist is taken for one WebVTT file.
    pub async fn segments(&self, client: &Client, auth: Option<&Auth>, fetch: FetchPolicy, at: usize) -> Result<Option<Vec<HlsSegment>>, HlsError> {
        match &self.sources[at] {
            Source::Read(segments) => Ok(Some(segments.clone())),
            Source::Playlist(url) => match crate::hls::media_segments(client, auth, url, fetch).await {
                Err(HlsError::Unavailable(_)) if self.info.tracks[at].kind == Kind::Subtitles => {
                    let whole = HlsSegment { index: 0, sequence: 0, url: url.clone(), duration_secs: 0.0, byte_range: None, encryption: None, init: None, discontinuity: false };
                    Ok(Some(vec![whole]))
                }
                listed => listed.map(Some),
            },
            Source::InVideo => Ok(None),
            Source::Dash(rendition) => crate::dash::segments(client, auth, fetch, rendition).await.map(Some),
        }
    }

    /// The part (see [`Part`]) each of `segments`, track `at`'s, is in: a new one after each
    /// discontinuity; a DASH representation's piece, the period it is in.
    pub fn parts_of(&self, at: usize, segments: &[HlsSegment]) -> Vec<usize> {
        let mut piece = 0;
        let mut parts = Vec::with_capacity(segments.len());
        for (n, segment) in segments.iter().enumerate() {
            piece += usize::from(n > 0 && segment.discontinuity);
            parts.push(match &self.sources[at] {
                // shortcut: a period a representation lists no segment in is not counted; count the
                // segments per period in `dash::segments` if such manifests turn up.
                Source::Dash(rendition) => rendition.pieces.get(piece).map_or(piece, |p| p.period),
                _ => piece,
            });
        }
        parts
    }

    /// The file extension of track `at`, whose segments are `segments`: a DASH representation's
    /// by its type; an HLS track's `mp4` (`m4a` for audio) when they have an init section, else
    /// that of packed audio (`.aac`, ...) its segments name, else `ts`.
    pub fn extension(&self, at: usize, segments: &[HlsSegment]) -> &'static str {
        match &self.sources[at] {
            Source::Dash(rendition) => rendition.ext,
            _ => segments_extension(self.info.tracks[at].kind, segments),
        }
    }

    /// Where a live recording of the stream at `link` reads track `at`'s segments: an HLS
    /// track's own playlist, a DASH track's manifest; None for audio inside its video's segments.
    pub fn live_origin(&self, link: &Url, at: usize) -> Option<Origin> {
        match &self.sources[at] {
            Source::InVideo => None,
            Source::Playlist(url) => Some(Origin { manifest: url.clone(), track: None }),
            Source::Read(_) | Source::Dash(_) => Some(Origin { manifest: link.clone(), track: Some(self.info.tracks[at].id.clone()) }),
        }
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

/// The file extension of an HLS track of `kind` whose segments are `segments`: `mp4` (`m4a` for
/// audio) when they have an init section, else that of packed audio (`.aac`, ...) they name, else
/// `ts`.
pub(crate) fn segments_extension(kind: Kind, segments: &[HlsSegment]) -> &'static str {
    if segments.iter().any(|s| s.init.is_some()) {
        return if kind == Kind::Audio { "m4a" } else { "mp4" };
    }
    let named = segments.first().and_then(|s| s.url.path().rsplit_once('.')).map(|(_, ext)| ext.to_ascii_lowercase());
    ["aac", "mp3", "ac3", "ec3"].into_iter().find(|ext| named.as_deref() == Some(*ext)).unwrap_or("ts")
}

/// The segments a live track's manifest lists now (see [`LiveSources::listings`]).
#[derive(Debug)]
pub(crate) struct Listing {
    /// Their `sequence` grows with time: an HLS media sequence number, a DASH segment's start in
    /// milliseconds.
    pub segments: Vec<HlsSegment>,
    /// When each starts, in seconds since 1970: by the manifest's clock (HLS
    /// `#EXT-X-PROGRAM-DATE-TIME`, a DASH timeline), else counted back from `now` at the newest.
    pub dates: Vec<f64>,
    /// Nothing comes after these: the stream has ended.
    pub ended: bool,
    /// Seconds until the manifest is worth reading again.
    pub refresh: f64,
    /// The extension of a file of them.
    pub ext: &'static str,
    /// Now, in seconds since 1970, by the clock of `dates`.
    pub now: f64,
}

/// The manifests of a live recording's tracks, read again each round (see
/// `HlsEngine::record`).
pub(crate) struct LiveSources {
    /// Each track's origin and kind.
    tracks: Vec<(Origin, Kind)>,
    /// The AES keys of the HLS playlists, fetched once.
    keys: std::collections::HashMap<Url, [u8; 16]>,
    /// Seconds the DASH server's clock is ahead of ours (see `dash::clock_offset`), once known.
    offset: Option<f64>,
    /// Segments whose links one of these matches are left out (see `DownloadOptions::ad_patterns`).
    pub ads: Vec<String>,
}

/// A manifest, read for one round of [`LiveSources::listings`].
enum Read {
    Playlist(String, Url),
    Mpd(crate::dash::Mpd),
}

impl LiveSources {
    pub fn new(tracks: Vec<(Origin, Kind)>) -> Self {
        Self { tracks, keys: Default::default(), offset: None, ads: Vec::new() }
    }

    /// What each track's manifest lists at `now` (seconds since 1970 by this computer's clock),
    /// after the segment numbered `after[track]` when given; each manifest read once. A live HLS
    /// playlist may not be a master playlist; a DASH track is found by its id.
    pub async fn listings(&mut self, client: &Client, auth: Option<&Auth>, fetch: FetchPolicy, now: f64, after: &[Option<u64>]) -> Result<Vec<Listing>, HlsError> {
        let mut read: Vec<(Url, Read)> = Vec::new();
        let mut listings = Vec::new();
        for (t, (origin, kind)) in self.tracks.iter().enumerate() {
            let after = after.get(t).copied().flatten();
            let at = match read.iter().position(|(url, _)| *url == origin.manifest) {
                Some(at) => at,
                None => {
                    let (body, base) = crate::hls::fetch_manifest(client, auth, &origin.manifest, fetch, false).await?;
                    let body = String::from_utf8_lossy(&body);
                    let body = body.trim_start_matches('\u{feff}').trim_start();
                    let manifest = if body.starts_with("#EXTM3U") {
                        if crate::hls::is_master(body) {
                            return Err(HlsError::Unsupported(format!("{} is a master playlist inside a master playlist", origin.manifest)));
                        }
                        Read::Playlist(body.to_string(), base)
                    } else if crate::dash::is_mpd(body.as_bytes()) {
                        let mpd = crate::dash::parse(body, &base)?;
                        if self.offset.is_none() {
                            self.offset = Some(crate::dash::clock_offset(client, auth, fetch, &mpd, &base, now).await);
                        }
                        Read::Mpd(mpd)
                    } else {
                        return Err(HlsError::Unavailable(format!("{} is no longer a playlist or manifest", origin.manifest)));
                    };
                    read.push((origin.manifest.clone(), manifest));
                    read.len() - 1
                }
            };
            let mut listing = match &read[at].1 {
                Read::Playlist(text, base) => crate::hls::live_playlist(client, auth, text, base, fetch, &mut self.keys, *kind, now, after).await?,
                Read::Mpd(mpd) => {
                    let track = mpd.tracks.iter().find(|(track, _)| origin.track.as_ref() == Some(&track.id));
                    let (_, rendition) = track.ok_or_else(|| HlsError::Unavailable(format!("{} no longer lists the track", origin.manifest)))?;
                    crate::dash::live_listing(mpd, rendition, now + self.offset.unwrap_or(0.0), after)?
                }
            };
            if !self.ads.is_empty() {
                // An ad left out is listed again next round, after the same segment: what follows it
                // then follows a gap.
                let (mut kept, mut dates, mut gap) = (Vec::new(), Vec::new(), false);
                for (segment, date) in listing.segments.into_iter().zip(listing.dates) {
                    if self.ads.iter().any(|pattern| pattern_matches(pattern, segment.url.as_str())) {
                        gap = true;
                        continue;
                    }
                    let discontinuity = std::mem::take(&mut gap) || segment.discontinuity;
                    kept.push(HlsSegment { discontinuity, ..segment });
                    dates.push(date);
                }
                (listing.segments, listing.dates) = (kept, dates);
            }
            listings.push(listing);
        }
        Ok(listings)
    }
}

/// Where a live recording starts (see `DownloadOptions::live_from_start` and `live_from`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LiveStart {
    /// At the newest segments, as players start (the default).
    Now,
    /// At the oldest segment the server still keeps.
    Oldest,
    /// This many seconds before now.
    Ago(f64),
    /// At this time, in seconds since 1970.
    At(f64),
}

impl LiveStart {
    /// A catch-up start as `DownloadOptions::live_from` gives it: a time ago (`30m`, `1h30m`,
    /// `90s`, `45` seconds, `M:SS`, `H:MM:SS`), or a clock time (`2026-10-10T21:00`, `2026-10-10
    /// 21:00:00`, with `Z` or `+08:00` after it for another time zone than this computer's).
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let bad = || format!("Unknown start \"{text}\": a time ago (30m, 1h30m, 90s, 01:00:00) or a clock time (2026-10-10T21:00, 2026-10-10 21:00:00+08:00)");
        if text.get(4..5) == Some("-") {
            return clock_time(text).map(Self::At).ok_or_else(bad);
        }
        let seconds = if text.contains(':') {
            let parts: Vec<Option<f64>> = text.split(':').map(|p| p.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0)).collect();
            match parts[..] {
                [Some(m), Some(s)] if s < 60.0 => Some(m * 60.0 + s),
                [Some(h), Some(m), Some(s)] if m < 60.0 && s < 60.0 => Some(h * 3600.0 + m * 60.0 + s),
                _ => None,
            }
        } else if let Ok(secs) = text.parse::<f64>() {
            Some(secs).filter(|s| s.is_finite() && *s >= 0.0)
        } else {
            // Numbers with units, largest first: `1h30m`, `90s`.
            let (mut total, mut number, mut units) = (0.0, String::new(), "hms");
            for c in text.chars() {
                if c.is_ascii_digit() || c == '.' {
                    number.push(c);
                    continue;
                }
                let at = units.find(c.to_ascii_lowercase()).ok_or_else(bad)?;
                total += number.parse::<f64>().map_err(|_| bad())? * [3600.0, 60.0, 1.0][at + 3 - units.len()];
                (number, units) = (String::new(), &units[at + 1..]);
            }
            Some(total).filter(|_| number.is_empty() && !text.is_empty())
        };
        seconds.map(Self::Ago).ok_or_else(bad)
    }
}

/// Seconds since 1970 of a clock time `YYYY-MM-DD[T ]HH:MM[:SS[.fff]]`, in this computer's time
/// zone unless `Z` or an offset (`+08:00`, `-0500`) follows it.
fn clock_time(text: &str) -> Option<f64> {
    let (day, rest) = (text.get(..10)?, text.get(10..)?);
    let time = rest.strip_prefix(['T', 't', ' '])?;
    let (clock, zone) = time.split_at(time.find(['Z', 'z', '+', '-']).unwrap_or(time.len()));
    let number = |s: &str| s.parse::<i64>().ok().filter(|_| s.bytes().all(|b| b.is_ascii_digit()) && !s.is_empty());
    let mut date = day.split('-').map(number);
    let (year, month, mday) = (date.next()??, date.next()??, date.next()??);
    let mut hms = clock.splitn(3, ':');
    let (hour, minute) = (number(hms.next()?)?, number(hms.next()?)?);
    let second = hms.next().map_or(Some(0.0), |s| s.parse::<f64>().ok().filter(|s| s.is_finite() && *s >= 0.0 && *s < 61.0))?;
    let valid = (1..=12).contains(&month) && (1..=31).contains(&mday) && hour < 24 && minute < 60;
    let local = (crate::engine::days_from_civil(year, month, mday) * 86_400 + hour * 3600 + minute * 60) as f64 + second;
    let offset = match zone {
        "" => local_offset(local as i64) as f64,
        "Z" | "z" => 0.0,
        _ => {
            let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
            let (h, m) = (number(digits.get(..2)?)?, number(digits.get(2..).filter(|m| !m.is_empty()).unwrap_or("0"))?);
            let secs = (h * 3600 + m * 60) as f64;
            if zone.starts_with('-') { -secs } else { secs }
        }
    };
    valid.then_some(local - offset)
}

/// Seconds since 1970, now.
pub(crate) fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Seconds this computer's time zone is ahead of UTC at `epoch` (seconds since 1970); 0 where that
/// cannot be told.
fn local_offset(epoch: i64) -> i64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::FILETIME;
        // FILETIMEs count 100 ns from 1601.
        let ticks = (epoch.saturating_add(11_644_473_600)).max(0) as u64 * 10_000_000;
        let utc = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
        let mut local = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        // SAFETY: both point at valid FILETIMEs for the length of the call.
        if unsafe { windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime(&utc, &mut local) } == 0 {
            return 0;
        }
        let local = (u64::from(local.dwHighDateTime) << 32 | u64::from(local.dwLowDateTime)) as i64;
        (local - ticks as i64) / 10_000_000
    }
    #[cfg(unix)]
    {
        let time = epoch as libc::time_t;
        // SAFETY: tm is plain data, which all zeros is a valid value of; localtime_r fills it from
        // a valid time.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
            return 0;
        }
        // Its type differs between platforms.
        #[allow(clippy::unnecessary_cast)]
        {
            tm.tm_gmtoff as i64
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = epoch;
        0
    }
}

/// `epoch` (seconds since 1970) as this computer's clock shows it: `2026-10-10 21:00:05`.
pub(crate) fn clock_text(epoch: f64) -> String {
    let local = epoch.floor() as i64 + local_offset(epoch.floor() as i64);
    let (year, month, day) = crate::engine::civil_from_days(local.div_euclid(86_400));
    let time = local.rem_euclid(86_400);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", time / 3600, time / 60 % 60, time % 60)
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

    /// A live start reads as a time ago (units, `M:SS`, `H:MM:SS`, seconds) or as a clock time,
    /// in this computer's time zone unless it gives one; anything else is refused, saying what
    /// is taken.
    #[test]
    fn test_a_live_start_reads_as_a_time_ago_or_a_clock_time() {
        for (text, secs) in [("30m", 1800.0), ("1h30m", 5400.0), (" 90s ", 90.0), ("1.5h", 5400.0), ("01:00:00", 3600.0), ("2:30", 150.0), ("45", 45.0)] {
            assert_eq!(LiveStart::parse(text), Ok(LiveStart::Ago(secs)), "{text}");
        }
        let day = crate::engine::days_from_civil(2026, 10, 10) as f64 * 86_400.0;
        assert_eq!(LiveStart::parse("2026-10-10T21:00Z"), Ok(LiveStart::At(day + 21.0 * 3600.0)));
        assert_eq!(LiveStart::parse("2026-10-10 21:00:30.5+08:00"), Ok(LiveStart::At(day + 13.0 * 3600.0 + 30.5)));
        assert_eq!(LiveStart::parse("2026-10-10T21:00-0130"), Ok(LiveStart::At(day + 22.5 * 3600.0)));
        let Ok(LiveStart::At(local)) = LiveStart::parse("2026-10-10 21:00") else { panic!("a clock time") };
        assert_eq!(clock_text(local), "2026-10-10 21:00:00");
        for bad in ["", "soon", "30x", "1m30h", "-5m", "1:75", "2026-13-10T21:00", "2026-10-10T25:00", "2026-10-10", "2026-10-10T21"] {
            let err = LiveStart::parse(bad).unwrap_err();
            assert!(err.contains("a time ago (30m"), "{bad}: {err}");
        }
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

    /// A pattern is a substring of any case, `*` any run of characters; a blank one matches
    /// nothing and is refused.
    #[test]
    fn test_patterns_match_substrings_with_stars() {
        let link = "https://CDN.example/ads/Ad_01.ts?x=1";
        for hit in ["ad_01", "/ADS/", "ad*.ts", "*", "cdn*ads*01", "x=1"] {
            assert!(pattern_matches(hit, link), "{hit}");
        }
        for miss in ["ad_02", "ts*ads", "", "  "] {
            assert!(!pattern_matches(miss, link), "{miss}");
        }
        assert!(check_pattern(" ").is_err() && check_pattern("*news*").is_ok());
    }

    /// Part ids are numbers and ranges, each once, in order; anything else is refused.
    #[test]
    fn test_part_ids_read_as_numbers_and_ranges() {
        assert_eq!(parse_part_ids(" 4, 0,2-4 ,").unwrap(), [0, 2, 3, 4]);
        assert_eq!(parse_part_ids("").unwrap(), Vec::<usize>::new());
        for bad in ["a", "3-1", "1-", "-2", "0-999999999"] {
            assert!(parse_part_ids(bad).unwrap_err().contains(&format!("\"{bad}\"")), "{bad}");
        }
    }

    fn segment(n: usize, secs: f64, discontinuity: bool) -> HlsSegment {
        HlsSegment {
            index: n,
            sequence: n as u64,
            url: Url::parse(&format!("https://cdn.example/{}.ts", if n == 2 { "ad".to_string() } else { n.to_string() })).unwrap(),
            duration_secs: secs,
            byte_range: None,
            encryption: None,
            init: None,
            discontinuity,
        }
    }

    /// Cuts keep the whole segments that overlap a section, and leave out the parts dropped and
    /// the segments whose links an ad pattern matches; what is left is numbered again, and what
    /// follows a gap starts a new run.
    #[test]
    fn test_cuts_keep_whole_segments_and_mark_the_gaps() {
        // 0-10, 10-20, ad 20-25 (part 1), 25-35 (part 2), 35-45.
        let segments = vec![segment(0, 10.0, false), segment(1, 10.0, false), segment(2, 5.0, true), segment(3, 10.0, true), segment(4, 10.0, false)];
        let parts = [0, 0, 1, 2, 2];
        let kept = |cuts: Cuts| cuts.apply(segments.clone(), &parts).iter().map(|s| (s.sequence, s.index, s.discontinuity)).collect::<Vec<_>>();
        assert_eq!(kept(Cuts::default()).len(), 5);
        assert_eq!(kept(Cuts { sections: &[(12.0, 26.0)], ..Default::default() }), [(1, 0, false), (2, 1, true), (3, 2, true)]);
        assert_eq!(kept(Cuts { parts: &[1], ..Default::default() }), [(0, 0, false), (1, 1, false), (3, 2, true), (4, 3, false)]);
        let ads = ["/AD.".to_string()];
        assert_eq!(kept(Cuts { ads: &ads, sections: &[(0.0, 5.0), (40.0, 41.0)], ..Default::default() }), [(0, 0, false), (4, 1, true)]);
        assert!(kept(Cuts { sections: &[(50.0, 60.0)], ..Default::default() }).is_empty());
    }

    /// Subtitles go by the subtitle languages: all of them when none are given, none for `none`;
    /// the same language and name only once.
    #[test]
    fn test_subtitles_are_chosen_by_language() {
        let text = |id: &str, lang: &str, name: &str| Track { language: Some(lang.into()), name: Some(name.into()), ..track(id, Kind::Subtitles, "") };
        let info = dash(vec![video("v0", 720, "avc1", 1), text("s0", "en-US", "English"), text("s1", "fr", "French"), text("s2", "en-US", "English"), text("s3", "en", "SDH")]);
        let ids = |langs: Option<&str>| choose_subtitles(&info, langs).into_iter().map(|at| info.tracks[at].id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids(None), ["s0", "s1", "s3"]);
        assert_eq!(ids(Some("all")), ["s0", "s1", "s3"]);
        assert_eq!(ids(Some("en")), ["s0", "s3"]);
        assert_eq!(ids(Some("fr,de")), ["s1"]);
        assert!(ids(Some("none")).is_empty() && ids(Some("")).is_empty());
    }

    /// A name pattern is filled from the tracks chosen, an empty value taking its separators and
    /// brackets with it; an unknown variable or a blank pattern is refused.
    #[test]
    fn test_name_patterns_fill_in_the_tracks() {
        let chosen = [
            &Track { frame_rate: Some(29.97), range: DynamicRange::Hdr10, ..video("v1", 1080, "hvc1.2.4.L123", 6_000_000) },
            &audio("a0", "en", "mp4a.40.2", 2, 1),
            &audio("a1", "ja", "ec-3", 6, 1),
        ];
        let value = |v: &str| match v {
            "name" => Some("Show".to_string()),
            "channel" => None,
            _ => track_value(v, &chosen),
        };
        let fill = |pattern: &str| fill_name(pattern, value);
        assert_eq!(fill("{name} - {height}p {codec} {range} [{audio}] {fps}fps {bitrate}k"), "Show - 1080p hevc hdr [en+ja] 29.97fps 6000k");
        assert_eq!(fill("{name} - {channel} - {width}x{height}"), "Show - 1920x1080");
        assert_eq!(fill("{channel}_{name}.({channel}).{unknown}"), "Show.{unknown}");
        assert_eq!(fill_name("{name} {height}p", |v| (v == "name").then(|| "Show".to_string())), "Show p");
        assert!(check_name_pattern("{name} {height}p {date} {time}").is_ok());
        assert!(check_name_pattern("{name} {size}").unwrap_err().contains("{size}"));
        assert!(check_name_pattern("{name").is_err() && check_name_pattern(" ").is_err());
    }
}
