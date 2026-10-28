//! DASH manifests (MPD): their periods' representations as tracks (see [`parse`]), and the
//! segments of a track on demand, whichever way the manifest addresses them (see [`Addressing`]).
//! A live (dynamic) manifest is read for its tracks, but its segments are not worked out here.

use std::borrow::Cow;
use std::sync::Arc;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use reqwest::Client;
use url::Url;
use crate::hls::{FetchPolicy, HlsError, HlsSegment, InitSection, MAX_SEGMENTS};
use crate::range::ByteRange;
use crate::stream::{mark_ads, summary_of, Kind, Part, Role, Track};
use crate::worker::Auth;

/// Most elements read of a manifest, and how deep they may nest.
const MAX_NODES: usize = if cfg!(test) { 20_000 } else { 2_000_000 };
const MAX_DEPTH: usize = 32;
/// Most representations read of a period, and periods of a manifest: each track is followed
/// through every period.
const MAX_TRACKS: usize = 200;
const MAX_PERIODS: usize = 1000;
/// Largest segment index (`sidx` box) read, and most read of a file (one may list others).
const MAX_INDEX_BYTES: u64 = 1024 * 1024;
const MAX_INDEXES: usize = 64;
/// A representation that is one file with no index is fetched in pieces this large, after a
/// first piece of [`HEAD_BYTES`] (see [`Addressing::Whole`]).
const PIECE_BYTES: u64 = if cfg!(test) { 4 * 1024 } else { 8 * 1024 * 1024 };
const HEAD_BYTES: u64 = if cfg!(test) { 1024 } else { 64 * 1024 };

/// A fault inside a real manifest: never [`HlsError::InvalidPlaylist`], or the manifest's text
/// would be saved as the download.
fn malformed(reason: impl std::fmt::Display) -> HlsError {
    HlsError::Unsupported(format!("malformed DASH manifest: {reason}"))
}

fn too_many() -> HlsError {
    HlsError::Unsupported(format!("stream has more than {MAX_SEGMENTS} segments"))
}

/// An element of a manifest; its name and its attributes' names without namespace prefixes.
#[derive(Debug, Default)]
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
    text: String,
}

impl Node {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.trim())
    }

    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    fn first(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
}

fn element(e: &BytesStart) -> Node {
    let lossy = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let attrs = e
        .attributes()
        .flatten()
        .map(|a| (lossy(a.key.local_name().as_ref()), a.unescape_value().map_or_else(|_| lossy(&a.value), Cow::into_owned)))
        .collect();
    Node { name: lossy(e.local_name().as_ref()), attrs, ..Default::default() }
}

/// The `MPD` element of `text`, with what it holds: at most [`MAX_NODES`] elements, nested at
/// most [`MAX_DEPTH`] deep.
fn parse_xml(text: &str) -> Result<Node, String> {
    let mut reader = Reader::from_str(text);
    // The document itself holds the root element.
    let mut open = vec![Node::default()];
    let mut count = 0;
    loop {
        let event = reader.read_event().map_err(|e| format!("not valid XML: {e}"))?;
        if matches!(event, Event::Start(_) | Event::Empty(_)) {
            count += 1;
            if count > MAX_NODES {
                return Err(format!("more than {MAX_NODES} elements"));
            }
        }
        match event {
            Event::Start(e) if open.len() > MAX_DEPTH => return Err(format!("elements nested more than {MAX_DEPTH} deep: {:?}", e.name())),
            Event::Start(e) => open.push(element(&e)),
            Event::Empty(e) => open.last_mut().ok_or("unbalanced elements")?.children.push(element(&e)),
            Event::End(_) => {
                let done = open.pop().filter(|_| !open.is_empty()).ok_or("unbalanced elements")?;
                open.last_mut().ok_or("unbalanced elements")?.children.push(done);
            }
            Event::Text(t) => {
                let text = t.unescape().map_or_else(|_| String::from_utf8_lossy(&t).into_owned(), Cow::into_owned);
                open.last_mut().ok_or("unbalanced elements")?.text.push_str(&text);
            }
            Event::CData(c) => open.last_mut().ok_or("unbalanced elements")?.text.push_str(&String::from_utf8_lossy(&c)),
            Event::Eof => break,
            _ => {}
        }
    }
    match open.pop() {
        Some(document) if open.is_empty() => document.children.into_iter().find(|n| n.name == "MPD").ok_or_else(|| "no MPD element".to_string()),
        _ => Err("the manifest breaks off".to_string()),
    }
}

/// Whether `body` starts like a DASH manifest: XML whose first element is an `MPD`.
pub(crate) fn is_mpd(body: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&body[..body.len().min(4096)]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with('<')
        && head.match_indices("MPD").any(|(at, _)| {
            let before = head[..at].chars().next_back();
            let after = head[at + 3..].chars().next();
            matches!(before, Some('<' | ':')) && after.is_none_or(|c| c.is_whitespace() || c == '>' || c == '/')
        })
}

/// Seconds of an ISO 8601 duration as manifests give them (`PT1H2M3.5S`, `P1DT2H`).
fn seconds(duration: &str) -> Option<f64> {
    let rest = duration.trim().strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total = 0.0;
    let mut add = |part: &str, units: &[(char, f64)]| -> Option<()> {
        let mut number = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                number.push(c);
                continue;
            }
            let (_, scale) = units.iter().find(|(unit, _)| *unit == c)?;
            total += number.parse::<f64>().ok()? * scale;
            number.clear();
        }
        number.is_empty().then_some(())
    };
    const DAY: f64 = 86_400.0;
    add(date, &[('Y', 365.0 * DAY), ('M', 30.0 * DAY), ('W', 7.0 * DAY), ('D', DAY)])?;
    add(time, &[('H', 3600.0), ('M', 60.0), ('S', 1.0)])?;
    total.is_finite().then_some(total)
}

/// A byte range as manifests give one: `first-last`, both included.
fn byte_range(spec: &str) -> Option<ByteRange> {
    let (first, last) = spec.trim().split_once('-')?;
    ByteRange::new(first.trim().parse().ok()?, last.trim().parse().ok()?).ok()
}

