//! File-sharing hosts (Pixeldrain, Gofile, OneDrive and SharePoint, Terabox): their file links
//! resolved to the file, their folders and lists read into one task per file.
//!
//! Gofile's download servers, and OneDrive's and SharePoint's, want a cookie that Gofile's API or
//! the share link itself hands out: [`with_cookie`] adds it to the engine's requests to that host.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, RANGE, USER_AGENT};
use reqwest::RequestBuilder;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::ingest::{clean_path, Task};
use crate::resolver::ResolverError;

const PIXELDRAIN: &[&str] = &["pixeldrain.com", "pixeldrain.net", "pixeldra.in"];

const TERABOX: &[&str] = &[
    "terabox.com", "terabox.app", "terabox.fun", "teraboxapp.com", "teraboxlink.com", "teraboxshare.com",
    "terafileshare.com", "terasharelink.com", "1024tera.com", "1024tera.co", "1024terabox.com", "4funbox.com",
    "4funbox.co", "mirrobox.com", "nephobox.com", "freeterabox.com", "momerybox.com", "tibibox.com",
];

const GOFILE_API: &str = "https://api.gofile.io";
/// What Gofile is told the browser is: its website token is made of them (see [`website_token`]).
const GOFILE_AGENT: &str = "Mozilla/5.0";
const GOFILE_LANGUAGE: &str = "en-US";
/// The key in Gofile's `wt.obf.js` (read 2026-10). ponytail: hardcoded; when Gofile changes it,
/// listings fail with "error-notPremium" until this is updated.
const GOFILE_WT_KEY: &str = "12af056dacea0b";

/// Most folders one Gofile listing reads.
const MAX_FOLDERS: usize = 200;

const MICROSOFT_FOLDER: &str = "OneDrive and SharePoint folders cannot be listed without signing in: open the folder in your browser and download it there, or add the links of its files";
const MICROSOFT_REFUSED: &str = "OneDrive or SharePoint did not hand over the file: the link may be private, expired, or a folder's";
const TERABOX_LOGIN: &str = "Terabox hands files only to a signed-in account: export your Terabox cookies to a cookies.txt file (a browser extension does this), set it as the cookies file (--load-cookies), and try again";
const LISTS_MANY: &str = "this link lists several files: add it as a new download to get one download per file";

/// Steam's public Web API, whose GetPublishedFileDetails wants no key.
const STEAM_API: &str = "https://api.steampowered.com";
/// Where a public channel's posts are shown to anyone, without Telegram's app.
const TELEGRAM: &str = "https://t.me";
const TELEGRAM_NOTHING: &str = "the Telegram post shows no photo or video on its public page: files and private channels are only in Telegram's app";
const STEAM_CLIENT_ONLY: &str ="this Steam Workshop item comes only through the Steam client, with its game: subscribe to it in Steam (only items stored the older way have a file on the web)";

