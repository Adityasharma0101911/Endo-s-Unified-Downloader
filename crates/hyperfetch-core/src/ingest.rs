//! Turns what the user enters (links, mirrors, magnet links, .metalink/.meta4/.torrent files and
//! URLs) into download tasks, the same way for every front end.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use url::Url;

use crate::{cloud, feeds, folders, media, mega, metalink, repos, resolver, shares, torrent};

/// Largest .metalink/.torrent document read from the network or the disk.
const MAX_DESCRIPTOR_BYTES: usize = 16 * 1024 * 1024;

pub const BLOB_MESSAGE: &str = "Browser-internal blob: URLs exist only in the browser's memory and cannot be downloaded by external tools. Copy the page URL from the address bar instead (e.g. https://www.youtube.com/watch?v=...).";

/// One file to download.
#[derive(Debug, Default, PartialEq)]
pub struct Task {
    /// Mirrors that all serve the same bytes.
    pub urls: Vec<Url>,
    /// File name (a relative path for metalinks and multi-file torrents) chosen by the input;
    /// None lets the server decide.
    pub name: Option<PathBuf>,
    /// Subfolder of the save folder the file goes in, when the input decides the folder but not
    /// the file name (a playlist entry, a feed episode named by the server). Cleaned like `name`
    /// (see `clean_path`).
    pub folder: Option<PathBuf>,
    /// How yt-dlp names the file of a media download that `name` does not name: an output
    /// template (a playlist entry's title and id, `%(title)s [%(id)s].%(ext)s`). None names it by
    /// its title.
    pub media_name: Option<String>,
    /// Checksum published by a metalink.
    pub checksum: Option<String>,
    /// Size published by a metalink or torrent.
    pub size: Option<u64>,
    /// Listed by a .metalink, .meta4 or .torrent, so its hosts are ones the user never named:
    /// the Authorization header the user gave is not sent to them.
    pub from_document: bool,
    /// The .metalink, .meta4 or .torrent link itself, left to the engine to download as a file
    /// (its host would not hand it over, or the torrent has no HTTP web seeds): a checksum given
    /// for the file it lists is not its own.
    pub document_itself: bool,
    /// Lines the download archive gets once the file is downloaded (see
    /// `engine::DownloadOptions::archive_lines`), which leave it out of a later listing with
    /// `only_new`: a feed episode's, or a cloud folder file's.
    pub archive: Vec<String>,
    /// Optional HTTP Referer header for links requiring anti-hotlinking bypass.
    pub referer: Option<String>,
    /// A BitTorrent swarm download: `urls` holds the magnet link, the .torrent link or the
    /// `file:` URL of a local .torrent (see `engine::DownloadOptions::bittorrent`).
    pub p2p: bool,
}

impl Task {
    /// Whether the download stays where it lands, never sorted into a category folder: it is in
    /// a folder of its own (a listing's, a playlist's, a torrent's), or a swarm download, which
    /// seeds from there.
    pub fn keeps_its_place(&self) -> bool {
        self.p2p || self.folder.is_some() || self.name.as_ref().is_some_and(|name| name.components().count() > 1)
    }

    /// Short name for progress output.
    pub fn label(&self) -> String {
        if let Some(name) = &self.name {
            return name.to_string_lossy().into_owned();
        }
        self.urls
            .first()
            .map(|u| {
                u.path_segments()
                    .and_then(|mut s| s.next_back())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| u.host_str().unwrap_or("download").to_string())
            })
            .unwrap_or_else(|| "download".to_string())
    }
}

/// How a link that lists many downloads (a folder, feed, playlist or channel) is read.
#[derive(Clone, Debug)]
pub struct ListOptions {
    /// The user's Google API key: lists a whole Google Drive folder through the Drive API.
    /// Without it only what the public folder page shows is listed.
    pub google_api_key: Option<String>,
    /// A video link that also names a playlist (watch?v=X&list=Y) lists the playlist instead of
    /// the one video.
    pub whole_playlist: bool,
    /// Only the newest N items of a channel, playlist or feed.
    pub latest: Option<usize>,
    /// Leave out items downloaded before (channel sync, feed updates, a cloud folder added again).
    pub only_new: bool,
    /// Cookies for yt-dlp listings of private or members-only lists.
    pub cookies: media::BrowserCookieSource,
    /// Proxy for listings made outside the HTTP client [`ingest`] is given (yt-dlp's).
    pub proxy: Option<String>,
    /// Where a listing sends what the user should know besides its downloads (items left out, a
    /// folder listed in part), for a front end to show; None logs it as a warning.
    pub notes: Option<std::sync::mpsc::Sender<ListNote>>,
    /// Magnet links and .torrent files without HTTP web seeds are downloaded from the swarm.
    pub p2p: bool,
    /// The debrid API key magnet links go through (with `debrid_magnets`).
    pub debrid_key: Option<String>,
    /// The debrid provider of `debrid_key` (see `engine::DownloadOptions::debrid_provider`).
    pub debrid_provider: Option<String>,
    /// Magnet links go through debrid when there is a `debrid_key`.
    pub debrid_magnets: bool,
    /// The password of the share being listed (a Gofile folder's), from a paste.
    pub password: Option<String>,
    /// The link being read and the Authorization header a paste gave it (its `user:pass@`):
    /// sent when a feed or a remote .torrent or .metalink is fetched from that link's host alone
    /// (see [`ListOptions::get`]).
    pub auth: Option<(Url, String)>,
}

impl ListOptions {
    /// A GET of `url` with [`ListOptions::auth`]'s header when `url` is on its link's host, and
    /// not over plain HTTP when the link is HTTPS (as `worker::Auth` scopes the user's).
    pub(crate) fn get(&self, http: &reqwest::Client, url: &Url) -> reqwest::RequestBuilder {
        let request = http.get(url.clone());
        let Some((link, header)) = &self.auth else { return request };
        let value = reqwest::header::HeaderValue::from_str(header).ok().map(|mut value| {
            value.set_sensitive(true);
            value
        });
        match value {
            Some(value) if link.host_str() == url.host_str() && (link.scheme() != "https" || url.scheme() == "https") => {
                request.header(reqwest::header::AUTHORIZATION, value)
            }
            _ => request,
        }
    }
}

