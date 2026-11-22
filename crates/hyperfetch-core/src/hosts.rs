//! What this process knows about the hosts it downloads from, shared by every download: how many
//! requests each has open, within a budget every open request holds a slot of, and what each host
//! was seen to do, so the next download from it can start from that instead of finding out again.
//!
//! Hosts are told apart by scheme, host and port. A fact is trusted for `PROFILE_TTL` after it was
//! learned, each on its own: hosts change, and a fact nobody sees again is forgotten.
//!
//! A host takes a new request while fewer are open than the smallest nonzero limit among that
//! request's and those of the requests already open or waiting for a slot (downloads with
//! differing `max_connections_per_host` share one host at the strictest, from the moment the
//! strict one asks), and than the connection cap the host was seen to enforce. A limit of 0 sets
//! none.
//!
//! A host that refused a request with 429/503 takes none for the rest it asked for (its
//! Retry-After), whichever download or probe would send it; nor while it leaves requests
//! unanswered just after (see [`HostSlot::unanswered`]). From then on, until it answers again,
//! requests go out one at a time, and one more at once with each answer, as a new download's do.
//!
//! Some facts are also kept across restarts once a front end names a file for them (see
//! [`keep_in`]): the connections a download from the host ended up fastest with, the connection
//! cap it was seen to enforce, the speed it held a download to whatever its connections, and what
//! it speaks besides HTTP/1.1 (see [`protocol`]). They
//! are kept for `KEPT_FOR` after they were last learned, by host only (`https://cdn.example:443`,
//! never a path or query). A kept cap holds the host's first requests in a run as one learned then
//! would. So is the user's line speed (see [`line`]), under a name no host has.
//!
//! A host a resolver says asks to be asked slowly (see [`set_gap`]) gets every request, a
//! download's or a listing's, a probe's or a chunk's, no sooner than its gap after the one before,
//! through `crate::polite`'s pacing.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use url::Url;

use crate::engine::{POOL_IDLE, POOL_MAX_IDLE};

/// How long a learned fact about a host is trusted.
pub const PROFILE_TTL: Duration = Duration::from_secs(10 * 60);
/// Least time between the refusal a connection cap was learned from (or its last rise) and the
/// next rise: a host that means its cap refuses every request offered past it.
const CAP_PROBE_INTERVAL: Duration = Duration::from_secs(60);
/// Hosts remembered before those with nothing fresh to tell are dropped.
const KEEP_HOSTS: usize = 256;
/// How long what is kept of a host across restarts lasts after it was last learned.
const KEPT_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Hosts kept across restarts: those learned about last.
const KEEP_KEPT: usize = 300;
/// What the line speed is kept under: no host's name, as those have a scheme.
const LINE: &str = "line";
/// How long after a host refused a request (429/503) a request it leaves unanswered (it refuses or
/// drops connections) is taken for a ban to wait out rather than a failure: firewalls shut out an
/// address that kept asking for minutes. Past it, a host that does not answer is dead.
pub const BANNED_FOR: Duration = Duration::from_secs(10 * 60);
/// The rest of a host after the first request it left unanswered within `BANNED_FOR`; twice as
/// long after each further one in a row, up to `MAX_BAN_REST`.
const BAN_REST: Duration = Duration::from_secs(5);
const MAX_BAN_REST: Duration = Duration::from_secs(60);
/// A download within this share of the fastest one went as fast as the line lets it.
const LINE_BAND: f64 = 0.1;

/// Maximum chunk size allowed for this host, if the host throttles large ranges.
/// For example, YouTube (`googlevideo.com`) severely throttles HTTP Range requests larger than 10 MiB (10,485,760 bytes).
pub const YOUTUBE_MAX_CHUNK_SIZE: u64 = 10 * 1024 * 1024;

static HOSTS: LazyLock<Hosts> = LazyLock::new(Hosts::default);
/// Least time between the starts of two requests to a host (by [`HostKey::origin`]), as
/// resolvers set it (see [`set_gap`]).
static GAPS: LazyLock<Mutex<HashMap<String, Duration>>> = LazyLock::new(Default::default);

/// A host as this module tells hosts apart: scheme, host, port, and optional proxy route.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostKey {
    scheme: String,
    host: String,
    port: Option<u16>,
    route_id: usize,
}

impl HostKey {
    pub fn of(url: &Url) -> Self {
        Self::routed(url, 0)
    }

    pub fn routed(url: &Url, route_id: usize) -> Self {
        Self {
            scheme: url.scheme().to_string(),
            host: url.host_str().unwrap_or_default().to_string(),
            port: url.port_or_known_default(),
            route_id,
        }
    }

    /// What it is kept under across restarts: scheme, host and port; None for a route's own
    /// (a proxy's, a network's), which a later run may number otherwise.
    fn origin(&self) -> Option<String> {
        let port = self.port.map_or_else(String::new, |port| format!(":{port}"));
        (self.route_id == 0).then(|| format!("{}://{}{}", self.scheme, self.host, port))
    }

    /// Default connection cap for hosts known to aggressively throttle or sinkhole concurrent connections.
    pub fn default_connection_cap(&self) -> Option<usize> {
        let host = self.host.to_ascii_lowercase();
        if host.contains("cloudatacdn") || host.contains("dood") || host.contains("ds2play") || host.contains("doodstream") {
            Some(3)
        } else if host.contains("pixeldra") {
            // Answers 403 to a fifth or sixth connection from one address.
            Some(4)
        } else if host == "archive.org" || host.ends_with(".archive.org") {
            // Internet Archive rate limits aggressively and returns HTTP 429 when concurrency exceeds 4-5.
            Some(4)
        } else if host == "export.arxiv.org" {
            // arXiv asks programs to use one connection to it, besides one request every 3 s
            // (see `crate::archives`, which sets that gap).
            Some(1)
        } else {
            None
        }
    }

    /// Whether the host is known to enforce streaming bitrate shaping after an initial burst window.
    pub fn is_bitrate_throttled(&self) -> bool {
        let host = self.host.to_ascii_lowercase();
        host.contains("cloudatacdn") || host.contains("dood") || host.contains("ds2play") || host.contains("doodstream") || host.contains("streamtape")
    }

    /// Whether the host is YouTube / GoogleVideo CDN.
    pub fn is_googlevideo(&self) -> bool {
        let host = self.host.to_ascii_lowercase();
        host.ends_with(".googlevideo.com") || host == "googlevideo.com" || host.contains("googlevideo")
    }

    /// The maximum chunk size recommended for this host.
    pub fn max_chunk_size(&self) -> Option<u64> {
        if self.is_googlevideo() {
            Some(YOUTUBE_MAX_CHUNK_SIZE)
        } else {
            None
        }
    }
}

/// What a host was seen to do. Each fact is `None` when it is unknown or older than `PROFILE_TTL`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostProfile {
    /// Whether it honours ranged GETs.
    pub accepts_ranges: Option<bool>,
    /// Whether it caps each connection's speed, so that more connections fetch faster.
    pub capped_per_connection: Option<bool>,
    /// Bytes per second one connection reached.
    pub connection_rate: Option<f64>,
    /// How long a new connection took to its first answer.
    pub setup_time: Option<Duration>,
    /// Connections it serves at once before refusing more with 429/503; at least 1 (a 0 recorded
    /// is ignored: every host serves one request, or it could never be downloaded from).
    pub connection_cap: Option<usize>,
}

/// What is known about `url`'s host.
pub fn profile(url: &Url) -> HostProfile {
    HOSTS.profile(&HostKey::of(url), Instant::now())
}

/// The maximum chunk size recommended for `url`'s host.
pub fn max_chunk_size(url: &Url) -> Option<u64> {
    HostKey::of(url).max_chunk_size()
}

/// Whether `url`'s host refused a request (429/503) within `BANNED_FOR`, on any route: it limits
/// how often it is asked, so it is asked only what is needed.
pub fn refused_lately(url: &Url) -> bool {
    HOSTS.refused_lately(&HostKey::of(url), clock())
}

/// The hosts that refused a request (429/503) within `BANNED_FOR`: a download takes them before
/// it asks anything, to tell whether the host that refuses it, the one a redirect led to, had
/// refused another download (see [`refused_in_download`], [`among`]).
pub(crate) fn refusing() -> Vec<HostKey> {
    let now = clock();
    let entries = HOSTS.entries.lock();
    let refused = entries.iter().filter(|(_, entry)| entry.refused.is_some_and(|(at, _)| now.saturating_duration_since(at) < BANNED_FOR));
    refused.map(|(key, _)| key.clone()).collect()
}

/// Whether `url`'s host, on any route, is among `hosts`.
pub(crate) fn among(hosts: &[HostKey], url: &Url) -> bool {
    let key = HostKey::of(url);
    hosts.iter().any(|other| (&other.scheme, &other.host, other.port) == (&key.scheme, &key.host, key.port))
}

/// How long `url`'s host, on any route, still rests after refusing a request: zero when it does not.
pub(crate) fn rest_left(url: &Url) -> Duration {
    HOSTS.rest_left(&HostKey::of(url), clock())
}

/// Whether `url`'s host limits how often it is asked, so a download from it asks only what it
/// needs (no HEAD beside its GET, no piece split for another request), and its checksum files
/// after its last byte, one at a time: it refused a request (429/503) within `BANNED_FOR` in this
/// run, or it is kept as limiting requests (see [`kept_limited`]).
pub fn limits_requests(url: &Url) -> bool {
    refused_lately(url) || kept_limited(url)
}

