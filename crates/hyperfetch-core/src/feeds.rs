//! Podcast and RSS/Atom feeds read into one download per episode.

use std::collections::HashSet;
use std::path::PathBuf;

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::reader::Reader;
use serde::Deserialize;
use url::Url;

use crate::history::{redact_url, DownloadHistoryManager, HistoryStatus};
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
const MEDIA_EXTENSIONS: &[&str] = &["mp3", "m4a", "m4b", "aac", "ogg", "oga", "opus", "flac", "wav", "mp4", "m4v", "mov", "webm", "mkv"];

/// Whether `url` names a feed (or a podcast show page) this module lists, from its shape alone:
/// an Apple Podcasts show or episode, a feed host (`feeds.`, `feed.`, `rss.`, as Simplecast,
/// Megaphone, Acast, Transistor, Buzzsprout, Art19, Podbean, SoundCloud and Libsyn use, or
/// `podcastfeeds.`), a path ending in `.rss`, `.xml`, `.atom` or `/podcast`, or with an `rss`,
/// `feed` or `feeds` part (Anchor's `/podcast/rss`, Spreaker's `/episodes/feed`, Patreon's
/// `/rss/...`, Supercast's `/feeds/...`), or a feed asked for in the query (Squarespace's
/// `?format=rss`, WordPress's `?feed=podcast`).
pub fn lists(url: &Url) -> bool {
    AppleLink::of(url).is_some() || looks_like_feed(url)
}

fn looks_like_feed(url: &Url) -> bool {
    let Some(host) = url.host_str() else { return false };
    let host = host.to_ascii_lowercase();
    let path = url.path().to_ascii_lowercase();
    let first_label = host.split('.').next().unwrap_or_default();
    let feed_query = |(key, value): (std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>)| {
        matches!(&*key, "format" | "feed") && ["rss", "atom", "podcast"].iter().any(|kind| value.to_ascii_lowercase().starts_with(kind))
    };
    first_label.contains("feed")
        || first_label == "rss"
        || [".rss", ".xml", ".atom"].iter().any(|ext| path.ends_with(ext))
        || path.trim_end_matches('/').ends_with("/podcast")
        || path.split('/').any(|s| matches!(s, "rss" | "feed" | "feeds"))
        || url.query_pairs().any(feed_query)
}

/// One task per episode of the feed at `url`; called only when [`lists`] takes `url`. None when
/// it is no feed after all, or a feed without audio or video (the link is then downloaded as it
/// is); `Some(Ok)` is never empty.
///
/// Episodes come newest first, each named "YYYY-MM-DD Title.ext" in a folder named after the
/// feed; `options.latest` keeps the newest N, and `options.only_new` then leaves out those whose
/// enclosure history records as downloaded. An Apple Podcasts show lists its public feed, and
/// an episode link that one episode of it.
pub async fn list(http: &reqwest::Client, url: &Url, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    if let Some(apple) = AppleLink::of(url) {
        return apple.list(http, options).await;
    }
    match read_feed(http, url).await {
        Ok(Some(feed)) if !feed.episodes.is_empty() => Some(show_tasks(feed, options).await),
        Ok(Some(_)) => {
            tracing::info!("{} is a feed without audio or video: it is downloaded as it is", redact_url(url.as_str()));
            None
        }
        Ok(None) => None,
        Err(e) => Some(Err(e)),
    }
}

/// The feed at `url`; None when it answers with a client error or is no RSS or Atom feed.
async fn read_feed(http: &reqwest::Client, url: &Url) -> Result<Option<Feed>, String> {
    let Some((bytes, base)) = fetch(http, url, true).await? else { return Ok(None) };
    let text = decode_text(&bytes).unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
    parse_feed(&text, &base).transpose()
}