/// What a listing tells the user besides its downloads (see [`ListOptions::notes`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListNote {
    pub text: String,
    /// Part of what the link lists could not be read (its host kept failing): the downloads
    /// listed are not all there are, and the listing counts as failed, though they are added.
    pub failed: bool,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self {
            google_api_key: None,
            whole_playlist: false,
            latest: None,
            only_new: true,
            cookies: media::BrowserCookieSource::None,
            proxy: None,
            notes: None,
            p2p: false,
            debrid_key: None,
            debrid_provider: None,
            debrid_magnets: true,
            password: None,
            auth: None,
        }
    }
}

impl ListOptions {
    /// Tells the user `text` about a listing, through `notes` or else as a warning.
    pub(crate) fn note(&self, text: String) {
        self.tell(ListNote { text, failed: false });
    }

    /// Tells the user that the listing failed in part, as `text` says (see [`ListNote::failed`]).
    pub(crate) fn fail(&self, text: String) {
        self.tell(ListNote { text, failed: true });
    }

    fn tell(&self, note: ListNote) {
        let unsent = match &self.notes {
            Some(notes) => notes.send(note).err().map(|e| e.0),
            None => Some(note),
        };
        if let Some(note) = unsent {
            tracing::warn!("{}", note.text);
        }
    }
}

#[derive(Clone, Copy)]
enum Descriptor {
    Metalink,
    Torrent,
}

fn descriptor_kind(path: &str) -> Option<Descriptor> {
    let path = path.to_ascii_lowercase();
    if path.ends_with(".metalink") || path.ends_with(".meta4") {
        Some(Descriptor::Metalink)
    } else if path.ends_with(".torrent") {
        Some(Descriptor::Torrent)
    } else {
        None
    }
}

/// `token` as an http(s) URL.
pub fn http_url(token: &str) -> Option<Url> {
    Url::parse(token).ok().filter(|u| matches!(u.scheme(), "http" | "https"))
}

/// A `blob:` URL, or a YouTube URL ending in a blob UUID pasted without its prefix.
pub fn is_blob_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("blob:")
        || (s.contains("youtube.com")
            && s.rsplit('/').next().is_some_and(|last| last.len() == 36 && last.matches('-').count() == 4))
}

/// Where a token points to a .metalink/.meta4/.torrent document, if it does.
enum Source {
    Remote(Url),
    Local(PathBuf),
}

fn descriptor_source(token: &str) -> Option<(Source, Descriptor)> {
    if torrent::is_magnet_uri(token) {
        return None;
    }
    match http_url(token) {
        Some(url) => {
            let url = resolver::unwrap_redirect(&url).unwrap_or(url);
            descriptor_kind(url.path()).map(|kind| (Source::Remote(url), kind))
        }
        None => descriptor_kind(token).map(|kind| (Source::Local(PathBuf::from(token)), kind)),
    }
}

/// Whether `text` names a .metalink, .meta4 or .torrent (a URL or a local path, alone or among
/// other tokens). [`ingest`] reads it, which takes I/O a UI thread leaves to the runtime.
pub fn names_document(text: &str) -> bool {
    descriptor_source(&unquote(text)).is_some() || split_tokens(text).iter().any(|t| descriptor_source(t).is_some())
}

/// Whether `url` may list many downloads (a folder, feed, playlist or channel), from its shape
/// alone: [`ingest`] then asks the listers.
pub fn might_list(url: &Url) -> bool {
    mega::lists(url)
        || shares::lists(url)
        || cloud::lists(url)
        || repos::lists(url)
        || folders::lists(url)
        || feeds::lists(url)
        || media::lists(url)
}

/// `token` as a link [`might_list`] takes, a "leaving this site" link replaced by its target.
fn listing_url(token: &str) -> Option<Url> {
    let url = http_url(token)?;
    let url = resolver::unwrap_redirect(&url).unwrap_or(url);
    might_list(&url).then_some(url)
}

/// Whether [`ingest`] reads `text` before it knows its downloads: it names a document (see
/// [`names_document`]), is one link that may list many, or is a magnet link (debrid or the swarm
/// may take it). A UI thread leaves that to the runtime.
pub fn needs_reading(text: &str) -> bool {
    names_document(text)
        || matches!(&split_tokens(text)[..], [token] if listing_url(token).is_some() || torrent::is_magnet_uri(token))
}

/// `name` (from a torrent, metalink or magnet, so untrusted) made safe as one path component on
/// every OS (see `engine::sanitize_component`). None when nothing is left, as for "." and "..".
fn clean_component(name: &str) -> Option<String> {
    Some(crate::engine::sanitize_component(name)).filter(|c| !c.is_empty())
}

/// A relative path built from untrusted name components, each cleaned by [`clean_component`].
pub(crate) fn clean_path<'a>(components: impl IntoIterator<Item = &'a str>) -> Result<PathBuf, String> {
    components
        .into_iter()
        .map(|c| clean_component(c).ok_or_else(|| format!("unusable file name {:?}", c)))
        .collect()
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches(['"', '\'']).to_string()
}

/// The whitespace-separated tokens of `line`, without quotes around them.
pub fn split_tokens(line: &str) -> Vec<String> {
    line.split_whitespace().map(unquote).filter(|t| !t.is_empty()).collect()
}

/// Splits one line of input into mirror tokens. A line naming a single existing local file, or
/// a local .metalink/.meta4/.torrent whether or not it exists (a path with spaces, possibly
/// quoted by drag and drop), stays one token, so reading it reports the real error; quotes
/// around tokens are removed.
pub async fn input_tokens(line: &str) -> Vec<String> {
    let whole = unquote(line);
    let local_document = matches!(descriptor_source(&whole), Some((Source::Local(_), _)));
    if local_document || tokio::fs::metadata(&whole).await.is_ok_and(|m| m.is_file()) {
        return vec![whole];
    }
    split_tokens(line)
}

