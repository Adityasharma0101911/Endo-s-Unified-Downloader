//! FTP downloads: a small client of its own over tokio, for `ftp://[user[:pass]@]host[:port]/path`
//! links (RFC 959, with EPSV from RFC 2428 and SIZE, MDTM and REST from RFC 3659). A link without
//! a login logs in as anonymous.
//!
//! An FTP download goes the way an HTTP one over ranges does (see `DownloadEngine::fetch_ranges`):
//! the same chunks, `.part` and resume state, progress, retries and cancellation, with [`work`] in
//! place of the HTTP worker. Each range is a connection of its own: log in, binary mode, passive
//! mode (EPSV, else PASV), `REST <offset>`, `RETR`, read the range, close. A server that takes no
//! more connections (421, or 530 "too many") lowers how many the download opens, as a 429 does
//! over HTTP. FTPS and proxies are not supported: such a download fails saying so.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use percent_encoding::percent_decode_str;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpSocket, TcpStream};
use url::{Host, Url};

use crate::chunk::{Chunk, StealRule, StealTiming};
use crate::engine::DownloadOptions;
use crate::hosts::{self, HostSlot};
use crate::worker::{FailureKind, WorkerEvent, WorkerShared};

/// The login of a link that has none.
const ANONYMOUS: (&str, &str) = ("anonymous", "anonymous@");
/// Longest reply line read as one; the rest of a longer one counts as the next line.
const MAX_LINE: u64 = 8 * 1024;
/// Most lines of one reply.
const MAX_LINES: usize = 256;
/// Tries of a probe while the server is busy or the connection fails.
const PROBE_ATTEMPTS: u32 = 3;
/// Most bytes one read from a data connection takes.
const READ_SIZE: usize = 64 * 1024;
/// Received bytes gathered before one disk write.
const WRITE_BATCH: usize = 512 * 1024;
/// Longest a received byte waits in memory, so progress and resume state keep up on slow links.
const WRITE_INTERVAL: Duration = Duration::from_millis(250);
/// How long a worker without work waits before looking again.
const IDLE_POLL: Duration = Duration::from_millis(50);
/// Silence after which an idle worker takes over what is left of a range, or four of its
/// mirror's answer times if longer.
const TAKEOVER_SILENCE: Duration = Duration::from_secs(2);
/// Pause of a range refused because the server takes no more connections than ours already open.
const THROTTLE_RETRY: Duration = Duration::from_millis(500);
/// Pause of a server that refused with none of ours open: it is just busy.
const THROTTLE_COOLDOWN: Duration = Duration::from_secs(1);

type Failure = (FailureKind, String);

/// Whether `url` is for this module: `ftp://`, or `ftps://` to refuse.
pub(crate) fn handles(url: &Url) -> bool {
    matches!(url.scheme(), "ftp" | "ftps")
}

/// How a download's FTP connections are made, with the logins its links carried. Never printed:
/// it holds passwords.
pub(crate) struct Net {
    connect_timeout: Duration,
    /// Longest wait for a reply.
    reply_timeout: Duration,
    /// Local addresses connections go out from, in turn (see `DownloadOptions::bind_addresses`
    /// and `ip_family`).
    locals: Vec<IpAddr>,
    /// User and password by server (`host:port`), taken out of the links.
    logins: Vec<(String, (String, String))>,
}

impl Net {
    /// The connections `options` ask for, and the download's ftp:// links without their
    /// `user:pass@`, which is kept here instead: no password reaches the resume state, the history
    /// or the log. Links of other schemes are left out.
    pub(crate) fn new(options: &DownloadOptions, urls: &[Url]) -> Result<(Self, Vec<Url>), String> {
        if urls.iter().any(|u| u.scheme() == "ftps") {
            return Err("FTPS (FTP over TLS) is not supported".to_string());
        }
        if options.proxy.is_some() || !options.proxy_pool.is_empty() {
            return Err("FTP downloads cannot go through a proxy: turn the proxy off for this download".to_string());
        }
        let (mut logins, mut links) = (Vec::new(), Vec::new());
        for url in urls {
            if url.scheme() != "ftp" {
                tracing::warn!("Leaving out {}: an FTP download takes only ftp:// mirrors", crate::history::redact_url(url.as_str()));
                continue;
            }
            let mut url = url.clone();
            if !url.username().is_empty() || url.password().is_some() {
                let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
                let login = (decode(url.username()), decode(url.password().unwrap_or_default()));
                let _ = url.set_password(None);
                let _ = url.set_username("");
                logins.push((server(&url), login));
            }
            if !links.contains(&url) {
                links.push(url);
            }
        }
        if links.is_empty() {
            return Err("No ftp:// link to download".to_string());
        }
        let locals = match options.bind_addresses.is_empty() {
            // `0.0.0.0` or `::` keeps the connections to that IP version.
            true => options.ip_family.local_address().into_iter().collect(),
            false => options.bind_addresses.clone(),
        };
        let net = Self {
            connect_timeout: Duration::from_secs(options.connect_timeout_secs.max(1)),
            reply_timeout: Duration::from_secs(options.stall_timeout_secs.max(1)),
            locals,
            logins,
        };
        Ok((net, links))
    }

