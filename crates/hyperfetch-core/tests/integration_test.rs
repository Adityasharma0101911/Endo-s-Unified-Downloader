use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, RwLock, RwLockReadGuard};
use url::Url;

use hyperfetch_core::engine::{DownloadEngine, DownloadOptions, EngineSnapshot};
use hyperfetch_core::history::{DownloadHistoryManager, HistoryEntry, HistoryStatus};
use hyperfetch_core::hosts::{self, HostProfile};
use hyperfetch_core::range::{merge_ranges, ByteRange};
use hyperfetch_core::state::DownloadState;

const KB: usize = 1024;
/// What the first mirror's probe asks for: the file's first MiB.
const PREFETCH: usize = 1024 * KB;

/// Every engine run records history in one per-process file. Tests that depend on what is
/// recorded take this exclusively, so a parallel test's history write cannot race theirs.
static HISTORY: RwLock<()> = RwLock::const_new(());

/// Points download history at a file under target/ so tests never touch the user's history.
/// Each run starts it afresh and reuses it (and its lock file) instead of piling up temp files.
fn isolate_history() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("integration-history.json");
        let _ = std::fs::remove_file(&path);
        std::env::set_var("ENDO_HISTORY_PATH", path);
    });
}

fn history_entry(path: &Path) -> Option<HistoryEntry> {
    let path = std::path::absolute(path).unwrap();
    DownloadHistoryManager::load().entries().iter().find(|e| e.file_path == path).cloned()
}

async fn setup() -> RwLockReadGuard<'static, ()> {
    isolate_history();
    HISTORY.read().await
}

async fn run(engine: &DownloadEngine, tx: Option<broadcast::Sender<EngineSnapshot>>) -> Result<PathBuf, String> {
    tokio::time::timeout(Duration::from_secs(30), engine.run(tx))
        .await
        .expect("engine hung")
}

fn payload(len: usize, seed: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * seed + i / 251) % 256) as u8).collect()
}

fn part_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// What the mock does with the n-th GET (0-based).
#[derive(Clone, Copy, Debug)]
enum Reply {
    Normal,
    /// Only a status line (plus Retry-After seconds if given).
    Status(u16, Option<u64>),
    /// Send the headers and this many body bytes, then close the connection.
    CloseAfter(usize),
    /// Send the headers and this many body bytes, then go silent without closing.
    StallAfter(usize),
    /// Ignore Range and If-Range and send the whole file with 200.
    IgnoreRange,
    /// Send a small HTML error page with 200 instead of the file.
    ErrorPage,
}

struct Mock {
    data: Vec<u8>,
    /// Advertise and honour byte ranges.
    ranges: bool,
    /// HEAD advertises `Accept-Ranges: bytes` even though GETs ignore Range.
    head_claims_ranges: bool,
    /// HEAD leaves out `Accept-Ranges`, as many servers do, even though GETs honour Range.
    head_hides_ranges: bool,
    /// HEAD answers only `Content-Length: 0`, as many dynamic endpoints do.
    empty_head: bool,
    /// HEAD answers with this status instead.
    head_status: Option<u16>,
    /// HEAD answers only after this long.
    head_delay: Duration,
    /// Every request waits this long for its answer, like a round trip over a slow network.
    latency: Duration,
    /// Only HEAD carries the ETag.
    etag_only_on_head: bool,
    /// Any GET with a Range header gets 400.
    rejects_range: bool,
    /// This many probes are answered 503 before the server recovers.
    busy_probes: AtomicUsize,
    /// No Content-Length anywhere; chunked transfer encoding; ranges ignored.
    chunked: bool,
    etag: Option<&'static str>,
    /// Content-Disposition value sent with every GET response.
    disposition: Option<String>,
    /// Content-Type value sent with every HEAD and GET response.
    content_type: Option<&'static str>,
    /// Content-Disposition value sent with HEAD responses only.
    head_disposition: Option<&'static str>,
    /// Report this total in Content-Range instead of the real one (a broken mirror).
    content_range_total: Option<usize>,
    /// Report the total in Content-Range as `*` (unknown).
    unknown_total: bool,
    /// Answer 503 when more than this many GETs are being served at once.
    max_active: Option<usize>,
    /// Redirect every request to this URL plus `?t=n`, as a signing redirector does: `t=0` for
    /// HEAD and the probes, a fresh `n` for every other GET.
    redirect: Option<String>,
    /// Answer GETs (other than probes) whose request target contains the text this way, as an
    /// expired signed URL is answered.
    expired: Option<(&'static str, Reply)>,
    /// What to do with each GET except the engine's probes, which are served as `probe_reply`.
    plan: fn(usize) -> Reply,
    /// What to do with the engine's probes once `busy_probes` are through.
    probe_reply: Reply,
    /// A probe's answer pauses this long once it has sent this many body bytes (a multiple of
    /// 16 KiB).
    probe_pause: Option<(usize, Duration)>,
    /// Pause between 16 KiB body writes, in microseconds (adjustable while running).
    delay_us: AtomicU64,
    /// If set, every GET checks that this file exists.
    must_exist_on_get: Mutex<Option<PathBuf>>,
    /// If set, every GET checks that the engine holds a slot on this URL's host.
    slot_on_get: Mutex<Option<Url>>,
    stats: Stats,
}

/// Counts cover GETs other than the probes (`Range: bytes=0-0`, or the first MiB) unless noted.
#[derive(Default)]
struct Stats {
    /// Requests of any kind: HEAD, probe or GET.
    requests: AtomicUsize,
    /// Connections being served now, and the most ever served at once.
    open: AtomicUsize,
    max_open: AtomicUsize,
    gets: AtomicUsize,
    probes: AtomicUsize,
    body_bytes: AtomicU64,
    /// Body bytes sent in answer to probes.
    probe_bytes: AtomicU64,
    active: AtomicUsize,
    /// Requests being answered now, probes and HEADs among them (all hold host slots), each until
    /// just before its last write (so never after the client could have seen the whole answer),
    /// and the most ever answered at once.
    serving: AtomicUsize,
    max_serving: AtomicUsize,
    /// GETs refused with 503 for exceeding `max_active`.
    refused: AtomicUsize,
    /// GETs refused for an `expired` target.
    denied: AtomicUsize,
    /// Ranges served with 206, in request order.
    ranges: Mutex<Vec<ByteRange>>,
    missing_on_get: AtomicUsize,
    /// GETs answered while the engine held no slot on `slot_on_get`'s host.
    unslotted: AtomicUsize,
    /// GETs sent with If-Range.
    if_ranges: AtomicUsize,
    /// Requests of any kind (HEAD, probe, GET) that carried an Authorization header.
    authorized: AtomicUsize,
}

impl Mock {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            ranges: true,
            head_claims_ranges: false,
            head_hides_ranges: false,
            empty_head: false,
            head_status: None,
            head_delay: Duration::ZERO,
            latency: Duration::ZERO,
            etag_only_on_head: false,
            rejects_range: false,
            busy_probes: AtomicUsize::new(0),
            chunked: false,
            etag: None,
            disposition: None,
            content_type: None,
            head_disposition: None,
            content_range_total: None,
            unknown_total: false,
            max_active: None,
            redirect: None,
            expired: None,
            plan: |_| Reply::Normal,
            probe_reply: Reply::Normal,
            probe_pause: None,
            delay_us: AtomicU64::new(0),
            must_exist_on_get: Mutex::new(None),
            slot_on_get: Mutex::new(None),
            stats: Stats::default(),
        }
    }

    fn served_ranges(&self) -> Vec<ByteRange> {
        self.stats.ranges.lock().unwrap().clone()
    }
}

struct ActiveGuard<'a>(&'a AtomicUsize);

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Starts the mock; it lives until the test's runtime shuts down. Every mock in this process has
/// a port of its own, below the ephemeral range and never one an earlier mock had: the engine
/// remembers what each host (scheme, host and port) was seen to do, which must not carry over
/// from one test's mock to another's.
async fn serve(mock: Arc<Mock>, file: &str) -> Url {
    static NEXT_PORT: AtomicU16 = AtomicU16::new(20_000);
    let listener = loop {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        assert!(port < 32_768, "out of ports below the ephemeral range");
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            break listener;
        }
    };
    let url = Url::parse(&format!("http://{}/{}", listener.local_addr().unwrap(), file)).unwrap();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(handle(socket, Arc::clone(&mock)));
        }
    });
    url
}

async fn read_head(socket: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 || buf.len() > 64 * KB {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

async fn handle(mut socket: TcpStream, mock: Arc<Mock>) {
    let s = &mock.stats;
    let open = s.open.fetch_add(1, Ordering::SeqCst) + 1;
    s.max_open.fetch_max(open, Ordering::SeqCst);
    let _open = ActiveGuard(&s.open);
    let Some(head) = read_head(&mut socket).await else { return };
    s.requests.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(mock.latency).await;
    let mut request_line = head.lines().next().unwrap_or("").split(' ');
    let method = request_line.next().unwrap_or("").to_string();
    let target = request_line.next().unwrap_or("/").to_string();
    let header = |name: &str| {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    if header("authorization").is_some() {
        s.authorized.fetch_add(1, Ordering::SeqCst);
    }
    let range = header("range");
    let probe = range.as_deref() == Some("bytes=0-0") || range == Some(format!("bytes=0-{}", PREFETCH - 1));
    if let Some(location) = &mock.redirect {
        let n = if method == "HEAD" || probe { 0 } else { s.gets.fetch_add(1, Ordering::SeqCst) + 1 };
        if probe {
            s.probes.fetch_add(1, Ordering::SeqCst);
        }
        let resp = format!("HTTP/1.1 302 Found\r\nLocation: {location}?t={n}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = socket.write_all(resp.as_bytes()).await;
        return;
    }
    let total = mock.data.len();
    let etag = mock.etag.map(|e| format!("ETag: {}\r\n", e)).unwrap_or_default();
    let get_etag = if mock.etag_only_on_head { "" } else { etag.as_str() };
    let accept = if mock.ranges && !mock.chunked { "Accept-Ranges: bytes\r\n" } else { "" };
    let length = |n: usize| if mock.chunked { "Transfer-Encoding: chunked\r\n".to_string() } else { format!("Content-Length: {}\r\n", n) };
    let content_type = mock.content_type.map(|t| format!("Content-Type: {}\r\n", t)).unwrap_or_default();
    let serving = s.serving.fetch_add(1, Ordering::SeqCst) + 1;
    s.max_serving.fetch_max(serving, Ordering::SeqCst);
    let mut serving = Some(ActiveGuard(&s.serving));

    if method == "HEAD" {
        tokio::time::sleep(mock.head_delay).await;
        let resp = if let Some(code) = mock.head_status {
            format!("HTTP/1.1 {} Mock\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", code)
        } else if mock.empty_head {
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        } else {
            let accept = if mock.head_hides_ranges {
                ""
            } else if mock.head_claims_ranges && !mock.chunked {
                "Accept-Ranges: bytes\r\n"
            } else {
                accept
            };
            let disposition = mock.head_disposition.map(|d| format!("Content-Disposition: {}\r\n", d)).unwrap_or_default();
            format!("HTTP/1.1 200 OK\r\n{}{}{}{}{}Connection: close\r\n\r\n", length(total), accept, etag, disposition, content_type)
        };
        drop(serving);
        let _ = socket.write_all(resp.as_bytes()).await;
        return;
    }

    let (reply, _guard) = if probe {
        s.probes.fetch_add(1, Ordering::SeqCst);
        let busy = mock.busy_probes.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
        (if busy { Reply::Status(503, None) } else { mock.probe_reply }, None)
    } else {
        let index = s.gets.fetch_add(1, Ordering::SeqCst);
        let active = s.active.fetch_add(1, Ordering::SeqCst) + 1;
        let guard = ActiveGuard(&s.active);
        if header("if-range").is_some() {
            s.if_ranges.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(path) = mock.must_exist_on_get.lock().unwrap().as_ref() {
            if !path.exists() {
                s.missing_on_get.fetch_add(1, Ordering::SeqCst);
            }
        }
        if let Some(url) = mock.slot_on_get.lock().unwrap().as_ref() {
            // Under a limit of one a slot is free only while the engine holds none.
            if hosts::try_acquire(url, 1).is_some() {
                s.unslotted.fetch_add(1, Ordering::SeqCst);
            }
        }
        let reply = if mock.max_active.is_some_and(|max| active > max) {
            s.refused.fetch_add(1, Ordering::SeqCst);
            Reply::Status(503, Some(1))
        } else if let Some((_, answer)) = mock.expired.filter(|(token, _)| target.contains(token)) {
            s.denied.fetch_add(1, Ordering::SeqCst);
            answer
        } else {
            (mock.plan)(index)
        };
        (reply, Some(guard))
    };
    let reply = if mock.rejects_range && header("range").is_some() { Reply::Status(400, None) } else { reply };
    if let Reply::Status(code, retry_after) = reply {
        let retry = retry_after.map(|s| format!("Retry-After: {}\r\n", s)).unwrap_or_default();
        let resp = format!("HTTP/1.1 {} Mock\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", code, retry);
        drop(serving);
        let _ = socket.write_all(resp.as_bytes()).await;
        return;
    }
    if let Reply::ErrorPage = reply {
        let page = "<html><body>Request has expired</body></html>";
        let resp = format!(
            "HTTP/1.1 200 OK
Content-Type: text/html
Content-Length: {}
Connection: close

{}",
            page.len(),
            page
        );
        drop(serving);
        let _ = socket.write_all(resp.as_bytes()).await;
        return;
    }

    let requested = header("range").and_then(|r| {
        let (a, b) = r.strip_prefix("bytes=")?.split_once('-')?;
        Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()))
    });
    let validator_ok = match header("if-range") {
        Some(v) => mock.etag == Some(v.as_str()),
        None => true,
    };

    let (status, start, end) = match requested {
        Some((a, b)) if mock.ranges && !mock.chunked && validator_ok && !matches!(reply, Reply::IgnoreRange) => {
            if a >= total {
                let resp = format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    total
                );
                drop(serving);
                let _ = socket.write_all(resp.as_bytes()).await;
                return;
            }
            let end = b.unwrap_or(total - 1).min(total - 1);
            if !probe {
                s.ranges.lock().unwrap().push(ByteRange::new(a as u64, end as u64).unwrap());
            }
            (206, a, end + 1)
        }
        _ => (200, 0, total),
    };
    let body = &mock.data[start..end];
    let content_range = if status == 206 {
        let total = if mock.unknown_total { "*".to_string() } else { mock.content_range_total.unwrap_or(total).to_string() };
        format!("Content-Range: bytes {}-{}/{}\r\n", start, end - 1, total)
    } else {
        String::new()
    };
    let disposition = mock.disposition.as_ref().map(|d| format!("Content-Disposition: {}\r\n", d)).unwrap_or_default();
    let resp = format!(
        "HTTP/1.1 {} Mock\r\n{}{}{}{}{}{}Connection: close\r\n\r\n",
        status,
        length(body.len()),
        content_range,
        accept,
        get_etag,
        disposition,
        content_type
    );
    let limit = match reply {
        Reply::CloseAfter(n) | Reply::StallAfter(n) => n.min(body.len()),
        _ => body.len(),
    };
    let pieces = limit.div_ceil(16 * KB);
    if pieces == 0 {
        drop(serving.take());
    }
    if socket.write_all(resp.as_bytes()).await.is_err() {
        return;
    }

    for (i, piece) in body[..limit].chunks(16 * KB).enumerate() {
        let delay = mock.delay_us.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_micros(delay)).await;
        }
        if let Some((_, pause)) = mock.probe_pause.filter(|&(after, _)| probe && i * 16 * KB == after) {
            tokio::time::sleep(pause).await;
        }
        if i + 1 == pieces {
            drop(serving.take());
        }
        let ok = if mock.chunked {
            socket.write_all(format!("{:x}\r\n", piece.len()).as_bytes()).await.is_ok()
                && socket.write_all(piece).await.is_ok()
                && socket.write_all(b"\r\n").await.is_ok()
        } else {
            socket.write_all(piece).await.is_ok()
        };
        if !ok {
            return;
        }
        let counter = if probe { &s.probe_bytes } else { &s.body_bytes };
        counter.fetch_add(piece.len() as u64, Ordering::SeqCst);
    }
    match reply {
        Reply::StallAfter(_) => tokio::time::sleep(Duration::from_secs(120)).await,
        Reply::CloseAfter(_) => {}
        _ if mock.chunked => {
            let _ = socket.write_all(b"0\r\n\r\n").await;
        }
        _ => {}
    }
    let _ = socket.shutdown().await;
}

