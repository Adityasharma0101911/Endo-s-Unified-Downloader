//! The Wayback Machine. A capture's link (`web.archive.org/web/<timestamp>/<url>`) downloads the
//! bytes it captured (its `id_` form), not the page the Wayback Machine rewrites for browsing. A
//! listing's link (`web.archive.org/web/*/<url>`, `web/2010*/<url>*` for one year, or a query of
//! its CDX API, `web.archive.org/cdx/search/cdx?url=...`) is its captures, each of a different
//! content (collapsed by digest), into a folder tree `host/path/timestamp/name`, each checked
//! against the SHA-1 the CDX API gives (checked live: the digest is that of the bytes `id_` serves).
//!
//! The Wayback Machine limits requests hard: it is asked once every [`GAP`], the CDX API and the
//! captures' downloads alike, under the host's shared rest (a 429 rests it, downloads too, for its
//! Retry-After); one listing gives at
//! most [`DEFAULT_CAPTURES`] captures not downloaded before (`ListOptions::max_items`), each with
//! its download-archive line, so adding the link again goes on with the next ones.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use url::Url;

use crate::ingest::{clean_path, ListOptions, Task};
use crate::polite;

const WAYBACK: &str = "https://web.archive.org";
/// Least time between two requests to the Wayback Machine: the CDX API's, and the captures'
/// downloads' (see [`paced`]).
const GAP: Duration = if cfg!(test) { Duration::ZERO } else { Duration::from_secs(4) };
/// Captures one listing gives, unless `ListOptions::max_items` says.
pub const DEFAULT_CAPTURES: usize = 500;
/// Rows asked of the CDX API at a time, and the pages one listing reads.
const PAGE: usize = 1000;
const MAX_PAGES: usize = 50;

/// What a link of web.archive.org names.
#[derive(Debug, PartialEq)]
enum Link {
    /// One capture: its timestamp (or the start of one) and the URL captured.
    Capture { timestamp: String, original: String },
    /// The captures a CDX query asks for: its parameters.
    Listing(Vec<(String, String)>),
}

/// The URL captured, from what follows the timestamp in a capture's path, and the link's query:
/// with a scheme (`http://` when none is given).
fn original(rest: &str, query: Option<&str>) -> String {
    let rest = rest.trim_start_matches('/');
    let mut original = if rest.contains("://") { rest.to_string() } else { format!("http://{rest}") };
    // `https:/x` is how some clients pass a URL in a path.
    for scheme in ["http:/", "https:/"] {
        if original.starts_with(scheme) && !original.starts_with(&format!("{scheme}/")) {
            original = original.replacen(scheme, &format!("{scheme}/"), 1);
        }
    }
    match query {
        Some(q) => format!("{original}?{q}"),
        None => original,
    }
}

fn link_of(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str()?.trim_end_matches('.') != "web.archive.org" {
        return None;
    }
    if url.path() == "/cdx/search/cdx" || url.path() == "/cdx/search" {
        let params: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        return params.iter().any(|(k, v)| k == "url" && !v.is_empty()).then_some(Link::Listing(params));
    }
    let rest = url.path().strip_prefix("/web/")?;
    let (stamp, rest) = rest.split_once('/')?;
    if rest.trim_matches(['/', '*']).is_empty() {
        return None;
    }
    let digits: String = stamp.chars().take_while(char::is_ascii_digit).collect();
    let flags = &stamp[digits.len()..];
    if flags.starts_with('*') || stamp.contains('*') {
        // web/*/url, web/2010*/url, web/2010*/url*: a listing, of the year (or month, day) given.
        let mut target = original(rest, url.query());
        let prefix = target.ends_with('*');
        target = target.trim_end_matches('*').to_string();
        let mut params = vec![("url".to_string(), target)];
        if prefix {
            params.push(("matchType".into(), "prefix".into()));
        }
        if !digits.is_empty() {
            params.push(("from".into(), digits.clone()));
            params.push(("to".into(), digits));
        }
        return Some(Link::Listing(params));
    }
    // A capture: its timestamp, then a flag like `id_` or `im_`.
    let flag_ok = flags.is_empty() || (flags.ends_with('_') && flags.len() <= 3 && flags[..flags.len() - 1].bytes().all(|b| b.is_ascii_lowercase()));
    ((4..=14).contains(&digits.len()) && flag_ok).then(|| Link::Capture { timestamp: digits, original: original(rest, url.query()) })
}

/// Whether `url` is a listing's link of the Wayback Machine (a capture's goes to [`resolve`]).
pub fn lists(url: &Url) -> bool {
    matches!(link_of(url), Some(Link::Listing(_)))
}

