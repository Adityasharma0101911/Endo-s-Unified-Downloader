//! The local HTTP API the browser extension talks to, on 127.0.0.1 only: `GET /ping` finds the
//! app, `POST /add` adds a download with what the browser sent to fetch it, and the `/record/…`
//! routes take the video the extension records from a page (see `recording`). Every route
//! answers only the extension and local programs, which send no web `Origin`; web pages get 403.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use eframe::egui;
use hyperfetch_core::{engine, updater};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::recording::{self, Recordings, Refusal};
use crate::AppEvent;

/// Ports tried in turn; the extension looks for the app on each.
const PORTS: [u16; 4] = [49152, 49153, 49154, 49155];
/// What `GET /ping` calls the app, which the extension checks.
const APP: &str = "endos-unified-downloader";
/// Origins of browser extensions, the only web origins trusted beyond `/add`.
const EXTENSION_ORIGINS: [&str; 3] = ["chrome-extension://", "moz-extension://", "safari-web-extension://"];
/// Largest request line and headers.
const MAX_HEAD: usize = 64 * 1024;
/// Largest body of `POST /add` (room for a [`MAX_PLAYLIST`] playlist escaped in JSON and a full
/// cookie jar), of a recording chunk, and of any other request.
const MAX_ADD_BODY: usize = 4 << 20;
const MAX_CHUNK_BODY: usize = 64 << 20;
const MAX_BODY: usize = 64 * 1024;
/// Largest playlist a page built itself that `/add` takes.
const MAX_PLAYLIST: usize = 1 << 20;
/// Most cookies of a `cookie_jar` written for a download.
const MAX_JAR_COOKIES: usize = 500;
/// How long a connection may take to send its request, so a stalled client never holds a task.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Request headers of the browser's that a download does not send: the engine sets them per
/// request (Range, Accept-Encoding, …), they belong to the browser's connection, or they reach
/// the download another way (Cookie, Referer). `sec-fetch-*` goes too.
const DROPPED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "range",
    "if-range",
    "if-match",
    "if-none-match",
    "if-modified-since",
    "if-unmodified-since",
    "accept-encoding",
    "cookie",
    "referer",
    "proxy-authorization",
    "proxy-connection",
];

/// A download sent to `POST /add`: the link, with what the browser sent to fetch it.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RemoteAddPayload {
    pub url: String,
    /// The Cookie header the browser sent to `url`.
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    /// Other request headers the browser sent (Origin, Authorization, sec-ch-ua, …).
    pub headers: Option<BTreeMap<String, String>>,
    /// The file name, with its extension, to save as in the save folder.
    pub filename: Option<String>,
    /// The browser saw the link read as an HLS playlist (see `DownloadOptions::hls`).
    #[serde(default)]
    pub hls: bool,
    /// Remux the HLS stream into an MP4 (see `DownloadOptions::hls_to_mp4`), over the Settings
    /// value, for this download alone.
    pub mp4: Option<bool>,
    /// The browser's cookies for every host the download fetches from, saved in place of
    /// `cookies` when one is usable (see [`jar_file_text`]).
    #[serde(default)]
    pub cookie_jar: Vec<JarCookie>,
    /// The link is a DASH manifest (see `DownloadOptions::dash`).
    #[serde(default)]
    pub dash: bool,
    /// The tallest video wanted, in pixels (see `DownloadOptions::height`).
    pub height: Option<u32>,
    /// An HLS playlist the page built itself, `url` being only the base of its links (see
    /// `DownloadOptions::playlist_text`); [`parse_add`] takes one of at most [`MAX_PLAYLIST`]
    /// bytes that starts with `#EXTM3U`.
    pub playlist: Option<String>,
}

/// A cookie of the browser's, as the extension reads it (chrome.cookies).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct JarCookie {
    pub domain: String,
    pub path: String,
    pub name: String,
    pub value: String,
    pub secure: bool,
    pub http_only: bool,
    /// Sent to `domain` alone, not to its subdomains.
    pub host_only: bool,
    /// When it expires, in Unix seconds; 0 for a session cookie.
    pub expires: u64,
}

impl RemoteAddPayload {
    /// The headers to send with the download (see `DownloadOptions::headers`): the browser's
    /// User-Agent and other headers, but for [`DROPPED_HEADERS`], `sec-fetch-*`, empty and
    /// invalid ones; and its Authorization header apart, which the download only sends to the
    /// hosts the user named (its `auth_header`).
    pub fn request_headers(&self) -> (Vec<(String, String)>, Option<String>) {
        let user_agent = self.user_agent.iter().map(|value| ("User-Agent", value.as_str()));
        let others = self.headers.iter().flatten().map(|(name, value)| (name.as_str(), value.as_str()));
        let mut headers = Vec::new();
        let mut auth = None;
        for (name, value) in user_agent.chain(others) {
            let (lower, value) = (name.to_ascii_lowercase(), value.trim());
            let dropped = DROPPED_HEADERS.contains(&lower.as_str()) || lower.starts_with("sec-fetch-");
            if dropped || value.is_empty() || engine::request_header(name, value).is_none() {
                continue;
            }
            if lower == "authorization" {
                auth = Some(value.to_string());
            } else {
                headers.push((name.to_string(), value.to_string()));
            }
        }
        (headers, auth)
    }

