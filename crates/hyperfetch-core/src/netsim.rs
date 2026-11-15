//! A network to test the engine against, as `bench/throttled_server.py` is one to benchmark it
//! against: [`serve`], a local HTTP server that behaves as each test sets (a speed cap on each
//! connection or on all of them together, a cap on connections at once, a slow first byte, no
//! ranges, a connection that slows down and stalls or is cut off), and [`Model`], the same caps as
//! numbers, with noise from a fixed seed, for a controller to be stepped through without a clock.
//!
//! Bodies have a fixed length of a few MB at most, and every connection ends: a stalled one is
//! closed after `HOLD`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

use crate::worker::RateLimiter;

/// Longest a stalled connection is held open.
const HOLD: Duration = Duration::from_secs(60);
/// Bytes written at a time, each paced.
const PIECE: usize = 16 * 1024;

/// The file's byte at `offset`.
pub(crate) fn byte(offset: usize) -> u8 {
    (offset % 251) as u8
}

/// The whole file of `size` bytes, to compare a download with.
pub(crate) fn file(size: usize) -> Vec<u8> {
    (0..size).map(byte).collect()
}

/// How the server behaves. The default serves `size` bytes as fast as it can, with ranges.
#[derive(Clone, Default)]
pub(crate) struct Server {
    pub size: usize,
    /// Bytes per second each connection is held to.
    pub per_connection: Option<f64>,
    /// Bytes per second all connections share: a cap that more connections do not raise.
    pub per_client: Option<f64>,
    /// GETs served at once; one more is refused with `refusal`.
    pub max_connections: Option<usize>,
    /// The status and Retry-After seconds of a refusal.
    pub refusal: (u16, Option<u64>),
    /// Wait before every answer.
    pub first_byte: Duration,
    /// Ranges are not served: every GET gets the whole file with 200.
    pub no_ranges: bool,
    /// The first GET whose range starts at `Decay::from` or later slows down and then stalls.
    pub decay: Option<Decay>,
    /// The first GET whose range starts at this offset or later is cut off after this many bytes.
    pub cut: Option<(usize, usize)>,
}

/// A connection that slows down and then stalls.
#[derive(Clone, Copy)]
pub(crate) struct Decay {
    pub from: usize,
    /// Bytes it sends as the others do first.
    pub full: usize,
    /// Its speed from then on, halved every `every` until under 1 KB/s, when it sends nothing more.
    pub rate: f64,
    pub every: Duration,
}

/// What the server saw.
#[derive(Default)]
pub(crate) struct Stats {
    /// GETs answered, refused ones among them.
    pub gets: AtomicUsize,
    /// GETs refused for `max_connections`.
    pub refused: AtomicUsize,
    /// GETs being answered now, and the most ever at once.
    pub open: AtomicUsize,
    pub peak: AtomicUsize,
    decayed: AtomicBool,
    cut: AtomicBool,
}

struct Open<'a>(&'a AtomicUsize);

impl Drop for Open<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serves `server` at `<url>` (`/f.bin`) on a port this process has not used before (see
/// `hosts::unseen_listener`), until the test's runtime ends.
pub(crate) async fn serve(server: Server) -> (Url, Arc<Stats>) {
    let listener = crate::hosts::unseen_listener().await;
    let url = Url::parse(&format!("http://{}/f.bin", listener.local_addr().unwrap())).unwrap();
    let (server, stats) = (Arc::new(server), Arc::new(Stats::default()));
    let shared = server.per_client.map(|rate| Arc::new(RateLimiter::new(rate as u64)));
    let seen = Arc::clone(&stats);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(answer(socket, Arc::clone(&server), Arc::clone(&seen), shared.clone()));
        }
    });
    (url, stats)
}

