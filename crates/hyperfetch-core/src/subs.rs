//! Subtitles read from WebVTT, TTML (IMSC, DFXP) and fragmented MP4 (`stpp`, `wvtt`) into one
//! list of timed cues, joined across segments, moved in time and written as SRT or WebVTT.
//!
//! For a stream: parse each segment, put the cues of all segments together, [`join`] them,
//! [`shift`] them by the media's start, then write them with [`to_srt`] or [`to_vtt`].

use std::collections::HashMap;
use std::ops::Range;

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

/// Largest gap (ms) between a cue and the same text after it for the two to be one cue: times
/// rounded to milliseconds may leave one where a segment cut a cue in two.
const JOIN_GAP_MS: u64 = 1;

/// Most text runs one TTML paragraph is read with; the rest is left out. Each paragraph's text
/// is cut at every run's begin and end, so this keeps that work small.
const MAX_PIECES: usize = 128;

/// The tags a cue's text may hold.
const TAGS: [&str; 6] = ["<i>", "</i>", "<b>", "</b>", "<u>", "</u>"];

/// One subtitle: shown from `start` to `end` (milliseconds on the media's clock), its `text` with
/// its line breaks and `<i>`, `<b>` and `<u>` tags as SRT and WebVTT write them (other characters
/// as they are, unescaped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start: u64,
    pub end: u64,
    pub text: String,
}

/// A cue from `start` to `end` (ms; a start before 0 is taken as 0), None when it shows nothing:
/// no text, or no time.
fn cue(start: i64, end: i64, text: String) -> Option<Cue> {
    let start = start.max(0);
    let text = tidy(&text);
    (end > start && !text.is_empty()).then_some(Cue { start: start as u64, end: end as u64, text })
}

/// `text` with each line trimmed and blank lines left out.
fn tidy(text: &str) -> String {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

// ---------------------------------------------------------------------------------------------
// WebVTT

/// The cues of a WebVTT file, on the media's clock by its `X-TIMESTAMP-MAP` (a cue at LOCAL
/// plays at MPEGTS) when it has one. Cue settings, notes, styles and regions are left out, and so
/// are tags other than `<i>`, `<b>` and `<u>` (their text is kept).
pub fn parse_vtt(text: &str) -> Vec<Cue> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = text.lines().collect();
    let mut offset = 0;
    let mut cues = Vec::new();
    for (i, block) in lines.split(|l| l.trim().is_empty()).filter(|b| !b.is_empty()).enumerate() {
        let first = block[0].trim();
        if i == 0 && first.starts_with("WEBVTT") {
            offset = block.iter().find_map(|l| l.trim().strip_prefix("X-TIMESTAMP-MAP=")).and_then(timestamp_map).unwrap_or(0);
            continue;
        }
        let kind = |k: &str| first.strip_prefix(k).is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
        if kind("NOTE") || kind("STYLE") || kind("REGION") {
            continue;
        }
        // An optional identifier, the timing line, then the text.
        let Some(at) = block.iter().take(2).position(|l| l.contains("-->")) else { continue };
        let Some((start, end)) = vtt_timing(block[at]) else { continue };
        cues.extend(cue(start + offset, end + offset, clean_vtt_text(&block[at + 1..].join("\n"))));
    }
    cues
}

/// Start and end (ms) of a cue's timing line (`00:00:01.000 --> 00:00:02.000 align:start`).
fn vtt_timing(line: &str) -> Option<(i64, i64)> {
    let (from, rest) = line.split_once("-->")?;
    Some((vtt_time(from)?, vtt_time(rest.split_whitespace().next()?)?))
}

/// Milliseconds of a WebVTT time (`01:02:03.456` or `02:03.456`; SRT's comma is taken too).
fn vtt_time(time: &str) -> Option<i64> {
    let (clock, fraction) = time.trim().split_once(['.', ','])?;
    let digits = |s: &str| !s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(fraction) {
        return None;
    }
    let fraction = &fraction[..fraction.len().min(3)];
    let millis = fraction.parse::<i64>().ok()? * 10i64.pow(3 - fraction.len() as u32);
    let parts: Vec<&str> = clock.split(':').collect();
    if !(2..=3).contains(&parts.len()) || !parts.iter().all(|p| digits(p)) {
        return None;
    }
    let seconds = parts.iter().try_fold(0i64, |total, n| Some(total * 60 + n.parse::<i64>().ok()?))?;
    Some(seconds * 1000 + millis)
}

/// Where cue time 0 plays on the media's clock (ms) by an `X-TIMESTAMP-MAP`
/// (`MPEGTS:900000,LOCAL:00:00:00.000`).
fn timestamp_map(map: &str) -> Option<i64> {
    let (mut mpegts, mut local) = (None, None);
    for part in map.split(',') {
        match part.trim().split_once(':')? {
            ("MPEGTS", ticks) => mpegts = ticks.trim().parse::<i64>().ok().filter(|t| (0..1 << 33).contains(t)),
            ("LOCAL", time) => local = vtt_time(time),
            _ => {}
        }
    }
    Some((mpegts? + 45) / 90 - local?)
}

/// WebVTT cue text with its `<i>`, `<b>` and `<u>` tags (classes dropped) and its character
/// references read; other tags (voices, classes, languages, ruby, timestamps) are left out, their
/// text kept.
fn clean_vtt_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(['<', '&']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if rest.starts_with('<') {
            let Some(close) = rest.find('>') else {
                out.push('<');
                rest = &rest[1..];
                continue;
            };
            let tag = &rest[1..close];
            let (slash, name) = tag.strip_prefix('/').map_or(("", tag), |name| ("/", name));
            let name = name.split(['.', ' ', '\t']).next().unwrap_or_default();
            if matches!(name, "i" | "b" | "u") {
                out.push_str(&format!("<{slash}{name}>"));
            }
            rest = &rest[close + 1..];
        } else {
            let named = rest[1..].find(';').filter(|&n| n <= 10).and_then(|n| Some((entity(&rest[1..1 + n])?, n + 2)));
            let (c, len) = named.unwrap_or(('&', 1));
            out.push(c);
            rest = &rest[len..];
        }
    }
    out.push_str(rest);
    out
}

/// The character an HTML character reference names (`amp`, `#38`, `#x26`).
fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "lrm" => '\u{200e}',
        "rlm" => '\u{200f}',
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)?
        }
    })
}

