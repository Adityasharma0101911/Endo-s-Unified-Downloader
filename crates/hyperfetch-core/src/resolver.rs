use std::collections::HashSet;
use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use base64::Engine as _;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_DISPOSITION, CONTENT_TYPE, USER_AGENT};
use reqwest::{Client, Response};
use serde::Deserialize;
use url::Url;
use thiserror::Error;

/// Upper bound for one resolver, including every request it makes and its body reads.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

/// Upper bound on how much of a landing page is read for scraping.
const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;

/// Last-segment extensions of URLs that are web pages rather than files.
const PAGE_EXTENSIONS: &[&str] = &["html", "htm", "shtml", "xhtml", "php", "asp", "aspx", "jsp", "cfm"];

/// Extensions of media URLs the engine can download (HLS playlists go to the HLS engine).
const MEDIA_EXTENSIONS: &[&str] = &["mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "ogv", "ts", "m3u8"];

/// `<meta property|name=...>` keys whose `content` points at the page's video.
const META_VIDEO_KEYS: &[&str] = &["og:video", "og:video:url", "og:video:secure_url", "twitter:player:stream"];

/// JSON keys used by custom players (Zoom recordings, Panopto, Loom, ...) for the video file.
const JSON_VIDEO_KEYS: &[&str] = &["viewMp4Url", "downloadUrl", "videoUrl", "video_url", "contentUrl", "stream_url", "fileUrl"];

const SOURCEFORGE_MIRRORS: &[&str] = &["autoselect", "netix", "phoenixnap", "netcologne", "jaist", "liquidtelecom"];

/// "You are leaving this site" links: their hosts (a trailing `*` for any of the site's country
/// domains, see `country_domain`), their path (trailing slash aside; `None` for any) and the query
/// parameters that may hold the target, in the order they are tried.
const REDIRECT_WRAPPERS: &[(&[&str], Option<&str>, &[&str])] = &[
    (&["youtube.com", "www.youtube.com", "m.youtube.com"], Some("/redirect"), &["q"]),
    // google.com, and google.co.uk, google.de, ... where Google Search runs outside the US.
    (&["google.*", "www.google.*"], Some("/url"), &["q", "url"]),
    (&["l.facebook.com", "lm.facebook.com", "l.messenger.com"], Some("/l.php"), &["u"]),
    (&["l.instagram.com", "l.threads.net", "l.threads.com"], Some(""), &["u"]),
    // `url` is the parameter older links carry.
    (&["steamcommunity.com"], Some("/linkfilter"), &["u", "url"]),
    (&["out.reddit.com"], None, &["url"]),
    (&["linkedin.com", "www.linkedin.com"], Some("/redir/redirect"), &["url"]),
    // LinkedIn's for links in messages.
    (&["linkedin.com", "www.linkedin.com"], Some("/safety/go"), &["url"]),
    (&["vk.com", "m.vk.com"], Some("/away.php"), &["to"]),
    (&["duckduckgo.com"], Some("/l"), &["uddg"]),
    (&["t.umblr.com"], Some("/redirect"), &["z"]),
    // SoundCloud's.
    (&["gate.sc"], Some(""), &["url"]),
];

/// How many wrappers around one link are taken off at most.
const MAX_UNWRAPS: usize = 5;

/// Last-segment extensions that name a file, never a web page (see `check_answer`).
const FILE_EXTENSIONS: &[&str] = &[
    // Archives, and the parts of split ones
    "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "zst", "lzma", "lz4", "cab", "001",
    // Installers and packages
    "exe", "msi", "msix", "msixbundle", "appx", "appxbundle", "dmg", "pkg", "deb", "rpm", "apk", "xapk", "ipa",
    "appimage", "flatpak", "snap", "jar", "whl", "nupkg", "vsix", "xpi", "crx",
    // Disk images
    "iso", "img", "vhd", "vhdx", "vmdk", "qcow2", "ova", "chd",
    // Audio and video
    "mp3", "flac", "wav", "m4a", "m4b", "aac", "ogg", "opus", "wma", "mka", "mp4", "m4v", "mkv", "webm", "mov",
    "avi", "wmv", "flv", "mpg", "mpeg", "ts", "m2ts", "ogv", "3gp",
    // Images
    "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", "tif", "tiff", "bmp", "psd",
    // Fonts
    "ttf", "otf", "woff", "woff2",
    // Documents
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "epub", "mobi", "djvu",
    "cbz", "cbr",
    // Model weights
    "safetensors", "gguf", "ckpt", "pt", "pth", "bin", "onnx", "h5",
    // Documents that list downloads
    "torrent", "metalink", "meta4",
];

/// File-share services not supported yet, by the domains (subdomains included) of their pages;
/// those of their pages yt-dlp downloads aside (see `yt_dlp_share_page`).
const UNSUPPORTED_SHARES: &[(&str, &[&str])] = &[
    ("MEGA", &["mega.nz", "mega.io", "mega.co.nz"]),
    ("OneDrive", &["1drv.ms", "onedrive.live.com"]),
    ("SharePoint", &["sharepoint.com"]),
    ("WeTransfer", &["wetransfer.com", "we.tl"]),
    (
        "Terabox",
        &[
            "terabox.com", "terabox.app", "terabox.fun", "teraboxapp.com", "teraboxlink.com", "teraboxshare.com",
            "terafileshare.com", "terasharelink.com", "1024tera.com", "1024tera.co", "1024terabox.com", "4funbox.com",
            "4funbox.co", "mirrobox.com", "nephobox.com", "freeterabox.com", "momerybox.com", "tibibox.com",
        ],
    ),
    ("Gofile", &["gofile.io"]),
    ("Pixeldrain", &["pixeldrain.com", "pixeldrain.net", "pixeldra.in"]),
    ("iCloud", &["icloud.com"]),
    (
        "Yandex Disk",
        &[
            "yadi.sk", "disk.yandex.ru", "disk.yandex.com", "disk.yandex.com.tr", "disk.yandex.by", "disk.yandex.kz",
            "disk.yandex.ua", "disk.360.yandex.ru", "disk.360.yandex.com",
        ],
    ),
    ("pCloud", &["pcloud.link", "pcloud.com"]),
    ("Box", &["box.com"]),
];

#[derive(Error, Debug)]
pub enum ResolverError {
    #[error("Network error during resolution: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Parsing error: {0}")]
    Parse(String),
    #[error("Direct download link not found: {0}")]
    NotFound(String),
    #[error("Link resolution timed out after {0}s")]
    Timeout(u64),
}

/// Trait implemented by host-specific resolvers to unpack landing pages,
/// bypass confirmation gates, and discover multi-cluster mirrors.
/// `Ok` is never empty, and every URL in it serves the same bytes.
pub trait HostResolver: Send + Sync {
    fn can_handle(&self, url: &Url) -> bool;
    fn resolve(&self, client: &Client, url: &Url) -> impl Future<Output = Result<Vec<Url>, ResolverError>> + Send;
}

/// Archive.org Multi-Cluster Resolver
pub struct ArchiveOrgResolver;

#[derive(Debug, Deserialize)]
struct ArchiveServerDir {
    server: Option<String>,
    dir: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArchiveAlternateLocations {
    servers: Option<Vec<ArchiveServerDir>>,
    workable: Option<Vec<ArchiveServerDir>>,
}

/// What the resolver reads of an item's metadata. The item's servers (`server`, `d1`, `d2`) are
/// left out: `workable_servers` lists those of them that are up, the only ones a download can use.
#[derive(Debug)]
struct ArchiveMetadata {
    dir: Option<String>,
    workable_servers: Option<Vec<String>>,
    alternate_locations: Option<ArchiveAlternateLocations>,
}

/// archive.org's item metadata: `/{id}` is the whole record (megabytes for an item with thousands
/// of files: 43 MB and 18 s for one with 180,000), `/{id}/{field}` one field of it.
const ARCHIVE_METADATA: &str = "https://archive.org/metadata";

/// How long the files of one item resolve with the metadata fetched for the first of them.
const ARCHIVE_METADATA_REUSE: Duration = Duration::from_secs(60);

/// One field of an item's metadata: `{"result": ...}`, or `{"error": ...}` when the item or the
/// field does not exist.
#[derive(Debug, Deserialize)]
struct ArchiveField<T> {
    result: Option<T>,
}

type SharedMetadata = std::sync::Arc<tokio::sync::OnceCell<std::sync::Arc<ArchiveMetadata>>>;

impl ArchiveMetadata {
    /// The metadata of item `identifier` at `base`, fetched once for every file of the item that
    /// resolves within [`ARCHIVE_METADATA_REUSE`] of the first: once a client has sent a dozen
    /// metadata requests, archive.org answers about one a second, so a batch of files from one
    /// item must not ask again for each file. A failed fetch is not kept.
    async fn shared(client: &Client, base: &str, identifier: &str) -> Result<std::sync::Arc<Self>, ResolverError> {
        static RECENT: parking_lot::Mutex<Vec<(String, std::time::Instant, SharedMetadata)>> =
            parking_lot::const_mutex(Vec::new());
        let item = format!("{}/{}", base, identifier);
        let metadata = {
            let mut recent = RECENT.lock();
            recent.retain(|(_, first, _)| first.elapsed() < ARCHIVE_METADATA_REUSE);
            match recent.iter().find(|(known, _, _)| *known == item) {
                Some((_, _, metadata)) => metadata.clone(),
                None => {
                    let metadata = SharedMetadata::default();
                    recent.push((item, std::time::Instant::now(), metadata.clone()));
                    metadata
                }
            }
        };
        metadata
            .get_or_try_init(|| async { Self::fetch(client, base, identifier).await.map(std::sync::Arc::new) })
            .await
            .cloned()
    }

    /// Fetches the fields the resolver reads, all at once, from the metadata at `base`.
    async fn fetch(client: &Client, base: &str, identifier: &str) -> Result<Self, ResolverError> {
        async fn field<T: serde::de::DeserializeOwned>(
            client: &Client,
            base: &str,
            identifier: &str,
            name: &str,
        ) -> Result<Option<T>, ResolverError> {
            let url = format!("{}/{}/{}", base, identifier, name);
            let bytes = client.get(&url).send().await?.error_for_status()?.bytes().await?;
            let field: ArchiveField<T> = serde_json::from_slice(&bytes)
                .map_err(|e| ResolverError::Parse(format!("archive.org metadata {}: {}", name, e)))?;
            Ok(field.result)
        }
        let (dir, workable_servers, alternate_locations) = tokio::try_join!(
            field(client, base, identifier, "dir"),
            field(client, base, identifier, "workable_servers"),
            field(client, base, identifier, "alternate_locations"),
        )?;
        Ok(Self { dir, workable_servers, alternate_locations })
    }
}

/// Splits an archive.org file URL into (item identifier, percent-encoded file path).
/// Accepts `/download/{id}/{file}` and data-node paths `[/{n}]/items/{id}/{file}`; never Wayback URLs.
fn archive_item_path(url: &Url) -> Option<(String, String)> {
    let host = url.host_str()?;
    if !(host == "archive.org" || host.ends_with(".archive.org")) || host == "web.archive.org" {
        return None;
    }
    let segs: Vec<&str> = url.path_segments()?.collect();
    let rest = match segs.as_slice() {
        ["download", rest @ ..] | ["items", rest @ ..] => rest,
        [n, "items", rest @ ..] if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => rest,
        _ => return None,
    };
    let [id, file @ ..] = rest else { return None };
    if id.is_empty() || file.iter().all(|s| s.is_empty()) {
        return None;
    }
    Some((id.to_string(), file.join("/")))
}

impl HostResolver for ArchiveOrgResolver {
    fn can_handle(&self, url: &Url) -> bool {
        archive_item_path(url).is_some()
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let (identifier, file) = archive_item_path(url)
            .ok_or_else(|| ResolverError::Parse(format!("Not an archive.org file URL: {}", url)))?;

        let metadata = ArchiveMetadata::shared(client, ARCHIVE_METADATA, &identifier).await?;

        let mut server_dirs: Vec<(String, String)> = Vec::new();
        let mut seen = HashSet::new();
        let mut add = |s: &String, d: &String| {
            if seen.insert((s.clone(), d.clone())) {
                server_dirs.push((s.clone(), d.clone()));
            }
        };

        if let Some(ref d) = metadata.dir {
            for s in metadata.workable_servers.iter().flatten() {
                add(s, d);
            }
        }
        if let Some(ref alt) = metadata.alternate_locations {
            for entry in alt.servers.iter().flatten().chain(alt.workable.iter().flatten()) {
                if let (Some(s), Some(d)) = (&entry.server, &entry.dir) {
                    add(s, d);
                }
            }
        }

        let mut mirror_urls = Vec::new();
        for (s, d) in server_dirs {
            let host = if s.contains('.') { s } else { format!("{}.archive.org", s) };
            let dir = d.trim_matches('/');
            let mirror_str = if dir.is_empty() {
                format!("https://{}/{}", host, file)
            } else {
                format!("https://{}/{}/{}", host, dir, file)
            };
            if let Ok(m_url) = Url::parse(&mirror_str) {
                mirror_urls.push(m_url);
            }
        }

        let lb_url_str = format!("https://archive.org/download/{}/{}", identifier, file);
        for candidate in [Url::parse(&lb_url_str).ok(), Some(url.clone())].into_iter().flatten() {
            if !mirror_urls.contains(&candidate) {
                mirror_urls.push(candidate);
            }
        }

        Ok(mirror_urls)
    }
}

/// Google Drive Resolver (Auto-bypasses virus scan warnings on large files)
pub struct GoogleDriveResolver;

impl HostResolver for GoogleDriveResolver {
    fn can_handle(&self, url: &Url) -> bool {
        match url.host_str() {
            Some("drive.google.com" | "drive.usercontent.google.com") => true,
            // Older Drive links: docs.google.com/uc?id=... and /file/d/{id}/...
            Some("docs.google.com") => url.path() == "/uc" || url.path().starts_with("/file/d/"),
            _ => false,
        }
    }

    /// The direct download URL, sending nothing: whether Drive serves the file there shows in the
    /// download's own probe (see `check_answer`).
    async fn resolve(&self, _client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let file_id = extract_google_drive_id(url)
            .ok_or_else(|| ResolverError::Parse("Could not extract Google Drive file ID".to_string()))?;
        Ok(vec![google_drive_direct_url(&file_id)?])
    }
}

/// The target of a "you are leaving this site" link (youtube.com/redirect?q=, google.com/url?q=,
/// l.facebook.com/l.php?u=, ...), read from the link itself without a request; None when `url` is
/// not such a link. Wrappers of wrappers are unwrapped too; the result is always http(s).
pub fn unwrap_redirect(url: &Url) -> Option<Url> {
    let mut target = unwrap_once(url)?;
    for _ in 1..MAX_UNWRAPS {
        match unwrap_once(&target) {
            Some(inner) => target = inner,
            None => break,
        }
    }
    Some(target)
}

/// The http(s) target one wrapper in [`REDIRECT_WRAPPERS`] holds, if `url` is one.
fn unwrap_once(url: &Url) -> Option<Url> {
    let host = url.host_str()?.trim_end_matches('.');
    let path = url.path().trim_end_matches('/');
    let listed = |pattern: &&str| match pattern.strip_suffix('*') {
        Some(stem) => host.strip_prefix(stem).is_some_and(country_domain),
        None => host == *pattern,
    };
    let (_, _, params) = REDIRECT_WRAPPERS
        .iter()
        .find(|(hosts, wrapper_path, _)| hosts.iter().any(listed) && wrapper_path.is_none_or(|p| p == path))?;
    params.iter().find_map(|name| {
        let (_, value) = url.query_pairs().find(|(key, _)| key == name)?;
        let target = Url::parse(value.trim()).ok()?;
        matches!(target.scheme(), "http" | "https").then_some(target)
    })
}

/// Whether `domain` ends a host as a site's country domains do: `com`, `de`, `co.uk`, `com.au`.
fn country_domain(domain: &str) -> bool {
    let tld = domain.strip_prefix("co.").or_else(|| domain.strip_prefix("com.")).unwrap_or(domain);
    !tld.is_empty() && tld.bytes().all(|b| b.is_ascii_lowercase())
}

/// Fails if the answer to `url` (which ended at `final_url`), with these headers, is a web page
/// instead of the file it stands for: HTML not sent as an attachment (a hosted .html file is
/// one) from Drive, from Google asking to sign in to a document, for a code-host folder, for a
/// link that names a file, or from a file-share service not supported yet. Other pages are looked
/// into for a video.
pub fn check_answer(url: &Url, final_url: &Url, headers: &HeaderMap) -> Result<(), ResolverError> {
    GoogleDriveResolver::check_answer(url, headers)?;
    if !html_type(headers) || is_attachment(headers) {
        return Ok(());
    }
    if GoogleDocsResolver.can_handle(url) || final_url.host_str() == Some("accounts.google.com") {
        return Err(ResolverError::NotFound(
            "Google asked to sign in: the document is private (share it as \"Anyone with the link\", or pass your browser's cookies with --load-cookies)"
                .to_string(),
        ));
    }
    if code_host_folder(url) || code_host_folder(final_url) {
        return Err(ResolverError::NotFound(
            "the link leads to a folder, not a file (link one of the files in it instead)".to_string(),
        ));
    }
    // Before the services: a link of theirs that names a file is one to the file, gone stale.
    if let Some(file) = named_file(url).or_else(|| named_file(final_url)) {
        return Err(ResolverError::NotFound(format!(
            "the server sent a web page instead of {} (the link may need a login, have expired, or lead to a download page)",
            file
        )));
    }
    if let Some(service) = unsupported_share_answer(url, final_url) {
        return Err(ResolverError::NotFound(format!("{} links are not supported yet", service)));
    }
    Ok(())
}

/// The file-share service in [`UNSUPPORTED_SHARES`] whose page `url` is on, if any.
pub(crate) fn unsupported_share(url: &Url) -> Option<&'static str> {
    let host = url.host_str()?.trim_end_matches('.');
    let on = |domain: &&str| host == *domain || host.strip_suffix(domain).is_some_and(|sub| sub.ends_with('.'));
    UNSUPPORTED_SHARES.iter().find(|(_, domains)| domains.iter().any(on)).map(|(service, _)| *service)
}

/// The file-share service not supported yet that answered with a page: where the answer ended,
/// unless yt-dlp downloads that page (see [`yt_dlp_share_page`]), else where it began, when the
/// service sent it on elsewhere (to a sign-in page).
fn unsupported_share_answer(url: &Url, final_url: &Url) -> Option<&'static str> {
    match unsupported_share(final_url) {
        Some(_) if yt_dlp_share_page(final_url) => None,
        Some(service) => Some(service),
        None => unsupported_share(url),
    }
}

