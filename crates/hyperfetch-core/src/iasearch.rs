//! Internet Archive collections, searches and a user's uploads, read into the files of their items:
//! a collection's page (`archive.org/details/<id>` of an item whose mediatype is "collection"), a
//! search (`archive.org/search?query=...`, `advancedsearch.php?q=...`) or an account's page
//! (`archive.org/details/@<name>`). Each item becomes its files as its own link would (see
//! `repos::archive_item`), under a folder named after the collection, search or account.
//!
//! Requests go one a second (archive.org slows to about that after a dozen metadata requests
//! anyway) under the host's shared rest. A run lists at most [`DEFAULT_ITEMS`] items with files to
//! download (`ListOptions::max_items`); adding the link again continues: every file carries
//! download-archive lines, so a file downloaded and checked before is left out, and an item all
//! of whose files were is passed over without asking archive.org again.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::ingest::{clean_path, ListOptions, Task};
use crate::polite;
use crate::repos::ARCHIVE;

/// Least time between two requests to archive.org.
const GAP: Duration = if cfg!(test) { Duration::ZERO } else { Duration::from_secs(1) };
/// Items with files to download one run lists, unless `ListOptions::max_items` says.
pub const DEFAULT_ITEMS: usize = 100;
/// Most items `ListOptions::max_items` may ask for.
pub const MAX_ITEMS: usize = 10_000;
/// Most items one run looks at, those passed over included.
const MAX_SCANNED: usize = 200_000;
/// Items asked of the scraping API at a time (it takes 100 to 10,000).
const SCRAPE_PAGE: usize = 1000;
/// Items asked of an account's uploads at a time, and the pages its search goes to.
const UPLOADS_PAGE: usize = 100;
const UPLOADS_PAGES: usize = 100;
/// Items in a row that could not be listed before the run gives up on archive.org.
const FAILURES_IN_A_ROW: usize = 5;

/// What a link names, from its shape.
#[derive(Debug, PartialEq)]
enum Shape {
    /// archive.org/details/{id}: a collection if its metadata says so, else an item; with the query
    /// a collection's page searches it with.
    Details { id: String, query: Option<String> },
    /// archive.org/details/@{name}: the account's uploads.
    Uploads(String),
    /// A search's query, in the archive's query language.
    Search(String),
}

/// `(q) AND (and[]...) AND NOT (not[]...)`: a search page's query and the filters it adds (its
/// `and[]` and `not[]` parameters, `mediatype:"texts"` say), None when it has neither.
fn query_of(url: &Url, key: &str) -> Option<String> {
    let mut parts = Vec::new();
    for (k, v) in url.query_pairs() {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        match k.as_ref() {
            k if k == key => parts.insert(0, format!("({v})")),
            "and[]" => parts.push(format!("({v})")),
            "not[]" => parts.push(format!("NOT ({v})")),
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| parts.join(" AND "))
}

fn shape_of(url: &Url) -> Option<Shape> {
    if !matches!(url.scheme(), "http" | "https") || !matches!(url.host_str()?.trim_end_matches('.'), "archive.org" | "www.archive.org") {
        return None;
    }
    let segs: Vec<String> = url.path_segments()?.filter(|s| !s.is_empty()).map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    match segs.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["details", name, ..] if name.len() > 1 && name.starts_with('@') => Some(Shape::Uploads(name.to_string())),
        ["details", id] if !id.starts_with('@') => Some(Shape::Details { id: id.to_string(), query: query_of(url, "query") }),
        ["search" | "search.php"] => query_of(url, "query").map(Shape::Search),
        ["advancedsearch.php"] => query_of(url, "q").map(Shape::Search),
        _ => None,
    }
}

/// Whether `url` may be a collection, search or account [`list`] reads; an item's page among them,
/// which [`list`] leaves to `crate::repos`.
pub fn lists(url: &Url) -> bool {
    shape_of(url).is_some()
}

/// The files of the items the collection, search or account at `url` names, as the module says;
/// None when it names one item (`crate::repos` lists it). `Ok` is empty when every file was
/// downloaded before (`ListOptions::only_new`).
pub async fn list(http: &reqwest::Client, url: &Url, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    list_at(http, ARCHIVE, url, options, None).await
}