/// `template` with its identifiers filled in: `$RepresentationID$`, `$Number$`, `$Time$` and
/// `$Bandwidth$` (numbers zero-padded to the width a `%0Nd` after the name asks for), and `$$`.
/// An unknown identifier is left as it is.
fn fill(template: &str, rep_id: &str, number: u64, time: u64, bandwidth: u64) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let (name, format) = after[..end].split_once('%').unwrap_or((&after[..end], ""));
        let width = format.strip_prefix('0').and_then(|f| f.strip_suffix('d')).and_then(|w| w.parse::<usize>().ok()).unwrap_or(0).min(32);
        let value = match name {
            "Number" => Some(number),
            "Time" => Some(time),
            "Bandwidth" => Some(bandwidth),
            _ => None,
        };
        match (name, value) {
            (_, Some(value)) => out.push_str(&format!("{value:0width$}")),
            ("", None) => out.push('$'),
            ("RepresentationID", None) => out.push_str(rep_id),
            _ => out.push_str(&rest[start..start + end + 2]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Seconds a period of a manifest lasts, and how one of its tracks' segments are found.
#[derive(Debug, Clone)]
pub(crate) struct Piece {
    /// Seconds the period lasts, when the manifest tells.
    pub length: Option<f64>,
    /// `ContentProtection` on its adaptation set or representation: DRM.
    pub protected: bool,
    /// Shared by every track that follows its representation.
    pub addressing: Arc<Addressing>,
    /// Seconds after the presentation's start the period starts, when the manifest tells.
    pub start: Option<f64>,
    /// Which period it is, from 0.
    pub period: usize,
}

/// A track's representation in each period it is in, in order, and its file extension.
#[derive(Debug, Clone)]
pub(crate) struct Rendition {
    pub ext: &'static str,
    pub pieces: Vec<Piece>,
    /// The representation the track is (see [`Mpd::follow`]).
    pub rep: Rep,
}

/// How a representation's segments are found: by its `SegmentTemplate`, `SegmentList` or
/// `SegmentBase`, else as the one file its `BaseURL` names.
#[derive(Debug, Clone)]
pub(crate) enum Addressing {
    Template(Template),
    /// Segments listed one by one (`SegmentURL`s, their links and byte ranges), each `duration`
    /// seconds long, after the init section (link and byte range) if there is one.
    List { init: Option<(Url, Option<ByteRange>)>, segments: Vec<(Url, Option<ByteRange>)>, duration: f64 },
    /// One file whose segment index (a `sidx` box at `index`) lists its segments; its init section
    /// is `init`, else all that comes before the index.
    Indexed { url: Url, index: ByteRange, init: Option<ByteRange> },
    /// One file with no index: its init section (`init`, else its first [`HEAD_BYTES`]), then the
    /// rest in pieces of [`PIECE_BYTES`].
    Whole { url: Url, init: Option<ByteRange> },
}

/// A `SegmentTemplate`, its attributes taken from the representation, else its adaptation set,
/// else its period.
#[derive(Debug, Clone)]
pub(crate) struct Template {
    pub base: Url,
    pub rep_id: String,
    pub bandwidth: u64,
    pub media: String,
    pub initialization: Option<String>,
    pub start_number: u64,
    pub end_number: Option<u64>,
    pub timescale: u64,
    /// Of every segment, in `timescale` units, when there is no timeline.
    pub duration: Option<u64>,
    /// `presentationTimeOffset`.
    pub offset: u64,
    /// The `S` elements of its `SegmentTimeline`: start (`t`), duration (`d`) and repeats (`r`;
    /// -1 repeats up to the next start, or the period's end).
    pub timeline: Vec<(Option<u64>, u64, i64)>,
}

impl Template {
    /// `levels` are the representation's, adaptation set's and period's `SegmentTemplate`s there
    /// are, in that order.
    fn of(levels: &[&Node], base: Url, rep_id: &str, bandwidth: u64) -> Self {
        let attr = |name: &str| levels.iter().find_map(|n| n.attr(name));
        let number = |name: &str| attr(name).and_then(|v| v.parse::<u64>().ok());
        let int = |node: &Node, name: &str| node.attr(name).and_then(|v| v.parse::<u64>().ok());
        let timeline = levels.iter().find_map(|n| n.first("SegmentTimeline")).map_or_else(Vec::new, |timeline| {
            timeline.all("S").map(|s| (int(s, "t"), int(s, "d").unwrap_or(0), s.attr("r").and_then(|r| r.parse().ok()).unwrap_or(0))).collect()
        });
        let initialization = attr("initialization").or_else(|| levels.iter().find_map(|n| n.first("Initialization")?.attr("sourceURL")));
        Self {
            base,
            rep_id: rep_id.to_string(),
            bandwidth,
            media: attr("media").unwrap_or_default().to_string(),
            initialization: initialization.map(str::to_string),
            start_number: number("startNumber").unwrap_or(1),
            end_number: number("endNumber"),
            timescale: number("timescale").filter(|&t| t > 0).unwrap_or(1),
            duration: number("duration").filter(|&d| d > 0),
            offset: number("presentationTimeOffset").unwrap_or(0),
            timeline,
        }
    }

    fn link(&self, template: &str, number: u64, time: u64) -> Result<Url, HlsError> {
        let link = fill(template, &self.rep_id, number, time, self.bandwidth);
        self.base.join(&link).map_err(|e| malformed(format!("invalid segment link '{link}': {e}")))
    }

    /// The init section's link, if it has one.
    pub fn init(&self) -> Result<Option<Url>, HlsError> {
        self.initialization.as_deref().map(|init| self.link(init, self.start_number, self.offset)).transpose()
    }

    /// The segments of an on-demand period `length` seconds long (None: not told), with their
    /// lengths in seconds.
    pub fn segments(&self, length: Option<f64>) -> Result<Vec<(Url, f64)>, HlsError> {
        let scale = self.timescale as f64;
        let end = length.map(|l| self.offset.saturating_add((l * scale).round() as u64));
        // Each segment's start and length, in `timescale` units.
        let mut times: Vec<(u64, u64)> = Vec::new();
        if !self.timeline.is_empty() {
            let mut time = 0u64;
            for (k, &(t, d, r)) in self.timeline.iter().enumerate() {
                time = t.unwrap_or(time);
                if d == 0 {
                    return Err(malformed("a SegmentTimeline entry without a duration"));
                }
                let count = match u64::try_from(r) {
                    Ok(r) => r.saturating_add(1),
                    Err(_) => {
                        let until = self.timeline.get(k + 1).and_then(|next| next.0).or(end);
                        until.ok_or_else(|| malformed("a SegmentTimeline repeats to an end the manifest does not give"))?.saturating_sub(time).div_ceil(d)
                    }
                };
                for _ in 0..count {
                    if times.len() == MAX_SEGMENTS {
                        return Err(too_many());
                    }
                    times.push((time, d));
                    time = time.saturating_add(d);
                }
            }
        } else if let Some(d) = self.duration {
            let count = match (self.end_number, length) {
                (Some(last), _) => last.saturating_add(1).saturating_sub(self.start_number),
                // A length of a whole number of segments may come out a hair over it.
                (None, Some(length)) => (length * scale / d as f64 - 1e-6).ceil().max(0.0) as u64,
                (None, None) => return Err(malformed("a SegmentTemplate without an end: the period's length is not given")),
            };
            if count > MAX_SEGMENTS as u64 {
                return Err(too_many());
            }
            times = (0..count).map(|n| (self.offset.saturating_add(n.saturating_mul(d)), d)).collect();
        } else {
            // A template naming a single segment: the whole period.
            times.push((self.offset, length.map_or(0, |l| (l * scale) as u64)));
        }
        let first = self.start_number;
        times.iter().enumerate().map(|(n, &(time, d))| Ok((self.link(&self.media, first.saturating_add(n as u64), time)?, d as f64 / scale))).collect()
    }
}

impl Piece {
    /// Its init section, and its segments: link, byte range and seconds. A segment index or the
    /// file's size is fetched where the manifest gives no list.
    async fn segments(
        &self,
        client: &Client,
        auth: Option<&Auth>,
        fetch: FetchPolicy,
    ) -> Result<(Option<Arc<InitSection>>, Vec<(Url, Option<ByteRange>, f64)>), HlsError> {
        let init = |url: Url, byte_range: Option<ByteRange>| Some(Arc::new(InitSection { url, byte_range, encryption: None }));
        Ok(match &*self.addressing {
            Addressing::Template(template) => {
                let listed = template.segments(self.length)?.into_iter().map(|(url, secs)| (url, None, secs)).collect();
                (template.init()?.and_then(|url| init(url, None)), listed)
            }
            Addressing::List { init: first, segments, duration } => {
                let listed = segments.iter().map(|(url, range)| (url.clone(), *range, *duration)).collect();
                (first.clone().and_then(|(url, range)| init(url, range)), listed)
            }
            Addressing::Indexed { url, index, init: given } => {
                // An index may list further indexes, each read in its place: at most MAX_INDEXES.
                // shortcut: a further index is read with up to 1 MiB of the segments after it; read
                // its box's size first if such files turn up.
                let mut listed = vec![(*index, 0.0, true)];
                for _ in 0..MAX_INDEXES {
                    let Some(at) = listed.iter().position(|s| s.2) else { break };
                    let range = listed[at].0;
                    let first = ByteRange::new(range.start, range.end.min(range.start.saturating_add(MAX_INDEX_BYTES - 1))).map_err(malformed)?;
                    let data = crate::hls::fetch_range(client, auth, url, first, MAX_INDEX_BYTES, fetch).await?;
                    let found = sidx_segments(&data, range.start)
                        .ok_or_else(|| malformed(format!("no segment index (sidx) at bytes {}-{} of {url}", range.start, range.end)))?;
                    listed.splice(at..=at, found);
                    if listed.len() > MAX_SEGMENTS {
                        return Err(too_many());
                    }
                }
                if listed.iter().any(|s| s.2) {
                    return Err(malformed(format!("more than {MAX_INDEXES} segment indexes in {url}")));
                }
                let head = given.or_else(|| index.start.checked_sub(1).and_then(|last| ByteRange::new(0, last).ok()));
                (head.and_then(|range| init(url.clone(), Some(range))), listed.into_iter().map(|(range, secs, _)| (url.clone(), Some(range), secs)).collect())
            }
            Addressing::Whole { url, init: given } => {
                let size = crate::hls::size_of(client, auth, url, fetch).await?;
                let head = given.or_else(|| ByteRange::new(0, HEAD_BYTES.min(size).saturating_sub(1)).ok());
                let from = head.map_or(0, |h| h.end.saturating_add(1));
                let length = self.length.unwrap_or(0.0);
                if from >= size {
                    // Small enough to be one segment.
                    let whole = ByteRange::new(0, size.saturating_sub(1)).ok();
                    return Ok((None, vec![(url.clone(), whole, length)]));
                }
                // The size is the server's word: bounded before anything is made of it.
                if (size - from).div_ceil(PIECE_BYTES) > MAX_SEGMENTS as u64 {
                    return Err(too_many());
                }
                let starts: Vec<u64> = (from..size).step_by(PIECE_BYTES as usize).collect();
                let secs = length / starts.len() as f64;
                let listed = starts
                    .iter()
                    .filter_map(|&start| ByteRange::new(start, start.saturating_add(PIECE_BYTES - 1).min(size - 1)).ok())
                    .map(|range| (url.clone(), Some(range), secs))
                    .collect();
                (head.and_then(|range| init(url.clone(), Some(range))), listed)
            }
        })
    }
}

/// The segments a segment index (`sidx` box) lists: their byte ranges in the file, their lengths
/// in seconds, and whether each is a further index (followed by the segments it lists). `data` is
/// the file from byte `at` on, where the index starts.
fn sidx_segments(data: &[u8], at: u64) -> Option<Vec<(ByteRange, f64, bool)>> {
    let u32_at = |bytes: &[u8], i: usize| Some(u32::from_be_bytes(bytes.get(i..i + 4)?.try_into().ok()?));
    let u64_at = |bytes: &[u8], i: usize| Some(u64::from_be_bytes(bytes.get(i..i + 8)?.try_into().ok()?));
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let (header, size) = match u32_at(data, pos)? {
            1 => (16, usize::try_from(u64_at(data, pos + 8)?).ok()?),
            0 => (8, data.len() - pos),
            size => (8, size as usize),
        };
        if size < header {
            return None;
        }
        if &data[pos + 4..pos + 8] != b"sidx" {
            pos = pos.checked_add(size)?;
            continue;
        }
        let body = data.get(pos + header..pos.checked_add(size)?)?;
        let timescale = f64::from(u32_at(body, 8)?).max(1.0);
        // Version 0 gives 32-bit times and offsets, version 1 64-bit ones.
        let (first_offset, rest) = match *body.first()? {
            0 => (u64::from(u32_at(body, 16)?), body.get(20..)?),
            _ => (u64_at(body, 20)?, body.get(28..)?),
        };
        let count = usize::from(u16::from_be_bytes(rest.get(2..4)?.try_into().ok()?));
        let mut offset = at.checked_add((pos + size) as u64)?.checked_add(first_offset)?;
        let mut listed = Vec::with_capacity(count);
        for reference in rest.get(4..4 + 12 * count)?.chunks_exact(12) {
            let word = u32_at(reference, 0)?;
            let length = u64::from(word & 0x7fff_ffff);
            let secs = f64::from(u32_at(reference, 4)?) / timescale;
            if length > 0 {
                listed.push((ByteRange::from_len(offset, length).ok()?, secs, word >> 31 == 1));
            }
            offset = offset.checked_add(length)?;
        }
        return Some(listed);
    }
    None
}

/// The number of channels an `AudioChannelConfiguration` gives.
fn channel_count(scheme: &str, value: &str) -> Option<u32> {
    if scheme.contains("dolby") {
        // A mask of speaker positions, some of whose bits stand for a pair.
        const PAIRS: u32 = 0x0674;
        let mask = u32::from_str_radix(value.trim(), 16).ok()? & 0xffff;
        Some(mask.count_ones() + (mask & PAIRS).count_ones()).filter(|&c| c > 0)
    } else if scheme.contains("cicp") {
        // ISO/IEC 23091-3 channel configurations.
        const COUNTS: [u32; 21] = [0, 1, 2, 3, 4, 5, 6, 8, 2, 3, 4, 7, 8, 24, 8, 12, 10, 12, 14, 12, 14];
        COUNTS.get(value.trim().parse::<usize>().ok()?).copied().filter(|&c| c > 0)
    } else {
        value.trim().parse().ok().filter(|&c| c > 0)
    }
}

/// Frames a second, as `frameRate` gives them (`25`, `30000/1001`).
fn frame_rate(text: &str) -> Option<f64> {
    let (frames, per) = text.split_once('/').unwrap_or((text, "1"));
    let rate = frames.trim().parse::<f64>().ok()? / per.trim().parse::<f64>().ok()?;
    (rate.is_finite() && rate > 0.0).then_some(rate)
}

/// Whether a period's `id` says it is an ad: "ad", "ads", "advert", "preroll", "midroll" or
/// "postroll" as a word of it.
fn names_an_ad(id: &str) -> bool {
    const WORDS: [&str; 6] = ["ad", "ads", "advert", "preroll", "midroll", "postroll"];
    id.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| WORDS.iter().any(|w| word.eq_ignore_ascii_case(w)))
}