/// Parses one input (a batch line split on whitespace, or the command-line URLs) into tasks.
///
/// The tokens are mirrors of one file (see [`link_task`]), unless they are one link that
/// [`might_list`]: the lister that takes it (folders, then feeds, then media) lists its
/// downloads, as `options` say, or finds it lists nothing after all. A local or remote
/// .metalink/.meta4/.torrent must stand alone and may yield several tasks, one per file. A
/// remote .torrent none of whose files has an HTTP web seed yields the .torrent itself, for a
/// torrent client. A remote document is fetched from where its host's resolver says the file is
/// (a GitHub /blob/ page's /raw/ link, a Dropbox share's `dl=1`); one whose host answers with a
/// client error (a login, a private GitHub file's 404; a timeout or a rate limit is an error) or
/// a web page yields the link itself, for the engine, which sends the user's cookies and
/// Authorization and refuses a page in place of the file. The link itself is always the one
/// typed, never where its resolver led (a swarm download's .torrent aside: nothing resolves it
/// again). `Ok` is empty only for a listing everything of which was
/// downloaded before (see [`ListOptions::only_new`]): nothing to do, not a failure.
pub async fn ingest(tokens: &[impl AsRef<str>], http: &reqwest::Client, options: &ListOptions) -> Result<Vec<Task>, String> {
    if let [token] = tokens {
        if let Some(tasks) = magnet_tasks(http, token.as_ref(), options).await? {
            return Ok(tasks);
        }
        if let Some((source, kind)) = descriptor_source(token.as_ref()) {
            return match source {
                Source::Remote(typed) => remote_tasks(http, typed, kind, options).await,
                Source::Local(path) => {
                    let swarm = std::path::absolute(&path).ok().and_then(|p| Url::from_file_path(p).ok()).filter(|_| options.p2p);
                    document_tasks(kind, &read_local(&path).await?, None, swarm.as_ref())
                }
            };
        }
        if let Some(url) = listing_url(token.as_ref()) {
            if let Some(listed) = list(http, &url, options).await {
                return listed;
            }
        }
    }
    if let Some(token) = tokens.iter().map(|t| t.as_ref()).find(|t| descriptor_source(t).is_some()) {
        return Err(format!("{}: a .metalink, .meta4 or .torrent input must be on its own", token));
    }
    link_task(tokens).map(|task| vec![task])
}

/// The downloads of the magnet link `token` when debrid or the swarm takes it: debrid when the
/// user has a key for magnets and it takes the magnet, else the swarm when the magnet has no HTTP
/// web seeds and `p2p` is on. None leaves it to [`link_task`] (its web seeds, or the error).
async fn magnet_tasks(http: &reqwest::Client, token: &str, options: &ListOptions) -> Result<Option<Vec<Task>>, String> {
    if !torrent::is_magnet_uri(token) {
        return Ok(None);
    }
    let mut debrid_failed = None;
    if let Some(key) = options.debrid_key.as_deref().filter(|k| options.debrid_magnets && !k.trim().is_empty()) {
        let provider = options.debrid_provider.as_deref().unwrap_or_default();
        match crate::debrid::magnet_tasks(http, token, key, provider).await {
            Ok(tasks) if !tasks.is_empty() => return Ok(Some(tasks)),
            Ok(_) => {}
            Err(e) => debrid_failed = Some(e),
        }
    }
    let parsed = torrent::parse_magnet_uri(token).ok().zip(Url::parse(token).ok());
    let web_seeds = parsed.as_ref().is_some_and(|(magnet, _)| !magnet.web_seeds.is_empty());
    let swarm = parsed.filter(|_| options.p2p && !web_seeds);
    // A debrid failure is the error only when neither web seeds nor the swarm can take it.
    if let Some(e) = debrid_failed {
        if swarm.is_none() && !web_seeds {
            return Err(e);
        }
        options.note(format!("{e}; downloading it from {} instead", if web_seeds { "its web seeds" } else { "the swarm" }));
    }
    let Some((magnet, url)) = swarm else { return Ok(None) };
    let name = magnet.display_name.as_deref().and_then(clean_component).map(PathBuf::from);
    Ok(Some(vec![Task { urls: vec![url], name, p2p: true, ..Task::default() }]))
}

/// The downloads the folder, feed or playlist at `url` lists, from the first lister whose `lists`
/// takes it and that finds a listing there; None when none does.
async fn list(http: &reqwest::Client, url: &Url, options: &ListOptions) -> Option<Result<Vec<Task>, String>> {
    if mega::lists(url) {
        return Some(mega::list(http, url).await);
    }
    if shares::lists(url) {
        return Some(shares::list(http, url, options.password.as_deref()).await);
    }
    if cloud::lists(url) {
        return Some(cloud::list(http, url).await);
    }
    if repos::lists(url) {
        let auth = options.auth.as_ref().filter(|(link, _)| link == url).map(|(_, header)| header.as_str());
        return Some(repos::list(http, url, auth).await);
    }
    if folders::lists(url) {
        if let Some(listed) = folders::list(http, url, options).await {
            return Some(listed);
        }
    }
    if feeds::lists(url) {
        if let Some(listed) = feeds::list(http, url, options).await {
            return Some(listed);
        }
    }
    if media::lists(url) {
        return media::list(http, url, options).await;
    }
    None
}

/// The mirrors of one file named by links: http(s) URLs, each "leaving this site" link replaced
/// by its target, and magnet links with HTTP web seeds (the first one also names the file).
pub fn link_task(tokens: &[impl AsRef<str>]) -> Result<Task, String> {
    let mut task = Task::default();
    for token in tokens {
        let token = token.as_ref();
        if is_blob_url(token) {
            return Err(BLOB_MESSAGE.to_string());
        }
        let urls = if torrent::is_magnet_uri(token) {
            let magnet = torrent::parse_magnet_uri(token).map_err(|e| format!("Invalid magnet link: {}", e))?;
            if magnet.web_seeds.is_empty() {
                let name = magnet.display_name.map(|n| format!(" for \"{}\"", n)).unwrap_or_default();
                return Err(format!(
                    "The magnet link{} has no HTTP web seeds (ws=). Peer-to-peer BitTorrent transfers are not supported, so it cannot be downloaded.",
                    name
                ));
            }
            if task.name.is_none() {
                task.name = magnet.display_name.as_deref().and_then(clean_component).map(PathBuf::from);
            }
            magnet.web_seeds
        } else {
            let url = Url::parse(token).map_err(|e| format!("Invalid URL '{}': {}", truncate_chars(token, 80), e))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(format!("Unsupported URL scheme '{}': only http and https can be downloaded", url.scheme()));
            }
            vec![resolver::unwrap_redirect(&url).unwrap_or(url)]
        };
        for url in urls {
            if !task.urls.contains(&url) {
                task.urls.push(url);
            }
        }
    }
    if task.urls.is_empty() {
        return Err("Enter a download URL".to_string());
    }
    Ok(task)
}