// ---------------------------------------------------------------------------------------------
// TTML

/// The cues of a TTML (IMSC, DFXP) document and how many images it has, which are left out
/// (`smpte:backgroundImage`, IMSC `image`). Times are clock times (`00:00:01.500`, `00:00:01:12`
/// in frames at `ttp:frameRate` × `ttp:frameRateMultiplier`) or offsets (`1.5s`, `500ms`, `2m`,
/// `2h`, `40f`, `9000t` in ticks at `ttp:tickRate`), each counted from its parent's begin
/// (`body`, `div`, `p`, `span`); text under timed `span`s shows only while they do. Italic and
/// bold text (inline or by a referenced style) gets `<i>` and `<b>`. Any namespace prefix is taken.
pub fn parse_ttml(xml: &str) -> Result<(Vec<Cue>, usize), String> {
    let (timed, images) = ttml_cues(xml)?;
    Ok((timed.into_iter().filter_map(|t| cue(secs_ms(t.begin), secs_ms(t.end?), t.text)).collect(), images))
}

/// Milliseconds of `seconds`, rounded.
fn secs_ms(seconds: f64) -> i64 {
    (seconds * 1000.0).round() as i64
}

/// Text of a TTML document shown from `begin` to `end` (seconds on the document's clock; None:
/// until what holds the document ends, which the document does not say).
struct Timed {
    begin: f64,
    end: Option<f64>,
    text: String,
}

/// What an open TTML element passes to what it holds.
#[derive(Clone, Copy, Default)]
struct Scope {
    begin: f64,
    end: Option<f64>,
    italic: bool,
    bold: bool,
    /// Inside `head`, `metadata` or an image: its text is not shown.
    skip: bool,
    /// Inside a `p`.
    paragraph: bool,
}

/// A run of a paragraph's text (None for a `<br/>`) and when it shows.
struct Piece {
    begin: f64,
    end: Option<f64>,
    text: Option<String>,
    italic: bool,
    bold: bool,
}

/// The `tt` element's frame and tick rates (per second) its times are counted in.
struct Rates {
    frame: f64,
    sub_frame: f64,
    tick: f64,
}

impl Default for Rates {
    fn default() -> Self {
        Rates { frame: 30.0, sub_frame: 1.0, tick: 1.0 }
    }
}

impl Rates {
    fn of(attrs: &[(String, String)]) -> Rates {
        let positive = |n: f64| (n.is_finite() && n > 0.0).then_some(n);
        let number = |key: &str| attr(attrs, key).and_then(|v| v.trim().parse::<f64>().ok()).and_then(positive);
        let multiplier = attr(attrs, "frameRateMultiplier").and_then(|v| {
            let mut n = v.split_whitespace().map(|x| x.parse::<f64>().ok());
            positive(n.next()?? / n.next()??)
        });
        let frame = number("frameRate").unwrap_or(30.0) * multiplier.unwrap_or(1.0);
        let sub_frame = number("subFrameRate").unwrap_or(1.0);
        let tick = number("tickRate").unwrap_or(if number("frameRate").is_some() { frame * sub_frame } else { 1.0 });
        Rates { frame, sub_frame, tick }
    }

    /// Seconds of a TTML time expression, None for one this does not read (a wall-clock time).
    fn time(&self, value: &str) -> Option<f64> {
        let value = value.trim();
        let n = |s: &str| s.parse::<f64>().ok();
        let seconds = if value.contains(':') {
            let parts: Vec<&str> = value.split(':').collect();
            if !(3..=4).contains(&parts.len()) {
                return None;
            }
            let mut seconds = n(parts[0])? * 3600.0 + n(parts[1])? * 60.0 + n(parts[2])?;
            if let Some(frames) = parts.get(3) {
                let (frames, sub) = frames.split_once('.').unwrap_or((frames, "0"));
                seconds += n(frames)? / self.frame + n(sub)? / (self.frame * self.sub_frame);
            }
            seconds
        } else {
            let (number, metric) = value.split_at(value.find(|c: char| c.is_ascii_alphabetic())?);
            let unit = match metric {
                "h" => 3600.0,
                "m" => 60.0,
                "s" => 1.0,
                "ms" => 0.001,
                "f" => 1.0 / self.frame,
                "t" => 1.0 / self.tick,
                _ => return None,
            };
            n(number)? * unit
        };
        (seconds.is_finite() && seconds >= 0.0).then_some(seconds)
    }
}

/// The value of the attribute named `key` (local name, any prefix).
fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// An element's attributes by local name, values unescaped.
fn attributes(element: &BytesStart) -> Vec<(String, String)> {
    let value = |a: &quick_xml::events::attributes::Attribute| a.unescape_value().map(|v| v.into_owned()).unwrap_or_default();
    element.attributes().flatten().map(|a| (String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned(), value(&a))).collect()
}