/// `base` with the first `BaseURL` of `node`, if any, resolved against it.
fn base_url(node: &Node, base: &Url) -> Url {
    let given = node.first("BaseURL").map(|b| b.text.trim()).filter(|t| !t.is_empty());
    given.and_then(|link| base.join(link).ok()).unwrap_or_else(|| base.clone())
}

/// A representation of a period, as a track (its id given later) and how its segments are found.
#[derive(Debug, Clone)]
pub(crate) struct Rep {
    track: Track,
    /// Its `id` in the manifest.
    key: String,
    /// The first codec family its codecs name.
    family: Option<&'static str>,
    ext: &'static str,
    protected: bool,
    addressing: Arc<Addressing>,
}

/// The first element called `name` in each of `levels` (a representation, its adaptation set and
/// its period) that has one, the most specific first.
fn levels<'a>(chain: [&'a Node; 3], name: &str) -> Vec<&'a Node> {
    chain.iter().filter_map(|node| node.first(name)).collect()
}

/// The representation `rep` of `set` in `period`, its links resolved against `base` (the set's);
/// None for one that is no video, audio or subtitles track (thumbnails, trick play).
fn representation(period: &Node, set: &Node, rep: &Node, base: &Url) -> Option<Rep> {
    let get = |name: &str| rep.attr(name).or_else(|| set.attr(name));
    let codecs = get("codecs").unwrap_or_default();
    let mime = get("mimeType").unwrap_or_default().to_ascii_lowercase();
    let properties = || set.all("EssentialProperty").chain(set.all("SupplementalProperty")).chain(rep.all("EssentialProperty")).chain(rep.all("SupplementalProperty"));
    if properties().any(|p| p.attr("schemeIdUri").is_some_and(|s| s.contains("trickmode"))) {
        return None;
    }
    let kind = match set.attr("contentType").unwrap_or_else(|| mime.split('/').next().unwrap_or_default()) {
        "video" => Kind::Video,
        "audio" => Kind::Audio,
        "text" => Kind::Subtitles,
        "image" => return None,
        _ if mime.contains("ttml") || mime.contains("vtt") || codecs.starts_with("stpp") || codecs.starts_with("wvtt") => Kind::Subtitles,
        _ => {
            let first = codecs.split(',').next().unwrap_or_default();
            crate::stream::family(first)?;
            if crate::stream::is_audio_codec(first) { Kind::Audio } else { Kind::Video }
        }
    };
    let roles: Vec<&str> = set.all("Role").chain(rep.all("Role")).filter_map(|r| r.attr("value")).collect();
    let role = roles.iter().find_map(|value| {
        Some(match *value {
            "main" => Role::Main,
            "alternate" => Role::Alternate,
            "commentary" => Role::Commentary,
            "dub" => Role::Dub,
            "description" => Role::Description,
            "caption" => Role::Caption,
            "subtitle" => Role::Subtitle,
            "forced-subtitle" | "forced_subtitle" => Role::Forced,
            _ => return None,
        })
    });
    let channels = rep.first("AudioChannelConfiguration").or_else(|| set.first("AudioChannelConfiguration"));
    let transfer = properties().filter(|p| p.attr("schemeIdUri").is_some_and(|s| s.ends_with("TransferCharacteristics"))).find_map(|p| p.attr("value"));
    let label = set.first("Label").or_else(|| rep.first("Label")).map(|l| l.text.trim().to_string()).filter(|l| !l.is_empty());
    let int = |name: &str| get(name).and_then(|v| v.parse::<u32>().ok());
    let track = Track {
        id: String::new(),
        kind,
        codecs: codecs.to_string(),
        width: int("width"),
        height: int("height"),
        frame_rate: get("frameRate").and_then(frame_rate),
        bandwidth: rep.attr("bandwidth").and_then(|b| b.parse().ok()),
        range: crate::stream::dynamic_range(codecs, match transfer {
            Some("16") => Some("PQ"),
            Some("18") => Some("HLG"),
            _ => None,
        }),
        channels: channels.and_then(|c| channel_count(c.attr("schemeIdUri").unwrap_or_default(), c.attr("value")?)),
        language: get("lang").filter(|l| !l.is_empty()).map(str::to_string),
        name: label,
        role: role.unwrap_or(if kind == Kind::Subtitles { Role::Subtitle } else { Role::Main }),
        default: roles.contains(&"main"),
        group: None,
    };
    let ext = match mime.as_str() {
        "video/webm" | "audio/webm" => "webm",
        "video/mp2t" => "ts",
        "text/vtt" => "vtt",
        "application/ttml+xml" => "ttml",
        _ if kind == Kind::Audio => "m4a",
        _ => "mp4",
    };
    let base = base_url(rep, base);
    let key = rep.attr("id").unwrap_or_default().to_string();
    let chain = [rep, set, period];
    let (templates, lists, bases) = (levels(chain, "SegmentTemplate"), levels(chain, "SegmentList"), levels(chain, "SegmentBase"));
    let addressing = if !templates.is_empty() {
        Addressing::Template(Template::of(&templates, base, &key, track.bandwidth.unwrap_or(0)))
    } else if !lists.is_empty() {
        let attr = |name: &str| lists.iter().find_map(|n| n.attr(name)).and_then(|v| v.parse::<u64>().ok());
        let duration = attr("duration").map_or(0.0, |d| d as f64 / attr("timescale").filter(|&t| t > 0).unwrap_or(1) as f64);
        let listed = lists.iter().find(|n| n.first("SegmentURL").is_some()).map_or(&[][..], |n| &n.children[..]);
        let segments = listed
            .iter()
            .filter(|n| n.name == "SegmentURL")
            .filter_map(|s| Some((s.attr("media").map_or(Some(base.clone()), |m| base.join(m).ok())?, s.attr("mediaRange").and_then(byte_range))))
            .collect();
        let init = lists.iter().find_map(|n| n.first("Initialization")).map(|i| {
            let url = i.attr("sourceURL").and_then(|s| base.join(s).ok()).unwrap_or_else(|| base.clone());
            (url, i.attr("range").and_then(byte_range))
        });
        Addressing::List { init, segments, duration }
    } else {
        let init = bases.iter().find_map(|n| n.first("Initialization")?.attr("range")).and_then(byte_range);
        match bases.iter().find_map(|n| n.attr("indexRange")).and_then(byte_range) {
            Some(index) => Addressing::Indexed { url: base, index, init },
            None => Addressing::Whole { url: base, init },
        }
    };
    let protected = set.first("ContentProtection").is_some() || rep.first("ContentProtection").is_some();
    let family = crate::stream::families(codecs).next();
    Some(Rep { track, key, family, ext, protected, addressing: Arc::new(addressing) })
}

