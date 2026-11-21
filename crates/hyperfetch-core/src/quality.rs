//! The qualities a media link has, read from what yt-dlp lists of it (its `-J` formats), and the
//! choice of one ([`MediaPick`]): kept for a site as a rule ("up to 1080p, AV1 first, German
//! audio"), which each link resolves to the formats it has (see [`Qualities::take`]).

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::engine::size_text;

/// The codecs a pick may put first, as [`MediaPick::codec`] names them.
pub const CODECS: [&str; 4] = ["av1", "vp9", "hevc", "h264"];

/// A size as the listing gives it: the exact one, or the site's estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Size {
    pub bytes: u64,
    pub exact: bool,
}

impl Size {
    /// What `format` lists: its `filesize`, else its `filesize_approx`; None with neither.
    fn of(format: &Value) -> Option<Size> {
        let bytes = |key: &str| format[key].as_f64().filter(|&bytes| bytes >= 1.0).map(|bytes| bytes as u64);
        let exact = bytes("filesize").map(|bytes| Size { bytes, exact: true });
        exact.or_else(|| bytes("filesize_approx").map(|bytes| Size { bytes, exact: false }))
    }

    /// The two together: exact only when both are; None when the other is unknown.
    fn and(self, other: Option<Size>) -> Option<Size> {
        other.map(|other| Size { bytes: self.bytes + other.bytes, exact: self.exact && other.exact })
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.exact {
            true => f.write_str(&size_text(self.bytes)),
            false => write!(f, "about {}", size_text(self.bytes)),
        }
    }
}

/// One way a height comes: the best format of one codec (and range) at that height.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoOption {
    /// yt-dlp's id of the format.
    pub id: String,
    /// "AV1", "VP9", "HEVC", "H.264", else the codec as listed ("" when none is).
    pub codec: String,
    /// Frames a second, when listed.
    pub fps: Option<u32>,
    pub hdr: bool,
    /// The format's own size (see [`Qualities::total`] for its download's).
    pub size: Option<Size>,
    /// It has its own sound, so no audio is merged in.
    pub has_audio: bool,
}

/// A height the video comes in, and its codecs there.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Height {
    /// As sites name it: by the shorter side (1080 for 1920x1080 and for 1080x1920).
    pub p: u32,
    /// AV1, VP9, HEVC, H.264, then any other; SDR before HDR.
    pub options: Vec<VideoOption>,
}

/// An audio format of the video.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioOption {
    pub id: String,
    /// "Opus", "AAC", … (as for video).
    pub codec: String,
    pub kbps: Option<u32>,
    pub size: Option<Size>,
    pub language: Option<String>,
    /// The site marks it as the video's own language (the others are dubbed).
    pub original: bool,
}

/// What a video offers to choose from.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Qualities {
    /// Tallest first.
    pub heights: Vec<Height>,
    /// The original language first, then by bitrate, Opus before AAC at the same.
    pub audio: Vec<AudioOption>,
    /// The audio languages, the original first; empty when there are fewer than two.
    pub languages: Vec<String>,
}

/// Audio alone, as a [`MediaPick`] takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioForm {
    /// As the site has it, not converted.
    Original,
    Mp3,
    M4a,
}

/// A choice of quality: video up to a height, a codec first and HDR or not, or audio alone; in a
/// language. Kept for a site without [`MediaPick::formats`], it is a rule each link resolves
/// (see [`Qualities::take`]). Written as the command line takes it (see [`MediaPick::parse`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaPick {
    /// Audio alone; None for video (with its best audio).
    pub audio: Option<AudioForm>,
    /// The tallest video wanted, by the shorter side; None for the best.
    pub up_to: Option<u32>,
    /// One of [`CODECS`], taken first where a height comes in several.
    pub codec: Option<String>,
    /// HDR first where a height has it; else SDR.
    pub hdr: bool,
    /// The audio language as the site lists it ("de", "en-US"); None for the original.
    pub language: Option<String>,
    /// The formats one link's card showed (yt-dlp ids, "399+251"), taken first; never kept for
    /// a site, as they differ from video to video.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formats: Option<String>,
}

/// "AV1" for "av01.0.08M.08", and so on; else the codec as listed, up to its first dot.
fn codec_name(codec: &str) -> String {
    let c = codec.to_ascii_lowercase();
    let known = [
        (&["av01", "av1"][..], "AV1"),
        (&["vp09", "vp9"], "VP9"),
        (&["hev", "hvc", "h265"], "HEVC"),
        (&["avc", "h264"], "H.264"),
        (&["opus"], "Opus"),
        (&["mp4a", "aac"], "AAC"),
        (&["vorbis"], "Vorbis"),
        (&["mp3"], "MP3"),
        (&["flac"], "FLAC"),
        (&["ec-3", "eac3"], "E-AC-3"),
        (&["ac-3", "ac3"], "AC-3"),
    ];
    match known.iter().find(|(starts, _)| starts.iter().any(|start| c.starts_with(start))) {
        Some((_, name)) => name.to_string(),
        None => codec.split('.').next().unwrap_or_default().to_string(),
    }
}