async fn answer(mut socket: tokio::net::TcpStream, server: Arc<Server>, stats: Arc<Stats>, shared: Option<Arc<RateLimiter>>) {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match socket.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let size = server.size;
    let range = head
        .lines()
        .find_map(|l| l.strip_prefix("range: bytes=")?.trim().split_once('-'))
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?.min(size - 1))))
        .filter(|_| !server.no_ranges);
    tokio::time::sleep(server.first_byte).await;
    let ranges = if server.no_ranges { "" } else { "Accept-Ranges: bytes\r\n" };
    if head.starts_with("head ") {
        let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\n{ranges}Connection: close\r\n\r\n").as_bytes()).await;
        return;
    }
    stats.gets.fetch_add(1, Ordering::SeqCst);
    let open = stats.open.fetch_add(1, Ordering::SeqCst) + 1;
    let _open = Open(&stats.open);
    if server.max_connections.is_some_and(|most| open > most) {
        stats.refused.fetch_add(1, Ordering::SeqCst);
        let (status, after) = server.refusal;
        let after = after.map(|secs| format!("Retry-After: {secs}\r\n")).unwrap_or_default();
        let _ = socket.write_all(format!("HTTP/1.1 {status} Busy\r\n{after}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await;
        return;
    }
    stats.peak.fetch_max(open, Ordering::SeqCst);
    let (start, end) = range.unwrap_or((0, size - 1));
    let status = match range {
        Some(_) => format!("206 Partial Content\r\nContent-Range: bytes {start}-{end}/{size}"),
        None => "200 OK".to_string(),
    };
    let reply = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n{ranges}ETag: \"v1\"\r\nConnection: close\r\n\r\n", end + 1 - start);
    if socket.write_all(reply.as_bytes()).await.is_err() {
        return;
    }
    let first = |from: usize, flag: &AtomicBool| start >= from && !flag.swap(true, Ordering::SeqCst);
    let decay = server.decay.filter(|decay| first(decay.from, &stats.decayed));
    let cut = server.cut.filter(|&(from, _)| first(from, &stats.cut)).map(|(_, after)| after);
    let own = server.per_connection.map(|rate| RateLimiter::new(rate as u64));
    let (mut sent, mut slowed) = (0, None);
    let (mut from_client, mut to_client) = socket.split();
    for offset in (start..=end).step_by(PIECE) {
        let piece: Vec<u8> = (offset..=end.min(offset + PIECE - 1)).map(byte).collect();
        if cut.is_some_and(|after| sent >= after) {
            return;
        }
        let pace = async {
            match decay.filter(|decay| sent >= decay.full) {
                Some(decay) => {
                    let since = *slowed.get_or_insert_with(tokio::time::Instant::now);
                    let rate = decay.rate / 2f64.powf(since.elapsed().as_secs_f64() / decay.every.as_secs_f64());
                    tokio::time::sleep(if rate < 1024.0 { HOLD } else { Duration::from_secs_f64(piece.len() as f64 / rate) }).await;
                    rate >= 1024.0
                }
                None => {
                    if let Some(own) = &own {
                        own.acquire(piece.len() as u64).await;
                    }
                    if let Some(shared) = &shared {
                        shared.acquire(piece.len() as u64).await;
                    }
                    true
                }
            }
        };
        // A connection the client closed stops counting at once, as with servers that watch for it.
        let paced = tokio::select! {
            paced = pace => paced,
            _ = closed(&mut from_client) => false,
        };
        if !paced || to_client.write_all(&piece).await.is_err() {
            return;
        }
        sent += piece.len();
    }
}

/// Once the client closes its side: it sends nothing after its request.
async fn closed(from_client: &mut tokio::net::tcp::ReadHalf<'_>) {
    let mut byte = [0u8; 1];
    while let Ok(1..) = from_client.read(&mut byte).await {}
}

/// The caps of a [`Server`] as numbers: what `width` connections bring together, in bytes per
/// second, for a controller stepped through measurement windows without a clock.
pub(crate) struct Model {
    pub per_connection: f64,
    pub per_client: f64,
    /// The user's line.
    pub line: f64,
    /// Connections served at once: more bring nothing.
    pub max_connections: usize,
    /// Each reading is off by up to this share, either way.
    pub noise: f64,
    /// The state of the generator the noise comes from (xorshift64), from a fixed seed.
    pub seed: u64,
}

impl Default for Model {
    fn default() -> Self {
        Self { per_connection: f64::INFINITY, per_client: f64::INFINITY, line: f64::INFINITY, max_connections: usize::MAX, noise: 0.04, seed: 0x9E37_79B9_7F4A_7C15 }
    }
}

