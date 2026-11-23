//! A working copy of a web page or a site: recorded with the browser on this computer (Edge or
//! Chrome, without a window, driven through its DevTools port) and played back in a browser
//! window (the CLI's `--working-copy` and `--open-copy`, the GUI's Save a working copy).
//!
//! Recording loads each page as a visitor's browser does and keeps every answer it gets (the
//! page, its style sheets, scripts, fonts and pictures, the data its scripts ask for, what its
//! frames and workers ask for) in one WARC file (see `crate::warc`), each with its request. On
//! each page it does what a visitor would before it moves on: scrolls to the end until the page
//! stops growing, asks for the pictures a page shows only once seen or on another screen
//! (`loading=lazy`, `srcset`, `data-src`), starts its video and audio muted, and waits for the
//! network to fall quiet. A site is followed from the links each page shows in the browser,
//! within the start link's folder on its host, the nearest pages first, as far as
//! [`CopyOptions`] says. robots.txt is honoured for a site's pages, with its Crawl-delay, one
//! page at a time. A site stopped early goes on where it stopped when started again: its state
//! is kept beside the file until the site is done.
//!
//! Playback opens a browser window that has no network: every request its pages make is
//! answered from the file (see [`Copy::find`] for which answer a request gets), so a page keeps
//! its own address, and its scripts, style sheets, fonts and animations run as they did. What
//! was never asked for while recording is not there: a page not visited, a search, a form sent,
//! a live connection (a WebSocket), the later parts of a long video.
//!
//! The browser runs on a profile of its own, made for the run and deleted after it: none of the
//! user's sign-ins, history or extensions. A site that asks for a sign-in or a browser check is
//! copied as it answers a stranger.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use crate::sitemirror::{robots_of, robots_path, Robots};
use crate::warc;

/// What a copy's file ends in.
pub const EXTENSION: &str = ".warc.gz";

/// How long the browser has to start, to answer a command, and a page to load.
const START_LIMIT: Duration = Duration::from_secs(30);
const CALL_LIMIT: Duration = Duration::from_secs(60);
const LOAD_LIMIT: Duration = Duration::from_secs(30);

/// The network is quiet once nothing was asked for or answered for this long; a page that never
/// falls quiet (it polls) is left after [`QUIET_LIMIT`].
const QUIET: Duration = Duration::from_millis(1200);
const QUIET_LIMIT: Duration = Duration::from_secs(15);

const GONE: &str = "the browser closed";
const NO_BROWSER: &str = "No Edge or Chrome was found on this computer: a working copy is recorded and played with one of them (ENDO_BROWSER names another Chromium browser's program)";

/// What a visitor does on a page before leaving it (see the module), in at most 25 seconds:
/// pictures that load only once seen are made to load, the page is scrolled a screen at a time
/// until it stops growing, video and audio start muted, and the pictures of other screens and
/// of lazy loaders are asked for.
const VISITOR: &str = r#"(async () => {
  const sleep = ms => new Promise(done => setTimeout(done, ms));
  const until = Date.now() + 20000;
  const wanted = new Set();
  const want = link => {
    if (!link || link.startsWith('data:')) return;
    try { const url = new URL(link, document.baseURI); if (url.protocol === 'http:' || url.protocol === 'https:') wanted.add(url.href); } catch (e) {}
  };
  const first = set => { for (const part of set.split(/,\s+/)) want(part.trim().split(/\s+/)[0]); };
  const look = () => {
    for (const el of document.querySelectorAll('img[loading="lazy"], iframe[loading="lazy"]')) el.loading = 'eager';
    for (const el of document.querySelectorAll('img[srcset], source[srcset]')) first(el.srcset);
    for (const el of document.querySelectorAll('[data-srcset]')) first(el.dataset.srcset);
    for (const el of document.querySelectorAll('[data-src], [data-lazy-src], [data-original]')) want(el.dataset.src || el.dataset.lazySrc || el.dataset.original);
  };
  look();
  let height = 0, still = 0;
  while (Date.now() < until && still < 3) {
    window.scrollBy(0, Math.max(300, Math.round(window.innerHeight * 0.8)));
    await sleep(200);
    look();
    const now = document.documentElement.scrollHeight;
    if (window.scrollY + window.innerHeight >= now - 2) { still = now === height ? still + 1 : 0; height = now; await sleep(350); }
  }
  for (const media of document.querySelectorAll('video, audio')) {
    try { media.muted = true; const played = media.play(); if (played) played.catch(() => {}); } catch (e) {}
  }
  const asked = [...wanted].slice(0, 400).map(link => new Promise(done => {
    const img = new Image(); img.onload = img.onerror = done; img.src = link; setTimeout(done, 5000);
  }));
  await Promise.race([Promise.all(asked), sleep(Math.max(0, until + 5000 - Date.now()))]);
  window.scrollTo(0, 0);
  await sleep(200);
  return true;
})()"#;

/// Where the page is, its title and the links it shows.
const READ: &str = "({url: location.href, title: document.title, links: Array.from(document.querySelectorAll('a[href]'), a => a.href)})";

/// The browser to drive: the program `ENDO_BROWSER` names, else Edge or Chrome (or Chromium)
/// where this system keeps them.
pub fn browser() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("ENDO_BROWSER").map(PathBuf::from).filter(|program| program.is_file()) {
        return Some(named);
    }
    let mut places: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for program in [r"Microsoft\Edge\Application\msedge.exe", r"Google\Chrome\Application\chrome.exe"] {
            for root in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
                places.extend(std::env::var_os(root).map(|root| PathBuf::from(root).join(program)));
            }
        }
    } else if cfg!(target_os = "macos") {
        for app in ["Microsoft Edge", "Google Chrome", "Chromium"] {
            places.push(format!("/Applications/{app}.app/Contents/MacOS/{app}").into());
        }
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for name in ["microsoft-edge", "microsoft-edge-stable", "google-chrome", "google-chrome-stable", "chromium", "chromium-browser"] {
            places.extend(std::env::split_paths(&path).map(|dir| dir.join(name)));
        }
    }
    places.into_iter().find(|place| place.is_file())
}

/// What the browser tells without being asked: of a session (a tab, a frame, a worker), or of
/// the browser itself.
struct Event {
    session: Option<String>,
    method: String,
    params: Value,
}

/// Who waits for which command's answer; None once the browser is gone.
type Waiting = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>>;

/// The browser's DevTools connection: commands go out, each one's answer comes back to its
/// caller, and what the browser tells besides goes to the receiver [`Cdp::connect`] returns.
#[derive(Clone)]
struct Cdp {
    out: mpsc::UnboundedSender<String>,
    waiting: Waiting,
    next: Arc<AtomicU64>,
}

impl Cdp {
    async fn connect(address: &str) -> Result<(Self, mpsc::UnboundedReceiver<Event>), String> {
        // An answer carries a whole file, as base64: no limit of this side's.
        let config = WebSocketConfig::default().max_message_size(None).max_frame_size(None);
        let (socket, _) = tokio_tungstenite::connect_async_with_config(address, Some(config), true).await.map_err(|e| format!("Cannot reach the browser: {e}"))?;
        let (mut write, mut read) = socket.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
        let (events, incoming) = mpsc::unbounded_channel();
        let waiting: Waiting = Arc::new(Mutex::new(Some(HashMap::new())));
        tokio::spawn(async move {
            while let Some(text) = outgoing.recv().await {
                if write.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            let _ = write.close().await;
        });
        let answers = Arc::clone(&waiting);
        tokio::spawn(async move {
            while let Some(Ok(message)) = read.next().await {
                let Ok(mut value) = message.to_text().map_err(|_| ()).and_then(|text| serde_json::from_str::<Value>(text).map_err(|_| ())) else { continue };
                match value.get("id").and_then(Value::as_u64) {
                    Some(id) => {
                        let answer = match value.get("error") {
                            Some(error) => Err(error["message"].as_str().unwrap_or("the browser refused").to_string()),
                            None => Ok(value["result"].take()),
                        };
                        if let Some(caller) = answers.lock().as_mut().and_then(|waiting| waiting.remove(&id)) {
                            let _ = caller.send(answer);
                        }
                    }
                    None => {
                        let session = value["sessionId"].as_str().map(String::from);
                        let method = value["method"].as_str().unwrap_or_default().to_string();
                        if events.send(Event { session, method, params: value["params"].take() }).is_err() {
                            break;
                        }
                    }
                }
            }
            // The browser is gone: nobody gets an answer any more.
            *answers.lock() = None;
        });
        Ok((Self { out, waiting, next: Arc::new(AtomicU64::new(1)) }, incoming))
    }

    /// Sends the command `method` with `params` to `session` (None: the browser itself) and
    /// waits for its answer, [`CALL_LIMIT`] at most.
    async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        let (caller, answer) = oneshot::channel();
        match self.waiting.lock().as_mut() {
            Some(waiting) => waiting.insert(id, caller),
            None => return Err(GONE.to_string()),
        };
        self.out.send(message.to_string()).map_err(|_| GONE.to_string())?;
        match tokio::time::timeout(CALL_LIMIT, answer).await {
            Ok(Ok(answer)) => answer.map_err(|e| format!("{method}: {e}")),
            Ok(Err(_)) => Err(GONE.to_string()),
            Err(_) => {
                self.waiting.lock().as_mut().and_then(|waiting| waiting.remove(&id));
                Err(format!("{method}: the browser did not answer"))
            }
        }
    }
}

