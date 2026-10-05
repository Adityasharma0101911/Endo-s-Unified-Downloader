//! Podcast and RSS/Atom feeds read into one download per episode.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::reader::Reader;
use serde::Deserialize;
use url::Url;

use crate::history::{is_redacted, redact_url, DownloadHistoryManager, HistoryEntry, HistoryStatus};
use crate::ingest::{clean_path, decode_text, ListOptions, Task};
use crate::resolver;

/// Largest feed (or Apple Podcasts answer) read. Long-running shows publish feeds of several MiB.
const MAX_FEED_BYTES: usize = 32 * 1024 * 1024;

/// How much of an answer is read, at most, to find its first element: an answer whose first
/// element is no `<rss>` or `<feed>` is left at once, so a large file is not read to find out.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Longest episode title (in bytes) put in a file name, which leaves room for the date, a
/// " (2)" that tells episodes of the same name apart and the extension.
const MAX_TITLE_BYTES: usize = 150;

/// Extensions of audio and video files an enclosure may have.
const MEDIA_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "ogg", "oga", "opus", "flac", "wav", "wma", "aif", "aiff", "amr", "mp2", "mka", "weba", "mp4", "m4v", "mov",
    "webm", "mkv", "avi", "wmv", "mpg", "mpeg", "3gp", "3g2", "ogv", "flv",
];

/// Apple's iTunes Lookup API, which answers without a key.
const LOOKUP_API: &str = "https://itunes.apple.com/lookup";

/// Podcast hosts whose feed links have an `rss`, `feed` or `feeds` part: Anchor's
/// `/podcast/rss`, Libsyn's `/rss`, Acast's `/rss/...`, Spreaker's `/episodes/feed`, Supercast's
/// `/feeds/...` and Patreon's `/rss/...`.
const FEED_PATH_HOSTS: &[&str] = &["anchor.fm", "libsyn.com", "acast.com", "spreaker.com", "supercast.com", "patreon.com"];

/// Whether `url` names a feed (or a podcast show page) this module lists, from its shape alone:
/// an Apple Podcasts show or episode, or a link [`feed_shape`] takes.
pub fn lists(url: &Url) -> bool {
    AppleLink::of(url).is_some() || feed_shape(url).is_some()
}

/// Whether `url` is shaped like a feed: `Some(true)` for a shape only feeds have (a feed host,
/// `feeds.`, `feed.`, `rss.` and `podcastfeeds.` as Simplecast, Megaphone, Acast, Transistor,
/// Buzzsprout, Art19, Podbean, SoundCloud, Libsyn and NBC use, or one whose first label ends in
/// `-feed`; a path ending in `.rss` or `.atom`; a feed asked for in the query, Squarespace's
/// `?format=rss` and WordPress's `?feed=podcast`; a [`FEED_PATH_HOSTS`] feed path), `Some(false)`
/// for one other files share (a path ending in `.xml` or `/podcast`, or with an `rss`, `feed` or
/// `feeds` part), None for neither.
fn feed_shape(url: &Url) -> Option<bool> {
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let path = url.path().to_ascii_lowercase();
    let first_label = host.split('.').next().unwrap_or_default();
    let feed_query = |(key, value): (std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>)| {
        matches!(&*key, "format" | "feed") && ["rss", "atom", "podcast"].iter().any(|kind| value.to_ascii_lowercase().starts_with(kind))
    };
    let feed_part = path.split('/').any(|s| matches!(s, "rss" | "feed" | "feeds"));
    let feed_path_host = FEED_PATH_HOSTS.iter().any(|h| host == *h || host.strip_suffix(h).is_some_and(|sub| sub.ends_with('.')));
    if matches!(first_label, "feed" | "feeds" | "rss" | "podcastfeeds")
        || first_label.ends_with("-feed")
        || path.ends_with(".rss")
        || path.ends_with(".atom")
        || url.query_pairs().any(feed_query)
        || (feed_part && feed_path_host)
    {
        return Some(true);
    }
    (feed_part || path.ends_with(".xml") || path.trim_end_matches('/').ends_with("/podcast")).then_some(false)
}

/// One task per episode of the feed at `url`; called only when [`lists`] takes `url`. None when
/// it is no feed after all, or a feed without audio or video (the link is then downloaded as it
/// is); `Some(Ok)` is empty only when every episode it would list was downloaded before.
///
/// Episodes come newest first, each named "YYYY-MM-DD Title.ext" in a folder named after the
/// feed; `options.latest` keeps the newest N, and `options.only_new` then leaves out those
/// history or the download archive records as downloaded (see [`Done`]). An Apple Podcasts show
/// lists its public feed, and an episode link that one episode of it (see [`AppleLink::list`]).
pub async fn list(http: &reqwest::Client, url: &Url, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    if let Some(apple) = AppleLink::of(url) {
        return apple.list(http, LOOKUP_API, options).await;
    }
    match read_feed(options.get(http, url), url, feed_shape(url) == Some(true)).await {
        Ok(Some(feed)) if !feed.episodes.is_empty() => Some(show_tasks(feed, options).await),
        Ok(Some(_)) => {
            tracing::info!("{} is a feed without audio or video: it is downloaded as it is", redact_url(url.as_str()));
            None
        }
        Ok(None) => None,
        Err(e) => Some(Err(e)),
    }
}

/// The feed `request` gets from `url`; None when it answers with a client error or is no RSS or
/// Atom feed, and unless `sure` it is one, when its host cannot be reached or is busy (see
/// [`fetch`]).
async fn read_feed(request: reqwest::RequestBuilder, url: &Url, sure: bool) -> Result<Option<Feed>, String> {
    let Some(Fetched { body, base, charset }) = fetch(request, url, true, sure).await? else { return Ok(None) };
    let feed = parse_feed(&decode_feed(&body, charset.as_deref()), &base).transpose()?;
    Ok(feed.map(|feed| Feed { source: redact_url(url.as_str()), ..feed }))
}

/// Labels of windows-1252, as the web reads them: ISO-8859-1 and US-ASCII stand for it too (it
/// only adds letters and signs at 0x80-0x9F).
const WINDOWS_1252_LABELS: &[&str] = &[
    "windows-1252", "cp1252", "x-cp1252", "iso-8859-1", "iso8859-1", "iso88591", "iso_8859-1", "iso_8859-1:1987", "iso-ir-100", "latin1",
    "latin-1", "l1", "csisolatin1", "cp819", "ibm819", "us-ascii", "ascii", "ansi_x3.4-1968",
];

/// A feed's text: UTF-8, or UTF-16 with a BOM (see [`decode_text`]). One that is neither is
/// windows-1252 when its XML declaration or `charset` (its Content-Type's) says it is (see
/// [`WINDOWS_1252_LABELS`]), else UTF-8 with a stray byte: that byte becomes U+FFFD and every
/// other letter stays. A feed in another single-byte encoding gets wrong letters, not an error.
fn decode_feed(bytes: &[u8], charset: Option<&str>) -> String {
    if let Ok(text) = decode_text(bytes) {
        return text;
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let latin = |label: &str| WINDOWS_1252_LABELS.contains(&label.trim().trim_matches(['"', '\'']).to_ascii_lowercase().as_str());
    if charset.is_some_and(latin) || declared_encoding(bytes).is_some_and(|label| latin(&label)) {
        bytes.iter().map(|&byte| windows_1252(byte)).collect()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// The encoding the XML declaration `bytes` start with names, if any, past whitespace some feeds
/// put before it.
fn declared_encoding(bytes: &[u8]) -> Option<String> {
    match Reader::from_reader(bytes.trim_ascii_start()).read_event_into(&mut Vec::new()) {
        Ok(Event::Decl(decl)) => Some(String::from_utf8_lossy(&decl.encoding()?.ok()?).into_owned()),
        _ => None,
    }
}

/// The character a windows-1252 byte stands for. The five bytes it leaves unassigned are the C1
/// controls of the same number, as browsers read them.
fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}',
        '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9F => HIGH[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

/// The body `request` gets from `url` and where it came from (after redirects), at most
/// [`MAX_FEED_BYTES`]; None when its host answers with a client error (a login, an expired
/// private feed), which the engine is left to report. With `feed`, also None when its first element is no `<rss>` or
/// `<feed>`, or it is too large to tell and not labelled a feed. An unreachable host, a
/// timeout, a rate limit and a server error are errors, to retry, when the link is `sure` to
/// be what is wanted; else None, and the engine downloads the link, retrying as it does. The
/// link is redacted in errors: a private feed's token is in it.
async fn fetch(mut request: reqwest::RequestBuilder, url: &Url, feed: bool, sure: bool) -> Result<Option<Fetched>, String> {
    use reqwest::header::{ACCEPT, CONTENT_TYPE};
    use reqwest::StatusCode;
    let fail = |e: reqwest::Error| {
        let error = format!("Cannot fetch {}: {}", redact_url(url.as_str()), e.without_url());
        if sure {
            return Err(error);
        }
        tracing::info!("{}: the link is downloaded as it is", error);
        Ok(None)
    };
    if feed {
        request = request.header(ACCEPT, "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.8");
    }
    let resp = match request.send().await {
        Ok(resp) => resp,
        Err(e) => return fail(e),
    };
    let status = resp.status();
    if status.is_client_error() && !matches!(status, StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS) {
        tracing::info!("{} answered HTTP {}", redact_url(url.as_str()), status);
        return Ok(None);
    }
    let mut resp = match resp.error_for_status() {
        Ok(resp) => resp,
        Err(e) => return fail(e),
    };
    let base = resp.url().clone();
    let content_type = resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let labelled = content_type.contains("rss") || content_type.contains("atom");
    let charset = content_type.split(';').skip(1).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        key.trim().eq_ignore_ascii_case("charset").then(|| value.trim().to_string())
    });
    let mut known_feed = !feed;
    let too_large = |known_feed: bool| {
        if labelled || known_feed {
            Err(format!("{} is larger than {} bytes", redact_url(url.as_str()), MAX_FEED_BYTES))
        } else {
            Ok(None)
        }
    };
    if resp.content_length().is_some_and(|len| len > MAX_FEED_BYTES as u64) {
        return too_large(known_feed);
    }
    let mut body = Vec::new();
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(e) => return fail(e),
        };
        body.extend_from_slice(&chunk);
        if !known_feed {
            match first_element(&body) {
                Some(name) if is_feed_root(&name) => known_feed = true,
                Some(_) => return Ok(None),
                None if body.len() >= MAX_HEAD_BYTES => return Ok(None),
                None => {}
            }
        }
        if body.len() > MAX_FEED_BYTES {
            return too_large(known_feed);
        }
    }
    Ok(known_feed.then_some(Fetched { body, base, charset }))
}

/// What [`fetch`] read: the body, where it came from and the charset its Content-Type names.
struct Fetched {
    body: Vec<u8>,
    base: Url,
    charset: Option<String>,
}