    /// The size of the file at `url` (SIZE) and its date as an HTTP date (MDTM), each `None` when
    /// the server does not tell. A busy server or a failed connection is tried again.
    pub(crate) async fn probe(&self, url: &Url) -> Result<(Option<u64>, Option<String>), String> {
        let path = path_of(url)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.probe_once(url, &path).await {
                Ok(found) => return Ok(found),
                Err((FailureKind::Transient | FailureKind::Throttled(_), _)) if attempt < PROBE_ATTEMPTS => {
                    tokio::time::sleep(crate::chunk::backoff_delay(attempt)).await
                }
                Err((_, e)) => return Err(format!("{}: {}", url, e)),
            }
        }
    }

    async fn probe_once(&self, url: &Url, path: &str) -> Result<(Option<u64>, Option<String>), Failure> {
        let mut control = self.login(url, 0).await?;
        let size = match control.command(&format!("SIZE {path}")).await? {
            (213, text) => text.trim().parse().ok(),
            // No such file, or a folder.
            (550, text) => return Err(refused(550, &text)),
            _ => None,
        };
        let modified = match control.command(&format!("MDTM {path}")).await? {
            (213, text) => http_date(text.trim()),
            _ => None,
        };
        // Gone before the download's connections come, which the server may count against a limit.
        let _ = control.command("QUIT").await;
        Ok((size, modified))
    }

    /// A control connection to `url`'s server, logged in and in binary mode, for worker `worker`.
    async fn login(&self, url: &Url, worker: usize) -> Result<Control, Failure> {
        let mut control = self.open(url, worker).await?;
        // 120 means "ready in a moment"; 220 follows.
        loop {
            match control.reply().await? {
                (120, _) => continue,
                (220, _) => break,
                (code, text) => return Err(refused(code, &text)),
            }
        }
        let (user, pass) = self.logins.iter().find(|(at, _)| *at == server(url)).map_or(ANONYMOUS, |(_, (u, p))| (u, p));
        match control.command(&format!("USER {user}")).await? {
            (230, _) => {}
            (331, _) => match control.command(&format!("PASS {pass}")).await? {
                (230 | 202, _) => {}
                (code, text) => return Err(refused(code, &text)),
            },
            (code, text) => return Err(refused(code, &text)),
        }
        match control.command("TYPE I").await? {
            (200, _) => Ok(control),
            (code, text) => Err(refused(code, &text)),
        }
    }

    /// A TCP connection to `url`'s server, over an address the local addresses can reach.
    async fn open(&self, url: &Url, worker: usize) -> Result<Control, Failure> {
        let port = url.port_or_known_default().unwrap_or(21);
        let addrs: Vec<SocketAddr> = match url.host() {
            Some(Host::Ipv4(ip)) => vec![SocketAddr::new(ip.into(), port)],
            Some(Host::Ipv6(ip)) => vec![SocketAddr::new(ip.into(), port)],
            Some(Host::Domain(name)) => match tokio::time::timeout(self.connect_timeout, tokio::net::lookup_host((name, port))).await {
                Ok(Ok(found)) => found.collect(),
                Ok(Err(e)) => return Err(transient(format!("cannot resolve {name}: {e}"))),
                Err(_) => return Err(transient(format!("cannot resolve {name} in time"))),
            },
            None => return Err((FailureKind::Fatal, format!("{url} names no server"))),
        };
        let mut failed = None;
        for addr in addrs.into_iter().filter(|a| self.takes(a)) {
            match self.connect(addr, worker).await {
                Ok(stream) => {
                    let peer = stream.peer_addr().map_err(|e| transient(e.to_string()))?.ip();
                    return Ok(Control { stream: BufReader::new(stream), peer, timeout: self.reply_timeout });
                }
                Err(e) => failed = Some(e),
            }
        }
        Err(failed.unwrap_or_else(|| transient(format!("{} has no address the local addresses can reach", url.host_str().unwrap_or("")))))
    }

    /// Whether a connection may go to `addr`: of the IP version of one of the local addresses,
    /// when there are any.
    fn takes(&self, addr: &SocketAddr) -> bool {
        self.locals.is_empty() || self.locals.iter().any(|l| l.is_ipv4() == addr.is_ipv4())
    }

    /// A TCP connection to `addr`, from the local address that is worker `worker`'s turn.
    async fn connect(&self, addr: SocketAddr, worker: usize) -> Result<TcpStream, Failure> {
        let fail = |e: std::io::Error| transient(format!("cannot connect to {addr}: {e}"));
        let socket = if addr.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }.map_err(fail)?;
        let locals: Vec<&IpAddr> = self.locals.iter().filter(|l| l.is_ipv4() == addr.is_ipv4()).collect();
        if let Some(local) = locals.get(worker % locals.len().max(1)) {
            socket.bind(SocketAddr::new(**local, 0)).map_err(fail)?;
        }
        match tokio::time::timeout(self.connect_timeout, socket.connect(addr)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                Ok(stream)
            }
            Ok(Err(e)) => Err(fail(e)),
            Err(_) => Err(transient(format!("cannot connect to {addr} within {}s", self.connect_timeout.as_secs()))),
        }
    }
}

/// The control connection to an FTP server.
struct Control {
    stream: BufReader<TcpStream>,
    /// The server: data connections go there too, whatever address PASV names.
    peer: IpAddr,
    timeout: Duration,
}