/// Whether `url`'s host is kept as limiting how often it is asked, in this run or an earlier one,
/// so a download from it asks for the file alone, over one connection: a download from it was
/// refused after another had been, once the host's rest was over (see [`refused_in_download`]).
/// For an hour the first time, twice as long each time a wider try is refused again, up to a day.
pub(crate) fn kept_limited(url: &Url) -> bool {
    let now = unix_now();
    HOSTS.kept(&HostKey::of(url), now).and_then(|kept| kept.limited).is_some_and(|(until, _)| now < until)
}

/// A download from `url`'s host was refused (429/503). When `again` (the host refused another
/// download within `BANNED_FOR` before this one began), or the host was kept as limiting requests
/// and that time is up, it is kept so (see [`kept_limited`]): an hour, or twice as long as the last
/// time, up to a day. One refusal alone keeps nothing past this run's memory of it.
pub(crate) fn refused_in_download(url: &Url, again: bool) {
    let now = unix_now();
    let key = HostKey::of(url);
    let last = HOSTS.kept(&key, now).and_then(|kept| kept.limited);
    if last.is_some_and(|(until, _)| now < until) || !(again || last.is_some()) {
        return;
    }
    let hours = last.map_or(LIMITED_HOURS, |(_, hours)| hours.saturating_mul(2).min(MOST_LIMITED_HOURS));
    HOSTS.keep(&key, now, |kept| kept.limited = Some((now + hours * 3600, hours)))
}

/// A download from `url`'s host went over more than one connection and was refused nothing: a
/// host kept as limiting requests whose time is up is no longer.
pub(crate) fn widened_unrefused(url: &Url) {
    let now = unix_now();
    let key = HostKey::of(url);
    if HOSTS.kept(&key, now).and_then(|kept| kept.limited).is_some_and(|(until, _)| now >= until) {
        HOSTS.keep(&key, now, |kept| kept.limited = None)
    }
}

/// Forgets what this run saw of `url`'s host (scheme, host and port, every route), as a restart
/// does, keeping what is kept across restarts.
#[cfg(test)]
pub(crate) fn forget_run(url: &Url) {
    let key = HostKey::of(url);
    HOSTS.entries.lock().retain(|other, _| (&other.scheme, &other.host, other.port) != (&key.scheme, &key.host, key.port));
}

/// Records what `url`'s host was seen to do: each fact given replaces the one known, and is
/// trusted for `PROFILE_TTL` from now. Facts left `None` are kept as they are.
pub fn record(url: &Url, seen: HostProfile) {
    HOSTS.record(&HostKey::of(url), seen, Instant::now())
}

/// Keeps what is kept of each host across restarts in `file` from now on, starting from what it
/// holds. A front end calls it once, at start, with a file in its data folder; without it nothing
/// outlives the process. Blocking.
pub fn keep_in(file: PathBuf) {
    HOSTS.keep_in(file, unix_now())
}

/// Writes what is kept of the hosts to its file, if that changed since. Blocking.
pub fn save() {
    HOSTS.save(unix_now())
}

/// The connections a download from `url`'s host ended up fastest with, if that is kept.
pub fn best_connections(url: &Url) -> Option<usize> {
    HOSTS.kept(&HostKey::of(url), unix_now()).and_then(|kept| kept.connections)
}

/// The speed, in bytes per second, more connections did not raise a download from `url`'s host
/// past, if that is kept: the most it serves one user.
pub fn limit(url: &Url) -> Option<f64> {
    HOSTS.kept(&HostKey::of(url), unix_now()).and_then(|kept| kept.limit)
}

/// Keeps what a download from `url`'s host settled on (see `crate::autopilot`): it was fastest
/// with `connections`, and more did not raise it past `limit`, if that is what held it.
pub fn record_settled(url: &Url, connections: usize, limit: Option<f64>) {
    HOSTS.keep(&HostKey::of(url), unix_now(), |kept| {
        kept.connections = Some(connections).filter(|&n| n > 0);
        kept.limit = limit;
    })
}

/// Bytes per second one connection to `url`'s host brought in the last download that measured one,
/// within `SPEED_KEPT_FOR`: a download with several links asks the fastest for its pieces first
/// (see `engine::add_mirror`).
pub fn speed(url: &Url) -> Option<f64> {
    let now = unix_now();
    let kept = HOSTS.kept(&HostKey::of(url), now)?.speed;
    kept.filter(|&(_, at)| Kept::fresh(at, now, SPEED_KEPT_FOR)).map(|(speed, _)| speed)
}

/// Keeps that one connection to `url`'s host brought `speed` bytes per second, unless about that
/// is kept already.
pub(crate) fn record_speed(url: &Url, speed: f64) {
    let now = unix_now();
    HOSTS.keep(&HostKey::of(url), now, |kept| {
        if kept.speed.is_none_or(|(was, at)| !about(was, speed) || !Kept::fresh(at, now, SPEED_KEPT_FOR)) {
            kept.speed = Some((speed, now));
        }
    })
}

/// Whether `a` and `b` are within a quarter of each other: the same fact seen again.
fn about(a: f64, b: f64) -> bool {
    (a - b).abs() <= 0.25 * a.abs().max(b.abs())
}

/// What `url`'s host was seen to speak besides HTTP/1.1, if that is kept.
pub fn protocol(url: &Url) -> Option<Protocol> {
    HOSTS.kept(&HostKey::of(url), unix_now()).and_then(|kept| kept.protocol)
}

/// Keeps that `url`'s host speaks `protocol`; an offer of HTTP/3 never replaces a refusal of
/// HTTP/1.1, which a download must know of first.
pub fn record_protocol(url: &Url, protocol: Protocol) {
    HOSTS.keep(&HostKey::of(url), unix_now(), |kept| {
        if !(protocol == Protocol::H3 && kept.protocol == Some(Protocol::H2)) {
            kept.protocol = Some(protocol);
        }
    })
}

/// The user's line speed, in bytes per second: the fastest a download went, once another went
/// within `LINE_BAND` of it. None before.
pub fn line() -> Option<f64> {
    HOSTS.line(unix_now())
}

/// A download went at most `speed` bytes per second. Learned only once a front end named a file
/// to keep it in (see [`keep_in`]): without one, there is no user whose line it is.
pub fn record_line(speed: f64) {
    HOSTS.record_line(speed, unix_now())
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// Now, by the runtime's clock, which a host's rest is waited out by (paused in tests).
fn clock() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// Holds every request to `url`'s host (scheme, host and port) in this process from now on to one
/// start every `gap`, for a site that asks for that (arXiv: one request every 3 s). The longest
/// gap set for a host holds.
pub(crate) fn set_gap(url: &Url, gap: Duration) {
    if let Some(origin) = HostKey::of(url).origin() {
        let mut gaps = GAPS.lock();
        let at = gaps.entry(origin).or_default();
        *at = (*at).max(gap);
    }
}

/// The gap set for `url`'s host (see [`set_gap`]); zero when none is.
pub(crate) fn gap(url: &Url) -> Duration {
    HostKey::of(url).origin().and_then(|origin| GAPS.lock().get(&origin).copied()).unwrap_or_default()
}

/// Waits until a request to `url`'s host may be opened under `limit` (see the module docs) and
/// returns its slot. Waiting is no failure: nothing else is held meanwhile, and dropping the
/// future gives up the wait. Never wait for a slot while holding another.
pub async fn acquire(url: &Url, limit: usize) -> HostSlot {
    acquire_routed(url, limit, 0).await
}

/// Waits until a request to `url`'s host on specific route `route_id` may be opened under `limit`.
pub async fn acquire_routed(url: &Url, limit: usize, route_id: usize) -> HostSlot {
    acquire_paced(url, limit, route_id, Duration::ZERO).await
}

/// [`acquire_routed`], then the host's gap waited out, or `gap` if that is longer (a listing's
/// pace, see `crate::polite`): the slot's request may start once this returns.
pub(crate) async fn acquire_paced(url: &Url, limit: usize, route_id: usize, gap: Duration) -> HostSlot {
    let slot = HOSTS.acquire(HostKey::routed(url, route_id), limit).await;
    let gap = gap.max(self::gap(url));
    if !gap.is_zero() {
        crate::polite::pace(url, gap).await;
    }
    slot
}

/// A slot for a request to `url`'s host under `limit`, if one is free now.
pub fn try_acquire(url: &Url, limit: usize) -> Option<HostSlot> {
    try_acquire_routed(url, limit, 0)
}

/// A slot for a request to `url`'s host on specific route `route_id` under `limit`, if one is free
/// now and the host's gap since the request before is over.
pub fn try_acquire_routed(url: &Url, limit: usize, route_id: usize) -> Option<HostSlot> {
    let slot = HOSTS.try_acquire(&HostKey::routed(url, route_id), limit, clock())?;
    let gap = gap(url);
    (gap.is_zero() || crate::polite::try_pace(url, gap)).then_some(slot)
}

/// The most requests `url`'s host has had open at once.
#[cfg(test)]
pub(crate) fn peak(url: &Url) -> usize {
    HOSTS.entries.lock().get(&HostKey::of(url)).map_or(0, |entry| entry.peak)
}

/// A local listener for a test server whose host (scheme, host and port) this process has seen
/// nothing of: its port is below the ephemeral range other listeners get theirs from, and no
/// other listener from here had it.
#[cfg(test)]
pub(crate) async fn unseen_listener() -> tokio::net::TcpListener {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT_PORT: AtomicU16 = AtomicU16::new(20_000);
    loop {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        assert!(port < 32_768, "out of ports below the ephemeral range");
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return listener;
        }
    }
}


