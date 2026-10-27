//! What a link's server says before anything is downloaded: --spider, -S (--server-response),
//! --dry-run, --no-clobber and --preview.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use hyperfetch_core::digest::published_checksum;
use hyperfetch_core::engine::{build_client, file_name_from, request_header, DownloadOptions};
use hyperfetch_core::history::{redact_text, redact_url};
use hyperfetch_core::resolver::SmartResolver;
use hyperfetch_core::torrent::parse_torrent_bytes;
use hyperfetch_core::zipdir::ZipEntry;
use indicatif::HumanBytes;
use reqwest::header::{
    HeaderMap, HeaderName, AUTHORIZATION, CONTENT_RANGE, CONTENT_TYPE, COOKIE, ETAG, LAST_MODIFIED, LOCATION,
    PROXY_AUTHORIZATION, SET_COOKIE,
};
use reqwest::{Client, Response, StatusCode};
use url::Url;

use crate::download::{item, Item, Job, Ui};

/// The largest .torrent --dry-run reads to list its files.
const MAX_TORRENT: usize = 16 << 20;

/// The link `url` leads to, as a download resolves it (a file host's page to its file).
pub async fn resolve(client: &Client, url: &Url, options: &DownloadOptions) -> Result<Url, String> {
    let (key, provider) = (options.debrid_api_key.as_deref(), options.debrid_provider.as_deref());
    let resolved = SmartResolver::resolve_with_options(client, url, key, provider, options.proxy.as_deref())
        .await
        .map_err(|e| format!("cannot resolve the link: {}", e))?;
    Ok(resolved.into_iter().next().unwrap_or_else(|| url.clone()))
}

/// Asks the server of `url` for its file's first byte as a download's probe asks: through the
/// client and with the headers a download with `options` has, the user's own headers, cookies and
/// sign-in only for `url`'s host (not after a redirect to another, nor over plain HTTP when it was
/// HTTPS). With `show`, each request's and answer's headers are printed there (-S), sign-ins and
/// cookies masked. The answer's body is left unread.
pub async fn probe(client: &Client, url: &Url, options: &DownloadOptions, show: Option<&Ui>) -> Result<Response, String> {
    let mut at = resolve(client, url, options).await?;
    for _ in 0..10 {
        let headers = request_headers(options, &at, url);
        if let Some(ui) = show {
            ui.error(&format!("> GET {}", redact_url(at.as_str())));
            header_lines('>', &headers).iter().for_each(|line| ui.error(line));
        }
        let response = client.get(at.clone()).headers(headers).send().await.map_err(|e| redact_text(&chain(&e)))?;
        if let Some(ui) = show {
            ui.error(&format!("< {:?} {}", response.version(), response.status()));
            header_lines('<', response.headers()).iter().for_each(|line| ui.error(line));
        }
        // A client carrying the user's headers stops at a redirect to another host; it is followed
        // here, without them.
        let location = response.headers().get(LOCATION).and_then(|l| l.to_str().ok());
        match location.filter(|_| response.status().is_redirection()).and_then(|l| response.url().join(l).ok()) {
            Some(next) => at = next,
            None => return Ok(response),
        }
    }
    Err("too many redirects".to_string())
}

/// `e` and the errors under it, which say why a request failed ("connection refused").
fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut under = e.source();
    while let Some(cause) = under {
        text.push_str(&format!(": {}", cause));
        under = cause.source();
    }
    text
}

/// The headers a download with `options` sends `url` when `own` is the link given (see
/// `engine::build_client`), with the probe's range. The user's own are marked sensitive.
fn request_headers(options: &DownloadOptions, url: &Url, own: &Url) -> HeaderMap {
    let pair = |name: &str, value: &String| (name.to_string(), value.clone());
    let mut headers = SmartResolver::default_anti_qos_headers();
    let referer = options.referer.iter().map(|value| pair("Referer", value));
    let agent = options.user_agent.iter().map(|value| pair("User-Agent", value));
    let probe = [("Range", "bytes=0-0"), ("Accept-Encoding", "identity")].map(|(name, value)| (name.to_string(), value.to_string()));
    for (name, value) in referer.chain(options.headers.iter().cloned()).chain(agent).chain(probe) {
        if let Some((name, value)) = request_header(&name, &value) {
            headers.insert(name, value);
        }
    }
    if url.host_str() == own.host_str() && (own.scheme() != "https" || url.scheme() == "https") {
        let auth = options.auth_header.iter().map(|value| pair("Authorization", value));
        let cookie = options.cookie_string.iter().map(|value| pair("Cookie", value));
        for (name, value) in auth.chain(options.secret_headers.iter().cloned()).chain(options.extra_headers.iter().cloned()).chain(cookie) {
            if let Some((name, mut value)) = request_header(&name, &value) {
                value.set_sensitive(true);
                headers.insert(name, value);
            }
        }
    }
    headers
}