/// Whether `url` is a share page of a service in [`UNSUPPORTED_SHARES`] that yt-dlp's own
/// extractor for it takes (as its URL patterns match them): Box shares, SharePoint videos, and
/// Yandex Disk shares. Such a page goes on to the page handling, which hands it to yt-dlp.
fn yt_dlp_share_page(url: &Url) -> bool {
    let (host, path) = (url.host_str().unwrap_or_default(), url.path());
    let has = |key: &str| url.query_pairs().any(|(k, _)| k == key);
    match unsupported_share(url) {
        // (app|ent).box.com/s/..., under a company's subdomain or not.
        Some("Box") => {
            let sub = host.strip_suffix(".box.com").unwrap_or_default();
            matches!(sub.rsplit('.').next(), Some("app" | "ent")) && path.starts_with("/s/")
        }
        Some("SharePoint") => path.starts_with("/:v:/") || (path.ends_with("/stream.aspx") && has("id")),
        Some("Yandex Disk") => {
            path.starts_with("/d/") || path.starts_with("/i/") || (path.starts_with("/public") && has("hash"))
        }
        _ => false,
    }
}

/// Whether `url` is a folder page on a code host, which lists files instead of being one:
/// GitHub /{owner}/{repo}/tree/..., GitLab /{namespace...}/-/tree/..., Hugging Face
/// [/datasets|/spaces]/{owner}/{repo}/tree/... (a file link to a folder ends there too).
fn code_host_folder(url: &Url) -> bool {
    let Some(segs) = url.path_segments().map(|s| s.collect::<Vec<_>>()) else { return false };
    // "tree" then the ref.
    let tree_at = |i: usize| segs.get(i) == Some(&"tree") && segs.get(i + 1).is_some_and(|r| !r.is_empty());
    match url.host_str().unwrap_or_default().trim_end_matches('.') {
        "github.com" => tree_at(2),
        "gitlab.com" => (3..segs.len()).any(|i| segs[i - 1] == "-" && tree_at(i)),
        "huggingface.co" | "hf.co" => {
            let start = usize::from(matches!(segs.first(), Some(&("datasets" | "spaces"))));
            tree_at(start + 2) || tree_at(start + 1)
        }
        _ => false,
    }
}

/// Whether `start`, the first bytes of an answer, show it is no web page whatever its headers
/// say, as a server that labels every file HTML still sends the file: binary data (a NUL byte,
/// or bytes that are no UTF-8 text) that does not open with markup. No bytes show nothing.
pub fn start_is_no_page(start: &[u8]) -> bool {
    let head = &start[..start.len().min(512)];
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let Some(&first) = head.iter().find(|b| !b.is_ascii_whitespace()) else { return false };
    // A character the 512 bytes cut in two is still text.
    let binary = head.contains(&0) || std::str::from_utf8(head).is_err_and(|e| e.error_len().is_some());
    first != b'<' && binary
}

/// The name of the file `url` names by one of the [`FILE_EXTENSIONS`], if it does.
fn named_file(url: &Url) -> Option<String> {
    let ext = last_segment_extension(url)?;
    let last = url.path_segments()?.next_back()?;
    FILE_EXTENSIONS
        .contains(&ext.as_str())
        .then(|| percent_encoding::percent_decode_str(last).decode_utf8_lossy().into_owned())
}

/// Whether `url`, which no host resolver takes, ended at `final_url` on another host that one
/// does, or that is a media site: the download is then that of `final_url` (a shortened link).
pub fn lands_elsewhere(url: &Url, final_url: &Url) -> bool {
    url.host_str() != final_url.host_str()
        && !SmartResolver::handles(url)
        && (SmartResolver::handles(final_url) || crate::media::is_supported_media_site(final_url))
}