/// [`list`] at `base`, with the download archive's lines `archived` (read when None).
async fn list_at(http: &reqwest::Client, base: &str, url: &Url, options: &ListOptions, archived: Option<HashSet<String>>) -> Option<Result<Vec<Task>, String>> {
    let (pager, folder, what) = match shape_of(url)? {
        Shape::Details { id, query } => {
            if !is_collection(http, base, &id).await {
                return None;
            }
            let q = match query {
                Some(query) => format!("collection:\"{id}\" AND {query}"),
                None => format!("collection:\"{id}\""),
            };
            (Pager::Scrape { q, cursor: None }, id.clone(), format!("the collection {id}"))
        }
        Shape::Uploads(name) => (Pager::Uploads { name: name.clone(), page: 1 }, name.clone(), format!("the uploads of {name}")),
        Shape::Search(q) => (Pager::Scrape { q: q.clone(), cursor: None }, format!("search {q}"), format!("the search {q}")),
    };
    let folder = match clean_path([crate::ingest::truncate_chars(&folder, 80).as_str()]) {
        Ok(folder) => folder,
        Err(e) => return Some(Err(e)),
    };
    Some(items(http, base, pager, folder, &what, options, archived).await)
}

/// Whether archive.org's item `id` is a collection, by its metadata's mediatype; not when that
/// cannot be read (its listing as an item then says what fails).
async fn is_collection(http: &reqwest::Client, base: &str, id: &str) -> bool {
    #[derive(Deserialize)]
    struct Field {
        result: Option<String>,
    }
    let Ok(mut url) = Url::parse(base) else { return false };
    let Ok(mut path) = url.path_segments_mut() else { return false };
    path.pop_if_empty().extend(["metadata", id, "metadata", "mediatype"]);
    drop(path);
    let Ok(answer) = polite::get(http, &url, GAP).await else { return false };
    let Ok(body) = answer.error_for_status().map(|a| a.bytes()) else { return false };
    body.await.ok().and_then(|b| serde_json::from_slice::<Field>(&b).ok()).and_then(|f| f.result).is_some_and(|m| m == "collection")
}

