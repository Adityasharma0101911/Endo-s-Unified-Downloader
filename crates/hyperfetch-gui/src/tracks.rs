//! The track chooser of an HLS or DASH download (Choose tracks… on the Add page and on a queue
//! row): what the stream holds, what is ticked, and the download options that makes.

use hyperfetch_core::engine::DownloadOptions;
use hyperfetch_core::queue::{QueueItem, QueueItemStatus};
use hyperfetch_core::{choose_subtitles, choose_tracks, LiveStart, StreamInfo, Track, TrackChoice, TrackKind};
use url::Url;

/// Whether `url` is an HLS or DASH stream's, as the engine tells them by the link.
pub fn is_stream(url: &Url) -> bool {
    url.path().to_ascii_lowercase().ends_with(".mpd") || url.as_str().contains(".m3u8")
}

/// The link in `input` when it is a single http(s) link to an HLS or DASH stream.
pub fn stream_link(input: &str) -> Option<Url> {
    let input = input.trim();
    let url = Url::parse(input).ok().filter(|u| matches!(u.scheme(), "http" | "https") && !input.contains(char::is_whitespace))?;
    is_stream(&url).then_some(url)
}

/// Whether queued download `item` offers Choose tracks…: a stream not running or done.
pub fn offered(item: &QueueItem) -> bool {
    matches!(item.status, QueueItemStatus::Queued | QueueItemStatus::Paused | QueueItemStatus::Failed(_))
        && (item.options.hls || item.options.dash || item.urls.first().is_some_and(is_stream))
}

/// Where a live recording starts (see `DownloadOptions::live_from`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Start {
    #[default]
    Now,
    /// The oldest segment the server keeps (`live_from_start`).
    Oldest,
    MinutesAgo,
    /// A clock time, as typed.
    At,
}

/// The chooser of one stream: for a queued download, or for the link in the form (`item` None).
pub struct Chooser {
    pub item: Option<usize>,
    pub link: String,
    /// What the stream holds once read, or why it could not be.
    pub info: Option<Result<StreamInfo, String>>,
    pub picks: Picks,
}

/// What the chooser has ticked and typed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Picks {
    /// Positions in the stream's tracks: the video (None: no video), the audio and subtitles
    /// tracks, in the order ticked (the first audio is the joined file's default).
    pub video: Option<usize>,
    pub audio: Vec<usize>,
    pub subtitles: Vec<usize>,
    /// The parts left out (`Part::id`).
    pub dropped: Vec<usize>,
    /// The time range as typed; empty is the start, the end.
    pub from: String,
    pub to: String,
    pub start: Start,
    pub minutes_ago: u32,
    pub at: String,
    /// Minutes to record, seconds without a new segment before it ends: 0 for no limit.
    pub stop_after: u64,
    pub idle: u64,
}

impl Picks {
    /// What `options` (the Settings', or a queued download's) take of `info`, ticked.
    pub fn of(info: &StreamInfo, options: &DownloadOptions) -> Self {
        let mut choice = options.tracks.clone();
        if let (true, Some(height)) = (choice.video.trim().is_empty(), options.height) {
            choice.video = format!("{height}p");
        }
        // A spec the stream does not fit starts from its best tracks.
        let chosen = choose_tracks(info, &choice).or_else(|_| choose_tracks(info, &TrackChoice::default())).unwrap_or_default();
        let of_kind = |kind| chosen.iter().copied().filter(|&at| info.tracks[at].kind == kind).collect::<Vec<_>>();
        let subtitles = match options.tracks.ids.is_empty() {
            true => choose_subtitles(info, options.subtitles.as_deref()),
            false => of_kind(TrackKind::Subtitles),
        };
        let (start, minutes_ago, at) = match options.live_from.as_deref().map(|text| (text, LiveStart::parse(text))) {
            Some((_, Ok(LiveStart::Ago(secs)))) => (Start::MinutesAgo, (secs / 60.0).round() as u32, String::new()),
            Some((text, Ok(_))) => (Start::At, 30, text.to_string()),
            _ if options.live_from_start => (Start::Oldest, 30, String::new()),
            _ => (Start::Now, 30, String::new()),
        };
        let end = info.duration.unwrap_or(f64::MAX);
        let time = |secs: f64| crate::util::format_duration(secs as u64);
        let (from, to) = options.sections.first().map_or_else(Default::default, |&(from, to)| {
            (if from > 0.0 { time(from) } else { String::new() }, if to < end { time(to) } else { String::new() })
        });
        Self {
            video: of_kind(TrackKind::Video).first().copied(),
            audio: of_kind(TrackKind::Audio),
            subtitles,
            dropped: options.drop_parts.clone(),
            from,
            to,
            start,
            minutes_ago,
            at,
            stop_after: options.record_seconds.unwrap_or(0) / 60,
            idle: options.live_idle_timeout.unwrap_or(0),
        }
    }