impl GoogleDriveResolver {
    /// Fails if `url` is Drive's and its answer, with these headers, is a web page instead of the
    /// file: HTML not sent as an attachment (a hosted .html file is one).
    pub fn check_answer(url: &Url, headers: &HeaderMap) -> Result<(), ResolverError> {
        if GoogleDriveResolver.can_handle(url) && html_type(headers) && !is_attachment(headers) {
            return Err(ResolverError::NotFound(
                "Google Drive served a web page instead of the file (it may be private, deleted, or over its download quota)"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// The usercontent download endpoint serves every file size directly; `confirm=t` skips the
/// virus-scan interstitial that large files otherwise get.
fn google_drive_direct_url(file_id: &str) -> Result<Url, ResolverError> {
    Url::parse_with_params(
        "https://drive.usercontent.google.com/download",
        &[("id", file_id), ("export", "download"), ("confirm", "t")],
    )
    .map_err(|e| ResolverError::Parse(e.to_string()))
}

/// Google Docs Resolver (Turns a Docs, Sheets or Slides link into the document exported as a file)
pub struct GoogleDocsResolver;

/// The pages of a document the Docs, Sheets and Slides apps show (after `/d/{id}/`), which stand
/// for the document itself.
const GOOGLE_DOCS_VIEWS: &[&str] = &["edit", "view", "preview", "htmlview", "mobilebasic", "present", "embed", "copy", "comment"];

/// The export of the document `url` links to, rewritten without a request, as Google's own
/// File > Download does it: a document as .docx, a spreadsheet as .xlsx with every sheet (the
/// sheet a link names, `gid`, is only the one the editor opened on, and Sheets names one in every
/// editor link), a presentation as .pptx; the account it names (`authuser`) is kept. An export
/// link is kept in the format it asks for (`format=csv&gid=` for one sheet). Other links on a document (gviz
/// queries, /pub copies) already give what they are for, so they are left alone. Exports are
/// made on the fly: one connection, no known size.
fn google_docs_export(url: &Url) -> Option<Url> {
    if url.host_str()? != "docs.google.com" {
        return None;
    }
    let segs: Vec<&str> = url.path_segments()?.collect();
    let (&kind, rest) = segs.split_first()?;
    let format = match kind {
        "document" => "docx",
        "spreadsheets" => "xlsx",
        "presentation" => "pptx",
        _ => return None,
    };
    // /u/{n}/ picks one of the signed-in accounts (the cookies passed); it is kept.
    let (account, rest) = match rest {
        ["u", n, rest @ ..] if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => (format!("/u/{}", n), rest),
        _ => (String::new(), rest),
    };
    // /d/e/{id} is a published copy: a web page, with no export.
    let ["d", id, rest @ ..] = rest else { return None };
    if id.is_empty() || *id == "e" {
        return None;
    }
    let mut export = url.clone();
    export.set_fragment(None);
    match rest {
        ["export", ..] => return Some(export),
        [] | [""] => {}
        [view] | [view, ""] if GOOGLE_DOCS_VIEWS.contains(view) => {}
        _ => return None,
    }
    export.set_path(&format!("/{}{}/d/{}/export", kind, account, id));
    let authuser = url.query_pairs().find(|(k, _)| k == "authuser").map(|(_, v)| v.into_owned());
    {
        let mut query = export.query_pairs_mut();
        query.clear().append_pair("format", format);
        if let Some(user) = authuser {
            query.append_pair("authuser", &user);
        }
    }
    Some(export)
}

impl HostResolver for GoogleDocsResolver {
    fn can_handle(&self, url: &Url) -> bool {
        google_docs_export(url).is_some()
    }

    async fn resolve(&self, _client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let export = google_docs_export(url).ok_or_else(|| ResolverError::Parse(format!("Not a Google Docs document URL: {}", url)))?;
        Ok(vec![export])
    }
}

/// MediaFire Resolver (Bypasses landing page to extract direct CDN link)
pub struct MediaFireResolver;

impl HostResolver for MediaFireResolver {
    fn can_handle(&self, url: &Url) -> bool {
        let path = url.path();
        matches!(url.host_str(), Some("mediafire.com" | "www.mediafire.com"))
            && (path.starts_with("/file/")
                || path.starts_with("/file_premium/")
                || path.starts_with("/download/")
                || url.query().is_some())
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let resp = client.get(url.clone()).send().await?;
        if !resp.status().is_success() {
            return Err(ResolverError::NotFound(format!("MediaFire returned HTTP {}", resp.status())));
        }
        if !is_html(&resp) {
            // The link already streams the file.
            return Ok(vec![resp.url().clone()]);
        }
        let page_url = resp.url().clone();
        let html = read_capped(resp, MAX_HTML_BYTES).await?;
        extract_mediafire_direct(&html, &page_url).map(|u| vec![u]).ok_or_else(|| {
            ResolverError::NotFound(
                "MediaFire download link not found on the page (the file may be removed or the page layout changed)".to_string(),
            )
        })
    }
}

/// Dropbox Resolver (Normalizes preview URLs to raw binary streaming links)
pub struct DropboxResolver;

impl HostResolver for DropboxResolver {
    fn can_handle(&self, url: &Url) -> bool {
        matches!(url.host_str(), Some("dropbox.com" | "www.dropbox.com"))
    }

    async fn resolve(&self, _client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let mut resolved = url.clone();

        // Replace dl=0 with dl=1
        let mut pairs: Vec<(String, String)> = resolved.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        let mut found = false;
        for (k, v) in &mut pairs {
            if k == "dl" {
                *v = "1".to_string();
                found = true;
            }
        }
        if !found {
            pairs.push(("dl".to_string(), "1".to_string()));
        }

        resolved.query_pairs_mut().clear().extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        Ok(vec![resolved])
    }
}

/// SourceForge Resolver (Generates direct mirror-CDN download URLs)
pub struct SourceForgeResolver;

/// Splits `/projects/{project}/files/{path}[/download]` into (project, percent-encoded file path).
/// Folder listings (trailing slash) and the moving `latest` alias are left alone.
fn sourceforge_file_path(url: &Url) -> Option<(String, String)> {
    if url.path().ends_with('/') {
        return None;
    }
    let segs: Vec<&str> = url.path_segments()?.collect();
    let ["projects", project, "files", rest @ ..] = segs.as_slice() else { return None };
    let rest = rest.strip_suffix(&["download"]).unwrap_or(rest);
    if project.is_empty() || rest.is_empty() || rest == ["latest"] || rest.iter().any(|s| s.is_empty()) {
        return None;
    }
    Some((project.to_string(), rest.join("/")))
}

impl HostResolver for SourceForgeResolver {
    fn can_handle(&self, url: &Url) -> bool {
        matches!(url.host_str(), Some("sourceforge.net" | "www.sourceforge.net")) && sourceforge_file_path(url).is_some()
    }

    async fn resolve(&self, _client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let (project, file) = sourceforge_file_path(url)
            .ok_or_else(|| ResolverError::Parse(format!("Not a SourceForge file URL: {}", url)))?;
        // downloads.sourceforge.net always redirects straight to the mirror, never to the HTML
        // "your download will start shortly" page that sourceforge.net serves to browsers.
        let base = Url::parse(&format!("https://downloads.sourceforge.net/project/{}/{}", project, file))
            .map_err(|e| ResolverError::Parse(e.to_string()))?;
        Ok(SOURCEFORGE_MIRRORS
            .iter()
            .map(|code| {
                let mut mirror = base.clone();
                mirror.query_pairs_mut().append_pair("use_mirror", code);
                mirror
            })
            .collect())
    }
}

/// Code Host Resolver (Turns a "view file" page on GitHub, GitLab, Codeberg, Bitbucket or Hugging
/// Face into the file's own download link)
pub struct CodeHostResolver;

/// The download link of the file whose "view file" page `url` is, rewritten without a request:
/// the one path segment naming the view becomes the one naming the file's bytes. Folder pages
/// ("tree", or a trailing slash) are left alone.
fn code_host_raw_url(url: &Url) -> Option<Url> {
    let mut segs: Vec<&str> = url.path_segments()?.collect();
    // Where the segment naming the view is, what it becomes, and how many segments name the ref.
    let (at, raw, refs) = match url.host_str()?.trim_end_matches('.') {
        // /{owner}/{repo}/blob/{ref}/{path}; /raw/ also serves Git LFS files.
        "github.com" if segs.get(2) == Some(&"blob") => (2, "raw", 1),
        // /{namespace...}/-/blob/{ref}/{path}, or /{namespace...}/blob/{ref}/{path} as older
        // links have it (GitLab reserves "blob" as a project name).
        "gitlab.com" => match segs.windows(2).skip(2).position(|w| w == ["-", "blob"]) {
            Some(i) => (i + 3, "raw", 1),
            None => {
                let at = segs.iter().skip(2).position(|s| *s == "blob")? + 2;
                (!segs[..at].contains(&"-")).then_some((at, "-/raw", 1))?
            }
        },
        // /{owner}/{repo}/src/{branch|tag|commit}/{ref}/{path}. /media/ serves what /raw/ does,
        // but for a Git LFS file the file itself instead of its pointer.
        "codeberg.org" if segs.get(2) == Some(&"src") && matches!(segs.get(3), Some(&("branch" | "tag" | "commit"))) => {
            (2, "media", 2)
        }
        // /{owner}/{repo}/src/{ref}/{path}
        "bitbucket.org" if segs.get(2) == Some(&"src") => (2, "raw", 1),
        // [/datasets|/spaces]/{owner}/{repo}/blob/{ref}/{path}, or {repo} alone for older repos.
        // /raw/ serves the file too, but a Git LFS file (model weights) as its pointer.
        "huggingface.co" | "hf.co" => {
            let start = usize::from(matches!(segs.first(), Some(&("datasets" | "spaces"))));
            let view = |&i: &usize| matches!(segs.get(i), Some(&("blob" | "raw")));
            ([start + 2, start + 1].into_iter().find(view)?, "resolve", 1)
        }
        _ => return None,
    };
    // The ref, then a path whose last segment names a file.
    if segs.len() <= at + refs + 1 || segs.last().is_none_or(|s| s.is_empty()) {
        return None;
    }
    segs[at] = raw;
    let mut raw_url = url.clone();
    raw_url.set_path(&format!("/{}", segs.join("/")));
    raw_url.set_fragment(None);
    Some(raw_url)
}

impl HostResolver for CodeHostResolver {
    fn can_handle(&self, url: &Url) -> bool {
        code_host_raw_url(url).is_some()
    }

    /// GitHub's /raw/ does not follow a renamed branch (master to main) or repository as its file
    /// page does, so a GitHub link is rewritten from where its page redirects; one whose page does
    /// not answer in time, or not with success (a private repository), is rewritten as it is.
    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let mut page = url.clone();
        if url.host_str() == Some("github.com") {
            match tokio::time::timeout(RESOLVE_TIMEOUT, client.head(url.clone()).send()).await {
                Ok(Ok(resp)) if resp.status().is_success() && code_host_raw_url(resp.url()).is_some() => {
                    page = resp.url().clone()
                }
                Ok(Ok(resp)) => tracing::debug!("{} answered HTTP {}; rewriting it as it is", url, resp.status()),
                Ok(Err(e)) => tracing::debug!("{} did not answer ({}); rewriting it as it is", url, e),
                Err(_) => tracing::debug!("{} did not answer in time; rewriting it as it is", url),
            }
        }
        let raw = code_host_raw_url(&page).ok_or_else(|| ResolverError::Parse(format!("Not a code host file URL: {}", url)))?;
        Ok(vec![raw])
    }
}

/// HTML5 Video Extractor (Finds the video a web page plays via <video>, <source>, OpenGraph or player JSON)
pub struct HtmlVideoResolver;

impl HostResolver for HtmlVideoResolver {
    /// Only URLs that can be web pages: no extension on the last path segment, or a page extension.
    fn can_handle(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
            && match last_segment_extension(url) {
                None => true,
                Some(ext) => PAGE_EXTENSIONS.contains(&ext.as_str()),
            }
    }

    /// The URL itself, sending nothing: whether it is a web page, and which video it plays, shows
    /// in its answer, which the download's own probe fetches (see `is_page`).
    async fn resolve(&self, _client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        Ok(vec![url.clone()])
    }
}

impl HtmlVideoResolver {
    /// Whether the answer to `url`, with these headers, is a web page to look into for the video
    /// it plays: HTML, for a URL this resolver would be handed (one that can be a page, and that
    /// no resolver `SmartResolver` tries first takes).
    pub fn is_page(url: &Url, headers: &HeaderMap) -> bool {
        !SmartResolver::handles(url) && HtmlVideoResolver.can_handle(url) && html_type(headers)
    }

    /// Where the page `html`, from `page_url`, sends the browser at once: the target of a
    /// `<meta http-equiv="refresh" content="0; url=...">` (t.co answers this way), if it has one.
    /// One inside `<noscript>`, for browsers without JavaScript, counts only when it leads to
    /// another site: on the same one it is a page asking for JavaScript (Google's answers so).
    pub fn meta_refresh(html: &str, page_url: &Url) -> Option<Url> {
        let lower = html.to_ascii_lowercase();
        start_tags(html, &lower, "meta").into_iter().find_map(|tag| {
            attr_value(tag, "http-equiv").filter(|v| v.eq_ignore_ascii_case("refresh"))?;
            // "<seconds>[.<fraction>][;|,] [url=]<target>", the target maybe quoted, as the HTML
            // standard reads it. Only a refresh at once: a later one is a page to look at first.
            let content = attr_value(tag, "content")?;
            let rest = content.trim_start();
            let seconds = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
            if seconds == 0 || rest[..seconds].bytes().any(|b| b != b'0') {
                return None;
            }
            let rest = rest[seconds..].trim_start_matches(|c: char| c.is_ascii_digit() || c == '.').trim_start();
            let rest = rest.strip_prefix([';', ',']).unwrap_or(rest).trim_start();
            let named = rest.get(..3).filter(|s| s.eq_ignore_ascii_case("url")).and_then(|_| rest[3..].trim_start().strip_prefix('='));
            let target = named.map_or(rest, str::trim_start);
            let target = match target.chars().next() {
                Some(quote @ ('\'' | '"')) => target[1..].split(quote).next().unwrap_or_default(),
                _ => target,
            };
            let target = target.trim();
            // Without a target the page only reloads itself.
            if target.is_empty() {
                return None;
            }
            let target = page_url.join(target).ok().filter(|u| matches!(u.scheme(), "http" | "https"))?;
            // `tag` is a slice of `html`, which `lower` matches byte for byte.
            let before = &lower[..tag.as_ptr() as usize - html.as_ptr() as usize];
            let in_noscript = before.rfind("<noscript") > before.rfind("</noscript");
            (!in_noscript || target.origin() != page_url.origin()).then_some(target)
        })
    }

    /// Hosts of link shorteners and mail link scanners: they answer a link with a redirect, so a
    /// page from one is a preview or a warning, never what the link stands for. `*` stands for
    /// any part of a host name. t.co is not among them: its page sends the browser on at once
    /// (see `meta_refresh`).
    pub const SHORTENER_HOSTS: &[&str] = &[
        "bit.ly",
        "bitly.com",
        "tinyurl.com",
        "is.gd",
        "ow.ly",
        "buff.ly",
        "cutt.ly",
        "rb.gy",
        "goo.gl",
        "shorturl.at",
        "lnkd.in",
        "*.safelinks.protection.outlook.com",
        "urldefense.com",
        "urldefense.proofpoint.com",
        "protect-*.mimecast.com",
        "*.mimecastprotect.com",
    ];

    /// Reads as much of a page's body as is looked into.
    pub async fn read_page(resp: Response) -> Result<String, ResolverError> {
        read_capped(resp, MAX_HTML_BYTES).await
    }

    /// The video file the page `html`, from `page_url`, plays, if one is found.
    pub fn video_in(html: &str, page_url: &Url) -> Option<Url> {
        let video = extract_html_video_source(html, page_url)?;
        tracing::info!("HtmlVideoResolver: {} plays {}", page_url, video);
        Some(video)
    }
}

/// Master Smart Resolver registry that chains all host resolvers
pub struct SmartResolver;

impl SmartResolver {
    /// Whether a host resolver (not the generic page one) takes `url`.
    pub fn handles(url: &Url) -> bool {
        GoogleDriveResolver.can_handle(url)
            || GoogleDocsResolver.can_handle(url)
            || MediaFireResolver.can_handle(url)
            || DropboxResolver.can_handle(url)
            || SourceForgeResolver.can_handle(url)
            || CodeHostResolver.can_handle(url)
            || ArchiveOrgResolver.can_handle(url)
    }

    /// Resolves `url` into download sources that are byte-identical copies of one file.
    /// `Ok` is never empty. `Err` means a resolver recognized the host but could not extract a direct link.
    pub async fn resolve(client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        // The landing pages of these hosts are never the file, so a failed extraction is an error.
        if GoogleDriveResolver.can_handle(url) {
            return with_timeout(GoogleDriveResolver.resolve(client, url)).await;
        }
        if GoogleDocsResolver.can_handle(url) {
            return GoogleDocsResolver.resolve(client, url).await;
        }
        if MediaFireResolver.can_handle(url) {
            return with_timeout(MediaFireResolver.resolve(client, url)).await;
        }
        if DropboxResolver.can_handle(url) {
            return DropboxResolver.resolve(client, url).await;
        }
        if SourceForgeResolver.can_handle(url) {
            return SourceForgeResolver.resolve(client, url).await;
        }
        if CodeHostResolver.can_handle(url) {
            return CodeHostResolver.resolve(client, url).await;
        }

        // These only add mirrors or find an embedded stream; the input URL remains a valid source.
        let found = if ArchiveOrgResolver.can_handle(url) {
            with_timeout(ArchiveOrgResolver.resolve(client, url)).await
        } else if HtmlVideoResolver.can_handle(url) {
            with_timeout(HtmlVideoResolver.resolve(client, url)).await
        } else {
            return Ok(vec![url.clone()]);
        };
        Ok(found.unwrap_or_else(|e| {
            tracing::warn!("Link resolution for {} failed, downloading it as given: {}", url, e);
            vec![url.clone()]
        }))
    }

    /// Infallible form of [`SmartResolver::resolve`]: falls back to `url` itself on error.
    pub async fn resolve_mirrors(client: &Client, url: &Url) -> Vec<Url> {
        Self::resolve(client, url).await.unwrap_or_else(|e| {
            tracing::warn!("Link resolution for {} failed: {}", url, e);
            vec![url.clone()]
        })
    }

    /// Generates browser-grade anti-QoS headers to prevent CDNs from throttling traffic.
    pub fn default_anti_qos_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
            ),
        );
        headers.insert("Accept", HeaderValue::from_static("*/*"));
        headers.insert("Accept-Language", HeaderValue::from_static("en-US,en;q=0.9"));
        headers.insert("Sec-Ch-Ua", HeaderValue::from_static("\"Chromium\";v=\"124\", \"Google Chrome\";v=\"124\", \"Not-A.Brand\";v=\"99\""));
        headers.insert("Sec-Ch-Ua-Mobile", HeaderValue::from_static("?0"));
        headers.insert("Sec-Ch-Ua-Platform", HeaderValue::from_static("\"Windows\""));
        headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));
        headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
        headers.insert("Sec-Fetch-Site", HeaderValue::from_static("cross-site"));
        headers
    }
}

