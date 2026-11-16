//! The speed benchmark: downloads from [`crate::netsim`] servers that behave as the links users
//! have do (a LAN, a server that caps each connection or each user, a long link that loses
//! connections, a host that limits requests, thousands of small files), timed. Not a test:
//!
//! `cargo test --release -p hyperfetch-core --lib speed_bench -- --ignored --nocapture`
//!
//! It writes the results as JSON to the file `ENDO_BENCH_OUT` names and as a markdown table to
//! `ENDO_BENCH_SUMMARY`'s (else prints both), titled with `ENDO_BENCH_LABEL`. With
//! `ENDO_BENCH_CHECK` set, a release build fails when a scenario is slower than
//! `bench/sim-thresholds.json` allows; `ENDO_BENCH_ONLY=lan,lossy` runs those alone. Every server
//! is new, on a port this process has not used, so no scenario starts with what another taught
//! the Autopilot.
//! At most 128 MiB is on disk at once (a temporary folder each run), and every run has a time limit.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use url::Url;

use crate::engine::{build_client, DownloadEngine, DownloadOptions};
use crate::netsim::{self, Decay, RateLimit, Server, Stats};

const MIB: usize = 1024 * 1024;
/// Longest one run may take.
const RUN_LIMIT: Duration = Duration::from_secs(90);

/// What a scenario downloads.
#[derive(Clone, Copy)]
enum Files {
    One(usize),
    /// `count` files of `min` to `max` bytes, `at_once` at a time over one client, as a batch is;
    /// `hashed`: with each file's SHA-256, as a listing that gives them has.
    Many { count: usize, min: usize, max: usize, at_once: usize, hashed: bool },
    /// One GET of `size` bytes over HTTP/2 through a link of `rtt` round trip (see [`h2_run`]),
    /// with the engine's HTTP/2 settings (`tuned`) or hyper's own.
    H2 { size: usize, rtt: Duration, tuned: bool },
}

struct Scenario {
    name: &'static str,
    what: &'static str,
    server: Server,
    files: Files,
    runs: usize,
}

fn scenarios() -> Vec<Scenario> {
    let rate_limit = RateLimit { requests: 6, window: Duration::from_secs(1), retry_after: 1, ban_after: 8, ban: Duration::from_secs(5) };
    let decay = Decay { from: 8 * MIB, full: 256 * 1024, rate: 64e3, every: Duration::from_secs(1) };
    vec![
        Scenario { name: "lan", what: "128 MiB, no cap", server: Server::default(), files: Files::One(128 * MIB), runs: 3 },
        Scenario {
            name: "per_connection",
            what: "32 MiB, 2 MB/s per connection",
            server: Server { per_connection: Some(2e6), ..Default::default() },
            files: Files::One(32 * MIB),
            runs: 3,
        },
        Scenario {
            name: "per_client",
            what: "32 MiB, 20 MB/s for all connections",
            server: Server { per_client: Some(20e6), ..Default::default() },
            files: Files::One(32 * MIB),
            runs: 3,
        },
        Scenario {
            name: "high_latency",
            what: "32 MiB, 150 ms to answer, 3 MB/s per connection",
            server: Server { per_connection: Some(3e6), first_byte: Duration::from_millis(150), ..Default::default() },
            files: Files::One(32 * MIB),
            runs: 3,
        },
        Scenario {
            name: "lossy",
            what: "32 MiB, 4 MB/s per connection, 80 ms to answer, one connection cut, one fading out",
            server: Server {
                per_connection: Some(4e6),
                first_byte: Duration::from_millis(80),
                cut: Some((4 * MIB, 512 * 1024)),
                decay: Some(decay),
                ..Default::default()
            },
            files: Files::One(32 * MIB),
            runs: 3,
        },
        Scenario {
            name: "rate_limited",
            what: "32 MiB, 4 MB/s per connection, 6 requests a second (429 past them)",
            server: Server { per_connection: Some(4e6), rate: Some(rate_limit), ..Default::default() },
            files: Files::One(32 * MIB),
            runs: 3,
        },
        Scenario {
            name: "h2_latency_hyper",
            what: "32 MiB over HTTP/2, 100 ms round trip, hyper's flow-control windows",
            server: Server::default(),
            files: Files::H2 { size: 32 * MIB, rtt: Duration::from_millis(100), tuned: false },
            runs: 3,
        },
        Scenario {
            name: "h2_latency",
            what: "the same with the engine's HTTP/2 settings",
            server: Server::default(),
            files: Files::H2 { size: 32 * MIB, rtt: Duration::from_millis(100), tuned: true },
            runs: 3,
        },
        Scenario {
            name: "small_files",
            what: "2,000 files of 1-100 KB, 10 ms to answer, 8 at a time",
            server: Server { first_byte: Duration::from_millis(10), ..Default::default() },
            files: Files::Many { count: 2000, min: 1000, max: 100_000, at_once: 8, hashed: false },
            runs: 1,
        },
        Scenario {
            name: "small_files_hashed",
            what: "the same, each with its SHA-256 (as a listing gives them)",
            server: Server { first_byte: Duration::from_millis(10), ..Default::default() },
            files: Files::Many { count: 2000, min: 1000, max: 100_000, at_once: 8, hashed: true },
            runs: 1,
        },
    ]
}