/// The shown text of a TTML document and how many images it has. Reading stops at the end of
/// the document's first element, so what follows it (an `stpp` sample's images) is not read.
fn ttml_cues(xml: &str) -> Result<(Vec<Timed>, usize), String> {
    let mut reader = Reader::from_str(xml);
    let mut rates = Rates::default();
    // Each style's italic and bold, where it says.
    let mut styles: HashMap<String, (Option<bool>, Option<bool>)> = HashMap::new();
    let mut stack: Vec<Scope> = Vec::new();
    let mut pieces: Vec<Piece> = Vec::new();
    let (mut out, mut images) = (Vec::new(), 0);
    loop {
        let event = reader.read_event().map_err(|e| format!("bad TTML at byte {}: {}", reader.buffer_position(), e))?;
        let (element, empty) = match event {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::End(_) => {
                if close(&mut stack, &mut pieces, &mut out) {
                    break;
                }
                continue;
            }
            Event::Text(t) => {
                let text = t.unescape().map(|t| t.into_owned()).unwrap_or_else(|_| String::from_utf8_lossy(&t).into_owned());
                add_text(&stack, &mut pieces, Some(text));
                continue;
            }
            Event::CData(t) => {
                add_text(&stack, &mut pieces, Some(String::from_utf8_lossy(&t).into_owned()));
                continue;
            }
            Event::Eof => break,
            _ => continue,
        };
        let name = String::from_utf8_lossy(element.local_name().as_ref()).into_owned();
        let attrs = attributes(&element);
        let parent = stack.last().copied().unwrap_or_default();
        if name == "tt" {
            rates = Rates::of(&attrs);
        }
        // Referenced styles in order, then the element's own.
        let mut look = (None, None);
        for id in attr(&attrs, "style").unwrap_or_default().split_whitespace() {
            if let Some(style) = styles.get(id) {
                look = (style.0.or(look.0), style.1.or(look.1));
            }
        }
        if let Some(v) = attr(&attrs, "fontStyle") {
            look.0 = Some(matches!(v.trim(), "italic" | "oblique"));
        }
        if let Some(v) = attr(&attrs, "fontWeight") {
            look.1 = Some(v.trim() == "bold");
        }
        if name == "style" {
            if let Some(id) = attr(&attrs, "id") {
                styles.insert(id.to_string(), look);
            }
        }
        // Times count from the parent's begin; an end or a duration never goes past the parent's end.
        let time = |key: &str| attr(&attrs, key).and_then(|v| rates.time(v));
        let begin = parent.begin + time("begin").unwrap_or(0.0);
        let mut end = time("end").map(|e| parent.begin + e).or(parent.end);
        if let Some(duration) = time("dur") {
            end = Some(end.map_or(begin + duration, |e| e.min(begin + duration)));
        }
        if let (Some(e), Some(limit)) = (end, parent.end) {
            end = Some(e.min(limit));
        }
        let image = !parent.skip && (attr(&attrs, "backgroundImage").is_some() || name == "image");
        images += image as usize;
        let scope = Scope {
            begin,
            end,
            italic: look.0.unwrap_or(parent.italic),
            bold: look.1.unwrap_or(parent.bold),
            skip: parent.skip || image || matches!(name.as_str(), "head" | "metadata"),
            paragraph: parent.paragraph || name == "p",
        };
        stack.push(scope);
        if name == "br" {
            add_text(&stack, &mut pieces, None);
        }
        if empty && close(&mut stack, &mut pieces, &mut out) {
            break;
        }
    }
    Ok((out, images))
}

/// Adds `text` (None: a line break) to the paragraph being read, when the innermost open element
/// shows it.
fn add_text(stack: &[Scope], pieces: &mut Vec<Piece>, text: Option<String>) {
    let Some(scope) = stack.last().filter(|s| s.paragraph && !s.skip) else { return };
    if pieces.len() < MAX_PIECES {
        pieces.push(Piece { begin: scope.begin, end: scope.end, text, italic: scope.italic, bold: scope.bold });
    }
}

/// Closes the innermost open element; a paragraph's end turns its pieces into timed text, one per
/// stretch in which the same pieces show. True once the document's first element is closed.
fn close(stack: &mut Vec<Scope>, pieces: &mut Vec<Piece>, out: &mut Vec<Timed>) -> bool {
    let Some(scope) = stack.pop() else { return true };
    if scope.paragraph && !stack.last().is_some_and(|s| s.paragraph) {
        let mut cuts: Vec<f64> = pieces.iter().flat_map(|p| [p.begin, p.end.unwrap_or(f64::INFINITY)]).collect();
        cuts.sort_by(f64::total_cmp);
        cuts.dedup();
        for pair in cuts.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            let text = styled(pieces.iter().filter(|p| p.begin <= from && p.end.is_none_or(|e| e >= to)));
            if !text.is_empty() {
                out.push(Timed { begin: from, end: to.is_finite().then_some(to), text });
            }
        }
        pieces.clear();
    }
    stack.is_empty()
}

/// The text of `pieces` with white space run together as TTML shows it, a `<br/>` as a line
/// break, and `<b>`/`<i>` around bold and italic runs.
fn styled<'a>(pieces: impl Iterator<Item = &'a Piece>) -> String {
    let mut out = String::new();
    let (mut italic, mut bold, mut space) = (false, false, false);
    for piece in pieces {
        let Some(text) = &piece.text else {
            out.push('\n');
            space = false;
            continue;
        };
        for c in text.chars() {
            if c.is_whitespace() {
                space = true;
                continue;
            }
            if space && !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            space = false;
            if (italic, bold) != (piece.italic, piece.bold) {
                out.push_str(match (italic, bold) {
                    (true, true) => "</i></b>",
                    (true, false) => "</i>",
                    (false, true) => "</b>",
                    (false, false) => "",
                });
                (italic, bold) = (piece.italic, piece.bold);
                out.push_str(if bold { "<b>" } else { "" });
                out.push_str(if italic { "<i>" } else { "" });
            }
            out.push(c);
        }
    }
    out.truncate(out.trim_end().len());
    out.push_str(if italic { "</i>" } else { "" });
    out.push_str(if bold { "</b>" } else { "" });
    tidy(&out)
}

// ---------------------------------------------------------------------------------------------
// Fragmented MP4