/// The client [`ingest`] fetches remote .metalink/.torrent documents with, through `proxy` if set.
pub fn descriptor_client(proxy: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent(resolver::APP_USER_AGENT);
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| format!("invalid proxy: {}", e))?);
    }
    builder.build().map_err(|e| format!("cannot create HTTP client: {}", e))
}

fn too_large(what: impl std::fmt::Display) -> String {
    format!("{} is larger than {} bytes", what, MAX_DESCRIPTOR_BYTES)
}

/// The tasks of the document at `typed`, fetched from where its host's resolver says the file is.
/// The link itself when its host answers with a client error (see [`fetch`]), or with a web page
/// that is no such document: a host may label any file a page (PHP sends text/html unless told
/// otherwise). A torrent without web seeds goes to the swarm when `p2p` is on.
async fn remote_tasks(http: &reqwest::Client, typed: Url, kind: Descriptor, options: &ListOptions) -> Result<Vec<Task>, String> {
    let url = resolver::SmartResolver::resolve_mirrors(http, &typed).await.into_iter().next().unwrap_or_else(|| typed.clone());
    match fetch(options.get(http, &url), &url).await? {
        Some((bytes, labelled_page)) if !labelled_page || (!starts_like_html(&bytes) && parses(kind, &bytes)) => {
            // The swarm reads the .torrent from where it was found: nothing resolves it there.
            document_tasks(kind, &bytes, Some(&typed), options.p2p.then_some(&url))
        }
        _ => {
            tracing::info!("{} answered with an error or a web page: the download engine takes {}", url, typed);
            Ok(vec![Task { urls: vec![typed], document_itself: true, ..Task::default() }])
        }
    }
}

/// The tasks the document `bytes` lists (see [`torrent_tasks`] for `remote` and `swarm`).
fn document_tasks(kind: Descriptor, bytes: &[u8], remote: Option<&Url>, swarm: Option<&Url>) -> Result<Vec<Task>, String> {
    match kind {
        Descriptor::Metalink => metalink_tasks(bytes),
        Descriptor::Torrent => torrent_tasks(bytes, remote, swarm),
    }
}

/// Whether `bytes` read as a document of this kind.
fn parses(kind: Descriptor, bytes: &[u8]) -> bool {
    match kind {
        Descriptor::Metalink => decode_text(bytes).and_then(|text| metalink::parse_metalink(&text)).is_ok(),
        Descriptor::Torrent => torrent::parse_torrent_bytes(bytes).is_ok(),
    }
}

/// Whether `body` opens as a web page: `<!doctype` or `<html`, after a BOM and whitespace.
fn starts_like_html(body: &[u8]) -> bool {
    let start = body.strip_prefix("\u{feff}".as_bytes()).unwrap_or(body).trim_ascii_start();
    [&b"<!doctype"[..], b"<html"].iter().any(|tag| start.get(..tag.len()).is_some_and(|s| s.eq_ignore_ascii_case(tag)))
}

/// The document `request` gets from `url` and whether its host labels it a web page; None when
/// its host answers with a client error (401 or 403 for a login, 404 for a private GitHub
/// repository's /raw/ link) or with a page larger than a document may be, which the engine is
/// left to download and report. A timeout (408) or a rate limit (429) says nothing of the file:
/// it is an error, to retry, as a server error is.
async fn fetch(request: reqwest::RequestBuilder, url: &Url) -> Result<Option<(Vec<u8>, bool)>, String> {
    use reqwest::StatusCode;
    let fail = |e: reqwest::Error| format!("Cannot fetch {}: {}", url, e);
    let resp = request.send().await.map_err(fail)?;
    let status = resp.status();
    if status.is_client_error() && !matches!(status, StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS) {
        tracing::info!("{} answered HTTP {}", url, status);
        return Ok(None);
    }
    let mut resp = resp.error_for_status().map_err(fail)?;
    let labelled_page = resolver::html_type(resp.headers());
    let oversized = || if labelled_page { Ok(None) } else { Err(too_large(url)) };
    if resp.content_length().is_some_and(|len| len > MAX_DESCRIPTOR_BYTES as u64) {
        return oversized();
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(fail)? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_DESCRIPTOR_BYTES {
            return oversized();
        }
    }
    Ok(Some((body, labelled_page)))
}

/// A local document, read only if it is a file no larger than a remote one may be: a renamed
/// disc image is not loaded into memory, and a pipe or device is not waited on.
async fn read_local(path: &Path) -> Result<Vec<u8>, String> {
    let fail = |e: std::io::Error| format!("Cannot read {}: {}", path.display(), e);
    let meta = tokio::fs::metadata(path).await.map_err(fail)?;
    if !meta.is_file() {
        return Err(format!("Cannot read {}: not a file", path.display()));
    }
    if meta.len() > MAX_DESCRIPTOR_BYTES as u64 {
        return Err(too_large(path.display()));
    }
    let mut bytes = Vec::new();
    let file = tokio::fs::File::open(path).await.map_err(fail)?;
    // It may have grown since.
    file.take(MAX_DESCRIPTOR_BYTES as u64 + 1).read_to_end(&mut bytes).await.map_err(fail)?;
    if bytes.len() > MAX_DESCRIPTOR_BYTES {
        return Err(too_large(path.display()));
    }
    Ok(bytes)
}

