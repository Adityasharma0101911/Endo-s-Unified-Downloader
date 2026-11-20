//! What the system says of the network, so that a download waits for a network that went away
//! instead of failing. Only the system is asked, never another host: on Windows its connectivity
//! hint (no network, a local network without internet, internet), which also tells at once when
//! it changes; elsewhere whether an interface that is up has a gateway. Where it cannot be told,
//! the network counts as there, and downloads fail as they always did.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use url::Url;

/// How long a download waits for the network before its tries count again, and it fails as
/// before: long enough for a router's restart, a provider's outage or a train's tunnel; past it, the
/// download no longer holds its place for a network that is not coming back.
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(2 * 3600);
/// How long the system's answer is kept before it is asked again.
const FRESH: Duration = Duration::from_secs(1);
/// How often a download waiting for the network asks the system, where it is not told of changes.
const POLL: Duration = Duration::from_secs(2);
/// The shortest and longest wait before a download waiting for the network tries again anyway:
/// half of how long the network has been gone, within these.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MOST: Duration = Duration::from_secs(60);

/// What the system says this computer reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// It cannot tell.
    Unknown,
    /// No network at all.
    Nothing,
    /// A local network, without the internet.
    Local,
    Internet,
}

struct State {
    /// The last answer, and when it was asked.
    seen: Option<(Instant, Reach)>,
    /// Since when the network has been gone.
    gone_since: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State { seen: None, gone_since: None });

/// Wakes the downloads waiting for the network when the system says it changed.
static CHANGED: LazyLock<tokio::sync::Notify> = LazyLock::new(|| {
    imp::watch();
    tokio::sync::Notify::new()
});

/// What the system says this computer reaches now (asked at most once a second).
pub fn reach() -> Reach {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let reach = match state.seen {
        Some((at, reach)) if now.duration_since(at) < FRESH => reach,
        _ => {
            let reach = imp::reach();
            state.seen = Some((now, reach));
            reach
        }
    };
    match reach {
        Reach::Nothing | Reach::Local => {
            state.gone_since.get_or_insert(now);
        }
        _ => state.gone_since = None,
    }
    reach
}

/// The system says the internet cannot be reached now: there is no network, or only a local one.
pub fn is_offline() -> bool {
    matches!(reach(), Reach::Nothing | Reach::Local)
}

/// Whether a request to `url` that failed for want of an answer is to wait for the network rather
/// than count as a try: the system says the network is gone (for a host on this computer or a
/// local network, only when there is no network at all), and has said so for less than
/// [`GIVE_UP_AFTER`].
pub fn waits(url: &Url) -> bool {
    #[cfg(test)]
    if let Some(gone) = fake::gone(url) {
        return gone;
    }
    let gone = match reach() {
        Reach::Nothing => true,
        Reach::Local => !local(url),
        _ => false,
    };
    let since = STATE.lock().unwrap_or_else(|e| e.into_inner()).gone_since;
    gone && since.is_some_and(|since| since.elapsed() < GIVE_UP_AFTER)
}

/// Resolves once the system says the network is back for `url` (at once where it tells of
/// changes, else checked every few seconds), or once the download is to try again anyway: after
/// half as long as the network has been gone, from 5 s up to a minute.
pub async fn back(url: &Url) {
    let since = STATE.lock().unwrap_or_else(|e| e.into_inner()).gone_since;
    let wait = (since.map_or(Duration::ZERO, |since| since.elapsed()) / 2).clamp(RETRY_FIRST, RETRY_MOST);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let changed = CHANGED.notified();
        tokio::select! {
            _ = changed => {}
            _ = tokio::time::sleep(POLL) => {}
            _ = tokio::time::sleep_until(deadline) => return,
        }
        if !waits(url) {
            return;
        }
    }
}

/// How many times the app's queue starts again by itself a download that failed for a network
/// reason (see [`failed_for_network`]).
pub const RERUNS: u32 = 3;

