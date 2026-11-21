//! A network to test the engine against, as `bench/throttled_server.py` is one to benchmark it
//! against: [`serve`], a local HTTP server that behaves as each test sets (a speed cap on each
//! connection or on all of them together, a cap on connections at once, a slow first byte, no
//! ranges, a connection that slows down and stalls or is cut off), and [`Model`], the same caps as
//! numbers, with noise from a fixed seed, for a controller to be stepped through without a clock.
//!
//! Bodies have a fixed length of a few MB at most (the speed benchmark's, `crate::speedbench`, of
//! up to 128 MiB), and every connection ends: a stalled one is closed after `HOLD`. A path
//! `/sized/<n>/...` serves a file of `n` bytes in place of `Server::size`.

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

/// The file's bytes from `offset`, `len` of them (at most `PIECE`), without a loop per byte: the
/// benchmark's fast scenarios must time the client, not this server.
fn bytes_at(offset: usize, len: usize) -> &'static [u8] {
    static PATTERN: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| (0..PIECE + 251).map(byte).collect());
    &PATTERN[offset % 251..offset % 251 + len]
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
    /// Each connection starts at a sixteenth of `per_connection` and doubles four times evenly over
    /// this long to reach it, as a connection's congestion window grows on a far host.
    pub slow_start: Option<Duration>,
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
    /// A `Link` header every answer carries.
    pub link: Option<String>,
    /// A Metalink, the answer to a GET of any path ending in `.meta4`.
    pub metalink: Option<String>,
    /// A zsync control file, the answer to a GET of any path ending in `.zsync`.
    pub zsync: Option<Vec<u8>>,
    /// A limit on how fast requests (of any path, HEAD too) may come.
    pub rate: Option<RateLimit>,
    /// A budget of requests (of any path, HEAD too) the client spends.
    pub budget: Option<Budget>,
    /// Bytes from the first offset up to the second are sent inverted: another version of the
    /// file under the same size and ETag, as a stale mirror serves.
    pub corrupt: Option<(usize, usize)>,
    /// The GETs, counted from 0 in the order they come, whose answer waits this much longer.
    pub hold: Vec<(usize, Duration)>,
    /// The GETs, counted so, refused with `refusal` whatever else is open.
    pub refuse: Vec<usize>,
}

/// At most `requests` requests in any `window`; one more is refused with 429 and Retry-After
/// `retry_after` seconds. A client refused `ban_after` times within a window is shut out for
/// `ban`: each of its connections is closed unanswered, as a firewall does.
#[derive(Clone, Copy)]
pub(crate) struct RateLimit {
    pub requests: usize,
    pub window: Duration,
    pub retry_after: u64,
    pub ban_after: usize,
    pub ban: Duration,
}

/// The budget of requests a host that limits how often it is asked keeps for each client: `burst`
/// at once, then one more every `every`, up to `burst` again. One past it is refused with 429 and
/// Retry-After `retry_after` seconds.
#[derive(Clone, Copy)]
pub(crate) struct Budget {
    pub burst: usize,
    pub every: Duration,
    pub retry_after: u64,
}

/// A request that comes this long after a 429, within the rest it asked for, came too early: one
/// sent before the 429 arrived may come a little later.
const EARLY_SLACK: Duration = Duration::from_millis(200);

/// Where a [`RateLimit`] stands.
#[derive(Default)]
struct Limiting {
    taken: std::collections::VecDeque<tokio::time::Instant>,
    refusals: std::collections::VecDeque<tokio::time::Instant>,
    last_refusal: Option<tokio::time::Instant>,
    shut_until: Option<tokio::time::Instant>,
    /// What is left of a [`Budget`], and when that was counted.
    left: Option<(f64, tokio::time::Instant)>,
}

enum Verdict {
    Served,
    Refused,
    Shut,
}

impl Limiting {
    fn shuts_out(&self, now: tokio::time::Instant) -> bool {
        self.shut_until.is_some_and(|until| until > now)
    }

