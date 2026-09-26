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
use hyperfetch_core::range::ByteRange;
use hyperfetch_core::state::DownloadState;

const KB: usize = 1024;

/// Every engine run records history in one per-process file. Tests that depend on what is
/// recorded take this exclusively, so a parallel test's history write cannot race theirs.
static HISTORY: RwLock<()> = RwLock::const_new(());

/// Points download history at a per-process temp file so tests never touch the user's history.
fn isolate_history() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let path = std::env::temp_dir().join(format!("endo-integration-history-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::env::set_var("ENDO_HISTORY_PATH", path);
    });
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
}

struct Mock {
    data: Vec<u8>,
    /// Advertise and honour byte ranges.
    ranges: bool,
    /// No Content-Length anywhere; chunked transfer encoding; ranges ignored.
    chunked: bool,
    etag: Option<&'static str>,
    /// Report this total in Content-Range instead of the real one (a broken mirror).
    content_range_total: Option<usize>,
    /// Answer 503 when more than this many GETs are being served at once.
    max_active: Option<usize>,
    plan: fn(usize) -> Reply,
    /// Pause between 16 KiB body writes, in microseconds (adjustable while running).
    delay_us: AtomicU64,
    /// If set, every GET checks that this file exists.
    must_exist_on_get: Mutex<Option<PathBuf>>,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    gets: AtomicUsize,
    body_bytes: AtomicU64,
    active: AtomicUsize,
    max_active_seen: AtomicUsize,
    /// Ranges served with 206, in request order.
    ranges: Mutex<Vec<ByteRange>>,
    missing_on_get: AtomicUsize,
}

