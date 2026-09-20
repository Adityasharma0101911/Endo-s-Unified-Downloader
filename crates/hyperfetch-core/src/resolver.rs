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
        let is_archive_host = url.host_str().map_or(false, |h| h.ends_with("archive.org"));
        let path = url.path();
        is_archive_host && (path.starts_with("/download/") || path.contains("/items/"))
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let segments: Vec<&str> = url.path_segments()
            .ok_or_else(|| ResolverError::Parse("Missing path segments".to_string()))?
            .collect();

        let (identifier, filename) = if segments.len() >= 3 && segments[0] == "download" {
            (segments[1].to_string(), segments[2..].join("/"))
        } else if let Some(pos) = segments.iter().position(|&s| s == "items") {
            if segments.len() > pos + 2 {
                (segments[pos + 1].to_string(), segments[pos + 2..].join("/"))
            } else {
                return Ok(vec![url.clone()]);
            }
        } else {
            return Ok(vec![url.clone()]);
        };

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

        let lb_url_str = format!("https://archive.org/download/{}/{}", identifier, filename);
        if let Ok(lb_url) = Url::parse(&lb_url_str) {
            if !mirror_urls.contains(&lb_url) {
                mirror_urls.push(lb_url);
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

/// Vimeo Resolver (Extracts unthrottled progressive MP4 and HLS streams via player config API)
pub struct VimeoResolver;

impl HostResolver for VimeoResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("vimeo.com"))
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let video_id = extract_vimeo_id(url);
        if let Some(id) = video_id {
            let config_url = format!("https://player.vimeo.com/video/{}/config", id);
            if let Ok(resp) = client.get(&config_url).send().await {
                if resp.status().is_success() {
                    if let Ok(bytes) = resp.bytes().await {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                            let mut urls = Vec::new();

                            // 1. Check progressive MP4 files (highest quality first)
                            if let Some(files) = json.pointer("/request/files/progressive").and_then(|v| v.as_array()) {
                                let mut progressive: Vec<(u64, String)> = files.iter().filter_map(|f| {
                                    let u = f.get("url")?.as_str()?;
                                    let height = f.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
                                    Some((height, u.to_string()))
                                }).collect();

                                progressive.sort_by(|a, b| b.0.cmp(&a.0));
                                for (_, u_str) in progressive {
                                    if let Ok(u) = Url::parse(&u_str) {
                                        urls.push(u);
                                    }
                                }
                            }

                            // 2. Fallback to HLS master playlist
                            if urls.is_empty() {
                                if let Some(cdns) = json.pointer("/request/files/hls/cdns").and_then(|v| v.as_object()) {
                                    for (_cdn, cdn_obj) in cdns {
                                        if let Some(hls_url_str) = cdn_obj.get("url").and_then(|u| u.as_str()) {
                                            if let Ok(u) = Url::parse(hls_url_str) {
                                                urls.push(u);
                                                break;
                                            }
                                        }
                                    }
                                }
                            }

                            if !urls.is_empty() {
                                return Ok(urls);
                            }
                        }
                    }
                }
            }
        }

        Ok(vec![url.clone()])
    }
}

/// Reddit Video Resolver (Extracts combined video+audio HLS streams and fallback DASH URLs)
pub struct RedditResolver;

