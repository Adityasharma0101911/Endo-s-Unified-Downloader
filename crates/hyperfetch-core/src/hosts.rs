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

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
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

static HOSTS: LazyLock<Hosts> = LazyLock::new(Hosts::default);

/// A host as this module tells hosts apart: scheme, host and port.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostKey {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl HostKey {
    pub fn of(url: &Url) -> Self {
        Self {
            scheme: url.scheme().to_string(),
            host: url.host_str().unwrap_or_default().to_string(),
            port: url.port_or_known_default(),
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

/// Records what `url`'s host was seen to do: each fact given replaces the one known, and is
/// trusted for `PROFILE_TTL` from now. Facts left `None` are kept as they are.
pub fn record(url: &Url, seen: HostProfile) {
    HOSTS.record(&HostKey::of(url), seen, Instant::now())
}

/// Waits until a request to `url`'s host may be opened under `limit` (see the module docs) and
/// returns its slot. Waiting is no failure: nothing else is held meanwhile, and dropping the
/// future gives up the wait. Never wait for a slot while holding another.
pub async fn acquire(url: &Url, limit: usize) -> HostSlot {
    HOSTS.acquire(HostKey::of(url), limit).await
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

/// A slot for a request to `url`'s host under `limit`, if one is free now.
pub fn try_acquire(url: &Url, limit: usize) -> Option<HostSlot> {
    HOSTS.try_acquire(&HostKey::of(url), limit, Instant::now())
}

/// One request open to a host, counted against the host's budget until dropped.
#[must_use = "the slot is given back when dropped"]
pub struct HostSlot {
    hosts: &'static Hosts,
    key: HostKey,
    limit: usize,
    opens_connection: bool,
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

    /// The host refused this request with 429/503. With other requests to it open at the time, it
    /// serves no more than those at once: every download to it is held to that, for
    /// `PROFILE_TTL`. With none, the host was just busy.
    pub fn throttled(&self) {
        self.hosts.throttled(&self.key, Instant::now())
    }

    /// The host accepted this request. Serving as many as the cap it was seen to enforce, it is
    /// offered one more, at most once per `CAP_PROBE_INTERVAL` after the cap was learned or last
    /// raised: a host that means its cap refuses each request offered past it.
    pub fn accepted(&self) {
        self.hosts.accepted(&self.key, Instant::now())
    }
}

impl Drop for HostSlot {
    fn drop(&mut self) {
        self.hosts.release(&self.key, self.limit, Instant::now())
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
    }

    /// The smallest limit among the requests open or waiting.
    fn strictest(&self) -> Option<usize> {
        [self.held.keys().next(), self.waiting.keys().next()].into_iter().flatten().min().copied()
    }

    /// Requests the host may have open once one under `limit` joins them.
    fn allowance(&self, limit: usize, now: Instant) -> usize {
        let own = (limit > 0).then_some(limit);
        [own, self.strictest(), fresh(self.facts.connection_cap, now)]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(usize::MAX)
    }
}

#[derive(Debug, Default)]
struct Hosts {
    entries: Mutex<HashMap<HostKey, Entry>>,
    /// Woken whenever a host may take more requests: a slot was freed or a cap rose.
    freed: Notify,
}

impl Hosts {
    fn profile(&self, key: &HostKey, now: Instant) -> HostProfile {
        self.entries.lock().get(key).map_or_else(HostProfile::default, |entry| entry.facts.profile(now))
    }

    fn record(&self, key: &HostKey, seen: HostProfile, now: Instant) {
        {
            let mut entries = self.entries.lock();
            entries.entry(key.clone()).or_default().facts.record(seen, now);
            forget_idle(&mut entries, now);
        }
        if seen.connection_cap.is_some() {
            self.freed.notify_waiters();
        }
    }

    fn try_acquire(&'static self, key: &HostKey, limit: usize, now: Instant) -> Option<HostSlot> {
        let mut entries = self.entries.lock();
        let entry = entries.entry(key.clone()).or_default();
        if entry.open >= entry.allowance(limit, now) {
            return None;
        }
        entry.open += 1;
        add_limit(&mut entry.held, limit);
        #[cfg(test)]
        {
            entry.peak = entry.peak.max(entry.open);
        }
        entry.idle.retain(|&at| now.saturating_duration_since(at) < POOL_IDLE);
        let opens_connection = entry.idle.pop().is_none();
        Some(HostSlot { hosts: self, key: key.clone(), limit, opens_connection })
    }

    async fn acquire(&'static self, key: HostKey, limit: usize) -> HostSlot {
        // A lenient download re-taking slots as its requests end would otherwise keep a strict
        // one waiting until it is nearly done: the strict limit counts from now on.
        let _waiting = {
            add_limit(&mut self.entries.lock().entry(key.clone()).or_default().waiting, limit);
            Waiting { hosts: self, key: key.clone(), limit }
        };
        loop {
            // Registered before looking, so a slot freed in between still wakes us.
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if let Some(slot) = self.try_acquire(&key, limit, Instant::now()) {
                return slot;
            }
            freed.await;
        }
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

    fn release(&self, key: &HostKey, limit: usize, now: Instant) {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.get_mut(key) {
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

    fn throttled(&self, key: &HostKey, now: Instant) {
        if let Some(entry) = self.entries.lock().get_mut(key) {
            // The refused request still holds its slot.
            let others = entry.open.saturating_sub(1);
            if others > 0 {
                entry.facts.connection_cap = Some((others, now));
            }
        }
    }

    fn accepted(&self, key: &HostKey, now: Instant) {
        let raised = self.entries.lock().get_mut(key).is_some_and(|entry| {
            match (entry.facts.connection_cap, fresh(entry.facts.connection_cap, now)) {
                (Some((cap, learned)), Some(_)) if entry.open >= cap => {
                    let since = entry.cap_raised.map_or(learned, |raised| raised.max(learned));
                    if now.saturating_duration_since(since) < CAP_PROBE_INTERVAL {
                        return false;
                    }
                    // The cap keeps the time it was learned at, so it still expires.
                    entry.facts.connection_cap = Some((cap + 1, learned));
                    entry.cap_raised = Some(now);
                    true
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
        alone.throttled();
        drop(alone);
        assert_eq!(hosts.profile(&host, t0).connection_cap, None);
        // Refused with two others open, it serves two at once.
        let three: Vec<HostSlot> = (0..3).map(|_| hosts.try_acquire(&host, 32, t0).unwrap()).collect();
        three[2].throttled();
        drop(three);
        assert_eq!(hosts.profile(&host, Instant::now()).connection_cap, Some(2));

        // A later download from the host starts at that cap, whatever its own limit allows.
        let now = Instant::now();
        let mut later: Vec<HostSlot> = (0..2).map(|_| hosts.try_acquire(&host, 32, now).unwrap()).collect();
        assert!(hosts.try_acquire(&host, 32, now).is_none());
        // Served in full at the cap, the host is offered one more request, but not every time:
        // each offer a capped host refuses costs a request.
        hosts.accepted(&host, now);
        assert!(hosts.try_acquire(&host, 32, now).is_none(), "the cap was just learned");
        let probe = now + CAP_PROBE_INTERVAL;
        hosts.accepted(&host, probe);
        later.push(hosts.try_acquire(&host, 32, probe).unwrap());
        assert_eq!(hosts.profile(&host, probe).connection_cap, Some(3));
        hosts.accepted(&host, probe);
        assert!(hosts.try_acquire(&host, 32, probe).is_none(), "the cap was just raised");
        // Refused again: back to what it served, and the next offer waits a full interval from then.
        let refused = probe + Duration::from_secs(5);
        hosts.throttled(&host, refused);
        drop(later.pop());
        hosts.accepted(&host, probe + CAP_PROBE_INTERVAL);
        assert_eq!(hosts.profile(&host, refused).connection_cap, Some(2));
        hosts.accepted(&host, refused + CAP_PROBE_INTERVAL);
        assert_eq!(hosts.profile(&host, refused).connection_cap, Some(3));
        // Ten minutes after it was seen, the cap is forgotten.
        assert!(hosts.try_acquire(&host, 32, refused + PROFILE_TTL).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_cap_of_zero_is_no_cap() {
        let (hosts, host) = (leaked(), key("http://zero.example/"));
        hosts.record(&host, HostProfile { connection_cap: Some(0), ..Default::default() }, Instant::now());
        assert_eq!(hosts.profile(&host, Instant::now()).connection_cap, None);
        let slot = tokio::time::timeout(Duration::from_secs(60), hosts.acquire(host.clone(), 4)).await;
        assert!(slot.is_ok(), "a host always takes one request");
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
    fn test_the_process_wide_profile() {
        let url = Url::parse("https://profile-test.hosts.example/file.iso").unwrap();
        record(&url, HostProfile { connection_cap: Some(6), ..Default::default() });
        let seen = profile(&Url::parse("https://profile-test.hosts.example/other.iso").unwrap());
        assert_eq!(seen.connection_cap, Some(6));
    }
}