/// The name of the first element in `head`, the start of an XML document in UTF-8 or (with a
/// BOM) UTF-16, in lower case; None while `head` ends before it does.
fn first_element(head: &[u8]) -> Option<String> {
    // The head may end inside a character: that end is read as a replacement character.
    let utf16 = |rest: &[u8], unit: fn([u8; 2]) -> u16| {
        let units = rest.chunks_exact(2).map(|pair| unit([pair[0], pair[1]]));
        let text: String = char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect();
        first_element(text.as_bytes())
    };
    match head {
        [0xFF, 0xFE, rest @ ..] => return utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => return utf16(rest, u16::from_be_bytes),
        _ => {}
    }
    let mut reader = Reader::from_reader(head);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e) | Event::Empty(e)) => return Some(name_of(e.name())),
            Ok(Event::Eof) | Err(_) => return None,
            Ok(_) => buf.clear(),
        }
    }
}

fn name_of(name: QName<'_>) -> String {
    String::from_utf8_lossy(name.as_ref()).to_ascii_lowercase()
}

/// Whether an element named `name` is the root of an RSS or Atom feed (`<rss>`, `<feed>`, or
/// `<atom:feed>` with any prefix).
fn is_feed_root(name: &str) -> bool {
    matches!(name.rsplit(':').next(), Some("rss" | "feed"))
}

/// A feed's title and its episodes with an audio or video enclosure, in the feed's order.
#[derive(Debug, Default)]
struct Feed {
    title: Option<String>,
    episodes: Vec<Episode>,
    /// What tells this feed from another of the same title: its link without its secrets (see
    /// [`redact_url`]), or the Apple show it was looked up for. The archive holds only a digest
    /// of it (see [`episode_archive`]).
    source: String,
}

#[derive(Debug, Default)]
struct Episode {
    title: String,
    date: Option<Date>,
    guid: Option<String>,
    enclosure: Option<Enclosure>,
}

#[derive(Debug)]
struct Enclosure {
    url: Url,
    extension: &'static str,
    length: Option<u64>,
}

/// The RSS 2.0 or Atom feed `text`, its relative links read against `base`. None when its root
/// element is neither `<rss>` nor `<feed>`; an error when it breaks off or is malformed past that.
fn parse_feed(text: &str, base: &Url) -> Option<Result<Feed, String>> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut feed = Feed::default();
    // The open elements, "rss/channel/item" say, and the text of the innermost one so far.
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut episode: Option<Episode> = None;
    // The root's prefix ("atom:" of `<atom:feed>`), taken off the names of the elements in it.
    let mut prefix = String::new();
    let local = |name: QName<'_>, prefix: &str| {
        let name = name_of(name);
        name.strip_prefix(prefix).map(str::to_string).unwrap_or(name)
    };
    loop {
        let event = match reader.read_event() {
            Ok(event) => event,
            Err(e) if path.is_empty() => {
                tracing::info!("not a feed: {}", e);
                return None;
            }
            Err(e) => return Some(Err(format!("the feed is not well-formed XML at byte {}: {}", reader.error_position(), e))),
        };
        match event {
            Event::Start(e) if path.is_empty() => {
                let root = name_of(e.name());
                if !is_feed_root(&root) {
                    return None;
                }
                let (root_prefix, root) = root.rsplit_once(':').unwrap_or(("", root.as_str()));
                if !root_prefix.is_empty() {
                    prefix = format!("{}:", root_prefix);
                }
                path.push(root.to_string());
            }
            // An empty `<rss/>` lists nothing either.
            Event::Empty(_) if path.is_empty() => return None,
            Event::Start(e) => {
                let name = local(e.name(), &prefix);
                let parent = path.join("/");
                match (parent.as_str(), name.as_str()) {
                    ("rss/channel", "item") | ("feed", "entry") => episode = Some(Episode::default()),
                    _ => enclosure(&mut episode, &parent, &name, &e, base),
                }
                path.push(name);
                text.clear();
            }
            Event::Empty(e) => enclosure(&mut episode, &path.join("/"), &local(e.name(), &prefix), &e, base),
            Event::Text(t) => text.push_str(&t.unescape().map_or_else(|_| String::from_utf8_lossy(&t).into_owned(), |t| t.into_owned())),
            Event::CData(c) => text.push_str(&String::from_utf8_lossy(&c)),
            Event::End(_) => {
                let Some(name) = path.pop() else { break };
                let parent = path.join("/");
                let value = || text.split_whitespace().collect::<Vec<_>>().join(" ");
                match (parent.as_str(), name.as_str()) {
                    ("rss/channel", "title") | ("feed", "title") => feed.title = Some(value()).filter(|t| !t.is_empty()),
                    ("rss/channel", "item") | ("feed", "entry") => {
                        if let Some(done) = episode.take().filter(|e| e.enclosure.is_some()) {
                            feed.episodes.push(done);
                        }
                    }
                    ("rss/channel/item" | "feed/entry", field) => {
                        if let Some(episode) = &mut episode {
                            match field {
                                "title" => episode.title = value(),
                                // An Atom entry's first publication, else its last update.
                                "pubdate" | "dc:date" | "published" => episode.date = parse_date(&text).or(episode.date),
                                "updated" => episode.date = episode.date.or_else(|| parse_date(&text)),
                                "guid" | "id" => episode.guid = Some(value()),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                text.clear();
                if path.is_empty() {
                    break;
                }
            }
            // The root element's end ends the loop above, so this is a document without one, or
            // one that breaks off.
            Event::Eof if path.is_empty() => return None,
            Event::Eof => return Some(Err("the feed breaks off before its end".to_string())),
            _ => {}
        }
    }
    Some(Ok(feed))
}

/// Takes the audio or video enclosure of `episode` from `element` when it is one: an RSS
/// `<enclosure url type length>` or an Atom `<link rel="enclosure" href type length>`. The
/// first one an episode has counts.
fn enclosure(episode: &mut Option<Episode>, parent: &str, name: &str, element: &BytesStart<'_>, base: &Url) {
    let Some(episode) = episode.as_mut().filter(|e| e.enclosure.is_none()) else { return };
    let attr = |wanted: &str| {
        let found = element.attributes().flatten().find(|a| a.key.as_ref().eq_ignore_ascii_case(wanted.as_bytes()))?;
        Some(match found.unescape_value() {
            Ok(value) => value.trim().to_string(),
            Err(_) => String::from_utf8_lossy(&found.value).trim().to_string(),
        })
    };
    let href = match (parent, name) {
        ("rss/channel/item", "enclosure") => attr("url"),
        ("feed/entry", "link") if attr("rel").is_some_and(|rel| rel.eq_ignore_ascii_case("enclosure")) => attr("href"),
        _ => return,
    };
    let Some(url) = href.and_then(|href| base.join(&href).ok()).filter(|u| matches!(u.scheme(), "http" | "https")) else { return };
    let url = resolver::unwrap_redirect(&url).unwrap_or(url);
    let mime = attr("type").map(|t| t.split(';').next().unwrap_or_default().trim().to_ascii_lowercase());
    if let Some(extension) = media_extension(&url, mime.as_deref()) {
        let length = attr("length").and_then(|l| l.parse().ok()).filter(|&l| l > 0);
        episode.enclosure = Some(Enclosure { url, extension, length });
    }
}

/// The extension a file at `url` of type `mime` is saved with, when it is audio or video: the
/// audio or video one its link ends in, else its type's, else "mp3" or "mp4" for any other audio
/// or video type. Never another one, so a feed cannot have an episode saved as a program.
fn media_extension(url: &Url, mime: Option<&str>) -> Option<&'static str> {
    let fallback = match mime.and_then(|m| m.split_once('/')) {
        Some(("audio", _)) => Some("mp3"),
        Some(("video", _)) => Some("mp4"),
        _ => None,
    };
    link_extension(url).or_else(|| mime.and_then(mime_extension)).or(fallback)
}

/// The audio or video extension the last part of `url`'s path ends in.
fn link_extension(url: &Url) -> Option<&'static str> {
    let last = url.path_segments()?.next_back()?;
    let (_, ext) = last.rsplit_once('.')?;
    MEDIA_EXTENSIONS.iter().copied().find(|known| known.eq_ignore_ascii_case(ext))
}

/// The extension of files of the audio or video type `mime`.
fn mime_extension(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "audio/mpeg" | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg" | "audio/x-mp3" => "mp3",
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" => "m4a",
        "audio/x-m4b" => "m4b",
        "audio/aac" | "audio/x-aac" => "aac",
        "audio/ogg" | "audio/vorbis" => "ogg",
        "audio/opus" => "opus",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/x-ms-wma" => "wma",
        "audio/aiff" | "audio/x-aiff" => "aif",
        "audio/amr" => "amr",
        "audio/x-matroska" => "mka",
        "audio/webm" | "video/webm" => "webm",
        "video/mp4" => "mp4",
        "video/x-m4v" => "m4v",
        "video/quicktime" => "mov",
        "video/x-matroska" => "mkv",
        "video/x-msvideo" | "video/avi" | "video/msvideo" => "avi",
        "video/x-ms-wmv" => "wmv",
        "video/mpeg" => "mpg",
        "video/3gpp" | "audio/3gpp" => "3gp",
        "video/3gpp2" | "audio/3gpp2" => "3g2",
        "video/ogg" => "ogv",
        "video/x-flv" => "flv",
        _ => return None,
    })
}

/// A publication date: the day as the feed writes it (in its own time zone), and the moment in
/// Unix seconds, which orders episodes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Date {
    year: i64,
    month: u32,
    day: u32,
    unix: i64,
}

impl Date {
    /// `seconds` into the day, less the time zone's `offset` (both in seconds). None for a year
    /// outside 1 to 9999, which a name cannot show in four digits and a hostile feed could make
    /// overflow.
    fn new(year: i64, month: u32, day: u32, seconds: i64, offset: i64) -> Option<Self> {
        ((1..=9999).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day))
            .then(|| Self { year, month, day, unix: days_from_civil(year, month, day) * 86_400 + seconds - offset })
    }
}

/// Days from 1970-01-01 to the given day of the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let (month, day) = (i64::from(month), i64::from(day));
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// An RSS date (RFC 822/2822, "Sat, 27 Sep 2026 10:00:00 GMT") or an Atom one (RFC 3339,
/// "2026-09-27T10:00:00Z"); feeds use either in either place.
fn parse_date(text: &str) -> Option<Date> {
    let text = text.trim();
    rfc3339(text).or_else(|| rfc2822(text))
}

fn rfc2822(text: &str) -> Option<Date> {
    let text = text.split_once(',').map_or(text, |(_, rest)| rest);
    let mut parts = text.split_whitespace();
    let day = parts.next()?.parse().ok()?;
    let month = parts.next()?.get(..3)?.to_ascii_lowercase();
    let month = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"].iter().position(|m| *m == month)?;
    let year: i64 = parts.next()?.parse().ok()?;
    let year = match year {
        0..=49 => year + 2000,
        50..=99 => year + 1900,
        _ => year,
    };
    let seconds = parts.next().map_or(Some(0), time_of_day)?;
    let offset = parts.next().map_or(0, zone_offset);
    Date::new(year, month as u32 + 1, day, seconds, offset)
}

fn rfc3339(text: &str) -> Option<Date> {
    let date = text.get(..10)?;
    let mut fields = date.split('-');
    let year = fields.next().filter(|y| y.len() == 4)?.parse().ok()?;
    let month = fields.next().filter(|m| m.len() == 2)?.parse().ok()?;
    let day = fields.next().filter(|d| d.len() == 2)?.parse().ok()?;
    let rest = text[10..].trim_start_matches(['T', 't', ' ']);
    let zone_at = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let seconds = if rest.is_empty() { 0 } else { time_of_day(&rest[..zone_at])? };
    Date::new(year, month, day, seconds, zone_offset(&rest[zone_at..]))
}