impl Control {
    async fn reply(&mut self) -> Result<(u16, String), Failure> {
        match tokio::time::timeout(self.timeout, read_reply(&mut self.stream)).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(e)) => Err(transient(format!("FTP connection failed: {e}"))),
            Err(_) => Err(transient(format!("no answer from the FTP server within {}s", self.timeout.as_secs()))),
        }
    }

    /// Sends `line` and reads the reply. A line holding a line break is refused: it would be
    /// commands of its own (a path or password built to inject them).
    async fn command(&mut self, line: &str) -> Result<(u16, String), Failure> {
        if line.contains(['\r', '\n', '\0']) {
            return Err((FailureKind::Fatal, "an FTP command cannot hold a line break".to_string()));
        }
        let line = format!("{line}\r\n");
        match tokio::time::timeout(self.timeout, self.stream.get_mut().write_all(line.as_bytes())).await {
            Ok(Ok(())) => self.reply().await,
            _ => Err(transient("the connection to the FTP server broke".to_string())),
        }
    }

    /// Starts sending `path` from `offset` and returns the data connection it comes over.
    async fn retrieve(&mut self, net: &Net, worker: usize, path: &str, offset: u64) -> Result<TcpStream, Failure> {
        let port = match self.command("EPSV").await? {
            (229, text) => epsv_port(&text),
            _ => match self.command("PASV").await? {
                (227, text) => pasv_port(&text),
                (code, text) => return Err(refused(code, &text)),
            },
        };
        let port = port.ok_or_else(|| (FailureKind::BadMirror, "the FTP server's passive mode answer cannot be read".to_string()))?;
        let data = net.connect(SocketAddr::new(self.peer, port), worker).await?;
        if offset > 0 {
            let (code, text) = self.command(&format!("REST {offset}")).await?;
            if code != 350 {
                let (_, why) = refused(code, &text);
                return Err((FailureKind::BadMirror, format!("the FTP server cannot start a transfer midway: {why}")));
            }
        }
        match self.command(&format!("RETR {path}")).await? {
            (125 | 150, _) => Ok(data),
            (code, text) => Err(refused(code, &text)),
        }
    }
}

/// One reply: its code, and its text (the lines of a multi-line one joined by spaces).
async fn read_reply(stream: &mut (impl AsyncBufRead + Unpin)) -> std::io::Result<(u16, String)> {
    let invalid = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string());
    let (mut code, mut text) = (None, String::new());
    for _ in 0..MAX_LINES {
        let mut line = Vec::new();
        (&mut *stream).take(MAX_LINE).read_until(b'\n', &mut line).await?;
        if line.is_empty() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end_matches(['\r', '\n']);
        let this = line.get(..3).and_then(|c| c.parse::<u16>().ok()).filter(|c| (100..600).contains(c));
        let first = match code {
            Some(first) => first,
            None => *code.insert(this.ok_or_else(|| invalid("not an FTP reply"))?),
        };
        let part = if this == Some(first) { line.get(4..).unwrap_or("") } else { line };
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(part.trim());
        // The last line starts with the code and a space; a line of a multi-line reply with a dash.
        if this == Some(first) && line.as_bytes().get(3) != Some(&b'-') {
            return Ok((first, text));
        }
    }
    Err(invalid("FTP reply too long"))
}

/// What a reply other than the one hoped for means for the download.
fn refused(code: u16, text: &str) -> Failure {
    let text: String = text.chars().filter(|c| !c.is_control()).take(200).collect();
    let busy = text.to_ascii_lowercase().contains("too many") || text.to_ascii_lowercase().contains("maximum");
    let kind = match code {
        421 => FailureKind::Throttled(None),
        530 if busy => FailureKind::Throttled(None),
        530 | 532 | 550 | 553 => FailureKind::Denied,
        400..=499 => FailureKind::Transient,
        _ => FailureKind::BadMirror,
    };
    (kind, format!("FTP {code} {}", text.trim()))
}

fn transient(error: String) -> Failure {
    (FailureKind::Transient, error)
}

/// The port of an EPSV reply: `Entering Extended Passive Mode (|||6446|)`.
fn epsv_port(text: &str) -> Option<u16> {
    let inside = text.split_once('(')?.1.split_once(')')?.0;
    let delimiter = inside.chars().next()?;
    inside.split(delimiter).nth(3)?.parse().ok().filter(|&p| p > 0)
}

/// The port of a PASV reply: `Entering Passive Mode (h1,h2,h3,h4,p1,p2)`. Its address is not
/// used: the data connection goes to the control connection's server, never to another host a
/// server names (or a private address behind its NAT).
fn pasv_port(text: &str) -> Option<u16> {
    let numbers: Vec<u16> = text.split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse().ok()).collect();
    let [.., high, low] = numbers[..] else { return None };
    Some(high.checked_mul(256)?.checked_add(low)?).filter(|&p| p > 0 && high < 256 && low < 256)
}

/// The file's path on the server: the link's, percent-decoded, without its leading `/`, so
/// relative to where the login starts, as RFC 1738 has it (`%2F` makes it absolute).
fn path_of(url: &Url) -> Result<String, String> {
    let raw = url.path().strip_prefix('/').unwrap_or(url.path());
    let path = percent_decode_str(raw).decode_utf8().map_err(|_| format!("{url}: the path is not UTF-8"))?;
    if path.is_empty() || path.ends_with('/') {
        return Err(format!("{url} names a folder, not a file"));
    }
    if path.contains(['\r', '\n', '\0']) {
        return Err(format!("{url}: the path holds a line break"));
    }
    Ok(path.into_owned())
}

/// `host:port` of `url`, which logins are kept by.
fn server(url: &Url) -> String {
    format!("{}:{}", url.host_str().unwrap_or_default(), url.port_or_known_default().unwrap_or(21))
}

