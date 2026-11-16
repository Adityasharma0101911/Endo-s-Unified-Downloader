//! BitTorrent swarm downloads (magnet links without HTTP web seeds, and .torrent files), with
//! librqbit. A .torrent's web seeds join the swarm as peers of their own (see `crate::webseed`),
//! as librqbit has no web seeds; through a proxy their files come over HTTP first instead (see
//! [`fill_from_web_seeds`]). One session serves the whole process, started with
//! the port, UPnP and proxy settings of the download that needs it; one that wants other
//! settings gets a new session once no torrent runs. The DHT keeps its state in the app's data
//! folder.
//!
//! A cancelled download leaves the session (its files stay); running it again adds it back, and
//! librqbit checks the pieces already on disk before fetching the rest. A finished torrent seeds
//! until its ratio or time limit, then leaves the session too.
//!
//! Some of a torrent's files alone (an archive.org item's, see `repos`) come into a hidden folder
//! beside its own and move into it once complete; such a torrent does not seed, and three of them
//! download at once.

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
use tokio::sync::{broadcast, watch, Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{DownloadEngine, DownloadOptions, EngineSnapshot, CANCELLED};

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
/// Fewest bytes a site's files come from its torrent for (its web seeds and swarm at once: an
/// archive.org item's, see `repos`, a Kiwix ZIM file's, see `archives`), else from its HTTP alone:
/// a swarm takes seconds to start (the torrent, its trackers, the first peers), longer than HTTP
/// needs for less, and small files seldom have peers anyway.
pub(crate) const SWARM_MIN: u64 = 100 << 20;

/// Whether web seeds join the swarm as peers (see `crate::webseed`) and a site's large files
/// take its torrent beside HTTP (see `ingest::ListOptions::swarm_with_http`).
// shortcut: off unless ENDO_WEB_SEED_PEERS is set, as archive.org's 288 MiB took 46-57 s that way
// and 7-9 s over HTTP first; on by default once a real run shows it no slower.
pub(crate) fn web_seed_peers() -> bool {
    cfg!(test) || std::env::var_os("ENDO_WEB_SEED_PEERS").is_some()
}
/// Torrents of chosen files (archive.org items', see `repos`) downloading at once: a collection's
/// items wait for one of these turns rather than each joining its swarm.
static CHOSEN: Semaphore = Semaphore::const_new(3);

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

/// Downloads the torrent `urls` names first (a magnet link, an http(s) .torrent link or a `file:`
/// URL of a local one) into `out_dir` and returns the file (one-file torrent) or the folder named
/// after it, and whether any of it arrived now (false: every piece was on disk from an earlier
/// run). The rest of `urls` are more web seeds of a one-file torrent (a Kiwix ZIM file's Metalink
/// mirrors, see `archives`). A name taken by files this torrent did not write is never written
/// over: it gets a numbered one.
pub async fn run(
    urls: &[Url],
    options: &DownloadOptions,
    out_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let (source, more_seeds) = urls.split_first().ok_or("No URLs to download")?;
    // Chosen files of a torrent (an archive.org item's): a collection's items take turns.
    let _turn = match options.torrent_files.is_empty() {
        true => None,
        false => tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CANCELLED.to_string()),
            turn = CHOSEN.acquire() => turn.ok(),
        },
    };
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
    // Chosen files (see `DownloadOptions::torrent_files`) come into a hidden folder beside the
    // torrent's own, which librqbit fills with every file of it, and move into it once complete.
    let only = match options.torrent_files.as_slice() {
        [] => None,
        chosen => {
            let picked: Vec<usize> = (0..files.len()).filter(|&i| chosen.contains(&files[i])).collect();
            if !multi || picked.len() != chosen.len() {
                return Err(failed("the torrent lacks files it was chosen for"));
            }
            Some(picked)
        }
    };
    let (folder, target, resuming) = match only {
        // The folder is ours: what is there is an earlier run's.
        Some(_) => (out_dir.join(format!(".{base}.torrent")), out_dir.join(&base), true),
        None => {
            let (folder, target) = (0..).map(place).find(|(_, target)| !taken(target) || claimed(&hash, target)).expect("a free name");
            // Resuming writes over what an earlier run of it left; librqbit keeps the pieces that check out.
            let resuming = taken(&target);
            claim(&hash, &target);
            (folder, target, resuming)
        }
    };
    // Web seeds are peers of the swarm (see `crate::webseed`): both serve the torrent at once.
    // Through a proxy librqbit cannot reach them on loopback, so their files come over HTTP
    // first; librqbit's first check then keeps the pieces of them that match, and the swarm
    // brings the rest. A one-file torrent's further web seeds (a ZIM file's mirrors) join its own.
    let seeded = match more_seeds {
        [] => listed.torrent_bytes.to_vec(),
        _ if multi => return Err(failed("only a one-file torrent takes more web seeds")),
        more => with_web_seeds(&listed.torrent_bytes, more)?,
    };
    let network = network_only(options);
    let web = match proxied || !web_seed_peers() {
        true => None,
        false => {
            let client = crate::engine::build_client(&network)?;
            let (connections, per_host) = (network.num_connections.clamp(4, 64), network.max_connections_per_host);
            match crate::webseed::start(&seeded, listed.info_hash.0, client, connections, per_host, &options.torrent_files).await {
                Ok(web) => Some(web),
                Err(why) => {
                    tracing::debug!("The torrent's web seeds are no peers: {why}");
                    #[cfg(test)]
                    crate::webseed::NOT_PEERS.lock().push(why);
                    None
                }
            }
        }
    };
    let kept: Vec<(PathBuf, u64)> = info.iter_file_details().filter(|f| !f.attrs().padding).map(|f| (f.filename.to_pathbuf(), f.len)).collect();
    let filled = match web {
        Some(_) => false,
        None => fill_from_web_seeds(&seeded, &kept, &folder, &target, options, &snapshots, &cancel).await?,
    };
    let seeds = web.as_ref().map(|web| web.addrs.clone()).unwrap_or_default();
    let torrent = match proxied {
        true => http_trackers_only(&listed.torrent_bytes)?.into(),
        false => listed.torrent_bytes,
    };
    let speed = options.max_speed.and_then(|bps| NonZeroU32::new(bps.min(u32::MAX.into()) as u32));
    let adding = AddTorrentOptions {
        overwrite: resuming || filled,
        output_folder: Some(folder.to_str().ok_or("The save folder's name is not valid Unicode")?.to_string()),
        initial_peers: Some([hints, listed.seen_peers, seeds].concat()),
        ratelimits: LimitsConfig { download_bps: speed, ..Default::default() },
        only_files: only.clone(),
        ..Default::default()
    };
    let (ours, handle) = match session.add_torrent(AddTorrent::from_bytes(torrent), Some(adding)).await {
        Ok(AddTorrentResponse::Added(_, handle)) => (true, handle),
        Ok(AddTorrentResponse::AlreadyManaged(..)) if only.is_some() => return Err(failed("the torrent is already downloading")),
        Ok(AddTorrentResponse::AlreadyManaged(_, handle)) => (false, handle),
        Ok(AddTorrentResponse::ListOnly(_)) => return Err(failed("the torrent was only listed")),
        Err(e) => return Err(failed(e)),
    };
    // A torrent the session already had (another download of it) is where that one put it.
    let (target, marked) = match (&only, multi) {
        (Some(_), _) => (target, folder.clone()),
        (None, true) => (handle.output_folder().to_path_buf(), handle.output_folder().to_path_buf()),
        (None, false) => (handle.output_folder().join(&files[0]), handle.output_folder().join(&files[0])),
    };

    let mark = options.post.mark_of_the_web.then_some(source);
    let watched = watch_until_complete(&session, &handle, &target, (marked.as_path(), mark), &snapshots, &cancel).await;
    if let Some(web) = web {
        tracing::debug!("{} bytes came from the web seeds", web.fetched());
    }
    // Chosen files leave the torrent's folder, so it cannot seed from them.
    let seeding = ours && only.is_none() && watched.is_ok() && options.seed_ratio > 0.0;
    if watched.is_ok() {
        emit(&snapshots, progress(&handle.stats(), &target, seeding));
    }
    if seeding {
        seed(session.clone(), handle.clone(), options.seed_ratio, options.seed_minutes);
    } else if ours {
        let _ = session.delete(handle.id().into(), false).await;
    }
    // librqbit has let go of the files by now.
    if let (Some(picked), Ok(_)) = (&only, &watched) {
        move_chosen(&folder, &target, picked.iter().map(|&i| &files[i]))?;
    }
    watched.map(|fresh| (target, fresh || filled))
}