/// The [`CODECS`] key of a codec [`codec_name`] gave.
pub fn codec_key(name: &str) -> Option<&'static str> {
    match name {
        "AV1" => Some("av1"),
        "VP9" => Some("vp9"),
        "HEVC" => Some("hevc"),
        "H.264" => Some("h264"),
        _ => None,
    }
}

/// A [`CODECS`] key as people write it.
fn codec_display(key: &str) -> &str {
    match key {
        "av1" => "AV1",
        "vp9" => "VP9",
        "hevc" => "HEVC",
        "h264" => "H.264",
        other => other,
    }
}

/// Where a codec goes among a height's options (and a bitrate's audio).
fn codec_rank(name: &str) -> usize {
    ["AV1", "VP9", "HEVC", "H.264", "Opus", "AAC"].iter().position(|known| *known == name).unwrap_or(6)
}

/// The site a pick is kept for: the link's host in lower case without `www.` or `m.`, a
/// youtu.be link YouTube's.
pub fn site(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let host = host.strip_prefix("www.").or_else(|| host.strip_prefix("m.")).unwrap_or(&host);
    match host {
        "youtu.be" | "youtube-nocookie.com" => "youtube.com".to_string(),
        host => host.to_string(),
    }
}

fn valid_language(language: &str) -> bool {
    (1..=20).contains(&language.len()) && language.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_formats(formats: &str) -> bool {
    (1..=100).contains(&formats.len())
        && formats.split('+').all(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.=".contains(&b)))
}

impl Qualities {
    /// What yt-dlp's listing of a video (`found`, its `-J`) offers: its video formats by height
    /// and codec, the best of each kind, and its audio; None when it lists no formats to choose
    /// from (a list, or a site's one file). Locked formats and loudness-squeezed (DRC) audio are
    /// left out, as are formats whose codecs are not said.
    pub fn of(found: &Value) -> Option<Qualities> {
        let mut video: Vec<(u32, VideoOption, f64)> = Vec::new();
        let mut audio = Vec::new();
        for format in found["formats"].as_array()? {
            let text = |key: &str| format[key].as_str().filter(|text| !text.is_empty());
            let Some(id) = text("format_id") else { continue };
            let note = text("format_note").unwrap_or_default().to_ascii_lowercase();
            let drm = !matches!(format.get("has_drm"), None | Some(Value::Null) | Some(Value::Bool(false)));
            if drm || id.ends_with("-drc") || note.contains("drc") {
                continue;
            }
            let tbr = format["tbr"].as_f64().unwrap_or(0.0);
            let side = |key: &str| format[key].as_u64().filter(|&n| n > 0).and_then(|n| u32::try_from(n).ok());
            let p = side("height").map(|height| side("width").map_or(height, |width| width.min(height)));
            let (vcodec, acodec) = (text("vcodec"), text("acodec"));
            match (vcodec, p) {
                (Some("none"), _) | (None, None) => {
                    let Some(codec) = acodec.filter(|codec| *codec != "none") else { continue };
                    let kbps = format["abr"].as_f64().filter(|&abr| abr > 0.0).or(Some(tbr).filter(|&tbr| tbr > 0.0));
                    let language = text("language").map(str::to_string);
                    let original = format["language_preference"].as_i64().is_some_and(|n| n >= 10) || note.contains("original");
                    let codec = codec_name(codec);
                    audio.push(AudioOption { id: id.to_string(), codec, kbps: kbps.map(|k| k.round() as u32), size: Size::of(format), language, original });
                }
                (_, Some(p)) => {
                    let option = VideoOption {
                        id: id.to_string(),
                        codec: vcodec.map(codec_name).unwrap_or_default(),
                        fps: format["fps"].as_f64().filter(|&fps| fps > 0.0).map(|fps| fps.round() as u32),
                        hdr: text("dynamic_range").is_some_and(|range| !range.eq_ignore_ascii_case("SDR")),
                        size: Size::of(format),
                        has_audio: acodec != Some("none"),
                    };
                    // Of one kind: the higher frame rate, then a size said (exact first), then the
                    // higher bitrate.
                    let better = |a: &VideoOption, a_tbr: f64, b: &VideoOption, b_tbr: f64| {
                        let rank = |o: &VideoOption| (o.fps.unwrap_or(0), o.size.map_or(0, |size| 1 + u8::from(size.exact)));
                        rank(a).cmp(&rank(b)).then(a_tbr.total_cmp(&b_tbr)).is_gt()
                    };
                    let kind = |(q, o, _): &&mut (u32, VideoOption, f64)| *q == p && o.codec == option.codec && o.hdr == option.hdr;
                    match video.iter_mut().find(kind) {
                        Some(kept) if better(&option, tbr, &kept.1, kept.2) => *kept = (p, option, tbr),
                        Some(_) => {}
                        None => video.push((p, option, tbr)),
                    }
                }
                _ => {}
            }
        }
        if video.is_empty() && audio.is_empty() {
            return None;
        }
        video.sort_by(|(p, a, _), (q, b, _)| q.cmp(p).then(codec_rank(&a.codec).cmp(&codec_rank(&b.codec))).then(a.hdr.cmp(&b.hdr)));
        let mut heights: Vec<Height> = Vec::new();
        for (p, option, _) in video {
            match heights.last_mut() {
                Some(height) if height.p == p => height.options.push(option),
                _ => heights.push(Height { p, options: vec![option] }),
            }
        }
        audio.sort_by(|a: &AudioOption, b| {
            b.original.cmp(&a.original).then(b.kbps.cmp(&a.kbps)).then(codec_rank(&a.codec).cmp(&codec_rank(&b.codec)))
        });
        let mut languages: Vec<String> = Vec::new();
        for language in audio.iter().filter_map(|a| a.language.clone()) {
            if !languages.contains(&language) {
                languages.push(language);
            }
        }
        if languages.len() < 2 {
            languages.clear();
        }
        Some(Qualities { heights, audio, languages })
    }

    /// The audio a video download takes in `language` (None: the original): the best there is
    /// in it, else the best of all; None with no audio formats.
    pub fn audio_for(&self, language: Option<&str>) -> Option<&AudioOption> {
        let of = |a: &&AudioOption| language.is_none_or(|language| a.language.as_deref() == Some(language));
        self.audio.iter().find(of).or_else(|| self.audio.first())
    }

    /// The best AAC audio in `language`, which M4A keeps as it is; None without one.
    fn aac_for(&self, language: Option<&str>) -> Option<&AudioOption> {
        let of = |a: &&AudioOption| a.codec == "AAC" && language.is_none_or(|language| a.language.as_deref() == Some(language));
        self.audio.iter().find(of)
    }

    /// The size of `option`'s download: with the audio it is merged with in `language`. None
    /// when either size is unknown.
    pub fn total(&self, option: &VideoOption, language: Option<&str>) -> Option<Size> {
        match self.audio_for(language) {
            Some(audio) if !option.has_audio => option.size?.and(audio.size),
            _ => option.size,
        }
    }

    /// What a card says of `option` in `language`: its codec, its frame rate over 30, HDR, and
    /// the size of its download.
    pub fn video_text(&self, option: &VideoOption, language: Option<&str>) -> String {
        let fps = option.fps.filter(|&fps| fps > 30).map(|fps| format!("{fps} fps"));
        let size = self.total(option, language).map(|size| size.to_string());
        let parts = [Some(option.codec.clone()).filter(|codec| !codec.is_empty()), fps, option.hdr.then(|| "HDR".to_string()), size];
        parts.into_iter().flatten().collect::<Vec<_>>().join(" · ")
    }

    /// What a card says of audio alone as `form`, in `language`: the original's codec, bitrate
    /// and size; whether M4A keeps an AAC format as it is or converts; MP3 converted.
    pub fn audio_text(&self, form: AudioForm, language: Option<&str>) -> String {
        let said = |a: &AudioOption| [Some(a.codec.clone()), a.kbps.map(|kbps| format!("{kbps} kbps"))].into_iter().flatten().collect::<Vec<_>>().join(" ");
        let best = self.audio_for(language);
        match form {
            AudioForm::Original => {
                let size = best.and_then(|a| a.size).map(|size| size.to_string());
                [best.map(said), size].into_iter().flatten().collect::<Vec<_>>().join(" · ")
            }
            AudioForm::M4a => match self.aac_for(language) {
                Some(aac) => format!("{}, not converted", said(aac)),
                None => best.map_or_else(|| "converted".to_string(), |a| format!("converted from {}", a.codec)),
            },
            AudioForm::Mp3 => best.map_or_else(|| "converted".to_string(), |a| format!("converted from {}", a.codec)),
        }
    }

    /// What `rule` takes from these, with the formats it is: its height, else the nearest below
    /// (the lowest when none is), its codec and range there where they are, its language where it
    /// is; and what the card says when it is not the rule's own.
    pub fn take(&self, rule: &MediaPick) -> (MediaPick, Option<String>) {
        let mut notes = Vec::new();
        let language = match rule.language.clone() {
            Some(language) if !self.languages.is_empty() && !self.languages.contains(&language) => {
                notes.push(format!("No {language} audio here: the original is taken"));
                None
            }
            language => language.filter(|_| !self.languages.is_empty()),
        };
        let audio_alone = |form: AudioForm| {
            let audio = match form {
                AudioForm::M4a => self.aac_for(language.as_deref()).or_else(|| self.audio_for(language.as_deref())),
                _ => self.audio_for(language.as_deref()),
            };
            MediaPick { audio: Some(form), language: language.clone(), formats: audio.map(|a| a.id.clone()), ..Default::default() }
        };
        let note = |notes: Vec<String>| Some(notes.join("; ")).filter(|note| !note.is_empty());
        if let Some(form) = rule.audio {
            return (audio_alone(form), note(notes));
        }
        let Some(height) = self.heights.iter().find(|h| h.p <= rule.up_to.unwrap_or(u32::MAX)).or(self.heights.last()) else {
            return (audio_alone(AudioForm::Original), note(notes));
        };
        if let Some(wanted) = rule.up_to.filter(|&wanted| wanted != height.p) {
            notes.insert(0, format!("No {wanted}p here: {}p is taken", height.p));
        }
        let codec_is = |o: &&VideoOption| rule.codec.as_deref().is_none_or(|codec| codec_key(&o.codec) == Some(codec));
        let option = (height.options.iter().find(|o| codec_is(o) && o.hdr == rule.hdr))
            .or_else(|| height.options.iter().find(codec_is))
            .or_else(|| height.options.iter().find(|o| o.hdr == rule.hdr))
            .unwrap_or(&height.options[0]);
        if let Some(codec) = rule.codec.as_deref().filter(|&codec| codec_key(&option.codec) != Some(codec)) {
            notes.push(format!("No {} at {}p: {} is taken", codec_display(codec), height.p, option.codec));
        }
        let audio = self.audio_for(language.as_deref()).filter(|_| !option.has_audio);
        let pick = MediaPick {
            up_to: Some(height.p),
            codec: codec_key(&option.codec).map(str::to_string),
            hdr: option.hdr,
            language,
            formats: Some(audio.map_or_else(|| option.id.clone(), |audio| format!("{}+{}", option.id, audio.id))),
            ..Default::default()
        };
        (pick, note(notes))
    }

    /// The qualities as the command line lists them: each with the name `--media-preset` takes.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for height in &self.heights {
            for option in &height.options {
                let pick = MediaPick { up_to: Some(height.p), codec: codec_key(&option.codec).map(str::to_string), hdr: option.hdr, ..Default::default() };
                lines.push(format!("  {:<18}{}", pick.to_string(), self.video_text(option, None)));
            }
        }
        if !self.audio.is_empty() {
            for (name, form) in [("original", AudioForm::Original), ("m4a", AudioForm::M4a), ("mp3", AudioForm::Mp3)] {
                lines.push(format!("  {name:<18}{}", self.audio_text(form, None)));
            }
        }
        if let Some(original) = self.languages.first() {
            lines.push(format!("  Audio in {} (the first is the original): add @{original} to take one", self.languages.join(", ")));
        }
        lines
    }
}