/// The representation among `reps` (another period's) that goes on with `like`: of its kind,
/// best its id, else its language, else its codec family, else the closest bandwidth.
fn follow<'a>(reps: &'a [Rep], like: &Rep) -> Option<&'a Rep> {
    reps.iter().filter(|r| r.track.kind == like.track.kind).max_by_key(|r| {
        (
            !like.key.is_empty() && r.key == like.key,
            r.track.language == like.track.language,
            r.family == like.family,
            std::cmp::Reverse(r.track.bandwidth.unwrap_or(0).abs_diff(like.track.bandwidth.unwrap_or(0))),
        )
    })
}

/// A DASH manifest, read (see [`parse`]).
pub(crate) struct Mpd {
    /// `type="dynamic"`.
    pub live: bool,
    /// Seconds, when it tells.
    pub duration: Option<f64>,
    pub tracks: Vec<(crate::stream::Track, Rendition)>,
    pub parts: Vec<Part>,
    /// `availabilityStartTime`, in seconds since 1970: when a live manifest's time starts.
    pub available_from: Option<f64>,
    /// `minimumUpdatePeriod`, in seconds: how long a live manifest stays as it is, at least.
    pub update_every: Option<f64>,
    /// `timeShiftBufferDepth`, in seconds: how far back a live manifest's segments are kept.
    pub buffer: Option<f64>,
    /// Its first `UTCTiming` that [`clock_offset`] reads: scheme and value.
    pub clock: Option<(String, String)>,
    /// Each period's representations, when it starts and how long it lasts (see [`Mpd::follow`]).
    periods: Vec<(Vec<Rep>, Option<f64>, Option<f64>)>,
}

impl Mpd {
    /// The track that is the representation `like` (see [`Rendition::rep`]), followed into each
    /// period (see [`follow`]): a live manifest's track ids change as its periods come and go.
    /// None when no period has one of its kind.
    pub fn follow(&self, like: &Rep) -> Option<Rendition> {
        let pieces: Vec<Piece> = self
            .periods
            .iter()
            .enumerate()
            .filter_map(|(n, (reps, start, length))| {
                let same = follow(reps, like)?;
                Some(Piece { length: *length, protected: same.protected, addressing: same.addressing.clone(), start: *start, period: n })
            })
            .collect();
        (!pieces.is_empty()).then(|| Rendition { ext: like.ext, pieces, rep: like.clone() })
    }
}

/// Reads the manifest `text`, fetched from `url` (after redirects; its `BaseURL`s resolve against
/// it). Its tracks are the representations of its longest period, by kind (`v0`, `a0`, `s0`, in
/// manifest order), each followed into the other periods (see [`follow`]); its parts are its
/// periods.
pub(crate) fn parse(text: &str, url: &Url) -> Result<Mpd, HlsError> {
    let mpd = parse_xml(text).map_err(malformed)?;
    let total = mpd.attr("mediaPresentationDuration").and_then(seconds);
    let base = base_url(&mpd, url);
    let live = mpd.attr("type") == Some("dynamic");
    // Of a live manifest, the newest periods.
    let skip = if live { mpd.all("Period").count().saturating_sub(MAX_PERIODS) } else { 0 };
    let periods: Vec<&Node> = mpd.all("Period").skip(skip).take(MAX_PERIODS).collect();
    // When each period starts, and how long it lasts.
    let mut starts: Vec<Option<f64>> = Vec::new();
    for (n, period) in periods.iter().enumerate() {
        let after = match n.checked_sub(1) {
            None => Some(0.0),
            Some(before) => starts[before].zip(periods[before].attr("duration").and_then(seconds)).map(|(start, length)| start + length),
        };
        starts.push(period.attr("start").and_then(seconds).or(after));
    }
    let lengths: Vec<Option<f64>> = (0..periods.len())
        .map(|n| {
            periods[n].attr("duration").and_then(seconds).or_else(|| {
                let end = match starts.get(n + 1) {
                    Some(next) => (*next)?,
                    None => total?,
                };
                Some(end - starts[n]?).filter(|length| *length >= 0.0)
            })
        })
        .collect();
    let reps: Vec<Vec<Rep>> = periods
        .iter()
        .map(|period| {
            let base = base_url(period, &base);
            let sets = period.all("AdaptationSet").map(|set| (set, base_url(set, &base)));
            let reps = sets.flat_map(|(set, base)| set.all("Representation").take(MAX_TRACKS).filter_map(|rep| representation(period, set, rep, &base)).collect::<Vec<_>>());
            reps.take(MAX_TRACKS).collect()
        })
        .collect();
    let longest = (0..reps.len()).filter(|&n| !reps[n].is_empty()).fold(None, |best: Option<usize>, n| match best {
        Some(b) if lengths[b].unwrap_or(0.0) >= lengths[n].unwrap_or(0.0) => Some(b),
        _ => Some(n),
    });
    let reference = longest.ok_or_else(|| malformed("no representation of video, audio or subtitles"))?;
    let mut counts = [0usize; 3];
    let mut tracks = Vec::new();
    for rep in &reps[reference] {
        let kind = rep.track.kind as usize;
        let id = format!("{}{}", ["v", "a", "s"][kind], counts[kind]);
        counts[kind] += 1;
        let pieces = reps
            .iter()
            .enumerate()
            .filter_map(|(n, period)| {
                let same = if n == reference { rep } else { follow(period, rep)? };
                Some(Piece { length: lengths[n], protected: same.protected, addressing: same.addressing.clone(), start: starts[n], period: n })
            })
            .collect();
        tracks.push((Track { id, ..rep.track.clone() }, Rendition { ext: rep.ext, pieces, rep: rep.clone() }));
    }
    let mut parts: Vec<Part> = reps
        .iter()
        .enumerate()
        .map(|(n, period)| {
            let lead = period.iter().find(|r| r.track.kind == Kind::Video).or(period.first());
            let summary = lead.map(|r| summary_of(&r.track)).unwrap_or_default();
            Part { id: n, duration: lengths[n].unwrap_or(0.0), summary, ad: periods[n].attr("id").is_some_and(names_an_ad) }
        })
        .collect();
    let settings: Vec<String> = parts.iter().map(|p| p.summary.clone()).collect();
    mark_ads(&mut parts, &settings);
    const CLOCKS: [&str; 4] = ["utc:direct", "utc:http-iso", "utc:http-xsdate", "utc:http-head"];
    let clock = mpd
        .all("UTCTiming")
        .filter_map(|timing| Some((timing.attr("schemeIdUri")?.to_string(), timing.attr("value")?.to_string())))
        .find(|(scheme, _)| CLOCKS.iter().any(|known| scheme.contains(known)));
    Ok(Mpd {
        live,
        duration: total.or_else(|| lengths.iter().copied().sum()),
        tracks,
        parts,
        available_from: mpd.attr("availabilityStartTime").and_then(crate::hls::epoch_seconds),
        update_every: mpd.attr("minimumUpdatePeriod").and_then(seconds),
        buffer: mpd.attr("timeShiftBufferDepth").and_then(seconds),
        clock,
        periods: reps.into_iter().zip(starts).zip(lengths).map(|((reps, start), length)| (reps, start, length)).collect(),
    })
}

/// Seconds back a live manifest that does not say how far back it keeps its segments
/// (`timeShiftBufferDepth`) is taken to keep them.
// shortcut: such a manifest keeps them all by the standard; six hours bounds what one listing makes.
const UNTOLD_BUFFER: f64 = 6.0 * 3600.0;

impl Template {
    /// Seconds after a segment its number and the clock give (no timeline) ends that it is
    /// listed: its server may publish it late, or keep a clock behind this computer's.
    fn late(&self) -> f64 {
        match self.duration {
            Some(d) if self.timeline.is_empty() => (d as f64 / self.timescale as f64).max(2.0),
            _ => 0.0,
        }
    }