    /// `options` with these picks for `info`, or why they cannot be had.
    pub fn apply(&self, info: &StreamInfo, options: &mut DownloadOptions) -> Result<(), String> {
        let ids: Vec<String> = self.video.iter().chain(&self.audio).chain(&self.subtitles).filter_map(|&at| info.tracks.get(at)).map(|t| t.id.clone()).collect();
        // No ids would leave the choice to the specs: not what was asked.
        if ids.is_empty() && !info.tracks.is_empty() {
            return Err("Tick a track to download".to_string());
        }
        if info.live {
            options.live_from = match self.start {
                Start::MinutesAgo => Some(format!("{}m", self.minutes_ago)),
                Start::At => LiveStart::parse(&self.at).map(|_| Some(self.at.trim().to_string()))?,
                Start::Now | Start::Oldest => None,
            };
            options.live_from_start = self.start == Start::Oldest;
            options.record_seconds = Some(self.stop_after * 60).filter(|&secs| secs > 0);
            options.live_idle_timeout = Some(self.idle).filter(|&secs| secs > 0);
        } else {
            options.sections = self.sections(info)?;
            options.drop_parts = info.parts.iter().map(|part| part.id).filter(|id| self.dropped.contains(id)).collect();
            if !info.parts.is_empty() && options.drop_parts.len() == info.parts.len() {
                return Err("Keep at least one part".to_string());
            }
        }
        options.tracks.ids = ids;
        Ok(())
    }

    /// The time range as `DownloadOptions::sections` takes it: none for the whole stream.
    fn sections(&self, info: &StreamInfo) -> Result<Vec<(f64, f64)>, String> {
        let end = info.duration.unwrap_or(f64::MAX);
        let time = |text: &str, empty: f64| match (text.trim(), LiveStart::parse(text)) {
            ("", _) => Ok(empty),
            (_, Ok(LiveStart::Ago(secs))) => Ok(secs),
            (text, _) => Err(format!("\"{text}\" is not a time in the stream (e.g. 90, 1:30 or 1:02:03)")),
        };
        let (from, to) = (time(&self.from, 0.0)?, time(&self.to, end)?);
        if from >= to {
            return Err("The time range ends before it starts".to_string());
        }
        Ok(if from <= 0.0 && to >= end { Vec::new() } else { vec![(from, to)] })
    }
}

/// Common languages by their ISO 639-1 code and ISO 639-2 codes, for naming a track's language
/// in words.
const LANGUAGES: [(&str, &str, &str); 40] = [
    ("en", "eng", "English"),
    ("es", "spa", "Spanish"),
    ("fr", "fra/fre", "French"),
    ("de", "deu/ger", "German"),
    ("it", "ita", "Italian"),
    ("pt", "por", "Portuguese"),
    ("ru", "rus", "Russian"),
    ("ja", "jpn", "Japanese"),
    ("ko", "kor", "Korean"),
    ("zh", "zho/chi", "Chinese"),
    ("ar", "ara", "Arabic"),
    ("hi", "hin", "Hindi"),
    ("bn", "ben", "Bengali"),
    ("ta", "tam", "Tamil"),
    ("te", "tel", "Telugu"),
    ("nl", "nld/dut", "Dutch"),
    ("sv", "swe", "Swedish"),
    ("no", "nor", "Norwegian"),
    ("nb", "nob", "Norwegian Bokmål"),
    ("da", "dan", "Danish"),
    ("fi", "fin", "Finnish"),
    ("pl", "pol", "Polish"),
    ("cs", "ces/cze", "Czech"),
    ("sk", "slk/slo", "Slovak"),
    ("hu", "hun", "Hungarian"),
    ("ro", "ron/rum", "Romanian"),
    ("bg", "bul", "Bulgarian"),
    ("hr", "hrv", "Croatian"),
    ("sr", "srp", "Serbian"),
    ("uk", "ukr", "Ukrainian"),
    ("el", "ell/gre", "Greek"),
    ("tr", "tur", "Turkish"),
    ("he", "heb", "Hebrew"),
    ("fa", "fas/per", "Persian"),
    ("th", "tha", "Thai"),
    ("vi", "vie", "Vietnamese"),
    ("id", "ind", "Indonesian"),
    ("ms", "msa/may", "Malay"),
    ("ca", "cat", "Catalan"),
    ("tl", "tgl/fil", "Filipino"),
];