/// Where the next items of a listing come from.
enum Pager {
    /// The scraping API with the query `q`, after `cursor`.
    Scrape { q: String, cursor: Option<String> },
    /// Page `page` of the account `name`'s uploads.
    Uploads { name: String, page: usize },
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Hit {
    identifier: String,
    mediatype: String,
}

impl Pager {
    /// The next items, and how many there are in all; None once there are no more.
    async fn next(&mut self, http: &reqwest::Client, base: &str) -> Result<Option<(Vec<Hit>, Option<u64>)>, String> {
        let mut url = Url::parse(base).map_err(|e| e.to_string())?;
        match self {
            Pager::Scrape { q, cursor } => {
                url.set_path("/services/search/v1/scrape");
                let count = SCRAPE_PAGE.to_string();
                url.query_pairs_mut().extend_pairs([("q", q.as_str()), ("fields", "identifier,mediatype"), ("count", count.as_str())]);
                match cursor.as_deref() {
                    Some("") => return Ok(None),
                    Some(at) => {
                        url.query_pairs_mut().append_pair("cursor", at);
                    }
                    None => {}
                }
                #[derive(Deserialize)]
                struct Page {
                    #[serde(default)]
                    items: Vec<Hit>,
                    cursor: Option<String>,
                    total: Option<u64>,
                    error: Option<String>,
                }
                let page: Page = read(http, &url).await?;
                if let Some(error) = page.error {
                    return Err(format!("archive.org's search refused the query: {error}"));
                }
                // No cursor: the last page.
                *cursor = Some(page.cursor.unwrap_or_default());
                Ok(Some((page.items, page.total)))
            }
            Pager::Uploads { name, page } => {
                if *page > UPLOADS_PAGES {
                    return Ok(None);
                }
                url.set_path("/services/search/beta/page_production/");
                let (at, size) = (page.to_string(), UPLOADS_PAGE.to_string());
                url.query_pairs_mut().extend_pairs([
                    ("page_type", "account_details"),
                    ("page_target", name.as_str()),
                    ("page_elements", "[\"uploads\"]"),
                    ("hits_per_page", size.as_str()),
                    ("page", at.as_str()),
                    ("aggregations", "false"),
                ]);
                let answer: serde_json::Value = read(http, &url).await?;
                let hits = &answer["response"]["body"]["page_elements"]["uploads"]["hits"];
                let Some(listed) = hits["hits"].as_array() else {
                    return Err(format!("archive.org has no account {name}"));
                };
                let found: Vec<Hit> = listed.iter().filter_map(|hit| serde_json::from_value(hit["fields"].clone()).ok()).collect();
                *page += 1;
                if found.is_empty() {
                    return Ok(None);
                }
                Ok(Some((found, hits["total"].as_u64())))
            }
        }
    }
}

/// The JSON archive.org answers at `url`, paced.
async fn read<T: serde::de::DeserializeOwned>(http: &reqwest::Client, url: &Url) -> Result<T, String> {
    let answer = polite::get(http, url, GAP).await?;
    if !answer.status().is_success() {
        return Err(polite::refused("archive.org", answer.status()));
    }
    let body = answer.bytes().await.map_err(|e| format!("Cannot reach archive.org: {e}"))?;
    serde_json::from_slice(&body).map_err(|_| "archive.org answered with something the app cannot read".to_string())
}

/// The download-archive line of a file of an item, its link's path and checksum: the same file,
/// changed on archive.org, is new.
fn file_line(task: &Task) -> String {
    let path = task.urls.first().map_or("", |u| u.path().trim_start_matches("/download/"));
    format!("archive.org {} {}", path, task.checksum.as_deref().unwrap_or("-"))
}

/// The download-archive lines of an item's `n` files as `formats` chose them, the `i`th's (from
/// 1): once all `n` are there, the item is passed over without asking archive.org (see
/// [`Done::item`]).
fn item_line(item: &str, formats: &str, i: usize, n: usize) -> String {
    format!("archive.org-item {item} {formats} {i}/{n}")
}

/// The formats a listing chooses files by, as one word of an item line.
fn formats_key(formats: &[String]) -> String {
    let mut key: Vec<String> = formats.iter().map(|f| f.trim().trim_start_matches('.').to_lowercase().replace(char::is_whitespace, "_")).filter(|f| !f.is_empty()).collect();
    key.sort();
    key.dedup();
    if key.is_empty() { "original".to_string() } else { key.join(",") }
}

/// What the download archive says was downloaded: its lines, and the items whose files all were.
#[derive(Default)]
struct Done {
    lines: HashSet<String>,
    /// For each item and formats key, how many files its lines say it has, and those noted.
    items: HashMap<(String, String), (usize, HashSet<usize>)>,
}

impl Done {
    fn of(lines: HashSet<String>) -> Self {
        let mut items: HashMap<(String, String), (usize, HashSet<usize>)> = HashMap::new();
        for line in &lines {
            let Some(rest) = line.strip_prefix("archive.org-item ") else { continue };
            let mut words = rest.split(' ');
            let (Some(item), Some(key), Some(count)) = (words.next(), words.next(), words.next()) else { continue };
            let Some((i, n)) = count.split_once('/').and_then(|(i, n)| Some((i.parse().ok()?, n.parse().ok()?))) else { continue };
            let noted = items.entry((item.to_string(), key.to_string())).or_insert_with(|| (n, HashSet::new()));
            if noted.0 == n {
                noted.1.insert(i);
            }
        }
        Self { lines, items }
    }

