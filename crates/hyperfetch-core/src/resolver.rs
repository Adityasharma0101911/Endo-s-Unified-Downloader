use std::collections::HashSet;
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use reqwest::Client;
use serde::Deserialize;
use url::Url;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ResolverError {
    #[error("Network error during resolution: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Parsing error: {0}")]
    Parse(String),
    #[error("Direct download link not found: {0}")]
    NotFound(String),
}

/// Trait implemented by host-specific resolvers to unpack landing pages,
/// bypass confirmation gates, and discover multi-cluster mirrors.
pub trait HostResolver: Send + Sync {
    fn can_handle(&self, url: &Url) -> bool;
    fn resolve(&self, client: &Client, url: &Url) -> impl std::future::Future<Output = Result<Vec<Url>, ResolverError>> + Send;
}

/// Archive.org Multi-Cluster Resolver
pub struct ArchiveOrgResolver;

#[derive(Debug, Deserialize)]
struct ArchiveMetadata {
    server: Option<String>,
    dir: Option<String>,
    workable_servers: Option<Vec<String>>,
}

impl HostResolver for ArchiveOrgResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.ends_with("archive.org"))
            && url.path().starts_with("/download/")
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let segments: Vec<&str> = url.path_segments()
            .ok_or_else(|| ResolverError::Parse("Missing path segments".to_string()))?
            .collect();

        if segments.len() < 3 || segments[0] != "download" {
            return Ok(vec![url.clone()]);
        }

        let identifier = segments[1];
        let filename = segments[2..].join("/");

        let metadata_url = format!("https://archive.org/metadata/{}", identifier);
        let resp = client.get(&metadata_url).send().await?;

        if !resp.status().is_success() {
            return Ok(vec![url.clone()]);
        }

        let bytes = resp.bytes().await?;
        let metadata: ArchiveMetadata = match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(_) => return Ok(vec![url.clone()]),
        };

        let dir = match metadata.dir {
            Some(d) => d,
            None => return Ok(vec![url.clone()]),
        };

        let mut servers = Vec::new();
        let mut seen = HashSet::new();

        if let Some(primary) = metadata.server {
            if seen.insert(primary.clone()) {
                servers.push(primary);
            }
        }

        if let Some(workable) = metadata.workable_servers {
            for ws in workable {
                if seen.insert(ws.clone()) {
                    servers.push(ws);
                }
            }
        }

        let mut mirror_urls = Vec::new();
        for s in servers {
            let mirror_str = format!("https://{}{}/{}", s, dir, filename);
            if let Ok(m_url) = Url::parse(&mirror_str) {
                mirror_urls.push(m_url);
            }
        }

        if !mirror_urls.iter().any(|u| u == url) {
            mirror_urls.push(url.clone());
        }

        Ok(mirror_urls)
    }
}

/// Google Drive Resolver (Auto-bypasses virus scan warnings on large files)
pub struct GoogleDriveResolver;

impl HostResolver for GoogleDriveResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("drive.google.com"))
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let file_id = extract_google_drive_id(url)
            .ok_or_else(|| ResolverError::Parse("Could not extract Google Drive file ID".to_string()))?;

        let initial_url = format!("https://drive.google.com/uc?export=download&id={}", file_id);
        let resp = client.get(&initial_url).send().await?;

        // If it directly redirected to the download stream
        if let Some(final_url) = resp.url().as_str().strip_prefix("https://docs.googleusercontent.com/") {
            let _ = final_url;
            return Ok(vec![resp.url().clone()]);
        }

        let text = resp.text().await.unwrap_or_default();

        // Search for confirm token in HTML response
        if let Some(confirm_token) = extract_confirm_token(&text) {
            let confirmed_url = format!(
                "https://drive.google.com/uc?export=download&confirm={}&id={}",
                confirm_token, file_id
            );
            if let Ok(u) = Url::parse(&confirmed_url) {
                return Ok(vec![u]);
            }
        }

        // Fallback to standard export URL
        Ok(vec![Url::parse(&initial_url).unwrap_or_else(|_| url.clone())])
    }
}