/// The body at `url` and where it came from (after redirects), at most [`MAX_FEED_BYTES`]; None
/// when its host answers with a client error (a login, an expired private feed), which the
/// engine is left to report. With `feed`, also None when its first element is no `<rss>` or
/// `<feed>`, or it is too large to tell and not labelled a feed. A timeout, a rate
/// limit and a server error are errors, to retry. The link is redacted in errors: a private
/// feed's token is in it.
async fn fetch(http: &reqwest::Client, url: &Url, feed: bool) -> Result<Option<(Vec<u8>, Url)>, String> {
    use reqwest::header::{ACCEPT, CONTENT_TYPE};
    use reqwest::StatusCode;
    let fail = |e: reqwest::Error| format!("Cannot fetch {}: {}", redact_url(url.as_str()), e.without_url());
    let mut request = http.get(url.clone());
    if feed {
        request = request.header(ACCEPT, "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.8");
    }
    let resp = request.send().await.map_err(fail)?;
    let status = resp.status();
    if status.is_client_error() && !matches!(status, StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS) {
        tracing::info!("{} answered HTTP {}", redact_url(url.as_str()), status);
        return Ok(None);
    }
    let mut resp = resp.error_for_status().map_err(fail)?;
    let base = resp.url().clone();
    let labelled = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.contains("rss") || t.contains("atom"));
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
    while let Some(chunk) = resp.chunk().await.map_err(fail)? {
        body.extend_from_slice(&chunk);
        if !known_feed {
            match first_element(&body) {
                Some(name) if name == "rss" || name == "feed" => known_feed = true,
                Some(_) => return Ok(None),
                None if body.len() >= MAX_HEAD_BYTES => return Ok(None),
                None => {}
            }
        }
        if body.len() > MAX_FEED_BYTES {
            return too_large(known_feed);
        }
    }
    Ok(known_feed.then_some((body, base)))
}

