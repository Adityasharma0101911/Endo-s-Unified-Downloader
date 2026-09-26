//! What this process knows about the hosts it downloads from, shared by every download: what each
//! host was seen to do, so the next download from it can start from that instead of finding out
//! again.
//!
//! Hosts are told apart by scheme, host and port. A fact is trusted for `PROFILE_TTL` after it was
//! learned, each on its own: hosts change, and a fact nobody sees again is forgotten.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use url::Url;

/// How long a learned fact about a host is trusted.
pub const PROFILE_TTL: Duration = Duration::from_secs(10 * 60);
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
    /// Connections it serves at once before refusing more with 429/503.
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
        learn(&mut self.connection_cap, seen.connection_cap, now);
    }
}

#[derive(Debug, Default)]
struct Entry {
    facts: Facts,
}

impl Entry {
    /// Whether nothing would be lost by forgetting the host.
    fn is_idle(&self, now: Instant) -> bool {
        self.facts.profile(now) == HostProfile::default()
    }
}

#[derive(Debug, Default)]
struct Hosts {
    entries: Mutex<HashMap<HostKey, Entry>>,
}

impl Hosts {
    fn profile(&self, key: &HostKey, now: Instant) -> HostProfile {
        self.entries.lock().get(key).map_or_else(HostProfile::default, |entry| entry.facts.profile(now))
    }

    fn record(&self, key: &HostKey, seen: HostProfile, now: Instant) {
        let mut entries = self.entries.lock();
        entries.entry(key.clone()).or_default().facts.record(seen, now);
        forget_idle(&mut entries, now);
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

    #[test]
    fn test_the_process_wide_profile() {
        let url = Url::parse("https://profile-test.hosts.example/file.iso").unwrap();
        record(&url, HostProfile { connection_cap: Some(6), ..Default::default() });
        let seen = profile(&Url::parse("https://profile-test.hosts.example/other.iso").unwrap());
        assert_eq!(seen.connection_cap, Some(6));
    }
}