/// What a share link is, from its shape.
#[derive(Debug, PartialEq)]
enum Share {
    /// A Pixeldrain file (/u/{id}) or list (/l/{id}).
    PixeldrainFile(String),
    PixeldrainList(String),
    /// A Gofile page (/d/{id}): always a folder, even for one file.
    GofileFolder(String),
    /// A file on one of Gofile's download servers, as a Gofile listing gives them.
    GofileFile,
    /// A OneDrive or SharePoint share link.
    Microsoft { folder: bool },
    /// A Terabox share, by its short id.
    Terabox(String),
    /// A Steam Workshop item (steamcommunity.com/sharedfiles/filedetails/?id={id}).
    SteamItem(String),
    /// A public Telegram channel's post (t.me/[s/]{channel}/{post}).
    TelegramPost { channel: String, post: u64 },
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

fn share_of(url: &Url) -> Option<Share> {
    let mut segs: Vec<&str> = url.path_segments()?.collect();
    // `/d/abc/` is `/d/abc`.
    if segs.len() > 1 && segs.last() == Some(&"") {
        segs.pop();
    }
    let id = |s: &str| is_id(s).then(|| s.to_string());
    let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    let host = url.host_str()?.trim_end_matches('.');
    if on(url, PIXELDRAIN) {
        return match segs[..] {
            ["u", file, ..] => id(file).map(Share::PixeldrainFile),
            ["l", list, ..] => id(list).map(Share::PixeldrainList),
            _ => None,
        };
    }
    if on(url, &["gofile.io"]) {
        return match segs[..] {
            ["d", folder] if matches!(host, "gofile.io" | "www.gofile.io") => id(folder).map(Share::GofileFolder),
            ["download", ..] if host.ends_with(".gofile.io") && !matches!(host, "www.gofile.io" | "api.gofile.io") => {
                Some(Share::GofileFile)
            }
            _ => None,
        };
    }
    if on(url, TERABOX) {
        let short = match segs[..] {
            ["s", short] => short.strip_prefix('1').and_then(id),
            ["sharing", "link"] | ["wap", "share", "filelist"] => param("surl").as_deref().and_then(id),
            _ => None,
        };
        return short.map(Share::Terabox);
    }
    if matches!(host, "t.me" | "telegram.me") {
        // A channel's username: 4 to 32 letters, digits and underscores ("s" is the preview's, "c" a private channel's).
        let channel = |s: &str| (4..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        return match segs[..] {
            ["s", name, post] | [name, post] if channel(name) => post.parse().ok().map(|post| Share::TelegramPost { channel: name.to_string(), post }),
            _ => None,
        };
    }
    if on(url, &["steamcommunity.com"]) {
        return match segs[..] {
            ["sharedfiles" | "workshop", "filedetails"] => param("id").filter(|id| id.bytes().all(|b| b.is_ascii_digit()) && !id.is_empty()).map(Share::SteamItem),
            _ => None,
        };
    }
    // The kind a share link names: f a folder; u any file; b w x p i t a PDF, Word, Excel,
    // PowerPoint, image or text file; v a video.
    let kind = if host == "1drv.ms" {
        segs.first().copied()?.to_string()
    } else if host == "onedrive.live.com" || host.ends_with(".sharepoint.com") {
        match segs.first()?.strip_prefix(':').and_then(|k| k.strip_suffix(':')) {
            Some(kind) => kind.to_string(),
            // Older OneDrive links (?cid=&id=, /redir?resid=): the answer tells.
            None if host == "onedrive.live.com" && param("ithint").is_some_and(|h| h.contains("folder")) => "f".into(),
            None if host == "onedrive.live.com" => "u".into(),
            None => return None,
        }
    } else {
        return None;
    };
    match kind.as_str() {
        "f" => Some(Share::Microsoft { folder: true }),
        // SharePoint's videos are yt-dlp's (see `resolver::yt_dlp_share_page`).
        "v" if host.ends_with(".sharepoint.com") => None,
        "u" | "b" | "w" | "x" | "p" | "i" | "t" | "v" => Some(Share::Microsoft { folder: false }),
        _ => None,
    }
}

/// Whether `url` is a folder or list link [`list`] reads, a Steam Workshop item (its file is
/// named only in Steam's answer) or a Telegram post (photos and videos).
pub fn lists(url: &Url) -> bool {
    matches!(
        share_of(url),
        Some(Share::PixeldrainList(_) | Share::GofileFolder(_) | Share::Microsoft { folder: true } | Share::SteamItem(_) | Share::TelegramPost { .. })
    )
}

/// One task per file the share at `url` holds, in a folder named after it; a Gofile folder
/// opened with `password` if it has one.
pub async fn list(http: &reqwest::Client, url: &Url, password: Option<&str>) -> Result<Vec<Task>, String> {
    let tasks = match share_of(url) {
        Some(Share::PixeldrainList(id)) => pixeldrain_list(http, &url.origin().ascii_serialization(), &id).await?,
        Some(Share::GofileFolder(id)) => gofile_list(http, GOFILE_API, &id, password).await?,
        Some(Share::Microsoft { folder: true }) => return Err(MICROSOFT_FOLDER.to_string()),
        Some(Share::SteamItem(id)) => vec![steam_item(http, STEAM_API, &id).await?],
        Some(Share::TelegramPost { channel, post }) => telegram_post(http, TELEGRAM, &channel, post).await?,
        _ => return Err(format!("{} is not a folder or list link", url)),
    };
    if tasks.is_empty() {
        return Err("the share holds no files the app can download".to_string());
    }
    Ok(tasks)
}

/// Whether `url` is a share link [`resolve`] takes (a folder's says it must be listed).
pub fn handles(url: &Url) -> bool {
    share_of(url).is_some()
}

/// Where the file of the share link `url` downloads from; a client of its own goes through
/// `proxy`, as `client` does.
pub async fn resolve(client: &reqwest::Client, url: &Url, proxy: Option<&str>) -> Result<Vec<Url>, ResolverError> {
    let base = url.origin().ascii_serialization();
    let found = match share_of(url) {
        Some(Share::PixeldrainFile(id)) => pixeldrain_file(client, &base, &id).await,
        Some(Share::GofileFile) => gofile_token(client, GOFILE_API).await.map(|_| url.clone()),
        Some(Share::Microsoft { folder: false }) => microsoft_file(url, proxy).await.map(|(file, cookies)| {
            if let (Some(host), Some(cookies)) = (file.host_str(), cookies) {
                remember(host, cookies);
            }
            file
        }),
        Some(Share::Terabox(short)) => terabox_file(client, &base, &short).await,
        Some(Share::SteamItem(id)) => steam_item(client, STEAM_API, &id).await.map(|task| task.urls[0].clone()),
        Some(Share::Microsoft { folder: true }) => Err(MICROSOFT_FOLDER.to_string()),
        Some(_) => Err(LISTS_MANY.to_string()),
        None => Err(format!("{} is not a share link", url)),
    };
    found.map(|file| vec![file]).map_err(ResolverError::NotFound)
}

/// Cookies a share's downloads go with, by the domain they are for (its subdomains included):
/// Gofile's guest account token, what a OneDrive or SharePoint share link set.
static COOKIES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

fn remember(domain: &str, cookie: String) {
    let mut cookies = COOKIES.lock().unwrap_or_else(|e| e.into_inner());
    cookies.retain(|(d, _)| d != domain);
    cookies.push((domain.to_string(), cookie));
}

/// `request` for `url` with the cookie a share's resolution left for its host, if any (HTTPS only).
pub(crate) fn with_cookie(request: RequestBuilder, url: &Url) -> RequestBuilder {
    if url.scheme() != "https" {
        return request;
    }
    let cookies = COOKIES.lock().unwrap_or_else(|e| e.into_inner());
    match cookies.iter().find(|(domain, _)| on(url, &[domain.as_str()])) {
        Some((_, cookie)) => request.header(COOKIE, cookie.as_str()),
        None => request,
    }
}

/// A file or folder name made safe as one path component, else its id.
fn component(name: &str, id: &str) -> Result<PathBuf, String> {
    clean_path([name]).or_else(|_| clean_path([id]))
}

/// The JSON `request` answers with, read as `T`; `service` names the host in errors, which leave
/// out the URL (it may hold a password's hash).
async fn get_json<T: DeserializeOwned>(request: RequestBuilder, service: &str) -> Result<T, String> {
    let answer = request.send().await.map_err(|e| format!("Cannot reach {}: {}", service, e.without_url()))?;
    let status = answer.status();
    let body = answer.bytes().await.map_err(|e| format!("Cannot reach {}: {}", service, e.without_url()))?;
    serde_json::from_slice(&body).map_err(|_| format!("{} answered HTTP {} with something the app cannot read", service, status))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PixeldrainFile {
    success: bool,
    message: String,
    id: String,
    name: String,
    size: Option<u64>,
    hash_sha256: String,
    /// Why it cannot be downloaded right now (a captcha wanted), when it cannot.
    availability: String,
    availability_message: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PixeldrainList {
    success: bool,
    message: String,
    title: String,
    files: Vec<PixeldrainFile>,
}

/// The download of Pixeldrain's file `id` (at `base`, the link's own scheme and host), once its
/// info says it may be downloaded now.
async fn pixeldrain_file(http: &reqwest::Client, base: &str, id: &str) -> Result<Url, String> {
    let info: PixeldrainFile = get_json(http.get(format!("{}/api/file/{}/info", base, id)), "Pixeldrain").await?;
    if !info.success {
        return Err(format!("Pixeldrain: {}", info.message));
    }
    if !info.availability.is_empty() {
        let why = if info.availability_message.is_empty() { &info.availability } else { &info.availability_message };
        return Err(format!("Pixeldrain wants a captcha solved before this download ({}): open the link in your browser, or try again later", why));
    }
    Url::parse(&format!("{}/api/file/{}?download", base, id)).map_err(|e| e.to_string())
}

/// Every file of Pixeldrain's list `id`, each a task for its /u/ link (which [`pixeldrain_file`]
/// checks), with its size and SHA-256.
async fn pixeldrain_list(http: &reqwest::Client, base: &str, id: &str) -> Result<Vec<Task>, String> {
    let list: PixeldrainList = get_json(http.get(format!("{}/api/list/{}", base, id)), "Pixeldrain").await?;
    if !list.success {
        return Err(format!("Pixeldrain: {}", list.message));
    }
    let folder = component(&list.title, id)?;
    list.files
        .iter()
        .filter(|file| is_id(&file.id))
        .map(|file| {
            Ok(Task {
                urls: vec![Url::parse(&format!("{}/u/{}", base, file.id)).map_err(|e| e.to_string())?],
                name: Some(component(&file.name, &file.id)?),
                folder: Some(folder.clone()),
                size: file.size,
                checksum: Some(format!("sha256:{}", file.hash_sha256)).filter(|c| crate::storage::validate_checksum(c).is_ok()),
                from_document: true,
                ..Task::default()
            })
        })
        .collect()
}

/// The guest account token Gofile's API and download servers want, made once per run; its cookie
/// goes with the downloads (see [`with_cookie`]).
static GOFILE_TOKEN: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

async fn gofile_token(http: &reqwest::Client, api: &str) -> Result<String, String> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Account {
        status: String,
        data: AccountData,
    }
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct AccountData {
        token: String,
    }
    let token = GOFILE_TOKEN
        .get_or_try_init(|| async {
            let request = http.post(format!("{}/accounts", api)).header(USER_AGENT, GOFILE_AGENT).header(CONTENT_TYPE, "application/json").body("{}");
            let account: Account = get_json(request, "Gofile").await?;
            match account.data.token {
                token if account.status == "ok" && token.bytes().all(|b| b.is_ascii_alphanumeric()) && !token.is_empty() => Ok(token),
                _ => Err(format!("Gofile did not make a guest account ({})", account.status)),
            }
        })
        .await?
        .clone();
    remember("gofile.io", format!("accountToken={}", token));
    Ok(token)
}

/// The X-Website-Token Gofile's site sends with `token` at `now` (seconds since the epoch): the
/// SHA-256 of its user agent, language, the token, the 4-hour period and a key (its `generateWT`).
fn website_token(token: &str, now: u64) -> String {
    let text = format!("{}::{}::{}::{}::{}", GOFILE_AGENT, GOFILE_LANGUAGE, token, now / 14400, GOFILE_WT_KEY);
    format!("{:x}", Sha256::digest(text))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GofileAnswer {
    status: String,
    data: GofileItem,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GofileItem {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    name: String,
    size: Option<u64>,
    link: Option<String>,
    md5: String,
    can_access: Option<bool>,
    password_status: Option<String>,
    children: HashMap<String, GofileItem>,
}

/// Gofile's folder `id`, its children with it, opened with `password` if given.
async fn gofile_contents(http: &reqwest::Client, api: &str, token: &str, id: &str, password: Option<&str>) -> Result<GofileItem, String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut url = format!("{}/contents/{}?page=1&pageSize=1000&sortField=name&sortDirection=1", api, id);
    // Gofile's site sends the password's SHA-256, in hex (`get_json` errors never show the URL).
    if let Some(password) = password {
        url += &format!("&password={:x}", Sha256::digest(password));
    }
    let request = http
        .get(url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .header("X-Website-Token", website_token(token, now))
        .header("X-BL", GOFILE_LANGUAGE)
        .header(USER_AGENT, GOFILE_AGENT);
    let answer: GofileAnswer = get_json(request, "Gofile").await?;
    let locked = match password {
        None => "the Gofile folder is protected by a password: add its link again with the password on the next line (Password: ...)",
        Some(_) => "the Gofile folder's password is wrong",
    };
    match answer.status.as_str() {
        "ok" if answer.data.password_status.as_deref().is_some_and(|s| s != "passwordOk") => Err(locked.to_string()),
        "ok" => Ok(answer.data),
        "error-passwordRequired" => Err(locked.to_string()),
        "error-notFound" => Err("the Gofile folder does not exist or was deleted".to_string()),
        "error-rateLimit" => Err("Gofile is limiting requests: try again in a few minutes".to_string()),
        "error-notPremium" => Err("Gofile refused the listing (its site changed: the app needs an update)".to_string()),
        other => Err(format!("Gofile answered {}", other)),
    }
}

/// Every file under Gofile's folder `id` (subfolders kept), each a task for its download link.
/// ponytail: one page of 1000 children per folder; page on when someone shares more.
async fn gofile_list(http: &reqwest::Client, api: &str, id: &str, password: Option<&str>) -> Result<Vec<Task>, String> {
    let token = gofile_token(http, api).await?;
    let mut queue = VecDeque::from([(id.to_string(), None::<PathBuf>)]);
    let (mut tasks, mut listed) = (Vec::new(), HashSet::new());
    while let Some((id, folder)) = queue.pop_front() {
        if !listed.insert(id.clone()) {
            continue;
        }
        if listed.len() > MAX_FOLDERS {
            return Err(format!("the Gofile folder holds more than {} folders: add a subfolder's link instead", MAX_FOLDERS));
        }
        let item = gofile_contents(http, api, &token, &id, password).await?;
        let folder = match folder {
            Some(folder) => folder,
            None => component(&item.name, &id)?,
        };
        let mut children: Vec<_> = item.children.into_values().filter(|c| c.can_access != Some(false)).collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        for child in children {
            match (child.kind.as_str(), &child.link) {
                ("folder", _) if is_id(&child.id) => {
                    queue.push_back((child.id.clone(), Some(folder.join(component(&child.name, &child.id)?))));
                }
                ("file", Some(link)) => tasks.push(Task {
                    urls: vec![Url::parse(link).map_err(|e| format!("Gofile gave a bad link: {}", e))?],
                    name: Some(component(&child.name, &child.id)?),
                    folder: Some(folder.clone()),
                    size: child.size,
                    checksum: Some(format!("md5:{}", child.md5)).filter(|c| crate::storage::validate_checksum(c).is_ok()),
                    from_document: true,
                    ..Task::default()
                }),
                _ => {}
            }
        }
    }
    Ok(tasks)
}

/// Where the OneDrive or SharePoint share link `link` hands over its file (`download=1`), and the
/// cookies set on the way, which the download needs.
/// Its own client, through `proxy`, for a cookie jar that sees the redirect's cookies.
async fn microsoft_file(link: &Url, proxy: Option<&str>) -> Result<(Url, Option<String>), String> {
    let mut link = link.clone();
    let kept: Vec<(String, String)> = link.query_pairs().filter(|(k, _)| k != "download").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    link.query_pairs_mut().clear().extend_pairs(kept).append_pair("download", "1");
    let jar = Arc::new(reqwest::cookie::Jar::default());
    let mut http = reqwest::Client::builder().cookie_provider(Arc::clone(&jar)).connect_timeout(Duration::from_secs(15)).timeout(Duration::from_secs(60));
    if let Some(proxy) = proxy {
        // Its error would name the proxy, password and all.
        http = http.proxy(reqwest::Proxy::all(proxy).map_err(|_| "The proxy setting is not valid".to_string())?);
    }
    let http = http.build().map_err(|e| e.to_string())?;
    let answer = http.get(link).header(RANGE, "bytes=0-0").send().await.map_err(|e| format!("Cannot reach OneDrive: {}", e))?;
    let page = answer.headers().get(CONTENT_TYPE).and_then(|t| t.to_str().ok()).is_some_and(|t| t.contains("html"));
    if !answer.status().is_success() || page {
        return Err(MICROSOFT_REFUSED.to_string());
    }
    let file = answer.url().clone();
    let cookies = reqwest::cookie::CookieStore::cookies(&*jar, &file).and_then(|c| c.to_str().ok().map(str::to_string));
    Ok((file, cookies))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TeraboxList {
    errno: i64,
    list: Vec<TeraboxItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TeraboxItem {
    /// 0 or 1, as a number or a string.
    isdir: serde_json::Value,
    dlink: String,
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let rest = &s[s.find(start)? + start.len()..];
    Some(&rest[..rest.find(end)?])
}

/// The download link of the one file of Terabox's share `short` (at `base`, the link's scheme and
/// host), which Terabox gives only to a signed-in account: `client` is the engine's, with the
/// user's cookies file. ponytail: single-file shares only; a multi-file share needs `list` to get
/// the user's cookies.
async fn terabox_file(client: &reqwest::Client, base: &str, short: &str) -> Result<Url, String> {
    let page = client.get(format!("{}/sharing/link?surl={}", base, short)).send().await.map_err(|e| format!("Cannot reach Terabox: {}", e))?;
    // Where the share page landed (a share domain sends on to Terabox's own).
    let host = page.url().origin().ascii_serialization();
    let html = page.text().await.map_err(|e| format!("Cannot reach Terabox: {}", e))?;
    let js_token = between(&html, "fn%28%22", "%22%29").ok_or(TERABOX_LOGIN)?;
    let mut url = Url::parse(&format!("{}/share/list", host)).map_err(|e| e.to_string())?;
    url.query_pairs_mut().extend_pairs([
        ("app_id", "250528"),
        ("web", "1"),
        ("channel", "dubox"),
        ("clienttype", "0"),
        ("jsToken", js_token),
        ("dp-logid", between(&html, "dp-logid=", "&").unwrap_or_default()),
        ("page", "1"),
        ("num", "100"),
        ("by", "name"),
        ("order", "asc"),
        ("shorturl", short),
        ("root", "1"),
    ]);
    let share: TeraboxList = get_json(client.get(url), "Terabox").await?;
    if share.errno != 0 {
        return Err(format!("Terabox did not list the share (error {}). {}", share.errno, TERABOX_LOGIN));
    }
    match &share.list[..] {
        [] => Err("the Terabox share is empty".to_string()),
        [file] if !(file.isdir == 1 || file.isdir == "1") => match Url::parse(&file.dlink) {
            Ok(link) => Ok(link),
            Err(_) => Err(TERABOX_LOGIN.to_string()),
        },
        items => Err(format!(
            "the Terabox share holds {} files or folders: only shares of one file can be downloaded for now",
            items.len()
        )),
    }
}

/// The photos and videos of post `post` of the public Telegram channel `channel`, as its public
/// page at `base` shows them (an album's all), named after the post.
async fn telegram_post(http: &reqwest::Client, base: &str, channel: &str, post: u64) -> Result<Vec<Task>, String> {
    let page = http.get(format!("{base}/{channel}/{post}?embed=1&mode=tme")).send().await.and_then(|a| a.error_for_status());
    let html = page.map_err(|e| format!("Cannot reach Telegram: {}", e.without_url()))?.text().await.map_err(|e| format!("Cannot reach Telegram: {}", e.without_url()))?;
    // A photo is a background image of its frame; a video, the source of its player.
    let photos = html.split("tgme_widget_message_photo_wrap").skip(1).filter_map(|s| between(s.split('>').next()?, "background-image:url('", "')"));
    let videos = html.split("<video").skip(1).filter_map(|s| between(s.split('>').next()?, "src=\"", "\""));
    let mut tasks: Vec<Task> = Vec::new();
    for found in photos.chain(videos) {
        let Ok(url) = Url::parse(&found.replace("&amp;", "&")) else { continue };
        if url.scheme() != "https" || tasks.iter().any(|t| t.urls[0] == url) {
            continue;
        }
        let ext = url.path().rsplit_once('.').map(|(_, e)| e).filter(|e| e.len() <= 4 && e.bytes().all(|b| b.is_ascii_alphanumeric())).unwrap_or("jpg");
        let name = component(&format!("{channel}-{post}-{}.{ext}", tasks.len() + 1), "telegram")?;
        tasks.push(Task { urls: vec![url], name: Some(name), from_document: true, ..Task::default() });
    }
    if tasks.is_empty() {
        return Err(TELEGRAM_NOTHING.to_string());
    }
    Ok(tasks)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SteamAnswer {
    response: SteamItems,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SteamItems {
    publishedfiledetails: Vec<SteamItem>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SteamItem {
    /// 1 when the item exists and is public.
    result: u32,
    filename: String,
    /// A number in a string.
    file_size: serde_json::Value,
    file_url: String,
}

/// The file of Steam Workshop item `id`, named and sized as Steam's public Web API at `api` gives
/// it. Only an item stored the older way has a file there; a newer one comes only through the
/// Steam client.
async fn steam_item(http: &reqwest::Client, api: &str, id: &str) -> Result<Task, String> {
    let form = [("itemcount", "1"), ("publishedfileids[0]", id)];
    let answer: SteamAnswer = get_json(http.post(format!("{api}/ISteamRemoteStorage/GetPublishedFileDetails/v1/")).form(&form), "Steam").await?;
    let item = answer.response.publishedfiledetails.into_iter().next().unwrap_or_default();
    if item.result != 1 {
        return Err(format!("Steam has no public Workshop item {id}: it may be private, hidden or removed"));
    }
    let url = Url::parse(&item.file_url).map_err(|_| STEAM_CLIENT_ONLY.to_string())?;
    let name = item.filename.rsplit('/').next().unwrap_or_default();
    Ok(Task {
        urls: vec![url],
        name: Some(component(name, &format!("{id}.bin"))?),
        size: item.file_size.as_str().and_then(|s| s.parse().ok()).or_else(|| item.file_size.as_u64()),
        from_document: true,
        ..Task::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn share(link: &str) -> Option<Share> {
        share_of(&Url::parse(link).unwrap())
    }

    #[test]
    fn share_links_are_told_by_their_shape() {
        assert_eq!(share("https://pixeldrain.com/u/SB1nGZJQ"), Some(Share::PixeldrainFile("SB1nGZJQ".into())));
        assert_eq!(share("https://pixeldra.in/u/abc?embed"), Some(Share::PixeldrainFile("abc".into())));
        assert_eq!(share("https://pixeldrain.net/l/ezRuCtHM#item=2"), Some(Share::PixeldrainList("ezRuCtHM".into())));
        assert_eq!(share("https://gofile.io/d/AbC123"), Some(Share::GofileFolder("AbC123".into())));
        assert_eq!(share("https://gofile.io/d/AbC123/"), Some(Share::GofileFolder("AbC123".into())));
        assert_eq!(share("https://store-eu-par-3.gofile.io/download/web/1-2/a%20b.zip"), Some(Share::GofileFile));
        assert_eq!(share("https://www.terabox.com/s/1AbC_d-e"), Some(Share::Terabox("AbC_d-e".into())));
        assert_eq!(share("https://1024terabox.com/sharing/link?surl=xyz"), Some(Share::Terabox("xyz".into())));
        let (file, folder) = (Some(Share::Microsoft { folder: false }), Some(Share::Microsoft { folder: true }));
        for link in [
            "https://1drv.ms/u/s!AhMqVPD44cDOhkPsOU2S_HFpY9dC",
            "https://1drv.ms/x/c/2d90e71fb9eb254f/EnMm8c2mP?e=x",
            "https://1drv.ms/v/s!abc",
            "https://onedrive.live.com/:u:/g/personal/CEC0E1F8F0542A13/s!AhMq?resid=x",
            "https://onedrive.live.com/?cid=abc&id=def",
            "https://contoso-my.sharepoint.com/:u:/g/personal/user_contoso_com/EZLHzG70?e=fxDPVK",
            "https://contoso.sharepoint.com/:x:/s/team/Eabc",
        ] {
            assert_eq!(share(link), file, "{link}");
        }
        for link in [
            "https://1drv.ms/f/s!AhIXJn_J-blW231MH2krnmLq5kkQ",
            "https://onedrive.live.com/:f:/g/personal/56B9/s!AhIX?ithint=folder",
            "https://onedrive.live.com/?cid=abc&id=def&ithint=folder,",
            "https://contoso-my.sharepoint.com/:f:/g/personal/user_contoso_com/Es-M92IX?e=Bs5ROw",
        ] {
            assert_eq!(share(link), folder, "{link}");
        }
        for other in [
            "https://pixeldrain.com/api/file/abc?download",
            "https://pixeldrain.com/u/",
            "https://gofile.io/",
            "https://api.gofile.io/download/x",
            "https://notgofile.io/d/abc",
            "https://www.terabox.com/main",
            "https://www.terabox.com/s/AbC",
            // SharePoint videos go to yt-dlp; a site's own pages are no shares.
            "https://contoso.sharepoint.com/:v:/g/personal/user/Eabc",
            "https://contoso.sharepoint.com/sites/team/Shared%20Documents/report.docx",
            "https://example.com/u/abc",
        ] {
            assert_eq!(share(other), None, "{other}");
        }
        assert!(lists(&Url::parse("https://gofile.io/d/abc").unwrap()) && !handles(&Url::parse("https://example.com/d/abc").unwrap()));
    }

    /// Gofile's token, from its own script: run with this user agent and language at 124385 periods.
    #[test]
    fn the_gofile_website_token_is_its_scripts() {
        assert_eq!(website_token("TOKEN", 124385 * 14400 + 5), "0b34c6cf70b4f706e1ef27541f233ef2ff246921e899e1f6fffa720382d01a7a");
    }

    /// A cookie left for a domain goes to it and its subdomains over HTTPS only.
    #[test]
    fn share_cookies_go_only_to_their_hosts() {
        remember("cookies.test", "a=1".into());
        remember("cookies.test", "a=2".into());
        let cookie = |link: &str| {
            let url = Url::parse(link).unwrap();
            let request = with_cookie(reqwest::Client::new().get(url.clone()), &url).build().unwrap();
            request.headers().get(COOKIE).map(|c| c.to_str().unwrap().to_string())
        };
        assert_eq!(cookie("https://cookies.test/x").as_deref(), Some("a=2"));
        assert_eq!(cookie("https://store-1.cookies.test/download/x").as_deref(), Some("a=2"));
        assert_eq!(cookie("http://cookies.test/x"), None);
        assert_eq!(cookie("https://evilcookies.test/x"), None);
        assert_eq!(cookie("https://cookies.test.evil/x"), None);
    }

    /// Every request head seen.
    type Seen = Arc<Mutex<Vec<String>>>;

    /// A local server at the base URL returned, answering each request with `answer(head)`:
    /// status, extra header lines, body.
    async fn serve(answer: fn(&str) -> (u16, String, String)) -> (String, Seen) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
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
                log.lock().unwrap().push(head.clone());
                let (status, headers, body) = answer(&head);
                let reply = format!("HTTP/1.1 {status} X\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (base, seen)
    }

    fn target(head: &str) -> &str {
        head.split(' ').nth(1).unwrap_or_default()
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    const JSON: &str = "Content-Type: application/json\r\n";

    fn pixeldrain(head: &str) -> (u16, String, String) {
        let body = match target(head) {
            "/api/file/ok1/info" => r#"{"success":true,"id":"ok1","name":"a.7z","availability":""}"#,
            "/api/file/busy/info" => {
                r#"{"success":true,"id":"busy","availability":"file_rate_limited_captcha_required","availability_message":"This file is using too much bandwidth"}"#
            }
            "/api/list/L1" => {
                r#"{"success":true,"title":"My: list","files":[
                    {"id":"f1","name":"a.jpg","size":5,"hash_sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"},
                    {"id":"f2","name":"..","size":7,"hash_sha256":""},
                    {"id":"../x","name":"b.jpg"}]}"#
            }
            _ => return (404, JSON.into(), r#"{"success":false,"value":"not_found","message":"The entity you requested could not be found"}"#.into()),
        };
        (200, JSON.into(), body.into())
    }

    /// A file is downloaded from the API once its info allows it; a captcha wanted or a missing
    /// file is said so. A list is one task per file, under the list's title.
    #[tokio::test]
    async fn pixeldrain_files_and_lists() {
        let (base, _) = serve(pixeldrain).await;
        let http = http();
        assert_eq!(pixeldrain_file(&http, &base, "ok1").await.unwrap().as_str(), format!("{base}/api/file/ok1?download"));
        let busy = pixeldrain_file(&http, &base, "busy").await.unwrap_err();
        assert!(busy.contains("captcha") && busy.contains("too much bandwidth"), "{busy}");
        assert!(pixeldrain_file(&http, &base, "gone").await.unwrap_err().contains("could not be found"));
        let tasks = pixeldrain_list(&http, &base, "L1").await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap(), t.size, t.checksum.clone())).collect();
        let sha = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string();
        assert_eq!(
            listed,
            [(format!("{base}/u/f1"), "a.jpg".into(), Some(5), Some(sha)), (format!("{base}/u/f2"), "f2".into(), Some(7), None)]
        );
        assert!(tasks.iter().all(|t| t.folder == Some("My_ list".into()) && t.from_document));
    }

    fn gofile(head: &str) -> (u16, String, String) {
        let (method, path) = (head.split(' ').next().unwrap_or_default(), target(head).split('?').next().unwrap_or_default());
        if method == "POST" && path == "/accounts" {
            return (200, JSON.into(), r#"{"status":"ok","data":{"token":"guestTok1","tier":"guest"}}"#.into());
        }
        let lower = head.to_ascii_lowercase();
        let wt = lower.lines().find_map(|l| l.strip_prefix("x-website-token: ")).unwrap_or_default();
        if !lower.contains("authorization: bearer guesttok1") || wt.len() != 64 {
            return (401, JSON.into(), r#"{"status":"error-notPremium","data":{}}"#.into());
        }
        let body = match path {
            "/contents/root" => {
                r#"{"status":"ok","data":{"type":"folder","id":"root","name":"Pack","children":{
                    "c1":{"type":"file","id":"c1","name":"b.bin","size":3,"link":"https://store1.gofile.io/download/web/c1/b.bin","md5":"5d41402abc4b2a76b9719d911017c592"},
                    "c2":{"type":"folder","id":"sub","name":"Sub/dir"},
                    "c3":{"type":"file","id":"c3","name":"a.bin","size":1,"link":"https://store1.gofile.io/download/web/c3/a.bin"},
                    "c4":{"type":"file","id":"c4","name":"premium.bin","canAccess":false,"link":"https://store1.gofile.io/download/web/c4/p.bin"}}}}"#
            }
            "/contents/sub" => {
                r#"{"status":"ok","data":{"type":"folder","id":"sub","name":"Sub/dir","children":{
                    "c5":{"type":"file","id":"c5","name":"c.bin","link":"https://store2.gofile.io/download/web/c5/c.bin"},
                    "c6":{"type":"folder","id":"root","name":"Loop back"}}}}"#
            }
            "/contents/locked" => match target(head).split_once("&password=") {
                Some((_, hash)) if hash == format!("{:x}", Sha256::digest("s3cret")) => {
                    r#"{"status":"ok","data":{"type":"folder","id":"locked","name":"Locked","passwordStatus":"passwordOk","children":{
                        "c7":{"type":"file","id":"c7","name":"l.bin","link":"https://store3.gofile.io/download/web/c7/l.bin"}}}}"#
                }
                Some(_) => r#"{"status":"ok","data":{"type":"folder","id":"locked","passwordStatus":"passwordWrong"}}"#,
                None => r#"{"status":"ok","data":{"type":"folder","id":"locked","passwordStatus":"passwordRequired"}}"#,
            },
            "/contents/busy" => r#"{"status":"error-rateLimit","data":{}}"#,
            _ => r#"{"status":"error-notFound","data":{}}"#,
        };
        (200, JSON.into(), body.into())
    }

    /// A folder tree is listed through the API with a guest account's token and the website
    /// token, each folder once, subfolders kept, files the account cannot get left out; the
    /// token's cookie then goes with the downloads. Its errors are said plainly.
    #[tokio::test]
    async fn gofile_folders_are_listed_with_a_guest_account() {
        let (api, seen) = serve(gofile).await;
        let http = http();
        let tasks = gofile_list(&http, &api, "root", None).await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.folder.clone().unwrap(), t.name.clone().unwrap(), t.urls[0].to_string())).collect();
        assert_eq!(
            listed,
            [
                ("Pack".into(), "a.bin".into(), "https://store1.gofile.io/download/web/c3/a.bin".to_string()),
                ("Pack".into(), "b.bin".into(), "https://store1.gofile.io/download/web/c1/b.bin".to_string()),
                (PathBuf::from("Pack").join("Sub_dir"), "c.bin".into(), "https://store2.gofile.io/download/web/c5/c.bin".to_string()),
            ]
        );
        assert_eq!(tasks[1].checksum.as_deref(), Some("md5:5d41402abc4b2a76b9719d911017c592"));
        assert!(tasks.iter().all(|t| t.from_document));
        // One account for the run; each folder read once.
        let heads = seen.lock().unwrap().clone();
        assert_eq!(heads.iter().filter(|h| h.starts_with("POST /accounts")).count(), 1);
        assert_eq!(heads.iter().filter(|h| h.starts_with("GET /contents/root")).count(), 1);
        let url = Url::parse("https://store9.gofile.io/download/web/x/y").unwrap();
        let download = with_cookie(http.get(url.clone()), &url).build().unwrap();
        assert_eq!(download.headers().get(COOKIE).unwrap(), "accountToken=guestTok1");
        for (id, said) in [("locked", "password"), ("busy", "try again"), ("gone", "does not exist")] {
            let err = gofile_list(&http, &api, id, None).await.unwrap_err();
            assert!(err.contains(said), "{id}: {err}");
        }
    }

    /// A folder with a password opens with its SHA-256, as Gofile's site sends it; a wrong one
    /// is said so. The password itself never goes out.
    #[tokio::test]
    async fn gofile_folders_open_with_their_password() {
        let (api, seen) = serve(gofile).await;
        let http = http();
        let tasks = gofile_list(&http, &api, "locked", Some("s3cret")).await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.name.clone().unwrap(), t.urls[0].to_string())).collect();
        assert_eq!(listed, [("l.bin".into(), "https://store3.gofile.io/download/web/c7/l.bin".to_string())]);
        assert_eq!(gofile_list(&http, &api, "locked", Some("n0pe")).await.unwrap_err(), "the Gofile folder's password is wrong");
        let none = gofile_list(&http, &api, "locked", None).await.unwrap_err();
        assert!(none.contains("Password:"), "{none}");
        assert!(!seen.lock().unwrap().iter().any(|head| head.contains("s3cret") || head.contains("n0pe")));
    }

    fn microsoft(head: &str) -> (u16, String, String) {
        match target(head) {
            "/:u:/g/personal/user/Eabc?e=x&download=1" => {
                (302, "Location: /personal/user/Documents/a.bin?ga=1\r\nSet-Cookie: FedAuth=guest1; path=/\r\n".into(), String::new())
            }
            "/personal/user/Documents/a.bin?ga=1" if head.contains("FedAuth=guest1") && head.contains("bytes=0-0") => {
                (206, "Content-Type: application/octet-stream\r\nContent-Range: bytes 0-0/3\r\n".into(), "a".into())
            }
            "/:f:/g/personal/user/Ef?download=1" => (200, "Content-Type: text/html\r\n".into(), "<html></html>".into()),
            _ => (403, String::new(), String::new()),
        }
    }

    /// A share link with download=1 sets a cookie and sends on to the file, which wants it: the
    /// file's URL and that cookie come back. A page instead of the file is refused.
    #[tokio::test]
    async fn microsoft_share_links_hand_over_the_file_and_its_cookie() {
        let (base, _) = serve(microsoft).await;
        let link = Url::parse(&format!("{base}/:u:/g/personal/user/Eabc?e=x&download=0")).unwrap();
        let (file, cookies) = microsoft_file(&link, None).await.unwrap();
        assert_eq!(file.as_str(), format!("{base}/personal/user/Documents/a.bin?ga=1"));
        assert_eq!(cookies.as_deref(), Some("FedAuth=guest1"));
        let folder = Url::parse(&format!("{base}/:f:/g/personal/user/Ef")).unwrap();
        assert_eq!(microsoft_file(&folder, None).await.unwrap_err(), MICROSOFT_REFUSED);
    }

    fn terabox(head: &str) -> (u16, String, String) {
        let signed_in = head.contains("ndus=me");
        let path = target(head);
        let body = if path.starts_with("/sharing/link?surl=") {
            r#"<script>var a = decodeURIComponent("fn%28%22JST0K%22%29"); var u = "/x?dp-logid=LOG9&y=1";</script>"#.to_string()
        } else if path.starts_with("/share/list?") && path.contains("jsToken=JST0K") && path.contains("dp-logid=LOG9") {
            match (signed_in, path.contains("shorturl=one"), path.contains("shorturl=two")) {
                (false, ..) => r#"{"errno":-6}"#.into(),
                (true, true, _) => r#"{"errno":0,"list":[{"isdir":"0","server_filename":"a.mp4","dlink":"https://d.terabox.test/file/abc"}]}"#.into(),
                (true, _, true) => r#"{"errno":0,"list":[{"isdir":0,"dlink":"https://d/1"},{"isdir":"1"}]}"#.into(),
                _ => r#"{"errno":0,"list":[]}"#.into(),
            }
        } else {
            return (404, String::new(), String::new());
        };
        (200, String::new(), body)
    }

    /// A signed-in account (the user's cookies) gets the link of a share's one file; without
    /// them, or with more files, it says what to do.
    #[tokio::test]
    async fn terabox_shares_need_the_users_cookies() {
        let (base, _) = serve(terabox).await;
        let signed_in = {
            let jar = Arc::new(reqwest::cookie::Jar::default());
            jar.add_cookie_str("ndus=me", &Url::parse(&base).unwrap());
            reqwest::Client::builder().no_proxy().cookie_provider(jar).build().unwrap()
        };
        assert_eq!(terabox_file(&signed_in, &base, "one").await.unwrap().as_str(), "https://d.terabox.test/file/abc");
        assert!(terabox_file(&signed_in, &base, "two").await.unwrap_err().contains("holds 2 files"));
        assert!(terabox_file(&signed_in, &base, "none").await.unwrap_err().contains("empty"));
        let anonymous = terabox_file(&http(), &base, "one").await.unwrap_err();
        assert!(anonymous.contains("cookies.txt") && anonymous.contains("error -6"), "{anonymous}");
    }

    fn steam(head: &str) -> (u16, String, String) {
        let form = head.to_ascii_lowercase().contains("content-type: application/x-www-form-urlencoded");
        let body = match target(head) {
            // As Steam answers, cut down.
            "/legacy/ISteamRemoteStorage/GetPublishedFileDetails/v1/" if form => r#"{"response":{"result":1,"resultcount":1,"publishedfiledetails":[{"publishedfileid":"104479831","result":1,
                "filename":"uploads/addons/stacker_stool.gm","file_size":"4197","title":"Stacker STool",
                "file_url":"https://cdn.steamusercontent.com/ugc/918994036723335461/714E79EDED6ECA83214391554E33127BF1D631C0/"}]}}"#,
            "/client/ISteamRemoteStorage/GetPublishedFileDetails/v1/" => r#"{"response":{"result":1,"resultcount":1,"publishedfiledetails":[{"publishedfileid":"104603291","result":1,"filename":"","file_size":"42347","file_url":""}]}}"#,
            _ => r#"{"response":{"result":1,"resultcount":1,"publishedfiledetails":[{"publishedfileid":"1","result":9}]}}"#,
        };
        (200, JSON.into(), body.into())
    }

    /// A Steam Workshop item stored the older way is its file, named and sized as Steam's Web API
    /// says; a newer one, and a missing or private one, say why they cannot be downloaded.
    #[tokio::test]
    async fn steam_workshop_items_stored_the_older_way_are_their_file() {
        let (base, _) = serve(steam).await;
        let task = steam_item(&http(), &format!("{base}/legacy"), "104479831").await.unwrap();
        assert_eq!(task.urls[0].as_str(), "https://cdn.steamusercontent.com/ugc/918994036723335461/714E79EDED6ECA83214391554E33127BF1D631C0/");
        assert_eq!((task.name, task.size, task.from_document), (Some(PathBuf::from("stacker_stool.gm")), Some(4197), true));
        assert_eq!(steam_item(&http(), &format!("{base}/client"), "104603291").await.unwrap_err(), STEAM_CLIENT_ONLY);
        assert!(steam_item(&http(), &format!("{base}/gone"), "1").await.unwrap_err().contains("no public Workshop item 1"));
        for link in ["https://steamcommunity.com/sharedfiles/filedetails/?id=104479831", "https://steamcommunity.com/workshop/filedetails?id=104479831&searchtext="] {
            assert_eq!(share(link), Some(Share::SteamItem("104479831".into())), "{link}");
            assert!(lists(&Url::parse(link).unwrap()) && handles(&Url::parse(link).unwrap()));
        }
        for other in ["https://steamcommunity.com/sharedfiles/filedetails/?id=x1", "https://steamcommunity.com/app/4000/workshop/", "https://steamcommunity.com/linkfilter/?u=https://a.example"] {
            assert_eq!(share(other), None, "{other}");
        }
    }

    fn telegram(head: &str) -> (u16, String, String) {
        // As a post's public page shows an album, cut down: two photos (one shown twice), a video and its thumbnail.
        const ALBUM: &str = r#"<div class="tgme_widget_message_grouped_wrap js-message_grouped_wrap">
            <a class="tgme_widget_message_photo_wrap grouped_media_wrap blured js-message_photo" style="left:0px;width:149px;background-image:url('https://cdn4.telesco.pe/file/AbC-1.jpg')" href="https://t.me/chan_1/300?single"></a>
            <a class="tgme_widget_message_photo_wrap grouped_media_wrap" style="background-image:url('https://cdn4.telesco.pe/file/AbC-1.jpg')"></a>
            <a class="tgme_widget_message_photo_wrap" style="background-image:url('http://cdn4.telesco.pe/file/plain.jpg')"></a>
            <i class="tgme_widget_message_video_thumb" style="background-image:url('https://cdn4.telesco.pe/file/thumb.jpg')"></i>
            <video src="https://cdn1.telesco.pe/file/f4a2c11d62.mp4?token=x&amp;h=1" class="tgme_widget_message_video js-message_video" playsinline></video>
            <a class="tgme_widget_message_photo_wrap" style="background-image:url('https://cdn5.telesco.pe/file/DeF_2.png')"></a></div>"#;
        match target(head) {
            "/chan_1/300?embed=1&mode=tme" => (200, "Content-Type: text/html\r\n".into(), ALBUM.into()),
            "/chan_1/301?embed=1&mode=tme" => (200, "Content-Type: text/html\r\n".into(), r#"<div class="tgme_widget_message_text">Only words</div>"#.into()),
            _ => (404, String::new(), String::new()),
        }
    }

    /// A public Telegram post is its photos and videos as its public page shows them, named after
    /// the post; one with none (words, a file) says why.
    #[tokio::test]
    async fn telegram_posts_are_their_photos_and_videos() {
        let (base, _) = serve(telegram).await;
        let tasks = telegram_post(&http(), &base, "chan_1", 300).await.unwrap();
        let listed: Vec<_> = tasks.iter().map(|t| (t.urls[0].to_string(), t.name.clone().unwrap())).collect();
        assert_eq!(
            listed,
            [
                ("https://cdn4.telesco.pe/file/AbC-1.jpg".to_string(), PathBuf::from("chan_1-300-1.jpg")),
                ("https://cdn5.telesco.pe/file/DeF_2.png".to_string(), PathBuf::from("chan_1-300-2.png")),
                ("https://cdn1.telesco.pe/file/f4a2c11d62.mp4?token=x&h=1".to_string(), PathBuf::from("chan_1-300-3.mp4")),
            ]
        );
        assert_eq!(telegram_post(&http(), &base, "chan_1", 301).await.unwrap_err(), TELEGRAM_NOTHING);
        assert!(telegram_post(&http(), &base, "chan_1", 302).await.unwrap_err().starts_with("Cannot reach Telegram"));
        let post = |channel: &str, post: u64| Some(Share::TelegramPost { channel: channel.into(), post });
        assert_eq!(share("https://t.me/durov/300"), post("durov", 300));
        assert_eq!(share("https://t.me/s/durov/300?single"), post("durov", 300));
        for other in ["https://t.me/durov", "https://t.me/c/1234567/5", "https://t.me/+AbCdEf/5", "https://t.me/joinchat/AbC", "https://t.me/s/durov"] {
            assert_eq!(share(other), None, "{other}");
        }
        assert!(lists(&Url::parse("https://t.me/durov/300").unwrap()));
    }

    /// Live: an album post of a public channel is its ten photos, and a video post its video.
    #[tokio::test]
    #[ignore]
    async fn live_telegram_posts() {
        let album = list(&http(), &Url::parse("https://t.me/durov/300").unwrap(), None).await.unwrap();
        assert_eq!(album.len(), 10, "{album:?}");
        assert!(album.iter().all(|t| t.urls[0].host_str().is_some_and(|h| h.ends_with(".telesco.pe"))), "{album:?}");
        let video = list(&http(), &Url::parse("https://t.me/telegram/300").unwrap(), None).await.unwrap();
        assert!(video.iter().any(|t| t.name.as_ref().is_some_and(|n| n.extension() == Some("mp4".as_ref()))), "{video:?}");
    }

    /// Live: a Workshop item stored the older way is its file on Steam's CDN; a newer one says it
    /// comes through the Steam client.
    #[tokio::test]
    #[ignore]
    async fn live_steam_workshop() {
        let link = Url::parse("https://steamcommunity.com/sharedfiles/filedetails/?id=104479831").unwrap();
        let [task] = &list(&http(), &link, None).await.unwrap()[..] else { panic!("one file") };
        assert_eq!((task.name.clone(), task.size), (Some(PathBuf::from("stacker_stool.gm")), Some(4197)));
        assert!(task.urls[0].as_str().starts_with("https://cdn.steamusercontent.com/ugc/"), "{task:?}");
        assert_eq!(steam_item(&http(), STEAM_API, "104603291").await.unwrap_err(), STEAM_CLIENT_ONLY);
    }

    /// Live: a public Pixeldrain file resolves to its API download.
    #[tokio::test]
    #[ignore]
    async fn live_pixeldrain_file() {
        let url = Url::parse("https://pixeldrain.com/u/SB1nGZJQ").unwrap();
        let found = resolve(&http(), &url, None).await.unwrap();
        assert_eq!(found[0].as_str(), "https://pixeldrain.com/api/file/SB1nGZJQ?download");
    }

    /// Live: Gofile makes a guest account, and its listing of a missing folder says so (which
    /// also proves the website token is still right: a wrong one is refused as notPremium).
    #[tokio::test]
    #[ignore]
    async fn live_gofile_token() {
        let err = gofile_list(&http(), GOFILE_API, "zzzzzz", None).await.unwrap_err();
        assert!(err.contains("does not exist") || err.contains("try again"), "{err}");
    }
}