/// Seconds into the day of "hh:mm" or "hh:mm:ss" (a fraction of a second is dropped); None
/// past 24:59:60.
fn time_of_day(text: &str) -> Option<i64> {
    let mut fields = text.split(':');
    let hours: u8 = fields.next()?.parse().ok().filter(|&h| h <= 24)?;
    let minutes: u8 = fields.next()?.parse().ok().filter(|&m| m <= 59)?;
    let seconds: u8 = fields.next().map_or(Some(0), |s| s.split('.').next()?.parse().ok()).filter(|&s| s <= 60)?;
    Some(i64::from(hours) * 3600 + i64::from(minutes) * 60 + i64::from(seconds))
}

/// The offset from UTC in seconds of a zone written "+hhmm", "-hh:mm", "Z", "GMT" or as a North
/// American zone ("EST", "PDT"); 0 for any other.
fn zone_offset(zone: &str) -> i64 {
    let zone = zone.trim();
    if let Some((sign, digits)) = zone.strip_prefix('+').map(|d| (1, d)).or_else(|| zone.strip_prefix('-').map(|d| (-1, d))) {
        let digits = digits.replace(':', "");
        let (hours, minutes) = (digits.get(..2).and_then(|h| h.parse::<i64>().ok()), digits.get(2..4).and_then(|m| m.parse::<i64>().ok()));
        return sign * (hours.unwrap_or(0) * 3600 + minutes.unwrap_or(0) * 60);
    }
    let hours = match zone.to_ascii_uppercase().as_str() {
        "EDT" => -4,
        "EST" | "CDT" => -5,
        "CST" | "MDT" => -6,
        "MST" | "PDT" => -7,
        "PST" => -8,
        _ => 0,
    };
    hours * 3600
}

/// The tasks of `feed`'s episodes as `options` say, reading the download history and archive
/// for `only_new` off the runtime's threads.
async fn show_tasks(feed: Feed, options: &ListOptions) -> Result<Vec<Task>, String> {
    let done = if options.only_new {
        let read = tokio::task::spawn_blocking(|| {
            let archive = crate::media::archive_file().map(|file| crate::media::read_archive(&file)).transpose()?;
            Ok::<_, String>(Done { archive: archive.unwrap_or_default(), ..Done::of(DownloadHistoryManager::load().entries()) })
        });
        read.await.map_err(|e| format!("Background task failed: {e}"))??
    } else {
        Done::default()
    };
    feed_tasks(feed, options.latest, &done)
}

/// The download archive's lines of the episode `task` of the feed `source` (see [`Feed`])
/// downloads (see `media::archive_file`), as [`Done`] tells it: its link, but for one that holds
/// a secret, which the archive is no place for, and its file in that feed, so another show of
/// the same title and an episode of its of the same day and title is not taken for it. The feed
/// is named by a digest of `source`, not by its link: a private feed's token may be in the path
/// (Supercast's `/feeds/<token>`), which [`redact_url`] leaves.
fn episode_archive(source: &str, task: &Task) -> Vec<String> {
    let links = task.urls.iter().map(Url::as_str).filter(|url| redact_url(url) == *url).map(|url| format!("feed {url}"));
    let feed = blake3::hash(source.as_bytes()).to_hex();
    let file = file_of(task).map(|file| format!("feed-file {} {file}", &feed[..32]));
    links.chain(file).collect()
}

/// "folder/name" of the file `task` saves to, in lower case; None for a feed without a title.
fn file_of(task: &Task) -> Option<String> {
    match (&task.folder, &task.name) {
        (Some(folder), Some(name)) => Some(format!("{}/{}", lower(folder), lower(name))),
        _ => None,
    }
}

fn lower(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// What history and the download archive record as downloaded, which tells the episodes
/// downloaded before. History keeps only its newest entries; the archive keeps every line.
#[derive(Default)]
struct Done {
    /// Links, as history saves them, but for those it took a secret out of: the secret may be
    /// what told one episode's link from another's (`download?key=EP1`, `?key=EP2`).
    links: HashSet<String>,
    /// Files, as the names of their folder and their own in lower case (Windows compares names
    /// so): an episode whose link changed (a new tracking prefix or query) is still in its
    /// show's folder under its name.
    files: HashSet<(String, String)>,
    /// The download archive's lines (see [`episode_archive`]).
    archive: HashSet<String>,
}

impl Done {
    fn of(entries: &[HistoryEntry]) -> Self {
        let mut done = Self::default();
        for entry in entries.iter().filter(|e| e.status == HistoryStatus::Completed) {
            done.links.extend(entry.urls.iter().filter(|u| !is_redacted(u)).cloned());
            if let (Some(folder), Some(file)) = (entry.file_path.parent().and_then(Path::file_name), entry.file_path.file_name()) {
                done.files.insert((folder.to_string_lossy().to_lowercase(), file.to_string_lossy().to_lowercase()));
            }
        }
        done
    }

    /// Whether the episode `task` downloads was downloaded before: from one of its links, or
    /// into the file it names.
    fn has(&self, task: &Task) -> bool {
        task.urls.iter().any(|url| self.links.contains(&redact_url(url.as_str())))
            || matches!((&task.folder, &task.name), (Some(folder), Some(name)) if self.files.contains(&(lower(folder), lower(name))))
            || task.archive.iter().any(|line| self.archive.contains(line))
            // The line of a file an archive noted before it named the feed.
            || file_of(task).is_some_and(|file| self.archive.contains(&format!("feed-file {file}")))
    }
}

/// One task per episode of `feed`, newest first (in the feed's order where dates are missing or
/// equal), an enclosure listed twice once: the newest `latest` of them, less those `done` has.
/// Names are given over the whole feed, so an episode keeps its name from one listing to the
/// next. None left is nothing new to do, not an error (see `ingest::ingest`).
fn feed_tasks(feed: Feed, latest: Option<usize>, done: &Done) -> Result<Vec<Task>, String> {
    let show = feed.title.as_deref().unwrap_or("the feed");
    let folder = feed.title.as_deref().and_then(|title| clean_path([title]).ok());
    let mut episodes = feed.episodes;
    episodes.sort_by_key(|e| std::cmp::Reverse(e.date.map(|d| d.unix)));
    let (mut seen, mut names) = (HashSet::new(), HashSet::new());
    let mut tasks = Vec::new();
    for episode in episodes {
        let Some(enclosure) = episode.enclosure.filter(|e| seen.insert(e.url.clone())) else { continue };
        let name = episode_name(&episode.title, episode.date, enclosure.extension, &enclosure.url, &mut names)?;
        let mut task = Task {
            urls: vec![enclosure.url],
            name: Some(name),
            folder: folder.clone(),
            size: enclosure.length,
            // Its host is not the feed's: the Authorization the user gave is not sent there.
            from_document: true,
            ..Task::default()
        };
        task.archive = episode_archive(&feed.source, &task);
        tasks.push(task);
    }
    let listed = tasks.len();
    if let Some(latest) = latest {
        tasks.truncate(latest);
    }
    let considered = tasks.len();
    tasks.retain(|task| !done.has(task));
    if tasks.is_empty() {
        let which = if considered < listed { format!("the newest {} of its {}", considered, listed) } else { format!("all {} of its", listed) };
        tracing::info!("Nothing new in {}: {} episodes were downloaded before", show, which);
    }
    Ok(tasks)
}

/// "YYYY-MM-DD Title.ext", cleaned for every OS, with " (2)" and up added to a name `taken`
/// already has (compared ignoring case, as Windows does); an episode without a title (or with
/// dots alone) is named after its file.
fn episode_name(title: &str, date: Option<Date>, extension: &str, url: &Url, taken: &mut HashSet<String>) -> Result<PathBuf, String> {
    let file = url.path_segments().and_then(|mut s| s.next_back()).map_or("", |last| last.rsplit_once('.').map_or(last, |(stem, _)| stem));
    let title = [title, file].into_iter().map(shorten).find(|t| !t.is_empty()).unwrap_or("Episode");
    let stem = match date {
        Some(d) => format!("{:04}-{:02}-{:02} {}", d.year, d.month, d.day, title),
        None => title.to_string(),
    };
    let mut n = 1;
    loop {
        let suffix = if n == 1 { String::new() } else { format!(" ({})", n) };
        let name = clean_path([format!("{}{}.{}", stem, suffix, extension).as_str()])?;
        if taken.insert(name.to_string_lossy().to_lowercase()) {
            return Ok(name);
        }
        n += 1;
    }
}

/// `title` cut to at most [`MAX_TITLE_BYTES`], without the dots and spaces it would then end in:
/// "Coming Clean." is not saved as "Coming Clean..mp3".
fn shorten(title: &str) -> &str {
    let cut = (0..=MAX_TITLE_BYTES.min(title.len())).rev().find(|&i| title.is_char_boundary(i)).unwrap_or(0);
    title[..cut].trim_end_matches(|c: char| c == '.' || c.is_whitespace())
}

/// An Apple Podcasts show link (podcasts.apple.com/{country}/podcast/{name}/id{show}), with the
/// episode it opens on (`?i=`) if any.
#[derive(Debug, PartialEq)]
struct AppleLink {
    show: u64,
    episode: Option<u64>,
    country: Option<String>,
}

impl AppleLink {
    fn of(url: &Url) -> Option<Self> {
        let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
        if host != "podcasts.apple.com" && host != "itunes.apple.com" {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        if !segments.contains(&"podcast") {
            return None;
        }
        let show = segments.last()?.strip_prefix("id")?.parse().ok()?;
        let episode = url.query_pairs().find(|(k, _)| k == "i").and_then(|(_, v)| v.parse().ok());
        let country = segments.first().filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())).map(|c| c.to_ascii_lowercase());
        Some(Self { show, episode, country })
    }

    /// The show's public feed, found through the keyless iTunes Lookup API at `api`, or the
    /// episode the link opens on, found in that feed by what the API says of it. A show whose
    /// feed Apple does not publish (for subscribers only, or hidden by its publisher) lists the
    /// newest episodes the API gives a public file for, and an episode link to it that episode.
    /// None for an episode the API or the feed does not know: the engine hands the link to
    /// yt-dlp, which reads Apple's own page.
    async fn list(&self, http: &reqwest::Client, api: &str, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
        let lookup = match self.lookup(http, api, self.episode.is_some()).await {
            Ok(lookup) => lookup,
            Err(e) => return Some(Err(e)),
        };
        match (&lookup.feed, self.episode) {
            (Some(feed_url), None) => {
                let feed = match read_feed(http.get(feed_url.clone()), feed_url, true).await {
                    Ok(Some(feed)) => feed,
                    Ok(None) => return Some(Err(format!("The show's feed ({}) is not a podcast feed", redact_url(feed_url.as_str())))),
                    Err(e) => return Some(Err(e)),
                };
                if feed.episodes.is_empty() {
                    return Some(Err(format!("{} lists no episodes", feed.title.as_deref().unwrap_or("The show's feed"))));
                }
                Some(show_tasks(feed, options).await)
            }
            (Some(feed_url), Some(track)) => {
                let wanted = lookup.episodes.iter().find(|e| e.track == Some(track))?;
                let feed = match read_feed(http.get(feed_url.clone()), feed_url, true).await {
                    Ok(Some(feed)) => feed,
                    Ok(None) => return None,
                    Err(e) => return Some(Err(e)),
                };
                let found = feed.episodes.into_iter().find(|e| wanted.matches(e))?;
                Some(feed_tasks(Feed { episodes: vec![found], ..feed }, None, &Done::default()))
            }
            (None, Some(track)) => {
                let found = lookup.episodes.iter().find(|e| e.track == Some(track)).and_then(LookupEpisode::episode)?;
                Some(feed_tasks(Feed { title: lookup.name, episodes: vec![found], source: self.source() }, None, &Done::default()))
            }
            (None, None) => {
                // The show's answer holds no episodes: ask for them.
                let lookup = match self.lookup(http, api, true).await {
                    Ok(lookup) => lookup,
                    Err(e) => return Some(Err(e)),
                };
                let episodes: Vec<Episode> = lookup.episodes.iter().filter_map(LookupEpisode::episode).collect();
                if episodes.is_empty() {
                    return Some(Err(format!(
                        "{} is not available: Apple does not publish its feed (it may be for subscribers only, or its publisher hid it), \
                         and lists no episode with a public file. A link to one of its episodes (with ?i=) may still download.",
                        lookup.name.as_deref().unwrap_or("This show")
                    )));
                }
                tracing::info!("Apple does not publish show {}'s feed: its newest {} episodes are listed", self.show, episodes.len());
                Some(show_tasks(Feed { title: lookup.name, episodes, source: self.source() }, options).await)
            }
        }
    }

    /// The [`Feed::source`] of a show whose feed Apple does not publish.
    fn source(&self) -> String {
        format!("https://podcasts.apple.com/podcast/id{}", self.show)
    }

    /// What the iTunes Lookup API at `api` says of the show, and of its newest episodes (200 at
    /// most) when `episodes` are asked for.
    async fn lookup(&self, http: &reqwest::Client, api: &str, episodes: bool) -> Result<Lookup, String> {
        let mut query = vec![("id", self.show.to_string())];
        if episodes {
            query.extend([("entity", "podcastEpisode".to_string()), ("limit", "200".to_string())]);
        } else {
            query.push(("entity", "podcast".to_string()));
        }
        if let Some(country) = &self.country {
            query.push(("country", country.clone()));
        }
        let url = Url::parse_with_params(api, &query).map_err(|e| e.to_string())?;
        match fetch(http.get(url.clone()), &url, false, true).await? {
            Some(answer) => read_lookup(&answer.body, self.show),
            None => Err(format!("Apple Podcasts refused to look up show {}; try again later", self.show)),
        }
    }
}

