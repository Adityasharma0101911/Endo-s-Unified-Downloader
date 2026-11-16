//! Known archive sites, each read into its files with the mirrors and checksums the site
//! publishes, from its documented API or files (never by scraping pages where its terms ask
//! otherwise):
//!
//! - Kiwix: a ZIM file on download.kiwix.org (or a mirror's `/kiwix/zim/` tree) is its Metalink:
//!   every mirror, its hashes and piece hashes. A folder there is its ZIM files so; a book's page
//!   on library.kiwix.org the ZIM file its OPDS catalog names.
//! - Wikimedia dumps: a run (`dumps.wikimedia.org/<wiki>/<date>/`) is its files with the SHA-1 its
//!   `dumpstatus.json` publishes; a documented mirror holding the same file (by the SHA-1 its own
//!   sums file gives) is one of its mirrors.
//! - Project Gutenberg: a book (`gutenberg.org/ebooks/<id>`) is its formats, read from its RDF on a
//!   mirror, each from the HTTPS mirrors of the list Gutenberg publishes (`MIRRORS.ALL`).
//! - arXiv: a paper (`arxiv.org/abs/<id>`, `/pdf/<id>`) is its PDF and its source, from the export
//!   host arXiv asks programs to use.
//! - Zenodo, Figshare and Dataverse: a record's files with their MD5, from the site's REST API.
//! - Software Heritage: a directory or revision (`swh:1:dir:...`) is the tarball its vault cooks.
//! - Common Crawl: a `*.paths.gz` list is its files on data.commoncrawl.org, a run at a time.
//!
//! Every request goes through `crate::polite`: paced per site, and under the host's shared rest.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use crate::ingest::{clean_path, ListOptions, Task};
use crate::polite;

const KIWIX: &str = "https://download.kiwix.org";
const KIWIX_LIBRARY: &str = "https://library.kiwix.org";
const WIKIMEDIA_DUMPS: &str = "https://dumps.wikimedia.org";
/// The mirrors of the Wikimedia dumps (dumps.wikimedia.org/mirrors.html) that keep its paths and
/// serve HTTPS (checked live: the same `sha1sums.txt` at the same path).
const DUMP_MIRRORS: &[&str] = &["https://dumps.wikimedia.your.org", "https://ftp.acc.umu.se/mirror/wikimedia.org/dumps", "https://wikipedia.mirror.pdapps.org"];
const GUTENBERG: &str = "https://www.gutenberg.org";
const ARXIV_EXPORT: &str = "https://export.arxiv.org";
const ZENODO: &str = "https://zenodo.org";
const FIGSHARE_API: &str = "https://api.figshare.com/v2";
const SOFTWARE_HERITAGE: &str = "https://archive.softwareheritage.org";

/// Least time between two requests to one of these sites. Zenodo allows 30 a minute; Software
/// Heritage 60 an hour without an account, so its vault is asked about a cooking every
/// [`VAULT_POLL`]; Common Crawl asks for no more than one a second.
const GAP: Duration = if cfg!(test) { Duration::ZERO } else { Duration::from_secs(1) };
const ZENODO_GAP: Duration = if cfg!(test) { Duration::ZERO } else { Duration::from_secs(2) };
/// arXiv asks for no more than one request every 3 seconds (and one connection, see
/// `hosts::HostKey::default_connection_cap`): its files' downloads are held to that.
const ARXIV_GAP: Duration = if cfg!(test) { Duration::ZERO } else { Duration::from_secs(3) };
const VAULT_POLL: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(30) };
/// How long a listing waits for Software Heritage to cook a tarball.
const VAULT_PATIENCE: Duration = if cfg!(test) { Duration::from_secs(5) } else { Duration::from_secs(20 * 60) };
/// Most ZIM files of one Kiwix folder upgraded to their Metalink (a request each).
const MAX_KIWIX_FILES: usize = 200;
/// Files of a Common Crawl list one run lists, unless `ListOptions::max_items` says.
pub const DEFAULT_CRAWL_FILES: usize = 100;
/// Largest answer read: a Metalink, a record, a paths list (and that list unpacked).
const MAX_ANSWER: usize = 16 << 20;
const MAX_UNPACKED: u64 = 64 << 20;

/// What a link names, from its shape.
#[derive(Debug, PartialEq)]
enum Site {
    /// A ZIM file, by its path on download.kiwix.org (`/zim/...`).
    KiwixFile(String),
    /// A folder of them, so.
    KiwixFolder(String),
    /// A book of library.kiwix.org, by its name and date (`wikipedia_en_all_maxi_2024-01`).
    KiwixBook(String),
    /// A run of the Wikimedia dumps.
    Dump { wiki: String, date: String },
    /// A Project Gutenberg book.
    Gutenberg(u64),
    /// An arXiv paper, `2101.00001v2` or `hep-th/9901001`.
    Arxiv(String),
    Zenodo(u64),
    Figshare { id: u64, version: Option<u32> },
    /// A Dataverse dataset at `origin` by its persistent id, at `version` (else the latest).
    Dataverse { origin: String, pid: String, version: Option<String> },
    /// A Software Heritage object: its kind ("dir", "rev" or "cnt") and hash.
    Heritage { kind: String, hash: String },
    /// A Common Crawl list of paths.
    CommonCrawl(Url),
}

fn decoded_segments(url: &Url) -> Vec<String> {
    url.path_segments().map_or_else(Vec::new, |s| s.filter(|s| !s.is_empty()).map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect())
}

/// `swh:1:<kind>:<40 hex>` (qualifiers after `;` aside) as its kind and hash.
fn swhid(text: &str) -> Option<(String, String)> {
    let core = text.split(';').next()?;
    let rest = core.strip_prefix("swh:1:")?;
    let (kind, hash) = rest.split_once(':')?;
    let ok = matches!(kind, "dir" | "rev" | "cnt") && hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit());
    ok.then(|| (kind.to_string(), hash.to_ascii_lowercase()))
}

/// Whether `id` looks like an arXiv identifier: `2101.00001`, `2101.00001v2`, `hep-th/9901001`.
fn arxiv_id(id: &str) -> bool {
    let (archive, number) = id.split_once('/').unwrap_or(("", id));
    let number = number.split_once('v').map_or(number, |(n, v)| if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) { n } else { "" });
    let new = number.split_once('.').is_some_and(|(a, b)| a.len() == 4 && (4..=5).contains(&b.len()) && (a.to_string() + b).bytes().all(|c| c.is_ascii_digit()));
    let old = archive.len() > 1 && archive.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-' || b == b'.') && number.len() == 7 && number.bytes().all(|b| b.is_ascii_digit());
    if archive.is_empty() { new } else { old }
}