    /// The browser's referer, when it is one a request can carry.
    pub fn referer(&self) -> Option<String> {
        let referer = self.referer.as_deref()?.trim();
        (!referer.is_empty() && engine::request_header("Referer", referer).is_some()).then(|| referer.to_string())
    }

    /// The name to save the file as, made safe for the save folder (see
    /// `engine::sanitize_filename`); None when nothing usable is left.
    pub fn file_name(&self) -> Option<String> {
        self.filename.as_deref().map(engine::sanitize_filename).filter(|name| !name.is_empty())
    }

    /// The playlist the page built, without a byte order mark or blank space around it.
    pub fn playlist_text(&self) -> Option<&str> {
        self.playlist.as_deref().map(|text| text.trim_start_matches('\u{feff}').trim())
    }

    /// The Netscape cookies file to download `url` with: the browser's `cookie_jar` (see
    /// [`jar_file_text`]), else the Cookie header it sent to `url` (see [`cookies_file_text`]).
    /// None when neither holds a usable cookie.
    pub fn cookies_file(&self, url: &Url) -> Option<String> {
        jar_file_text(&self.cookie_jar).or_else(|| cookies_file_text(url, self.cookies.as_deref()?))
    }
}

/// A Netscape cookies file holding the cookies of `jar` (the first [`MAX_JAR_COOKIES`] usable
/// ones): each for its domain and its subdomains, or for its host alone when it is host-only,
/// HttpOnly ones marked so. One without a domain or name, or with a field a tab or line break
/// would end, is left out; a path that is no path is written as `/`. None when none is left.
pub fn jar_file_text(jar: &[JarCookie]) -> Option<String> {
    let flag = |on: bool| if on { "TRUE" } else { "FALSE" };
    let lines: Vec<String> = jar
        .iter()
        .filter_map(|cookie| {
            let host = cookie.domain.trim_start_matches('.');
            let path = if cookie.path.starts_with('/') { cookie.path.as_str() } else { "/" };
            let fields = [host, path, cookie.name.as_str(), cookie.value.as_str()];
            if host.is_empty() || cookie.name.is_empty() || fields.iter().any(|field| field.contains(char::is_control)) {
                return None;
            }
            let (domain, subdomains) = if cookie.host_only { (host.to_string(), false) } else { (format!(".{}", host), true) };
            let http_only = if cookie.http_only { "#HttpOnly_" } else { "" };
            let (subdomains, secure, expires) = (flag(subdomains), flag(cookie.secure), cookie.expires);
            Some(format!("{http_only}{domain}\t{subdomains}\t{path}\t{secure}\t{expires}\t{}\t{}", cookie.name, cookie.value))
        })
        .take(MAX_JAR_COOKIES)
        .collect();
    (!lines.is_empty()).then(|| format!("# Netscape HTTP Cookie File\n{}\n", lines.join("\n")))
}

/// A Netscape cookies file holding the cookies of `cookie_header` (what the browser sent to
/// `url`) for the host of `url` alone, as session cookies, secure when `url` is https. None when
/// it holds no usable cookie.
pub fn cookies_file_text(url: &Url, cookie_header: &str) -> Option<String> {
    let host = url.host_str()?;
    let secure = if url.scheme() == "https" { "TRUE" } else { "FALSE" };
    let lines: Vec<String> = cookie_header
        .split(';')
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            let (name, value) = (name.trim(), value.trim());
            // A tab or a line break would end the field or the line.
            let usable = !name.is_empty() && !format!("{}{}", name, value).contains(char::is_control);
            usable.then(|| format!("{}\tFALSE\t/\t{}\t0\t{}\t{}", host, secure, name, value))
        })
        .collect();
    (!lines.is_empty()).then(|| format!("# Netscape HTTP Cookie File\n{}\n", lines.join("\n")))
}

/// Saves the cookies file `text` (see [`RemoteAddPayload::cookies_file`]) of the download of
/// `url` as `<home>/.hyperfetch/cookies/<host>.txt`, in place of the last one of that host, for
/// the download to read. None when it could not be written (logged).
pub fn save_cookies(url: &Url, text: &str) -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok()?;
    let dir = PathBuf::from(home).join(".hyperfetch").join("cookies");
    let path = dir.join(format!("{}.txt", engine::sanitize_filename(url.host_str()?)));
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, text)) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!("Could not save the browser's cookies as {}: {}", path.display(), e);
            None
        }
    }
}

/// Serves the API on the first free port of [`PORTS`] for as long as the app runs, handing what
/// it receives to the UI thread through `events`.
pub fn spawn(events: mpsc::Sender<AppEvent>, ctx: egui::Context, rt: &tokio::runtime::Handle) {
    rt.spawn(async move {
        for port in PORTS {
            match TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => {
                    tracing::info!("Local IPC listener bound on 127.0.0.1:{}", port);
                    let recordings = Recordings::new(std::env::temp_dir().join("endos-recordings"));
                    let server = Arc::new(Server { port, events, ctx, recordings, app_dir: updater::app_dir() });
                    // What an earlier run left recording is joined before this one records.
                    for finished in server.recordings.recover().await {
                        server.send(AppEvent::Recorded(finished));
                    }
                    return listen(listener, server).await;
                }
                Err(e) => tracing::debug!("Could not bind IPC port {}: {}", port, e),
            }
        }
        tracing::warn!("Failed to bind local IPC listener on ports 49152-49155");
    });
}