/// The -S lines of `headers`, each after `mark`: sign-ins, cookies and the user's own headers
/// masked.
fn header_lines(mark: char, headers: &HeaderMap) -> Vec<String> {
    const SECRET: [HeaderName; 4] = [AUTHORIZATION, PROXY_AUTHORIZATION, COOKIE, SET_COOKIE];
    headers
        .iter()
        .map(|(name, value)| {
            let value = match value.is_sensitive() || SECRET.contains(name) {
                true => "***".into(),
                false => String::from_utf8_lossy(value.as_bytes()),
            };
            format!("{} {}: {}", mark, name, value)
        })
        .collect()
}

/// What --spider tells of a link.
struct Found {
    /// Where the answer came from, after redirects.
    url: Url,
    name: String,
    size: Option<u64>,
    kind: Option<String>,
    resumable: bool,
    etag: Option<String>,
    modified: Option<String>,
    /// The checksum the site publishes.
    checksum: Option<String>,
}

impl Found {
    /// What the answer to `probe` says, or why the link is bad.
    fn of(response: &Response) -> Result<Self, String> {
        let status = response.status();
        // 416: an empty file has no first byte to send.
        if !status.is_success() && status != StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(format!("HTTP {}", status));
        }
        let header = |name: HeaderName| response.headers().get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
        let ranged = matches!(status, StatusCode::PARTIAL_CONTENT | StatusCode::RANGE_NOT_SATISFIABLE);
        let total = header(CONTENT_RANGE).and_then(|range| range.rsplit('/').next()?.trim().parse().ok());
        Ok(Self {
            url: response.url().clone(),
            name: file_name_from(response.headers(), response.url()),
            size: if ranged { total } else { response.content_length() },
            kind: header(CONTENT_TYPE),
            resumable: status == StatusCode::PARTIAL_CONTENT,
            etag: header(ETAG),
            modified: header(LAST_MODIFIED),
            checksum: None,
        })
    }

    fn lines(&self) -> Vec<String> {
        let size = self.size.map_or("unknown".to_string(), |n| format!("{} ({} bytes)", HumanBytes(n), n));
        let mut lines = vec![
            self.name.clone(),
            format!("  URL:       {}", redact_url(self.url.as_str())),
            format!("  Size:      {}", size),
            format!("  Type:      {}", self.kind.as_deref().unwrap_or("unknown")),
            format!("  Resumable: {}", if self.resumable { "yes" } else { "no" }),
        ];
        lines.extend(self.etag.iter().map(|etag| format!("  ETag:      {}", etag)));
        lines.extend(self.modified.iter().map(|date| format!("  Modified:  {}", date)));
        let checksum = self.checksum.as_ref().map_or("none published".to_string(), |sum| format!("{} (published by the site)", sum));
        lines.push(format!("  Checksum:  {}", checksum));
        lines
    }

    /// The --json item of `link`, with the keys only --spider has.
    fn json(&self, link: &str) -> Item {
        let mut fields = item("ok", Some(link), None, self.size, self.checksum.as_deref(), None, None);
        fields.extend([
            ("name", self.name.clone().into()),
            ("type", self.kind.clone().into()),
            ("resumable", self.resumable.into()),
            ("etag", self.etag.clone().into()),
            ("last_modified", self.modified.clone().into()),
        ]);
        fields
    }
}

/// --spider: what the server of each of `jobs` says of its file, printed (-S: with the headers);
/// how many links are bad. A link not probed is given with the size its input told, if any.
pub async fn spider(jobs: &[(Job, Option<u64>)], ui: &Ui, show: bool) -> usize {
    let mut failed = 0;
    for (job, size) in jobs {
        let Some(url) = job.urls.first() else { continue };
        // shortcut: FTP links and swarm downloads are not probed; give them a probe of their own
        // if --spider is wanted for them.
        if job.options.bittorrent || !matches!(url.scheme(), "http" | "https") {
            if !ui.json {
                ui.print(&format!("{}: not probed (only HTTP and HTTPS links are)", job.label));
            }
            ui.report(item("ok", Some(url.as_str()), None, *size, None, None, None));
            continue;
        }
        let found = async {
            let client = build_client(&job.options)?;
            let response = probe(&client, url, &job.options, show.then_some(ui)).await?;
            let mut found = Found::of(&response)?;
            let headers = response.headers().clone();
            drop(response);
            found.checksum = published_checksum(&client, &found.url, &headers).await;
            Ok::<_, String>(found)
        }
        .await;
        match found {
            Ok(found) => {
                if !ui.json {
                    found.lines().iter().for_each(|line| ui.print(line));
                }
                ui.report(found.json(url.as_str()));
            }
            Err(e) => {
                failed += 1;
                ui.error(&format!("[FAILED] {}: {}", job.label, e));
                ui.report(item("failed", Some(url.as_str()), None, None, None, None, Some(&e)));
            }
        }
    }
    failed
}

/// Whether `path` names a folder the server or yt-dlp names the file in (see `job` in main).
fn ends_with_separator(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().last().is_some_and(|&b| std::path::is_separator(b as char))
}