    /// Whether every file of `item` as `key` chose them was downloaded.
    fn item(&self, item: &str, key: &str) -> bool {
        self.items.get(&(item.to_string(), key.to_string())).is_some_and(|(n, noted)| (1..=*n).all(|i| noted.contains(&i)))
    }
}

/// The download archive's lines, read off the runtime's threads.
pub(crate) async fn read_archive() -> Result<HashSet<String>, String> {
    let Some(file) = crate::media::archive_file() else { return Ok(HashSet::new()) };
    tokio::task::spawn_blocking(move || crate::media::read_archive(&file)).await.map_err(|e| format!("Background task failed: {e}"))?
}

/// The files of the items `pager` gives, as the module says, under `folder`.
async fn items(http: &reqwest::Client, base: &str, mut pager: Pager, folder: PathBuf, what: &str, options: &ListOptions, archived: Option<HashSet<String>>) -> Result<Vec<Task>, String> {
    let cap = options.max_items.unwrap_or(DEFAULT_ITEMS).clamp(1, MAX_ITEMS);
    let done = match archived {
        Some(lines) => Done::of(lines),
        None if options.only_new => Done::of(read_archive().await?),
        None => Done::default(),
    };
    let key = formats_key(&options.formats);
    let (mut tasks, mut listed, mut scanned, mut before, mut collections, mut total) = (Vec::new(), 0, 0, 0, 0, None);
    let (mut failed, mut in_a_row): (Vec<String>, usize) = (Vec::new(), 0);
    let mut seen = HashSet::new();
    let mut more = false;
    'pages: while let Some((hits, all)) = pager.next(http, base).await? {
        total = total.or(all);
        for hit in hits {
            if listed == cap || scanned == MAX_SCANNED {
                more = true;
                break 'pages;
            }
            scanned += 1;
            if hit.identifier.is_empty() || !seen.insert(hit.identifier.clone()) {
                continue;
            }
            // A collection in it (or an account's own collection) holds no files of its items.
            if matches!(hit.mediatype.as_str(), "collection" | "account") {
                collections += 1;
                continue;
            }
            if done.item(&hit.identifier, &key) {
                before += 1;
                continue;
            }
            polite::pace(&Url::parse(base).map_err(|e| e.to_string())?, GAP).await;
            let found = match crate::repos::archive_item(http, base, &hit.identifier, &options.formats, options.swarm_with_http()).await {
                Ok(found) => found,
                Err(e) => {
                    in_a_row += 1;
                    failed.push(format!("{}: {e}", hit.identifier));
                    if in_a_row == FAILURES_IN_A_ROW {
                        more = true;
                        break 'pages;
                    }
                    continue;
                }
            };
            in_a_row = 0;
            let n = found.len();
            let mut new = Vec::new();
            for (i, mut task) in found.into_iter().enumerate() {
                let line = file_line(&task);
                task.archive = vec![line.clone(), item_line(&hit.identifier, &key, i + 1, n)];
                task.folder = Some(folder.join(task.folder.take().unwrap_or_default()));
                if !done.lines.contains(&line) {
                    new.push(task);
                }
            }
            if new.is_empty() {
                before += 1;
                continue;
            }
            listed += 1;
            tasks.extend(new);
        }
    }
    let total = total.map_or_else(String::new, |t| format!(" ({t} in all)"));
    if !failed.is_empty() {
        let shown = failed.iter().take(3).cloned().collect::<Vec<_>>().join("; ");
        options.fail(format!("{} item(s) of {what} could not be listed: {shown}{}", failed.len(), if failed.len() > 3 { "; ..." } else { "" }));
    }
    if collections > 0 {
        options.note(format!("{collections} collection(s) in {what} were left out: add their links to get their items"));
    }
    if before > 0 {
        options.note(format!("{before} item(s) of {what} were downloaded before and left out"));
    }
    if tasks.is_empty() {
        return match (listed, before, scanned) {
            (_, 0, 0) => Err(format!("{what} has no items")),
            (_, 0, _) if !failed.is_empty() => Err(format!("no item of {what} could be listed")),
            (_, 0, _) => Err(format!("no item of {what} has files to download")),
            _ => Ok(tasks),
        };
    }
    let bytes: u64 = tasks.iter().filter_map(|t| t.size).sum();
    let mut said = format!("{} files ({}) of {listed} item(s) of {what}{total}", tasks.len(), size_text(bytes));
    if more {
        said.push_str(&format!(
            ": the first {listed} items with files to download; add the link again for the next ones (what was downloaded is left out){}",
            if options.only_new { "" } else { ", with only new items on" }
        ));
    }
    options.note(said);
    Ok(tasks)
}