/// One request open to a host, counted against the host's budget until dropped.
#[must_use = "the slot is given back when dropped"]
pub struct HostSlot {
    hosts: &'static Hosts,
    key: HostKey,
    limit: usize,
    opens_connection: bool,
    /// Its request has no answer yet (counted in `Entry::asking`).
    asking: std::sync::atomic::AtomicBool,
    /// Its request was answered and the host serves it (counted in `Entry::serving`).
    served: std::sync::atomic::AtomicBool,
}

impl HostSlot {
    /// The host this slot is for.
    pub fn host(&self) -> &HostKey {
        &self.key
    }

    /// Whether this slot's request opens a new connection: no request to the host that ended
    /// within `POOL_IDLE` left one that another request has not taken since. A client keeps up to
    /// `POOL_MAX_IDLE` connections to a host for reuse, each for `POOL_IDLE` after its request
    /// ends; a request made without a slot, as resolvers make theirs, may have left one all the
    /// same.
    pub fn opens_connection(&self) -> bool {
        self.opens_connection
    }

    /// The host refused this request with 429/503, asking for a `pause` (its Retry-After): no
    /// request goes to it until that is over. With other requests to it served at the time (see
    /// [`HostSlot::serving`]), it serves no more than those at once: every download to it is held
    /// to that, for `PROFILE_TTL`. With none, the host was just busy. Whether it answered a request within
    /// `BANNED_FOR`: then it paces us rather than refusing everything.
    pub fn throttled(&self, pause: Duration) -> bool {
        let served = self.served.swap(false, std::sync::atomic::Ordering::SeqCst);
        self.hosts.throttled(&self.key, self.answer(), served, pause, clock())
    }

    /// The host accepted this request. Serving as many as the cap it was seen to enforce, it is
    /// offered one more, at most once per `CAP_PROBE_INTERVAL` after the cap was learned or last
    /// raised: a host that means its cap refuses each request offered past it.
    pub fn accepted(&self) {
        let asked = self.answer();
        self.served.fetch_or(asked, std::sync::atomic::Ordering::SeqCst);
        self.hosts.accepted(&self.key, asked, clock())
    }

    /// Requests to this slot's host that it answered and still serves, of every download: what it
    /// was seen to serve at once. A request still waiting for its answer is not counted, as the
    /// answer may be a refusal.
    pub fn serving(&self) -> usize {
        self.hosts.entries.lock().get(&self.key).map_or(0, |entry| entry.serving)
    }

    /// This request got no answer: its connection failed or was dropped. Within `BANNED_FOR` of
    /// the host refusing a request, that is the host shutting us out for a while: it rests,
    /// `BAN_REST` and twice as long after each such request in a row, up to `MAX_BAN_REST`, and
    /// that rest is returned. None otherwise: the request failed.
    pub fn unanswered(&self) -> Option<Duration> {
        self.hosts.unanswered(&self.key, self.answer(), clock())
    }

    /// Whether its request was waiting for an answer, which it no longer is.
    fn answer(&self) -> bool {
        self.asking.swap(false, std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for HostSlot {
    fn drop(&mut self) {
        self.hosts.release(&self.key, self.limit, self.answer(), self.served.load(std::sync::atomic::Ordering::SeqCst), clock())
    }
}

/// A request waiting for a slot, whose limit holds the host's other requests too while it waits.
struct Waiting {
    hosts: &'static Hosts,
    key: HostKey,
    limit: usize,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.hosts.stop_waiting(&self.key, self.limit, Instant::now())
    }
}

/// A fact and when it was learned.
type Fact<T> = Option<(T, Instant)>;

fn fresh<T: Copy>(fact: Fact<T>, now: Instant) -> Option<T> {
    fact.filter(|&(_, at)| now.saturating_duration_since(at) < PROFILE_TTL).map(|(value, _)| value)
}

fn learn<T>(fact: &mut Fact<T>, seen: Option<T>, now: Instant) {
    if let Some(value) = seen {
        *fact = Some((value, now));
    }
}

#[derive(Debug, Default)]
struct Facts {
    accepts_ranges: Fact<bool>,
    capped_per_connection: Fact<bool>,
    connection_rate: Fact<f64>,
    setup_time: Fact<Duration>,
    connection_cap: Fact<usize>,
}

impl Facts {
    fn profile(&self, now: Instant) -> HostProfile {
        HostProfile {
            accepts_ranges: fresh(self.accepts_ranges, now),
            capped_per_connection: fresh(self.capped_per_connection, now),
            connection_rate: fresh(self.connection_rate, now),
            setup_time: fresh(self.setup_time, now),
            connection_cap: fresh(self.connection_cap, now),
        }
    }

    fn record(&mut self, seen: HostProfile, now: Instant) {
        learn(&mut self.accepts_ranges, seen.accepts_ranges, now);
        learn(&mut self.capped_per_connection, seen.capped_per_connection, now);
        learn(&mut self.connection_rate, seen.connection_rate, now);
        learn(&mut self.setup_time, seen.setup_time, now);
        learn(&mut self.connection_cap, seen.connection_cap.filter(|&cap| cap > 0), now);
    }
}

/// Nonzero limits, and how many requests are under each.
type Limits = BTreeMap<usize, usize>;

fn add_limit(limits: &mut Limits, limit: usize) {
    if limit > 0 {
        *limits.entry(limit).or_default() += 1;
    }
}

fn remove_limit(limits: &mut Limits, limit: usize) {
    if let Some(count) = limits.get_mut(&limit) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            limits.remove(&limit);
        }
    }
}

#[derive(Debug, Default)]
struct Entry {
    facts: Facts,
    /// Requests open to the host.
    open: usize,
    /// The limits those requests were opened under.
    held: Limits,
    /// The limits of requests waiting for a slot.
    waiting: Limits,
    /// When the requests to the host whose connections may still be kept for reuse ended, oldest
    /// first: a request taking a slot takes the latest.
    idle: Vec<Instant>,
    /// When the connection cap was last raised.
    cap_raised: Option<Instant>,
    /// Requests open that have no answer yet.
    asking: usize,
    /// Requests open whose answer was accepted: those the host serves at once.
    serving: usize,
    /// No request goes out before this.
    rest_until: Option<Instant>,
    /// When the host last refused a request (429/503), and the answers it gave since: while it
    /// gave fewer than the requests waiting for one, no other goes out.
    refused: Option<(Instant, usize)>,
    /// Requests it left unanswered in a row within `BANNED_FOR` of that.
    unanswered: u32,
    /// When it last answered a request.
    answered_at: Option<Instant>,
    /// The most requests ever open at once.
    #[cfg(test)]
    peak: usize,
}

impl Entry {
    /// Whether nothing would be lost by forgetting the host.
    fn is_idle(&self, now: Instant) -> bool {
        self.open == 0
            && self.waiting.is_empty()
            && self.facts.profile(now) == HostProfile::default()
            && self.idle.iter().all(|&at| now.saturating_duration_since(at) >= POOL_IDLE)
            && self.refused.is_none_or(|(at, _)| now.saturating_duration_since(at) >= BANNED_FOR)
    }

    /// Whether a request may go out `now` as far as the host's rest and its answers since it
    /// refused one go; also when the rest ends, if it rests.
    fn ready(&self, now: Instant) -> Result<(), Option<Instant>> {
        if let Some(until) = self.rest_until.filter(|&until| until > now) {
            return Err(Some(until));
        }
        match self.refused {
            Some((_, answers)) if self.asking > answers => Err(None),
            _ => Ok(()),
        }
    }

    /// The smallest limit among the requests open or waiting.
    fn strictest(&self) -> Option<usize> {
        [self.held.keys().next(), self.waiting.keys().next()].into_iter().flatten().min().copied()
    }

    /// Requests the host may have open once one under `limit` joins them.
    fn allowance(&self, key: &HostKey, limit: usize, now: Instant) -> usize {
        let own = (limit > 0).then_some(limit);
        let cap = fresh(self.facts.connection_cap, now).or_else(|| key.default_connection_cap());
        [own, self.strictest(), cap]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(usize::MAX)
    }
}

/// What is kept of a host across restarts, each as last learned, and when that was (seconds since
/// the Unix epoch).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct Kept {
    /// The connections a download from it ended up fastest with (see `crate::autopilot`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connections: Option<usize>,
    /// The connection cap it was seen to enforce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cap: Option<usize>,
    /// The speed more connections did not raise a download from it past (see [`limit`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limit: Option<f64>,
    /// Under `LINE` only: the fastest a download went, and how many went within `LINE_BAND` of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    line: Option<(f64, u32)>,
    /// The HTTP version it is asked in (see [`protocol`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protocol: Option<Protocol>,
    /// Until when it is kept as limiting how often it is asked, and how many hours that was (see
    /// [`kept_limited`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limited: Option<(u64, u64)>,
    /// The facts below each come with when they were learned, and are trusted for an age of their
    /// own (see [`Kept::fresh`]). Whether it honours ranged GETs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ranges: Option<(bool, u64)>,
    /// How long a new connection to it took to its first answer, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    setup_ms: Option<(u64, u64)>,
    /// The speed it holds each connection to (bytes per second), or none when one went too fast
    /// for any cap to matter (see `HostProfile::capped_per_connection`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    per_connection: Option<(Option<f64>, u64)>,
    /// Bytes per second one connection to it brought in the last download that measured one (see
    /// [`speed`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    speed: Option<(f64, u64)>,
    at: u64,
}

/// Hours a host is first kept as limiting requests (see [`refused_in_download`]), and the most.
const LIMITED_HOURS: u64 = 1;
const MOST_LIMITED_HOURS: u64 = 24;
/// How long a host's speed is kept: a mirror's load changes with the week.
const SPEED_KEPT_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