/// One task per metalink file, named by its relative path ("dir/file.iso" is saved in dir/).
fn metalink_tasks(bytes: &[u8]) -> Result<Vec<Task>, String> {
    let text = decode_text(bytes)?;
    metalink::parse_metalink(&text)?
        .into_iter()
        .map(|file| {
            if file.urls.is_empty() {
                return Err(format!("metalink file '{}' has no http(s) URLs", file.name));
            }
            // A malformed hash (of the wrong length) is passed over for the next strongest.
            let checksum = ["sha512", "sha256", "sha1", "md5"].iter().find_map(|algo| {
                file.hashes
                    .iter()
                    .filter(|(t, _)| t == algo)
                    .map(|(t, h)| format!("{}:{}", t, h))
                    .find(|c| crate::storage::validate_checksum(c).is_ok())
            });
            let name = Some(clean_path(file.name.split('/'))?);
            Ok(Task { name, urls: file.urls, checksum, size: file.size, from_document: true, ..Task::default() })
        })
        .collect()
}

/// One task per torrent file with HTTP web seeds. When no file has any, the torrent at `swarm`
/// (its link, or the `file:` URL of a local one, when the swarm is allowed) is one swarm download;
/// else the torrent at `remote` (the link typed for it, if it was fetched) is the one download: a
/// torrent client takes it from there.
fn torrent_tasks(bytes: &[u8], remote: Option<&Url>, swarm: Option<&Url>) -> Result<Vec<Task>, String> {
    let info = torrent::parse_torrent_bytes(bytes)?;
    let seedless = info.files.iter().all(|f| f.urls.is_empty());
    if let Some(url) = swarm.filter(|_| seedless) {
        let name = clean_component(&info.name).map(PathBuf::from);
        return Ok(vec![Task { urls: vec![url.clone()], name, p2p: true, ..Task::default() }]);
    }
    if let Some(url) = remote.filter(|_| seedless) {
        return Ok(vec![Task { urls: vec![url.clone()], document_itself: true, ..Task::default() }]);
    }
    // A BitTorrent v2-only torrent lists its files in a `file tree`, which is not read.
    if info.files.is_empty() {
        return Err("the torrent lists no files this app can read (BitTorrent v2-only torrents are not supported)".to_string());
    }
    let single_file = matches!(&info.files[..], [f] if f.path == [info.name.clone()]);
    info.files
        .iter()
        .map(|file| {
            let relative = clean_path(file.path.iter().map(String::as_str))?;
            if file.urls.is_empty() {
                return Err(format!(
                    "torrent file '{}' has no HTTP web seeds (url-list); BitTorrent swarm downloads are not supported",
                    relative.display()
                ));
            }
            let name = if single_file { relative } else { clean_path([info.name.as_str()])?.join(relative) };
            Ok(Task { urls: file.urls.clone(), name: Some(name), size: Some(file.length), from_document: true, ..Task::default() })
        })
        .collect()
}

/// Decodes a text file saved as UTF-8 (with or without BOM) or UTF-16 with a BOM.
pub fn decode_text(bytes: &[u8]) -> Result<String, String> {
    let utf16 = |data: &[u8], from: fn([u8; 2]) -> u16| {
        if !data.len().is_multiple_of(2) {
            return Err("invalid UTF-16 text (odd length)".to_string());
        }
        let units: Vec<u16> = data.chunks_exact(2).map(|c| from([c[0], c[1]])).collect();
        String::from_utf16(&units).map_err(|_| "invalid UTF-16 text".to_string())
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8(rest.to_vec()).map_err(|_| "invalid UTF-8 text".to_string()),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8(bytes.to_vec()).map_err(|_| "text is not UTF-8 or UTF-16 with a BOM".to_string()),
    }
}