/// One scenario's result: the median run's (by time), with its time to the first byte.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct Measured {
    name: String,
    what: String,
    bytes: u64,
    seconds: f64,
    mb_per_s: f64,
    /// From the start until the server sent the first byte of a body.
    ttfb_ms: f64,
    /// Requests the server answered (HEAD and GET), in the median run.
    requests: usize,
    files_per_s: f64,
}

#[derive(serde::Deserialize, Default)]
struct Threshold {
    min_mb_per_s: Option<f64>,
    max_ttfb_ms: Option<f64>,
}

/// The sizes of a batch's files, the same every time (xorshift64 from a fixed seed).
fn sizes(count: usize, min: usize, max: usize) -> Vec<usize> {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    (0..count)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            min + (state % (max - min + 1) as u64) as usize
        })
        .collect()
}

/// Whether the file at `path` holds netsim's `size` bytes, read a MiB at a time.
fn holds(path: &std::path::Path, size: usize) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let (mut buf, mut at) = (vec![0u8; MIB], 0);
    loop {
        match file.read(&mut buf) {
            Ok(0) => return at == size,
            Ok(n) if buf[..n].iter().enumerate().all(|(i, &b)| b == netsim::byte(at + i)) => at += n,
            _ => return false,
        }
    }
}

fn options(dir: &std::path::Path, name: &str) -> DownloadOptions {
    // As the command line and the app download: the Autopilot on, up to 16 connections.
    DownloadOptions { output_path: Some(dir.join(name)), num_connections: 16, adaptive: true, remote_checksum: false, ..Default::default() }
}

/// One run: how long it took, the time to the first byte, the server's requests.
async fn run(scenario: &Scenario) -> (Duration, Duration, usize) {
    let dir = tempfile::tempdir().unwrap();
    let size = match scenario.files {
        Files::One(size) => size,
        Files::Many { .. } => 1,
        Files::H2 { size, rtt, tuned } => return h2_run(size, rtt, tuned).await,
    };
    let (url, stats) = netsim::serve(Server { size, ..scenario.server.clone() }).await;
    let files: Vec<(Url, usize, std::path::PathBuf)> = match scenario.files {
        Files::One(size) => vec![(url, size, dir.path().join("f.bin"))],
        Files::Many { count, min, max, .. } => sizes(count, min, max)
            .into_iter()
            .enumerate()
            .map(|(i, size)| (url.join(&format!("/sized/{size}/{i}.bin")).unwrap(), size, dir.path().join(format!("{i}.bin"))))
            .collect(),
        Files::H2 { .. } => unreachable!(),
    };
    let hashed = matches!(scenario.files, Files::Many { hashed: true, .. });
    let checksum = |size: usize| {
        use sha2::Digest;
        hashed.then(|| format!("sha256:{}", sha2::Sha256::digest(netsim::file(size)).iter().map(|b| format!("{b:02x}")).collect::<String>()))
    };
    let at_once = match scenario.files {
        Files::Many { at_once, .. } => at_once,
        _ => 1,
    };
    // One client for all the files, as a batch has (see the command line's `run_jobs`).
    let client = build_client(&options(dir.path(), "")).unwrap();
    let started = Instant::now();
    let downloads = futures_util::stream::iter(files.iter().cloned())
        .map(|(url, size, path)| {
            let options = DownloadOptions { output_path: Some(path), expected_checksum: checksum(size), ..options(dir.path(), "") };
            let engine = DownloadEngine::with_client(vec![url], options, client.clone());
            async move { engine.run(None).await }
        })
        .buffer_unordered(at_once)
        .collect::<Vec<_>>();
    let done = tokio::time::timeout(RUN_LIMIT, downloads).await;
    let took = started.elapsed();
    for result in done.unwrap_or_else(|_| panic!("{} took over {RUN_LIMIT:?}", scenario.name)) {
        result.unwrap();
    }
    for (_, size, path) in &files {
        assert!(holds(path, *size), "{}: {} is wrong", scenario.name, path.display());
    }
    (took, first_byte(&stats, started), requests(&stats))
}