    fn admit(&mut self, limit: &RateLimit, stats: &Stats, now: tokio::time::Instant) -> Verdict {
        if self.shuts_out(now) {
            return Verdict::Shut;
        }
        let rest = Duration::from_secs(limit.retry_after);
        if self.last_refusal.is_some_and(|at| (EARLY_SLACK..rest).contains(&(now - at))) {
            stats.early.fetch_add(1, Ordering::SeqCst);
        }
        self.taken.retain(|&at| now - at < limit.window);
        if self.taken.len() < limit.requests {
            self.taken.push_back(now);
            return Verdict::Served;
        }
        stats.throttled.fetch_add(1, Ordering::SeqCst);
        self.refusals.retain(|&at| now - at < limit.window);
        self.refusals.push_back(now);
        self.last_refusal = Some(now);
        if self.refusals.len() >= limit.ban_after {
            self.shut_until = Some(now + limit.ban);
            self.refusals.clear();
            self.taken.clear();
        }
        Verdict::Refused
    }

    /// Whether a request may spend one of `budget` at `now`.
    fn spend(&mut self, budget: &Budget, stats: &Stats, now: tokio::time::Instant) -> bool {
        let rest = Duration::from_secs(budget.retry_after);
        if self.last_refusal.is_some_and(|at| (EARLY_SLACK..rest).contains(&(now - at))) {
            stats.early.fetch_add(1, Ordering::SeqCst);
        }
        let (left, counted) = self.left.get_or_insert((budget.burst as f64, now));
        *left = (*left + (now - *counted).as_secs_f64() / budget.every.as_secs_f64()).min(budget.burst as f64);
        *counted = now;
        if *left >= 1.0 {
            *left -= 1.0;
            return true;
        }
        stats.throttled.fetch_add(1, Ordering::SeqCst);
        self.last_refusal = Some(now);
        false
    }
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
    /// Requests of any kind, refused ones among them.
    pub asked: AtomicUsize,
    /// GETs answered, refused ones among them, and HEADs.
    pub gets: AtomicUsize,
    pub heads: AtomicUsize,
    /// GETs refused for `max_connections` or `Server::refuse`.
    pub refused: AtomicUsize,
    /// GETs being answered now, and the most ever at once.
    pub open: AtomicUsize,
    pub peak: AtomicUsize,
    /// Bytes of the file sent.
    pub served: AtomicUsize,
    /// Requests refused for `Server::rate`, those that came in a rest a 429 asked for, and
    /// connections closed unanswered while the client was shut out.
    pub throttled: AtomicUsize,
    pub early: AtomicUsize,
    pub shut: AtomicUsize,
    /// When the first byte of a body went out.
    pub first_sent: std::sync::OnceLock<std::time::Instant>,
    /// The path of each GET answered and each request refused for `Server::budget`, in order,
    /// with whether it was refused.
    pub log: parking_lot::Mutex<Vec<(String, bool)>>,
    /// Each request as it was answered: when after the first came, what it asked, and the status.
    wire: parking_lot::Mutex<Vec<String>>,
    began: std::sync::OnceLock<std::time::Instant>,
    decayed: AtomicBool,
    cut: AtomicBool,
}

impl Stats {
    /// What the server saw, a request a line, for a failing test to tell.
    pub(crate) fn wire(&self) -> String {
        self.wire.lock().join("\n")
    }

    fn note(&self, head: &str, status: &str) {
        let ms = self.began.get_or_init(std::time::Instant::now).elapsed().as_millis();
        let method = head.split_whitespace().next().unwrap_or("?").to_ascii_uppercase();
        let range = head.lines().find_map(|l| l.strip_prefix("range: ")).unwrap_or("-").trim().to_string();
        self.wire.lock().push(format!("{ms:>6} ms  {method} {range} -> {status}"));
    }
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
    let limiting = Arc::new(parking_lot::Mutex::new(Limiting::default()));
    let seen = Arc::clone(&stats);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            if limiting.lock().shuts_out(tokio::time::Instant::now()) {
                seen.shut.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            tokio::spawn(answer(socket, Arc::clone(&server), Arc::clone(&seen), shared.clone(), Arc::clone(&limiting)));
        }
    });
    (url, stats)
}