/// What the iTunes Lookup API says of a show.
#[derive(Debug)]
struct Lookup {
    name: Option<String>,
    /// Its public feed; None when Apple does not publish one.
    feed: Option<Url>,
    /// Its newest episodes, when they were asked for.
    episodes: Vec<LookupEpisode>,
}

/// What the iTunes Lookup API says of an episode: its id, and its guid and file as its feed
/// gives them (a subscription episode has no public file).
#[derive(Debug, Default, PartialEq)]
struct LookupEpisode {
    track: Option<u64>,
    guid: Option<String>,
    url: Option<Url>,
    title: String,
    date: Option<Date>,
    extension: Option<&'static str>,
}

impl LookupEpisode {
    fn of(result: &LookupResult) -> Self {
        let url = result.episode_url.as_deref().and_then(|u| Url::parse(u).ok()).filter(|u| matches!(u.scheme(), "http" | "https"));
        let extension = url.as_ref().and_then(|url| {
            let named = result.episode_file_extension.as_deref().and_then(|ext| MEDIA_EXTENSIONS.iter().copied().find(|known| known.eq_ignore_ascii_case(ext)));
            // "audio" or "video".
            let kind = result.episode_content_type.as_deref().unwrap_or("audio");
            named.or_else(|| media_extension(url, Some(&format!("{}/", kind))))
        });
        Self {
            track: result.track_id,
            guid: result.episode_guid.clone(),
            url,
            title: result.track_name.as_deref().map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default(),
            date: result.release_date.as_deref().and_then(parse_date),
            extension,
        }
    }

    /// Whether `episode` of the feed is this one: the same guid, or the same file (its link's
    /// host and path; the query carries tracking that may differ).
    fn matches(&self, episode: &Episode) -> bool {
        let same_guid = self.guid.is_some() && self.guid == episode.guid;
        let same_file = match (&self.url, &episode.enclosure) {
            (Some(url), Some(enclosure)) => url.host_str() == enclosure.url.host_str() && url.path() == enclosure.url.path(),
            _ => false,
        };
        same_guid || same_file
    }

    /// The episode as a feed would list it, when it has a public audio or video file.
    fn episode(&self) -> Option<Episode> {
        let enclosure = Enclosure { url: self.url.clone()?, extension: self.extension?, length: None };
        Some(Episode { title: self.title.clone(), date: self.date, guid: self.guid.clone(), enclosure: Some(enclosure) })
    }
}

