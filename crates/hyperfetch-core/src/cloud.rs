//! Cloud storage share links (WeTransfer, pCloud, Yandex Disk, Box): their file links resolved
//! to the file through the service's own API (Box's through its share page), their folders read
//! into one task per file, each a link of the service's that [`resolve`] takes.
//!
//! A Yandex Disk video the API will not hand over, and a Box share whose page the app cannot
//! read, resolve to the share link itself: its page goes on to yt-dlp, whose extractors take
//! them (see `resolver::yt_dlp_share_page`), as all of their links did before.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use percent_encoding::percent_decode_str;
use reqwest::header::{CONTENT_TYPE, RANGE};
use reqwest::{RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use crate::ingest::{clean_path, Task};
use crate::resolver::ResolverError;

const PCLOUD_API: &str = "https://api.pcloud.com";
const PCLOUD_EU_API: &str = "https://eapi.pcloud.com";
const YANDEX_API: &str = "https://cloud-api.yandex.net";

const YANDEX: &[&str] = &[
    "yadi.sk", "disk.yandex.ru", "disk.yandex.com", "disk.yandex.com.tr", "disk.yandex.by", "disk.yandex.kz",
    "disk.yandex.ua", "disk.360.yandex.ru", "disk.360.yandex.com",
];

/// Most folders one listing reads, and most pages of one Box listing (20 entries each).
const MAX_FOLDERS: usize = 200;
const MAX_PAGES: usize = 1000;

const LISTS_MANY: &str = "this link is a folder: add it as a new download to get one download per file";
const WETRANSFER_GONE: &str = "the WeTransfer transfer has expired or was deleted";
const YANDEX_GONE: &str = "the Yandex Disk link does not exist or was removed";
const BOX_GONE: &str = "the Box link does not exist or was removed";

/// What a share link is, from its shape.
#[derive(Debug, PartialEq)]
enum Link {
    /// A WeTransfer transfer (/downloads/{id}/[{recipient}/]{hash}), or a we.tl link to one.
    WeTransfer,
    /// A pCloud public link, at the API of its region; the file in its folder `file` names.
    PCloud { api: &'static str, code: String, file: Option<u64> },
    /// A Yandex Disk public link (`key`, its /d/{id} or /i/{id} link) and the path in it ("/"
    /// for itself).
    Yandex { key: String, path: String },
    /// A Box shared link (/s/{name}), the file or folder in it it names.
    Box { name: String, file: Option<u64>, folder: Option<u64> },
}

/// Whether `url`'s host is one of `domains` or under one.
fn on(url: &Url, domains: &[&str]) -> bool {
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    domains.iter().any(|d| host == *d || host.strip_suffix(d).is_some_and(|sub| sub.ends_with('.')))
}

/// Whether `s` is an id as these services make them.
fn is_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn link_of(url: &Url) -> Option<Link> {
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let host = url.host_str()?.trim_end_matches('.');
    let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    if host == "we.tl" {
        return matches!(segs[..], [short] if is_id(short)).then_some(Link::WeTransfer);
    }
    if on(url, &["wetransfer.com"]) {
        return transfer_of(url).map(|_| Link::WeTransfer);
    }
    if on(url, &["pcloud.link", "pcloud.com"]) {
        // publink/show?code=..., or the web app's #page=publink&code=...
        let fragment: Vec<(String, String)> =
            url::form_urlencoded::parse(url.fragment().unwrap_or_default().as_bytes()).into_owned().collect();
        let in_fragment = |name: &str| fragment.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let code = match segs[..] {
            ["publink", "show"] => param("code"),
            [] if in_fragment("page").as_deref() == Some("publink") => in_fragment("code"),
            _ => None,
        };
        let file = match param("fileid") {
            Some(file) => Some(file.parse().ok()?),
            None => None,
        };
        let api = if host.starts_with("e.") { PCLOUD_EU_API } else { PCLOUD_API };
        return code.filter(|c| is_id(c)).map(|code| Link::PCloud { api, code, file });
    }
    if on(url, YANDEX) {
        return match segs[..] {
            [kind @ ("d" | "i"), id, ref rest @ ..] if is_id(id) && (kind == "d" || rest.is_empty()) => {
                let rest: Vec<_> = rest.iter().map(|s| percent_decode_str(s).decode_utf8_lossy()).collect();
                Some(Link::Yandex { key: format!("https://{}/{}/{}", host, kind, id), path: format!("/{}", rest.join("/")) })
            }
            _ => None,
        };
    }
    if on(url, &["box.com"]) {
        let link = |name: &str, file, folder| is_id(name).then(|| Link::Box { name: name.to_string(), file, folder });
        return match segs[..] {
            ["s", name] => link(name, None, None),
            ["s", name, "file", file] => link(name, Some(file.parse().ok()?), None),
            ["s", name, "folder", folder] => link(name, None, Some(folder.parse().ok()?)),
            _ => None,
        };
    }
    None
}

/// The transfer id, recipient id (if any) and security hash a WeTransfer download link names.
fn transfer_of(url: &Url) -> Option<(String, Option<String>, String)> {
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segs[..] {
        ["downloads", id, hash] if is_id(id) && is_id(hash) => Some((id.into(), None, hash.into())),
        ["downloads", id, recipient, hash] if [id, recipient, hash].iter().all(|s| is_id(s)) => {
            Some((id.into(), Some(recipient.into()), hash.into()))
        }
        _ => None,
    }
}

/// Whether `url` is a link [`list`] reads: a pCloud, Yandex Disk or Box share that may be a
/// folder (their links do not say).
pub fn lists(url: &Url) -> bool {
    match link_of(url) {
        Some(Link::PCloud { file, .. } | Link::Box { file, .. }) => file.is_none(),
        Some(Link::Yandex { key, .. }) => key.contains("/d/"),
        _ => false,
    }
}

/// One task per file the folder at `url` holds, in a folder named after it; a file's link is its
/// one task, as typed.
pub async fn list(http: &reqwest::Client, url: &Url) -> Result<Vec<Task>, String> {
    let listed = match link_of(url) {
        Some(Link::PCloud { api, code, file: None }) => pcloud_list(http, api, url, &code).await?,
        Some(Link::Yandex { key, path }) => yandex_list(http, YANDEX_API, &key, &path).await?,
        Some(Link::Box { name, file: None, folder }) => box_list(http, url, &name, folder).await?,
        _ => return Err(format!("{} is not a folder link", url)),
    };
    match listed {
        None => Ok(vec![Task { urls: vec![url.clone()], ..Task::default() }]),
        Some(tasks) if tasks.is_empty() => Err("the folder holds no files the app can download".to_string()),
        Some(tasks) => Ok(tasks),
    }
}

/// Whether `url` is a share link [`resolve`] takes (a folder's says it must be listed).
pub fn handles(url: &Url) -> bool {
    link_of(url).is_some()
}

/// Where the file of the share link `url` downloads from (all of pCloud's servers that hold it);
/// the link itself for a share yt-dlp takes over (see the module's doc).
pub async fn resolve(client: &reqwest::Client, url: &Url, _proxy: Option<&str>) -> Result<Vec<Url>, ResolverError> {
    let found = match link_of(url) {
        Some(Link::WeTransfer) => wetransfer_file(client, url).await.map(|file| vec![file]),
        Some(Link::PCloud { api, code, file }) => pcloud_file(client, api, &code, file).await,
        Some(Link::Yandex { key, path }) => {
            yandex_file(client, YANDEX_API, &key, &path).await.map(|file| vec![file.unwrap_or_else(|| url.clone())])
        }
        Some(Link::Box { name, file, .. }) => box_file(client, url, &name, file).await.map(|file| vec![file.unwrap_or_else(|| url.clone())]),
        None => Err(format!("{} is not a share link", url)),
    };
    found.map_err(ResolverError::NotFound)
}

/// A file or folder name made safe as one path component, else its id.
fn component(name: &str, id: &str) -> Result<PathBuf, String> {
    clean_path([name]).or_else(|_| clean_path([id]))
}

/// The status `request` is answered with, and its JSON read as `T` (the default when an error
/// status comes without JSON); `service` names the host in errors, which never name the URL.
async fn get_json<T: DeserializeOwned + Default>(request: RequestBuilder, service: &str) -> Result<(StatusCode, T), String> {
    let cannot_reach = |e: reqwest::Error| format!("Cannot reach {}: {}", service, e.without_url());
    let answer = request.send().await.map_err(cannot_reach)?;
    let status = answer.status();
    let body = answer.bytes().await.map_err(cannot_reach)?;
    match serde_json::from_slice(&body) {
        Ok(json) => Ok((status, json)),
        Err(_) if !status.is_success() => Ok((status, T::default())),
        Err(_) => Err(format!("{} answered HTTP {} with something the app cannot read", service, status)),
    }
}

/// Where WeTransfer serves the transfer `link` names (a .zip of its files, or its one file),
/// once a we.tl link is followed to it.
async fn wetransfer_file(http: &reqwest::Client, link: &Url) -> Result<Url, String> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Answer {
        direct_link: Option<String>,
        message: String,
    }
    let link = match transfer_of(link) {
        Some(_) => link.clone(),
        None => {
            let answer = http.get(link.clone()).send().await;
            answer.map_err(|e| format!("Cannot reach WeTransfer: {}", e.without_url()))?.url().clone()
        }
    };
    let (id, recipient, hash) = transfer_of(&link).ok_or(WETRANSFER_GONE)?;
    let mut body = serde_json::json!({ "intent": "entire_transfer", "security_hash": hash });
    if let Some(recipient) = recipient {
        body["recipient_id"] = recipient.into();
    }
    let request = http
        .post(format!("{}/api/v4/transfers/{}/download", link.origin().ascii_serialization(), id))
        .header(CONTENT_TYPE, "application/json")
        .header("X-Requested-With", "XMLHttpRequest")
        .body(body.to_string());
    let (status, answer): (_, Answer) = get_json(request, "WeTransfer").await?;
    match answer.direct_link.as_deref().and_then(|l| Url::parse(l).ok()) {
        Some(file) if matches!(file.scheme(), "https" | "http") => Ok(file),
        _ if matches!(status.as_u16(), 404 | 410) => Err(WETRANSFER_GONE.to_string()),
        _ => Err(format!("WeTransfer did not hand over the transfer (HTTP {}): {}", status, answer.message)),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PCloudAnswer {
    result: u32,
    error: String,
    path: String,
    hosts: Vec<String>,
    metadata: Option<PCloudItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PCloudItem {
    name: String,
    isfolder: bool,
    fileid: u64,
    folderid: u64,
    size: Option<u64>,
    contents: Vec<PCloudItem>,
}

/// pCloud's `method` at `api` with `query`, when it succeeds.
async fn pcloud_call(http: &reqwest::Client, api: &str, method: &str, query: &[(&str, String)]) -> Result<PCloudAnswer, String> {
    let url = Url::parse_with_params(&format!("{}/{}", api, method), query).map_err(|e| e.to_string())?;
    let (_, answer): (_, PCloudAnswer) = get_json(http.get(url), "pCloud").await?;
    match answer.result {
        0 => Ok(answer),
        // "Please provide 'fileid'": a folder's link.
        1029 => Err(LISTS_MANY.to_string()),
        // Its errors are sentences: "This link is deleted by the owner."
        code if answer.error.is_empty() => Err(format!("pCloud refused the link (error {})", code)),
        _ => Err(format!("pCloud: {}", answer.error)),
    }
}

/// Every server pCloud serves the file of its public link `code` from (the file `file` of it,
/// for a folder's).
async fn pcloud_file(http: &reqwest::Client, api: &str, code: &str, file: Option<u64>) -> Result<Vec<Url>, String> {
    let mut query = vec![("code", code.to_string())];
    query.extend(file.map(|file| ("fileid", file.to_string())));
    let answer = pcloud_call(http, api, "getpublinkdownload", &query).await?;
    let host_name = |h: &&String| !h.is_empty() && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let servers: Vec<Url> = answer
        .hosts
        .iter()
        .filter(host_name)
        .filter(|_| answer.path.starts_with('/'))
        .filter_map(|host| Url::parse(&format!("https://{}{}", host, answer.path)).ok())
        .collect();
    if servers.is_empty() {
        return Err("pCloud did not say where the file is".to_string());
    }
    Ok(servers)
}

/// Every file under the folder of pCloud's public link `code` (subfolders kept), each a task for
/// `link` naming it; None when the link is a file's.
async fn pcloud_list(http: &reqwest::Client, api: &str, link: &Url, code: &str) -> Result<Option<Vec<Task>>, String> {
    let answer = pcloud_call(http, api, "showpublink", &[("code", code.to_string())]).await?;
    let Some(root) = answer.metadata.filter(|m| m.isfolder) else { return Ok(None) };
    let mut tasks = Vec::new();
    let mut folders = vec![(component(&root.name, code)?, root)];
    let mut listed = 0;
    while let Some((folder, item)) = folders.pop() {
        listed += 1;
        if listed > MAX_FOLDERS {
            return Err(format!("the pCloud folder holds more than {} folders: add a subfolder's link instead", MAX_FOLDERS));
        }
        for child in item.contents {
            if child.isfolder {
                folders.push((folder.join(component(&child.name, &child.folderid.to_string())?), child));
                continue;
            }
            let mut url = link.clone();
            url.set_path("/publink/show");
            url.set_fragment(None);
            url.query_pairs_mut().clear().append_pair("code", code).append_pair("fileid", &child.fileid.to_string());
            tasks.push(Task {
                urls: vec![url],
                name: Some(component(&child.name, &child.fileid.to_string())?),
                folder: Some(folder.clone()),
                size: child.size,
                from_document: true,
                ..Task::default()
            });
        }
    }
    Ok(Some(tasks))
}

/// What Yandex Disk's public API answers: a resource (a folder's first entries with it), a
/// download's `href`, or an error's `description`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct YandexItem {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    path: String,
    size: Option<u64>,
    sha256: String,
    media_type: String,
    #[serde(rename = "_embedded")]
    embedded: Option<YandexList>,
    href: String,
    description: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct YandexList {
    items: Vec<YandexItem>,
    total: usize,
}

/// Yandex Disk's public resource `endpoint` ("" for the resource itself, "/download") at `api`,
/// for `path` in the share `key`, its entries from `offset` on.
async fn yandex_get(http: &reqwest::Client, api: &str, endpoint: &str, key: &str, path: &str, offset: usize) -> Result<YandexItem, String> {
    let query = [("public_key", key), ("path", path), ("limit", "1000"), ("offset", &offset.to_string())];
    let url = Url::parse_with_params(&format!("{}/v1/disk/public/resources{}", api, endpoint), query).map_err(|e| e.to_string())?;
    let (status, item): (_, YandexItem) = get_json(http.get(url), "Yandex Disk").await?;
    match status.as_u16() {
        200 => Ok(item),
        404 => Err(YANDEX_GONE.to_string()),
        _ if item.description.is_empty() => Err(format!("Yandex Disk answered HTTP {}", status)),
        _ => Err(format!("Yandex Disk: {}", item.description)),
    }
}

/// Where Yandex Disk serves `path` of the share `key` (a folder as a .zip); None for the share's
/// own video, which the API will not hand over but yt-dlp may still download from its page.
async fn yandex_file(http: &reqwest::Client, api: &str, key: &str, path: &str) -> Result<Option<Url>, String> {
    let refused = match yandex_get(http, api, "/download", key, path, 0).await {
        Ok(answer) => {
            return match Url::parse(&answer.href) {
                Ok(file) if matches!(file.scheme(), "https" | "http") => Ok(Some(file)),
                _ => Err("Yandex Disk did not say where the file is".to_string()),
            };
        }
        Err(e) if e == YANDEX_GONE => return Err(e),
        Err(e) => e,
    };
    let video = path == "/" && yandex_get(http, api, "", key, path, 0).await.is_ok_and(|item| item.media_type == "video");
    if video {
        return Ok(None);
    }
    Err(refused)
}

/// Every file under `path` of the share `key` (subfolders kept), each a task for the share's
/// link to it; None when `path` is a file.
async fn yandex_list(http: &reqwest::Client, api: &str, key: &str, path: &str) -> Result<Option<Vec<Task>>, String> {
    let root = yandex_get(http, api, "", key, path, 0).await?;
    if root.kind != "dir" {
        return Ok(None);
    }
    let base = Url::parse(key).map_err(|e| e.to_string())?;
    let mut tasks = Vec::new();
    let mut folders = VecDeque::from([(path.to_string(), component(&root.name, "Yandex Disk")?)]);
    let mut listed = 0;
    while let Some((dir, folder)) = folders.pop_front() {
        listed += 1;
        if listed > MAX_FOLDERS {
            return Err(format!("the Yandex Disk folder holds more than {} folders: add a subfolder's link instead", MAX_FOLDERS));
        }
        let mut offset = 0;
        loop {
            let page = yandex_get(http, api, "", key, &dir, offset).await?.embedded.unwrap_or_default();
            let read = page.items.len();
            for item in page.items {
                // Untrusted names: an unusable one is left out.
                let Ok(name) = clean_path([item.name.as_str()]) else { continue };
                match item.kind.as_str() {
                    "dir" => folders.push_back((item.path, folder.join(name))),
                    "file" => {
                        let mut url = base.clone();
                        url.path_segments_mut()
                            .map_err(|_| "unusable Yandex Disk link".to_string())?
                            .extend(item.path.split('/').filter(|s| !matches!(*s, "" | "." | "..")));
                        let checksum = Some(format!("sha256:{}", item.sha256)).filter(|c| crate::storage::validate_checksum(c).is_ok());
                        tasks.push(Task {
                            urls: vec![url],
                            name: Some(name),
                            folder: Some(folder.clone()),
                            size: item.size,
                            checksum,
                            from_document: true,
                            ..Task::default()
                        });
                    }
                    _ => {}
                }
            }
            offset += read;
            if read == 0 || offset >= page.total {
                break;
            }
        }
    }
    Ok(Some(tasks))
}

/// What a Box share page says of the share (its `Box.postStreamData`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BoxData {
    #[serde(rename = "/app-api/enduserapp/shared-item")]
    item: Option<BoxShared>,
    #[serde(rename = "/app-api/enduserapp/shared-folder")]
    folder: Option<BoxFolder>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct BoxShared {
    #[serde(rename = "itemID")]
    item_id: u64,
    item_type: String,
}

/// One page of a shared folder's entries.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct BoxFolder {
    current_folder_name: String,
    items: Vec<BoxItem>,
    page_count: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct BoxItem {
    #[serde(rename = "type")]
    kind: String,
    id: u64,
    name: String,
    item_size: Option<u64>,
    granted_permissions: Option<BoxPermissions>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct BoxPermissions {
    item_download: Option<bool>,
}

/// What Box's share page `url` says of its share; None when it says nothing the app can read (a
/// private share's sign-in page, or Box changed the page). A link gone is an error.
async fn box_page(http: &reqwest::Client, url: Url) -> Result<Option<BoxData>, String> {
    const DATA: &str = "Box.postStreamData = ";
    let answer = http.get(url).send().await.map_err(|e| format!("Cannot reach Box: {}", e.without_url()))?;
    if answer.status() == StatusCode::NOT_FOUND {
        return Err(BOX_GONE.to_string());
    }
    let html = answer.text().await.map_err(|e| format!("Cannot reach Box: {}", e.without_url()))?;
    let Some(start) = html.find(DATA) else { return Ok(None) };
    // The object, whatever script follows it.
    Ok(serde_json::Deserializer::from_str(&html[start + DATA.len()..]).into_iter::<BoxData>().next().and_then(Result::ok))
}

/// The download of the file `id` of Box's share `name`, at `origin` (its link's).
fn box_download(origin: &str, name: &str, id: u64) -> Result<Url, String> {
    let query = [("rm", "box_download_shared_file"), ("shared_name", name), ("file_id", &format!("f_{}", id))];
    Url::parse_with_params(&format!("{}/index.php", origin), query).map_err(|e| e.to_string())
}

/// Where Box serves the file of its share `link` (named `name`; the file `file` in it); None when
/// its page says nothing the app can read, or Box will not serve the file (a share with downloads
/// off, whose video yt-dlp may still get from its streams).
async fn box_file(http: &reqwest::Client, link: &Url, name: &str, file: Option<u64>) -> Result<Option<Url>, String> {
    let id = match file {
        Some(id) => id,
        None => match box_page(http, link.clone()).await?.and_then(|data| data.item) {
            Some(item) if item.item_type == "file" => item.item_id,
            Some(item) if item.item_type == "folder" => return Err(LISTS_MANY.to_string()),
            _ => return Ok(None),
        },
    };
    let download = box_download(&link.origin().ascii_serialization(), name, id)?;
    // Its first byte: Box answers a file it will not serve with an error or a page.
    let answer = http.get(download.clone()).header(RANGE, "bytes=0-0").send().await;
    let page = |answer: &reqwest::Response| answer.headers().get(CONTENT_TYPE).is_some_and(|t| t.as_bytes().starts_with(b"text/html"));
    Ok(answer.is_ok_and(|answer| answer.status().is_success() && !page(&answer)).then_some(download))
}

/// Every file under the folder of Box's share `link` (named `name`; the folder `folder` in it),
/// subfolders kept, each a task for its /s/{name}/file/{id} link; None when the share is a file,
/// or its page says nothing the app can read.
async fn box_list(http: &reqwest::Client, link: &Url, name: &str, folder: Option<u64>) -> Result<Option<Vec<Task>>, String> {
    let origin = link.origin().ascii_serialization();
    let root = match folder {
        Some(id) => id,
        None => match box_page(http, link.clone()).await?.and_then(|data| data.item) {
            Some(item) if item.item_type == "folder" => item.item_id,
            _ => return Ok(None),
        },
    };
    let mut folders = VecDeque::from([(root, None::<PathBuf>)]);
    let (mut tasks, mut listed, mut pages) = (Vec::new(), HashSet::new(), 0);
    while let Some((id, mut folder)) = folders.pop_front() {
        if !listed.insert(id) {
            continue;
        }
        if listed.len() > MAX_FOLDERS {
            return Err(format!("the Box folder holds more than {} folders: add a subfolder's link instead", MAX_FOLDERS));
        }
        let mut page = 1;
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                return Err("the Box folder holds too many files to list: add a subfolder's link instead".to_string());
            }
            let url = Url::parse(&format!("{}/s/{}/folder/{}?page={}", origin, name, id, page)).map_err(|e| e.to_string())?;
            let Some(listing) = box_page(http, url).await?.and_then(|data| data.folder) else {
                return Err("Box did not list the folder (its page changed: the app needs an update)".to_string());
            };
            let here = match folder {
                Some(here) => here,
                None => component(&listing.current_folder_name, &id.to_string())?,
            };
            for item in listing.items {
                if item.granted_permissions.is_some_and(|p| p.item_download == Some(false)) {
                    continue;
                }
                let item_name = component(&item.name, &item.id.to_string())?;
                match item.kind.as_str() {
                    "folder" => folders.push_back((item.id, Some(here.join(item_name)))),
                    "file" => tasks.push(Task {
                        urls: vec![Url::parse(&format!("{}/s/{}/file/{}", origin, name, item.id)).map_err(|e| e.to_string())?],
                        name: Some(item_name),
                        folder: Some(here.clone()),
                        size: item.item_size,
                        from_document: true,
                        ..Task::default()
                    }),
                    _ => {}
                }
            }
            folder = Some(here);
            if page >= listing.page_count {
                break;
            }
            page += 1;
        }
    }
    Ok(Some(tasks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::{serve, Reply};

    fn link(link: &str) -> Option<Link> {
        link_of(&Url::parse(link).unwrap())
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn json(status: u16, body: &str) -> Reply {
        (status, "Content-Type: application/json\r\n".into(), body.as_bytes().to_vec())
    }

    #[test]
    fn cloud_links_are_told_by_their_shape() {
        for transfer in [
            "https://we.tl/t-AbC123xyz",
            "https://wetransfer.com/downloads/4a5b6c/20240101/f00d",
            "https://wetransfer.com/downloads/4a5b6c/beef/f00d",
            "https://company.wetransfer.com/downloads/4a5b6c/f00d/",
        ] {
            assert_eq!(link(transfer), Some(Link::WeTransfer), "{transfer}");
        }
        let pcloud = |api, code: &str, file| Some(Link::PCloud { api, code: code.into(), file });
        assert_eq!(link("https://u.pcloud.link/publink/show?code=XZtT1"), pcloud(PCLOUD_API, "XZtT1", None));
        assert_eq!(link("https://e.pcloud.link/publink/show?code=XZ9E&fileid=42"), pcloud(PCLOUD_EU_API, "XZ9E", Some(42)));
        assert_eq!(link("https://my.pcloud.com/#page=publink&code=kZB"), pcloud(PCLOUD_API, "kZB", None));
        let yandex = |key: &str, path: &str| Some(Link::Yandex { key: key.into(), path: path.into() });
        assert_eq!(link("https://yadi.sk/d/-8AWymOPyVZns"), yandex("https://yadi.sk/d/-8AWymOPyVZns", "/"));
        assert_eq!(link("https://disk.yandex.ru/i/abc_D"), yandex("https://disk.yandex.ru/i/abc_D", "/"));
        assert_eq!(link("https://disk.yandex.com/d/abc/sub/a%20b.pdf"), yandex("https://disk.yandex.com/d/abc", "/sub/a b.pdf"));
        let box_link = |file, folder| Some(Link::Box { name: "17vthb1z".into(), file, folder });
        assert_eq!(link("https://app.box.com/s/17vthb1z"), box_link(None, None));
        assert_eq!(link("https://company.ent.box.com/s/17vthb1z/file/272056"), box_link(Some(272056), None));
        assert_eq!(link("https://app.box.com/s/17vthb1z/folder/5012"), box_link(None, Some(5012)));
        for other in [
            "https://wetransfer.com/",
            "https://wetransfer.com/downloads/abc",
            "https://we.tl/",
            "https://u.pcloud.link/publink/show",
            "https://u.pcloud.link/publink/show?code=abc&fileid=x",
            "https://www.pcloud.com/pricing.html",
            "https://disk.yandex.ru/client/disk",
            "https://disk.yandex.ru/i/abc/sub",
            "https://disk.yandex.ru/public?hash=abc",
            "https://app.box.com/folder/123",
            "https://app.box.com/s/abc/file/x",
            "https://example.com/s/abc",
        ] {
            assert_eq!(link(other), None, "{other}");
        }
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(lists(&url("https://yadi.sk/d/abc")) && lists(&url("https://u.pcloud.link/publink/show?code=a")));
        assert!(lists(&url("https://app.box.com/s/abc")) && lists(&url("https://app.box.com/s/abc/folder/1")));
        assert!(!lists(&url("https://yadi.sk/i/abc")) && !lists(&url("https://app.box.com/s/abc/file/1")));
        assert!(!lists(&url("https://we.tl/t-abc")) && !lists(&url("https://e.pcloud.link/publink/show?code=a&fileid=1")));
        assert!(handles(&url("https://we.tl/t-abc")) && !handles(&url("https://example.com/d/abc")));
    }

    fn wetransfer(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        match path {
            "/t-short" => (302, "Location: /downloads/t1/hash1\r\n".into(), Vec::new()),
            "/t-expired" => (302, "Location: /\r\n".into(), Vec::new()),
            "/api/v4/transfers/t1/download" => json(200, r#"{"direct_link":"https://download.wetransfer.test/t1.zip?token=x"}"#),
            _ => json(404, r#"{"message":"Couldn't find Transfer"}"#),
        }
    }

    /// A transfer link, or a short link to one, hands over the transfer's download; one gone,
    /// or a short link that lands nowhere, says it has expired.
    #[tokio::test]
    async fn wetransfer_transfers_are_handed_over() {
        let (addr, hits) = serve(wetransfer).await;
        let base = format!("http://{addr}");
        let at = |path: &str| Url::parse(&format!("{base}{path}")).unwrap();
        let file = "https://download.wetransfer.test/t1.zip?token=x";
        assert_eq!(wetransfer_file(&http(), &at("/downloads/t1/hash1")).await.unwrap().as_str(), file);
        assert_eq!(wetransfer_file(&http(), &at("/t-short")).await.unwrap().as_str(), file);
        assert_eq!(hits.lock().get("/api/v4/transfers/t1/download"), Some(&2));
        assert_eq!(wetransfer_file(&http(), &at("/downloads/gone/r1/hash1")).await.unwrap_err(), WETRANSFER_GONE);
        assert_eq!(wetransfer_file(&http(), &at("/t-expired")).await.unwrap_err(), WETRANSFER_GONE);
    }

    fn pcloud(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        let body = match path {
            "/showpublink?code=FOLDER" => {
                r#"{"result":0,"metadata":{"name":"Pack/1","isfolder":true,"folderid":1,"contents":[
                    {"name":"a.bin","isfolder":false,"fileid":11,"size":3,"hash":17816866203410390504},
                    {"name":"..","isfolder":false,"fileid":12},
                    {"name":"Sub","isfolder":true,"folderid":2,"contents":[{"name":"b.bin","isfolder":false,"fileid":21,"size":5}]}]}}"#
            }
            "/showpublink?code=FILE" => r#"{"result":0,"metadata":{"name":"llama.sh","isfolder":false,"fileid":9}}"#,
            "/getpublinkdownload?code=FILE" => {
                r#"{"result":0,"path":"/D4Zz/llama.sh","hosts":["p1.pcloud.com","evil.test/x?","def4.pcloud.com"]}"#
            }
            "/getpublinkdownload?code=FOLDER&fileid=21" => r#"{"result":0,"path":"/D4Zy/b.bin","hosts":["p1.pcloud.com"]}"#,
            "/getpublinkdownload?code=FOLDER" => r#"{"result":1029,"error":"Please provide 'fileid'."}"#,
            _ => r#"{"result":7002,"error":"This link is deleted by the owner."}"#,
        };
        json(200, body)
    }

    /// A file link resolves to every server pCloud names (only real host names); a folder is one
    /// task per file, subfolders kept, each a link to its file; a link gone says why.
    #[tokio::test]
    async fn pcloud_files_and_folders() {
        let (addr, _) = serve(pcloud).await;
        let (api, http) = (format!("http://{addr}"), http());
        let servers = pcloud_file(&http, &api, "FILE", None).await.unwrap();
        let servers: Vec<_> = servers.iter().map(Url::as_str).collect();
        assert_eq!(servers, ["https://p1.pcloud.com/D4Zz/llama.sh", "https://def4.pcloud.com/D4Zz/llama.sh"]);
        assert_eq!(pcloud_file(&http, &api, "FOLDER", Some(21)).await.unwrap()[0].as_str(), "https://p1.pcloud.com/D4Zy/b.bin");
        assert_eq!(pcloud_file(&http, &api, "FOLDER", None).await.unwrap_err(), LISTS_MANY);
        let link = Url::parse("https://e.pcloud.link/publink/show?code=FOLDER").unwrap();
        let tasks = pcloud_list(&http, &api, &link, "FOLDER").await.unwrap().unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].as_str(), t.folder.clone().unwrap(), t.name.clone().unwrap(), t.size)).collect();
        assert_eq!(
            listed,
            [
                ("https://e.pcloud.link/publink/show?code=FOLDER&fileid=11", "Pack_1".into(), "a.bin".into(), Some(3)),
                ("https://e.pcloud.link/publink/show?code=FOLDER&fileid=12", "Pack_1".into(), "12".into(), None),
                (
                    "https://e.pcloud.link/publink/show?code=FOLDER&fileid=21",
                    PathBuf::from("Pack_1").join("Sub"),
                    "b.bin".into(),
                    Some(5)
                ),
            ]
        );
        assert!(tasks.iter().all(|t| t.from_document));
        assert_eq!(link_of(&tasks[2].urls[0]), Some(Link::PCloud { api: PCLOUD_EU_API, code: "FOLDER".into(), file: Some(21) }));
        assert_eq!(pcloud_list(&http, &api, &link, "FILE").await.unwrap(), None);
        assert_eq!(pcloud_file(&http, &api, "GONE", None).await.unwrap_err(), "pCloud: This link is deleted by the owner.");
    }

    fn yandex(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        let query: Vec<(String, String)> = Url::parse(&format!("http://x{path}")).unwrap().query_pairs().into_owned().collect();
        let get = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or_default();
        let download = path.starts_with("/v1/disk/public/resources/download?");
        let body = match (download, get("public_key"), get("path"), get("offset")) {
            (true, "https://yadi.sk/d/F", "/Sub/b c.pdf", _) => r#"{"href":"https://downloader.disk.yandex.ru/disk/b?x=1","method":"GET"}"#,
            (true, "https://yadi.sk/i/V", "/", _) => {
                return json(403, r#"{"error":"DiskResourceDownloadLimitExceededError","description":"Download limit exceeded."}"#)
            }
            (false, "https://yadi.sk/i/V", "/", _) => r#"{"type":"file","name":"clip.mp4","media_type":"video"}"#,
            (true, "https://yadi.sk/d/F", "/", _) => {
                return json(403, r#"{"error":"DiskResourceDownloadLimitExceededError","description":"Download limit exceeded."}"#)
            }
            (false, "https://yadi.sk/d/F", "/", "0") => {
                r#"{"type":"dir","name":"Pack","path":"/","_embedded":{"total":4,"items":[
                    {"type":"file","name":"a.pdf","path":"/a.pdf","size":3,"sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"},
                    {"type":"dir","name":"Sub","path":"/Sub"}]}}"#
            }
            (false, "https://yadi.sk/d/F", "/", "2") => {
                r#"{"type":"dir","name":"Pack","path":"/","_embedded":{"total":4,"items":[
                    {"type":"file","name":"..","path":"/.."},
                    {"type":"file","name":"z.txt","path":"/z.txt","size":1}]}}"#
            }
            (false, "https://yadi.sk/d/F", "/Sub", "0") => {
                r#"{"type":"dir","name":"Sub","path":"/Sub","_embedded":{"total":1,"items":[{"type":"file","name":"b c.pdf","path":"/Sub/b c.pdf","size":7}]}}"#
            }
            (false, "https://yadi.sk/d/F", "/a.pdf", _) => r#"{"type":"file","name":"a.pdf","path":"/a.pdf"}"#,
            _ => return json(404, r#"{"error":"DiskNotFoundError","description":"Resource not found."}"#),
        };
        json(200, body)
    }

    /// A folder is one task per file, page after page, subfolders kept, each a link to its file
    /// in the share that resolves to its download; a video the API will not hand over goes on
    /// to yt-dlp, another file's refusal says why, a link gone says so.
    #[tokio::test]
    async fn yandex_disk_files_and_folders() {
        let (addr, _) = serve(yandex).await;
        let (api, http) = (format!("http://{addr}"), http());
        let tasks = yandex_list(&http, &api, "https://yadi.sk/d/F", "/").await.unwrap().unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].as_str(), t.folder.clone().unwrap(), t.name.clone().unwrap(), t.size)).collect();
        assert_eq!(
            listed,
            [
                ("https://yadi.sk/d/F/a.pdf", "Pack".into(), "a.pdf".into(), Some(3)),
                ("https://yadi.sk/d/F/z.txt", "Pack".into(), "z.txt".into(), Some(1)),
                ("https://yadi.sk/d/F/Sub/b%20c.pdf", PathBuf::from("Pack").join("Sub"), "b c.pdf".into(), Some(7)),
            ]
        );
        let sha = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(tasks[0].checksum.as_deref(), Some(sha));
        assert_eq!(tasks[1].checksum, None);
        let Some(Link::Yandex { key, path }) = link_of(&tasks[2].urls[0]) else { panic!("not a Yandex link") };
        let file = yandex_file(&http, &api, &key, &path).await.unwrap().unwrap();
        assert_eq!(file.as_str(), "https://downloader.disk.yandex.ru/disk/b?x=1");
        assert_eq!(yandex_list(&http, &api, "https://yadi.sk/d/F", "/a.pdf").await.unwrap(), None);
        assert_eq!(yandex_file(&http, &api, "https://yadi.sk/i/V", "/").await.unwrap(), None);
        assert_eq!(yandex_file(&http, &api, "https://yadi.sk/d/F", "/").await.unwrap_err(), "Yandex Disk: Download limit exceeded.");
        assert_eq!(yandex_file(&http, &api, "https://yadi.sk/d/GONE", "/").await.unwrap_err(), YANDEX_GONE);
        assert_eq!(yandex_list(&http, &api, "https://yadi.sk/d/GONE", "/").await.unwrap_err(), YANDEX_GONE);
    }

    /// A Box page with its share data, as Box writes it (escaped slashes, a script after it).
    fn box_html(data: &str) -> Reply {
        let html = format!("<html><script> Box.postStreamData = {data}; Box.other = {{}};</script></html>");
        (200, "Content-Type: text/html\r\n".into(), html.replace('/', "\\/").replace("<\\/", "</").into_bytes())
    }

    fn box_share(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        let shared = |kind: &str, id: u64| format!(r#""/app-api/enduserapp/shared-item":{{"sharedName":"S","itemID":{id},"itemType":"{kind}"}}"#);
        match path {
            "/s/file1" => box_html(&format!("{{{}}}", shared("file", 77))),
            "/s/fold1" => box_html(&format!("{{{}}}", shared("folder", 1))),
            "/s/fold1/folder/1?page=1" => box_html(&format!(
                r#"{{{},"/app-api/enduserapp/shared-folder":{{"currentFolderName":"My/Pack","pageCount":2,"items":[
                    {{"type":"file","id":11,"name":"a.zip","itemSize":3,"grantedPermissions":{{"itemDownload":true}}}},
                    {{"type":"folder","id":2,"name":"Sub"}},
                    {{"type":"file","id":12,"name":"locked.zip","grantedPermissions":{{"itemDownload":false}}}}]}}}}"#,
                shared("folder", 1)
            )),
            "/s/fold1/folder/1?page=2" => box_html(
                r#"{"/app-api/enduserapp/shared-folder":{"currentFolderName":"My/Pack","pageCount":2,"items":[
                    {"type":"file","id":13,"name":"b.zip"},{"type":"web_link","id":14,"name":"site"}]}}"#,
            ),
            "/s/fold1/folder/2?page=1" => box_html(
                r#"{"/app-api/enduserapp/shared-folder":{"currentFolderName":"Sub","pageCount":1,"items":[
                    {"type":"file","id":21,"name":"..","itemSize":9},{"type":"folder","id":1,"name":"Loop back"}]}}"#,
            ),
            "/s/odd1" => (200, "Content-Type: text/html\r\n".into(), b"<html>Sign in</html>".to_vec()),
            // Downloads off.
            "/s/view1" => box_html(&format!("{{{}}}", shared("file", 78))),
            "/index.php?rm=box_download_shared_file&shared_name=view1&file_id=f_78" => (403, "Content-Type: text/html\r\n".into(), b"<html>No</html>".to_vec()),
            p if p.starts_with("/index.php?") => (206, "Content-Type: application/zip\r\n".into(), b"P".to_vec()),
            _ => (404, "Content-Type: text/html\r\n".into(), b"<html>gone</html>".to_vec()),
        }
    }

    /// A file share resolves to its download (from its page, or from the file id its link names);
    /// a folder is one task per file Box lets download, page after page, subfolders kept, each
    /// read once; a page the app cannot read, or a file Box will not serve, goes on to yt-dlp; a
    /// link gone says so.
    #[tokio::test]
    async fn box_files_and_folders() {
        let (addr, _) = serve(box_share).await;
        let (base, http) = (format!("http://{addr}"), http());
        let at = |path: &str| Url::parse(&format!("{base}{path}")).unwrap();
        let download = |name: &str, id: &str| format!("{base}/index.php?rm=box_download_shared_file&shared_name={name}&file_id=f_{id}");
        assert_eq!(box_file(&http, &at("/s/file1"), "file1", None).await.unwrap().unwrap().as_str(), download("file1", "77"));
        assert_eq!(box_file(&http, &at("/s/fold1/file/5"), "fold1", Some(5)).await.unwrap().unwrap().as_str(), download("fold1", "5"));
        assert_eq!(box_file(&http, &at("/s/fold1"), "fold1", None).await.unwrap_err(), LISTS_MANY);
        assert_eq!(box_file(&http, &at("/s/odd1"), "odd1", None).await.unwrap(), None);
        assert_eq!(box_file(&http, &at("/s/view1"), "view1", None).await.unwrap(), None);
        assert_eq!(box_file(&http, &at("/s/gone1"), "gone1", None).await.unwrap_err(), BOX_GONE);
        let tasks = box_list(&http, &at("/s/fold1"), "fold1", None).await.unwrap().unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.folder.clone().unwrap(), t.name.clone().unwrap(), t.size)).collect();
        assert_eq!(
            listed,
            [
                (format!("{base}/s/fold1/file/11"), "My_Pack".into(), "a.zip".into(), Some(3)),
                (format!("{base}/s/fold1/file/13"), "My_Pack".into(), "b.zip".into(), None),
                (format!("{base}/s/fold1/file/21"), PathBuf::from("My_Pack").join("Sub"), "21".into(), Some(9)),
            ]
        );
        assert!(tasks.iter().all(|t| t.from_document));
        // From the subfolder, the folder it loops back to is read too, once.
        assert_eq!(box_list(&http, &at("/s/fold1/folder/2"), "fold1", Some(2)).await.unwrap().unwrap().len(), 3);
        assert_eq!(box_list(&http, &at("/s/file1"), "file1", None).await.unwrap(), None);
        assert_eq!(box_list(&http, &at("/s/gone1"), "gone1", None).await.unwrap_err(), BOX_GONE);
    }

    fn live(link: &str) -> Url {
        Url::parse(link).unwrap()
    }

    /// Live: a small public pCloud file resolves to its servers; a public folder lists its files.
    #[tokio::test]
    #[ignore]
    async fn live_pcloud() {
        let file = resolve(&http(), &live("https://u.pcloud.link/publink/show?code=XZtTpwVZnMvzSRIY2vhs6zCQgfHtI8Oya3YV"), None).await.unwrap();
        assert!(file[0].path().ends_with("/llama.sh"), "{file:?}");
        let tasks = list(&http(), &live("https://u.pcloud.link/publink/show?code=kZBjoO0Z0N6DdHilOAyiT3Vnb4u0nRJ2WBxk")).await.unwrap();
        assert!(tasks.len() > 1 && tasks.iter().all(|t| t.folder == Some("debs".into())), "{tasks:?}");
        assert!(resolve(&http(), &tasks[0].urls[0], None).await.is_ok());
    }

    /// Live: a small public Yandex Disk file resolves to its download; a public folder lists its
    /// files; a link gone says so.
    #[tokio::test]
    #[ignore]
    async fn live_yandex_disk() {
        let file = resolve(&http(), &live("https://yadi.sk/d/BdyBP6SpI1-HVg"), None).await.unwrap();
        assert!(file[0].as_str().contains("filename=AllDatasets.zip"), "{file:?}");
        let tasks = list(&http(), &live("https://yadi.sk/d/loPpY45J3EAYfU")).await.unwrap();
        assert!(tasks.len() > 50 && tasks.iter().all(|t| t.folder == Some("RL_lectures".into())), "{}", tasks.len());
        let err = resolve(&http(), &live("https://yadi.sk/d/MBruQdRo3WSdCx"), None).await.unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    /// Live: a public Box file share resolves to its download (not fetched); a folder lists its
    /// files.
    #[tokio::test]
    #[ignore]
    async fn live_box() {
        let file = resolve(&http(), &live("https://app.box.com/s/17vthb1zl0zeh340m4gaw0luuf2vscne"), None).await.unwrap();
        assert!(file[0].as_str().ends_with("file_id=f_272056515134"), "{file:?}");
        let tasks = list(&http(), &live("https://app.box.com/s/1cwdnolsmtf0s04o0hshbv4vxiuqcmi9")).await.unwrap();
        assert!(tasks.iter().any(|t| t.name == Some("osgeopy-data-misc.zip".into())), "{tasks:?}");
    }

    /// Live: WeTransfer's API says a transfer that does not exist has expired.
    #[tokio::test]
    #[ignore]
    async fn live_wetransfer_gone() {
        let link = live("https://wetransfer.com/downloads/0123456789abcdef0123456789abcdef20240101000000/abc123");
        assert_eq!(resolve(&http(), &link, None).await.unwrap_err().to_string(), ResolverError::NotFound(WETRANSFER_GONE.into()).to_string());
    }
}