/// The subtitles of a fragmented MP4 file (`stpp`: a TTML document in each sample; `wvtt`:
/// WebVTT boxes), given as its init segment followed by its media segments, and how many images
/// its TTML had (left out). Each sample is timed by its fragment's `tfdt` and the sample
/// durations of its `trun` (or the `tfhd`/`trex` defaults) in the track's `mdhd` timescale. A
/// TTML sample's own times may be on the media's clock or count from the sample's start: the one
/// that puts its text inside the sample is taken. Cues of following samples are not joined:
/// that is [`join`]'s work.
pub fn parse_mp4(data: &[u8]) -> Result<(Vec<Cue>, usize), String> {
    let track = SubtitleTrack::read(data).ok_or("no stpp or wvtt subtitle track in the MP4 file")?;
    let (mut cues, mut images) = (Vec::new(), 0);
    let top = boxes(data);
    let mut next_time = 0u64;
    // Each sample read costs its size and at least 1, so samples claimed without their bytes
    // (or reusing the same bytes) cannot keep the reading going.
    let mut budget = data.len();
    for (i, (kind, moof_start, moof)) in top.iter().enumerate() {
        if kind != b"moof" {
            continue;
        }
        let moof = &data[moof.clone()];
        // Where sample data starts when a `trun` does not say: the `mdat` after this `moof`.
        let mut cursor = top[i + 1..].iter().find(|b| &b.0 == b"mdat").map(|b| b.2.start);
        for (_, _, traf) in boxes(moof).into_iter().filter(|b| &b.0 == b"traf") {
            let traf = &moof[traf];
            let Some(tfhd) = child(traf, b"tfhd") else { continue };
            let Some(flags) = be(tfhd, 1, 3) else { continue };
            if be(tfhd, 4, 4) != Some(track.id as u64) {
                continue;
            }
            let mut at = 8;
            let mut optional = |on: bool, len: usize| {
                let value = if on { be(tfhd, at, len) } else { None };
                at += if on { len } else { 0 };
                value
            };
            // shortcut: a base-data-offset is taken as counting from the start of `data`, which holds when the
            // init and the segments are given whole, as they are on the server.
            let base = optional(flags & 0x1 != 0, 8).unwrap_or(*moof_start as u64);
            optional(flags & 0x2 != 0, 4);
            let duration = optional(flags & 0x8 != 0, 4).unwrap_or(track.duration);
            let size = optional(flags & 0x10 != 0, 4).unwrap_or(track.size);
            let tfdt = child(traf, b"tfdt").and_then(|t| if t.first() == Some(&1) { be(t, 4, 8) } else { be(t, 4, 4) });
            let mut time = tfdt.unwrap_or(next_time);
            for (_, _, trun) in boxes(traf).into_iter().filter(|b| &b.0 == b"trun") {
                let trun = &traf[trun];
                let (Some(&version), Some(flags), Some(count)) = (trun.first(), be(trun, 1, 3), be(trun, 4, 4)) else { continue };
                let mut at = 8;
                let mut field = |bit: u64| -> Result<Option<u64>, ()> {
                    if flags & bit == 0 {
                        return Ok(None);
                    }
                    let value = be(trun, at, 4).ok_or(())?;
                    at += 4;
                    Ok(Some(value))
                };
                if let Ok(Some(offset)) = field(0x1) {
                    cursor = base.checked_add_signed(offset as u32 as i32 as i64).map(|p| p as usize);
                }
                let _ = field(0x4);
                for _ in 0..count {
                    let (Ok(sample_duration), Ok(sample_size), Ok(_), Ok(offset)) = (field(0x100), field(0x200), field(0x400), field(0x800)) else {
                        break;
                    };
                    let sample_duration = sample_duration.unwrap_or(duration);
                    let Some(pos) = cursor else { break };
                    let end = pos.checked_add(sample_size.unwrap_or(size) as usize);
                    let Some(sample) = end.and_then(|end| data.get(pos..end)) else { break };
                    let Some(left) = budget.checked_sub(sample.len().max(1)) else { break };
                    budget = left;
                    cursor = end;
                    let offset = offset.map_or(0, |o| if version == 0 { o as i128 } else { o as u32 as i32 as i128 });
                    let from = time as i128 + offset;
                    let (from, to) = (ticks_ms(from, track.timescale), ticks_ms(from + sample_duration as i128, track.timescale));
                    if track.wvtt {
                        cues.extend(wvtt_cues(sample, from, to));
                    } else {
                        images += stpp_cues(sample, from, to, &mut cues);
                    }
                    time = time.saturating_add(sample_duration);
                }
            }
            next_time = time;
        }
    }
    Ok((cues, images))
}

/// Milliseconds of `ticks` at `timescale` per second, rounded; never below 0.
fn ticks_ms(ticks: i128, timescale: u64) -> i64 {
    let timescale = timescale as i128;
    ((ticks * 1000 + timescale / 2) / timescale).clamp(0, i64::MAX as i128) as i64
}

/// The cues of a `wvtt` sample shown from `from` to `to` (ms): one per `vttc` box, from its
/// `payl` text; a `vtte` box (no cue) has none.
fn wvtt_cues(sample: &[u8], from: i64, to: i64) -> Vec<Cue> {
    let cue_boxes = boxes(sample).into_iter().filter(|b| &b.0 == b"vttc");
    cue_boxes.filter_map(|b| cue(from, to, clean_vtt_text(&String::from_utf8_lossy(child(&sample[b.2], b"payl")?)))).collect()
}

/// Adds the cues of an `stpp` sample shown from `from` to `to` (ms) to `cues`, and gives how
/// many images it has. Text untimed in the document shows for the whole sample.
fn stpp_cues(sample: &[u8], from: i64, to: i64, cues: &mut Vec<Cue>) -> usize {
    let length = sample.iter().rposition(|&b| b != 0).map_or(0, |n| n + 1);
    let Ok((timed, images)) = ttml_cues(&String::from_utf8_lossy(&sample[..length])) else { return 0 };
    let spans: Vec<(i64, Option<i64>, String)> = timed.into_iter().map(|t| (secs_ms(t.begin), t.end.map(secs_ms), t.text)).collect();
    // How long the text shows inside the sample when its times are moved by `shift`.
    let inside = |shift: i64| -> i64 { spans.iter().map(|(b, e, _)| (e.map_or(to, |e| e + shift).min(to) - (b + shift).max(from)).max(0)).sum() };
    // A tie (the sample starts at 0, or the text is untimed) counts from the sample's start.
    let shift = if inside(from) >= inside(0) { from } else { 0 };
    cues.extend(spans.into_iter().filter_map(|(b, e, text)| cue(b + shift, e.map_or(to, |e| e + shift), text)));
    images
}

/// The subtitle track of an MP4 init segment and its sample defaults.
struct SubtitleTrack {
    id: u32,
    timescale: u64,
    wvtt: bool,
    duration: u64,
    size: u64,
}

impl SubtitleTrack {
    /// The first `stpp` or `wvtt` track in `data`'s `moov`, with its `trex` defaults.
    fn read(data: &[u8]) -> Option<SubtitleTrack> {
        let moov = child(data, b"moov")?;
        let (id, timescale, wvtt) = boxes(moov).into_iter().filter(|b| &b.0 == b"trak").find_map(|b| {
            let trak = &moov[b.2];
            let tkhd = child(trak, b"tkhd")?;
            let id = be(tkhd, if *tkhd.first()? == 1 { 20 } else { 12 }, 4)? as u32;
            let mdia = child(trak, b"mdia")?;
            let mdhd = child(mdia, b"mdhd")?;
            let timescale = be(mdhd, if *mdhd.first()? == 1 { 20 } else { 12 }, 4)?;
            let stsd = [b"minf", b"stbl", b"stsd"].iter().try_fold(mdia, |d, kind| child(d, kind))?;
            let wvtt = match &boxes(stsd.get(8..)?).first()?.0 {
                b"wvtt" => true,
                b"stpp" => false,
                _ => return None,
            };
            (timescale > 0).then_some((id, timescale, wvtt))
        })?;
        let trex = child(moov, b"mvex").into_iter().flat_map(|mvex| boxes(mvex).into_iter().map(move |b| (b.0, &mvex[b.2])));
        let defaults = trex.filter(|(kind, _)| kind == b"trex").map(|(_, t)| t).find(|t| be(t, 4, 4) == Some(id as u64));
        let default = |at| defaults.and_then(|t| be(t, at, 4)).unwrap_or(0);
        Some(SubtitleTrack { id, timescale, wvtt, duration: default(12), size: default(16) })
    }
}

