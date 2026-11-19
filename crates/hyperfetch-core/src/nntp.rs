//! Usenet downloads from an NZB: each file a `.nzb` lists (a local file, one dropped, or a link to
//! one) is a download of its own, its articles fetched over NNTP (RFC 3977, with `AUTHINFO
//! USER/PASS` of RFC 4643) over TLS or plain from the user's own servers (see [`Server`]): there is
//! no Usenet without a provider's account.
//!
//! The first article of a file names it and gives its size (its yEnc header). Then every server's
//! connections ask for the articles left (`BODY <message-id>`, a few at once on each connection),
//! decode them (yEnc), check each part's CRC-32, and write it where its header says it goes in
//! the `.part` file, as the HTTP workers write their ranges out of order. An article a server lacks
//! (430) or holds damaged goes to the next server by priority; one no server gives fails the
//! download, which keeps what it has: which articles are done is saved beside the `.part`, and the
//! next run asks only for the rest. A file is done only when its parts cover it whole, and the
//! whole file's CRC-32 is checked when a header gives it. A server's connections are counted
//! across every download (see [`slots`]).
//!
//! A file is named by the NZB's subject, else by its yEnc header; when a PAR2 set is in its folder
//! (`.par2`), the file is checked against it and given the name it has there when it was posted
//! under another (an obfuscated post). PAR2 sets are checked, not repaired: a damaged file fails,
//! saying how many blocks a PAR2 tool would have to repair, and stays a `.part`.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use md5::{Digest, Md5};
use parking_lot::Mutex;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::sync::{broadcast, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{DownloadOptions, EngineSnapshot, TargetClaim, CANCELLED};
use crate::ingest::{clean_path, Task};
use crate::storage::DiskWriter;
use crate::worker::RateLimiter;

const SCHEME: &str = "nzb";
/// Connections a server gets unless the user says.
pub const DEFAULT_CONNECTIONS: usize = 8;
pub const MAX_CONNECTIONS: usize = 100;
/// Articles asked for at once on one connection.
const PIPELINE: usize = 4;
/// Longest line read as one.
const MAX_LINE: u64 = 16 * 1024;
/// Largest article read (a yEnc part is usually under 1 MB).
const MAX_ARTICLE: usize = 16 << 20;
/// Most articles of all an NZB's files.
const MAX_SEGMENTS: usize = 2_000_000;
/// Segments of a file tried for its first part (its name and size) before giving up.
const FIRST_TRIES: usize = 5;
/// Articles sampled (STAT) across a file before it downloads, to tell an expired post at once.
const SAMPLE: usize = 12;
const SHOW_EVERY: Duration = Duration::from_millis(250);
const SAVE_EVERY: Duration = Duration::from_secs(2);
/// How often a backup server's idle connection looks for articles the servers before it lacked.
const WAIT: Duration = Duration::from_millis(50);
pub const NO_SERVER: &str = "An NZB downloads from a Usenet server of your own provider's account: add one (host, user name, password) in the settings or with --nntp-server";
/// Why a server answers 480 to an article: it is asked without a user name.
const SIGN_IN: &str = "asks to sign in: add the user name and password of your account to this server";

/// A Usenet server of the user's own account (see `DownloadOptions::nntp`).
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Server {
    pub host: String,
    /// 0 for the protocol's own: 563 over TLS, 119 plain.
    pub port: u16,
    pub tls: bool,
    pub user: String,
    pub password: String,
    /// Connections open to it at once, across every download (see [`slots`]): the provider's
    /// limit is the user's to set.
    pub connections: usize,
    /// Servers are asked lowest first; a later one only for the articles those before it lack.
    pub priority: u32,
}

impl Default for Server {
    fn default() -> Self {
        Self { host: String::new(), port: 0, tls: true, user: String::new(), password: String::new(), connections: DEFAULT_CONNECTIONS, priority: 0 }
    }
}

/// Never shows the password.
impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls)
            .field("user", &self.user)
            .field("connections", &self.connections)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

impl Server {
    fn port(&self) -> u16 {
        match (self.port, self.tls) {
            (0, true) => 563,
            (0, false) => 119,
            (port, _) => port,
        }
    }

    /// The server a link names, asked at `priority`: `nntps://[user[:password]@]host[:port]`
    /// (`nntp://` for plain, without TLS), `?connections=N` for other than [`DEFAULT_CONNECTIONS`].
    pub fn from_link(link: &str, priority: u32) -> Result<Self, String> {
        let url = Url::parse(link.trim()).map_err(|e| format!("not a Usenet server link ({e})"))?;
        let tls = match url.scheme() {
            "nntps" | "snews" => true,
            "nntp" | "news" => false,
            other => return Err(format!("{other}: a Usenet server's link starts with nntps:// (or nntp:// without TLS)")),
        };
        let host = url.host_str().filter(|h| !h.is_empty()).ok_or("the Usenet server link names no host")?.to_string();
        let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
        let connections = match url.query_pairs().find(|(k, _)| k == "connections") {
            Some((_, n)) => n.parse().ok().filter(|n| (1..=MAX_CONNECTIONS).contains(n)).ok_or(format!("connections must be 1 to {MAX_CONNECTIONS}"))?,
            None => DEFAULT_CONNECTIONS,
        };
        let server = Self { host, port: url.port().unwrap_or(0), tls, user: decode(url.username()), password: url.password().map(decode).unwrap_or_default(), connections, priority };
        server.check()?;
        Ok(server)
    }

    /// Refuses a user name or password that would end the command it is sent in.
    fn check(&self) -> Result<(), String> {
        if [&self.host, &self.user, &self.password].iter().any(|s| s.chars().any(char::is_control)) {
            return Err(format!("{}: a Usenet server's host, user name and password hold no line breaks", self.host));
        }
        Ok(())
    }
}

/// An NZB: its title and files.
#[derive(Debug, Default, PartialEq)]
struct Nzb {
    title: Option<String>,
    files: Vec<NzbFile>,
}

#[derive(Debug, Default, PartialEq)]
struct NzbFile {
    subject: String,
    /// In order, each number once.
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq)]
struct Segment {
    number: u32,
    /// The message-id, without its angle brackets.
    id: String,
}

/// The NZB `text` is (an XML document of `<file subject=...>` with `<segment number=...>` ids).
fn parse_nzb(text: &str) -> Result<Nzb, String> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let (mut nzb, mut file, mut segment, mut meta, mut root) = (Nzb::default(), None::<NzbFile>, None::<u32>, None::<String>, false);
    let (mut count, mut buf) = (0, Vec::new());
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let attr = |wanted: &[u8]| {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == wanted)
                        .map(|a| a.unescape_value().map(|v| v.into_owned()).unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned()))
                };
                match e.local_name().as_ref() {
                    b"nzb" => root = true,
                    b"file" => file = Some(NzbFile { subject: attr(b"subject").unwrap_or_default(), segments: Vec::new() }),
                    b"segment" => segment = attr(b"number").and_then(|n| n.trim().parse().ok()),
                    b"meta" => meta = attr(b"type"),
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().map(|t| t.trim().to_string()).unwrap_or_default();
                if let (Some(file), Some(number)) = (file.as_mut(), segment.take()) {
                    let id = text.trim_start_matches('<').trim_end_matches('>');
                    // An id that would end the command it is sent in is no id.
                    if !id.is_empty() && !id.contains(|c: char| c.is_whitespace() || c.is_control() || c == '<' || c == '>') {
                        file.segments.push(Segment { number, id: id.to_string() });
                        count += 1;
                    }
                } else if meta.take().is_some_and(|kind| kind == "title") && !text.is_empty() {
                    nzb.title = Some(text);
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"file" => {
                    if let Some(mut done) = file.take() {
                        done.segments.sort_by_key(|s| s.number);
                        done.segments.dedup_by_key(|s| s.number);
                        if !done.segments.is_empty() {
                            nzb.files.push(done);
                        }
                    }
                }
                b"segment" => segment = None,
                b"meta" => meta = None,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("the NZB cannot be read: {e}")),
            _ => {}
        }
        if count > MAX_SEGMENTS {
            return Err(format!("the NZB lists more than {MAX_SEGMENTS} articles"));
        }
        buf.clear();
    }
    if !root || nzb.files.is_empty() {
        return Err("the NZB lists no files".to_string());
    }
    Ok(nzb)
}

/// The file name a post's subject gives: the quoted one (`[1/9] - "name.rar" yEnc (1/50)`), else
/// the last word with a dot before its `yEnc` and `(1/50)`. None when it has no extension (an
/// obfuscated post's random name), so the yEnc header names it.
fn subject_name(subject: &str) -> Option<String> {
    let quoted = subject.split('"').nth(1).filter(|s| subject.matches('"').count() >= 2 && !s.trim().is_empty());
    let name = quoted.map(str::to_string).or_else(|| {
        let head = subject.split(" yEnc").next().unwrap_or(subject).trim_end();
        let head = head.strip_suffix(')').and_then(|h| h.rsplit_once(" (")).map_or(head, |(h, _)| h);
        head.split_whitespace().next_back().map(str::to_string)
    })?;
    let name = name.trim();
    name.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && (1..=10).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| name.to_string())
}

/// Whether `url` is a file of an NZB (see [`tasks`]).
pub(crate) fn handles(url: &Url) -> bool {
    url.scheme() == SCHEME
}

fn parse_bytes(bytes: &[u8]) -> Result<Nzb, String> {
    parse_nzb(&crate::ingest::decode_text(bytes).unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned()))
}

/// Whether `bytes` read as an NZB.
pub(crate) fn is_nzb(bytes: &[u8]) -> bool {
    parse_bytes(bytes).is_ok()
}

/// One download per file of the NZB `bytes`, found at `source` (its link, or a local one's `file:`
/// URL), in a folder named after its title (else after the NZB). The NZB is kept (see [`keep`])
/// for the downloads to read.
pub(crate) fn tasks(bytes: &[u8], source: &Url) -> Result<Vec<Task>, String> {
    parse_bytes(bytes)?;
    tasks_from(bytes, source, &keep(bytes)?)
}

/// [`tasks`], each `nzb:<kept>#<n>`: `kept` the `file:` URL the download reads the NZB from.
fn tasks_from(bytes: &[u8], source: &Url, kept: &Url) -> Result<Vec<Task>, String> {
    let nzb = parse_bytes(bytes)?;
    let stem = source.path_segments().and_then(|mut s| s.next_back()).map(|last| percent_encoding::percent_decode_str(last).decode_utf8_lossy().into_owned());
    let stem = stem.as_deref().map(|last| last.rsplit_once('.').map_or(last, |(stem, _)| stem).to_string());
    let folder = nzb.title.iter().chain(&stem).find_map(|name| clean_path([crate::ingest::truncate_chars(name.trim(), 100).as_str()]).ok());
    nzb.files
        .iter()
        .enumerate()
        .map(|(n, file)| {
            Ok(Task {
                urls: vec![Url::parse(&format!("{SCHEME}:{kept}#{n}")).map_err(|e| format!("the NZB's link cannot be kept: {e}"))?],
                name: subject_name(&file.subject).and_then(|name| clean_path([name.as_str()]).ok()),
                folder: folder.clone(),
                // Its articles come from the user's Usenet servers, none of the hosts the user typed.
                from_document: true,
                ..Task::default()
            })
        })
        .collect()
}

/// The NZB link `link` names and which of its files.
fn link_parts(link: &Url) -> Result<(Url, usize), String> {
    let broken = || format!("{link} names no file of an NZB");
    let (source, n) = link.as_str().strip_prefix("nzb:").and_then(|rest| rest.rsplit_once('#')).ok_or_else(broken)?;
    Ok((Url::parse(source).map_err(|_| broken())?, n.parse().map_err(|_| broken())?))
}