/// `tag` (`en`, `pt-BR`, `jpn`) in words: `English`, `Portuguese (BR)`; the tag itself when its
/// language is not a common one.
pub fn language_name(tag: &str) -> String {
    let mut parts = tag.split(['-', '_']);
    let primary = parts.next().unwrap_or_default().to_ascii_lowercase();
    let Some((_, _, name)) = LANGUAGES.iter().find(|(two, three, _)| *two == primary || three.split('/').any(|t| t == primary)) else {
        return tag.to_string();
    };
    match parts.next() {
        Some(region) => format!("{name} ({region})"),
        None => name.to_string(),
    }
}

/// One line for `track` in the chooser: its language in words, then what `Track::describe` says
/// of it but its id and kind.
pub fn label(track: &Track) -> String {
    let described = track.describe();
    let mut words: Vec<&str> = described.split(' ').skip(2).collect();
    if let Some(at) = track.language.as_deref().and_then(|tag| words.iter().position(|word| *word == tag)) {
        words.remove(at);
    }
    let line = track.language.as_deref().map(language_name).into_iter().chain(words.into_iter().map(str::to_string)).collect::<Vec<_>>().join(" ");
    if line.is_empty() { track.id.clone() } else { line }
}

#[cfg(test)]
mod tests {
    use hyperfetch_core::{Part, StreamFormat};

    use super::*;

    fn track(id: &str, kind: TrackKind, language: Option<&str>) -> Track {
        Track { id: id.into(), kind, language: language.map(str::to_string), ..Track::default() }
    }

    fn stream(live: bool) -> StreamInfo {
        let part = |id, duration| Part { id, duration, summary: String::new(), ad: false };
        StreamInfo {
            format: StreamFormat::Dash,
            live,
            duration: (!live).then_some(600.0),
            tracks: vec![
                Track { height: Some(1080), bandwidth: Some(5_000_000), ..track("v0", TrackKind::Video, None) },
                Track { height: Some(720), bandwidth: Some(2_000_000), ..track("v1", TrackKind::Video, None) },
                Track { default: true, ..track("a0", TrackKind::Audio, Some("en")) },
                track("a1", TrackKind::Audio, Some("ja")),
                track("s0", TrackKind::Subtitles, Some("pt-BR")),
            ],
            parts: if live { Vec::new() } else { vec![part(0, 570.0), part(1, 30.0)] },
        }
    }