/// Whether `url` is a capture's link, which [`resolve`] turns into its captured bytes'.
pub fn handles(url: &Url) -> bool {
    matches!(link_of(url), Some(Link::Capture { .. }))
}

/// The link of the bytes a capture's `url` captured (its `id_` form).
pub fn resolve(url: &Url) -> Option<Url> {
    match link_of(url)? {
        Link::Capture { timestamp, original } => Url::parse(&format!("{WAYBACK}/web/{timestamp}id_/{original}")).ok().inspect(paced),
        Link::Listing(_) => None,
    }
}

/// Holds every request to the Wayback Machine, a capture's download's too, to one every [`GAP`]
/// (see `crate::hosts::set_gap`): it shuts out a client that asks faster, whatever it asks.
fn paced(url: &Url) {
    crate::hosts::set_gap(url, GAP);
}

/// The CDX query link the CLI's `--wayback` asks with: the captures of `target` (everything
/// under it when it ends in `*`) from `from` to `to` (timestamps or their start: `2010`,
/// `201006`), as `filters` say (`mimetype:text/html`, `statuscode:200`, `!mimetype:image/.*`).
pub fn cdx_link(target: &str, from: Option<&str>, to: Option<&str>, filters: &[String]) -> Result<Url, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("--wayback needs a URL".to_string());
    }
    let mut url = Url::parse(WAYBACK).map_err(|e| e.to_string())?;
    url.set_path("/cdx/search/cdx");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("url", target.trim_end_matches('*'));
        if target.ends_with('*') {
            query.append_pair("matchType", "prefix");
        }
        for (key, value) in [("from", from), ("to", to)] {
            if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
                if !value.bytes().all(|b| b.is_ascii_digit()) || value.len() > 14 {
                    return Err(format!("--wayback-{key} takes a timestamp such as 2010 or 20100603: not {value}"));
                }
                query.append_pair(key, value);
            }
        }
        for filter in filters.iter().map(|f| f.trim()).filter(|f| !f.is_empty()) {
            let field = filter.trim_start_matches('!').split(':').next().unwrap_or_default();
            if !matches!(field, "mimetype" | "statuscode" | "original" | "urlkey" | "digest" | "length") || !filter.contains(':') {
                return Err(format!("--wayback-filter takes FIELD:REGEX with a field of mimetype, statuscode, original, urlkey, digest or length: not {filter}"));
            }
            query.append_pair("filter", filter);
        }
    }
    Ok(url)
}

/// The capture a CDX row names: its timestamp, the URL captured, and its digest.
struct Row {
    timestamp: String,
    original: String,
    digest: String,
}

