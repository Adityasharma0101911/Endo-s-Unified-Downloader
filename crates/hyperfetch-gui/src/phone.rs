//! Links sent from the user's phone, while "Send links from your phone" is on: a small page on
//! the local network (`GET /phone`, opened from a QR code) and the route it posts to (`POST /add`,
//! `{"url": "…"}` with `Authorization: Bearer <code>`), which a phone app can use too. A link that
//! arrives goes the way the browser extension's do (`AppEvent::RemoteAdd`).
//!
//! Only peers on a local network are answered, anyone else is closed at once; only a request with
//! the code adds anything. The code travels in the page's address after `#`, which a browser never
//! sends, so it reaches no log or Referer. The extension's server (see `ipc`) stays on 127.0.0.1.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::{JoinHandle, JoinSet};

use crate::ipc::RemoteAddPayload;
use crate::util::lock;
use crate::AppEvent;

/// Ports tried in turn; the QR code holds the one taken.
pub const PORTS: std::ops::RangeInclusive<u16> = 49160..=49169;
/// Where the server listens: every IPv4 network. Tests listen on loopback, which opens no port on
/// the network (and asks no firewall); [`local_peer`] still turns this computer away there.
const HOST: Ipv4Addr = if cfg!(test) { Ipv4Addr::LOCALHOST } else { Ipv4Addr::UNSPECIFIED };
/// Largest request line and headers, and body.
const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 16 * 1024;
/// Connections served at once; one more is closed at once.
const MAX_CONNECTIONS: usize = 8;
/// How long a connection may take to send its request and take the answer.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The page the QR code opens: one file, nothing loaded from elsewhere.
const PAGE: &str = include_str!("phone.html");
/// What the page may do: its own style and script, and post to this server; nothing else.
const PAGE_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; \
                           form-action 'none'; base-uri 'none'; frame-ancestors 'none'";
const WRONG_CODE: &str = "Wrong or missing code: scan the QR code in the app again.";

/// Whether `peer` is on a local network: private IPv4 (10/8, 172.16/12, 192.168/16), link-local
/// (169.254/16, fe80::/10) or an IPv6 unique local address (fc00::/7). Not loopback: this
/// computer's programs have the extension's server.
pub fn local_peer(peer: IpAddr) -> bool {
    match peer.to_canonical() {
        IpAddr::V4(a) => a.is_private() || a.is_link_local(),
        IpAddr::V6(a) => a.segments()[0] & 0xfe00 == 0xfc00 || a.segments()[0] & 0xffc0 == 0xfe80,
    }
}

/// This computer's addresses on its local networks, with their network's name: IPv4, as the
/// server listens.
pub fn addresses() -> Vec<(String, IpAddr)> {
    let local = hyperfetch_core::netif::usable().into_iter().filter(|n| n.address.is_ipv4() && local_peer(n.address));
    local.map(|n| (n.name, n.address)).collect()
}

/// The link the QR code holds: the page at `address`, the code after `#`.
pub fn page_link(address: IpAddr, port: u16, token: &Token) -> String {
    format!("http://{}/phone#t={}", SocketAddr::new(address, port), token.0)
}

/// The code a link needs: 128 random bits from the OS, as 32 hex digits, saved in the settings.
/// Its Debug never shows it, so no log line or error can.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(hidden)")
    }
}

impl Token {
    pub fn new() -> io::Result<Self> {
        let mut bits = [0u8; 16];
        getrandom::getrandom(&mut bits).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Self(bits.iter().map(|b| format!("{b:02x}")).collect()))
    }

    /// One [`Token::new`] made; not none, nor one edited in the settings file.
    pub fn usable(&self) -> bool {
        self.0.len() == 32 && self.0.bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// Whether `sent` is this code, compared in a time that does not tell where they differ.
    pub fn matches(&self, sent: &str) -> bool {
        let (ours, sent) = (self.0.as_bytes(), sent.as_bytes());
        // Every code has 32 digits: the length tells nothing.
        let differ = ours.iter().zip(sent).fold(0, |differ, (a, b)| differ | (a ^ b));
        self.usable() && ours.len() == sent.len() && std::hint::black_box(differ) == 0
    }
}