/// `bytes` as the app shows sizes: "4.20 GiB".
pub(crate) fn size_text(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.2} MiB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.2} KiB", b as f64 / (1u64 << 10) as f64),
        b => format!("{b} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{serve, Reply};

    fn shape(link: &str) -> Option<Shape> {
        shape_of(&Url::parse(link).unwrap())
    }

    #[test]
    fn collection_search_and_account_links_are_told_by_their_shape() {
        let details = |id: &str, query: Option<&str>| Some(Shape::Details { id: id.into(), query: query.map(Into::into) });
        assert_eq!(shape("https://archive.org/details/nasa"), details("nasa", None));
        assert_eq!(shape("https://archive.org/details/nasa?query=apollo&and[]=mediatype%3A%22movies%22"), details("nasa", Some("(apollo) AND (mediatype:\"movies\")")));
        assert_eq!(shape("https://archive.org/details/@jason_scott?tab=uploads"), Some(Shape::Uploads("@jason_scott".into())));
        assert_eq!(
            shape("https://archive.org/search?query=apollo+11&and%5B%5D=year%3A1969&not%5B%5D=mediatype%3Acollection"),
            Some(Shape::Search("(apollo 11) AND (year:1969) AND NOT (mediatype:collection)".into()))
        );
        assert_eq!(shape("https://archive.org/advancedsearch.php?q=subject%3Arockets&fl%5B%5D=identifier&rows=50"), Some(Shape::Search("(subject:rockets)".into())));
        for other in [
            "https://archive.org/details/nasa/page/2",
            "https://archive.org/download/nasa",
            "https://archive.org/search",
            "https://archive.org/details/@",
            "https://web.archive.org/details/nasa",
            "https://example.com/details/nasa",
        ] {
            assert_eq!(shape(other), None, "{other}");
        }
    }

    /// archive.org as it answers (checked live): an item's mediatype, the scraping API's pages
    /// (a cursor on all but the last), an account's uploads and the items' files.
    fn archive_api(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        let json = |body: &str| (200, "Content-Type: application/json\r\n".to_string(), body.as_bytes().to_vec());
        let file = |name: &str, size: u64, sha1: &str| format!(r#"{{"name":"{name}","source":"original","format":"Flac","size":"{size}","sha1":"{sha1}"}}"#);
        let sha1 = |c: char| c.to_string().repeat(40);
        match path {
            "/metadata/coll/metadata/mediatype" => json(r#"{"result":"collection"}"#),
            "/metadata/one/metadata/mediatype" => json(r#"{"result":"audio"}"#),
            p if p.starts_with("/services/search/v1/scrape?") && p.contains("cursor=next") => json(r#"{"items":[{"identifier":"c","mediatype":"audio"}],"count":1,"total":4}"#),
            p if p.starts_with("/services/search/v1/scrape?") && p.contains("q=collection%3A%22coll%22&") => json(
                r#"{"items":[{"identifier":"a","mediatype":"audio"},{"identifier":"sub","mediatype":"collection"},{"identifier":"b","mediatype":"audio"}],"count":3,"cursor":"next","total":4}"#,
            ),
            p if p.starts_with("/services/search/v1/scrape?") => json(r#"{"items":[],"count":0,"total":0}"#),
            p if p.starts_with("/services/search/beta/page_production/?") && p.contains("page=1&") => json(
                r#"{"response":{"body":{"page_elements":{"uploads":{"hits":{"total":1,"returned":1,"hits":[{"fields":{"identifier":"b","mediatype":"audio"}}]}}}}}}"#,
            ),
            p if p.starts_with("/services/search/beta/page_production/?") => json(r#"{"response":{"body":{"page_elements":{"uploads":{"hits":{"total":1,"returned":0,"hits":[]}}}}}}"#),
            "/metadata/a/files" => json(&format!(r#"{{"result":[{},{}]}}"#, file("one.flac", 10, &sha1('a')), file("two.flac", 20, &sha1('b')))),
            "/metadata/b/files" => json(&format!(r#"{{"result":[{}]}}"#, file("three.flac", 30, &sha1('c')))),
            "/metadata/c/files" => json(&format!(r#"{{"result":[{}]}}"#, file("four.flac", 40, &sha1('d')))),
            _ => (404, String::new(), b"{}".to_vec()),
        }
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn names(tasks: &[Task]) -> Vec<String> {
        tasks.iter().map(|t| t.folder.as_ref().unwrap().join(t.name.as_ref().unwrap()).to_string_lossy().replace('\\', "/")).collect()
    }

    /// A collection lists the files of its items page after page (its collections left out), in
    /// a folder named after it, each with its archive lines; a run lists at most the items asked
    /// for, and the next one goes on where it stopped: files downloaded are left out, an item all
    /// of whose files were is passed over without asking for its files. An item's page is no
    /// collection: it is left to its own listing.
    #[tokio::test]
    async fn a_collection_lists_its_items_files_and_goes_on_where_it_stopped() {
        let (addr, hits) = serve(archive_api).await;
        let base = format!("http://{addr}");
        let (notes, noted) = std::sync::mpsc::channel();
        let options = ListOptions { only_new: true, max_items: Some(2), notes: Some(notes), ..ListOptions::default() };
        let link = Url::parse("https://archive.org/details/coll").unwrap();
        let first = list_at(&http(), &base, &link, &options, Some(HashSet::new())).await.unwrap().unwrap();
        assert_eq!(names(&first), ["coll/a/one.flac", "coll/a/two.flac", "coll/b/three.flac"]);
        assert_eq!(first[0].urls[0].as_str(), format!("{base}/download/a/one.flac"));
        assert_eq!(first[0].archive, [format!("archive.org a/one.flac sha1:{}", "a".repeat(40)), "archive.org-item a original 1/2".to_string()]);
        let said: Vec<String> = noted.try_iter().map(|n| n.text).collect();
        assert!(said.iter().any(|n| n.contains("1 collection(s)")), "{said:?}");
        assert!(said.iter().any(|n| n.starts_with("3 files (60 B) of 2 item(s) of the collection coll (4 in all): the first 2")), "{said:?}");

        // a's files downloaded, and one of b's: the next run lists c, and nothing of a.
        let downloaded: HashSet<String> = first[0].archive.iter().chain(&first[1].archive).cloned().collect();
        let asked_a = hits.lock()["/metadata/a/files"];
        let next = list_at(&http(), &base, &link, &options, Some(downloaded)).await.unwrap().unwrap();
        assert_eq!(names(&next), ["coll/b/three.flac", "coll/c/four.flac"]);
        assert_eq!(hits.lock()["/metadata/a/files"], asked_a, "a was downloaded whole: not asked again");

        // An account's uploads; a search with no items; an item's page.
        let none_yet = || Some(HashSet::new());
        let uploads = list_at(&http(), &base, &Url::parse("https://archive.org/details/@someone").unwrap(), &ListOptions::default(), none_yet()).await.unwrap().unwrap();
        assert_eq!(names(&uploads), ["@someone/b/three.flac"]);
        let none = list_at(&http(), &base, &Url::parse("https://archive.org/search?query=nothing").unwrap(), &ListOptions::default(), none_yet()).await.unwrap();
        assert_eq!(none.unwrap_err(), "the search (nothing) has no items");
        assert!(list_at(&http(), &base, &Url::parse("https://archive.org/details/one").unwrap(), &ListOptions::default(), none_yet()).await.is_none());
    }

    #[test]
    fn an_item_is_done_once_all_its_files_are() {
        let lines = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        let done = Done::of(lines(&["archive.org-item a original 1/2", "archive.org-item a original 2/2", "archive.org-item b pdf 1/2", "archive.org-item c original 1/1"]));
        assert!(done.item("a", "original") && done.item("c", "original"));
        assert!(!done.item("b", "pdf") && !done.item("a", "pdf") && !done.item("d", "original"));
        assert_eq!(formats_key(&["Text PDF".into(), ".pdf".into(), " ".into()]), "pdf,text_pdf");
        assert_eq!(formats_key(&[]), "original");
    }
}
