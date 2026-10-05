//! BitTorrent swarm downloads (magnet links and .torrent files without HTTP web seeds), with
//! librqbit. One session serves the whole process, started with the port, UPnP and proxy
//! settings of the download that needs it; one that wants other settings gets a new session once
//! no torrent runs. The DHT keeps its state in the app's data folder.
//!
//! A cancelled download leaves the session (its files stay); running it again adds it back, and
//! librqbit checks the pieces already on disk before fetching the rest. A finished torrent seeds
//! until its ratio or time limit, then leaves the session too.

use std::io::Write;
use std::net::{Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use librqbit::dht::DhtPersistenceConfig;
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ConnectionOptions, DhtSessionConfig, ListenerOptions, ManagedTorrent,
    Session, SessionOptions, TorrentStats, TorrentStatsState,
};
use tokio::sync::{broadcast, watch, Mutex};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{DownloadOptions, EngineSnapshot, CANCELLED};

/// A torrent's swarm side of its progress (see `EngineSnapshot::torrent`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TorrentProgress {
    pub peers: usize,
    pub uploaded_bytes: u64,
    pub upload_speed: f64,
    /// Every piece is in and the torrent is only uploading now.
    pub seeding: bool,
}

/// The process's session and the settings it was started with (see [`session`]).
static SESSION: Mutex<Option<(Setup, Arc<Session>)>> = Mutex::const_new(None);
/// How many swarm downloads hold the session (see [`Hold`]).
static HOLDS: AtomicUsize = AtomicUsize::new(0);
/// How many finished torrents are still seeding (see [`wait_seeding`]).
static SEEDING: LazyLock<watch::Sender<usize>> = LazyLock::new(|| watch::Sender::new(0));
/// A .torrent file larger than this is not read (as in `ingest`).
const MAX_TORRENT_BYTES: usize = 16 * 1024 * 1024;

/// What a session is started with: the listen port (0 = librqbit's range), UPnP, and the SOCKS5
/// proxy every connection goes through.
#[derive(Clone, Debug, PartialEq)]
struct Setup {
    port: u16,
    upnp: bool,
    proxy: Option<String>,
}

impl Setup {
    /// The setup `options` ask for. librqbit sends peers and HTTP trackers through a SOCKS5
    /// proxy alone; any other proxy is refused rather than bypassed.
    fn of(options: &DownloadOptions) -> Result<Self, String> {
        let proxy = match options.proxy.as_deref().or(options.proxy_pool.first().map(String::as_str)) {
            None => None,
            Some(proxy) => match Url::parse(proxy) {
                // Peers are addresses, so socks5h has nothing more to resolve on the proxy.
                Ok(mut url) if matches!(url.scheme(), "socks5" | "socks5h") && url.host_str().is_some() && url.port().is_some() => {
                    let _ = url.set_scheme("socks5");
                    Some(url.to_string())
                }
                _ => {
                    return Err("BitTorrent goes through a proxy only when it is SOCKS5 (socks5://host:port); \
                                set one, or clear the proxy to download torrents"
                        .into())
                }
            },
        };
        Ok(Self { port: options.bt_port.unwrap_or(0), upnp: options.bt_upnp, proxy })
    }