/// A CDX digest (base32 of a SHA-1) as `sha1:<hex>`.
fn sha1_of(digest: &str) -> Option<String> {
    let mut bits = 0u64;
    let (mut count, mut bytes) = (0, Vec::new());
    for c in digest.trim().bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u64::from(value);
        count += 5;
        if count >= 8 {
            count -= 8;
            bytes.push((bits >> count) as u8);
        }
    }
    (bytes.len() == 20).then(|| format!("sha1:{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// Where a capture of `original` taken at `timestamp` is saved: `host/path.../timestamp/name`,
/// the name its URL's last part (with its query) or `index.html`.
fn place(original: &str, timestamp: &str) -> Option<(PathBuf, PathBuf)> {
    let url = Url::parse(original).ok()?;
    let mut parts: Vec<String> = url.path_segments().map_or_else(Vec::new, |s| s.map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect());
    let mut name = parts.pop().filter(|n| !n.is_empty()).unwrap_or_else(|| "index.html".to_string());
    if let Some(query) = url.query() {
        name = format!("{name}?{query}");
    }
    parts.retain(|p| !p.is_empty());
    let host = url.host_str()?.to_string();
    let folder = clean_path(std::iter::once(host.as_str()).chain(parts.iter().map(String::as_str)).chain([timestamp])).ok()?;
    let name = clean_path([crate::ingest::truncate_chars(&name, 150).as_str()]).ok()?;
    Some((folder, name))
}

/// The download of a capture: its captured bytes, in its place, checked against its digest.
fn capture_task(timestamp: &str, original: &str, digest: &str) -> Option<Task> {
    let (folder, name) = place(original, timestamp)?;
    Some(Task {
        urls: vec![Url::parse(&format!("{WAYBACK}/web/{timestamp}id_/{original}")).ok().inspect(paced)?],
        name: Some(name),
        folder: Some(folder),
        checksum: sha1_of(digest),
        archive: vec![format!("wayback {timestamp} {original}")],
        ..Task::default()
    })
}

/// The captures the listing at `url` names, as the module says; None when it is not a listing.
pub async fn list(http: &reqwest::Client, url: &Url, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    match link_of(url)? {
        Link::Capture { .. } => None,
        Link::Listing(params) => Some(captures(http, WAYBACK, params, options, None).await),
    }
}

/// The captures the CDX API at `base` lists for `params`, as the module says, with the download
/// archive's lines `archived` (read when None).
async fn captures(http: &reqwest::Client, base: &str, mut params: Vec<(String, String)>, options: &ListOptions, archived: Option<HashSet<String>>) -> Result<Vec<Task>, String> {
    let set = |params: &mut Vec<(String, String)>, key: &str, value: &str| {
        params.retain(|(k, _)| k != key);
        params.push((key.to_string(), value.to_string()));
    };
    set(&mut params, "output", "json");
    set(&mut params, "fl", "timestamp,original,statuscode,digest");
    set(&mut params, "showResumeKey", "true");
    set(&mut params, "limit", &PAGE.to_string());
    params.retain(|(k, _)| k != "resumeKey" && k != "page");
    if !params.iter().any(|(k, _)| k == "collapse") {
        params.push(("collapse".into(), "digest".into()));
    }
    // Redirects and errors are captures too: only those of a page served are asked for, unless
    // the link says which.
    if !params.iter().any(|(k, v)| k == "filter" && v.trim_start_matches('!').starts_with("statuscode")) {
        params.push(("filter".into(), "statuscode:200".into()));
    }
    let target = params.iter().find(|(k, _)| k == "url").map(|(_, v)| v.clone()).unwrap_or_default();
    let cap = options.max_items.unwrap_or(DEFAULT_CAPTURES).clamp(1, crate::iasearch::MAX_ITEMS);
    let done = match archived {
        Some(lines) => lines,
        None if options.only_new => crate::iasearch::read_archive().await?,
        None => HashSet::new(),
    };
    let (mut tasks, mut seen, mut before, mut resume, mut more) = (Vec::new(), 0, 0, None::<String>, false);
    for page in 0.. {
        if page == MAX_PAGES {
            more = true;
            break;
        }
        let mut url = Url::parse(base).map_err(|e| e.to_string())?;
        url.set_path("/cdx/search/cdx");
        url.query_pairs_mut().extend_pairs(params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if let Some(key) = &resume {
            url.query_pairs_mut().append_pair("resumeKey", key);
        }
        let answer = polite::get(http, &url, GAP).await?;
        if !answer.status().is_success() {
            return Err(polite::refused("The Wayback Machine", answer.status()));
        }
        let body = answer.bytes().await.map_err(|e| format!("Cannot reach the Wayback Machine: {e}"))?;
        let rows: Vec<Vec<String>> = if body.iter().all(u8::is_ascii_whitespace) {
            Vec::new()
        } else {
            serde_json::from_slice(&body).map_err(|_| "The Wayback Machine answered with something the app cannot read".to_string())?
        };
        // The header row, the captures, then an empty row and the resume key when there are more.
        let mut rows = rows.into_iter().skip(1);
        resume = None;
        let mut captured = Vec::new();
        for row in rows.by_ref() {
            if row.is_empty() {
                resume = rows.next().and_then(|r| r.into_iter().next());
                break;
            }
            if let [timestamp, original, _status, digest, ..] = &row[..] {
                captured.push(Row { timestamp: timestamp.clone(), original: original.clone(), digest: digest.clone() });
            }
        }
        for row in captured {
            seen += 1;
            if done.contains(&format!("wayback {} {}", row.timestamp, row.original)) {
                before += 1;
                continue;
            }
            if tasks.len() == cap {
                more = true;
                break;
            }
            tasks.extend(capture_task(&row.timestamp, &row.original, &row.digest));
        }
        if more || resume.is_none() {
            break;
        }
    }
    if seen == 0 {
        return Err(format!("the Wayback Machine has no captures of {target} that match"));
    }
    if before > 0 {
        options.note(format!("{before} capture(s) of {target} were downloaded before and left out"));
    }
    if more {
        options.note(format!(
            "The first {} captures of {target} not downloaded before; add the link again for the next ones (--max-items sets how many)",
            tasks.len()
        ));
    }
    Ok(tasks)
}

/// The manifest line of a capture's download `task` (see `capture_task`): its timestamp, the URL
/// captured, its SHA-1 and its file, tab-separated; None for any other download.
pub fn manifest_line(task: &Task) -> Option<String> {
    let line = task.archive.iter().find_map(|l| l.strip_prefix("wayback "))?;
    let (timestamp, original) = line.split_once(' ')?;
    let file = task.folder.as_ref()?.join(task.name.as_ref()?);
    Some(format!("{timestamp}\t{original}\t{}\t{}", task.checksum.as_deref().unwrap_or("-"), file.to_string_lossy().replace('\\', "/")))
}

/// The file a CLI run keeps its captures' manifest lines in, in its download folder.
pub const MANIFEST: &str = "wayback-manifest.tsv";

/// Adds `lines` (see [`manifest_line`]) to the manifest in `dir`, each once, after its header.
/// Blocking.
pub fn write_manifest(dir: &std::path::Path, lines: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    let file = dir.join(MANIFEST);
    let known = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let had: HashSet<&str> = known.lines().collect();
    let mut added = String::new();
    if known.is_empty() {
        added.push_str("timestamp\toriginal\tsha1\tfile\n");
    }
    for line in lines.iter().filter(|l| !had.contains(l.as_str())) {
        added.push_str(line);
        added.push('\n');
    }
    std::fs::OpenOptions::new().create(true).append(true).open(file)?.write_all(added.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::serve;

    fn link(text: &str) -> Option<Link> {
        link_of(&Url::parse(text).unwrap())
    }

    fn params(pairs: &[(&str, &str)]) -> Option<Link> {
        Some(Link::Listing(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()))
    }

    #[test]
    fn capture_and_listing_links_are_told_by_their_shape() {
        let capture = |timestamp: &str, original: &str| Some(Link::Capture { timestamp: timestamp.into(), original: original.into() });
        assert_eq!(link("https://web.archive.org/web/20100603215612/http://www.example.com/"), capture("20100603215612", "http://www.example.com/"));
        assert_eq!(link("https://web.archive.org/web/2010im_/https://example.com/a.png?x=1"), capture("2010", "https://example.com/a.png?x=1"));
        assert_eq!(link("https://web.archive.org/web/20100603215612/example.com/a"), capture("20100603215612", "http://example.com/a"));
        assert_eq!(
            resolve(&Url::parse("https://web.archive.org/web/20100603215612/http://www.example.com/").unwrap()).unwrap().as_str(),
            "https://web.archive.org/web/20100603215612id_/http://www.example.com/"
        );
        assert_eq!(link("https://web.archive.org/web/*/example.com/*"), params(&[("url", "http://example.com/"), ("matchType", "prefix")]));
        assert_eq!(link("https://web.archive.org/web/2010*/http://example.com/a"), params(&[("url", "http://example.com/a"), ("from", "2010"), ("to", "2010")]));
        assert_eq!(link("https://web.archive.org/cdx/search/cdx?url=example.com&from=2010"), params(&[("url", "example.com"), ("from", "2010")]));
        for other in ["https://web.archive.org/", "https://web.archive.org/web/", "https://web.archive.org/web/2010/", "https://web.archive.org/web/abc/http://x.example/", "https://archive.org/web/2010/http://x.example/", "https://web.archive.org/cdx/search/cdx?from=2010"] {
            assert_eq!(link(other), None, "{other}");
        }
        assert!(handles(&Url::parse("https://web.archive.org/web/2010/http://x.example/").unwrap()));
        assert!(!handles(&Url::parse("https://web.archive.org/web/*/x.example/*").unwrap()));
        assert!(lists(&Url::parse("https://web.archive.org/web/*/x.example/*").unwrap()) && !lists(&Url::parse("https://web.archive.org/web/2010/http://x.example/").unwrap()));
    }

    #[test]
    fn digests_places_and_query_links() {
        assert_eq!(sha1_of("COSFPXIHL6FDWTZZOQFPYN5HBTZ4Z57M").unwrap().len(), 45);
        assert_eq!(sha1_of("3I42H3S6NNFQ2MSVX7XZKYAYSCX5QBYJ").as_deref(), Some("sha1:da39a3ee5e6b4b0d3255bfef95601890afd80709"), "the SHA-1 of nothing");
        assert_eq!(sha1_of("-"), None);
        let (folder, name) = place("http://www.example.com:80/a/b/page.php?id=3", "20100603215612").unwrap();
        assert_eq!((folder, name), (PathBuf::from("www.example.com/a/b/20100603215612"), PathBuf::from("page.php_id=3")));
        assert_eq!(place("http://example.com/", "2010").unwrap(), (PathBuf::from("example.com/2010"), PathBuf::from("index.html")));
        let query = cdx_link("example.com/*", Some("2010"), Some("2011"), &["!mimetype:image/.*".into()]).unwrap();
        assert_eq!(query.as_str(), "https://web.archive.org/cdx/search/cdx?url=example.com%2F&matchType=prefix&from=2010&to=2011&filter=%21mimetype%3Aimage%2F.*");
        assert!(cdx_link("example.com", Some("June"), None, &[]).is_err());
        assert!(cdx_link("example.com", None, None, &["size:3".into()]).is_err());
    }

    /// The CDX API as it answers (checked live): a page, the empty row and the resume key; then
    /// the last page.
    fn cdx(path: &str, _: Option<crate::range::ByteRange>) -> crate::hls::tests::Reply {
        let json = |body: &str| (200, "Content-Type: application/json\r\n".to_string(), body.as_bytes().to_vec());
        if !path.starts_with("/cdx/search/cdx?") {
            return (404, String::new(), Vec::new());
        }
        if path.contains("resumeKey=next") {
            return json(r#"[["timestamp","original","statuscode","digest"],["20240101002939","https://www.example.com/b.css","200","R64ERPIFYZPFMGHNKQCEEIWNGDR3ICUQ"]]"#);
        }
        if path.contains("url=nothing") {
            return json("[]");
        }
        json(
            r#"[["timestamp","original","statuscode","digest"],
            ["20020120142510","http://example.com:80/","200","HT2DYGA5UKZCPBSFVCV3JOBXGW2G5UUA"],
            ["20100603215612","http://www.example.com/","200","COSFPXIHL6FDWTZZOQFPYN5HBTZ4Z57M"],
            [],
            ["next"]]"#,
        )
    }

    /// A listing pages through the CDX API (collapsed by digest, served pages only), each capture
    /// a download of its bytes in its place, checked by its digest; a run gives at most the
    /// captures asked for, and the next one goes on with those not downloaded.
    #[tokio::test]
    async fn a_listing_pages_through_captures_and_goes_on_where_it_stopped() {
        let (addr, hits) = serve(cdx).await;
        let base = format!("http://{addr}");
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let wanted = vec![("url".to_string(), "example.com".to_string())];
        let all = captures(&http, &base, wanted.clone(), &ListOptions { only_new: false, ..ListOptions::default() }, None).await.unwrap();
        let places: Vec<String> = all.iter().map(|t| t.folder.as_ref().unwrap().join(t.name.as_ref().unwrap()).to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(places, ["example.com/20020120142510/index.html", "www.example.com/20100603215612/index.html", "www.example.com/20240101002939/b.css"]);
        assert_eq!(all[1].urls[0].as_str(), "https://web.archive.org/web/20100603215612id_/http://www.example.com/");
        assert_eq!(all[1].archive, ["wayback 20100603215612 http://www.example.com/"]);
        assert!(all.iter().all(|t| t.checksum.as_deref().is_some_and(|c| c.starts_with("sha1:"))));
        let asked: Vec<String> = hits.lock().keys().cloned().collect();
        assert!(asked.iter().all(|p| p.contains("collapse=digest") && p.contains("filter=statuscode%3A200") && p.contains("showResumeKey=true")), "{asked:?}");
        assert_eq!(manifest_line(&all[1]).unwrap(), format!("20100603215612\thttp://www.example.com/\t{}\twww.example.com/20100603215612/index.html", all[1].checksum.as_deref().unwrap()));

        // Two at a time; once the first two were downloaded, the third.
        let (notes, noted) = std::sync::mpsc::channel();
        let two = ListOptions { max_items: Some(2), notes: Some(notes), ..ListOptions::default() };
        let first = captures(&http, &base, wanted.clone(), &two, Some(HashSet::new())).await.unwrap();
        assert_eq!(first.len(), 2);
        assert!(noted.try_iter().any(|n| n.text.starts_with("The first 2 captures")));
        let downloaded: HashSet<String> = first.iter().flat_map(|t| t.archive.clone()).collect();
        let next = captures(&http, &base, wanted, &two, Some(downloaded)).await.unwrap();
        assert_eq!(next.iter().map(|t| t.urls[0].as_str()).collect::<Vec<_>>(), ["https://web.archive.org/web/20240101002939id_/https://www.example.com/b.css"]);
        let none = captures(&http, &base, vec![("url".into(), "nothing".into())], &ListOptions::default(), Some(HashSet::new())).await.unwrap_err();
        assert_eq!(none, "the Wayback Machine has no captures of nothing that match");
    }

    #[test]
    fn the_manifest_keeps_each_line_once() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), &["a".into(), "b".into()]).unwrap();
        write_manifest(dir.path(), &["b".into(), "c".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join(MANIFEST)).unwrap(), "timestamp\toriginal\tsha1\tfile\na\nb\nc\n");
    }
}