impl Model {
    pub fn speed(&mut self, width: usize) -> f64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        let unit = (self.seed >> 11) as f64 / (1u64 << 53) as f64;
        let ideal = (width.min(self.max_connections) as f64 * self.per_connection).min(self.per_client).min(self.line);
        ideal * (1.0 + self.noise * (2.0 * unit - 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    async fn get(url: &Url, range: Option<(usize, usize)>) -> reqwest::Response {
        let mut request = reqwest::Client::new().get(url.clone());
        if let Some((start, end)) = range {
            request = request.header("Range", format!("bytes={start}-{end}"));
        }
        tokio::time::timeout(Duration::from_secs(10), request.send()).await.unwrap().unwrap()
    }

    /// Each connection holds to its cap, all of them together to the client's, one past the most
    /// served at once is refused, and a server without ranges sends the whole file.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_the_server_keeps_to_its_caps() {
        let size = 256 * 1024;
        let (url, stats) = serve(Server { size, per_connection: Some(512.0 * 1024.0), ..Default::default() }).await;
        let started = Instant::now();
        let body = get(&url, Some((0, size - 1))).await.bytes().await.unwrap();
        assert_eq!(body.to_vec(), file(size));
        assert!(started.elapsed() >= Duration::from_millis(350), "{:?}", started.elapsed());
        assert_eq!(stats.gets.load(Ordering::SeqCst), 1);

        // Four connections share 1 MB/s: 512 KiB takes about half a second, as one would.
        let (url, _) = serve(Server { size: 512 * 1024, per_client: Some(1e6), ..Default::default() }).await;
        let started = Instant::now();
        let quarters = (0..4).map(|n| get(&url, Some((n * 128 * 1024, (n + 1) * 128 * 1024 - 1))));
        for answer in futures_util::future::join_all(quarters).await {
            assert_eq!(answer.bytes().await.unwrap().len(), 128 * 1024);
        }
        assert!(started.elapsed() >= Duration::from_millis(400), "{:?}", started.elapsed());

        let (url, stats) = serve(Server { size, per_connection: Some(64.0 * 1024.0), max_connections: Some(1), refusal: (429, Some(3)), ..Default::default() }).await;
        let held = get(&url, None).await;
        let refused = get(&url, None).await;
        assert_eq!((refused.status().as_u16(), refused.headers()["retry-after"].to_str().unwrap()), (429, "3"));
        drop(held);
        assert_eq!((stats.refused.load(Ordering::SeqCst), stats.peak.load(Ordering::SeqCst)), (1, 1));

        let (url, _) = serve(Server { size: 1000, no_ranges: true, ..Default::default() }).await;
        let whole = get(&url, Some((500, 999))).await;
        assert_eq!(whole.status().as_u16(), 200);
        assert_eq!(whole.bytes().await.unwrap().to_vec(), file(1000));
    }

    /// The first GET from the given offset slows down and then goes silent; or is cut off. Others
    /// are served as usual.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_a_connection_slows_down_and_stalls_or_is_cut_off() {
        let size = 256 * 1024;
        let decay = Decay { from: 1, full: 32 * 1024, rate: 64.0 * 1024.0, every: Duration::from_millis(200) };
        let (url, _) = serve(Server { size, decay: Some(decay), ..Default::default() }).await;
        let mut slowing = get(&url, Some((1, size - 1))).await;
        let mut got = 0;
        let stalled = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(bytes) = slowing.chunk().await.unwrap() {
                got += bytes.len();
            }
        });
        assert!(stalled.await.is_err(), "the body went silent");
        assert!((32 * 1024..size - 1).contains(&got), "{got}");
        assert_eq!(get(&url, Some((1, size - 1))).await.bytes().await.unwrap().len(), size - 1, "once");

        let (url, _) = serve(Server { size, cut: Some((0, 64 * 1024)), ..Default::default() }).await;
        assert!(get(&url, Some((0, size - 1))).await.bytes().await.is_err(), "cut off");
        assert_eq!(get(&url, Some((0, size - 1))).await.bytes().await.unwrap().len(), size);
    }

    /// The model's readings: the least of its caps, off by its noise at most, the same for the
    /// same seed.
    #[test]
    fn test_the_model_reads_the_least_of_its_caps() {
        let mut model = Model { per_connection: 1e6, per_client: 5e6, max_connections: 4, ..Default::default() };
        let readings: Vec<f64> = [1, 2, 4, 8, 16].iter().map(|&width| model.speed(width)).collect();
        for (reading, ideal) in readings.iter().zip([1e6, 2e6, 4e6, 4e6, 4e6]) {
            assert!((reading / ideal - 1.0).abs() <= 0.05, "{reading} for {ideal}");
        }
        let mut again = Model { per_connection: 1e6, per_client: 5e6, max_connections: 4, ..Default::default() };
        assert_eq!(readings, [1, 2, 4, 8, 16].iter().map(|&width| again.speed(width)).collect::<Vec<f64>>());
    }
}
