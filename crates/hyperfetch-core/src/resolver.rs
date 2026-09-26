use std::collections::HashSet;
use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use base64::Engine as _;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, USER_AGENT};
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

#[derive(Debug, Deserialize)]
struct ArchiveMetadata {
    server: Option<String>,
    d1: Option<String>,
    d2: Option<String>,
    dir: Option<String>,
    workable_servers: Option<Vec<String>>,
    alternate_locations: Option<ArchiveAlternateLocations>,
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

        let metadata_url = format!("https://archive.org/metadata/{}", identifier);
        let bytes = client.get(&metadata_url).send().await?.error_for_status()?.bytes().await?;
        let metadata: ArchiveMetadata = serde_json::from_slice(&bytes)
            .map_err(|e| ResolverError::Parse(format!("archive.org metadata: {}", e)))?;

        let mut server_dirs: Vec<(String, String)> = Vec::new();
        let mut seen = HashSet::new();
        let mut add = |s: &String, d: &String| {
            if seen.insert((s.clone(), d.clone())) {
                server_dirs.push((s.clone(), d.clone()));
            }
        };

        if let Some(ref d) = metadata.dir {
            let named = [&metadata.server, &metadata.d1, &metadata.d2];
            for s in named.into_iter().flatten().chain(metadata.workable_servers.iter().flatten()) {
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
        matches!(url.host_str(), Some("drive.google.com" | "drive.usercontent.google.com"))
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let file_id = extract_google_drive_id(url)
            .ok_or_else(|| ResolverError::Parse("Could not extract Google Drive file ID".to_string()))?;
        let direct = google_drive_direct_url(&file_id)?;
        ensure_file_response(client, &direct, "Google Drive").await?;
        Ok(vec![direct])
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

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let resp = client.get(url.clone()).send().await?;
        if !resp.status().is_success() || !is_html(&resp) {
            return Ok(vec![url.clone()]);
        }
        let page_url = resp.url().clone();
        let html = read_capped(resp, MAX_HTML_BYTES).await?;
        match extract_html_video_source(&html, &page_url) {
            Some(video) => {
                tracing::info!("HtmlVideoResolver: {} plays {}", url, video);
                Ok(vec![video])
            }
            None => Ok(vec![url.clone()]),
        }
    }
}

/// Master Smart Resolver registry that chains all host resolvers
pub struct SmartResolver;

impl SmartResolver {
    /// Resolves `url` into download sources that are byte-identical copies of one file.
    /// `Ok` is never empty. `Err` means a resolver recognized the host but could not extract a direct link.
    pub async fn resolve(client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        // The landing pages of these hosts are never the file, so a failed extraction is an error.
        if GoogleDriveResolver.can_handle(url) {
            return with_timeout(GoogleDriveResolver.resolve(client, url)).await;
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
    resp.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
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

/// Fails unless `url` answers with a successful, non-HTML response. The body is never read.
async fn ensure_file_response(client: &Client, url: &Url, host: &str) -> Result<(), ResolverError> {
    let resp = client.get(url.clone()).send().await?;
    if !resp.status().is_success() {
        return Err(ResolverError::NotFound(format!("{} returned HTTP {}", host, resp.status())));
    }
    if is_html(&resp) {
        return Err(ResolverError::NotFound(format!(
            "{} served a web page instead of the file (it may be private, deleted, or over its download quota)",
            host
        )));
    }
    Ok(())
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

    #[tokio::test]
    async fn test_smart_resolver_scrapes_html_page() {
        let html = br#"<video controls><source src="/media/clip.mp4" type="video/mp4"></video>"#.to_vec();
        let page = serve_once("text/html; charset=utf-8", html).await;
        let resolved = SmartResolver::resolve(&local_client(), &page).await.unwrap();
        assert_eq!(resolved, vec![page.join("/media/clip.mp4").unwrap()]);
    }

    #[tokio::test]
    async fn test_html_video_resolver_ignores_non_html() {
        let body = br#"<video src="/media/clip.mp4"></video>"#.to_vec();
        let page = serve_once("application/octet-stream", body).await;
        let resolved = HtmlVideoResolver.resolve(&local_client(), &page).await.unwrap();
        assert_eq!(resolved, vec![page]);
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
    async fn test_ensure_file_response_rejects_html() {
        let client = local_client();
        let page = serve_once("text/html", b"<html>Quota exceeded</html>".to_vec()).await;
        assert!(matches!(ensure_file_response(&client, &page, "Google Drive").await, Err(ResolverError::NotFound(_))));

        let file = serve_once("application/octet-stream", vec![0u8; 64]).await;
        assert!(ensure_file_response(&client, &file, "Google Drive").await.is_ok());
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