fn site_of(url: &Url) -> Option<Site> {
    if url.scheme() == "swh" {
        return swhid(url.as_str()).map(|(kind, hash)| Site::Heritage { kind, hash });
    }
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let segs = decoded_segments(url);
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let path = url.path();
    // A ZIM file or folder on Kiwix's server or a mirror's copy of its tree.
    let kiwix = match host.as_str() {
        "download.kiwix.org" | "lb.download.kiwix.org" | "mirror.download.kiwix.org" => path.starts_with("/zim/").then(|| path.to_string()),
        _ => ["/kiwix/zim/", "/download.kiwix.org/zim/"].iter().find_map(|tree| path.find(tree).map(|at| path[at + tree.len() - 5..].to_string())),
    };
    if let Some(zim) = kiwix {
        if zim.ends_with(".zim") {
            return Some(Site::KiwixFile(zim));
        }
        return zim.ends_with('/').then_some(Site::KiwixFolder(zim));
    }
    match host.as_str() {
        "library.kiwix.org" | "browse.library.kiwix.org" => {
            let book = match segs[..] {
                ["content", book, ..] => Some(book.to_string()),
                ["viewer"] => url.fragment().and_then(|f| f.split('/').next()).map(|b| percent_encoding::percent_decode_str(b).decode_utf8_lossy().into_owned()),
                _ => None,
            };
            book.filter(|b| !b.is_empty() && !b.contains(['/', '\\', '?'])).map(Site::KiwixBook)
        }
        "dumps.wikimedia.org" => match segs[..] {
            [wiki, date] if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) && wiki.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => {
                Some(Site::Dump { wiki: wiki.to_string(), date: date.to_string() })
            }
            _ => None,
        },
        "gutenberg.org" | "www.gutenberg.org" => match segs[..] {
            ["ebooks", id] => id.parse().ok().map(Site::Gutenberg),
            _ => None,
        },
        "arxiv.org" | "www.arxiv.org" | "export.arxiv.org" => match segs[..] {
            ["abs" | "pdf", ref rest @ ..] if !rest.is_empty() && rest.len() <= 2 => {
                let id = rest.join("/");
                let id = id.strip_suffix(".pdf").unwrap_or(&id).to_string();
                arxiv_id(&id).then_some(Site::Arxiv(id))
            }
            _ => None,
        },
        "zenodo.org" | "www.zenodo.org" => match segs[..] {
            ["records" | "record", id, ..] => id.parse().ok().map(Site::Zenodo),
            _ => None,
        },
        "doi.org" | "dx.doi.org" => match segs[..] {
            ["10.5281", record] => record.strip_prefix("zenodo.").and_then(|id| id.parse().ok()).map(Site::Zenodo),
            _ => None,
        },
        "archive.softwareheritage.org" => match segs[..] {
            [id] => swhid(id).map(|(kind, hash)| Site::Heritage { kind, hash }),
            ["browse", kind @ ("directory" | "revision" | "content"), hash] => {
                let hash = hash.strip_prefix("sha1_git:").unwrap_or(hash);
                let kind = match kind {
                    "directory" => "dir",
                    "revision" => "rev",
                    _ => "cnt",
                };
                swhid(&format!("swh:1:{kind}:{hash}")).map(|(kind, hash)| Site::Heritage { kind, hash })
            }
            _ => None,
        },
        "data.commoncrawl.org" => path.ends_with(".paths.gz").then(|| Site::CommonCrawl(url.clone())),
        "api.figshare.com" => match segs[..] {
            ["v2", "articles", id] => id.parse().ok().map(|id| Site::Figshare { id, version: None }),
            ["v2", "articles", id, "versions", v] => Some(Site::Figshare { id: id.parse().ok()?, version: v.parse().ok() }),
            _ => None,
        },
        h if h == "figshare.com" || h.ends_with(".figshare.com") => {
            let number = |s: &str| s.parse::<u64>().ok();
            match segs[..] {
                ["articles", _, _, id, v] if number(id).is_some() && number(v).is_some() => Some(Site::Figshare { id: number(id)?, version: v.parse().ok() }),
                ["articles", _, _, id] if number(id).is_some() => Some(Site::Figshare { id: number(id)?, version: None }),
                ["articles", _, id] if number(id).is_some() => Some(Site::Figshare { id: number(id)?, version: None }),
                _ => None,
            }
        }
        // Dataverse runs on many hosts; its dataset pages have this shape on every one.
        _ => {
            let pid = url.query_pairs().find(|(k, _)| k == "persistentId").map(|(_, v)| v.into_owned())?;
            let version = url.query_pairs().find(|(k, _)| k == "version").map(|(_, v)| v.into_owned()).filter(|v| v.bytes().all(|b| b.is_ascii_digit() || b == b'.'));
            (path == "/dataset.xhtml" && ["doi:", "hdl:", "perma:"].iter().any(|p| pid.starts_with(p)))
                .then(|| Site::Dataverse { origin: url.origin().ascii_serialization(), pid, version })
        }
    }
}

/// Whether `url` names a file, record, run or list of a site this module reads.
pub fn lists(url: &Url) -> bool {
    site_of(url).is_some()
}

/// The files at `url`, as the module says; None when it names nothing this module reads.
pub async fn list(http: &reqwest::Client, url: &Url, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    let listed = match site_of(url)? {
        Site::KiwixFile(path) => kiwix_file(http, KIWIX, &path, options).await,
        Site::KiwixFolder(path) => kiwix_folder(http, KIWIX, &path, options).await,
        Site::KiwixBook(book) => kiwix_book(http, KIWIX_LIBRARY, &book, options).await,
        Site::Dump { wiki, date } => dump(http, WIKIMEDIA_DUMPS, DUMP_MIRRORS, &wiki, &date, options).await,
        Site::Gutenberg(id) => gutenberg(http, GUTENBERG, id, &["https"], options).await,
        Site::Arxiv(id) => arxiv(ARXIV_EXPORT, &id, options),
        Site::Zenodo(id) => zenodo(http, ZENODO, id).await,
        Site::Figshare { id, version } => figshare(http, FIGSHARE_API, id, version).await,
        Site::Dataverse { origin, pid, version } => dataverse(http, &origin, &pid, version.as_deref(), options).await,
        Site::Heritage { kind, hash } => heritage(http, SOFTWARE_HERITAGE, &kind, &hash).await,
        Site::CommonCrawl(url) => common_crawl(http, &url, options, None).await,
    };
    Some(match listed {
        Ok(tasks) if tasks.is_empty() && !matches!(site_of(url), Some(Site::CommonCrawl(_))) => Err("the link holds no files the app can download".to_string()),
        listed => listed,
    })
}

/// `base` with `segments` added to its path, each percent-encoded as one segment.
fn url_at<'a>(base: &str, segments: impl IntoIterator<Item = &'a str>) -> Result<Url, String> {
    let mut url = Url::parse(base).map_err(|e| e.to_string())?;
    url.path_segments_mut().map_err(|_| format!("{} takes no path", base))?.pop_if_empty().extend(segments);
    Ok(url)
}

fn component(name: &str, fallback: &str) -> Result<PathBuf, String> {
    clean_path([crate::ingest::truncate_chars(name.trim(), 100).as_str()]).or_else(|_| clean_path([fallback]))
}

fn checksum(algo: &str, hex: &str) -> Option<String> {
    Some(format!("{}:{}", algo, hex.trim().to_ascii_lowercase())).filter(|c| crate::storage::validate_checksum(c).is_ok())
}

/// The body of `answer`, at most [`MAX_ANSWER`] bytes.
async fn body(mut answer: Response, site: &str) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = answer.chunk().await.map_err(|e| format!("Cannot reach {site}: {e}"))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_ANSWER {
            return Err(format!("{site} answered with more than the app reads"));
        }
    }
    Ok(body)
}

/// What `site` answers at `url`, paced: None for 404 and 410 (no such thing).
async fn fetch(http: &reqwest::Client, url: &Url, gap: Duration, site: &str) -> Result<Option<Vec<u8>>, String> {
    let answer = polite::get(http, url, gap).await?;
    match answer.status() {
        StatusCode::NOT_FOUND | StatusCode::GONE => Ok(None),
        status if !status.is_success() => Err(polite::refused(site, status)),
        _ => body(answer, site).await.map(Some),
    }
}

/// The JSON `site` answers at `url`, as [`fetch`] gets it; `missing` says what 404 means.
async fn json_at<T: DeserializeOwned>(http: &reqwest::Client, url: &Url, gap: Duration, site: &str, missing: impl FnOnce() -> String) -> Result<T, String> {
    let bytes = fetch(http, url, gap, site).await?.ok_or_else(missing)?;
    serde_json::from_slice(&bytes).map_err(|_| format!("{site} answered with something the app cannot read"))
}

/// The tasks of the Metalink at `meta4` (Kiwix's, made for each request by its mirror server).
async fn metalink_at(http: &reqwest::Client, meta4: &Url) -> Result<Vec<Task>, String> {
    let bytes = fetch(http, meta4, GAP, "Kiwix").await?.ok_or_else(|| "Kiwix has no such file".to_string())?;
    crate::ingest::metalink_tasks(&bytes)
}

/// The ZIM file at `path` on Kiwix's server `base`, as its Metalink lists it.
async fn kiwix_file(http: &reqwest::Client, base: &str, path: &str, options: &ListOptions) -> Result<Vec<Task>, String> {
    let file = Url::parse(&format!("{base}{path}")).map_err(|e| e.to_string())?;
    let tasks = metalink_at(http, &Url::parse(&format!("{file}.meta4")).map_err(|e| e.to_string())?).await?;
    options.note(format!("Downloading from the mirrors of its Metalink; it is on BitTorrent too: {file}.torrent"));
    Ok(tasks)
}

/// The files of the folder at `path` on Kiwix's server `base` (an open directory), each ZIM file
/// as its Metalink lists it, under its place in the folder.
async fn kiwix_folder(http: &reqwest::Client, base: &str, path: &str, options: &ListOptions) -> Result<Vec<Task>, String> {
    let folder = Url::parse(&format!("{base}{path}")).map_err(|e| e.to_string())?;
    let listed = crate::folders::list_directory(http, &folder, options).await.ok_or_else(|| "Kiwix's folder could not be read".to_string())??;
    let zim = |task: &Task| task.urls.first().is_some_and(|u| u.path().ends_with(".zim"));
    let count = listed.iter().filter(|t| zim(t)).count();
    if count > MAX_KIWIX_FILES {
        return Err(format!("the folder holds {count} ZIM files, more than the {MAX_KIWIX_FILES} the app lists at once: add a subfolder's or a file's link"));
    }
    let mut tasks = Vec::new();
    for task in listed {
        if !zim(&task) {
            tasks.push(task);
            continue;
        }
        let meta4 = Url::parse(&format!("{}.meta4", task.urls[0])).map_err(|e| e.to_string())?;
        let found = metalink_at(http, &meta4).await?.into_iter().next().ok_or_else(|| format!("{meta4} lists no file"))?;
        tasks.push(Task { name: task.name, folder: task.folder, ..found });
    }
    if count > 0 {
        options.note(format!("{count} ZIM file(s) download from the mirrors of their Metalinks; each is on BitTorrent too (its link with .torrent added)"));
    }
    Ok(tasks)
}