/// MediaFire Resolver (Bypasses landing page to extract direct CDN link)
pub struct MediaFireResolver;

impl HostResolver for MediaFireResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("mediafire.com"))
            && (url.path().starts_with("/file/") || url.query().is_some())
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let resp = client.get(url.clone()).send().await?;
        let text = resp.text().await.unwrap_or_default();

        // Match direct download link from MediaFire landing page HTML
        // E.g.: aria-label="Download file" href="https://downloadXXXX.mediafire.com/..."
        if let Some(direct) = extract_mediafire_direct(&text) {
            if let Ok(direct_url) = Url::parse(&direct) {
                return Ok(vec![direct_url]);
            }
        }

        Ok(vec![url.clone()])
    }
}

/// Dropbox Resolver (Normalizes preview URLs to raw binary streaming links)
pub struct DropboxResolver;

impl HostResolver for DropboxResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("dropbox.com"))
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

/// SourceForge Resolver (Extracts multi-mirror CDN endpoints)
pub struct SourceForgeResolver;

impl HostResolver for SourceForgeResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("sourceforge.net"))
            && url.path().contains("/files/")
    }

    async fn resolve(&self, _client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let mut mirrors = Vec::new();
        let base_url = url.as_str().trim_end_matches("/download");

        // Generate direct mirrors across top global CDNs
        let mirror_codes = ["autoselect", "fastly", "heanet", "jaist", "netcologne", "liquidtelecom"];
        for code in mirror_codes {
            let mirror_str = format!("{}/download?use_mirror={}", base_url, code);
            if let Ok(u) = Url::parse(&mirror_str) {
                mirrors.push(u);
            }
        }

        if mirrors.is_empty() {
            mirrors.push(url.clone());
        }

        Ok(mirrors)
    }
}

/// HTML5 Video Extractor (Extracts direct media streams from webpage <video>, <source>, and OpenGraph tags)
pub struct HtmlVideoResolver;

impl HostResolver for HtmlVideoResolver {
    fn can_handle(&self, url: &Url) -> bool {
        // Handle web pages that are not direct binary/archive downloads
        let path = url.path().to_ascii_lowercase();
        let is_direct_file = path.ends_with(".zip")
            || path.ends_with(".iso")
            || path.ends_with(".exe")
            || path.ends_with(".tar")
            || path.ends_with(".gz")
            || path.ends_with(".7z")
            || path.ends_with(".bin");

        !is_direct_file && (url.scheme() == "http" || url.scheme() == "https")
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let resp = match client.get(url.clone()).send().await {
            Ok(r) => r,
            Err(_) => return Ok(vec![url.clone()]),
        };

        // Check if content-type is HTML
        let is_html = resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map_or(false, |ct| ct.contains("text/html"));

        if !is_html {
            return Ok(vec![url.clone()]);
        }

        let html = resp.text().await.unwrap_or_default();
        let sources = extract_html_video_sources(&html, url);

        if !sources.is_empty() {
            tracing::info!(
                "HtmlVideoResolver: discovered {} video stream source(s) in {}",
                sources.len(),
                url
            );
            return Ok(sources);
        }

        Ok(vec![url.clone()])
    }
}

/// Master Smart Resolver registry that chains all host resolvers
pub struct SmartResolver;