/// The browser, running on a profile of its own for one recording or one playback.
struct Browser {
    child: tokio::process::Child,
    /// Its renderers and helpers: they end with it, and all of them with this app.
    tree: Option<crate::media::ProcessTree>,
    profile: PathBuf,
    cdp: Cdp,
}

impl Browser {
    /// Starts the browser: `hidden`, without a window (a recording, a check), else in a window
    /// of its own; `offline`, with no network at all (a playback), else through `proxy` when one
    /// is set. Its DevTools port is on this computer alone, and its number the system's choice.
    async fn launch(hidden: bool, offline: bool, proxy: Option<&str>) -> Result<(Self, mpsc::UnboundedReceiver<Event>), String> {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let program = browser().ok_or_else(|| NO_BROWSER.to_string())?;
        let profile = std::env::temp_dir().join(format!("endo-browser-{}-{}", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(&profile).map_err(|e| format!("Cannot create {}: {e}", profile.display()))?;
        let mut command = tokio::process::Command::new(&program);
        command.arg(format!("--user-data-dir={}", profile.display())).args([
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-sync",
            "--disable-background-networking",
            "--disable-background-mode",
            "--disable-component-update",
            "--disable-default-apps",
        ]);
        match hidden {
            true => command.args(["--headless=new", "--window-size=1920,1080", "--hide-scrollbars", "--mute-audio"]),
            false => command.args(["--app=about:blank", "--window-size=1280,860"]),
        };
        match (offline, proxy) {
            // Nothing listens there, and nothing goes round it: not even this computer's own servers.
            (true, _) => command.args(["--proxy-server=http://127.0.0.1:9", "--proxy-bypass-list=<-loopback>"]),
            (false, Some(proxy)) => command.arg(format!("--proxy-server={}", browser_proxy(proxy))),
            (false, None) => &mut command,
        };
        command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        // A compatibility setting of this app's is not the browser's: Edge, started under one,
        // starts itself again without it and ends, and the browser would not be this one's child.
        command.env_remove("__COMPAT_LAYER");
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                leave(profile);
                return Err(format!("Cannot start {}: {e}", program.display()));
            }
        };
        let tree = Some(crate::media::ProcessTree::attach(&child));
        // The browser writes the port it took, and its own address there, into its profile.
        let (port, started) = (profile.join("DevToolsActivePort"), Instant::now());
        let address = loop {
            let text = std::fs::read_to_string(&port).unwrap_or_default();
            let mut lines = text.lines().map(str::trim);
            if let (Some(port), Some(path)) = (lines.next().filter(|port| !port.is_empty()), lines.next().filter(|path| path.starts_with('/'))) {
                break format!("ws://127.0.0.1:{port}{path}");
            }
            // One that ends well has handed over to a browser it started: the port file tells.
            let ended = child.try_wait().ok().flatten().is_some_and(|status| !status.success());
            if ended || started.elapsed() > START_LIMIT {
                let _ = child.start_kill();
                drop(tree);
                leave(profile);
                return Err(format!("{} did not start: {}", program.display(), if ended { "it closed at once" } else { "it took too long" }));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let browser = |cdp| Self { child, tree, profile, cdp };
        match Cdp::connect(&address).await {
            Ok((cdp, events)) => Ok((browser(cdp), events)),
            Err(e) => {
                // Dropped: stopped, its profile deleted.
                let (cdp, _) = Cdp::unconnected();
                drop(browser(cdp));
                Err(e)
            }
        }
    }

    /// Closes the browser, waiting a moment for it to leave by itself, and deletes its profile.
    async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.cdp.call(None, "Browser.close", json!({}))).await;
        if !matches!(tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await, Ok(Ok(_))) {
            let _ = self.child.kill().await;
        }
        // Reaped: its id is not its own any more.
        if let Some(mut tree) = self.tree.take() {
            tree.disarm();
        }
        // Its helpers let go of the profile's files a moment after it.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.profile).is_ok() || !self.profile.exists() {
                self.profile = PathBuf::new();
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

impl Cdp {
    /// A connection to nothing, for a browser that could not be reached.
    fn unconnected() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (out, outgoing) = mpsc::unbounded_channel();
        (Self { out, waiting: Arc::default(), next: Arc::new(AtomicU64::new(1)) }, outgoing)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        drop(self.tree.take());
        if !self.profile.as_os_str().is_empty() {
            leave(std::mem::take(&mut self.profile));
        }
    }
}

/// A proxy as the browser's command line takes it: its scheme, host and port, without a name and
/// password (the browser takes none there), and `socks5h` as the `socks5` it is to a browser.
fn browser_proxy(proxy: &str) -> String {
    match Url::parse(proxy) {
        Ok(url) => match (url.host_str(), url.port_or_known_default()) {
            (Some(host), Some(port)) => format!("{}://{host}:{port}", url.scheme().replace("socks5h", "socks5")),
            _ => proxy.to_string(),
        },
        Err(_) => proxy.to_string(),
    }
}

/// Deletes a browser's profile once the browser has let go of its files: tried here for half a
/// second (a program that ends right after must not leave it behind), then for ten more off
/// this thread.
fn leave(profile: PathBuf) {
    for _ in 0..10 {
        if std::fs::remove_dir_all(&profile).is_ok() || !profile.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::spawn(move || {
        for _ in 0..40 {
            if std::fs::remove_dir_all(&profile).is_ok() || !profile.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

/// What a recording does and where it stops.
#[derive(Clone, Debug)]
pub struct CopyOptions {
    /// Follow the links of each page, within the start link's folder on its host.
    pub whole_site: bool,
    /// How many links away from the start a page may be.
    pub depth: usize,
    /// Most pages one run copies.
    pub max_pages: usize,
    /// Most bytes of answers one run keeps.
    pub max_bytes: u64,
    /// Most bytes of one answer: a larger one (a long video) is left out.
    pub max_answer: u64,
    /// Between two pages, unless robots.txt asks for more.
    pub delay: Duration,
    /// The proxy the browser goes through.
    pub proxy: Option<String>,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self { whole_site: false, depth: 5, max_pages: 100, max_bytes: 1 << 30, max_answer: 64 << 20, delay: Duration::from_secs(1), proxy: None }
    }
}

/// How a recording went, so far or in the end.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Copied {
    pub file: PathBuf,
    /// The first page's title.
    pub title: String,
    /// Pages in the copy, earlier runs' counted.
    pub pages: usize,
    /// Answers this run kept, and their bytes.
    pub answers: usize,
    pub bytes: u64,
    /// Answers not kept: larger than [`CopyOptions::max_answer`], past [`CopyOptions::max_bytes`],
    /// or gone before they were read.
    pub left: usize,
    /// Pages robots.txt keeps out.
    pub disallowed: usize,
    /// Pages that could not be loaded, each with why.
    pub failed: Vec<(String, String)>,
    /// Why it stopped before the site was done (a limit), when it did: started again, it goes on.
    pub stopped: Option<String>,
}

/// A site's recording between two runs: the pages still to visit, each with how many links it
/// is from the start, and every page ever queued.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    start: String,
    /// Where the start landed: the site's pages are told by it.
    #[serde(default)]
    landed: String,
    title: String,
    pages: usize,
    queue: VecDeque<(String, usize)>,
    seen: HashSet<String>,
}

fn state_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".state.json");
    PathBuf::from(name)
}

/// The name of the copy of `start`, without its extension: its host and what of its path a file
/// name may hold (`example.com docs guide`).
fn copy_name(start: &Url) -> String {
    let host = start.host_str().unwrap_or("page").trim_start_matches("www.");
    let path: String = start.path().chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { ' ' }).collect();
    let name = format!("{host} {}", path.split_whitespace().collect::<Vec<_>>().join(" "));
    crate::ingest::truncate_chars(name.trim().trim_end_matches('.'), 120)
}