/// --dry-run: what each of `jobs` would download, and where, its size when the input told it;
/// the files of a swarm torrent one by one. Nothing is written. With `no_clobber`, a file that is
/// there already is "skipped".
pub async fn dry_run(jobs: &[(Job, Option<u64>)], http: &Client, ui: &Ui, no_clobber: bool) {
    for (job, size) in jobs {
        let Some(path) = &job.options.output_path else { continue };
        let url = job.urls.first();
        let swarm = match url.filter(|_| job.options.bittorrent) {
            Some(url) => torrent_files(url, http, path).await,
            None => None,
        };
        for (path, size) in swarm.unwrap_or_else(|| vec![(path.clone(), *size)]) {
            let skipped = no_clobber && path.is_file();
            if !ui.json {
                let shown = size.map_or("size unknown".to_string(), |n| HumanBytes(n).to_string());
                let named = if ends_with_separator(&path) { " (named when downloaded)" } else { "" };
                let there = if skipped { " (there already: skipped)" } else { "" };
                ui.print(&format!("{:>12}  {}{}{}", shown, path.display(), named, there));
            }
            let status = if skipped { "skipped" } else { "ok" };
            ui.report(item(status, url.map(Url::as_str), Some(&path), size, job.options.expected_checksum.as_deref(), None, None));
        }
    }
}

/// The files of the torrent at `url` (a `file:` or HTTP(S) link) under `root`, its folder (its
/// file, for a single-file torrent), with their sizes; None for a magnet or a torrent unread.
async fn torrent_files(url: &Url, http: &Client, root: &Path) -> Option<Vec<(PathBuf, Option<u64>)>> {
    let bytes = match url.scheme() {
        "file" => tokio::fs::read(url.to_file_path().ok()?).await.ok()?,
        "http" | "https" => {
            let mut response = http.get(url.clone()).send().await.ok()?.error_for_status().ok()?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.ok()? {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > MAX_TORRENT {
                    return None;
                }
            }
            bytes
        }
        _ => return None,
    };
    let info = parse_torrent_bytes(&bytes).ok()?;
    let single = matches!(&info.files[..], [file] if file.path == [info.name.clone()]);
    let path = |parts: &[String]| if single { root.to_path_buf() } else { parts.iter().fold(root.to_path_buf(), |path, part| path.join(part)) };
    Some(info.files.iter().map(|file| (path(&file.path), Some(file.length))).collect())
}

/// Where `job` saves its file when that is known before it runs: its own path, else the name its
/// server gives (asked as [`probe`] asks) in its folder, for a direct HTTP(S) or FTP link.
async fn target_of(job: &Job) -> Option<PathBuf> {
    let path = job.options.output_path.clone()?;
    if !ends_with_separator(&path) {
        return Some(path);
    }
    let url = job.urls.first().filter(|_| !job.needs_ffmpeg() && !job.options.bittorrent)?;
    let name = match url.scheme() {
        "ftp" => file_name_from(&HeaderMap::new(), url),
        "http" | "https" => {
            let client = build_client(&job.options).ok()?;
            let response = probe(&client, url, &job.options, None).await.ok().filter(|r| r.status().is_success())?;
            file_name_from(response.headers(), response.url())
        }
        _ => return None,
    };
    Some(path.join(name))
}

/// `jobs` less those whose file is there already (--no-clobber), each said to be skipped.
pub async fn without_existing(jobs: Vec<Job>, ui: &Ui) -> Vec<Job> {
    let targets: Vec<(Job, Option<PathBuf>)> = futures_util::stream::iter(jobs)
        .map(|job| async move {
            let target = target_of(&job).await;
            (job, target)
        })
        .buffered(8)
        .collect()
        .await;
    let mut kept = Vec::new();
    for (job, target) in targets {
        let Some(path) = target.filter(|path| path.is_file()) else {
            kept.push(job);
            continue;
        };
        if !ui.quiet() {
            ui.print(&format!("[SKIPPED] {}: it is there already (--no-clobber)", path.display()));
        }
        let size = std::fs::metadata(&path).ok().map(|m| m.len());
        let checksum = job.options.expected_checksum.as_deref();
        ui.report(item("skipped", job.urls.first().map(Url::as_str), Some(&path), size, checksum, None, None));
    }
    kept
}

/// -S before downloading: the headers of each HTTP(S) link's probe.
pub async fn show_responses(jobs: &[Job], ui: &Ui) {
    for job in jobs {
        let Some(url) = job.urls.first().filter(|url| matches!(url.scheme(), "http" | "https") && !job.options.bittorrent) else {
            continue;
        };
        let shown = async { probe(&build_client(&job.options)?, url, &job.options, Some(ui)).await.map(drop) }.await;
        if let Err(e) = shown {
            ui.error(&format!("{}: {}", job.label, e));
        }
    }
}

/// --preview's lines for a ZIP's `entries`: the size and name of each, then the total.
pub fn zip_lines(entries: &[ZipEntry]) -> Vec<String> {
    let mut lines: Vec<String> = entries.iter().map(|e| format!("{:>12}  {}", HumanBytes(e.size).to_string(), e.name)).collect();
    let (size, packed) = entries.iter().fold((0u64, 0u64), |(size, packed), e| (size.saturating_add(e.size), packed.saturating_add(e.compressed)));
    lines.push(format!("{} entries, {} ({} compressed)", entries.len(), HumanBytes(size), HumanBytes(packed)));
    lines
}
