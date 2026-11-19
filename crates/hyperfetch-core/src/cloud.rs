//! Cloud storage share links (WeTransfer, pCloud, Yandex Disk, Box): their file links resolved
//! to the file through the service's own API (Box's through its share page), their folders read
//! into one task per file, each a link of the service's that [`resolve`] takes.
//!
//! A Yandex Disk video the API will not hand over, and a Box share whose page the app cannot
//! read, resolve to the share link itself: its page goes on to yt-dlp, whose extractors take
//! them (see `resolver::yt_dlp_share_page`), as all of their links did before.
//!
//! Nextcloud and ownCloud public shares (`/s/{token}` on any host that runs one) are listed over
//! the share's public WebDAV into one task per file, a shared file into its one task.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use percent_encoding::percent_decode_str;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, RANGE};
use reqwest::{RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use crate::ingest::{clean_path, ListOptions, Task};
use crate::resolver::ResolverError;

const PCLOUD_API: &str = "https://api.pcloud.com";
const PCLOUD_EU_API: &str = "https://eapi.pcloud.com";
const YANDEX_API: &str = "https://cloud-api.yandex.net";
const SWISSTRANSFER: &str = "https://www.swisstransfer.com";
const SWISSTRANSFER_GONE: &str = "the SwissTransfer transfer has expired or was deleted";
const SWISSTRANSFER_LOCKED: &str = "the SwissTransfer transfer wants a password: add its link again with the password on the next line (Password: ...)";
const SWISSTRANSFER_REFUSED: &str = "the SwissTransfer transfer's password is wrong";
/// Cloud Storage's JSON API, which lists a public bucket to anyone.
const GCS_API: &str = "https://storage.googleapis.com";
/// The Azure Blob service version asked for: older ones cannot read every kind of blob.
const AZURE_VERSION: &str = "2021-08-06";
/// Most objects one bucket listing reads.
const MAX_OBJECTS: usize = 100_000;
const BUCKET_PRIVATE: &str = "the bucket does not let anyone list it: listing it needs its owner's access key, which the app signs with only for S3 and S3-compatible servers; add the links of its files instead";
const S3_PRIVATE: &str = "the bucket does not let anyone list it: give an access key that may (in the app, Settings > Debrid & hosts; in the CLI, --s3-access-key and --s3-secret-key)";
const S3_REFUSED: &str = "the bucket refused the access key: check the key, its secret, the session token and the region (set the region when the bucket's address does not name it)";
const BUCKET_GONE: &str = "the bucket does not exist, or is private (Azure answers so for a private container)";

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
const SHARE_GONE: &str = "the share does not exist or was removed (or has no such folder)";
const SHARE_LOCKED: &str = "the share wants a password (or no longer exists): add its link with the password before the host, https://TOKEN:PASSWORD@host/s/TOKEN, where TOKEN is the share's";
const SHARE_REFUSED: &str = "the share's password is wrong (or the share no longer exists); give it as https://TOKEN:PASSWORD@host/s/TOKEN, where TOKEN is the share's";

/// What a Nextcloud or ownCloud share's WebDAV is asked of each entry.
const PROPFIND: &str = r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><d:displayname/><d:getcontentlength/><d:resourcetype/><oc:checksums/></d:prop></d:propfind>"#;

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
    /// A SwissTransfer transfer (swisstransfer.com/d/{link}).
    SwissTransfer { link: String },
    /// A public bucket, or a folder of it: the objects whose keys start with `prefix`.
    Bucket { bucket: Bucket, prefix: String },
    /// A WebDAV file or folder (webdav://, webdavs://, dav:// or davs://), at its http(s) address.
    WebDav { link: Url },
}

/// The http(s) address of a `webdav://`, `webdavs://`, `dav://` or `davs://` link, without its
/// sign-in.
fn webdav_of(url: &Url) -> Option<Url> {
    let scheme = match url.scheme() {
        "webdav" | "dav" => "http",
        "webdavs" | "davs" => "https",
        _ => return None,
    };
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
    let path = if url.path().is_empty() { "/" } else { url.path() };
    Url::parse(&format!("{scheme}://{}{port}{path}{query}", url.host_str()?)).ok()
}

/// Where a bucket is listed and its objects are read.
#[derive(Debug, PartialEq)]
enum Bucket {
    /// An S3 or S3-compatible bucket `name`, at `base` (its own host's root, or the endpoint's
    /// /{bucket}/); `md5_etags` when a one-part upload's ETag is its MD5, as Amazon's are.
    S3 { base: Url, name: String, md5_etags: bool },
    /// A Google Cloud Storage bucket.
    Gcs { bucket: String },
    /// An Azure Blob container `name`, at https://{account}.blob.core.windows.net/{name}/.
    Azure { base: Url, name: String },
}

/// The bucket and prefix of a link that lists one: `s3://` and `gs://` with no key or a key
/// ending in `/`, an S3, S3-compatible, Cloud Storage or Azure address of a bucket or of a folder
/// (a path ending in `/`, or `?prefix=`). A link to an object is not one.
fn bucket_of(url: &Url) -> Option<(Bucket, String)> {
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let path = percent_decode_str(url.path()).decode_utf8_lossy().into_owned();
    let query = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    let folder = |key: &str| (key.is_empty() || key.ends_with('/')).then(|| key.to_string());
    let https = |s: &str| Url::parse(s).ok();
    match url.scheme() {
        "s3" => return Some((Bucket::S3 { base: https(&format!("https://{host}.s3.amazonaws.com/"))?, name: host, md5_etags: true }, folder(path.trim_start_matches('/'))?)),
        "gs" => return Some((Bucket::Gcs { bucket: host }, folder(path.trim_start_matches('/'))?)),
        "http" | "https" => {}
        _ => return None,
    }
    let origin = url.origin().ascii_serialization();
    // The key after the bucket in a path-style address, and the bucket.
    let path_style = || {
        let (bucket, key) = path.trim_start_matches('/').split_once('/').unwrap_or((path.trim_start_matches('/'), ""));
        Some((bucket.to_string(), query("prefix").or_else(|| folder(key))?)).filter(|(b, _)| !b.is_empty())
    };
    let labels: Vec<&str> = host.split('.').collect();
    let s3_hosts = ["amazonaws.com", "wasabisys.com", "backblazeb2.com"];
    if s3_hosts.iter().any(|h| host.ends_with(&format!(".{h}"))) {
        // {bucket}.s3[.-{region}].{host} or s3[.-{region}].{host}/{bucket}; not a website endpoint.
        let at = labels.iter().position(|l| *l == "s3" || (l.starts_with("s3-") && !l.starts_with("s3-website")))?;
        let md5_etags = host.ends_with(".amazonaws.com");
        return if at == 0 {
            let (bucket, prefix) = path_style()?;
            Some((Bucket::S3 { base: https(&format!("{origin}/{bucket}/"))?, name: bucket, md5_etags }, prefix))
        } else {
            let name = labels[..at].join(".");
            Some((Bucket::S3 { base: https(&format!("{origin}/"))?, name, md5_etags }, query("prefix").or_else(|| folder(path.trim_start_matches('/')))?))
        };
    }
    if host.ends_with(".digitaloceanspaces.com") {
        // {bucket}.{region}.digitaloceanspaces.com, or {region}.digitaloceanspaces.com/{bucket}.
        return match labels.len() {
            4 => Some((Bucket::S3 { base: https(&format!("{origin}/"))?, name: labels[0].to_string(), md5_etags: false }, query("prefix").or_else(|| folder(path.trim_start_matches('/')))?)),
            3 => path_style().and_then(|(bucket, prefix)| Some((Bucket::S3 { base: https(&format!("{origin}/{bucket}/"))?, name: bucket, md5_etags: false }, prefix))),
            _ => None,
        };
    }
    if host == "storage.googleapis.com" {
        let (bucket, prefix) = path_style()?;
        return (!["storage", "download", "upload"].contains(&bucket.as_str())).then_some((Bucket::Gcs { bucket }, prefix));
    }
    if let Some(bucket) = host.strip_suffix(".storage.googleapis.com") {
        return Some((Bucket::Gcs { bucket: bucket.to_string() }, query("prefix").or_else(|| folder(path.trim_start_matches('/')))?));
    }
    if host.ends_with(".blob.core.windows.net") {
        let listing = query("comp").as_deref() == Some("list");
        let (container, key) = path.trim_start_matches('/').split_once('/').unwrap_or((path.trim_start_matches('/'), ""));
        let prefix = query("prefix").or_else(|| if listing { Some(String::new()) } else { folder(key) })?;
        return (!container.is_empty()).then(|| Some((Bucket::Azure { base: https(&format!("{origin}/{container}/"))?, name: container.to_string() }, prefix))).flatten();
    }
    None
}