/// The link sent, when the app downloads its kind: http, https or ftp, or a magnet, with no
/// blank or control character in it. Else why not.
pub fn check_link(text: &str) -> Result<String, String> {
    let text = text.trim();
    let refused = || Err("Send an http, https, ftp or magnet link.".to_string());
    if text.len() > MAX_BODY || text.contains(|c: char| c.is_whitespace() || c.is_control()) {
        return refused();
    }
    match url::Url::parse(text) {
        Ok(url) if matches!(url.scheme(), "http" | "https" | "ftp") => Ok(text.to_string()),
        Ok(_) if hyperfetch_core::torrent::is_magnet_uri(text) => Ok(text.to_string()),
        _ => refused(),
    }
}

/// What the server and the window share: the code it takes, how many links came, and the way to
/// the window.
pub struct Shared {
    token: Mutex<Token>,
    received: AtomicUsize,
    events: mpsc::Sender<AppEvent>,
    ctx: egui::Context,
}

impl Shared {
    pub fn new(token: Token, events: mpsc::Sender<AppEvent>, ctx: egui::Context) -> Self {
        Self { token: Mutex::new(token), received: AtomicUsize::new(0), events, ctx }
    }

    /// A new code: the old one is refused from the next request on.
    pub fn set_token(&self, token: Token) {
        *lock(&self.token) = token;
    }

    /// The links added since the app started.
    pub fn received(&self) -> usize {
        self.received.load(Ordering::Relaxed)
    }