/// Where a fragmented MP4 starts on its first track's clock, in ms: its first fragment's `tfdt` at
/// that track's `mdhd` timescale. `data` is its start, the init segment and a first fragment.
pub fn mp4_start(data: &[u8]) -> Option<u64> {
    let trak = child(child(data, b"moov")?, b"trak")?;
    let tkhd = child(trak, b"tkhd")?;
    let id = be(tkhd, if *tkhd.first()? == 1 { 20 } else { 12 }, 4)?;
    let mdhd = child(child(trak, b"mdia")?, b"mdhd")?;
    let timescale = be(mdhd, if *mdhd.first()? == 1 { 20 } else { 12 }, 4).filter(|&t| t > 0)?;
    let moof = child(data, b"moof")?;
    let mut trafs = boxes(moof).into_iter().filter(|b| &b.0 == b"traf").map(|b| &moof[b.2]);
    let traf = trafs.find(|traf| child(traf, b"tfhd").and_then(|tfhd| be(tfhd, 4, 4)) == Some(id))?;
    let tfdt = child(traf, b"tfdt")?;
    let time = if tfdt.first() == Some(&1) { be(tfdt, 4, 8) } else { be(tfdt, 4, 4) }?;
    Some(ticks_ms(time as i128, timescale) as u64)
}

/// The boxes laid one after another in `data`: type, where the box starts and where its contents
/// are. A box that runs past the end stops the list.
fn boxes(data: &[u8]) -> Vec<([u8; 4], usize, Range<usize>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let (Some(size), Some(kind)) = (be(data, at, 4), data.get(at + 4..at + 8)) {
        let (header, size) = match size {
            0 => (8, (data.len() - at) as u64),
            1 => match be(data, at + 8, 8) {
                Some(size) => (16, size),
                None => break,
            },
            size => (8, size),
        };
        if size < header as u64 || size > (data.len() - at) as u64 {
            break;
        }
        let size = size as usize;
        out.push(([kind[0], kind[1], kind[2], kind[3]], at, at + header..at + size));
        at += size;
    }
    out
}

/// The contents of the first box of type `kind` in `data`.
fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).into_iter().find(|b| &b.0 == kind).map(|b| &data[b.2])
}

/// The big-endian number in the `len` bytes at `at` of `data`.
fn be(data: &[u8], at: usize, len: usize) -> Option<u64> {
    data.get(at..at.checked_add(len)?).map(|b| b.iter().fold(0, |n, &x| n << 8 | x as u64))
}

// ---------------------------------------------------------------------------------------------
// Joining, moving and writing

/// `cues` (of many segments, put together) in order of start, with a cue repeated in a later
/// segment kept once and a cue cut in two by a segment boundary (the same text going on where it
/// ended) made whole again.
pub fn join(mut cues: Vec<Cue>) -> Vec<Cue> {
    cues.sort_by_key(|c| c.start);
    let mut out: Vec<Cue> = Vec::with_capacity(cues.len());
    // The last cue of each text.
    let mut last: HashMap<String, usize> = HashMap::new();
    for cue in cues {
        if let Some(&i) = last.get(&cue.text) {
            if out[i].end + JOIN_GAP_MS >= cue.start {
                out[i].end = out[i].end.max(cue.end);
                continue;
            }
        }
        last.insert(cue.text.clone(), out.len());
        out.push(cue);
    }
    out
}

/// `cues` moved by `by_ms` (negative: earlier, as by the media's start so the text lines up with
/// a file that starts at 0). A cue that would end by 0 is left out; one that would start before 0
/// starts at 0.
pub fn shift(cues: Vec<Cue>, by_ms: i64) -> Vec<Cue> {
    let moved = |t: u64| t.saturating_add_signed(by_ms);
    cues.into_iter().filter(|c| moved(c.end) > 0).map(|c| Cue { start: moved(c.start), end: moved(c.end), text: c.text }).collect()
}

/// `cues` as an SRT file.
pub fn to_srt(cues: &[Cue]) -> String {
    let blocks = shown(cues).enumerate();
    blocks.map(|(n, (c, text))| format!("{}\n{} --> {}\n{}\n\n", n + 1, stamp(c.start, ','), stamp(c.end, ','), text)).collect()
}

/// `cues` as a WebVTT file.
pub fn to_vtt(cues: &[Cue]) -> String {
    let blocks = shown(cues).map(|(c, text)| format!("\n{} --> {}\n{}\n", stamp(c.start, '.'), stamp(c.end, '.'), escape_vtt(&text)));
    std::iter::once("WEBVTT\n".to_string()).chain(blocks).collect()
}

/// The cues that have text, each with that text tidied (a blank line would end its block).
fn shown(cues: &[Cue]) -> impl Iterator<Item = (&Cue, String)> {
    cues.iter().map(|c| (c, tidy(&c.text))).filter(|(_, text)| !text.is_empty())
}