/// The https address of the object an `s3://` or `gs://` link names (one whose key does not end
/// in `/`, which lists a folder instead).
pub fn bucket_object(url: &Url) -> Option<Url> {
    let key = url.path().trim_start_matches('/');
    if key.is_empty() || key.ends_with('/') {
        return None;
    }
    let host = url.host_str()?;
    match url.scheme() {
        "s3" => Url::parse(&format!("https://{host}.s3.amazonaws.com/{key}")).ok(),
        "gs" => Url::parse(&format!("https://storage.googleapis.com/{host}/{key}")).ok(),
        _ => None,
    }
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
    if let Some((bucket, prefix)) = bucket_of(url) {
        return Some(Link::Bucket { bucket, prefix });
    }
    if let Some(link) = webdav_of(url) {
        return Some(Link::WebDav { link });
    }
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
    if on(url, &["swisstransfer.com"]) {
        return match segs[..] {
            ["d", link] if is_id(link) => Some(Link::SwissTransfer { link: link.to_string() }),
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
/// folder (their links do not say), a SwissTransfer transfer, or one shaped as a Nextcloud or
/// ownCloud share.
pub fn lists(url: &Url) -> bool {
    match link_of(url) {
        Some(Link::PCloud { file, .. } | Link::Box { file, .. }) => file.is_none(),
        Some(Link::Yandex { key, .. }) => key.contains("/d/"),
        Some(Link::SwissTransfer { .. } | Link::Bucket { .. } | Link::WebDav { .. }) => true,
        _ => share_of(url).is_some(),
    }
}

/// One task per file the folder at `url` holds, in a folder named after it; a file's link is its
/// one task, as typed. A Nextcloud or ownCloud share is read with the Authorization `options`
/// gave its link (a share's password, see [`share_tasks`]); None when its host runs neither. A
/// SwissTransfer transfer opens with the password a paste gave.
pub async fn list(http: &reqwest::Client, url: &Url, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    if let Some((bucket, prefix)) = options.s3.as_ref().and_then(|key| endpoint_bucket(url, key)) {
        return Some(bucket_list(http, &bucket, &prefix, GCS_API, options.s3.as_ref()).await);
    }
    match link_of(url) {
        Some(Link::SwissTransfer { link }) => return Some(swisstransfer_list(http, SWISSTRANSFER, &link, options.password.as_deref()).await),
        Some(Link::WebDav { link }) => {
            // The sign-in a paste took out of the link, else one still in it.
            use base64::Engine as _;
            let pasted = options.auth.as_ref().filter(|(typed, _)| typed == url).map(|(_, auth)| auth.clone());
            let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
            let pair = format!("{}:{}", decode(url.username()), decode(url.password().unwrap_or_default()));
            let inline = (!url.username().is_empty()).then(|| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(pair)));
            return Some(webdav_list(http, &link, pasted.or(inline).as_deref()).await);
        }
        _ => {}
    }
    match share_of(url) {
        Some(share) => {
            let auth = options.auth.as_ref().filter(|(link, _)| link == url).map(|(_, header)| header.as_str());
            share_tasks(http, &share, auth).await
        }
        None => Some(list_link(http, url, options.s3.as_ref()).await),
    }
}

/// Whether `url` is a folder link on the S3-compatible server the user's key names (see
/// `sigv4::S3Key::endpoint`), which [`list`] lists as a bucket.
pub fn endpoint_lists(url: &Url, options: &ListOptions) -> bool {
    options.s3.as_ref().is_some_and(|key| endpoint_bucket(url, key).is_some())
}

/// The bucket and prefix of a folder link on `key`'s endpoint, path-style: /{bucket}/ or
/// /{bucket}/{folder}/, or with `?prefix=`.
fn endpoint_bucket(url: &Url, key: &crate::sigv4::S3Key) -> Option<(Bucket, String)> {
    let endpoint = key.endpoint.as_ref()?;
    let same = endpoint.host_str()?.eq_ignore_ascii_case(url.host_str()?) && endpoint.port_or_known_default() == url.port_or_known_default();
    if !same || !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let path = percent_decode_str(url.path()).decode_utf8_lossy().into_owned();
    let (bucket, folder) = path.trim_start_matches('/').split_once('/')?;
    let prefix = url.query_pairs().find(|(k, _)| k == "prefix").map(|(_, v)| v.into_owned());
    let prefix = prefix.or_else(|| (folder.is_empty() || folder.ends_with('/')).then(|| folder.to_string()))?;
    let base = Url::parse(&format!("{}/{bucket}/", url.origin().ascii_serialization())).ok()?;
    (!bucket.is_empty()).then(|| (Bucket::S3 { base, name: bucket.to_string(), md5_etags: false }, prefix))
}

/// [`list`] for the services' own links; a bucket's with `s3`, the user's key.
async fn list_link(http: &reqwest::Client, url: &Url, s3: Option<&crate::sigv4::S3Key>) -> Result<Vec<Task>, String> {
    let listed = match link_of(url) {
        Some(Link::PCloud { api, code, file: None }) => pcloud_list(http, api, url, &code).await?,
        Some(Link::Yandex { key, path }) => yandex_list(http, YANDEX_API, &key, &path).await?,
        Some(Link::Box { name, file: None, folder }) => box_list(http, url, &name, folder).await?,
        Some(Link::Bucket { bucket, prefix }) => Some(bucket_list(http, &bucket, &prefix, GCS_API, s3).await?),
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
        Some(Link::SwissTransfer { .. } | Link::Bucket { .. } | Link::WebDav { .. }) => Err(LISTS_MANY.to_string()),
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

/// A link shaped as a Nextcloud or ownCloud public share: {base}[/index.php]/s/{token}[?path=...].
#[derive(Debug, PartialEq)]
struct Share {
    /// Where the server's own pages are: its scheme, host and the path it is installed under.
    base: String,
    /// The link up to its token, as typed.
    link: String,
    token: String,
    /// The folder in the share the link opens (`?path=`), "/" for its top.
    path: String,
}

fn share_of(url: &Url) -> Option<Share> {
    if link_of(url).is_some() || !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let segs: Vec<&str> = url.path_segments()?.collect();
    let segs = segs.strip_suffix(&[""]).unwrap_or(&segs);
    let [before @ .., "s", token] = segs else { return None };
    if !is_id(token) {
        return None;
    }
    let origin = url.origin().ascii_serialization();
    let join = |parts: &[&str]| std::iter::once(origin.as_str()).chain(parts.iter().copied()).collect::<Vec<_>>().join("/");
    let path = url.query_pairs().find(|(k, _)| k == "path").map_or_else(|| "/".to_string(), |(_, v)| v.into_owned());
    Some(Share {
        base: join(before.strip_suffix(&["index.php"]).unwrap_or(before)),
        link: join(&[before, &["s", token]].concat()),
        token: token.to_string(),
        path,
    })
}

/// One file or folder a WebDAV listing names.
#[derive(Debug, Default)]
struct DavEntry {
    href: String,
    name: String,
    folder: bool,
    size: Option<u64>,
    /// The `oc:checksums` the server keeps ("SHA1:... MD5:..."), if any.
    checksums: String,
}

/// The entries of a WebDAV PROPFIND answer.
fn dav_entries(xml: &str) -> Vec<DavEntry> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let (mut entries, mut entry, mut tag) = (Vec::new(), None::<DavEntry>, String::new());
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                match tag.as_str() {
                    "response" => entry = Some(DavEntry::default()),
                    "collection" => entry.iter_mut().for_each(|entry| entry.folder = true),
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if e.local_name().as_ref() == b"collection" => entry.iter_mut().for_each(|entry| entry.folder = true),
            Ok(Event::Text(t)) => {
                let text = t.unescape().map_or_else(|_| String::from_utf8_lossy(&t).into_owned(), |t| t.into_owned());
                let Some(entry) = entry.as_mut() else { continue };
                match tag.as_str() {
                    "href" => entry.href = text,
                    "displayname" => entry.name = text,
                    "getcontentlength" => entry.size = text.trim().parse().ok(),
                    "checksum" => entry.checksums = format!("{} {}", entry.checksums, text),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"response" {
                    entries.extend(entry.take());
                }
                tag.clear();
            }
            Ok(Event::Eof) | Err(_) => return entries,
            _ => {}
        }
    }
}

/// The strongest checksum among a server's `oc:checksums`.
fn dav_checksum(checksums: &str) -> Option<String> {
    ["sha256", "sha1", "md5"].into_iter().find_map(|algo| {
        let hex = checksums.split_whitespace().find_map(|c| c.split_once(':').filter(|(a, _)| a.eq_ignore_ascii_case(algo)).map(|(_, hex)| hex))?;
        Some(format!("{}:{}", algo, hex.to_ascii_lowercase())).filter(|c| crate::storage::validate_checksum(c).is_ok())
    })
}

/// The path `href` names inside the WebDAV folder `root`, a part per folder, decoded; None when it
/// is not inside it.
fn dav_path(root: &Url, href: &str) -> Option<Vec<String>> {
    let path = root.join(href).ok()?.path().to_string();
    let rest = path.strip_prefix(root.path()).or_else(|| (path == root.path().trim_end_matches('/')).then_some(""))?;
    Some(rest.split('/').filter(|s| !s.is_empty()).map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned()).collect())
}

/// How a share's WebDAV answered.
enum Walk {
    Listed(Vec<Task>),
    /// It wants a password, or a right one.
    Locked,
    /// Not there: an older server without it, or no such share or folder.
    Absent,
}

/// The files of the share `share` on a host that runs Nextcloud or ownCloud (its `status.php`
/// says it is installed), read with `auth` (the user's `TOKEN:PASSWORD` sign-in, for a share with
/// a password); None for any other host. Nextcloud 29 and later list it over
/// `public.php/dav/files/{token}` (any user name goes with the password), ownCloud and older ones
/// over `public.php/webdav`, whose user name is the token.
async fn share_tasks(http: &reqwest::Client, share: &Share, auth: Option<&str>) -> Option<Result<Vec<Task>, String>> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Status {
        installed: bool,
    }
    let status = get_json::<Status>(http.get(format!("{}/status.php", share.base)), "the share's server").await;
    if !status.is_ok_and(|(code, status)| code.is_success() && status.installed) {
        return None;
    }
    let mut locked = false;
    for (root, legacy) in [(format!("{}/public.php/dav/files/{}/", share.base, share.token), false), (format!("{}/public.php/webdav/", share.base), true)] {
        match share_walk(http, share, &root, legacy, auth).await {
            Ok(Walk::Listed(tasks)) if tasks.is_empty() => return Some(Err("the share holds no files the app can download".to_string())),
            Ok(Walk::Listed(tasks)) => return Some(Ok(tasks)),
            Ok(Walk::Locked) => locked = true,
            Ok(Walk::Absent) => {}
            Err(e) => return Some(Err(e)),
        }
    }
    Some(Err(match (locked, auth) {
        (false, _) => SHARE_GONE,
        (true, None) => SHARE_LOCKED,
        (true, Some(_)) => SHARE_REFUSED,
    }
    .to_string()))
}

/// Every file under the share's folder in the WebDAV at `root` (`legacy`: the one whose user name
/// is the token), subfolders kept, folder after folder. A file downloads from the share's own
/// download link, or with `auth` from its WebDAV link, which takes the user's sign-in.
async fn share_walk(http: &reqwest::Client, share: &Share, root: &str, legacy: bool, auth: Option<&str>) -> Result<Walk, String> {
    let cannot_reach = |e: reqwest::Error| format!("Cannot reach the share's server: {}", e.without_url());
    let method = reqwest::Method::from_bytes(b"PROPFIND").map_err(|e| e.to_string())?;
    let root = Url::parse(root).map_err(|e| e.to_string())?;
    let start: Vec<String> = share.path.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
    let (mut folders, mut tasks, mut top) = (VecDeque::from([start.clone()]), Vec::new(), None::<PathBuf>);
    let mut listed = 0;
    while let Some(dir) = folders.pop_front() {
        listed += 1;
        if listed > MAX_FOLDERS {
            return Err(format!("the share holds more than {} folders: add a subfolder's link instead", MAX_FOLDERS));
        }
        let mut url = root.clone();
        url.path_segments_mut().map_err(|_| "unusable share link".to_string())?.pop_if_empty().extend(&dir).push("");
        let request = http.request(method.clone(), url).header("Depth", "1").header("X-Requested-With", "XMLHttpRequest");
        let request = match auth.and_then(|auth| reqwest::header::HeaderValue::from_str(auth).ok()) {
            Some(mut auth) => {
                auth.set_sensitive(true);
                request.header(AUTHORIZATION, auth)
            }
            None if legacy => request.basic_auth(&share.token, None::<&str>),
            None => request,
        };
        let answer = request.header(CONTENT_TYPE, "application/xml").body(PROPFIND).send().await.map_err(cannot_reach)?;
        match answer.status().as_u16() {
            207 => {}
            401 => return Ok(Walk::Locked),
            _ if top.is_none() => return Ok(Walk::Absent),
            status => return Err(format!("the share's server answered HTTP {} while listing it", status)),
        }
        let xml = answer.text().await.map_err(cannot_reach)?;
        for entry in dav_entries(&xml) {
            let Some(path) = dav_path(&root, &entry.href) else { continue };
            if path == dir {
                if top.is_none() && !entry.folder {
                    // A shared file: the share is that file, named as the server names it.
                    let url = match auth {
                        Some(_) => root.join(&entry.href).map_err(|e| e.to_string())?,
                        None => Url::parse(&format!("{}/download", share.link)).map_err(|e| e.to_string())?,
                    };
                    let name = clean_path([entry.name.as_str()]).ok();
                    return Ok(Walk::Listed(vec![Task { urls: vec![url], name, size: entry.size, checksum: dav_checksum(&entry.checksums), ..Task::default() }]));
                }
                if top.is_none() {
                    top = Some(component(&entry.name, &share.token)?);
                }
                continue;
            }
            // Only what is in the folder asked for: a server's answer is not trusted to stay in it.
            if path.len() != dir.len() + 1 || !path.starts_with(&dir) {
                continue;
            }
            if entry.folder {
                folders.push_back(path);
                continue;
            }
            // Untrusted names: an unusable one is left out.
            let Ok(name) = clean_path(path[start.len()..].iter().map(String::as_str)) else { continue };
            let url = match auth {
                Some(_) => root.join(&entry.href).map_err(|e| e.to_string())?,
                None => {
                    let (file, parent) = path.split_last().ok_or("unusable share entry")?;
                    let query = [("path", format!("/{}", parent.join("/"))), ("files", file.clone())];
                    Url::parse_with_params(&format!("{}/download", share.link), query).map_err(|e| e.to_string())?
                }
            };
            tasks.push(Task {
                urls: vec![url],
                name: Some(name),
                folder: top.clone(),
                size: entry.size,
                checksum: dav_checksum(&entry.checksums),
                ..Task::default()
            });
        }
    }
    Ok(Walk::Listed(tasks))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SwissAnswer {
    data: SwissTransfer,
}

/// A transfer, or `kind` and `message` when it cannot be read (expired, need_password, ...).
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SwissTransfer {
    #[serde(rename = "type")]
    kind: String,
    message: String,
    #[serde(rename = "linkUUID")]
    link_uuid: String,
    download_host: String,
    container: SwissContainer,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SwissContainer {
    #[serde(rename = "UUID")]
    uuid: String,
    files: Vec<SwissFile>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SwissFile {
    #[serde(rename = "UUID")]
    uuid: String,
    file_name: String,
    file_size_in_bytes: Option<u64>,
    /// The folder it was uploaded in, if any.
    path: Option<String>,
    deleted_date: Option<String>,
}

/// One task per file of SwissTransfer's transfer `link` (read at `base`), subfolders kept, each
/// from the download host the transfer names; a transfer with a password opens with `password`,
/// and each of its files then downloads with a token made for it.
async fn swisstransfer_list(http: &reqwest::Client, base: &str, link: &str, password: Option<&str>) -> Result<Vec<Task>, String> {
    use base64::Engine as _;
    let mut request = http.get(format!("{base}/api/links/{link}"));
    if let Some(password) = password {
        // As SwissTransfer's own apps send it: the password in base64, no scheme.
        request = request.header(AUTHORIZATION, base64::engine::general_purpose::STANDARD.encode(password));
    }
    // An unknown link is sent on to a "not found" page, which is no JSON.
    let (_, answer) = get_json::<SwissAnswer>(request, "SwissTransfer").await.map_err(|_| SWISSTRANSFER_GONE.to_string())?;
    let transfer = answer.data;
    match transfer.kind.as_str() {
        "" => {}
        "expired" => return Err(SWISSTRANSFER_GONE.to_string()),
        kind if kind.contains("password") => return Err(if password.is_some() { SWISSTRANSFER_REFUSED } else { SWISSTRANSFER_LOCKED }.to_string()),
        _ => return Err(format!("SwissTransfer: {}", transfer.message)),
    }
    let mut tasks = Vec::new();
    for file in transfer.container.files.into_iter().filter(|f| f.deleted_date.is_none()) {
        let mut url = Url::parse(&format!("https://{}/api/download/{}/{}", transfer.download_host, transfer.link_uuid, file.uuid))
            .map_err(|_| "SwissTransfer answered with a download host the app cannot read".to_string())?;
        if let Some(password) = password {
            let body = serde_json::json!({ "containerUUID": transfer.container.uuid, "fileUUID": file.uuid, "password": password });
            let token = http.post(format!("{base}/api/generateDownloadToken")).header(CONTENT_TYPE, "application/json").body(body.to_string());
            let token = token.send().await.and_then(|a| a.error_for_status()).map_err(|e| format!("Cannot reach SwissTransfer: {}", e.without_url()))?;
            let token = token.text().await.map_err(|e| format!("Cannot reach SwissTransfer: {}", e.without_url()))?;
            url.query_pairs_mut().append_pair("token", token.trim().trim_matches('"'));
        }
        // shortcut: `path` is taken as the folder, or the folder and name, not seen live; check it on a transfer with folders.
        let path = file.path.as_deref().unwrap_or_default();
        let folder = path.strip_suffix(file.file_name.as_str()).unwrap_or(path);
        let Ok(name) = clean_path(folder.split('/').filter(|s| !s.is_empty()).chain([file.file_name.as_str()])) else {
            tracing::warn!("left out {:?}: not a usable file name", file.file_name);
            continue;
        };
        tasks.push(Task { urls: vec![url], name: Some(name), size: file.file_size_in_bytes, from_document: true, ..Task::default() });
    }
    if tasks.is_empty() {
        return Err("the transfer holds no files the app can download".to_string());
    }
    Ok(tasks)
}

/// The `record` elements of an XML listing, each its child elements' text by name, and the text of
/// the elements outside them (the next page's token, an error's code).
pub(crate) fn xml_records(xml: &str, record: &[u8]) -> (Vec<HashMap<String, String>>, HashMap<String, String>) {
    let mut reader = Reader::from_str(xml.trim_start_matches('\u{feff}'));
    reader.config_mut().trim_text(true);
    let (mut records, mut outside, mut current, mut tag) = (Vec::new(), HashMap::new(), None::<HashMap<String, String>>, String::new());
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == record {
                    current = Some(HashMap::new());
                }
                tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
            }
            Ok(Event::Text(t)) => {
                let text = t.unescape().map_or_else(|_| String::from_utf8_lossy(&t).into_owned(), |t| t.into_owned());
                current.as_mut().unwrap_or(&mut outside).insert(tag.clone(), text);
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == record {
                    records.extend(current.take());
                }
                tag.clear();
            }
            Ok(Event::Eof) | Err(_) => return (records, outside),
            _ => {}
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GcsPage {
    items: Vec<GcsObject>,
    next_page_token: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GcsObject {
    name: String,
    /// A number in a string.
    size: Option<String>,
    /// Base64; a composite object has none.
    md5_hash: Option<String>,
}

/// `md5:<hex>` of a base64 MD5.
fn md5_base64(b64: &str) -> Option<String> {
    use base64::Engine as _;
    let digest = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok().filter(|d| d.len() == 16)?;
    Some(crate::digest::checksum("md5", &digest))
}

/// What a bucket's host answered with an error `status` (and the error `code` its XML gives).
fn bucket_refused(status: StatusCode, code: Option<&String>) -> String {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => BUCKET_PRIVATE.to_string(),
        StatusCode::NOT_FOUND => BUCKET_GONE.to_string(),
        _ => format!("the bucket's host answered HTTP {}{}", status.as_u16(), code.map(|c| format!(" ({c})")).unwrap_or_default()),
    }
}

/// One page of `bucket`'s objects under `prefix`, from `token` on: each object's key, size and MD5
/// (where the service gives a real one), and the next page's token. An S3 bucket is asked with
/// `s3`, the user's key, when it is for its host.
async fn bucket_page(
    http: &reqwest::Client,
    bucket: &Bucket,
    prefix: &str,
    token: Option<&str>,
    gcs_api: &str,
    s3: Option<&crate::sigv4::S3Key>,
) -> Result<(Vec<(String, Option<u64>, Option<String>)>, Option<String>), String> {
    let cannot_reach = |e: reqwest::Error| format!("Cannot reach the bucket's host: {}", e.without_url());
    let (request, record) = match bucket {
        Bucket::Gcs { bucket } => {
            let mut url = Url::parse(gcs_api).map_err(|e| e.to_string())?;
            url.path_segments_mut().map_err(|_| "bad Cloud Storage address".to_string())?.extend(["storage", "v1", "b", bucket, "o"]);
            url.query_pairs_mut().append_pair("prefix", prefix).append_pair("fields", "items(name,size,md5Hash),nextPageToken");
            if let Some(token) = token {
                url.query_pairs_mut().append_pair("pageToken", token);
            }
            let (status, page) = get_json::<GcsPage>(http.get(url), "Cloud Storage").await?;
            if !status.is_success() {
                return Err(bucket_refused(status, None));
            }
            let objects = page.items.into_iter().map(|o| (o.name, o.size.and_then(|s| s.parse().ok()), o.md5_hash.as_deref().and_then(md5_base64)));
            return Ok((objects.collect(), page.next_page_token.filter(|t| !t.is_empty())));
        }
        Bucket::S3 { base, .. } => {
            let mut url = base.clone();
            url.query_pairs_mut().append_pair("list-type", "2").append_pair("prefix", prefix);
            if let Some(token) = token {
                url.query_pairs_mut().append_pair("continuation-token", token);
            }
            let key = s3.filter(|key| key.applies(&url));
            let request = |url: &Url| match key {
                Some(key) => crate::sigv4::signed(http.get(url.clone()), key),
                None => http.get(url.clone()),
            };
            // A signed request turned away for a clock minutes off goes again, as the engine's do.
            let answer = crate::worker::send(request, &url).await.map_err(cannot_reach)?;
            let status = answer.status();
            let xml = answer.text().await.map_err(cannot_reach)?;
            let (records, outside) = xml_records(&xml, b"Contents");
            return match status {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err((if key.is_some() { S3_REFUSED } else { S3_PRIVATE }).to_string()),
                _ if !status.is_success() => Err(bucket_refused(status, outside.get("Code"))),
                _ => Ok(s3_objects(bucket, &records, &outside)),
            };
        }
        Bucket::Azure { base, .. } => {
            let mut url = base.clone();
            url.set_path(base.path().trim_end_matches('/'));
            url.query_pairs_mut().append_pair("restype", "container").append_pair("comp", "list").append_pair("prefix", prefix);
            if let Some(token) = token {
                url.query_pairs_mut().append_pair("marker", token);
            }
            (http.get(url).header("x-ms-version", AZURE_VERSION), b"Blob".as_slice())
        }
    };
    let answer = request.send().await.map_err(cannot_reach)?;
    let status = answer.status();
    let xml = answer.text().await.map_err(cannot_reach)?;
    let (records, outside) = xml_records(&xml, record);
    if !status.is_success() {
        return Err(bucket_refused(status, outside.get("Code")));
    }
    let field = |r: &HashMap<String, String>, name: &str| r.get(name).cloned().unwrap_or_default();
    let objects = records.iter().map(|r| (field(r, "Name"), field(r, "Content-Length").parse().ok(), md5_base64(&field(r, "Content-MD5"))));
    Ok((objects.collect(), outside.get("NextMarker").filter(|m| !m.is_empty()).cloned()))
}

/// The objects of an S3 listing's `records` and the next page's token.
#[allow(clippy::type_complexity)]
fn s3_objects(bucket: &Bucket, records: &[HashMap<String, String>], outside: &HashMap<String, String>) -> (Vec<(String, Option<u64>, Option<String>)>, Option<String>) {
    let field = |r: &HashMap<String, String>, name: &str| r.get(name).cloned().unwrap_or_default();
    let md5_etags = matches!(bucket, Bucket::S3 { md5_etags: true, .. });
    // A multipart upload's ETag ("...-3") is no MD5 of the object.
    let md5 = |etag: String| {
        let hex = etag.trim_matches('"').to_ascii_lowercase();
        (md5_etags && hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| format!("md5:{hex}"))
    };
    let objects = records.iter().map(|r| (field(r, "Key"), field(r, "Size").parse().ok(), md5(field(r, "ETag"))));
    let truncated = outside.get("IsTruncated").is_some_and(|t| t == "true");
    (objects.collect(), outside.get("NextContinuationToken").filter(|_| truncated).cloned())
}

/// Every object of `bucket` whose key starts with `prefix`, page by page, each named by its key
/// after the prefix's folder (folders kept), in a folder named after that folder or the bucket.
/// An S3 bucket is listed with `s3`, the user's key, when it is for its host.
async fn bucket_list(http: &reqwest::Client, bucket: &Bucket, prefix: &str, gcs_api: &str, s3: Option<&crate::sigv4::S3Key>) -> Result<Vec<Task>, String> {
    // "a/b" lists "a/b1.txt" as "b1.txt", in a folder "a".
    let parent = prefix.rfind('/').map_or("", |at| &prefix[..=at]);
    let (base, name) = match bucket {
        Bucket::S3 { base, name, .. } | Bucket::Azure { base, name } => (base.clone(), name.as_str()),
        Bucket::Gcs { bucket } => (Url::parse(&format!("{gcs_api}/{bucket}/")).map_err(|e| e.to_string())?, bucket.as_str()),
    };
    let folder = component(parent.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(name), "bucket")?;
    let (mut tasks, mut token) = (Vec::new(), None);
    for _ in 0..MAX_PAGES {
        let (objects, next) = bucket_page(http, bucket, prefix, token.as_deref(), gcs_api, s3).await?;
        for (key, size, checksum) in objects {
            // A key ending in `/` is a folder's marker, no file.
            let Some(rest) = key.strip_prefix(parent).filter(|r| !r.is_empty() && !r.ends_with('/')) else { continue };
            let Ok(file) = clean_path(rest.split('/')) else {
                tracing::warn!("left out {:?}: not a usable file name", key);
                continue;
            };
            let mut url = base.clone();
            url.path_segments_mut().map_err(|_| "bad bucket address".to_string())?.pop_if_empty().extend(key.split('/'));
            let checksum = checksum.filter(|c| crate::storage::validate_checksum(c).is_ok());
            tasks.push(Task { urls: vec![url], name: Some(file), folder: Some(folder.clone()), size, checksum, from_document: true, ..Task::default() });
        }
        if tasks.len() > MAX_OBJECTS {
            return Err(format!("the bucket holds more than {MAX_OBJECTS} objects there: add the link of one of its folders instead"));
        }
        match next {
            Some(next) => token = Some(next),
            None => return Ok(tasks),
        }
    }
    Err(format!("the bucket's listing goes on past {MAX_PAGES} pages: add the link of one of its folders instead"))
}

const WEBDAV_LOCKED: &str = "the WebDAV server wants a sign-in: add the link with it, webdavs://USER:PASSWORD@host/path";
const WEBDAV_REFUSED: &str = "the WebDAV server refused the sign-in given with the link";

/// The file at the WebDAV address `link`, or one task per file under the folder there (subfolders
/// kept, in a folder named after it), each with its size and the checksum the server keeps, read
/// with PROPFIND and `auth` (the link's sign-in), which goes with the downloads too.
async fn webdav_list(http: &reqwest::Client, link: &Url, auth: Option<&str>) -> Result<Vec<Task>, String> {
    let cannot_reach = |e: reqwest::Error| format!("Cannot reach the WebDAV server: {}", e.without_url());
    let method = reqwest::Method::from_bytes(b"PROPFIND").map_err(|e| e.to_string())?;
    // Paths are read below `root`, the folder's address with its last slash.
    let mut root = link.clone();
    root.path_segments_mut().map_err(|_| "unusable WebDAV link".to_string())?.pop_if_empty().push("");
    let top = link.path_segments().and_then(|s| s.filter(|s| !s.is_empty()).last()).map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned());
    let folder = component(top.as_deref().unwrap_or_default(), link.host_str().unwrap_or("webdav"))?;
    let (mut folders, mut tasks, mut listed) = (VecDeque::from([Vec::<String>::new()]), Vec::new(), 0);
    while let Some(dir) = folders.pop_front() {
        listed += 1;
        if listed > MAX_FOLDERS {
            return Err(format!("the WebDAV folder holds more than {} folders: add a subfolder's link instead", MAX_FOLDERS));
        }
        let url = if dir.is_empty() {
            link.clone()
        } else {
            let mut url = root.clone();
            url.path_segments_mut().map_err(|_| "unusable WebDAV link".to_string())?.pop_if_empty().extend(&dir).push("");
            url
        };
        let mut request = http.request(method.clone(), url).header("Depth", "1").header(CONTENT_TYPE, "application/xml").body(PROPFIND);
        if let Some(mut auth) = auth.and_then(|a| reqwest::header::HeaderValue::from_str(a).ok()) {
            auth.set_sensitive(true);
            request = request.header(AUTHORIZATION, auth);
        }
        let answer = request.send().await.map_err(cannot_reach)?;
        match answer.status().as_u16() {
            207 => {}
            401 | 403 => return Err(if auth.is_some() { WEBDAV_REFUSED } else { WEBDAV_LOCKED }.to_string()),
            404 => return Err("the WebDAV server has nothing at that link".to_string()),
            405 | 501 if dir.is_empty() => return Err("the server does not answer as WebDAV (PROPFIND) at that link".to_string()),
            status => return Err(format!("the WebDAV server answered HTTP {} while listing it", status)),
        }
        let xml = answer.text().await.map_err(cannot_reach)?;
        for entry in dav_entries(&xml) {
            let Some(path) = dav_path(&root, &entry.href) else { continue };
            if path == dir {
                if dir.is_empty() && !entry.folder {
                    // The link is a file's.
                    let name = clean_path([top.as_deref().unwrap_or(entry.name.as_str())]).ok();
                    return Ok(vec![Task { urls: vec![link.clone()], name, size: entry.size, checksum: dav_checksum(&entry.checksums), ..Task::default() }]);
                }
                continue;
            }
            // Only what is in the folder asked for: a server's answer is not trusted to stay in it.
            if path.len() != dir.len() + 1 || !path.starts_with(&dir) {
                continue;
            }
            if entry.folder {
                folders.push_back(path);
                continue;
            }
            let Ok(name) = clean_path(path.iter().map(String::as_str)) else { continue };
            let mut url = root.clone();
            url.path_segments_mut().map_err(|_| "unusable WebDAV link".to_string())?.pop_if_empty().extend(&path);
            // Not from a document: the link's sign-in goes with it, to the link's own host.
            tasks.push(Task { urls: vec![url], name: Some(name), folder: Some(folder.clone()), size: entry.size, checksum: dav_checksum(&entry.checksums), ..Task::default() });
        }
    }
    Ok(tasks)
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

    #[test]
    fn nextcloud_shares_are_told_by_their_shape() {
        let share = |link: &str| share_of(&Url::parse(link).unwrap());
        assert_eq!(
            share("https://cloud.example.org/index.php/s/kFy9Lek5sm928xP"),
            Some(Share {
                base: "https://cloud.example.org".into(),
                link: "https://cloud.example.org/index.php/s/kFy9Lek5sm928xP".into(),
                token: "kFy9Lek5sm928xP".into(),
                path: "/".into(),
            })
        );
        assert_eq!(
            share("https://h.example/owncloud/s/Tok3n/?path=%2FA%20Folder"),
            Some(Share { base: "https://h.example/owncloud".into(), link: "https://h.example/owncloud/s/Tok3n".into(), token: "Tok3n".into(), path: "/A Folder".into() })
        );
        for other in [
            // A file's own download link, the share's whole download, Box's and Terabox's shares.
            "https://cloud.example.org/s/kFy9Lek5sm928xP/download?path=%2F&files=a.txt",
            "https://cloud.example.org/s/kFy9Lek5sm928xP/download",
            "https://app.box.com/s/17vthb1z",
            "https://cloud.example.org/s/",
            "https://cloud.example.org/s/a.b",
            "ftp://cloud.example.org/s/kFy9Lek5sm928xP",
        ] {
            assert_eq!(share(other), None, "{other}");
        }
        assert!(lists(&Url::parse("https://cloud.example.org/s/kFy9Lek5sm928xP").unwrap()));
        assert!(!handles(&Url::parse("https://cloud.example.org/s/kFy9Lek5sm928xP").unwrap()), "as one download it is any link");
    }

    /// A WebDAV answer as Nextcloud writes it: one `<d:response>` per (href, displayname,
    /// size or folder, checksums).
    fn multistatus(entries: &[(&str, &str, Option<u64>, &str)]) -> Reply {
        let response = |(href, name, size, sums): &(&str, &str, Option<u64>, &str)| match size {
            Some(size) => format!(
                "<d:response><d:href>{href}</d:href><d:propstat><d:prop><d:displayname>{name}</d:displayname><d:getcontentlength>{size}</d:getcontentlength><d:resourcetype/><oc:checksums><oc:checksum>{sums}</oc:checksum></oc:checksums></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
            ),
            None => format!(
                "<d:response><d:href>{href}</d:href><d:propstat><d:prop><d:displayname>{name}</d:displayname><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat><d:propstat><d:prop><d:getcontentlength/><oc:checksums/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response>"
            ),
        };
        let body = format!(
            r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">{}</d:multistatus>"#,
            entries.iter().map(response).collect::<String>()
        );
        (207, "Content-Type: application/xml; charset=utf-8\r\n".into(), body.into_bytes())
    }

    fn nextcloud(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        let sha1 = "SHA1:3E20A009994405F535CDF07CDC2974CEF2FCE8F2 MD5:9366a4b09386bf673c447e33d806d904 ADLER32:2283b5fd";
        match path {
            "/nc/status.php" | "/old/status.php" | "/gone/status.php" => json(200, r#"{"installed":true,"maintenance":false,"productname":"Nextcloud"}"#),
            "/nc/public.php/dav/files/FOLDER/" => multistatus(&[
                ("/nc/public.php/dav/files/FOLDER/", "My/Pack", None, ""),
                ("/nc/public.php/dav/files/FOLDER/a%20b.txt", "a b.txt", Some(3), sha1),
                ("/nc/public.php/dav/files/FOLDER/Sub/", "Sub", None, ""),
                // Outside the share, and deeper than asked.
                ("/nc/public.php/dav/files/OTHER/x.bin", "x.bin", Some(1), ""),
                ("/nc/public.php/dav/files/FOLDER/Sub/deep/y.bin", "y.bin", Some(1), ""),
            ]),
            "/nc/public.php/dav/files/FOLDER/Sub/" => multistatus(&[
                ("/nc/public.php/dav/files/FOLDER/Sub/", "Sub", None, ""),
                ("/nc/public.php/dav/files/FOLDER/Sub/c.bin", "c.bin", Some(5), "ADLER32:1"),
                ("/nc/public.php/dav/files/FOLDER/Sub/..", "..", Some(1), ""),
            ]),
            // An older server, or ownCloud: no public.php/dav, a shared file at its WebDAV's top.
            "/old/public.php/webdav/" => multistatus(&[("/old/public.php/webdav/", "report.pdf", Some(9), "")]),
            "/nc/public.php/dav/files/LOCKED/" | "/nc/public.php/webdav/" => (401, String::new(), Vec::new()),
            _ => (404, String::new(), Vec::new()),
        }
    }

    /// A folder share is one task per file, folder after folder, subfolders kept, each the share's
    /// own download link with its size and strongest checksum; nothing outside the folder asked
    /// for is listed. An older server's shared file is its one task; a share with a password says
    /// how to give it; a host that runs neither Nextcloud nor ownCloud is left to the other listers.
    #[tokio::test]
    async fn nextcloud_shares_list_their_files() {
        let (addr, _) = serve(nextcloud).await;
        let at = |path: &str| share_of(&Url::parse(&format!("http://{addr}{path}")).unwrap()).unwrap();
        let tasks = share_tasks(&http(), &at("/nc/index.php/s/FOLDER"), None).await.unwrap().unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap(), t.size, t.checksum.clone())).collect();
        let download = format!("http://{addr}/nc/index.php/s/FOLDER/download");
        assert_eq!(
            listed,
            [
                (format!("{download}?path=%2F&files=a+b.txt"), PathBuf::from("a b.txt"), Some(3), Some("sha1:3e20a009994405f535cdf07cdc2974cef2fce8f2".into())),
                (format!("{download}?path=%2FSub&files=c.bin"), PathBuf::from("Sub").join("c.bin"), Some(5), None),
            ]
        );
        assert!(tasks.iter().all(|t| t.folder == Some("My_Pack".into()) && !t.from_document));
        let sub = share_tasks(&http(), &at("/nc/s/FOLDER?path=/Sub"), None).await.unwrap().unwrap();
        assert_eq!((sub[0].name.clone(), sub[0].folder.clone()), (Some("c.bin".into()), Some("Sub".into())));
        let file = share_tasks(&http(), &at("/old/s/FILE"), None).await.unwrap().unwrap();
        assert_eq!(file, [Task { urls: vec![Url::parse(&format!("http://{addr}/old/s/FILE/download")).unwrap()], name: Some("report.pdf".into()), size: Some(9), ..Task::default() }]);
        assert_eq!(share_tasks(&http(), &at("/nc/s/LOCKED"), None).await.unwrap().unwrap_err(), SHARE_LOCKED);
        assert_eq!(share_tasks(&http(), &at("/nc/s/LOCKED"), Some("Basic eDp5")).await.unwrap().unwrap_err(), SHARE_REFUSED);
        assert_eq!(share_tasks(&http(), &at("/gone/s/GONE"), None).await.unwrap().unwrap_err(), SHARE_GONE);
        assert!(share_tasks(&http(), &at("/other/s/FOLDER"), None).await.is_none());
    }

    /// A share with a password is listed with the user's sign-in, and its files download from
    /// their WebDAV links, which take it.
    #[tokio::test]
    async fn a_share_with_a_password_lists_and_downloads_with_the_sign_in() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0; 8192];
                let n = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..n]).to_ascii_lowercase();
                let (status, body) = if request.starts_with("get /status.php") {
                    (200, r#"{"installed":true}"#.to_string())
                } else if request.contains("authorization: basic dg9rzw46chc=") {
                    let (_, _, body) = multistatus(&[("/public.php/dav/files/TOKEN/", "Pack", None, ""), ("/public.php/dav/files/TOKEN/a.txt", "a.txt", Some(3), "")]);
                    (207, String::from_utf8(body).unwrap())
                } else {
                    (401, String::new())
                };
                let head = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(format!("{head}{body}").as_bytes()).await;
            }
        });
        let share = share_of(&Url::parse(&format!("http://{addr}/s/TOKEN")).unwrap()).unwrap();
        // "token:pw"
        let tasks = share_tasks(&http(), &share, Some("Basic dG9rZW46cHc=")).await.unwrap().unwrap();
        assert_eq!(tasks[0].urls[0].as_str(), format!("http://{addr}/public.php/dav/files/TOKEN/a.txt"));
        assert!(!tasks[0].from_document, "the user's sign-in goes with it");
        assert_eq!(share_tasks(&http(), &share, None).await.unwrap().unwrap_err(), SHARE_LOCKED);
    }

    /// A SwissTransfer transfer is one task per file from its download host, subfolders kept; one
    /// with a password opens with it (sent as SwissTransfer's apps send it) and its files download
    /// with a token; an expired or unknown one, and a missing or wrong password, say so.
    #[tokio::test]
    async fn swisstransfer_transfers_list_their_files() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // As SwissTransfer answers, cut down (`needPassword` is a number).
        const TRANSFER: &str = r#"{"result":"success","data":{"linkUUID":"c96350da-4d8c-4c43-aa6a-40f2d2b31eac","containerUUID":"77f4f5c8","downloadHost":"dl-x.swisstransfer.com",
            "container":{"UUID":"77f4f5c8","needPassword":0,"numberOfFile":3,"files":[
                {"UUID":"2e085789","fileName":"app (1).aab","fileSizeInBytes":26070874,"deletedDate":null,"path":null},
                {"UUID":"3f196890","fileName":"b.txt","fileSizeInBytes":3,"deletedDate":null,"path":"Docs/Old"},
                {"UUID":"4a2b7c01","fileName":"gone.txt","fileSizeInBytes":3,"deletedDate":"2024-12-01T00:00:00+01:00","path":null}]}}}"#;
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0; 8192];
                let n = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..n]).to_ascii_lowercase();
                let body = match request.split(' ').nth(1).unwrap_or_default() {
                    "/api/links/open" => TRANSFER.to_string(),
                    // "pw"
                    "/api/links/locked" if request.contains("authorization: chc=\r\n") => TRANSFER.to_string(),
                    "/api/links/locked" if request.contains("authorization:") => r#"{"result":"success","data":{"type":"wrong_password","message":"Wrong password"}}"#.into(),
                    "/api/links/locked" => r#"{"result":"success","data":{"type":"need_password","message":"Transfer need a password"}}"#.into(),
                    "/api/links/old" => r#"{"result":"success","data":{"type":"expired","message":"Transfer is expired"}}"#.into(),
                    "/api/generatedownloadtoken" if request.starts_with("post ") => "\"tok\"".into(),
                    _ => "<!DOCTYPE html><html>not found</html>".into(),
                };
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(format!("{head}{body}").as_bytes()).await;
            }
        });
        let files = |tasks: &[Task]| tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap(), t.size)).collect::<Vec<_>>();
        let download = "https://dl-x.swisstransfer.com/api/download/c96350da-4d8c-4c43-aa6a-40f2d2b31eac";
        let open = swisstransfer_list(&http(), &base, "open", None).await.unwrap();
        assert_eq!(
            files(&open),
            [
                (format!("{download}/2e085789"), PathBuf::from("app (1).aab"), Some(26070874)),
                (format!("{download}/3f196890"), PathBuf::from("Docs").join("Old").join("b.txt"), Some(3)),
            ]
        );
        assert!(open.iter().all(|t| t.from_document));
        let locked = swisstransfer_list(&http(), &base, "locked", Some("pw")).await.unwrap();
        assert_eq!(locked[0].urls[0].as_str(), format!("{download}/2e085789?token=tok"));
        assert_eq!(swisstransfer_list(&http(), &base, "locked", None).await.unwrap_err(), SWISSTRANSFER_LOCKED);
        assert_eq!(swisstransfer_list(&http(), &base, "locked", Some("no")).await.unwrap_err(), SWISSTRANSFER_REFUSED);
        assert_eq!(swisstransfer_list(&http(), &base, "old", None).await.unwrap_err(), SWISSTRANSFER_GONE);
        assert_eq!(swisstransfer_list(&http(), &base, "unknown", None).await.unwrap_err(), SWISSTRANSFER_GONE);
        let link = Url::parse("https://www.swisstransfer.com/d/c96350da-4d8c-4c43-aa6a-40f2d2b31eac").unwrap();
        assert_eq!(link_of(&link), Some(Link::SwissTransfer { link: "c96350da-4d8c-4c43-aa6a-40f2d2b31eac".into() }));
        assert!(lists(&link) && handles(&link));
        assert_eq!(link_of(&Url::parse("https://www.swisstransfer.com/en-us").unwrap()), None);
    }

    fn buckets(path: &str, _: Option<crate::range::ByteRange>) -> Reply {
        let xml = |status: u16, body: &str| (status, "Content-Type: application/xml\r\n".to_string(), body.as_bytes().to_vec());
        match path {
            // As S3 answers, cut down: a folder's marker, a one-part and a multipart upload, a key that climbs out.
            "/bk/?list-type=2&prefix=data%2F" => xml(200, r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>bk</Name><Prefix>data/</Prefix>
                <NextContinuationToken>t+1/=</NextContinuationToken><IsTruncated>true</IsTruncated>
                <Contents><Key>data/</Key><Size>0</Size><ETag>&quot;d41d8cd98f00b204e9800998ecf8427e&quot;</ETag></Contents>
                <Contents><Key>data/a b.txt</Key><Size>25333</Size><ETag>&quot;A82389915A3D59147C906FE72463312A&quot;</ETag></Contents>
                <Contents><Key>data/../evil.exe</Key><Size>1</Size></Contents></ListBucketResult>"#),
            "/bk/?list-type=2&prefix=data%2F&continuation-token=t%2B1%2F%3D" => xml(200, r#"<ListBucketResult><IsTruncated>false</IsTruncated>
                <Contents><Key>data/sub/c.bin</Key><Size>9</Size><ETag>"0a3d8b5b0fb4a2a3b3c3d3e3f3a3b3c3-2"</ETag></Contents></ListBucketResult>"#),
            "/private/?list-type=2&prefix=" => xml(403, "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>"),
            // As Azure answers (a BOM first), two pages.
            "/cont?restype=container&comp=list&prefix=" => xml(200, "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults ContainerName=\"cont\"><Blobs><Blob><Name>x/y.parquet</Name><Properties><Content-Length>51431175</Content-Length><Content-MD5 /></Properties></Blob></Blobs><NextMarker>m2</NextMarker></EnumerationResults>"),
            "/cont?restype=container&comp=list&prefix=&marker=m2" => xml(200, "<EnumerationResults><Blobs><Blob><Name>z.txt</Name><Properties><Content-Length>3</Content-Length><Content-MD5>ziZNd8uikTtDCkBTgS5Riw==</Content-MD5></Properties></Blob></Blobs><NextMarker /></EnumerationResults>"),
            "/gone?restype=container&comp=list&prefix=" => xml(404, "<Error><Code>ResourceNotFound</Code></Error>"),
            _ if path.starts_with("/storage/v1/b/bk/o?prefix=LC08%2Fa&fields=") => {
                let body = if path.ends_with("pageToken=p2") {
                    r#"{"items":[{"name":"LC08/b.txt","size":"3"}]}"#
                } else {
                    r#"{"items":[{"name":"LC08/a_MTL.txt","size":"8454","md5Hash":"ziZNd8uikTtDCkBTgS5Riw=="}],"nextPageToken":"p2"}"#
                };
                (200, "Content-Type: application/json\r\n".into(), body.as_bytes().to_vec())
            }
            _ => xml(404, "<Error><Code>NoSuchBucket</Code></Error>"),
        }
    }

    /// A public bucket is one task per object under the prefix (S3 and Azure page by page, Cloud
    /// Storage too), named from the prefix's folder, subfolders kept, with its size and its MD5
    /// where it is one (not a multipart ETag); a bucket that is private or missing says so.
    #[tokio::test]
    async fn public_buckets_list_their_objects() {
        let (addr, _) = serve(buckets).await;
        let base = format!("http://{addr}");
        let at = |path: &str| Url::parse(&format!("{base}{path}")).unwrap();
        let listed = |tasks: Vec<Task>| tasks.into_iter().map(|t| (t.urls[0].to_string(), t.folder.unwrap(), t.name.unwrap(), t.size, t.checksum)).collect::<Vec<_>>();
        let s3 = Bucket::S3 { base: at("/bk/"), name: "bk".into(), md5_etags: true };
        assert_eq!(
            listed(bucket_list(&http(), &s3, "data/", &base, None).await.unwrap()),
            [
                (format!("{base}/bk/data/a%20b.txt"), PathBuf::from("data"), PathBuf::from("a b.txt"), Some(25333), Some("md5:a82389915a3d59147c906fe72463312a".to_string())),
                (format!("{base}/bk/data/sub/c.bin"), PathBuf::from("data"), PathBuf::from("sub").join("c.bin"), Some(9), None),
            ]
        );
        let private = Bucket::S3 { base: at("/private/"), name: "private".into(), md5_etags: true };
        assert_eq!(bucket_list(&http(), &private, "", &base, None).await.unwrap_err(), S3_PRIVATE);
        let azure = Bucket::Azure { base: at("/cont/"), name: "cont".into() };
        assert_eq!(
            listed(bucket_list(&http(), &azure, "", &base, None).await.unwrap()),
            [
                (format!("{base}/cont/x/y.parquet"), PathBuf::from("cont"), PathBuf::from("x").join("y.parquet"), Some(51431175), None),
                (format!("{base}/cont/z.txt"), PathBuf::from("cont"), PathBuf::from("z.txt"), Some(3), Some("md5:ce264d77cba2913b430a4053812e518b".to_string())),
            ]
        );
        assert_eq!(bucket_list(&http(), &Bucket::Azure { base: at("/gone/"), name: "gone".into() }, "", &base, None).await.unwrap_err(), BUCKET_GONE);
        assert_eq!(
            listed(bucket_list(&http(), &Bucket::Gcs { bucket: "bk".into() }, "LC08/a", &base, None).await.unwrap()),
            [
                (format!("{base}/bk/LC08/a_MTL.txt"), PathBuf::from("LC08"), PathBuf::from("a_MTL.txt"), Some(8454), Some("md5:ce264d77cba2913b430a4053812e518b".to_string())),
                (format!("{base}/bk/LC08/b.txt"), PathBuf::from("LC08"), PathBuf::from("b.txt"), Some(3), None),
            ]
        );
    }

    /// Which links list a bucket, and where: S3's s3://, virtual-hosted and path-style addresses
    /// (and S3-compatible hosts'), Cloud Storage's gs:// and storage.googleapis.com, Azure's
    /// container; an object's link is not one, and an s3:// or gs:// object's is its https address.
    #[test]
    fn bucket_links_are_told_by_their_shape() {
        let s3 = |base: &str, name: &str, md5_etags: bool, prefix: &str| Some((Bucket::S3 { base: Url::parse(base).unwrap(), name: name.into(), md5_etags }, prefix.to_string()));
        let of = |link: &str| bucket_of(&Url::parse(link).unwrap());
        assert_eq!(of("s3://noaa-ghcn-pds/csv/"), s3("https://noaa-ghcn-pds.s3.amazonaws.com/", "noaa-ghcn-pds", true, "csv/"));
        assert_eq!(of("s3://bk"), s3("https://bk.s3.amazonaws.com/", "bk", true, ""));
        assert_eq!(of("https://my.bk.s3.eu-west-1.amazonaws.com/a%20b/"), s3("https://my.bk.s3.eu-west-1.amazonaws.com/", "my.bk", true, "a b/"));
        assert_eq!(of("https://bk.s3.amazonaws.com/?prefix=csv/by_year/176"), s3("https://bk.s3.amazonaws.com/", "bk", true, "csv/by_year/176"));
        assert_eq!(of("https://s3.us-east-2.amazonaws.com/bk"), s3("https://s3.us-east-2.amazonaws.com/bk/", "bk", true, ""));
        assert_eq!(of("https://s3-eu-west-1.amazonaws.com/bk/x/"), s3("https://s3-eu-west-1.amazonaws.com/bk/", "bk", true, "x/"));
        assert_eq!(of("https://s3.us-west-004.backblazeb2.com/bk/"), s3("https://s3.us-west-004.backblazeb2.com/bk/", "bk", false, ""));
        assert_eq!(of("https://bk.s3.eu-central-1.wasabisys.com/"), s3("https://bk.s3.eu-central-1.wasabisys.com/", "bk", false, ""));
        assert_eq!(of("https://bk.nyc3.digitaloceanspaces.com/"), s3("https://bk.nyc3.digitaloceanspaces.com/", "bk", false, ""));
        assert_eq!(of("https://nyc3.digitaloceanspaces.com/bk/d/"), s3("https://nyc3.digitaloceanspaces.com/bk/", "bk", false, "d/"));
        assert_eq!(of("gs://gcp-public-data-landsat/LC08/"), Some((Bucket::Gcs { bucket: "gcp-public-data-landsat".into() }, "LC08/".into())));
        assert_eq!(of("https://storage.googleapis.com/bk/"), Some((Bucket::Gcs { bucket: "bk".into() }, String::new())));
        let azure = |prefix: &str| Some((Bucket::Azure { base: Url::parse("https://acct.blob.core.windows.net/cont/").unwrap(), name: "cont".into() }, prefix.to_string()));
        assert_eq!(of("https://acct.blob.core.windows.net/cont?restype=container&comp=list"), azure(""));
        assert_eq!(of("https://acct.blob.core.windows.net/cont?restype=container&comp=list&prefix=a/b"), azure("a/b"));
        assert_eq!(of("https://acct.blob.core.windows.net/cont"), azure(""));
        for object in [
            "s3://bk/a.zip", "https://bk.s3.amazonaws.com/a.zip", "https://s3.amazonaws.com/bk/a.zip", "https://storage.googleapis.com/bk/a.zip",
            "https://storage.googleapis.com/storage/v1/b/bk/o", "https://acct.blob.core.windows.net/cont/a.zip", "https://bk.s3-website-us-east-1.amazonaws.com/",
            "https://ec2-1-2-3-4.compute-1.amazonaws.com/", "https://example.com/bk/",
        ] {
            assert_eq!(of(object), None, "{object}");
        }
        let object = |link: &str| bucket_object(&Url::parse(link).unwrap()).map(|u| u.to_string());
        assert_eq!(object("s3://bk/a%20b/c.zip").as_deref(), Some("https://bk.s3.amazonaws.com/a%20b/c.zip"));
        assert_eq!(object("gs://bk/c.zip").as_deref(), Some("https://storage.googleapis.com/bk/c.zip"));
        assert_eq!(object("s3://bk/dir/"), None);
        assert!(lists(&Url::parse("s3://bk/").unwrap()) && handles(&Url::parse("gs://bk/").unwrap()));
    }

    /// Live: public buckets on S3, Cloud Storage and Azure list their objects, with the MD5 S3 and
    /// Cloud Storage give.
    #[tokio::test]
    #[ignore]
    async fn live_public_buckets() {
        let client = crate::ingest::descriptor_client(None).unwrap();
        let read = |link: &'static str| {
            let client = client.clone();
            async move { list(&client, &live(link), &ListOptions::default()).await.unwrap().unwrap() }
        };
        let s3 = read("https://noaa-ghcn-pds.s3.amazonaws.com/?prefix=csv/by_year/176").await;
        let first = s3.iter().find(|t| t.name == Some("1763.csv".into())).unwrap();
        assert_eq!((first.size, first.checksum.as_deref()), (Some(25333), Some("md5:a82389915a3d59147c906fe72463312a")));
        let gcs = read("gs://gcp-public-data-landsat/LC08/01/044/034/LC08_L1GT_044034_20130330_20170310_01_T2/").await;
        let mtl = gcs.iter().find(|t| t.name == Some("LC08_L1GT_044034_20130330_20170310_01_T2_MTL.txt".into())).unwrap();
        assert_eq!((mtl.size, mtl.checksum.as_deref()), (Some(8454), Some("md5:ce264d77cba2913b430a4053812e518b")));
        let azure = read("https://azureopendatastorage.blob.core.windows.net/nyctlc?restype=container&comp=list&prefix=_manifest").await;
        assert!(!azure.is_empty() && azure.iter().all(|t| t.size.is_some()), "{azure:?}");
    }

    /// A WebDAV folder is one task per file under it (subfolders kept, nothing outside it), read
    /// with PROPFIND and the link's sign-in; a file's link is that file; a missing or wrong
    /// sign-in, and a server that is no WebDAV one, say so.
    #[tokio::test]
    async fn webdav_folders_list_their_files() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0; 8192];
                let n = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..n]).to_string();
                let lower = request.to_ascii_lowercase();
                let target = request.split(' ').nth(1).unwrap_or_default();
                let (status, body) = if !request.starts_with("PROPFIND ") {
                    (405, Vec::new())
                } else if !lower.contains("\r\ndepth: 1\r\n") || !lower.contains("authorization: basic ym9ionb3") {
                    // "bob:pw"
                    (401, Vec::new())
                } else {
                    let (status, _, body) = match target {
                        // Apache's mod_dav names no displayname: the href does.
                        "/dav/Docs" => multistatus(&[("/dav/Docs/", "", None, ""), ("/dav/Docs/a%20b.txt", "", Some(3), "SHA1:A9993E364706816ABA3E25717850C26C9CD0D89D"), ("/dav/Docs/Sub/", "", None, ""), ("/dav/Docs2/out.txt", "", Some(1), ""), ("/dav/x.txt", "", Some(1), "")]),
                        "/dav/Docs/Sub/" => multistatus(&[("/dav/Docs/Sub/", "", None, ""), ("/dav/Docs/Sub/c.bin", "", Some(5), "")]),
                        "/dav/one.iso" => multistatus(&[("/dav/one.iso", "", Some(9), "")]),
                        _ => (404, String::new(), Vec::new()),
                    };
                    (status, body)
                };
                let head = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });
        let base = format!("http://{addr}");
        let bob = Some("Basic Ym9iOnB3");
        let folder = webdav_list(&http(), &Url::parse(&format!("{base}/dav/Docs")).unwrap(), bob).await.unwrap();
        let listed: Vec<_> = folder.iter().map(|t| (t.urls[0].to_string(), t.folder.clone().unwrap(), t.name.clone().unwrap(), t.size, t.checksum.clone(), t.from_document)).collect();
        assert_eq!(
            listed,
            [
                (format!("{base}/dav/Docs/a%20b.txt"), PathBuf::from("Docs"), PathBuf::from("a b.txt"), Some(3), Some("sha1:a9993e364706816aba3e25717850c26c9cd0d89d".to_string()), false),
                (format!("{base}/dav/Docs/Sub/c.bin"), PathBuf::from("Docs"), PathBuf::from("Sub").join("c.bin"), Some(5), None, false),
            ]
        );
        let file = webdav_list(&http(), &Url::parse(&format!("{base}/dav/one.iso")).unwrap(), bob).await.unwrap();
        assert_eq!((file[0].urls[0].to_string(), file[0].name.clone(), file[0].size), (format!("{base}/dav/one.iso"), Some(PathBuf::from("one.iso")), Some(9)));
        let docs = Url::parse(&format!("{base}/dav/Docs")).unwrap();
        assert_eq!(webdav_list(&http(), &docs, None).await.unwrap_err(), WEBDAV_LOCKED);
        assert_eq!(webdav_list(&http(), &docs, Some("Basic eDp5")).await.unwrap_err(), WEBDAV_REFUSED);
        // Through `list`: the sign-in in the link itself, or the one a paste took out of it.
        let inline = Url::parse(&format!("webdav://bob:pw@{addr}/dav/one.iso")).unwrap();
        assert_eq!(list(&http(), &inline, &ListOptions::default()).await.unwrap().unwrap().len(), 1);
        let pasted = Url::parse(&format!("webdav://{addr}/dav/one.iso")).unwrap();
        let options = ListOptions { auth: Some((pasted.clone(), "Basic Ym9iOnB3".into())), ..ListOptions::default() };
        assert_eq!(list(&http(), &pasted, &options).await.unwrap().unwrap().len(), 1);
        for (link, http_link) in [
            ("webdav://h.example/a/", "http://h.example/a/"),
            ("webdavs://u:p@h.example:8443/a%20b", "https://h.example:8443/a%20b"),
            ("davs://h.example", "https://h.example/"),
            ("dav://h.example/x?y=1", "http://h.example/x?y=1"),
        ] {
            assert_eq!(link_of(&Url::parse(link).unwrap()), Some(Link::WebDav { link: Url::parse(http_link).unwrap() }), "{link}");
            assert!(lists(&Url::parse(link).unwrap()));
        }
    }

    /// Live: an expired SwissTransfer transfer says so (transfers last 30 days at most, so no
    /// public one stays to list).
    #[tokio::test]
    #[ignore]
    async fn live_swisstransfer_expired() {
        let link = live("https://www.swisstransfer.com/d/1759d19c-516c-4682-826a-1346c75aea10");
        let listed = list(&crate::ingest::descriptor_client(None).unwrap(), &link, &ListOptions::default()).await.unwrap();
        assert_eq!(listed.unwrap_err(), SWISSTRANSFER_GONE);
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
        let tasks = list_link(&http(), &live("https://u.pcloud.link/publink/show?code=kZBjoO0Z0N6DdHilOAyiT3Vnb4u0nRJ2WBxk"), None).await.unwrap();
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
        let tasks = list_link(&http(), &live("https://yadi.sk/d/loPpY45J3EAYfU"), None).await.unwrap();
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
        let tasks = list_link(&http(), &live("https://app.box.com/s/1cwdnolsmtf0s04o0hshbv4vxiuqcmi9"), None).await.unwrap();
        assert!(tasks.iter().any(|t| t.name == Some("osgeopy-data-misc.zip".into())), "{tasks:?}");
    }

    /// Live: public shares on Nextcloud 30 and 35 (public.php/dav), Nextcloud 26 and ownCloud 10
    /// (public.php/webdav) list their files, and the first answers a range request with its bytes.
    #[tokio::test]
    #[ignore]
    async fn live_nextcloud_and_owncloud() {
        for link in [
            "https://libdrive.ethz.ch/index.php/s/wfmZSbsx4x7yFPA",
            "https://dataserv.ub.tum.de/s/m1459172",
            "https://kiwi.cmla.ens-cachan.fr/index.php/s/ss3rZwNSKwGtyQW",
            "https://box.skoltech.ru/index.php/s/ib52BOoV58ztuPM",
        ] {
            let tasks = list(&http(), &live(link), &ListOptions::default()).await.unwrap().unwrap();
            let answer = http().get(tasks[0].urls[0].clone()).header(RANGE, "bytes=0-1").send().await.unwrap();
            let page = answer.headers().get(CONTENT_TYPE).is_some_and(|t| t.as_bytes().starts_with(b"text/html"));
            assert!(answer.status() == StatusCode::PARTIAL_CONTENT && !page, "{link}: {:?} {}", tasks[0], answer.status());
        }
    }

    /// Live: WeTransfer's API says a transfer that does not exist has expired.
    #[tokio::test]
    #[ignore]
    async fn live_wetransfer_gone() {
        let link = live("https://wetransfer.com/downloads/0123456789abcdef0123456789abcdef20240101000000/abc123");
        assert_eq!(resolve(&http(), &link, None).await.unwrap_err().to_string(), ResolverError::NotFound(WETRANSFER_GONE.into()).to_string());
    }
}