async fn listen(listener: TcpListener, server: Arc<Server>) {
    loop {
        match listener.accept().await {
            Ok((socket, _)) => {
                let server = Arc::clone(&server);
                tokio::spawn(async move { server.serve(socket).await });
            }
            // A connection lost before it was taken; the pause keeps a lasting failure (no
            // sockets left) from spinning.
            Err(e) => {
                tracing::debug!("IPC accept failed: {}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// What the API answers from: its port, the recordings open, the way to the UI thread, and the
/// app's folder, whose `extension` folder an update replaces.
struct Server {
    port: u16,
    events: mpsc::Sender<AppEvent>,
    ctx: egui::Context,
    recordings: Recordings,
    app_dir: Option<PathBuf>,
}

impl Server {
    /// Answers the one request `stream` sends, then closes it.
    async fn serve(&self, mut stream: impl AsyncRead + AsyncWrite + Unpin) {
        let response = match tokio::time::timeout(READ_TIMEOUT, read_request(&mut stream)).await {
            Ok(Ok(request)) => self.respond(request).await,
            Ok(Err(response)) => response,
            Err(_) => Response::error("408 Request Timeout", None, "the request took too long"),
        };
        let _ = stream.write_all(&response.to_bytes()).await;
        let _ = stream.shutdown().await;
    }

    fn send(&self, event: AppEvent) {
        if self.events.send(event).is_ok() {
            self.ctx.request_repaint();
        }
    }

    async fn respond(&self, request: Request) -> Response {
        let Request { method, path, query, allow_origin, content_length, body } = request;
        let reply = |status: &'static str, body: serde_json::Value| Response::json(status, allow_origin.clone(), body);
        let refused = |refusal: Refusal| match refusal {
            Refusal::Unknown => Response::error("404 Not Found", allow_origin.clone(), "unknown recording"),
            Refusal::Limit(why) => Response::error("507 Insufficient Storage", allow_origin.clone(), why),
            Refusal::Disk(why) => Response::error("500 Internal Server Error", allow_origin.clone(), &why),
        };
        match (method.as_str(), path.as_str()) {
            ("GET", "/ping") => {
                let mut ping = json!({"app": APP, "version": env!("CARGO_PKG_VERSION"), "port": self.port});
                // The version of the extension next to the app (read anew, as an update may have
                // replaced it), so an older one loaded from there reloads itself.
                if let Some(version) = self.app_dir.as_deref().and_then(updater::extension_version) {
                    ping["extension"] = version.into();
                }
                reply("200 OK", ping)
            }
            ("POST", "/add") => match parse_add(&body) {
                Ok(payload) => {
                    self.send(AppEvent::RemoteAdd(payload));
                    reply("200 OK", json!({"status": "queued"}))
                }
                Err(e) => Response::error("400 Bad Request", allow_origin.clone(), &e),
            },
            ("POST", "/record/start") => {
                #[derive(Default, serde::Deserialize)]
                #[serde(default)]
                struct Start {
                    title: String,
                    page_url: String,
                }
                match serde_json::from_slice::<Start>(&body) {
                    Ok(start) => {
                        // Room is made by joining what browsers that went away left open.
                        for finished in self.recordings.close_stale().await {
                            self.send(AppEvent::Recorded(finished));
                        }
                        match self.recordings.start(&start.title, &start.page_url).await {
                            Ok(id) => reply("200 OK", json!({"id": id})),
                            Err(refusal) => refused(refusal),
                        }
                    }
                    Err(e) => Response::error("400 Bad Request", allow_origin.clone(), &format!("invalid JSON: {}", e)),
                }
            }
            ("POST", path) => match record_route(path) {
                Some((id, "chunk")) => {
                    let param = |key: &str| url::form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
                    let track = param("track").and_then(|t| t.parse::<u8>().ok()).filter(|&t| t <= recording::MAX_TRACK);
                    // Part 0 unless it says another, of 0 to 255.
                    let part = param("part").map_or(Some(0), |p| p.parse::<u8>().ok());
                    let (Some(track), Some(part), Some(_)) = (track, part, content_length) else {
                        let why = "a chunk needs a track of 0 to 15, a part of 0 to 255 and a Content-Length";
                        return Response::error("400 Bad Request", allow_origin.clone(), why);
                    };
                    match self.recordings.append(id, track, part, &param("mime").unwrap_or_default(), &body).await {
                        Ok(bytes) => reply("200 OK", json!({"ok": true, "bytes": bytes})),
                        Err(refusal) => refused(refusal),
                    }
                }
                Some((id, "finish")) => match self.recordings.finish(id).await {
                    Ok(finished) => {
                        // One too small to keep was deleted: nothing to tell.
                        if let Some(finished) = finished {
                            self.send(AppEvent::Recorded(finished));
                        }
                        reply("200 OK", json!({"status": "merging"}))
                    }
                    Err(refusal) => refused(refusal),
                },
                Some((id, "abort")) => match self.recordings.abort(id).await {
                    Ok(()) => reply("200 OK", json!({"status": "aborted"})),
                    Err(refusal) => refused(refusal),
                },
                _ => Response::error("404 Not Found", allow_origin.clone(), "not found"),
            },
            _ => Response::error("404 Not Found", allow_origin.clone(), "not found"),
        }
    }
}

/// `/record/<id>/<action>` as its id and action, when the id is one a recording can have.
fn record_route(path: &str) -> Option<(&str, &str)> {
    path.strip_prefix("/record/")?.split_once('/').filter(|(id, _)| recording::valid_id(id))
}

/// The download `body` asks for: JSON with an http(s) `url`, and a `playlist`, if any, that is
/// one; else why it is refused.
fn parse_add(body: &[u8]) -> Result<RemoteAddPayload, String> {
    let payload: RemoteAddPayload = serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {}", e))?;
    if payload.playlist_text().is_some_and(|text| text.len() > MAX_PLAYLIST || !text.starts_with("#EXTM3U")) {
        return Err("playlist must be an HLS playlist (#EXTM3U) of at most 1 MiB".to_string());
    }
    match Url::parse(payload.url.trim()) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(payload),
        _ => Err("url must be an http or https link".to_string()),
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    /// What the response's Access-Control-Allow-Origin says (see [`admit`]).
    allow_origin: Option<String>,
    content_length: Option<usize>,
    body: Vec<u8>,
}

/// Reads one request: its head (at most [`MAX_HEAD`] bytes), then, if [`admit`] lets it in, its
/// body of Content-Length bytes. Otherwise the response to send at once.
async fn read_request(stream: &mut (impl AsyncRead + Unpin)) -> Result<Request, Response> {
    let bad = |allow_origin, why: &str| Response::error("400 Bad Request", allow_origin, why);
    let mut buf = Vec::new();
    let head_len = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        // The head and its blank line end within these bytes, or the head is too long.
        let room = MAX_HEAD + 4 - buf.len();
        if room == 0 {
            return Err(Response::error("431 Request Header Fields Too Large", None, "the request head is too long"));
        }
        let mut chunk = [0; 8192];
        match stream.read(&mut chunk[..room.min(8192)]).await {
            Ok(0) | Err(_) => return Err(bad(None, "incomplete request")),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_len]);
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target)) = (request_line.next(), request_line.next()) else {
        return Err(bad(None, "bad request line"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (mut origin, mut content_length) = (None, None);
    for (name, value) in lines.filter_map(|line| line.split_once(':')) {
        if name.trim().eq_ignore_ascii_case("origin") {
            origin = Some(value.trim());
        } else if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = Some(value.trim().parse::<usize>().map_err(|_| bad(None, "bad Content-Length"))?);
        }
    }
    let (allow_origin, max_body) = admit(method, path, origin)?;
    let length = content_length.unwrap_or(0);
    if length > max_body {
        return Err(Response::error("413 Payload Too Large", allow_origin, "the request body is too large"));
    }
    let mut body = buf[head_len + 4..].to_vec();
    let have = body.len().min(length);
    body.resize(length, 0);
    if stream.read_exact(&mut body[have..]).await.is_err() {
        return Err(bad(allow_origin, "incomplete body"));
    }
    Ok(Request { method: method.to_string(), path: path.to_string(), query: query.to_string(), allow_origin, content_length, body })
}

/// Whether a request from `origin` may reach `path`: then the Access-Control-Allow-Origin of its
/// response and the largest body it may send; else the response to send at once (a preflight's
/// answer is one). Only the extension and local programs, which send no `Origin`, reach any
/// route: a web page could otherwise queue downloads on the user's machine.
fn admit(method: &str, path: &str, origin: Option<&str>) -> Result<(Option<String>, usize), Response> {
    let trusted = origin.is_none_or(|origin| EXTENSION_ORIGINS.iter().any(|scheme| origin.starts_with(scheme)));
    if method == "OPTIONS" {
        // A web page gets no CORS headers, so its browser blocks the request.
        let allow_origin = trusted.then(|| origin.unwrap_or("*").to_string());
        return Err(Response { status: "204 No Content", allow_origin, body: None });
    }
    if !trusted {
        return Err(Response::error("403 Forbidden", None, "forbidden origin"));
    }
    let max_body = match path {
        "/add" => MAX_ADD_BODY,
        _ if record_route(path).is_some_and(|(_, action)| action == "chunk") => MAX_CHUNK_BODY,
        _ => MAX_BODY,
    };
    Ok((origin.map(str::to_string), max_body))
}

/// A response, sent with `Connection: close`.
#[derive(Debug, PartialEq)]
struct Response {
    status: &'static str,
    /// Access-Control-Allow-Origin: which page may read it (see [`admit`]).
    allow_origin: Option<String>,
    /// The JSON body; None for the answer to a preflight.
    body: Option<String>,
}

impl Response {
    fn json(status: &'static str, allow_origin: Option<String>, body: serde_json::Value) -> Self {
        Self { status, allow_origin, body: Some(body.to_string()) }
    }

    fn error(status: &'static str, allow_origin: Option<String>, why: &str) -> Self {
        Self::json(status, allow_origin, json!({"error": why}))
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {}\r\n", self.status);
        if let Some(origin) = &self.allow_origin {
            head.push_str(&format!("Access-Control-Allow-Origin: {}\r\n", origin));
            if self.body.is_none() {
                head.push_str(
                    "Access-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\n\
                     Access-Control-Allow-Private-Network: true\r\nAccess-Control-Max-Age: 600\r\n",
                );
            }
        }
        if let Some(body) = &self.body {
            head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
        }
        head.push_str("Connection: close\r\n\r\n");
        head.push_str(self.body.as_deref().unwrap_or_default());
        head.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn server(root: &Path) -> (Server, mpsc::Receiver<AppEvent>) {
        let (events, received) = mpsc::channel();
        let recordings = Recordings::new(root.to_path_buf());
        let server = Server { port: 49152, events, ctx: egui::Context::default(), recordings, app_dir: Some(root.to_path_buf()) };
        (server, received)
    }

    /// Sends `request` to `server` and returns its whole response.
    async fn exchange(server: &Server, request: &[u8]) -> String {
        let (mut client, side) = tokio::io::duplex(1 << 20);
        let ((), response) = tokio::join!(server.serve(side), async {
            client.write_all(request).await.unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).await.unwrap();
            response
        });
        response
    }

    fn request(method: &str, path: &str, origin: Option<&str>, body: Option<&[u8]>) -> Vec<u8> {
        let mut head = format!("{} {} HTTP/1.1\r\nHost: 127.0.0.1:49152\r\n", method, path);
        if let Some(origin) = origin {
            head.push_str(&format!("Origin: {}\r\n", origin));
        }
        if let Some(body) = body {
            head.push_str(&format!("Content-Type: application/octet-stream\r\nContent-Length: {}\r\n", body.len()));
        }
        let mut request = format!("{}\r\n", head).into_bytes();
        request.extend_from_slice(body.unwrap_or_default());
        request
    }

    /// The status line and the JSON body of `response`.
    fn parts(response: &str) -> (&str, serde_json::Value) {
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        (head.lines().next().unwrap(), serde_json::from_str(body).unwrap_or_default())
    }

    #[test]
    fn web_pages_reach_nothing() {
        assert_eq!(admit("POST", "/add", None), Ok((None, MAX_ADD_BODY)));
        assert_eq!(admit("GET", "/ping", None), Ok((None, MAX_BODY)));
        let extension = "chrome-extension://abcdefghijklmnop";
        assert_eq!(admit("GET", "/ping", Some(extension)), Ok((Some(extension.to_string()), MAX_BODY)));
        assert!(admit("POST", "/record/start", Some("moz-extension://1234")).is_ok());
        assert_eq!(admit("POST", "/record/a1/chunk", Some("safari-web-extension://x")).map(|(_, max)| max), Ok(MAX_CHUNK_BODY));
        let forbidden = Err(Response::error("403 Forbidden", None, "forbidden origin"));
        for (method, path, origin) in [
            ("POST", "/add", "https://page.example"),
            ("GET", "/ping", "https://page.example"),
            ("POST", "/record/start", "null"),
            ("POST", "/record/a1/chunk", "http://127.0.0.1:8000"),
            ("POST", "/record/a1/finish", "https://chrome-extension.example"),
            ("GET", "/elsewhere", "https://page.example"),
        ] {
            assert_eq!(admit(method, path, Some(origin)), forbidden, "{method} {path} from {origin}");
        }
    }

    #[tokio::test]
    async fn preflights_are_answered_for_whom_may_call() {
        let root = tempfile::tempdir().unwrap();
        let (server, _) = server(root.path());
        let response = exchange(&server, &request("OPTIONS", "/add", Some("chrome-extension://abc"), None)).await;
        assert!(response.starts_with("HTTP/1.1 204 No Content\r\n"), "{response}");
        for header in [
            "Access-Control-Allow-Origin: chrome-extension://abc\r\n",
            "Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n",
            "Access-Control-Allow-Headers: Content-Type\r\n",
            "Access-Control-Allow-Private-Network: true\r\n",
            "Access-Control-Max-Age: 600\r\n",
        ] {
            assert!(response.contains(header), "{header} in {response}");
        }
        assert!(!response.contains("Content-Length"), "{response}");
        let response = exchange(&server, &request("OPTIONS", "/record/start", None, None)).await;
        assert!(response.contains("Access-Control-Allow-Origin: *\r\n"), "{response}");
        for path in ["/record/start", "/add"] {
            let response = exchange(&server, &request("OPTIONS", path, Some("https://page.example"), None)).await;
            assert!(response.starts_with("HTTP/1.1 204") && !response.contains("Access-Control"), "{path}: {response}");
        }
    }

    #[tokio::test]
    async fn ping_names_the_app_and_its_port() {
        let root = tempfile::tempdir().unwrap();
        let (server, _) = server(root.path());
        let response = exchange(&server, &request("GET", "/ping", Some("chrome-extension://abc"), None)).await;
        assert!(response.contains("Content-Type: application/json\r\n") && response.contains("Connection: close\r\n"), "{response}");
        let expected = json!({"app": "endos-unified-downloader", "version": env!("CARGO_PKG_VERSION"), "port": 49152});
        assert_eq!(parts(&response), ("HTTP/1.1 200 OK", expected));

        // With the extension next to the app, its version too, for the extension to reload when older.
        std::fs::create_dir(root.path().join("extension")).unwrap();
        std::fs::write(root.path().join("extension").join("manifest.json"), r#"{"version": "1.4.0"}"#).unwrap();
        let response = exchange(&server, &request("GET", "/ping", Some("chrome-extension://abc"), None)).await;
        let expected = json!({"app": "endos-unified-downloader", "version": env!("CARGO_PKG_VERSION"), "port": 49152, "extension": "1.4.0"});
        assert_eq!(parts(&response), ("HTTP/1.1 200 OK", expected));

        let response = exchange(&server, &request("GET", "/ping", Some("https://page.example"), None)).await;
        assert_eq!(parts(&response), ("HTTP/1.1 403 Forbidden", json!({"error": "forbidden origin"})));
        assert!(!response.contains("Access-Control"), "{response}");
    }

    #[tokio::test]
    async fn add_takes_a_link_with_the_browsers_request() {
        let root = tempfile::tempdir().unwrap();
        let (server, received) = server(root.path());
        let page = Some("chrome-extension://abc");
        let response = exchange(&server, &request("POST", "/add", page, Some(b"{not json"))).await;
        assert_eq!(parts(&response).0, "HTTP/1.1 400 Bad Request");
        assert!(response.contains("Access-Control-Allow-Origin: chrome-extension://abc\r\n"), "the extension may read why: {response}");
        let response = exchange(&server, &request("POST", "/add", Some("https://page.example"), Some(br#"{"url":"https://h.example/a.mp4"}"#))).await;
        assert_eq!(parts(&response).0, "HTTP/1.1 403 Forbidden");
        for body in [&br#"{"url":"ftp://h.example/a.mp4"}"#[..], br#"{"cookies":"a=1"}"#, br#"{"url":"https://h.example/a.mp4","headers":{"X":1}}"#] {
            let response = exchange(&server, &request("POST", "/add", page, Some(body))).await;
            assert_eq!(parts(&response).0, "HTTP/1.1 400 Bad Request", "{}", String::from_utf8_lossy(body));
        }
        let too_big = format!("POST /add HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_ADD_BODY + 1);
        assert_eq!(parts(&exchange(&server, too_big.as_bytes()).await).0, "HTTP/1.1 413 Payload Too Large");
        assert!(received.try_recv().is_err(), "nothing refused is added");

        let body = br#"{"url":"https://cdn.example/v/master.m3u8","referer":"https://page.example/watch","headers":{"Origin":"https://page.example"},"filename":"Talk.mp4"}"#;
        let response = exchange(&server, &request("POST", "/add", page, Some(body))).await;
        assert_eq!(parts(&response), ("HTTP/1.1 200 OK", json!({"status": "queued"})));
        let Ok(AppEvent::RemoteAdd(payload)) = received.try_recv() else { panic!("the download reaches the app") };
        assert_eq!(payload.url, "https://cdn.example/v/master.m3u8");
        assert_eq!((payload.referer(), payload.file_name()), (Some("https://page.example/watch".to_string()), Some("Talk.mp4".to_string())));
        assert_eq!(payload.request_headers().0, [("Origin".to_string(), "https://page.example".to_string())]);
    }

    #[test]
    fn the_browsers_headers_are_filtered_and_its_authorization_kept_apart() {
        let payload: RemoteAddPayload = serde_json::from_value(json!({
            "url": "https://cdn.example/a.mp4",
            "user_agent": "Mozilla/5.0 Test",
            "referer": "https://page.example/\r\nX-Evil: 1",
            "headers": {
                "Origin": "https://page.example",
                "Authorization": "Bearer t0k",
                "sec-ch-ua": "\"Chromium\";v=\"126\"",
                "X-Custom": " kept ",
                "Range": "bytes=0-", "Accept-Encoding": "br", "Cookie": "a=1", "Referer": "https://x/", "Host": "x",
                "Sec-Fetch-Mode": "cors", "Proxy-Authorization": "Basic x", "Content-Type": "text/plain",
                "Connection": "keep-alive", "If-None-Match": "\"e\"", "TE": "trailers",
                "Bad Name": "x", "X-Newline": "a\r\nb", "X-Empty": "  ",
            },
        }))
        .unwrap();
        let (headers, auth) = payload.request_headers();
        let mut names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["Origin", "User-Agent", "X-Custom", "sec-ch-ua"]);
        assert!(headers.contains(&("X-Custom".to_string(), "kept".to_string())));
        assert!(headers.contains(&("User-Agent".to_string(), "Mozilla/5.0 Test".to_string())));
        assert_eq!(auth.as_deref(), Some("Bearer t0k"));
        assert_eq!(payload.referer(), None, "a referer no request can carry is dropped");

        let bare: RemoteAddPayload = serde_json::from_value(json!({"url": "https://cdn.example/a.mp4", "headers": null})).unwrap();
        assert_eq!(bare.request_headers(), (Vec::new(), None));
        assert_eq!((bare.referer(), bare.file_name()), (None, None));
    }

    #[test]
    fn file_names_are_made_safe_for_the_save_folder() {
        let name = |filename: &str| RemoteAddPayload { filename: Some(filename.to_string()), ..Default::default() }.file_name();
        assert_eq!(name("My Video.mp4").as_deref(), Some("My Video.mp4"));
        assert_eq!(name("../../etc/passwd").as_deref(), Some("_.._etc_passwd"));
        assert_eq!(name("a\\b:c*?.mp4").as_deref(), Some("a_b_c__.mp4"));
        assert_eq!(name("CON.mp4").as_deref(), Some("_CON.mp4"));
        assert_eq!(name("tab\there.mp4").as_deref(), Some("tab_here.mp4"));
        for unusable in ["", "  ", "..", "..."] {
            assert_eq!(name(unusable), None, "{unusable:?}");
        }
        assert!(name(&format!("{}.mp4", "x".repeat(400))).is_some_and(|n| n.len() <= 200 && n.ends_with(".mp4")));
    }

    #[test]
    fn cookies_are_saved_for_their_host_alone() {
        let https = Url::parse("https://cdn.example:8443/v/a.m3u8").unwrap();
        assert_eq!(
            cookies_file_text(&https, " a=1; b = two=2 ;junk; =nameless; c=b\tad").as_deref(),
            Some("# Netscape HTTP Cookie File\ncdn.example\tFALSE\t/\tTRUE\t0\ta\t1\ncdn.example\tFALSE\t/\tTRUE\t0\tb\ttwo=2\n")
        );
        let http = Url::parse("http://cdn.example/a.mp4").unwrap();
        assert_eq!(cookies_file_text(&http, "s=x").as_deref(), Some("# Netscape HTTP Cookie File\ncdn.example\tFALSE\t/\tFALSE\t0\ts\tx\n"));
        assert_eq!(cookies_file_text(&http, "junk; ;"), None);
    }

    /// The browser's cookie jar is written whole, each cookie for its domain and subdomains or
    /// its host alone, in place of the Cookie header, which is kept for when it has none usable.
    #[test]
    fn a_cookie_jar_is_saved_in_place_of_the_cookie_header() {
        let payload: RemoteAddPayload = serde_json::from_value(json!({
            "url": "https://cdn.example/v/a.m3u8",
            "cookies": "h=1",
            "cookie_jar": [
                {"domain": ".example.com", "path": "/", "name": "sid", "value": "a b", "secure": true, "http_only": true, "host_only": false, "expires": 0},
                {"domain": "cdn.example", "path": "/v", "name": "tok", "value": "x=y", "secure": false, "http_only": false, "host_only": true, "expires": 4102444800u64},
                {"domain": "keys.example", "path": "", "name": "k", "value": "1"},
                {"domain": "", "name": "nodomain", "value": "1"},
                {"domain": "a.example", "name": "", "value": "1"},
                {"domain": "a.example", "name": "tab", "value": "a\tb"},
                {"domain": "a.example\nevil", "name": "line", "value": "1"},
                {"domain": "a.example", "path": "/x\r", "name": "cr", "value": "1"},
            ],
        }))
        .unwrap();
        let url = Url::parse(&payload.url).unwrap();
        assert_eq!(
            payload.cookies_file(&url).as_deref(),
            Some(
                "# Netscape HTTP Cookie File\n\
                 #HttpOnly_.example.com\tTRUE\t/\tTRUE\t0\tsid\ta b\n\
                 cdn.example\tFALSE\t/v\tFALSE\t4102444800\ttok\tx=y\n\
                 .keys.example\tTRUE\t/\tFALSE\t0\tk\t1\n"
            )
        );
        let unusable = RemoteAddPayload { cookie_jar: payload.cookie_jar[3..].to_vec(), ..payload.clone() };
        assert_eq!(unusable.cookies_file(&url), cookies_file_text(&url, "h=1"), "the Cookie header when the jar has nothing usable");
        let many: Vec<JarCookie> =
            (0..MAX_JAR_COOKIES + 5).map(|n| JarCookie { domain: "a.example".into(), name: format!("c{n}"), ..Default::default() }).collect();
        assert_eq!(jar_file_text(&many).unwrap().lines().count(), 1 + MAX_JAR_COOKIES);
        assert_eq!(RemoteAddPayload::default().cookies_file(&url), None);
    }

    #[test]
    fn a_playlist_the_page_built_must_be_one() {
        let add = |playlist: serde_json::Value| parse_add(json!({"url": "https://page.example/watch", "playlist": playlist}).to_string().as_bytes());
        let payload = add(json!("\u{feff} #EXTM3U\n#EXTINF:4,\nseg0.ts\n ")).unwrap();
        assert_eq!(payload.playlist_text(), Some("#EXTM3U\n#EXTINF:4,\nseg0.ts"));
        for refused in [json!("<html>"), json!(""), json!(format!("#EXTM3U\n{}", "#".repeat(MAX_PLAYLIST)))] {
            assert_eq!(add(refused).unwrap_err(), "playlist must be an HLS playlist (#EXTM3U) of at most 1 MiB");
        }
        let payload = parse_add(br#"{"url":"https://cdn.example/a.mpd","dash":true,"height":720}"#).unwrap();
        assert!(payload.dash && payload.height == Some(720) && payload.playlist.is_none() && payload.cookie_jar.is_empty());
    }

    /// A recording is started, fed and finished by the extension alone, with its id and track
    /// checked on every route.
    #[tokio::test]
    async fn recordings_are_fed_by_the_extension() {
        let root = tempfile::tempdir().unwrap();
        let (server, received) = server(root.path());
        let ext = Some("chrome-extension://abc");
        let response = exchange(&server, &request("POST", "/record/start", ext, Some(br#"{"title":"Talk","page_url":"https://page.example/"}"#))).await;
        let (status, reply) = parts(&response);
        assert_eq!(status, "HTTP/1.1 200 OK");
        let id = reply["id"].as_str().unwrap().to_string();
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(root.path().join(&id).join("meta.json")).unwrap()).unwrap();
        assert_eq!((meta["title"].as_str(), meta["page_url"].as_str()), (Some("Talk"), Some("https://page.example/")));
        assert!(response.contains("Access-Control-Allow-Origin: chrome-extension://abc\r\n"), "{response}");

        let chunk = |id: &str, query: &str, origin: Option<&'static str>, body: Option<Vec<u8>>| {
            let path = format!("/record/{}/chunk{}", id, query);
            let server = &server;
            async move { exchange(server, &request("POST", &path, origin, body.as_deref())).await }
        };
        let response = chunk(&id, "?track=0&mime=video%2Fwebm%3B%20codecs%3Dvp9", ext, Some(vec![1; 40 * 1024])).await;
        assert_eq!(parts(&response), ("HTTP/1.1 200 OK", json!({"ok": true, "bytes": 40 * 1024})));
        let response = chunk(&id, "?track=0&part=0&mime=video%2Fmp4", ext, Some(vec![2; 40 * 1024])).await;
        assert_eq!(parts(&response).1, json!({"ok": true, "bytes": 80 * 1024}));
        let response = chunk(&id, "?track=0&part=255&mime=video%2Fmp4", ext, Some(vec![3; 10])).await;
        assert_eq!(parts(&response).1, json!({"ok": true, "bytes": 80 * 1024 + 10}), "the track's parts together");
        for bad in ["?track=0&part=256", "?track=0&part=x", "?track=0&part=-1", "?track=0&part="] {
            assert_eq!(parts(&chunk(&id, bad, ext, Some(vec![0])).await).0, "HTTP/1.1 400 Bad Request", "{bad}");
        }
        assert_eq!(parts(&chunk(&id, "?track=16&mime=video%2Fwebm", ext, Some(vec![0])).await).0, "HTTP/1.1 400 Bad Request");
        assert_eq!(parts(&chunk(&id, "?mime=video%2Fwebm", ext, Some(vec![0])).await).0, "HTTP/1.1 400 Bad Request");
        assert_eq!(parts(&chunk(&id, "?track=0&mime=video%2Fwebm", ext, None).await).0, "HTTP/1.1 400 Bad Request", "no Content-Length");
        assert_eq!(parts(&chunk("nope", "?track=0", ext, Some(vec![0])).await).0, "HTTP/1.1 404 Not Found");
        assert_eq!(parts(&chunk("..", "?track=0", ext, Some(vec![0])).await).0, "HTTP/1.1 404 Not Found");
        assert_eq!(parts(&chunk(&id, "?track=0", Some("https://page.example"), Some(vec![0])).await).0, "HTTP/1.1 403 Forbidden");
        assert!(received.try_recv().is_err());

        let response = exchange(&server, &request("POST", &format!("/record/{}/finish", id), ext, Some(b""))).await;
        assert_eq!(parts(&response), ("HTTP/1.1 200 OK", json!({"status": "merging"})));
        let Ok(AppEvent::Recorded(finished)) = received.try_recv() else { panic!("the recording reaches the app") };
        let dir = root.path().join(&id);
        assert_eq!(finished.title, "Talk");
        assert_eq!(finished.tracks, [(0, vec![dir.join("track0.part0.webm"), dir.join("track0.part255.mp4")])]);
        assert_eq!(std::fs::read(&finished.tracks[0].1[0]).unwrap().len(), 80 * 1024);
        let response = exchange(&server, &request("POST", &format!("/record/{}/abort", id), ext, None)).await;
        assert_eq!(parts(&response).0, "HTTP/1.1 404 Not Found", "finished already");

        let response = exchange(&server, &request("POST", "/record/start", None, Some(b"{}"))).await;
        let id = parts(&response).1["id"].as_str().unwrap().to_string();
        let response = exchange(&server, &request("POST", &format!("/record/{}/abort", id), None, None)).await;
        assert_eq!(parts(&response), ("HTTP/1.1 200 OK", json!({"status": "aborted"})));
        assert!(!root.path().join(&id).exists());
    }

    #[tokio::test]
    async fn a_request_head_too_long_or_broken_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let (server, _) = server(root.path());
        let long = format!("GET /ping HTTP/1.1\r\nX-Pad: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert_eq!(parts(&exchange(&server, long.as_bytes()).await).0, "HTTP/1.1 431 Request Header Fields Too Large");
        let bad_length = b"POST /add HTTP/1.1\r\nContent-Length: lots\r\n\r\n";
        assert_eq!(parts(&exchange(&server, bad_length).await).0, "HTTP/1.1 400 Bad Request");
        assert_eq!(parts(&exchange(&server, b"GET /nowhere HTTP/1.1\r\n\r\n").await).0, "HTTP/1.1 404 Not Found");
    }
}