/// How long the queue waits, once the network is there, before it starts such a download again
/// the `nth` time (from 1): a minute, then 5, then 25.
pub fn rerun_after(nth: u32) -> Duration {
    Duration::from_secs(60 * 5u64.pow(nth.clamp(1, 4) - 1))
}

/// Whether a download that failed with `error` failed for a network reason, which the queue may
/// start again by itself: no answer, a connection cut or refused, a name that did not resolve.
/// Never an answer that says no (401, 403, 404, 410), a checksum that does not match, a full or
/// failing disk, or a download the user stopped.
pub fn failed_for_network(error: &str) -> bool {
    const NOT: [&str; 9] = ["checksum", "hash", "disk", "space", "cancel", "forbidden", "not found", "unauthorized", "gone"];
    const NETWORK: [&str; 14] = [
        "error sending request",
        "dns error",
        "failed to lookup",
        "connection refused",
        "connection reset",
        "connection closed",
        "connection aborted",
        "timed out",
        "no response within",
        "stalled: no data",
        "read error at offset",
        "unreachable",
        "broken pipe",
        "unexpected eof",
    ];
    let lower = error.to_ascii_lowercase();
    !crate::relink::refused(error) && !NOT.iter().any(|word| lower.contains(word)) && NETWORK.iter().any(|word| lower.contains(word))
}