impl MediaPick {
    /// The pick in the words the command line takes: `best` or `<n>p`, then `:av1`, `:vp9`,
    /// `:hevc` or `:h264` and `:hdr`; or `original`, `mp3` or `m4a`; each with `@<language>`
    /// after, as `1080p:av1@de`.
    pub fn parse(text: &str) -> Result<MediaPick, String> {
        let (choice, language) = match text.trim().split_once('@') {
            Some((choice, language)) => (choice, Some(language.to_string())),
            None => (text.trim(), None),
        };
        let mut parts = choice.split(':').map(str::to_ascii_lowercase);
        let first = parts.next().unwrap_or_default();
        let mut pick = MediaPick { language, ..Default::default() };
        match first.as_str() {
            "original" | "audio" => pick.audio = Some(AudioForm::Original),
            "mp3" => pick.audio = Some(AudioForm::Mp3),
            "m4a" => pick.audio = Some(AudioForm::M4a),
            "best" => {}
            height => pick.up_to = Some(height.strip_suffix('p').and_then(|n| n.parse().ok()).ok_or_else(|| format!("{height} is no quality: best, 1080p, original, mp3 or m4a"))?),
        }
        for part in parts {
            let codec = match part.as_str() {
                "hdr" if pick.audio.is_none() => {
                    pick.hdr = true;
                    continue;
                }
                "av1" | "av01" => "av1",
                "vp9" => "vp9",
                "hevc" | "h265" => "hevc",
                "h264" | "avc" => "h264",
                _ => return Err(format!("{part} is no codec: av1, vp9, hevc, h264 or hdr after a video quality")),
            };
            if pick.audio.is_some() {
                return Err(format!("{codec} is a video codec, for a video quality"));
            }
            pick.codec = Some(codec.to_string());
        }
        pick.check()?;
        Ok(pick)
    }