async fn answer(
    mut socket: tokio::net::TcpStream,
    server: Arc<Server>,
    stats: Arc<Stats>,
    shared: Option<Arc<RateLimiter>>,
    limiting: Arc<parking_lot::Mutex<Limiting>>,
) {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match socket.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    stats.asked.fetch_add(1, Ordering::SeqCst);
    stats.began.get_or_init(std::time::Instant::now);
    let asked_for = String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or("/").to_string();
    let lower = String::from_utf8_lossy(&head).to_ascii_lowercase();
    if let Some(budget) = &server.budget {
        if !limiting.lock().spend(budget, &stats, tokio::time::Instant::now()) {
            stats.note(&lower, "429 (budget)");
            stats.log.lock().push((asked_for, true));
            let refusal = format!("HTTP/1.1 429 Too Many Requests\r\nRetry-After: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", budget.retry_after);
            let _ = socket.write_all(refusal.as_bytes()).await;
            return;
        }
    }
    if let Some(limit) = &server.rate {
        let verdict = limiting.lock().admit(limit, &stats, tokio::time::Instant::now());
        match verdict {
            Verdict::Served => {}
            Verdict::Shut => {
                stats.note(&lower, "shut out");
                stats.shut.fetch_add(1, Ordering::SeqCst);
                return;
            }
            Verdict::Refused => {
                stats.note(&lower, "429 (rate)");
                let refusal = format!("HTTP/1.1 429 Too Many Requests\r\nRetry-After: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", limit.retry_after);
                let _ = socket.write_all(refusal.as_bytes()).await;
                return;
            }
        }
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let path = head.split_whitespace().nth(1).unwrap_or("/");
    let size = path.strip_prefix("/sized/").and_then(|rest| rest.split('/').next()?.parse().ok()).unwrap_or(server.size);
    let range = head
        .lines()
        .find_map(|l| l.strip_prefix("range: bytes=")?.trim().split_once('-'))
        // `bytes=0-`: to the end.
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, if b.is_empty() { size - 1 } else { b.parse::<usize>().ok()?.min(size - 1) })))
        .filter(|_| !server.no_ranges);
    tokio::time::sleep(server.first_byte).await;
    let ranges = if server.no_ranges { "" } else { "Accept-Ranges: bytes\r\n" };
    let link = server.link.as_ref().map(|link| format!("Link: {link}\r\n")).unwrap_or_default();
    if let Some(meta) = server.metalink.as_ref().filter(|_| head.split_whitespace().nth(1).is_some_and(|path| path.ends_with(".meta4"))) {
        let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{meta}", meta.len()).as_bytes()).await;
        return;
    }
    if let Some(zsync) = server.zsync.as_ref().filter(|_| head.split_whitespace().nth(1).is_some_and(|path| path.ends_with(".zsync"))) {
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", zsync.len());
        let _ = socket.write_all(&[head.as_bytes(), zsync].concat()).await;
        return;
    }
    if head.starts_with("head ") {
        stats.note(&head, "200");
        stats.heads.fetch_add(1, Ordering::SeqCst);
        let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\n{ranges}{link}Connection: close\r\n\r\n").as_bytes()).await;
        return;
    }
    let nth = stats.gets.fetch_add(1, Ordering::SeqCst);
    stats.log.lock().push((asked_for, false));
    if let Some(&(_, wait)) = server.hold.iter().find(|(n, _)| *n == nth) {
        tokio::time::sleep(wait).await;
    }
    let open = stats.open.fetch_add(1, Ordering::SeqCst) + 1;
    let _open = Open(&stats.open);
    if server.max_connections.is_some_and(|most| open > most) || server.refuse.contains(&nth) {
        stats.refused.fetch_add(1, Ordering::SeqCst);
        let (status, after) = server.refusal;
        stats.note(&head, &format!("{status} (GET {nth})"));
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
    stats.note(&head, &format!("{} (GET {nth}, {} bytes)", if range.is_some() { 206 } else { 200 }, end + 1 - start));
    let reply = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n{ranges}{link}ETag: \"v1\"\r\nConnection: close\r\n\r\n", end + 1 - start);
    if socket.write_all(reply.as_bytes()).await.is_err() {
        return;
    }
    let first = |from: usize, flag: &AtomicBool| start >= from && !flag.swap(true, Ordering::SeqCst);
    let decay = server.decay.filter(|decay| first(decay.from, &stats.decayed));
    let cut = server.cut.filter(|&(from, _)| first(from, &stats.cut)).map(|(_, after)| after);
    let own = server.per_connection.map(|rate| RateLimiter::new(rate as u64));
    let (mut sent, mut slowed, began) = (0, None, tokio::time::Instant::now());
    let (mut from_client, mut to_client) = socket.split();
    for offset in (start..=end).step_by(PIECE) {
        let mut piece = std::borrow::Cow::Borrowed(bytes_at(offset, end.min(offset + PIECE - 1) + 1 - offset));
        if let Some((from, to)) = server.corrupt.filter(|&(from, to)| from < offset + piece.len() && to > offset) {
            for at in from.max(offset)..to.min(offset + piece.len()) {
                piece.to_mut()[at - offset] ^= 0xff;
            }
        }
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
                    // Paced by the bytes sent in all, which a coarse timer cannot make faster: at
                    // `rate · 2^(4(t/T - 1))`, `B` bytes are out by `T · (1 + log2(4 ln2 · B / (rate · T) + 1/16) / 4)`.
                    let due = server.slow_start.zip(server.per_connection).and_then(|(within, rate)| {
                        let (t, b) = (within.as_secs_f64(), (sent + piece.len()) as f64);
                        let at = t * (1.0 + (4.0 * std::f64::consts::LN_2 * b / (rate * t) + 1.0 / 16.0).log2() / 4.0);
                        (at < t).then(|| began + Duration::from_secs_f64(at))
                    });
                    if let Some(due) = due {
                        tokio::time::sleep_until(due).await;
                    } else if let Some(own) = &own {
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
        stats.first_sent.get_or_init(std::time::Instant::now);
        sent += piece.len();
        stats.served.fetch_add(piece.len(), Ordering::SeqCst);
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

    /// Past its requests per window the server answers 429, and past its refusals it closes
    /// connections unanswered until the ban is over; requests within an asked rest are told apart.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_the_server_limits_how_fast_requests_come_and_shuts_out_who_keeps_asking() {
        let rate = RateLimit { requests: 2, window: Duration::from_secs(5), retry_after: 2, ban_after: 2, ban: Duration::from_millis(600) };
        let (url, stats) = serve(Server { size: 1000, rate: Some(rate), ..Default::default() }).await;
        let try_get = || async { tokio::time::timeout(Duration::from_secs(10), reqwest::Client::new().get(url.clone()).send()).await.unwrap() };
        for _ in 0..2 {
            assert_eq!(try_get().await.unwrap().status().as_u16(), 200);
        }
        let refused = try_get().await.unwrap();
        assert_eq!((refused.status().as_u16(), refused.headers()["retry-after"].to_str().unwrap()), (429, "2"));
        tokio::time::sleep(EARLY_SLACK).await;
        assert_eq!(try_get().await.unwrap().status().as_u16(), 429, "asked again within the rest");
        assert_eq!((stats.throttled.load(Ordering::SeqCst), stats.early.load(Ordering::SeqCst)), (2, 1));
        assert!(try_get().await.is_err(), "shut out");
        assert_eq!(stats.shut.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(try_get().await.unwrap().status().as_u16(), 200, "the ban is over and the window starts afresh");
    }

    /// A budget of 2 requests and one more every 400 ms: a third at once is refused with its
    /// Retry-After, one after 400 ms is served, and every request counts, HEAD too.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_the_server_keeps_a_budget_of_requests() {
        let budget = Budget { burst: 2, every: Duration::from_millis(400), retry_after: 3 };
        let (url, stats) = serve(Server { size: 1000, budget: Some(budget), ..Default::default() }).await;
        let client = reqwest::Client::new();
        let status = |request: reqwest::RequestBuilder| async move { tokio::time::timeout(Duration::from_secs(10), request.send()).await.unwrap().unwrap().status().as_u16() };
        assert_eq!(status(client.head(url.clone())).await, 200);
        assert_eq!(status(client.get(url.clone())).await, 200);
        let refused = tokio::time::timeout(Duration::from_secs(10), client.get(url.clone()).send()).await.unwrap().unwrap();
        assert_eq!((refused.status().as_u16(), refused.headers()["retry-after"].to_str().unwrap()), (429, "3"));
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert_eq!(status(client.get(url.clone())).await, 200, "one more after 400 ms");
        assert_eq!(status(client.get(url.clone())).await, 429, "and only one");
        assert_eq!((stats.asked.load(Ordering::SeqCst), stats.throttled.load(Ordering::SeqCst)), (5, 2));
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