/// The app's own User-Agent, for requests that do not pass as a browser's.
pub(crate) const APP_USER_AGENT: &str = concat!("EndosUnifiedDownloader/", env!("CARGO_PKG_VERSION"));

/// Hosts that refuse the browser User-Agent of `SmartResolver::default_anti_qos_headers`, and
/// get [`APP_USER_AGENT`] instead: Codeberg answers /raw/ and /media/ for an older Chrome with
/// "403 Access denied, old Chrome version".
const OWN_AGENT_HOSTS: &[&str] = &["codeberg.org"];

/// `request`, for `url`, with the User-Agent `url`'s host takes.
pub(crate) fn with_agent_for(request: reqwest::RequestBuilder, url: &Url) -> reqwest::RequestBuilder {
    match url.host_str() {
        Some(host) if OWN_AGENT_HOSTS.contains(&host.trim_end_matches('.')) => request.header(USER_AGENT, APP_USER_AGENT),
        _ => request,
    }
}

async fn with_timeout(
    fut: impl Future<Output = Result<Vec<Url>, ResolverError>>,
) -> Result<Vec<Url>, ResolverError> {
    tokio::time::timeout(RESOLVE_TIMEOUT, fut)
        .await
        .unwrap_or(Err(ResolverError::Timeout(RESOLVE_TIMEOUT.as_secs())))
}

/// Parses a Netscape / Mozilla cookies.txt file and loads cookies into a reqwest CookieJar.
pub fn parse_netscape_cookies(content: &str, jar: &reqwest::cookie::Jar) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    // `lines()` already strips "\n" / "\r\n". Tabs are not trimmed: an empty value leaves a trailing tab.
    for line in content.lines() {
        let (line, http_only) = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => (rest, true),
            None if line.trim().is_empty() || line.trim_start().starts_with('#') => continue,
            None => (line, false),
        };

        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 6 {
            continue;
        }
        let domain = fields[0].trim();
        let include_subdomains = fields[1].eq_ignore_ascii_case("true");
        let path = fields[2];
        let secure = fields[3].eq_ignore_ascii_case("true");
        let expires = fields[4].trim().parse::<u64>().unwrap_or(0);
        let name = fields[5];
        let value = fields.get(6).copied().unwrap_or("");

        // Expiry 0 marks a session cookie.
        if name.is_empty() || (expires != 0 && expires <= now) {
            continue;
        }

        let host = domain.trim_start_matches('.');
        let scheme = if secure { "https" } else { "http" };
        let Ok(cookie_url) = Url::parse(&format!("{}://{}{}", scheme, host, path)) else { continue };

        let mut cookie = format!("{}={}; Path={}", name, value, path);
        // Without a Domain attribute the jar keeps the cookie host-only.
        if include_subdomains {
            cookie.push_str("; Domain=");
            cookie.push_str(host);
        }
        if secure {
            cookie.push_str("; Secure");
        }
        if http_only {
            cookie.push_str("; HttpOnly");
        }
        jar.add_cookie_str(&cookie, &cookie_url);
    }
}

fn is_html(resp: &Response) -> bool {
    html_type(resp.headers())
}

/// Whether headers say the body is HTML.
fn html_type(headers: &HeaderMap) -> bool {
    headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|ct| {
        let ct = ct.to_ascii_lowercase();
        ct.contains("text/html") || ct.contains("application/xhtml")
    })
}

/// Reads at most `cap` bytes of the body; the rest is never downloaded.
async fn read_capped(mut resp: Response, cap: usize) -> Result<String, ResolverError> {
    let mut body = Vec::new();
    while body.len() < cap {
        let Some(chunk) = resp.chunk().await? else { break };
        body.extend_from_slice(&chunk[..chunk.len().min(cap - body.len())]);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Whether headers say the body is sent as an attachment.
fn is_attachment(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|cd| cd.trim_start().to_ascii_lowercase().starts_with("attachment"))
}

/// Lower-cased extension of the last path segment, if it has one.
fn last_segment_extension(url: &Url) -> Option<String> {
    let last = url.path_segments()?.next_back()?;
    let (_, ext) = last.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

fn is_media_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && last_segment_extension(url).is_some_and(|ext| MEDIA_EXTENSIONS.contains(&ext.as_str()))
}

fn extract_google_drive_id(url: &Url) -> Option<String> {
    // Matches /file/d/{id}/...
    let path = url.path();
    if let Some(idx) = path.find("/d/") {
        let sub = &path[idx + 3..];
        let id = sub.split('/').next().unwrap_or("");
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }

    // Matches ?id={id}
    for (k, v) in url.query_pairs() {
        if k == "id" && !v.is_empty() {
            return Some(v.to_string());
        }
    }

    None
}

/// Finds the download button's link: a plain `href` to a downloadNNNN.mediafire.com host, or the
/// base64 `data-scrambled-url` newer pages use instead.
fn extract_mediafire_direct(html: &str, page_url: &Url) -> Option<Url> {
    let is_download_host = |u: &Url| {
        matches!(u.scheme(), "http" | "https")
            && u.host_str().is_some_and(|h| h.starts_with("download") && h.ends_with(".mediafire.com"))
    };
    let lower = html.to_ascii_lowercase();
    start_tags(html, &lower, "a").into_iter().find_map(|tag| {
        let href = attr_value(tag, "href").and_then(|h| page_url.join(&h).ok()).filter(is_download_host);
        href.or_else(|| {
            let scrambled = attr_value(tag, "data-scrambled-url")?;
            let decoded = base64::engine::general_purpose::STANDARD.decode(scrambled.trim()).ok()?;
            Url::parse(std::str::from_utf8(&decoded).ok()?).ok().filter(is_download_host)
        })
    })
}

/// Picks the one video a page plays. Candidates are often different encodes of the same video,
/// which are not byte-identical, so only the first media URL found is returned:
/// the page's own <video>/<source> tags, then OpenGraph/Twitter metadata, then player JSON.
fn extract_html_video_source(html: &str, base_url: &Url) -> Option<Url> {
    let lower = html.to_ascii_lowercase();
    let media = |raw: String| base_url.join(&raw).ok().filter(is_media_url);

    for name in ["video", "source"] {
        for tag in start_tags(html, &lower, name) {
            if let Some(url) = attr_value(tag, "src").and_then(media) {
                return Some(url);
            }
        }
    }

    for tag in start_tags(html, &lower, "meta") {
        let key = attr_value(tag, "property").or_else(|| attr_value(tag, "name"));
        if !key.is_some_and(|k| META_VIDEO_KEYS.contains(&k.to_ascii_lowercase().as_str())) {
            continue;
        }
        if let Some(url) = attr_value(tag, "content").and_then(media) {
            return Some(url);
        }
    }

    for key in JSON_VIDEO_KEYS {
        let needle = format!("\"{}\"", key);
        let mut from = 0;
        while let Some(idx) = html[from..].find(&needle) {
            from += idx + needle.len();
            if let Some(url) = json_string_after_key(&html[from..]).and_then(media) {
                return Some(url);
            }
        }
    }

    None
}

/// Given the text right after a JSON object key, returns its string value with every JSON escape decoded.
fn json_string_after_key(after_key: &str) -> Option<String> {
    let value = after_key.trim_start().strip_prefix(':')?.trim_start();
    serde_json::Deserializer::from_str(value).into_iter::<String>().next()?.ok()
}

/// Source text of every `<name ...>` start tag (case-insensitive), up to but excluding `>`.
/// `lower` must be `html.to_ascii_lowercase()`, which keeps byte offsets identical.
fn start_tags<'a>(html: &'a str, lower: &str, name: &str) -> Vec<&'a str> {
    let open = format!("<{}", name);
    let mut tags = Vec::new();
    let mut from = 0;
    while let Some(idx) = lower[from..].find(&open) {
        let start = from + idx;
        let attrs = start + open.len();
        let Some(len) = lower[attrs..].find('>') else { break };
        if lower[attrs..].starts_with(|c: char| c.is_ascii_whitespace()) {
            tags.push(&html[start..attrs + len]);
        }
        from = attrs + len;
    }
    tags
}