    /// Why the pick may not be taken, if it may not: a height outside 1 to 10000, a codec not
    /// among [`CODECS`], a language or formats that are no code or ids.
    pub fn check(&self) -> Result<(), String> {
        if self.up_to.is_some_and(|n| n == 0 || n > 10_000) {
            return Err("a video quality is 1p to 10000p".to_string());
        }
        if self.codec.as_deref().is_some_and(|codec| !CODECS.contains(&codec)) {
            return Err(format!("the codec must be one of {}", CODECS.join(", ")));
        }
        if self.language.as_deref().is_some_and(|language| !valid_language(language)) {
            return Err("the language must be a code like en or pt-BR".to_string());
        }
        if self.formats.as_deref().is_some_and(|formats| !valid_formats(formats)) {
            return Err("the formats must be ids like 399+251".to_string());
        }
        Ok(())
    }

    /// The rule to keep for a site: the pick without one link's formats.
    pub fn rule(&self) -> MediaPick {
        MediaPick { formats: None, ..self.clone() }
    }

    /// The pick as people read it: "Up to 1080p, AV1 first", "Audio as MP3, in de".
    pub fn label(&self) -> String {
        let what = match self.audio {
            Some(AudioForm::Original) => "Audio as the site has it".to_string(),
            Some(AudioForm::Mp3) => "Audio as MP3".to_string(),
            Some(AudioForm::M4a) => "Audio as M4A".to_string(),
            None => {
                let mut what = self.up_to.map_or_else(|| "The best video".to_string(), |n| format!("Up to {n}p"));
                if let Some(codec) = &self.codec {
                    what += &format!(", {} first", codec_display(codec));
                }
                if self.hdr {
                    what += ", HDR first";
                }
                what
            }
        };
        match &self.language {
            Some(language) => format!("{what}, audio in {language}"),
            None => what,
        }
    }