/// `HH:MM:SS<sep>mmm` of `ms` milliseconds.
fn stamp(ms: u64, sep: char) -> String {
    format!("{:02}:{:02}:{:02}{}{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, sep, ms % 1000)
}

/// Cue text for WebVTT: `&`, `<` and `>` escaped except in its `<i>`, `<b>` and `<u>` tags.
fn escape_vtt(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some(tag) = TAGS.iter().find(|t| rest.starts_with(**t)) {
            out.push_str(tag);
            rest = &rest[tag.len()..];
            continue;
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn c(start: u64, end: u64, text: &str) -> Cue {
        Cue { start, end, text: text.to_string() }
    }

    // ----- WebVTT

    /// Settings, notes, styles, regions, identifiers and unknown tags are left out; the
    /// timestamp map moves every cue onto the media's clock.
    #[test]
    fn vtt_cues_are_read_and_mapped() {
        let vtt = "\u{feff}WEBVTT - title\r\nX-TIMESTAMP-MAP=LOCAL:00:00:01.000,MPEGTS:900000\r\n\r\n\
                   NOTE a note\r\nspanning lines\r\n\r\nSTYLE\r\n::cue { color: red }\r\n\r\nREGION\r\nid:r1\r\n\r\n\
                   intro\r\n00:00:01.000 --> 00:00:02.500 align:start line:0\r\n<v Bob>Hi &amp; <i.loud>bye</i>\r\n\
                   <c.yellow>second</c> line&nbsp;&#x21;\r\n\r\n02:03.5 --> 02:04.000\r\nshort form\r\n\r\n\
                   00:05.000 --> nonsense\r\nbad timing\r\n\r\n00:06.000 --> 00:07.000\r\n\r\n";
        assert_eq!(
            parse_vtt(vtt),
            [c(10_000, 11_500, "Hi & <i>bye</i>\nsecond line\u{a0}!"), c(132_500, 133_000, "short form")]
        );
    }

    #[test]
    fn vtt_without_a_map_keeps_its_times() {
        let vtt = "WEBVTT\n\n1\n01:00:00.000 --> 01:00:01.000\n<b>one</b> <u>two</u> <ruby>x<rt>y</rt></ruby> <00:00:00.500>a < b\n";
        assert_eq!(parse_vtt(vtt), [c(3_600_000, 3_601_000, "<b>one</b> <u>two</u> xy a < b")]);
        // A map before LOCAL leaves cues there at 0 rather than below it.
        let early = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:05.000\n\n00:00:04.000 --> 00:00:06.000\nx\n";
        assert_eq!(parse_vtt(early), [c(0, 1000, "x")]);
    }

    // ----- TTML

    /// Prefixed namespaces, a style by reference and inline, `<br/>`, nested spans and head
    /// content that is not shown.
    #[test]
    fn ttml_text_styles_and_prefixes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<tt:tt xmlns:tt="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling" xml:lang="en">
  <tt:head><tt:styling><tt:style xml:id="it" tts:fontStyle="italic"/></tt:styling>
  <tt:metadata><tt:title>not shown</tt:title></tt:metadata></tt:head>
  <tt:body><tt:div>
    <tt:p begin="00:00:01.000" end="00:00:02.500">Hello<tt:br/>
      <tt:span style="it">big <tt:span tts:fontWeight="bold">world</tt:span></tt:span> &amp; more</tt:p>
    <tt:p begin="00:00:03.000" end="00:00:04.000"><![CDATA[a < b]]></tt:p>
  </tt:div></tt:body>
</tt:tt>"#;
        let (cues, images) = parse_ttml(xml).unwrap();
        assert_eq!(cues, [c(1000, 2500, "Hello\n<i>big </i><b><i>world </i></b>& more"), c(3000, 4000, "a < b")]);
        assert_eq!(images, 0);
        // A line break in the document's source is only white space.
        let source_break = "<tt><body><p begin=\"0s\" end=\"1s\">a<span>b</span>\n</p><p begin=\"1s\" end=\"2s\">c\nd</p></body></tt>";
        assert_eq!(parse_ttml(source_break).unwrap().0, [c(0, 1000, "ab"), c(1000, 2000, "c d")]);
    }

    #[test]
    fn ttml_frame_tick_and_offset_times() {
        let frames = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttp="http://www.w3.org/ns/ttml#parameter"
            ttp:frameRate="25" ttp:frameRateMultiplier="1000 1001" ttp:subFrameRate="2"><body>
            <p begin="00:00:01:12" end="00:00:02:00">frames</p><p begin="50f" end="00:00:03:01.1">more</p></body></tt>"#;
        // A frame at 25 × 1000/1001 fps is 40.04 ms: 12 of them 480.48 ms, 50 of them 2002 ms,
        // and one frame and one of its two sub-frames 60.06 ms.
        assert_eq!(parse_ttml(frames).unwrap().0, [c(1480, 2000, "frames"), c(2002, 3060, "more")]);
        let offsets = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttp="http://www.w3.org/ns/ttml#parameter" ttp:tickRate="10000000">
            <body><div><p begin="15000000t" end="25000000t">ticks</p><p begin="12.3s" dur="500ms">seconds</p>
            <p begin="2h" dur="1m">hours</p><p begin="1s" end="wallclock(2020-01-01)">no end</p></div></body></tt>"#;
        assert_eq!(parse_ttml(offsets).unwrap().0, [c(1500, 2500, "ticks"), c(12_300, 12_800, "seconds"), c(7_200_000, 7_260_000, "hours")]);
        // Without a tick rate but with a frame rate, a tick is a frame.
        let ticks = r#"<tt ttp:frameRate="25" xmlns:ttp="p"><body><p begin="50t" end="75t">x</p></body></tt>"#;
        assert_eq!(parse_ttml(ticks).unwrap().0, [c(2000, 3000, "x")]);
    }

    /// Times count from the parent's begin and never go past its end.
    #[test]
    fn ttml_timing_is_inherited() {
        let xml = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body begin="10s"><div begin="1s" end="5s">
            <p begin="1s" dur="2s">a</p><p begin="3s" end="10s">b</p><p>c</p></div></body></tt>"#;
        assert_eq!(parse_ttml(xml).unwrap().0, [c(12_000, 14_000, "a"), c(14_000, 15_000, "b"), c(11_000, 15_000, "c")]);
    }

    /// Text under a timed span shows only while it does.
    #[test]
    fn ttml_timed_spans_split_a_paragraph() {
        let xml = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><p begin="0s" end="4s">Always <span begin="1s" end="2s">sometimes</span></p></body></tt>"#;
        assert_eq!(parse_ttml(xml).unwrap().0, [c(0, 1000, "Always"), c(1000, 2000, "Always sometimes"), c(2000, 4000, "Always")]);
    }

    #[test]
    fn ttml_images_are_counted_and_skipped() {
        let xml = r##"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:smpte="http://www.smpte-ra.org/schemas/2052-1/2010/smpte-tt">
            <head><metadata><smpte:image xml:id="img0" imagetype="PNG">iVBORw0K</smpte:image></metadata></head>
            <body><div smpte:backgroundImage="#img0" begin="0s" end="1s"><p>alt</p></div>
            <div><image src="#img1" begin="1s" end="2s"/><p begin="1s" end="2s">text</p></div></body></tt>"##;
        assert_eq!(parse_ttml(xml).unwrap(), (vec![c(1000, 2000, "text")], 2));
    }

    #[test]
    fn bad_ttml_is_an_error() {
        assert!(parse_ttml("<tt><body><p begin='1s' end='2s'>x</div></body></tt>").is_err());
        assert_eq!(parse_ttml("").unwrap(), (vec![], 0));
    }

    // ----- MP4

    pub(crate) fn bx(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
        let body = parts.concat();
        [&((body.len() + 8) as u32).to_be_bytes()[..], kind, &body].concat()
    }

    fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        bx(kind, &[&[version], &flags.to_be_bytes()[1..], body])
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    pub(crate) fn trak(id: u32, codec: &[u8; 4], timescale: u32) -> Vec<u8> {
        let tkhd = full(b"tkhd", 0, 3, &[&words(&[0, 0, id])[..], &[0; 68]].concat());
        let mdhd = full(b"mdhd", 0, 0, &words(&[0, 0, timescale, 0, 0]));
        let stsd = full(b"stsd", 0, 0, &[&words(&[1])[..], &bx(codec, &[&[0; 8]])].concat());
        bx(b"trak", &[&tkhd, &bx(b"mdia", &[&mdhd, &bx(b"minf", &[&bx(b"stbl", &[&stsd])])])])
    }

    /// An init segment with a video track (1) and a subtitle track (7) of `codec`, whose `trex`
    /// gives samples 1000 ticks.
    pub(crate) fn init(codec: &[u8; 4], timescale: u32) -> Vec<u8> {
        let mvex = bx(b"mvex", &[&full(b"trex", 0, 0, &words(&[1, 1, 3000, 0, 0])), &full(b"trex", 0, 0, &words(&[7, 1, 1000, 0, 0]))]);
        [bx(b"ftyp", &[b"iso6"]), bx(b"moov", &[&trak(1, b"avc1", 90_000), &trak(7, codec, timescale), &mvex])].concat()
    }

    pub(crate) enum Durations {
        Trun,
        Tfhd(u32),
        Trex,
    }

    /// A `moof` + `mdat` of track 7 with `samples` (duration, data) from `time` (no `tfdt` when
    /// None); the `trun` gives the data's offset when `offset`.
    pub(crate) fn fragment(time: Option<u64>, samples: &[(u32, Vec<u8>)], durations: Durations, offset: bool) -> Vec<u8> {
        let tfhd = match durations {
            Durations::Tfhd(d) => full(b"tfhd", 0, 0x020008, &words(&[7, d])),
            _ => full(b"tfhd", 0, 0x020000, &words(&[7])),
        };
        let tfdt = time.map(|t| full(b"tfdt", 1, 0, &t.to_be_bytes())).unwrap_or_default();
        let in_trun = matches!(durations, Durations::Trun);
        let flags = 0x200 | if in_trun { 0x100 } else { 0 } | if offset { 1 } else { 0 };
        let entries: Vec<u32> = samples.iter().flat_map(|(d, s)| [*d, s.len() as u32].into_iter().skip(!in_trun as usize)).collect();
        let moof = |data_offset: u32| {
            let head = if offset { vec![samples.len() as u32, data_offset] } else { vec![samples.len() as u32] };
            let trun = full(b"trun", 0, flags, &words(&[head, entries.clone()].concat()));
            bx(b"moof", &[&full(b"mfhd", 0, 0, &words(&[1])), &bx(b"traf", &[&tfhd, &tfdt, &trun])])
        };
        let size = moof(0).len() as u32;
        let data: Vec<u8> = samples.iter().flat_map(|(_, s)| s.clone()).collect();
        [moof(size + 8), bx(b"mdat", &[&data])].concat()
    }

    pub(crate) fn vttc(text: &str) -> Vec<u8> {
        bx(b"vttc", &[&bx(b"iden", &[b"7"]), &bx(b"sttg", &[b"align:start"]), &bx(b"payl", &[text.as_bytes()])])
    }

    #[test]
    fn wvtt_samples_are_timed_by_their_fragment() {
        let data = [
            init(b"wvtt", 1000),
            fragment(Some(10_000), &[(2000, [vttc("<v Bob>Hi &amp; <i>bye</i>"), vttc("second cue")].concat()), (1000, bx(b"vtte", &[]))], Durations::Trun, true),
            fragment(Some(13_000), &[(0, vttc("later"))], Durations::Tfhd(1500), false),
            // No tfdt: it follows the fragment before; durations from the trex.
            fragment(None, &[(0, vttc("last"))], Durations::Trex, true),
        ]
        .concat();
        assert_eq!(
            parse_mp4(&data).unwrap(),
            (vec![c(10_000, 12_000, "Hi & <i>bye</i>"), c(10_000, 12_000, "second cue"), c(13_000, 14_500, "later"), c(14_500, 15_500, "last")], 0)
        );
    }

    #[test]
    fn wvtt_cue_across_fragments_is_joined() {
        let data = [
            init(b"wvtt", 1000),
            fragment(Some(0), &[(0, vttc("same"))], Durations::Trex, true),
            fragment(Some(1000), &[(0, vttc("same"))], Durations::Trex, true),
        ]
        .concat();
        let (cues, _) = parse_mp4(&data).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(join(cues), [c(0, 2000, "same")]);
    }

    fn ttml(body: &str) -> Vec<u8> {
        format!(r#"<?xml version="1.0"?><tt xmlns="http://www.w3.org/ns/ttml" xmlns:smpte="s"><body><div>{body}</div></body></tt>"#).into_bytes()
    }

    /// An stpp sample's times may count from the sample's start or be on the media's clock.
    #[test]
    fn stpp_relative_and_absolute_times() {
        let relative = [
            init(b"stpp", 1000),
            fragment(Some(10_000), &[(2000, ttml(r#"<p begin="00:00:00.500" end="00:00:01.500">rel</p>"#))], Durations::Trun, true),
        ]
        .concat();
        assert_eq!(parse_mp4(&relative).unwrap().0, [c(10_500, 11_500, "rel")]);
        let absolute = [
            init(b"stpp", 10_000_000),
            fragment(
                Some(100_000_000),
                &[(20_000_000, ttml(r#"<p begin="00:00:09.000" end="00:00:10.800">carried</p><p begin="10.5s" end="11.5s">abs</p>"#))],
                Durations::Trun,
                true,
            ),
        ]
        .concat();
        assert_eq!(parse_mp4(&absolute).unwrap().0, [c(9000, 10_800, "carried"), c(10_500, 11_500, "abs")]);
        // Near the start both readings overlap the sample: the one that fits it better wins.
        let early = [
            init(b"stpp", 1000),
            fragment(Some(1000), &[(2000, ttml(r#"<p begin="1s" end="2.4s">abs</p>"#))], Durations::Trun, true),
            fragment(Some(3000), &[(2000, ttml(r#"<p begin="0s" end="2s">rel</p>"#))], Durations::Trun, true),
        ]
        .concat();
        assert_eq!(parse_mp4(&early).unwrap().0, [c(1000, 2400, "abs"), c(3000, 5000, "rel")]);
    }

    /// Untimed text shows for the whole sample; an image and the image data after the document
    /// are left out.
    #[test]
    fn stpp_untimed_text_and_images() {
        let sample = [ttml(r##"<div smpte:backgroundImage="#i"/><p>untimed</p>"##), b"\x89PNG\r\n\x1a\n\0\0<<garbage".to_vec(), vec![0; 4]].concat();
        let data = [init(b"stpp", 1000), fragment(Some(20_000), &[(2000, sample)], Durations::Trun, true)].concat();
        assert_eq!(parse_mp4(&data).unwrap(), (vec![c(20_000, 22_000, "untimed")], 1));
    }

    #[test]
    fn mp4_without_subtitles_is_an_error() {
        let video = [bx(b"ftyp", &[b"iso6"]), bx(b"moov", &[&trak(1, b"avc1", 90_000)])].concat();
        assert!(parse_mp4(&video).is_err());
        assert!(parse_mp4(b"").is_err());
    }

    /// Cut short or damaged anywhere, a file never panics (and boxes claiming more than there is
    /// stop the reading).
    #[test]
    fn broken_mp4_does_not_panic() {
        let data = [
            init(b"wvtt", 1000),
            fragment(Some(0), &[(1000, vttc("a")), (1000, vttc("b"))], Durations::Trun, true),
        ]
        .concat();
        let stpp = [init(b"stpp", 1000), fragment(Some(0), &[(1000, ttml("<p>x</p>"))], Durations::Tfhd(500), false)].concat();
        for data in [data, stpp] {
            for len in 0..=data.len() {
                let _ = parse_mp4(&data[..len]);
            }
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            for _ in 0..2000 {
                let mut broken = data.clone();
                for _ in 0..3 {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let at = seed as usize % broken.len();
                    broken[at] = (seed >> 32) as u8;
                }
                let _ = parse_mp4(&broken);
            }
        }
        // A trun that claims four billion samples without the data for them: sizes in the trun
        // stop at its end; empty samples by default stop when the file's bytes are spent.
        let mut huge = [init(b"wvtt", 1000), fragment(Some(0), &[(1000, vttc("a"))], Durations::Trex, true)].concat();
        let at = huge.windows(4).position(|w| w == b"trun").unwrap() + 4;
        huge[at + 4..at + 8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(parse_mp4(&huge).unwrap().0, [c(0, 1000, "a")]);
        huge[at + 1..at + 4].copy_from_slice(&[0, 0, 1]);
        assert_eq!(parse_mp4(&huge).unwrap(), (vec![], 0));
    }

    /// A fragmented MP4 starts at its first fragment's `tfdt`, on its first track's clock; one whose
    /// first fragment is another track's tells nothing.
    #[test]
    fn mp4_start_is_the_first_fragments_decode_time() {
        let moov = |codec: &[u8; 4]| [bx(b"ftyp", &[b"iso6"]), bx(b"moov", &[&trak(7, codec, 1000)])].concat();
        let data = [moov(b"wvtt"), fragment(Some(5_250), &[(1000, vttc("x"))], Durations::Trun, true)].concat();
        assert_eq!(mp4_start(&data), Some(5_250));
        assert_eq!(mp4_start(&[init(b"wvtt", 1000), fragment(Some(5_250), &[(1000, vttc("x"))], Durations::Trun, true)].concat()), None);
        assert_eq!(mp4_start(&moov(b"stpp")), None);
    }

    // ----- Joining, moving, writing

    #[test]
    fn join_merges_repeats_and_splits_only() {
        let first = vec![c(1000, 2000, "a"), c(1500, 4000, "b")];
        let second = vec![c(1000, 2000, "a"), c(4000, 6000, "b"), c(4000, 5000, "c"), c(7000, 8000, "a"), c(8001, 9000, "a")];
        assert_eq!(
            join([first, second].concat()),
            [c(1000, 2000, "a"), c(1500, 6000, "b"), c(4000, 5000, "c"), c(7000, 9000, "a")]
        );
    }

    #[test]
    fn shift_moves_and_drops_cues_before_zero() {
        let cues = vec![c(500, 1500, "x"), c(2000, 3000, "y"), c(0, 900, "z"), c(0, 1000, "w")];
        assert_eq!(shift(cues.clone(), -1000), [c(0, 500, "x"), c(1000, 2000, "y")]);
        assert_eq!(shift(cues[..1].to_vec(), 250), [c(750, 1750, "x")]);
    }

    #[test]
    fn writers() {
        let cues = [c(0, 1500, "Hi & <i>bye</i>\n\n second"), c(3_723_004, 3_724_000, "  "), c(3_723_004, 3_725_000, "x < y -->")];
        assert_eq!(
            to_srt(&cues),
            "1\n00:00:00,000 --> 00:00:01,500\nHi & <i>bye</i>\nsecond\n\n2\n01:02:03,004 --> 01:02:05,000\nx < y -->\n\n"
        );
        let vtt = to_vtt(&cues);
        assert_eq!(vtt, "WEBVTT\n\n00:00:00.000 --> 00:00:01.500\nHi &amp; <i>bye</i>\nsecond\n\n01:02:03.004 --> 01:02:05.000\nx &lt; y --&gt;\n");
        assert_eq!(parse_vtt(&vtt), [c(0, 1500, "Hi & <i>bye</i>\nsecond"), c(3_723_004, 3_725_000, "x < y -->")]);
        assert_eq!(to_vtt(&[]), "WEBVTT\n");
    }
}