    /// The answer to the one request `stream` sends.
    async fn answer(&self, stream: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
        let (head, mut body) = match read_head(stream).await {
            Ok(read) => read,
            Err(why) => return error("400 Bad Request", why),
        };
        match (head.method.as_str(), head.path.as_str()) {
            ("GET", "/phone") => response("200 OK", "text/html; charset=utf-8", &format!("Content-Security-Policy: {PAGE_POLICY}\r\n"), PAGE),
            ("POST", "/add") => {
                let length = match head.length {
                    None => return error("411 Length Required", "Send the body with its Content-Length."),
                    Some(length) if length > MAX_BODY => return error("413 Payload Too Large", "Send at most 16 KB."),
                    Some(length) => length,
                };
                // Read whole before the answer, so that closing never cuts the answer off.
                let have = body.len().min(length);
                body.resize(length, 0);
                if stream.read_exact(&mut body[have..]).await.is_err() {
                    return error("400 Bad Request", "The request ended early.");
                }
                if !head.code.as_deref().is_some_and(|code| lock(&self.token).matches(code)) {
                    return error("403 Forbidden", WRONG_CODE);
                }
                #[derive(serde::Deserialize)]
                struct Sent {
                    url: String,
                }
                let sent = serde_json::from_slice::<Sent>(&body).map_err(|_| r#"Send JSON like {"url": "https://…"}."#.to_string());
                match sent.and_then(|sent| check_link(&sent.url)) {
                    Ok(url) => {
                        self.received.fetch_add(1, Ordering::Relaxed);
                        // The extension's way in: nothing but the link, no cookies or headers.
                        if self.events.send(AppEvent::RemoteAdd(RemoteAddPayload { url, ..Default::default() })).is_ok() {
                            self.ctx.request_repaint();
                        }
                        response("200 OK", "application/json", "", &json!({"status": "queued"}).to_string())
                    }
                    Err(why) => error("400 Bad Request", &why),
                }
            }
            (_, "/phone") => response("405 Method Not Allowed", "application/json", "Allow: GET\r\n", r#"{"error":"GET only"}"#),
            (_, "/add") => response("405 Method Not Allowed", "application/json", "Allow: POST\r\n", r#"{"error":"POST only"}"#),
            _ => error("404 Not Found", "Not found."),
        }
    }
}

/// The server while it runs; dropping it stops it, and the connections it holds.
pub struct Server {
    pub port: u16,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the server on the first free port of [`PORTS`], on every network, answering peers on a
/// local network alone (see [`local_peer`]).
pub fn start(rt: &tokio::runtime::Handle, shared: Arc<Shared>) -> io::Result<Server> {
    let mut failed = io::Error::new(io::ErrorKind::AddrInUse, "ports 49160 to 49169 are taken");
    for port in PORTS {
        match std::net::TcpListener::bind((HOST, port)) {
            Ok(listener) => {
                listener.set_nonblocking(true)?;
                let _runtime = rt.enter();
                let listener = TcpListener::from_std(listener)?;
                tracing::info!("Listening for links from the phone on port {}", port);
                return Ok(Server { port, task: rt.spawn(listen(listener, shared, local_peer)) });
            }
            Err(e) => failed = e,
        }
    }
    Err(failed)
}

/// Serves the connections of peers `admit` lets in, [`MAX_CONNECTIONS`] at once; anyone else is
/// closed at once, unanswered.
async fn listen(listener: TcpListener, shared: Arc<Shared>, admit: fn(IpAddr) -> bool) {
    // Dropped with this task, which ends every connection.
    let mut open = JoinSet::new();
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            // The pause keeps a lasting failure (no sockets left) from spinning.
            Err(e) => {
                tracing::debug!("Phone link server: accept failed: {}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        while open.try_join_next().is_some() {}
        if admit(peer.ip()) && open.len() < MAX_CONNECTIONS {
            open.spawn(serve(socket, Arc::clone(&shared)));
        }
    }
}

/// Answers the one request `stream` sends, then closes it; all within [`TIMEOUT`].
async fn serve(mut stream: impl AsyncRead + AsyncWrite + Unpin, shared: Arc<Shared>) {
    let _ = tokio::time::timeout(TIMEOUT, async {
        let answer = shared.answer(&mut stream).await;
        let _ = stream.write_all(&answer).await;
        let _ = stream.shutdown().await;
    })
    .await;
}

/// A request's line and the headers that matter here.
struct Head {
    method: String,
    path: String,
    /// The code of its `Authorization: Bearer` header.
    code: Option<String>,
    length: Option<usize>,
}

/// Reads a request's head (at most [`MAX_HEAD`] bytes): it, and what came of the body with it.
async fn read_head(stream: &mut (impl AsyncRead + Unpin)) -> Result<(Head, Vec<u8>), &'static str> {
    let mut buf = Vec::new();
    let end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        let room = (MAX_HEAD + 4).saturating_sub(buf.len());
        if room == 0 {
            return Err("The request head is too long.");
        }
        let mut chunk = [0; 4096];
        match stream.read(&mut chunk[..room.min(4096)]).await {
            Ok(0) | Err(_) => return Err("Incomplete request."),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let text = String::from_utf8_lossy(&buf[..end]);
    let mut lines = text.split("\r\n");
    let mut line = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target)) = (line.next(), line.next()) else { return Err("Bad request line.") };
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    let mut head = Head { method: method.to_string(), path: path.to_string(), code: None, length: None };
    for (name, value) in lines.filter_map(|line| line.split_once(':')) {
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("content-length") {
            head.length = Some(value.parse().map_err(|_| "Bad Content-Length.")?);
        } else if name.eq_ignore_ascii_case("authorization") {
            let bearer = value.get(..7).is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer "));
            head.code = bearer.then(|| value[7..].trim().to_string());
        }
    }
    Ok((head, buf[end + 4..].to_vec()))
}

/// A response, sent with `Connection: close`: `extra` holds headers of its own, each ending in
/// CRLF. Nothing in it may be cached or sent on as a Referer; no CORS header lets another site
/// read it.
fn response(status: &str, content_type: &str, extra: &str, body: &str) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    );
    [head.as_bytes(), body.as_bytes()].concat()
}

/// A JSON error, `{"error": why}`: never the code sent, nor the one taken.
fn error(status: &str, why: &str) -> Vec<u8> {
    response(status, "application/json", "", &json!({ "error": why }).to_string())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::time::Instant;

    use super::*;

    #[test]
    fn only_local_networks_are_answered() {
        for peer in [
            "10.0.0.1",
            "10.255.255.254",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.0.1",
            "192.168.1.20",
            "169.254.10.20",
            "fc00::1",
            "fd12:3456:789a::1",
            "fe80::1c2b:3cff:fe4d:5e6f",
            "::ffff:192.168.1.5",
        ] {
            assert!(local_peer(peer.parse().unwrap()), "{peer}");
        }
        for peer in [
            "127.0.0.1",
            "::1",
            "0.0.0.0",
            "::",
            "8.8.8.8",
            "1.1.1.1",
            "11.0.0.1",
            "172.15.255.254",
            "172.32.0.1",
            "192.169.0.1",
            "100.64.0.1",
            "224.0.0.251",
            "255.255.255.255",
            "2001:4860:4860::8888",
            "fec0::1",
            "ff02::1",
            "::ffff:8.8.8.8",
            "::ffff:127.0.0.1",
        ] {
            assert!(!local_peer(peer.parse().unwrap()), "{peer}");
        }
    }

    #[test]
    fn the_code_matches_itself_alone() {
        let token = Token::new().unwrap();
        assert!(token.usable() && token.0.len() == 32, "128 bits as hex");
        assert_ne!(Token::new().unwrap(), token, "random");
        assert!(token.matches(&token.0));
        let mut wrong = token.0.clone().into_bytes();
        wrong[31] = if wrong[31] == b'0' { b'1' } else { b'0' };
        // Wrong, missing, shorter and longer.
        for sent in [String::from_utf8(wrong).unwrap(), String::new(), token.0[..31].to_string(), format!("{}0", token.0)] {
            assert!(!token.matches(&sent), "{sent}");
        }
        // No code yet, or one edited by hand, takes nothing.
        assert!(!Token::default().matches("") && !Token::default().usable());
        assert!(!Token("abc".into()).matches("abc"));
    }

    #[test]
    fn only_links_the_app_downloads_are_taken() {
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=ubuntu.iso";
        for link in ["https://h.example/a.iso", " http://192.168.1.9:8080/b.zip\n", "ftp://ftp.example/pub/c.tar", "HTTPS://H.EXAMPLE/D", magnet] {
            assert_eq!(check_link(link), Ok(link.trim().to_string()));
        }
        let long = format!("https://h.example/{}", "a".repeat(MAX_BODY));
        for link in ["", "not a link", "file:///C:/Windows/win.ini", "javascript:alert(1)", "data:text/html,x", "magnet:", "https://h.example/a b", "https://h.example/a\nhttps://b.example", long.as_str()] {
            assert!(check_link(link).is_err(), "{link}");
        }
    }

    /// The code shows in no Debug output (the settings' included, which a log or a failed test
    /// could print) and in no answer.
    #[test]
    fn the_code_is_never_shown() {
        let token = Token::new().unwrap();
        let settings = crate::settings::Settings { phone_token: token.clone(), ..Default::default() };
        for shown in [format!("{token:?}"), format!("{settings:?}"), format!("{settings:#?}")] {
            assert!(!shown.contains(&token.0), "{shown}");
        }
        assert!(serde_json::to_string(&settings).unwrap().contains(&token.0), "but it is saved");
        for answer in [error("403 Forbidden", WRONG_CODE), response("200 OK", "text/html", "", PAGE)] {
            assert!(!String::from_utf8(answer).unwrap().contains(&token.0));
        }
    }

    fn shared() -> (Arc<Shared>, mpsc::Receiver<AppEvent>, Token) {
        let (events, received) = mpsc::channel();
        let token = Token::new().unwrap();
        (Arc::new(Shared::new(token.clone(), events, egui::Context::default())), received, token)
    }

    /// Sends `request` to 127.0.0.1:`port`: the whole answer, read within 5 s ("" when the
    /// server closed the connection unanswered).
    fn ask(port: u16, request: &[u8]) -> String {
        let to = SocketAddr::from(([127, 0, 0, 1], port));
        let mut stream = std::net::TcpStream::connect_timeout(&to, Duration::from_secs(5)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = stream.write_all(request);
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// `POST /add` with `body`, and the code when given.
    fn post(code: Option<&str>, body: &str) -> Vec<u8> {
        let auth = code.map(|code| format!("Authorization: Bearer {code}\r\n")).unwrap_or_default();
        let head = format!("POST /add HTTP/1.1\r\nHost: 192.168.1.20:49160\r\nContent-Type: application/json\r\n{auth}");
        format!("{head}Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    /// On 127.0.0.1, standing in for the phone: the page comes to anyone; a link is taken only with
    /// the code, and reaches the app as the extension's do; a body too big, another method, a
    /// preflight from a site are refused; after New code the old code is; and once stopped, the
    /// port is closed.
    #[test]
    fn the_page_sends_links_with_the_code_alone() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (shared, received, token) = shared();
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = Server { port, task: rt.spawn(listen(listener, Arc::clone(&shared), |peer: IpAddr| peer.is_loopback())) };

        let page = ask(port, b"GET /phone?url=https%3A%2F%2Fh.example%2Fa.iso HTTP/1.1\r\nHost: 192.168.1.20\r\n\r\n");
        assert!(page.starts_with("HTTP/1.1 200 OK\r\n") && page.contains("Content-Type: text/html; charset=utf-8\r\n"), "{page}");
        assert!(page.contains("<title>Endo's Unified Downloader</title>") && page.contains("location.hash") && page.contains("prefers-color-scheme: dark"));
        assert!(page.contains("Content-Security-Policy: default-src 'none';") && page.contains("Referrer-Policy: no-referrer\r\n"));
        assert!(!page.contains("Access-Control") && !page.contains(&token.0) && !page.contains(" src=") && !page.contains("href="), "{page}");

        let link = r#"{"url":"https://h.example/a.iso"}"#;
        let wrong = Token::new().unwrap().0;
        let headers = |code: &str| format!("POST /add HTTP/1.1\r\nauthorization: {code}\r\nContent-Length: {}\r\n\r\n{link}", link.len());
        let mut refused = vec![post(None, link), post(Some(wrong.as_str()), link), post(Some(""), link), post(Some(&token.0[..16]), link)];
        refused.push(headers(&token.0).into_bytes());
        refused.push(headers(&format!("Basic {}", token.0)).into_bytes());
        for request in refused {
            let answer = ask(port, &request);
            assert!(answer.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{answer}");
            assert!(!answer.contains(&token.0) && !answer.contains(&wrong) && !answer.contains("Access-Control"), "{answer}");
        }
        assert!(received.try_recv().is_err(), "nothing added without the code");

        let answer = ask(port, &post(Some(token.0.as_str()), link));
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n") && answer.ends_with(r#"{"status":"queued"}"#), "{answer}");
        let Ok(AppEvent::RemoteAdd(payload)) = received.recv_timeout(Duration::from_secs(5)) else { panic!("the link reaches the app") };
        assert_eq!((payload.url.as_str(), payload.cookies, payload.headers), ("https://h.example/a.iso", None, None));
        // The scheme is any case, as browsers send it.
        let answer = ask(port, headers(&format!("bearer {}", token.0)).as_bytes());
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(matches!(received.recv_timeout(Duration::from_secs(5)), Ok(AppEvent::RemoteAdd(_))));
        assert_eq!(shared.received(), 2);

        for body in [r#"{"url":"file:///etc/passwd"}"#, "{not json", r#"{"link":"https://h.example/a.iso"}"#] {
            assert!(ask(port, &post(Some(token.0.as_str()), body)).starts_with("HTTP/1.1 400 Bad Request\r\n"), "{body}");
        }
        // Said, never sent: the body would be bigger than the server reads.
        let too_big = format!("POST /add HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n", token.0, MAX_BODY + 1);
        assert!(ask(port, too_big.as_bytes()).starts_with("HTTP/1.1 413 Payload Too Large\r\n"));
        let preflight = ask(port, b"OPTIONS /add HTTP/1.1\r\nOrigin: https://site.example\r\nAccess-Control-Request-Method: POST\r\n\r\n");
        assert!(preflight.starts_with("HTTP/1.1 405 Method Not Allowed\r\n") && !preflight.contains("Access-Control"), "{preflight}");
        assert!(ask(port, b"GET /add HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 405"));
        assert!(ask(port, b"GET / HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 404"));
        assert!(received.try_recv().is_err(), "nothing refused is added");

        // New code: the old one is refused at once.
        let new = Token::new().unwrap();
        shared.set_token(new.clone());
        assert!(ask(port, &post(Some(token.0.as_str()), link)).starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(ask(port, &post(Some(new.0.as_str()), link)).starts_with("HTTP/1.1 200 OK\r\n"));

        drop(server);
        let start = Instant::now();
        while std::net::TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(200)).is_ok() {
            assert!(start.elapsed() < Duration::from_secs(5), "still listening");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A handful of connections at once: one more is closed unanswered, until one of them ends.
    #[test]
    fn a_handful_of_connections_at_once() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (shared, _received, _) = shared();
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = Server { port, task: rt.spawn(listen(listener, shared, |peer: IpAddr| peer.is_loopback())) };
        let to = SocketAddr::from(([127, 0, 0, 1], port));
        let idle: Vec<_> = (0..MAX_CONNECTIONS).map(|_| std::net::TcpStream::connect_timeout(&to, Duration::from_secs(5)).unwrap()).collect();
        // Each is taken before the next, so all of them hold a place by now.
        assert_eq!(ask(port, b"GET /phone HTTP/1.1\r\n\r\n"), "");
        drop(idle);
        let start = Instant::now();
        while !ask(port, b"GET /phone HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200 OK") {
            assert!(start.elapsed() < Duration::from_secs(5), "a place is free again");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The server as the app starts it answers no one off a local network: this computer
    /// (loopback, where it listens in tests) neither.
    #[test]
    fn the_app_s_server_turns_this_computer_away() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (shared, received, token) = shared();
        let server = start(rt.handle(), shared).unwrap();
        assert!(PORTS.contains(&server.port));
        assert_eq!(ask(server.port, &post(Some(token.0.as_str()), r#"{"url":"https://h.example/a.iso"}"#)), "");
        assert!(received.try_recv().is_err());
    }
}