/// Moves the chosen `files` of a torrent from the hidden folder `stage` into `folder`, each where
/// no file is yet (see `engine::free_path`), then removes `stage` with the rest librqbit put there.
fn move_chosen<'a>(stage: &Path, folder: &Path, files: impl Iterator<Item = &'a PathBuf>) -> Result<(), String> {
    for file in files {
        let to = crate::engine::free_path(&folder.join(file));
        let moved = std::fs::create_dir_all(to.parent().unwrap_or(folder)).and_then(|()| std::fs::rename(stage.join(file), &to));
        moved.map_err(|e| format!("Cannot move {} into {}: {e}", file.display(), folder.display()))?;
    }
    if let Err(e) = std::fs::remove_dir_all(stage) {
        tracing::warn!("Cannot remove {}: {e}", stage.display());
    }
    Ok(())
}

/// Downloads over HTTP each file of the torrent `bytes` that has web seeds and is not on disk yet,
/// to where librqbit puts it in `folder` (`kept`: its files' names and sizes, padding files left
/// out), checked by the torrent's piece hashes where they hold it alone (see
/// `torrent::TorrentFile::pieces`). A file that fails keeps what came: its `.part` takes its
/// place, for librqbit's check (not one the probe brought whole, up to 1 MiB, which has no state
/// to say what in it is good). Whether any file was written; an error only when `cancel` fires.
///
/// A boxed Send future of a declared type: the engine's run leads here again (for a torrent),
/// which the compiler could not size or prove Send otherwise.
#[allow(clippy::too_many_arguments)]
fn fill_from_web_seeds<'a>(
    bytes: &'a [u8],
    kept: &'a [(PathBuf, u64)],
    folder: &'a Path,
    target: &'a Path,
    options: &'a DownloadOptions,
    snapshots: &'a Option<broadcast::Sender<EngineSnapshot>>,
    cancel: &'a CancellationToken,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>> {
    Box::pin(fill(bytes, kept, folder, target, options, snapshots, cancel))
}