    fn session_options(&self) -> SessionOptions {
        if let Some(proxy) = &self.proxy {
            // Only what goes through the proxy: no DHT, no local discovery, no listening (and so
            // no uTP); UDP trackers are left out of each torrent (see [`http_trackers_only`]).
            return SessionOptions {
                dht: None,
                disable_local_service_discovery: true,
                listen: None,
                connect: Some(ConnectionOptions { proxy_url: Some(proxy.clone()), ..Default::default() }),
                ..Default::default()
            };
        }
        let dht_state = crate::media::managed_bin_dir().and_then(|bin| Some(bin.parent()?.join("dht.json")));
        let persistence = dht_state.map(|file| DhtPersistenceConfig { config_filename: Some(file), ..Default::default() });
        SessionOptions {
            dht: Some(DhtSessionConfig { persistence, ..Default::default() }),
            listen: Some(ListenerOptions {
                listen_addr: (Ipv6Addr::UNSPECIFIED, self.port).into(),
                enable_upnp_port_forwarding: self.upnp,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

/// A swarm download's hold on the session: one being listed is not among its torrents yet, and
/// must not see it stopped for a new one.
struct Hold(Arc<Session>);

impl Drop for Hold {
    fn drop(&mut self) {
        HOLDS.fetch_sub(1, Ordering::SeqCst);
    }
}

impl std::ops::Deref for Hold {
    type Target = Arc<Session>;

    fn deref(&self) -> &Arc<Session> {
        &self.0
    }
}

/// Downloads the torrent `source` (a magnet link, an http(s) .torrent link or a `file:` URL of a
/// local one) into `out_dir` and returns the file (one-file torrent) or the folder named after it,
/// and whether any of it arrived now (false: every piece was on disk from an earlier run). A name
/// taken by files this torrent did not write is never written over: it gets a numbered one.
pub async fn run(
    source: &Url,
    options: &DownloadOptions,
    out_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let setup = Setup::of(options)?;
    let proxied = setup.proxy.is_some();
    let session = session(setup, out_dir.to_path_buf()).await?;
    emit(&snapshots, EngineSnapshot { torrent: Some(TorrentProgress::default()), ..Default::default() });
    let add = match source.scheme() {
        "magnet" if proxied => AddTorrent::from_url(without_udp_trackers(source).to_string()),
        "magnet" => AddTorrent::from_url(source.to_string()),
        _ => AddTorrent::from_bytes(torrent_bytes(source, options.proxy.as_deref()).await?),
    };
    let hints = peer_hints(source);
    // Only listed first (a magnet's metadata comes from the swarm), so that no file is created
    // before every name in it is checked.
    let listing = AddTorrentOptions { list_only: true, initial_peers: Some(hints.clone()), ..Default::default() };
    let listed = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(CANCELLED.to_string()),
        listed = session.add_torrent(add, Some(listing)) => listed.map_err(failed)?,
    };
    let AddTorrentResponse::ListOnly(listed) = listed else { return Err(failed("the torrent was not listed")) };

    let info = &listed.info;
    // librqbit lists at least one file.
    let files: Vec<PathBuf> = info.iter_file_details().map(|file| file.filename.to_pathbuf()).collect();
    if let Some(bad) = files.iter().find(|f| outside(f)) {
        return Err(format!("The torrent names a file outside its folder ({}); it was not downloaded", bad.display()));
    }
    // A multi-file torrent goes in a folder named after it (named as `ingest` names its task).
    let multi = info.info().files.is_some();
    let hash = listed.info_hash.as_string();
    let base = match multi {
        true => info.name().map(|n| crate::engine::sanitize_component(&n)).filter(|n| !n.is_empty()).unwrap_or_else(|| hash.clone()),
        false => files[0].file_stem().unwrap_or_default().to_string_lossy().into_owned(),
    };
    // Where it goes: its own name, else, when that is taken by what this torrent did not write,
    // a numbered one; a one-file torrent then in a folder of its own, as librqbit names the file.
    let place = |n: usize| {
        let folder = match (multi, n) {
            (false, 0) => out_dir.to_path_buf(),
            (true, 0) => out_dir.join(&base),
            (_, n) => out_dir.join(format!("{base} ({n})")),
        };
        let target = if multi { folder.clone() } else { folder.join(&files[0]) };
        (folder, target)
    };
    let taken = |path: &Path| std::fs::symlink_metadata(path).is_ok();
    let (folder, target) = (0..).map(place).find(|(_, target)| !taken(target) || claimed(&hash, target)).expect("a free name");
    // Resuming writes over what an earlier run of it left; librqbit keeps the pieces that check out.
    let resuming = taken(&target);
    claim(&hash, &target);
    let torrent = match proxied {
        true => http_trackers_only(&listed.torrent_bytes)?.into(),
        false => listed.torrent_bytes,
    };
    let speed = options.max_speed.and_then(|bps| NonZeroU32::new(bps.min(u32::MAX.into()) as u32));
    let adding = AddTorrentOptions {
        overwrite: resuming,
        output_folder: Some(folder.to_str().ok_or("The save folder's name is not valid Unicode")?.to_string()),
        initial_peers: Some([hints, listed.seen_peers].concat()),
        ratelimits: LimitsConfig { download_bps: speed, ..Default::default() },
        ..Default::default()
    };
    let (ours, handle) = match session.add_torrent(AddTorrent::from_bytes(torrent), Some(adding)).await {
        Ok(AddTorrentResponse::Added(_, handle)) => (true, handle),
        Ok(AddTorrentResponse::AlreadyManaged(_, handle)) => (false, handle),
        Ok(AddTorrentResponse::ListOnly(_)) => return Err(failed("the torrent was only listed")),
        Err(e) => return Err(failed(e)),
    };
    // A torrent the session already had (another download of it) is where that one put it.
    let target = if multi { handle.output_folder().to_path_buf() } else { handle.output_folder().join(&files[0]) };

    let mark = options.post.mark_of_the_web.then_some(source);
    let watched = watch_until_complete(&session, &handle, &target, mark, &snapshots, &cancel).await;
    let seeding = ours && watched.is_ok() && options.seed_ratio > 0.0;
    if watched.is_ok() {
        emit(&snapshots, progress(&handle.stats(), &target, seeding));
    }
    if seeding {
        seed(session.clone(), handle.clone(), options.seed_ratio, options.seed_minutes);
    } else if ours {
        let _ = session.delete(handle.id().into(), false).await;
    }
    watched.map(|fresh| (target, fresh))
}

/// Resolves once no torrent is seeding any more (the command line waits for it before exiting).
pub async fn wait_seeding() {
    let _ = SEEDING.subscribe().wait_for(|&n| n == 0).await;
}

/// Stops the BitTorrent session cleanly (on exit, or BitTorrent turned off), keeping the
/// downloaded files; the next swarm download starts a new one.
pub async fn shutdown() {
    let stopped = SESSION.lock().await.take();
    if let Some((_, session)) = stopped {
        session.stop().await;
    }
}

/// The process's session for `setup`: the running one when it was started with it, or when it
/// is busy (downloads hold it, or torrents seed) and `setup` adds no proxy it lacks (new port
/// and UPnP settings wait for those to finish); else a new one, started in `out_dir`.
async fn session(setup: Setup, out_dir: PathBuf) -> Result<Hold, String> {
    let mut slot = SESSION.lock().await;
    let hold = |session: &Arc<Session>| {
        HOLDS.fetch_add(1, Ordering::SeqCst);
        Hold(session.clone())
    };
    if let Some((had, running)) = slot.as_ref() {
        let busy = HOLDS.load(Ordering::SeqCst) > 0 || running.with_torrents(|torrents| torrents.count()) > 0;
        if *had == setup || busy && (setup.proxy.is_none() || setup.proxy == had.proxy) {
            return Ok(hold(running));
        }
        if busy {
            return Err("BitTorrent is still running torrents without this proxy; try again once they finish".into());
        }
        running.stop().await;
        *slot = None;
    }
    let started = Session::new_with_opts(out_dir, setup.session_options()).await.map_err(|e| format!("Cannot start BitTorrent: {e:#}"))?;
    *slot = Some((setup, started.clone()));
    Ok(hold(&started))
}

/// Sends progress every half second until the torrent has every piece, then says whether any of
/// it arrived now; an error when it fails, leaves the session (another download of it stopped,
/// or BitTorrent did) or `cancel` fires. Its files are marked as from the internet with `mark`
/// as they appear, not only once it is complete.
async fn watch_until_complete(
    session: &Session,
    handle: &ManagedTorrent,
    target: &Path,
    mut mark: Option<&Url>,
    snapshots: &Option<broadcast::Sender<EngineSnapshot>>,
    cancel: &CancellationToken,
) -> Result<bool, String> {
    let mut completed = handle.wait_until_completed();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CANCELLED.to_string()),
            done = &mut completed => {
                done.map_err(failed)?;
                return Ok(handle.stats().live.is_none_or(|live| live.snapshot.downloaded_and_checked_bytes > 0));
            }
            _ = tick.tick() => {
                let stats = handle.stats();
                if let Some(error) = stats.error {
                    return Err(failed(error));
                }
                if session.get(handle.id().into()).is_none() || matches!(stats.state, TorrentStatsState::Paused) {
                    return Err(failed("the torrent was stopped"));
                }
                // librqbit has made every file by the time the torrent is live.
                if let Some(source) = mark.filter(|_| stats.live.is_some()) {
                    mark = None;
                    let (target, source) = (target.to_path_buf(), source.clone());
                    tokio::task::spawn_blocking(move || crate::postprocess::mark_from_internet(&target, &source));
                }
                emit(snapshots, progress(&stats, target, false));
            }
        }
    }
}

/// Where the torrent `hash` was put by a run of it (maybe stopped): a list in the app's data
/// folder, one `<hash>\t<path>` a line.
fn claims_file() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!("hf-unit-torrents-{}.txt", std::process::id())));
    }
    Some(crate::media::managed_bin_dir()?.parent()?.join("torrents.txt"))
}