/// The file in `dir` a copy of `start` goes into, and the state of the run it continues: an
/// unfinished copy of that link's `site` goes on; any other is kept, and the new one numbered.
fn copy_file(dir: &Path, start: &Url, site: bool) -> (PathBuf, State) {
    let name = copy_name(start);
    for n in 0.. {
        let file = dir.join(if n == 0 { format!("{name}{EXTENSION}") } else { format!("{name} ({n}){EXTENSION}") });
        let state = std::fs::read(state_path(&file)).ok().and_then(|text| serde_json::from_slice::<State>(&text).ok());
        match state {
            Some(state) if site && state.start == start.as_str() => return (file, state),
            None if !file.exists() => return (file, State::default()),
            _ => {}
        }
    }
    unreachable!("a number is free")
}

/// Whether `url` is a page of the site that starts at `first`, as it landed: on its host and
/// scheme, in its folder or below, and not a file by its name.
fn within(first: &Url, url: &Url) -> bool {
    let folder = &first.path()[..=first.path().rfind('/').unwrap_or(0)];
    url.origin() == first.origin() && url.path().starts_with(folder) && crate::resolver::named_file(url).is_none()
}

/// What one request and its answer are kept under, and found by: the method, the link without
/// its fragment, and, for a request that sent something, the digest of what it sent.
fn key(method: &str, url: &str, sent: &str) -> String {
    let url = url.split('#').next().unwrap_or(url);
    match sent {
        "" => format!("{method} {url}"),
        sent => format!("{method} {url} {sent}"),
    }
}

/// The digest of what a request sent (see `warc::payload_digest`); empty when it sent nothing.
fn sent_digest(body: &[u8]) -> String {
    if body.is_empty() { String::new() } else { warc::payload_digest(body) }
}

/// The parameters a page adds to a link only so that it is fetched afresh: a number under one
/// of these names, and any tracking tag.
const BUSTERS: &[&str] = &["_", "_t", "_ts", "_dc", "cb", "cachebust", "cachebuster", "bust", "nocache", "rand", "random", "t", "ts", "time", "timestamp"];

/// `url` without the parameters that only keep it from a cache (see [`BUSTERS`]).
fn without_busters(url: &Url) -> String {
    let buster = |name: &str, value: &str| {
        let name = name.to_ascii_lowercase();
        name.starts_with("utm_") || (BUSTERS.contains(&name.as_str()) && !value.is_empty() && value.chars().all(|c| c.is_ascii_digit() || c == '.'))
    };
    let kept: Vec<(String, String)> = url.query_pairs().filter(|(name, value)| !buster(name, value)).map(|(name, value)| (name.into_owned(), value.into_owned())).collect();
    let mut url = url.clone();
    url.set_fragment(None);
    match kept.is_empty() {
        true => url.set_query(None),
        false => {
            url.query_pairs_mut().clear().extend_pairs(kept);
        }
    }
    url.to_string()
}

/// The extensions of files a site names with a version or a stamp after `?` that changes
/// nothing of the file: a request for one is answered by the file whatever follows the `?`.
const STATIC: &[&str] = &["js", "mjs", "css", "json", "png", "jpg", "jpeg", "gif", "webp", "avif", "svg", "ico", "woff", "woff2", "ttf", "otf", "mp4", "webm", "mp3", "wasm"];

/// `url` without its query, when it names a file of [`STATIC`].
fn without_query(url: &Url) -> Option<String> {
    let extension = url.path().rsplit_once('.').map(|(_, extension)| extension.to_ascii_lowercase())?;
    STATIC.contains(&extension.as_str()).then(|| {
        let mut url = url.clone();
        url.set_query(None);
        url.set_fragment(None);
        url.to_string()
    })
}

/// An answer of a copy: where its record is in the file, and what asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    offset: u64,
    method: String,
    url: String,
    /// The digest of what the request sent; empty when nothing.
    sent: String,
    /// The digest of the answer's body.
    digest: String,
}

/// A working copy read back: its answers, findable by what asks for them.
pub struct Copy {
    file: std::fs::File,
    kept: Vec<Kept>,
    /// The answers by what they were asked with: as asked, without what only keeps a link from
    /// a cache, and without a static file's query (see [`Copy::find`]).
    exact: HashMap<String, Vec<usize>>,
    loose: HashMap<String, Vec<usize>>,
    bare: HashMap<String, Vec<usize>>,
    /// How often each was asked for so far.
    turns: HashMap<String, usize>,
}

impl Copy {
    /// Reads the copy in `path`: every record's head, no answer's body.
    pub fn open(path: &Path) -> Result<Self, String> {
        let records = warc::scan(path)?;
        let mut kept: Vec<Kept> = Vec::new();
        let mut by_id: HashMap<&str, usize> = HashMap::new();
        for record in &records {
            match record.kind.as_str() {
                "response" => {
                    by_id.insert(&record.id, kept.len());
                    kept.push(Kept { offset: record.offset, method: "GET".to_string(), url: record.url.clone(), sent: String::new(), digest: record.digest.clone() });
                }
                "request" => {
                    if let Some(&n) = record.answers.as_deref().and_then(|id| by_id.get(id)) {
                        kept[n].method = record.first.split(' ').next().unwrap_or("GET").to_string();
                        kept[n].sent = Some(record.digest.clone()).filter(|digest| *digest != warc::payload_digest(b"")).unwrap_or_default();
                    }
                }
                _ => {}
            }
        }
        if kept.is_empty() {
            return Err(format!("{} holds no page: it is not a working copy, or its recording never began", path.display()));
        }
        let (mut exact, mut loose, mut bare) = (HashMap::<String, Vec<usize>>::new(), HashMap::<String, Vec<usize>>::new(), HashMap::<String, Vec<usize>>::new());
        for (n, answer) in kept.iter().enumerate() {
            exact.entry(key(&answer.method, &answer.url, &answer.sent)).or_default().push(n);
            if let Ok(url) = Url::parse(&answer.url) {
                loose.entry(key(&answer.method, &without_busters(&url), "")).or_default().push(n);
                if let Some(file) = without_query(&url) {
                    bare.entry(key(&answer.method, &file, "")).or_default().push(n);
                }
            }
        }
        let file = std::fs::File::open(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        Ok(Self { file, kept, exact, loose, bare, turns: HashMap::new() })
    }

    /// The link of the copy's first page: the first one asked for.
    pub fn start(&self) -> &str {
        &self.kept[0].url
    }

    /// How many answers the copy holds.
    pub fn len(&self) -> usize {
        self.kept.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }

    /// The answer the copy has for a request of `method` for `url` that sent `body`: the one it
    /// got for just that request; else for the link without the parameters that only keep it
    /// from a cache (a time stamp, a tracking tag), whatever it sent; else, for a static file
    /// (a script, a style sheet, a picture), for the file whatever its query. Where it was
    /// answered several times differently while recording, each asking gets the next of them,
    /// and the last one from then on; with `first`, always the first (a part of a file).
    fn find(&mut self, method: &str, url: &str, body: &[u8], first: bool) -> Option<usize> {
        let parsed = Url::parse(url).ok();
        let keys = [
            Some(key(method, url, &sent_digest(body))),
            parsed.as_ref().map(|url| key(method, &without_busters(url), "")),
            parsed.as_ref().and_then(without_query).map(|file| key(method, &file, "")),
        ];
        for (map, key) in [&self.exact, &self.loose, &self.bare].into_iter().zip(keys) {
            let Some((key, found)) = key.and_then(|key| map.get(&key).map(|found| (key, found))) else { continue };
            let turn = self.turns.entry(key).or_insert(0);
            let n = found[if first { 0 } else { (*turn).min(found.len() - 1) }];
            *turn += 1;
            return Some(n);
        }
        None
    }

    /// The answer `n`: its status, headers and body.
    fn read(&mut self, n: usize) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
        warc::read_at(&mut self.file, self.kept[n].offset)
    }
}