impl Kept {
    fn tells(&self) -> bool {
        self.connections.is_some()
            || self.cap.is_some()
            || self.limit.is_some()
            || self.line.is_some()
            || self.protocol.is_some()
            || self.limited.is_some()
            || self.ranges.is_some()
            || self.setup_ms.is_some()
            || self.per_connection.is_some()
            || self.speed.is_some()
    }

    /// What of it is still trusted `now`: the facts without an age of their own for `KEPT_FOR`
    /// after the last change to them (`at`), the others each for `KEPT_FOR` after it was learned
    /// (the speed for `SPEED_KEPT_FOR`), and that it limits requests for `KEPT_FOR` after that
    /// ended (a wider try refused then is kept twice as long).
    fn trusted(mut self, now: u64) -> Self {
        let fresh = |at: u64| Kept::fresh(at, now, KEPT_FOR);
        if !fresh(self.at) {
            (self.connections, self.cap, self.limit, self.line, self.protocol) = (None, None, None, None, None);
        }
        self.limited = self.limited.filter(|&(until, _)| fresh(until));
        self.ranges = self.ranges.filter(|&(_, at)| fresh(at));
        self.setup_ms = self.setup_ms.filter(|&(_, at)| fresh(at));
        self.per_connection = self.per_connection.filter(|&(_, at)| fresh(at));
        self.speed = self.speed.filter(|&(_, at)| Kept::fresh(at, now, SPEED_KEPT_FOR));
        self
    }

    /// When anything of it was last learned.
    fn newest(&self) -> u64 {
        let aged = [self.ranges.map(|r| r.1), self.setup_ms.map(|s| s.1), self.per_connection.map(|p| p.1), self.speed.map(|s| s.1)];
        aged.into_iter().flatten().chain([self.at]).max().unwrap_or(0)
    }

    /// Whether a fact learned at `at` (seconds since the Unix epoch) is still trusted `now`, `age`
    /// after it.
    fn fresh(at: u64, now: u64, age: Duration) -> bool {
        now.saturating_sub(at) < age.as_secs()
    }
}

/// What a host was seen to speak besides HTTP/1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    /// It refused HTTP/1.1 and answered over HTTP/2: a download from it asks over HTTP/2 at once.
    #[serde(rename = "h2")]
    H2,
    /// It offers HTTP/3 (`Alt-Svc`), for a build that speaks it: reqwest's HTTP/3 still needs
    /// the unstable `reqwest_unstable` flag. This one asks over HTTP/1.1 as before.
    #[serde(rename = "h3")]
    H3,
}

#[derive(Debug, Default)]
struct Memory {
    /// By `HostKey::origin`.
    hosts: HashMap<String, Kept>,
    /// Where it is saved, once a front end named a file (see [`keep_in`]).
    file: Option<PathBuf>,
    changed: bool,
}

impl Memory {
    /// Drops what is older than `KEPT_FOR` or tells nothing, and past `KEEP_KEPT` what was
    /// learned first: what is kept is only what is trusted (see `Kept::trusted`). Whether that
    /// changed anything.
    fn prune(&mut self, now: u64) -> bool {
        let before = self.hosts.clone();
        for kept in self.hosts.values_mut() {
            *kept = kept.trusted(now);
        }
        self.hosts.retain(|_, kept| kept.tells());
        if self.hosts.len() > KEEP_KEPT {
            let mut ages: Vec<u64> = self.hosts.values().map(Kept::newest).collect();
            ages.sort_unstable_by(|a, b| b.cmp(a));
            let oldest_kept = ages[KEEP_KEPT - 1];
            self.hosts.retain(|_, kept| kept.newest() >= oldest_kept);
        }
        self.hosts != before
    }
}

/// Saves of `Memory` one at a time, each writing the file outside its lock (see `Hosts::save`).
static SAVING: Mutex<()> = Mutex::new(());

#[derive(Debug, Default)]
struct Hosts {
    entries: Mutex<HashMap<HostKey, Entry>>,
    /// Woken whenever a host may take more requests: a slot was freed or a cap rose.
    freed: Notify,
    /// Locked after `entries` when both are.
    kept: Mutex<Memory>,
}

