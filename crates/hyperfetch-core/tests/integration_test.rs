use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, RwLock, RwLockReadGuard};
use url::Url;

use hyperfetch_core::engine::{DownloadEngine, DownloadOptions, EngineSnapshot};
use hyperfetch_core::history::{DownloadHistoryManager, HistoryEntry, HistoryStatus};
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
}

struct Mock {
    data: Vec<u8>,
    /// Advertise and honour byte ranges.
    ranges: bool,
    /// HEAD advertises `Accept-Ranges: bytes` even though GETs ignore Range.
    head_claims_ranges: bool,
    /// HEAD answers only `Content-Length: 0`, as many dynamic endpoints do.
    empty_head: bool,
    /// HEAD answers with this status instead.
    head_status: Option<u16>,
    /// HEAD answers only after this long.
    head_delay: Duration,
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
    /// Report this total in Content-Range instead of the real one (a broken mirror).
    content_range_total: Option<usize>,
    /// Report the total in Content-Range as `*` (unknown).
    unknown_total: bool,
    /// Answer 503 when more than this many GETs are being served at once.
    max_active: Option<usize>,
    /// What to do with each GET except the engine's probes, which are always served.
    plan: fn(usize) -> Reply,
    /// Pause between 16 KiB body writes, in microseconds (adjustable while running).
    delay_us: AtomicU64,
    /// If set, every GET checks that this file exists.
    must_exist_on_get: Mutex<Option<PathBuf>>,
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
    /// GETs refused with 503 for exceeding `max_active`.
    refused: AtomicUsize,
    /// Ranges served with 206, in request order.
    ranges: Mutex<Vec<ByteRange>>,
    missing_on_get: AtomicUsize,
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
            empty_head: false,
            head_status: None,
            head_delay: Duration::ZERO,
            etag_only_on_head: false,
            rejects_range: false,
            busy_probes: AtomicUsize::new(0),
            chunked: false,
            etag: None,
            disposition: None,
            content_range_total: None,
            unknown_total: false,
            max_active: None,
            plan: |_| Reply::Normal,
            delay_us: AtomicU64::new(0),
            must_exist_on_get: Mutex::new(None),
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

/// Starts the mock; it lives until the test's runtime shuts down.
async fn serve(mock: Arc<Mock>, file: &str) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
    let mut lines = head.lines();
    let method = lines.next().unwrap_or("").split(' ').next().unwrap_or("").to_string();
    let header = |name: &str| {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    if header("authorization").is_some() {
        s.authorized.fetch_add(1, Ordering::SeqCst);
    }
    let total = mock.data.len();
    let etag = mock.etag.map(|e| format!("ETag: {}\r\n", e)).unwrap_or_default();
    let get_etag = if mock.etag_only_on_head { "" } else { etag.as_str() };
    let accept = if mock.ranges && !mock.chunked { "Accept-Ranges: bytes\r\n" } else { "" };
    let length = |n: usize| if mock.chunked { "Transfer-Encoding: chunked\r\n".to_string() } else { format!("Content-Length: {}\r\n", n) };

    if method == "HEAD" {
        tokio::time::sleep(mock.head_delay).await;
        let resp = if let Some(code) = mock.head_status {
            format!("HTTP/1.1 {} Mock\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", code)
        } else if mock.empty_head {
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        } else {
            let accept = if mock.head_claims_ranges && !mock.chunked { "Accept-Ranges: bytes\r\n" } else { accept };
            format!("HTTP/1.1 200 OK\r\n{}{}{}Connection: close\r\n\r\n", length(total), accept, etag)
        };
        let _ = socket.write_all(resp.as_bytes()).await;
        return;
    }

    let range = header("range");
    let probe = range.as_deref() == Some("bytes=0-0") || range == Some(format!("bytes=0-{}", PREFETCH - 1));
    let (reply, _guard) = if probe {
        s.probes.fetch_add(1, Ordering::SeqCst);
        let busy = mock.busy_probes.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
        (if busy { Reply::Status(503, None) } else { Reply::Normal }, None)
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
        let reply = if mock.max_active.is_some_and(|max| active > max) {
            s.refused.fetch_add(1, Ordering::SeqCst);
            Reply::Status(503, Some(1))
        } else {
            (mock.plan)(index)
        };
        (reply, Some(guard))
    };
    let reply = if mock.rejects_range && header("range").is_some() { Reply::Status(400, None) } else { reply };
    if let Reply::Status(code, retry_after) = reply {
        let retry = retry_after.map(|s| format!("Retry-After: {}\r\n", s)).unwrap_or_default();
        let resp = format!("HTTP/1.1 {} Mock\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", code, retry);
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
        "HTTP/1.1 {} Mock\r\n{}{}{}{}{}Connection: close\r\n\r\n",
        status,
        length(body.len()),
        content_range,
        accept,
        get_etag,
        disposition
    );
    if socket.write_all(resp.as_bytes()).await.is_err() {
        return;
    }

    let limit = match reply {
        Reply::CloseAfter(n) | Reply::StallAfter(n) => n.min(body.len()),
        _ => body.len(),
    };
    for piece in body[..limit].chunks(16 * KB) {
        let delay = mock.delay_us.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_micros(delay)).await;
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
    assert_eq!(mock.stats.gets.load(Ordering::SeqCst), 1, "one stream, no failed range requests");
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
async fn test_connections_follow_the_size_of_the_file() {
    let _history = setup().await;
    let data = payload(3 * PREFETCH, 167);
    let mock = Arc::new(Mock::new(data.clone()));
    mock.delay_us.store(1_000, Ordering::SeqCst); // long enough for connections to overlap
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
async fn test_slow_probe_holds_up_the_workers_only_for_a_small_file() {
    let _history = setup().await;
    for (size, whole) in [(512 * KB, true), (3 * PREFETCH, false)] {
        let data = payload(size, 191);
        let mock = Arc::new(Mock::new(data.clone()));
        mock.delay_us.store(20_000, Ordering::SeqCst); // ~0.8 MB/s per connection
        let url = serve(Arc::clone(&mock), "slow_probe.bin").await;
        let temp = tempdir().unwrap();
        let out = temp.path().join("slow_probe.bin");

        let engine = DownloadEngine::new(vec![url], options(&out, 4, 256 * KB));
        run(&engine, None).await.expect("download should succeed");

        assert_file(&out, &data);
        let s = &mock.stats;
        if whole {
            assert_eq!(s.gets.load(Ordering::SeqCst), 0, "the probe alone brings a small file, however slowly");
        } else {
            let probed = s.probe_bytes.load(Ordering::SeqCst);
            assert!(probed < PREFETCH as u64 / 2, "workers waited for {probed} bytes from the probe");
        }
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