/// An MDTM time (`YYYYMMDDhhmmss`, UTC, maybe with a fraction) as an HTTP date, as Last-Modified
/// gives one.
fn http_date(mdtm: &str) -> Option<String> {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let digits = mdtm.split('.').next()?;
    if digits.len() != 14 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |at: usize, len: usize| digits[at..at + len].parse::<i64>().ok();
    let (year, month, day) = (n(0, 4)?, n(4, 2)?, n(6, 2)?);
    let (hour, minute, second) = (n(8, 2)?, n(10, 2)?, n(12, 2)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // 1970-01-01 was a Thursday.
    let weekday = DAYS[crate::engine::days_from_civil(year, month, day).rem_euclid(7) as usize];
    let month = MONTHS[month as usize - 1];
    Some(format!("{weekday}, {day:02} {month} {year:04} {hour:02}:{minute:02}:{second:02} GMT"))
}

/// One connection of an FTP download (see the module docs): takes ranges as an HTTP worker does
/// (fresh ones, the rest of a silent one, part of a slow one) and fetches each over a connection
/// of its own, until the download stops it or lowers its width below this worker.
pub(crate) async fn work(worker_id: usize, shared: WorkerShared, net: Arc<Net>) {
    let s = &shared;
    loop {
        if worker_id >= s.width.load(Ordering::Relaxed) {
            return;
        }
        let taken = tokio::select! {
            biased;
            _ = s.cancel.cancelled() => return,
            taken = take(s, worker_id) => taken,
        };
        let Some((chunk, mirror_id, url, slot)) = taken else {
            tokio::select! {
                _ = s.cancel.cancelled() => return,
                _ = tokio::time::sleep(IDLE_POLL) => continue,
            }
        };
        let outcome = fetch(s, &net, worker_id, &chunk, mirror_id, &url, &slot).await;
        if s.cancel.is_cancelled() {
            s.mirrors.lock().release_mirror(mirror_id);
            return;
        }
        // Settled before looking for more work, and reported so the engine notices the end or a
        // fatal error.
        let event = settle(s, worker_id, &chunk, mirror_id, outcome, &slot);
        drop(slot);
        if let Some(event) = event {
            if s.events.send(event).await.is_err() {
                return;
            }
        }
    }
}

/// A range for worker `worker_id` on the best mirror whose server has room (else the best one,
/// once it has), with the mirror, its link and the slot on its server; `None` while no mirror
/// takes another connection or nothing is left to take.
async fn take(s: &WorkerShared, worker_id: usize) -> Option<(Chunk, usize, Url, HostSlot)> {
    let ranked: Vec<(usize, Url)> = {
        let racer = s.mirrors.lock();
        racer.ranked().into_iter().filter_map(|id| Some((id, racer.get_mirror(id)?.url.clone()))).collect()
    };
    let free = ranked.iter().find_map(|(id, url)| Some((*id, url.clone(), hosts::try_acquire(url, s.host_limit)?)));
    let (mirror_id, url, slot) = match free {
        Some(free) => free,
        None => {
            let (id, url) = ranked.into_iter().next()?;
            let slot = hosts::acquire(&url, s.host_limit).await;
            (id, url, slot)
        }
    };
    let mut racer = s.mirrors.lock();
    // Meanwhile the mirror may have filled up or cooled down.
    racer.get_mirror(mirror_id).filter(|m| m.score(Instant::now()) >= 0.0)?;
    let chunk = {
        let mut chunks = s.chunks.lock();
        chunks
            .get_next_work(worker_id, mirror_id)
            .or_else(|| {
                let after = |m: usize| TAKEOVER_SILENCE.max(racer.get_mirror(m).map_or(Duration::ZERO, |m| m.ttfb() * 4));
                chunks.take_over_silent(worker_id, mirror_id, after)
            })
            .or_else(|| {
                let costs: Vec<StealTiming> =
                    racer.mirrors().iter().map(|m| StealTiming { startup: m.ttfb(), rate: m.speed_ewma }).collect();
                let steal = StealRule { min_bytes: s.min_steal, timing: Some(&costs) };
                chunks.steal_work(worker_id, mirror_id, steal).map(|(_, chunk)| chunk)
            })?
    };
    racer.acquire_mirror(mirror_id);
    Some((chunk, mirror_id, url, slot))
}

/// Fetches what is left of `chunk` from `url` over a connection of its own: logged in, the
/// transfer started at the chunk's offset, its bytes read up to the chunk's end. The connections
/// close once it returns; the server ends the transfer on its own.
async fn fetch(s: &WorkerShared, net: &Net, worker_id: usize, chunk: &Chunk, mirror_id: usize, url: &Url, slot: &HostSlot) -> Result<(), Failure> {
    let start = chunk.current_offset.load(Ordering::SeqCst);
    if start > chunk.end_offset.load(Ordering::SeqCst) {
        return Ok(()); // stolen away entirely before it started
    }
    let path = path_of(url).map_err(|e| (FailureKind::Fatal, e))?;
    let sent_at = Instant::now();
    let opening = async {
        let mut control = net.login(url, worker_id).await?;
        let data = control.retrieve(net, worker_id, &path, start).await?;
        Ok::<_, Failure>((control, data))
    };
    let (control, data) = tokio::select! {
        biased;
        _ = s.cancel.cancelled() => return Err(cancelled()),
        _ = chunk.revoked.cancelled() => return Err(taken_over()),
        opened = opening => opened?,
    };
    slot.accepted();
    // A new range costs all of that: steals are timed by it.
    let _ = s.events.try_send(WorkerEvent::Ttfb { worker_id, mirror_id, ttfb: sent_at.elapsed() });
    let received = receive(s, worker_id, chunk, mirror_id, data, start).await;
    drop(control);
    received
}

/// Writes what `data` brings, the chunk's bytes from `start`, into the file up to the chunk's end
/// (which a thief may lower meanwhile), in batches on the blocking pool. What arrived stays
/// written whatever ends the transfer: a retry starts after it. Each read waits `body_idle` at
/// most.
async fn receive(s: &WorkerShared, worker_id: usize, chunk: &Chunk, mirror_id: usize, mut data: TcpStream, start: u64) -> Result<(), Failure> {
    chunk.answered(Instant::now());
    let (mut pos, mut batch) = (start, Vec::with_capacity(WRITE_BATCH));
    let mut buf = vec![0u8; READ_SIZE];
    let flush_due = tokio::time::sleep(WRITE_INTERVAL);
    tokio::pin!(flush_due);
    let (mut pending, mut reported) = (0u64, Instant::now());
    let result = loop {
        // Only waiting for the server counts as the chunk's silence.
        chunk.wait_for_server(Instant::now());
        let read = tokio::select! {
            biased;
            _ = s.cancel.cancelled() => break Err(cancelled()),
            _ = chunk.revoked.cancelled() => break Err(taken_over()),
            _ = &mut flush_due, if !batch.is_empty() => {
                if let Err(e) = flush(s, chunk, &mut pos, &mut batch).await {
                    break Err(e);
                }
                if pos > chunk.end_offset.load(Ordering::SeqCst) {
                    break Ok(());
                }
                continue;
            }
            read = tokio::time::timeout(s.body_idle, data.read(&mut buf)) => read,
        };
        let at = pos + batch.len() as u64;
        let n = match read {
            Ok(Ok(0)) => break Ok(()),
            Ok(Ok(n)) => n,
            Ok(Err(e)) => break Err(transient(format!("read error at offset {at}: {e}"))),
            Err(_) => break Err(transient(format!("stalled: no data for {:?} at offset {at}", s.body_idle))),
        };
        chunk.busy();
        if let Some(limiter) = &s.limiter {
            tokio::select! {
                biased;
                _ = s.cancel.cancelled() => break Err(cancelled()),
                _ = chunk.revoked.cancelled() => break Err(taken_over()),
                _ = limiter.acquire(n as u64) => {}
            }
        }
        // Re-read the end: a thief may have taken the tail of this chunk.
        let end = chunk.end_offset.load(Ordering::SeqCst);
        if at > end {
            break Ok(());
        }
        let keep = (n as u64).min(end - at + 1) as usize;
        if batch.is_empty() {
            flush_due.as_mut().reset(tokio::time::Instant::now() + WRITE_INTERVAL);
        }
        batch.extend_from_slice(&buf[..keep]);
        pending += keep as u64;
        if pending >= 1024 * 1024 || reported.elapsed() >= Duration::from_millis(100) {
            report(s, worker_id, chunk.id, mirror_id, &mut pending, &mut reported);
        }
        if batch.len() >= WRITE_BATCH {
            if let Err(e) = flush(s, chunk, &mut pos, &mut batch).await {
                break Err(e);
            }
        }
        if keep < n {
            break Ok(());
        }
    };
    // A disk error outranks the network outcome.
    let flushed = flush(s, chunk, &mut pos, &mut batch).await;
    report(s, worker_id, chunk.id, mirror_id, &mut pending, &mut reported);
    flushed.and(result)?;
    let end = chunk.end_offset.load(Ordering::SeqCst);
    if pos <= end {
        return Err(transient(format!("connection closed at offset {pos}, expected data up to {end}")));
    }
    Ok(())
}

/// Writes `batch`, the bytes from `pos`, up to the chunk's end, then moves `pos` and the chunk's
/// offset past them. Nothing is written once the chunk was taken over: it is another worker's.
async fn flush(s: &WorkerShared, chunk: &Chunk, pos: &mut u64, batch: &mut Vec<u8>) -> Result<(), Failure> {
    chunk.busy();
    let end = chunk.end_offset.load(Ordering::SeqCst);
    batch.truncate((end + 1).saturating_sub(*pos).min(batch.len() as u64) as usize);
    if chunk.revoked.is_cancelled() || batch.is_empty() {
        batch.clear();
        return Ok(());
    }
    let (writer, at, bytes) = (s.writer.clone(), *pos, std::mem::take(batch));
    let (mut bytes, written) = tokio::task::spawn_blocking(move || {
        let written = writer.write_chunk_slice(at, &bytes);
        (bytes, written)
    })
    .await
    .map_err(|e| (FailureKind::Fatal, format!("Disk write task failed: {e}")))?;
    written.map_err(|e| (FailureKind::Fatal, format!("Disk write error: {e}")))?;
    *pos += bytes.len() as u64;
    chunk.current_offset.store(*pos, Ordering::SeqCst);
    bytes.clear();
    *batch = bytes;
    Ok(())
}

/// Mirror speed for the engine; dropped rather than holding up the data when it is busy.
fn report(s: &WorkerShared, worker_id: usize, chunk_id: usize, mirror_id: usize, pending: &mut u64, reported: &mut Instant) {
    if *pending > 0 {
        let bytes_received = std::mem::take(pending);
        let event = WorkerEvent::Progress { worker_id, chunk_id, mirror_id, bytes_received, duration: reported.elapsed() };
        let _ = s.events.try_send(event);
    }
    *reported = Instant::now();
}

/// Applies an attempt's outcome to chunk, mirror and server bookkeeping, as the HTTP worker does,
/// and returns the event to send. A server taking no more connections while others of ours are
/// open holds the mirror and its server to those, and costs the range no retry; refusing with
/// none open, it is just busy, and cools down. A chunk another worker took over is no longer this
/// worker's to settle.
fn settle(s: &WorkerShared, worker_id: usize, chunk: &Chunk, mirror_id: usize, outcome: Result<(), Failure>, slot: &HostSlot) -> Option<WorkerEvent> {
    let mut racer = s.mirrors.lock();
    racer.release_mirror(mirror_id);
    let mut chunks = s.chunks.lock();
    let (chunk_id, revoked) = (chunk.id, chunk.revoked.is_cancelled());
    let (kind, error) = match outcome {
        Ok(()) if revoked => return None,
        Ok(()) => {
            if let Err(e) = chunks.mark_completed(chunk_id) {
                chunks.abort(chunk_id, &e.to_string());
            }
            return Some(WorkerEvent::ChunkCompleted { worker_id, chunk_id });
        }
        Err(failure) => failure,
    };
    let failed = Some(WorkerEvent::ChunkFailed { worker_id, chunk_id, mirror_id, kind, error: error.clone() });
    let Some(mirror) = racer.get_mirror_mut(mirror_id).filter(|_| kind != FailureKind::Fatal) else {
        chunks.abort(chunk_id, &error);
        return failed;
    };
    if revoked {
        mirror.record_failure();
        return failed;
    }
    let (delay, counts) = match kind {
        FailureKind::Throttled(_) => {
            slot.throttled();
            let others = mirror.in_flight;
            mirror.record_throttled(others);
            if others > 0 {
                (THROTTLE_RETRY, false)
            } else {
                mirror.cool_down(Instant::now() + THROTTLE_COOLDOWN);
                (THROTTLE_COOLDOWN, true)
            }
        }
        FailureKind::BadMirror | FailureKind::Denied => {
            if mirror.record_bad_response() {
                tracing::warn!("Disabling mirror {}: {}", mirror.url, error);
            }
            if racer.all_inactive() {
                chunks.abort(chunk_id, &format!("every mirror failed; last error: {error}"));
                return failed;
            }
            (Duration::ZERO, false)
        }
        _ => {
            mirror.record_failure();
            (Duration::ZERO, true)
        }
    };
    if let Err(e) = chunks.mark_failed(chunk_id, &error, delay, counts) {
        chunks.abort(chunk_id, &e.to_string());
    }
    failed
}

fn cancelled() -> Failure {
    transient("cancelled".to_string())
}

fn taken_over() -> Failure {
    transient("no data for too long; another connection took over".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{DownloadEngine, CANCELLED};
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// What the test server serves, and how.
    #[derive(Clone)]
    struct Serve {
        file: Arc<Vec<u8>>,
        /// The login it takes.
        login: (String, String),
        /// Most sessions at once; one more is refused with 421 (0 for no limit).
        max: usize,
        /// Bytes a data connection carries before it is cut (0 for no cut).
        cut_after: usize,
        /// Pause after each 16 KiB written.
        pace: Duration,
        /// Answers EPSV with 502, so PASV is asked.
        no_epsv: bool,
    }

    impl Serve {
        fn file(file: Vec<u8>) -> Self {
            let login = (ANONYMOUS.0.to_string(), ANONYMOUS.1.to_string());
            Self { file: Arc::new(file), login, max: 0, cut_after: 0, pace: Duration::ZERO, no_epsv: false }
        }
    }

    /// What the test server saw.
    #[derive(Default)]
    struct Seen {
        commands: Mutex<Vec<String>>,
        open: AtomicUsize,
        peak: AtomicUsize,
        refused: AtomicUsize,
    }

    impl Seen {
        fn commands(&self) -> Vec<String> {
            self.commands.lock().clone()
        }

        /// The offsets of the `REST` commands seen.
        fn rests(&self) -> Vec<u64> {
            self.commands().iter().filter_map(|c| c.strip_prefix("REST ")?.parse().ok()).collect()
        }
    }

    /// An FTP server on 127.0.0.1 serving `serve.file` under any path, and the link to it.
    async fn ftp_server(serve: Serve, path: &str) -> (Url, Arc<Seen>) {
        let listener = crate::hosts::unseen_listener().await;
        let url = Url::parse(&format!("ftp://127.0.0.1:{}/{path}", listener.local_addr().unwrap().port())).unwrap();
        let seen = Arc::new(Seen::default());
        let watching = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(session(serve.clone(), Arc::clone(&watching), socket));
            }
        });
        (url, seen)
    }

    async fn session(serve: Serve, seen: Arc<Seen>, socket: TcpStream) {
        let (read, mut write) = socket.into_split();
        if serve.max > 0 && seen.open.load(Ordering::SeqCst) >= serve.max {
            seen.refused.fetch_add(1, Ordering::SeqCst);
            let _ = write.write_all(b"421 Too many connections from your address\r\n").await;
            return;
        }
        let open = seen.open.fetch_add(1, Ordering::SeqCst) + 1;
        seen.peak.fetch_max(open, Ordering::SeqCst);
        struct Close(Arc<Seen>);
        impl Drop for Close {
            fn drop(&mut self) {
                self.0.open.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _close = Close(Arc::clone(&seen));
        let mut lines = BufReader::new(read).lines();
        let _ = write.write_all(b"220-Test server\r\n220-for the FTP tests\r\n220 Ready\r\n").await;
        let (mut user, mut rest, mut passive) = (String::new(), 0usize, None::<TcpListener>);
        while let Ok(Some(line)) = lines.next_line().await {
            seen.commands.lock().push(line.clone());
            let (command, arg) = line.split_once(' ').unwrap_or((&line, ""));
            let reply = match command {
                "USER" => {
                    user = arg.to_string();
                    "331 Password, please".to_string()
                }
                "PASS" if (user.as_str(), arg) == (serve.login.0.as_str(), serve.login.1.as_str()) => "230 Logged in".to_string(),
                "PASS" => "530 Login incorrect".to_string(),
                "TYPE" => "200 Binary it is".to_string(),
                "SIZE" => format!("213 {}", serve.file.len()),
                "MDTM" => "213 20240229123456".to_string(),
                "EPSV" if serve.no_epsv => "502 Not here".to_string(),
                "EPSV" | "PASV" => {
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let port = listener.local_addr().unwrap().port();
                    passive = Some(listener);
                    match command {
                        "EPSV" => format!("229 Entering Extended Passive Mode (|||{port}|)"),
                        // An address the client must not use: only the port counts.
                        _ => format!("227 Entering Passive Mode (10,9,8,7,{},{})", port / 256, port % 256),
                    }
                }
                "REST" => {
                    rest = arg.parse().unwrap();
                    "350 Restarting there".to_string()
                }
                "RETR" => {
                    let Some(listener) = passive.take() else { return };
                    let _ = write.write_all(b"150 Here it comes\r\n").await;
                    let Ok((mut data, _)) = listener.accept().await else { return };
                    let mut body = &serve.file[rest.min(serve.file.len())..];
                    if serve.cut_after > 0 {
                        body = &body[..serve.cut_after.min(body.len())];
                    }
                    let mut sent = true;
                    for piece in body.chunks(16 * 1024) {
                        if data.write_all(piece).await.is_err() {
                            sent = false;
                            break;
                        }
                        tokio::time::sleep(serve.pace).await;
                    }
                    drop(data);
                    rest = 0;
                    if sent { "226 Done" } else { "426 Transfer aborted" }.to_string()
                }
                "QUIT" => {
                    let _ = write.write_all(b"221 Bye\r\n").await;
                    return;
                }
                _ => "502 Not implemented".to_string(),
            };
            if write.write_all(format!("{reply}\r\n").as_bytes()).await.is_err() {
                return;
            }
        }
    }

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn options_into(dir: &std::path::Path, connections: usize) -> DownloadOptions {
        DownloadOptions { output_path: Some(dir.join("")), num_connections: connections, ..Default::default() }
    }

    /// Connections open within `connect_timeout_secs`, from `0.0.0.0` or `::` for one IP version,
    /// unless local addresses are given.
    #[test]
    fn connections_take_the_ip_version_and_connect_timeout() {
        let url = Url::parse("ftp://h.example/f.bin").unwrap();
        let v6 = DownloadOptions { ip_family: crate::engine::IpFamily::V6, connect_timeout_secs: 3, ..Default::default() };
        let (net, _) = Net::new(&v6, std::slice::from_ref(&url)).unwrap();
        assert_eq!((net.locals, net.connect_timeout), (vec![IpAddr::from(std::net::Ipv6Addr::UNSPECIFIED)], Duration::from_secs(3)));
        let bound = DownloadOptions { bind_addresses: vec![IpAddr::from([10, 0, 0, 1])], ..v6 };
        assert_eq!(Net::new(&bound, &[url]).unwrap().0.locals, bound.bind_addresses);
    }

    /// Anonymous, over several connections, each starting where REST says, each reading its range;
    /// the file dated as MDTM says with `remote_time`. A file over `max_filesize` fails at once.
    #[tokio::test]
    async fn downloads_over_several_connections_each_from_its_offset() {
        let file = bytes(3 * 1024 * 1024 + 123);
        let (url, seen) = ftp_server(Serve::file(file.clone()), "pub/files/data%20set.bin").await;
        let dir = tempfile::tempdir().unwrap();
        let limited = DownloadOptions { max_filesize: Some(1024 * 1024), ..options_into(dir.path(), 4) };
        let error = DownloadEngine::new(vec![url.clone()], limited).run(None).await.unwrap_err();
        assert_eq!(error, "File is larger than the limit of 1.0 MiB (3.0 MiB)");
        let options = DownloadOptions { remote_time: true, ..options_into(dir.path(), 4) };
        let saved = DownloadEngine::new(vec![url], options).run(None).await.unwrap();
        assert_eq!(saved.file_name().unwrap(), "data set.bin");
        assert_eq!(std::fs::read(&saved).unwrap(), file);
        let modified = std::fs::metadata(&saved).unwrap().modified().unwrap();
        assert_eq!(modified, std::time::UNIX_EPOCH + Duration::from_secs(1_709_210_096), "2024-02-29 12:34:56 UTC");
        let commands = seen.commands();
        assert!(commands.contains(&"USER anonymous".to_string()), "{commands:?}");
        assert!(commands.contains(&"RETR pub/files/data set.bin".to_string()), "{commands:?}");
        assert!(seen.rests().len() >= 2, "{commands:?}");
        assert!(seen.peak.load(Ordering::SeqCst) >= 2, "one connection at a time");
    }

    /// A link's login is used, PASV's address ignored, and the password stays out of the history.
    #[tokio::test]
    async fn logs_in_as_the_link_says_and_keeps_the_password_off_disk() {
        let file = bytes(200_000);
        let serve = Serve { login: ("alice".into(), "p@ss word".into()), no_epsv: true, ..Serve::file(file.clone()) };
        let (url, seen) = ftp_server(serve, "f.bin").await;
        let with_login = Url::parse(&url.as_str().replace("ftp://", "ftp://alice:p%40ss%20word@")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let saved = DownloadEngine::new(vec![with_login], options_into(dir.path(), 2)).run(None).await.unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), file);
        let commands = seen.commands();
        assert!(commands.contains(&"PASS p@ss word".to_string()) && commands.contains(&"PASV".to_string()), "{commands:?}");
        let history = std::fs::read_to_string(crate::history::DownloadHistoryManager::default_history_path()).unwrap();
        assert!(history.contains(url.as_str()) && !history.contains("p%40ss") && !history.contains("alice"), "{history}");
    }

    /// A transfer cut short goes on from where it stopped, with REST, over a new connection.
    #[tokio::test]
    async fn a_cut_transfer_resumes_where_it_stopped() {
        let file = bytes(1024 * 1024);
        let (url, seen) = ftp_server(Serve { cut_after: 300 * 1024, ..Serve::file(file.clone()) }, "f.bin").await;
        let dir = tempfile::tempdir().unwrap();
        let saved = DownloadEngine::new(vec![url], options_into(dir.path(), 1)).run(None).await.unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), file);
        assert_eq!(seen.rests(), [300 * 1024, 600 * 1024, 900 * 1024]);
    }

    /// A server that takes two connections at once gets no more for long, and the download
    /// finishes over those.
    #[tokio::test]
    async fn a_connection_limit_lowers_how_many_are_opened() {
        // Large enough for six connections to start.
        let file = bytes(6 * 1024 * 1024);
        let serve = Serve { max: 2, pace: Duration::from_millis(2), ..Serve::file(file.clone()) };
        let (url, seen) = ftp_server(serve, "f.bin").await;
        let dir = tempfile::tempdir().unwrap();
        let saved = DownloadEngine::new(vec![url], options_into(dir.path(), 6)).run(None).await.unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), file);
        assert!(seen.refused.load(Ordering::SeqCst) >= 1);
        assert!(seen.peak.load(Ordering::SeqCst) <= 2);
        let logins = seen.commands().iter().filter(|c| c.starts_with("USER")).count();
        assert!(logins < 12, "{logins} logins: the width did not come down");
    }

    /// A cancelled download keeps its `.part` and state, and the next run asks only for the rest.
    #[tokio::test]
    async fn a_cancelled_download_resumes_from_its_state() {
        let file = bytes(2 * 1024 * 1024);
        let (url, seen) = ftp_server(Serve { pace: Duration::from_millis(5), ..Serve::file(file.clone()) }, "f.bin").await;
        let dir = tempfile::tempdir().unwrap();
        let engine = DownloadEngine::new(vec![url.clone()], options_into(dir.path(), 1));
        let (tx, mut rx) = tokio::sync::broadcast::channel(64);
        let running = tokio::spawn({
            let engine = engine.clone();
            async move { engine.run(Some(tx)).await }
        });
        while let Ok(snapshot) = rx.recv().await {
            if snapshot.downloaded_bytes >= 256 * 1024 {
                engine.cancel();
                break;
            }
        }
        assert_eq!(running.await.unwrap().unwrap_err(), CANCELLED);
        assert!(dir.path().join("f.bin.part.hfstate").exists());
        seen.commands.lock().clear();

        let saved = DownloadEngine::new(vec![url], options_into(dir.path(), 1)).run(None).await.unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), file);
        let (rests, retrs) = (seen.rests(), seen.commands().iter().filter(|c| c.starts_with("RETR")).count());
        assert_eq!(rests.len(), retrs, "a RETR from the start: {:?}", seen.commands());
        assert!(rests[0] >= 256 * 1024, "{rests:?}");
    }

    #[tokio::test]
    async fn ftps_and_proxies_are_refused() {
        let ftps = Url::parse("ftps://127.0.0.1/f.bin").unwrap();
        let error = DownloadEngine::new(vec![ftps], DownloadOptions::default()).run(None).await.unwrap_err();
        assert!(error.contains("FTPS"), "{error}");
        let ftp = Url::parse("ftp://127.0.0.1/f.bin").unwrap();
        let options = DownloadOptions { proxy: Some("http://127.0.0.1:9".into()), ..Default::default() };
        let error = DownloadEngine::new(vec![ftp], options).run(None).await.unwrap_err();
        assert!(error.contains("proxy"), "{error}");
    }

    #[tokio::test]
    async fn replies_are_read_whole() {
        let mut one: &[u8] = b"213 1234\r\n";
        assert_eq!(read_reply(&mut one).await.unwrap(), (213, "1234".to_string()));
        let mut many: &[u8] = b"211-Features:\r\n EPSV\r\n211-MDTM\r\n211 End\r\n230 next\r\n";
        assert_eq!(read_reply(&mut many).await.unwrap(), (211, "Features: EPSV MDTM End".to_string()));
        assert_eq!(read_reply(&mut many).await.unwrap().0, 230, "the next reply is left");
        assert!(read_reply(&mut &b"hello\r\n"[..]).await.is_err());
        assert!(read_reply(&mut &b"220-never ends\r\n"[..]).await.is_err());
    }

    #[test]
    fn passive_replies_and_dates_are_read() {
        assert_eq!(epsv_port("Entering Extended Passive Mode (|||6446|)"), Some(6446));
        assert_eq!(epsv_port("Entering Extended Passive Mode (!!!21!)"), Some(21));
        assert_eq!(epsv_port("no port"), None);
        assert_eq!(pasv_port("Entering Passive Mode (192,168,1,2,19,137)."), Some(19 * 256 + 137));
        assert_eq!(pasv_port("Entering Passive Mode 10,0,0,1,4,1"), Some(1025));
        assert_eq!(pasv_port("Entering Passive Mode (1,2,3,4,999,1)"), None);
        assert_eq!(http_date("19941106084937").as_deref(), Some("Sun, 06 Nov 1994 08:49:37 GMT"));
        assert_eq!(http_date("20240229123456.789").as_deref(), Some("Thu, 29 Feb 2024 12:34:56 GMT"));
        assert_eq!(http_date("2024022912345"), None);
        assert_eq!(http_date("20241329123456"), None);
    }

    #[test]
    fn paths_are_checked_and_logins_taken_out_of_links() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(path_of(&url("ftp://h/pub/a%20b.iso")).unwrap(), "pub/a b.iso");
        assert_eq!(path_of(&url("ftp://h/%2Fetc/f")).unwrap(), "/etc/f");
        assert!(path_of(&url("ftp://h/pub/")).is_err());
        assert!(path_of(&url("ftp://h/f%0D%0ADELE%20x")).is_err());
        let links = [url("ftp://bob:se%3Acret@h:2121/f"), url("https://h/f"), url("ftp://h/f")];
        let (net, clean) = Net::new(&DownloadOptions::default(), &links).unwrap();
        assert_eq!(clean, [url("ftp://h:2121/f"), url("ftp://h/f")]);
        assert_eq!(net.logins, [("h:2121".to_string(), ("bob".to_string(), "se:cret".to_string()))]);
        assert!(refused(530, "Sorry, the maximum number of clients (3) are connected").0 == FailureKind::Throttled(None));
        assert_eq!(refused(530, "Login incorrect").0, FailureKind::Denied);
        assert_eq!(refused(421, "Too many").0, FailureKind::Throttled(None));
    }
}