fn claim_line(hash: &str, path: &Path) -> String {
    format!("{hash}\t{}", std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).display())
}

/// Whether the torrent `hash` was put at `path` before, so what is there is its own to resume.
fn claimed(hash: &str, path: &Path) -> bool {
    let line = claim_line(hash, path);
    claims_file().and_then(|file| std::fs::read_to_string(file).ok()).is_some_and(|text| text.lines().any(|l| l == line))
}

/// Notes that the torrent `hash` goes at `path` (see [`claimed`]). Best effort.
fn claim(hash: &str, path: &Path) {
    if claimed(hash, path) {
        return;
    }
    let Some(file) = claims_file() else { return };
    let appended = std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))
        .and_then(|()| std::fs::OpenOptions::new().create(true).append(true).open(&file))
        .and_then(|mut f| writeln!(f, "{}", claim_line(hash, path)));
    if let Err(e) = appended {
        tracing::warn!("Cannot note where torrent {hash} goes in {}: {e}", file.display());
    }
}

/// `magnet` without its UDP trackers, which librqbit reaches around the proxy.
fn without_udp_trackers(magnet: &Url) -> Url {
    let mut kept = magnet.clone();
    let pairs: Vec<(String, String)> = magnet
        .query_pairs()
        .filter(|(key, value)| !(key == "tr" && value.to_ascii_lowercase().starts_with("udp:")))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    kept.query_pairs_mut().clear().extend_pairs(pairs);
    kept
}