impl Hosts {
    fn keep_in(&self, file: PathBuf, now: u64) {
        let read = std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice::<HashMap<String, Kept>>(&bytes).ok());
        let mut memory = self.kept.lock();
        for (origin, mut kept) in read.unwrap_or_default() {
            // A clock set wrong when it was kept, or an edit, holds a host no longer than a run would.
            let most = now.saturating_add(MOST_LIMITED_HOURS * 3600);
            kept.limited = kept.limited.map(|(until, hours)| (until.min(most), hours.min(MOST_LIMITED_HOURS)));
            memory.hosts.entry(origin).or_insert(kept);
        }
        // What is past its age leaves the file at the next save.
        if memory.prune(now) {
            memory.changed = true;
        }
        memory.file = Some(file);
    }

    fn save(&self, now: u64) {
        let _saving = SAVING.lock();
        let (file, bytes) = {
            let mut memory = self.kept.lock();
            let Some(file) = memory.file.clone().filter(|_| memory.changed) else { return };
            memory.prune(now);
            memory.changed = false;
            (file, serde_json::to_vec(&memory.hosts))
        };
        // shortcut: the last process to save wins (the CLI's run beside the app's); merge with the
        // file first if what one learns going missing ever matters.
        let written = bytes.map_err(std::io::Error::from).and_then(|bytes| {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = file.with_extension("json.tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &file)
        });
        if let Err(e) = written {
            self.kept.lock().changed = true;
            tracing::warn!("Could not save what is known of hosts to {}: {}", file.display(), e);
        }
    }

    /// What is kept of `key`'s host, if anything and not too old.
    fn kept(&self, key: &HostKey, now: u64) -> Option<Kept> {
        self.kept_as(&key.origin()?, now)
    }

    /// What is kept under `name`, if anything and not too old.
    fn kept_as(&self, name: &str, now: u64) -> Option<Kept> {
        let kept = self.kept.lock().hosts.get(name)?.trusted(now);
        kept.tells().then_some(kept)
    }

    /// Changes what is kept of `key`'s host, learned `now`.
    fn keep(&self, key: &HostKey, now: u64, change: impl FnOnce(&mut Kept)) {
        if let Some(origin) = key.origin() {
            self.keep_as(origin, now, change)
        }
    }

    fn line(&self, now: u64) -> Option<f64> {
        self.kept_as(LINE, now).and_then(|kept| kept.line).filter(|&(_, seen)| seen >= 2).map(|(fastest, _)| fastest)
    }

    fn record_line(&self, speed: f64, now: u64) {
        if self.kept.lock().file.is_none() {
            return;
        }
        let line = match self.kept_as(LINE, now).and_then(|kept| kept.line) {
            // Slower tells nothing of the line: the server held it back, or other downloads shared it.
            Some((fastest, _)) if speed < fastest * (1.0 - LINE_BAND) => return,
            Some((fastest, seen)) if speed <= fastest * (1.0 + LINE_BAND) => (fastest.max(speed), seen + 1),
            _ if speed > 0.0 => (speed, 1),
            _ => return,
        };
        self.keep_as(LINE.to_string(), now, |kept| kept.line = Some(line))
    }

    /// Changes what is kept of `name`. Only a change is written out (see `save`), and only a change
    /// of the facts without an age of their own makes the host's `at` `now`.
    fn keep_as(&self, name: String, now: u64, change: impl FnOnce(&mut Kept)) {
        let mut memory = self.kept.lock();
        let kept = memory.hosts.entry(name).or_default();
        let before = *kept;
        change(kept);
        if *kept == before {
            return;
        }
        let ageless = |k: &Kept| (k.connections, k.cap, k.limit, k.line, k.protocol, k.limited);
        if ageless(kept) != ageless(&before) || before.at == 0 {
            kept.at = now;
        }
        memory.changed = true;
    }

    /// `key`'s entry; a new one starts from what is kept of the host, as if just learned.
    fn entry<'a>(&self, entries: &'a mut HashMap<HostKey, Entry>, key: &HostKey, now: Instant) -> &'a mut Entry {
        entries.entry(key.clone()).or_insert_with(|| Entry { facts: self.kept_facts(key, now), ..Entry::default() })
    }

    /// What is kept of `key`'s host across restarts, as facts learned `now`: its connection cap,
    /// whether it honours ranges, how long a new connection to it takes and the speed it holds
    /// each connection to, each while trusted.
    fn kept_facts(&self, key: &HostKey, now: Instant) -> Facts {
        let mut facts = Facts::default();
        let unix = unix_now();
        let Some(kept) = self.kept(key, unix) else { return facts };
        let fresh = |at: u64| Kept::fresh(at, unix, KEPT_FOR);
        facts.connection_cap = kept.cap.map(|cap| (cap, now));
        facts.accepts_ranges = kept.ranges.filter(|&(_, at)| fresh(at)).map(|(ranges, _)| (ranges, now));
        facts.setup_time = kept.setup_ms.filter(|&(_, at)| fresh(at)).map(|(ms, _)| (Duration::from_millis(ms), now));
        if let Some((rate, _)) = kept.per_connection.filter(|&(_, at)| fresh(at)) {
            facts.capped_per_connection = Some((rate.is_some(), now));
            facts.connection_rate = rate.map(|rate| (rate, now));
        }
        facts
    }

    fn profile(&self, key: &HostKey, now: Instant) -> HostProfile {
        let mut prof = match self.entries.lock().get(key) {
            Some(entry) => entry.facts.profile(now),
            None => self.kept_facts(key, now).profile(now),
        };
        if prof.connection_cap.is_none() {
            prof.connection_cap = key.default_connection_cap();
        }
        prof
    }

    fn refused_lately(&self, key: &HostKey, now: Instant) -> bool {
        self.entries.lock().iter().any(|(other, entry)| {
            (&other.scheme, &other.host, other.port) == (&key.scheme, &key.host, key.port)
                && entry.refused.is_some_and(|(at, _)| now.saturating_duration_since(at) < BANNED_FOR)
        })
    }

    fn rest_left(&self, key: &HostKey, now: Instant) -> Duration {
        let entries = self.entries.lock();
        let same = entries.iter().filter(|(other, _)| (&other.scheme, &other.host, other.port) == (&key.scheme, &key.host, key.port));
        same.filter_map(|(_, entry)| entry.rest_until).map(|until| until.saturating_duration_since(now)).max().unwrap_or_default()
    }

    fn record(&self, key: &HostKey, seen: HostProfile, now: Instant) {
        {
            let mut entries = self.entries.lock();
            self.entry(&mut entries, key, now).facts.record(seen, now);
            forget_idle(&mut entries, now);
        }
        // What the host is like is kept for the next run too, each fact with when it was learned:
        // seen again about the same while it is trusted, it keeps that age.
        if seen.accepts_ranges.is_some() || seen.setup_time.is_some() || seen.capped_per_connection.is_some() {
            let unix = unix_now();
            let fresh = |at: u64| Kept::fresh(at, unix, KEPT_FOR);
            self.keep(key, unix, |kept| {
                if let Some(ranges) = seen.accepts_ranges.filter(|&ranges| kept.ranges.is_none_or(|(was, at)| was != ranges || !fresh(at))) {
                    kept.ranges = Some((ranges, unix));
                }
                let setup = seen.setup_time.map(|setup| setup.as_millis() as u64);
                if let Some(ms) = setup.filter(|&ms| kept.setup_ms.is_none_or(|(was, at)| !about(was as f64, ms as f64) || !fresh(at))) {
                    kept.setup_ms = Some((ms, unix));
                }
                if let Some(capped) = seen.capped_per_connection {
                    let rate = seen.connection_rate.filter(|_| capped);
                    let same = |was: Option<f64>| match (was, rate) {
                        (Some(was), Some(rate)) => about(was, rate),
                        (was, rate) => was.is_none() && rate.is_none(),
                    };
                    if kept.per_connection.is_none_or(|(was, at)| !same(was) || !fresh(at)) {
                        kept.per_connection = Some((rate, unix));
                    }
                }
            });
        }
        if seen.connection_cap.is_some() {
            self.freed.notify_waiters();
        }
    }

    fn try_acquire(&'static self, key: &HostKey, limit: usize, now: Instant) -> Option<HostSlot> {
        self.take(key, limit, now).ok()
    }

    /// A slot, else when the host's rest ends if that is what holds the request back.
    fn take(&'static self, key: &HostKey, limit: usize, now: Instant) -> Result<HostSlot, Option<Instant>> {
        let mut entries = self.entries.lock();
        let entry = self.entry(&mut entries, key, now);
        entry.ready(now)?;
        if entry.open >= entry.allowance(key, limit, now) {
            return Err(None);
        }
        entry.open += 1;
        entry.asking += 1;
        add_limit(&mut entry.held, limit);
        #[cfg(test)]
        {
            entry.peak = entry.peak.max(entry.open);
        }
        entry.idle.retain(|&at| now.saturating_duration_since(at) < POOL_IDLE);
        let opens_connection = entry.idle.pop().is_none();
        Ok(HostSlot { hosts: self, key: key.clone(), limit, opens_connection, asking: true.into(), served: false.into() })
    }

    async fn acquire(&'static self, key: HostKey, limit: usize) -> HostSlot {
        // A lenient download re-taking slots as its requests end would otherwise keep a strict
        // one waiting until it is nearly done: the strict limit counts from now on.
        let _waiting = {
            add_limit(&mut self.entry(&mut self.entries.lock(), &key, Instant::now()).waiting, limit);
            Waiting { hosts: self, key: key.clone(), limit }
        };
        loop {
            // Registered before looking, so a slot freed in between still wakes us.
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            match self.take(&key, limit, clock()) {
                Ok(slot) => return slot,
                // Nothing is freed when a rest ends.
                Err(Some(rested)) => {
                    tokio::select! {
                        _ = freed => {}
                        _ = tokio::time::sleep_until(rested.into()) => {}
                    }
                }
                Err(None) => freed.await,
            }
        }
    }

    /// Counts a request that `asked` for an answer until now as no longer asking.
    fn answered(entry: &mut Entry, asked: bool) {
        if asked {
            entry.asking = entry.asking.saturating_sub(1);
        }
    }

    /// Rests `entry`'s host until at least `until`.
    fn rest(entry: &mut Entry, until: Instant) {
        entry.rest_until = Some(entry.rest_until.map_or(until, |rest| rest.max(until)));
    }

    fn stop_waiting(&self, key: &HostKey, limit: usize, now: Instant) {
        let loosened = {
            let mut entries = self.entries.lock();
            let loosened = entries.get_mut(key).is_some_and(|entry| {
                let before = entry.strictest();
                remove_limit(&mut entry.waiting, limit);
                entry.strictest() != before
            });
            forget_idle(&mut entries, now);
            loosened
        };
        // A wait given up may leave the others a larger allowance.
        if loosened {
            self.freed.notify_waiters();
        }
    }

    fn release(&self, key: &HostKey, limit: usize, asked: bool, served: bool, now: Instant) {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.get_mut(key) {
                Self::answered(entry, asked);
                if served {
                    entry.serving = entry.serving.saturating_sub(1);
                }
                entry.open = entry.open.saturating_sub(1);
                remove_limit(&mut entry.held, limit);
                if entry.idle.len() >= POOL_MAX_IDLE {
                    entry.idle.remove(0);
                }
                entry.idle.push(now);
            }
            forget_idle(&mut entries, now);
        }
        self.freed.notify_waiters();
    }

    fn throttled(&self, key: &HostKey, asked: bool, served: bool, pause: Duration, now: Instant) -> bool {
        let mut entries = self.entries.lock();
        let Some(entry) = entries.get_mut(key) else { return false };
        Self::answered(entry, asked);
        if served {
            entry.serving = entry.serving.saturating_sub(1);
        }
        Self::rest(entry, now + pause);
        entry.refused = Some((now, 0));
        entry.unanswered = 0;
        // Not `open` less the refused one: requests of ours still waiting for their answer (another
        // of the same round, say) may be refused as well, and the host was then taken to serve
        // more than it does. A served one whose answer is still on its way counts for less instead,
        // which the cap's probing upward (see `accepted`) makes good.
        let others = entry.serving;
        if others > 0 {
            entry.facts.connection_cap = Some((others, now));
            let unix = unix_now();
            self.keep(key, unix, |kept| {
                kept.cap = Some(others);
                // A download that settled while a slot it had just freed still counted settled past
                // what the host serves at once: no later one starts there.
                kept.connections = kept.connections.map(|n| n.min(others));
            });
        }
        entry.answered_at.is_some_and(|at| now.saturating_duration_since(at) < BANNED_FOR)
    }

    fn unanswered(&self, key: &HostKey, asked: bool, now: Instant) -> Option<Duration> {
        let mut entries = self.entries.lock();
        let entry = entries.get_mut(key)?;
        Self::answered(entry, asked);
        if !asked || entry.refused.is_none_or(|(at, _)| now.saturating_duration_since(at) >= BANNED_FOR) {
            return None;
        }
        // Requests out before the host began to rest for this fail alike: they are one sign.
        if entry.unanswered > 0 && entry.rest_until.is_some_and(|until| until > now) {
            return entry.rest_until.map(|until| until - now);
        }
        entry.unanswered += 1;
        let pause = BAN_REST.saturating_mul(1 << (entry.unanswered - 1).min(8)).min(MAX_BAN_REST);
        Self::rest(entry, now + pause);
        entry.refused = entry.refused.map(|(at, _)| (at, 0));
        Some(pause)
    }

    fn accepted(&self, key: &HostKey, asked: bool, now: Instant) {
        let raised = self.entries.lock().get_mut(key).is_some_and(|entry| {
            Self::answered(entry, asked);
            entry.serving += usize::from(asked);
            entry.unanswered = 0;
            entry.answered_at = Some(now);
            // Each answer since a refusal lets one more request wait for its answer at once.
            let grew = entry.refused.as_mut().map(|(_, answers)| *answers += 1).is_some();
            grew | match (entry.facts.connection_cap, fresh(entry.facts.connection_cap, now)) {
                (Some((cap, learned)), Some(_)) if entry.open >= cap => {
                    let since = entry.cap_raised.map_or(learned, |raised| raised.max(learned));
                    if now.saturating_duration_since(since) < CAP_PROBE_INTERVAL {
                        return grew;
                    }
                    // The cap keeps the time it was learned at, so it still expires.
                    entry.facts.connection_cap = Some((cap + 1, learned));
                    entry.cap_raised = Some(now);
                    self.keep(key, unix_now(), |kept| kept.cap = Some(cap + 1));
                    true
                }
                // Serving more than a cap kept from before, which has since expired, the host
                // has none so low any more.
                (_, None) => {
                    if self.kept(key, unix_now()).and_then(|kept| kept.cap).is_some_and(|cap| entry.open > cap) {
                        self.keep(key, unix_now(), |kept| kept.cap = None);
                    }
                    false
                }
                _ => false,
            }
        });
        if raised {
            self.freed.notify_waiters();
        }
    }
}