async fn fill(
    bytes: &[u8],
    kept: &[(PathBuf, u64)],
    folder: &Path,
    target: &Path,
    options: &DownloadOptions,
    snapshots: &Option<broadcast::Sender<EngineSnapshot>>,
    cancel: &CancellationToken,
) -> Result<bool, String> {
    let Ok(torrent) = crate::torrent::parse_torrent_bytes(bytes) else { return Ok(false) };
    // Both are the torrent's files in order; at any doubt the swarm brings them all.
    let same = torrent.files.len() == kept.len() && torrent.files.iter().zip(kept).all(|(f, (_, len))| f.length == *len);
    if !same {
        return Ok(false);
    }
    let (total, mut done, mut filled) = (torrent.total_length, 0, false);
    for (file, (name, _)) in torrent.files.iter().zip(kept) {
        let path = folder.join(name);
        let left_out = !options.torrent_files.is_empty() && !options.torrent_files.contains(name);
        if file.urls.is_empty() || left_out || std::fs::symlink_metadata(&path).is_ok() {
            done += file.length;
            continue;
        }
        // The user's network settings only: no sign-in, cookies or headers of theirs go to a seed.
        let http = DownloadOptions { output_path: Some(path.clone()), metalink_pieces: file.pieces.clone(), ..network_only(options) };
        let progress = Progress { total, before: done, target: target.to_path_buf(), torrent: true };
        let result = run_nested(DownloadEngine::new(file.urls.clone(), http), snapshots, progress, cancel).await;
        match result {
            _ if cancel.is_cancelled() => return Err(CANCELLED.to_string()),
            Ok(_) => filled = true,
            Err(e) => {
                tracing::warn!("{} did not come whole from its web seeds ({}); the swarm brings the rest", path.display(), e);
                let part = crate::engine::part_path(&path);
                if std::fs::rename(&part, &path).is_ok() {
                    let _ = crate::state::DownloadState::remove(&crate::state::DownloadState::state_file_path(&part));
                    filled = true;
                }
            }
        }
        done += file.length;
    }
    Ok(filled)
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
/// or BitTorrent did) or `cancel` fires. Its files (at `marked`) are marked as from the internet
/// with `mark` as they appear, not only once it is complete.
async fn watch_until_complete(
    session: &Session,
    handle: &ManagedTorrent,
    target: &Path,
    (marked, mut mark): (&Path, Option<&Url>),
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
                    let (target, source) = (marked.to_path_buf(), source.clone());
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

/// The .torrent file `torrent` with `more` among its web seeds (BEP 19's `url-list`), its trackers
/// kept: the same info, so the same torrent.
fn with_web_seeds(torrent: &[u8], more: &[Url]) -> Result<Vec<u8>, String> {
    let parsed = librqbit::torrent_from_bytes(torrent).map_err(failed)?;
    let had = crate::torrent::parse_torrent_bytes(torrent)?.web_seeds;
    let text = |out: &mut Vec<u8>, bytes: &[u8]| {
        out.extend(format!("{}:", bytes.len()).as_bytes());
        out.extend(bytes);
    };
    let mut out = b"d13:announce-listl".to_vec();
    for tracker in parsed.iter_announce() {
        out.push(b'l');
        text(&mut out, tracker.as_ref());
        out.push(b'e');
    }
    out.extend(b"e4:info");
    out.extend(parsed.info.raw_bytes.as_ref());
    out.extend(b"8:url-listl");
    for seed in had.iter().chain(more.iter().filter(|seed| !had.contains(seed))) {
        text(&mut out, seed.as_str().as_bytes());
    }
    out.extend(b"ee");
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

/// The user's network settings alone, for a download the app makes as part of another (a torrent's
/// file from its web seed, an image's layer): no sign-in, cookies or headers of theirs, no
/// checksum from the server, no Mark-of-the-Web on a part of something else.
pub(crate) fn network_only(options: &DownloadOptions) -> DownloadOptions {
    DownloadOptions {
        remote_checksum: false,
        post: crate::postprocess::PostOptions { mark_of_the_web: false, ..Default::default() },
        num_connections: options.num_connections,
        max_connections_per_host: options.max_connections_per_host,
        proxy: options.proxy.clone(),
        proxy_pool: options.proxy_pool.clone(),
        no_proxy: options.no_proxy,
        max_speed: options.max_speed,
        priority: options.priority,
        max_retries: options.max_retries,
        stall_timeout_secs: options.stall_timeout_secs,
        fsync_on_complete: options.fsync_on_complete,
        bind_addresses: options.bind_addresses.clone(),
        spread_addresses: options.spread_addresses,
        adaptive: options.adaptive,
        insecure: options.insecure,
        ip_family: options.ip_family,
        connect_timeout_secs: options.connect_timeout_secs,
        user_agent: options.user_agent.clone(),
        ..Default::default()
    }
}

/// How a part's progress shows as the whole's: `before` bytes of `total` done ahead of it, at
/// `target`, as a torrent's when `torrent`.
pub(crate) struct Progress {
    pub total: u64,
    pub before: u64,
    pub target: PathBuf,
    pub torrent: bool,
}

/// Runs `engine`, one part of a bigger download, its snapshots sent on `out` as `progress` makes
/// them the whole's. A cancel of `cancel` lets it save its state first, for the next run to resume.
///
/// A boxed Send future of a declared type: the engine's run leads here again, which the compiler
/// could not size or prove Send otherwise.
pub(crate) fn run_nested<'a>(
    engine: DownloadEngine,
    out: &'a Option<broadcast::Sender<EngineSnapshot>>,
    progress: Progress,
    cancel: &'a CancellationToken,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<PathBuf, String>> + Send + 'a>> {
    Box::pin(async move {
        let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(64);
        let shown = out.clone();
        let forward = tokio::spawn(async move {
            let Progress { total, before, target, torrent } = progress;
            loop {
                match rx.recv().await {
                    Ok(s) => emit(&shown, EngineSnapshot {
                        total_bytes: total,
                        downloaded_bytes: before + s.downloaded_bytes,
                        speed_bytes_per_sec: s.speed_bytes_per_sec,
                        progress_ratio: if total > 0 { (before + s.downloaded_bytes) as f64 / total as f64 } else { 0.0 },
                        active_workers: s.active_workers,
                        target_path: Some(target.clone()),
                        torrent: torrent.then(TorrentProgress::default),
                        host: s.host,
                        ..Default::default()
                    }),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
        let stop = engine.clone();
        let stopping = async {
            cancel.cancelled().await;
            stop.cancel();
            std::future::pending::<()>().await
        };
        let result = tokio::select! {
            result = engine.run(Some(tx)) => result,
            () = stopping => unreachable!(),
        };
        forward.abort();
        result
    })
}

fn emit(snapshots: &Option<broadcast::Sender<EngineSnapshot>>, snapshot: EngineSnapshot) {
    if let Some(tx) = snapshots {
        let _ = tx.send(snapshot);
    }
}

fn failed(e: impl std::fmt::Display) -> String {
    format!("BitTorrent download failed: {e:#}")
}

/// A session that only talks to peers it is told of, on loopback (tests, the speed benchmark).
#[cfg(test)]
pub(crate) async fn offline(dir: &Path) -> Arc<Session> {
    let options = SessionOptions {
        dht: None,
        disable_local_service_discovery: true,
        listen: Some(ListenerOptions { listen_addr: (std::net::Ipv4Addr::LOCALHOST, 0).into(), ..Default::default() }),
        ..Default::default()
    };
    Session::new_with_opts(dir.to_path_buf(), options).await.unwrap()
}

/// Makes an [`offline`] session in `dir` the process's, for the downloads [`run`] makes.
#[cfg(test)]
pub(crate) async fn go_offline(dir: &Path) -> Arc<Session> {
    let session = offline(dir).await;
    let before = SESSION.lock().await.replace((Setup::of(&DownloadOptions::default()).unwrap(), session.clone()));
    if let Some((_, before)) = before {
        before.stop().await;
    }
    session
}

#[cfg(test)]
mod tests {
    use super::*;
    use librqbit::spawn_utils::BlockingSpawner;
    use librqbit::{create_torrent, CreateTorrentOptions};

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

        // More web seeds join its own, the trackers and info kept.
        let seeded = format!("{}8:url-listl{}ee", &torrent[..torrent.len() - 1], text("https://a.example/a.bin"));
        let more = [Url::parse("https://a.example/a.bin").unwrap(), Url::parse("https://b.example/a.bin").unwrap()];
        let rewritten = with_web_seeds(seeded.as_bytes(), &more).unwrap();
        let parsed = librqbit::torrent_from_bytes(&rewritten).unwrap();
        assert_eq!(parsed.info.raw_bytes.as_ref(), info.as_bytes());
        let trackers: Vec<&[u8]> = parsed.iter_announce().map(|t| t.as_ref()).collect();
        assert!(trackers.contains(&b"udp://t.example:1".as_slice()) && trackers.contains(&b"https://t.example/a".as_slice()));
        let seeds = crate::torrent::parse_torrent_bytes(&rewritten).unwrap().files[0].urls.clone();
        assert_eq!(seeds, more);
    }

    /// The torrent of `file` in 64 KiB pieces, and its bytes with `web` as its web seed.
    async fn with_web_seed(file: &Path, web: &Url) -> (librqbit::CreateTorrentResult, Vec<u8>) {
        let pieces = CreateTorrentOptions { piece_length: Some(64 * 1024), ..Default::default() };
        let made = create_torrent(file, pieces, &BlockingSpawner::new(1)).await.unwrap();
        let bytes = made.as_bytes().unwrap();
        // The root dictionary's last key, after "info".
        let seeded = [&bytes[..bytes.len() - 1], format!("8:url-list{}:{web}e", web.as_str().len()).as_bytes()].concat();
        (made, seeded)
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
        let session = go_offline(&out).await;
        let options = DownloadOptions::default();
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
        assert_eq!(run(std::slice::from_ref(&magnet), &options, &out, Some(tx), cancel).await.unwrap_err(), CANCELLED);
        let seen = seen.await.unwrap();
        assert!(seen.iter().any(|s| s.torrent.as_ref().is_some_and(|t| t.peers >= 1)), "{seen:?}");
        let partial = seen.last().unwrap().downloaded_bytes;
        assert!(partial > 0 && partial < payload.len() as u64, "{partial}");
        assert_eq!(seen.last().unwrap().target_path.as_deref(), Some(target.as_path()));
        assert_eq!(session.with_torrents(|t| t.count()), 0);
        let uploaded = seeding.stats().uploaded_bytes;

        // Run again: the pieces on disk are kept, only the rest comes from the swarm.
        let (path, fresh) = run(std::slice::from_ref(&magnet), &options, &out, None, CancellationToken::new()).await.unwrap();
        assert_eq!((path.as_path(), fresh), (target.as_path(), true));
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        let resent = seeding.stats().uploaded_bytes - uploaded;
        assert!(resent < payload.len() as u64, "sent {resent} bytes again");
        assert_eq!(session.with_torrents(|t| t.count()), 0, "no seeding with a ratio of 0");
        assert_eq!(std::fs::read(out.join("payload.bin")).unwrap(), b"mine");
        // Once more: all of it is there already, so nothing new arrives.
        let again = run(std::slice::from_ref(&magnet), &options, &out, None, CancellationToken::new()).await.unwrap();
        assert_eq!(again, (target, false));

        // A file name with a drive in it is refused before anything is written.
        if cfg!(windows) {
            let mut evil = b"d4:infod6:lengthi5e4:name9:C:bad.bin12:piece lengthi16384e6:pieces20:".to_vec();
            evil.extend([0u8; 20]);
            evil.extend(b"ee");
            let file = tmp.path().join("evil.torrent");
            std::fs::write(&file, evil).unwrap();
            let source = Url::from_file_path(&file).unwrap();
            let refused = run(std::slice::from_ref(&source), &options, &out, None, CancellationToken::new());
            let error = tokio::time::timeout(Duration::from_secs(10), refused).await.expect("refused at once");
            assert!(error.unwrap_err().contains("outside its folder"));
            assert!(!Path::new("C:bad.bin").exists());
            assert_eq!(session.with_torrents(|t| t.count()), 0);
        }

        // A .torrent with a web seed and no peer known: all of it comes from the seed, as a peer,
        // checked by its pieces.
        let size = 1536 * 1024;
        let (web, stats) = crate::netsim::serve(crate::netsim::Server { size, ..Default::default() }).await;
        let content = crate::netsim::file(size);
        std::fs::write(seeds.join("web.bin"), &content).unwrap();
        let (_, bytes) = with_web_seed(&seeds.join("web.bin"), &web).await;
        let file = tmp.path().join("web.torrent");
        std::fs::write(&file, bytes).unwrap();
        crate::webseed::NOT_PEERS.lock().clear();
        let (path, fresh) = run(&[Url::from_file_path(&file).unwrap()], &options, &out, None, CancellationToken::new()).await.unwrap();
        assert!(crate::webseed::NOT_PEERS.lock().is_empty(), "the seed was no peer: {:?}", crate::webseed::NOT_PEERS.lock());
        assert_eq!((path.as_path(), fresh), (out.join("web.bin").as_path(), true));
        assert!(std::fs::read(&path).unwrap() == content);
        assert!(stats.served.load(Ordering::SeqCst) >= size);

        // A slow seed serving other bytes in one piece, and a seeder as slow in the swarm: both
        // serve the download at once, and the file is the torrent's, the seed's wrong piece left
        // out. The torrent is larger than what librqbit asks of all of them at once (2 MiB of the
        // seeder, 1 MiB of each of the seed's peers), so neither can hold every piece.
        let size = 8 << 20;
        let (slow_web, at_web) = crate::netsim::serve(crate::netsim::Server { size, per_client: Some(1e6), ..Default::default() }).await;
        let mut changed = crate::netsim::file(size);
        changed[200_000..210_000].fill(0xee);
        std::fs::write(seeds.join("mixed.bin"), &changed).unwrap();
        let (made, bytes) = with_web_seed(&seeds.join("mixed.bin"), &slow_web).await;
        let seeding = AddTorrentOptions {
            overwrite: true,
            output_folder: Some(seeds.to_str().unwrap().into()),
            ratelimits: LimitsConfig { upload_bps: NonZeroU32::new(1 << 20), ..Default::default() },
            ..Default::default()
        };
        let added = seeder.add_torrent(AddTorrent::from_bytes(made.as_bytes().unwrap()), Some(seeding)).await;
        let mixed = added.unwrap().into_handle().unwrap();
        mixed.wait_until_completed().await.unwrap();
        let file = tmp.path().join("mixed.torrent");
        std::fs::write(&file, bytes).unwrap();
        // The local file's link names the seeder as a magnet's `x.pe` would.
        let mut source = Url::from_file_path(&file).unwrap();
        source.set_query(Some(&format!("x.pe=127.0.0.1:{port}")));
        // What each had sent every 100 ms, and whether the seeder had the download connected.
        let (watched, done) = (mixed.clone(), CancellationToken::new());
        let (at, stop) = (at_web.clone(), done.clone());
        let samples = tokio::spawn(async move {
            let mut samples = Vec::new();
            while !stop.is_cancelled() {
                let stats = watched.stats();
                let live = stats.live.as_ref().map_or(0, |l| l.snapshot.peer_stats.live);
                samples.push((at.served.load(Ordering::SeqCst), stats.uploaded_bytes, live));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            samples
        });
        // The download's own count of peers, as it reports it.
        let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(256);
        let began = Instant::now();
        let peers = tokio::spawn(async move {
            let mut peers = Vec::new();
            while let Ok(snapshot) = rx.recv().await {
                if let Some(t) = snapshot.torrent {
                    if peers.last().is_none_or(|&(_, n)| n != t.peers) {
                        peers.push((began.elapsed().as_millis(), t.peers));
                    }
                }
            }
            peers
        });
        let running = run(std::slice::from_ref(&source), &options, &out, Some(tx), CancellationToken::new());
        let (path, _) = tokio::time::timeout(Duration::from_secs(60), running).await.expect("done within a minute").unwrap();
        done.cancel();
        let (samples, peers) = (samples.await.unwrap(), peers.await.unwrap());
        assert!(crate::webseed::NOT_PEERS.lock().is_empty(), "the seed was no peer: {:?}", crate::webseed::NOT_PEERS.lock());
        assert!(std::fs::read(&path).unwrap() == changed);
        // The swarm brought at least the piece the seed had wrong, the seed a good share of the rest.
        let sent = mixed.stats().uploaded_bytes;
        let served = at_web.served.load(Ordering::SeqCst);
        let trace = format!("(seed, swarm, seeder's live peers) every 100 ms {samples:?}; the download's peers (ms, n) {peers:?}");
        assert!(sent >= 64 * 1024 && served >= size / 4, "the swarm sent {sent} bytes, the seed {served}: {trace}");
        let both = samples.windows(4).any(|w| w[3].0 > w[0].0 && w[3].1 > w[0].1);
        assert!(both, "within 300 ms both sent some: {trace}");

        // Some files of a torrent alone (an archive.org item's): they come into a hidden folder and
        // move into the torrent's, beside what is there (a file of the user's keeps its name).
        let pick = seeds.join("pick");
        std::fs::create_dir_all(pick.join("sub")).unwrap();
        let (a, b, c) = (crate::netsim::file(100 << 10), crate::netsim::file(70 << 10), crate::netsim::file(50 << 10));
        std::fs::write(pick.join("a.bin"), &a).unwrap();
        std::fs::write(pick.join("sub").join("b.bin"), &b).unwrap();
        std::fs::write(pick.join("c.bin"), &c).unwrap();
        let pieces = CreateTorrentOptions { piece_length: Some(64 * 1024), ..Default::default() };
        let made = create_torrent(&pick, pieces, &BlockingSpawner::new(1)).await.unwrap();
        let seeding = AddTorrentOptions { overwrite: true, output_folder: Some(pick.to_str().unwrap().into()), ..Default::default() };
        let added = seeder.add_torrent(AddTorrent::from_bytes(made.as_bytes().unwrap()), Some(seeding)).await;
        added.unwrap().into_handle().unwrap().wait_until_completed().await.unwrap();
        let file = tmp.path().join("pick.torrent");
        std::fs::write(&file, made.as_bytes().unwrap()).unwrap();
        let mut source = Url::from_file_path(&file).unwrap();
        source.set_query(Some(&format!("x.pe=127.0.0.1:{port}")));
        std::fs::create_dir_all(out.join("pick")).unwrap();
        std::fs::write(out.join("pick").join("a.bin"), b"mine").unwrap();
        let chosen = DownloadOptions { torrent_files: vec!["a.bin".into(), Path::new("sub").join("b.bin")], ..DownloadOptions::default() };
        let running = run(std::slice::from_ref(&source), &chosen, &out, None, CancellationToken::new());
        let (path, fresh) = tokio::time::timeout(Duration::from_secs(60), running).await.expect("done within a minute").unwrap();
        assert_eq!((path.as_path(), fresh), (out.join("pick").as_path(), true));
        assert_eq!(std::fs::read(path.join("a.bin")).unwrap(), b"mine");
        assert!(std::fs::read(path.join("a (1).bin")).unwrap() == a);
        assert!(std::fs::read(path.join("sub").join("b.bin")).unwrap() == b);
        assert!(!path.join("c.bin").exists() && !out.join(".pick.torrent").exists());
        assert_eq!(session.with_torrents(|t| t.count()), 0, "chosen files do not seed");
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
        let run = run(std::slice::from_ref(&magnet), &options, tmp.path(), Some(tx), cancel);
        let result = tokio::time::timeout(Duration::from_secs(180), run).await.expect("a megabyte within 3 minutes");
        assert_eq!(result.unwrap_err(), CANCELLED);
        let last = seen.await.unwrap();
        println!("{last:?}");
        assert!(last.downloaded_bytes > 1 << 20 && last.target_path == Some(tmp.path().join("Sintel")));        shutdown().await;
    }
}