/// The .torrent file `torrent` with its HTTP(S) trackers alone (see [`without_udp_trackers`]):
/// the same info, so the same torrent.
fn http_trackers_only(torrent: &[u8]) -> Result<Vec<u8>, String> {
    let parsed = librqbit::torrent_from_bytes(torrent).map_err(failed)?;
    let mut out = b"d13:announce-listl".to_vec();
    for tracker in parsed.iter_announce().map(|t| t.as_ref()).filter(|t| t.to_ascii_lowercase().starts_with(b"http")) {
        out.extend(format!("l{}:", tracker.len()).as_bytes());
        out.extend(tracker);
        out.push(b'e');
    }
    out.extend(b"e4:info");
    out.extend(parsed.info.raw_bytes.as_ref());
    out.push(b'e');
    Ok(out)
}

/// Keeps a finished torrent seeding in the background until [`seeded_enough`], then drops it
/// from the session (its files stay).
fn seed(session: Arc<Session>, handle: Arc<ManagedTorrent>, ratio: f64, minutes: u32) {
    SEEDING.send_modify(|n| *n += 1);
    tokio::spawn(async move {
        let started = Instant::now();
        loop {
            let stats = handle.stats();
            if stats.error.is_some() || seeded_enough(stats.uploaded_bytes, stats.total_bytes, ratio, started.elapsed(), minutes) {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = session.cancellation_token().cancelled() => break,
            }
        }
        let _ = session.delete(handle.id().into(), false).await;
        SEEDING.send_modify(|n| *n -= 1);
    });
}

/// Whether a torrent of `size` bytes that uploaded `uploaded` has seeded enough: `ratio` times its
/// size, or for `minutes` (0 = no time limit), whichever comes first.
fn seeded_enough(uploaded: u64, size: u64, ratio: f64, seeded: Duration, minutes: u32) -> bool {
    uploaded as f64 >= ratio * size as f64 || (minutes > 0 && seeded >= Duration::from_secs(u64::from(minutes) * 60))
}

/// Whether the torrent's file `path` (relative to its folder) leaves it. librqbit refuses `..`
/// and separators in names, but not a Windows drive (`C:x` would land in the current folder of
/// drive C:); any other ':' would write an alternate data stream instead of a file.
fn outside(path: &Path) -> bool {
    path.components().any(|c| !matches!(c, Component::Normal(_)))
        || (cfg!(windows) && path.as_os_str().to_string_lossy().contains(':'))
}

/// The `x.pe` peer addresses of a magnet link (BEP 9), tried before the DHT and trackers find any.
fn peer_hints(source: &Url) -> Vec<SocketAddr> {
    source.query_pairs().filter(|(key, _)| key == "x.pe").filter_map(|(_, peer)| peer.parse().ok()).collect()
}