/// The ZIM file of library.kiwix.org's book `book` (`wikipedia_en_all_maxi_2024-01`): the one
/// its OPDS catalog at `library` gives for its name, at the date the link names.
async fn kiwix_book(http: &reqwest::Client, library: &str, book: &str, options: &ListOptions) -> Result<Vec<Task>, String> {
    // A book's name ends in its date, which the catalog does not search by.
    let name = book.rsplit_once('_').filter(|(_, date)| date.len() == 7 && date.as_bytes()[4] == b'-').map_or(book, |(name, _)| name);
    let mut url = url_at(library, ["catalog", "v2", "entries"])?;
    url.query_pairs_mut().append_pair("name", name).append_pair("count", "50");
    let bytes = fetch(http, &url, GAP, "Kiwix's library").await?.ok_or_else(|| format!("Kiwix's library has no book {book}"))?;
    let feed = String::from_utf8_lossy(&bytes);
    // Each entry's page (`.../content/<book>`) and its file's Metalink.
    let mut entries = Vec::new();
    for entry in feed.split("<entry>").skip(1) {
        let entry = entry.split("</entry>").next().unwrap_or_default();
        let lower = entry.to_ascii_lowercase();
        let (mut page, mut meta4) = (None, None);
        for tag in crate::resolver::start_tags(entry, &lower, "link") {
            let href = crate::resolver::attr_value(tag, "href").unwrap_or_default();
            if crate::resolver::attr_value(tag, "type").as_deref() == Some("text/html") {
                page = Some(href);
            } else if crate::resolver::attr_value(tag, "rel").is_some_and(|r| r.contains("acquisition")) && href.ends_with(".meta4") {
                meta4 = Some(href);
            }
        }
        if let Some(meta4) = meta4 {
            entries.push((page.unwrap_or_default(), meta4));
        }
    }
    let own = entries.iter().find(|(page, _)| page.trim_end_matches('/').ends_with(&format!("/content/{book}")));
    let meta4 = match (own, entries.first()) {
        (Some((_, meta4)), _) => meta4.clone(),
        // An older version of the book: in the same folder as the newest.
        (None, Some((_, newest))) => match newest.rsplit_once('/') {
            Some((folder, _)) => format!("{folder}/{book}.zim.meta4"),
            None => return Err(format!("Kiwix's library has no book {book}")),
        },
        (None, None) => return Err(format!("Kiwix's library has no book {book}")),
    };
    let meta4 = Url::parse(library).and_then(|l| l.join(&meta4)).map_err(|e| e.to_string())?;
    let tasks = metalink_at(http, &meta4).await?;
    options.note(format!("Downloading from the mirrors of its Metalink; it is on BitTorrent too: {}", meta4.as_str().trim_end_matches(".meta4").to_string() + ".torrent"));
    Ok(tasks)
}

#[derive(Deserialize)]
struct DumpStatus {
    #[serde(default)]
    jobs: HashMap<String, DumpJob>,
}

#[derive(Deserialize)]
struct DumpJob {
    #[serde(default)]
    status: String,
    #[serde(default)]
    files: HashMap<String, DumpFile>,
}

#[derive(Deserialize)]
struct DumpFile {
    size: Option<u64>,
    url: Option<String>,
    md5: Option<String>,
    sha1: Option<String>,
}

/// The files of the Wikimedia dump run `wiki`/`date` at `base` whose jobs are done, each with
/// its SHA-1, and as mirrors those of `mirrors` whose own `sha1sums.txt` of the run gives the
/// same SHA-1 for it.
async fn dump(http: &reqwest::Client, base: &str, mirrors: &[&str], wiki: &str, date: &str, options: &ListOptions) -> Result<Vec<Task>, String> {
    let status_url = url_at(base, [wiki, date, "dumpstatus.json"])?;
    let status: DumpStatus = json_at(http, &status_url, GAP, "Wikimedia's dumps", || format!("Wikimedia has no dump run {wiki}/{date}")).await?;
    let unfinished = status.jobs.values().filter(|job| job.status != "done" && job.status != "skipped").count();
    let mut files: Vec<(String, DumpFile)> = status.jobs.into_values().filter(|job| job.status == "done").flat_map(|job| job.files).collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let sums_name = format!("{wiki}-{date}-sha1sums.txt");
    let mut held: Vec<(Url, HashMap<String, String>)> = Vec::new();
    for mirror in mirrors {
        let Ok(run) = url_at(mirror, [wiki, date, ""]) else { continue };
        let Ok(sums) = run.join(&sums_name) else { continue };
        // A mirror that does not answer, or has not the run, is no mirror of it.
        if let Ok(Some(bytes)) = fetch(http, &sums, GAP, "a dumps mirror").await {
            let sums = String::from_utf8_lossy(&bytes).lines().filter_map(|l| l.split_once(char::is_whitespace)).map(|(sha1, name)| (name.trim().to_string(), sha1.to_ascii_lowercase())).collect();
            held.push((run, sums));
        }
    }
    let folder = component(&format!("{wiki}-{date}"), wiki)?;
    let mut tasks = Vec::new();
    for (name, file) in files {
        let Some(primary) = file.url.as_deref().and_then(|path| Url::parse(base).ok()?.join(path).ok()) else { continue };
        let sha1 = file.sha1.as_deref().map(str::to_ascii_lowercase);
        let mut urls = vec![primary];
        for (run, sums) in &held {
            if sha1.is_some() && sums.get(&name) == sha1.as_ref() {
                urls.extend(run.join(&name).ok());
            }
        }
        tasks.push(Task {
            urls,
            name: Some(component(&name, "file")?),
            folder: Some(folder.clone()),
            size: file.size,
            checksum: sha1.as_deref().and_then(|h| checksum("sha1", h)).or_else(|| file.md5.as_deref().and_then(|h| checksum("md5", h))),
            from_document: true,
            ..Task::default()
        });
    }
    if unfinished > 0 {
        options.note(format!("{unfinished} job(s) of the dump run are not finished: their files are left out"));
    }
    let mirrored = held.len();
    if mirrored > 0 {
        options.note(format!("{mirrored} mirror(s) of the Wikimedia dumps hold this run: each file also downloads from those whose SHA-1 of it matches"));
    }
    Ok(tasks)
}

/// How a mirror of Project Gutenberg lays out a book's files.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Layout {
    /// The collection by folders of the id's digits (`1/3/4/1342/1342-0.txt`), and the files made
    /// of each book under `cache/epub/<id>/` (a mirror whose note says it has them).
    Full,
    /// The collection by folders of the id's digits only.
    Dirs,
    /// Only the made files, under `<id>/`.
    Epub,
}

/// The mirrors `MIRRORS.ALL` (a table: continent | nation | location | provider | url | note)
/// lists over one of `schemes`, Gutenberg's own site aside, with how each lays out its files.
fn gutenberg_mirrors(text: &str, schemes: &[&str]) -> Vec<(Url, Layout)> {
    let mut mirrors = Vec::new();
    for line in text.lines() {
        let columns: Vec<&str> = line.split('|').map(str::trim).collect();
        let (Some(link), note) = (columns.get(4), columns.get(5).copied().unwrap_or_default()) else { continue };
        let Ok(url) = Url::parse(link) else { continue };
        if !schemes.contains(&url.scheme()) || url.host_str().is_some_and(|h| h == "gutenberg.org" || h.ends_with(".gutenberg.org")) {
            continue;
        }
        let layout = if note.contains("cache/generated") {
            Layout::Full
        } else if url.path().trim_end_matches('/').ends_with("gutenberg-epub") {
            Layout::Epub
        } else {
            Layout::Dirs
        };
        if !mirrors.iter().any(|(known, _)| *known == url) {
            mirrors.push((url, layout));
        }
    }
    mirrors
}

/// The folders of the id's digits a book is under in the collection: `1/3/4/1342`, `0/7`.
fn gutenberg_dirs(id: u64) -> String {
    let digits = id.to_string();
    if digits.len() == 1 {
        return format!("0/{digits}");
    }
    let mut path: Vec<String> = digits[..digits.len() - 1].chars().map(String::from).collect();
    path.push(digits);
    path.join("/")
}

/// What a book's RDF names a file: one of the files made of the book (`<id>.epub3.images`, by its
/// name in `cache/epub/<id>/`) or one of the collection's (`files/<id>/...`), with its kind.
#[derive(Debug, PartialEq)]
enum BookFile {
    Made { name: String, kind: String },
    Collection(String),
}

