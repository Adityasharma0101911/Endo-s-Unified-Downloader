//! Cloud storage folder links (Google Drive, MediaFire) read into one download per file.
//!
//! Every file is a task under the folder's own name (subfolders kept), marked `from_document`:
//! its link is one the listing made, so the Authorization header the user gave is not sent there.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use crate::ingest::{clean_path, ListOptions, Task};
use crate::resolver::{google_docs_export, google_drive_direct_url};

/// The Drive API: the only host the user's Google API key is sent to.
const DRIVE_API: &str = "https://www.googleapis.com/drive/v3/";

/// What the Drive API tells of each child of a folder (see [`DriveItem`]).
const DRIVE_FIELDS: &str = "nextPageToken,files(id,name,mimeType,size,md5Checksum,resourceKey,shortcutDetails)";

const DRIVE_SHORTCUT: &str = "application/vnd.google-apps.shortcut";

/// A folder a link names, from its shape.
#[derive(Debug, PartialEq)]
enum Folder {
    /// A Google Drive folder, and the resource key one shared by link before 2021 opens with.
    Drive { id: String, resource_key: Option<String> },
    /// A MediaFire folder; `maybe` for a `/?key` link, which names a file or a folder.
    MediaFire { key: String, maybe: bool },
}

/// Whether `s` is an id or key as Drive and MediaFire make them.
fn token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The folder `url` names: drive.google.com/drive[/u/N]/folders/{id}, mediafire.com/folder/{key}[/...]
/// or mediafire.com/?{key}.
fn folder_of(url: &Url) -> Option<Folder> {
    let segs: Vec<&str> = url.path_segments()?.collect();
    match url.host_str()?.trim_end_matches('.') {
        "drive.google.com" => {
            let id = match segs[..] {
                ["drive", "folders", id, ..] => id,
                ["drive", "u", n, "folders", id, ..] if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => id,
                _ => return None,
            };
            let resource_key = url.query_pairs().find(|(k, _)| k == "resourcekey").map(|(_, v)| v.into_owned());
            token(id).then(|| Folder::Drive { id: id.to_string(), resource_key })
        }
        "mediafire.com" | "www.mediafire.com" => match segs[..] {
            ["folder", key, ..] if token(key) => Some(Folder::MediaFire { key: key.to_string(), maybe: false }),
            [""] => url.query().filter(|q| token(q)).map(|key| Folder::MediaFire { key: key.to_string(), maybe: true }),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `url` names a cloud storage folder this module lists, from its shape alone.
pub fn lists(url: &Url) -> bool {
    folder_of(url).is_some()
}

/// One task per file in the folder at `url`, subfolders kept in `Task::folder` or `Task::name`;
/// called only when [`lists`] takes `url`. None when it is no folder after all (the link is then
/// downloaded as it is); `Some(Ok)` is never empty.
///
/// A Google Drive folder is listed through the Drive API with the user's Google API key, else
/// from its public page. Requests are made one at a time.
pub async fn list(http: &reqwest::Client, url: &Url, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    let listed = match folder_of(url)? {
        Folder::Drive { id, resource_key } => {
            let source = match options.google_api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
                Some(key) => DriveSource::Api { base: DRIVE_API, key },
                None => {
                    tracing::warn!(
                        "Listing the Google Drive folder from its public page, which gives no sizes or checksums and may not \
                         show every file of a very large folder: add a Google API key (--google-api-key, or the Google API \
                         key setting) to list it whole through the Drive API"
                    );
                    DriveSource::Page { link: url }
                }
            };
            drive_list(http, &source, id, resource_key).await
        }
        Folder::MediaFire { key, maybe } => mediafire_list(http, url.scheme(), &key, maybe).await?,
    };
    Some(listed.and_then(|tasks| {
        if tasks.is_empty() {
            Err("the folder holds no files that can be downloaded".to_string())
        } else {
            Ok(tasks)
        }
    }))
}

/// `name` made one path component, else `id` (a name like ".." leaves nothing).
fn component(name: &str, id: &str) -> Result<PathBuf, String> {
    clean_path([name]).or_else(|_| clean_path([id]))
}

/// Where a Drive folder is read.
enum DriveSource<'a> {
    /// The Drive API at `base`, with the user's key.
    Api { base: &'a str, key: &'a str },
    /// The public embedded view of the folder, on the host and scheme of the link typed.
    Page { link: &'a Url },
}

/// What a child of a Drive folder is to a download.
#[derive(Debug, PartialEq)]
enum Kind {
    Folder,
    File,
    /// A Docs, Sheets or Slides document, downloaded as its export.
    Export(Url),
    /// A form, drawing, site or other item with nothing to download.
    Other,
}

/// A child of a Drive folder, as the API or the folder page lists it.
#[derive(Debug)]
struct DriveChild {
    id: String,
    name: String,
    kind: Kind,
    resource_key: Option<String>,
    size: Option<u64>,
    md5: Option<String>,
}

/// A child of a folder as a listing takes it.
enum Child {
    /// A subfolder, and its path under the save folder.
    Folder { id: String, resource_key: Option<String>, path: PathBuf },
    File(Task),
    Other,
}

/// An item as the Drive API describes it (the fields [`DRIVE_FIELDS`] asks for).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveItem {
    id: String,
    name: String,
    mime_type: String,
    /// A decimal string, as the API gives 64-bit numbers.
    size: Option<String>,
    md5_checksum: Option<String>,
    resource_key: Option<String>,
    shortcut_details: Option<DriveShortcut>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveShortcut {
    target_id: String,
    target_mime_type: String,
    target_resource_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DrivePage {
    #[serde(default)]
    files: Vec<DriveItem>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct DriveName {
    name: String,
}

impl From<DriveItem> for DriveChild {
    /// A shortcut stands for its target, under the shortcut's name.
    fn from(item: DriveItem) -> Self {
        let (id, mime_type, resource_key, size, md5) = match item.shortcut_details {
            Some(target) if item.mime_type == DRIVE_SHORTCUT => {
                (target.target_id, target.target_mime_type, target.target_resource_key, None, None)
            }
            _ => (item.id, item.mime_type, item.resource_key, item.size.and_then(|s| s.parse().ok()), item.md5_checksum),
        };
        let kind = drive_kind(&id, &mime_type);
        DriveChild { id, name: item.name, kind, resource_key, size, md5 }
    }
}

/// What a Drive item of this MIME type is: Docs, Sheets and Slides export as Google's own
/// File > Download does (see `google_docs_export`); forms, drawings, sites and the like are none.
fn drive_kind(id: &str, mime_type: &str) -> Kind {
    let editor = match mime_type.strip_prefix("application/vnd.google-apps.") {
        None => return Kind::File,
        Some("folder") => return Kind::Folder,
        Some("document") => "document",
        Some("spreadsheet") => "spreadsheets",
        Some("presentation") => "presentation",
        Some(_) => return Kind::Other,
    };
    let document = Url::parse(&format!("https://docs.google.com/{}/d/{}/edit", editor, id)).ok();
    document.and_then(|d| google_docs_export(&d)).map_or(Kind::Other, Kind::Export)
}

/// The child of a Drive folder the embedded view links as `href`: a folder, a file, or a
/// document of Docs, Sheets or Slides (by its editor link); anything else is left out.
fn page_child(href: &str, name: String) -> DriveChild {
    let other = |name| DriveChild { id: String::new(), name, kind: Kind::Other, resource_key: None, size: None, md5: None };
    let Ok(url) = Url::parse(href) else { return other(name) };
    let resource_key = url.query_pairs().find(|(k, _)| k == "resourcekey").map(|(_, v)| v.into_owned());
    if let Some(Folder::Drive { id, resource_key }) = folder_of(&url) {
        return DriveChild { id, name, kind: Kind::Folder, resource_key, size: None, md5: None };
    }
    let segs: Vec<&str> = url.path_segments().map(Iterator::collect).unwrap_or_default();
    let (id, kind) = match (url.host_str().unwrap_or_default(), &segs[..]) {
        ("drive.google.com", ["file", "d", id, ..]) => (*id, Kind::File),
        ("docs.google.com", [_, "d", id, ..]) => (*id, google_docs_export(&url).map_or(Kind::Other, Kind::Export)),
        _ => return other(name),
    };
    if !token(id) {
        return other(name);
    }
    DriveChild { id: id.to_string(), name, kind, resource_key, size: None, md5: None }
}

impl DriveChild {
    /// What this child of the folder at `folder` (a path under the save folder) becomes.
    fn into_child(self, folder: &Path) -> Result<Child, String> {
        let task = |urls, name: &str, size, checksum| -> Result<Child, String> {
            Ok(Child::File(Task {
                urls,
                folder: Some(folder.to_path_buf()),
                name: Some(component(name, &self.id)?),
                size,
                checksum,
                from_document: true,
                ..Task::default()
            }))
        };
        match &self.kind {
            Kind::Folder => {
                let path = folder.join(component(&self.name, &self.id)?);
                Ok(Child::Folder { id: self.id.clone(), resource_key: self.resource_key.clone(), path })
            }
            Kind::File => {
                let url = google_drive_direct_url(&self.id, self.resource_key.as_deref()).map_err(|e| e.to_string())?;
                let checksum = self.md5.as_ref().map(|md5| format!("md5:{}", md5));
                let checksum = checksum.filter(|c| crate::storage::validate_checksum(c).is_ok());
                task(vec![url], &self.name, self.size, checksum)
            }
            // Made on the fly: no size or checksum is known.
            Kind::Export(export) => {
                let format = export.query_pairs().find(|(k, _)| k == "format").map(|(_, v)| v.into_owned()).unwrap_or_default();
                task(vec![export.clone()], &with_extension(&self.name, &format), None, None)
            }
            Kind::Other => Ok(Child::Other),
        }
    }
}

/// `name` ending in `.extension`, which a document's name has not.
fn with_extension(name: &str, extension: &str) -> String {
    let dotted = format!(".{}", extension);
    if extension.is_empty() || name.to_ascii_lowercase().ends_with(&dotted) {
        name.to_string()
    } else {
        name.to_string() + &dotted
    }
}

/// Every file under the Drive folder `id`, one task each, in a folder named after it. A folder
/// reached twice (a shortcut up the tree) is listed once.
async fn drive_list(
    http: &reqwest::Client,
    source: &DriveSource<'_>,
    id: String,
    resource_key: Option<String>,
) -> Result<Vec<Task>, String> {
    let mut queue = VecDeque::from([(id, resource_key, None)]);
    let mut listed = HashSet::new();
    let (mut tasks, mut left_out) = (Vec::new(), 0);
    while let Some((id, resource_key, path)) = queue.pop_front() {
        if !listed.insert(id.clone()) {
            continue;
        }
        let (name, children) = source.read(http, &id, resource_key.as_deref(), path.is_none()).await?;
        let folder = match path {
            Some(path) => path,
            None => component(name.as_deref().unwrap_or_default(), &id)?,
        };
        for child in children {
            match child.into_child(&folder)? {
                Child::Folder { id, resource_key, path } => queue.push_back((id, resource_key, Some(path))),
                Child::File(task) => tasks.push(task),
                Child::Other => left_out += 1,
            }
        }
    }
    if left_out > 0 {
        tracing::warn!("{} items of the Google Drive folder (forms, drawings, sites, ...) are no files and were left out", left_out);
    }
    Ok(tasks)
}

impl DriveSource<'_> {
    /// The children of the Drive folder `id`, and its name if `named` (the page always gives it).
    async fn read(
        &self,
        http: &reqwest::Client,
        id: &str,
        resource_key: Option<&str>,
        named: bool,
    ) -> Result<(Option<String>, Vec<DriveChild>), String> {
        match self {
            DriveSource::Api { base, key } => {
                let keys = resource_key.map(|rk| format!("{}/{}", id, rk));
                let url = |path: &str, params: &[(&str, &str)]| {
                    let params = params.iter().copied().chain([("supportsAllDrives", "true"), ("key", *key)]);
                    Url::parse_with_params(&format!("{}{}", base, path), params).map_err(|e| e.to_string())
                };
                let name = if named {
                    let folder = url(&format!("files/{}", id), &[("fields", "name")])?;
                    Some(drive_api_get::<DriveName>(http, folder, keys.as_deref()).await?.name)
                } else {
                    None
                };
                let query = format!("'{}' in parents and trashed=false", id);
                let mut children = Vec::new();
                let mut page_token = None;
                loop {
                    let mut params = vec![("q", query.as_str()), ("fields", DRIVE_FIELDS), ("pageSize", "1000"), ("includeItemsFromAllDrives", "true")];
                    params.extend(page_token.as_deref().map(|token| ("pageToken", token)));
                    let page: DrivePage = drive_api_get(http, url("files", &params)?, keys.as_deref()).await?;
                    children.extend(page.files.into_iter().map(DriveChild::from));
                    match page.next_page_token {
                        Some(token) if !token.is_empty() => page_token = Some(token),
                        _ => break,
                    }
                }
                Ok((name, children))
            }
            DriveSource::Page { link } => {
                let html = drive_page(http, link, id, resource_key).await?;
                let (name, entries) = parse_embedded_view(&html).ok_or_else(|| {
                    "the Google Drive folder page could not be read (its layout may have changed); add a Google API key to list the folder through the Drive API".to_string()
                })?;
                Ok((Some(name), entries.into_iter().map(|(href, name)| page_child(&href, name)).collect()))
            }
        }
    }
}

/// The Drive API's answer at `url`, which carries the user's key: no error shows the URL.
async fn drive_api_get<T: DeserializeOwned>(http: &reqwest::Client, url: Url, resource_keys: Option<&str>) -> Result<T, String> {
    let mut request = http.get(url);
    if let Some(keys) = resource_keys {
        request = request.header("X-Goog-Drive-Resource-Keys", keys);
    }
    let fail = |e: reqwest::Error| format!("Cannot reach the Google Drive API: {}", e.without_url());
    let resp = request.send().await.map_err(fail)?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await.map_err(fail)?;
    if !(200..300).contains(&status) {
        return Err(drive_api_error(status, &body));
    }
    serde_json::from_slice(&body).map_err(|e| format!("Unexpected answer from the Google Drive API: {}", e))
}

/// What the Drive API's error answer (`{"error": {"message": ...}}`) says, a folder the key
/// cannot see in plain words.
fn drive_api_error(status: u16, body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Answer {
        error: Detail,
    }
    #[derive(Deserialize)]
    struct Detail {
        message: String,
    }
    if status == 404 {
        return "Google Drive folder not found: it is private, deleted, or the link is wrong (a Google API key lists only folders shared as \"Anyone with the link\")".to_string();
    }
    match serde_json::from_slice::<Answer>(body) {
        Ok(answer) => format!("Google Drive API: {} (HTTP {})", answer.error.message, status),
        Err(_) => format!("Google Drive API answered HTTP {}", status),
    }
}

/// The embedded view of the Drive folder `id` (drive.google.com/embeddedfolderview?id=), which
/// lists a whole folder (1000 of 1000 files, checked live) where its /drive/folders/ page embeds
/// only the first 50.
async fn drive_page(http: &reqwest::Client, link: &Url, id: &str, resource_key: Option<&str>) -> Result<String, String> {
    let mut url = link.clone();
    url.set_path("/embeddedfolderview");
    url.set_fragment(None);
    url.query_pairs_mut().clear().append_pair("id", id).extend_pairs(resource_key.map(|rk| ("resourcekey", rk)));
    let fail = |e: reqwest::Error| format!("Cannot read the Google Drive folder: {}", e);
    let resp = http.get(url).send().await.map_err(fail)?;
    match resp.status().as_u16() {
        401 | 403 => return Err(DRIVE_PRIVATE.to_string()),
        _ if resp.url().host_str() == Some("accounts.google.com") => return Err(DRIVE_PRIVATE.to_string()),
        404 => return Err("Google Drive folder not found: it was deleted, or the link is wrong".to_string()),
        _ => {}
    }
    resp.error_for_status().map_err(fail)?.text().await.map_err(fail)
}

const DRIVE_PRIVATE: &str =
    "the Google Drive folder is private: it must be shared as \"Anyone with the link\" (a link from before 2021 may also need its resourcekey)";

/// The folder's name and its entries (link, name) in Drive's embedded folder view, as checked
/// live: `<title>` names the folder, and in `<div class="flip-entries">` each
/// `<div class="flip-entry" ...>` holds an `<a href>` to the item and a
/// `<div class="flip-entry-title">` with its name. None when the page has no such list.
fn parse_embedded_view(html: &str) -> Option<(String, Vec<(String, String)>)> {
    let title = between(html, "<title>", "<")?;
    let (_, list) = html.split_once("class=\"flip-entries\"")?;
    let entries = list
        .split("class=\"flip-entry\"")
        .skip(1)
        .filter_map(|entry| {
            let href = between(entry, "href=\"", "\"")?;
            let name = between(entry, "class=\"flip-entry-title\">", "<")?;
            Some((unescape(href), unescape(name)))
        })
        .collect();
    Some((unescape(title), entries))
}

/// The text of `s` between the first `start` and the next `end`.
fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let (_, rest) = s.split_once(start)?;
    rest.split_once(end).map(|(inside, _)| inside)
}

/// `text` from HTML with its character references decoded (as it is if one is unknown).
fn unescape(text: &str) -> String {
    quick_xml::escape::unescape(text).map_or_else(|_| text.to_string(), |t| t.into_owned()).trim().to_string()
}

/// MediaFire's answer to an API call.
#[derive(Debug, Deserialize)]
struct MfAnswer {
    response: MfResponse,
}

#[derive(Debug, Deserialize)]
struct MfResponse {
    /// "Success" or "Error".
    result: String,
    message: Option<String>,
    folder_info: Option<MfFolder>,
    folder_content: Option<MfContent>,
}

/// One chunk of a folder's files or subfolders (MediaFire gives 100 at a time).
#[derive(Debug, Default, Deserialize)]
struct MfContent {
    #[serde(default)]
    files: Vec<MfFile>,
    #[serde(default)]
    folders: Vec<MfFolder>,
    /// "yes" when another chunk follows.
    #[serde(default)]
    more_chunks: String,
}

#[derive(Debug, Deserialize)]
struct MfFile {
    quickkey: String,
    filename: String,
    /// The file's SHA-256, in hex.
    #[serde(default)]
    hash: String,
    /// A decimal string.
    #[serde(default)]
    size: String,
    #[serde(default)]
    password_protected: String,
}

#[derive(Debug, Deserialize)]
struct MfFolder {
    #[serde(default)]
    folderkey: String,
    name: String,
}

/// MediaFire's answer in `body`, or what its error says (an unknown folder is answered 404).
fn mediafire_answer(status: u16, body: &[u8]) -> Result<MfResponse, String> {
    let Ok(MfAnswer { response }) = serde_json::from_slice::<MfAnswer>(body) else {
        return Err(format!("MediaFire answered HTTP {} instead of listing the folder", status));
    };
    if response.result != "Success" {
        let message = response.message.unwrap_or_else(|| format!("HTTP {}", status));
        return Err(format!("MediaFire cannot list the folder: {} (it may be private or deleted)", message));
    }
    Ok(response)
}

/// Calls `method` of MediaFire's folder API at `api`.
async fn mediafire_get(http: &reqwest::Client, api: &str, method: &str, params: &[(&str, &str)]) -> Result<MfResponse, String> {
    let params = params.iter().copied().chain([("response_format", "json")]);
    let url = Url::parse_with_params(&format!("{}{}", api, method), params).map_err(|e| e.to_string())?;
    let fail = |e: reqwest::Error| format!("Cannot reach MediaFire: {}", e);
    let resp = http.get(url).send().await.map_err(fail)?;
    let status = resp.status().as_u16();
    mediafire_answer(status, &resp.bytes().await.map_err(fail)?)
}

/// Every file (`content_type` "files") or subfolder ("folders") of the MediaFire folder `key`,
/// a chunk at a time.
async fn mediafire_content(http: &reqwest::Client, api: &str, key: &str, content_type: &str) -> Result<MfContent, String> {
    let mut all = MfContent::default();
    for chunk in 1u32.. {
        let chunk = chunk.to_string();
        let params = [("folder_key", key), ("content_type", content_type), ("chunk", chunk.as_str())];
        let content = mediafire_get(http, api, "get_content.php", &params).await?.folder_content.unwrap_or_default();
        // A chunk with nothing in it ends the listing whatever it says.
        let more = content.more_chunks == "yes" && !(content.files.is_empty() && content.folders.is_empty());
        all.files.extend(content.files);
        all.folders.extend(content.folders);
        if !more {
            break;
        }
    }
    Ok(all)
}

/// Every file under the MediaFire folder `key`, each a task for its page (which
/// `MediaFireResolver` downloads), with its SHA-256; files behind a password are left out. The
/// API is asked at the scheme of the link typed. None for a `maybe` link that names no folder.
async fn mediafire_list(http: &reqwest::Client, scheme: &str, key: &str, maybe: bool) -> Option<Result<Vec<Task>, String>> {
    let api = format!("{}://www.mediafire.com/api/1.5/folder/", scheme);
    let root = match mediafire_get(http, &api, "get_info.php", &[("folder_key", key)]).await {
        Ok(info) => info.folder_info.map(|f| f.name).unwrap_or_default(),
        // A file's link: the engine downloads it.
        Err(_) if maybe => return None,
        Err(e) => return Some(Err(e)),
    };
    Some(mediafire_files(http, scheme, &api, key, &root).await)
}

async fn mediafire_files(http: &reqwest::Client, scheme: &str, api: &str, key: &str, root: &str) -> Result<Vec<Task>, String> {
    let mut queue = VecDeque::from([(key.to_string(), component(root, key)?)]);
    let mut listed = HashSet::new();
    let (mut tasks, mut locked) = (Vec::new(), 0);
    while let Some((key, folder)) = queue.pop_front() {
        if !listed.insert(key.clone()) {
            continue;
        }
        for file in mediafire_content(http, api, &key, "files").await?.files {
            let page = token(&file.quickkey).then(|| Url::parse(&format!("{}://www.mediafire.com/file/{}", scheme, file.quickkey)));
            let Some(Ok(page)) = page else { continue };
            if file.password_protected == "yes" {
                locked += 1;
                continue;
            }
            let checksum = Some(format!("sha256:{}", file.hash)).filter(|c| crate::storage::validate_checksum(c).is_ok());
            tasks.push(Task {
                urls: vec![page],
                folder: Some(folder.clone()),
                name: Some(component(&file.filename, &file.quickkey)?),
                checksum,
                size: file.size.parse().ok(),
                from_document: true,
                ..Task::default()
            });
        }
        for sub in mediafire_content(http, api, &key, "folders").await?.folders {
            if token(&sub.folderkey) {
                let path = folder.join(component(&sub.name, &sub.folderkey)?);
                queue.push_back((sub.folderkey, path));
            }
        }
    }
    if locked > 0 {
        tracing::warn!("{} files of the MediaFire folder are protected by a password and were left out", locked);
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn folder(link: &str) -> Option<Folder> {
        folder_of(&Url::parse(link).unwrap())
    }

    #[test]
    fn folder_links_are_told_by_their_shape() {
        let drive = |id: &str, key: Option<&str>| Some(Folder::Drive { id: id.into(), resource_key: key.map(Into::into) });
        assert_eq!(folder("https://drive.google.com/drive/folders/1KpLl_1tcK0eeehzN980zbG-3M2nhbVks"), drive("1KpLl_1tcK0eeehzN980zbG-3M2nhbVks", None));
        assert_eq!(folder("https://drive.google.com/drive/u/1/folders/1aB-c?resourcekey=0-xY_z&usp=sharing"), drive("1aB-c", Some("0-xY_z")));
        let mediafire = |key: &str, maybe| Some(Folder::MediaFire { key: key.into(), maybe });
        assert_eq!(folder("https://www.mediafire.com/folder/rww7bhhi0yc1l/NewFolder"), mediafire("rww7bhhi0yc1l", false));
        assert_eq!(folder("http://mediafire.com/folder/rww7bhhi0yc1l"), mediafire("rww7bhhi0yc1l", false));
        // The old form names a folder or a file: the folder API tells which.
        assert_eq!(folder("https://www.mediafire.com/?rww7bhhi0yc1l"), mediafire("rww7bhhi0yc1l", true));
        for other in [
            "https://drive.google.com/file/d/1Z2VYnXb01h/view",
            "https://drive.google.com/drive/my-drive",
            "https://drive.google.com/drive/u/x/folders/1aB",
            "https://drive.google.com/drive/folders/",
            "https://drive.google.com/drive/folders/a%27b",
            "https://docs.google.com/drive/folders/1aB",
            "https://www.mediafire.com/file/ierdl9nle7ask6i/a.png/file",
            "https://www.mediafire.com/?sharekey=abc",
            "https://www.mediafire.com/",
            "https://mediafire.example/folder/abc",
        ] {
            assert_eq!(folder(other), None, "{other}");
        }
        assert!(crate::ingest::needs_reading("https://drive.google.com/drive/folders/1KpLl_1tc"));
        assert!(crate::ingest::needs_reading("https://www.google.com/url?q=https%3A%2F%2Fdrive.google.com%2Fdrive%2Ffolders%2F1aB"));
        assert!(crate::ingest::needs_reading("https://www.google.com/url?q=https%3A%2F%2Fwww.mediafire.com%2Ffolder%2Fabc"));
    }

    /// drive.google.com/embeddedfolderview?id=1KpLl_1tcK0eeehzN980zbG-3M2nhbVks as it answered,
    /// its styles, scripts and all but one file cut, with the entry of a Slides document from
    /// another folder's.
    const EMBEDDED_VIEW: &str = concat!(
        r#"<!DOCTYPE html><html><head><title>gdown_folder_test</title><meta http-equiv="content-type" content="text/html; charset=utf-8"/></head>"#,
        r#"<body class="flip-embedded"><div class="flip-butter-container"><div id="flip-butter"></div></div><div id="flip-contents" class="flip-contents flip-list-view">"#,
        r#"<div class="flip-list-header"><div role="heading" aria-level="2" class="flip-list-title-header">TITLE</div><div role="heading" aria-level="2" class="flip-list-last-modified-header">LAST MODIFIED</div></div><div class="flip-entries">"#,
        r#"<div class="flip-entry" id="entry-1aMZqPaU03E7XOQNXtjSCdguRHBaIQ82m" tabindex="0" role="link"><div class="flip-entry-info"><a href="https://drive.google.com/drive/folders/1aMZqPaU03E7XOQNXtjSCdguRHBaIQ82m" target="_blank"><div class="flip-entry-visual"><div class="flip-entry-visual-card"><div class="flip-entry-icon"><div aria-label="Folder" class="icon-color-1 drive-sprite-folder-grid-shared-icon"></div></div></div></div><div class="flip-entry-list-icon"><div aria-label="Folder" class="icon-color-1 drive-sprite-folder-list-shared-icon"></div></div><div class="flip-entry-title">directory-0</div></a></div><div class="flip-entry-last-modified"><div>10/29/20</div></div></div>"#,
        r#"<div class="flip-entry" id="entry-1Z2VYnXb01h-3uvEptoQ48Fo__eAn0wc1" tabindex="0" role="link"><div class="flip-entry-info"><a href="https://drive.google.com/file/d/1Z2VYnXb01h-3uvEptoQ48Fo__eAn0wc1/view?usp=drive_web" target="_blank"><div class="flip-entry-visual"><div class="flip-entry-visual-card"><div class="flip-entry-thumb"><img src="https://lh3.googleusercontent.com/drive-storage/AJQWtBNqvVfAhDLnj=s190" alt="JPEG Image"/></div></div></div><div class="flip-entry-list-icon"><img src="https://drive-thirdparty.googleusercontent.com/16/type/image/jpeg" alt=""/></div><div class="flip-entry-title">fractal.jpg</div></a></div><div class="flip-entry-last-modified"><div>10/29/20</div></div></div>"#,
        r#"<div class="flip-entry" id="entry-1DvsG277pWa4WMssXjD9qYYAdF51y7hVidZ6eklfq480" tabindex="0" role="link"><div class="flip-entry-info"><a href="https://docs.google.com/presentation/d/1DvsG277pWa4WMssXjD9qYYAdF51y7hVidZ6eklfq480/edit?usp=drive_web" target="_blank"><div class="flip-entry-visual"><div class="flip-entry-visual-card"><div class="flip-entry-icon"><img src="https://drive-thirdparty.googleusercontent.com/128/type/application/vnd.google-apps.presentation" alt="Presentation"/></div></div></div><div class="flip-entry-list-icon"><img src="https://drive-thirdparty.googleusercontent.com/16/type/application/vnd.google-apps.presentation" alt=""/></div><div class="flip-entry-title">gdown</div></a></div><div class="flip-entry-last-modified"><div>Apr 11</div></div></div></div></div></body></html>"#,
    );

    /// The embedded view names the folder and links each child: a subfolder by its folder link, a
    /// file by its file page, a document by its editor; a page without the list is none.
    #[test]
    fn the_embedded_folder_view_is_read() {
        let (name, entries) = parse_embedded_view(EMBEDDED_VIEW).unwrap();
        assert_eq!(name, "gdown_folder_test");
        let children: Vec<_> = entries.into_iter().map(|(href, name)| page_child(&href, name)).collect();
        let shown: Vec<_> = children.iter().map(|c| (c.id.as_str(), c.name.as_str(), &c.kind)).collect();
        let export = Url::parse("https://docs.google.com/presentation/d/1DvsG277pWa4WMssXjD9qYYAdF51y7hVidZ6eklfq480/export?format=pptx").unwrap();
        assert_eq!(
            shown,
            [
                ("1aMZqPaU03E7XOQNXtjSCdguRHBaIQ82m", "directory-0", &Kind::Folder),
                ("1Z2VYnXb01h-3uvEptoQ48Fo__eAn0wc1", "fractal.jpg", &Kind::File),
                ("1DvsG277pWa4WMssXjD9qYYAdF51y7hVidZ6eklfq480", "gdown", &Kind::Export(export)),
            ]
        );
        let keyed = page_child("https://drive.google.com/file/d/1Z2/view?usp=drive_web&resourcekey=0-k", "a".into());
        assert_eq!(keyed.resource_key.as_deref(), Some("0-k"));
        for other in ["https://docs.google.com/forms/d/1Fm/edit", "https://sites.google.com/view/x", "not a link"] {
            assert_eq!(page_child(other, "x".into()).kind, Kind::Other, "{other}");
        }
        // Names come decoded.
        let entry = r#"<title>R&amp;D &#39;26</title><div class="flip-entries"><div class="flip-entry" id="e"><a href="https://drive.google.com/file/d/1a/view?a=1&amp;b=2"><div class="flip-entry-title">Q&amp;A.pdf</div></a></div></div>"#;
        let (name, entries) = parse_embedded_view(entry).unwrap();
        assert_eq!((name.as_str(), entries), ("R&D '26", vec![("https://drive.google.com/file/d/1a/view?a=1&b=2".to_string(), "Q&A.pdf".to_string())]));
        assert_eq!(parse_embedded_view("<!doctype html><title>Sign in - Google Accounts</title>"), None);
    }

    #[test]
    fn drive_items_become_downloads_documents_their_export() {
        let items: Vec<DriveItem> = serde_json::from_str(
            r#"[
            {"id": "f1", "name": "a.bin", "mimeType": "application/octet-stream", "size": "5", "md5Checksum": "5d41402abc4b2a76b9719d911017c592", "resourceKey": "0-r"},
            {"id": "d1", "name": "Notes", "mimeType": "application/vnd.google-apps.document", "size": "1024"},
            {"id": "x1", "name": "Budget.XLSX", "mimeType": "application/vnd.google-apps.spreadsheet"},
            {"id": "fm1", "name": "Survey", "mimeType": "application/vnd.google-apps.form"},
            {"id": "s1", "name": "Link to b", "mimeType": "application/vnd.google-apps.shortcut", "shortcutDetails": {"targetId": "f9", "targetMimeType": "image/png", "targetResourceKey": "0-t"}},
            {"id": "s2", "name": "Link to sub", "mimeType": "application/vnd.google-apps.shortcut", "shortcutDetails": {"targetId": "sub9", "targetMimeType": "application/vnd.google-apps.folder"}},
            {"id": "s3", "name": "Broken link", "mimeType": "application/vnd.google-apps.shortcut"}
            ]"#,
        )
        .unwrap();
        let root = Path::new("Pack");
        let mut tasks = Vec::new();
        let mut folders = Vec::new();
        let mut others = 0;
        for item in items {
            match DriveChild::from(item).into_child(root).unwrap() {
                Child::File(task) => tasks.push(task),
                Child::Folder { id, resource_key, path } => folders.push((id, resource_key, path)),
                Child::Other => others += 1,
            }
        }
        let direct = |id, key| Some(google_drive_direct_url(id, key).unwrap());
        let docs = |link: &str| Url::parse(link).ok();
        let shown: Vec<_> =
            tasks.iter().map(|t| (t.urls.first().cloned(), t.name.clone().unwrap(), t.size, t.checksum.clone())).collect();
        assert_eq!(
            shown,
            [
                (direct("f1", Some("0-r")), PathBuf::from("a.bin"), Some(5), Some("md5:5d41402abc4b2a76b9719d911017c592".to_string())),
                (docs("https://docs.google.com/document/d/d1/export?format=docx"), PathBuf::from("Notes.docx"), None, None),
                (docs("https://docs.google.com/spreadsheets/d/x1/export?format=xlsx"), PathBuf::from("Budget.XLSX"), None, None),
                (direct("f9", Some("0-t")), PathBuf::from("Link to b"), None, None),
            ]
        );
        assert!(tasks.iter().all(|t| t.folder.as_deref() == Some(root) && t.from_document));
        assert_eq!(folders, [("sub9".to_string(), None, root.join("Link to sub"))]);
        assert_eq!(others, 2, "a form and a shortcut without a target have nothing to download");
        assert_eq!(with_extension("Deck", "pptx"), "Deck.pptx");
    }

    /// Drive API errors as the API answered them (a bad key, no key); a folder it cannot see is
    /// answered 404.
    #[test]
    fn drive_api_errors_say_what_is_wrong() {
        let bad_key = br#"{"error": {"code": 400, "message": "API key not valid. Please pass a valid API key.", "errors": [{"message": "API key not valid. Please pass a valid API key.", "domain": "global", "reason": "badRequest"}], "status": "INVALID_ARGUMENT"}}"#;
        assert_eq!(drive_api_error(400, bad_key), "Google Drive API: API key not valid. Please pass a valid API key. (HTTP 400)");
        let no_key = br#"{"error": {"code": 403, "message": "Method doesn't allow unregistered callers (callers without established identity). Please use API Key or other form of API consumer identity to call this API.", "status": "PERMISSION_DENIED"}}"#;
        assert!(drive_api_error(403, no_key).starts_with("Google Drive API: Method doesn't allow unregistered callers"));
        let missing = br#"{"error": {"code": 404, "message": "File not found: 1aB.", "errors": [{"reason": "notFound"}]}}"#;
        assert!(drive_api_error(404, missing).starts_with("Google Drive folder not found: it is private, deleted"));
        assert_eq!(drive_api_error(502, b"<html>Bad Gateway</html>"), "Google Drive API answered HTTP 502");
    }

    /// The target of each request, with the resource keys it sent.
    type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

    /// A local stand-in for the Drive API at the base URL returned, answering each request with
    /// `answer(target)` as JSON.
    async fn serve(answer: fn(&str) -> (u16, String)) -> (String, Seen) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/drive/v3/", listener.local_addr().unwrap());
        let seen = Seen::default();
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let target = head.split(' ').nth(1).unwrap_or_default().to_string();
                let keys = head.lines().find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.eq_ignore_ascii_case("x-goog-drive-resource-keys").then(|| value.trim().to_string())
                });
                log.lock().unwrap().push((target.clone(), keys));
                let (status, body) = answer(&target);
                let head = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all((head + &body).as_bytes()).await;
            }
        });
        (base, seen)
    }

    /// A folder "Pack: 1" whose files come on two pages, with a subfolder that needs its resource
    /// key and holds a shortcut back up to "Pack: 1".
    fn drive_api(target: &str) -> (u16, String) {
        let url = Url::parse(&format!("http://api.test{}", target)).unwrap();
        let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
        if param("key").as_deref() != Some("AIzaKey") || param("supportsAllDrives").as_deref() != Some("true") {
            return (400, r#"{"error": {"code": 400, "message": "API key not valid."}}"#.into());
        }
        let listing = param("fields").as_deref() == Some(DRIVE_FIELDS) && param("includeItemsFromAllDrives").as_deref() == Some("true");
        let body = match (url.path(), param("q").as_deref(), param("pageToken").as_deref()) {
            ("/drive/v3/files/root1", None, None) if param("fields").as_deref() == Some("name") => r#"{"name": "Pack: 1"}"#,
            ("/drive/v3/files", Some("'root1' in parents and trashed=false"), None) if listing => {
                r#"{"nextPageToken": "p2", "files": [
                    {"id": "f1", "name": "a.bin", "mimeType": "application/octet-stream", "size": "5", "md5Checksum": "5d41402abc4b2a76b9719d911017c592"},
                    {"id": "sub1", "name": "Sub", "mimeType": "application/vnd.google-apps.folder", "resourceKey": "0-rk"}]}"#
            }
            ("/drive/v3/files", Some("'root1' in parents and trashed=false"), Some("p2")) if listing => {
                r#"{"files": [{"id": "d1", "name": "Notes", "mimeType": "application/vnd.google-apps.document"}]}"#
            }
            ("/drive/v3/files", Some("'sub1' in parents and trashed=false"), None) if listing => {
                r#"{"files": [
                    {"id": "s1", "name": "Back up", "mimeType": "application/vnd.google-apps.shortcut", "shortcutDetails": {"targetId": "root1", "targetMimeType": "application/vnd.google-apps.folder"}},
                    {"id": "f2", "name": "b.txt", "mimeType": "text/plain", "size": "7"}]}"#
            }
            _ => return (404, r#"{"error": {"code": 404, "message": "File not found: x."}}"#.into()),
        };
        (200, body.into())
    }

    /// A whole tree is listed through the API, page by page, each folder once, with the key on
    /// every request and a folder's resource key on the requests for it.
    #[tokio::test]
    async fn the_drive_api_lists_a_folder_tree() {
        let (base, seen) = serve(drive_api).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let source = DriveSource::Api { base: &base, key: "AIzaKey" };
        let tasks = drive_list(&http, &source, "root1".into(), None).await.unwrap();
        let top = PathBuf::from("Pack_ 1");
        let listed: Vec<_> = tasks.iter().map(|t| (t.folder.clone().unwrap(), t.name.clone().unwrap(), t.urls[0].to_string())).collect();
        assert_eq!(
            listed,
            [
                (top.clone(), PathBuf::from("a.bin"), google_drive_direct_url("f1", None).unwrap().to_string()),
                (top.clone(), PathBuf::from("Notes.docx"), "https://docs.google.com/document/d/d1/export?format=docx".to_string()),
                (top.join("Sub"), PathBuf::from("b.txt"), google_drive_direct_url("f2", None).unwrap().to_string()),
            ]
        );
        assert_eq!((tasks[0].size, tasks[0].checksum.as_deref()), (Some(5), Some("md5:5d41402abc4b2a76b9719d911017c592")));
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 4, "the name, two pages and the subfolder, and Pack: 1 once: {seen:?}");
        assert_eq!(seen[3].1.as_deref(), Some("sub1/0-rk"));
        assert!(seen[..3].iter().all(|(_, keys)| keys.is_none()), "{seen:?}");

        let missing = drive_list(&http, &source, "gone".into(), None).await.unwrap_err();
        assert!(missing.starts_with("Google Drive folder not found"), "{missing}");
        let wrong_key = DriveSource::Api { base: &base, key: "AIzaOld" };
        let refused = drive_list(&http, &wrong_key, "root1".into(), None).await.unwrap_err();
        assert_eq!(refused, "Google Drive API: API key not valid. (HTTP 400)");
        // A request that fails shows no URL, which holds the key.
        let closed = DriveSource::Api { base: "http://127.0.0.1:1/drive/v3/", key: "AIzaKey" };
        let unreachable = drive_list(&http, &closed, "root1".into(), None).await.unwrap_err();
        assert!(unreachable.starts_with("Cannot reach the Google Drive API") && !unreachable.contains("AIzaKey"), "{unreachable}");
    }

    /// folder/get_content.php as MediaFire answered it for a public folder, and for a key that
    /// names no folder (HTTP 404).
    #[test]
    fn mediafire_answers_are_read() {
        let files = br#"{"response":{"action":"folder\/get_content","asynchronous":"no","folder_content":{"chunk_size":"100","content_type":"files","chunk_number":"1","folderkey":"gtrp6u25m6nmb","files":[{"quickkey":"lrryifc0vut4jl6","hash":"59cfdccb8770ce0984e6f1da9de8508377d9a84516dfd89e03ee7d9413213d7d","filename":"Lorem ipsum.txt","description":"","size":"3771","privacy":"public","created":"2021-09-28 01:35:49","password_protected":"no","mimetype":"text\/plain","filetype":"document","view":"0","edit":"1","revision":"1029","flag":"16","permissions":{"value":"1","explicit":"0","read":"1","write":"0"},"downloads":"103","views":"0","links":{"normal_download":"https:\/\/www.mediafire.com\/file\/lrryifc0vut4jl6\/Lorem_ipsum.txt\/file"},"created_utc":"2021-09-28T06:35:49Z"}],"more_chunks":"no","revision":"2996"},"result":"Success","current_api_version":"1.5"}}"#;
        let content = mediafire_answer(200, files).unwrap().folder_content.unwrap();
        let [file] = &content.files[..] else { panic!("one file: {content:?}") };
        assert_eq!((file.quickkey.as_str(), file.filename.as_str(), file.size.as_str()), ("lrryifc0vut4jl6", "Lorem ipsum.txt", "3771"));
        assert_eq!((file.hash.len(), file.password_protected.as_str(), content.more_chunks.as_str()), (64, "no", "no"));
        let folders = br#"{"response":{"action":"folder\/get_content","asynchronous":"no","folder_content":{"chunk_size":"100","content_type":"folders","chunk_number":"1","folderkey":"gtrp6u25m6nmb","folders":[{"folderkey":"34gxd4kmqz5nn","name":"InnerFolder","description":"","tags":"","privacy":"public","created":"2022-10-29 13:54:16","revision":"1502","flag":"0","permissions":{"value":"1","explicit":"0","read":"1","write":"0"},"file_count":"0","folder_count":"0","dropbox_enabled":"no","created_utc":"2022-10-29T18:54:16Z"}],"more_chunks":"no","revision":"2996"},"result":"Success","current_api_version":"1.5"}}"#;
        let content = mediafire_answer(200, folders).unwrap().folder_content.unwrap();
        assert_eq!(content.folders.iter().map(|f| (f.folderkey.as_str(), f.name.as_str())).collect::<Vec<_>>(), [("34gxd4kmqz5nn", "InnerFolder")]);
        let unknown = br#"{"response":{"action":"folder\/get_content","message":"Unknown or invalid FolderKey","error":112,"result":"Error","current_api_version":"1.5"}}"#;
        assert_eq!(
            mediafire_answer(404, unknown).unwrap_err(),
            "MediaFire cannot list the folder: Unknown or invalid FolderKey (it may be private or deleted)"
        );
        assert_eq!(mediafire_answer(503, b"<html>busy</html>").unwrap_err(), "MediaFire answered HTTP 503 instead of listing the folder");
    }
}