/// A host that needs no internet: this computer, or one on a local network (a private, link-local
/// or unique local address, `localhost`, a `.local` name).
fn local(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80,
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost") || name.ends_with(".local")
        }
        None => true,
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{BOOLEAN, HANDLE, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

    use super::Reach;

    /// NL_NETWORK_CONNECTIVITY_HINT.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Hint {
        pub level: i32,
        pub cost: i32,
        pub approaching_data_limit: BOOLEAN,
        pub over_data_limit: BOOLEAN,
        pub roaming: BOOLEAN,
    }

    type GetHint = unsafe extern "system" fn(*mut Hint) -> u32;
    type Callback = unsafe extern "system" fn(*const c_void, Hint);
    type NotifyHint = unsafe extern "system" fn(Option<Callback>, *const c_void, BOOLEAN, *mut HANDLE) -> u32;

    /// iphlpapi.dll's `name`: these came with Windows 10 2004, so they are looked up rather than
    /// linked, and the app still starts on an older build.
    fn function(name: &'static [u8]) -> Option<unsafe extern "system" fn() -> isize> {
        static LIBRARY: OnceLock<usize> = OnceLock::new();
        let library = *LIBRARY.get_or_init(|| {
            let wide: Vec<u16> = "iphlpapi.dll".encode_utf16().chain([0]).collect();
            // SAFETY: a NUL-terminated name, loaded from System32 only, kept for the process.
            unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) as usize }
        });
        if library == 0 {
            return None;
        }
        // SAFETY: a NUL-terminated name in a module loaded for good.
        unsafe { GetProcAddress(library as HMODULE, name.as_ptr()) }
    }

    /// The system's connectivity hint, where it has one.
    pub fn hint() -> Option<Hint> {
        static GET: OnceLock<Option<GetHint>> = OnceLock::new();
        // SAFETY: the function the name stands for has this signature (netioapi.h).
        let get = (*GET.get_or_init(|| function(b"GetNetworkConnectivityHint\0").map(|f| unsafe { std::mem::transmute::<_, GetHint>(f) })))?;
        let mut hint = Hint::default();
        // SAFETY: fills the struct it is given.
        (unsafe { get(&mut hint) } == 0).then_some(hint)
    }

    pub fn reach() -> Reach {
        match hint().map(|hint| hint.level) {
            // NetworkConnectivityLevelHintNone, LocalAccess, InternetAccess, ConstrainedInternetAccess.
            Some(1) => Reach::Nothing,
            Some(2) => Reach::Local,
            Some(3 | 4) => Reach::Internet,
            Some(_) => Reach::Unknown,
            None => super::interfaces_reach(),
        }
    }

    /// Wakes the waiting downloads whenever the system's connectivity changes, for the rest of
    /// the process.
    pub fn watch() {
        unsafe extern "system" fn changed(_: *const c_void, _: Hint) {
            super::STATE.lock().unwrap_or_else(|e| e.into_inner()).seen = None;
            super::CHANGED.notify_waiters();
        }
        // SAFETY: the function the name stands for has this signature (netioapi.h).
        let Some(notify) = function(b"NotifyNetworkConnectivityHintChange\0").map(|f| unsafe { std::mem::transmute::<_, NotifyHint>(f) }) else { return };
        let mut handle: HANDLE = std::ptr::null_mut();
        // SAFETY: a callback that lives as long as the process, no context, and the registration
        // kept for the process (never cancelled).
        if unsafe { notify(Some(changed), std::ptr::null(), 0, &mut handle) } != 0 {
            tracing::debug!("The system will not tell when the network changes; it is asked every few seconds");
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn reach() -> super::Reach {
        super::interfaces_reach()
    }

    /// The system is asked every few seconds instead.
    pub fn watch() {}
}

/// The network as the interfaces tell it: none when no interface but loopback is up with a
/// gateway or an address that leaves the computer (Wi-Fi off, the cable out); else it cannot
/// tell whether the internet is there. A bridge for virtual machines counts as a network, so
/// there the network never counts as gone.
fn interfaces_reach() -> Reach {
    let interfaces = netdev::get_interfaces();
    let connected = |i: &netdev::Interface| i.is_up() && !i.is_loopback() && (i.gateway.is_some() || i.ip_addrs().into_iter().any(crate::netif::bindable));
    match interfaces.iter().any(connected) || interfaces.is_empty() {
        true => Reach::Unknown,
        false => Reach::Nothing,
    }
}

/// A network that goes away in tests, for the requests to one port alone.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Mutex;

    use url::Url;

    static GONE: Mutex<Vec<u16>> = Mutex::new(Vec::new());

    /// The network is gone for requests to `port` (true) or back (false).
    pub fn set(port: u16, gone: bool) {
        let mut ports = GONE.lock().unwrap();
        ports.retain(|p| *p != port);
        if gone {
            ports.push(port);
        }
        super::CHANGED.notify_waiters();
    }

    /// Whether the network is gone for `url`, when its port is one a test watches.
    pub(super) fn gone(url: &Url) -> Option<bool> {
        GONE.lock().unwrap().contains(&url.port()?).then_some(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Hosts on this computer or a local network need no internet; others do.
    #[test]
    fn local_hosts_need_no_internet() {
        for link in ["http://127.0.0.1:8080/a", "http://192.168.1.4/a", "http://10.0.0.2/a", "http://[::1]/a", "http://[fd00::4]/a", "http://nas.local/a", "http://localhost/a"] {
            assert!(local(&Url::parse(link).unwrap()), "{link}");
        }
        for link in ["https://example.com/a", "http://8.8.8.8/a", "http://[2001:db8::1]/a"] {
            assert!(!local(&Url::parse(link).unwrap()), "{link}");
        }
    }

    /// Failures for want of an answer are the network's; refusals, checksums, disks and the
    /// user's stop never are. The queue waits longer before each new start.
    #[test]
    fn network_failures_are_told_from_the_rest() {
        for error in [
            "Download failed: chunk 3 failed 9 times in a row without progress: request failed: error sending request for url (https://a.example/f.iso)",
            "Download failed after 9 attempts: no response within 30s",
            "Failed to probe https://a.example/f.iso: dns error: failed to lookup address information",
            "Download failed: chunk 0 failed 9 times in a row without progress: stalled: no data for 5s at offset 1048576",
            "tcp connect error: A socket operation was attempted to an unreachable network. (os error 10051)",
        ] {
            assert!(failed_for_network(error), "{error}");
        }
        for error in [
            "Download failed: every mirror failed; last error: HTTP 404 Not Found",
            "HTTP 403 Forbidden: request failed: error sending request",
            "Checksum mismatch: expected sha256:ab, got sha256:cd",
            "Disk write error: There is not enough space on the disk. (os error 112)",
            crate::engine::CANCELLED,
            "The remote file changed while downloading",
        ] {
            assert!(!failed_for_network(error), "{error}");
        }
        assert_eq!([1, 2, 3].map(rerun_after), [60, 300, 1500].map(Duration::from_secs));
    }

    /// Whatever the system says of this computer, asking it twice within a second asks it once,
    /// and a test's port is gone only while it says so.
    #[test]
    fn the_system_is_asked_and_tests_take_one_port_offline() {
        let first = reach();
        assert_eq!(reach(), first);
        let url = Url::parse("http://127.0.0.1:1/a").unwrap();
        fake::set(1, true);
        assert!(waits(&url));
        fake::set(1, false);
        assert_eq!(fake::gone(&url), None);
    }

    /// A file server on this computer that answers HEAD and its first GET, then drops every
    /// connection while `down`: as if the network went away once the download had started.
    async fn flaky_server(body: Arc<Vec<u8>>, down: Arc<AtomicBool>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let gets = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (body, down, gets) = (Arc::clone(&body), Arc::clone(&down), Arc::clone(&gets));
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    let head = text.starts_with("head ");
                    if !head && gets.fetch_add(1, Ordering::SeqCst) > 0 && down.load(Ordering::SeqCst) {
                        return;
                    }
                    let last = body.len() - 1;
                    let range = text.lines().find_map(|l| l.strip_prefix("range: bytes=")).and_then(|r| {
                        let (from, to) = r.trim().split_once('-')?;
                        Some((from.parse::<usize>().ok()?, to.parse::<usize>().map_or(last, |to| to.min(last))))
                    });
                    let (from, to) = range.unwrap_or((0, last));
                    let (status, part) = match range {
                        Some(_) => ("206 Partial Content", format!("Content-Range: bytes {from}-{to}/{}\r\n", body.len())),
                        None => ("200 OK", String::new()),
                    };
                    let length = to + 1 - from;
                    let answer = format!("HTTP/1.1 {status}\r\n{part}Content-Length: {length}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n");
                    let _ = socket.write_all(answer.as_bytes()).await;
                    if !head {
                        let _ = socket.write_all(&body[from..=to]).await;
                    }
                });
            }
        });
        port
    }

    /// A download whose network goes away once it has started waits for it, its one try allowed
    /// untouched, and finishes once the network is back; with the network there, the same server
    /// going silent fails it as before.
    #[tokio::test]
    async fn a_download_waits_for_the_network_to_come_back() {
        let body: Arc<Vec<u8>> = Arc::new((0..4u32 << 20).map(|i| (i % 251) as u8).collect());
        let dir = tempfile::tempdir().unwrap();
        let fetch = |port: u16| {
            let folder = dir.path().join(port.to_string());
            std::fs::create_dir_all(&folder).unwrap();
            let options = crate::DownloadOptions {
                output_path: Some(folder),
                num_connections: 4,
                max_retries: 1,
                stall_timeout_secs: 5,
                remote_checksum: false,
                ..Default::default()
            };
            crate::DownloadEngine::new(vec![Url::parse(&format!("http://127.0.0.1:{port}/file.bin")).unwrap()], options)
        };
        let limit = Duration::from_secs(60);

        let down = Arc::new(AtomicBool::new(true));
        let port = flaky_server(Arc::clone(&body), Arc::clone(&down)).await;
        let failed = tokio::time::timeout(limit, fetch(port).run(None)).await.unwrap();
        assert!(failed.is_err(), "the network there, the server's silence fails it: {failed:?}");

        let port = flaky_server(Arc::clone(&body), Arc::clone(&down)).await;
        fake::set(port, true);
        let back = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            down.store(false, Ordering::SeqCst);
            fake::set(port, false);
        });
        let started = Instant::now();
        let path = tokio::time::timeout(limit, fetch(port).run(None)).await.unwrap().expect("waited for the network");
        back.await.unwrap();
        assert!(started.elapsed() >= Duration::from_secs(3), "it waited: {:?}", started.elapsed());
        assert!(std::fs::read(path).unwrap() == *body);
    }
}