/// At most `max` characters, ending in "..." when shortened. Never splits a character.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(line: &str) -> Result<Vec<Task>, String> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let tokens: Vec<&str> = line.split_whitespace().collect();
        rt.block_on(ingest(&tokens, &reqwest::Client::new(), &ListOptions::default()))
    }

    fn link(text: &str) -> Result<Task, String> {
        link_task(&split_tokens(text))
    }

    #[test]
    fn mirrors_form_one_task() {
        let tasks = run("https://a.example/f.iso  http://b.example/f.iso https://a.example/f.iso").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].urls.len(), 2);
        assert_eq!(tasks[0].name, None);
        assert_eq!(tasks[0].label(), "f.iso");
    }

    #[test]
    fn rejects_non_http_inputs() {
        assert!(run("ftp://x.example/f").unwrap_err().contains("scheme 'ftp'"));
        assert!(run("blob:https://www.youtube.com/abc").unwrap_err().contains("blob:"));
        assert!(run("C:\\file.bin").is_err());
        assert!(run("https://a.example/x.torrent https://b.example/y").unwrap_err().contains("on its own"));
    }

    #[test]
    fn links_are_mirrors_of_one_file_and_bad_input_is_refused() {
        let task = link("  https://a.com/f.iso\thttp://b.com/f.iso https://a.com/f.iso ").unwrap();
        assert_eq!(task.urls.len(), 2, "duplicates are dropped");
        assert_eq!(link("   ").unwrap_err(), "Enter a download URL");
        assert!(link("ftp://a.com/f").unwrap_err().contains("scheme"));
        assert!(link("not a url").unwrap_err().contains("Invalid URL"));
        assert_eq!(link("blob:https://www.youtube.com/x").unwrap_err(), BLOB_MESSAGE);
        assert_eq!(link("https://www.youtube.com/0b9f5e2c-1d3a-4c5e-9f7a-123456789abc").unwrap_err(), BLOB_MESSAGE);
        let err = link(&"x".repeat(100)).unwrap_err();
        assert!(err.contains(&format!("'{}...'", "x".repeat(77))), "long input is shortened: {}", err);
    }

    /// The queue and history hold the target of a "leaving this site" link, not the link itself,
    /// and a wrapped document is read as one. The CLI goes through `ingest`, the GUI's form and
    /// queue through `link_task`.
    #[test]
    fn leaving_links_are_replaced_by_their_target() {
        let target = [Url::parse("https://www.mediafire.com/file/abc/mod.zip").unwrap()];
        for wrapped in [
            "https://www.google.com/url?q=https%3A%2F%2Fwww.mediafire.com%2Ffile%2Fabc%2Fmod.zip",
            // As YouTube writes the links in a video's description.
            "https://www.youtube.com/redirect?event=video_description&redir_token=QUFF&q=https%3A%2F%2Fwww.mediafire.com%2Ffile%2Fabc%2Fmod.zip&v=dQw4w9WgXcQ",
        ] {
            assert_eq!(link(wrapped).unwrap().urls, target, "{wrapped}");
            assert_eq!(run(wrapped).unwrap()[0].urls, target, "{wrapped}");
        }
        assert!(names_document("https://www.google.com/url?q=https%3A%2F%2Fm.example%2Flist.meta4"));
    }

    #[test]
    fn magnet_web_seeds_and_name() {
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let tasks = run(&format!("magnet:?xt=urn:btih:{}&dn=my+file.iso&ws=https%3A%2F%2Fseed.example%2Fdl%2F", hash)).unwrap();
        assert_eq!(tasks[0].name, Some(PathBuf::from("my file.iso")));
        assert_eq!(tasks[0].urls[0].as_str(), "https://seed.example/dl/my%20file.iso");

        let err = run(&format!("magnet:?xt=urn:btih:{}&dn=Ubuntu", hash)).unwrap_err();
        assert!(err.contains("\"Ubuntu\"") && err.contains("no HTTP web seeds"), "{}", err);
        assert!(run("magnet:?dn=x").unwrap_err().starts_with("Invalid magnet link"));
        // A magnet whose name ends in .torrent is still a magnet, not a file to read.
        let named = format!("magnet:?xt=urn:btih:{}&ws=https%3A%2F%2Fs.example%2Fa&dn=a.torrent", hash);
        assert!(!names_document(&named));
        assert_eq!(run(&named).unwrap()[0].name, Some(PathBuf::from("a.torrent")));
    }

    #[test]
    fn documents_are_told_from_links() {
        assert!(names_document("https://a.example/x.torrent"));
        assert!(names_document("https://a.example/list.META4?x=1"));
        assert!(names_document("\"C:\\My Files\\list.metalink\""));
        assert!(names_document("https://a.example/x.torrent https://b.example/y"), "refused later, off the UI thread");
        assert!(!names_document("https://a.example/x.iso https://b.example/x.iso"));
        assert!(!names_document("https://a.example/get?file=x.torrent"));
    }

    /// A document is read before its downloads are known, a link to one file is not; a listing
    /// leaves out what was downloaded before unless told otherwise.
    #[test]
    fn documents_need_reading_and_listings_skip_old_items() {
        assert!(needs_reading("\"C:\\My Files\\list.metalink\""));
        assert!(needs_reading("https://a.example/x.torrent https://b.example/y"));
        assert!(!needs_reading("https://a.example/x.iso https://b.example/x.iso"));
        assert!(ListOptions::default().only_new);
    }

    fn write_temp(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn local_metalink_yields_named_tasks_with_checksums() {
        let (sha256, md5) = ("AB".repeat(32), "01".repeat(16));
        let xml = format!(
            r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink">
            <file name="a.bin"><hash type="sha-256">{sha256}</hash><url>https://m1.example/a.bin</url></file>
            <file name="b.bin"><hash type="md5">{md5}</hash><url>https://m1.example/b.bin</url></file>
            </metalink>"#
        );
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(xml.as_bytes());
        let (_dir, path) = write_temp("list.meta4", &bytes);

        let tasks = run(path.to_str().unwrap()).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, Some(PathBuf::from("a.bin")));
        assert_eq!(tasks[0].checksum, Some(format!("sha256:{}", sha256.to_lowercase())));
        assert_eq!(tasks[1].checksum, Some(format!("md5:{md5}")));
        assert!(tasks.iter().all(|t| t.from_document), "the user never named these hosts");
        assert!(!run("https://a.example/f.iso").unwrap()[0].from_document);
    }

    /// The strongest checksum a metalink publishes is checked; one that gives only SHA-1 or
    /// SHA-512 (Metalink 3 files often give SHA-1 alone) is checked too. A malformed one is passed
    /// over for the next strongest.
    #[test]
    fn metalink_checksums_are_taken_strongest_first() {
        let (sha512, sha256, sha1) = ("ab".repeat(64), "12".repeat(32), "cd".repeat(20));
        let xml = format!(
            r#"<metalink version="3.0" xmlns="http://www.metalinker.org/"><files>
            <file name="a.iso"><verification><hash type="sha1">{sha1}</hash><hash type="md5">{md5}</hash><hash type="sha512">{sha512}</hash></verification>
            <resources><url type="http">https://m.example/a.iso</url></resources></file>
            <file name="b.iso"><verification><hash type="sha1">{sha1}</hash><hash type="md5">{md5}</hash></verification>
            <resources><url type="http">https://m.example/b.iso</url></resources></file>
            <file name="c.iso"><verification><hash>{sha512}</hash></verification><resources><url>https://m.example/c.iso</url></resources></file>
            <file name="d.iso"><verification><hash type="sha512">{short}</hash><hash type="sha256">{sha256}</hash></verification>
            <resources><url>https://m.example/d.iso</url></resources></file>
            </files></metalink>"#,
            md5 = "ef".repeat(16),
            short = "ab".repeat(63),
        );
        let checksums: Vec<_> = metalink_tasks(xml.as_bytes()).unwrap().into_iter().map(|t| t.checksum.unwrap()).collect();
        assert_eq!(
            checksums,
            [format!("sha512:{sha512}"), format!("sha1:{sha1}"), format!("sha512:{sha512}"), format!("sha256:{sha256}")]
        );
        for checksum in &checksums {
            assert!(crate::storage::validate_checksum(checksum).is_ok(), "{checksum}");
        }
    }

    /// A local document is read only if it is a file no larger than a remote one may be.
    #[test]
    fn local_documents_must_be_small_files() {
        let (dir, huge) = write_temp("disc.iso.torrent", b"");
        std::fs::File::options().write(true).open(&huge).unwrap().set_len(MAX_DESCRIPTOR_BYTES as u64 + 1).unwrap();
        assert!(run(huge.to_str().unwrap()).unwrap_err().contains("is larger than"));
        let folder = dir.path().join("folder.torrent");
        std::fs::create_dir(&folder).unwrap();
        assert!(run(folder.to_str().unwrap()).unwrap_err().ends_with("not a file"));
    }

    #[test]
    fn metalink_files_keep_their_folders() {
        let xml = r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink">
            <file name="dir/sub/file.iso"><url>https://m.example/file.iso</url></file>
            <file name="./top: level?.txt"><url>https://m.example/top.txt</url></file>
            <file name="con/aux.txt"><url>https://m.example/aux.txt</url></file>
            </metalink>"#;
        let names: Vec<_> = metalink_tasks(xml.as_bytes()).unwrap().into_iter().map(|t| t.name.unwrap()).collect();
        assert_eq!(
            names,
            [Path::new("dir").join("sub").join("file.iso"), PathBuf::from("top_ level_.txt"), Path::new("_con").join("_aux.txt")]
        );
        for bad in ["../escape.iso", "dir/../../escape.iso", "/etc/passwd", "C:\\Windows\\x.dll", "dir/.../x"] {
            let xml = format!(r#"<metalink><file name="{}"><url>https://m.example/x</url></file></metalink>"#, bad);
            assert!(metalink_tasks(xml.as_bytes()).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn multi_file_torrent_becomes_one_task_per_file() {
        let torrent = b"d8:url-list20:https://s.example/d/4:infod5:filesld6:lengthi3e4:pathl1:a5:x.binee\
d6:lengthi4e4:pathl5:y.bineee4:name4:root12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        let tasks = torrent_tasks(torrent, None, None).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, Some(Path::new("root").join("a").join("x.bin")));
        assert_eq!((tasks[0].size, tasks[1].size), (Some(3), Some(4)));
        assert_eq!(tasks[1].urls[0].as_str(), "https://s.example/d/root/y.bin");
    }

    /// A torrent that lists no files this app reads is an error, not a download of nothing; a
    /// remote one is saved itself, as one without web seeds is.
    #[test]
    fn a_torrent_without_files_to_read_is_an_error() {
        let v2_only = b"d4:infod9:file treed5:a.bind0:d6:lengthi3e11:pieces root32:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaeee\
12:meta versioni2e4:name4:pack12:piece lengthi16384eee";
        let err = torrent_tasks(v2_only, None, None).unwrap_err();
        assert!(err.contains("BitTorrent v2-only torrents are not supported"), "{err}");
        let (_dir, path) = write_temp("pack.torrent", v2_only);
        assert_eq!(run(path.to_str().unwrap()).unwrap_err(), err);
        let url = Url::parse("https://releases.example/pack.torrent").unwrap();
        assert_eq!(torrent_tasks(v2_only, Some(&url), None).unwrap(), [Task { urls: vec![url], document_itself: true, ..Task::default() }]);
    }

    /// A torrent without HTTP web seeds cannot be downloaded over HTTP; a remote one is then
    /// saved itself, for a torrent client, as the link stands for that file.
    #[test]
    fn a_remote_torrent_without_web_seeds_is_saved_itself() {
        let no_seeds = b"d4:infod6:lengthi3e4:name5:x.bin12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        assert!(torrent_tasks(no_seeds, None, None).unwrap_err().contains("no HTTP web seeds"));
        let url = Url::parse("https://releases.example/x.bin.torrent").unwrap();
        let tasks = torrent_tasks(no_seeds, Some(&url), None).unwrap();
        assert_eq!(tasks, [Task { urls: vec![url], document_itself: true, ..Task::default() }]);
    }

    /// With the swarm allowed, a magnet link or a .torrent (remote or local) without web seeds is
    /// one swarm download; a magnet with web seeds is still downloaded over HTTP.
    #[test]
    fn seedless_torrents_go_to_the_swarm_when_allowed() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let p2p = ListOptions { p2p: true, ..ListOptions::default() };
        let read = |token: &str| rt.block_on(ingest(&[token], &reqwest::Client::new(), &p2p));
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let magnet = format!("magnet:?xt=urn:btih:{}&dn=Ubuntu", hash);
        assert!(needs_reading(&magnet));
        let swarm = Task { urls: vec![Url::parse(&magnet).unwrap()], name: Some("Ubuntu".into()), p2p: true, ..Task::default() };
        assert_eq!(read(&magnet).unwrap(), [swarm]);
        let seeded = read(&format!("{}&ws=https%3A%2F%2Fs.example%2F", magnet)).unwrap();
        assert!(!seeded[0].p2p && seeded[0].urls[0].scheme() == "https");

        let no_seeds = b"d4:infod6:lengthi3e4:name5:x.bin12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
        let url = Url::parse("https://releases.example/x.bin.torrent").unwrap();
        let swarm = Task { urls: vec![url.clone()], name: Some("x.bin".into()), p2p: true, ..Task::default() };
        assert_eq!(torrent_tasks(no_seeds, Some(&url), Some(&url)).unwrap(), [swarm]);
        let (_dir, path) = write_temp("x.bin.torrent", no_seeds);
        let local = read(path.to_str().unwrap()).unwrap();
        assert!(local[0].p2p && local[0].urls[0].scheme() == "file", "{:?}", local);
    }

    /// A debrid failure on a magnet leaves it to its web seeds or the swarm, with a note; it is
    /// the error only when neither takes the magnet.
    #[test]
    fn a_debrid_failure_falls_through_to_what_else_takes_the_magnet() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        // A proxy that hangs up at once: every debrid request fails.
        let proxy = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move { while let Ok((socket, _)) = listener.accept().await { drop(socket) } });
            format!("http://{address}")
        });
        let http = reqwest::Client::builder().proxy(reqwest::Proxy::all(&proxy).unwrap()).build().unwrap();
        let (notes, noted) = std::sync::mpsc::channel();
        let options = |p2p| ListOptions { p2p, debrid_key: Some("sekret123".into()), notes: Some(notes.clone()), ..ListOptions::default() };
        let read = |token: &str, p2p| rt.block_on(ingest(&[token], &http, &options(p2p)));
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=a";
        assert!(read(magnet, true).unwrap()[0].p2p);
        let seeded = read(&format!("{magnet}&ws=https%3A%2F%2Fs.example%2F"), false).unwrap();
        assert_eq!(seeded[0].urls[0].scheme(), "https");
        let told: Vec<String> = noted.try_iter().map(|note| note.text).collect();
        assert!(told.len() == 2 && told[0].ends_with("from the swarm instead") && told[1].ends_with("from its web seeds instead"), "{told:?}");
        let error = read(magnet, false).unwrap_err();
        assert!(!error.is_empty() && !told.iter().chain([&error]).any(|t| t.contains("sekret123")), "{error}");
    }

    /// A pasted sign-in goes with the reading of its link's host alone, never over plain HTTP
    /// after HTTPS.
    #[test]
    fn a_pasted_sign_in_goes_to_the_read_links_host_alone() {
        let http = reqwest::Client::new();
        let link = Url::parse("https://feeds.example/private.rss").unwrap();
        let options = ListOptions { auth: Some((link, "Basic Ym9iOnB3".into())), ..ListOptions::default() };
        let sent = |url: &str| {
            let request = options.get(&http, &Url::parse(url).unwrap()).build().unwrap();
            request.headers().get(reqwest::header::AUTHORIZATION).map(|v| v.to_str().unwrap().to_string())
        };
        assert_eq!(sent("https://feeds.example/private.rss?page=2").as_deref(), Some("Basic Ym9iOnB3"));
        for elsewhere in ["http://feeds.example/private.rss", "https://cdn.example/1.mp3", "https://evil.feeds.example/x"] {
            assert_eq!(sent(elsewhere), None, "{elsewhere}");
        }
        let plain = ListOptions::default().get(&http, &Url::parse("https://feeds.example/").unwrap()).build().unwrap();
        assert!(plain.headers().get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn untrusted_names_are_cleaned() {
        let name = "\u{1b}[31mRED\u{1b}[0m?.bin";
        let torrent = format!(
            "d8:url-list20:https://s.example/d/4:infod6:lengthi3e4:name{}:{}12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee",
            name.len(),
            name
        );
        let tasks = torrent_tasks(torrent.as_bytes(), None, None).unwrap();
        assert_eq!(tasks[0].name, Some(PathBuf::from("_[31mRED_[0m_.bin")));

        let hash = "0123456789abcdef0123456789abcdef01234567";
        let tasks = run(&format!("magnet:?xt=urn:btih:{}&dn=Ep+1%3A+Pilot%1B.mkv..&ws=https%3A%2F%2Fs.example%2Fx", hash)).unwrap();
        assert_eq!(tasks[0].name, Some(PathBuf::from("Ep 1_ Pilot_.mkv")));

        assert_eq!(clean_component(".."), None);
        assert_eq!(clean_component(" . "), None);
        // Names Windows keeps for devices never reach the disk as they are.
        assert_eq!(clean_component("con.txt").as_deref(), Some("_con.txt"));
        assert!(clean_path(["ok", ".."]).unwrap_err().contains("unusable"));
    }

    #[test]
    fn a_local_path_with_spaces_stays_one_token() {
        let (_dir, path) = write_temp("my file.torrent", b"x");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let plain = rt.block_on(input_tokens(path.to_str().unwrap()));
        let quoted = rt.block_on(input_tokens(&format!("\"{}\"", path.display())));
        let mirrors = rt.block_on(input_tokens("\"https://a.example/f\"  https://b.example/f"));
        assert_eq!(plain, [path.to_str().unwrap()]);
        assert_eq!(quoted, plain);
        assert_eq!(mirrors, ["https://a.example/f", "https://b.example/f"]);
        assert!(names_document(path.to_str().unwrap()));

        // A missing one is read as the one path it is, which says what is wrong with it.
        let missing = path.with_file_name("no such file.metalink");
        let line = format!("\"{}\"", missing.display());
        let tokens = rt.block_on(input_tokens(&line));
        assert_eq!(tokens, [missing.to_str().unwrap()]);
        let error = rt.block_on(ingest(&tokens, &reqwest::Client::new(), &ListOptions::default())).unwrap_err();
        assert!(error.starts_with(&format!("Cannot read {}: ", missing.display())), "{}", error);
        // A link line never becomes one token.
        let links = rt.block_on(input_tokens("https://a.example/y https://b.example/x.torrent"));
        assert_eq!(links, ["https://a.example/y", "https://b.example/x.torrent"]);
    }

    #[test]
    fn text_decoding_handles_boms() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhttps://x\n").unwrap(), "https://x\n");
        let utf16le: Vec<u8> = [0xFF, 0xFE].into_iter().chain("a\nb".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(decode_text(&utf16le).unwrap(), "a\nb");
        let utf16be: Vec<u8> = [0xFE, 0xFF].into_iter().chain("é".encode_utf16().flat_map(u16::to_be_bytes)).collect();
        assert_eq!(decode_text(&utf16be).unwrap(), "é");
        assert!(decode_text(&[0xFF, 0xFE, 0x41]).is_err());
        assert!(decode_text(&[0xC3]).is_err());
    }

    /// Fetching through the proxy is tested against a mock proxy in the integration tests.
    #[test]
    fn an_invalid_proxy_is_reported() {
        assert!(descriptor_client(Some("socks5://127.0.0.1:1080")).is_ok());
        assert!(descriptor_client(Some("::not a proxy::")).unwrap_err().starts_with("invalid proxy"));
    }

    #[test]
    fn truncate_chars_never_splits_characters() {
        let url = format!("https://ja.wikipedia.org/wiki/{}", "東京都の区市町村".repeat(5));
        let short = truncate_chars(&url, 55);
        assert_eq!(short.chars().count(), 55);
        assert!(short.ends_with("..."));
        assert_eq!(truncate_chars("héllo", 5), "héllo");
        assert_eq!(truncate_chars("héllo!", 5), "hé...");
    }
}