impl SmartResolver {
    /// Resolves an incoming URL into direct, multi-mirror streaming URLs.
    pub async fn resolve_mirrors(client: &Client, url: &Url) -> Vec<Url> {
        if ArchiveOrgResolver.can_handle(url) {
            if let Ok(mirrors) = ArchiveOrgResolver.resolve(client, url).await {
                if !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        if GoogleDriveResolver.can_handle(url) {
            if let Ok(mirrors) = GoogleDriveResolver.resolve(client, url).await {
                if !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        if MediaFireResolver.can_handle(url) {
            if let Ok(mirrors) = MediaFireResolver.resolve(client, url).await {
                if !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        if DropboxResolver.can_handle(url) {
            if let Ok(mirrors) = DropboxResolver.resolve(client, url).await {
                if !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        if SourceForgeResolver.can_handle(url) {
            if let Ok(mirrors) = SourceForgeResolver.resolve(client, url).await {
                if !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        // Check for embedded HTML video sources on webpages
        if HtmlVideoResolver.can_handle(url) {
            if let Ok(video_sources) = HtmlVideoResolver.resolve(client, url).await {
                if video_sources != vec![url.clone()] && !video_sources.is_empty() {
                    return video_sources;
                }
            }
        }

        // Default fallback: single mirror
        vec![url.clone()]
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

fn extract_confirm_token(html: &str) -> Option<String> {
    // Matches confirm=([0-9a-zA-Z_-]+)
    if let Some(idx) = html.find("confirm=") {
        let sub = &html[idx + 8..];
        let token: String = sub.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
        if !token.is_empty() {
            return Some(token);
        }
    }
    None
}

fn extract_mediafire_direct(html: &str) -> Option<String> {
    // Matches https://download[0-9]+\.mediafire\.com/[^"'\s]+
    let target = "https://download";
    let mut cursor = 0;

    while let Some(idx) = html[cursor..].find(target) {
        let start = cursor + idx;
        let sub = &html[start..];
        if let Some(end) = sub.find(|c| c == '"' || c == '\'' || c == ' ' || c == '<') {
            let candidate = &sub[..end];
            if candidate.contains(".mediafire.com/") {
                return Some(candidate.to_string());
            }
        }
        cursor = start + target.len();
    }

    None
}

fn extract_html_video_sources(html: &str, base_url: &Url) -> Vec<Url> {
    let mut sources = Vec::new();
    let mut seen = HashSet::new();

    // 1. Scan for <source ... src="..."> and <video ... src="...">
    let tag_targets = ["<source", "<video", "<SOURCE", "<VIDEO"];
    for tag in tag_targets {
        let mut cursor = 0;
        while let Some(idx) = html[cursor..].find(tag) {
            let start = cursor + idx;
            let sub = &html[start..];
            if let Some(tag_end) = sub.find('>') {
                let tag_content = &sub[..tag_end];
                if let Some(src) = extract_attribute_value(tag_content, "src") {
                    if is_probable_video_url(&src) {
                        if let Ok(resolved) = base_url.join(&src) {
                            if seen.insert(resolved.to_string()) {
                                sources.push(resolved);
                            }
                        }
                    }
                }
                cursor = start + tag_end;
            } else {
                break;
            }
        }
    }

    // 2. Scan for OpenGraph and Twitter video metadata:
    // <meta property="og:video" content="..."> or <meta name="twitter:player:stream" content="...">
    let meta_targets = ["og:video", "og:video:url", "og:video:secure_url", "twitter:player:stream"];
    for target in meta_targets {
        let mut cursor = 0;
        while let Some(idx) = html[cursor..].find(target) {
            let meta_pos = cursor + idx;
            // Look backward for <meta and forward for >
            let line_start = html[..meta_pos].rfind('<').unwrap_or(meta_pos);
            let line_end = html[meta_pos..].find('>').map(|e| meta_pos + e).unwrap_or(html.len());
            let meta_tag = &html[line_start..line_end];

            if let Some(content) = extract_attribute_value(meta_tag, "content") {
                if is_probable_video_url(&content) || content.starts_with("http") {
                    if let Ok(resolved) = base_url.join(&content) {
                        if seen.insert(resolved.to_string()) {
                            sources.push(resolved);
                        }
                    }
                }
            }
            cursor = line_end;
        }
    }

    sources
}

fn extract_attribute_value(tag: &str, attr_name: &str) -> Option<String> {
    let target = format!("{}=", attr_name);
    let idx = tag.find(&target)?;
    let after_eq = &tag[idx + target.len()..];
    let quote = after_eq.chars().next()?;

    if quote == '"' || quote == '\'' {
        let value = &after_eq[1..];
        let end_idx = value.find(quote)?;
        Some(value[..end_idx].trim().to_string())
    } else {
        // Unquoted attribute
        let end_idx = after_eq.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(after_eq.len());
        Some(after_eq[..end_idx].trim().to_string())
    }
}

fn is_probable_video_url(url_str: &str) -> bool {
    let lower = url_str.to_ascii_lowercase();
    lower.contains(".mp4")
        || lower.contains(".webm")
        || lower.contains(".mkv")
        || lower.contains(".m4v")
        || lower.contains(".mov")
        || lower.contains(".flv")
        || lower.contains(".avi")
        || lower.contains(".ts")
        || lower.contains(".m3u8")
        || lower.contains(".mpd")
        || lower.contains("video/")
        || lower.contains("/video")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_html_video_sources() {
        let base_url = Url::parse("https://example.com/watch/video1").unwrap();
        let html = r#"
            <!DOCTYPE html>
            <html>
            <head>
                <meta property="og:video" content="https://cdn.example.com/og_video.mp4" />
                <meta name="twitter:player:stream" content="/stream/twitter_video.webm" />
            </head>
            <body>
                <video controls width="800">
                    <source src="/media/720p.mp4" type="video/mp4">
                    <source src="https://cdn.example.com/1080p.mp4" type="video/mp4">
                    <source src="/media/fallback.webm" type="video/webm">
                </video>
            </body>
            </html>
        "#;

        let sources = extract_html_video_sources(html, &base_url);
        assert_eq!(sources.len(), 5);
        assert!(sources.contains(&Url::parse("https://example.com/media/720p.mp4").unwrap()));
        assert!(sources.contains(&Url::parse("https://cdn.example.com/1080p.mp4").unwrap()));
        assert!(sources.contains(&Url::parse("https://example.com/media/fallback.webm").unwrap()));
        assert!(sources.contains(&Url::parse("https://cdn.example.com/og_video.mp4").unwrap()));
        assert!(sources.contains(&Url::parse("https://example.com/stream/twitter_video.webm").unwrap()));
    }

    #[tokio::test]
    async fn test_dropbox_url_normalization() {
        let client = Client::new();
        let url = Url::parse("https://www.dropbox.com/s/sample123/file.zip?dl=0").unwrap();
        let resolved = DropboxResolver.resolve(&client, &url).await.unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].query(), Some("dl=1"));
    }

    #[tokio::test]
    async fn test_google_drive_id_extraction() {
        let url1 = Url::parse("https://drive.google.com/file/d/1BxyzABC_12345/view?usp=sharing").unwrap();
        assert_eq!(extract_google_drive_id(&url1), Some("1BxyzABC_12345".to_string()));

        let url2 = Url::parse("https://drive.google.com/uc?id=1BxyzABC_12345&export=download").unwrap();
        assert_eq!(extract_google_drive_id(&url2), Some("1BxyzABC_12345".to_string()));
    }

    #[tokio::test]
    async fn test_sourceforge_mirror_generation() {
        let client = Client::new();
        let url = Url::parse("https://sourceforge.net/projects/sevenzip/files/7-Zip/24.09/7z2409-x64.exe/download").unwrap();
        let resolved = SourceForgeResolver.resolve(&client, &url).await.unwrap();
        assert!(resolved.len() >= 5);
        assert!(resolved.iter().any(|u| u.query().unwrap_or("").contains("use_mirror=fastly")));
    }

    #[test]
    fn test_mediafire_html_extraction() {
        let html = r#"<div><a class="input popsok" aria-label="Download file" href="https://download1590.mediafire.com/xyz123/sample.zip" id="downloadButton">Download</a></div>"#;
        let direct = extract_mediafire_direct(html).unwrap();
        assert_eq!(direct, "https://download1590.mediafire.com/xyz123/sample.zip");
    }
}