fn book_file(about: &str, id: u64) -> Option<BookFile> {
    let rest = about.split_once("://")?.1;
    let rest = rest.strip_prefix("www.").unwrap_or(rest).strip_prefix("gutenberg.org/")?;
    if let Some(kind) = rest.strip_prefix(&format!("ebooks/{id}.")) {
        let name = match kind {
            "epub3.images" => format!("pg{id}-images-3.epub"),
            "epub.images" => format!("pg{id}-images.epub"),
            "epub.noimages" => format!("pg{id}.epub"),
            "kf8.images" => format!("pg{id}-images-kf8.mobi"),
            "kindle.images" => format!("pg{id}-images.mobi"),
            "kindle.noimages" => format!("pg{id}.mobi"),
            "txt.utf-8" => format!("pg{id}.txt"),
            "html.images" => format!("pg{id}-images.html"),
            "rdf" => format!("pg{id}.rdf"),
            _ => return None,
        };
        return Some(BookFile::Made { name, kind: kind.to_string() });
    }
    if let Some(name) = rest.strip_prefix(&format!("cache/epub/{id}/")) {
        return Some(BookFile::Made { name: name.to_string(), kind: String::new() });
    }
    rest.strip_prefix(&format!("files/{id}/")).filter(|r| !r.is_empty()).map(|r| BookFile::Collection(r.to_string()))
}

/// The title of a book's RDF and its files: what each is (its `rdf:about`), its size and type.
fn parse_rdf(rdf: &str) -> (String, Vec<(String, Option<u64>, String)>) {
    let between = |s: &str, start: &str, end: &str| -> Option<String> {
        let from = s.find(start)? + start.len();
        let from = from + s[from..].find('>')? + 1;
        Some(s[from..from + s[from..].find(end)?].trim().to_string())
    };
    let title = between(rdf, "<dcterms:title", "</dcterms:title>").map(|t| crate::folders::unescape(&t)).unwrap_or_default();
    let mut files = Vec::new();
    for part in rdf.split("<pgterms:file ").skip(1) {
        let part = part.split("</pgterms:file>").next().unwrap_or_default();
        let Some(about) = crate::resolver::attr_value(&format!(" {}", part.split('>').next().unwrap_or_default()), "rdf:about") else { continue };
        let size = between(part, "<dcterms:extent", "</dcterms:extent>").and_then(|s| s.parse().ok());
        let mime = between(part, "<rdf:value", "</rdf:value>").unwrap_or_default();
        files.push((about, size, mime));
    }
    (title, files)
}

/// The formats of Project Gutenberg's book `id`: its RDF read from a mirror of those the
/// `MIRRORS.ALL` of `site` lists over `schemes`, each format from every such mirror that holds
/// it, as `options.formats` choose them (by extension, kind such as "epub3" or "kindle", or
/// type); without any, each but the RDF itself and the HTML pages (which need their images).
async fn gutenberg(http: &reqwest::Client, site: &str, id: u64, schemes: &[&str], options: &ListOptions) -> Result<Vec<Task>, String> {
    let list = url_at(site, ["MIRRORS.ALL"])?;
    let text = fetch(http, &list, GAP, "Project Gutenberg").await?.ok_or_else(|| "Project Gutenberg's mirror list is gone".to_string())?;
    let mirrors = gutenberg_mirrors(&String::from_utf8_lossy(&text), schemes);
    let made_at = |base: &Url, layout: Layout, name: &str| -> Option<Url> {
        match layout {
            Layout::Full => base.join(&format!("cache/epub/{id}/{name}")).ok(),
            Layout::Epub => base.join(&format!("{id}/{name}")).ok(),
            Layout::Dirs => None,
        }
    };
    let mut rdf = None;
    for (base, layout) in &mirrors {
        let Some(at) = made_at(base, *layout, &format!("pg{id}.rdf")) else { continue };
        if let Ok(Some(bytes)) = fetch(http, &at, GAP, "a Project Gutenberg mirror").await {
            rdf = Some(String::from_utf8_lossy(&bytes).into_owned());
            break;
        }
    }
    let rdf = rdf.ok_or_else(|| format!("no mirror of Project Gutenberg has book {id} yet (mirrors take a day or so to have a new one)"))?;
    let (title, files) = parse_rdf(&rdf);
    let folder = component(&format!("{} ({id})", if title.is_empty() { "Project Gutenberg" } else { title.as_str() }), &id.to_string())?;
    let wanted: Vec<String> = options.formats.iter().map(|f| f.trim().trim_start_matches('.').to_ascii_lowercase()).filter(|f| !f.is_empty()).collect();
    let mut tasks = Vec::new();
    for (about, size, mime) in files {
        let Some(file) = book_file(&about, id) else { continue };
        let (name, kind) = match &file {
            BookFile::Made { name, kind } => (name.clone(), kind.clone()),
            BookFile::Collection(path) => (path.clone(), String::new()),
        };
        let extension = name.rsplit_once('.').map_or("", |(_, e)| e).to_ascii_lowercase();
        let chosen = if wanted.is_empty() {
            !matches!(extension.as_str(), "rdf" | "html" | "htm")
        } else {
            wanted.iter().any(|f| *f == extension || (!kind.is_empty() && kind.starts_with(f.as_str())) || mime.to_ascii_lowercase().contains(f.as_str()))
        };
        if !chosen {
            continue;
        }
        let urls: Vec<Url> = mirrors
            .iter()
            .filter_map(|(base, layout)| match (&file, layout) {
                (BookFile::Made { .. }, _) => made_at(base, *layout, &name),
                (BookFile::Collection(path), Layout::Full | Layout::Dirs) => base.join(&format!("{}/{path}", gutenberg_dirs(id))).ok(),
                (BookFile::Collection(_), Layout::Epub) => None,
            })
            .collect();
        let Ok(path) = clean_path(name.split('/')) else { continue };
        if urls.is_empty() {
            continue;
        }
        tasks.push(Task { urls, name: Some(path), folder: Some(folder.clone()), size, from_document: true, ..Task::default() });
    }
    if tasks.is_empty() {
        return Err(match wanted.is_empty() {
            true => format!("no mirror of Project Gutenberg holds a format of book {id}"),
            false => format!("book {id} has no format {} on Project Gutenberg's mirrors", wanted.join(", ")),
        });
    }
    Ok(tasks)
}

/// arXiv's paper `id`: its PDF and its source (a tarball, or the one file it is), from `export`.
fn arxiv(export: &str, id: &str, options: &ListOptions) -> Result<Vec<Task>, String> {
    let folder = component(&format!("arXiv {}", id.replace('/', "_")), "arXiv")?;
    let pdf = url_at(export, std::iter::once("pdf").chain(id.split('/')))?;
    let source = url_at(export, std::iter::once("src").chain(id.split('/')))?;
    crate::hosts::set_gap(&pdf, ARXIV_GAP);
    options.note("arXiv asks for one request every 3 seconds over one connection: its files download one at a time".to_string());
    Ok(vec![
        Task { urls: vec![pdf], name: Some(component(&format!("{}.pdf", id.replace('/', "_")), "paper.pdf")?), folder: Some(folder.clone()), ..Task::default() },
        // Named as arXiv names it: a tarball, or the one gzipped file a paper's source may be.
        Task { urls: vec![source], folder: Some(folder), ..Task::default() },
    ])
}

/// The files of Zenodo's record `id` at `base`, with their MD5.
async fn zenodo(http: &reqwest::Client, base: &str, id: u64) -> Result<Vec<Task>, String> {
    #[derive(Deserialize)]
    struct Record {
        #[serde(default)]
        files: Vec<File>,
    }
    #[derive(Deserialize)]
    struct File {
        key: String,
        size: Option<u64>,
        checksum: Option<String>,
        links: Links,
    }
    #[derive(Deserialize)]
    struct Links {
        #[serde(rename = "self")]
        content: Option<String>,
    }
    let url = url_at(base, ["api", "records", &id.to_string()])?;
    let record: Record = json_at(http, &url, ZENODO_GAP, "Zenodo", || format!("Zenodo has no record {id}")).await?;
    let folder = component(&format!("zenodo {id}"), "zenodo")?;
    let own = Url::parse(base).map_err(|e| e.to_string())?;
    let tasks: Vec<Task> = record
        .files
        .into_iter()
        .filter_map(|file| {
            let url = Url::parse(file.links.content.as_deref()?).ok().filter(|u| u.origin() == own.origin())?;
            let md5 = file.checksum.as_deref().and_then(|c| c.split_once(':')).and_then(|(algo, hex)| checksum(algo, hex));
            Some(Task { urls: vec![url], name: clean_path(file.key.split('/')).ok(), folder: Some(folder.clone()), size: file.size, checksum: md5, ..Task::default() })
        })
        .collect();
    if tasks.is_empty() {
        return Err(format!("Zenodo's record {id} has no files open to everyone"));
    }
    Ok(tasks)
}