impl HostResolver for RedditResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("reddit.com") || h.contains("redd.it"))
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        // Case 1: Direct v.redd.it/:id
        if url.host_str().map_or(false, |h| h.contains("v.redd.it")) {
            let id = url.path().trim_matches('/');
            if !id.is_empty() && !id.contains('/') {
                // v.redd.it provides an HLSPlaylist.m3u8 containing synchronized video + audio
                let hls_url = format!("https://v.redd.it/{}/HLSPlaylist.m3u8", id);
                if let Ok(u) = Url::parse(&hls_url) {
                    return Ok(vec![u]);
                }
            }
        }

        // Case 2: reddit.com/r/.../comments/:id/...
        if let Some(comment_id) = extract_reddit_id(url) {
            let json_url = format!("https://www.reddit.com/comments/{}.json", comment_id);
            if let Ok(resp) = client.get(&json_url).send().await {
                if resp.status().is_success() {
                    if let Ok(bytes) = resp.bytes().await {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                            // Traverse post data to find reddit_video
                            if let Some(hls_url_str) = find_json_string(&json, "hls_url") {
                                if let Ok(u) = Url::parse(&hls_url_str) {
                                    return Ok(vec![u]);
                                }
                            }
                            if let Some(fb_url_str) = find_json_string(&json, "fallback_url") {
                                if let Ok(u) = Url::parse(&fb_url_str) {
                                    return Ok(vec![u]);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(vec![url.clone()])
    }
}

/// Twitter / X Video Resolver (Extracts highest bitrate MP4 streams via syndication API)
pub struct TwitterResolver;

impl HostResolver for TwitterResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("twitter.com") || h.contains("x.com"))
            && url.path().contains("/status/")
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let status_id = extract_status_id(url);
        if let Some(id) = status_id {
            let syndication_url = format!("https://cdn.syndication.twimg.com/tweet-result?id={}&token=x", id);
            if let Ok(resp) = client.get(&syndication_url).send().await {
                if resp.status().is_success() {
                    if let Ok(bytes) = resp.bytes().await {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                            if let Some(variants) = json.pointer("/video/variants").and_then(|v| v.as_array()) {
                                let mut mp4s: Vec<(u64, String)> = variants.iter().filter_map(|v| {
                                    let content_type = v.get("type")?.as_str()?;
                                    if content_type == "video/mp4" {
                                        let u = v.get("src")?.as_str()?;
                                        let bitrate = v.get("bitrate").and_then(|b| b.as_u64()).unwrap_or(0);
                                        Some((bitrate, u.to_string()))
                                    } else {
                                        None
                                    }
                                }).collect();

                                mp4s.sort_by(|a, b| b.0.cmp(&a.0));
                                let mut urls = Vec::new();
                                for (_, u_str) in mp4s {
                                    if let Ok(u) = Url::parse(&u_str) {
                                        urls.push(u);
                                    }
                                }
                                if !urls.is_empty() {
                                    return Ok(urls);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(vec![url.clone()])
    }
}

/// TikTok Video Resolver (Extracts direct play and download CDN streams)
pub struct TikTokResolver;

impl HostResolver for TikTokResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("tiktok.com"))
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        if let Ok(resp) = client.get(url.clone()).send().await {
            if resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                // Search for playAddr or downloadAddr in HTML / JSON rehydration state
                if let Some(play_addr) = extract_tiktok_addr(&text, "playAddr")
                    .or_else(|| extract_tiktok_addr(&text, "downloadAddr"))
                {
                    // Clean escaped unicode / backslashes in JSON (e.g. \u0026 -> &)
                    let cleaned = play_addr.replace("\\u0026", "&").replace("\\/", "/");
                    if let Ok(u) = Url::parse(&cleaned) {
                        return Ok(vec![u]);
                    }
                }
            }
        }

        Ok(vec![url.clone()])
    }
}

/// Facebook Video Resolver (Extracts direct playable HD and SD progressive streams)
pub struct FacebookResolver;

impl HostResolver for FacebookResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("facebook.com") || h.contains("fb.watch"))
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        if let Ok(resp) = client.get(url.clone()).send().await {
            if resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                let urls = extract_facebook_video_urls(&text);
                if !urls.is_empty() {
                    return Ok(urls);
                }
            }
        }

        Ok(vec![url.clone()])
    }
}

/// Dailymotion Resolver (Extracts highest resolution MP4 or HLS master playlist from metadata API)
pub struct DailymotionResolver;