/// One GET of `size` bytes over HTTP/2 (cleartext, known to the client beforehand) through a link
/// of `rtt` round trip, with the engine's HTTP/2 settings (`tuned`, see `engine::http2_tuned`) or
/// hyper's: what one download gets from a server that speaks HTTP/2 alone, whose ranges share one
/// connection (see `DownloadOptions::http2`).
async fn h2_run(size: usize, rtt: Duration, tuned: bool) -> (Duration, Duration, usize) {
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = server.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((socket, _)) = server.accept().await {
            tokio::spawn(async move {
                let Ok(mut connection) = h2::server::handshake(socket).await else { return };
                // Accepting drives the connection, the body's frames too.
                while let Some(Ok((_, mut respond))) = connection.accept().await {
                    let Ok(mut body) = respond.send_response(http::Response::new(()), false) else { return };
                    let _ = body.send_data(bytes::Bytes::from(netsim::file(size)), true);
                }
            });
        }
    });
    let link = delayed(at, rtt / 2).await;
    let builder = reqwest::Client::builder().http2_prior_knowledge();
    let client = if tuned { crate::engine::http2_tuned(builder) } else { builder }.build().unwrap();
    let started = Instant::now();
    let fetch = async {
        let mut response = client.get(format!("http://{link}/f.bin")).send().await.unwrap();
        let (mut first, mut got) = (None, 0);
        while let Some(chunk) = response.chunk().await.unwrap() {
            first.get_or_insert_with(|| started.elapsed());
            got += chunk.len();
        }
        assert_eq!(got, size);
        first.unwrap_or_default()
    };
    let first = tokio::time::timeout(RUN_LIMIT, fetch).await.expect("within the time limit");
    (started.elapsed(), first, 1)
}

/// A link to `to` that holds what goes through it `delay` each way, with no cap on how much it
/// holds: the round trip alone limits what flow control lets through.
async fn delayed(to: std::net::SocketAddr, delay: Duration) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((near, _)) = listener.accept().await {
            let Ok(far) = tokio::net::TcpStream::connect(to).await else { continue };
            let ((near_from, near_to), (far_from, far_to)) = (near.into_split(), far.into_split());
            tokio::spawn(pump(near_from, far_to, delay));
            tokio::spawn(pump(far_from, near_to, delay));
        }
    });
    at
}

async fn pump(mut from: tokio::net::tcp::OwnedReadHalf, mut to: tokio::net::tcp::OwnedWriteHalf, delay: Duration) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
    let writer = tokio::spawn(async move {
        while let Some((read_at, bytes)) = rx.recv().await {
            tokio::time::sleep_until(read_at + delay).await;
            if to.write_all(&bytes).await.is_err() {
                return;
            }
        }
        let _ = to.shutdown().await;
    });
    let mut buf = vec![0u8; 64 * 1024];
    while let Ok(n @ 1..) = from.read(&mut buf).await {
        if tx.send((tokio::time::Instant::now(), buf[..n].to_vec())).is_err() {
            break;
        }
    }
    drop(tx);
    let _ = writer.await;
}

fn first_byte(stats: &Stats, started: Instant) -> Duration {
    stats.first_sent.get().map_or(Duration::ZERO, |at| at.saturating_duration_since(started))
}

fn requests(stats: &Stats) -> usize {
    stats.gets.load(Ordering::SeqCst) + stats.heads.load(Ordering::SeqCst) + stats.throttled.load(Ordering::SeqCst)
}

async fn measure(scenario: &Scenario) -> Measured {
    let mut runs = Vec::new();
    for _ in 0..scenario.runs {
        runs.push(run(scenario).await);
    }
    runs.sort_by_key(|run| run.0);
    let (took, ttfb, requests) = runs[runs.len() / 2];
    let (bytes, files) = match scenario.files {
        Files::One(size) | Files::H2 { size, .. } => (size as u64, 1),
        Files::Many { count, min, max, .. } => (sizes(count, min, max).iter().sum::<usize>() as u64, count),
    };
    let seconds = took.as_secs_f64();
    Measured {
        name: scenario.name.into(),
        what: scenario.what.into(),
        bytes,
        seconds,
        mb_per_s: bytes as f64 / 1e6 / seconds,
        ttfb_ms: ttfb.as_secs_f64() * 1e3,
        requests,
        files_per_s: files as f64 / seconds,
    }
}