    /// The yt-dlp arguments of the pick: its formats first, then the rule. Video is merged into
    /// MP4 with `-S` sorting by height first (`res:<n>`: the tallest up to it, else the lowest),
    /// then HDR and the codec; audio alone is taken as it is, or converted to MP3 or M4A (an AAC
    /// format kept as it is). A language or formats that fail [`MediaPick::check`] are left out.
    pub fn to_args(&self) -> Vec<String> {
        let language = self.language.as_deref().filter(|l| valid_language(l)).map_or_else(String::new, |l| format!("[language^={l}]"));
        let formats = self.formats.iter().filter(|f| valid_formats(f)).cloned();
        // Each once, in order.
        let selector = |rest: Vec<String>| {
            let mut alternatives: Vec<String> = Vec::new();
            for alternative in formats.clone().chain(rest) {
                if !alternatives.contains(&alternative) {
                    alternatives.push(alternative);
                }
            }
            alternatives.join("/")
        };
        let mut args = Vec::new();
        match self.audio {
            Some(form) => {
                let aac = form == AudioForm::M4a;
                let wanted = [aac.then(|| format!("ba[acodec^=mp4a]{language}")), Some(format!("ba{language}")), aac.then(|| "ba[acodec^=mp4a]".to_string())];
                let rest = wanted.into_iter().flatten().chain(["ba".to_string(), "b".to_string()]).collect();
                args.extend(["-f".to_string(), selector(rest)]);
                if form != AudioForm::Original {
                    let format = if aac { "m4a" } else { "mp3" };
                    args.extend(["-x", "--audio-format", format].map(String::from));
                }
            }
            None => {
                let sdr = if self.hdr { "" } else { "[dynamic_range=?SDR]" };
                let rest = vec![format!("bv*{sdr}+ba{language}"), format!("bv*{sdr}+ba"), "bv*+ba".to_string(), "b".to_string()];
                args.extend(["-f".to_string(), selector(rest)]);
                let codec = self.codec.as_deref().and_then(|codec| match codec {
                    "av1" => Some("vcodec:av01"),
                    "vp9" => Some("vcodec:vp9"),
                    "hevc" => Some("vcodec:h265"),
                    "h264" => Some("vcodec:h264"),
                    _ => None,
                });
                if self.up_to.is_some() || self.hdr || codec.is_some() {
                    let res = self.up_to.map_or_else(|| "res".to_string(), |n| format!("res:{n}"));
                    let sort = [Some(res.as_str()), self.hdr.then_some("hdr:12"), codec].into_iter().flatten().collect::<Vec<_>>().join(",");
                    args.extend(["-S".to_string(), sort]);
                }
                args.extend(["--merge-output-format", "mp4", crate::media::NO_FASTSTART[0], crate::media::NO_FASTSTART[1]].map(String::from));
            }
        }
        args
    }
}