fn options(out: &Path, connections: usize, chunk: usize) -> DownloadOptions {
    DownloadOptions {
        num_connections: connections,
        base_chunk_size: chunk as u64,
        min_steal_threshold: 32 * KB as u64,
        output_path: Some(out.to_path_buf()),
        ..Default::default()
    }
}

fn assert_file(path: &Path, expected: &[u8]) {
    let actual = std::fs::read(path).unwrap();
    assert_eq!(actual.len(), expected.len(), "{}", path.display());
    assert!(actual == expected, "content of {} differs", path.display());
}

fn assert_no_leftovers(final_path: &Path) {
    let part = part_of(final_path);
    assert!(!part.exists(), "{} left behind", part.display());
    assert!(!DownloadState::state_file_path(&part).exists(), "state left behind");
}

#[tokio::test]
async fn test_multi_threaded_download_and_verification() {
    let _history = setup().await;
    let data = payload(PREFETCH + 4096 * KB, 37);
    let mock = Arc::new(Mock::new(data.clone()));
    let url = serve(Arc::clone(&mock), "test_payload.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("downloaded.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 4, 256 * KB));
    let path = run(&engine, None).await.expect("download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_no_leftovers(&out);
    assert!(mock.served_ranges().len() >= 4, "expected one request per chunk");
}