/// Value of attribute `attr` (exact name, case-insensitive) in a start tag's source, with `&amp;` decoded.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{}=", attr);
    let mut from = 0;
    while let Some(idx) = lower[from..].find(&needle) {
        let start = from + idx;
        from = start + needle.len();
        // Require a word boundary so `src` does not match `data-src`.
        if start == 0 || !lower.as_bytes()[start - 1].is_ascii_whitespace() {
            continue;
        }
        let rest = &tag[from..];
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let inner = &rest[1..];
                &inner[..inner.find(q)?]
            }
            _ => &rest[..rest.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(rest.len())],
        };
        return Some(value.trim().replace("&amp;", "&"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves one HTTP response with the given content type and body on a local port.
    async fn serve_once(content_type: &'static str, body: Vec<u8>) -> Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                content_type,
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body).await;
        });
        Url::parse(&format!("http://{}/watch/page", addr)).unwrap()
    }

    fn local_client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    #[test]
    fn test_html_video_source_picks_one_best_candidate() {
        let base_url = Url::parse("https://example.com/watch/video1").unwrap();
        let html = r#"
            <html><head>
                <meta property="og:video:type" content="video/mp4" />
                <meta property="og:video" content="https://cdn.example.com/og_video.mp4" />
            </head><body>
                <VIDEO controls width="800">
                    <source data-src="/lazy.mp4" src="/media/720p.mp4" type="video/mp4">
                    <source src="https://cdn.example.com/1080p.mp4" type="video/mp4">
                </VIDEO>
            </body></html>
        "#;
        assert_eq!(
            extract_html_video_source(html, &base_url),
            Some(Url::parse("https://example.com/media/720p.mp4").unwrap())
        );
    }

    #[test]
    fn test_html_video_source_meta_exact_and_media_only() {
        let base_url = Url::parse("https://news.example.com/story").unwrap();
        // og:video:type must not be read as a URL, and a YouTube embed is not a downloadable file.
        let embed_only = r#"
            <meta property="og:video:type" content="video/mp4">
            <meta property="og:video" content="https://www.youtube.com/embed/abc123">
            <meta property="og:image" content="https://cdn.example.com/video/poster.jpg">
        "#;
        assert_eq!(extract_html_video_source(embed_only, &base_url), None);

        let with_stream = r#"
            <meta property="og:video" content="https://www.youtube.com/embed/abc123">
            <meta name="twitter:player:stream" content="/stream/clip.webm">
        "#;
        assert_eq!(
            extract_html_video_source(with_stream, &base_url),
            Some(Url::parse("https://news.example.com/stream/clip.webm").unwrap())
        );

        // The extension is checked on the parsed path, so ".tsx" or a ".mp4" query value do not count.
        let not_media = r#"<video src="/app/Player.tsx"></video><source src="/page?file=a.mp4">"#;
        assert_eq!(extract_html_video_source(not_media, &base_url), None);
    }

    #[test]
    fn test_html_video_source_json_unescape() {
        let base_url = Url::parse("https://zoom.us/rec/play/abc123xyz").unwrap();
        let html = r#"
            <script>
                window.__data__ = {
                    "viewMp4Url": "https:\/\/ssrweb.zoom.us/rec\/play\/video_hd.mp4?auth=token123&sig=a\"b",
                    "downloadUrl": "https://ssrweb.zoom.us/rec/download/video_original.mp4"
                };
            </script>
        "#;
        assert_eq!(
            extract_html_video_source(html, &base_url).unwrap().as_str(),
            "https://ssrweb.zoom.us/rec/play/video_hd.mp4?auth=token123&sig=a%22b"
        );
    }

    #[test]
    fn test_html_video_resolver_can_handle() {
        for file in ["video.mp4", "archive.rar", "installer.msi", "pkg.deb", "App.AppImage", "lib-1.0-py3-none-any.whl", "a.tar.zst"] {
            let url = Url::parse(&format!("https://example.com/dl/{}", file)).unwrap();
            assert!(!HtmlVideoResolver.can_handle(&url), "{}", file);
        }
        assert!(HtmlVideoResolver.can_handle(&Url::parse("https://example.com/watch/video").unwrap()));
        assert!(HtmlVideoResolver.can_handle(&Url::parse("https://example.com/").unwrap()));
        assert!(HtmlVideoResolver.can_handle(&Url::parse("https://example.com/view.php?id=4").unwrap()));
        assert!(!HtmlVideoResolver.can_handle(&Url::parse("ftp://example.com/watch").unwrap()));
    }

    /// Whether anything connects to `listener` within a moment.
    async fn contacted(listener: &tokio::net::TcpListener) -> bool {
        tokio::time::timeout(Duration::from_millis(200), listener.accept()).await.is_ok()
    }

    #[tokio::test]
    async fn test_smart_resolver_leaves_pages_to_the_probe() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let page = Url::parse(&format!("http://{}/watch/page", listener.local_addr().unwrap())).unwrap();
        let resolved = SmartResolver::resolve(&local_client(), &page).await.unwrap();
        assert_eq!(resolved, vec![page]);
        assert!(!contacted(&listener).await, "the page was fetched before the download's probe");
    }

    fn headers(pairs: &[(reqwest::header::HeaderName, &'static str)]) -> HeaderMap {
        pairs.iter().map(|(name, value)| (name.clone(), HeaderValue::from_static(value))).collect()
    }

    #[test]
    fn test_only_html_answers_to_urls_left_to_the_video_resolver_are_pages() {
        let html = headers(&[(CONTENT_TYPE, "text/html; charset=utf-8")]);
        let page = Url::parse("https://example.com/watch/video").unwrap();
        assert!(HtmlVideoResolver::is_page(&page, &html));
        assert!(HtmlVideoResolver::is_page(&Url::parse("https://example.com/view.php?id=4").unwrap(), &html));
        assert!(!HtmlVideoResolver::is_page(&page, &headers(&[(CONTENT_TYPE, "application/octet-stream")])));
        assert!(!HtmlVideoResolver::is_page(&page, &HeaderMap::new()));
        for file_or_taken in [
            "https://example.com/dl/video.mp4",
            "https://drive.usercontent.google.com/download?id=abc&export=download&confirm=t",
            "https://www.mediafire.com/file/abc/name",
            "https://www.dropbox.com/s/abc/name?dl=1",
        ] {
            assert!(!HtmlVideoResolver::is_page(&Url::parse(file_or_taken).unwrap(), &html), "{file_or_taken}");
        }
    }

    #[tokio::test]
    async fn test_read_capped_stops_at_cap() {
        let url = serve_once("text/html", vec![b'a'; 100_000]).await;
        let resp = local_client().get(url).send().await.unwrap();
        assert_eq!(read_capped(resp, 10).await.unwrap(), "aaaaaaaaaa");
    }

    #[tokio::test]
    async fn test_dropbox_url_normalization() {
        let client = Client::new();
        let url = Url::parse("https://www.dropbox.com/s/sample123/file.zip?dl=0").unwrap();
        let resolved = DropboxResolver.resolve(&client, &url).await.unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].query(), Some("dl=1"));
    }

    #[test]
    fn test_google_drive_id_extraction() {
        let url1 = Url::parse("https://drive.google.com/file/d/1BxyzABC_12345/view?usp=sharing").unwrap();
        assert_eq!(extract_google_drive_id(&url1), Some("1BxyzABC_12345".to_string()));

        let url2 = Url::parse("https://drive.google.com/uc?id=1BxyzABC_12345&export=download").unwrap();
        assert_eq!(extract_google_drive_id(&url2), Some("1BxyzABC_12345".to_string()));
    }

    #[test]
    fn test_google_drive_direct_url() {
        let direct = google_drive_direct_url("1BxyzABC_12345").unwrap();
        assert_eq!(
            direct.as_str(),
            "https://drive.usercontent.google.com/download?id=1BxyzABC_12345&export=download&confirm=t"
        );
    }

    #[tokio::test]
    async fn test_google_drive_without_file_id_is_an_error() {
        let folder = Url::parse("https://drive.google.com/drive/folders/abc").unwrap();
        let result = SmartResolver::resolve(&Client::new(), &folder).await;
        assert!(matches!(result, Err(ResolverError::Parse(_))));
        assert_eq!(SmartResolver::resolve_mirrors(&Client::new(), &folder).await, vec![folder]);
    }

    #[tokio::test]
    async fn test_google_drive_resolver_sends_nothing() {
        // Every request would go through this proxy.
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{}", proxy.local_addr().unwrap())).unwrap())
            .build()
            .unwrap();
        let shared = Url::parse("https://drive.google.com/file/d/1BxyzABC_12345/view?usp=sharing").unwrap();
        let resolved = SmartResolver::resolve(&client, &shared).await.unwrap();
        assert_eq!(resolved, vec![google_drive_direct_url("1BxyzABC_12345").unwrap()]);
        assert!(!contacted(&proxy).await, "Drive was asked before the download's probe");
    }

    #[test]
    fn test_google_drive_web_page_answers_are_rejected() {
        let drive = google_drive_direct_url("1BxyzABC_12345").unwrap();
        let page = headers(&[(CONTENT_TYPE, "text/html; charset=utf-8")]);
        let err = GoogleDriveResolver::check_answer(&drive, &page).unwrap_err();
        assert!(err.to_string().contains("Google Drive served a web page instead of the file"), "{err}");

        assert!(GoogleDriveResolver::check_answer(&drive, &headers(&[(CONTENT_TYPE, "application/octet-stream")])).is_ok());
        // A hosted .html file is served as an attachment; the warning/quota page is not.
        let html_file = headers(&[(CONTENT_TYPE, "text/html"), (CONTENT_DISPOSITION, "attachment; filename=\"page.html\"")]);
        assert!(GoogleDriveResolver::check_answer(&drive, &html_file).is_ok());
        // Only Drive's answers are judged so.
        assert!(GoogleDriveResolver::check_answer(&Url::parse("https://example.com/download").unwrap(), &page).is_ok());
    }

    fn unwrapped(link: &str) -> Option<String> {
        unwrap_redirect(&Url::parse(link).unwrap()).map(String::from)
    }

    #[test]
    fn test_leaving_this_site_links_give_their_target() {
        let target = "https://www.mediafire.com/file/abc/mod.zip/file?dkey=1&r=2";
        let enc = "https%3A%2F%2Fwww.mediafire.com%2Ffile%2Fabc%2Fmod.zip%2Ffile%3Fdkey%3D1%26r%3D2";
        for link in [
            format!("https://www.youtube.com/redirect?event=video_description&redir_token=QUFF&q={enc}&v=dQw4w9WgXcQ"),
            format!("https://youtube.com/redirect?q={enc}"),
            format!("https://m.youtube.com/redirect/?q={enc}"),
            format!("https://www.google.com/url?q={enc}&sa=D&source=docs&ust=1&usg=AOv"),
            format!("https://google.com/url?sa=t&url={enc}"),
            format!("https://www.google.co.uk/url?q={enc}"),
            format!("https://www.google.de/url?sa=t&url={enc}"),
            format!("https://www.google.com.au/url?q={enc}"),
            format!("https://google.co.in/url?q={enc}"),
            format!("https://l.facebook.com/l.php?u={enc}&h=AT0"),
            format!("https://lm.facebook.com/l.php?u={enc}"),
            format!("https://l.messenger.com/l.php?u={enc}"),
            format!("https://l.instagram.com/?u={enc}&e=AT1"),
            format!("https://l.threads.net/?u={enc}"),
            format!("https://steamcommunity.com/linkfilter/?u={enc}"),
            format!("https://steamcommunity.com/linkfilter/?url={enc}"),
            format!("https://out.reddit.com/t3_1abcd?url={enc}&token=AQAA&app_name=web2x"),
            format!("https://www.linkedin.com/redir/redirect?url={enc}&urlhash=x"),
            format!("https://www.linkedin.com/safety/go?url={enc}&trk=flagship-messaging-web&messageThreadUrn=urn"),
            format!("https://vk.com/away.php?to={enc}&cc_key="),
            format!("https://m.vk.com/away.php?to={enc}"),
            format!("https://duckduckgo.com/l/?uddg={enc}&rut=abc"),
            format!("https://t.umblr.com/redirect?z={enc}&t=MjQ"),
            format!("https://gate.sc/?url={enc}&token=1a2b"),
        ] {
            assert_eq!(unwrapped(&link).as_deref(), Some(target), "{link}");
        }
    }

    #[test]
    fn test_wrapped_wrappers_are_unwrapped_up_to_a_limit() {
        let file = "https://drive.google.com/file/d/abc/view";
        let youtube = format!("https://www.youtube.com/redirect?q={}", utf8_percent_encode(file));
        let google = format!("https://www.google.com/url?q={}", utf8_percent_encode(&youtube));
        assert_eq!(unwrapped(&google).as_deref(), Some(file));

        // Six wrappers deep, the last is left on.
        let mut link = file.to_string();
        for _ in 0..MAX_UNWRAPS + 1 {
            link = format!("https://www.google.com/url?q={}", utf8_percent_encode(&link));
        }
        let left = unwrapped(&link).unwrap();
        assert_eq!(unwrap_once(&Url::parse(&left).unwrap()).map(String::from).as_deref(), Some(file));
        // An inner wrapper without a usable target stops the unwrapping, not the outer one's.
        let broken = format!("https://www.google.com/url?q={}", utf8_percent_encode("https://vk.com/away.php?to=nowhere"));
        assert_eq!(unwrapped(&broken).as_deref(), Some("https://vk.com/away.php?to=nowhere"));
    }

    #[test]
    fn test_links_that_are_not_wrappers_or_hold_no_target_are_kept() {
        for link in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&q=https%3A%2F%2Fexample.com",
            "https://www.google.com/search?q=https%3A%2F%2Fexample.com",
            "https://www.google.co.uk/search?q=https%3A%2F%2Fexample.com",
            // Only Google's own country domains.
            "https://google.example.com/url?q=https%3A%2F%2Fexample.org",
            "https://www.google.co.uk.example.com/url?q=https%3A%2F%2Fexample.org",
            "https://notgoogle.com/url?q=https%3A%2F%2Fexample.org",
            "https://www.linkedin.com/safety/help?url=https%3A%2F%2Fexample.org",
            "https://example.com/redirect?q=https%3A%2F%2Fexample.org",
            "https://www.youtube.com/redirect?event=video_description",
            "https://www.youtube.com/redirect?q=example.com%2Ffile.zip",
            "https://www.youtube.com/redirect?q=javascript%3Aalert(1)",
            "https://www.google.com/url?q=file%3A%2F%2F%2Fetc%2Fpasswd",
            "https://www.google.com/url?q=https%3A%2F%2F",
            "https://l.instagram.com/p/abc?u=https%3A%2F%2Fexample.com",
            "https://lnkd.in/abc123",
        ] {
            assert_eq!(unwrapped(link), None, "{link}");
        }
        // A target that is not a URL is skipped for the next parameter that holds one.
        assert_eq!(
            unwrapped("https://www.google.com/url?q=weather&url=https%3A%2F%2Fexample.com%2Fa.zip").as_deref(),
            Some("https://example.com/a.zip")
        );
    }

    #[test]
    fn test_code_host_file_pages_become_their_download_links() {
        // Each download link was checked to serve the file.
        for (page, file) in [
            (
                "https://github.com/yt-dlp/yt-dlp/blob/master/README.md#L10-L20",
                "https://github.com/yt-dlp/yt-dlp/raw/master/README.md",
            ),
            (
                "https://github.com/o/r/blob/feature/x/src/My%20App.zip",
                "https://github.com/o/r/raw/feature/x/src/My%20App.zip",
            ),
            (
                "https://gitlab.com/gitlab-org/gitlab-runner/-/blob/main/README.md?ref_type=heads",
                "https://gitlab.com/gitlab-org/gitlab-runner/-/raw/main/README.md?ref_type=heads",
            ),
            (
                "https://gitlab.com/group/sub/group/project/-/blob/v1.0/dist/app.tar.gz",
                "https://gitlab.com/group/sub/group/project/-/raw/v1.0/dist/app.tar.gz",
            ),
            // The older form, without "/-/", still shows the page.
            (
                "https://gitlab.com/gitlab-org/gitlab-runner/blob/main/README.md",
                "https://gitlab.com/gitlab-org/gitlab-runner/-/raw/main/README.md",
            ),
            (
                "https://gitlab.com/group/sub/project/blob/v1.0/dist/app.tar.gz",
                "https://gitlab.com/group/sub/project/-/raw/v1.0/dist/app.tar.gz",
            ),
            (
                "https://codeberg.org/forgejo/forgejo/src/branch/forgejo/README.md",
                "https://codeberg.org/forgejo/forgejo/media/branch/forgejo/README.md",
            ),
            (
                "https://codeberg.org/forgejo/forgejo/src/tag/v12.0.0/README.md",
                "https://codeberg.org/forgejo/forgejo/media/tag/v12.0.0/README.md",
            ),
            (
                "https://bitbucket.org/multicoreware/x265_git/src/master/COPYING?at=master",
                "https://bitbucket.org/multicoreware/x265_git/raw/master/COPYING?at=master",
            ),
            (
                "https://huggingface.co/openai-community/gpt2/blob/main/model.safetensors",
                "https://huggingface.co/openai-community/gpt2/resolve/main/model.safetensors",
            ),
            ("https://huggingface.co/gpt2/blob/main/config.json", "https://huggingface.co/gpt2/resolve/main/config.json"),
            // /raw/ gives a Git LFS file's pointer, not the file.
            (
                "https://huggingface.co/openai-community/gpt2/raw/main/model.safetensors",
                "https://huggingface.co/openai-community/gpt2/resolve/main/model.safetensors",
            ),
            ("https://huggingface.co/gpt2/raw/main/config.json", "https://huggingface.co/gpt2/resolve/main/config.json"),
            (
                "https://hf.co/openai-community/gpt2/blob/main/config.json",
                "https://hf.co/openai-community/gpt2/resolve/main/config.json",
            ),
            (
                "https://huggingface.co/datasets/stanfordnlp/imdb/blob/main/README.md",
                "https://huggingface.co/datasets/stanfordnlp/imdb/resolve/main/README.md",
            ),
            ("https://huggingface.co/datasets/squad/blob/main/README.md", "https://huggingface.co/datasets/squad/resolve/main/README.md"),
            (
                "https://huggingface.co/spaces/gradio/hello_world/blob/main/app.py",
                "https://huggingface.co/spaces/gradio/hello_world/resolve/main/app.py",
            ),
        ] {
            let page = Url::parse(page).unwrap();
            assert!(SmartResolver::handles(&page), "{page}");
            assert_eq!(code_host_raw_url(&page).map(String::from).as_deref(), Some(file), "{page}");
        }
    }

    #[test]
    fn test_code_host_folders_and_other_pages_are_left_alone() {
        for page in [
            "https://github.com/o/r/tree/main/docs",
            "https://github.com/o/r/blob/main",
            "https://github.com/o/r/blob/main/docs/",
            "https://github.com/o/r/raw/main/app.zip",
            "https://github.com/o/r/releases/download/v1.0/app.zip",
            "https://github.com/o/blob",
            "https://gitlab.com/group/project/-/tree/main/docs",
            "https://gitlab.com/group/-/blob/main/app.zip",
            "https://gitlab.com/group/project/-/blob/main",
            "https://gitlab.com/group/project/blob/main",
            "https://gitlab.com/group/project/-/tree/main/blob/app.zip",
            "https://gitlab.com/blob/main/app.zip",
            "https://codeberg.org/o/r/src/branch/main/",
            "https://codeberg.org/o/r/src/branch/main",
            "https://codeberg.org/o/r/src/main/app.zip",
            "https://bitbucket.org/o/r/src/master/",
            "https://bitbucket.org/o/r/src/master",
            "https://huggingface.co/openai-community/gpt2/tree/main",
            "https://huggingface.co/openai-community/gpt2",
            "https://huggingface.co/datasets/stanfordnlp/imdb/blob/main",
            "https://example.com/o/r/blob/main/app.zip",
        ] {
            let page = Url::parse(page).unwrap();
            assert_eq!(code_host_raw_url(&page), None, "{page}");
            assert!(!CodeHostResolver.can_handle(&page), "{page}");
        }
    }

    /// Codeberg refuses the browser User-Agent the engine sends elsewhere ("403 Access denied,
    /// old Chrome version" on /media/ and /raw/, checked with curl), and takes the app's own.
    #[test]
    fn test_codeberg_gets_the_apps_own_user_agent() {
        let client = Client::builder().default_headers(SmartResolver::default_anti_qos_headers()).build().unwrap();
        let agent = |link: &str| {
            let url = Url::parse(link).unwrap();
            let request = with_agent_for(client.get(url.clone()), &url).build().unwrap();
            request.headers().get(USER_AGENT).map(|v| v.to_str().unwrap().to_string())
        };
        let app = Some(APP_USER_AGENT.to_string());
        assert_eq!(agent("https://codeberg.org/forgejo/forgejo/media/branch/forgejo/README.md"), app);
        assert_eq!(agent("https://codeberg.org./o/r/raw/branch/main/app.zip"), app);
        // Elsewhere the client's own, a browser's, stays.
        for other in ["https://github.com/o/r/raw/main/app.zip", "https://docs.codeberg.org/x", "https://example.com/codeberg.org"] {
            assert_eq!(agent(other), None, "{other}");
        }
    }

    /// Only GitHub's file page is asked where it is (its branch may have been renamed); when it
    /// does not say, the link is rewritten as it is. Other code hosts are sent nothing.
    #[tokio::test]
    async fn test_code_host_resolver_asks_only_github_where_its_file_page_is() {
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{}", proxy.local_addr().unwrap())).unwrap())
            .build()
            .unwrap();
        for (page, raw) in [
            ("https://gitlab.com/g/p/-/blob/main/app.zip", "https://gitlab.com/g/p/-/raw/main/app.zip"),
            ("https://huggingface.co/o/m/blob/main/model.gguf", "https://huggingface.co/o/m/resolve/main/model.gguf"),
        ] {
            let resolved = SmartResolver::resolve(&client, &Url::parse(page).unwrap()).await.unwrap();
            assert_eq!(resolved, vec![Url::parse(raw).unwrap()]);
            assert!(!contacted(&proxy).await, "{page} was asked before the download's probe");
        }

        // The proxy turns the request away: the link is rewritten as it is.
        let turned_away = tokio::spawn(async move {
            let (mut socket, _) = proxy.accept().await.unwrap();
            let mut head = [0u8; 1024];
            let n = tokio::io::AsyncReadExt::read(&mut socket, &mut head).await.unwrap();
            let request = String::from_utf8_lossy(&head[..n]).into_owned();
            tokio::io::AsyncWriteExt::write_all(&mut socket, b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            request
        });
        let page = Url::parse("https://github.com/o/r/blob/main/app.zip").unwrap();
        let resolved = SmartResolver::resolve(&client, &page).await.unwrap();
        assert_eq!(resolved, vec![Url::parse("https://github.com/o/r/raw/main/app.zip").unwrap()]);
        let request = turned_away.await.unwrap();
        assert!(request.starts_with("CONNECT github.com:443 "), "{request}");
    }

    #[test]
    fn test_google_docs_links_become_their_export() {
        let (doc, sheet, deck) = (
            "195j9eDD3ccgjQRttHhJPymLJUCOUjs-jmwTrekvdjFE",
            "1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms",
            "1EAYk18WDjIG-zp_0vLm3CsfQh_i8eXc67Jo2O9C6Vuc",
        );
        let docs = |path: &str| format!("https://docs.google.com/{}", path);
        // Each export link was checked to serve the document as an attachment.
        for (link, export) in [
            (format!("document/d/{doc}/edit?usp=sharing"), format!("document/d/{doc}/export?format=docx")),
            (format!("document/u/0/d/{doc}/edit"), format!("document/u/0/d/{doc}/export?format=docx")),
            (format!("document/d/{doc}"), format!("document/d/{doc}/export?format=docx")),
            (format!("spreadsheets/d/{sheet}/edit?usp=sharing"), format!("spreadsheets/d/{sheet}/export?format=xlsx")),
            // The whole workbook, whichever sheet the editor was on: Sheets names it in the address
            // bar's link, and every sheet but that one would be lost in a .csv.
            (format!("spreadsheets/d/{sheet}/edit#gid=0"), format!("spreadsheets/d/{sheet}/export?format=xlsx")),
            (format!("spreadsheets/u/1/d/{sheet}/edit?gid=1234#gid=1234"), format!("spreadsheets/u/1/d/{sheet}/export?format=xlsx")),
            (format!("spreadsheets/d/{sheet}/htmlview#gid=abc"), format!("spreadsheets/d/{sheet}/export?format=xlsx")),
            (format!("presentation/d/{deck}/edit#slide=id.p"), format!("presentation/d/{deck}/export?format=pptx")),
            (format!("presentation/u/1/d/{deck}/view"), format!("presentation/u/1/d/{deck}/export?format=pptx")),
            (format!("presentation/d/{deck}/present?slide=id.p"), format!("presentation/d/{deck}/export?format=pptx")),
            (format!("presentation/d/{deck}/embed?start=false"), format!("presentation/d/{deck}/export?format=pptx")),
            (format!("document/d/{doc}/mobilebasic"), format!("document/d/{doc}/export?format=docx")),
            (format!("document/d/{doc}/preview"), format!("document/d/{doc}/export?format=docx")),
            (format!("document/d/{doc}/copy"), format!("document/d/{doc}/export?format=docx")),
            (format!("document/d/{doc}/edit/"), format!("document/d/{doc}/export?format=docx")),
            (format!("document/d/{doc}/"), format!("document/d/{doc}/export?format=docx")),
            // The signed-in account the link names is kept (the export drops it when there is none).
            (format!("document/d/{doc}/edit?usp=sharing&authuser=1"), format!("document/d/{doc}/export?format=docx&authuser=1")),
            (
                format!("spreadsheets/d/{sheet}/edit?authuser=me%40example.com#gid=7"),
                format!("spreadsheets/d/{sheet}/export?format=xlsx&authuser=me%40example.com"),
            ),
            // An export in a format of the user's choosing is kept; a gid means nothing to a document.
            (format!("document/d/{doc}/export?format=pdf#top"), format!("document/d/{doc}/export?format=pdf")),
            (format!("spreadsheets/d/{sheet}/export?format=csv&gid=7"), format!("spreadsheets/d/{sheet}/export?format=csv&gid=7")),
            (format!("presentation/d/{deck}/export/pdf"), format!("presentation/d/{deck}/export/pdf")),
            (format!("document/d/{doc}/edit#gid=5"), format!("document/d/{doc}/export?format=docx")),
        ] {
            let link = Url::parse(&docs(&link)).unwrap();
            assert!(SmartResolver::handles(&link), "{link}");
            assert_eq!(google_docs_export(&link).map(String::from), Some(docs(&export)), "{link}");
        }
        for page in [
            "https://docs.google.com/document/d/e/2PACX-1vR/pub",
            "https://docs.google.com/spreadsheets/d/e/2PACX-1vR/pubhtml",
            "https://docs.google.com/forms/d/e/1FAIpQLSf/viewform",
            "https://docs.google.com/drawings/d/abc/edit",
            "https://docs.google.com/document/u/0/",
            "https://docs.google.com/spreadsheets/d/",
            "https://example.com/document/d/abc/edit",
            // Links that already give what they are for: a gviz query (a CSV attachment, or JSON or
            // HTML for scripts), and older /pub copies published to the web.
            "https://docs.google.com/spreadsheets/d/abc/gviz/tq?tqx=out:csv&sheet=Class%20Data",
            "https://docs.google.com/spreadsheets/u/1/d/abc/gviz/tq?tqx=out:json&gid=0",
            "https://docs.google.com/spreadsheets/d/abc/pub?output=csv",
            "https://docs.google.com/spreadsheets/d/abc/pubhtml",
            "https://docs.google.com/document/d/abc/pub",
            "https://docs.google.com/presentation/d/abc/pub?start=false",
            "https://docs.google.com/document/d/abc/template/preview",
            "https://docs.google.com/document/d/abc/edit/extra",
        ] {
            let link = Url::parse(page).unwrap();
            assert!(!GoogleDocsResolver.can_handle(&link), "{page}");
            assert!(!SmartResolver::handles(&link), "{page}");
        }
    }

    #[tokio::test]
    async fn test_google_docs_links_that_give_a_file_are_downloaded_as_they_are() {
        let link = Url::parse("https://docs.google.com/spreadsheets/d/abc/gviz/tq?tqx=out:csv&sheet=Sheet1").unwrap();
        assert_eq!(SmartResolver::resolve(&Client::new(), &link).await.unwrap(), vec![link.clone()]);
        // Their HTML answers (a gviz table for scripts, a copy published to the web) are what they
        // are for, not an export's sign-in page.
        let html = headers(&[(CONTENT_TYPE, "text/html; charset=utf-8")]);
        for page in ["https://docs.google.com/spreadsheets/d/abc/gviz/tq?tqx=out:html", "https://docs.google.com/document/d/abc/pub"] {
            assert_eq!(refusal(page, None, &html), None, "{page}");
        }
    }

    #[tokio::test]
    async fn test_older_drive_links_on_docs_google_com_go_to_drive() {
        let direct = google_drive_direct_url("1BxyzABC_12345").unwrap();
        for link in [
            "https://docs.google.com/uc?export=download&id=1BxyzABC_12345",
            "https://docs.google.com/file/d/1BxyzABC_12345/edit",
        ] {
            let resolved = SmartResolver::resolve(&Client::new(), &Url::parse(link).unwrap()).await.unwrap();
            assert_eq!(resolved, vec![direct.clone()], "{link}");
        }
    }

    #[tokio::test]
    async fn test_google_docs_resolver_sends_nothing() {
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{}", proxy.local_addr().unwrap())).unwrap())
            .build()
            .unwrap();
        let link = Url::parse("https://docs.google.com/document/d/abc/edit").unwrap();
        let resolved = SmartResolver::resolve(&client, &link).await.unwrap();
        assert_eq!(resolved, vec![Url::parse("https://docs.google.com/document/d/abc/export?format=docx").unwrap()]);
        assert!(!contacted(&proxy).await, "Google Docs was asked before the download's probe");
    }

    fn utf8_percent_encode(s: &str) -> String {
        percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
    }

    /// `check_answer` for `url`, which ended at `final_url` (or at itself), as an error message.
    fn refusal(url: &str, final_url: Option<&str>, headers: &HeaderMap) -> Option<String> {
        let url = Url::parse(url).unwrap();
        let final_url = final_url.map_or(url.clone(), |f| Url::parse(f).unwrap());
        check_answer(&url, &final_url, headers).err().map(|e| e.to_string())
    }

    #[test]
    fn test_web_pages_in_place_of_drive_files_and_private_documents_are_refused() {
        let page = headers(&[(CONTENT_TYPE, "text/html; charset=utf-8")]);
        let drive = "https://drive.usercontent.google.com/download?id=abc&export=download&confirm=t";
        let err = refusal(drive, None, &page).unwrap();
        assert!(err.contains("Google Drive served a web page instead of the file"), "{err}");

        // A Google document that asks to sign in: on docs.google.com, or sent on to Google's sign-in.
        let export = "https://docs.google.com/document/d/abc/export?format=docx";
        for (url, final_url) in [
            (export, None),
            (export, Some("https://accounts.google.com/v3/signin/identifier?continue=https%3A%2F%2Fdocs.google.com")),
            ("https://example.com/report", Some("https://accounts.google.com/ServiceLogin?service=wise")),
        ] {
            let err = refusal(url, final_url, &page).unwrap_or_else(|| panic!("{url} -> {final_url:?}"));
            assert!(err.contains("private") && err.contains("Anyone with the link") && err.contains("--load-cookies"), "{err}");
        }
        let docx = headers(&[
            (CONTENT_TYPE, "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
            (CONTENT_DISPOSITION, "attachment; filename=\"Report.docx\""),
        ]);
        assert_eq!(refusal(export, Some("https://doc-0g-3o-docstext.googleusercontent.com/export/x"), &docx), None);
    }

    #[test]
    fn test_web_pages_from_file_shares_not_supported_yet_are_refused() {
        let page = headers(&[(CONTENT_TYPE, "text/html")]);
        for (url, service) in [
            ("https://mega.nz/file/abc#key", "MEGA"),
            ("https://mega.nz/folder/abc#key", "MEGA"),
            ("https://1drv.ms/u/s!abc", "OneDrive"),
            ("https://onedrive.live.com/?cid=abc&id=def", "OneDrive"),
            ("https://contoso.sharepoint.com/:u:/g/abc", "SharePoint"),
            ("https://contoso-my.sharepoint.com/:x:/p/abc", "SharePoint"),
            ("https://we.tl/t-abc", "WeTransfer"),
            ("https://wetransfer.com/downloads/abc/def", "WeTransfer"),
            ("https://www.terabox.com/s/1abc", "Terabox"),
            ("https://1024terabox.com/s/1abc", "Terabox"),
            ("https://www.nephobox.com/s/1abc", "Terabox"),
            ("https://gofile.io/d/abc", "Gofile"),
            ("https://pixeldrain.com/u/abc", "Pixeldrain"),
            ("https://www.icloud.com/iclouddrive/abc#file", "iCloud"),
            ("https://disk.yandex.ru/client/disk", "Yandex Disk"),
            ("https://u.pcloud.link/publink/show?code=abc", "pCloud"),
            ("https://acme.app.box.com/v/files", "Box"),
            ("https://app.box.com/folder/123", "Box"),
            ("https://contoso.sharepoint.com/sites/team/_layouts/15/stream.aspx", "SharePoint"),
        ] {
            assert_eq!(refusal(url, None, &page), Some(format!("Direct download link not found: {} links are not supported yet", service)));
        }
        // A shortened link that lands on one, and share links yt-dlp would take that sent the
        // browser on to sign in instead: yt-dlp is handed the page answered, not the link.
        for (url, final_url, service) in [
            ("https://bit.ly/abc", "https://mega.nz/file/abc", "MEGA"),
            ("https://yadi.sk/d/abc", "https://passport.yandex.ru/auth?retpath=x", "Yandex Disk"),
            ("https://app.box.com/s/abc", "https://account.box.com/login?redirect_url=%2Fs%2Fabc", "Box"),
            (
                "https://contoso.sharepoint.com/:v:/g/personal/user_contoso_com/EabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRS",
                "https://login.microsoftonline.com/common/oauth2/authorize?client_id=x",
                "SharePoint",
            ),
        ] {
            let err = refusal(url, Some(final_url), &page).unwrap_or_else(|| panic!("{url}"));
            assert!(err.contains(&format!("{service} links are not supported yet")), "{err}");
        }
        // The files these services serve are downloads like any other, and look-alike hosts are not theirs.
        let file = headers(&[(CONTENT_TYPE, "application/octet-stream")]);
        assert_eq!(refusal("https://pixeldrain.com/api/file/abc?download", None, &file), None);
        for elsewhere in ["https://notbox.com/s/abc", "https://example.com/mega.nz", "https://megalodon.nz/file/abc"] {
            assert_eq!(refusal(elsewhere, None, &page), None, "{elsewhere}");
        }
        // A link of theirs that names a file is one to the file, gone stale: said so, not that the
        // service is not supported.
        for (url, file) in [
            ("https://p-lux3.pcloud.com/cBZabcdefghij/Backup%202024.zip", "Backup 2024.zip"),
            ("https://app.box.com/shared/static/abcdefghijklmnop.zip", "abcdefghijklmnop.zip"),
            ("https://contoso.sharepoint.com/sites/team/Shared%20Documents/report.docx", "report.docx"),
        ] {
            let err = refusal(url, None, &page).unwrap_or_else(|| panic!("{url}"));
            assert!(err.contains(&format!("the server sent a web page instead of {file} (")), "{err}");
        }
    }

    #[test]
    fn test_share_pages_yt_dlp_downloads_go_on_to_the_page_handling() {
        // As yt-dlp's Box, SharePoint and Yandex Disk extractors match them.
        let page = headers(&[(CONTENT_TYPE, "text/html; charset=utf-8")]);
        for (url, final_url) in [
            ("https://app.box.com/s/abc123", None),
            ("https://acme.app.box.com/s/abc123/file/456", None),
            ("https://acme.ent.box.com/s/abc123", None),
            ("https://box.com/s/abc123", Some("https://app.box.com/s/abc123")),
            ("https://bit.ly/abc", Some("https://app.box.com/s/abc123")),
            ("https://contoso.sharepoint.com/:v:/g/personal/user_contoso_com/EabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRS?e=x", None),
            (
                "https://contoso.sharepoint.com/:v:/g/personal/user_contoso_com/EabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRS",
                Some("https://contoso-my.sharepoint.com/personal/user_contoso_com/_layouts/15/stream.aspx?id=%2Fpersonal%2Fv.mp4"),
            ),
            ("https://yadi.sk/d/abc", Some("https://disk.yandex.ru/d/abc")),
            ("https://yadi.sk/i/abc", None),
            ("https://disk.yandex.com/d/abc", None),
            ("https://disk.360.yandex.ru/d/abc", None),
            ("https://disk.yandex.ru/public?hash=abc%3D", None),
        ] {
            assert_eq!(refusal(url, final_url, &page), None, "{url} -> {final_url:?}");
            let landed = Url::parse(final_url.unwrap_or(url)).unwrap();
            assert!(HtmlVideoResolver::is_page(&landed, &page), "{landed}");
        }
    }

    #[test]
    fn test_code_host_folders_are_refused() {
        let page = headers(&[(CONTENT_TYPE, "text/html; charset=utf-8")]);
        for (url, final_url) in [
            // A file link to a folder, which GitHub sends on to the folder's page.
            ("https://github.com/o/r/blob/main/docs", Some("https://github.com/o/r/tree/main/docs")),
            ("https://github.com/o/r/tree/main", None),
            ("https://gitlab.com/group/sub/project/-/tree/main/docs", None),
            ("https://huggingface.co/openai-community/gpt2/tree/main/onnx", None),
            ("https://huggingface.co/datasets/stanfordnlp/imdb/tree/main", None),
            ("https://hf.co/gpt2/tree/main", None),
        ] {
            let err = refusal(url, final_url, &page).unwrap_or_else(|| panic!("{url}"));
            assert!(err.contains("the link leads to a folder, not a file"), "{err}");
        }
        for page_url in [
            "https://github.com/o/r",
            "https://github.com/o/tree",
            "https://github.com/o/r/tree/",
            "https://gitlab.com/group/tree/main",
            "https://huggingface.co/owner/tree",
            "https://example.com/o/r/tree/main",
        ] {
            assert_eq!(refusal(page_url, None, &page), None, "{page_url}");
        }
        // A folder's listing sent as a file is one.
        let json = headers(&[(CONTENT_TYPE, "application/json")]);
        assert_eq!(refusal("https://huggingface.co/api/models/gpt2/tree/main", None, &json), None);
    }

    #[test]
    fn test_web_pages_in_place_of_a_named_file_are_refused() {
        let page = headers(&[(CONTENT_TYPE, "text/html; charset=utf-8")]);
        for (url, final_url, file) in [
            ("https://example.com/files/app.zip", None, "app.zip"),
            ("https://example.com/dl/My%20Setup%201.2.exe", None, "My Setup 1.2.exe"),
            ("https://example.com/src/project-1.0.tar.gz", None, "project-1.0.tar.gz"),
            ("https://example.com/os/Distro.ISO", None, "Distro.ISO"),
            ("https://example.com/files/app.zip", Some("https://example.com/login?next=%2Ffiles%2Fapp.zip"), "app.zip"),
            ("https://example.com/get?id=3", Some("https://cdn.example.com/m/model.safetensors"), "model.safetensors"),
            ("https://example.com/t/linux.torrent", None, "linux.torrent"),
            ("https://example.com/t/release.meta4", None, "release.meta4"),
            // A hotlinked or expired image, a stream's segment, a font, an extension.
            ("https://cdn.example/photos/sunset.jpg", None, "sunset.jpg"),
            ("https://cdn.example/photos/IMG_0001.HEIC", None, "IMG_0001.HEIC"),
            ("https://cdn.example/live/stream.ts", None, "stream.ts"),
            ("https://cdn.example/fonts/Inter.woff2", None, "Inter.woff2"),
            ("https://example.com/addons/tool.xpi", None, "tool.xpi"),
            ("https://example.com/rom/game.7z.001", None, "game.7z.001"),
        ] {
            let err = refusal(url, final_url, &page).unwrap_or_else(|| panic!("{url}"));
            assert!(err.contains(&format!("the server sent a web page instead of {} (the link may need a login", file)), "{err}");
        }
        // The file itself, or a page sent as an attachment, is what was asked for.
        let zip = headers(&[(CONTENT_TYPE, "application/zip")]);
        assert_eq!(refusal("https://example.com/files/app.zip", None, &zip), None);
        assert_eq!(refusal("https://example.com/files/app.zip", None, &HeaderMap::new()), None);
        let attached = headers(&[(CONTENT_TYPE, "text/html"), (CONTENT_DISPOSITION, "attachment; filename=\"app.zip\"")]);
        assert_eq!(refusal("https://example.com/files/app.zip", None, &attached), None);

        // Pages go on to be looked into: an .html file, and names that only look like files.
        for url in [
            "https://example.com/docs/page.html",
            "https://example.com/watch",
            "https://example.com/",
            "https://example.com/view.php?file=app.zip",
            "https://example.com/releases/v1.2",
            "https://example.com/people/john.doe",
            "https://example.com/docs/readme.md",
        ] {
            assert_eq!(refusal(url, None, &page), None, "{url}");
        }
    }

    /// A server may label every file HTML: the file's own first bytes show it is none.
    #[test]
    fn test_only_binary_data_shows_an_answer_labelled_html_is_no_page() {
        for file in [
            &b"PK\x03\x04\x14\x00\x00\x00\x08\x00"[..],
            b"MZ\x90\x00\x03\x00\x00\x00",
            b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n",
            b"\x1F\x8B\x08\x00\x00\x00\x00\x00",
            b"GGUF\x03\x00\x00\x00",
            b"\xEF\xBB\xBF\x00\x01",
        ] {
            assert!(start_is_no_page(file), "{file:?}");
        }
        for page in [
            &b""[..],
            b"   \r\n",
            b"<!DOCTYPE html><html><body>Sign in</body></html>",
            b"\xEF\xBB\xBF\n  <html lang=\"fr\"><title>T\xE9l\xE9charger</title>",
            b"File not found.",
            "Datei nicht gefunden: überprüfen Sie den Link".as_bytes(),
            // A character cut in two by the 512 bytes looked at.
            &[b"x".repeat(511), "é".as_bytes().to_vec()].concat(),
        ] {
            assert!(!start_is_no_page(page), "{:?}", String::from_utf8_lossy(page));
        }
    }

    #[test]
    fn test_sourceforge_file_path_parsing() {
        let url = Url::parse("https://sourceforge.net/projects/sevenzip/files/7-Zip/24.09/7z%202409.exe/download?use_mirror=foo&ts=1").unwrap();
        assert_eq!(
            sourceforge_file_path(&url),
            Some(("sevenzip".to_string(), "7-Zip/24.09/7z%202409.exe".to_string()))
        );
        for skipped in [
            "https://sourceforge.net/projects/sevenzip/files/7-Zip/24.09/",
            "https://sourceforge.net/projects/sevenzip/files/latest/download",
            "https://sourceforge.net/projects/sevenzip/",
        ] {
            assert!(!SourceForgeResolver.can_handle(&Url::parse(skipped).unwrap()), "{}", skipped);
        }
    }

    #[tokio::test]
    async fn test_sourceforge_mirror_generation() {
        let client = Client::new();
        let url = Url::parse("https://sourceforge.net/projects/sevenzip/files/7-Zip/24.09/7z2409-x64.exe/download?ts=123").unwrap();
        let resolved = SourceForgeResolver.resolve(&client, &url).await.unwrap();
        assert_eq!(resolved.len(), SOURCEFORGE_MIRRORS.len());
        assert_eq!(
            resolved[1].as_str(),
            "https://downloads.sourceforge.net/project/sevenzip/7-Zip/24.09/7z2409-x64.exe?use_mirror=netix"
        );
        assert!(resolved.iter().all(|u| u.query_pairs().count() == 1));
    }

    #[test]
    fn test_mediafire_html_extraction() {
        let page = Url::parse("https://www.mediafire.com/file/abc/sample.zip/file").unwrap();
        let html = r#"<div><a class="input popsok" aria-label="Download file" href="https://download1590.mediafire.com/xyz123/sample.zip" id="downloadButton">Download</a></div>"#;
        assert_eq!(
            extract_mediafire_direct(html, &page).unwrap().as_str(),
            "https://download1590.mediafire.com/xyz123/sample.zip"
        );

        // base64("https://download2390.mediafire.com/q/sample.zip")
        let scrambled = r#"<a class="input popsok" href="javascript:void(0)" data-scrambled-url="aHR0cHM6Ly9kb3dubG9hZDIzOTAubWVkaWFmaXJlLmNvbS9xL3NhbXBsZS56aXA=">Download</a>"#;
        assert_eq!(
            extract_mediafire_direct(scrambled, &page).unwrap().as_str(),
            "https://download2390.mediafire.com/q/sample.zip"
        );

        let other_links = r#"<a href="https://www.mediafire.com/upgrade">Upgrade</a><a href="https://download.evil.com/x">x</a>"#;
        assert_eq!(extract_mediafire_direct(other_links, &page), None);
    }

    #[tokio::test]
    async fn test_mediafire_missing_link_is_an_error() {
        let page = serve_once("text/html", b"<html><a href=\"/help\">Help</a></html>".to_vec()).await;
        let result = MediaFireResolver.resolve(&local_client(), &page).await;
        assert!(matches!(result, Err(ResolverError::NotFound(_))));
    }

    #[test]
    fn test_archive_org_url_parsing() {
        let data_node = Url::parse("https://dn720001.ca.archive.org/0/items/fn-v8-archive/builds/8.51-CL-6165369.7z").unwrap();
        assert_eq!(
            archive_item_path(&data_node),
            Some(("fn-v8-archive".to_string(), "builds/8.51-CL-6165369.7z".to_string()))
        );
        let download = Url::parse("https://archive.org/download/item/file.zip").unwrap();
        assert_eq!(archive_item_path(&download), Some(("item".to_string(), "file.zip".to_string())));

        for rejected in [
            "https://web.archive.org/web/2020/http://example.com/items/foo/bar.zip",
            "https://archive.org/details/item",
            "https://archive.org/download/item",
            "https://example.com/download/item/file.zip",
            "https://notarchive.org/download/item/file.zip",
        ] {
            assert!(!ArchiveOrgResolver.can_handle(&Url::parse(rejected).unwrap()), "{}", rejected);
        }
    }

    /// Serves archive.org's metadata answers for the item "nasa" (`alternate_locations` made
    /// missing) at `/metadata`, and a 503 for every request while `down` is set. Logs the paths
    /// asked for.
    async fn serve_archive_metadata(
        down: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> (String, std::sync::Arc<parking_lot::Mutex<Vec<String>>>) {
        fn reply(path: &str) -> &'static str {
            match path {
                "/metadata/nasa/dir" => r#"{"result":"/6/items/nasa"}"#,
                "/metadata/nasa/workable_servers" => r#"{"result":["ia801607.us.archive.org","ia601607.us.archive.org"]}"#,
                "/metadata/nasa/alternate_locations" => r#"{"error":"Couldn't get 'alternate_locations' for item nasa"}"#,
                _ => r#"{"d1":"ia601607.us.archive.org","dir":"/6/items/nasa","files":[]}"#,
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/metadata", listener.local_addr().unwrap());
        let requested = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let log = requested.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (log, down) = (log.clone(), down.clone());
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let path = String::from_utf8_lossy(&buf[..n]).split_whitespace().nth(1).unwrap_or("").to_string();
                    let (status, body) = if down.load(std::sync::atomic::Ordering::Relaxed) {
                        ("503 Service Unavailable", "")
                    } else {
                        ("200 OK", reply(&path))
                    };
                    log.lock().push(path);
                    let head = format!(
                        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        status,
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                });
            }
        });
        (base, requested)
    }

    #[tokio::test]
    async fn test_archive_org_metadata_fetches_only_the_fields_it_reads() {
        let (base, requested) = serve_archive_metadata(Default::default()).await;
        let metadata = ArchiveMetadata::fetch(&local_client(), &base, "nasa").await.unwrap();
        assert_eq!(metadata.dir.as_deref(), Some("/6/items/nasa"));
        assert_eq!(metadata.workable_servers.as_ref().map(Vec::len), Some(2));
        assert!(metadata.alternate_locations.is_none(), "a missing field is absent");

        // Never the whole record, and no field workable_servers already covers.
        let mut paths = requested.lock().clone();
        paths.sort();
        let fields = ["alternate_locations", "dir", "workable_servers"];
        assert_eq!(paths, fields.map(|f| format!("/metadata/nasa/{}", f)));
    }

    #[tokio::test]
    async fn test_archive_org_files_of_one_item_share_its_metadata() {
        let down = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (base, requested) = serve_archive_metadata(down.clone()).await;
        let client = local_client();
        // A failed fetch is not kept.
        assert!(ArchiveMetadata::shared(&client, &base, "nasa").await.is_err());
        down.store(false, std::sync::atomic::Ordering::Relaxed);
        requested.lock().clear();

        // Files of a batch resolving at once, and one a moment later.
        let (a, b) = tokio::join!(ArchiveMetadata::shared(&client, &base, "nasa"), ArchiveMetadata::shared(&client, &base, "nasa"));
        let c = ArchiveMetadata::shared(&client, &base, "nasa").await.unwrap();
        assert!(std::sync::Arc::ptr_eq(&a.unwrap(), &c) && std::sync::Arc::ptr_eq(&b.unwrap(), &c));
        assert_eq!(c.dir.as_deref(), Some("/6/items/nasa"));
        assert_eq!(requested.lock().len(), 3, "one fetch: {:?}", requested.lock());
    }

    #[tokio::test]
    #[ignore = "requires network"]
    async fn test_archive_org_resolver() {
        let client = Client::new();
        let url = Url::parse("https://dn720001.ca.archive.org/0/items/fn-v8-archive/builds/8.51-CL-6165369.7z").unwrap();
        let mirrors = ArchiveOrgResolver.resolve(&client, &url).await.unwrap();
        assert!(mirrors.len() >= 2);
    }

    #[test]
    fn test_parse_netscape_cookies() {
        use reqwest::cookie::CookieStore;
        let cookie_content = "\
# Netscape HTTP Cookie File\r
.example.com\tTRUE\t/\tTRUE\t0\tsession_id\tabc123xyz\r
#HttpOnly_.example.com\tTRUE\t/\tFALSE\t4102444800\ttoken\tsecret_val
.example.com\tTRUE\t/\tFALSE\t0\tflag\t
host.example.com\tFALSE\t/\tFALSE\t0\thostonly\t1
.example.com\tTRUE\t/\tFALSE\t1000000000\texpired\t1
";
        let jar = reqwest::cookie::Jar::default();
        parse_netscape_cookies(cookie_content, &jar);
        let header = |u: &str| {
            jar.cookies(&Url::parse(u).unwrap())
                .map(|h| h.to_str().unwrap().to_string())
                .unwrap_or_default()
        };

        let https = header("https://example.com/");
        assert!(https.contains("session_id=abc123xyz"), "{}", https);
        assert!(https.contains("token=secret_val"), "{}", https);
        assert!(https.contains("flag="), "{}", https);
        assert!(!https.contains("expired"), "{}", https);

        // Secure cookies never go over plain HTTP.
        let http = header("http://example.com/");
        assert!(!http.contains("session_id"), "{}", http);
        assert!(http.contains("token=secret_val"), "{}", http);

        // Host-only cookies are not widened to subdomains.
        assert!(header("http://host.example.com/").contains("hostonly=1"));
        assert!(!header("http://sub.host.example.com/").contains("hostonly"));
    }
}