/// Drops the hosts with nothing to tell once there are many, so a long session stays small.
fn forget_idle(entries: &mut HashMap<HostKey, Entry>, now: Instant) {
    if entries.len() > KEEP_HOSTS {
        entries.retain(|_, entry| !entry.is_idle(now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(url: &str) -> HostKey {
        HostKey::of(&Url::parse(url).unwrap())
    }

    #[test]
    fn test_youtube_googlevideo_chunk_bounding() {
        let yt_url = Url::parse("https://rr4---sn-ab5sznzl.googlevideo.com/videoplayback?expire=123").unwrap();
        assert!(HostKey::of(&yt_url).is_googlevideo());
        assert_eq!(max_chunk_size(&yt_url), Some(10 * 1024 * 1024));

        let generic_url = Url::parse("https://example.com/largefile.iso").unwrap();
        assert!(!HostKey::of(&generic_url).is_googlevideo());
        assert_eq!(max_chunk_size(&generic_url), None);
    }

    #[test]
    fn test_hosts_are_told_apart_by_scheme_host_and_port() {
        assert_eq!(key("http://files.example/a.bin"), key("http://FILES.example:80/other?x=1"));
        assert_ne!(key("http://files.example/"), key("https://files.example/"));
        assert_ne!(key("http://files.example/"), key("http://files.example:8080/"));
        assert_ne!(key("http://files.example/"), key("http://cdn.files.example/"));
    }

    #[test]
    fn test_recorded_facts_merge_and_each_expires_on_its_own() {
        let hosts = Hosts::default();
        let (host, t0) = (key("https://files.example/"), Instant::now());
        assert_eq!(hosts.profile(&host, t0), HostProfile::default(), "nothing known yet");

        hosts.record(&host, HostProfile { accepts_ranges: Some(true), setup_time: Some(Duration::from_millis(80)), ..Default::default() }, t0);
        let later = t0 + Duration::from_secs(300);
        hosts.record(&host, HostProfile { connection_rate: Some(2e6), capped_per_connection: Some(true), ..Default::default() }, later);
        let seen = hosts.profile(&host, later);
        assert_eq!(seen.accepts_ranges, Some(true), "a later record keeps what it does not mention");
        assert_eq!((seen.connection_rate, seen.capped_per_connection), (Some(2e6), Some(true)));
        assert_eq!(seen.setup_time, Some(Duration::from_millis(80)));

        // Ten minutes after the first record, its facts are gone; the later ones stay.
        let expired = hosts.profile(&host, t0 + PROFILE_TTL);
        assert_eq!((expired.accepts_ranges, expired.setup_time), (None, None));
        assert_eq!(expired.connection_rate, Some(2e6));
        // A fact seen again is trusted afresh.
        hosts.record(&host, HostProfile { accepts_ranges: Some(false), ..Default::default() }, t0 + PROFILE_TTL);
        assert_eq!(hosts.profile(&host, t0 + PROFILE_TTL).accepts_ranges, Some(false));
        assert_eq!(hosts.profile(&key("https://other.example/"), t0), HostProfile::default());
    }

    #[test]
    fn test_many_hosts_with_nothing_fresh_are_forgotten() {
        let hosts = Hosts::default();
        let t0 = Instant::now();
        let cap = HostProfile { connection_cap: Some(4), ..Default::default() };
        for n in 0..=KEEP_HOSTS {
            hosts.record(&key(&format!("http://h{n}.example/")), cap, t0);
        }
        assert_eq!(hosts.entries.lock().len(), KEEP_HOSTS + 1, "fresh facts are kept");
        hosts.record(&key("http://new.example/"), cap, t0 + PROFILE_TTL);
        assert_eq!(hosts.entries.lock().len(), 1);
    }

    fn leaked() -> &'static Hosts {
        Box::leak(Box::default())
    }

    #[test]
    fn test_a_host_takes_no_more_requests_than_the_strictest_download_using_it_allows() {
        let (hosts, host, now) = (leaked(), key("http://files.example/"), Instant::now());
        let mut four: Vec<HostSlot> = (0..4).map(|_| hosts.try_acquire(&host, 4, now).unwrap()).collect();
        assert!(hosts.try_acquire(&host, 4, now).is_none());
        assert!(hosts.try_acquire(&host, 8, now).is_none(), "the requests open allow no more than 4");
        assert!(hosts.try_acquire(&key("http://other.example/"), 4, now).is_some(), "each host has its own budget");

        four.truncate(1);
        let two = hosts.try_acquire(&host, 2, now).unwrap();
        assert!(hosts.try_acquire(&host, 4, now).is_none(), "a download allowing 2 holds the host to 2");
        assert!(hosts.try_acquire(&host, 0, now).is_none(), "one without a limit too");
        drop((two, four));
        let unlimited: Vec<HostSlot> = (0..100).map(|_| hosts.try_acquire(&host, 0, now).unwrap()).collect();
        assert_eq!(hosts.entries.lock()[&host].open, 100);
        drop(unlimited);
        assert_eq!(hosts.entries.lock()[&host].open, 0, "every slot is given back");
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_waiting_request_gets_the_slot_another_frees() {
        let (hosts, host) = (leaked(), key("http://files.example/"));
        let first = hosts.try_acquire(&host, 1, Instant::now()).unwrap();
        // A wait given up holds nothing.
        assert!(tokio::time::timeout(Duration::from_secs(60), hosts.acquire(host.clone(), 1)).await.is_err());
        let waiting = tokio::spawn(hosts.acquire(host.clone(), 1));
        tokio::task::yield_now().await;
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(60), waiting).await.expect("the waiter gets the freed slot");
        assert_eq!(second.unwrap().host(), &host);
        assert_eq!(hosts.entries.lock()[&host].open, 0);
    }

    #[test]
    fn test_a_cap_seen_in_refusals_holds_every_download_until_it_expires() {
        let (hosts, host, t0) = (leaked(), key("http://capped.example/"), Instant::now());
        // Refused with nothing else open, the host was only busy.
        let alone = hosts.try_acquire(&host, 32, t0).unwrap();
        alone.throttled(Duration::ZERO);
        drop(alone);
        assert_eq!(hosts.profile(&host, t0).connection_cap, None);
        // Refused with two others open, it serves two at once. (Since the refusal each request
        // waits for the answers of those before it.)
        let three: Vec<HostSlot> = (0..3).map(|_| answered(hosts.try_acquire(&host, 32, Instant::now()).unwrap())).collect();
        three[2].throttled(Duration::ZERO);
        drop(three);
        assert_eq!(hosts.profile(&host, Instant::now()).connection_cap, Some(2));

        // A later download from the host starts at that cap, whatever its own limit allows.
        let now = Instant::now();
        let mut later: Vec<HostSlot> = (0..2).map(|_| answered(hosts.try_acquire(&host, 32, now).unwrap())).collect();
        assert!(hosts.try_acquire(&host, 32, now).is_none());
        // Served in full at the cap, the host is offered one more request, but not every time:
        // each offer a capped host refuses costs a request.
        hosts.accepted(&host, false, now);
        assert!(hosts.try_acquire(&host, 32, now).is_none(), "the cap was just learned");
        let probe = now + CAP_PROBE_INTERVAL;
        hosts.accepted(&host, false, probe);
        later.push(answered(hosts.try_acquire(&host, 32, probe).unwrap()));
        assert_eq!(hosts.profile(&host, probe).connection_cap, Some(3));
        hosts.accepted(&host, false, probe);
        assert!(hosts.try_acquire(&host, 32, probe).is_none(), "the cap was just raised");
        // Refused again: back to what it served, and the next offer waits a full interval from then.
        let refused = probe + Duration::from_secs(5);
        let last = later.pop().unwrap();
        hosts.throttled(&host, last.answer(), last.served.swap(false, std::sync::atomic::Ordering::SeqCst), Duration::ZERO, refused);
        drop(last);
        hosts.accepted(&host, false, probe + CAP_PROBE_INTERVAL);
        assert_eq!(hosts.profile(&host, refused).connection_cap, Some(2));
        hosts.accepted(&host, false, refused + CAP_PROBE_INTERVAL);
        assert_eq!(hosts.profile(&host, refused).connection_cap, Some(3));
        // Ten minutes after it was seen, the cap is forgotten.
        assert!(hosts.try_acquire(&host, 32, refused + PROFILE_TTL).is_some());
    }

    fn answered(slot: HostSlot) -> HostSlot {
        slot.accepted();
        slot
    }

    /// Refused while two more requests of ours wait for their answers, a host that serves three
    /// was taken to serve four: those two may be refused as well (here both are, the second before
    /// the first gives its slot back). Only the requests it answered and still serves count.
    #[test]
    fn test_a_refusal_counts_only_the_requests_being_served() {
        let (hosts, host, t0) = (leaked(), key("http://still-asking.example/"), Instant::now());
        let served: Vec<HostSlot> = (0..3).map(|_| answered(hosts.try_acquire(&host, 32, t0).unwrap())).collect();
        let (first, second) = (hosts.try_acquire(&host, 32, t0).unwrap(), hosts.try_acquire(&host, 32, t0).unwrap());
        first.throttled(Duration::ZERO);
        assert_eq!(hosts.profile(&host, Instant::now()).connection_cap, Some(3));
        second.throttled(Duration::ZERO);
        assert_eq!(hosts.profile(&host, Instant::now()).connection_cap, Some(3));
        assert_eq!(hosts.kept(&host, unix_now()).and_then(|kept| kept.cap), Some(3));
        drop((first, second, served));
        assert_eq!(hosts.entries.lock()[&host].serving, 0, "every answered slot is given back");
    }

    #[tokio::test]
    async fn test_a_refusal_rests_the_host_and_requests_come_back_one_at_a_time() {
        let (hosts, host) = (leaked(), key("http://resting.example/"));
        let refused = hosts.try_acquire(&host, 0, clock()).unwrap();
        hosts.throttled(&host, refused.answer(), false, Duration::from_millis(300), clock());
        drop(refused);
        assert!(hosts.try_acquire(&host, 0, clock()).is_none(), "no request goes out while it rests");
        let started = tokio::time::Instant::now();
        let first = tokio::time::timeout(Duration::from_secs(10), hosts.acquire(host.clone(), 0)).await.expect("taken once the rest is over");
        assert!(started.elapsed() >= Duration::from_millis(250));
        // The next waits for the first's answer; after it, two may wait for theirs at once.
        let second = tokio::spawn(hosts.acquire(host.clone(), 0));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!second.is_finished(), "one request at a time");
        first.accepted();
        let _second = tokio::time::timeout(Duration::from_secs(10), second).await.expect("the answer lets the next go").unwrap();
        let _third = hosts.try_acquire(&host, 0, clock()).expect("one answer lets two ask at once");
        assert!(hosts.try_acquire(&host, 0, clock()).is_none());
    }

    #[test]
    fn test_a_host_that_shuts_us_out_after_refusing_us_is_waited_for_with_growing_rests() {
        let (hosts, host, t0) = (leaked(), key("http://banning.example/"), Instant::now());
        let slot = hosts.try_acquire(&host, 0, t0).unwrap();
        assert_eq!(hosts.unanswered(&host, slot.answer(), t0), None, "a host that never refused us: the request failed");
        drop(slot);
        let (out, also) = (hosts.try_acquire(&host, 0, t0).unwrap(), hosts.try_acquire(&host, 0, t0).unwrap());
        let refused = hosts.try_acquire(&host, 0, t0).unwrap();
        assert!(!hosts.throttled(&host, refused.answer(), false, Duration::from_secs(1), t0), "it never answered: it does not pace us");
        drop(refused);
        // Two requests out when it shut us out fail alike: one sign, one rest.
        let at = t0 + Duration::from_secs(1);
        assert_eq!(hosts.unanswered(&host, out.answer(), at), Some(BAN_REST));
        assert_eq!(hosts.unanswered(&host, also.answer(), at + Duration::from_secs(1)), Some(BAN_REST - Duration::from_secs(1)));
        drop((out, also));
        // From then on one request at a time, each rest twice the last, up to a bound.
        let (mut at, mut rests) = (at + BAN_REST, Vec::new());
        for _ in 0..5 {
            let slot = hosts.try_acquire(&host, 0, at).expect("the rest is over");
            assert!(hosts.try_acquire(&host, 0, at).is_none(), "one request at a time");
            let rest = hosts.unanswered(&host, slot.answer(), at).unwrap();
            assert!(hosts.try_acquire(&host, 0, at + rest - Duration::from_millis(1)).is_none(), "it rests");
            rests.push(rest.as_secs());
            at += rest;
        }
        assert_eq!(rests, [10, 20, 40, 60, 60]);
        // An answer ends the streak.
        hosts.accepted(&host, hosts.try_acquire(&host, 0, at).unwrap().answer(), at);
        let slot = hosts.try_acquire(&host, 0, at).unwrap();
        assert_eq!(hosts.unanswered(&host, slot.answer(), at), Some(BAN_REST));
        drop(slot);
        // Long after the refusal a host that does not answer is dead: its requests fail.
        let late = t0 + BANNED_FOR;
        let slot = hosts.try_acquire(&host, 0, late).unwrap();
        assert_eq!(hosts.unanswered(&host, slot.answer(), late), None);
        drop(slot);
        // A refusal by a host that answered lately paces us; long after its last answer it does not.
        let slot = hosts.try_acquire(&host, 0, late).unwrap();
        assert!(hosts.throttled(&host, slot.answer(), false, Duration::ZERO, late));
        drop(slot);
        let slot = hosts.try_acquire(&host, 0, at + BANNED_FOR).unwrap();
        assert!(!hosts.throttled(&host, slot.answer(), false, Duration::ZERO, at + BANNED_FOR));
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_cap_of_zero_is_no_cap() {
        let (hosts, host) = (leaked(), key("http://zero.example/"));
        hosts.record(&host, HostProfile { connection_cap: Some(0), ..Default::default() }, Instant::now());
        assert_eq!(hosts.profile(&host, Instant::now()).connection_cap, None);
        let slot = tokio::time::timeout(Duration::from_secs(60), hosts.acquire(host.clone(), 4)).await;
        assert!(slot.is_ok(), "a host always takes one request");
    }

    /// A host given a gap takes one request every gap, whichever way its slots are taken; a
    /// listing's longer pace holds too, and other hosts are not held at all.
    #[tokio::test(start_paused = true)]
    async fn test_a_host_with_a_gap_takes_one_request_every_gap() {
        let url = Url::parse("http://gap.example/f").unwrap();
        set_gap(&url, Duration::from_secs(3));
        set_gap(&url, Duration::from_secs(1));
        assert_eq!(gap(&url), Duration::from_secs(3), "the longest gap set holds");
        let started = tokio::time::Instant::now();
        for _ in 0..3 {
            drop(acquire(&url, 0).await);
        }
        assert_eq!(started.elapsed(), Duration::from_secs(6), "three requests, two gaps");
        assert!(try_acquire(&url, 0).is_none(), "the next is 3 s away");
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(try_acquire(&url, 0).is_some());
        drop(acquire_paced(&url, 0, 0, Duration::from_secs(5)).await);
        drop(acquire(&url, 0).await);
        assert_eq!(started.elapsed(), Duration::from_secs(9 + 3 + 5), "a listing's longer pace holds the next");
        let other = Url::parse("http://nogap.example/f").unwrap();
        let at = tokio::time::Instant::now();
        drop(acquire(&other, 0).await);
        drop(acquire(&other, 0).await);
        assert_eq!(at.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_strict_download_waiting_holds_a_lenient_one_to_its_limit() {
        let (hosts, host) = (leaked(), key("http://shared.example/"));
        let mut lenient: Vec<HostSlot> = (0..20).map(|_| hosts.try_acquire(&host, 32, Instant::now()).unwrap()).collect();
        let strict = tokio::spawn(hosts.acquire(host.clone(), 4));
        tokio::task::yield_now().await;
        // Its requests ending one by one, the lenient download may not take their slots again
        // while it has 4 or more open: the strict one waits for fewer than 4.
        lenient.truncate(4);
        assert!(hosts.try_acquire(&host, 32, Instant::now()).is_none());
        lenient.pop();
        let strict = tokio::time::timeout(Duration::from_secs(60), strict).await.expect("the strict download gets a slot");
        assert!(hosts.try_acquire(&host, 32, Instant::now()).is_none(), "4 are open, the strict one's among them");
        drop(strict);

        // A wait given up lifts its limit, and wakes whoever it held back.
        lenient.push(hosts.try_acquire(&host, 32, Instant::now()).unwrap());
        let started = tokio::time::Instant::now();
        let (gave_up, more) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(1), hosts.acquire(host.clone(), 4)),
            tokio::time::timeout(Duration::from_secs(60), hosts.acquire(host.clone(), 32)),
        );
        assert!(gave_up.is_err());
        let more = more.expect("the lenient download may open more once nothing stricter waits");
        assert_eq!(more.host(), &host);
        assert!(started.elapsed() >= Duration::from_secs(1), "not while the strict one waited");
        assert!(hosts.entries.lock()[&host].waiting.is_empty());
    }

    #[test]
    fn test_a_slot_tells_whether_its_request_opens_a_connection() {
        let (hosts, host, t0) = (leaked(), key("http://idle.example/"), Instant::now());
        let first = hosts.try_acquire(&host, 0, t0).unwrap();
        let open = hosts.try_acquire(&host, 0, t0).unwrap();
        assert!(first.opens_connection());
        assert!(open.opens_connection(), "a request still open leaves no connection to reuse");
        drop((first, open));

        // Two requests ended just now, so two more reuse their connections; a third, although
        // requests to the host keep ending, finds none left to reuse.
        let now = Instant::now();
        let reused: Vec<HostSlot> = (0..2).map(|_| hosts.try_acquire(&host, 0, now).unwrap()).collect();
        assert!(reused.iter().all(|slot| !slot.opens_connection()));
        assert!(hosts.try_acquire(&host, 0, now).unwrap().opens_connection());
        drop(reused);
        // A connection is kept for so long after its request ended.
        assert!(hosts.try_acquire(&host, 0, Instant::now() + POOL_IDLE).unwrap().opens_connection());
        // And no more of them than a client keeps.
        let many: Vec<HostSlot> = (0..POOL_MAX_IDLE + 1).map(|_| hosts.try_acquire(&host, 0, Instant::now()).unwrap()).collect();
        drop(many);
        let now = Instant::now();
        let again: Vec<HostSlot> = (0..POOL_MAX_IDLE + 1).map(|_| hosts.try_acquire(&host, 0, now).unwrap()).collect();
        assert_eq!(again.iter().filter(|slot| slot.opens_connection()).count(), 1);
    }

    #[tokio::test]
    async fn test_the_process_wide_budget() {
        let url = Url::parse("http://budget-test.hosts.example/f.bin").unwrap();
        let slot = acquire(&url, 1).await;
        assert_eq!(slot.host(), &HostKey::of(&url));
        assert!(try_acquire(&Url::parse("http://budget-test.hosts.example/other.bin").unwrap(), 1).is_none());
        drop(slot);
        assert!(try_acquire(&url, 1).is_some());
    }

    #[test]
    fn test_a_cap_and_the_best_connection_count_are_kept_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hosts.json");
        let (host, now) = (key("http://kept.example/dir/f.bin?token=secret"), unix_now());
        let before = leaked();
        before.keep_in(file.clone(), now);
        let three: Vec<HostSlot> = (0..3).map(|_| before.try_acquire(&host, 32, Instant::now()).unwrap()).collect();
        three[..2].iter().for_each(HostSlot::accepted);
        three[2].throttled(Duration::ZERO);
        drop(three);
        before.keep(&host, now, |kept| kept.connections = Some(6));
        before.save(now);
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(saved.contains("http://kept.example:80") && !saved.contains("dir") && !saved.contains("secret"), "{saved}");

        // The next run starts from them: its first requests to the host are held to the cap.
        let after = leaked();
        after.keep_in(file.clone(), now);
        assert_eq!(after.kept(&host, now).map(|kept| (kept.connections, kept.cap)), Some((Some(6), Some(2))));
        let t0 = Instant::now();
        let two: Vec<HostSlot> = (0..2).map(|_| after.try_acquire(&host, 32, t0).unwrap()).collect();
        assert!(after.try_acquire(&host, 32, t0).is_none());
        drop(two);
        // Once it expired as any cap learned does, a host serving more has none so low any more.
        let late = t0 + PROFILE_TTL;
        let _three: Vec<HostSlot> = (0..3).map(|_| after.try_acquire(&host, 32, late).unwrap()).collect();
        after.accepted(&host, false, late);
        assert_eq!(after.kept(&host, now).map(|kept| (kept.connections, kept.cap)), Some((Some(6), None)));

        // Thirty days after it was learned, nothing is kept.
        let later = now + KEPT_FOR.as_secs();
        assert_eq!(after.kept(&host, later), None);
        let reloaded = leaked();
        reloaded.keep_in(file, later);
        assert!(reloaded.kept.lock().hosts.is_empty());
        let many = (0..KEEP_KEPT + 10).map(|n| (format!("http://h{n}.example:80"), Kept { connections: Some(4), at: now + n as u64, ..Kept::default() }));
        let mut memory = Memory { hosts: many.collect(), ..Memory::default() };
        memory.prune(now);
        assert_eq!(memory.hosts.len(), KEEP_KEPT, "those learned about last");
        assert!(!memory.hosts.contains_key("http://h9.example:80") && memory.hosts.contains_key("http://h10.example:80"));
    }

    /// What a host is like outlives the run, each fact for an age of its own from when it was
    /// learned: the connections it served when it refused one more, whether it honours ranges, how
    /// long a connection takes to start and what it holds one to, for thirty days. A refusal
    /// itself keeps nothing more. A new run's first look at the host starts from it.
    #[test]
    fn test_what_a_host_is_like_is_kept_across_restarts_each_for_its_own_age() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hosts.json");
        let (host, now) = (key("https://recipe.example/f.bin"), unix_now());
        let before = leaked();
        before.keep_in(file.clone(), now);
        let alone = before.try_acquire(&host, 32, Instant::now()).unwrap();
        alone.throttled(Duration::ZERO);
        drop(alone);
        assert_eq!(before.kept(&host, now), None, "refused with nothing else open, it was only busy");
        let three: Vec<HostSlot> = (0..3).map(|_| answered(before.try_acquire(&host, 32, Instant::now()).unwrap())).collect();
        three[2].throttled(Duration::from_secs(3));
        drop(three);
        let seen = HostProfile { accepts_ranges: Some(true), setup_time: Some(Duration::from_millis(120)), capped_per_connection: Some(true), connection_rate: Some(2e6), ..Default::default() };
        before.record(&host, seen, Instant::now());
        before.save(now);
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(!saved.contains("limited") && !saved.contains("refused"), "{saved}");

        let after = leaked();
        after.keep_in(file.clone(), now);
        assert_eq!(after.kept(&host, now).map(|kept| kept.limited), Some(None));
        let first = after.profile(&host, Instant::now());
        assert_eq!(first, HostProfile { connection_cap: Some(2), ..seen });
        // Seen the same again later, nothing changes: the file is not written again, and each fact
        // keeps the age it had.
        let later = now + 3600;
        after.record(&host, seen, Instant::now());
        assert!(!after.kept.lock().changed, "nothing new");
        assert_eq!(after.kept(&host, later).and_then(|kept| kept.ranges), Some((true, now)));
        // Each is found out again once its age is over.
        assert_eq!(after.kept(&host, now + KEPT_FOR.as_secs()), None);
    }

    /// What the file holds is held to what a run keeps: a host is kept as limiting requests no
    /// longer than a day from now (after a clock set wrong, or an edit), and a fact past its age
    /// leaves the file, also while another of the host's is still trusted.
    #[test]
    fn test_what_the_file_holds_is_held_to_its_age() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hosts.json");
        let now = unix_now();
        let stale = now - KEPT_FOR.as_secs() - 1;
        let kept = Kept { limited: Some((u64::MAX, u64::MAX)), ranges: Some((true, stale)), speed: Some((1e6, now)), at: now, ..Kept::default() };
        let origin = key("https://clock.example/f.bin").origin().unwrap();
        std::fs::write(&file, serde_json::to_vec(&HashMap::from([(origin.clone(), kept)])).unwrap()).unwrap();
        let hosts = leaked();
        hosts.keep_in(file.clone(), now);
        let (until, hours) = hosts.kept(&key("https://clock.example/f.bin"), now).and_then(|kept| kept.limited).unwrap();
        assert!(until <= now + MOST_LIMITED_HOURS * 3600 && hours == MOST_LIMITED_HOURS, "{until} {hours}");
        hosts.save(now);
        let saved: HashMap<String, Kept> = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!((saved[&origin].ranges, saved[&origin].speed), (None, Some((1e6, now))), "the stale fact left the file");
    }

    /// One refusal keeps nothing; a download refused after another was, once the host's rest was
    /// over, keeps the host as limiting requests for an hour. When that is up, a download that
    /// widens and is refused keeps it twice as long, up to a day; one that widens and is refused
    /// nothing takes the mark away.
    #[test]
    fn test_a_host_is_kept_as_limiting_requests_only_once_refused_again_and_that_wears_off() {
        let url = Url::parse("https://wears-off.example/f.bin").unwrap();
        let limited = || HOSTS.kept(&HostKey::of(&url), unix_now()).and_then(|kept| kept.limited);
        refused_in_download(&url, false);
        assert!(!kept_limited(&url) && limited().is_none(), "one refusal keeps nothing");
        refused_in_download(&url, true);
        let (until, hours) = limited().unwrap();
        assert!(kept_limited(&url) && hours == 1 && until.abs_diff(unix_now() + 3600) <= 2, "{until} {hours}");
        widened_unrefused(&url);
        refused_in_download(&url, true);
        assert_eq!(limited().map(|(_, hours)| hours), Some(1), "neither while it holds");
        // Its time up, a refused wider try doubles it; up to a day.
        let expire = || HOSTS.keep(&HostKey::of(&url), unix_now(), |kept| kept.limited = kept.limited.map(|(_, hours)| (unix_now() - 1, hours)));
        for hours in [2, 4, 8, 16, 24, 24] {
            expire();
            assert!(!kept_limited(&url));
            refused_in_download(&url, false);
            assert_eq!(limited().map(|(_, hours)| hours), Some(hours));
        }
        expire();
        widened_unrefused(&url);
        assert!(limited().is_none() && !limits_requests(&url), "gone once a wider download went through");
    }

    /// What a download settled on is kept by host, the speed that held it among it; the line's
    /// speed once two downloads went about as fast, and only with a file to keep it in.
    #[test]
    fn test_a_hosts_limit_and_the_line_are_kept_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hosts.json");
        let (host, now) = (key("https://capped.example/a/b.iso?sig=1"), unix_now());
        let hosts = leaked();
        hosts.record_line(9e6, now);
        assert_eq!(hosts.kept_as(LINE, now), None, "no file, no user");
        hosts.keep_in(file.clone(), now);
        hosts.keep(&host, now, |kept| (kept.connections, kept.limit, kept.protocol) = (Some(2), Some(4e6), Some(Protocol::H2)));
        for (speed, line) in [(9e6, None), (5e6, None), (9.5e6, Some(9.5e6)), (20e6, None), (19e6, Some(20e6))] {
            hosts.record_line(speed, now);
            assert_eq!(hosts.line(now), line, "after {speed}");
        }
        hosts.save(now);
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(saved.contains("\"line\"") && saved.contains("https://capped.example:443") && !saved.contains("sig"), "{saved}");
        assert!(saved.contains("\"protocol\":\"h2\""), "{saved}");

        let after = leaked();
        after.keep_in(file, now);
        let kept = after.kept(&host, now).map(|kept| (kept.connections, kept.limit, kept.protocol));
        assert_eq!(kept, Some((Some(2), Some(4e6), Some(Protocol::H2))));
        assert_eq!(after.line(now), Some(20e6));
        assert_eq!(after.line(now + KEPT_FOR.as_secs()), None, "forgotten as the rest is");
    }

    #[test]
    fn test_the_process_wide_profile() {
        let url = Url::parse("https://profile-test.hosts.example/file.iso").unwrap();
        record(&url, HostProfile { connection_cap: Some(6), ..Default::default() });
        let seen = profile(&Url::parse("https://profile-test.hosts.example/other.iso").unwrap());
        assert_eq!(seen.connection_cap, Some(6));
    }

    #[test]
    fn test_archive_org_default_connection_cap() {
        let direct = HostKey::of(&Url::parse("https://archive.org/download/item/file.mp4").unwrap());
        assert_eq!(direct.default_connection_cap(), Some(4));
        let server = HostKey::of(&Url::parse("https://ia800100.us.archive.org/items/item/file.mp4").unwrap());
        assert_eq!(server.default_connection_cap(), Some(4));
    }
}