/// The files of Figshare's article `id` (at `version`, else its latest) through the API at `api`.
async fn figshare(http: &reqwest::Client, api: &str, id: u64, version: Option<u32>) -> Result<Vec<Task>, String> {
    #[derive(Deserialize)]
    struct Article {
        #[serde(default)]
        files: Vec<File>,
    }
    #[derive(Deserialize)]
    struct File {
        name: String,
        size: Option<u64>,
        computed_md5: Option<String>,
        download_url: String,
        #[serde(default)]
        is_link_only: bool,
    }
    let id_text = id.to_string();
    let url = match version {
        Some(v) => url_at(api, ["articles", id_text.as_str(), "versions", &v.to_string()])?,
        None => url_at(api, ["articles", id_text.as_str()])?,
    };
    let article: Article = json_at(http, &url, GAP, "Figshare", || format!("Figshare has no public item {id}")).await?;
    let folder = component(&format!("figshare {id}"), "figshare")?;
    Ok(article
        .files
        .into_iter()
        .filter(|f| !f.is_link_only)
        .filter_map(|file| {
            Some(Task {
                urls: vec![Url::parse(&file.download_url).ok()?],
                name: clean_path([file.name.as_str()]).ok(),
                folder: Some(folder.clone()),
                size: file.size,
                checksum: file.computed_md5.as_deref().and_then(|h| checksum("md5", h)),
                ..Task::default()
            })
        })
        .collect())
}