/// The .torrent file at `source`: a `file:` URL, or an http(s) link fetched through `proxy`.
async fn torrent_bytes(source: &Url, proxy: Option<&str>) -> Result<Vec<u8>, String> {
    // reqwest's errors name the URL, which may carry a private tracker's passkey: left out.
    let fail = |e: &dyn std::fmt::Display| format!("Cannot read the .torrent file: {e}");
    if source.scheme() == "file" {
        let path = source.to_file_path().map_err(|()| fail(&"not a local file"))?;
        return tokio::fs::read(path).await.map_err(|e| fail(&e));
    }
    let request = crate::ingest::descriptor_client(proxy)?.get(source.clone()).send().await;
    let mut response = request.and_then(|r| r.error_for_status()).map_err(|e| fail(&e.without_url()))?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| fail(&e.without_url()))? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_TORRENT_BYTES {
            return Err(fail(&format!("larger than {MAX_TORRENT_BYTES} bytes")));
        }
    }
    Ok(bytes)
}

/// The engine's progress for a torrent with `stats`, saved at `target`.
fn progress(stats: &TorrentStats, target: &Path, seeding: bool) -> EngineSnapshot {
    let live = stats.live.as_ref();
    // librqbit measures speeds in MiB/s.
    let speed = |mib: f64| mib * 1024.0 * 1024.0;
    let peers = live.map_or(0, |l| l.snapshot.peer_stats.live as usize);
    EngineSnapshot {
        total_bytes: stats.total_bytes,
        downloaded_bytes: stats.progress_bytes,
        speed_bytes_per_sec: live.map_or(0.0, |l| speed(l.download_speed.mbps)),
        progress_ratio: if stats.total_bytes > 0 { stats.progress_bytes as f64 / stats.total_bytes as f64 } else { 0.0 },
        active_workers: peers,
        target_path: Some(target.to_path_buf()),
        torrent: Some(TorrentProgress {
            peers,
            uploaded_bytes: stats.uploaded_bytes,
            upload_speed: live.map_or(0.0, |l| speed(l.upload_speed.mbps)),
            seeding,
        }),
        ..Default::default()
    }
}

fn emit(snapshots: &Option<broadcast::Sender<EngineSnapshot>>, snapshot: EngineSnapshot) {
    if let Some(tx) = snapshots {
        let _ = tx.send(snapshot);
    }
}