    /// The segments of a period `length` seconds long (when told) that have ended `elapsed`
    /// seconds after it started, and not `buffer` seconds or more before that: each its number,
    /// its time (in `timescale` units), and its start and length in seconds from the period's
    /// start.
    fn live(&self, elapsed: f64, length: Option<f64>, buffer: f64) -> Result<Vec<(u64, u64, f64, f64)>, HlsError> {
        let scale = self.timescale as f64;
        let end = length.unwrap_or(f64::INFINITY);
        let since = if buffer.is_finite() { elapsed - buffer } else { f64::NEG_INFINITY };
        let mut out = Vec::new();
        let mut keep = |number: u64, time: u64, at: f64, secs: f64| {
            if at + secs <= elapsed + 1e-6 && at < end - 1e-6 && at + secs > since {
                if out.len() == MAX_SEGMENTS {
                    return Err(too_many());
                }
                out.push((number, time, at, secs));
            }
            Ok(())
        };
        if !self.timeline.is_empty() {
            // Times in `timescale` units, as the timeline gives them (from `presentationTimeOffset`).
            let at = |secs: f64| self.offset as f64 + secs * scale;
            let (now, gone, stop) = (at(elapsed), at(since), at(end));
            let (mut time, mut number) = (0u64, self.start_number);
            for (k, &(t, d, r)) in self.timeline.iter().enumerate() {
                time = t.unwrap_or(time);
                if d == 0 {
                    return Err(malformed("a SegmentTimeline entry without a duration"));
                }
                let count = match u64::try_from(r) {
                    Ok(r) => r.saturating_add(1),
                    Err(_) => {
                        let next = self.timeline.get(k + 1).and_then(|next| next.0).map_or(stop.min(now), |next| next as f64);
                        ((next - time as f64) / d as f64).ceil().max(0.0) as u64
                    }
                };
                // Only those that have ended by now and start before the period's end, from about the
                // oldest still kept, and at most the newest MAX_SEGMENTS: `r` is the server's word.
                let ready = (((now - time as f64) / d as f64).floor().max(0.0) as u64).min(count).min(((stop - time as f64) / d as f64).ceil().max(0.0) as u64);
                let first = ((((gone - time as f64) / d as f64).floor() - 1.0).max(0.0) as u64).max(ready.saturating_sub(MAX_SEGMENTS as u64));
                for n in first.min(ready)..ready {
                    let start = time.saturating_add(n.saturating_mul(d));
                    keep(number.saturating_add(n), start, (start as f64 - self.offset as f64) / scale, d as f64 / scale)?;
                }
                time = time.saturating_add(count.saturating_mul(d));
                number = number.saturating_add(count);
            }
        } else if let Some(d) = self.duration {
            let secs = d as f64 / scale;
            if !end.is_finite() && !elapsed.is_finite() && self.end_number.is_none() {
                return Err(malformed("a SegmentTemplate without an end: the period's length is not given"));
            }
            // Segment k runs from k * secs to (k + 1) * secs: the last is the last to have ended
            // [`Self::late`] seconds ago, or to start before the period's end.
            let mut last = (((elapsed - self.late()) / secs + 1e-6).floor() as i64).saturating_sub(1).min(((end / secs - 1e-6).ceil() as i64).saturating_sub(1));
            if let Some(end) = self.end_number {
                last = last.min(end.checked_sub(self.start_number).map_or(-1, |n| i64::try_from(n).unwrap_or(i64::MAX)));
            }
            let first = ((since / secs).floor() as i64).saturating_sub(1).max(0).max(last.saturating_sub(MAX_SEGMENTS as i64 - 1));
            for k in first..=last {
                let k = k as u64;
                keep(self.start_number.saturating_add(k), self.offset.saturating_add(k.saturating_mul(d)), k as f64 * secs, secs)?;
            }
        } else {
            return Err(malformed("a live SegmentTemplate with neither a duration nor a timeline"));
        }
        Ok(out)
    }
}

/// The segments of `rendition` a live manifest `mpd` offers at `now` (seconds since 1970 by its
/// server's clock), after the one numbered `after` (see [`crate::stream::Listing`]): its periods'
/// segments that have ended by then and are not older than its `timeShiftBufferDepth`, all of
/// them once it is static. A segment is numbered by its start in milliseconds from the
/// presentation's start; each period after the first starts a new part. It has ended once it is
/// static, its presentation's length has passed, or its last period has with no updates to come.
/// Only segments a `SegmentTemplate` gives are found; a DRM-protected representation is refused.
pub(crate) fn live_listing(mpd: &Mpd, rendition: &Rendition, now: f64, after: Option<u64>) -> Result<crate::stream::Listing, HlsError> {
    if rendition.pieces.iter().any(|p| p.protected) {
        return Err(HlsError::Unsupported("DRM-protected stream".to_string()));
    }
    let origin = mpd.available_from.unwrap_or(0.0);
    let (mut segments, mut dates) = (Vec::new(), Vec::new());
    for (n, piece) in rendition.pieces.iter().enumerate() {
        // shortcut: a live SegmentList or SegmentBase is refused; read their segments by position if one turns up.
        let Addressing::Template(template) = &*piece.addressing else {
            return Err(HlsError::Unsupported("a live DASH stream whose segments no SegmentTemplate gives".to_string()));
        };
        let start = piece.start.unwrap_or(0.0);
        // A static manifest has all of each period.
        let elapsed = if mpd.live { now - origin - start } else { f64::INFINITY };
        let buffer = match (mpd.live, mpd.buffer) {
            (true, Some(buffer)) => buffer,
            (true, None) => UNTOLD_BUFFER,
            _ => f64::INFINITY,
        };
        let init = template.init()?.map(|url| Arc::new(InitSection { url, byte_range: None, encryption: None }));
        for (k, (number, time, at, secs)) in template.live(elapsed, piece.length, buffer)?.into_iter().enumerate() {
            let sequence = ((start + at) * 1000.0).round().max(0.0) as u64;
            if after.is_some_and(|after| sequence <= after) {
                continue;
            }
            let url = template.link(&template.media, number, time)?;
            let index = segments.len();
            segments.push(HlsSegment { index, sequence, url, duration_secs: secs, byte_range: None, encryption: None, init: init.clone(), discontinuity: n > 0 && k == 0 });
            dates.push(origin + start + at);
        }
    }
    // A manifest that may still change (`minimumUpdatePeriod`) ends by turning static; else once
    // its last segment is listed (see `Template::late`).
    let last = rendition.pieces.last();
    let late = match last.map(|p| &*p.addressing) {
        Some(Addressing::Template(template)) => template.late(),
        _ => 0.0,
    };
    let over = |length: Option<f64>, start: Option<f64>| length.is_some_and(|length| now - origin - start.unwrap_or(0.0) >= length + late);
    let ended = !mpd.live || (mpd.update_every.is_none() && (over(mpd.duration, None) || last.is_some_and(|p| over(p.length, p.start))));
    let lasts = segments.last().map_or(2.0, |s| s.duration_secs);
    Ok(crate::stream::Listing { segments, dates, ended, refresh: mpd.update_every.map_or(lasts, |every| every.min(lasts)), ext: rendition.ext, now })
}

/// Seconds the clock of `mpd`'s server (fetched from `base`) is ahead of this computer's at `now`
/// (seconds since 1970), as its `UTCTiming` tells: the time itself (`direct`), or a link whose
/// answer gives it in its body (`http-iso`, `http-xsdate`) or `Date` header (`http-head`). 0 when
/// it gives none this reads, or that fails: the clocks are taken to agree.
pub(crate) async fn clock_offset(client: &Client, auth: Option<&Auth>, fetch: FetchPolicy, mpd: &Mpd, base: &Url, now: f64) -> f64 {
    let Some((scheme, value)) = &mpd.clock else { return 0.0 };
    let link = value.split_whitespace().next().and_then(|first| base.join(first).ok());
    let server = match link {
        _ if scheme.contains("direct") => crate::hls::epoch_seconds(value),
        Some(link) if scheme.contains("http-head") => {
            let request = |url: &Url| crate::worker::authorize(client.head(url.clone()), auth, url);
            let answer = tokio::time::timeout(fetch.stall_timeout, crate::worker::send(request, &link)).await.ok().and_then(Result::ok);
            let date = answer.and_then(|a| a.headers().get(reqwest::header::DATE)?.to_str().ok().and_then(crate::engine::parse_http_date));
            date.map(|secs| secs as f64)
        }
        Some(link) => {
            let body = crate::hls::fetch_manifest(client, auth, &link, fetch, false).await.ok();
            body.and_then(|(body, _)| crate::hls::epoch_seconds(String::from_utf8_lossy(&body).trim()))
        }
        None => None,
    };
    match server {
        Some(server) => server - now,
        None => {
            tracing::warn!("Could not read the time from the stream's server ({scheme} {value}); going by this computer's clock");
            0.0
        }
    }
}