/// The files of the Dataverse dataset `pid` at `origin` (at `version`, else its latest) open to
/// everyone, in their folders, each with its checksum; a tabular file the site converted is its
/// original upload.
async fn dataverse(http: &reqwest::Client, origin: &str, pid: &str, version: Option<&str>, options: &ListOptions) -> Result<Vec<Task>, String> {
    #[derive(Deserialize)]
    struct Answer {
        data: serde_json::Value,
    }
    #[derive(Deserialize)]
    struct Entry {
        #[serde(default)]
        restricted: bool,
        #[serde(rename = "directoryLabel")]
        directory: Option<String>,
        #[serde(rename = "dataFile")]
        file: DataFile,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct DataFile {
        id: u64,
        filename: String,
        filesize: Option<u64>,
        checksum: Option<Sum>,
        original_file_name: Option<String>,
        original_file_size: Option<u64>,
    }
    #[derive(Deserialize)]
    struct Sum {
        #[serde(rename = "type")]
        kind: String,
        value: String,
    }
    let mut url = match version {
        Some(v) => url_at(origin, ["api", "datasets", ":persistentId", "versions", v])?,
        None => url_at(origin, ["api", "datasets", ":persistentId", ""])?,
    };
    url.query_pairs_mut().append_pair("persistentId", pid);
    let answer: Answer = json_at(http, &url, GAP, "Dataverse", || format!("Dataverse has no public dataset {pid}")).await?;
    let files = if version.is_some() { &answer.data["files"] } else { &answer.data["latestVersion"]["files"] };
    let entries: Vec<Entry> = serde_json::from_value(files.clone()).map_err(|_| "Dataverse answered with something the app cannot read".to_string())?;
    let folder = component(&pid.replace([':', '/'], "_"), "dataverse")?;
    let (mut tasks, mut restricted) = (Vec::new(), 0);
    for entry in entries {
        if entry.restricted {
            restricted += 1;
            continue;
        }
        let file = entry.file;
        let mut link = url_at(origin, ["api", "access", "datafile", &file.id.to_string()])?;
        let (name, size) = match file.original_file_name {
            Some(original) => {
                link.set_query(Some("format=original"));
                (original, file.original_file_size)
            }
            None => (file.filename, file.filesize),
        };
        let parts: Vec<&str> = entry.directory.as_deref().unwrap_or_default().split('/').filter(|p| !p.is_empty()).chain([name.as_str()]).collect();
        let Ok(name) = clean_path(parts) else { continue };
        let sum = file.checksum.and_then(|s| {
            let algo = match s.kind.to_ascii_uppercase().as_str() {
                "MD5" => "md5",
                "SHA-1" => "sha1",
                "SHA-256" => "sha256",
                "SHA-512" => "sha512",
                _ => return None,
            };
            checksum(algo, &s.value)
        });
        tasks.push(Task { urls: vec![link], name: Some(name), folder: Some(folder.clone()), size, checksum: sum, ..Task::default() });
    }
    if restricted > 0 {
        options.note(format!("{restricted} file(s) of the dataset are restricted and were left out"));
    }
    Ok(tasks)
}

/// The tarball of Software Heritage's directory or revision `hash`, cooked by its vault at
/// `base` (asked to, if it has not), waited for politely; a content is its raw bytes.
async fn heritage(http: &reqwest::Client, base: &str, kind: &str, hash: &str) -> Result<Vec<Task>, String> {
    let swhid = format!("swh:1:{kind}:{hash}");
    if kind == "cnt" {
        let raw = url_at(base, ["api", "1", "content", &format!("sha1_git:{hash}"), "raw", ""])?;
        return Ok(vec![Task { urls: vec![raw], name: Some(component(hash, "content")?), ..Task::default() }]);
    }
    #[derive(Deserialize)]
    struct Cooking {
        #[serde(default)]
        status: String,
        fetch_url: Option<String>,
        progress_message: Option<String>,
    }
    // A directory as a tarball of its files, a revision as a bare git repository.
    let cooker = if kind == "dir" { "flat" } else { "git-bare" };
    let url = url_at(base, ["api", "1", "vault", cooker, &swhid, ""])?;
    let deadline = tokio::time::Instant::now() + VAULT_PATIENCE;
    let mut asked = false;
    loop {
        let answer = polite::get(http, &url, GAP).await?;
        let answer = if answer.status() == StatusCode::NOT_FOUND && !asked {
            asked = true;
            polite::send(&url, GAP, || http.post(url.clone())).await?
        } else {
            answer
        };
        if answer.status() == StatusCode::NOT_FOUND {
            return Err(format!("Software Heritage has no {swhid}"));
        }
        if !answer.status().is_success() {
            return Err(polite::refused("Software Heritage", answer.status()));
        }
        let cooking: Cooking = serde_json::from_slice(&body(answer, "Software Heritage").await?).map_err(|_| "Software Heritage answered with something the app cannot read".to_string())?;
        match cooking.status.as_str() {
            "done" => {
                let fetch = cooking.fetch_url.as_deref().and_then(|f| Url::parse(base).ok()?.join(f).ok()).ok_or("Software Heritage gave no link to the tarball")?;
                return Ok(vec![Task { urls: vec![fetch], ..Task::default() }]);
            }
            "failed" => return Err(format!("Software Heritage could not prepare {swhid}: {}", cooking.progress_message.unwrap_or_default())),
            _ if tokio::time::Instant::now() + VAULT_POLL > deadline => {
                return Err(format!("Software Heritage is still preparing {swhid}: add the link again later"));
            }
            _ => tokio::time::sleep(VAULT_POLL).await,
        }
    }
}

/// The files the Common Crawl list at `url` (a gzipped text of paths) names, on its host: at most
/// `options.max_items` (else [`DEFAULT_CRAWL_FILES`]) not downloaded before, each with its
/// download-archive line, so adding the list again goes on with the next ones. `archived`: the
/// download archive's lines (read when None).
async fn common_crawl(http: &reqwest::Client, url: &Url, options: &ListOptions, archived: Option<HashSet<String>>) -> Result<Vec<Task>, String> {
    let packed = fetch(http, url, GAP, "Common Crawl").await?.ok_or_else(|| "Common Crawl has no such list".to_string())?;
    let mut text = String::new();
    flate2::read::GzDecoder::new(&packed[..]).take(MAX_UNPACKED).read_to_string(&mut text).map_err(|e| format!("the list cannot be unpacked: {e}"))?;
    let done = match archived {
        Some(lines) => lines,
        None if options.only_new => crate::iasearch::read_archive().await?,
        None => HashSet::new(),
    };
    let cap = options.max_items.unwrap_or(DEFAULT_CRAWL_FILES).max(1);
    let origin = url.origin().ascii_serialization();
    let (mut tasks, mut listed, mut before) = (Vec::new(), 0, 0);
    for path in text.lines().map(str::trim).filter(|p| !p.is_empty() && !p.starts_with('#')) {
        listed += 1;
        let line = format!("commoncrawl {path}");
        if done.contains(&line) {
            before += 1;
            continue;
        }
        if tasks.len() == cap {
            continue;
        }
        let Ok(name) = clean_path(path.split('/')) else { continue };
        let Ok(file) = url_at(&origin, path.split('/')) else { continue };
        tasks.push(Task { urls: vec![file], name: Some(name), archive: vec![line], ..Task::default() });
    }
    if listed == 0 {
        return Err("the list names no files".to_string());
    }
    let left = listed - before - tasks.len();
    if left > 0 {
        options.note(format!(
            "The list names {listed} files ({before} downloaded before): these are the next {}; add it again for the {left} after (--max-items sets how many)",
            tasks.len()
        ));
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{serve, Reply};

    fn site(link: &str) -> Option<Site> {
        site_of(&Url::parse(link).unwrap())
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn json(body: &str) -> Reply {
        (200, "Content-Type: application/json\r\n".to_string(), body.as_bytes().to_vec())
    }

    fn text(body: &str) -> Reply {
        (200, String::new(), body.as_bytes().to_vec())
    }

    fn missing() -> Reply {
        (404, String::new(), Vec::new())
    }

    /// A test server whose `handler` is told the server's own base URL, and that URL.
    async fn serve_knowing(handler: fn(&str, &str) -> Reply) -> String {
        let base = std::sync::Arc::new(std::sync::OnceLock::<String>::new());
        let known = std::sync::Arc::clone(&base);
        let (addr, _) = serve(move |path, _| handler(known.get().map_or("", String::as_str), path)).await;
        base.set(format!("http://{addr}")).unwrap();
        format!("http://{addr}")
    }

    fn named(tasks: &[Task]) -> Vec<String> {
        tasks.iter().map(|t| t.folder.clone().unwrap_or_default().join(t.name.clone().unwrap_or_default()).to_string_lossy().replace('\\', "/")).collect()
    }

    #[test]
    fn links_are_told_by_their_shape() {
        assert_eq!(site("https://download.kiwix.org/zim/gutenberg/gutenberg_ale_all_2025-09.zim"), Some(Site::KiwixFile("/zim/gutenberg/gutenberg_ale_all_2025-09.zim".into())));
        assert_eq!(site("https://ftp.nluug.nl/pub/kiwix/zim/gutenberg/x.zim"), Some(Site::KiwixFile("/zim/gutenberg/x.zim".into())));
        assert_eq!(site("https://mirror-sites-in.mblibrary.info/mirror-sites/download.kiwix.org/zim/other/"), Some(Site::KiwixFolder("/zim/other/".into())));
        assert_eq!(site("https://download.kiwix.org/zim/gutenberg/"), Some(Site::KiwixFolder("/zim/gutenberg/".into())));
        assert_eq!(site("https://library.kiwix.org/viewer#wikipedia_en_all_maxi_2024-01/A/Main_Page"), Some(Site::KiwixBook("wikipedia_en_all_maxi_2024-01".into())));
        assert_eq!(site("https://browse.library.kiwix.org/content/gutenberg_ale_all_2025-09/A/x"), Some(Site::KiwixBook("gutenberg_ale_all_2025-09".into())));
        assert_eq!(site("https://dumps.wikimedia.org/abwiki/20261001/"), Some(Site::Dump { wiki: "abwiki".into(), date: "20261001".into() }));
        assert_eq!(site("https://www.gutenberg.org/ebooks/1342"), Some(Site::Gutenberg(1342)));
        assert_eq!(site("https://arxiv.org/abs/2101.00001v2"), Some(Site::Arxiv("2101.00001v2".into())));
        assert_eq!(site("https://arxiv.org/pdf/2101.00001.pdf"), Some(Site::Arxiv("2101.00001".into())));
        assert_eq!(site("http://export.arxiv.org/abs/hep-th/9901001v1"), Some(Site::Arxiv("hep-th/9901001v1".into())));
        assert_eq!(site("https://zenodo.org/records/8433301"), Some(Site::Zenodo(8433301)));
        assert_eq!(site("https://doi.org/10.5281/zenodo.8433301"), Some(Site::Zenodo(8433301)));
        assert_eq!(site("https://figshare.com/articles/dataset/Reference_x/34208244/2"), Some(Site::Figshare { id: 34208244, version: Some(2) }));
        assert_eq!(site("https://uni.figshare.com/articles/journal_contribution/x/123"), Some(Site::Figshare { id: 123, version: None }));
        assert_eq!(
            site("https://dataverse.harvard.edu/dataset.xhtml?persistentId=doi:10.7910/DVN/27221&version=1.0"),
            Some(Site::Dataverse { origin: "https://dataverse.harvard.edu".into(), pid: "doi:10.7910/DVN/27221".into(), version: Some("1.0".into()) })
        );
        let hash = "d198bc9d7a6bcf6db04f476d29314f157507d505";
        let heritage = |kind: &str| Some(Site::Heritage { kind: kind.into(), hash: hash.into() });
        assert_eq!(site(&format!("swh:1:dir:{hash};origin=https://github.com/x/y")), heritage("dir"));
        assert_eq!(site(&format!("https://archive.softwareheritage.org/swh:1:rev:{hash}")), heritage("rev"));
        assert_eq!(site(&format!("https://archive.softwareheritage.org/browse/content/sha1_git:{hash}/")), heritage("cnt"));
        assert!(matches!(site("https://data.commoncrawl.org/crawl-data/CC-MAIN-2024-10/wet.paths.gz"), Some(Site::CommonCrawl(_))));
        for other in [
            "https://download.kiwix.org/zim/gutenberg/x.zim.meta4",
            "https://download.kiwix.org/release/kiwix-desktop/",
            "https://library.kiwix.org/#lang=eng",
            "https://dumps.wikimedia.org/abwiki/latest/",
            "https://www.gutenberg.org/ebooks/search/?query=x",
            "https://arxiv.org/abs/notanid",
            "https://arxiv.org/list/hep-th/new",
            "https://zenodo.org/communities/x",
            "https://example.com/dataset.xhtml?persistentId=x",
            "swh:1:snp:d198bc9d7a6bcf6db04f476d29314f157507d505",
            "https://data.commoncrawl.org/crawl-data/CC-MAIN-2024-10/index.html",
        ] {
            assert_eq!(site(other), None, "{other}");
        }
    }

    /// Kiwix as it answers (checked live): a folder's index, and a Metalink for each file.
    fn kiwix_api(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        let meta4 = |name: &str| {
            text(&format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="{name}"><size>2092566</size>
                <hash type="md5">aaf250e3398bb22f129e38d8f7730449</hash><hash type="sha-256">90532f41434bd797a943e7b9dd945915a3206f3c1c095188de52694ba562e41d</hash>
                <url location="nl" priority="1">https://ftp.nluug.nl/pub/kiwix/zim/gutenberg/{name}</url><url location="dk" priority="2">https://mirrors.dotsrc.org/kiwix/zim/gutenberg/{name}</url></file></metalink>"#
            ))
        };
        match path {
            "/zim/gutenberg/" => (200, "Content-Type: text/html\r\n".into(), br#"<html><head><title>Index of /zim/gutenberg</title></head><body><pre><a href="/zim/">Parent Directory</a>
<a href="a.zim">a.zim</a>  2025-06-06 17:00  1.9M
<a href="README">README</a>  2025-06-06 17:00  1.0K</pre></body></html>"#.to_vec()),
            "/zim/gutenberg/a.zim.meta4" => meta4("a.zim"),
            "/zim/gutenberg/gutenberg_ale_all_2025-09.zim.meta4" => meta4("gutenberg_ale_all_2025-09.zim"),
            "/catalog/v2/entries?name=gutenberg_ale_all&count=50" => text(
                r#"<feed><entry><name>gutenberg_ale_all</name><link type="text/html" href="https://browse.library.kiwix.org/content/gutenberg_ale_all_2025-09" />
                <link rel="http://opds-spec.org/acquisition/open-access" type="application/x-zim" href="/zim/gutenberg/gutenberg_ale_all_2025-09.zim.meta4" length="2093056" /></entry></feed>"#,
            ),
            _ => missing(),
        }
    }

    /// A ZIM file is its Metalink's (every mirror, the strongest hash); a folder each of its ZIM
    /// files so, its other files as they are; a library book the file its catalog names, at the
    /// date its link names.
    #[tokio::test]
    async fn kiwix_files_come_with_their_mirrors_and_hashes() {
        let (addr, _) = serve(kiwix_api).await;
        let base = format!("http://{addr}");
        let file = kiwix_file(&http(), &base, "/zim/gutenberg/a.zim", &ListOptions::default()).await.unwrap();
        assert_eq!(file.len(), 1);
        assert_eq!(file[0].urls.len(), 2);
        assert_eq!((file[0].checksum.as_deref(), file[0].size), (Some("sha256:90532f41434bd797a943e7b9dd945915a3206f3c1c095188de52694ba562e41d"), Some(2092566)));
        let folder = kiwix_folder(&http(), &base, "/zim/gutenberg/", &ListOptions::default()).await.unwrap();
        assert_eq!(named(&folder), ["gutenberg/a.zim", "gutenberg/README"]);
        assert!(folder[0].urls[0].as_str().starts_with("https://ftp.nluug.nl/") && folder[0].checksum.is_some() && folder[0].from_document);
        assert_eq!(folder[1].urls[0].as_str(), format!("{base}/zim/gutenberg/README"));
        let book = kiwix_book(&http(), &base, "gutenberg_ale_all_2025-09", &ListOptions::default()).await.unwrap();
        assert_eq!(named(&book), ["gutenberg_ale_all_2025-09.zim"]);
        let older = kiwix_book(&http(), &base, "gutenberg_ale_all_2025-06", &ListOptions::default()).await.unwrap_err();
        assert_eq!(older, "Kiwix has no such file", "the older one is looked for beside the newest");
    }

    /// Wikimedia's dumps as they answer (checked live): a run's status, and the sums of the run on
    /// a mirror that holds it (one file's sum differs there) and on one that does not.
    fn dumps_api(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        match path {
            "/abwiki/20261001/dumpstatus.json" => json(
                r#"{"jobs": {"xmlstubsdump": {"status": "done", "files": {
                    "abwiki-20261001-stub-articles.xml.gz": {"size": 3379603, "url": "/abwiki/20261001/abwiki-20261001-stub-articles.xml.gz", "md5": "41adfcf6703bbfd55cf327f3b49a3430", "sha1": "7ccc2177e518815942aa716b74def11d1f6bac60"},
                    "abwiki-20261001-stub-meta-current.xml.gz": {"size": 3634233, "url": "/abwiki/20261001/abwiki-20261001-stub-meta-current.xml.gz", "md5": "02ebaeb822291b343ead7bdc0d112e04", "sha1": "67dce7643b26a47058ab63d8c8a8773cfd1e1cb4"}}},
                  "articlesdump": {"status": "waiting", "files": {}}}}"#,
            ),
            "/mirror/abwiki/20261001/abwiki-20261001-sha1sums.txt" => text(
                "7ccc2177e518815942aa716b74def11d1f6bac60  abwiki-20261001-stub-articles.xml.gz\n0000000000000000000000000000000000000000  abwiki-20261001-stub-meta-current.xml.gz\n",
            ),
            _ => missing(),
        }
    }

    /// A run lists the files of its finished jobs with their SHA-1; a mirror is added to a file
    /// only where its own sums give the same SHA-1, and a mirror without the run to none.
    #[tokio::test]
    async fn a_dump_run_lists_its_files_with_mirrors_that_hold_them() {
        let (addr, _) = serve(dumps_api).await;
        let base = format!("http://{addr}");
        let (mirror, gone) = (format!("{base}/mirror"), format!("{base}/gone"));
        let (notes, noted) = std::sync::mpsc::channel();
        let options = ListOptions { notes: Some(notes), ..ListOptions::default() };
        let tasks = dump(&http(), &base, &[&mirror, &gone], "abwiki", "20261001", &options).await.unwrap();
        assert_eq!(named(&tasks), ["abwiki-20261001/abwiki-20261001-stub-articles.xml.gz", "abwiki-20261001/abwiki-20261001-stub-meta-current.xml.gz"]);
        assert_eq!(tasks[0].urls.iter().map(Url::as_str).collect::<Vec<_>>(), [
            format!("{base}/abwiki/20261001/abwiki-20261001-stub-articles.xml.gz"),
            format!("{mirror}/abwiki/20261001/abwiki-20261001-stub-articles.xml.gz"),
        ]);
        assert_eq!(tasks[1].urls.len(), 1, "the mirror's copy differs");
        assert_eq!((tasks[0].checksum.as_deref(), tasks[0].size), (Some("sha1:7ccc2177e518815942aa716b74def11d1f6bac60"), Some(3379603)));
        assert!(noted.try_iter().any(|n| n.text.starts_with("1 job(s)")));
        let none = dump(&http(), &base, &[], "abwiki", "20200101", &ListOptions::default()).await.unwrap_err();
        assert_eq!(none, "Wikimedia has no dump run abwiki/20200101");
    }

    /// Project Gutenberg's mirror list and a mirror as they answer (checked live, shortened).
    fn gutenberg_api(base: &str, path: &str) -> Reply {
        match path {
            "/MIRRORS.ALL" => text(&format!(
                " continent | nation | location | provider | url | note\n---+---+---+---+---+---\n \
                 Europe | Great Britain | Kent | UK Mirror Service | ftp://ftp.mirrorservice.org/gutenberg/ | \n \
                 North America | United States | Chapel Hill | ibiblio | https://www.gutenberg.org/dirs/ | Main Project Gutenberg Collection Site\n \
                 North America | United States | San Diego | Project Gutenberg | {base}/pglaf/ | High-speed mirror. Includes cache/generated files (epub, mobi, etc.).\n \
                 North America | United States | Norfolk | ODU | {base}/gutenberg-epub/ | High speed connection\n(4 rows)\n"
            )),
            "/pglaf/cache/epub/1342/pg1342.rdf" => text(
                r#"<rdf:RDF><pgterms:ebook rdf:about="ebooks/1342"><dcterms:title>Pride and Prejudice</dcterms:title>
                <dcterms:hasFormat><pgterms:file rdf:about="https://www.gutenberg.org/ebooks/1342.epub3.images"><dcterms:extent rdf:datatype="x">24864457</dcterms:extent>
                <dcterms:format><rdf:Description><rdf:value rdf:datatype="y">application/epub+zip</rdf:value></rdf:Description></dcterms:format></pgterms:file></dcterms:hasFormat>
                <dcterms:hasFormat><pgterms:file rdf:about="https://www.gutenberg.org/ebooks/1342.kindle.noimages"><dcterms:extent rdf:datatype="x">540013</dcterms:extent>
                <dcterms:format><rdf:Description><rdf:value>application/x-mobipocket-ebook</rdf:value></rdf:Description></dcterms:format></pgterms:file></dcterms:hasFormat>
                <dcterms:hasFormat><pgterms:file rdf:about="https://www.gutenberg.org/files/1342/1342-0.txt"><dcterms:extent rdf:datatype="x">738046</dcterms:extent>
                <dcterms:format><rdf:Description><rdf:value>text/plain; charset=utf-8</rdf:value></rdf:Description></dcterms:format></pgterms:file></dcterms:hasFormat>
                <dcterms:hasFormat><pgterms:file rdf:about="https://www.gutenberg.org/ebooks/1342.html.images"><dcterms:extent>852589</dcterms:extent></pgterms:file></dcterms:hasFormat>
                <dcterms:hasFormat><pgterms:file rdf:about="https://www.gutenberg.org/ebooks/1342.rdf"><dcterms:extent>18220</dcterms:extent></pgterms:file></dcterms:hasFormat>
                </pgterms:ebook></rdf:RDF>"#,
            ),
            _ => missing(),
        }
    }

    #[test]
    fn gutenberg_mirrors_are_read_from_its_list() {
        let list = " continent | nation | location | provider | url | note\n\
                     ---+---\n \
                     Europe | Great Britain | Kent | UK | http://www.mirrorservice.org/sites/gutenberg/ | \n \
                     North America | US | Chapel Hill | ibiblio | https://www.gutenberg.org/dirs/ | Main\n \
                     North America | US | Norfolk | ODU | https://mirror.cs.odu.edu/gutenberg/ | High speed\n \
                     North America | US | Norfolk | ODU | https://mirror.cs.odu.edu/gutenberg-epub/ | High speed\n \
                     North America | US | San Diego | PG | https://gutenberg.pglaf.org/ | Includes cache/generated files (epub, mobi, etc.).\n \
                     North America | US | San Diego | PG | rsync://gutenberg.pglaf.org/gutenberg | Includes cache/generated files\n";
        let found: Vec<(String, Layout)> = gutenberg_mirrors(list, &["https"]).into_iter().map(|(u, l)| (u.to_string(), l)).collect();
        assert_eq!(found, [
            ("https://mirror.cs.odu.edu/gutenberg/".to_string(), Layout::Dirs),
            ("https://mirror.cs.odu.edu/gutenberg-epub/".to_string(), Layout::Epub),
            ("https://gutenberg.pglaf.org/".to_string(), Layout::Full),
        ]);
        assert_eq!((gutenberg_dirs(1342), gutenberg_dirs(7), gutenberg_dirs(10)), ("1/3/4/1342".into(), "0/7".into(), "1/10".into()));
        assert_eq!(book_file("https://www.gutenberg.org/ebooks/1342.epub3.images", 1342), Some(BookFile::Made { name: "pg1342-images-3.epub".into(), kind: "epub3.images".into() }));
        assert_eq!(book_file("http://gutenberg.org/files/1342/1342-h/1342-h.htm", 1342), Some(BookFile::Collection("1342-h/1342-h.htm".into())));
        assert_eq!(book_file("https://www.gutenberg.org/ebooks/1342.qrcode", 1342), None);
    }

    /// A book is its formats (not its RDF or HTML pages), each from the mirrors that keep it where
    /// they keep such files; formats choose some.
    #[tokio::test]
    async fn a_gutenberg_book_lists_its_formats_from_mirrors() {
        let base = serve_knowing(gutenberg_api).await;
        let tasks = gutenberg(&http(), &base, 1342, &["http"], &ListOptions::default()).await.unwrap();
        assert_eq!(named(&tasks), ["Pride and Prejudice (1342)/pg1342-images-3.epub", "Pride and Prejudice (1342)/pg1342.mobi", "Pride and Prejudice (1342)/1342-0.txt"]);
        assert_eq!(tasks[0].urls.iter().map(Url::as_str).collect::<Vec<_>>(), [
            format!("{base}/pglaf/cache/epub/1342/pg1342-images-3.epub"),
            format!("{base}/gutenberg-epub/1342/pg1342-images-3.epub"),
        ]);
        assert_eq!(tasks[2].urls.iter().map(Url::as_str).collect::<Vec<_>>(), [format!("{base}/pglaf/1/3/4/1342/1342-0.txt")]);
        assert_eq!(tasks[0].size, Some(24864457));
        let epub = ListOptions { formats: vec!["epub".into()], ..ListOptions::default() };
        assert_eq!(named(&gutenberg(&http(), &base, 1342, &["http"], &epub).await.unwrap()), ["Pride and Prejudice (1342)/pg1342-images-3.epub"]);
        let kindle = ListOptions { formats: vec!["kindle".into(), "text/plain".into()], ..ListOptions::default() };
        assert_eq!(gutenberg(&http(), &base, 1342, &["http"], &kindle).await.unwrap().len(), 2);
        assert!(gutenberg(&http(), &base, 99, &["http"], &ListOptions::default()).await.unwrap_err().contains("no mirror"));
    }

    #[test]
    fn an_arxiv_paper_is_its_pdf_and_source_from_the_export_host() {
        let tasks = arxiv(ARXIV_EXPORT, "hep-th/9901001v1", &ListOptions::default()).unwrap();
        assert_eq!(tasks[0].urls[0].as_str(), "https://export.arxiv.org/pdf/hep-th/9901001v1");
        assert_eq!(tasks[1].urls[0].as_str(), "https://export.arxiv.org/src/hep-th/9901001v1");
        assert_eq!(named(&tasks[..1]), ["arXiv hep-th_9901001v1/hep-th_9901001v1.pdf"]);
        assert_eq!(tasks[1].name, None, "named as arXiv names it");
        assert!(arxiv_id("2101.00001") && arxiv_id("2101.00001v12") && arxiv_id("0704.0001") && arxiv_id("math.GT/0309136"));
        assert!(!arxiv_id("2101.001") && !arxiv_id("hep-th/99") && !arxiv_id("2101.00001v") && !arxiv_id("x"));
    }

    /// Zenodo, Figshare and Dataverse as they answer (checked live, shortened).
    fn records_api(base: &str, path: &str) -> Reply {
        match path {
            "/api/records/8433301" => json(&format!(
                r#"{{"id": 8433301, "files": [{{"id": "e048", "key": "data and code.zip", "size": 6429758, "checksum": "md5:bdf900a870fec8bf9776a1142fa95031", "links": {{"self": "{base}/api/records/8433301/files/data%20and%20code.zip/content"}}}},
                {{"key": "elsewhere.zip", "size": 1, "links": {{"self": "https://evil.example/x"}}}}]}}"#
            )),
            "/api/records/1" => json(r#"{"id": 1, "files": []}"#),
            "/articles/34208244/versions/2" => json(
                r#"{"files": [{"id": 71491834, "name": "D_brevipes.fasta", "size": 15994, "is_link_only": false, "download_url": "https://ndownloader.figshare.com/files/71491834", "computed_md5": "cedaa99f2b06070b9f4a5adcf11ce49f"},
                {"id": 2, "name": "link", "size": 0, "is_link_only": true, "download_url": "https://example.com/x"}]}"#,
            ),
            "/api/datasets/:persistentId/?persistentId=doi%3A10.7910%2FDVN%2F27221" => json(
                r#"{"status": "OK", "data": {"latestVersion": {"files": [
                {"label": "lat_am.zip", "restricted": false, "dataFile": {"id": 2498426, "filename": "lat_am.zip", "filesize": 2545568, "checksum": {"type": "MD5", "value": "f57184de03766641c510cdc911d9fad1"}}},
                {"label": "t.tab", "directoryLabel": "data/raw", "restricted": false, "dataFile": {"id": 7, "filename": "t.tab", "filesize": 10, "originalFileName": "t.csv", "originalFileSize": 12, "checksum": {"type": "SHA-1", "value": "3e20a009994405f535cdf07cdc2974cef2fce8f2"}}},
                {"label": "secret.zip", "restricted": true, "dataFile": {"id": 8, "filename": "secret.zip"}}]}}}"#,
            ),
            _ => missing(),
        }
    }

    /// A record's files with their MD5 (Zenodo's links on Zenodo only); an article's (not those
    /// that only link elsewhere); a dataset's open files in their folders, a converted table as
    /// uploaded, and what was left out said.
    #[tokio::test]
    async fn research_records_list_their_files_with_checksums() {
        let base = serve_knowing(records_api).await;
        let zenodo_files = zenodo(&http(), &base, 8433301).await.unwrap();
        assert_eq!(named(&zenodo_files), ["zenodo 8433301/data and code.zip"]);
        assert_eq!(zenodo_files[0].checksum.as_deref(), Some("md5:bdf900a870fec8bf9776a1142fa95031"));
        assert!(zenodo(&http(), &base, 1).await.unwrap_err().contains("no files open to everyone"));
        assert_eq!(zenodo(&http(), &base, 2).await.unwrap_err(), "Zenodo has no record 2");
        let article = figshare(&http(), &base, 34208244, Some(2)).await.unwrap();
        assert_eq!(named(&article), ["figshare 34208244/D_brevipes.fasta"]);
        assert_eq!((article[0].size, article[0].checksum.as_deref()), (Some(15994), Some("md5:cedaa99f2b06070b9f4a5adcf11ce49f")));
        let (notes, noted) = std::sync::mpsc::channel();
        let options = ListOptions { notes: Some(notes), ..ListOptions::default() };
        let dataset = dataverse(&http(), &base, "doi:10.7910/DVN/27221", None, &options).await.unwrap();
        assert_eq!(named(&dataset), ["doi_10.7910_DVN_27221/lat_am.zip", "doi_10.7910_DVN_27221/data/raw/t.csv"]);
        assert_eq!(dataset[1].urls[0].as_str(), format!("{base}/api/access/datafile/7?format=original"));
        assert_eq!((dataset[1].size, dataset[1].checksum.as_deref()), (Some(12), Some("sha1:3e20a009994405f535cdf07cdc2974cef2fce8f2")));
        assert!(noted.try_iter().any(|n| n.text.starts_with("1 file(s) of the dataset are restricted")));
    }

    /// The vault: a directory not cooked yet is asked for, then waited for until it is done.
    #[tokio::test]
    async fn a_software_heritage_directory_is_cooked_then_fetched() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static POLLS: AtomicUsize = AtomicUsize::new(0);
        let hash = "d198bc9d7a6bcf6db04f476d29314f157507d505";
        let (addr, hits) = serve(move |path, _| {
            if !path.starts_with("/api/1/vault/flat/swh:1:dir:") {
                return missing();
            }
            match POLLS.fetch_add(1, Ordering::SeqCst) {
                0 => missing(),
                1 | 2 => json(r#"{"status": "pending", "progress_message": "Processing..."}"#),
                _ => json(r#"{"status": "done", "fetch_url": "/api/1/vault/flat/swh:1:dir:x/raw/"}"#),
            }
        })
        .await;
        let base = format!("http://{addr}");
        let tasks = heritage(&http(), &base, "dir", hash).await.unwrap();
        assert_eq!(tasks[0].urls[0].as_str(), format!("{base}/api/1/vault/flat/swh:1:dir:x/raw/"));
        assert_eq!(hits.lock().values().sum::<usize>(), 4, "asked, told to cook, pending once more, done");
        let content = heritage(&http(), &base, "cnt", hash).await.unwrap();
        assert_eq!(content[0].urls[0].as_str(), format!("{base}/api/1/content/sha1_git:{hash}/raw/"));
    }

    /// A paths list is its files on its host, a run's worth at a time, each with its archive line.
    #[tokio::test]
    async fn a_common_crawl_list_is_its_files_a_run_at_a_time() {
        use std::io::Write;
        let mut packed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        packed.write_all(b"crawl-data/CC/segments/1/wet/a.warc.wet.gz\ncrawl-data/CC/segments/1/wet/b.warc.wet.gz\ncrawl-data/CC/segments/1/wet/c.warc.wet.gz\n").unwrap();
        let packed = packed.finish().unwrap();
        let (addr, _) = serve(move |path, _| if path == "/crawl-data/CC/wet.paths.gz" { (200, String::new(), packed.clone()) } else { missing() }).await;
        let url = Url::parse(&format!("http://{addr}/crawl-data/CC/wet.paths.gz")).unwrap();
        let (notes, noted) = std::sync::mpsc::channel();
        let options = ListOptions { max_items: Some(2), only_new: false, notes: Some(notes), ..ListOptions::default() };
        let tasks = common_crawl(&http(), &url, &options, Some(HashSet::new())).await.unwrap();
        assert_eq!(named(&tasks), ["crawl-data/CC/segments/1/wet/a.warc.wet.gz", "crawl-data/CC/segments/1/wet/b.warc.wet.gz"]);
        assert_eq!(tasks[0].urls[0].as_str(), format!("http://{addr}/crawl-data/CC/segments/1/wet/a.warc.wet.gz"));
        assert_eq!(tasks[0].archive, ["commoncrawl crawl-data/CC/segments/1/wet/a.warc.wet.gz"]);
        assert!(noted.try_iter().any(|n| n.text.contains("the 1 after")));
    }
}