/// The part `range` (a request's `Range` header) asks of an answer that holds the whole file:
/// status 206, its headers with that part's length and place, and the part. The answer as it is
/// when the header is none this app reads, or asks for nothing the file has.
fn part_of((status, headers, body): (u16, Vec<(String, String)>, Vec<u8>), range: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let total = body.len() as u64;
    let asked = range.trim().strip_prefix("bytes=").filter(|spec| !spec.contains(',')).and_then(|spec| spec.split_once('-'));
    let span = asked.and_then(|(from, to)| match (from.trim().parse::<u64>(), to.trim().parse::<u64>()) {
        (Ok(from), Ok(to)) => Some((from, to.min(total.saturating_sub(1)))),
        (Ok(from), Err(_)) if to.trim().is_empty() => Some((from, total.saturating_sub(1))),
        // The last so many bytes.
        (Err(_), Ok(last)) if from.trim().is_empty() => Some((total.saturating_sub(last), total.saturating_sub(1))),
        _ => None,
    });
    let Some((from, to)) = span.filter(|(from, to)| from <= to && *to < total) else { return (status, headers, body) };
    let part = body[from as usize..=to as usize].to_vec();
    let mut headers: Vec<(String, String)> = headers.into_iter().filter(|(name, _)| !matches!(name.to_ascii_lowercase().as_str(), "content-length" | "content-range")).collect();
    headers.push(("Content-Length".to_string(), part.len().to_string()));
    headers.push(("Content-Range".to_string(), format!("bytes {from}-{to}/{total}")));
    (206, headers, part)
}

/// Whether an answer of `status` with `headers` and a body of `len` bytes holds its whole file:
/// not a part of it (206), unless that part is all of it from its start.
fn whole(status: u16, headers: &[(String, String)], len: usize) -> bool {
    let range = headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("content-range")).map(|(_, value)| value.as_str());
    match (status, range) {
        (200, _) => true,
        (206, Some(range)) => range.trim().strip_prefix("bytes 0-").and_then(|rest| rest.split_once('/')).is_some_and(|(_, total)| total.trim().parse() == Ok(len)),
        _ => false,
    }
}

/// The headers of a CDP message (`[{name, value}]`, or `{name: value}`) as pairs.
fn pairs(headers: &Value) -> Vec<(String, String)> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    match headers {
        Value::Array(list) => list.iter().map(|header| (text(&header["name"]), text(&header["value"]))).collect(),
        Value::Object(map) => map.iter().map(|(name, value)| (name.clone(), text(value))).collect(),
        _ => Vec::new(),
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

/// What a recording keeps as it goes: the file, what it holds already (each request's answers,
/// by their digests, so that the same answer is kept once), and its counts.
struct Store {
    writer: Option<warc::Writer>,
    known: HashMap<String, Vec<String>>,
    answers: usize,
    bytes: u64,
    left: usize,
    max_answer: u64,
    /// Most bytes the run keeps; `full` once an answer was left out for it.
    max_bytes: u64,
    full: bool,
    error: Option<String>,
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    fn keep(&mut self, method: &str, url: &Url, sent: &[(String, String)], body: &[u8], status: u16, headers: Vec<(String, String)>, payload: &[u8]) {
        let digest = warc::payload_digest(payload);
        let known = self.known.entry(key(method, url.as_str(), &sent_digest(body))).or_default();
        if known.contains(&digest) {
            return;
        }
        if self.bytes + payload.len() as u64 > self.max_bytes {
            (self.full, self.left) = (true, self.left + 1);
            return;
        }
        known.push(digest);
        // The body is as the browser had it, decoded and whole: the headers are made to say so.
        let mut headers: Vec<(String, String)> = headers.into_iter().filter(|(name, _)| !matches!(name.to_ascii_lowercase().as_str(), "content-encoding" | "content-length" | "transfer-encoding")).collect();
        headers.push(("Content-Length".to_string(), payload.len().to_string()));
        let Some(writer) = self.writer.as_mut() else { return };
        match writer.exchange(url, SystemTime::now(), method, sent, body, status, &headers, payload) {
            Ok(()) => (self.answers, self.bytes) = (self.answers + 1, self.bytes + payload.len() as u64),
            Err(e) => self.error = Some(e),
        }
    }
}

/// What a recording's two halves share: the one that keeps the answers as they come, and the
/// one that goes from page to page.
struct Shared {
    store: Mutex<Store>,
    /// The requests under way, and when the last one began or ended.
    open: Mutex<(HashSet<String>, Instant)>,
    /// How many pages have loaded.
    loads: AtomicU64,
}

/// Makes `session` (a tab, a frame, a worker) pause each of its requests: once answered
/// (`answered`: a recording, which keeps the answer), or before it is sent (a playback, which
/// answers it); and has what it opens (frames of other sites, workers) attached and held until
/// they are set up the same. What a kind of session cannot do is passed over.
async fn hold_requests(cdp: &Cdp, session: &str, answered: bool) {
    let pattern = if answered { json!({"urlPattern": "*", "requestStage": "Response"}) } else { json!({"urlPattern": "*"}) };
    let _ = cdp.call(Some(session), "Fetch.enable", json!({"patterns": [pattern]})).await;
    let _ = cdp.call(Some(session), "Network.setBypassServiceWorker", json!({"bypass": true})).await;
    let _ = cdp.call(Some(session), "Target.setAutoAttach", json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true})).await;
}

/// Keeps one answer the browser paused at (a `Fetch.requestPaused` once answered), then lets it
/// through to the page.
async fn keep_answer(cdp: Cdp, session: Option<String>, paused: Value, shared: Arc<Shared>) {
    let session = session.as_deref();
    let request = &paused["request"];
    let id = paused["requestId"].clone();
    let url = request["url"].as_str().and_then(|url| Url::parse(url).ok()).filter(|url| matches!(url.scheme(), "http" | "https"));
    if let (Some(url), Some(status)) = (url, paused["responseStatusCode"].as_u64()) {
        let status = status as u16;
        let headers = pairs(&paused["responseHeaders"]);
        let max = shared.store.lock().max_answer;
        // Too long to keep; or a stream of events, which has no end to wait for.
        let long = header(&headers, "content-length").and_then(|n| n.trim().parse::<u64>().ok()).is_some_and(|n| n > max);
        let endless = header(&headers, "content-type").is_some_and(|kind| kind.trim().to_ascii_lowercase().starts_with("text/event-stream"));
        // A redirect has no body to ask for, and neither has an answer that says so.
        let bodiless = matches!(status, 204 | 205 | 301 | 302 | 303 | 304 | 307 | 308) || request["method"] == "HEAD";
        let body = match (long || endless, bodiless) {
            (true, _) => None,
            (false, true) => Some(Vec::new()),
            (false, false) => cdp.call(session, "Fetch.getResponseBody", json!({"requestId": id})).await.ok().and_then(|answer| match answer["base64Encoded"].as_bool() {
                Some(true) => base64::engine::general_purpose::STANDARD.decode(answer["body"].as_str().unwrap_or_default()).ok(),
                _ => Some(answer["body"].as_str().unwrap_or_default().as_bytes().to_vec()),
            }),
        };
        let mut store = shared.store.lock();
        match body.filter(|body| body.len() as u64 <= max) {
            Some(body) => {
                let sent = request["postData"].as_str().unwrap_or_default().as_bytes();
                store.keep(request["method"].as_str().unwrap_or("GET"), &url, &pairs(&request["headers"]), sent, status, headers, &body);
            }
            None => store.left += 1,
        }
    }
    let _ = cdp.call(session, "Fetch.continueRequest", json!({"requestId": id})).await;
}

