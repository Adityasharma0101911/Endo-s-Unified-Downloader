//! Name look-ups and connections to a host's addresses. A name the system resolved is kept for
//! `NAME_KEPT`, for the downloads' HTTP client (see [`KeptNames`]) and the protocols that open
//! their own TCP connections (FTP, SFTP, news) alike. Those race a host's addresses as the HTTP
//! client races its two IP versions (hyper-util's `HttpConnector`): the families take turns, and
//! the next address is tried `NEXT_ADDRESS` after the one before began, at once when every one
//! before failed, the first to connect winning. (The HTTP client itself tries the addresses of one
//! IP version one after another, each with an even share of the connect timeout; reqwest does not
//! let that be set.)

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};
use parking_lot::Mutex;

/// How long a name the system resolved is kept: each connection of a download, and each download
/// of a batch, would ask again, and a host's addresses rarely change within a minute.
pub(crate) const NAME_KEPT: Duration = Duration::from_secs(60);
/// How long an attempt to connect goes on alone before the next address is tried beside it: as
/// long as the HTTP client waits before it tries the other IP version.
pub(crate) const NEXT_ADDRESS: Duration = Duration::from_millis(300);
const MAX_NAMES: usize = 1024;

static NAMES: LazyLock<Mutex<HashMap<String, (Instant, Vec<IpAddr>)>>> = LazyLock::new(Default::default);

/// `host`'s addresses at `port`, in the order the system gives them; what it gave is kept for
/// `NAME_KEPT` (a failure is not).
pub(crate) async fn lookup(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    let key = host.to_ascii_lowercase();
    if let Some((_, ips)) = NAMES.lock().get(&key).filter(|(at, _)| at.elapsed() < NAME_KEPT) {
        return Ok(ips.iter().map(|&ip| SocketAddr::new(ip, port)).collect());
    }
    let found: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    if !found.is_empty() {
        let mut names = NAMES.lock();
        if names.len() >= MAX_NAMES {
            names.clear();
        }
        names.insert(key, (Instant::now(), found.iter().map(SocketAddr::ip).collect()));
    }
    Ok(found)
}

/// Keeps `ips` as `host`'s answer, as a look-up would.
#[cfg(test)]
pub(crate) fn keep_name(host: &str, ips: Vec<IpAddr>) {
    NAMES.lock().insert(host.to_ascii_lowercase(), (Instant::now(), ips));
}

/// The downloads' HTTP clients' resolver: the system's, its answers kept (see [`lookup`]).
pub(crate) struct KeptNames;

impl reqwest::dns::Resolve for KeptNames {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            // Port 0: the client puts the link's in.
            let found = lookup(&host, 0).await?;
            Ok(Box::new(found.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// `addrs` with the IP versions taking turns, the first address's first (RFC 8305).
fn interleaved(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let v4 = addrs.first().is_some_and(SocketAddr::is_ipv4);
    let (first, other): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|a| a.is_ipv4() == v4);
    let (mut out, mut other) = (Vec::with_capacity(first.len() + other.len()), other.into_iter());
    for addr in first {
        out.push(addr);
        out.extend(other.next());
    }
    out.extend(other);
    out
}

/// What `connect` makes of the first of `addrs` it connects to, the IP versions taking turns: the
/// next address is tried `NEXT_ADDRESS` after the one before began, or at once when every one
/// tried failed, and the attempts still going are dropped once one connects. The last error when
/// none does; `None` without addresses.
pub(crate) async fn race<T, E, F, Fut>(addrs: Vec<SocketAddr>, connect: F) -> Result<T, Option<E>>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut waiting = interleaved(addrs).into_iter();
    let mut trying = FuturesUnordered::new();
    let mut failed = None;
    let mut next_at = tokio::time::Instant::now();
    loop {
        if trying.is_empty() || tokio::time::Instant::now() >= next_at {
            match waiting.next() {
                Some(addr) => {
                    trying.push(connect(addr));
                    next_at = tokio::time::Instant::now() + NEXT_ADDRESS;
                }
                None if trying.is_empty() => return Err(failed),
                None => {}
            }
        }
        tokio::select! {
            Some(done) = trying.next() => match done {
                Ok(conn) => return Ok(conn),
                Err(e) => failed = Some(e),
            },
            () = tokio::time::sleep_until(next_at), if waiting.len() > 0 => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(addr: &str) -> SocketAddr {
        addr.parse().unwrap()
    }

    #[test]
    fn the_ip_versions_take_turns_the_first_addresss_first() {
        let addrs = ["10.0.0.1:80", "10.0.0.2:80", "10.0.0.3:80", "[::1]:80", "[::2]:80"].map(at).to_vec();
        let expected = ["10.0.0.1:80", "[::1]:80", "10.0.0.2:80", "[::2]:80", "10.0.0.3:80"].map(at).to_vec();
        assert_eq!(interleaved(addrs), expected);
        let addrs = ["[::1]:80", "10.0.0.1:80", "10.0.0.2:80"].map(at).to_vec();
        assert_eq!(interleaved(addrs), ["[::1]:80", "10.0.0.1:80", "10.0.0.2:80"].map(at).to_vec());
    }

    /// One that never answers holds the next back `NEXT_ADDRESS`, not its whole timeout; one that
    /// fails at once not at all; with none answering, the last error. On the test's own clock.
    #[tokio::test(start_paused = true)]
    async fn the_next_address_is_tried_beside_one_that_does_not_answer() {
        let (dead, refusing, live) = (at("10.0.0.1:80"), at("10.0.0.2:80"), at("10.0.0.3:80"));
        let connect = |addr: SocketAddr| async move {
            match addr {
                a if a == dead => std::future::pending().await,
                a if a == refusing => Err(format!("{a} refused")),
                a => Ok(a),
            }
        };
        let started = tokio::time::Instant::now();
        assert_eq!(race(vec![dead, live], connect).await, Ok(live));
        assert_eq!(started.elapsed(), NEXT_ADDRESS);
        let started = tokio::time::Instant::now();
        assert_eq!(race(vec![refusing, live], connect).await, Ok(live));
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(race(vec![refusing, refusing], connect).await, Err(Some(format!("{refusing} refused"))));
        assert_eq!(race(Vec::new(), connect).await, Err(None));
    }

    #[tokio::test]
    async fn a_name_is_looked_up_once_a_minute() {
        let kept = || NAMES.lock().get("localhost").map(|(at, _)| *at);
        let first = lookup("LocalHost", 21).await.unwrap();
        let since = kept().unwrap();
        let again = lookup("localhost", 22).await.unwrap();
        assert_eq!(kept(), Some(since), "not asked again");
        assert_eq!(first.iter().map(SocketAddr::ip).collect::<Vec<_>>(), again.iter().map(SocketAddr::ip).collect::<Vec<_>>());
        assert!(first.iter().all(|a| a.port() == 21) && again.iter().all(|a| a.port() == 22));
        NAMES.lock().get_mut("localhost").unwrap().0 -= NAME_KEPT;
        lookup("localhost", 21).await.unwrap();
        assert!(kept().unwrap().elapsed() < NAME_KEPT, "asked again after a minute");
    }
}