impl fmt::Display for MediaPick {
    /// As [`MediaPick::parse`] reads it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.audio {
            Some(AudioForm::Original) => f.write_str("original")?,
            Some(AudioForm::Mp3) => f.write_str("mp3")?,
            Some(AudioForm::M4a) => f.write_str("m4a")?,
            None => {
                match self.up_to {
                    Some(n) => write!(f, "{n}p")?,
                    None => f.write_str("best")?,
                }
                if let Some(codec) = &self.codec {
                    write!(f, ":{codec}")?;
                }
                if self.hdr {
                    f.write_str(":hdr")?;
                }
            }
        }
        match &self.language {
            Some(language) => write!(f, "@{language}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A real listing of a 4K film at 60 fps (49 formats: four codecs, HLS copies without sizes,
    /// loudness-squeezed audio), and of a square music video at 25 fps (38).
    fn listing(name: &str) -> Value {
        let text = match name {
            "4k60" => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/media-4k60.json")),
            _ => include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/media-square.json")),
        };
        serde_json::from_str(text).unwrap()
    }

    fn ids(height: &Height) -> Vec<(&str, &str)> {
        height.options.iter().map(|o| (o.codec.as_str(), o.id.as_str())).collect()
    }

    /// Each height once, tallest first, each codec at it once: the format with a size over its
    /// HLS copy without one; the audio without its DRC copies, the best first.
    #[test]
    fn a_listing_gives_its_heights_codecs_and_audio() {
        let q = Qualities::of(&listing("4k60")).unwrap();
        assert_eq!(q.heights.iter().map(|h| h.p).collect::<Vec<_>>(), [2160, 1440, 1080, 720, 480, 360, 240, 144]);
        assert_eq!(ids(&q.heights[0]), [("AV1", "401"), ("VP9", "315")]);
        assert_eq!(ids(&q.heights[2]), [("AV1", "399"), ("VP9", "303"), ("H.264", "299")]);
        let av1 = &q.heights[2].options[0];
        assert_eq!((av1.fps, av1.hdr, av1.has_audio, av1.size), (Some(60), false, false, Some(Size { bytes: 124_386_876, exact: true })));
        assert_eq!(q.audio.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["251", "140", "250", "249", "139"]);
        assert_eq!((q.audio[0].codec.as_str(), q.audio[0].kbps, q.audio[1].codec.as_str()), ("Opus", Some(129), "AAC"));
        assert!(q.languages.is_empty());

        // 1080p AV1 with its Opus audio: both sizes exact.
        assert_eq!(q.total(av1, None), Some(Size { bytes: 124_386_876 + 10_202_210, exact: true }));
        assert_eq!(q.video_text(av1, None), "AV1 · 60 fps · 128.4 MiB");
        assert_eq!(q.audio_text(AudioForm::Original, None), "Opus 129 kbps · 9.7 MiB");
        assert_eq!(q.audio_text(AudioForm::M4a, None), "AAC 129 kbps, not converted");
        assert_eq!(q.audio_text(AudioForm::Mp3, None), "converted from Opus");

        // Square: 1080x1080 is 1080p; 25 fps is not said; the "Premium" HLS copy has no size.
        let square = Qualities::of(&listing("square")).unwrap();
        assert_eq!(square.heights[0].p, 1080);
        assert_eq!(ids(&square.heights[0]), [("AV1", "399"), ("VP9", "248"), ("H.264", "137")]);
        assert_eq!(square.video_text(&square.heights[0].options[0], None), "AV1 · 13.1 MiB");
    }

    /// A size the listing only estimates is "about"; one it lacks is left out, and so is the size
    /// of a download whose audio has none. A portrait video is named by its width; HDR is a kind
    /// of its own; the audio languages are a choice, the original first.
    #[test]
    fn sizes_are_exact_about_or_left_out() {
        let found = json!({"formats": [
            {"format_id": "a-de", "vcodec": "none", "acodec": "opus", "abr": 128, "filesize": 1_000_000, "language": "de", "language_preference": 10},
            {"format_id": "a-en", "vcodec": "none", "acodec": "opus", "abr": 128, "filesize_approx": 1_100_000, "language": "en", "format_note": "English, dubbed"},
            {"format_id": "v1", "vcodec": "vp09.00.40.08", "acodec": "none", "width": 1080, "height": 1920, "fps": 30, "dynamic_range": "SDR", "filesize_approx": 50_000_000},
            {"format_id": "v2", "vcodec": "vp09.02.40.10", "acodec": "none", "width": 1080, "height": 1920, "fps": 30, "dynamic_range": "HDR10"},
            {"format_id": "v3", "vcodec": "avc1.64001F", "acodec": "none", "width": 720, "height": 1280, "fps": 30, "filesize": 20_000_000},
            {"format_id": "locked", "vcodec": "avc1", "acodec": "none", "height": 4320, "has_drm": true},
        ]});
        let q = Qualities::of(&found).unwrap();
        assert_eq!(q.heights.iter().map(|h| h.p).collect::<Vec<_>>(), [1080, 720]);
        let [sdr, hdr] = &q.heights[0].options[..] else { panic!("{:?}", q.heights[0]) };
        assert_eq!((sdr.id.as_str(), hdr.id.as_str(), hdr.hdr), ("v1", "v2", true));
        assert_eq!(q.languages, ["de", "en"]);
        assert_eq!(q.video_text(sdr, None), "VP9 · about 48.6 MiB");
        assert_eq!(q.video_text(sdr, Some("en")), "VP9 · about 48.7 MiB");
        assert_eq!(q.video_text(hdr, None), "VP9 · HDR");
        assert_eq!(q.video_text(&q.heights[1].options[0], None), "H.264 · 20.0 MiB");
        assert_eq!(q.video_text(&q.heights[1].options[0], Some("en")), "H.264 · about 20.1 MiB");
        assert_eq!(q.audio_text(AudioForm::M4a, Some("en")), "converted from Opus");

        // Nothing to choose from: a list, a site's one file.
        assert_eq!(Qualities::of(&json!({"_type": "playlist", "entries": []})), None);
        assert_eq!(Qualities::of(&json!({"title": "Clip", "height": 720, "vcodec": "h264"})), None);
    }

    /// A rule takes its height, else the nearest below; its codec there, else the first; and
    /// says when it took another. Its formats are those of the card's choice.
    #[test]
    fn a_rule_takes_the_nearest_below() {
        let q = Qualities::of(&listing("4k60")).unwrap();
        let take = |text: &str| q.take(&MediaPick::parse(text).unwrap());
        let picked = |text: &str| take(text).0.formats.unwrap();
        assert_eq!(take("1080p:av1"), (MediaPick { formats: Some("399+251".into()), ..MediaPick::parse("1080p:av1").unwrap() }, None));
        assert_eq!(picked("720p:h264"), "298+251");
        assert_eq!(picked("best"), "401+251");
        assert_eq!(take("1200p"), (MediaPick { formats: Some("399+251".into()), ..MediaPick::parse("1080p:av1").unwrap() }, Some("No 1200p here: 1080p is taken".into())));
        assert_eq!(take("2160p:h264"), (MediaPick { formats: Some("401+251".into()), ..MediaPick::parse("2160p:av1").unwrap() }, Some("No H.264 at 2160p: AV1 is taken".into())));
        assert_eq!(take("100p").1.as_deref(), Some("No 100p here: 144p is taken"));
        assert_eq!(take("m4a").0, MediaPick { formats: Some("140".into()), ..MediaPick::parse("m4a").unwrap() });
        assert_eq!(take("original").0.formats.as_deref(), Some("251"));
        assert_eq!(take("mp3@de"), (MediaPick { formats: Some("251".into()), ..MediaPick::parse("mp3").unwrap() }, None), "one language: no choice");

        let square = Qualities::of(&listing("square")).unwrap();
        assert_eq!(square.take(&MediaPick::parse("2160p:vp9").unwrap()), (
            MediaPick { formats: Some("248+251".into()), ..MediaPick::parse("1080p:vp9").unwrap() },
            Some("No 2160p here: 1080p is taken".into()),
        ));

        let dubbed = json!({"formats": [
            {"format_id": "a-de", "vcodec": "none", "acodec": "opus", "abr": 128, "language": "de", "language_preference": 10},
            {"format_id": "a-en", "vcodec": "none", "acodec": "opus", "abr": 128, "language": "en"},
            {"format_id": "v", "vcodec": "avc1", "acodec": "none", "height": 720},
        ]});
        let q = Qualities::of(&dubbed).unwrap();
        assert_eq!(q.take(&MediaPick::parse("720p@en").unwrap()).0.formats.as_deref(), Some("v+a-en"));
        assert_eq!(q.take(&MediaPick::parse("720p@fr").unwrap()), (
            MediaPick { formats: Some("v+a-de".into()), ..MediaPick::parse("720p:h264").unwrap() },
            Some("No fr audio here: the original is taken".into()),
        ));
    }

    /// The command line's words for a pick, both ways, and those it refuses.
    #[test]
    fn a_pick_is_written_as_the_command_line_takes_it() {
        for text in ["best", "1080p", "2160p:vp9:hdr", "1440p:av1@de-DE", "best:h264", "original", "mp3@en", "m4a"] {
            assert_eq!(MediaPick::parse(text).unwrap().to_string(), text);
        }
        assert_eq!(MediaPick::parse(" 720P:AVC ").unwrap(), MediaPick { up_to: Some(720), codec: Some("h264".into()), ..Default::default() });
        assert_eq!(MediaPick::parse("audio").unwrap().audio, Some(AudioForm::Original));
        for wrong in ["", "1080", "1080x", "fast", "mp3:av1", "original:hdr", "1080p:xyz", "1080p@de]x", "0p", "99999p"] {
            assert!(MediaPick::parse(wrong).is_err(), "{wrong}");
        }
        assert_eq!(MediaPick::parse("1080p:av1:hdr@de").unwrap().label(), "Up to 1080p, AV1 first, HDR first, audio in de");
        assert_eq!(MediaPick::parse("m4a").unwrap().label(), "Audio as M4A");
        let rule = MediaPick { formats: Some("399+251".into()), ..MediaPick::parse("1080p").unwrap() };
        assert_eq!(rule.rule(), MediaPick::parse("1080p").unwrap());
        assert!(MediaPick { formats: Some("399+251;rm".into()), ..Default::default() }.check().is_err());
        // Kept in the settings without the formats.
        assert_eq!(serde_json::to_value(rule.rule()).unwrap(), json!({"audio": null, "up_to": 1080, "codec": null, "hdr": false, "language": null}));
    }

    /// What yt-dlp is asked for: the card's formats first, then the rule, sorted by height first.
    #[test]
    fn a_pick_asks_yt_dlp_for_its_formats_then_its_rule() {
        let args = |pick: MediaPick| pick.to_args();
        let merge = ["--merge-output-format", "mp4", "--postprocessor-args", "Merger+ffmpeg_o:-movflags -faststart"];
        let video = MediaPick { formats: Some("399+251".into()), ..MediaPick::parse("1080p:av1").unwrap() };
        let expected: Vec<&str> = ["-f", "399+251/bv*[dynamic_range=?SDR]+ba/bv*+ba/b", "-S", "res:1080,vcodec:av01"].into_iter().chain(merge).collect();
        assert_eq!(args(video), expected);
        let hdr = MediaPick::parse("best:hdr@de").unwrap();
        let expected: Vec<&str> = ["-f", "bv*+ba[language^=de]/bv*+ba/b", "-S", "res,hdr:12"].into_iter().chain(merge).collect();
        assert_eq!(args(hdr), expected);
        let expected: Vec<&str> = ["-f", "bv*[dynamic_range=?SDR]+ba/bv*+ba/b"].into_iter().chain(merge).collect();
        assert_eq!(args(MediaPick::default()), expected);
        assert_eq!(args(MediaPick::parse("original@de").unwrap()), ["-f", "ba[language^=de]/ba/b"]);
        assert_eq!(args(MediaPick::parse("mp3").unwrap()), ["-f", "ba/b", "-x", "--audio-format", "mp3"]);
        let m4a = MediaPick { formats: Some("140".into()), ..MediaPick::parse("m4a").unwrap() };
        assert_eq!(args(m4a), ["-f", "140/ba[acodec^=mp4a]/ba/b", "-x", "--audio-format", "m4a"]);
        // What fails the check is not passed on.
        let bad = MediaPick { language: Some("x]/b".into()), formats: Some("1/2".into()), audio: Some(AudioForm::Original), ..Default::default() };
        assert_eq!(args(bad), ["-f", "ba/b"]);
    }

    #[test]
    fn a_pick_is_kept_for_the_site() {
        for (link, kept) in [
            ("https://www.youtube.com/watch?v=a", "youtube.com"),
            ("https://m.youtube.com/watch?v=a", "youtube.com"),
            ("https://youtu.be/a", "youtube.com"),
            ("https://vimeo.com/1", "vimeo.com"),
            ("https://Player.Vimeo.com/video/1", "player.vimeo.com"),
        ] {
            assert_eq!(site(&Url::parse(link).unwrap()), kept, "{link}");
        }
    }

    /// The command line's list: each quality under the name --media-preset takes, then audio.
    #[test]
    fn the_command_line_lists_the_qualities() {
        let lines = Qualities::of(&listing("4k60")).unwrap().lines();
        assert_eq!(lines[0], "  2160p:av1         AV1 · 60 fps · 689.2 MiB");
        assert!(lines.contains(&"  1080p:h264        H.264 · 60 fps · 255.4 MiB".to_string()), "{lines:#?}");
        assert_eq!(lines[lines.len() - 3..], ["  original          Opus 129 kbps · 9.7 MiB", "  m4a               AAC 129 kbps, not converted", "  mp3               converted from Opus"]);
    }
}