/// The half of a recording that keeps the answers: every request of the page `page`, and of
/// what it opens, is paused once answered, kept (see [`keep_answer`]; each in a task of its own,
/// so that a slow one holds up no other) and let through. It also
/// notes when the page has loaded and which requests are under way, and closes a page's
/// dialogs, which would hold it up.
async fn keep_answers(cdp: Cdp, mut events: mpsc::UnboundedReceiver<Event>, shared: Arc<Shared>, page: String) {
    let mut held = HashSet::new();
    while let Some(event) = events.recv().await {
        if event.method == "Fetch.requestPaused" {
            tokio::spawn(keep_answer(cdp.clone(), event.session, event.params, Arc::clone(&shared)));
            continue;
        }
        let session = event.session.as_deref();
        match event.method.as_str() {
            "Network.requestWillBeSent" => {
                let mut open = shared.open.lock();
                open.0.insert(event.params["requestId"].as_str().unwrap_or_default().to_string());
                open.1 = Instant::now();
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                let mut open = shared.open.lock();
                open.0.remove(event.params["requestId"].as_str().unwrap_or_default());
                open.1 = Instant::now();
            }
            "Page.loadEventFired" if session == Some(page.as_str()) => {
                shared.loads.fetch_add(1, Ordering::Relaxed);
            }
            // Each answered in a task of its own: nothing waited for here may hold up the answers
            // (a page whose request is held answers no command until that is let through).
            "Page.javascriptDialogOpening" => {
                let (cdp, session) = (cdp.clone(), event.session.clone());
                tokio::spawn(async move { cdp.call(session.as_deref(), "Page.handleJavaScriptDialog", json!({"accept": true})).await });
            }
            // What the page opened: the page itself, told of when this attached to it, is set up.
            "Target.attachedToTarget" => {
                let Some(child) = event.params["sessionId"].as_str().filter(|child| *child != page) else { continue };
                // One frame or worker can be told of twice: its requests are held once.
                let new = held.insert(event.params["targetInfo"]["targetId"].as_str().unwrap_or(child).to_string());
                let (cdp, child) = (cdp.clone(), child.to_string());
                tokio::spawn(async move {
                    if new {
                        hold_requests(&cdp, &child, true).await;
                    }
                    let _ = cdp.call(Some(&child), "Runtime.runIfWaitingForDebugger", json!({})).await;
                });
            }
            _ => {}
        }
    }
}

/// What a page showed once visited: where it landed, its title and its links.
struct Seen {
    url: Url,
    title: String,
    links: Vec<String>,
}