/// The name of the first element in `head`, the start of an XML document, in lower case; None
/// while `head` ends before it does.
fn first_element(head: &[u8]) -> Option<String> {
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

/// A feed's title and its episodes with an audio or video enclosure, in the feed's order.
#[derive(Debug, Default)]
struct Feed {
    title: Option<String>,
    episodes: Vec<Episode>,
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
    extension: Option<&'static str>,
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
                if root != "rss" && root != "feed" {
                    return None;
                }
                path.push(root);
            }
            // An empty `<rss/>` lists nothing either.
            Event::Empty(_) if path.is_empty() => return None,
            Event::Start(e) => {
                let name = name_of(e.name());
                let parent = path.join("/");
                match (parent.as_str(), name.as_str()) {
                    ("rss/channel", "item") | ("feed", "entry") => episode = Some(Episode::default()),
                    _ => enclosure(&mut episode, &parent, &name, &e, base),
                }
                path.push(name);
                text.clear();
            }
            Event::Empty(e) => enclosure(&mut episode, &path.join("/"), &name_of(e.name()), &e, base),
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
    let from_link = link_extension(&url);
    let media = mime.as_deref().is_some_and(|m| m.starts_with("audio/") || m.starts_with("video/")) || from_link.is_some();
    if media {
        let extension = from_link.or_else(|| mime.as_deref().and_then(mime_extension));
        let length = attr("length").and_then(|l| l.parse().ok()).filter(|&l| l > 0);
        episode.enclosure = Some(Enclosure { url, extension, length });
    }
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
        "audio/webm" | "video/webm" => "webm",
        "video/mp4" => "mp4",
        "video/x-m4v" => "m4v",
        "video/quicktime" => "mov",
        "video/x-matroska" => "mkv",
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
    /// `seconds` into the day, less the time zone's `offset` (both in seconds).
    fn new(year: i64, month: u32, day: u32, seconds: i64, offset: i64) -> Option<Self> {
        ((1..=12).contains(&month) && (1..=31).contains(&day))
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

/// Seconds into the day of "hh:mm" or "hh:mm:ss" (a fraction of a second is dropped).
fn time_of_day(text: &str) -> Option<i64> {
    let mut fields = text.split(':');
    let hours: i64 = fields.next()?.parse().ok()?;
    let minutes: i64 = fields.next()?.parse().ok()?;
    let seconds: i64 = fields.next().map_or(Some(0), |s| s.split('.').next()?.parse().ok())?;
    Some(hours * 3600 + minutes * 60 + seconds)
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

/// The tasks of `feed`'s episodes as `options` say, reading the download history for
/// `only_new` off the runtime's threads.
async fn show_tasks(feed: Feed, options: &ListOptions) -> Result<Vec<Task>, String> {
    let done = if options.only_new { completed_links().await } else { HashSet::new() };
    feed_tasks(feed, options.latest, &done)
}

/// The links of the downloads history records as completed, as it saves them (redacted).
async fn completed_links() -> HashSet<String> {
    tokio::task::spawn_blocking(|| {
        let history = DownloadHistoryManager::load();
        history.entries().iter().filter(|e| e.status == HistoryStatus::Completed).flat_map(|e| e.urls.clone()).collect()
    })
    .await
    .unwrap_or_default()
}

/// One task per episode of `feed`, newest first (in the feed's order where dates are missing or
/// equal), an enclosure listed twice once: the newest `latest` of them, less those whose
/// enclosure link is in `done` (compared redacted, as history keeps it). An error when that
/// leaves none.
fn feed_tasks(feed: Feed, latest: Option<usize>, done: &HashSet<String>) -> Result<Vec<Task>, String> {
    let show = feed.title.as_deref().unwrap_or("the feed");
    let folder = feed.title.as_deref().and_then(|title| clean_path([title]).ok());
    let mut episodes = feed.episodes;
    episodes.sort_by_key(|e| std::cmp::Reverse(e.date.map(|d| d.unix)));
    let mut seen = HashSet::new();
    episodes.retain(|e| e.enclosure.as_ref().is_some_and(|enclosure| seen.insert(enclosure.url.clone())));
    let listed = episodes.len();
    if let Some(latest) = latest {
        episodes.truncate(latest);
    }
    let considered = episodes.len();
    episodes.retain(|e| e.enclosure.as_ref().is_some_and(|enclosure| !done.contains(&redact_url(enclosure.url.as_str()))));
    if episodes.is_empty() {
        let which = if considered < listed { format!("the newest {} of its {}", considered, listed) } else { format!("all {} of its", listed) };
        return Err(format!("Nothing new in {}: {} episodes were downloaded before", show, which));
    }
    let mut names = HashSet::new();
    episodes
        .into_iter()
        .filter_map(|episode| {
            let enclosure = episode.enclosure?;
            let name = episode_name(&episode.title, episode.date, enclosure.extension, &enclosure.url, &mut names);
            Some(name.map(|name| Task {
                urls: vec![enclosure.url],
                name: Some(name),
                folder: folder.clone(),
                size: enclosure.length,
                // Its host is not the feed's: the Authorization the user gave is not sent there.
                from_document: true,
                ..Task::default()
            }))
        })
        .collect()
}

/// "YYYY-MM-DD Title.ext", cleaned for every OS, with " (2)" and up added to a name `taken`
/// already has (compared ignoring case, as Windows does); an episode without a title is named
/// after its file.
fn episode_name(title: &str, date: Option<Date>, extension: Option<&str>, url: &Url, taken: &mut HashSet<String>) -> Result<PathBuf, String> {
    let file = url.path_segments().and_then(|mut s| s.next_back()).map(|last| last.rsplit_once('.').map_or(last, |(stem, _)| stem));
    let title = if title.is_empty() { file.filter(|f| !f.is_empty()).unwrap_or("Episode") } else { title };
    let cut = (0..=MAX_TITLE_BYTES.min(title.len())).rev().find(|&i| title.is_char_boundary(i)).unwrap_or(0);
    // "Coming Clean." is not saved as "Coming Clean..mp3".
    let title = title[..cut].trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    let stem = match date {
        Some(d) => format!("{:04}-{:02}-{:02} {}", d.year, d.month, d.day, title),
        None => title.to_string(),
    };
    let extension = extension.map(|e| format!(".{}", e)).unwrap_or_default();
    let mut n = 1;
    loop {
        let suffix = if n == 1 { String::new() } else { format!(" ({})", n) };
        let name = clean_path([format!("{}{}{}", stem, suffix, extension).as_str()])?;
        if taken.insert(name.to_string_lossy().to_lowercase()) {
            return Ok(name);
        }
        n += 1;
    }
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

    /// The show's feed, through the keyless iTunes Lookup API, or the episode the link opens on,
    /// found in it by what the API says of it. None for an episode the API or the feed does not
    /// know: the engine hands the link to yt-dlp.
    async fn list(&self, http: &reqwest::Client, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
        let entity = if self.episode.is_some() { "podcastEpisode" } else { "podcast" };
        let mut query = vec![("id", self.show.to_string()), ("entity", entity.to_string())];
        if self.episode.is_some() {
            query.push(("limit", "200".to_string()));
        }
        if let Some(country) = &self.country {
            query.push(("country", country.clone()));
        }
        let lookup = match Url::parse_with_params("https://itunes.apple.com/lookup", &query) {
            Ok(lookup) => lookup,
            Err(e) => return Some(Err(e.to_string())),
        };
        let answer = match fetch(http, &lookup, false).await {
            Ok(Some((answer, _))) => answer,
            Ok(None) => return Some(Err(format!("Apple Podcasts refused to look up show {}; try again later", self.show))),
            Err(e) => return Some(Err(e)),
        };
        let (feed_url, episode) = match read_lookup(&answer, self.show, self.episode) {
            Ok(found) => found,
            Err(e) => return Some(Err(e)),
        };
        let feed = match read_feed(http, &feed_url).await {
            Ok(Some(feed)) => feed,
            Ok(None) if self.episode.is_some() => return None,
            Ok(None) => return Some(Err(format!("The show's feed ({}) is not a podcast feed", redact_url(feed_url.as_str())))),
            Err(e) => return Some(Err(e)),
        };
        if self.episode.is_none() {
            if feed.episodes.is_empty() {
                return Some(Err(format!("{} lists no episodes", feed.title.as_deref().unwrap_or("The show's feed"))));
            }
            return Some(show_tasks(feed, options).await);
        }
        let wanted = episode?;
        let found = feed.episodes.into_iter().find(|e| wanted.matches(e))?;
        Some(feed_tasks(Feed { title: feed.title, episodes: vec![found] }, None, &HashSet::new()))
    }
}

/// What the iTunes Lookup API says of an episode: the guid and file the feed gives it.
#[derive(Debug, PartialEq)]
struct LookupEpisode {
    guid: Option<String>,
    url: Option<Url>,
}

impl LookupEpisode {
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
    episode_guid: Option<String>,
    episode_url: Option<String>,
}

/// The feed of `show` from an iTunes Lookup `answer`, and for an `episode` link what it says of
/// that episode (None when it is not among the recent episodes the API lists). A show without
/// a public feed, or an episode without a file, is on Apple Podcasts only (a subscription): an
/// error that says it is not available.
fn read_lookup(answer: &[u8], show: u64, episode: Option<u64>) -> Result<(Url, Option<LookupEpisode>), String> {
    let answer: LookupAnswer = serde_json::from_slice(answer).map_err(|e| format!("Apple Podcasts answered the lookup of show {} with {}", show, e))?;
    let Some(record) = answer.results.iter().find(|r| r.kind.as_deref() == Some("podcast") && r.collection_id == Some(show)) else {
        return Err(format!("Apple Podcasts has no show with id {} (it may have been removed, or be listed in another country only)", show));
    };
    let name = record.collection_name.as_deref().unwrap_or("This show");
    let feed = record
        .feed_url
        .as_deref()
        .and_then(|feed| Url::parse(feed).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .ok_or_else(|| {
            format!("{} is not available: it is on Apple Podcasts only (a subscription show) and has no public feed to download from", name)
        })?;
    let Some(episode) = episode else { return Ok((feed, None)) };
    let Some(found) = answer.results.iter().find(|r| r.wrapper_type.as_deref() == Some("podcastEpisode") && r.track_id == Some(episode)) else {
        return Ok((feed, None));
    };
    let url = found.episode_url.as_deref().and_then(|u| Url::parse(u).ok());
    if url.is_none() {
        return Err(format!(
            "This episode of {} is not available: it is on Apple Podcasts only (a subscription episode) and has no public file to download",
            name
        ));
    }
    Ok((feed, Some(LookupEpisode { guid: found.episode_guid.clone(), url })))
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
        ] {
            assert!(!lists(&url(other)), "{other}");
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

    fn parse(text: &str, base: &str) -> Option<Result<Feed, String>> {
        parse_feed(text, &url(base))
    }

    fn names(tasks: &[Task]) -> Vec<String> {
        tasks.iter().map(|t| t.name.as_ref().unwrap().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn an_rss_feed_lists_its_audio_and_video_newest_first() {
        let feed = parse(DAILY, "https://feeds.simplecast.com/Sl5CSM3S?token=s3cret").unwrap().unwrap();
        assert_eq!(feed.title.as_deref(), Some("The Daily"));
        let tasks = feed_tasks(feed, None, &HashSet::new()).unwrap();
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

    /// The newest N, less what history has downloaded, compared as history keeps links: without
    /// their secrets.
    #[test]
    fn latest_and_only_new_pick_the_episodes() {
        let feed = || parse(DAILY, "https://feeds.simplecast.com/Sl5CSM3S").unwrap().unwrap();
        assert_eq!(feed_tasks(feed(), Some(2), &HashSet::new()).unwrap().len(), 2);
        let newest = feed_tasks(feed(), Some(1), &HashSet::new()).unwrap().remove(0).urls.remove(0);
        let done = HashSet::from([redact_url(newest.as_str()), "https://cdn.example/play?id=7".to_string()]);
        let left = feed_tasks(feed(), None, &done).unwrap();
        assert_eq!(left.len(), 3);
        assert!(names(&left)[0].starts_with("2026-09-26 "));
        let err = feed_tasks(feed(), Some(1), &done).unwrap_err();
        assert_eq!(err, "Nothing new in The Daily: the newest 1 of its 5 episodes were downloaded before");

        let signed = r#"<rss><channel><title>Members</title><item><title>E</title><enclosure type="audio/mpeg" url="https://c.example/e.mp3?token=abc"/></item></channel></rss>"#;
        let done = HashSet::from([redact_url("https://c.example/e.mp3?token=xyz")]);
        let err = feed_tasks(parse(signed, "https://f.example/rss").unwrap().unwrap(), None, &done).unwrap_err();
        assert_eq!(err, "Nothing new in Members: all 1 of its episodes were downloaded before");
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
        let tasks = feed_tasks(feed, None, &HashSet::new()).unwrap();
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
    }

    #[test]
    fn names_are_cleaned_shortened_and_told_apart() {
        let long = "Ω".repeat(200);
        let mut taken = HashSet::new();
        let date = parse_date("2026-09-27");
        let name = episode_name(&long, date, Some("mp3"), &url("https://a.example/x.mp3"), &mut taken).unwrap();
        let name = name.to_string_lossy().into_owned();
        assert!(name.starts_with("2026-09-27 ΩΩ") && name.ends_with("Ω.mp3") && name.len() <= 11 + MAX_TITLE_BYTES + 4, "{name}");
        let again = episode_name(&long, date, Some("mp3"), &url("https://a.example/y.mp3"), &mut taken).unwrap();
        assert!(again.to_string_lossy().ends_with("Ω (2).mp3"));
        // Without a title, the file's name; differing only in case, told apart for Windows.
        assert_eq!(episode_name("", None, None, &url("https://a.example/ep/Show-12.MP3"), &mut taken).unwrap(), Path::new("Show-12"));
        assert_eq!(episode_name("show-12", None, None, &url("https://a.example/z"), &mut taken).unwrap(), Path::new("show-12 (2)"));
        assert_eq!(episode_name("CON", None, Some("mp3"), &url("https://a.example/z"), &mut taken).unwrap(), Path::new("_CON.mp3"));
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
        let (feed, episode) = read_lookup(LOOKUP.as_bytes(), 1_200_361_736, None).unwrap();
        assert_eq!((feed.as_str(), episode), ("https://feeds.simplecast.com/Sl5CSM3S", None));
        let (_, episode) = read_lookup(LOOKUP.as_bytes(), 1_200_361_736, Some(1_000_791_857_941)).unwrap();
        let episode = episode.unwrap();
        assert_eq!(episode.guid.as_deref(), Some("d9759ebd-1c66-4ffd-907d-f40e391e2a01"));
        // Not among the episodes the API lists: left to yt-dlp.
        assert_eq!(read_lookup(LOOKUP.as_bytes(), 1_200_361_736, Some(1)).unwrap().1, None);

        let feed = parse(DAILY, "https://feeds.simplecast.com/Sl5CSM3S").unwrap().unwrap();
        let found: Vec<_> = feed.episodes.iter().filter(|e| episode.matches(e)).map(|e| e.title.as_str()).collect();
        assert_eq!(found, ["The Best TV Shows of the 21st Century"]);
        // By its file alone, whatever tracking its link carries.
        let by_file = LookupEpisode { guid: Some("other".into()), url: Some(url("https://feeds.simplecast.com/audio/bonus.m4a?src=apple")) };
        let found: Vec<_> = feed.episodes.iter().filter(|e| by_file.matches(e)).map(|e| e.title.as_str()).collect();
        assert_eq!(found, ["Bonus: Q&A / \"Live\""]);
        let nothing = LookupEpisode { guid: None, url: None };
        assert!(!feed.episodes.iter().any(|e| nothing.matches(e)));
    }

    /// A show without a public feed, or an episode without a file, is Apple's only; a show the
    /// API does not know, or an answer that is no lookup, is an error too.
    #[test]
    fn apple_only_shows_and_episodes_are_not_available() {
        let private = LOOKUP.replace(r#""feedUrl":"https://feeds.simplecast.com/Sl5CSM3S", "trackViewUrl""#, r#""trackViewUrl""#);
        let err = read_lookup(private.as_bytes(), 1_200_361_736, None).unwrap_err();
        assert!(err.starts_with("The Daily is not available: it is on Apple Podcasts only"), "{err}");
        let err = read_lookup(private.as_bytes(), 1_200_361_736, Some(1_000_791_857_941)).unwrap_err();
        assert!(err.contains("not available"), "{err}");
        let paid = LOOKUP.replace(r#""episodeUrl":"#, r#""paidEpisodeUrl":"#);
        let err = read_lookup(paid.as_bytes(), 1_200_361_736, Some(1_000_791_857_941)).unwrap_err();
        assert!(err.starts_with("This episode of The Daily is not available"), "{err}");
        let err = read_lookup(br#"{"resultCount":0,"results":[]}"#, 42, None).unwrap_err();
        assert!(err.starts_with("Apple Podcasts has no show with id 42"), "{err}");
        assert!(read_lookup(b"<html>", 42, None).unwrap_err().starts_with("Apple Podcasts answered the lookup of show 42"));
    }
}