fn table(results: &[Measured], profile: &str) -> String {
    let label = std::env::var("ENDO_BENCH_LABEL").map(|l| format!(", {l}")).unwrap_or_default();
    let mut out = format!("### Speed benchmark ({profile} build{label})\n\n| Scenario | What | Time | MB/s | First byte | Requests | Files/s |\n|---|---|---:|---:|---:|---:|---:|\n");
    for m in results {
        out += &format!(
            "| `{}` | {} | {:.2} s | {:.1} | {:.0} ms | {} | {:.1} |\n",
            m.name, m.what, m.seconds, m.mb_per_s, m.ttfb_ms, m.requests, m.files_per_s
        );
    }
    out
}

/// Each result past its threshold, as a line.
fn regressions(results: &[Measured], thresholds: &std::collections::HashMap<String, Threshold>) -> Vec<String> {
    let mut found = Vec::new();
    for m in results {
        let Some(t) = thresholds.get(&m.name) else { continue };
        if let Some(min) = t.min_mb_per_s.filter(|&min| m.mb_per_s < min) {
            found.push(format!("{}: {:.1} MB/s, under the {min} MB/s allowed", m.name, m.mb_per_s));
        }
        if let Some(max) = t.max_ttfb_ms.filter(|&max| m.ttfb_ms > max) {
            found.push(format!("{}: first byte after {:.0} ms, over the {max} ms allowed", m.name, m.ttfb_ms));
        }
    }
    found
}

#[test]
fn test_regressions_are_told_from_the_thresholds() {
    let m = |name: &str, mb_per_s, ttfb_ms| Measured {
        name: name.into(),
        what: String::new(),
        bytes: 0,
        seconds: 1.0,
        mb_per_s,
        ttfb_ms,
        requests: 0,
        files_per_s: 0.0,
    };
    let thresholds: std::collections::HashMap<String, Threshold> =
        serde_json::from_str(r#"{"a": {"min_mb_per_s": 10}, "b": {"max_ttfb_ms": 100}}"#).unwrap();
    let found = regressions(&[m("a", 9.0, 999.0), m("b", 1.0, 101.0), m("c", 0.0, 1e9), m("a", 10.0, 0.0)], &thresholds);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].starts_with("a:") && found[1].starts_with("b:"));
    // The checked-in thresholds parse, and name only scenarios there are.
    let checked_in = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/sim-thresholds.json")).unwrap();
    let checked_in: std::collections::HashMap<String, Threshold> = serde_json::from_str(&checked_in).unwrap();
    let names: Vec<&str> = scenarios().iter().map(|s| s.name).collect();
    assert!(checked_in.keys().all(|k| names.contains(&k.as_str())), "{:?}", checked_in.keys());
    assert_eq!(sizes(3, 1, 10), sizes(3, 1, 10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn speed_bench() {
    let only = std::env::var("ENDO_BENCH_ONLY").ok();
    let mut results = Vec::new();
    for scenario in scenarios().iter().filter(|s| only.as_deref().is_none_or(|only| only.split(',').any(|n| n == s.name))) {
        let measured = measure(scenario).await;
        eprintln!("{}: {:.2} s, {:.1} MB/s, first byte {:.0} ms, {} requests", measured.name, measured.seconds, measured.mb_per_s, measured.ttfb_ms, measured.requests);
        results.push(measured);
    }
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let json = serde_json::to_string_pretty(&serde_json::json!({ "profile": profile, "os": std::env::consts::OS, "scenarios": results })).unwrap();
    let table = table(&results, profile);
    match std::env::var_os("ENDO_BENCH_OUT") {
        Some(out) => std::fs::write(out, &json).unwrap(),
        None => println!("{json}"),
    }
    let thresholds = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/sim-thresholds.json")).unwrap();
    let found = regressions(&results, &serde_json::from_str(&thresholds).unwrap());
    let checked = std::env::var_os("ENDO_BENCH_CHECK").is_some() && !cfg!(debug_assertions);
    let verdict = match (checked, found.is_empty()) {
        (false, _) => String::new(),
        (true, true) => "\nEvery scenario is within `bench/sim-thresholds.json`.\n".into(),
        (true, false) => format!("\n**Slower than `bench/sim-thresholds.json` allows:**\n\n{}\n", found.iter().map(|f| format!("- {f}\n")).collect::<String>()),
    };
    match std::env::var_os("ENDO_BENCH_SUMMARY") {
        Some(out) => std::fs::write(out, format!("{table}{verdict}")).unwrap(),
        None => println!("{table}{verdict}"),
    }
    assert!(!checked || found.is_empty(), "{found:?}");
}