/// Loads `url` in the page `session`, does there what a visitor would (see [`VISITOR`]), waits
/// for the network to fall quiet, and reads what the page shows.
async fn visit(cdp: &Cdp, session: &str, shared: &Shared, url: &Url) -> Result<Seen, String> {
    let before = shared.loads.load(Ordering::Relaxed);
    let went = cdp.call(Some(session), "Page.navigate", json!({"url": url.as_str()})).await?;
    if let Some(error) = went["errorText"].as_str().filter(|error| !error.is_empty()) {
        return Err(error.to_string());
    }
    let started = Instant::now();
    while shared.loads.load(Ordering::Relaxed) == before && started.elapsed() < LOAD_LIMIT {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A page that leaves for another while it is scrolled is read where it went.
    let _ = cdp.call(Some(session), "Runtime.evaluate", json!({"expression": VISITOR, "awaitPromise": true, "returnByValue": true, "userGesture": true})).await;
    let started = Instant::now();
    while started.elapsed() < QUIET_LIMIT {
        let quiet = {
            let open = shared.open.lock();
            open.0.is_empty() && open.1.elapsed() >= QUIET
        };
        if quiet {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let read = cdp.call(Some(session), "Runtime.evaluate", json!({"expression": READ, "returnByValue": true})).await?;
    let read = &read["result"]["value"];
    let landed = read["url"].as_str().and_then(|url| Url::parse(url).ok()).filter(|url| matches!(url.scheme(), "http" | "https")).unwrap_or_else(|| url.clone());
    let links = read["links"].as_array().map(|links| links.iter().filter_map(|link| link.as_str().map(String::from)).collect()).unwrap_or_default();
    Ok(Seen { url: landed, title: read["title"].as_str().unwrap_or_default().trim().to_string(), links })
}

/// Records the page at `start`, or with `options.whole_site` the site that starts there, into a
/// file of `dir` (see the module), telling `tell` how it goes after each page. `http` asks for
/// the site's robots.txt. An unfinished copy of the same link in `dir` goes on; a finished one
/// is kept, and the new one numbered.
pub async fn record(http: &reqwest::Client, start: &Url, dir: &Path, options: &CopyOptions, mut tell: impl FnMut(&Copied)) -> Result<Copied, String> {
    if !matches!(start.scheme(), "http" | "https") {
        return Err(format!("{start} is not a web page's link"));
    }
    let robots = match options.whole_site {
        true => robots_of(http, start, options.delay).await?,
        false => Robots::default(),
    };
    if !robots.allows(&robots_path(start)) {
        return Err(format!("The robots.txt of {} keeps this page out of copies of the site; the page alone can be copied", start.host_str().unwrap_or_default()));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create the folder {}: {e}", dir.display()))?;
    let (file, mut state) = copy_file(dir, start, options.whole_site);
    let fresh = !file.exists();
    // What an earlier run kept is not kept twice.
    let mut known: HashMap<String, Vec<String>> = HashMap::new();
    if !fresh {
        let path = file.clone();
        if let Ok(Ok(copy)) = tokio::task::spawn_blocking(move || Copy::open(&path)).await {
            for answer in &copy.kept {
                known.entry(key(&answer.method, &answer.url, &answer.sent)).or_default().push(answer.digest.clone());
            }
        }
    }
    if state.start.is_empty() {
        state = State { start: start.to_string(), queue: VecDeque::from([(start.to_string(), 0)]), seen: HashSet::from([start.to_string()]), ..State::default() };
    }
    let (browser, events) = Browser::launch(true, false, options.proxy.as_deref()).await?;
    let writer = match warc::Writer::open(&file) {
        Ok(writer) => writer,
        Err(e) => {
            browser.close().await;
            return Err(e);
        }
    };
    let store = Store { writer: Some(writer), known, answers: 0, bytes: 0, left: 0, max_answer: options.max_answer, max_bytes: options.max_bytes, full: false, error: None };
    let shared = Arc::new(Shared { store: Mutex::new(store), open: Mutex::new((HashSet::new(), Instant::now())), loads: AtomicU64::new(0) });
    let mut begun = Begun { shared: Arc::clone(&shared), file: file.clone(), kept: !fresh };
    let went = pages(&browser.cdp, events, &shared, &file, &mut state, &robots, options, &mut begun.kept, &mut tell).await;
    browser.close().await;
    // The file's index, whatever happened; the state, while pages are left.
    let (writer, error) = {
        let mut store = shared.store.lock();
        (store.writer.take(), store.error.take())
    };
    if let Some(writer) = writer {
        writer.finish()?;
    }
    let done = went?;
    match (state.queue.is_empty(), options.whole_site) {
        (false, true) => save_state(&file, &state)?,
        _ => {
            let _ = std::fs::remove_file(state_path(&file));
        }
    }
    match error {
        Some(error) => Err(error),
        None => Ok(done),
    }
}

/// A copy whose first page never arrived is none: its files go with the run that began it, when
/// that fails or is stopped before then.
struct Begun {
    shared: Arc<Shared>,
    file: PathBuf,
    kept: bool,
}

impl Drop for Begun {
    fn drop(&mut self) {
        if !self.kept {
            // Closed first: it writes its index as it goes.
            drop(self.shared.store.lock().writer.take());
            for file in [self.file.clone(), warc::index_path(&self.file), state_path(&self.file)] {
                let _ = std::fs::remove_file(file);
            }
        }
    }
}

fn save_state(file: &Path, state: &State) -> Result<(), String> {
    let (path, text) = (state_path(file), serde_json::to_vec(state).map_err(|e| e.to_string())?);
    let part = path.with_extension("json.part");
    std::fs::write(&part, text).and_then(|()| std::fs::rename(&part, &path)).map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

/// The half of a recording that goes from page to page (see [`record`]): the pages of `state`'s
/// queue, each visited in one tab, its links queued when a site is copied, until the queue is
/// empty or a limit of `options` is reached. `kept` is set once the copy has its first page.
#[allow(clippy::too_many_arguments)]
async fn pages(
    cdp: &Cdp,
    events: mpsc::UnboundedReceiver<Event>,
    shared: &Arc<Shared>,
    file: &Path,
    state: &mut State,
    robots: &Robots,
    options: &CopyOptions,
    kept: &mut bool,
    tell: &mut impl FnMut(&Copied),
) -> Result<Copied, String> {
    // A link that is a file's would be downloaded: none is.
    let _ = cdp.call(None, "Browser.setDownloadBehavior", json!({"behavior": "deny"})).await;
    let tab = cdp.call(None, "Target.createTarget", json!({"url": "about:blank"})).await?;
    let attached = cdp.call(None, "Target.attachToTarget", json!({"targetId": tab["targetId"], "flatten": true})).await?;
    let session = attached["sessionId"].as_str().ok_or("the browser opened no tab")?.to_string();
    cdp.call(Some(&session), "Page.enable", json!({})).await?;
    cdp.call(Some(&session), "Network.enable", json!({})).await?;
    hold_requests(cdp, &session, true).await;
    let keeping = tokio::spawn(keep_answers(cdp.clone(), events, Arc::clone(shared), session.clone()));
    let mut done = Copied { file: file.to_path_buf(), title: state.title.clone(), pages: state.pages, ..Copied::default() };
    let delay = robots.delay.map_or(options.delay, |asked| asked.max(options.delay));
    // The site's pages are told by where its start landed.
    let mut scope = Url::parse(if state.landed.is_empty() { &state.start } else { &state.landed }).ok();
    let (mut first, mut run) = (true, 0usize);
    while let Some((link, depth)) = state.queue.pop_front() {
        let over = match () {
            () if run >= options.max_pages => Some(format!("this run copied {run} pages, the most asked for")),
            () if shared.store.lock().full => Some("this run reached the size asked for".to_string()),
            // The file takes no more (a full disk): the pages left wait for the next run.
            () if shared.store.lock().error.is_some() => Some("the copy's file could not be written".to_string()),
            () => None,
        };
        if let Some(over) = over {
            state.queue.push_front((link, depth));
            done.stopped = Some(over);
            break;
        }
        let Ok(url) = Url::parse(&link) else { continue };
        if !robots.allows(&robots_path(&url)) {
            done.disallowed += 1;
            continue;
        }
        if !first {
            tokio::time::sleep(delay).await;
        }
        match visit(cdp, &session, shared, &url).await {
            Ok(seen) => {
                if state.pages == 0 {
                    (done.title, scope) = (seen.title.clone(), Some(seen.url.clone()));
                    (state.title, state.landed) = (seen.title, seen.url.to_string());
                    // Where the start landed is the site: links to it are not new pages.
                    state.seen.insert(seen.url.to_string());
                }
                (run, done.pages) = (run + 1, done.pages + 1);
                (state.pages, *kept) = (done.pages, true);
                if let (true, Some(scope)) = (options.whole_site && depth < options.depth, scope.as_ref()) {
                    for mut found in seen.links.iter().filter_map(|link| Url::parse(link).ok()) {
                        found.set_fragment(None);
                        if within(scope, &found) && state.seen.insert(found.to_string()) {
                            state.queue.push_back((found.to_string(), depth + 1));
                        }
                    }
                }
            }
            // The page asked for is the copy: without it there is none.
            Err(e) if state.pages == 0 => {
                keeping.abort();
                return Err(format!("{link} could not be loaded: {e}"));
            }
            Err(e) => done.failed.push((link, e)),
        }
        first = false;
        {
            let store = shared.store.lock();
            (done.answers, done.bytes, done.left) = (store.answers, store.bytes, store.left);
        }
        tell(&done);
        if options.whole_site {
            save_state(file, state)?;
        }
    }
    keeping.abort();
    Ok(done)
}

/// The page a playback shows for a link the copy does not hold.
fn not_there(url: &str) -> Vec<u8> {
    let shown: String = url.chars().flat_map(|c| match c {
        '<' => "&lt;".chars().collect::<Vec<_>>(),
        '>' => "&gt;".chars().collect(),
        '&' => "&amp;".chars().collect(),
        c => vec![c],
    }).collect();
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Not in this copy</title>\
         <body style=\"font: 16px system-ui, sans-serif; margin: 3rem; max-width: 40rem\"><h1>Not in this copy</h1>\
         <p>{shown}</p><p>This page was not visited when the copy was made, so the copy does not hold it.</p>\
         <p><a href=\"javascript:history.back()\">Back</a></p></body>"
    )
    .into_bytes()
}

/// Answers one request the browser paused before sending it (a `Fetch.requestPaused`) from the
/// copy; a link the copy does not hold gets a 404, a page's with a few words.
async fn answer(cdp: Cdp, session: Option<String>, paused: Value, copy: Arc<Mutex<Copy>>) {
    let session = session.as_deref();
    let request = &paused["request"];
    let id = paused["requestId"].clone();
    let url = request["url"].as_str().unwrap_or_default();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        let _ = cdp.call(session, "Fetch.continueRequest", json!({"requestId": id})).await;
        return;
    }
    let method = request["method"].as_str().unwrap_or("GET");
    let asked = pairs(&request["headers"]);
    let range = header(&asked, "range");
    let found = {
        let mut copy = copy.lock();
        let body = request["postData"].as_str().unwrap_or_default().as_bytes();
        copy.find(method, url, body, range.is_some()).and_then(|n| copy.read(n).ok())
    };
    let (status, headers, body) = match (found, range) {
        (Some(found), Some(range)) if whole(found.0, &found.1, found.2.len()) => part_of(found, range),
        (Some(found), _) => found,
        (None, _) if paused["resourceType"] == "Document" => (404, vec![("Content-Type".to_string(), "text/html; charset=utf-8".to_string())], not_there(url)),
        (None, _) => (404, vec![("Content-Type".to_string(), "text/plain".to_string())], Vec::new()),
    };
    let headers: Vec<Value> = headers.iter().map(|(name, value)| json!({"name": name, "value": value})).collect();
    let body = base64::engine::general_purpose::STANDARD.encode(body);
    let _ = cdp.call(session, "Fetch.fulfillRequest", json!({"requestId": id, "responseCode": status, "responseHeaders": headers, "body": body})).await;
}

/// The half of a playback that answers: every tab and window of the browser, and what they open
/// (frames, workers), has its requests held and answered from `copy` (see [`answer`]). `first`
/// is told the first tab's session. It ends with the last tab, and closes the browser then.
async fn answer_all(cdp: Cdp, mut events: mpsc::UnboundedReceiver<Event>, copy: Copy, first: oneshot::Sender<String>) {
    let copy = Arc::new(Mutex::new(copy));
    // The sessions whose requests are held, by what they are of; and those of them that are tabs.
    let (mut first, mut held, mut tabs) = (Some(first), HashMap::<String, String>::new(), HashSet::<String>::new());
    while let Some(event) = events.recv().await {
        match event.method.as_str() {
            "Fetch.requestPaused" => {
                tokio::spawn(answer(cdp.clone(), event.session, event.params, Arc::clone(&copy)));
            }
            "Target.attachedToTarget" => {
                let Some(child) = event.params["sessionId"].as_str() else { continue };
                let target = &event.params["targetInfo"];
                let id = target["targetId"].as_str().unwrap_or(child);
                // A worker is told of by the browser and by its page: its requests are held once.
                let new = !held.contains_key(id);
                let mut tell = None;
                if new {
                    held.insert(id.to_string(), child.to_string());
                    if event.session.is_none() && target["type"] == "page" {
                        tabs.insert(child.to_string());
                        tell = first.take();
                    }
                }
                // In a task of its own: nothing waited for here may hold up the answers.
                let (cdp, child) = (cdp.clone(), child.to_string());
                tokio::spawn(async move {
                    if new {
                        hold_requests(&cdp, &child, false).await;
                    }
                    let _ = cdp.call(Some(&child), "Runtime.runIfWaitingForDebugger", json!({})).await;
                    if let Some(tell) = tell {
                        let _ = tell.send(child);
                    }
                });
            }
            "Target.detachedFromTarget" => {
                let gone = event.params["sessionId"].as_str().unwrap_or_default();
                held.retain(|_, session| session != gone);
                // Its last window closed: the browser has nothing left to show.
                if tabs.remove(gone) && tabs.is_empty() {
                    let _ = cdp.call(None, "Browser.close", json!({})).await;
                    break;
                }
            }
            _ => {}
        }
    }
}

/// A copy playing in a browser (see [`Player::open`]).
pub struct Player {
    browser: Browser,
    /// The first tab's session.
    #[cfg_attr(not(test), allow(dead_code))]
    page: String,
    answering: tokio::task::JoinHandle<()>,
}

impl Player {
    /// Opens the copy in `file` in a browser window of its own (`hidden`: in none, for a check)
    /// that has no network, and shows its first page: every request is answered from the copy
    /// (see [`answer_all`]), in every tab and window its pages open.
    pub async fn open(file: &Path, hidden: bool) -> Result<Self, String> {
        let path = file.to_path_buf();
        let copy = tokio::task::spawn_blocking(move || Copy::open(&path)).await.map_err(|e| e.to_string())??;
        let start = copy.start().to_string();
        let (browser, events) = Browser::launch(hidden, true, None).await?;
        let (first, page) = oneshot::channel();
        let answering = tokio::spawn(answer_all(browser.cdp.clone(), events, copy, first));
        match Self::show(&browser.cdp, page, &start).await {
            Ok(page) => Ok(Self { browser, page, answering }),
            Err(e) => {
                answering.abort();
                browser.close().await;
                Err(e)
            }
        }
    }

    /// Has the browser's tabs held (the one it opened with, and those its pages open), and shows
    /// `start` in the first one. Gives that tab's session.
    async fn show(cdp: &Cdp, mut page: oneshot::Receiver<String>, start: &str) -> Result<String, String> {
        cdp.call(None, "Target.setAutoAttach", json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true})).await?;
        // A browser that opened with no tab gets one.
        let page = match tokio::time::timeout(Duration::from_secs(3), &mut page).await {
            Ok(page) => page.ok(),
            Err(_) => {
                cdp.call(None, "Target.createTarget", json!({"url": "about:blank"})).await?;
                tokio::time::timeout(START_LIMIT, page).await.ok().and_then(Result::ok)
            }
        };
        let page = page.ok_or("the browser opened no window")?;
        cdp.call(Some(&page), "Page.enable", json!({})).await?;
        cdp.call(Some(&page), "Page.navigate", json!({"url": start})).await?;
        Ok(page)
    }

    /// Waits until the copy's last window is closed, then lets the browser go.
    pub async fn closed(self) {
        let Self { browser, answering, .. } = self;
        let _ = answering.await;
        browser.close().await;
    }

    /// What the script `expression` gives in the copy's first page.
    #[cfg(test)]
    async fn eval(&self, expression: &str) -> Result<Value, String> {
        let answer = self.browser.cdp.call(Some(&self.page), "Runtime.evaluate", json!({"expression": expression, "returnByValue": true, "awaitPromise": true})).await?;
        Ok(answer["result"]["value"].clone())
    }

    /// Closes a copy that plays in no window.
    #[cfg(test)]
    async fn shut(self) {
        self.answering.abort();
        self.browser.close().await;
    }
}

/// Plays the copy in `file` in a browser window (see [`Player::open`]) until that is closed.
pub async fn play(file: &Path) -> Result<(), String> {
    Player::open(file, false).await?.closed().await;
    Ok(())
}

/// Whether `file` is a working copy by its name.
pub fn is_copy(file: &Path) -> bool {
    file.file_name().is_some_and(|name| name.to_string_lossy().to_ascii_lowercase().ends_with(EXTENSION))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn a_copy_is_named_by_its_link_and_a_site_is_its_starts_folder() {
        assert_eq!(copy_name(&url("https://www.example.com/docs/guide/?a=1")), "example.com docs guide");
        assert_eq!(copy_name(&url("https://example.com/")), "example.com");
        let start = url("https://example.com/docs/guide");
        assert!(within(&start, &url("https://example.com/docs/other")));
        assert!(within(&start, &url("https://example.com/docs/deep/page.html")));
        assert!(!within(&start, &url("https://example.com/blog/")));
        assert!(!within(&start, &url("http://example.com/docs/other")));
        assert!(!within(&start, &url("https://cdn.example.com/docs/other")));
        // A file is downloaded, not visited.
        assert!(!within(&start, &url("https://example.com/docs/setup.zip")));
        assert!(is_copy(Path::new("x/example.com docs.WARC.GZ")) && !is_copy(Path::new("x/page.html")));
        assert_eq!(browser_proxy("socks5h://name:word@10.0.0.1:1080"), "socks5://10.0.0.1:1080");
        assert_eq!(browser_proxy("http://proxy.example:8080/"), "http://proxy.example:8080");
    }

    #[test]
    fn what_only_keeps_a_link_from_a_cache_is_left_out_of_it() {
        assert_eq!(without_busters(&url("https://a.example/x?b=2&utm_source=n&t=1700000000&a=1#f")), "https://a.example/x?b=2&a=1");
        assert_eq!(without_busters(&url("https://a.example/x?_=1")), "https://a.example/x");
        // A parameter of such a name that is no number is the page's own.
        assert_eq!(without_busters(&url("https://a.example/x?t=dark")), "https://a.example/x?t=dark");
        assert_eq!(without_query(&url("https://a.example/app.JS?v=1")).as_deref(), Some("https://a.example/app.JS"));
        assert_eq!(without_query(&url("https://a.example/search?q=1")), None);
        assert_eq!(key("GET", "https://a.example/x#top", ""), "GET https://a.example/x");
    }

    #[test]
    fn a_part_of_a_whole_answer_is_cut_as_the_request_asks() {
        let pairs = |list: &[(&str, &str)]| list.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect::<Vec<_>>();
        let answer = || (200u16, pairs(&[("Content-Type", "video/mp4"), ("Content-Length", "10")]), b"0123456789".to_vec());
        let (status, headers, body) = part_of(answer(), "bytes=2-5");
        assert_eq!((status, body.as_slice()), (206, &b"2345"[..]));
        assert_eq!((header(&headers, "content-range"), header(&headers, "content-length"), header(&headers, "content-type")), (Some("bytes 2-5/10"), Some("4"), Some("video/mp4")));
        assert_eq!(part_of(answer(), "bytes=7-").2, b"789");
        assert_eq!(part_of(answer(), "bytes=-3").2, b"789");
        assert_eq!(part_of(answer(), "bytes=4-99").2, b"456789");
        // What asks for nothing the file has, or for several parts, gets the file.
        assert_eq!(part_of(answer(), "bytes=20-30"), answer());
        assert_eq!(part_of(answer(), "bytes=0-1,4-5"), answer());
        assert!(whole(200, &[], 10));
        assert!(whole(206, &pairs(&[("Content-Range", "bytes 0-9/10")]), 10));
        assert!(!whole(206, &pairs(&[("Content-Range", "bytes 5-9/10")]), 5));
        assert!(!whole(404, &[], 10));
    }

    #[test]
    fn a_copy_gets_a_file_of_its_own_and_an_unfinished_site_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let start = url("https://example.com/docs/");
        let name = |file: &Path| file.file_name().unwrap().to_string_lossy().into_owned();
        let (first, state) = copy_file(dir.path(), &start, true);
        assert_eq!((name(&first).as_str(), state.start.as_str()), ("example.com docs.warc.gz", ""));
        // A finished copy is kept.
        std::fs::write(&first, b"").unwrap();
        assert_eq!(name(&copy_file(dir.path(), &start, true).0), "example.com docs (1).warc.gz");
        // An unfinished site goes on; a page's copy never does.
        save_state(&first, &State { start: start.to_string(), pages: 3, ..State::default() }).unwrap();
        let (again, state) = copy_file(dir.path(), &start, true);
        assert_eq!((again, state.pages), (first.clone(), 3));
        assert_eq!(name(&copy_file(dir.path(), &start, false).0), "example.com docs (1).warc.gz");
    }

    #[test]
    fn a_copy_answers_what_it_was_asked_and_what_differs_only_by_a_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("site.warc.gz");
        let mut store = Store { writer: Some(warc::Writer::open(&file).unwrap()), known: HashMap::new(), answers: 0, bytes: 0, left: 0, max_answer: 1 << 20, max_bytes: 40, full: false, error: None };
        let plain = || vec![("Content-Type".to_string(), "text/plain".to_string()), ("Content-Encoding".to_string(), "br".to_string()), ("Content-Length".to_string(), "3".to_string())];
        for (method, link, sent, body) in [
            ("GET", "https://a.example/", "", "<h1>home</h1>"),
            ("GET", "https://a.example/app.js?v=3", "", "run()"),
            ("GET", "https://a.example/api/items?page=2&_=1700000000000", "", "[1]"),
            ("GET", "https://a.example/api/feed", "", "first"),
            // The same answer again is kept once.
            ("GET", "https://a.example/api/feed", "", "first"),
            ("GET", "https://a.example/api/feed", "", "second"),
            ("POST", "https://a.example/api/find", "q=cats", "cats"),
            ("POST", "https://a.example/api/find", "q=dogs", "dogs"),
        ] {
            store.keep(method, &url(link), &[], sent.as_bytes(), 200, plain(), body.as_bytes());
        }
        assert_eq!((store.answers, store.bytes, store.full, &store.error), (7, 40, false, &None));
        // The run's size is kept to: one answer more is left out.
        store.keep("GET", &url("https://a.example/more"), &[], b"", 200, plain(), b"x");
        assert_eq!((store.answers, store.left, store.full), (7, 1, true));
        store.writer.take().unwrap().finish().unwrap();

        let mut copy = Copy::open(&file).unwrap();
        assert_eq!((copy.start(), copy.len()), ("https://a.example/", 7));
        let mut body = |method: &str, link: &str, sent: &str| copy.find(method, link, sent.as_bytes(), false).map(|n| String::from_utf8(copy.read(n).unwrap().2).unwrap());
        let text = |text: &str| Some(text.to_string());
        assert_eq!(body("GET", "https://a.example/#top", ""), text("<h1>home</h1>"));
        // A stamp in the link is passed over; another page of the same list is another answer.
        assert_eq!(body("GET", "https://a.example/api/items?page=2&_=1800000000000", ""), text("[1]"));
        assert_eq!(body("GET", "https://a.example/api/items?page=3&_=1800000000000", ""), None);
        // A script is the script, whatever follows its name.
        assert_eq!(body("GET", "https://a.example/app.js?v=4", ""), text("run()"));
        // What answered differently each time does so again, in that order, then stays.
        let feed: Vec<_> = (0..3).map(|_| body("GET", "https://a.example/api/feed", "").unwrap()).collect();
        assert_eq!(feed, ["first", "second", "second"]);
        // A request that sent something gets the answer to just that; one never sent, the first.
        assert_eq!(body("POST", "https://a.example/api/find", "q=dogs"), text("dogs"));
        assert_eq!(body("POST", "https://a.example/api/find", "q=cats"), text("cats"));
        assert_eq!(body("POST", "https://a.example/api/find", "q=birds"), text("cats"));
        assert_eq!(body("GET", "https://a.example/api/find", ""), None);
        // The body is kept as the browser had it, decoded: its headers say so.
        let n = copy.find("GET", "https://a.example/", b"", true).unwrap();
        let (status, headers, _) = copy.read(n).unwrap();
        assert_eq!((status, header(&headers, "content-length"), header(&headers, "content-encoding")), (200, Some("13"), None));
        // A file that is no copy says so.
        std::fs::write(dir.path().join("empty.warc.gz"), b"").unwrap();
        assert!(Copy::open(&dir.path().join("empty.warc.gz")).err().unwrap().contains("holds no page"));
    }

    const HOME: &str = r#"<!doctype html><html><head><title>Shop</title><link rel="stylesheet" href="/style.css"></head><body>