/// The segments of `rendition`: its periods one after the other, each after the first starting a
/// new part (a discontinuity), with the segment index or the file's size fetched where the
/// manifest gives no list. A DRM-protected representation is refused.
pub(crate) async fn segments(client: &Client, auth: Option<&Auth>, fetch: FetchPolicy, rendition: &Rendition) -> Result<Vec<HlsSegment>, HlsError> {
    if rendition.pieces.iter().any(|p| p.protected) {
        return Err(HlsError::Unsupported("DRM-protected stream".to_string()));
    }
    let mut segments = Vec::new();
    for (n, piece) in rendition.pieces.iter().enumerate() {
        let (init, listed) = piece.segments(client, auth, fetch).await?;
        for (k, (url, byte_range, duration_secs)) in listed.into_iter().enumerate() {
            if segments.len() == MAX_SEGMENTS {
                return Err(too_many());
            }
            let index = segments.len();
            let discontinuity = n > 0 && k == 0;
            segments.push(HlsSegment { index, sequence: index as u64, url, duration_secs, byte_range, encryption: None, init: init.clone(), discontinuity });
        }
    }
    if segments.is_empty() {
        return Err(HlsError::NoSegments);
    }
    Ok(segments)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::hls::tests::{ok, Reply};
    use std::time::Duration;

    const FETCH: FetchPolicy = FetchPolicy { stall_timeout: Duration::from_millis(500), max_retries: 2, host_limit: 0 };

    /// `body`, or the part of it `range` asks for, as a server that serves byte ranges answers.
    pub(crate) fn ranged(body: &[u8], range: Option<ByteRange>) -> Reply {
        match range {
            Some(r) if r.start < body.len() as u64 => {
                let end = r.end.min(body.len() as u64 - 1);
                let header = format!("Content-Range: bytes {}-{}/{}\r\n", r.start, end, body.len());
                (206, header, body[r.start as usize..=end as usize].to_vec())
            }
            Some(_) => (416, String::new(), Vec::new()),
            None => ok(body.to_vec()),
        }
    }

    /// A segment index (`sidx`, version 0) of segments of these sizes and milliseconds.
    pub(crate) fn sidx(segments: &[(u32, u32)]) -> Vec<u8> {
        let mut body = vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0x03, 0xe8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        body.extend((segments.len() as u16).to_be_bytes());
        for &(size, millis) in segments {
            body.extend(size.to_be_bytes());
            body.extend(millis.to_be_bytes());
            body.extend([0x90, 0, 0, 0]);
        }
        [((body.len() + 8) as u32).to_be_bytes().as_slice(), b"sidx", &body].concat()
    }

    fn parsed(mpd: &str) -> Mpd {
        parse(mpd, &Url::parse("https://host.example/show/manifest.mpd").unwrap()).unwrap()
    }

    type Listed = Vec<(String, Option<(u64, u64)>, f64, bool)>;

    async fn links(mpd: &Mpd, track: usize) -> Listed {
        let segments = segments(&Client::new(), None, FETCH, &mpd.tracks[track].1).await.unwrap();
        segments.iter().map(|s| (s.url.to_string(), s.byte_range.map(|r| (r.start, r.end)), s.duration_secs, s.discontinuity)).collect()
    }

    #[test]
    fn test_durations_templates_and_ranges_read_as_manifests_write_them() {
        assert_eq!([seconds("PT1H2M3.5S"), seconds("P1DT2H"), seconds("PT0S"), seconds("PT90M")], [Some(3723.5), Some(93_600.0), Some(0.0), Some(5400.0)]);
        assert_eq!([seconds("1H"), seconds("PT1X"), seconds("PT5")], [None, None, None]);
        assert_eq!(fill("$RepresentationID$/$Number%05d$-$Time$-$Bandwidth%03d$$$.m4s", "v1", 42, 9000, 7), "v1/00042-9000-007$.m4s");
        assert_eq!(fill("a$Unknown$b$Number$c$", "x", 1, 0, 0), "a$Unknown$b1c$");
        assert_eq!(byte_range(" 100 - 199 "), ByteRange::new(100, 199).ok());
        assert!(is_mpd(b"\xef\xbb\xbf <?xml version=\"1.0\"?>\n<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\">"));
        assert!(is_mpd(b"<mpd:MPD xmlns:mpd=\"urn:mpeg:dash:schema:mpd:2011\"/>") && !is_mpd(b"<html><body>MPD</body></html>"));
        let dolby = "tag:dolby.com,2014:dash:audio_channel_configuration:2011";
        assert_eq!([channel_count("urn:mpeg:dash:23003:3:audio_channel_configuration:2011", "2"), channel_count(dolby, "F801")], [Some(2), Some(6)]);
        assert_eq!([channel_count(dolby, "FA01"), channel_count("urn:mpeg:mpegB:cicp:ChannelConfiguration", "7")], [Some(8), Some(8)]);
        assert_eq!(frame_rate("30000/1001").map(|f| (f * 1000.0).round()), Some(29970.0));
    }

    /// A segment index lists its segments' byte ranges after the box, and their lengths; one cut
    /// short, or not there, lists none.
    #[test]
    fn test_a_segment_index_lists_the_segments_after_it() {
        let index = [b"\0\0\0\x08free".as_slice(), &sidx(&[(500, 2000), (300, 1000)])].concat();
        let listed = sidx_segments(&index, 1000).unwrap();
        let ranges: Vec<(u64, u64, f64, bool)> = listed.iter().map(|&(r, secs, further)| (r.start, r.end, secs, further)).collect();
        assert_eq!(ranges, [(1064, 1563, 2.0, false), (1564, 1863, 1.0, false)]);
        assert_eq!(sidx_segments(&index[..index.len() - 4], 0), None);
        assert_eq!(sidx_segments(b"\0\0\0\x08free", 0), None);
    }

    /// Every way a manifest addresses segments: a template inherited from the adaptation set
    /// (numbers, zero-padded), a template of the representation's own over it (a timeline with
    /// repeats up to the period's end, times, bandwidth and `$$`), a list with byte ranges; base
    /// links at every level, resolved against the manifest's, and an absolute one; thumbnails
    /// left out; what each track is, by its attributes.
    #[tokio::test]
    async fn test_each_addressing_form_lists_its_segments() {
        let mpd = parsed(
            r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT8S">
  <BaseURL>cdn/</BaseURL>
  <Period id="p0">
    <BaseURL>p0/</BaseURL>
    <AdaptationSet contentType="video" mimeType="video/mp4" codecs="avc1.64001f">
      <BaseURL>video/</BaseURL>
      <SegmentTemplate timescale="1000" duration="2000" startNumber="5" media="$RepresentationID$/seg-$Number%03d$.m4s" initialization="$RepresentationID$/init.mp4"/>
      <Representation id="v720" bandwidth="3000000" width="1280" height="720" frameRate="30000/1001"/>
      <Representation id="v1080" bandwidth="6000000" width="1920" height="1080">
        <SupplementalProperty schemeIdUri="urn:mpeg:mpegB:cicp:TransferCharacteristics" value="16"/>
        <SegmentTemplate timescale="90000" presentationTimeOffset="900" media="hd/$Time$-$Bandwidth$.m4s?x=$$">
          <SegmentTimeline><S t="900" d="180000" r="1"/><S d="90000" r="-1"/></SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4" lang="en">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <Label>English &amp; more</Label>
      <AudioChannelConfiguration schemeIdUri="tag:dolby.com,2014:dash:audio_channel_configuration:2011" value="F801"/>
      <Representation id="a" bandwidth="128000" codecs="ec-3">
        <BaseURL>https://other.example/a/</BaseURL>
        <SegmentList timescale="10" duration="40">
          <Initialization sourceURL="init.mp4" range="0-99"/>
          <SegmentURL media="a1.m4s"/><SegmentURL media="a2.m4s" mediaRange="100-199"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="image/jpeg"><Representation id="thumbs" bandwidth="1"/></AdaptationSet>
  </Period>
</MPD>"#,
        );
        assert!(!mpd.live);
        assert_eq!(mpd.duration, Some(8.0));
        let described: Vec<String> = mpd.tracks.iter().map(|(t, r)| format!("{} .{}", t.describe(), r.ext)).collect();
        assert_eq!(described, [
            "v0 video 1280x720 29.97fps avc 3000 kbps .mp4",
            "v1 video 1920x1080 avc hdr10 6000 kbps .mp4",
            "a0 audio en eac3 6ch 128 kbps \"English & more\" default .m4a",
        ]);
        let base = "https://host.example/show/cdn/p0/video/";
        let v720 = links(&mpd, 0).await;
        let numbers: Vec<String> = v720.iter().map(|(url, ..)| url.replace(base, "")).collect();
        assert_eq!(numbers, ["v720/seg-005.m4s", "v720/seg-006.m4s", "v720/seg-007.m4s", "v720/seg-008.m4s"]);
        assert!(v720.iter().all(|(_, range, secs, _)| range.is_none() && *secs == 2.0));
        let first = segments(&Client::new(), None, FETCH, &mpd.tracks[0].1).await.unwrap().remove(0);
        assert_eq!(first.init.unwrap().url.as_str(), format!("{base}v720/init.mp4"));
        let v1080: Vec<(String, f64)> = links(&mpd, 1).await.into_iter().map(|(url, _, secs, _)| (url.replace(base, ""), secs)).collect();
        let expected = [(900, 2.0), (180_900, 2.0), (360_900, 1.0), (450_900, 1.0), (540_900, 1.0), (630_900, 1.0)];
        assert_eq!(v1080, expected.map(|(time, secs)| (format!("hd/{time}-6000000.m4s?x=$"), secs)));
        assert_eq!(links(&mpd, 2).await, [
            ("https://other.example/a/a1.m4s".to_string(), None, 4.0, false),
            ("https://other.example/a/a2.m4s".to_string(), Some((100, 199)), 4.0, false),
        ]);
        let first = segments(&Client::new(), None, FETCH, &mpd.tracks[2].1).await.unwrap().remove(0);
        let init = first.init.unwrap();
        assert_eq!((init.url.as_str(), init.byte_range), ("https://other.example/a/init.mp4", ByteRange::new(0, 99).ok()));
    }

    /// A track is followed across periods by its representation's id, else by kind and language
    /// (into an ad break with representations of its own); each period after the first starts a
    /// new part, and a period named an ad, or short and unlike its neighbours, looks like one. The
    /// tracks are those of the longest period.
    #[tokio::test]
    async fn test_tracks_are_followed_across_periods() {
        let period = |id: &str, timing: &str, folder: &str, seconds: u32| {
            format!(
                r#"<Period id="{id}" {timing}>
  <AdaptationSet contentType="video"><SegmentTemplate media="{folder}/$RepresentationID$-$Number$.m4s" initialization="{folder}/$RepresentationID$-init.mp4" duration="{seconds}"/>
    <Representation id="hi" bandwidth="5000000" width="1920" height="1080" codecs="avc1.640028"/>
    <Representation id="lo" bandwidth="1000000" width="640" height="360" codecs="avc1.4d401e"/>
  </AdaptationSet>
  <AdaptationSet contentType="audio" lang="en"><SegmentTemplate media="{folder}/aud-$Number$.m4s" duration="{seconds}"/><Representation id="aud" bandwidth="128000" codecs="mp4a.40.2"/></AdaptationSet>
</Period>"#
            )
        };
        let ad = r#"<Period id="ad-break" duration="PT4S">
  <AdaptationSet contentType="video"><SegmentTemplate media="ad/v-$Number$.m4s" initialization="ad/v-init.mp4" duration="4"/><Representation id="adv" bandwidth="2000000" width="1280" height="720" codecs="avc1.64001f"/></AdaptationSet>
  <AdaptationSet contentType="audio" lang="en"><SegmentTemplate media="ad/a-$Number$.m4s" duration="4"/><Representation id="ada" bandwidth="96000" codecs="mp4a.40.2"/></AdaptationSet>
</Period>"#;
        let mpd = parsed(&format!(
            r#"<MPD type="static" mediaPresentationDuration="PT40S">{}{ad}{}</MPD>"#,
            period("main-1", r#"duration="PT20S""#, "m1", 10),
            period("main-2", r#"start="PT24S""#, "m2", 8),
        ));
        let ids: Vec<&str> = mpd.tracks.iter().map(|(t, _)| t.id.as_str()).collect();
        assert_eq!(ids, ["v0", "v1", "a0"]);
        let names = |links: Listed| -> Vec<(String, bool)> {
            links.into_iter().map(|(url, _, _, discontinuity)| (url.replace("https://host.example/show/", ""), discontinuity)).collect()
        };
        let low = [("m1/lo-1.m4s", false), ("m1/lo-2.m4s", false), ("ad/v-1.m4s", true), ("m2/lo-1.m4s", true), ("m2/lo-2.m4s", false)];
        assert_eq!(names(links(&mpd, 1).await), low.map(|(url, cut)| (url.to_string(), cut)));
        let audio = [("m1/aud-1.m4s", false), ("m1/aud-2.m4s", false), ("ad/a-1.m4s", true), ("m2/aud-1.m4s", true), ("m2/aud-2.m4s", false)];
        assert_eq!(names(links(&mpd, 2).await), audio.map(|(url, cut)| (url.to_string(), cut)));
        let parts: Vec<(usize, f64, &str, bool)> = mpd.parts.iter().map(|p| (p.id, p.duration, p.summary.as_str(), p.ad)).collect();
        assert_eq!(parts, [(0, 20.0, "1920x1080 avc", false), (1, 4.0, "1280x720 avc", true), (2, 16.0, "1920x1080 avc", false)]);
        assert_eq!(mpd.duration, Some(40.0));
    }

    /// A period a whole number of segments long lists that many, though its length (from the
    /// next period's start) comes out a hair over; an end number before the start number lists
    /// none, on demand or live.
    #[tokio::test]
    async fn test_whole_periods_list_no_extra_segment() {
        let period = |start: &str, folder: &str| {
            format!(
                r#"<Period start="{start}"><AdaptationSet contentType="audio"><SegmentTemplate timescale="1000" duration="2002" media="{folder}/$Number$.m4s"/>
                <Representation id="a" bandwidth="1" codecs="mp4a.40.2"/></AdaptationSet></Period>"#
            )
        };
        let mpd = parsed(&format!(r#"<MPD type="static" mediaPresentationDuration="PT8.008S">{}{}</MPD>"#, period("PT2.002S", "a"), period("PT6.006S", "b")));
        let names: Vec<String> = links(&mpd, 0).await.into_iter().map(|(url, ..)| url.replace("https://host.example/show/", "")).collect();
        assert_eq!(names, ["a/1.m4s", "a/2.m4s", "b/1.m4s"]);
        let backwards = parsed(
            r#"<MPD mediaPresentationDuration="PT8S"><Period><AdaptationSet contentType="audio"><SegmentTemplate duration="2" startNumber="5" endNumber="4" media="$Number$.m4s"/>
            <Representation id="a" bandwidth="1" codecs="mp4a.40.2"/></AdaptationSet></Period></MPD>"#,
        );
        assert!(matches!(segments(&Client::new(), None, FETCH, &backwards.tracks[0].1).await, Err(HlsError::NoSegments)));
        let Addressing::Template(template) = &*backwards.tracks[0].1.pieces[0].addressing else { panic!("a template") };
        assert!(template.live(100.0, None, f64::INFINITY).unwrap().is_empty());
    }

    /// Protection on a chosen track refuses it as DRM; a live manifest is read as live; one that
    /// is no XML, has no MPD or nothing to play, or a timeline with no end, is malformed.
    #[tokio::test]
    async fn test_drm_live_and_malformed_manifests() {
        let protected = parsed(
            r#"<MPD><Period duration="PT4S"><AdaptationSet contentType="video">
            <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>
            <SegmentTemplate media="v-$Number$.m4s" duration="2"/><Representation id="v" bandwidth="1" codecs="avc1"/>
            </AdaptationSet></Period></MPD>"#,
        );
        let err = segments(&Client::new(), None, FETCH, &protected.tracks[0].1).await.unwrap_err();
        assert!(err.to_string().contains("DRM-protected stream"), "{err}");
        let live = parsed(
            r#"<MPD type="dynamic" availabilityStartTime="2024-01-01T00:00:00Z"><Period start="PT0S">
            <AdaptationSet contentType="video"><SegmentTemplate media="v-$Number$.m4s" duration="2"/><Representation id="v" bandwidth="1" codecs="avc1"/></AdaptationSet></Period></MPD>"#,
        );
        assert!(live.live && live.tracks.len() == 1);
        let url = Url::parse("https://host.example/m.mpd").unwrap();
        for bad in ["<MPD><Period>", "<html/>", "<MPD><Period><AdaptationSet mimeType=\"image/png\"/></Period></MPD>"] {
            assert!(matches!(parse(bad, &url), Err(HlsError::Unsupported(_))), "{bad}");
        }
        let endless = parsed(
            r#"<MPD><Period><AdaptationSet contentType="audio"><SegmentTemplate media="a-$Time$.m4s"><SegmentTimeline><S d="2" r="-1"/></SegmentTimeline></SegmentTemplate>
            <Representation id="a" bandwidth="1" codecs="mp4a.40.2"/></AdaptationSet></Period></MPD>"#,
        );
        let err = segments(&Client::new(), None, FETCH, &endless.tracks[0].1).await.unwrap_err();
        assert!(matches!(err, HlsError::Unsupported(_)), "{err}");
    }

    /// The segments of a live manifest's track at `now` seconds after its start, after `after`:
    /// (number in its link, start in ms, date after the start, discontinuity).
    fn live_at(mpd: &Mpd, now: f64, after: Option<u64>) -> (Vec<(String, u64, f64, bool)>, bool) {
        let origin = mpd.available_from.unwrap();
        let listing = live_listing(mpd, &mpd.tracks[0].1, origin + now, after).unwrap();
        let name = |s: &HlsSegment| s.url.path().rsplit('/').next().unwrap().trim_end_matches(".m4s").to_string();
        let segments = listing.segments.iter().zip(&listing.dates).map(|(s, date)| (name(s), s.sequence, date - origin, s.discontinuity)).collect();
        (segments, listing.ended)
    }

    /// A live manifest offers the segments its clock has made: by number, those that have ended
    /// and are still in its time-shift buffer; by a timeline (repeating up to now), what it lists
    /// that has ended; a later period goes on from the one before, its first segment a new part.
    /// Each is numbered by its start in milliseconds, and dated from the manifest's start. Turned
    /// static, it offers all of each period and has ended.
    #[test]
    fn test_a_live_manifest_offers_the_segments_its_clock_has_made() {
        let numbered = |kind: &str, update: &str| {
            parsed(&format!(
                r#"<MPD type="{kind}" availabilityStartTime="2024-01-01T00:00:00Z" timeShiftBufferDepth="PT6S" {update} mediaPresentationDuration="PT12S">
                <Period id="a" start="PT0S" duration="PT8S"><AdaptationSet contentType="audio"><SegmentTemplate media="a/$Number$.m4s" duration="2" startNumber="10"/>
                <Representation id="a" bandwidth="1" codecs="mp4a.40.2"/></AdaptationSet></Period>
                <Period id="b" start="PT8S"><AdaptationSet contentType="audio"><SegmentTemplate media="b/$Number$.m4s" initialization="b/init.mp4" duration="2"/>
                <Representation id="a" bandwidth="1" codecs="mp4a.40.2"/></AdaptationSet></Period></MPD>"#
            ))
        };
        let live = numbered("dynamic", r#"minimumUpdatePeriod="PT1S""#);
        assert_eq!((live.available_from, live.update_every, live.buffer), (Some(1_704_067_200.0), Some(1.0), Some(6.0)));
        // 9.5 s in: segments 0 to 2 of the first period ended 2 s ago or more (3 only 1.5 s ago),
        // 1 and 2 within the buffer.
        let segments = vec![("11".into(), 2000, 2.0, false), ("12".into(), 4000, 4.0, false)];
        assert_eq!(live_at(&live, 9.5, None), (segments, false));
        // 12 s in, after the second: the first period's last one, and the second's first, a new part.
        assert_eq!(live_at(&live, 12.0, Some(4000)), (vec![("13".into(), 6000, 6.0, false), ("1".into(), 8000, 8.0, true)], false));
        assert!(!live_at(&live, 14.5, None).1, "it may still change");
        let untold = numbered("dynamic", "");
        assert!(!live_at(&untold, 13.5, None).1, "its last segment is still to be listed");
        let (segments, ended) = live_at(&untold, 14.0, Some(8000));
        assert_eq!((segments, ended), (vec![("2".into(), 10_000, 10.0, false)], true), "the presentation's length has passed");
        let (segments, ended) = live_at(&numbered("static", ""), 1.0, Some(8000));
        assert_eq!((segments, ended), (vec![("2".into(), 10_000, 10.0, false)], true));
        let timeline = parsed(
            r#"<MPD type="dynamic" availabilityStartTime="2024-01-01T00:00:00Z"><Period start="PT10S"><AdaptationSet contentType="audio">
            <SegmentTemplate timescale="10" presentationTimeOffset="100" media="t/$Time$.m4s"><SegmentTimeline><S t="100" d="20" r="1"/><S d="10" r="-1"/></SegmentTimeline></SegmentTemplate>
            <Representation id="a" bandwidth="1" codecs="mp4a.40.2"/></AdaptationSet></Period></MPD>"#,
        );
        // 7.5 s into the period: two of 2 s, then of 1 s up to now.
        let (segments, _) = live_at(&timeline, 17.5, None);
        let starts: Vec<(String, u64)> = segments.into_iter().map(|(name, sequence, ..)| (name, sequence)).collect();
        let expected = [("100", 10_000), ("120", 12_000), ("140", 14_000), ("150", 15_000), ("160", 16_000)];
        assert_eq!(starts, expected.map(|(name, at)| (name.to_string(), at)));
        assert!(live_at(&timeline, 5.0, None).0.is_empty(), "the period has not started");
        let listed = parsed(
            r#"<MPD type="dynamic" availabilityStartTime="2024-01-01T00:00:00Z"><Period><AdaptationSet contentType="audio">
            <SegmentList duration="2"><SegmentURL media="a.m4s"/></SegmentList><Representation id="a" bandwidth="1" codecs="mp4a.40.2"/></AdaptationSet></Period></MPD>"#,
        );
        assert!(matches!(live_listing(&listed, &listed.tracks[0].1, 1e9, None), Err(HlsError::Unsupported(_))));
    }

    /// A live timeline that repeats far past its period's end lists only what plays in the
    /// period, at once, at most the newest MAX_SEGMENTS; one repeating up to now for weeks, with
    /// no time-shift buffer given, the newest six hours.
    #[test]
    fn test_a_live_timeline_lists_a_bounded_window() {
        let timeline = |period: &str, entry: &str| {
            parsed(&format!(
                r#"<MPD type="dynamic" availabilityStartTime="2024-01-01T00:00:00Z"><Period start="PT0S" {period}><AdaptationSet contentType="audio">
                <SegmentTemplate timescale="10000" media="t/$Time$.m4s"><SegmentTimeline>{entry}</SegmentTimeline></SegmentTemplate>
                <Representation id="a" bandwidth="1" codecs="mp4a.40.2"/></AdaptationSet></Period></MPD>"#
            ))
        };
        let huge = timeline(r#"duration="PT1S""#, r#"<S t="0" d="1" r="9000000000000000000"/>"#);
        let Addressing::Template(template) = &*huge.tracks[0].1.pieces[0].addressing else { panic!("a template") };
        let listed = template.live(1e9, Some(1.0), f64::INFINITY).unwrap();
        assert_eq!((listed.len(), listed.last().map(|s| s.1)), (MAX_SEGMENTS, Some(9999)));
        let now = 30.0 * 86_400.0;
        let (segments, _) = live_at(&timeline("", r#"<S t="0" d="300000" r="-1"/>"#), now, None);
        let (first, last) = (segments.first().unwrap().2, segments.last().unwrap().2);
        assert_eq!((segments.len(), first, last), (720, now - UNTOLD_BUFFER, now - 30.0));
    }

    /// The server's clock is read from the manifest's `UTCTiming`: given in it, or not at all.
    #[tokio::test]
    async fn test_the_servers_clock_is_read_from_the_manifest() {
        let mpd = |timing: &str| parsed(&format!(r#"<MPD type="dynamic"><UTCTiming {timing}/><Period><AdaptationSet contentType="audio"><SegmentTemplate media="$Number$" duration="2"/><Representation id="a" bandwidth="1" codecs="mp4a.40.2"/></AdaptationSet></Period></MPD>"#));
        let base = Url::parse("http://127.0.0.1:9/m.mpd").unwrap();
        let direct = mpd(r#"schemeIdUri="urn:mpeg:dash:utc:direct:2014" value="2024-01-01T00:00:30Z""#);
        assert_eq!(clock_offset(&Client::new(), None, FETCH, &direct, &base, 1_704_067_225.0).await, 5.0);
        let unknown = mpd(r#"schemeIdUri="urn:mpeg:dash:utc:ntp:2014" value="pool.ntp.org""#);
        assert!(unknown.clock.is_none());
        assert_eq!(clock_offset(&Client::new(), None, FETCH, &unknown, &base, 0.0).await, 0.0);
    }

    /// A file with a segment index is fetched as its init section and the segments the index
    /// lists, those of an index it points at in that one's place; one with only a base link, as
    /// its start and pieces after it, by the size a request for its first byte tells; a server
    /// that serves no ranges is refused.
    #[tokio::test]
    async fn test_indexed_and_whole_files_are_fetched_in_pieces() {
        let indexed = [vec![b'I'; 100], sidx(&[(500, 2000), (300, 1000)]), vec![b'M'; 800]].concat();
        // An index of a further index (56 bytes, then the 800 it lists) and a segment of 200.
        let chained = [vec![b'I'; 100], sidx(&[(0x8000_0000 | 856, 3000), (200, 1000)]), sidx(&[(500, 2000), (300, 1000)]), vec![b'M'; 1000]].concat();
        let whole: Vec<u8> = (0..10_000u32).map(|n| n as u8).collect();
        let (addr, _) = crate::hls::tests::serve(move |path: &str, range| match path {
            "/indexed.mp4" => ranged(&indexed, range),
            "/chained.mp4" => ranged(&chained, range),
            "/whole.mp4" => ranged(&whole, range),
            "/norange.mp4" => ok(b"no ranges here".to_vec()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let mpd = parse(
            r#"<MPD mediaPresentationDuration="PT3S"><Period><AdaptationSet contentType="video" codecs="avc1">
                <Representation id="i" bandwidth="1"><BaseURL>indexed.mp4</BaseURL><SegmentBase indexRange="100-155"><Initialization range="0-99"/></SegmentBase></Representation>
                <Representation id="w" bandwidth="2"><BaseURL>whole.mp4</BaseURL></Representation>
                <Representation id="n" bandwidth="3"><BaseURL>norange.mp4</BaseURL></Representation>
                <Representation id="c" bandwidth="4"><BaseURL>chained.mp4</BaseURL><SegmentBase indexRange="100-155"/></Representation>
            </AdaptationSet></Period></MPD>"#,
            &Url::parse(&format!("http://{addr}/stream/../manifest.mpd")).unwrap(),
        )
        .unwrap();
        let ranges = |listed: &[HlsSegment]| -> Vec<(u64, u64)> { listed.iter().filter_map(|s| s.byte_range.map(|r| (r.start, r.end))).collect() };
        let listed = segments(&Client::new(), None, FETCH, &mpd.tracks[0].1).await.unwrap();
        assert_eq!(ranges(&listed), [(156, 655), (656, 955)]);
        assert_eq!(listed[0].init.as_ref().unwrap().byte_range, ByteRange::new(0, 99).ok());
        let listed = segments(&Client::new(), None, FETCH, &mpd.tracks[1].1).await.unwrap();
        assert_eq!(ranges(&listed), [(1024, 5119), (5120, 9215), (9216, 9999)]);
        assert_eq!(listed[0].init.as_ref().unwrap().byte_range, ByteRange::new(0, 1023).ok());
        let err = segments(&Client::new(), None, FETCH, &mpd.tracks[2].1).await.unwrap_err();
        assert!(err.to_string().contains("serves no byte ranges"), "{err}");
        let listed = segments(&Client::new(), None, FETCH, &mpd.tracks[3].1).await.unwrap();
        assert_eq!(ranges(&listed), [(212, 711), (712, 1011), (1012, 1211)]);
        assert_eq!(listed.iter().map(|s| s.duration_secs).collect::<Vec<_>>(), [2.0, 1.0, 1.0]);
    }
}