impl HostResolver for DailymotionResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("dailymotion.com") || h.contains("dai.ly"))
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        let video_id = extract_dailymotion_id(url);
        if let Some(id) = video_id {
            let meta_url = format!("https://www.dailymotion.com/player/metadata/video/{}", id);
            if let Ok(resp) = client.get(&meta_url).send().await {
                if resp.status().is_success() {
                    if let Ok(bytes) = resp.bytes().await {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                            let mut urls = Vec::new();

                            if let Some(qualities) = json.get("qualities").and_then(|q| q.as_object()) {
                                let mut quality_keys: Vec<&String> = qualities.keys().collect();
                                quality_keys.sort_by(|a, b| {
                                    let a_num = a.parse::<u64>().unwrap_or(0);
                                    let b_num = b.parse::<u64>().unwrap_or(0);
                                    b_num.cmp(&a_num)
                                });

                                for key in quality_keys {
                                    if let Some(arr) = qualities.get(key).and_then(|v| v.as_array()) {
                                        for item in arr {
                                            if let Some(u_str) = item.get("url").and_then(|u| u.as_str()) {
                                                if let Ok(u) = Url::parse(u_str) {
                                                    if !urls.contains(&u) {
                                                        urls.push(u);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            if !urls.is_empty() {
                                return Ok(urls);
                            }
                        }
                    }
                }
            }
        }

        Ok(vec![url.clone()])
    }
}

/// Instagram Video Resolver (Extracts progressive MP4 streams from video versions metadata)
pub struct InstagramResolver;

impl HostResolver for InstagramResolver {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str().map_or(false, |h| h.contains("instagram.com"))
    }

    async fn resolve(&self, client: &Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
        if let Ok(resp) = client.get(url.clone()).send().await {
            if resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                let urls = extract_instagram_video_urls(&text);
                if !urls.is_empty() {
                    return Ok(urls);
                }
            }
        }

        Ok(vec![url.clone()])
    }
}

/// HTML5 Video Extractor (Extracts direct media streams from webpage <video>, <source>, and OpenGraph tags)
pub struct HtmlVideoResolver;

impl HostResolver for HtmlVideoResolver {
    fn can_handle(&self, url: &Url) -> bool {
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

        if VimeoResolver.can_handle(url) {
            if let Ok(mirrors) = VimeoResolver.resolve(client, url).await {
                if mirrors != vec![url.clone()] && !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        if RedditResolver.can_handle(url) {
            if let Ok(mirrors) = RedditResolver.resolve(client, url).await {
                if mirrors != vec![url.clone()] && !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        if TwitterResolver.can_handle(url) {
            if let Ok(mirrors) = TwitterResolver.resolve(client, url).await {
                if mirrors != vec![url.clone()] && !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        if TikTokResolver.can_handle(url) {
            if let Ok(mirrors) = TikTokResolver.resolve(client, url).await {
                if mirrors != vec![url.clone()] && !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        if FacebookResolver.can_handle(url) {
            if let Ok(mirrors) = FacebookResolver.resolve(client, url).await {
                if mirrors != vec![url.clone()] && !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        if DailymotionResolver.can_handle(url) {
            if let Ok(mirrors) = DailymotionResolver.resolve(client, url).await {
                if mirrors != vec![url.clone()] && !mirrors.is_empty() {
                    return mirrors;
                }
            }
        }

        if InstagramResolver.can_handle(url) {
            if let Ok(mirrors) = InstagramResolver.resolve(client, url).await {
                if mirrors != vec![url.clone()] && !mirrors.is_empty() {
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

fn extract_vimeo_id(url: &Url) -> Option<String> {
    for seg in url.path_segments()? {
        if !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()) {
            return Some(seg.to_string());
        }
    }
    None
}

fn extract_reddit_id(url: &Url) -> Option<String> {
    let segments: Vec<&str> = url.path_segments()?.collect();
    if let Some(pos) = segments.iter().position(|&s| s == "comments") {
        if let Some(&id) = segments.get(pos + 1) {
            return Some(id.to_string());
        }
    }
    None
}

fn extract_status_id(url: &Url) -> Option<String> {
    let segments: Vec<&str> = url.path_segments()?.collect();
    if let Some(pos) = segments.iter().position(|&s| s == "status") {
        if let Some(&id) = segments.get(pos + 1) {
            let num: String = id.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !num.is_empty() {
                return Some(num);
            }
        }
    }
    None
}

fn find_json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(val) = map.get(key).and_then(|v| v.as_str()) {
                return Some(val.to_string());
            }
            for v in map.values() {
                if let Some(res) = find_json_string(v, key) {
                    return Some(res);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                if let Some(res) = find_json_string(v, key) {
                    return Some(res);
                }
            }
        }
        _ => {}
    }
    None
}

fn extract_tiktok_addr(html: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", key);
    if let Some(idx) = html.find(&pattern) {
        let sub = &html[idx + pattern.len()..];
        if let Some(end) = sub.find('"') {
            return Some(sub[..end].to_string());
        }
    }
    None
}

fn extract_dailymotion_id(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    if host.contains("dai.ly") {
        return segments.first().map(|s| s.to_string());
    }
    if let Some(pos) = segments.iter().position(|&s| s == "video") {
        if let Some(&id) = segments.get(pos + 1) {
            let clean_id = id.split('_').next().unwrap_or(id);
            return Some(clean_id.to_string());
        }
    }
    None
}

fn extract_facebook_video_urls(html: &str) -> Vec<Url> {
    let mut urls = Vec::new();
    let keys = [
        "playable_url_quality_hd",
        "browser_native_hd_url",
        "playable_url",
        "browser_native_sd_url",
    ];
    for key in keys {
        let pattern = format!("\"{}\":\"", key);
        let mut search_idx = 0;
        while let Some(idx) = html[search_idx..].find(&pattern) {
            let actual_idx = search_idx + idx + pattern.len();
            let sub = &html[actual_idx..];
            if let Some(end) = sub.find('"') {
                let raw_url = &sub[..end];
                let cleaned = raw_url
                    .replace("\\u0026", "&")
                    .replace("\\/", "/")
                    .replace("&amp;", "&");
                if let Ok(u) = Url::parse(&cleaned) {
                    if !urls.contains(&u) {
                        urls.push(u);
                    }
                }
                search_idx = actual_idx + end;
            } else {
                break;
            }
        }
    }
    urls
}

fn extract_instagram_video_urls(html: &str) -> Vec<Url> {
    let mut urls = Vec::new();
    let pattern = "\"video_url\":\"";
    let mut search_idx = 0;
    while let Some(idx) = html[search_idx..].find(pattern) {
        let actual_idx = search_idx + idx + pattern.len();
        let sub = &html[actual_idx..];
        if let Some(end) = sub.find('"') {
            let raw_url = &sub[..end];
            let cleaned = raw_url
                .replace("\\u0026", "&")
                .replace("\\/", "/")
                .replace("&amp;", "&");
            if let Ok(u) = Url::parse(&cleaned) {
                if !urls.contains(&u) {
                    urls.push(u);
                }
            }
            search_idx = actual_idx + end;
        } else {
            break;
        }
    }
    urls
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

    #[test]
    fn test_platform_id_extractions() {
        let vimeo_url = Url::parse("https://vimeo.com/123456789").unwrap();
        assert_eq!(extract_vimeo_id(&vimeo_url), Some("123456789".to_string()));

        let reddit_url = Url::parse("https://www.reddit.com/r/rust/comments/abc123z/awesome_post/").unwrap();
        assert_eq!(extract_reddit_id(&reddit_url), Some("abc123z".to_string()));

        let twitter_url = Url::parse("https://twitter.com/user/status/1789012345678901234?s=20").unwrap();
        assert_eq!(extract_status_id(&twitter_url), Some("1789012345678901234".to_string()));

        let tiktok_html = r#"<script id="SIGI_STATE">{"playAddr":"https:\/\/v16.tiktokcdn.com\/video\/123\/?token=abc"}</script>"#;
        assert_eq!(extract_tiktok_addr(tiktok_html, "playAddr"), Some("https:\\/\\/v16.tiktokcdn.com\\/video\\/123\\/?token=abc".to_string()));

        let dailymotion_url = Url::parse("https://www.dailymotion.com/video/x8xyz12_some-video-title").unwrap();
        assert_eq!(extract_dailymotion_id(&dailymotion_url), Some("x8xyz12".to_string()));

        let fb_html = r#"{"playable_url_quality_hd":"https:\/\/video.xx.fbcdn.net\/v\/hd.mp4?oh=123\u0026oe=456","playable_url":"https:\/\/video.xx.fbcdn.net\/v\/sd.mp4?oh=123\u0026oe=456"}"#;
        let fb_urls = extract_facebook_video_urls(fb_html);
        assert_eq!(fb_urls.len(), 2);
        assert_eq!(fb_urls[0].as_str(), "https://video.xx.fbcdn.net/v/hd.mp4?oh=123&oe=456");

        let ig_html = r#"{"video_url":"https:\/\/instagram.xx.fbcdn.net\/v\/t50.2886-16\/video.mp4?_nc_cat=100\u0026oh=789"}"#;
        let ig_urls = extract_instagram_video_urls(ig_html);
        assert_eq!(ig_urls.len(), 1);
        assert_eq!(ig_urls[0].as_str(), "https://instagram.xx.fbcdn.net/v/t50.2886-16/video.mp4?_nc_cat=100&oh=789");
    }

    #[test]
    fn test_resolver_can_handle() {
        assert!(VimeoResolver.can_handle(&Url::parse("https://vimeo.com/123456").unwrap()));
        assert!(RedditResolver.can_handle(&Url::parse("https://v.redd.it/xyz123").unwrap()));
        assert!(TwitterResolver.can_handle(&Url::parse("https://x.com/user/status/987654").unwrap()));
        assert!(TikTokResolver.can_handle(&Url::parse("https://www.tiktok.com/@user/video/12345").unwrap()));
        assert!(FacebookResolver.can_handle(&Url::parse("https://www.facebook.com/watch/?v=12345").unwrap()));
        assert!(DailymotionResolver.can_handle(&Url::parse("https://dai.ly/x8xyz").unwrap()));
        assert!(InstagramResolver.can_handle(&Url::parse("https://www.instagram.com/reel/C12345/").unwrap()));
        assert!(ArchiveOrgResolver.can_handle(&Url::parse("https://archive.org/download/item/file.zip").unwrap()));
        assert!(ArchiveOrgResolver.can_handle(&Url::parse("https://dn720001.ca.archive.org/0/items/fn-v8-archive/builds/8.51-CL-6165369.7z").unwrap()));
    }
}