<h1 id="head">Loading</h1>
<a href="/about.html">About</a> <a href="/files/tool.zip">Tool</a> <a href="http://elsewhere.invalid/">Elsewhere</a> <a href="/#top">Top</a>
<div style="height: 6000px"></div>
<img loading="lazy" src="/far.png" width="10" height="10">
<img data-src="/later.png" width="10" height="10">
<script src="/app.js?v=3"></script></body></html>"#;
    const APP: &str = "fetch('/api/items?_=' + Date.now()).then(r => r.json()).then(items => { document.getElementById('head').textContent = items.join(','); });\n\
        fetch('/api/save', {method: 'POST', body: 'a=1'}).then(r => r.text()).then(text => { document.body.dataset.saved = text; });";

    /// Whether `script` gives `want` in the copy's first page within fifteen seconds.
    async fn shows(player: &Player, script: &str, want: Value) -> bool {
        for _ in 0..150 {
            if player.eval(script).await.is_ok_and(|got| got == want) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// The whole way with the browser on this computer: a small site on a server here is
    /// recorded, then played with no network. Run by hand (`-- --ignored`): it needs Edge or
    /// Chrome, which a build server need not have.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts the browser on this computer"]
    async fn a_site_is_recorded_with_the_browser_and_played_without_a_network() {
        use crate::hls::tests::{serve, Reply};
        let reply = |kind: &str, body: &[u8]| -> Reply { (200, format!("Content-Type: {kind}\r\n"), body.to_vec()) };
        let (addr, hits) = serve(move |path, _| match path.split('?').next().unwrap_or(path) {
            "/" => reply("text/html; charset=utf-8", HOME.as_bytes()),
            "/about.html" => reply("text/html", b"<title>About</title><p id=\"about\">About us</p><a href=\"/\">Home</a>"),
            "/style.css" => reply("text/css", b"h1 { color: rgb(1, 2, 3); }"),
            "/app.js" => reply("text/javascript", APP.as_bytes()),
            "/api/items" => reply("application/json", b"[\"a\",\"b\"]"),
            "/api/save" => reply("text/plain", b"saved"),
            "/far.png" | "/later.png" => reply("image/png", b"PNG"),
            _ => (404, "Content-Type: text/plain\r\n".to_string(), b"no".to_vec()),
        })
        .await;
        let work = async {
            let dir = tempfile::tempdir().unwrap();
            let start = url(&format!("http://{addr}/"));
            let options = CopyOptions { whole_site: true, depth: 1, delay: Duration::ZERO, ..CopyOptions::default() };
            let mut told = 0;
            let done = record(&reqwest::Client::new(), &start, dir.path(), &options, |_| told += 1).await.unwrap();
            assert_eq!((done.pages, done.title.as_str(), told, &done.stopped, &done.failed), (2, "Shop", 2, &None, &Vec::new()));
            assert!(!state_path(&done.file).exists(), "a site that is done leaves no state");
            let seen = hits.lock().clone();
            for asked in ["/about.html", "/style.css", "/app.js?v=3", "/far.png", "/later.png"] {
                assert!(seen.contains_key(asked), "{asked} was asked for: {seen:?}");
            }
            assert!(!seen.contains_key("/files/tool.zip"), "a file is no page to visit: {seen:?}");

            let player = Player::open(&done.file, true).await.unwrap();
            let text = |text: &str| Value::String(text.to_string());
            // The page's script ran: it asked for its data (under another stamp) and sent its form.
            assert!(shows(&player, "document.getElementById('head').textContent", text("a,b")).await);
            assert!(shows(&player, "document.body.dataset.saved", text("saved")).await);
            assert!(shows(&player, "getComputedStyle(document.getElementById('head')).color", text("rgb(1, 2, 3)")).await);
            assert!(shows(&player, "location.href", text(start.as_str())).await, "the page keeps its own address");
            assert!(shows(&player, "fetch('/nope').then(r => r.status)", Value::from(404)).await);
            // Its links lead on within the copy; one the copy does not hold says so.
            let _ = player.eval("location.href = '/about.html'").await;
            assert!(shows(&player, "document.getElementById('about') ? document.getElementById('about').textContent : ''", text("About us")).await);
            let _ = player.eval("location.href = '/missing.html'").await;
            assert!(shows(&player, "document.title", text("Not in this copy")).await);
            assert_eq!(*hits.lock(), seen, "the copy plays without asking the site for anything");
            player.shut().await;
        };
        tokio::time::timeout(Duration::from_secs(180), work).await.expect("done in three minutes");
        // Each browser's profile went with it.
        let mine = format!("endo-browser-{}-", std::process::id());
        let left: Vec<_> = std::fs::read_dir(std::env::temp_dir()).unwrap().flatten().filter(|entry| entry.file_name().to_string_lossy().starts_with(&mine)).collect();
        assert!(left.is_empty(), "{left:?}");
    }
}