    /// The picks start from what the options take (the best video, the default audio, every
    /// subtitles track), else from what an earlier choice saved; they make the options back.
    #[test]
    fn picks_start_from_the_options_and_make_them() {
        let info = stream(false);
        let picks = Picks::of(&info, &DownloadOptions::default());
        assert_eq!((picks.video, picks.audio.as_slice(), picks.subtitles.as_slice()), (Some(0), &[2][..], &[4][..]));
        let picks = Picks::of(&info, &DownloadOptions { height: Some(720), subtitles: Some("none".into()), ..Default::default() });
        assert_eq!((picks.video, picks.subtitles.len()), (Some(1), 0));
        // A spec the stream cannot fit starts from its best.
        let unfit = DownloadOptions { tracks: TrackChoice { video: "av1".into(), ..Default::default() }, ..Default::default() };
        assert_eq!(Picks::of(&info, &unfit).video, Some(0));

        let chosen = Picks { video: Some(1), audio: vec![3, 2], subtitles: Vec::new(), dropped: vec![1], from: "1:30".into(), ..picks };
        let mut options = DownloadOptions::default();
        chosen.apply(&info, &mut options).unwrap();
        assert_eq!(options.tracks.ids, ["v1", "a1", "a0"]);
        assert_eq!((options.drop_parts.as_slice(), options.sections.as_slice()), (&[1][..], &[(90.0, 600.0)][..]));
        let again = Picks::of(&info, &options);
        assert_eq!(again, Picks { from: "01:30".into(), ..chosen.clone() }, "an earlier choice opens as it was saved");

        let nothing = Picks { video: None, audio: Vec::new(), ..chosen.clone() };
        assert_eq!(nothing.apply(&info, &mut options).unwrap_err(), "Tick a track to download");
        let all_dropped = Picks { dropped: vec![0, 1], ..chosen.clone() };
        assert_eq!(all_dropped.apply(&info, &mut options).unwrap_err(), "Keep at least one part");
        for (from, to, error) in [("x", "", "\"x\" is not a time"), ("2:00", "1:00", "ends before it starts"), ("2026-10-10 21:00", "", "is not a time")] {
            let range = Picks { from: from.into(), to: to.into(), ..chosen.clone() };
            assert!(range.apply(&info, &mut options).unwrap_err().contains(error), "{from}-{to}");
        }
        let whole = Picks { from: String::new(), to: "10:00".into(), ..chosen };
        whole.apply(&info, &mut options).unwrap();
        assert!(options.sections.is_empty(), "the whole stream is no range");
    }

    /// A live stream's start, stop and idle time become the recording's options, and open again
    /// as they were saved; a clock time that is none is refused.
    #[test]
    fn a_live_stream_takes_its_start_and_stop() {
        let info = stream(true);
        let picks = Picks::of(&info, &DownloadOptions::default());
        assert_eq!((picks.start, picks.stop_after, picks.idle), (Start::Now, 0, 0));
        let mut options = DownloadOptions::default();
        let ago = Picks { start: Start::MinutesAgo, minutes_ago: 45, stop_after: 90, idle: 20, ..picks.clone() };
        ago.apply(&info, &mut options).unwrap();
        assert_eq!((options.live_from.as_deref(), options.live_from_start), (Some("45m"), false));
        assert_eq!((options.record_seconds, options.live_idle_timeout), (Some(5400), Some(20)));
        assert_eq!(Picks::of(&info, &options), ago);

        let at = Picks { start: Start::At, at: " 2026-10-10 21:00 ".into(), ..picks.clone() };
        at.apply(&info, &mut options).unwrap();
        assert_eq!((options.live_from.as_deref(), options.record_seconds, options.live_idle_timeout), (Some("2026-10-10 21:00"), None, None));
        assert_eq!(Picks::of(&info, &options).start, Start::At);
        let oldest = Picks { start: Start::Oldest, ..picks.clone() };
        oldest.apply(&info, &mut options).unwrap();
        assert!(options.live_from.is_none() && options.live_from_start);
        assert!(Picks { start: Start::At, at: "tonight".into(), ..picks }.apply(&info, &mut options).unwrap_err().starts_with("Unknown start"));
    }

    #[test]
    fn languages_and_tracks_are_named_in_words() {
        assert_eq!([language_name("en"), language_name("pt-BR"), language_name("jpn"), language_name("fre"), language_name("xx")], ["English", "Portuguese (BR)", "Japanese", "French", "xx"]);
        let audio = Track { codecs: "ec-3".into(), channels: Some(6), default: true, ..track("a0", TrackKind::Audio, Some("en-US")) };
        assert_eq!(label(&audio), "English (US) eac3 6ch default");
        assert_eq!(label(&track("s3", TrackKind::Subtitles, None)), "s3");
    }

    #[test]
    fn stream_links_are_told_by_their_path() {
        for yes in ["https://cdn.example/live/master.m3u8?token=1", "http://h.example/v/Manifest.MPD"] {
            assert!(stream_link(&format!(" {yes} ")).is_some(), "{yes}");
        }
        for no in ["https://h.example/a.mp4", "ftp://h.example/a.m3u8", "https://h.example/a.m3u8 https://h.example/b.m3u8", "list.m3u8"] {
            assert!(stream_link(no).is_none(), "{no}");
        }
    }
}