#[derive(Deserialize)]
struct LookupAnswer {
    #[serde(default)]
    results: Vec<LookupResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LookupResult {
    wrapper_type: Option<String>,
    kind: Option<String>,
    collection_id: Option<u64>,
    track_id: Option<u64>,
    collection_name: Option<String>,
    feed_url: Option<String>,
    track_name: Option<String>,
    release_date: Option<String>,
    episode_guid: Option<String>,
    episode_url: Option<String>,
    episode_file_extension: Option<String>,
    episode_content_type: Option<String>,
}

/// What an iTunes Lookup `answer` says of `show` and the episodes of it the answer lists. An
/// error when it does not know the show.
fn read_lookup(answer: &[u8], show: u64) -> Result<Lookup, String> {
    let answer: LookupAnswer = serde_json::from_slice(answer).map_err(|e| format!("Apple Podcasts answered the lookup of show {} with {}", show, e))?;
    let Some(record) = answer.results.iter().find(|r| r.kind.as_deref() == Some("podcast") && r.collection_id == Some(show)) else {
        return Err(format!("Apple Podcasts has no show with id {} (it may have been removed, or be listed in another country only)", show));
    };
    let feed = record.feed_url.as_deref().and_then(|feed| Url::parse(feed).ok()).filter(|u| matches!(u.scheme(), "http" | "https"));
    let episodes = answer
        .results
        .iter()
        .filter(|r| r.wrapper_type.as_deref() == Some("podcastEpisode") && r.collection_id == Some(show))
        .map(LookupEpisode::of)
        .collect();
    Ok(Lookup { name: record.collection_name.clone().filter(|n| !n.trim().is_empty()), feed, episodes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// Feeds as the podcast hosts publish them (taken from the iTunes directory), Apple Podcasts
    /// show and episode links, and links to other things.
    #[test]
    fn feeds_and_apple_links_are_known_by_their_shape() {
        for feed in [
            "https://feeds.simplecast.com/Sl5CSM3S",
            "https://feeds.megaphone.fm/ADL8973993370",
            "https://cybersecuritytoday.libsyn.com/rss",
            "https://feeds.libsyn.com/630487/rss",
            "https://rss.buzzsprout.com/1004689.rss",
            "https://rss.art19.com/absolutely-not",
            "https://access.acast.com/rss/6215faef4b795a5d1ffd3b62",
            "https://feeds.acast.com/public/shows/6215faef4b795a5d1ffd3b62",
            "https://www.omnycontent.com/d/playlist/e73c998e/d02e7811/659ad4ad/podcast.rss",
            "https://feeds.transistor.fm/techsurge-deep-tech-podcast",
            "https://feed.podbean.com/HDSR/feed.xml",
            "https://www.spreaker.com/show/4836142/episodes/feed",
            "https://feeds.soundcloud.com/users/soundcloud:users:213371008/sounds.rss",
            "https://anchor.fm/s/101adcf44/podcast/rss",
            "https://feeds.captivate.fm/et-morning-brief/",
            "https://podcasts.files.bbci.co.uk/b006qykl.rss",
            "https://www.patreon.com/rss/somecreator?auth=abc123",
            "https://show.supercast.com/feeds/Tok3n",
            "https://example.org/blog/atom.atom",
            "https://example.org/show/podcast/",
            "https://podcastfeeds.nbcnews.com/l7jK75d0",
            "https://leftrightandcenter-feed.kcrw.com",
            "https://www.arnewsline.org/?format=rss",
            "https://example.org/?feed=podcast",
            "https://podcasts.apple.com/us/podcast/the-daily/id1200361736",
            "https://podcasts.apple.com/podcast/id1200361736",
            "https://podcasts.apple.com/gb/podcast/the-daily/id1200361736?i=1000791857941",
            "https://itunes.apple.com/us/podcast/the-daily/id1200361736?mt=2",
        ] {
            assert!(lists(&url(feed)), "{feed}");
        }
        for other in [
            "https://example.org/files/release.zip",
            "https://example.org/podcasts/episode-1.mp3",
            "https://www.youtube.com/watch?v=abc",
            "https://podcasts.apple.com/us/browse",
            "https://podcasts.apple.com/us/podcast/the-daily",
            "https://apple.com/us/podcast/x/id123",
            "https://example.org/list?format=json",
            "https://feedback.example.com/files/app.zip",
            "https://datafeed.example.com/products/dump.zip",
            "https://feedly.com/i/latest",
        ] {
            assert!(!lists(&url(other)), "{other}");
        }
        // Only feeds have these shapes; files of other kinds share the others, so a busy or
        // unreachable host is left to the engine (see `fetch`).
        for feed in [
            "https://feeds.simplecast.com/Sl5CSM3S",
            "https://rss.art19.com/absolutely-not",
            "https://feed.podbean.com/HDSR/feed.xml",
            "https://podcastfeeds.nbcnews.com/l7jK75d0",
            "https://leftrightandcenter-feed.kcrw.com",
            "https://podcasts.files.bbci.co.uk/b006qykl.rss",
            "https://example.org/blog/atom.atom",
            "https://www.arnewsline.org/?format=rss",
            "https://anchor.fm/s/101adcf44/podcast/rss",
            "https://cybersecuritytoday.libsyn.com/rss",
            "https://access.acast.com/rss/6215faef4b795a5d1ffd3b62",
            "https://www.spreaker.com/show/4836142/episodes/feed",
            "https://show.supercast.com/feeds/Tok3n",
            "https://www.patreon.com/rss/somecreator?auth=abc123",
        ] {
            assert_eq!(feed_shape(&url(feed)), Some(true), "{feed}");
        }
        for maybe in [
            "https://github.com/someone/rss",
            "https://example.org/data/books.xml",
            "https://example.org/show/podcast/",
            "https://example.org/news/feed",
            "https://notlibsyn.com/rss",
        ] {
            assert_eq!(feed_shape(&url(maybe)), Some(false), "{maybe}");
        }
        let episode = AppleLink::of(&url("https://podcasts.apple.com/GB/podcast/the-daily/id1200361736?i=1000791857941")).unwrap();
        assert_eq!(episode, AppleLink { show: 1_200_361_736, episode: Some(1_000_791_857_941), country: Some("gb".into()) });
        let show = AppleLink::of(&url("https://podcasts.apple.com/podcast/id1200361736")).unwrap();
        assert_eq!(show, AppleLink { show: 1_200_361_736, episode: None, country: None });
    }

    /// The start of The Daily's feed as Simplecast serves it, cut to two episodes, followed by
    /// episodes that test the edges: a relative enclosure found by its extension, a transcript
    /// and an episode without an enclosure, two episodes of one name and day found by their
    /// type, and an enclosure listed twice.
    const DAILY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
  <channel>
    <atom:link href="https://feeds.simplecast.com/Sl5CSM3S" rel="self" title="MP3 Audio" type="application/atom+xml"/>
    <title>The Daily</title>
    <pubDate>Sun, 27 Sep 2026 10:00:00 +0000</pubDate>
    <image>
      <title>Not the show's title</title>
      <url>https://image.simplecastcdn.com/images/art.jpg?aid=rss_feed</url>
    </image>
    <item>
      <guid isPermaLink="false">a5194659-370d-42e0-bc0b-c320072b24e6</guid>
      <title>Sylvester Stallone Hid His Struggles for Decades. Now He’s Coming Clean.</title>
      <pubDate>Sat, 26 Sep 2026 10:00:00 +0000</pubDate>
      <enclosure length="76111894" type="audio/mpeg" url="https://dts.podtrac.com/redirect.mp3/pdst.fm/e/pfx.vpixl.com/6qj4J/pscrb.fm/rss/p/nyt.simplecastaudio.com/03d8b493-87fc-4bd1-931f-8a8e9b945d8a/episodes/3c0a5121-a722-4a1e-9b11-b78033a37931/audio/128/default.mp3?aid=rss_feed&amp;awCollectionId=03d8b493-87fc-4bd1-931f-8a8e9b945d8a&amp;awEpisodeId=3c0a5121-a722-4a1e-9b11-b78033a37931&amp;feed=Sl5CSM3S"/>
      <itunes:title>Stallone</itunes:title>
    </item>
    <item>
      <guid isPermaLink="false">d9759ebd-1c66-4ffd-907d-f40e391e2a01</guid>
      <title>The Best TV Shows of the 21st Century</title>
      <pubDate>Sun, 27 Sep 2026 10:00:00 +0000</pubDate>
      <enclosure length="54994347" type="audio/mpeg" url="https://dts.podtrac.com/redirect.mp3/pdst.fm/e/pfx.vpixl.com/6qj4J/pscrb.fm/rss/p/nyt.simplecastaudio.com/03d8b493-87fc-4bd1-931f-8a8e9b945d8a/episodes/11b83b1f-3a09-4400-a504-507be826a493/audio/128/default.mp3?aid=rss_feed&amp;awCollectionId=03d8b493-87fc-4bd1-931f-8a8e9b945d8a&amp;awEpisodeId=11b83b1f-3a09-4400-a504-507be826a493&amp;feed=Sl5CSM3S"/>
    </item>
    <item>
      <title><![CDATA[Bonus: Q&A / "Live"]]></title>
      <pubDate>Thu, 24 Sep 2026 23:30:00 -0700</pubDate>
      <enclosure url="/audio/bonus.m4a" length="0"/>
    </item>
    <item>
      <title>Transcript only</title>
      <pubDate>Wed, 23 Sep 2026 10:00:00 GMT</pubDate>
      <enclosure url="https://cdn.example/transcript.pdf" type="application/pdf" length="10"/>
    </item>
    <item><title>No audio</title><pubDate>Tue, 22 Sep 2026 10:00:00 GMT</pubDate></item>
    <item>
      <title>Repeat</title>
      <pubDate>Mon, 21 Sep 2026 10:00:00 GMT</pubDate>
      <enclosure url="https://cdn.example/play?id=7" type="video/mp4" length="99"/>
    </item>
    <item>
      <title>Repeat</title>
      <pubDate>Mon, 21 Sep 2026 09:00:00 GMT</pubDate>
      <enclosure url="https://cdn.example/play?id=8" type="Video/MP4; codecs=avc1"/>
    </item>
    <item>
      <title>Listed twice</title>
      <pubDate>Mon, 21 Sep 2026 08:00:00 GMT</pubDate>
      <enclosure url="https://cdn.example/play?id=7" type="video/mp4"/>
    </item>
  </channel>
</rss>"#;

    /// The feed `text` at `base`, as [`read_feed`] reads it.
    fn parse(text: &str, base: &str) -> Option<Result<Feed, String>> {
        Some(parse_feed(text, &url(base))?.map(|feed| Feed { source: redact_url(base), ..feed }))
    }

    fn names(tasks: &[Task]) -> Vec<String> {
        tasks.iter().map(|t| t.name.as_ref().unwrap().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn an_rss_feed_lists_its_audio_and_video_newest_first() {
        let feed = parse(DAILY, "https://feeds.simplecast.com/Sl5CSM3S?token=s3cret").unwrap().unwrap();
        assert_eq!(feed.title.as_deref(), Some("The Daily"));
        let tasks = feed_tasks(feed, None, &Done::default()).unwrap();
        assert_eq!(
            names(&tasks),
            [
                "2026-09-27 The Best TV Shows of the 21st Century.mp3",
                "2026-09-26 Sylvester Stallone Hid His Struggles for Decades. Now He’s Coming Clean.mp3",
                "2026-09-24 Bonus_ Q&A _ _Live_.m4a",
                "2026-09-21 Repeat.mp4",
                "2026-09-21 Repeat (2).mp4",
            ]
        );
        assert!(tasks[0].urls[0].as_str().ends_with(
            "/default.mp3?aid=rss_feed&awCollectionId=03d8b493-87fc-4bd1-931f-8a8e9b945d8a&awEpisodeId=11b83b1f-3a09-4400-a504-507be826a493&feed=Sl5CSM3S"
        ));
        // A relative enclosure is on the feed's host, without the feed's query and its token.
        assert_eq!(tasks[2].urls, [url("https://feeds.simplecast.com/audio/bonus.m4a")]);
        assert!(tasks.iter().all(|t| !t.urls[0].as_str().contains("s3cret")));
        assert_eq!(tasks.iter().map(|t| t.size).collect::<Vec<_>>(), [Some(54_994_347), Some(76_111_894), None, Some(99), None]);
        assert!(tasks.iter().all(|t| t.folder.as_deref() == Some(Path::new("The Daily")) && t.from_document));
    }

    /// A completed download history records: `file` (a path under a download folder) from `link`.
    fn downloaded(file: &str, link: &str) -> HistoryEntry {
        let path = Path::new("downloads").join(file);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        HistoryEntry::new(name, path, 1, vec![link.to_string()])
    }

    /// The newest N, less what history has downloaded: from the same link, as history keeps it,
    /// or into the same file in the show's folder. Names do not change with what is left out.
    #[test]
    fn latest_and_only_new_pick_the_episodes() {
        let feed = || parse(DAILY, "https://feeds.simplecast.com/Sl5CSM3S").unwrap().unwrap();
        assert_eq!(feed_tasks(feed(), Some(2), &Done::default()).unwrap().len(), 2);
        let newest = feed_tasks(feed(), Some(1), &Done::default()).unwrap().remove(0).urls.remove(0);
        let mut failed = downloaded("elsewhere/x.mp3", "https://cdn.example/play?id=8");
        failed.status = HistoryStatus::Failed("reset".into());
        let done = Done::of(&[downloaded("elsewhere/a.mp3", newest.as_str()), downloaded("elsewhere/b.mp4", "https://cdn.example/play?id=7"), failed]);
        let left = feed_tasks(feed(), None, &done).unwrap();
        assert_eq!(names(&left).len(), 3);
        assert!(names(&left)[0].starts_with("2026-09-26 "));
        assert_eq!(names(&left)[2], "2026-09-21 Repeat (2).mp4");
        // Nothing new is nothing to do, as a playlist's is.
        assert!(feed_tasks(feed(), Some(1), &done).unwrap().is_empty());
    }

    /// A link history took a secret out of matches no episode, as the secret may be what told
    /// episodes apart; the file an episode was saved to still does, whatever its link is now.
    #[test]
    fn only_new_goes_by_file_where_links_cannot_tell() {
        let private = r#"<rss><channel><title>Members</title>
<item><title>One</title><enclosure type="audio/mpeg" url="https://h.example/download?key=EP1"/></item>
<item><title>Two</title><enclosure type="audio/mpeg" url="https://h.example/download?key=EP2"/></item>
</channel></rss>"#;
        let feed = || parse(private, "https://f.example/rss").unwrap().unwrap();
        let done = Done::of(&[downloaded("Members/One.mp3", "https://h.example/download?key=EP1")]);
        assert!(done.links.is_empty());
        assert_eq!(names(&feed_tasks(feed(), None, &done).unwrap()), ["Two.mp3"]);
        // The same link with another key, saved elsewhere, is no match.
        let done = Done::of(&[downloaded("Other/Three.mp3", "https://h.example/download?key=EP9")]);
        assert_eq!(names(&feed_tasks(feed(), None, &done).unwrap()), ["One.mp3", "Two.mp3"]);

        // A new tracking prefix: the episode is in its show's folder under its name (in any case).
        let tracked = r#"<rss><channel><title>Show</title><item><title>Ep</title><pubDate>Thu, 01 Jan 2026 08:00:00 GMT</pubDate>
<enclosure type="audio/mpeg" url="https://tracking.example/new-prefix/cdn.example/ep.mp3?updated=2"/></item></channel></rss>"#;
        let feed = || parse(tracked, "https://f.example/show.rss").unwrap().unwrap();
        let done = Done::of(&[downloaded("SHOW/2026-01-01 ep.MP3", "https://tracking.example/old-prefix/cdn.example/ep.mp3")]);
        assert!(feed_tasks(feed(), None, &done).unwrap().is_empty());
        let done = Done::of(&[downloaded("Another show/2026-01-01 Ep.mp3", "https://tracking.example/old-prefix/cdn.example/ep.mp3")]);
        assert_eq!(feed_tasks(feed(), None, &done).unwrap().len(), 1);
    }

    /// Each episode carries its lines for the download archive, which keeps them after history
    /// has let the download go: its link (but one with a secret) and its file in its feed (a
    /// digest of the feed's link), in lower case. A line an older archive has for the file alone
    /// still counts.
    #[test]
    fn the_download_archive_tells_episodes_history_forgot() {
        let show = r#"<rss><channel><title>Show</title>
<item><title>One</title><enclosure type="audio/mpeg" url="https://cdn.example/1.mp3"/></item>
<item><title>Two</title><enclosure type="audio/mpeg" url="https://h.example/download?key=EP2"/></item>
</channel></rss>"#;
        let feed = || parse(show, "https://f.example/rss").unwrap().unwrap();
        let tasks = feed_tasks(feed(), None, &Done::default()).unwrap();
        let lines: Vec<_> = tasks.iter().map(|task| task.archive.clone()).collect();
        // "https://f.example/rss", by its BLAKE3 hash's first 16 bytes.
        let file = |name: &str| format!("feed-file 5b934f15a443a3e1a95f894dd3436ef4 show/{name}");
        assert_eq!(lines, [vec!["feed https://cdn.example/1.mp3".to_string(), file("one.mp3")], vec![file("two.mp3")]]);
        let done = |line: &str| Done { archive: HashSet::from([line.to_string()]), ..Done::default() };
        assert_eq!(names(&feed_tasks(feed(), None, &done("feed https://cdn.example/1.mp3")).unwrap()), ["Two.mp3"]);
        assert_eq!(names(&feed_tasks(feed(), None, &done(&file("two.mp3"))).unwrap()), ["One.mp3"]);
        assert_eq!(names(&feed_tasks(feed(), None, &done("feed-file show/two.mp3")).unwrap()), ["One.mp3"]);
        assert_eq!(feed_tasks(feed(), None, &done("feed https://cdn.example/2.mp3")).unwrap().len(), 2);
    }

    /// Two feeds of one title, each with an episode of the same day and title on a link of its
    /// own: the one downloaded from the first feed does not stand for the second's. A private
    /// feed's token, in its query or its path, is not written to the archive, and a new token in
    /// the query is the same feed.
    #[test]
    fn a_feed_of_the_same_title_has_episodes_of_its_own() {
        let show = |n: u8| {
            format!(
                r#"<rss><channel><title>News</title><item><title>Today</title><pubDate>Mon, 01 Jun 2026 08:00:00 GMT</pubDate>
<enclosure type="audio/mpeg" url="https://h.example/download?key=EP{n}"/></item></channel></rss>"#
            )
        };
        let archive = |n: u8, feed: &str| feed_tasks(parse(&show(n), feed).unwrap().unwrap(), None, &Done::default()).unwrap().remove(0).archive;
        let archived = archive(1, "https://a.example/news.rss?token=s3cret");
        assert_eq!(archived.len(), 1);
        assert!(archived[0].starts_with("feed-file ") && archived[0].ends_with(" news/2026-06-01 today.mp3"), "{archived:?}");
        let done = Done { archive: archived.iter().cloned().collect(), ..Done::default() };
        let second = parse(&show(2), "https://b.example/news.rss").unwrap().unwrap();
        assert_eq!(names(&feed_tasks(second, None, &done).unwrap()), ["2026-06-01 Today.mp3"]);
        let first = parse(&show(1), "https://a.example/news.rss?token=0ther").unwrap().unwrap();
        assert!(feed_tasks(first, None, &done).unwrap().is_empty());
        for (feed, token) in [("https://a.example/news.rss?token=s3cret", "s3cret"), ("https://show.supercast.com/feeds/Tok3n", "Tok3n")] {
            let line = &archive(1, feed)[0];
            assert!(!line.contains(token) && !line.contains(".example") && !line.contains("supercast"), "{line}");
        }
        assert_ne!(archive(1, "https://show.supercast.com/feeds/Tok3n"), archived);
    }

    #[test]
    fn an_atom_feed_lists_its_enclosure_links() {
        let atom = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title type="text">Atom &amp; Eve</title>
  <entry>
    <title>First</title>
    <id>urn:1</id>
    <updated>2026-01-05T10:00:00Z</updated>
    <published>2026-01-02T23:00:00-05:00</published>
    <link rel="alternate" href="https://a.example/first.html"/>
    <link rel="enclosure" href="https://a.example/first.opus" type="audio/opus" length="12"/>
  </entry>
  <entry>
    <title>Second</title>
    <updated>2026-01-03T05:00:00Z</updated>
    <link rel="enclosure" href="media/second" type="audio/ogg"/>
  </entry>
  <entry><title>Page only</title><link href="https://a.example/page.mp3"/></entry>
</feed>"#;
        let feed = parse(atom, "https://a.example/feeds/show.atom").unwrap().unwrap();
        assert_eq!(feed.episodes[0].guid.as_deref(), Some("urn:1"));
        let tasks = feed_tasks(feed, None, &Done::default()).unwrap();
        // Published at 04:00 UTC on the 3rd, the first is older than the second.
        assert_eq!(names(&tasks), ["2026-01-03 Second.ogg", "2026-01-02 First.opus"]);
        assert_eq!(tasks[0].urls, [url("https://a.example/feeds/media/second")]);
        assert_eq!((tasks[1].size, tasks[1].folder.as_deref()), (Some(12), Some(Path::new("Atom & Eve"))));
    }

    /// A page, another XML document or an empty feed is none to list; a feed that breaks off or
    /// is malformed is an error.
    #[test]
    fn what_is_no_feed_is_left_alone() {
        let base = "https://a.example/feed";
        assert!(parse("<!DOCTYPE html><html><body>Blog</body></html>", base).is_none());
        assert!(parse(r#"<?xml version="1.0"?><urlset><url><loc>x</loc></url></urlset>"#, base).is_none());
        assert!(parse("<rss/>", base).is_none());
        assert!(parse("PK\u{3}\u{4} binary", base).is_none());
        let blog = parse("<rss><channel><title>Blog</title><item><title>Post</title></item></channel></rss>", base).unwrap().unwrap();
        assert!(blog.episodes.is_empty());
        assert!(parse("<rss><channel><title>Cut", base).unwrap().is_err());
        assert!(parse("<rss><channel></item></channel></rss>", base).unwrap().is_err());

        assert_eq!(first_element(b"\xEF\xBB\xBF<?xml version=\"1.0\"?>\n<!-- <html> -->\n<rss version=\"2.0\">").as_deref(), Some("rss"));
        assert_eq!(first_element(b"<!doctype html><HTML lang=en>").as_deref(), Some("html"));
        assert_eq!(first_element(b"<?xml version=\"1.0\"?><fe"), None);
    }

    fn utf16(text: &str, big_endian: bool) -> Vec<u8> {
        let bom = if big_endian { [0xFE, 0xFF] } else { [0xFF, 0xFE] };
        let units = text.encode_utf16().flat_map(|u| if big_endian { u.to_be_bytes() } else { u.to_le_bytes() });
        bom.into_iter().chain(units).collect()
    }

    /// A feed saved as UTF-16 with a BOM is known by its first element, whichever byte order it
    /// has and wherever its head breaks off, and read; so is an Atom feed whose elements carry a
    /// prefix.
    #[test]
    fn utf16_and_prefixed_atom_feeds_are_read() {
        let rss = r#"<?xml version="1.0" encoding="UTF-16"?><rss version="2.0"><channel><title>Wide</title><item><title>Ünïcode</title><enclosure url="https://w.example/e.mp3" type="audio/mpeg"/></item></channel></rss>"#;
        for big_endian in [false, true] {
            let bytes = utf16(rss, big_endian);
            assert_eq!(first_element(&bytes).as_deref(), Some("rss"));
            assert_eq!(first_element(&bytes[..bytes.len() - 1]).as_deref(), Some("rss"));
            let feed = parse(&decode_text(&bytes).unwrap(), "https://w.example/feed.xml").unwrap().unwrap();
            assert_eq!((feed.title.as_deref(), feed.episodes[0].title.as_str()), (Some("Wide"), "Ünïcode"));
        }
        let html = utf16("<!DOCTYPE html><html><body>page</body></html>", false);
        assert_eq!(first_element(&html).as_deref(), Some("html"));

        let atom = r#"<?xml version="1.0"?>
<atom:feed xmlns:atom="http://www.w3.org/2005/Atom">
  <atom:title>Prefixed</atom:title>
  <atom:entry>
    <atom:title>Only</atom:title>
    <atom:id>urn:only</atom:id>
    <atom:updated>2026-02-01T00:00:00Z</atom:updated>
    <atom:link rel="enclosure" href="https://p.example/only.mp3" type="audio/mpeg"/>
  </atom:entry>
</atom:feed>"#;
        assert_eq!(first_element(atom.as_bytes()).as_deref(), Some("atom:feed"));
        assert!(is_feed_root("atom:feed") && !is_feed_root("atom:entry") && !is_feed_root("feeds"));
        let feed = parse(atom, "https://p.example/feed.atom").unwrap().unwrap();
        assert_eq!(feed.title.as_deref(), Some("Prefixed"));
        let tasks = feed_tasks(feed, None, &Done::default()).unwrap();
        assert_eq!(names(&tasks), ["2026-02-01 Only.mp3"]);
    }

    /// A feed saved as ISO-8859-1 or windows-1252 keeps its accented letters and typographic
    /// signs, which name its episodes' files.
    #[test]
    fn latin1_feeds_are_read_as_windows_1252() {
        let mut rss = br#"<?xml version="1.0" encoding="ISO-8859-1"?><rss version="2.0"><channel><title>Caf"#.to_vec();
        rss.extend_from_slice(b"\xE9</title><item><title>\x93Folge 1\x94 \x96 Gr\xFC\xDFe \x80</title>");
        rss.extend_from_slice(br#"<enclosure url="https://l.example/1.mp3" type="audio/mpeg"/></item></channel></rss>"#);
        let feed = parse(&decode_feed(&rss, None), "https://l.example/feed.xml").unwrap().unwrap();
        assert_eq!((feed.title.as_deref(), feed.episodes[0].title.as_str()), (Some("Café"), "“Folge 1” – Grüße €"));
        assert_eq!(decode_feed("Ünïcode".as_bytes(), Some("iso-8859-1")), "Ünïcode");
        // Without a declaration, the Content-Type's charset tells.
        assert_eq!(decode_feed(b"\x81\x8D\x8F\x90\x9D\xFF", Some("\"Windows-1252\"")), "\u{81}\u{8D}\u{8F}\u{90}\u{9D}ÿ");
        assert_eq!(decode_feed(b"<?xml version='1.0' encoding='us-ascii'?><rss>\xE9", None), "<?xml version='1.0' encoding='us-ascii'?><rss>é");
        // Whitespace some feeds put before the declaration does not hide it.
        let padded = [b"\r\n \t".as_slice(), &rss].concat();
        let feed = parse(&decode_feed(&padded, None), "https://l.example/feed.xml").unwrap().unwrap();
        assert_eq!(feed.title.as_deref(), Some("Café"));
    }

    /// A UTF-8 feed with a stray windows-1252 byte loses that byte alone: its other accented
    /// letters, which name its folder and files, stay, and so does a BOM's absence.
    #[test]
    fn a_stray_byte_in_a_utf8_feed_garbles_nothing_else() {
        let mut rss = "\u{FEFF}<?xml version=\"1.0\" encoding=\"UTF-8\"?><rss><channel><title>Café Grüße</title><item><title>Épisode ".as_bytes().to_vec();
        rss.extend_from_slice(b"\x92s</title><enclosure url=\"https://u.example/1.mp3\" type=\"audio/mpeg\"/></item></channel></rss>");
        for charset in [None, Some("utf-8")] {
            let text = decode_feed(&rss, charset);
            assert!(text.starts_with("<?xml"), "{text}");
            let feed = parse(&text, "https://u.example/feed.xml").unwrap().unwrap();
            assert_eq!((feed.title.as_deref(), feed.episodes[0].title.as_str()), (Some("Café Grüße"), "Épisode \u{FFFD}s"));
        }
        assert_eq!(decode_feed(b"<rss>Caf\xC3\xA9 \xFF", None), "<rss>Café \u{FFFD}");
    }

    #[test]
    fn dates_are_read_as_feeds_write_them() {
        let day = |text: &str| parse_date(text).map(|d| (d.year, d.month, d.day, d.unix));
        let noon = days_from_civil(2026, 9, 27) * 86_400 + 12 * 3600;
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(day("Sun, 27 Sep 2026 12:00:00 +0000"), Some((2026, 9, 27, noon)));
        assert_eq!(day("Sun, 27 Sep 2026 12:00:00 GMT"), Some((2026, 9, 27, noon)));
        assert_eq!(day("27 September 2026 08:00 EDT"), Some((2026, 9, 27, noon)));
        assert_eq!(day("Sun, 27 Sep 26 05:00:00 PDT"), Some((2026, 9, 27, noon)));
        assert_eq!(day("Sun, 27 Sep 2026"), Some((2026, 9, 27, noon - 12 * 3600)));
        assert_eq!(day("2026-09-27T12:00:00Z"), Some((2026, 9, 27, noon)));
        assert_eq!(day("2026-09-27T12:00:00.123+00:00"), Some((2026, 9, 27, noon)));
        assert_eq!(day("2026-09-28T01:30:00+13:30"), Some((2026, 9, 28, noon)));
        assert_eq!(day("2026-09-27 07:00:00-0500"), Some((2026, 9, 27, noon)));
        assert_eq!(day("2026-09-27"), Some((2026, 9, 27, noon - 12 * 3600)));
        for bad in ["", "yesterday", "Sun, 27 Foo 2026", "2026-13-01", "32 Sep 2026", "2026-09-27T25"] {
            assert_eq!(day(bad), None, "{bad}");
        }
        // A hostile year or time is no date, not an overflow.
        for hostile in [
            "Mon, 01 Jan 99999999999999999 00:00:00 GMT",
            "Mon, 01 Jan 10000 00:00:00 GMT",
            "Mon, 01 Jan -5 00:00:00 GMT",
            "Sun, 27 Sep 2026 9999999999999999:00:00 GMT",
            "Sun, 27 Sep 2026 12:99:00 GMT",
            "Sun, 27 Sep 2026 -1:00:00 GMT",
            "2026-09-27T9999999999999:00:00Z",
            "0000-01-01",
        ] {
            assert_eq!(day(hostile), None, "{hostile}");
        }
    }

    #[test]
    fn names_are_cleaned_shortened_and_told_apart() {
        let long = "Ω".repeat(200);
        let mut taken = HashSet::new();
        let date = parse_date("2026-09-27");
        let name = episode_name(&long, date, "mp3", &url("https://a.example/x.mp3"), &mut taken).unwrap();
        let name = name.to_string_lossy().into_owned();
        assert!(name.starts_with("2026-09-27 ΩΩ") && name.ends_with("Ω.mp3") && name.len() <= 11 + MAX_TITLE_BYTES + 4, "{name}");
        let again = episode_name(&long, date, "mp3", &url("https://a.example/y.mp3"), &mut taken).unwrap();
        assert!(again.to_string_lossy().ends_with("Ω (2).mp3"));
        // Without a title, the file's name; differing only in case, told apart for Windows.
        assert_eq!(episode_name("", None, "mp3", &url("https://a.example/ep/Show-12.MP3"), &mut taken).unwrap(), Path::new("Show-12.mp3"));
        assert_eq!(episode_name("show-12", None, "mp3", &url("https://a.example/z"), &mut taken).unwrap(), Path::new("show-12 (2).mp3"));
        assert_eq!(episode_name("CON", None, "mp3", &url("https://a.example/z"), &mut taken).unwrap(), Path::new("_CON.mp3"));
        // A title of dots alone is none; the extension always comes last, never from the title.
        assert_eq!(episode_name("...", None, "wma", &url("https://a.example/play?id=1"), &mut taken).unwrap(), Path::new("play.wma"));
        assert_eq!(episode_name(". . .", date, "mp3", &url("https://a.example/"), &mut taken).unwrap(), Path::new("2026-09-27 Episode.mp3"));
        assert_eq!(episode_name("Setup.exe", date, "mp3", &url("https://a.example/f"), &mut taken).unwrap(), Path::new("2026-09-27 Setup.exe.mp3"));
    }

    /// An enclosure is saved with the audio or video extension its link or type gives, or "mp3"
    /// or "mp4" for another audio or video type, never without one or with another kind.
    #[test]
    fn enclosures_get_a_media_extension() {
        let item = |url: &str, mime: &str| {
            let rss = format!(r#"<rss><channel><title>S</title><item><title>Interview with Dr. Smith</title><enclosure url="{url}" type="{mime}"/></item></channel></rss>"#);
            let feed = parse(&rss, "https://s.example/feed").unwrap().unwrap();
            feed.episodes.first().and_then(|e| e.enclosure.as_ref()).map(|e| e.extension)
        };
        assert_eq!(item("https://s.example/ep.wma", "audio/x-ms-wma"), Some("wma"));
        assert_eq!(item("https://s.example/ep.WMA", ""), Some("wma"));
        assert_eq!(item("https://s.example/ep.avi", "video/x-msvideo"), Some("avi"));
        assert_eq!(item("https://s.example/play?id=1", "video/mpeg"), Some("mpg"));
        assert_eq!(item("https://s.example/ep.3gp", "video/3gpp"), Some("3gp"));
        assert_eq!(item("https://s.example/play?id=1", "audio/x-foo"), Some("mp3"));
        assert_eq!(item("https://s.example/stream.php", "video/x-foo"), Some("mp4"));
        // A program's link with an audio type is still saved as audio.
        assert_eq!(item("https://s.example/Setup.exe", "audio/x-foo"), Some("mp3"));
        assert_eq!(item("https://s.example/Setup.exe", "application/octet-stream"), None);
        assert_eq!(item("https://s.example/notes.pdf", "application/pdf"), None);
        // One episode titled with dots alone does not stop the others being listed.
        let feed = parse(
            r#"<rss><channel><title>S</title>
<item><title>Interview with Dr. Smith</title><pubDate>Sun, 27 Sep 2026 10:00:00 GMT</pubDate><enclosure url="https://s.example/ep" type="audio/x-foo"/></item>
<item><title>...</title><enclosure url="https://s.example/play?id=1" type="audio/x-ms-wma"/></item>
</channel></rss>"#,
            "https://s.example/feed",
        );
        let tasks = feed_tasks(feed.unwrap().unwrap(), None, &Done::default()).unwrap();
        assert_eq!(names(&tasks), ["2026-09-27 Interview with Dr. Smith.mp3", "play.wma"]);
    }

    /// The iTunes Lookup API's answer for The Daily with `entity=podcastEpisode`, cut to the show
    /// and one episode.
    const LOOKUP: &str = r#"{
 "resultCount":2,
 "results": [
{"wrapperType":"track", "kind":"podcast", "artistId":121664449, "collectionId":1200361736, "trackId":1200361736, "artistName":"The New York Times", "collectionName":"The Daily", "trackName":"The Daily", "collectionViewUrl":"https://podcasts.apple.com/us/podcast/the-daily/id1200361736?uo=4", "feedUrl":"https://feeds.simplecast.com/Sl5CSM3S", "trackViewUrl":"https://podcasts.apple.com/us/podcast/the-daily/id1200361736?uo=4", "releaseDate":"2026-09-26T10:00:00Z", "trackCount":2731, "country":"USA", "primaryGenreName":"Daily News"},
{"previewUrl":"https://dts.podtrac.com/redirect.mp3/pdst.fm/e/pfx.vpixl.com/6qj4J/pscrb.fm/rss/p/nyt.simplecastaudio.com/03d8b493-87fc-4bd1-931f-8a8e9b945d8a/episodes/11b83b1f-3a09-4400-a504-507be826a493/audio/128/default.mp3?aid=rss_feed&awCollectionId=03d8b493-87fc-4bd1-931f-8a8e9b945d8a&awEpisodeId=11b83b1f-3a09-4400-a504-507be826a493&feed=Sl5CSM3S", "collectionViewUrl":"https://itunes.apple.com/us/podcast/the-daily/id1200361736?mt=2&uo=4", "trackTimeMillis":3437000,
"episodeUrl":"https://dts.podtrac.com/redirect.mp3/pdst.fm/e/pfx.vpixl.com/6qj4J/pscrb.fm/rss/p/nyt.simplecastaudio.com/03d8b493-87fc-4bd1-931f-8a8e9b945d8a/episodes/11b83b1f-3a09-4400-a504-507be826a493/audio/128/default.mp3?aid=rss_feed&awCollectionId=03d8b493-87fc-4bd1-931f-8a8e9b945d8a&awEpisodeId=11b83b1f-3a09-4400-a504-507be826a493&feed=Sl5CSM3S", "artistIds":[], "genres":[{"name":"Daily News", "id":"1526"}], "episodeGuid":"d9759ebd-1c66-4ffd-907d-f40e391e2a01", "releaseDate":"2026-09-27T10:00:00Z", "trackId":1000791857941, "trackName":"The Best TV Shows of the 21st Century", "feedUrl":"https://feeds.simplecast.com/Sl5CSM3S", "collectionId":1200361736, "collectionName":"The Daily", "kind":"podcast-episode", "wrapperType":"podcastEpisode"}]
}"#;

    #[test]
    fn apple_lookup_gives_the_feed_and_finds_the_episode_in_it() {
        let lookup = read_lookup(LOOKUP.as_bytes(), 1_200_361_736).unwrap();
        assert_eq!(lookup.name.as_deref(), Some("The Daily"));
        assert_eq!(lookup.feed.as_ref().map(Url::as_str), Some("https://feeds.simplecast.com/Sl5CSM3S"));
        let [episode] = &lookup.episodes[..] else { panic!("{:?}", lookup.episodes) };
        assert_eq!((episode.track, episode.guid.as_deref()), (Some(1_000_791_857_941), Some("d9759ebd-1c66-4ffd-907d-f40e391e2a01")));
        assert_eq!((episode.title.as_str(), episode.extension), ("The Best TV Shows of the 21st Century", Some("mp3")));

        let feed = parse(DAILY, "https://feeds.simplecast.com/Sl5CSM3S").unwrap().unwrap();
        let found: Vec<_> = feed.episodes.iter().filter(|e| episode.matches(e)).map(|e| e.title.as_str()).collect();
        assert_eq!(found, ["The Best TV Shows of the 21st Century"]);
        // By its file alone, whatever tracking its link carries.
        let by_file = LookupEpisode { guid: Some("other".into()), url: Some(url("https://feeds.simplecast.com/audio/bonus.m4a?src=apple")), ..LookupEpisode::default() };
        let found: Vec<_> = feed.episodes.iter().filter(|e| by_file.matches(e)).map(|e| e.title.as_str()).collect();
        assert_eq!(found, ["Bonus: Q&A / \"Live\""]);
        assert!(!feed.episodes.iter().any(|e| LookupEpisode::default().matches(e)));
    }

    /// The Lookup API's answer (`entity=podcastEpisode`) for an Apple Original, which Apple hosts
    /// and publishes no feed of: the show alone, cut to its main fields.
    const APPLE_ORIGINAL: &str = r#"{"resultCount":1,"results":[{"wrapperType":"track", "kind":"podcast", "artistId":1513466631, "collectionId":1461515071, "trackId":1461515071, "artistName":"Apple Music", "collectionName":"The Zane Lowe Interview Series", "trackName":"The Zane Lowe Interview Series", "collectionViewUrl":"https://podcasts.apple.com/us/podcast/the-zane-lowe-interview-series/id1461515071?uo=4", "collectionPrice":0.0, "releaseDate":"2026-09-17T17:00:00Z", "trackCount":334, "country":"USA", "primaryGenreName":"Music Interviews"}]}"#;

    /// The Lookup API's record of a show whose publisher hid its feed (show 1724561745, its text
    /// replaced), as `entity=podcast` answers it.
    const HIDDEN_SHOW: &str = r#"{"wrapperType":"track", "kind":"podcast", "collectionId":1724561745, "trackId":1724561745, "collectionName":"Bedtime Stories", "trackName":"Bedtime Stories", "collectionViewUrl":"https://podcasts.apple.com/us/podcast/bedtime-stories/id1724561745?uo=4", "collectionPrice":0.0, "releaseDate":"2026-09-24T23:00:00Z", "trackCount":445, "country":"USA", "primaryGenreName":"Relationships"}"#;

    /// Two of its episodes as `entity=podcastEpisode` answers them: with public files.
    const HIDDEN_EPISODES: &str = r#"{"trackViewUrl":"https://podcasts.apple.com/us/podcast/until-next-time/id1724561745?i=1000699698115&uo=4", "episodeContentType":"audio", "episodeFileExtension":"mp3", "episodeUrl":"https://c10.patreonusercontent.com/4/patreon-media/p/post/124647799/33f9aa017c444682ae9f3c506841377c/eyJhIjoxLCJwIjoxfQ%3D%3D/1.mp3?token-time=1743206400&token-hash=8GMwbjL4O0YzT7i5__fZncrPbPhU3M_eJSAtOHvpW9U%3D", "episodeGuid":"124647799", "releaseDate":"2025-03-18T17:55:23Z", "trackId":1000699698115, "trackName":"Until Next Time...", "collectionId":1724561745, "collectionName":"Bedtime Stories", "kind":"podcast-episode", "wrapperType":"podcastEpisode"},
{"trackViewUrl":"https://podcasts.apple.com/us/podcast/the-second-story/id1724561745?i=1000699698015&uo=4", "episodeContentType":"audio", "episodeFileExtension":"mp3", "episodeUrl":"https://c10.patreonusercontent.com/4/patreon-media/p/post/124506468/74ef19bb69d040efaf2307ab689d2c9c/eyJhIjoxLCJwIjoxfQ%3D%3D/1.mp3?token-time=1743206400&token-hash=cl-9oZgzO3PpGkjgAv8dSvtkEZdiP-r0mqKl9WQ49zk%3D", "episodeGuid":"124506468", "releaseDate":"2025-03-16T20:42:00Z", "trackId":1000699698015, "trackName":"The Second Story", "collectionId":1724561745, "collectionName":"Bedtime Stories", "kind":"podcast-episode", "wrapperType":"podcastEpisode"}"#;

    fn answer(records: &[&str]) -> String {
        format!(r#"{{"resultCount":{},"results":[{}]}}"#, records.len(), records.join(","))
    }

    /// A show without a public feed has none in its lookup; the episodes the API lists with a
    /// public file are episodes as a feed would list them. A show the API does not know, or an
    /// answer that is no lookup, is an error.
    #[test]
    fn apple_shows_without_a_public_feed_are_read_from_the_lookup() {
        let original = read_lookup(APPLE_ORIGINAL.as_bytes(), 1_461_515_071).unwrap();
        assert_eq!((original.name.as_deref(), &original.feed, original.episodes.len()), (Some("The Zane Lowe Interview Series"), &None, 0));

        let hidden = read_lookup(answer(&[HIDDEN_SHOW, HIDDEN_EPISODES]).as_bytes(), 1_724_561_745).unwrap();
        assert_eq!((hidden.name.as_deref(), &hidden.feed), (Some("Bedtime Stories"), &None));
        let episodes: Vec<Episode> = hidden.episodes.iter().filter_map(LookupEpisode::episode).collect();
        let tasks = feed_tasks(Feed { title: hidden.name, episodes, ..Feed::default() }, None, &Done::default()).unwrap();
        assert_eq!(names(&tasks), ["2025-03-18 Until Next Time.mp3", "2025-03-16 The Second Story.mp3"]);
        assert!(tasks[0].urls[0].as_str().starts_with("https://c10.patreonusercontent.com/4/patreon-media/p/post/124647799/"));
        assert!(tasks.iter().all(|t| t.folder.as_deref() == Some(Path::new("Bedtime Stories")) && t.from_document));

        // An episode without a public file (or one that is no audio or video) is none to list.
        let no_file = HIDDEN_EPISODES.replacen(r#""episodeUrl":"#, r#""otherUrl":"#, 1);
        let lookup = read_lookup(answer(&[HIDDEN_SHOW, &no_file]).as_bytes(), 1_724_561_745).unwrap();
        assert_eq!(lookup.episodes.iter().map(|e| e.episode().is_some()).collect::<Vec<_>>(), [false, true]);
        let text = HIDDEN_EPISODES.replacen(r#""episodeContentType":"audio", "episodeFileExtension":"mp3", "episodeUrl":"https://c10.patreonusercontent.com/4/patreon-media/p/post/124647799/33f9aa017c444682ae9f3c506841377c/eyJhIjoxLCJwIjoxfQ%3D%3D/1.mp3"#, r#""episodeContentType":"text", "episodeUrl":"https://c10.patreonusercontent.com/notes"#, 1);
        let lookup = read_lookup(answer(&[HIDDEN_SHOW, &text]).as_bytes(), 1_724_561_745).unwrap();
        assert_eq!(lookup.episodes.iter().map(|e| e.episode().is_some()).collect::<Vec<_>>(), [false, true]);

        let err = read_lookup(br#"{"resultCount":0,"results":[]}"#, 42).unwrap_err();
        assert!(err.starts_with("Apple Podcasts has no show with id 42"), "{err}");
        assert!(read_lookup(b"<html>", 42).unwrap_err().starts_with("Apple Podcasts answered the lookup of show 42"));
    }

    /// Answers requests on a local port with the body of the first route whose text the request
    /// target holds (404 when none does); `routes` is given the port's address.
    async fn serve_routes(routes: impl FnOnce(&Url) -> Vec<(String, String)>) -> Url {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = url(&format!("http://{}/", listener.local_addr().unwrap()));
        let routes = routes(&base);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut head = vec![0u8; 8192];
                let n = socket.read(&mut head).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&head[..n]).into_owned();
                let target = head.split_whitespace().nth(1).unwrap_or_default();
                let reply = match routes.iter().find(|(route, _)| target.contains(route.as_str())) {
                    Some((_, body)) => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body),
                    None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        base
    }

    /// Apple Podcasts links through a lookup API and feed served locally: a show lists its feed
    /// and an episode link that episode of it; a show whose feed Apple does not publish lists
    /// the episodes the API gives files for, and an episode link to it that one; a show with
    /// neither is not available, and an episode the API does not list is left to yt-dlp.
    #[tokio::test]
    async fn apple_links_are_listed_through_the_lookup_api() {
        let base = serve_routes(|base| {
            let daily = LOOKUP.replace(r#""feedUrl":"https://feeds.simplecast.com/Sl5CSM3S""#, &format!(r#""feedUrl":"{}daily.rss""#, base));
            vec![
                ("id=1200361736&".to_string(), daily),
                ("/daily.rss".to_string(), DAILY.to_string()),
                ("id=1724561745&entity=podcast&".to_string(), answer(&[HIDDEN_SHOW])),
                ("id=1724561745&entity=podcastEpisode&".to_string(), answer(&[HIDDEN_SHOW, HIDDEN_EPISODES])),
                ("id=1461515071&".to_string(), APPLE_ORIGINAL.to_string()),
            ]
        })
        .await;
        let api = base.join("lookup").unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        // History is not read: only-new is tested with feeds.
        let options = ListOptions { only_new: false, ..ListOptions::default() };
        let list = |link: &str| {
            let apple = AppleLink::of(&url(link)).unwrap();
            let (http, api, options) = (&http, api.as_str(), &options);
            async move { apple.list(http, api, options).await }
        };

        let show = list("https://podcasts.apple.com/us/podcast/the-daily/id1200361736").await.unwrap().unwrap();
        assert_eq!(show.len(), 5);
        assert!(show.iter().all(|t| t.folder.as_deref() == Some(Path::new("The Daily"))));
        let episode = list("https://podcasts.apple.com/us/podcast/the-daily/id1200361736?i=1000791857941").await.unwrap().unwrap();
        assert_eq!(names(&episode), ["2026-09-27 The Best TV Shows of the 21st Century.mp3"]);
        assert!(list("https://podcasts.apple.com/us/podcast/the-daily/id1200361736?i=1").await.is_none());

        let hidden = list("https://podcasts.apple.com/us/podcast/bedtime-stories/id1724561745").await.unwrap().unwrap();
        assert_eq!(names(&hidden), ["2025-03-18 Until Next Time.mp3", "2025-03-16 The Second Story.mp3"]);
        let latest = ListOptions { latest: Some(1), ..options.clone() };
        let apple = AppleLink::of(&url("https://podcasts.apple.com/us/podcast/bedtime-stories/id1724561745")).unwrap();
        assert_eq!(apple.list(&http, api.as_str(), &latest).await.unwrap().unwrap().len(), 1);
        let episode = list("https://podcasts.apple.com/us/podcast/the-second-story/id1724561745?i=1000699698015").await.unwrap().unwrap();
        assert_eq!(names(&episode), ["2025-03-16 The Second Story.mp3"]);
        assert!(list("https://podcasts.apple.com/us/podcast/older/id1724561745?i=1000600000000").await.is_none());

        let err = list("https://podcasts.apple.com/us/podcast/the-zane-lowe-interview-series/id1461515071").await.unwrap().unwrap_err();
        assert!(err.starts_with("The Zane Lowe Interview Series is not available: Apple does not publish its feed"), "{err}");
        assert!(list("https://podcasts.apple.com/us/podcast/an-interview/id1461515071?i=1000700000000").await.is_none());
        let err = list("https://podcasts.apple.com/us/podcast/gone/id42").await.unwrap().unwrap_err();
        assert!(err.starts_with("Apple Podcasts refused to look up show 42"), "{err}");
    }

    /// A feed without a declaration is read in the charset its Content-Type names.
    #[tokio::test]
    async fn the_content_type_names_a_feeds_charset() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let feed = url(&format!("http://{}/show.rss", listener.local_addr().unwrap()));
        tokio::spawn(async move {
            let rss = b"<rss><channel><title>Caf\xE9</title><item><enclosure url=\"/1.mp3\"/></item></channel></rss>";
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml; Charset=ISO-8859-1\r\nContent-Length: {}\r\n\r\n", rss.len());
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0u8; 8192]).await;
            socket.write_all(&[head.as_bytes(), rss].concat()).await.unwrap();
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(read_feed(http.get(feed.clone()), &feed, true).await.unwrap().unwrap().title.as_deref(), Some("Café"));
    }
}