/// Keeps the NZB `bytes` in the app's data folder, once, for its files' downloads to read later:
/// nothing is fetched again (an indexer counts every fetch, and its link may carry the account's
/// key), and a local NZB moved or deleted meanwhile is no loss. Its `file:` URL.
pub(crate) fn keep(bytes: &[u8]) -> Result<Url, String> {
    let dir = crate::media::app_data_dir().ok_or("the app has no data folder to keep the NZB in")?.join("nzb");
    keep_in(bytes, &dir)
}

/// [`keep`] into `dir`, the copy named by its SHA-256.
fn keep_in(bytes: &[u8], dir: &Path) -> Result<Url, String> {
    let path = dir.join(format!("{:x}.nzb", sha2::Sha256::digest(bytes)));
    if !path.is_file() {
        let fail = |e: std::io::Error| format!("Cannot keep the NZB in {}: {e}", dir.display());
        std::fs::create_dir_all(dir).map_err(fail)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &path)).map_err(fail)?;
    }
    Url::from_file_path(&path).map_err(|()| format!("{} is no usable path", path.display()))
}

/// The NZB kept at `source` (see [`keep`]).
async fn read_source(source: &Url) -> Result<Vec<u8>, String> {
    let path = source.to_file_path().map_err(|()| format!("{source} is no NZB the app kept: add the NZB again"))?;
    crate::ingest::read_local(&path).await
}

/// A decoded yEnc part: the file's name, size and count of parts from its header, where the part
/// goes in it, its bytes, and the whole file's CRC-32 when the article gives it.
#[derive(Debug, PartialEq)]
struct Part {
    name: String,
    size: u64,
    total: Option<u64>,
    offset: u64,
    data: Vec<u8>,
    file_crc: Option<u32>,
}

/// The value of `key` among the `key=value` words of a yEnc header line before its `name=`.
fn key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let keys = match line.find("name=") {
        Some(at) if at == 0 || line[..at].ends_with(' ') => &line[..at],
        _ => line,
    };
    keys.split_whitespace().find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

fn key_u64(line: &str, name: &str) -> Option<u64> {
    key(line, name)?.parse().ok()
}

fn key_crc(line: &str, name: &str) -> Option<u32> {
    u32::from_str_radix(key(line, name)?, 16).ok()
}