#[tokio::test]
async fn test_zero_byte_download() {
    let _history = setup().await;
    let mock = Arc::new(Mock::new(Vec::new()));
    let url = serve(mock, "empty.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("empty_downloaded.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
    let path = run(&engine, None).await.expect("0-byte download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &[]);
    assert_no_leftovers(&out);
}

#[tokio::test]
async fn test_zero_byte_download_never_truncates_existing_file() {
    let _history = setup().await;
    let mock = Arc::new(Mock::new(Vec::new()));
    let url = serve(mock, "empty.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("empty.bin");
    std::fs::write(&out, b"precious").unwrap();

    let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
    let path = run(&engine, None).await.expect("download should succeed");

    assert_eq!(path, temp.path().join("empty (1).bin"));
    assert_eq!(std::fs::read(&out).unwrap(), b"precious");
}

#[tokio::test]
async fn test_non_range_server_download() {
    let _history = setup().await;
    let data = payload(PREFETCH + 512 * KB, 19);
    let mut mock = Mock::new(data.clone());
    mock.ranges = false;
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "non_range.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("non_range_downloaded.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 8, 64 * KB));
    let path = run(&engine, None).await.expect("non-range download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_no_leftovers(&out);
}

#[tokio::test]
async fn test_unknown_length_chunked_download() {
    let _history = setup().await;
    let data = payload(300 * KB + 7, 23);
    let mut mock = Mock::new(data.clone());
    mock.chunked = true;
    let url = serve(Arc::new(mock), "stream.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("stream.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 8, 64 * KB));
    let path = run(&engine, None).await.expect("chunked download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_no_leftovers(&out);
}

#[tokio::test]
async fn test_part_file_lifecycle() {
    let _history = setup().await;
    let data = payload(3 * PREFETCH, 29);
    let mock = Arc::new(Mock::new(data.clone()));
    mock.delay_us.store(5_000, Ordering::SeqCst); // ~3.2 MB/s per connection
    let url = serve(Arc::clone(&mock), "lifecycle.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("lifecycle.bin");
    let part = part_of(&out);
    let state = DownloadState::state_file_path(&part);
    *mock.must_exist_on_get.lock().unwrap() = Some(state.clone());

    let engine = DownloadEngine::new(vec![url], options(&out, 2, 256 * KB));
    let task = tokio::spawn(async move { run(&engine, None).await });

    let mut saw_part = false;
    while !task.is_finished() {
        if out.exists() {
            // The rename is the last step: once the final name exists the download is whole.
            assert!(!part.exists(), "final name appeared while the .part still exists");
            assert_eq!(std::fs::metadata(&out).unwrap().len(), data.len() as u64);
        } else if mock.stats.body_bytes.load(Ordering::SeqCst) > 0 {
            saw_part |= part.exists() && state.exists();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let path = task.await.unwrap().expect("download should succeed");

    assert!(saw_part, ".part and its state should exist mid-download");
    assert_eq!(mock.stats.missing_on_get.load(Ordering::SeqCst), 0, "state must be saved before any GET");
    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_no_leftovers(&out);
}

#[tokio::test]
async fn test_resume_from_part_and_state_fetches_only_gaps() {
    let _history = setup().await;
    let size = 2 * PREFETCH;
    let data = payload(size, 41);
    let mut mock = Mock::new(data.clone());
    mock.etag = Some("\"abc\"");
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "resume_test.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("resume_test.bin");
    let part = part_of(&out);

    // A previous run finished a non-aligned prefix beyond the probe's first MiB; the rest of the
    // .part is garbage.
    let done_len = PREFETCH + 128 * KB;
    let done = ByteRange::new(0, done_len as u64 - 1).unwrap();
    let mut on_disk = vec![0xAAu8; size];
    on_disk[..done_len].copy_from_slice(&data[..done_len]);
    std::fs::write(&part, &on_disk).unwrap();
    let mut state = DownloadState::new("resume_test.bin".into(), size as u64, 256 * KB as u64, vec![url.to_string()]);
    state.etag = Some("\"abc\"".into());
    state.completed_ranges.push(done);
    state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();

    let engine = DownloadEngine::new(vec![url], options(&out, 1, 256 * KB));
    let path = run(&engine, None).await.expect("resumed download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_no_leftovers(&out);
    assert_eq!(mock.stats.body_bytes.load(Ordering::SeqCst), (size - done_len) as u64);
    assert!(mock.served_ranges().iter().all(|r| !r.intersects(&done)));
}

#[tokio::test]
async fn test_changed_etag_invalidates_resume() {
    let _history = setup().await;
    let size = 2 * PREFETCH;
    let new_data = payload(size, 43);
    let mut mock = Mock::new(new_data.clone());
    mock.etag = Some("\"v2\"");
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "changed.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("changed.bin");
    let part = part_of(&out);

    std::fs::write(&part, payload(size, 7)).unwrap(); // v1 bytes
    let mut state = DownloadState::new("changed.bin".into(), size as u64, 256 * KB as u64, vec![url.to_string()]);
    state.etag = Some("\"v1\"".into());
    state.completed_ranges.push(ByteRange::new(0, (PREFETCH + 512 * KB) as u64 - 1).unwrap());
    state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();

    // One connection, so work stealing cannot fetch anything twice.
    let engine = DownloadEngine::new(vec![url], options(&out, 1, 128 * KB));
    let path = run(&engine, None).await.expect("download should restart and succeed");

    assert_eq!(path, out);
    assert_file(&out, &new_data);
    let s = &mock.stats;
    let served = s.probe_bytes.load(Ordering::SeqCst) + s.body_bytes.load(Ordering::SeqCst);
    assert_eq!(served, size as u64, "the stale bytes must be fetched again");
}

#[tokio::test]
async fn test_dynamic_work_stealing_integration() {
    let _history = setup().await;
    let data = payload(PREFETCH + 4096 * KB, 53);
    let mock = Arc::new(Mock::new(data.clone()));
    mock.delay_us.store(2_000, Ordering::SeqCst);
    let url = serve(Arc::clone(&mock), "stealing_test.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("stealing_test.bin");

    // After the probe's first MiB, one 4 MiB chunk and four connections: three must steal.
    let engine = DownloadEngine::new(vec![url], options(&out, 4, 4096 * KB));
    let path = run(&engine, None).await.expect("work stealing download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    let starts: std::collections::BTreeSet<u64> = mock.served_ranges().iter().map(|r| r.start).collect();
    assert!(starts.len() > 1, "no work was stolen: {starts:?}");
}

#[tokio::test]
async fn test_fatal_failure_detection() {
    let _history = setup().await;
    let mut mock = Mock::new(payload(2 * PREFETCH, 3));
    mock.plan = |_| Reply::Status(500, None);
    let url = serve(Arc::new(mock), "fatal_test.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("fatal_test.bin");

    let mut opts = options(&out, 2, 512 * KB);
    opts.max_retries = 2;
    let engine = DownloadEngine::new(vec![url], opts);
    let started = Instant::now();
    let err = run(&engine, None).await.expect_err("persistent 500s must fail the download");

    assert!(err.contains("500"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(!out.exists());
}

#[tokio::test]
async fn test_chunk_failure_keeps_progress() {
    let _history = setup().await;
    let data = payload(PREFETCH + 256 * KB, 73);
    let mut mock = Mock::new(data.clone());
    mock.plan = |i| if i == 0 { Reply::CloseAfter(16 * KB) } else { Reply::Normal };
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "retry_test.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("retry_test.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 1, 256 * KB));
    let path = run(&engine, None).await.expect("download should recover from premature EOF");

    assert_eq!(path, out);
    assert_file(&out, &data);
    let ranges = mock.served_ranges();
    assert_eq!(ranges[0].start, PREFETCH as u64);
    assert_eq!(ranges[1].start, (PREFETCH + 16 * KB) as u64, "the retry must continue where the first attempt stopped");
}

#[tokio::test]
async fn test_stalled_connection_is_retried() {
    let _history = setup().await;
    let data = payload(PREFETCH + 256 * KB, 79);
    let mut mock = Mock::new(data.clone());
    mock.plan = |i| if i == 0 { Reply::StallAfter(32 * KB) } else { Reply::Normal };
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "stall.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("stall.bin");

    let mut opts = options(&out, 1, 256 * KB);
    opts.stall_timeout_secs = 1;
    let engine = DownloadEngine::new(vec![url], opts);
    let started = Instant::now();
    let path = run(&engine, None).await.expect("stalled connection should be retried");

    assert!(started.elapsed() < Duration::from_secs(8), "took {:?}", started.elapsed());
    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_eq!(mock.served_ranges()[1].start, (PREFETCH + 32 * KB) as u64);
}

#[tokio::test]
async fn test_503_bursts_with_retry_after_do_not_abort() {
    let _history = setup().await;
    let data = payload(PREFETCH + 4096 * KB, 83);
    let mut mock = Mock::new(data.clone());
    mock.plan = |i| if i < 6 { Reply::Status(503, Some(1)) } else { Reply::Normal };
    let url = serve(Arc::new(mock), "busy.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("busy.bin");

    let mut opts = options(&out, 4, 128 * KB);
    opts.max_retries = 2;
    let engine = DownloadEngine::new(vec![url], opts);
    let path = run(&engine, None).await.expect("a burst of 503s must not abort the download");

    assert_eq!(path, out);
    assert_file(&out, &data);
}

#[tokio::test]
async fn test_server_allowing_fewer_connections_still_completes() {
    let _history = setup().await;
    let data = payload(PREFETCH + 4096 * KB, 89);
    let mut mock = Mock::new(data.clone());
    mock.max_active = Some(2);
    let mock = Arc::new(mock);
    mock.delay_us.store(2_000, Ordering::SeqCst);
    let url = serve(Arc::clone(&mock), "limited.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("limited.bin");

    // Four connections for the 4 MiB after the probe's first MiB.
    let mut opts = options(&out, 8, 256 * KB);
    opts.max_retries = 2;
    let engine = DownloadEngine::new(vec![url], opts);
    let path = run(&engine, None).await.expect("download should adapt to the connection limit");

    assert_eq!(path, out);
    assert_file(&out, &data);
    // Adapting means lowering the connection cap after refusals, then probing one connection
    // above it now and then (about 20 refusals here). An engine that kept all 4 connections
    // retrying is refused hundreds of times.
    let refused = mock.stats.refused.load(Ordering::SeqCst);
    assert!(refused < 64, "{refused} requests refused: the engine did not adapt to the limit");
}

#[tokio::test]
async fn test_cancel_saves_state_and_resume_skips_completed_bytes() {
    let _history = setup().await;
    let size = PREFETCH + 4096 * KB;
    let data = payload(size, 97);
    let mut mock = Mock::new(data.clone());
    mock.etag = Some("\"e1\"");
    let mock = Arc::new(mock);
    mock.delay_us.store(10_000, Ordering::SeqCst);
    let url = serve(Arc::clone(&mock), "cancel.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("cancel.bin");
    let part = part_of(&out);

    let engine = DownloadEngine::new(vec![url.clone()], options(&out, 4, 256 * KB));
    let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(256);
    let canceller = engine.clone();
    let cancel_at = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(s) if s.downloaded_bytes >= s.total_bytes / 4 && s.total_bytes > 0 => break,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => panic!("download ended before cancel"),
            }
        }
        canceller.cancel();
        Instant::now()
    });
    let err = run(&engine, Some(tx)).await.expect_err("cancelled download must return an error");
    let returned = Instant::now();
    let cancelled = cancel_at.await.unwrap();

    assert_eq!(err, "Download cancelled by user");
    assert!(returned.duration_since(cancelled) < Duration::from_secs(2), "cancel took {:?}", returned - cancelled);
    assert!(!out.exists());
    assert!(part.exists());
    let state = DownloadState::load_from_path(&DownloadState::state_file_path(&part)).unwrap().unwrap();
    let saved: u64 = state.completed_ranges.iter().map(ByteRange::len).sum();
    assert!(saved >= (size / 4) as u64 - 64 * KB as u64, "only {saved} bytes recorded");
    for r in &state.completed_ranges {
        let (a, b) = (r.start as usize, r.end as usize + 1);
        assert!(std::fs::read(&part).unwrap()[a..b] == data[a..b], "recorded range {r} is not on disk");
    }

    // Resume with one connection so no bytes are fetched twice by work stealing. First let the
    // server notice that the cancelled connections are gone, so their writes are not counted.
    mock.delay_us.store(0, Ordering::SeqCst);
    let drained = Instant::now();
    while mock.stats.active.load(Ordering::SeqCst) > 0 {
        assert!(drained.elapsed() < Duration::from_secs(10), "cancelled connections are still being served");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (before, probed_before) = (mock.stats.body_bytes.load(Ordering::SeqCst), mock.stats.probe_bytes.load(Ordering::SeqCst));
    let first_new_range = mock.served_ranges().len();
    let engine = DownloadEngine::new(vec![url], options(&out, 1, 256 * KB));
    let path = run(&engine, None).await.expect("resume should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_no_leftovers(&out);
    // The resumed state and the new probe's first bytes are all the workers skip.
    let prefetched = mock.stats.probe_bytes.load(Ordering::SeqCst) - probed_before;
    let mut have = state.completed_ranges.clone();
    have.push(ByteRange::new(0, prefetched - 1).unwrap());
    let have = merge_ranges(have);
    let had: u64 = have.iter().map(ByteRange::len).sum();
    assert_eq!(mock.stats.body_bytes.load(Ordering::SeqCst) - before, size as u64 - had);
    for r in &mock.served_ranges()[first_new_range..] {
        assert!(have.iter().all(|done| !done.intersects(r)), "re-fetched {r}");
    }
}

#[tokio::test]
async fn test_mirror_with_wrong_content_range_total_is_rejected() {
    let _history = setup().await;
    let data = payload(3 * PREFETCH, 101);
    let good = Arc::new(Mock::new(data.clone()));
    let mut bad = Mock::new(vec![0xEE; data.len()]);
    bad.content_range_total = Some(data.len() + 1);
    let bad = Arc::new(bad);
    let other = Arc::new(Mock::new(vec![0xDD; data.len() + 5]));
    let good_url = serve(Arc::clone(&good), "mirror.bin").await;
    let bad_url = serve(Arc::clone(&bad), "mirror.bin").await;
    let other_url = serve(Arc::clone(&other), "mirror.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("mirror.bin");

    let engine = DownloadEngine::new(vec![good_url, bad_url, other_url], options(&out, 4, 64 * KB));
    let path = run(&engine, None).await.expect("download should finish from the good mirror");

    assert_eq!(path, out);
    assert_file(&out, &data);
    // The probe's ranged GET already reveals the wrong total, so no data is ever taken from it.
    assert!(bad.stats.probes.load(Ordering::SeqCst) >= 1, "the bad mirror was never probed");
    assert_eq!(bad.stats.gets.load(Ordering::SeqCst), 0, "a mirror reporting another total must be dropped at probe time");
    assert_eq!(other.stats.gets.load(Ordering::SeqCst), 0, "a mirror with a different size must be dropped at probe time");
}

#[tokio::test]
async fn test_max_speed_is_roughly_honored() {
    let _history = setup().await;
    let data = payload(2 * PREFETCH, 103);
    let url = serve(Arc::new(Mock::new(data.clone())), "slow.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("slow.bin");

    let mut opts = options(&out, 4, 128 * KB);
    opts.max_speed = Some(1024 * KB as u64);
    let engine = DownloadEngine::new(vec![url], opts);
    let started = Instant::now();
    run(&engine, None).await.expect("download should succeed");
    let elapsed = started.elapsed();

    // 2 MiB at 1 MiB/s, the probe's share included, is 2s, minus a little burst allowance.
    assert!(elapsed >= Duration::from_millis(1700), "too fast: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(6), "too slow: {elapsed:?}");
    assert_file(&out, &data);
}

#[tokio::test]
async fn test_redownload_existing_complete_file_does_not_overwrite() {
    isolate_history();
    let _history = HISTORY.write().await;
    let data = payload(1024 * KB, 43);
    let mock = Arc::new(Mock::new(data.clone()));
    let url = serve(Arc::clone(&mock), "complete_file.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("complete_file.bin");
    let opts = options(&out, 4, 256 * KB);

    let engine = DownloadEngine::new(vec![url.clone()], opts.clone());
    assert_eq!(run(&engine, None).await.expect("first download should succeed"), out);
    let modified = std::fs::metadata(&out).unwrap().modified().unwrap();
    let gets = mock.stats.gets.load(Ordering::SeqCst);

    let engine = DownloadEngine::new(vec![url], opts);
    let (tx, mut rx) = broadcast::channel(16);
    let path = run(&engine, Some(tx)).await.expect("second run should detect the finished file");

    assert_eq!(path, out);
    assert_eq!(mock.stats.gets.load(Ordering::SeqCst), gets, "nothing may be downloaded again");
    assert_eq!(std::fs::metadata(&out).unwrap().modified().unwrap(), modified);
    assert_eq!(rx.try_recv().unwrap().progress_ratio, 1.0);
    assert_file(&out, &data);
}

#[tokio::test]
async fn test_redownload_different_file_auto_renames() {
    isolate_history();
    let _history = HISTORY.write().await;
    let data = payload(1024 * KB, 51);
    let url = serve(Arc::new(Mock::new(data.clone())), "collision_test.bin").await;
    let existing = b"Existing pre-allocated unrelated file with different size";

    let first = tempdir().unwrap();
    let out = first.path().join("collision_test.bin");
    std::fs::write(&out, existing).unwrap();
    let engine = DownloadEngine::new(vec![url.clone()], options(&out, 4, 256 * KB));
    let recorded = run(&engine, None).await.expect("download should auto-rename and succeed");
    assert_eq!(recorded, first.path().join("collision_test (1).bin"));
    assert_eq!(std::fs::read(&out).unwrap(), existing);
    assert_file(&recorded, &data);
    assert!(history_entry(&recorded).is_some(), "the download must be recorded");

    // Another directory with the same names, where `collision_test (1).bin` even has the recorded
    // size and URL but other bytes: only a record of this exact path could vouch for it.
    let second = tempdir().unwrap();
    let out = second.path().join("collision_test.bin");
    std::fs::write(&out, existing).unwrap();
    let impostor = payload(1024 * KB, 7);
    let same_name = second.path().join("collision_test (1).bin");
    std::fs::write(&same_name, &impostor).unwrap();
    let engine = DownloadEngine::new(vec![url], options(&out, 4, 256 * KB));
    let path = run(&engine, None).await.expect("download should auto-rename and succeed");

    assert_eq!(path, second.path().join("collision_test (2).bin"));
    assert_file(&path, &data);
    assert_eq!(std::fs::read(&same_name).unwrap(), impostor);
    assert_eq!(std::fs::read(&out).unwrap(), existing);
}

#[tokio::test]
async fn test_existing_unrelated_file_is_never_modified() {
    let _history = setup().await;
    let data = payload(512 * KB, 19);
    let url = serve(Arc::new(Mock::new(data.clone())), "partial_resume.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("partial_resume.bin");

    // Same name and size as the download, but different bytes and no record of downloading it.
    let same_size = payload(512 * KB, 7);
    std::fs::write(&out, &same_size).unwrap();
    // And a same-named prefix of the real data with no state: it may not be "resumed" either.
    let prefix_path = temp.path().join("partial_resume (1).bin");
    std::fs::write(&prefix_path, &data[..100 * KB]).unwrap();

    let engine = DownloadEngine::new(vec![url], options(&out, 4, 128 * KB));
    let path = run(&engine, None).await.expect("download should succeed");

    assert_eq!(path, temp.path().join("partial_resume (2).bin"));
    assert_file(&path, &data);
    assert_eq!(std::fs::read(&out).unwrap(), same_size);
    assert_eq!(std::fs::read(&prefix_path).unwrap(), &data[..100 * KB]);
}

#[tokio::test]
async fn test_output_into_missing_directory() {
    let _history = setup().await;
    let data = payload(200 * KB, 107);
    let url = serve(Arc::new(Mock::new(data.clone())), "nested.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("not").join("yet").join("nested.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 2, 64 * KB));
    let path = run(&engine, None).await.expect("missing directories should be created");

    assert_eq!(path, out);
    assert_file(&out, &data);
}

#[tokio::test]
async fn test_missing_file_fails_fast() {
    let _history = setup().await;
    for ranges in [true, false] {
        let mut mock = Mock::new(payload(PREFETCH + 512 * KB, 109));
        mock.ranges = ranges;
        mock.plan = |_| Reply::Status(404, None);
        if !ranges {
            // The probe's answer would be the download itself: the file is gone by then, and only
            // HEAD (from a stale cache, say) still finds it.
            mock.probe_reply = Reply::Status(404, None);
        }
        let url = serve(Arc::new(mock), "gone.bin").await;
        let temp = tempdir().unwrap();
        let out = temp.path().join("gone.bin");

        let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
        let started = Instant::now();
        let err = run(&engine, None).await.expect_err("a 404 must fail the download");

        assert!(err.contains("404"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?} (ranges: {ranges})", started.elapsed());
    }
}

/// Lists a directory's file names, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[tokio::test]
async fn test_concurrent_downloads_of_same_name_get_separate_files() {
    let _history = setup().await;
    let (a, b) = (payload(512 * KB, 3), payload(512 * KB, 5));
    let mocks: Vec<Arc<Mock>> = [&a, &b]
        .into_iter()
        .map(|data| {
            let mut mock = Mock::new(data.clone());
            mock.ranges = false; // single stream: the .part appears only once the response arrives
            mock.delay_us.store(2_000, Ordering::SeqCst);
            Arc::new(mock)
        })
        .collect();
    let url_a = serve(Arc::clone(&mocks[0]), "file.bin").await;
    let url_b = serve(Arc::clone(&mocks[1]), "file.bin").await;
    let temp = tempdir().unwrap();

    let engine_a = DownloadEngine::new(vec![url_a], options(temp.path(), 4, 64 * KB));
    let engine_b = DownloadEngine::new(vec![url_b], options(temp.path(), 4, 64 * KB));
    let (res_a, res_b) = tokio::join!(run(&engine_a, None), run(&engine_b, None));
    let (path_a, path_b) = (res_a.expect("download A"), res_b.expect("download B"));

    assert_ne!(path_a, path_b);
    assert_file(&path_a, &a);
    assert_file(&path_b, &b);
    assert_eq!(names_in(temp.path()), ["file (1).bin", "file.bin"], "no .part, state or lock may be left");
}

#[tokio::test]
async fn test_output_directory_that_does_not_exist_yet() {
    let _history = setup().await;
    let data = payload(200 * KB, 111);
    let url = serve(Arc::new(Mock::new(data.clone())), "server_name.bin").await;
    let temp = tempdir().unwrap();
    let dir = temp.path().join("newdir");
    let mut dir_arg = dir.clone().into_os_string();
    dir_arg.push(std::path::MAIN_SEPARATOR_STR);

    let engine = DownloadEngine::new(vec![url], options(Path::new(&dir_arg), 2, 64 * KB));
    let path = run(&engine, None).await.expect("the directory should be created");

    assert_eq!(path, dir.join("server_name.bin"));
    assert_file(&path, &data);
}

#[tokio::test]
async fn test_server_ignoring_ranges_it_advertises_falls_back_to_one_stream() {
    let _history = setup().await;
    let data = payload(PREFETCH + 512 * KB, 113);
    let mut mock = Mock::new(data.clone());
    mock.ranges = false;
    mock.head_claims_ranges = true;
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "liar.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("liar.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
    let path = run(&engine, None).await.expect("a plain GET works, so the download must too");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_eq!(mock.stats.gets.load(Ordering::SeqCst), 0, "the probe's answer was the one stream, no range requests");
}

#[tokio::test]
async fn test_a_single_stream_goes_on_with_the_probes_answer() {
    let _history = setup().await;
    let data = payload(PREFETCH + 512 * KB, 257);
    const CUT: usize = 256 * KB;
    // A server ignoring ranges, one that cannot say how long the file is, and a probe answer that
    // breaks off, so the stream has to ask again.
    for (chunked, probe_reply, gets) in [(false, Reply::Normal, 0), (true, Reply::Normal, 0), (false, Reply::CloseAfter(CUT), 1)] {
        let mut mock = Mock::new(data.clone());
        mock.ranges = false;
        mock.chunked = chunked;
        mock.probe_reply = probe_reply;
        mock.delay_us.store(5_000, Ordering::SeqCst);
        let mock = Arc::new(mock);
        let url = serve(Arc::clone(&mock), "whole.bin").await;
        let temp = tempdir().unwrap();
        let out = temp.path().join("whole.bin");

        let opts = DownloadOptions { max_connections_per_host: 1, ..options(&out, 4, 64 * KB) };
        let engine = DownloadEngine::new(vec![url.clone()], opts);
        let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(256);
        // Whichever answer it reads, the stream holds its host's one slot. (Past the cut: a
        // snapshot of the broken answer may be looked at only once that answer is given up.)
        let watch = async {
            let mut seen = 0;
            while let Ok(snapshot) = rx.recv().await {
                if (CUT as u64 + 1..data.len() as u64).contains(&snapshot.downloaded_bytes) {
                    assert!(hosts::try_acquire(&url, 1).is_none(), "the stream holds no slot");
                    seen += 1;
                }
            }
            seen
        };
        let (done, seen) = tokio::join!(run(&engine, Some(tx)), watch);
        done.expect("download should succeed");

        assert_file(&out, &data);
        // Taken while writing: a broken answer's bytes are not in it.
        let recorded = history_entry(&out).and_then(|entry| entry.blake3_hash);
        assert_eq!(recorded.as_deref(), Some(blake3::hash(&data).to_hex().as_str()), "probe: {probe_reply:?}");
        assert!(seen > 0, "the stream was never seen under way");
        assert_eq!(mock.stats.gets.load(Ordering::SeqCst), gets, "chunked: {chunked}, probe: {probe_reply:?}");
    }
}

#[tokio::test]
async fn test_unknown_total_in_content_range_uses_one_stream() {
    let _history = setup().await;
    let data = payload(300 * KB, 127);
    let mut mock = Mock::new(data.clone());
    mock.unknown_total = true;
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "star.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("star.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
    let path = run(&engine, None).await.expect("download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_eq!(mock.stats.gets.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_busy_probe_is_retried_instead_of_losing_ranges() {
    let _history = setup().await;
    let data = payload(PREFETCH + 512 * KB, 151);
    let mock = Arc::new(Mock::new(data.clone()));
    mock.busy_probes.store(1, Ordering::SeqCst);
    let url = serve(Arc::clone(&mock), "busy_probe.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("busy_probe.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
    run(&engine, None).await.expect("download should succeed");

    assert_file(&out, &data);
    assert_eq!(mock.stats.probes.load(Ordering::SeqCst), 2);
    assert!(mock.served_ranges().len() >= 8, "a momentary 503 must not demote the download to one stream");
}

#[tokio::test]
async fn test_probe_busy_on_every_try_never_loses_the_progress() {
    let _history = setup().await;
    let size = 2 * PREFETCH;
    let data = payload(size, 193);
    let done_len = size - 200 * KB;
    for head_hides_ranges in [true, false] {
        let mut mock = Mock::new(data.clone());
        mock.etag = Some("\"b1\"");
        mock.head_hides_ranges = head_hides_ranges;
        // HEAD answers, but every try of the ranged probe finds the server busy: that says
        // nothing about range support.
        mock.busy_probes = AtomicUsize::new(100);
        let mock = Arc::new(mock);
        let url = serve(Arc::clone(&mock), "busy_resume.bin").await;
        let temp = tempdir().unwrap();
        let out = temp.path().join("busy_resume.bin");
        let part = part_of(&out);

        // A previous run got 90% of the file.
        let mut on_disk = vec![0u8; size];
        on_disk[..done_len].copy_from_slice(&data[..done_len]);
        std::fs::write(&part, &on_disk).unwrap();
        let mut state =
            DownloadState::new("busy_resume.bin".into(), size as u64, 256 * KB as u64, vec![url.to_string()]);
        state.etag = Some("\"b1\"".into());
        state.completed_ranges.push(ByteRange::new(0, done_len as u64 - 1).unwrap());
        state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();

        if head_hides_ranges {
            // Nothing says the server takes ranges, so the progress cannot resume yet: it is
            // kept, never replaced by a single stream from the start.
            let engine = DownloadEngine::new(vec![url.clone()], options(&out, 4, 64 * KB));
            let err = run(&engine, None).await.unwrap_err();
            assert!(err.contains("kept"), "{err}");
            assert_eq!(std::fs::read(&part).unwrap(), on_disk, "the progress must survive");
            assert!(DownloadState::state_file_path(&part).exists());
            assert_eq!(mock.stats.gets.load(Ordering::SeqCst), 0, "no single-stream fallback");
            mock.busy_probes.store(0, Ordering::SeqCst);
        }
        // HEAD's Accept-Ranges, or else the recovered server, resumes the download.
        let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
        run(&engine, None).await.expect("the download resumes");
        assert_file(&out, &data);
        assert_eq!(mock.stats.body_bytes.load(Ordering::SeqCst), (size - done_len) as u64, "only the missing bytes");
    }
}

#[tokio::test]
async fn test_fresh_download_completes_although_every_probe_is_busy() {
    let _history = setup().await;
    let data = payload(PREFETCH + 512 * KB, 199);
    // The server answers 503 to the probes' range requests but serves the file: with ranges
    // when HEAD advertises them, else as one stream.
    for head_hides_ranges in [false, true] {
        let mut mock = Mock::new(data.clone());
        mock.head_hides_ranges = head_hides_ranges;
        mock.busy_probes = AtomicUsize::new(100);
        let mock = Arc::new(mock);
        let url = serve(Arc::clone(&mock), "throttled.bin").await;
        let temp = tempdir().unwrap();
        let out = temp.path().join("throttled.bin");

        let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
        run(&engine, None).await.expect("a busy probe must not fail a download the server serves");
        assert_file(&out, &data);
        assert_no_leftovers(&out);
        if head_hides_ranges {
            assert_eq!(mock.stats.gets.load(Ordering::SeqCst), 1, "one stream");
        } else {
            assert!(mock.served_ranges().len() >= 8, "HEAD's Accept-Ranges keeps the connections");
        }
    }
}

#[tokio::test]
async fn test_partial_download_resumes_from_the_mirror_that_takes_ranges() {
    let _history = setup().await;
    let size = 2 * PREFETCH;
    let data = payload(size, 211);
    // The first mirror ignores Range (a CDN edge, say); the second honours it.
    let mut edge = Mock::new(data.clone());
    edge.ranges = false;
    let edge = Arc::new(edge);
    let origin = Arc::new(Mock::new(data.clone()));
    let edge_url = serve(Arc::clone(&edge), "mirrored.bin").await;
    let origin_url = serve(Arc::clone(&origin), "mirrored.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("mirrored.bin");
    let part = part_of(&out);

    // A previous run, while the first mirror was down, got most of the file from the second.
    let done_len = size - 300 * KB;
    let mut on_disk = vec![0u8; size];
    on_disk[..done_len].copy_from_slice(&data[..done_len]);
    std::fs::write(&part, &on_disk).unwrap();
    let urls = vec![edge_url.to_string(), origin_url.to_string()];
    let mut state = DownloadState::new("mirrored.bin".into(), size as u64, 256 * KB as u64, urls);
    state.completed_ranges.push(ByteRange::new(0, done_len as u64 - 1).unwrap());
    state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();

    let engine = DownloadEngine::new(vec![edge_url, origin_url], options(&out, 4, 64 * KB));
    run(&engine, None).await.expect("the mirror taking ranges resumes the download");
    assert_file(&out, &data);
    assert_eq!(edge.stats.gets.load(Ordering::SeqCst), 0, "the first mirror only answered the probe");
    assert_eq!(origin.stats.body_bytes.load(Ordering::SeqCst), (size - done_len) as u64, "only the missing bytes");
}

#[tokio::test]
async fn test_head_that_never_answers_does_not_hold_up_the_download() {
    let _history = setup().await;
    let data = payload(PREFETCH + 256 * KB, 197);
    let mut mock = Mock::new(data.clone());
    mock.head_delay = Duration::from_secs(120);
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "no_head.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("no_head.bin");

    let started = Instant::now();
    let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
    run(&engine, None).await.expect("the ranged GET alone is enough");
    assert_file(&out, &data);
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
}

#[tokio::test]
async fn test_server_rejecting_head_and_ranges_is_fetched_with_a_plain_get() {
    let _history = setup().await;
    let data = payload(256 * KB, 157);
    let mut mock = Mock::new(data.clone());
    mock.head_status = Some(405);
    mock.rejects_range = true;
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "picky.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("picky.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
    let path = run(&engine, None).await.expect("a plain GET works, so the download must too");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_eq!(mock.stats.gets.load(Ordering::SeqCst), 1, "the probe's plain GET brought the whole file");
}

#[tokio::test]
async fn test_get_probe_supplies_name_and_validator_missing_from_head() {
    let _history = setup().await;
    let data = payload(PREFETCH + 512 * KB, 131);
    let mut mock = Mock::new(data.clone());
    mock.empty_head = true;
    mock.etag = Some("\"real-v1\"");
    mock.disposition = Some("attachment; filename=\"real.zip\"".to_string());
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "dl.cgi").await;
    let temp = tempdir().unwrap();

    let engine = DownloadEngine::new(vec![url], options(temp.path(), 4, 64 * KB));
    let path = run(&engine, None).await.expect("download should succeed");

    assert_eq!(path, temp.path().join("real.zip"));
    assert_file(&path, &data);
    let (gets, validated) = (mock.stats.gets.load(Ordering::SeqCst), mock.stats.if_ranges.load(Ordering::SeqCst));
    assert!(gets > 1 && validated == gets, "range requests must carry If-Range ({validated} of {gets})");
}

#[tokio::test]
async fn test_long_server_file_name_is_shortened() {
    let _history = setup().await;
    let data = payload(64 * KB, 137);
    let mut mock = Mock::new(data.clone());
    mock.disposition = Some(format!("attachment; filename=\"{}.bin\"", "a".repeat(245)));
    let url = serve(Arc::new(mock), "dl").await;
    let temp = tempdir().unwrap();

    let engine = DownloadEngine::new(vec![url], options(temp.path(), 2, 64 * KB));
    let path = run(&engine, None).await.expect("a long server name must not break the download");

    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(name.len() <= 200 && name.starts_with("aaaa") && name.ends_with(".bin"), "{name}");
    assert_file(&path, &data);
}

#[tokio::test]
async fn test_range_ignored_once_under_if_range_is_retried() {
    let _history = setup().await;
    let data = payload(PREFETCH + 512 * KB, 139);
    let mut mock = Mock::new(data.clone());
    mock.etag = Some("\"same\"");
    // The second chunk's request gets the whole, unchanged file (same ETag) with 200.
    mock.plan = |i| if i == 1 { Reply::IgnoreRange } else { Reply::Normal };
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "cdn.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("cdn.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 1, 128 * KB));
    let path = run(&engine, None).await.expect("an unchanged file is no reason to abort");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_eq!(mock.served_ranges().len() + 1, mock.stats.gets.load(Ordering::SeqCst), "exactly one ignored range");
}

#[tokio::test]
async fn test_authorization_reaches_the_user_host() {
    let _history = setup().await;
    let data = payload(PREFETCH + 256 * KB, 149);
    let mock = Arc::new(Mock::new(data.clone()));
    let url = serve(Arc::clone(&mock), "private.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("private.bin");

    let mut opts = options(&out, 2, 64 * KB);
    opts.auth_header = Some("Bearer token".to_string());
    let engine = DownloadEngine::new(vec![url], opts);
    run(&engine, None).await.expect("download should succeed");

    assert_file(&out, &data);
    let s = &mock.stats;
    assert!(s.gets.load(Ordering::SeqCst) > 0);
    assert_eq!(
        s.authorized.load(Ordering::SeqCst),
        s.requests.load(Ordering::SeqCst),
        "every request to the user's host is authorized"
    );
}

#[tokio::test]
async fn test_small_file_takes_one_get_and_no_worker() {
    let _history = setup().await;
    for (size, ranges) in [(PREFETCH, true), (300 * KB, false)] {
        let data = payload(size, 163);
        let mut mock = Mock::new(data.clone());
        mock.ranges = ranges;
        mock.head_delay = Duration::from_millis(300);
        let mock = Arc::new(mock);
        let url = serve(Arc::clone(&mock), "small.bin").await;
        let temp = tempdir().unwrap();
        let out = temp.path().join("small.bin");

        let engine = DownloadEngine::new(vec![url], options(&out, 16, 64 * KB));
        let path = run(&engine, None).await.expect("download should succeed");

        assert_eq!(path, out);
        assert_file(&out, &data);
        assert_no_leftovers(&out);
        let s = &mock.stats;
        let counts = (s.requests.load(Ordering::SeqCst), s.probes.load(Ordering::SeqCst), s.gets.load(Ordering::SeqCst));
        assert_eq!(counts, (2, 1, 0), "one HEAD and one GET, nothing else (ranges: {ranges})");
        assert_eq!(s.max_open.load(Ordering::SeqCst), 2, "HEAD and GET must be in flight together");
    }
}

#[tokio::test]
async fn test_file_the_probe_brought_whole_is_written_plainly_without_state() {
    isolate_history();
    let _history = HISTORY.write().await;
    let data = payload(300 * KB, 211);
    let url = serve(Arc::new(Mock::new(data.clone())), "whole.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("whole.bin");
    // Such a file has nothing to resume, so no state is saved: one that cannot be changes nothing.
    std::fs::create_dir(DownloadState::state_file_path(&part_of(&out))).unwrap();

    let engine = DownloadEngine::new(vec![url], options(&out, 4, 64 * KB));
    let path = run(&engine, None).await.expect("download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert!(!part_of(&out).exists());
    let entry = history_entry(&out).expect("the download is recorded");
    assert_eq!(entry.blake3_hash, Some(blake3::hash(&data).to_hex().to_string()));
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
        let attributes = std::fs::metadata(&out).unwrap().file_attributes();
        assert_eq!(attributes & FILE_ATTRIBUTE_SPARSE_FILE, 0, "a small file is written plainly, not as a sparse file");
    }
}

#[tokio::test]
async fn test_sha256_checksum_of_a_multi_connection_download() {
    use sha2::Digest;
    isolate_history();
    let _history = HISTORY.write().await;
    let data = payload(4 * PREFETCH + 12345, 227);
    let sha256: String = sha2::Sha256::digest(&data).iter().map(|b| format!("{:02x}", b)).collect();
    let mock = Arc::new(Mock::new(data.clone()));
    let url = serve(Arc::clone(&mock), "summed.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("summed.bin");

    let mut opts = options(&out, 4, 256 * KB);
    opts.expected_checksum = Some(format!("sha256:{}", "0".repeat(64)));
    let err = run(&DownloadEngine::new(vec![url.clone()], opts.clone()), None).await.expect_err("a wrong checksum fails");
    assert!(err.contains("Checksum verification failed") && err.contains(&sha256), "{err}");
    assert!(!out.exists());
    assert_no_leftovers(&out);

    opts.expected_checksum = Some(format!("sha256:{}", sha256));
    let path = run(&DownloadEngine::new(vec![url], opts), None).await.expect("the right checksum passes");
    assert_eq!(path, out);
    assert_file(&out, &data);
    assert!(mock.served_ranges().len() >= 4, "fetched over several connections");
    let entry = history_entry(&out).expect("the download is recorded");
    assert_eq!(entry.blake3_hash, Some(blake3::hash(&data).to_hex().to_string()));
}

#[tokio::test]
async fn test_resumed_download_is_hashed_whole() {
    use sha2::Digest;
    isolate_history();
    let _history = HISTORY.write().await;
    let size = 3 * PREFETCH;
    let data = payload(size, 229);
    let md5: String = md5::Md5::digest(&data).iter().map(|b| format!("{:02x}", b)).collect();
    let mut mock = Mock::new(data.clone());
    mock.etag = Some("\"m1\"");
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "resumed.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("resumed.bin");
    let part = part_of(&out);

    // An earlier run finished two unaligned stretches, one of them where the probe's MiB ends; the
    // rest of the .part is junk.
    let done = [(PREFETCH / 2, PREFETCH + 3 * KB + 5), (2 * PREFETCH + 7, 2 * PREFETCH + 300 * KB)];
    let mut on_disk = vec![0xAAu8; size];
    let mut state = DownloadState::new("resumed.bin".into(), size as u64, 256 * KB as u64, vec![url.to_string()]);
    state.etag = Some("\"m1\"".into());
    for (from, to) in done {
        on_disk[from..to].copy_from_slice(&data[from..to]);
        state.completed_ranges.push(ByteRange::new(from as u64, to as u64 - 1).unwrap());
    }
    std::fs::write(&part, &on_disk).unwrap();
    state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();

    let mut opts = options(&out, 3, 256 * KB);
    opts.expected_checksum = Some(format!("md5:{}", md5));
    let path = run(&DownloadEngine::new(vec![url], opts), None).await.expect("resumed download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_no_leftovers(&out);
    let entry = history_entry(&out).expect("the download is recorded");
    assert_eq!(entry.blake3_hash, Some(blake3::hash(&data).to_hex().to_string()));
}

#[tokio::test]
async fn test_connections_follow_the_size_of_the_file_on_a_fast_server() {
    let _history = setup().await;
    let data = payload(3 * PREFETCH, 167);
    let mut mock = Mock::new(data.clone());
    // Connections take 50 ms to answer, then the probe's MiB arrives before any rate is measured:
    // a connection per MiB, as more would spend longer starting than fetching.
    mock.latency = Duration::from_millis(50);
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "three.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("three.bin");

    let opts = DownloadOptions { num_connections: 16, output_path: Some(out.clone()), ..Default::default() };
    let engine = DownloadEngine::new(vec![url], opts);
    run(&engine, None).await.expect("download should succeed");

    assert_file(&out, &data);
    assert!(mock.served_ranges().len() >= 2, "the rest is still fetched in parallel");
    let most = mock.stats.max_open.load(Ordering::SeqCst);
    assert!(most <= 3, "{most} connections at once for a 3 MiB file");
}

#[tokio::test]
async fn test_prefetched_start_is_not_downloaded_again() {
    let _history = setup().await;
    let data = payload(3 * PREFETCH + 123, 173);
    let mut mock = Mock::new(data.clone());
    mock.etag = Some("\"p1\"");
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "big.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("big.bin");

    // One connection, so work stealing cannot fetch anything twice either.
    let engine = DownloadEngine::new(vec![url], options(&out, 1, 256 * KB));
    run(&engine, None).await.expect("download should succeed");

    assert_file(&out, &data);
    let s = &mock.stats;
    let (probed, fetched) = (s.probe_bytes.load(Ordering::SeqCst), s.body_bytes.load(Ordering::SeqCst));
    assert_eq!(probed, PREFETCH as u64);
    assert_eq!(probed + fetched, data.len() as u64, "every byte is served once");
    assert!(mock.served_ranges().iter().all(|r| r.start >= PREFETCH as u64));
}

#[tokio::test]
async fn test_resume_with_prefetch_overlapping_completed_state() {
    let _history = setup().await;
    let size = 3 * PREFETCH;
    let data = payload(size, 179);
    let mut mock = Mock::new(data.clone());
    mock.etag = Some("\"r1\"");
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "overlap.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("overlap.bin");
    let part = part_of(&out);

    // A previous run finished [512 KiB, 1.5 MiB), which the probe's first MiB overlaps. The rest
    // of the .part, including the start the probe brings again, is garbage.
    let (from, to) = (PREFETCH / 2, PREFETCH + PREFETCH / 2);
    let mut on_disk = vec![0xAAu8; size];
    on_disk[from..to].copy_from_slice(&data[from..to]);
    std::fs::write(&part, &on_disk).unwrap();
    let mut state = DownloadState::new("overlap.bin".into(), size as u64, 256 * KB as u64, vec![url.to_string()]);
    state.etag = Some("\"r1\"".into());
    state.completed_ranges.push(ByteRange::new(from as u64, to as u64 - 1).unwrap());
    state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();

    let engine = DownloadEngine::new(vec![url], options(&out, 1, 256 * KB));
    let path = run(&engine, None).await.expect("resumed download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_no_leftovers(&out);
    let have = ByteRange::new(0, to as u64 - 1).unwrap();
    assert_eq!(mock.stats.probe_bytes.load(Ordering::SeqCst), PREFETCH as u64);
    assert_eq!(mock.stats.body_bytes.load(Ordering::SeqCst), (size - to) as u64);
    assert!(mock.served_ranges().iter().all(|r| !r.intersects(&have)), "{:?}", mock.served_ranges());
}

#[tokio::test]
async fn test_a_capped_probe_hands_the_rest_to_workers() {
    let _history = setup().await;
    // ~3 MB/s per connection, and new connections start at once: even a file the probe could
    // bring alone is fetched over several, the probe's own connection among them.
    for size in [PREFETCH, 3 * PREFETCH] {
        let data = payload(size, 191);
        let mock = Arc::new(Mock::new(data.clone()));
        mock.delay_us.store(5_000, Ordering::SeqCst);
        let url = serve(Arc::clone(&mock), "slow_probe.bin").await;
        let temp = tempdir().unwrap();
        let out = temp.path().join("slow_probe.bin");

        let engine = DownloadEngine::new(vec![url], options(&out, 4, 256 * KB));
        // The download's first snapshot, before any worker starts, has what the probe read.
        let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(256);
        let (done, first) = tokio::join!(run(&engine, Some(tx)), rx.recv());
        done.expect("download should succeed");

        assert_file(&out, &data);
        let read = first.expect("a snapshot").downloaded_bytes;
        assert!(read < size as u64 / 2, "workers waited for {read} of {size} bytes from the probe");
        assert!(mock.served_ranges().len() >= 2, "{size}: the rest is fetched in parallel");
        // The probe's answer is not dropped when the workers start: it goes on as the first chunk
        // (and keeps at least half of it, whatever other workers steal).
        let probed = mock.stats.probe_bytes.load(Ordering::SeqCst);
        assert!(probed >= read + 64 * KB as u64, "{size}: the probe's answer stopped at {probed} bytes, {read} of them read before the workers started");
    }
}

#[tokio::test]
async fn test_a_probe_cut_short_goes_on_as_the_first_chunk() {
    let _history = setup().await;
    let data = payload(PREFETCH + 512 * KB, 257);
    // The probe's answer pauses after 64 KiB, past the time the download waits for it: the
    // download starts without the rest, which that answer still brings, on one connection or
    // with a speed limit alike.
    for (connections, max_speed) in [(1, None), (4, Some(2048 * KB as u64))] {
        let mut mock = Mock::new(data.clone());
        mock.probe_pause = Some((64 * KB, Duration::from_millis(600)));
        let mock = Arc::new(mock);
        let url = serve(Arc::clone(&mock), "cut.bin").await;
        let temp = tempdir().unwrap();
        let out = temp.path().join("cut.bin");

        let engine = DownloadEngine::new(vec![url], DownloadOptions { max_speed, ..options(&out, connections, 128 * KB) });
        run(&engine, None).await.expect("download should succeed");

        assert_file(&out, &data);
        let starts: Vec<u64> = mock.served_ranges().iter().map(|r| r.start).collect();
        assert!(!starts.contains(&(64 * KB as u64)), "{connections}: the bytes after the probe's were asked for again: {starts:?}");
        assert!(starts.contains(&(192 * KB as u64)), "{connections}: the first chunk is the rest of the probe's answer: {starts:?}");
    }
}

#[tokio::test]
async fn test_prefetch_is_dropped_when_its_response_lacks_the_validator() {
    let _history = setup().await;
    let data = payload(PREFETCH + 512 * KB, 181);
    let mut mock = Mock::new(data.clone());
    mock.etag = Some("\"h1\"");
    mock.etag_only_on_head = true;
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "head_etag.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("head_etag.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 1, 256 * KB));
    run(&engine, None).await.expect("download should succeed");

    assert_file(&out, &data);
    // Nothing ties the probe's bytes to the ETag the download validates with, so they are
    // fetched again under If-Range.
    assert_eq!(mock.stats.body_bytes.load(Ordering::SeqCst), data.len() as u64);
    assert_eq!(mock.served_ranges()[0].start, 0);
}

const ONE_SEGMENT_PLAYLIST: &str ="#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:1,\nseg.ts\n#EXT-X-ENDLIST\n";

#[tokio::test]
async fn test_empty_hls_playlist_is_an_error_not_a_text_download() {
    let _history = setup().await;
    let playlist = b"#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-ENDLIST\n".to_vec();
    let url = serve(Arc::new(Mock::new(playlist)), "index.m3u8").await;
    let temp = tempdir().unwrap();

    let engine = DownloadEngine::new(vec![url], options(temp.path(), 2, 64 * KB));
    let err = run(&engine, None).await.expect_err("a stream without segments cannot succeed");

    assert!(err.contains("segments"), "{err}");
    assert!(names_in(temp.path()).is_empty(), "the playlist text must not be saved");
}

#[tokio::test]
async fn test_hls_download_verifies_checksum_and_records_history() {
    isolate_history();
    let _history = HISTORY.write().await;
    // Every path of the mock serves this text, so the single segment is the playlist itself.
    let text = ONE_SEGMENT_PLAYLIST.as_bytes().to_vec();
    let url = serve(Arc::new(Mock::new(text.clone())), "index.m3u8").await;
    let temp = tempdir().unwrap();

    let mut opts = options(temp.path(), 2, 64 * KB);
    opts.expected_checksum = Some(format!("blake3:{}", "0".repeat(64)));
    let engine = DownloadEngine::new(vec![url.clone()], opts.clone());
    let err = run(&engine, None).await.expect_err("a wrong checksum must fail the download");
    assert!(err.contains("Checksum verification failed"), "{err}");
    assert!(history_entry(&temp.path().join("index.ts")).is_none());

    let blake3 = blake3::hash(&text).to_hex().to_string();
    opts.expected_checksum = Some(blake3.clone());
    let engine = DownloadEngine::new(vec![url], opts);
    let path = run(&engine, None).await.expect("the right checksum passes");
    assert_file(&path, &text);
    let entry = history_entry(&path).expect("HLS downloads are recorded");
    assert_eq!(entry.status, HistoryStatus::Completed);
    assert_eq!(entry.blake3_hash.as_deref(), Some(blake3.as_str()));
}

#[tokio::test]
async fn test_slow_hls_playlist_is_not_cut_off_by_the_probe_timeout() {
    let _history = setup().await;
    let mut mock = Mock::new(ONE_SEGMENT_PLAYLIST.as_bytes().to_vec());
    // The playlist takes 16s to arrive (well within HLS's own stall timeout); the segment is gone.
    mock.plan = |i| if i == 0 { Reply::Normal } else { Reply::Status(404, None) };
    mock.delay_us.store(16_000_000, Ordering::SeqCst);
    let url = serve(Arc::new(mock), "slow.m3u8").await;
    let temp = tempdir().unwrap();

    let engine = DownloadEngine::new(vec![url], options(temp.path(), 2, 64 * KB));
    let err = run(&engine, None).await.expect_err("the segment is missing");

    assert!(!err.contains("Timed out"), "the playlist fetch was cut off: {err}");
    assert!(err.contains("404"), "{err}");
}

#[tokio::test]
async fn test_connection_quiet_mid_body_is_replaced_long_before_the_stall_timeout() {
    let _history = setup().await;
    let data = payload(PREFETCH + 256 * KB, 229);
    let mut mock = Mock::new(data.clone());
    mock.plan = |i| if i == 0 { Reply::StallAfter(32 * KB) } else { Reply::Normal };
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "quiet.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("quiet.bin");

    // The default 30 s stall timeout: only the answer's wait gets that long, not a quiet body.
    let engine = DownloadEngine::new(vec![url], options(&out, 1, 256 * KB));
    let started = Instant::now();
    run(&engine, None).await.expect("the quiet connection is replaced");

    assert!(started.elapsed() < Duration::from_secs(12), "took {:?}", started.elapsed());
    assert_file(&out, &data);
    assert_eq!(mock.served_ranges()[1].start, (PREFETCH + 32 * KB) as u64, "the retry continues where the first stopped");
}

#[tokio::test]
async fn test_idle_worker_takes_over_a_silent_chunk() {
    let _history = setup().await;
    let data = payload(PREFETCH + 4096 * KB, 241);
    let mut mock = Mock::new(data.clone());
    // Less than a steal takes (32 KiB here), so part of the chunk stays with the silent connection.
    mock.plan = |i| if i == 0 { Reply::StallAfter(16 * KB) } else { Reply::Normal };
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "silent.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("silent.bin");

    // The others finish at once and sit idle; one of them takes over from the silent connection
    // after about 2 s, well before its own 5 s idle timeout would end it.
    let engine = DownloadEngine::new(vec![url], options(&out, 4, 256 * KB));
    let started = Instant::now();
    run(&engine, None).await.expect("download should succeed");

    assert!(started.elapsed() < Duration::from_millis(4500), "took {:?}", started.elapsed());
    assert_file(&out, &data);
}

/// A mirror at a redirector (`/latest.bin`) that sends every request to `file`, the real server.
async fn redirected_to(file: &Arc<Mock>) -> (Arc<Mock>, Url) {
    let target = serve(Arc::clone(file), "real.bin").await;
    let mut redirector = Mock::new(Vec::new());
    redirector.redirect = Some(target.to_string());
    let redirector = Arc::new(redirector);
    let url = serve(Arc::clone(&redirector), "latest.bin").await;
    (redirector, url)
}

#[tokio::test]
async fn test_chunk_requests_go_straight_to_the_redirect_target() {
    let _history = setup().await;
    let data = payload(PREFETCH + 1024 * KB, 227);
    let file = Arc::new(Mock::new(data.clone()));
    let (redirector, url) = redirected_to(&file).await;
    let temp = tempdir().unwrap();

    let engine = DownloadEngine::new(vec![url], options(temp.path(), 4, 128 * KB));
    let path = run(&engine, None).await.expect("download should succeed");

    assert_eq!(path, temp.path().join("real.bin"));
    assert_file(&path, &data);
    assert!(file.stats.gets.load(Ordering::SeqCst) >= 2, "the chunks come from the redirect target");
    assert_eq!(redirector.stats.requests.load(Ordering::SeqCst), 2, "only the probe's HEAD and GET are redirected");
}

#[tokio::test]
async fn test_a_request_waiting_for_a_slow_answer_is_not_taken_over() {
    let _history = setup().await;
    let data = payload(PREFETCH + 1536 * KB, 271);
    let mut file = Mock::new(data.clone());
    // Longer than the silence after which an idle connection takes over a chunk. The mirror's
    // probe went through a redirect, which chunk requests skip.
    file.latency = Duration::from_millis(2500);
    let file = Arc::new(file);
    let (_redirector, url) = redirected_to(&file).await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("slow.bin");

    // Two connections for one chunk and no steals: one of them waits for the chunk's answer, the
    // other has nothing to do but take it over, and would, each time, from whichever asked last.
    let opts = DownloadOptions { min_steal_threshold: u64::MAX, ..options(&out, 4, 8 * PREFETCH) };
    let engine = DownloadEngine::new(vec![url], opts);
    run(&engine, None).await.expect("download should succeed");

    assert_file(&out, &data);
    let mut starts: Vec<u64> = file.served_ranges().iter().map(|r| r.start).collect();
    let requests = starts.len();
    starts.sort_unstable();
    starts.dedup();
    assert_eq!(starts.len(), requests, "a chunk was asked for again before its answer came: {:?}", file.served_ranges());
}

#[tokio::test]
async fn test_a_chunk_that_just_answered_is_not_split_again_and_again() {
    let _history = setup().await;
    let data = payload(PREFETCH + 16 * PREFETCH, 277);
    let mut mock = Mock::new(data.clone());
    // Every answer takes two seconds, then comes at a few MB/s per connection.
    mock.latency = Duration::from_secs(2);
    mock.delay_us.store(4000, Ordering::SeqCst);
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "slow-start.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("slow-start.bin");

    // More chunks than connections, so connections run out of work at different times, and the
    // default floor for steals.
    let opts = DownloadOptions { min_steal_threshold: DownloadOptions::default().min_steal_threshold, ..options(&out, 4, 3 * PREFETCH) };
    let engine = DownloadEngine::new(vec![url], opts);
    run(&engine, None).await.expect("download should succeed");

    assert_file(&out, &data);
    // A thief's request, once its first bytes land after the long wait, looks no slower than it
    // is: nobody splits it again, which would cost another wait for an answer each time.
    let ranges = mock.served_ranges();
    let inside = |r: &ByteRange, of: &ByteRange| r.start > of.start && r.start <= of.end;
    let stolen: Vec<&ByteRange> = ranges.iter().filter(|r| ranges.iter().any(|v| inside(r, v))).collect();
    assert!(stolen.iter().all(|s| !ranges.iter().any(|r| inside(r, s))), "a stolen part was split again: {ranges:?}");
}

#[tokio::test]
async fn test_expired_redirect_target_falls_back_to_the_mirrors_own_url() {
    let _history = setup().await;
    let data = payload(PREFETCH + 1024 * KB, 233);
    // However the expired target answers: refused, a 400 (Google Cloud Storage's ExpiredToken),
    // or an error page with 200 that is not the file (not the version If-Range asks for).
    for answer in [Reply::Status(403, None), Reply::Status(400, None), Reply::ErrorPage] {
        let mut file = Mock::new(data.clone());
        file.etag = Some("\"v1\"");
        // The target the probe was sent to expires right away; fresh redirects work.
        file.expired = Some(("t=0", answer));
        let file = Arc::new(file);
        let (redirector, url) = redirected_to(&file).await;
        let temp = tempdir().unwrap();

        // Every connection finds the target expired: that must neither count against the only
        // mirror nor end the download, and the retries must not keep asking the dead target.
        let opts = DownloadOptions { max_retries: 1, ..options(temp.path(), 4, 128 * KB) };
        let engine = DownloadEngine::new(vec![url], opts);
        let path = run(&engine, None).await.unwrap_or_else(|e| panic!("{answer:?}: the mirror's own URL still serves the file: {e}"));

        assert_file(&path, &data);
        assert!(file.stats.denied.load(Ordering::SeqCst) >= 1, "{answer:?}: the redirect target was tried first");
        assert!(redirector.stats.gets.load(Ordering::SeqCst) >= 1, "{answer:?}: then the mirror's own URL, redirecting afresh");
    }
}

#[tokio::test]
async fn test_head_is_not_awaited_once_the_ranged_get_says_it_all() {
    let _history = setup().await;
    let data = payload(PREFETCH + 256 * KB, 239);
    // HEAD answers after the GET, well within its grace, and only it names the file "head.bin".
    for (etag, expected) in [(Some("\"g1\""), "get.bin"), (None, "head.bin")] {
        let mut mock = Mock::new(data.clone());
        mock.etag = etag;
        mock.head_delay = Duration::from_millis(150);
        mock.head_disposition = Some("attachment; filename=\"head.bin\"");
        let url = serve(Arc::new(mock), "get.bin").await;
        let temp = tempdir().unwrap();

        let engine = DownloadEngine::new(vec![url], options(temp.path(), 4, 64 * KB));
        let path = run(&engine, None).await.expect("download should succeed");

        // A 206 with size, validator and a name in the URL does not wait for HEAD; without a
        // validator it does, and then takes HEAD's name.
        assert_eq!(path, temp.path().join(expected), "etag: {etag:?}");
        assert_file(&path, &data);
    }
}

#[tokio::test]
async fn test_downloads_to_one_host_share_its_connection_budget() {
    let _history = setup().await;
    let data = payload(PREFETCH + 4096 * KB, 241);
    let mock = Arc::new(Mock::new(data.clone()));
    mock.delay_us.store(10_000, Ordering::SeqCst);
    let first = serve(Arc::clone(&mock), "first.bin").await;
    let second = first.join("second.bin").unwrap();
    let temp = tempdir().unwrap();
    let engine = |url: Url, name: &str| {
        let out = temp.path().join(name);
        let opts = DownloadOptions {
            max_connections_per_host: 4,
            // Slot waits sit outside every timeout.
            stall_timeout_secs: 1,
            // No steals: a stolen-from request is dropped once its part is in, which the mock
            // only notices at its next write. (With steals and takeovers the engine's own tests
            // count the slots held instead.) For the same reason chunks are as long as the
            // probe's range, so its answer, which goes on as the first chunk, is read to its end.
            min_steal_threshold: u64::MAX,
            ..options(&out, 8, PREFETCH)
        };
        (DownloadEngine::new(vec![url], opts), out)
    };
    let ((a, a_out), (b, b_out)) = (engine(first, "first.bin"), engine(second, "second.bin"));

    let (a_done, b_done) = tokio::join!(run(&a, None), run(&b, None));
    a_done.expect("the first download should succeed");
    b_done.expect("the second download should succeed");

    assert_file(&a_out, &data);
    assert_file(&b_out, &data);
    let most = mock.stats.max_serving.load(Ordering::SeqCst);
    assert_eq!(most, 4, "two downloads of 8 connections each, probes included, share the host's 4");
}

#[tokio::test]
async fn test_probes_take_their_hosts_budget_slots() {
    let _history = setup().await;
    let data = payload(64 * KB, 251);
    let mut mock = Mock::new(data.clone());
    mock.etag = Some("\"s1\"");
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "slots.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("slots.bin");

    // One request to the host at a time: HEAD waits for the slot the ranged GET holds, and that
    // GET's answer has everything HEAD could add.
    let opts = DownloadOptions { max_connections_per_host: 1, ..options(&out, 4, 64 * KB) };
    let engine = DownloadEngine::new(vec![url], opts);
    run(&engine, None).await.expect("download should succeed");

    assert_file(&out, &data);
    let s = &mock.stats;
    assert_eq!(s.max_open.load(Ordering::SeqCst), 1, "HEAD and GET were in flight together");
    assert_eq!(s.requests.load(Ordering::SeqCst), 1, "HEAD never got a slot, and was not needed");
}

#[tokio::test]
async fn test_what_downloads_see_of_their_hosts_is_remembered() {
    let _history = setup().await;
    let data = payload(3 * PREFETCH, 263);
    // A server capping each connection (~3 MB/s); one far away (new connections take 200 ms)
    // sending a small file as fast as it can; one sending a small file too slowly to tell either
    // way; one ignoring ranges; and one sending a file within the probe's range whole, as a
    // server may however it takes ranges.
    let capped = Mock::new(data.clone());
    capped.delay_us.store(5_000, Ordering::SeqCst);
    let mut fast = Mock::new(data[..256 * KB].to_vec());
    fast.latency = Duration::from_millis(200);
    let short = Mock::new(data[..20 * KB].to_vec());
    short.delay_us.store(30_000, Ordering::SeqCst);
    let mut whole = Mock::new(data.clone());
    whole.ranges = false;
    let mut small_whole = Mock::new(data[..256 * KB].to_vec());
    small_whole.ranges = false;
    for (mock, ranges, is_capped) in [
        (capped, Some(true), Some(true)),
        (fast, Some(true), Some(false)),
        (short, Some(true), None),
        (whole, Some(false), None),
        (small_whole, None, None),
    ] {
        let data = mock.data.clone();
        let url = serve(Arc::new(mock), "seen.bin").await;
        let temp = tempdir().unwrap();
        let out = temp.path().join("seen.bin");
        run(&DownloadEngine::new(vec![url.clone()], options(&out, 4, 256 * KB)), None).await.expect("download should succeed");

        assert_file(&out, &data);
        let seen = hosts::profile(&url);
        assert_eq!((seen.accepts_ranges, seen.capped_per_connection), (ranges, is_capped), "{seen:?}");
        assert_eq!(seen.connection_rate.is_some(), is_capped == Some(true), "{seen:?}");
        assert!(seen.setup_time.is_some(), "the probe's connection was new: {seen:?}");
    }
}

#[tokio::test]
async fn test_a_host_seen_capped_is_split_at_once() {
    let _history = setup().await;
    let data = payload(PREFETCH + 512 * KB, 269);
    let mock = Arc::new(Mock::new(data.clone()));
    let url = serve(Arc::clone(&mock), "seen_capped.bin").await;
    // An earlier download found each connection to the host capped at 1 MB/s.
    let seen = HostProfile { capped_per_connection: Some(true), connection_rate: Some(1e6), ..Default::default() };
    hosts::record(&url, seen);
    let temp = tempdir().unwrap();
    let out = temp.path().join("seen_capped.bin");

    let engine = DownloadEngine::new(vec![url], options(&out, 4, 128 * KB));
    // The download's first snapshot, before any worker starts, has what the probe read.
    let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(256);
    let (done, first) = tokio::join!(run(&engine, Some(tx)), rx.recv());
    done.expect("download should succeed");

    assert_file(&out, &data);
    assert_eq!(first.expect("a snapshot").downloaded_bytes, 0, "the workers waited for the probe's answer");
    let starts: Vec<u64> = mock.served_ranges().iter().map(|r| r.start).collect();
    assert!(!starts.contains(&0), "the probe's answer was dropped for the first chunk: {starts:?}");
    assert!(starts.iter().any(|&start| start < PREFETCH as u64), "{starts:?}");
}

#[tokio::test]
async fn test_a_busy_probe_goes_by_the_ranges_its_host_was_seen_to_take() {
    let _history = setup().await;
    let size = 2 * PREFETCH;
    let data = payload(size, 271);
    let done_len = size - 200 * KB;
    // Every try of the ranged probe finds the server busy, and HEAD leaves out Accept-Ranges:
    // only what an earlier download saw says the server takes ranges.
    let mut mock = Mock::new(data.clone());
    mock.etag = Some("\"r1\"");
    mock.head_hides_ranges = true;
    mock.busy_probes = AtomicUsize::new(100);
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "seen_ranges.bin").await;
    hosts::record(&url, HostProfile { accepts_ranges: Some(true), ..Default::default() });
    let temp = tempdir().unwrap();
    let out = temp.path().join("seen_ranges.bin");
    let part = part_of(&out);
    // A previous run got 90% of the file.
    let mut on_disk = vec![0u8; size];
    on_disk[..done_len].copy_from_slice(&data[..done_len]);
    std::fs::write(&part, &on_disk).unwrap();
    let mut state = DownloadState::new("seen_ranges.bin".into(), size as u64, 256 * KB as u64, vec![url.to_string()]);
    state.etag = Some("\"r1\"".into());
    state.completed_ranges.push(ByteRange::new(0, done_len as u64 - 1).unwrap());
    state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();

    run(&DownloadEngine::new(vec![url], options(&out, 4, 64 * KB)), None).await.expect("the download resumes");
    assert_file(&out, &data);
    assert_eq!(mock.stats.body_bytes.load(Ordering::SeqCst), (size - done_len) as u64, "only the missing bytes");

    // Nor is a server seen ignoring ranges taken at HEAD's word that it takes them.
    let mut liar = Mock::new(data.clone());
    liar.ranges = false;
    liar.head_claims_ranges = true;
    liar.busy_probes = AtomicUsize::new(100);
    let liar = Arc::new(liar);
    let url = serve(Arc::clone(&liar), "seen_liar.bin").await;
    hosts::record(&url, HostProfile { accepts_ranges: Some(false), ..Default::default() });
    let out = temp.path().join("seen_liar.bin");
    run(&DownloadEngine::new(vec![url], options(&out, 4, 64 * KB)), None).await.expect("one stream fetches it");
    assert_file(&out, &data);
    assert_eq!(liar.stats.gets.load(Ordering::SeqCst), 1, "one stream, no failed range requests");
}

#[tokio::test]
async fn test_a_mirror_slow_to_answer_does_not_hold_up_the_download() {
    let _history = setup().await;
    let data = payload(2 * PREFETCH, 281);
    // Listed after the mirror that answers or before it, a mirror that does not answer is not
    // waited for.
    for slow_first in [false, true] {
        let fast = Arc::new(Mock::new(data.clone()));
        let mut slow = Mock::new(data.clone());
        slow.latency = Duration::from_secs(60);
        let slow = Arc::new(slow);
        let fast_url = serve(Arc::clone(&fast), "late.bin").await;
        let slow_url = serve(Arc::clone(&slow), "late.bin").await;
        let urls = if slow_first { vec![slow_url, fast_url] } else { vec![fast_url, slow_url] };
        let temp = tempdir().unwrap();
        let out = temp.path().join("late.bin");

        let started = Instant::now();
        run(&DownloadEngine::new(urls, options(&out, 4, 64 * KB)), None).await.expect("the mirror that answers serves it");
        assert_file(&out, &data);
        assert!(started.elapsed() < Duration::from_secs(10), "slow first: {slow_first}: took {:?}", started.elapsed());
    }
}

#[tokio::test]
async fn test_an_answer_waiting_for_the_other_probes_holds_no_host_slot() {
    let _history = setup().await;
    let data = payload(2 * PREFETCH, 287);
    // Two mirrors on one host, which ignores ranges and was seen to serve one connection at a
    // time: the first mirror's answer, kept for the one stream, must not keep the second mirror's
    // probe from the host while the download waits for it.
    let mut mock = Mock::new(data.clone());
    mock.ranges = false;
    let mock = Arc::new(mock);
    let first = serve(Arc::clone(&mock), "a.bin").await;
    let second = first.join("b.bin").unwrap();
    hosts::record(&first, HostProfile { connection_cap: Some(1), ..Default::default() });
    let temp = tempdir().unwrap();
    let out = temp.path().join("a.bin");

    run(&DownloadEngine::new(vec![first, second], options(&out, 4, 64 * KB)), None).await.expect("download should succeed");
    assert_file(&out, &data);
}

#[tokio::test]
async fn test_a_capped_host_is_learned_from_a_download_its_probe_could_not_measure() {
    let _history = setup().await;
    let data = payload(4 * PREFETCH, 307);
    // ~3 MB/s per connection, from a host every answer takes 200 ms to come from: the rate is
    // looked at every 100 ms, so when the workers stop waiting for the probe's answer it had held
    // only once.
    let mut mock = Mock::new(data.clone());
    mock.latency = Duration::from_millis(200);
    mock.delay_us.store(5_000, Ordering::SeqCst);
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "far.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("far.bin");

    run(&DownloadEngine::new(vec![url.clone()], options(&out, 4, 256 * KB)), None).await.expect("download should succeed");
    assert_file(&out, &data);
    // Going on as the first chunk, alone until the workers' answers came, it told the rest.
    let seen = hosts::profile(&url);
    assert_eq!(seen.capped_per_connection, Some(true), "{seen:?}");
    assert!(seen.connection_rate.is_some(), "{seen:?}");
}

#[tokio::test]
async fn test_a_mirror_answering_later_joins_the_download() {
    let _history = setup().await;
    let data = payload(4 * PREFETCH, 283);
    // The first mirror sends ~0.8 MB/s per connection, and stops serving the file six requests
    // in: by then the second mirror's answer, 400 ms late, has come in, and it serves the rest.
    // The third, as late, has another file.
    let mut first = Mock::new(data.clone());
    first.delay_us.store(20_000, Ordering::SeqCst);
    first.plan = |i| if i < 6 { Reply::Normal } else { Reply::Status(404, None) };
    let mut second = Mock::new(data.clone());
    second.latency = Duration::from_millis(400);
    let mut other = Mock::new(payload(4 * PREFETCH + 1, 283));
    other.latency = Duration::from_millis(400);
    let (second, other) = (Arc::new(second), Arc::new(other));
    let urls = vec![
        serve(Arc::new(first), "joined.bin").await,
        serve(Arc::clone(&second), "joined.bin").await,
        serve(Arc::clone(&other), "joined.bin").await,
    ];
    let temp = tempdir().unwrap();
    let out = temp.path().join("joined.bin");

    let engine = DownloadEngine::new(urls, options(&out, 4, 256 * KB));
    let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(1024);
    let mirrors_seen = async {
        let mut seen = Vec::new();
        while let Ok(snapshot) = rx.recv().await {
            seen.push(snapshot.mirror_speeds.len());
        }
        seen
    };
    let (done, seen) = tokio::join!(run(&engine, Some(tx)), mirrors_seen);
    done.expect("the mirror that joined serves the rest");

    assert_file(&out, &data);
    assert_eq!(seen.first(), Some(&1), "the download waited for the other mirrors' answers: {seen:?}");
    assert_eq!(seen.iter().max(), Some(&2), "{seen:?}");
    assert!(second.stats.gets.load(Ordering::SeqCst) > 0);
    assert_eq!(other.stats.gets.load(Ordering::SeqCst), 0, "a mirror of another file joined");
}

#[tokio::test]
async fn test_a_web_page_is_downloaded_as_the_video_it_plays() {
    isolate_history();
    let _history = HISTORY.write().await;
    let video = payload(PREFETCH + 256 * KB, 293);
    let video_url = serve(Arc::new(Mock::new(video.clone())), "clip.mp4").await;
    // A page the probe reads whole, one of unknown length whose answer the probe leaves
    // coming, and one larger than the probe reads, with the video past that: each is looked
    // into from what the probe brought, else asked for again, all of it.
    for (chunked, padding, requests, gets) in [(false, 0, 2, 0), (true, 0, 2, 0), (false, PREFETCH, 3, 1)] {
        let html = format!("<html><body>{}<video controls src=\"{}\"></video></body></html>", " ".repeat(padding), video_url);
        let mut page = Mock::new(html.into_bytes());
        page.content_type = Some("text/html; charset=utf-8");
        page.chunked = chunked;
        let page = Arc::new(page);
        let page_url = serve(Arc::clone(&page), "watch").await;
        *page.slot_on_get.lock().unwrap() = Some(page_url.clone());
        let temp = tempdir().unwrap();
        let out = temp.path().join("clip.mp4");

        run(&DownloadEngine::new(vec![page_url.clone()], options(&out, 4, 256 * KB)), None).await.expect("the video should download");
        assert_file(&out, &video);
        let s = &page.stats;
        let seen = (s.requests.load(Ordering::SeqCst), s.gets.load(Ordering::SeqCst));
        assert_eq!(seen, (requests, gets), "chunked: {chunked}, padding: {padding}");
        assert_eq!(s.unslotted.load(Ordering::SeqCst), 0, "the page was asked for again without a host slot");
        assert!(page.served_ranges().is_empty(), "the page was asked for in parts");
        // History lists the video along with the page: a repair finds it there.
        let urls = history_entry(&out).expect("the download is recorded").urls;
        assert_eq!(urls, [page_url.to_string(), video_url.to_string()]);
    }
}

#[tokio::test]
async fn test_a_web_page_without_a_video_is_downloaded_as_it_is() {
    let _history = setup().await;
    let html = b"<html><body><p>Nothing to play here.</p></body></html>".to_vec();
    let mut page = Mock::new(html.clone());
    page.content_type = Some("text/html");
    let page = Arc::new(page);
    let url = serve(Arc::clone(&page), "about").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("about.html");
    // Asked whether one of its sites takes the page, yt-dlp says none does.
    let opts = DownloadOptions { ytdlp_path: Some(no_site_ytdlp(temp.path())), ..options(&out, 4, 64 * KB) };

    run(&DownloadEngine::new(vec![url], opts), None).await.expect("the page should download");
    assert_file(&out, &html);
    assert_eq!(page.stats.requests.load(Ordering::SeqCst), 2, "the probe brought all of it");
}

#[tokio::test]
async fn test_a_download_starts_at_the_connection_cap_its_host_was_seen_to_enforce() {
    let _history = setup().await;
    let data = payload(5 * PREFETCH, 277);
    let mock = Arc::new(Mock::new(data.clone()));
    let url = serve(Arc::clone(&mock), "seen_cap.bin").await;
    // The host was seen to refuse a third connection.
    hosts::record(&url, HostProfile { connection_cap: Some(2), ..Default::default() });
    let temp = tempdir().unwrap();
    let out = temp.path().join("seen_cap.bin");

    // The default chunk size splits the rest evenly among the connections the download opens.
    let opts = DownloadOptions {
        base_chunk_size: DownloadOptions::default().base_chunk_size,
        min_steal_threshold: u64::MAX,
        ..options(&out, 8, 0)
    };
    run(&DownloadEngine::new(vec![url], opts), None).await.expect("download should succeed");
    assert_file(&out, &data);
    assert_eq!(mock.served_ranges().len(), 2, "{:?}", mock.served_ranges());
}

// Links that stand for a file elsewhere: code-host file pages, Google Docs, and web pages served
// in place of a file.

/// Options that send every request through `proxy`, a mock that answers for whatever host is
/// asked: links to real hosts, as the resolvers rewrite them, reach it over plain HTTP.
fn through(proxy: &Url, out: &Path) -> DownloadOptions {
    DownloadOptions { proxy: Some(format!("http://{}", proxy.authority())), ..options(out, 4, 64 * KB) }
}

#[tokio::test]
async fn test_a_code_host_file_page_downloads_the_file() {
    let _history = setup().await;
    let data = payload(PREFETCH + 256 * KB, 311);
    // Past the probe's first MiB, the "view file" page is no source of the file.
    let mut mock = Mock::new(data.clone());
    mock.expired = Some(("/blob/", Reply::Status(404, None)));
    let mock = Arc::new(mock);
    let proxy = serve(Arc::clone(&mock), "").await;
    let temp = tempdir().unwrap();
    let page = Url::parse("http://github.com/links-lane/repo/blob/main/dist/app.zip").unwrap();

    let path = run(&DownloadEngine::new(vec![page], through(&proxy, temp.path())), None)
        .await
        .expect("the file should download");
    assert_eq!(path, temp.path().join("app.zip"));
    assert_file(&path, &data);
    assert!(mock.stats.gets.load(Ordering::SeqCst) > 0);
    assert_eq!(mock.stats.denied.load(Ordering::SeqCst), 0, "the page was asked for the file");
}

#[tokio::test]
async fn test_a_google_docs_link_downloads_the_export_over_one_connection() {
    let _history = setup().await;
    // As Google sends an export: made on the fly, so no length and no ranges, named by
    // Content-Disposition. The first answer breaks off, so the export is asked for again, from
    // its start: the editor's page, asked for so, is not it.
    let data = payload(PREFETCH + 300 * KB + 11, 313);
    let mut mock = Mock::new(data.clone());
    mock.chunked = true;
    mock.content_type = Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document");
    mock.disposition = Some("attachment; filename=\"QuarterlyReport.docx\"; filename*=UTF-8''Quarterly%20Report.docx".to_string());
    mock.probe_reply = Reply::CloseAfter(PREFETCH + 64 * KB);
    mock.expired = Some(("/edit", Reply::ErrorPage));
    let mock = Arc::new(mock);
    let proxy = serve(Arc::clone(&mock), "").await;
    let temp = tempdir().unwrap();
    let link = Url::parse("http://docs.google.com/document/d/1LinksLaneDoc/edit?usp=sharing").unwrap();

    let path = run(&DownloadEngine::new(vec![link], through(&proxy, temp.path())), None)
        .await
        .expect("the export should download");
    assert_eq!(path, temp.path().join("Quarterly Report.docx"));
    assert_file(&path, &data);
    assert_no_leftovers(&path);
    let s = &mock.stats;
    assert!(mock.served_ranges().is_empty(), "the export was asked for in parts");
    assert_eq!((s.gets.load(Ordering::SeqCst), s.denied.load(Ordering::SeqCst)), (1, 0), "asked for once more, as the export");
}

#[tokio::test]
async fn test_a_web_page_served_for_a_named_file_is_an_error_not_a_download() {
    let _history = setup().await;
    let html = b"<html><body>Please sign in to download setup.exe</body></html>".to_vec();
    let mut page = Mock::new(html.clone());
    page.content_type = Some("text/html; charset=utf-8");
    let page = Arc::new(page);
    let temp = tempdir().unwrap();

    let url = serve(Arc::clone(&page), "setup.exe").await;
    let err = run(&DownloadEngine::new(vec![url], options(temp.path(), 4, 64 * KB)), None)
        .await
        .expect_err("a web page is no setup.exe");
    assert!(err.contains("the server sent a web page instead of setup.exe"), "{err}");
    assert!(names_in(temp.path()).is_empty(), "{:?}", names_in(temp.path()));

    // A hosted .html file is what was asked for.
    let url = serve(Arc::clone(&page), "guide.html").await;
    let path = run(&DownloadEngine::new(vec![url], options(temp.path(), 4, 64 * KB)), None)
        .await
        .expect("the .html file should download");
    assert_eq!(path, temp.path().join("guide.html"));
    assert_file(&path, &html);
}

#[tokio::test]
async fn test_private_documents_and_unsupported_shares_are_errors_not_downloads() {
    let _history = setup().await;
    let mut page = Mock::new(b"<!doctype html><title>Sign in</title>".to_vec());
    page.content_type = Some("text/html; charset=utf-8");
    let proxy = serve(Arc::new(page), "").await;
    for (link, expected) in [
        ("http://docs.google.com/spreadsheets/d/1LinksLanePrivate/edit#gid=0", "Google asked to sign in: the document is private"),
        ("http://mega.nz/file/links-lane#key", "MEGA links are not supported yet"),
    ] {
        let temp = tempdir().unwrap();
        let err = run(&DownloadEngine::new(vec![Url::parse(link).unwrap()], through(&proxy, temp.path())), None)
            .await
            .expect_err(link);
        assert!(err.contains(expected), "{link}: {err}");
        assert!(names_in(temp.path()).is_empty(), "{link}: {:?}", names_in(temp.path()));
    }
}

#[tokio::test]
async fn test_a_code_host_folder_is_an_error_not_a_download() {
    let _history = setup().await;
    let mut page = Mock::new(b"<!doctype html><title>docs at main</title>".to_vec());
    page.content_type = Some("text/html; charset=utf-8");
    let proxy = serve(Arc::new(page), "").await;
    let temp = tempdir().unwrap();
    let folder = Url::parse("http://github.com/links-lane/repo/tree/main/docs").unwrap();

    let err = run(&DownloadEngine::new(vec![folder], through(&proxy, temp.path())), None)
        .await
        .expect_err("a folder's page is no file");
    assert!(err.contains("the link leads to a folder, not a file"), "{err}");
    assert!(names_in(temp.path()).is_empty(), "{:?}", names_in(temp.path()));
}

// ---- Links judged by where they lead: short links, pages that send the browser on, shorteners ----

use hyperfetch_core::media::MediaQualityPreset;

/// A whole HTTP response with `body`, which a HEAD request does not get.
fn response(method: &str, status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    if method != "HEAD" {
        out.extend_from_slice(body);
    }
    out
}

/// The proxy a download sends every request through, standing in for the hosts they name:
/// `answer` gives the response to a request by its method and target (an absolute URL). It
/// lists the targets asked for, each with whether the request carried an Authorization header.
async fn serve_proxy(answer: fn(&str, &str) -> Vec<u8>) -> (String, Arc<Mutex<Vec<(String, bool)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let Some(head) = read_head(&mut socket).await else { return };
                let mut request_line = head.lines().next().unwrap_or("").split(' ');
                let (method, target) = (request_line.next().unwrap_or(""), request_line.next().unwrap_or(""));
                let authorized = head.lines().any(|l| l.to_ascii_lowercase().starts_with("authorization:"));
                log.lock().unwrap().push((target.to_string(), authorized));
                let _ = socket.write_all(&answer(method, target)).await;
            });
        }
    });
    (proxy, seen)
}

/// A stand-in for yt-dlp that works in `dir`. Asked to find what a link holds (`-J`), it prints
/// `dir/info.json`, or fails as yt-dlp does for a DRM-protected video while `dir/drm` exists,
/// else as it does for a link none of its sites takes; asked to download, it writes `output` and
/// reports it. It writes the cookies file it is given, as yt-dlp does as it exits, and lists
/// the arguments of each run (see `runs_of`).
fn fake_ytdlp(dir: &Path, output: &Path) -> PathBuf {
    let (dir_s, out_s, out_dir) = (dir.display(), output.display(), output.parent().unwrap().display());
    #[cfg(windows)]
    let (bin, script) = (
        dir.join("yt-dlp.cmd"),
        format!(
            "@echo off\r\n\
             if \"%~2\"==\"--version\" exit /b 1\r\n\
             >>\"{dir_s}\\runs.txt\" echo run\r\n\
             set find=\r\n\
             if \"%~4\"==\"-J\" set find=1\r\n\
             :scan\r\n\
             if \"%~1\"==\"\" goto scanned\r\n\
             >>\"{dir_s}\\runs.txt\" echo \"%~1\"\r\n\
             if \"%~1\"==\"--cookies\" (>\"%~2\" echo # This file is generated by yt-dlp.  Do not edit.)\r\n\
             shift\r\n\
             goto scan\r\n\
             :scanned\r\n\
             if not defined find goto download\r\n\
             if exist \"{dir_s}\\info.json\" (type \"{dir_s}\\info.json\"& exit /b 0)\r\n\
             if exist \"{dir_s}\\drm\" (>&2 echo ERROR: [FakeSite] clip1: This video is DRM protected& exit /b 1)\r\n\
             >&2 echo ERROR: No suitable extractor found for URL\r\n\
             exit /b 1\r\n\
             :download\r\n\
             if not exist \"{out_dir}\" mkdir \"{out_dir}\"\r\n\
             >\"{out_s}\" echo media\r\n\
             echo HFPATH {out_s}\r\n\
             exit /b 0\r\n"
        ),
    );
    #[cfg(not(windows))]
    let (bin, script) = (
        dir.join("yt-dlp"),
        format!(
            "#!/bin/sh\n\
             [ \"$2\" = --version ] && exit 1\n\
             echo run >> '{dir_s}/runs.txt'\n\
             find=; [ \"$4\" = -J ] && find=1\n\
             prev=\n\
             for a in \"$@\"; do\n\
             printf '%s\\n' \"$a\" >> '{dir_s}/runs.txt'\n\
             [ \"$prev\" = --cookies ] && echo '# This file is generated by yt-dlp.  Do not edit.' > \"$a\"\n\
             prev=$a\n\
             done\n\
             if [ -n \"$find\" ]; then\n\
             [ -e '{dir_s}/info.json' ] && {{ cat '{dir_s}/info.json'; exit 0; }}\n\
             [ -e '{dir_s}/drm' ] && {{ echo 'ERROR: [FakeSite] clip1: This video is DRM protected' >&2; exit 1; }}\n\
             echo 'ERROR: No suitable extractor found for URL' >&2; exit 1\n\
             fi\n\
             mkdir -p '{out_dir}'\n\
             echo media > '{out_s}'\n\
             echo 'HFPATH {out_s}'\n"
        ),
    );
    std::fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// The arguments of each run `fake_ytdlp` in `dir` saw.
fn runs_of(dir: &Path) -> Vec<Vec<String>> {
    let log = std::fs::read_to_string(dir.join("runs.txt")).unwrap_or_default();
    let mut runs: Vec<Vec<String>> = Vec::new();
    for line in log.lines().map(|l| l.trim().trim_matches('"')) {
        match runs.last_mut() {
            Some(run) if line != "run" => run.push(line.to_string()),
            _ => runs.push(Vec::new()),
        }
    }
    runs
}

/// Whether `args` holds `flag` followed by `value`.
fn has_arg(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|pair| pair[0] == flag && pair[1] == value)
}

/// A yt-dlp none of whose sites takes any link, in `dir`.
fn no_site_ytdlp(dir: &Path) -> PathBuf {
    fake_ytdlp(dir, &dir.join("never.mp4"))
}

/// A web page that plays nothing and sends the browser nowhere.
async fn plain_page(name: &str) -> (Arc<Mock>, Url) {
    let mut page = Mock::new(b"<html><body><p>Nothing to play here.</p></body></html>".to_vec());
    page.content_type = Some("text/html; charset=utf-8");
    let page = Arc::new(page);
    let url = serve(Arc::clone(&page), name).await;
    (page, url)
}

/// A short link to a Dropbox file, whose link as shared answers with a preview page.
fn short_link_to_dropbox(method: &str, target: &str) -> Vec<u8> {
    if target.starts_with("http://go.short.invalid/") {
        response(method, "302 Found", "Location: http://www.dropbox.com/s/k3y/report.bin?dl=0\r\n", b"")
    } else if target == "http://www.dropbox.com/s/k3y/report.bin?dl=1" {
        response(method, "200 OK", "Content-Type: application/octet-stream\r\n", &payload(64 * KB, 311))
    } else {
        response(method, "200 OK", "Content-Type: text/html; charset=utf-8\r\n", b"<html><body>Preview of report.bin</body></html>")
    }
}

#[tokio::test]
async fn test_a_short_link_is_downloaded_from_the_file_host_it_lands_on() {
    isolate_history();
    let _history = HISTORY.write().await;
    let (proxy, seen) = serve_proxy(short_link_to_dropbox).await;
    let temp = tempdir().unwrap();
    let short = Url::parse("http://go.short.invalid/report").unwrap();
    let opts = DownloadOptions { proxy: Some(proxy), auth_header: Some("Bearer secret".into()), ..options(temp.path(), 4, 64 * KB) };

    let path = run(&DownloadEngine::new(vec![short.clone()], opts), None).await.expect("the file should download");
    assert_eq!(path, temp.path().join("report.bin"));
    assert_file(&path, &payload(64 * KB, 311));
    // History lists where the link landed along with it: a repair finds the file there.
    let landed = "http://www.dropbox.com/s/k3y/report.bin?dl=0";
    assert_eq!(history_entry(&path).expect("the download is recorded").urls, [short.to_string(), landed.to_string()]);
    // The file came as Dropbox's resolver asks for it, and the credentials went only to the host
    // the user named.
    let seen = seen.lock().unwrap().clone();
    assert!(seen.iter().any(|(target, _)| target.ends_with("report.bin?dl=1")), "{seen:?}");
    for (target, authorized) in &seen {
        assert_eq!(*authorized, target.starts_with("http://go.short.invalid/"), "{target}");
    }
}

/// A web page at `name` that sends the browser on to `to` at once, with JavaScript or without
/// (`<noscript>`, as t.co answers).
async fn refreshing_page(name: &str, to: &str, noscript: bool) -> (Arc<Mock>, Url) {
    let meta = format!(r#"<META http-equiv="refresh" content="0;URL='{to}'">"#);
    let meta = if noscript { format!("<noscript>{meta}</noscript>") } else { meta };
    let mut page = Mock::new(format!("<html><head>{meta}</head></html>").into_bytes());
    page.content_type = Some("text/html; charset=utf-8");
    let page = Arc::new(page);
    let url = serve(Arc::clone(&page), name).await;
    (page, url)
}

#[tokio::test]
async fn test_a_page_that_sends_the_browser_on_at_once_is_downloaded_as_its_target() {
    isolate_history();
    let _history = HISTORY.write().await;
    let data = payload(PREFETCH + 256 * KB, 313);
    let file_url = serve(Arc::new(Mock::new(data.clone())), "setup.exe").await;
    let (_, page_url) = refreshing_page("l/abc", file_url.as_str(), true).await;
    let temp = tempdir().unwrap();

    let path = run(&DownloadEngine::new(vec![page_url.clone()], options(temp.path(), 4, 256 * KB)), None)
        .await
        .expect("the file should download");
    assert_eq!(path, temp.path().join("setup.exe"));
    assert_file(&path, &data);
    assert_eq!(history_entry(&path).expect("the download is recorded").urls, [page_url.to_string(), file_url.to_string()]);
}

#[tokio::test]
async fn test_a_download_given_its_history_as_mirrors_gets_the_file_again() {
    isolate_history();
    let _history = HISTORY.write().await;
    // What the GUI's Redownload does: the URLs history lists for a download that followed its
    // link, as mirrors of a new one. The link's answer comes first, and is a page.
    let again = |path: &Path| -> Vec<Url> {
        history_entry(path).expect("the download is recorded").urls.iter().map(|u| Url::parse(u).unwrap()).collect()
    };

    let (proxy, _) = serve_proxy(short_link_to_dropbox).await;
    let (first, second) = (tempdir().unwrap(), tempdir().unwrap());
    let short = Url::parse("http://go.short.invalid/report").unwrap();
    let opts = |dir: &Path| DownloadOptions { proxy: Some(proxy.clone()), ..options(dir, 4, 64 * KB) };
    let path = run(&DownloadEngine::new(vec![short], opts(first.path())), None).await.expect("the file should download");
    let path = run(&DownloadEngine::new(again(&path), opts(second.path())), None).await.expect("the file should download again");
    assert_eq!(path, second.path().join("report.bin"));
    assert_file(&path, &payload(64 * KB, 311));

    let data = payload(PREFETCH + 256 * KB, 331);
    let file_url = serve(Arc::new(Mock::new(data.clone())), "setup.exe").await;
    let (_, page_url) = refreshing_page("l/again", file_url.as_str(), true).await;
    let (first, second) = (tempdir().unwrap(), tempdir().unwrap());
    let path = run(&DownloadEngine::new(vec![page_url], options(first.path(), 4, 256 * KB)), None).await.expect("the file should download");
    let path = run(&DownloadEngine::new(again(&path), options(second.path(), 4, 256 * KB)), None)
        .await
        .expect("the file should download again");
    assert_eq!(path, second.path().join("setup.exe"));
    assert_file(&path, &data);
}

#[tokio::test]
async fn test_links_are_followed_at_most_three_times_and_never_back() {
    let _history = setup().await;
    let temp = tempdir().unwrap();
    let ytdlp = no_site_ytdlp(temp.path());
    // Four pages, each sending the browser on to the next, the last to the file.
    let file = Arc::new(Mock::new(payload(64 * KB, 317)));
    let mut to = serve(Arc::clone(&file), "far.bin").await.to_string();
    let mut pages = Vec::new();
    for n in (1..=4).rev() {
        let (page, url) = refreshing_page(&format!("hop{n}"), &to, true).await;
        to = url.to_string();
        pages.insert(0, page);
    }
    let out = temp.path().join("hops.html");
    let first = Url::parse(&to).unwrap();
    let opts = |out: &Path| DownloadOptions { ytdlp_path: Some(ytdlp.clone()), ..options(out, 4, 64 * KB) };
    run(&DownloadEngine::new(vec![first], opts(&out)), None).await.expect("the last page should download");
    assert_file(&out, &pages[3].data);
    assert_eq!(file.stats.requests.load(Ordering::SeqCst), 0, "a fourth link was followed");

    // A page sending the browser to itself is followed once, not again.
    let (page, url) = refreshing_page("start", "/again", false).await;
    let out = temp.path().join("again.html");
    run(&DownloadEngine::new(vec![url], opts(&out)), None).await.expect("the page should download");
    assert_file(&out, &page.data);
    assert_eq!(page.stats.probes.load(Ordering::SeqCst), 2, "the start page, then the one it sends the browser to");
}

#[tokio::test]
async fn test_a_page_asking_for_javascript_is_downloaded_as_it_is() {
    let _history = setup().await;
    let temp = tempdir().unwrap();
    // As Google answers without JavaScript: the page asks for it on its own site.
    let (page, url) = refreshing_page("search", "/httpservice/retry/enablejs?sei=x", true).await;
    let out = temp.path().join("search.html");
    let opts = DownloadOptions { ytdlp_path: Some(no_site_ytdlp(temp.path())), ..options(&out, 4, 64 * KB) };

    run(&DownloadEngine::new(vec![url], opts), None).await.expect("the page should download");
    assert_file(&out, &page.data);
    assert_eq!(page.stats.requests.load(Ordering::SeqCst), 2, "the page it asks for was asked for");
}

/// A link shortener's warning page, as it shows one instead of redirecting.
fn shortener_warning(method: &str, _target: &str) -> Vec<u8> {
    response(method, "200 OK", "Content-Type: text/html; charset=utf-8\r\n", b"<html><body>Warning! This link may be unsafe.</body></html>")
}

/// A link shortener's page that sends the browser on to the file at once.
fn shortener_page_sending_on(method: &str, target: &str) -> Vec<u8> {
    if target.starts_with("http://files.short.invalid/") {
        response(method, "200 OK", "Content-Type: application/octet-stream\r\n", &payload(64 * KB, 337))
    } else {
        let page = br#"<html><head><meta http-equiv="refresh" content="0; url=http://files.short.invalid/x.bin"></head></html>"#;
        response(method, "200 OK", "Content-Type: text/html; charset=utf-8\r\n", page)
    }
}

#[tokio::test]
async fn test_a_link_shortener_showing_a_page_is_an_error_not_a_download() {
    let _history = setup().await;
    let link = Url::parse("http://bit.ly/3xYz").unwrap();
    // A page that leads nowhere, and one that leads on: neither is clicked through.
    for answer in [shortener_warning as fn(&str, &str) -> Vec<u8>, shortener_page_sending_on] {
        let (proxy, seen) = serve_proxy(answer).await;
        let temp = tempdir().unwrap();
        let opts = DownloadOptions { proxy: Some(proxy), ..options(temp.path(), 4, 64 * KB) };

        let err = run(&DownloadEngine::new(vec![link.clone()], opts), None).await.expect_err("a warning is not what the link stands for");
        assert_eq!(err, "bit.ly showed a page instead of redirecting (a preview or a warning): open the link in your browser");
        assert_eq!(names_in(temp.path()), Vec::<String>::new());
        let seen = seen.lock().unwrap().clone();
        assert!(seen.iter().all(|(target, _)| target.starts_with("http://bit.ly/")), "{seen:?}");
    }
}

#[tokio::test]
async fn test_a_page_one_of_yt_dlps_sites_takes_is_downloaded_as_its_media() {
    isolate_history();
    let _history = HISTORY.write().await;
    let (tools, temp) = (tempdir().unwrap(), tempdir().unwrap());
    let video = payload(PREFETCH + 128 * KB, 347);
    let video_url = serve(Arc::new(Mock::new(video.clone())), "v/stream").await;
    let (_, page_url) = plain_page("clips/1").await;
    // What the site's extractor finds there: one file, which the engine can fetch itself.
    let output = temp.path().join("A clip.mp4");
    let info = serde_json::json!({
        "_type": "video", "extractor_key": "FakeSite", "id": "clip1", "title": "A clip",
        "url": video_url.as_str(), "protocol": "http", "format_id": "0", "ext": "mp4",
        "requested_downloads": [{ "filename": output }],
    });
    std::fs::write(tools.path().join("info.json"), serde_json::to_vec(&info).unwrap()).unwrap();
    let opts = DownloadOptions { ytdlp_path: Some(fake_ytdlp(tools.path(), &output)), ..options(temp.path(), 4, 64 * KB) };

    let path = run(&DownloadEngine::new(vec![page_url.clone()], opts), None).await.expect("the video should download");
    assert_eq!(path, output);
    assert_file(&path, &video);
    assert_eq!(names_in(temp.path()), ["A clip.mp4"], "the page was saved too");
    // yt-dlp was asked once, only its own sites: the download went by what it found then.
    let runs = runs_of(tools.path());
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(has_arg(&runs[0], "--ies", "default,-generic") && runs[0].contains(&"-J".to_string()), "{runs:?}");
    assert_eq!(runs[0].last(), Some(&page_url.to_string()));
    assert_eq!(history_entry(&path).expect("the download is recorded").urls, [page_url.to_string()]);
}

#[tokio::test]
async fn test_a_web_page_none_of_yt_dlps_sites_takes_is_downloaded_as_it_is_and_the_cookies_file_kept() {
    let _history = setup().await;
    let (tools, temp) = (tempdir().unwrap(), tempdir().unwrap());
    let (page, url) = plain_page("about").await;
    let cookies = temp.path().join("cookies.txt");
    let mine = "# Netscape HTTP Cookie File\n# kept as I wrote it\n";
    std::fs::write(&cookies, mine).unwrap();
    let out = temp.path().join("about.html");
    let opts = DownloadOptions {
        cookies_path: Some(cookies.clone()),
        ytdlp_path: Some(no_site_ytdlp(tools.path())),
        ..options(&out, 4, 64 * KB)
    };

    run(&DownloadEngine::new(vec![url], opts), None).await.expect("the page should download");
    assert_file(&out, &page.data);
    // yt-dlp read a copy of the cookies, and rewrote that, not the user's file.
    let runs = runs_of(tools.path());
    assert_eq!(runs.len(), 1, "{runs:?}");
    let given = runs[0].windows(2).find(|pair| pair[0] == "--cookies").map(|pair| PathBuf::from(&pair[1]));
    assert!(given.as_ref().is_some_and(|file| *file != cookies && !file.exists()), "{runs:?}");
    assert_eq!(std::fs::read_to_string(&cookies).unwrap(), mine);
}

#[tokio::test]
async fn test_a_page_whose_video_is_drm_protected_is_an_error_not_a_download() {
    let _history = setup().await;
    let (tools, temp) = (tempdir().unwrap(), tempdir().unwrap());
    std::fs::write(tools.path().join("drm"), b"").unwrap();
    let (_, url) = plain_page("films/1").await;
    let opts = DownloadOptions { ytdlp_path: Some(no_site_ytdlp(tools.path())), ..options(temp.path(), 4, 64 * KB) };

    let err = run(&DownloadEngine::new(vec![url], opts), None).await.expect_err("the page is not the film");
    assert_eq!(err, "DRM-protected: not supported");
    assert_eq!(names_in(temp.path()), Vec::<String>::new());
}

/// A short link to a video on one of the media sites.
fn short_link_to_a_video(method: &str, target: &str) -> Vec<u8> {
    if target.starts_with("http://go.short.invalid/") {
        response(method, "302 Found", "Location: http://www.youtube.com/watch?v=abc\r\n", b"")
    } else {
        response(method, "200 OK", "Content-Type: text/html; charset=utf-8\r\n", b"<html><body>A video</body></html>")
    }
}

#[tokio::test]
async fn test_a_short_link_to_a_media_site_is_downloaded_with_yt_dlp() {
    isolate_history();
    let _history = HISTORY.write().await;
    let (proxy, _) = serve_proxy(short_link_to_a_video).await;
    let (tools, temp) = (tempdir().unwrap(), tempdir().unwrap());
    let output = temp.path().join("A video.mp3");
    let short = Url::parse("http://go.short.invalid/v").unwrap();
    let opts = DownloadOptions {
        proxy: Some(proxy),
        page_media_preset: Some(MediaQualityPreset::AudioMp3),
        ytdlp_path: Some(fake_ytdlp(tools.path(), &output)),
        ..options(temp.path(), 4, 64 * KB)
    };

    let path = run(&DownloadEngine::new(vec![short.clone()], opts), None).await.expect("the video should download");
    assert_eq!(path, output);
    let landed = "http://www.youtube.com/watch?v=abc";
    assert_eq!(history_entry(&path).expect("the download is recorded").urls, [short.to_string(), landed.to_string()]);
    // yt-dlp got where the link landed, in the quality chosen for links found to be media.
    let runs = runs_of(tools.path());
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(has_arg(&runs[0], "--audio-format", "mp3") && !runs[0].contains(&"-J".to_string()), "{runs:?}");
    assert_eq!(runs[0].last().map(String::as_str), Some(landed));
}