impl Mock {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            ranges: true,
            chunked: false,
            etag: None,
            content_range_total: None,
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
    let Some(head) = read_head(&mut socket).await else { return };
    let mut lines = head.lines();
    let method = lines.next().unwrap_or("").split(' ').next().unwrap_or("").to_string();
    let header = |name: &str| {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    let total = mock.data.len();
    let etag = mock.etag.map(|e| format!("ETag: {}\r\n", e)).unwrap_or_default();
    let accept = if mock.ranges && !mock.chunked { "Accept-Ranges: bytes\r\n" } else { "" };
    let length = |n: usize| if mock.chunked { "Transfer-Encoding: chunked\r\n".to_string() } else { format!("Content-Length: {}\r\n", n) };

    if method == "HEAD" {
        let resp = format!("HTTP/1.1 200 OK\r\n{}{}{}Connection: close\r\n\r\n", length(total), accept, etag);
        let _ = socket.write_all(resp.as_bytes()).await;
        return;
    }

    let s = &mock.stats;
    let index = s.gets.fetch_add(1, Ordering::SeqCst);
    let active = s.active.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = ActiveGuard(&s.active);
    if let Some(path) = mock.must_exist_on_get.lock().unwrap().as_ref() {
        if !path.exists() {
            s.missing_on_get.fetch_add(1, Ordering::SeqCst);
        }
    }

    let reply = if mock.max_active.is_some_and(|max| active > max) {
        Reply::Status(503, Some(1))
    } else {
        (mock.plan)(index)
    };
    if let Reply::Status(code, retry_after) = reply {
        let retry = retry_after.map(|s| format!("Retry-After: {}\r\n", s)).unwrap_or_default();
        let resp = format!("HTTP/1.1 {} Mock\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", code, retry);
        let _ = socket.write_all(resp.as_bytes()).await;
        return;
    }
    s.max_active_seen.fetch_max(active, Ordering::SeqCst);

    let requested = header("range").and_then(|r| {
        let (a, b) = r.strip_prefix("bytes=")?.split_once('-')?;
        Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()))
    });
    let validator_ok = match header("if-range") {
        Some(v) => mock.etag == Some(v.as_str()),
        None => true,
    };

    let (status, start, end) = match requested {
        Some((a, b)) if mock.ranges && !mock.chunked && validator_ok => {
            if a >= total {
                let resp = format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    total
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                return;
            }
            let end = b.unwrap_or(total - 1).min(total - 1);
            s.ranges.lock().unwrap().push(ByteRange::new(a as u64, end as u64).unwrap());
            (206, a, end + 1)
        }
        _ => (200, 0, total),
    };
    let body = &mock.data[start..end];
    let content_range = if status == 206 {
        format!("Content-Range: bytes {}-{}/{}\r\n", start, end - 1, mock.content_range_total.unwrap_or(total))
    } else {
        String::new()
    };
    let resp = format!(
        "HTTP/1.1 {} Mock\r\n{}{}{}{}Connection: close\r\n\r\n",
        status,
        length(body.len()),
        content_range,
        accept,
        etag
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
        s.body_bytes.fetch_add(piece.len() as u64, Ordering::SeqCst);
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
    let data = payload(1024 * KB, 37);
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
    let data = payload(512 * KB, 19);
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
    let data = payload(1024 * KB, 29);
    let mock = Arc::new(Mock::new(data.clone()));
    mock.delay_us.store(20_000, Ordering::SeqCst); // ~0.8 MB/s per connection
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
    let size = 512 * KB;
    let data = payload(size, 41);
    let mut mock = Mock::new(data.clone());
    mock.etag = Some("\"abc\"");
    let mock = Arc::new(mock);
    let url = serve(Arc::clone(&mock), "resume_test.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("resume_test.bin");
    let part = part_of(&out);

    // A previous run finished a non-aligned 128 KiB prefix; the rest of the .part is garbage.
    let done = ByteRange::new(0, 128 * KB as u64 - 1).unwrap();
    let mut on_disk = vec![0xAAu8; size];
    on_disk[..128 * KB].copy_from_slice(&data[..128 * KB]);
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
    assert_eq!(mock.stats.body_bytes.load(Ordering::SeqCst), (size - 128 * KB) as u64);
    assert!(mock.served_ranges().iter().all(|r| !r.intersects(&done)));
}

#[tokio::test]
async fn test_changed_etag_invalidates_resume() {
    let _history = setup().await;
    let size = 512 * KB;
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
    state.completed_ranges.push(ByteRange::new(0, 256 * KB as u64 - 1).unwrap());
    state.save_atomic(&DownloadState::state_file_path(&part)).unwrap();

    let engine = DownloadEngine::new(vec![url], options(&out, 2, 128 * KB));
    let path = run(&engine, None).await.expect("download should restart and succeed");

    assert_eq!(path, out);
    assert_file(&out, &new_data);
    assert!(mock.served_ranges().iter().any(|r| r.start == 0), "the stale prefix must be fetched again");
}

#[tokio::test]
async fn test_dynamic_work_stealing_integration() {
    let _history = setup().await;
    let data = payload(1024 * KB, 53);
    let mock = Arc::new(Mock::new(data.clone()));
    mock.delay_us.store(2_000, Ordering::SeqCst);
    let url = serve(Arc::clone(&mock), "stealing_test.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("stealing_test.bin");

    // One 1 MiB chunk and four connections: the other three must steal.
    let engine = DownloadEngine::new(vec![url], options(&out, 4, 1024 * KB));
    let path = run(&engine, None).await.expect("work stealing download should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    let starts: std::collections::BTreeSet<u64> = mock.served_ranges().iter().map(|r| r.start).collect();
    assert!(starts.len() > 1, "no work was stolen: {starts:?}");
}

#[tokio::test]
async fn test_fatal_failure_detection() {
    let _history = setup().await;
    let mut mock = Mock::new(payload(1024 * KB, 3));
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
    let data = payload(256 * KB, 73);
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
    assert_eq!(ranges[0].start, 0);
    assert_eq!(ranges[1].start, 16 * KB as u64, "the retry must continue where the first attempt stopped");
}

#[tokio::test]
async fn test_stalled_connection_is_retried() {
    let _history = setup().await;
    let data = payload(256 * KB, 79);
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
    assert_eq!(mock.served_ranges()[1].start, 32 * KB as u64);
}

#[tokio::test]
async fn test_503_bursts_with_retry_after_do_not_abort() {
    let _history = setup().await;
    let data = payload(1024 * KB, 83);
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
    let data = payload(2048 * KB, 89);
    let mut mock = Mock::new(data.clone());
    mock.max_active = Some(2);
    let mock = Arc::new(mock);
    mock.delay_us.store(2_000, Ordering::SeqCst);
    let url = serve(Arc::clone(&mock), "limited.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("limited.bin");

    let mut opts = options(&out, 8, 128 * KB);
    opts.max_retries = 2;
    let engine = DownloadEngine::new(vec![url], opts);
    let path = run(&engine, None).await.expect("download should adapt to the connection limit");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert!(mock.stats.max_active_seen.load(Ordering::SeqCst) <= 2);
}

#[tokio::test]
async fn test_cancel_saves_state_and_resume_skips_completed_bytes() {
    let _history = setup().await;
    let size = 2048 * KB;
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
    let before = mock.stats.body_bytes.load(Ordering::SeqCst);
    let first_new_range = mock.served_ranges().len();
    let engine = DownloadEngine::new(vec![url], options(&out, 1, 256 * KB));
    let path = run(&engine, None).await.expect("resume should succeed");

    assert_eq!(path, out);
    assert_file(&out, &data);
    assert_no_leftovers(&out);
    assert_eq!(mock.stats.body_bytes.load(Ordering::SeqCst) - before, size as u64 - saved);
    for r in &mock.served_ranges()[first_new_range..] {
        assert!(state.completed_ranges.iter().all(|done| !done.intersects(r)), "re-fetched {r}");
    }
}

#[tokio::test]
async fn test_mirror_with_wrong_content_range_total_is_rejected() {
    let _history = setup().await;
    let data = payload(1024 * KB, 101);
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
    assert!(bad.stats.gets.load(Ordering::SeqCst) >= 1, "the bad mirror was never tried");
    assert_eq!(other.stats.gets.load(Ordering::SeqCst), 0, "a mirror with a different size must be dropped at probe time");
}

#[tokio::test]
async fn test_max_speed_is_roughly_honored() {
    let _history = setup().await;
    let data = payload(1024 * KB, 103);
    let url = serve(Arc::new(Mock::new(data.clone())), "slow.bin").await;
    let temp = tempdir().unwrap();
    let out = temp.path().join("slow.bin");

    let mut opts = options(&out, 4, 128 * KB);
    opts.max_speed = Some(512 * KB as u64);
    let engine = DownloadEngine::new(vec![url], opts);
    let started = Instant::now();
    run(&engine, None).await.expect("download should succeed");
    let elapsed = started.elapsed();

    // 1 MiB at 512 KiB/s is 2s, minus a little burst allowance.
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
    let _history = setup().await;
    let data = payload(1024 * KB, 51);
    let url = serve(Arc::new(Mock::new(data.clone())), "collision_test.bin").await;
    let existing = b"Existing pre-allocated unrelated file with different size";

    // Twice, with the same history: the first run's record must not claim the second directory's file.
    for _ in 0..2 {
        let temp = tempdir().unwrap();
        let out = temp.path().join("collision_test.bin");
        std::fs::write(&out, existing).unwrap();

        let engine = DownloadEngine::new(vec![url.clone()], options(&out, 4, 256 * KB));
        let path = run(&engine, None).await.expect("download should auto-rename and succeed");

        let renamed = temp.path().join("collision_test (1).bin");
        assert_eq!(path, renamed);
        assert_eq!(std::fs::read(&out).unwrap(), existing);
        assert_file(&renamed, &data);
    }
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
        let mut mock = Mock::new(payload(512 * KB, 109));
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