fn failed(e: impl std::fmt::Display) -> String {
    format!("BitTorrent download failed: {e:#}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use librqbit::spawn_utils::BlockingSpawner;
    use librqbit::{create_torrent, CreateTorrentOptions};
    use std::net::Ipv4Addr;

    #[test]
    fn seeding_stops_at_the_ratio_or_the_time_limit() {
        let (hour, none) = (Duration::from_secs(3600), Duration::ZERO);
        assert!(seeded_enough(100, 100, 1.0, none, 60), "ratio reached");
        assert!(!seeded_enough(99, 100, 1.0, Duration::from_secs(59 * 60), 60));
        assert!(seeded_enough(0, 100, 1.0, hour, 60), "time is up");
        assert!(!seeded_enough(50, 100, 1.0, hour * 100, 0), "0 minutes: the ratio alone");
        // --seed-time alone (ratio f64::MAX): only the time ends it.
        assert!(!seeded_enough(u64::MAX, 100, f64::MAX, none, 60));
        assert!(seeded_enough(0, 100, f64::MAX, hour, 60));
    }

    #[test]
    fn names_that_leave_the_folder() {
        assert!(!outside(Path::new("a/b.txt")) && !outside(Path::new(".hidden")));
        assert!(outside(Path::new("../x")) && outside(Path::new("/etc/passwd")));
        assert_eq!(outside(Path::new("C:x")), cfg!(windows));
        assert_eq!(outside(Path::new("a/b:c")), cfg!(windows));
    }

    #[test]
    fn magnet_peer_hints() {
        let magnet = Url::parse("magnet:?xt=urn:btih:00&x.pe=127.0.0.1:6881&x.pe=host:1&x.pe=[::1]:2").unwrap();
        assert_eq!(peer_hints(&magnet), vec!["127.0.0.1:6881".parse().unwrap(), "[::1]:2".parse().unwrap()]);
    }

    /// Behind a proxy: SOCKS5 alone, and no UDP tracker is left to reach around it.
    #[test]
    fn proxies_and_their_trackers() {
        let with = |proxy: &str| Setup::of(&DownloadOptions { proxy: Some(proxy.into()), ..Default::default() });
        assert_eq!(with("socks5h://u:p@127.0.0.1:9050").unwrap().proxy.as_deref(), Some("socks5://u:p@127.0.0.1:9050"));
        assert!(with("http://127.0.0.1:8080").unwrap_err().contains("SOCKS5"));
        assert!(with("socks5://127.0.0.1").is_err(), "librqbit needs the port");
        let pooled = DownloadOptions { proxy_pool: vec!["http://p:1".into()], ..Default::default() };
        assert!(Setup::of(&pooled).is_err());
        assert_eq!(Setup::of(&DownloadOptions::default()).unwrap(), Setup { port: 0, upnp: false, proxy: None });

        let hash = "0123456789abcdef0123456789abcdef01234567";
        let magnet = Url::parse(&format!("magnet:?xt=urn:btih:{hash}&dn=a&tr=UDP%3A%2F%2Ft.example%3A1&tr=https%3A%2F%2Ft.example%2Fa")).unwrap();
        let kept = librqbit::Magnet::parse(without_udp_trackers(&magnet).as_str()).unwrap();
        assert_eq!((kept.as_id20().unwrap().as_string(), kept.trackers), (hash.to_string(), vec!["https://t.example/a".to_string()]));

        let text = |s: &str| format!("{}:{s}", s.len());
        let info = format!("d6:lengthi5e4:name{}12:piece lengthi16384e6:pieces20:{}e", text("a.bin"), "0".repeat(20));
        let torrent = format!(
            "d8:announce{}13:announce-listll{}el{}ee4:info{info}e",
            text("udp://t.example:1"),
            text("udp://t.example:1"),
            text("https://t.example/a")
        );
        let rewritten = http_trackers_only(torrent.as_bytes()).unwrap();
        let parsed = librqbit::torrent_from_bytes(&rewritten).unwrap();
        assert_eq!(parsed.info.raw_bytes.as_ref(), info.as_bytes());
        let trackers: Vec<&[u8]> = parsed.iter_announce().map(|t| t.as_ref()).collect();
        assert_eq!(trackers, [b"https://t.example/a".as_slice()]);
    }

    /// A session that only talks to peers it is told of, on loopback.
    async fn offline(dir: &Path) -> Arc<Session> {
        let options = SessionOptions {
            dht: None,
            disable_local_service_discovery: true,
            listen: Some(ListenerOptions { listen_addr: (Ipv4Addr::LOCALHOST, 0).into(), ..Default::default() }),
            ..Default::default()
        };
        Session::new_with_opts(dir.to_path_buf(), options).await.unwrap()
    }

    // One test, as the process-wide session lives on the runtime that started it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn downloads_from_a_local_swarm_and_resumes_after_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let (seeds, out) = (tmp.path().join("seed"), tmp.path().join("out"));
        std::fs::create_dir_all(&seeds).unwrap();
        let payload: Vec<u8> = (0..2u32 << 20).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(seeds.join("payload.bin"), &payload).unwrap();
        let options = CreateTorrentOptions { piece_length: Some(64 * 1024), ..Default::default() };
        let torrent = create_torrent(&seeds.join("payload.bin"), options, &BlockingSpawner::new(1)).await.unwrap();

        // The seeder uploads slowly enough (512 KiB/s) for a cancel to land halfway.
        let seeder = offline(&seeds).await;
        let slow = LimitsConfig { upload_bps: NonZeroU32::new(512 * 1024), ..Default::default() };
        let seeding = AddTorrentOptions {
            overwrite: true,
            output_folder: Some(seeds.to_str().unwrap().into()),
            ratelimits: slow,
            ..Default::default()
        };
        let added = seeder.add_torrent(AddTorrent::from_bytes(torrent.as_bytes().unwrap()), Some(seeding)).await;
        let seeding = added.unwrap().into_handle().unwrap();
        seeding.wait_until_completed().await.unwrap();
        let port = seeder.listen_addr().unwrap().port();
        let magnet = format!("magnet:?xt=urn:btih:{}&dn=payload.bin&x.pe=127.0.0.1:{port}", torrent.info_hash().as_string());
        let magnet = Url::parse(&magnet).unwrap();
        let session = offline(&out).await;
        let options = DownloadOptions::default();
        *SESSION.lock().await = Some((Setup::of(&options).unwrap(), session.clone()));
        // A file of the user's with the torrent's name is never written over.
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("payload.bin"), b"mine").unwrap();
        let target = out.join("payload (1)").join("payload.bin");

        // Cancelled once some of it is in: the files stay, the session lets it go.
        let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(64);
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let seen = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Ok(snapshot) = rx.recv().await {
                // The peer count can trail the first bytes by a snapshot: wait for both.
                if snapshot.downloaded_bytes > 0 && snapshot.torrent.as_ref().is_some_and(|t| t.peers >= 1) {
                    stop.cancel();
                }
                seen.push(snapshot);
            }
            seen
        });
        assert_eq!(run(&magnet, &options, &out, Some(tx), cancel).await.unwrap_err(), CANCELLED);
        let seen = seen.await.unwrap();
        assert!(seen.iter().any(|s| s.torrent.as_ref().is_some_and(|t| t.peers >= 1)), "{seen:?}");
        let partial = seen.last().unwrap().downloaded_bytes;
        assert!(partial > 0 && partial < payload.len() as u64, "{partial}");
        assert_eq!(seen.last().unwrap().target_path.as_deref(), Some(target.as_path()));
        assert_eq!(session.with_torrents(|t| t.count()), 0);
        let uploaded = seeding.stats().uploaded_bytes;

        // Run again: the pieces on disk are kept, only the rest comes from the swarm.
        let (path, fresh) = run(&magnet, &options, &out, None, CancellationToken::new()).await.unwrap();
        assert_eq!((path.as_path(), fresh), (target.as_path(), true));
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        let resent = seeding.stats().uploaded_bytes - uploaded;
        assert!(resent < payload.len() as u64, "sent {resent} bytes again");
        assert_eq!(session.with_torrents(|t| t.count()), 0, "no seeding with a ratio of 0");
        assert_eq!(std::fs::read(out.join("payload.bin")).unwrap(), b"mine");
        // Once more: all of it is there already, so nothing new arrives.
        let again = run(&magnet, &options, &out, None, CancellationToken::new()).await.unwrap();
        assert_eq!(again, (target, false));

        // A file name with a drive in it is refused before anything is written.
        if cfg!(windows) {
            let mut evil = b"d4:infod6:lengthi5e4:name9:C:bad.bin12:piece lengthi16384e6:pieces20:".to_vec();
            evil.extend([0u8; 20]);
            evil.extend(b"ee");
            let file = tmp.path().join("evil.torrent");
            std::fs::write(&file, evil).unwrap();
            let source = Url::from_file_path(&file).unwrap();
            let refused = run(&source, &options, &out, None, CancellationToken::new());
            let error = tokio::time::timeout(Duration::from_secs(10), refused).await.expect("refused at once");
            assert!(error.unwrap_err().contains("outside its folder"));
            assert!(!Path::new("C:bad.bin").exists());
            assert_eq!(session.with_torrents(|t| t.count()), 0);
        }
        seeder.stop().await;
    }

    // Live: the real session (DHT, trackers) on a public magnet without web seeds (Sintel, the
    // Blender film), cancelled once a megabyte is in. Its DHT state goes under %LOCALAPPDATA%.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn live_magnet_from_the_swarm() {
        let tmp = tempfile::tempdir().unwrap();
        let magnet = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel\
            &tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&tr=udp%3A%2F%2Fexplodie.org%3A6969";
        let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(64);
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let seen = tokio::spawn(async move {
            let mut last = EngineSnapshot::default();
            while let Ok(snapshot) = rx.recv().await {
                if snapshot.downloaded_bytes > 1 << 20 {
                    stop.cancel();
                }
                last = snapshot;
            }
            last
        });
        let options = DownloadOptions::default();
        let magnet = Url::parse(magnet).unwrap();
        let run = run(&magnet, &options, tmp.path(), Some(tx), cancel);
        let result = tokio::time::timeout(Duration::from_secs(180), run).await.expect("a megabyte within 3 minutes");
        assert_eq!(result.unwrap_err(), CANCELLED);
        let last = seen.await.unwrap();
        println!("{last:?}");
        assert!(last.downloaded_bytes > 1 << 20 && last.target_path == Some(tmp.path().join("Sintel")));        shutdown().await;
    }
}