/// The yEnc part an article's `body` holds (its lines, CRLF ends kept), checked against the sizes
/// and CRC-32 its trailer gives.
fn decode(body: &[u8]) -> Result<Part, String> {
    let (mut head, mut range, mut tail) = (None::<String>, None::<(u64, u64)>, None::<String>);
    let (mut data, mut escaped) = (Vec::with_capacity(body.len()), false);
    for line in body.split(|&b| b == b'\n') {
        if head.is_none() {
            if let Some(rest) = line.strip_prefix(b"=ybegin ") {
                head = Some(String::from_utf8_lossy(rest).trim_end().to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"=ypart ") {
            let rest = String::from_utf8_lossy(rest);
            range = key_u64(&rest, "begin").zip(key_u64(&rest, "end"));
            if range.is_none() {
                return Err("a yEnc part names no place in its file".to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"=yend") {
            tail = Some(String::from_utf8_lossy(rest).into_owned());
            break;
        }
        for &b in line {
            match b {
                b'\r' => {}
                b'=' if !escaped => escaped = true,
                _ => {
                    data.push(if escaped { b.wrapping_sub(106) } else { b.wrapping_sub(42) });
                    escaped = false;
                }
            }
        }
    }
    let head = head.ok_or("the article is no yEnc part")?;
    let tail = tail.ok_or("the yEnc part is cut short")?;
    let size = key_u64(&head, "size").ok_or("the yEnc part does not give its file's size")?;
    let name = match head.find("name=") {
        Some(at) if at == 0 || head[..at].ends_with(' ') => head[at + 5..].trim().to_string(),
        _ => String::new(),
    };
    let (begin, end) = range.unwrap_or((1, size));
    let len = data.len() as u64;
    if begin == 0 || end < begin || end > size || end - begin + 1 != len || key_u64(&tail, "size").is_some_and(|s| s != len) {
        return Err(format!("the yEnc part holds {len} bytes, not those of {begin}-{end} of {size}"));
    }
    let crc = crc32fast::hash(&data);
    let part_crc = key_crc(&tail, "pcrc32").or_else(|| range.is_none().then(|| key_crc(&tail, "crc32")).flatten());
    if part_crc.is_some_and(|expected| expected != crc) {
        return Err("the yEnc part fails its CRC-32 check".to_string());
    }
    let file_crc = if range.is_none() { Some(crc) } else { key_crc(&tail, "crc32") };
    Ok(Part { name, size, total: key_u64(&head, "total"), offset: begin - 1, data, file_crc })
}

/// Why a server did not give what it was asked.
#[derive(Debug)]
enum Failure {
    /// It takes no more connections now (400 or 502).
    Busy(String),
    /// It refused the account: asking again is no use.
    Refused(String),
    /// The connection failed or broke: another try may do.
    Broken(String),
}

impl Failure {
    fn text(self) -> String {
        match self {
            Self::Busy(why) | Self::Refused(why) | Self::Broken(why) => why,
        }
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// The certificates of the public CAs, as the app's HTTPS checks them.
fn tls_connector() -> Result<tokio_rustls::TlsConnector, String> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

/// The connections each server (host, port and user) may have open, shared by every download: a
/// provider's limit is the account's, not a download's.
fn slots(server: &Server) -> Arc<Semaphore> {
    static SLOTS: LazyLock<Mutex<HashMap<String, (usize, Arc<Semaphore>)>>> = LazyLock::new(Mutex::default);
    let size = server.connections.clamp(1, MAX_CONNECTIONS);
    let key = format!("{}:{}/{}", server.host.to_ascii_lowercase(), server.port(), server.user);
    let mut slots = SLOTS.lock();
    let slot = slots.entry(key).or_insert_with(|| (size, Arc::new(Semaphore::new(size))));
    // shortcut: a changed setting starts a fresh count, so connections opened under the old one go uncounted until they close.
    if slot.0 != size {
        *slot = (size, Arc::new(Semaphore::new(size)));
    }
    Arc::clone(&slot.1)
}

/// A connection to a server, signed in, holding one of its [`slots`].
struct Conn {
    io: BufStream<Box<dyn Stream>>,
    stall: Duration,
    _slot: OwnedSemaphorePermit,
}

impl Conn {
    /// Waits for a free slot of `server`, then connects and signs in.
    async fn open(server: &Server, stall: Duration) -> Result<Self, Failure> {
        server.check().map_err(Failure::Refused)?;
        let slot = slots(server).acquire_owned().await.map_err(|e| Failure::Broken(e.to_string()))?;
        let host = &server.host;
        let broken = |why: String| Failure::Broken(format!("{host}: {why}"));
        let connect = tokio::net::TcpStream::connect((host.as_str(), server.port()));
        let tcp = tokio::time::timeout(stall, connect).await.map_err(|_| broken("no answer".into()))?.map_err(|e| broken(e.to_string()))?;
        let _ = tcp.set_nodelay(true);
        let io: Box<dyn Stream> = if server.tls {
            let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|_| Failure::Refused(format!("{host} is no host name TLS can check")))?;
            let tls = tokio::time::timeout(stall, tls_connector().map_err(Failure::Refused)?.connect(name, tcp)).await;
            Box::new(tls.map_err(|_| broken("no TLS answer".into()))?.map_err(|e| broken(format!("TLS: {e}")))?)
        } else {
            Box::new(tcp)
        };
        let mut conn = Self { io: BufStream::new(io), stall, _slot: slot };
        match conn.reply().await.map_err(broken)? {
            (200 | 201, _) => {}
            (400 | 502, text) => return Err(Failure::Busy(format!("{host}: {text}"))),
            (code, text) => return Err(broken(format!("greeted with {code} {text}"))),
        }
        if server.user.is_empty() {
            return Ok(conn);
        }
        let code = match conn.ask(&format!("AUTHINFO USER {}", server.user)).await.map_err(broken)? {
            (381, _) => conn.ask(&format!("AUTHINFO PASS {}", server.password)).await.map_err(broken)?.0,
            (code, _) => code,
        };
        match code {
            281 => Ok(conn),
            481 | 482 | 452 => Err(Failure::Refused(format!("{host} refused the user name or password"))),
            400 | 502 => Err(Failure::Busy(format!("{host} takes no more connections of the account"))),
            code => Err(broken(format!("answered {code} to the sign-in"))),
        }
    }

    async fn ask(&mut self, line: &str) -> Result<(u16, String), String> {
        self.io.write_all(format!("{line}\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
        self.io.flush().await.map_err(|e| e.to_string())?;
        self.reply().await
    }

    async fn line(&mut self, into: &mut Vec<u8>) -> Result<(), String> {
        into.clear();
        let mut limited = (&mut self.io).take(MAX_LINE);
        match tokio::time::timeout(self.stall, limited.read_until(b'\n', into)).await {
            Err(_) => Err(format!("nothing came for {} s", self.stall.as_secs())),
            Ok(Err(e)) => Err(e.to_string()),
            Ok(Ok(0)) => Err("the server closed the connection".to_string()),
            Ok(Ok(_)) => Ok(()),
        }
    }

    async fn reply(&mut self) -> Result<(u16, String), String> {
        let mut line = Vec::new();
        self.line(&mut line).await?;
        let text = String::from_utf8_lossy(&line).trim().to_string();
        let code = text.get(..3).and_then(|c| c.parse().ok()).ok_or_else(|| format!("the server answered {:?}", crate::ingest::truncate_chars(&text, 60)))?;
        Ok((code, text.get(4..).unwrap_or_default().to_string()))
    }

    /// A multi-line answer's lines, up to the lone dot, their dot-stuffing undone, CRLF kept.
    async fn body(&mut self) -> Result<Vec<u8>, String> {
        let (mut body, mut line) = (Vec::new(), Vec::new());
        loop {
            self.line(&mut line).await?;
            if line.strip_suffix(b"\n").map(|l| l.strip_suffix(b"\r").unwrap_or(l)) == Some(&b"."[..]) {
                return Ok(body);
            }
            body.extend_from_slice(if line.starts_with(b"..") { &line[1..] } else { &line[..] });
            if body.len() > MAX_ARTICLE {
                return Err("an article larger than the app reads".to_string());
            }
        }
    }

    /// Asks for every article of `ids` at once (`BODY`), then reads the answers in turn: each the
    /// part, or why not (the server lacks it: 430; it is damaged). The error that broke the
    /// connection, if one did ([`SIGN_IN`] when it asks to sign in); the answers after it are
    /// unknown.
    async fn bodies(&mut self, ids: &[&str]) -> (Vec<Result<Part, String>>, Option<String>) {
        let mut found = Vec::new();
        let asked: String = ids.iter().map(|id| format!("BODY <{id}>\r\n")).collect();
        if let Err(e) = async { self.io.write_all(asked.as_bytes()).await?; self.io.flush().await }.await {
            return (found, Some(e.to_string()));
        }
        for _ in ids {
            let answer = match self.reply().await {
                Ok((222, _)) => match self.body().await {
                    Ok(body) => decode(&body),
                    Err(e) => return (found, Some(e)),
                },
                Ok((code @ (430 | 423), _)) => Err(format!("no such article ({code})")),
                Ok((480, _)) => return (found, Some(SIGN_IN.to_string())),
                Ok((code, text)) => return (found, Some(format!("answered {code} {text}"))),
                Err(e) => return (found, Some(e)),
            };
            found.push(answer);
        }
        (found, None)
    }

    /// Which of `ids` the server holds (`STAT`, all asked at once); None when it could not tell.
    async fn holds(&mut self, ids: &[&str]) -> Option<Vec<bool>> {
        let asked: String = ids.iter().map(|id| format!("STAT <{id}>\r\n")).collect();
        self.io.write_all(asked.as_bytes()).await.ok()?;
        self.io.flush().await.ok()?;
        let mut held = Vec::new();
        for _ in ids {
            held.push(match self.reply().await.ok()?.0 {
                223 => true,
                430 | 423 => false,
                _ => return None,
            });
        }
        Some(held)
    }

    async fn quit(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.ask("QUIT")).await;
    }
}

/// What a file's download has done, saved beside its `.part` to resume it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    /// The first article's message-id: the state of another file is not taken.
    first: String,
    size: u64,
    /// One bit per segment, base64.
    done: String,
    /// The bytes of the file those segments wrote (see [`cover`]).
    spans: Vec<(u64, u64)>,
    file_crc: Option<u32>,
}

/// Adds the bytes `start..end` to `spans`, the runs of bytes written (in order, apart), merging
/// the runs it meets.
fn cover(spans: &mut Vec<(u64, u64)>, start: u64, end: u64) {
    let at = spans.partition_point(|run| run.1 < start);
    let (mut start, mut end, mut stop) = (start, end, at);
    while stop < spans.len() && spans[stop].0 <= end {
        (start, end) = (start.min(spans[stop].0), end.max(spans[stop].1));
        stop += 1;
    }
    spans.splice(at..stop, [(start, end)]);
}

fn covered(spans: &[(u64, u64)]) -> u64 {
    spans.iter().map(|(start, end)| end - start).sum()
}

fn state_path(target: &Path) -> PathBuf {
    let mut path = crate::engine::part_path(target).into_os_string();
    path.push(".nntp");
    PathBuf::from(path)
}

fn bits(done: &[bool]) -> String {
    let mut bytes = vec![0u8; done.len().div_ceil(8)];
    for (n, _) in done.iter().enumerate().filter(|(_, d)| **d) {
        bytes[n / 8] |= 1 << (n % 8);
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// What a download has done: the file's size, the segments done, the bytes they wrote (see
/// [`cover`]) and the whole file's CRC-32 when an article gave it.
type State = (u64, Vec<bool>, Vec<(u64, u64)>, Option<u32>);

/// The state of `file`'s download into `target`, when its `.part` is still there.
fn load_state(target: &Path, file: &NzbFile) -> Option<State> {
    if !crate::engine::part_path(target).is_file() {
        return None;
    }
    let saved: Saved = serde_json::from_slice(&std::fs::read(state_path(target)).ok()?).ok()?;
    if saved.first != file.segments.first()?.id || saved.size == 0 || saved.spans.iter().any(|&(start, end)| start >= end || end > saved.size) {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(saved.done).ok()?;
    let done = (0..file.segments.len()).map(|n| bytes.get(n / 8).is_some_and(|b| b & (1 << (n % 8)) != 0)).collect();
    Some((saved.size, done, saved.spans, saved.file_crc))
}

/// What the workers share: the segments, which server's turn each is, and the file.
struct Job {
    segments: Vec<Segment>,
    servers: Vec<Server>,
    board: Mutex<Board>,
    writer: DiskWriter,
    limiter: Option<Arc<RateLimiter>>,
    size: u64,
    /// Bytes received this run, for the speed.
    received: AtomicU64,
    stall: Duration,
    max_retries: u32,
}

struct Board {
    /// The segments each server is to be asked for next.
    queues: Vec<VecDeque<usize>>,
    /// Segments asked of each server and not yet answered.
    busy: Vec<usize>,
    /// Workers of each server still running.
    alive: Vec<usize>,
    /// Why a server is no longer asked.
    out: Vec<Option<String>>,
    /// Broken connections a segment met on its server.
    tries: HashMap<usize, u32>,
    done: Vec<bool>,
    /// The bytes of the file the done segments wrote (see [`cover`]).
    spans: Vec<(u64, u64)>,
    /// Segments no server gave.
    missing: Vec<usize>,
    file_crc: Option<u32>,
    /// A failure that ends the download (the disk's).
    failed: Option<String>,
}

enum Take {
    Batch(Vec<usize>),
    /// Servers before this one may yet pass it some.
    Wait,
    Done,
}

impl Job {
    fn take(&self, s: usize) -> Take {
        let mut b = self.board.lock();
        if b.out[s].is_some() || b.failed.is_some() {
            return Take::Done;
        }
        if !b.queues[s].is_empty() {
            let n = PIPELINE.min(b.queues[s].len());
            let batch: Vec<usize> = b.queues[s].drain(..n).collect();
            b.busy[s] += n;
            return Take::Batch(batch);
        }
        if (0..=s).any(|j| !b.queues[j].is_empty() || b.busy[j] > 0) {
            Take::Wait
        } else {
            Take::Done
        }
    }

    /// `seg` goes to the next server still asked, else it is missing.
    fn pass_on(b: &mut Board, s: usize, seg: usize) {
        b.tries.remove(&seg);
        match (s + 1..b.queues.len()).find(|&n| b.out[n].is_none()) {
            Some(next) => b.queues[next].push_back(seg),
            None => b.missing.push(seg),
        }
    }

    /// Server `s` lacks `seg` or holds it damaged.
    fn lacks(&self, s: usize, seg: usize, why: &str) {
        tracing::debug!("{} has no usable article {}: {why}", self.servers[s].host, self.segments[seg].id);
        let mut b = self.board.lock();
        b.busy[s] -= 1;
        Self::pass_on(&mut b, s, seg);
    }

    /// `segs`, asked of `s` and unanswered, go back to its queue; a broken connection counts as
    /// one try of each, and one tried too often goes on to the next server.
    fn give_back(&self, s: usize, segs: &[usize], counts: bool) {
        let mut b = self.board.lock();
        for &seg in segs.iter().rev() {
            b.busy[s] -= 1;
            let tries = b.tries.get(&seg).copied().unwrap_or(0) + counts as u32;
            if tries > self.max_retries || b.out[s].is_some() {
                Self::pass_on(&mut b, s, seg);
            } else {
                b.tries.insert(seg, tries);
                b.queues[s].push_front(seg);
            }
        }
    }

    /// Server `s` is asked no more, for `why`: its segments go on to the next.
    fn server_out(&self, s: usize, why: String) {
        Self::put_out(&mut self.board.lock(), s, why);
    }

    fn put_out(b: &mut Board, s: usize, why: String) {
        tracing::warn!("{why}");
        b.out[s] = Some(why);
        while let Some(seg) = b.queues[s].pop_front() {
            Self::pass_on(b, s, seg);
        }
    }

    /// A worker of server `s` stops; with `if_others`, only while other workers of it are left.
    /// The last one to stop for a failure (`why`) puts the server out, so that the segments left
    /// to it go on instead of waiting for no one.
    fn leave(&self, s: usize, why: Option<String>, if_others: bool) -> bool {
        let mut b = self.board.lock();
        if if_others && b.alive[s] <= 1 {
            return false;
        }
        b.alive[s] -= 1;
        if let Some(why) = why.filter(|_| b.alive[s] == 0 && b.out[s].is_none()) {
            Self::put_out(&mut b, s, why);
        }
        true
    }

    /// Writes `part` where it goes and counts `seg` done.
    async fn store(&self, seg: usize, part: Part) -> Result<(), String> {
        if let Some(limiter) = &self.limiter {
            limiter.acquire(part.data.len() as u64).await;
        }
        let (writer, offset, len, crc) = (self.writer.clone(), part.offset, part.data.len() as u64, part.file_crc);
        let data = part.data;
        tokio::task::spawn_blocking(move || writer.write_chunk_slice(offset, &data))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("Disk write error: {e}"))?;
        self.received.fetch_add(len, Ordering::Relaxed);
        let mut b = self.board.lock();
        b.done[seg] = true;
        cover(&mut b.spans, offset, offset + len);
        b.tries.remove(&seg);
        b.file_crc = b.file_crc.or(crc);
        Ok(())
    }

    fn state(&self) -> Saved {
        let b = self.board.lock();
        Saved { first: self.segments[0].id.clone(), size: self.size, done: bits(&b.done), spans: b.spans.clone(), file_crc: b.file_crc }
    }
}

/// One connection's worth of work for server `s`: batches of its queue, until no server before
/// it can pass it more. It connects only once it has work, so a backup server is asked only for
/// what the others lack.
async fn work(job: Arc<Job>, s: usize, cancel: CancellationToken) {
    let server = &job.servers[s];
    let (mut conn, mut failures, mut left) = (None::<Conn>, 0u32, false);
    loop {
        let batch = match job.take(s) {
            Take::Done => break,
            Take::Wait => {
                // With nothing to ask, the connection's slot goes to a worker that waits for one:
                // kept, two downloads each holding half the slots would wait for each other.
                conn = None;
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(WAIT) => continue,
                }
            }
            Take::Batch(batch) => batch,
        };
        if conn.is_none() {
            let opened = tokio::select! {
                _ = cancel.cancelled() => {
                    job.give_back(s, &batch, false);
                    break;
                }
                opened = Conn::open(server, job.stall) => opened,
            };
            match opened {
                Ok(opened) => conn = Some(opened),
                Err(Failure::Refused(why)) => {
                    job.give_back(s, &batch, false);
                    job.server_out(s, why);
                    break;
                }
                Err(failure) => {
                    let busy = matches!(failure, Failure::Busy(_));
                    let why = failure.text();
                    job.give_back(s, &batch, !busy);
                    failures += 1;
                    // A server that takes fewer connections keeps those it took.
                    if busy && job.leave(s, None, true) {
                        tracing::info!("{why}: one connection fewer");
                        left = true;
                        break;
                    }
                    if failures > job.max_retries {
                        left = job.leave(s, Some(why), false);
                        break;
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(u64::from(failures.min(10)))) => continue,
                    }
                }
            }
        }
        let Some(open) = conn.as_mut() else { continue };
        let ids: Vec<&str> = batch.iter().map(|&n| job.segments[n].id.as_str()).collect();
        let (answers, broke) = tokio::select! {
            _ = cancel.cancelled() => {
                job.give_back(s, &batch, false);
                break;
            }
            got = open.bodies(&ids) => got,
        };
        let answered = answers.len();
        for (&seg, answer) in batch.iter().zip(answers) {
            match answer.and_then(|part| if part.size == job.size { Ok(part) } else { Err("a part of another file".to_string()) }) {
                Ok(part) => {
                    let stored = job.store(seg, part).await;
                    let mut b = job.board.lock();
                    b.busy[s] -= 1;
                    if let Err(e) = stored {
                        b.failed.get_or_insert(e);
                    }
                }
                Err(why) => job.lacks(s, seg, &why),
            }
        }
        match broke {
            Some(why) if why == SIGN_IN => {
                job.give_back(s, &batch[answered..], false);
                job.server_out(s, format!("{} {SIGN_IN}", server.host));
                break;
            }
            Some(why) => {
                tracing::info!("{}: {why}; connecting again", server.host);
                conn = None;
                job.give_back(s, &batch[answered..], true);
                failures += 1;
            }
            None => failures = 0,
        }
    }
    if let Some(open) = conn {
        open.quit().await;
    }
    if !left {
        job.leave(s, None, false);
    }
}

/// The first part a server gives of the first few segments, and its segment: it names the file
/// and gives its size.
async fn first_part(servers: &[Server], segments: &[Segment], stall: Duration) -> Result<(usize, Part), String> {
    let mut why = Vec::new();
    for (at, segment) in segments.iter().enumerate().take(FIRST_TRIES) {
        for server in servers {
            let got = async {
                let mut conn = Conn::open(server, stall).await.map_err(Failure::text)?;
                let (mut answers, broke) = conn.bodies(&[segment.id.as_str()]).await;
                conn.quit().await;
                let host = &server.host;
                answers.pop().unwrap_or_else(|| Err(broke.unwrap_or_default())).map_err(|e| if e == SIGN_IN { format!("{host} {e}") } else { format!("{host}: {e}") })
            };
            match got.await {
                Ok(part) => return Ok((at, part)),
                Err(e) => why.push(e),
            }
        }
    }
    let shown = why.iter().find(|e| e.contains("refused") || e.contains(SIGN_IN)).or(why.first()).cloned().unwrap_or_default();
    Err(format!("No Usenet server gave the file's first articles ({shown})"))
}

/// Of up to [`SAMPLE`] of `pending` segments spread over the file, how many and how many of them
/// no server holds (`STAT`); servers that cannot tell count as holding them.
async fn sample(servers: &[Server], segments: &[Segment], pending: &[usize], stall: Duration) -> (usize, usize) {
    let step = pending.len().div_ceil(SAMPLE).max(1);
    let mut left: Vec<&str> = pending.iter().step_by(step).map(|&n| segments[n].id.as_str()).collect();
    let asked = left.len();
    for server in servers {
        if left.is_empty() {
            break;
        }
        let Ok(mut conn) = Conn::open(server, stall).await else { return (asked, 0) };
        let held = conn.holds(&left).await;
        conn.quit().await;
        let Some(held) = held else { return (asked, 0) };
        left = left.into_iter().zip(held).filter(|(_, held)| !held).map(|(id, _)| id).collect();
    }
    (asked, left.len())
}

/// Downloads the file of an NZB `link` names (see the module docs), from `options.nntp`, into
/// `options.output_path` (else a file in `save_dir` named as its yEnc header says). Whether
/// anything was downloaded, for the engine (see `DownloadEngine::unless_rerun`).
pub(crate) fn run<'a>(
    link: &'a Url,
    options: &'a DownloadOptions,
    limiter: Option<Arc<RateLimiter>>,
    save_dir: &'a Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Pin<Box<dyn Future<Output = Result<(PathBuf, bool), String>> + Send + 'a>> {
    Box::pin(download(link, options, limiter, save_dir, snapshots, cancel))
}

async fn download(
    link: &Url,
    options: &DownloadOptions,
    limiter: Option<Arc<RateLimiter>>,
    save_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let mut servers: Vec<Server> = options.nntp.iter().filter(|s| !s.host.trim().is_empty()).cloned().collect();
    if servers.is_empty() {
        return Err(NO_SERVER.to_string());
    }
    servers.sort_by_key(|s| s.priority);
    let (source, n) = link_parts(link)?;
    let nzb = parse_bytes(&read_source(&source).await?)?;
    let file = nzb.files.into_iter().nth(n).ok_or("the NZB no longer lists this file")?;
    let stall = Duration::from_secs(options.stall_timeout_secs.max(5));
    let named = options.output_path.clone().filter(|p| !crate::engine::is_dir_target(p));

    // A download begun before goes on where it stopped; else the first part names the file.
    let mut first = None;
    let (target, (size, done, spans, file_crc), _claim) = match named.as_ref().and_then(|t| Some((t.clone(), load_state(t, &file)?))) {
        Some((target, state)) => {
            let claim = take_target(&target)?;
            (target, state, claim)
        }
        None => {
            let (at, part) = tokio::select! {
                _ = cancel.cancelled() => return Err(CANCELLED.to_string()),
                got = first_part(&servers, &file.segments, stall) => got?,
            };
            let base = named.unwrap_or_else(|| {
                let name = Some(part.name.as_str()).filter(|n| !n.is_empty()).map(str::to_string).or_else(|| subject_name(&file.subject)).unwrap_or_default();
                // A name of nothing usable (`..`) would be the folder itself.
                let clean = crate::engine::sanitize_component(&name);
                save_dir.join(if clean.is_empty() { "file" } else { clean.as_str() })
            });
            let listed = file.segments.len() as u64;
            if let Some(total) = part.total.filter(|&total| total > listed) {
                return Err(format!(
                    "The NZB lists {listed} of the {total} articles of {}: the file cannot come out whole, and another NZB of the post may list them all",
                    file_label(&base)
                ));
            }
            let (target, state, claim) = match load_state(&base, &file) {
                Some(state) => {
                    let claim = take_target(&base)?;
                    (base, state, claim)
                }
                None => {
                    let (target, claim) = take_free_target(&base)?;
                    (target, (part.size, vec![false; file.segments.len()], Vec::new(), None), claim)
                }
            };
            first = Some((at, part)).filter(|(at, part)| !state.1[*at] && part.size == state.0);
            (target, state, claim)
        }
    };
    // No article holds more than MAX_ARTICLE: a larger size is a broken or hostile header, and
    // its `.part` would take the disk.
    if size > file.segments.len() as u64 * MAX_ARTICLE as u64 {
        return Err(format!("{} claims {size} bytes, more than its {} articles can hold: the post is broken", file_label(&target), file.segments.len()));
    }
    if options.max_filesize.is_some_and(|max| size > max) {
        return Err(format!("{} is larger than the most allowed ({size} bytes)", target.display()));
    }
    let part_path = crate::engine::part_path(&target);
    let writer = DiskWriter::open_or_create(&part_path, size).map_err(|e| format!("Cannot create {}: {e}", part_path.display()))?;
    let pending: Vec<usize> = (0..file.segments.len()).filter(|&n| !done[n] && first.as_ref().is_none_or(|(at, _)| *at != n)).collect();
    let count = servers.len();
    let job = Arc::new(Job {
        board: Mutex::new(Board {
            queues: std::iter::once(pending.iter().copied().collect::<VecDeque<usize>>()).chain((1..count).map(|_| VecDeque::new())).collect(),
            busy: vec![0; count],
            alive: servers.iter().map(|s| s.connections.clamp(1, MAX_CONNECTIONS)).collect(),
            out: vec![None; count],
            tries: HashMap::new(),
            done,
            spans,
            missing: Vec::new(),
            file_crc,
            failed: None,
        }),
        segments: file.segments,
        servers,
        writer,
        limiter,
        size,
        received: AtomicU64::new(0),
        stall,
        max_retries: options.max_retries,
    });
    if let Some((at, part)) = first {
        job.store(at, part).await?;
    }
    let mut notes = Vec::new();
    if !pending.is_empty() {
        let (asked, absent) = tokio::select! {
            _ = cancel.cancelled() => return Err(CANCELLED.to_string()),
            got = sample(&job.servers, &job.segments, &pending, stall) => got,
        };
        if asked >= 3 && absent == asked {
            save(&job, &target);
            return Err(format!("None of {asked} articles of {} checked is on your Usenet servers: the post has expired or was taken down", file_label(&target)));
        }
        if absent > 0 {
            notes.push(format!("{absent} of {asked} articles checked are on none of your Usenet servers: the file may come out incomplete"));
        }
    }

    let mut workers = JoinSet::new();
    for s in 0..job.servers.len() {
        for _ in 0..job.servers[s].connections.clamp(1, MAX_CONNECTIONS) {
            workers.spawn(work(Arc::clone(&job), s, cancel.clone()));
        }
    }
    let (started, mut saved_at) = (Instant::now(), Instant::now());
    loop {
        let finished = tokio::select! {
            joined = workers.join_next() => joined.is_none(),
            _ = tokio::time::sleep(SHOW_EVERY) => false,
        };
        if let Some(tx) = &snapshots {
            let b = job.board.lock();
            let covered = covered(&b.spans).min(size);
            let _ = tx.send(EngineSnapshot {
                total_bytes: size,
                downloaded_bytes: covered,
                speed_bytes_per_sec: job.received.load(Ordering::Relaxed) as f64 / started.elapsed().as_secs_f64().max(0.001),
                progress_ratio: if size > 0 { covered as f64 / size as f64 } else { 1.0 },
                active_workers: b.alive.iter().sum(),
                target_path: Some(target.clone()),
                ..Default::default()
            });
        }
        if saved_at.elapsed() >= SAVE_EVERY {
            save(&job, &target);
            saved_at = Instant::now();
        }
        if finished {
            break;
        }
    }
    save(&job, &target);
    if cancel.is_cancelled() {
        return Err(CANCELLED.to_string());
    }
    let (missing, failed, all_done, spans, file_crc, out, segments) = {
        let b = job.board.lock();
        (b.missing.len(), b.failed.clone(), b.done.iter().all(|d| *d), b.spans.clone(), b.file_crc, b.out.iter().flatten().cloned().collect::<Vec<_>>(), job.segments.len())
    };
    // The `.part` is closed before it is checked, renamed or deleted.
    drop(job);
    if let Some(e) = failed {
        return Err(e);
    }
    if missing > 0 || !all_done {
        let why = if out.is_empty() { String::new() } else { format!(" ({})", out.join("; ")) };
        return Err(format!(
            "{missing} of {segments} articles of {} are missing or damaged on every Usenet server{why}; what came is kept, and another server can be tried",
            file_label(&target)
        ));
    }
    // What came is no use and a next try starts over, as these servers' articles may differ.
    let discard = |why: String| async {
        let _ = tokio::fs::remove_file(&part_path).await;
        let _ = tokio::fs::remove_file(state_path(&target)).await;
        why
    };
    if spans != [(0, size)] {
        let left_out = size - covered(&spans);
        return Err(discard(format!("The articles of {} leave {left_out} of its {size} bytes out: the NZB lists not all of its parts, or they overlap", file_label(&target))).await);
    }
    if let Some(crc) = file_crc {
        let path = part_path.clone();
        let found = tokio::task::spawn_blocking(move || crc_of(&path)).await.map_err(|e| e.to_string())?.map_err(|e| format!("Cannot read {}: {e}", part_path.display()))?;
        if found != crc {
            return Err(discard(format!("{} fails the CRC-32 its posting gives; it is downloaded again on the next try", file_label(&target))).await);
        }
    }
    tokio::fs::rename(&part_path, &target).await.map_err(|e| format!("Cannot rename {}: {e}", part_path.display()))?;
    let checked = target.clone();
    let (path, checked_notes) = match tokio::task::spawn_blocking(move || par2::check(&checked)).await.map_err(|e| e.to_string())? {
        Ok(checked) => checked,
        Err(damaged) => {
            // Kept with its state, so that a next try checks it again rather than downloading it
            // again under another name.
            let _ = tokio::fs::rename(&target, &part_path).await;
            return Err(format!("{damaged}. It is kept as {}", file_label(&part_path)));
        }
    };
    let _ = tokio::fs::remove_file(state_path(&target)).await;
    notes.extend(checked_notes);
    if let (Some(tx), false) = (&snapshots, notes.is_empty()) {
        let _ = tx.send(EngineSnapshot { total_bytes: size, downloaded_bytes: size, progress_ratio: 1.0, target_path: Some(path.clone()), notes, ..Default::default() });
    }
    Ok((path, true))
}

/// Claims `target` for this download (see `engine::claim_target`): another download of it, in
/// this process or another, is refused.
fn take_target(target: &Path) -> Result<TargetClaim, String> {
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    }
    crate::engine::claim_target(target)?.ok_or_else(|| format!("{} is being downloaded by another download", file_label(target)))
}

/// The first of `base`, `base (1)`, ... that neither a file nor another download has, claimed.
pub(crate) fn take_free_target(base: &Path) -> Result<(PathBuf, TargetClaim), String> {
    if let Some(dir) = base.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    }
    let free = |path: &Path| !path.exists() && !crate::engine::part_path(path).exists();
    for n in 0.. {
        let candidate = crate::engine::numbered(base, n);
        if free(&candidate) {
            if let Some(claim) = crate::engine::claim_target(&candidate)?.filter(|_| free(&candidate)) {
                return Ok((candidate, claim));
            }
        }
    }
    unreachable!("an endless range")
}

fn file_label(target: &Path) -> String {
    target.file_name().map_or_else(|| "the file".to_string(), |n| n.to_string_lossy().into_owned())
}

/// Saves which segments are done, beside the `.part`, in one step.
fn save(job: &Job, target: &Path) {
    let path = state_path(target);
    let tmp = path.with_extension("nntp.tmp");
    let Ok(json) = serde_json::to_vec(&job.state()) else { return };
    if std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path)).is_err() {
        tracing::warn!("Cannot save the state of {}", target.display());
    }
}

fn crc_of(path: &Path) -> std::io::Result<u32> {
    let (mut file, mut hasher, mut buf) = (std::fs::File::open(path)?, crc32fast::Hasher::new(), vec![0u8; 1 << 20]);
    loop {
        match file.read(&mut buf)? {
            0 => return Ok(hasher.finalize()),
            n => hasher.update(&buf[..n]),
        }
    }
}

/// PAR2 sets (the PAR 2.0 specification): their file descriptions and slice checksums, read to
/// check files and name them. Recovery slices are counted, never used: nothing is repaired.
mod par2 {
    use super::*;

    const MAGIC: &[u8] = b"PAR2\0PKT";
    const MAIN: &[u8] = b"PAR 2.0\0Main\0\0\0\0";
    const FILE_DESC: &[u8] = b"PAR 2.0\0FileDesc";
    const SLICES: &[u8] = b"PAR 2.0\0IFSC\0\0\0\0";
    /// Largest index file read.
    const MAX_INDEX: u64 = 64 << 20;
    /// Largest slice checked one by one.
    const MAX_SLICE: u64 = 256 << 20;

    #[derive(Debug, Clone)]
    pub(super) struct Desc {
        pub id: [u8; 16],
        pub md5: [u8; 16],
        pub md5_16k: [u8; 16],
        pub length: u64,
        pub name: String,
    }

    #[derive(Debug, Default)]
    pub(super) struct Set {
        pub slice: u64,
        pub files: Vec<Desc>,
        /// The MD5 of each slice of each file, by file id.
        pub slices: HashMap<[u8; 16], Vec<[u8; 16]>>,
    }

    fn array(bytes: &[u8]) -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(&bytes[..16]);
        out
    }

    fn u64_at(bytes: &[u8]) -> u64 {
        let mut out = [0u8; 8];
        out.copy_from_slice(&bytes[..8]);
        u64::from_le_bytes(out)
    }

    /// The set the packets in `bytes` describe; a packet whose MD5 fails is passed over.
    pub(super) fn parse(bytes: &[u8]) -> Set {
        let mut set = Set::default();
        let mut at = 0;
        while let Some(found) = bytes[at..].windows(MAGIC.len()).position(|w| w == MAGIC) {
            let start = at + found;
            let len = bytes.get(start + 8..start + 16).map_or(0, u64_at);
            let end = usize::try_from(len).ok().and_then(|len| start.checked_add(len)).filter(|&end| end <= bytes.len());
            let Some(end) = end.filter(|_| len >= 64 && len % 4 == 0 && Md5::digest(&bytes[start + 32..start + len as usize]).as_slice() == &bytes[start + 16..start + 32]) else {
                at = start + MAGIC.len();
                continue;
            };
            let (kind, body) = (&bytes[start + 48..start + 64], &bytes[start + 64..end]);
            if kind == MAIN && body.len() >= 12 {
                set.slice = u64_at(body);
            } else if kind == FILE_DESC && body.len() >= 56 && !set.files.iter().any(|d| d.id == body[..16]) {
                let name = String::from_utf8_lossy(&body[56..]).trim_end_matches('\0').to_string();
                set.files.push(Desc { id: array(body), md5: array(&body[16..]), md5_16k: array(&body[32..]), length: u64_at(&body[48..]), name });
            } else if kind == SLICES && body.len() >= 16 {
                set.slices.insert(array(body), body[16..].chunks_exact(20).map(array).collect());
            }
            at = end;
        }
        set
    }

    /// What checking a file against its description found.
    #[derive(Debug, PartialEq)]
    pub(super) enum Verdict {
        Intact,
        /// Of its slices, how many fail their MD5 and how many there are.
        Damaged { bad: usize, total: usize },
    }

    fn head_md5(path: &Path) -> std::io::Result<([u8; 16], u64)> {
        let mut file = std::fs::File::open(path)?;
        let mut head = Vec::with_capacity(16 * 1024);
        (&mut file).take(16 * 1024).read_to_end(&mut head)?;
        Ok((array(&Md5::digest(&head)), file.metadata()?.len()))
    }

    /// The description of `path` in `set`: by its name, else by the MD5 of its first 16 KiB and its
    /// length (a file posted under another name).
    pub(super) fn desc_of<'a>(set: &'a Set, path: &Path) -> Option<&'a Desc> {
        let name = path.file_name()?.to_string_lossy().into_owned();
        set.files.iter().find(|d| d.name == name).or_else(|| {
            let (head, len) = head_md5(path).ok()?;
            set.files.iter().find(|d| d.md5_16k == head && d.length == len)
        })
    }

    /// `path` checked against `desc`: its MD5 and length, and when they fail, which slices do.
    pub(super) fn verify(set: &Set, desc: &Desc, path: &Path) -> std::io::Result<Verdict> {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let sums = set.slices.get(&desc.id).filter(|_| (1..=MAX_SLICE).contains(&set.slice));
        let mut buf = vec![0u8; if sums.is_some() { set.slice as usize } else { 1 << 20 }];
        let (mut whole, mut bad, mut total) = (Md5::new(), 0, 0);
        loop {
            let mut filled = 0;
            while filled < buf.len() {
                match file.read(&mut buf[filled..])? {
                    0 => break,
                    n => filled += n,
                }
            }
            if filled == 0 {
                break;
            }
            whole.update(&buf[..filled]);
            if let Some(sums) = sums {
                // The last slice is checked padded with zeros.
                buf[filled..].fill(0);
                if sums.get(total).is_none_or(|sum| Md5::digest(&buf).as_slice() != sum) {
                    bad += 1;
                }
            }
            total += 1;
        }
        if len == desc.length && whole.finalize().as_slice() == desc.md5 {
            return Ok(Verdict::Intact);
        }
        // Slices the file falls short of are damaged too.
        let short = sums.map_or(0, |s| s.len().saturating_sub(total));
        Ok(Verdict::Damaged { bad: (bad + short).max(1), total: total + short })
    }

    /// Whether `name` is a recovery volume's (`x.vol03+04.par2`), not the set's index.
    fn is_volume(name: &str) -> bool {
        name.to_ascii_lowercase().rsplit_once(".vol").is_some_and(|(_, rest)| rest.contains('+'))
    }

    fn is_par2(path: &Path) -> bool {
        path.extension().is_some_and(|e| e.eq_ignore_ascii_case("par2"))
    }

    /// The recovery blocks the `.volNN+MM.par2` files of `dir` hold, by their names.
    pub(super) fn recovery_blocks(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                let stem = name.strip_suffix(".par2")?;
                stem.rsplit_once('+')?.1.parse::<u64>().ok()
            })
            .sum()
    }

    /// `path` checked against `set`, and named as the set names it when it was posted under another
    /// name and that one is free. Err when it is damaged, left as it is.
    fn one(set: &Set, path: &Path, recovery: u64) -> Result<(PathBuf, Option<String>), String> {
        let Some(desc) = desc_of(set, path) else { return Ok((path.to_path_buf(), None)) };
        let label = super::file_label(path);
        let unchecked = match verify(set, desc, path) {
            Ok(Verdict::Intact) => None,
            Ok(Verdict::Damaged { bad, total }) => {
                return Err(format!(
                    "{label} does not match its PAR2 set: {bad} of {total} blocks are damaged, and {recovery} recovery blocks are in its folder. The app checks PAR2 sets but does not repair them: a PAR2 tool can, with at least {bad} recovery blocks"
                ))
            }
            Err(e) => Some(format!("{label} could not be checked against its PAR2 set: {e}")),
        };
        let clean = crate::engine::sanitize_component(&desc.name);
        let named = path.with_file_name(&clean);
        if !clean.is_empty() && named != path && !named.exists() && std::fs::rename(path, &named).is_ok() {
            return Ok((named, unchecked.or_else(|| Some(format!("Renamed {label} to {clean}, its name in the PAR2 set")))));
        }
        Ok((path.to_path_buf(), unchecked.or_else(|| Some(format!("{label} matches its PAR2 set")))))
    }

    /// Checks `done`, a file just downloaded, against the PAR2 set of its folder when one is there
    /// (its smallest `.par2`): its path (renamed as the set names it) and what to tell the user, or
    /// why it is damaged. When `done` is the set itself, the files of it already there are checked
    /// instead, and what they are told only.
    pub(super) fn check(done: &Path) -> Result<(PathBuf, Vec<String>), String> {
        let unchecked = || Ok((done.to_path_buf(), Vec::new()));
        let Some(dir) = done.parent() else { return unchecked() };
        let index = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| is_par2(&e.path()) && e.metadata().is_ok_and(|m| m.is_file() && m.len() <= MAX_INDEX))
            .min_by_key(|e| (is_volume(&e.file_name().to_string_lossy()), e.metadata().map_or(u64::MAX, |m| m.len())));
        let Some(bytes) = index.and_then(|e| std::fs::read(e.path()).ok()) else { return unchecked() };
        let set = parse(&bytes);
        if set.files.is_empty() {
            return unchecked();
        }
        let recovery = recovery_blocks(dir);
        if !is_par2(done) {
            let (path, note) = one(&set, done, recovery)?;
            return Ok((path, note.into_iter().collect()));
        }
        let partial = |p: &Path| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
            [".part", ".part.nntp", ".part.lock", ".tmp"].iter().any(|ext| name.ends_with(ext))
        };
        let mut notes = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_file() && !is_par2(&path) && !partial(&path) {
                match one(&set, &path, recovery) {
                    Ok((_, note)) => notes.extend(note),
                    Err(why) => notes.push(why),
                }
            }
        }
        Ok((done.to_path_buf(), notes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// `data` as a yEnc article: one part of `total` (`part`, from `begin`) of a file of `file`'s
    /// bytes, or the whole file when `part` is None.
    fn yenc(name: &str, file: &[u8], begin: usize, data: &[u8], part: Option<(u32, u32)>) -> Vec<u8> {
        let size = file.len();
        let mut out = match part {
            Some((n, total)) => format!("=ybegin part={n} total={total} line=128 size={size} name={name}\r\n=ypart begin={} end={}\r\n", begin + 1, begin + data.len()),
            None => format!("=ybegin line=128 size={size} name={name}\r\n"),
        }
        .into_bytes();
        let mut column = 0;
        for &b in data {
            let e = b.wrapping_add(42);
            if matches!(e, 0 | b'\n' | b'\r' | b'=') {
                out.extend([b'=', e.wrapping_add(64)]);
                column += 2;
            } else {
                out.push(e);
                column += 1;
            }
            if column >= 128 {
                out.extend(b"\r\n");
                column = 0;
            }
        }
        if column > 0 {
            out.extend(b"\r\n");
        }
        let crc = crc32fast::hash(data);
        let tail = match part {
            Some((n, _)) => format!("=yend size={} part={n} pcrc32={crc:08x} crc32={:08x}\r\n", data.len(), crc32fast::hash(file)),
            None => format!("=yend size={} crc32={crc:08x}\r\n", data.len()),
        };
        out.extend(tail.into_bytes());
        out
    }

    /// Every byte value, so every escape is met; a line starting with a dot is met too.
    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    /// One data byte of a part's article (after its two header lines) changed, not an escape nor
    /// a line end: the part keeps its length and fails its CRC-32.
    fn damage(article: &mut [u8]) {
        let data = article.iter().enumerate().filter(|(_, b)| **b == b'\n').nth(1).unwrap().0 + 1;
        let at = (data + 10..).find(|&i| ![b'=', b'\r', b'\n', b'x'].contains(&article[i]) && article[i - 1] != b'=').unwrap();
        article[at] = b'x';
    }

    #[test]
    fn yenc_parts_decode_and_are_checked() {
        let file = bytes(3000);
        let article = yenc("a b.bin", &file, 1000, &file[1000..2000], Some((2, 3)));
        let part = decode(&article).unwrap();
        assert_eq!((part.name.as_str(), part.size, part.offset, part.file_crc), ("a b.bin", 3000, 1000, Some(crc32fast::hash(&file))));
        assert_eq!(part.data, &file[1000..2000]);
        let whole = decode(&yenc("w.bin", &file, 0, &file, None)).unwrap();
        assert_eq!((whole.offset, whole.data.len(), whole.file_crc), (0, 3000, Some(crc32fast::hash(&file))));
        // One byte changed fails the part's CRC-32; a cut part and a part of the wrong length fail.
        let mut damaged = article.clone();
        damage(&mut damaged);
        assert!(decode(&damaged).unwrap_err().contains("CRC"), "{:?}", decode(&damaged));
        let cut = &article[..article.len() - 100];
        assert!(decode(cut).is_err());
        assert!(decode(b"hello\r\n").unwrap_err().contains("no yEnc"));
        assert_eq!(key("size=10 name=size=3 x", "size"), Some("10"));
        assert_eq!((part.total, whole.total), (Some(3), None));
    }

    /// Workers of a server that fail at once leave one at a time: the last stays to try again, and
    /// when it too gives up, what is queued for the server goes on to the next one.
    #[test]
    fn the_last_worker_of_a_server_hands_its_segments_on() {
        let dir = tempfile::tempdir().unwrap();
        let job = Job {
            segments: (0..4).map(|n| Segment { number: n + 1, id: format!("s{n}@test") }).collect(),
            servers: vec![Server::default(), Server::default()],
            board: Mutex::new(Board {
                queues: vec![(0..4).collect(), VecDeque::new()],
                busy: vec![0, 0],
                alive: vec![2, 2],
                out: vec![None, None],
                tries: HashMap::new(),
                done: vec![false; 4],
                spans: Vec::new(),
                missing: Vec::new(),
                file_crc: None,
                failed: None,
            }),
            writer: DiskWriter::open_or_create(dir.path().join("x.part"), 4).unwrap(),
            limiter: None,
            size: 4,
            received: AtomicU64::new(0),
            stall: Duration::from_secs(1),
            max_retries: 1,
        };
        assert!(job.leave(0, None, true));
        assert!(!job.leave(0, None, true), "the last worker stays");
        assert!(matches!(job.take(1), Take::Wait));
        assert!(job.leave(0, Some("busy".into()), false));
        assert!(matches!(job.take(1), Take::Batch(batch) if batch == [0, 1, 2, 3]));
        assert_eq!(job.board.lock().out[0].as_deref(), Some("busy"));
    }

    #[test]
    fn runs_of_bytes_merge_as_they_meet() {
        let mut spans = Vec::new();
        for (start, end) in [(10, 20), (0, 5), (30, 40), (5, 10)] {
            cover(&mut spans, start, end);
        }
        assert_eq!(spans, [(0, 20), (30, 40)]);
        cover(&mut spans, 12, 18);
        assert_eq!((spans.as_slice(), covered(&spans)), (&[(0, 20), (30, 40)][..], 30));
        cover(&mut spans, 15, 35);
        assert_eq!(spans, [(0, 40)]);
    }

    /// A connection over `io`, as if to a server.
    async fn over(io: tokio::io::DuplexStream) -> Conn {
        let slot = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        Conn { io: BufStream::new(Box::new(io) as Box<dyn Stream>), stall: Duration::from_secs(10), _slot: slot }
    }

    /// A line longer than the app reads as one is cut there, and an article larger than it reads
    /// ends the connection: neither is read whole into memory.
    #[tokio::test]
    async fn oversized_lines_and_articles_are_not_read_whole() {
        let (client, mut server) = tokio::io::duplex(64 << 10);
        let mut conn = over(client).await;
        tokio::spawn(async move {
            let _ = server.write_all(&vec![b'a'; 1 << 20]).await;
        });
        let refused = conn.reply().await.unwrap_err();
        assert!(refused.starts_with("the server answered") && refused.len() < 200, "{refused}");

        let (client, mut server) = tokio::io::duplex(64 << 10);
        let mut conn = over(client).await;
        tokio::spawn(async move {
            let line = [vec![b'b'; 998], b"\r\n".to_vec()].concat();
            let _ = server.write_all(b"222 0 <a@b>\r\n").await;
            for _ in 0..MAX_ARTICLE / line.len() + 10 {
                if server.write_all(&line).await.is_err() {
                    return;
                }
            }
            let _ = server.write_all(b".\r\n").await;
        });
        let (answers, broke) = conn.bodies(&["a@b"]).await;
        assert!(answers.is_empty() && broke.as_deref() == Some("an article larger than the app reads"), "{broke:?}");
    }

    const NZB: &str = r#"<?xml version="1.0" encoding="iso-8859-1" ?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
 <head><meta type="title">My Show</meta><meta type="password">x</meta></head>
 <file poster="a &lt;a@b&gt;" date="1" subject="[1/2] - &quot;data.bin&quot; yEnc (1/5)">
  <groups><group>alt.binaries.test</group></groups>
  <segments>
   <segment bytes="10" number="2">p2@test</segment>
   <segment bytes="10" number="1">&lt;p1@test&gt;</segment>
   <segment bytes="10" number="3">p3@test</segment>
   <segment bytes="10" number="3">p3@test</segment>
   <segment bytes="10" number="4">p4@test</segment>
   <segment bytes="10" number="5">p5@test</segment>
   <segment bytes="10" number="6">bad id@test</segment>
  </segments>
 </file>
 <file poster="a" date="1" subject="a1b2c3d4e5f6 yEnc (1/1)">
  <segments><segment bytes="10" number="1">q1@test</segment></segments>
 </file>
 <file poster="a" date="1" subject="Here's your file!  abc-mr2a.r01 (1/2)">
  <segments><segment bytes="10" number="1">r1@test</segment></segments>
 </file>
</nzb>"#;

    #[test]
    fn nzb_files_become_downloads_named_by_their_subjects() {
        let nzb = parse_nzb(NZB).unwrap();
        assert_eq!(nzb.title.as_deref(), Some("My Show"));
        let ids: Vec<&str> = nzb.files[0].segments.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["p1@test", "p2@test", "p3@test", "p4@test", "p5@test"]);
        // The NZB is kept once, by its contents, and its files read that copy: the indexer's link
        // (and the account's key in it) is in none of them.
        let source = Url::parse("https://indexer.example/get/show.nzb?apikey=k").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let kept = keep_in(NZB.as_bytes(), dir.path()).unwrap();
        assert_eq!(keep_in(NZB.as_bytes(), dir.path()).unwrap(), kept);
        assert_eq!(std::fs::read(kept.to_file_path().unwrap()).unwrap(), NZB.as_bytes());
        let tasks = tasks_from(NZB.as_bytes(), &source, &kept).unwrap();
        let names: Vec<Option<&str>> = tasks.iter().map(|t| t.name.as_deref().and_then(Path::to_str)).collect();
        assert_eq!(names, [Some("data.bin"), None, Some("abc-mr2a.r01")]);
        assert!(tasks.iter().all(|t| t.folder.as_deref() == Some(Path::new("My Show")) && t.from_document && handles(&t.urls[0])));
        assert!(tasks.iter().all(|t| !t.urls[0].as_str().contains("apikey")));
        assert_eq!(link_parts(&tasks[2].urls[0]).unwrap(), (kept, 2));
        assert_eq!(subject_name("\"x.part01.rar\" yEnc (1/9)").as_deref(), Some("x.part01.rar"));
        assert!(parse_nzb("<nzb></nzb>").is_err() && parse_nzb("<html>").is_err());
        let server = Server::from_link("nntps://me:p%40ss@news.example.com?connections=20", 1).unwrap();
        assert_eq!((server.port(), server.user.as_str(), server.password.as_str(), server.connections), (563, "me", "p@ss", 20));
        assert_eq!(Server::from_link("nntp://news.example.com:8119", 0).unwrap().port(), 8119);
        assert!(!format!("{server:?}").contains("p@ss"));
        assert!(Server::from_link("https://x", 0).is_err() && Server::from_link("nntps://a%0d%0aQUIT@x", 0).is_err());
    }

    /// What a test news server saw.
    #[derive(Default)]
    struct Counts {
        /// BODY requests answered with an article.
        bodies: AtomicUsize,
        /// Connections open now, and the most open at once.
        open: AtomicUsize,
        peak: AtomicUsize,
        accepted: AtomicUsize,
    }

    /// A connection a test news server counts open while it lives.
    struct Open(Arc<Counts>);

    impl Open {
        fn new(counts: &Arc<Counts>) -> Self {
            let now = counts.open.fetch_add(1, Ordering::SeqCst) + 1;
            counts.peak.fetch_max(now, Ordering::SeqCst);
            Self(Arc::clone(counts))
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            self.0.open.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// How a test news server behaves.
    #[derive(Clone, Copy, Default)]
    struct Behaviour {
        /// The user name and password it takes.
        login: Option<(&'static str, &'static str)>,
        /// Connections it serves; every one after is told it takes no more (502).
        serves: Option<usize>,
        /// How long it takes over each article.
        delay: Duration,
    }

    /// An NNTP server on 127.0.0.1 serving `articles` (message-id, yEnc body), signed in to as
    /// `login` when given.
    async fn news(articles: Vec<(String, Vec<u8>)>, login: Option<(&'static str, &'static str)>) -> (Server, Arc<Counts>) {
        news_with(articles, Behaviour { login, ..Behaviour::default() }).await
    }

    async fn news_with(articles: Vec<(String, Vec<u8>)>, behaviour: Behaviour) -> (Server, Arc<Counts>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let articles: Arc<HashMap<String, Vec<u8>>> = Arc::new(articles.into_iter().collect());
        let counts = Arc::new(Counts::default());
        let seen = Arc::clone(&counts);
        let Behaviour { login, serves, delay } = behaviour;
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (articles, counted) = (Arc::clone(&articles), Arc::clone(&seen));
                tokio::spawn(async move {
                    let mut io = BufStream::new(socket);
                    if serves.is_some_and(|n| counted.accepted.fetch_add(1, Ordering::SeqCst) >= n) {
                        let _ = io.write_all(b"502 too many connections\r\n").await;
                        let _ = io.flush().await;
                        return;
                    }
                    let open = Open::new(&counted);
                    let _ = io.write_all(b"200 test news\r\n").await;
                    let _ = io.flush().await;
                    let (mut signed, mut line) = (login.is_none(), String::new());
                    loop {
                        line.clear();
                        if io.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let (verb, arg) = line.trim_end().split_once(' ').unwrap_or((line.trim_end(), ""));
                        let verb = verb.to_ascii_uppercase();
                        let reply: Vec<u8> = match verb.as_str() {
                            "AUTHINFO" => match arg.split_once(' ') {
                                Some(("USER", user)) if login.is_some_and(|l| l.0 == user) => b"381 more\r\n".to_vec(),
                                Some(("PASS", pass)) if login.is_some_and(|l| l.1 == pass) => {
                                    signed = true;
                                    b"281 welcome\r\n".to_vec()
                                }
                                _ => b"481 no\r\n".to_vec(),
                            },
                            "QUIT" => {
                                // Counted closed before the client hears it, which may then connect again.
                                drop(open);
                                let _ = io.write_all(b"205 bye\r\n").await;
                                let _ = io.flush().await;
                                return;
                            }
                            _ if !signed => b"480 sign in first\r\n".to_vec(),
                            "BODY" | "STAT" => match articles.get(arg.trim_matches(['<', '>'])) {
                                Some(body) if verb == "BODY" => {
                                    tokio::time::sleep(delay).await;
                                    counted.bodies.fetch_add(1, Ordering::SeqCst);
                                    let mut out = format!("222 0 {arg}\r\n").into_bytes();
                                    for line in body.split_inclusive(|&b| b == b'\n') {
                                        if line.starts_with(b".") {
                                            out.push(b'.');
                                        }
                                        out.extend_from_slice(line);
                                    }
                                    out.extend(b".\r\n");
                                    out
                                }
                                Some(_) => format!("223 0 {arg}\r\n").into_bytes(),
                                None => b"430 no such article\r\n".to_vec(),
                            },
                            _ => b"500 what\r\n".to_vec(),
                        };
                        if io.write_all(&reply).await.is_err() || io.flush().await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let server = Server { host: "127.0.0.1".into(), port, tls: false, connections: 2, ..Server::default() };
        (server, counts)
    }

    /// The five articles of a 50 000-byte file, the second damaged and the fourth missing where
    /// `broken` says.
    fn articles(file: &[u8], broken: bool) -> Vec<(String, Vec<u8>)> {
        let ids = ["p1@test", "p2@test", "p3@test", "p4@test", "p5@test"];
        let mut out = Vec::new();
        for (n, id) in ids.iter().enumerate() {
            let mut article = yenc("data.bin", file, n * 10_000, &file[n * 10_000..(n + 1) * 10_000], Some((n as u32 + 1, 5)));
            if broken && n == 1 {
                damage(&mut article);
            }
            if !(broken && n == 3) {
                out.push((id.to_string(), article));
            }
        }
        out
    }

    /// `file` posted in `parts` parts as `name`, the articles' ids `<name>.<n>@test`.
    fn post(name: &str, file: &[u8], parts: usize) -> Vec<(String, Vec<u8>)> {
        let len = file.len().div_ceil(parts);
        file.chunks(len).enumerate().map(|(n, data)| (format!("{name}.{}@test", n + 1), yenc(name, file, n * len, data, Some((n as u32 + 1, parts as u32))))).collect()
    }

    fn ids(articles: &[(String, Vec<u8>)]) -> Vec<String> {
        articles.iter().map(|(id, _)| id.clone()).collect()
    }

    /// `article` without the whole file's CRC-32 (its trailer's `crc32=`) and its count of parts
    /// (its header's `total=`).
    fn bare(article: &[u8]) -> Vec<u8> {
        let head = article.iter().position(|&b| b == b'\n').unwrap() + 1;
        let tail = article[..article.len() - 1].iter().rposition(|&b| b == b'\n').unwrap() + 1;
        let strip = |line: &[u8], key: &str| {
            let text = String::from_utf8(line.to_vec()).unwrap();
            format!("{}\r\n", text.trim_end().split(' ').filter(|w| !w.starts_with(key)).collect::<Vec<_>>().join(" ")).into_bytes()
        };
        [strip(&article[..head], "total="), article[head..tail].to_vec(), strip(&article[tail..], "crc32=")].concat()
    }

    /// An NZB of `files`: each one's name and its segments' ids, in order.
    fn nzb_of(files: &[(&str, Vec<String>)]) -> String {
        let mut out = String::from("<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n");
        for (name, ids) in files {
            out += &format!(" <file poster=\"a\" date=\"1\" subject=\"&quot;{name}&quot; yEnc (1/{})\"><segments>\n", ids.len());
            for (n, id) in ids.iter().enumerate() {
                out += &format!("  <segment bytes=\"1\" number=\"{}\">{id}</segment>\n", n + 1);
            }
            out += " </segments></file>\n";
        }
        out + "</nzb>\n"
    }

    /// The downloads of the NZB `text`, saved in `dir` as `post.nzb` and read from there.
    fn listed(dir: &Path, text: &str) -> Vec<Task> {
        let path = dir.join("post.nzb");
        std::fs::write(&path, text).unwrap();
        let link = Url::from_file_path(&path).unwrap();
        tasks_from(text.as_bytes(), &link, &link).unwrap()
    }

    /// Downloads file `n` of the NZB `text` (see [`listed`]) into `dir`.
    async fn fetch_file(dir: &Path, text: &str, n: usize, options: DownloadOptions) -> Result<(PathBuf, bool), String> {
        let tasks = listed(dir, text);
        run(&tasks[n].urls[0], &options, None, dir, None, CancellationToken::new()).await
    }

    fn into(target: &Path, servers: Vec<Server>) -> DownloadOptions {
        DownloadOptions { output_path: Some(target.to_path_buf()), nntp: servers, max_retries: 1, ..Default::default() }
    }

    /// A server that takes no more connections, refusing them all at once, neither hangs the
    /// download nor has it say "0 articles missing": alone, it says how many are missing and why;
    /// with a backup server, the backup gives them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_server_refusing_every_connection_at_once_hands_its_articles_on() {
        let file = bytes(50_000);
        let dir = tempfile::tempdir().unwrap();
        let nzb = NZB.replace("&lt;p1@test&gt;", "p1@test");
        for backup in [false, true] {
            // The first connection (the file's first article) is served, every one after refused.
            let (busy, _) = news_with(articles(&file, false), Behaviour { serves: Some(1), ..Behaviour::default() }).await;
            let mut servers = vec![Server { connections: 8, ..busy }];
            if backup {
                let (spare, _) = news(articles(&file, false), None).await;
                servers.push(Server { priority: 1, ..spare });
            }
            let target = dir.path().join(format!("{backup}.bin"));
            let done = tokio::time::timeout(Duration::from_secs(60), fetch_file(dir.path(), &nzb, 0, into(&target, servers))).await.expect("the download ends");
            if backup {
                assert_eq!(std::fs::read(done.unwrap().0).unwrap(), file);
            } else {
                let why = done.unwrap_err();
                assert!(why.contains("4 of 5 articles") && why.contains("too many connections"), "{why}");
            }
        }
    }

    /// Two downloads from one server at once open no more connections together than its setting
    /// says: a provider's limit is the account's, not a download's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn downloads_at_once_share_their_servers_connections() {
        let (one, two) = (bytes(48_000), bytes(36_000).iter().map(|b| b ^ 0x5a).collect::<Vec<u8>>());
        let (a, b) = (post("one.bin", &one, 12), post("two.bin", &two, 12));
        let nzb = nzb_of(&[("one.bin", ids(&a)), ("two.bin", ids(&b))]);
        let (server, counts) = news_with([a, b].concat(), Behaviour { delay: Duration::from_millis(15), ..Behaviour::default() }).await;
        let dir = tempfile::tempdir().unwrap();
        let tasks = listed(dir.path(), &nzb);
        let (first, second) = (into(&dir.path().join("one.bin"), vec![server.clone()]), into(&dir.path().join("two.bin"), vec![server.clone()]));
        let both = async {
            tokio::join!(
                run(&tasks[0].urls[0], &first, None, dir.path(), None, CancellationToken::new()),
                run(&tasks[1].urls[0], &second, None, dir.path(), None, CancellationToken::new())
            )
        };
        let (x, y) = tokio::time::timeout(Duration::from_secs(60), both).await.expect("the downloads end");
        assert_eq!(std::fs::read(x.unwrap().0).unwrap(), one);
        assert_eq!(std::fs::read(y.unwrap().0).unwrap(), two);
        let peak = counts.peak.load(Ordering::SeqCst);
        assert!(peak <= server.connections, "{peak} connections at once");
    }

    /// A connection with nothing left to ask gives its slot up: a worker that holds articles and
    /// waits for a slot is not kept from it by workers of its own download that wait for it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_idle_connection_gives_its_slot_to_a_worker_that_waits() {
        let file = bytes(48_000);
        let a = post("solo.bin", &file, 12);
        let nzb = nzb_of(&[("solo.bin", ids(&a))]);
        let (server, _) = news_with(a, Behaviour { delay: Duration::from_millis(15), ..Behaviour::default() }).await;
        let dir = tempfile::tempdir().unwrap();
        let tasks = listed(dir.path(), &nzb);
        // Another download holds one of the server's two connections throughout.
        let _held = slots(&server).acquire_owned().await.unwrap();
        let options = into(&dir.path().join("solo.bin"), vec![server.clone()]);
        let run = run(&tasks[0].urls[0], &options, None, dir.path(), None, CancellationToken::new());
        let done = tokio::time::timeout(Duration::from_secs(30), run).await.expect("the download ends");
        assert_eq!(std::fs::read(done.unwrap().0).unwrap(), file);
    }

    /// A file is done only when its parts cover it whole: an NZB that leaves a part out, or parts
    /// that overlap, fail even when no article gives the whole file's CRC-32, and so does a size
    /// its articles cannot hold, before anything is written; a whole post without one downloads.
    #[tokio::test]
    async fn a_file_is_done_only_when_its_parts_cover_it() {
        let file = bytes(50_000);
        let dir = tempfile::tempdir().unwrap();
        let mut articles: Vec<(String, Vec<u8>)> = post("data.bin", &file, 5).into_iter().map(|(id, a)| (id, bare(&a))).collect();
        let whole = ids(&articles);
        // A third part that says it goes where the second does.
        articles.push(("over@test".into(), bare(&yenc("data.bin", &file, 10_000, &file[10_000..20_000], Some((3, 5))))));
        // Parts that give their count (5) and the whole file's CRC-32.
        let counted = post("full.bin", &file, 5);
        let counted_ids = ids(&counted);
        articles.extend(counted);
        // One part of 100 bytes of a file it says is 10 TB.
        let small = bytes(100);
        let mut big = bare(&yenc("big.bin", &small, 0, &small, Some((1, 1))));
        let at = big.windows(9).position(|w| w == b"size=100 ").unwrap();
        big.splice(at..at + 9, *b"size=10995116277760 ");
        articles.push(("big@test".into(), big));
        let (server, _) = news(articles, None).await;
        let get = |name: &str, ids: Vec<String>| {
            let (dir, target, server, text) = (dir.path().to_path_buf(), dir.path().join(name), server.clone(), nzb_of(&[(name, ids)]));
            async move { fetch_file(&dir, &text, 0, into(&target, vec![server])).await }
        };

        let (done, _) = get("whole.bin", whole.clone()).await.unwrap();
        assert_eq!(std::fs::read(done).unwrap(), file);
        let lacking = [&whole[..2], &whole[3..]].concat();
        let gap = get("gap.bin", lacking).await.unwrap_err();
        assert!(gap.contains("leave 10000 of its 50000 bytes out"), "{gap}");
        let overlapping = [&whole[..2], &["over@test".to_string()], &whole[3..]].concat();
        let overlap = get("overlap.bin", overlapping).await.unwrap_err();
        assert!(overlap.contains("leave 10000 of its 50000 bytes out"), "{overlap}");
        let short = get("short.bin", [&counted_ids[..2], &counted_ids[3..]].concat()).await.unwrap_err();
        assert!(short.contains("lists 4 of the 5 articles"), "{short}");
        assert!(!crate::engine::part_path(&dir.path().join("short.bin")).exists());
        let huge = get("big.bin", vec!["big@test".into()]).await.unwrap_err();
        assert!(huge.contains("claims 10995116277760 bytes"), "{huge}");
        assert!(!crate::engine::part_path(&dir.path().join("big.bin")).exists());
    }

    /// A file that fails the CRC-32 its posting gives is not kept as done: the next try
    /// downloads it again, under the same name.
    #[tokio::test]
    async fn a_file_failing_its_crc_downloads_again_on_the_next_try() {
        let file = bytes(30_000);
        let wrong: Vec<u8> = file.iter().map(|b| b ^ 1).collect();
        let parts = |whole: &[u8]| (0..3).map(|n| (format!("c{n}@test"), yenc("c.bin", whole, n * 10_000, &file[n * 10_000..(n + 1) * 10_000], Some((n as u32 + 1, 3))))).collect::<Vec<_>>();
        // Each part is sound, but the whole file's CRC-32 they give is another file's.
        let (lying, _) = news(parts(&wrong), None).await;
        let (honest, counts) = news(parts(&file), None).await;
        let dir = tempfile::tempdir().unwrap();
        let nzb = nzb_of(&[("c.bin", (0..3).map(|n| format!("c{n}@test")).collect())]);
        let target = dir.path().join("c.bin");
        let failed = fetch_file(dir.path(), &nzb, 0, into(&target, vec![lying])).await.unwrap_err();
        assert!(failed.contains("fails the CRC-32"), "{failed}");
        assert!(!crate::engine::part_path(&target).exists() && !state_path(&target).exists());
        let (done, _) = fetch_file(dir.path(), &nzb, 0, into(&target, vec![honest])).await.unwrap();
        assert_eq!((done.as_path(), std::fs::read(&done).unwrap()), (target.as_path(), file));
        assert_eq!(counts.bodies.load(Ordering::SeqCst), 3);
    }

    /// A server that asks to sign in (480) when no user name is set says so, for the first
    /// article and for those after.
    #[tokio::test]
    async fn a_server_asking_to_sign_in_says_so() {
        let file = bytes(50_000);
        let (locked, _) = news(articles(&file, false), Some(("me", "secret"))).await;
        let (first_only, _) = news(articles(&file, false).into_iter().take(1).collect(), None).await;
        let dir = tempfile::tempdir().unwrap();
        let nzb = NZB.replace("&lt;p1@test&gt;", "p1@test");
        for (n, servers) in [vec![locked.clone()], vec![first_only, Server { priority: 1, ..locked }]].into_iter().enumerate() {
            let failed = fetch_file(dir.path(), &nzb, 0, into(&dir.path().join(format!("{n}.bin")), servers)).await.unwrap_err();
            assert!(failed.contains("user name and password"), "{failed}");
        }
    }

    /// A post naming its file ".." is saved as "file" in the folder chosen, not beside it.
    #[tokio::test]
    async fn a_file_named_dot_dot_stays_in_its_folder() {
        let data = bytes(1000);
        let (server, _) = news(vec![("q1@test".into(), yenc("..", &data, 0, &data, None))], None).await;
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("in");
        let tasks = listed(dir.path(), NZB);
        let options = DownloadOptions { nntp: vec![server], ..Default::default() };
        let (done, _) = run(&tasks[1].urls[0], &options, None, &save, None, CancellationToken::new()).await.unwrap();
        assert_eq!((done.as_path(), std::fs::read(&done).unwrap()), (save.join("file").as_path(), data));
    }

    /// A file that fails its PAR2 check stays a `.part` with its state, so the next try checks it
    /// again instead of downloading it again under another name.
    #[tokio::test]
    async fn a_file_failing_its_par2_check_is_checked_again_not_downloaded_again() {
        let file = bytes(50_000);
        let (server, counts) = news(articles(&file, false), None).await;
        let dir = tempfile::tempdir().unwrap();
        let other: Vec<u8> = file.iter().map(|b| b ^ 1).collect();
        std::fs::write(dir.path().join("set.par2"), par2_index(&[("data.bin", &other[..])])).unwrap();
        let nzb = NZB.replace("&lt;p1@test&gt;", "p1@test");
        let target = dir.path().join("data.bin");
        for _ in 0..2 {
            let damaged = fetch_file(dir.path(), &nzb, 0, into(&target, vec![server.clone()])).await.unwrap_err();
            assert!(damaged.contains("does not match its PAR2 set") && damaged.contains("kept as data.bin.part"), "{damaged}");
            assert!(!target.exists() && crate::engine::part_path(&target).exists() && state_path(&target).exists());
        }
        assert_eq!(counts.bodies.load(Ordering::SeqCst), 5, "the second try downloads nothing");
    }

    /// A file's articles come from the first server that has them whole: the one it lacks and
    /// the one it holds damaged (its CRC-32 fails) from the backup server, nothing else from it. A
    /// download cut short (the backup not there) keeps what came, and the next run asks only for
    /// the rest. A refused password says so.
    #[tokio::test]
    async fn articles_come_from_the_servers_that_have_them_and_a_cut_download_resumes() {
        let file = bytes(50_000);
        let (primary, _) = news(articles(&file, true), Some(("me", "secret"))).await;
        let primary = Server { user: "me".into(), password: "secret".into(), ..primary };
        let (backup, asked) = news(articles(&file, false), None).await;
        let backup = Server { priority: 1, ..backup };
        let dir = tempfile::tempdir().unwrap();
        let nzb = dir.path().join("show.nzb");
        std::fs::write(&nzb, NZB.replace("&lt;p1@test&gt;", "p1@test")).unwrap();
        let link = Url::from_file_path(&nzb).unwrap();
        let tasks = tasks_from(&std::fs::read(&nzb).unwrap(), &link, &link).unwrap();
        let target = dir.path().join("My Show").join("data.bin");
        let options = |servers: Vec<Server>| DownloadOptions { output_path: Some(target.clone()), nntp: servers, max_retries: 1, ..Default::default() };

        let cut = run(&tasks[0].urls[0], &options(vec![primary.clone()]), None, dir.path(), None, CancellationToken::new()).await.unwrap_err();
        assert!(cut.contains("2 of 5 articles of data.bin are missing or damaged"), "{cut}");
        assert!(crate::engine::part_path(&target).exists() && state_path(&target).exists());
        // Another download holding the target: this one does not write into it.
        let held = crate::engine::claim_target(&target).unwrap().unwrap();
        let taken = run(&tasks[0].urls[0], &options(vec![backup.clone(), primary.clone()]), None, dir.path(), None, CancellationToken::new()).await.unwrap_err();
        assert!(taken.contains("being downloaded"), "{taken}");
        drop(held);
        let (done, fresh) = run(&tasks[0].urls[0], &options(vec![backup.clone(), primary.clone()]), None, dir.path(), None, CancellationToken::new()).await.unwrap();
        assert!(fresh);
        assert_eq!((done.as_path(), std::fs::read(&done).unwrap()), (target.as_path(), file.clone()));
        assert_eq!(asked.bodies.load(Ordering::SeqCst), 2, "only the articles the first run lacked");
        assert!(!state_path(&target).exists());

        // Both servers at once: the backup is asked for those two alone.
        std::fs::remove_file(&target).unwrap();
        asked.bodies.store(0, Ordering::SeqCst);
        let (done, _) = run(&tasks[0].urls[0], &options(vec![backup.clone(), primary.clone()].into_iter().rev().collect()), None, dir.path(), None, CancellationToken::new()).await.unwrap();
        assert_eq!(std::fs::read(&done).unwrap(), file);
        assert_eq!(asked.bodies.load(Ordering::SeqCst), 2);

        let wrong = Server { password: "guess".into(), ..primary };
        std::fs::remove_file(&target).unwrap();
        let refused = run(&tasks[0].urls[0], &options(vec![wrong]), None, dir.path(), None, CancellationToken::new()).await.unwrap_err();
        assert!(refused.contains("refused the user name or password"), "{refused}");
        let none = run(&tasks[0].urls[0], &options(Vec::new()), None, dir.path(), None, CancellationToken::new()).await.unwrap_err();
        assert_eq!(none, NO_SERVER);
    }

    fn packet(kind: &[u8], body: &[u8]) -> Vec<u8> {
        let rest = [[7u8; 16].as_slice(), kind, body].concat();
        let len = (32 + rest.len()) as u64;
        assert_eq!(len % 4, 0);
        [MAGIC_FOR_TESTS, len.to_le_bytes().as_slice(), Md5::digest(&rest).as_slice(), rest.as_slice()].concat()
    }

    const MAGIC_FOR_TESTS: &[u8] = b"PAR2\0PKT";

    /// A PAR2 index for `files` (name, bytes) with 4096-byte slices, as the specification lays it.
    fn par2_index(files: &[(&str, &[u8])]) -> Vec<u8> {
        let slice = 4096usize;
        let main = [(slice as u64).to_le_bytes().as_slice(), (files.len() as u32).to_le_bytes().as_slice(), [1u8; 16].as_slice()].concat();
        let mut out = packet(b"PAR 2.0\0Main\0\0\0\0", &main);
        for (n, (name, data)) in files.iter().enumerate() {
            let id = [n as u8 + 1; 16];
            let mut padded_name = name.as_bytes().to_vec();
            padded_name.resize(name.len().div_ceil(4) * 4, 0);
            let head = &data[..data.len().min(16 * 1024)];
            let length = (data.len() as u64).to_le_bytes();
            let desc = [id.as_slice(), Md5::digest(data).as_slice(), Md5::digest(head).as_slice(), length.as_slice(), padded_name.as_slice()].concat();
            out.extend(packet(b"PAR 2.0\0FileDesc", &desc));
            let mut sums = id.to_vec();
            for chunk in data.chunks(slice) {
                let mut padded = chunk.to_vec();
                padded.resize(slice, 0);
                sums.extend(Md5::digest(&padded));
                sums.extend(crc32fast::hash(&padded).to_le_bytes());
            }
            out.extend(packet(b"PAR 2.0\0IFSC\0\0\0\0", &sums));
        }
        out
    }

    /// A file posted under a random name takes its name in the PAR2 set and is checked intact; a
    /// damaged one fails, saying how many blocks; the set arriving last checks those already there.
    #[test]
    fn par2_sets_check_and_name_files() {
        let dir = tempfile::tempdir().unwrap();
        let (real, other) = (bytes(10_000), bytes(9_000).iter().map(|b| b ^ 0x55).collect::<Vec<u8>>());
        let index = par2_index(&[("real.bin", &real[..]), ("other.bin", &other[..])]);
        let set = par2::parse(&[b"junk".as_slice(), &index].concat());
        assert_eq!((set.slice, set.files.len(), set.slices.len()), (4096, 2, 2));
        std::fs::write(dir.path().join("set.par2"), &index).unwrap();
        std::fs::write(dir.path().join("set.vol00+02.par2"), b"").unwrap();
        std::fs::write(dir.path().join("set.vol02+04.par2"), b"").unwrap();
        let obfuscated = dir.path().join("f3a9c0d1e2b4");
        std::fs::write(&obfuscated, &real).unwrap();
        let (path, notes) = par2::check(&obfuscated).unwrap();
        assert_eq!(path, dir.path().join("real.bin"));
        assert!(notes[0].contains("Renamed f3a9c0d1e2b4 to real.bin"), "{notes:?}");
        let mut damaged = other.clone();
        damaged[5000] ^= 1;
        std::fs::write(dir.path().join("other.bin"), &damaged).unwrap();
        let refused = par2::check(&dir.path().join("other.bin")).unwrap_err();
        assert!(refused.contains("1 of 3 blocks are damaged, and 6 recovery blocks"), "{refused}");
        let (_, notes) = par2::check(&dir.path().join("set.par2")).unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("real.bin matches")) && notes.iter().any(|n| n.contains("other.bin does not match")));
        assert_eq!(par2::check(&dir.path().join("set.vol00+02.par2")).unwrap().0, dir.path().join("set.vol00+02.par2"));
    }
}
