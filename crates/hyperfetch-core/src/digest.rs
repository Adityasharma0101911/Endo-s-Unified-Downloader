//! The checksum a site publishes for a file, and the last checks a finished download goes
//! through: that checksum when the user gave none, a metalink's piece hashes, and the BLAKE3
//! piece hashes history keeps for `--verify` and repair (see `history::piece_size`).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use blake3::hazmat::HasherExt;
use futures_util::StreamExt;
use md5::Md5;
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_RANGE};
use reqwest::Client;
use sha1::Sha1;
use sha2::digest::DynDigest;
use sha2::{Digest, Sha256, Sha512};
use tokio::task::JoinHandle;
use url::Url;

use crate::engine::DownloadOptions;
use crate::metalink::Pieces;
use crate::range::{compute_gaps, ByteRange};
use crate::state::DownloadState;
use crate::storage::{DiskWriter, VerifyError};
use crate::worker::{authorize, send, Auth};

/// How long the sidecar files are looked for, all of them together.
const SIDECAR_BUDGET: Duration = Duration::from_secs(3);
/// Sidecar files asked for at a time.
const SIDECARS_AT_ONCE: usize = 2;
/// How long what a folder was seen not to publish is remembered (see `MISSING`).
const FOLDER_MEMORY: Duration = Duration::from_secs(10 * 60);
/// Files of one kind in a folder that had no checksum file of their own before the next of that
/// kind there are taken to have none.
const BARE_FILES: u8 = 3;
/// Most lists and kinds remembered at once.
const MAX_REMEMBERED: usize = 4096;
/// Largest sidecar file read.
const MAX_SIDECAR: usize = 64 * 1024;
/// Bytes read at a time when a finished file is read back.
const READ_BLOCK: usize = 4 * 1024 * 1024;
/// Algorithms a published checksum may use, strongest first, with their digest length in bytes.
const ALGOS: [(&str, usize); 4] = [("sha512", 64), ("sha256", 32), ("sha1", 20), ("md5", 16)];
/// What a download fails with while pieces that failed a metalink's hashes wait to be fetched
/// again; the engine then fetches them (see [`REFETCH_ROUNDS`]).
pub(crate) const REFETCH: &str = "Some pieces do not match the metalink's hashes; fetching them again";
/// Most times those pieces are fetched again, each time from one mirror, another than before when
/// there are several.
pub(crate) const REFETCH_ROUNDS: usize = 3;
/// What a download fails with while the ranges other mirrors wrote wait to be fetched again from
/// the one it went by, its whole-file hash having failed (see [`finish`]).
pub(crate) const FROM_PRIMARY: &str = "The file does not match its checksum; fetching what other mirrors sent again from the first";

/// The checksum the site publishes for the file at `url`, in the form `--checksum` takes
/// (`sha256:<hex>`, `sha512:`, `sha1:`, `md5:`), whose probe answered with `probe_headers`.
///
/// The headers come first, the strongest digest of the whole file they give: `Repr-Digest`,
/// `Content-Digest` (RFC 9530) and `Digest` (RFC 3230), `Content-MD5`, and the hashes cloud
/// storage sends (`x-goog-hash` md5, `x-amz-checksum-sha256`/`-sha1` unless they are of parts,
/// `x-ms-blob-content-md5`). Nothing is taken from an encoded answer, nor a content digest from a
/// partial one, and an ETag never counts as a hash. Then the checksum files beside it, from the
/// same site: `<url>.sha256`, `.sha512`, `.sha1`, `.md5`, then `SHA256SUMS`, `sha256sums.txt` and
/// `CHECKSUMS` in its folder, by the line for its name. Those are fetched two at a time, the
/// first in that order with a checksum taken, with GET only, each at most 64 KiB, within 3
/// seconds in all; none of a host that refused a request (429/503) lately, so none after the
/// first it refuses.
pub async fn published_checksum(client: &Client, url: &Url, probe_headers: &HeaderMap) -> Option<String> {
    published(client, None, 0, url, probe_headers, None).await
}

/// [`published_checksum`], the checksum files asked for with `auth` as the download's own
/// requests are (see `worker::authorize`): its credentials go to its own hosts alone. Each takes
/// a slot of the host's `limit` (see `crate::hosts`), as every request of a download does. Between
/// the headers and the checksum files comes the Metalink they name for the file (`described`, its
/// link and the file's size; see `metalink::described`): its strongest hash of the whole file.
async fn published(
    client: &Client,
    auth: Option<&Auth>,
    limit: usize,
    url: &Url,
    probe_headers: &HeaderMap,
    described: Option<(Url, Option<u64>)>,
) -> Option<String> {
    if let Some(found) = from_headers(probe_headers) {
        return Some(found);
    }
    // Not of a host that limits how often it is asked (see `fetch_small`).
    if let Some((metalink, size)) = described.filter(|(metalink, _)| !crate::hosts::limits_requests(metalink)) {
        // One of another size describes another file.
        let file = crate::metalink::described(client, &metalink, size, SIDECAR_BUDGET).await.filter(|f| f.size.is_none() || f.size == size);
        let found: Vec<_> = file.iter().flat_map(|f| &f.hashes).filter_map(|(algo, hex)| named(algo, hex)).collect();
        if let Some((algo, digest)) = ALGOS.iter().find_map(|(algo, _)| found.iter().find(|(name, _)| name == algo)) {
            return Some(checksum(algo, digest));
        }
    }
    from_sidecars(client, auth, limit, url).await
}

/// `algo:hex` for `digest` of `algo` (one of `ALGOS`).
pub(crate) fn checksum(algo: &str, digest: &[u8]) -> String {
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{algo}:{hex}")
}

/// The digest bytes of `value`, base64 (or hex, as some servers send) of a digest named `algo`
/// (`SHA-256`, `sha-512`, `SHA` for SHA-1, `MD5`), if it has that algorithm's length. Returns the
/// algorithm as `ALGOS` names it.
fn named(algo: &str, value: &str) -> Option<(&'static str, Vec<u8>)> {
    let algo = algo.trim().to_ascii_lowercase().replace('-', "");
    let algo = if algo == "sha" { "sha1" } else { algo.as_str() };
    let &(name, len) = ALGOS.iter().find(|(name, _)| *name == algo)?;
    let value = value.trim();
    let base64 = base64::engine::general_purpose::STANDARD.decode(value).ok().filter(|d| d.len() == len);
    let digest = base64.or_else(|| from_hex(value).filter(|d| d.len() == len))?;
    Some((name, digest))
}

fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

/// The strongest checksum of the whole file the headers of its probe give (see
/// [`published_checksum`]).
fn from_headers(headers: &HeaderMap) -> Option<String> {
    let all = |name: &str| headers.get_all(name).iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(",");
    // Digests of an encoded body are not of the file as saved.
    let encoding = all(CONTENT_ENCODING.as_str());
    if !encoding.trim().is_empty() && !encoding.trim().eq_ignore_ascii_case("identity") {
        return None;
    }
    // A partial answer's content digests are of that part.
    let whole_content = !headers.contains_key(CONTENT_RANGE);
    let mut found = Vec::new();
    // `sha-256=:<base64>:` (RFC 9530) and `SHA-256=<base64>` (RFC 3230), comma-separated.
    for (header, applies) in [("repr-digest", true), ("content-digest", whole_content), ("digest", true)] {
        for item in all(header).split(',').filter(|_| applies) {
            if let Some((algo, value)) = item.split_once('=') {
                found.extend(named(algo, value.trim().trim_matches(':')));
            }
        }
    }
    if whole_content {
        found.extend(named("md5", &all("content-md5")));
    }
    // `crc32c=...,md5=...`, of the object as stored: not of a gzipped one served unzipped.
    let stored = all("x-goog-stored-content-encoding");
    let unzipped = !stored.trim().is_empty() && !stored.trim().eq_ignore_ascii_case("identity");
    found.extend(all("x-goog-hash").split(',').filter(|_| !unzipped).filter_map(|item| match item.trim().split_once('=') {
        Some(("md5", value)) => named("md5", value),
        _ => None,
    }));
    // S3 marks a checksum of the parts of an upload with a `-N` suffix.
    for algo in ["sha256", "sha1"] {
        let value = all(&format!("x-amz-checksum-{algo}"));
        if !value.contains('-') {
            found.extend(named(algo, &value));
        }
    }
    found.extend(named("md5", &all("x-ms-blob-content-md5")));
    ALGOS.iter().find_map(|(algo, _)| found.iter().find(|(name, _)| name == algo)).map(|(algo, digest)| checksum(algo, digest))
}

/// The checksum the files beside `url` give for it (see [`published_checksum`]). None is asked
/// for of a host that limits how often it is asked (see `hosts::limits_requests`), nor of one
/// whose downloads found none beside them lately (see `hosts::checksums`); what they find is kept
/// for that. Until the host is known to publish some, the downloads of one folder ask in turn,
/// each with what the one before learned: files that start together cost one set of lookups, not
/// one each. The wait for a turn is part of the 3 seconds.
async fn from_sidecars(client: &Client, auth: Option<&Auth>, limit: usize, url: &Url) -> Option<String> {
    let skipped = || !matches!(url.scheme(), "http" | "https") || crate::hosts::limits_requests(url) || crate::hosts::checksums(url) == Some(false);
    if skipped() {
        return None;
    }
    let name = url.path_segments()?.next_back().filter(|n| !n.is_empty())?;
    let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy().into_owned();
    let mut file = url.clone();
    file.set_query(None);
    file.set_fragment(None);
    let deadline = tokio::time::Instant::now() + SIDECAR_BUDGET;
    let held = match file.join(".") {
        Ok(folder) if crate::hosts::checksums(url) != Some(true) => Some(tokio::time::timeout_at(deadline, turn(folder.as_str()).lock_owned()).await.ok()?),
        _ => None,
    };
    if skipped() {
        return None;
    }
    // Known meanwhile to publish some: the others go on at once.
    let _held = held.filter(|_| crate::hosts::checksums(url) != Some(true));
    let beside = ["sha256", "sha512", "sha1", "md5"].map(|ext| {
        let mut sidecar = file.clone();
        sidecar.set_path(&format!("{}.{ext}", file.path()));
        (sidecar, Some(ext))
    });
    let folder = ["SHA256SUMS", "sha256sums.txt", "CHECKSUMS"].map(|list| file.join(list).ok().map(|u| (u, None)));
    // The folder's files of this kind, by their extension: the next of them go by what these had.
    let ext = name.rsplit_once('.').map_or("", |(_, ext)| ext).to_ascii_lowercase();
    let kind = file.join(".").ok().map(|folder| format!("{folder}*.{ext}"));
    let bare = kind.as_deref().is_some_and(|kind| missing(kind, BARE_FILES));
    let own = beside.len() as u8;
    let asked: Vec<_> = beside.into_iter().filter(|_| !bare).chain(folder.into_iter().flatten().filter(|(list, _)| !missing(list.as_str(), 1))).collect();
    let asking = asked.len();
    // A few at a time, over the connections the probe left open: a finished download waits for
    // them, and a host that takes few connections keeps the rest for the download. The first in
    // this order that names a checksum wins, whichever answers first.
    let name = &name;
    // How many of the file's own were said not to be there, and whether one named its checksum;
    // how many of all asked for were said not to be there.
    let (none_beside, one_beside, none_at_all) = (&AtomicU8::new(0), &AtomicU8::new(0), &AtomicUsize::new(0));
    let lookups: Vec<_> = asked
        .into_iter()
        .map(|(sidecar, ext)| async move {
            match fetch_small(client, auth, limit, sidecar.clone()).await {
                Sidecar::Text(text) => {
                    let found = checksum_line(&text, name, ext);
                    if found.is_some() && ext.is_some() {
                        one_beside.store(1, Ordering::Relaxed);
                    }
                    found
                }
                Sidecar::Missing => {
                    none_at_all.fetch_add(1, Ordering::Relaxed);
                    match ext {
                        Some(_) => _ = none_beside.fetch_add(1, Ordering::Relaxed),
                        None => note_missing(sidecar.into()),
                    }
                    None
                }
                Sidecar::Unknown => None,
            }
        })
        .collect();
    let lookups = futures_util::stream::iter(lookups).buffered(SIDECARS_AT_ONCE).filter_map(std::future::ready);
    let found = tokio::time::timeout_at(deadline, std::pin::pin!(lookups).next()).await;
    let none = found.is_ok() && none_beside.load(Ordering::Relaxed) == own;
    if let Some(kind) = kind {
        if one_beside.load(Ordering::Relaxed) > 0 {
            MISSING.lock().remove(&kind);
        } else if none {
            note_missing(kind);
        }
    }
    // Every one asked for not there: the host publishes none, and the downloads from it after this
    // one ask for none (see `hosts::checksums`); one found takes that back.
    let all_missing = found.is_ok() && asking > 0 && none_at_all.load(Ordering::Relaxed) == asking;
    let found = found.ok().flatten();
    if found.is_some() || all_missing {
        crate::hosts::record_checksums(url, found.is_some());
    }
    found
}

/// What asking for a checksum file gave.
enum Sidecar {
    Text(String),
    /// The server says it is not there, or sent a web page or more than `MAX_SIDECAR` in its place.
    Missing,
    /// Nothing to go by: no answer, or a server that is busy or failing.
    Unknown,
}

/// What sites were seen not to publish, for the downloads that follow: a checksum list that is
/// not in its folder (by its URL), and how many of a folder's files of one kind had no checksum
/// file of their own (by `<folder>*.<extension>`). Each with when it was last seen so.
static MISSING: LazyLock<Mutex<HashMap<String, (Instant, u8)>>> = LazyLock::new(Default::default);

/// Whose turn it is to ask for a folder's checksum files, by the folder's URL (see `from_sidecars`).
static TURNS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);

fn turn(folder: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut turns = TURNS.lock();
    if turns.len() >= MAX_REMEMBERED {
        turns.retain(|_, turn| Arc::strong_count(turn) > 1);
    }
    Arc::clone(turns.entry(folder.to_string()).or_default())
}

/// Whether `key` was seen missing `times` or more within `FOLDER_MEMORY`.
fn missing(key: &str, times: u8) -> bool {
    MISSING.lock().get(key).is_some_and(|(seen, count)| seen.elapsed() < FOLDER_MEMORY && *count >= times)
}

/// Counts `key` as seen missing once more.
fn note_missing(key: String) {
    let mut all = MISSING.lock();
    if all.len() >= MAX_REMEMBERED {
        all.clear();
    }
    let now = Instant::now();
    let seen = all.entry(key).or_insert((now, 0));
    *seen = (now, if seen.0.elapsed() < FOLDER_MEMORY { seen.1.saturating_add(1) } else { 1 });
}

/// The checksum file at `url`: one that is there, not a web page and at most `MAX_SIDECAR`. Not
/// asked for of a host that limits how often it is asked (see `hosts::limits_requests`): the
/// download's own requests need what it allows. A refusal (429/503)
/// rests the host and so ends the lookups after it.
async fn fetch_small(client: &Client, auth: Option<&Auth>, limit: usize, url: Url) -> Sidecar {
    let slot = crate::hosts::acquire(&url, limit).await;
    if crate::hosts::limits_requests(&url) {
        return Sidecar::Unknown;
    }
    let request = |url: &Url| authorize(client.get(url.clone()), auth, url).header(ACCEPT_ENCODING, "identity");
    let Ok(mut resp) = send(request, &url).await else { return Sidecar::Unknown };
    let status = resp.status();
    if matches!(status.as_u16(), 429 | 503) {
        slot.throttled(crate::worker::retry_after(resp.headers()).unwrap_or_default());
        return Sidecar::Unknown;
    }
    // Busy (408, 429) is no answer about the file.
    if status.is_client_error() && !matches!(status.as_u16(), 408 | 429) {
        return Sidecar::Missing;
    }
    if !status.is_success() {
        return Sidecar::Unknown;
    }
    if crate::resolver::html_type(resp.headers()) || resp.content_length().is_some_and(|len| len > MAX_SIDECAR as u64) {
        return Sidecar::Missing;
    }
    let mut body = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            Ok(None) => return Sidecar::Text(String::from_utf8_lossy(&body).into_owned()),
            Err(_) => return Sidecar::Unknown,
        }
        if body.len() > MAX_SIDECAR {
            return Sidecar::Missing;
        }
    }
}

/// The checksum the checksum file `text` gives for the file `name`: a line `<hex>  <name>`
/// (`*<name>` too) or `SHA256 (<name>) = <hex>`; the line for the file itself (`./<name>` too)
/// before one for a file of that name in a folder. In a file named after the download
/// (`<url>.<ext>`), whose extension names the algorithm, the hex may stand alone. Elsewhere the
/// algorithm goes by the hex's length.
fn checksum_line(text: &str, name: &str, ext: Option<&str>) -> Option<String> {
    let line_for = |same: &dyn Fn(&str) -> bool| {
        text.lines().find_map(|line| {
            let line = line.trim();
            let bsd = line.split_once(" (").and_then(|(tag, rest)| Some((tag, rest.rsplit_once(") = ")?)));
            let (tag, hex, file) = match bsd {
                Some((tag, (file, hex))) => (Some(tag), hex.trim(), Some(file)),
                None => {
                    let (hex, file) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
                    let file = file.trim().trim_start_matches('*');
                    (None, hex, (!file.is_empty()).then_some(file))
                }
            };
            match file {
                Some(file) if !same(file) => return None,
                None if ext.is_none() => return None,
                _ => {}
            }
            let digest = from_hex(hex)?;
            let algo = match tag.or(ext) {
                Some(algo) => named(algo, hex)?.0,
                None => ALGOS.iter().find(|(_, len)| *len == digest.len())?.0,
            };
            (ALGOS.iter().any(|&(name, len)| name == algo && len == digest.len())).then(|| checksum(algo, &digest))
        })
    };
    line_for(&|file| file.strip_prefix("./").or(file.strip_prefix(".\\")).unwrap_or(file) == name)
        .or_else(|| line_for(&|file| file.rsplit(['/', '\\']).next() == Some(name)))
}

/// "SHA-256" and so on, for `algo` as `ALGOS` names it.
fn algo_name(algo: &str) -> String {
    match algo {
        "sha1" => "SHA-1".to_string(),
        "md5" => "MD5".to_string(),
        other => other.to_ascii_uppercase().replace("SHA", "SHA-"),
    }
}

/// The site's published checksum for one download: being looked for, then what confirmed the
/// file ("the site's SHA-256"). Shared by the clones of the download's engine.
#[derive(Default)]
pub(crate) struct Site {
    search: Option<JoinHandle<Option<String>>>,
    /// Lets `search` ask the site for anything (see [`go`]).
    go: Option<tokio::sync::watch::Sender<bool>>,
    /// What `search` gives, as soon as it is found: the download's writer hashes the file with it
    /// as the file arrives (see [`found`]).
    found: Arc<OnceLock<String>>,
    pub(crate) note: Option<String>,
    /// The mirror the download went by, and the ranges written from others in its last run: a
    /// whole-file hash that fails has those fetched again from it alone (`primary_only`).
    pub(crate) primary: Option<Url>,
    pub(crate) copies: Vec<ByteRange>,
    pub(crate) primary_only: bool,
}

/// Starts looking for the site's published checksum of the download at `url`, whose probe
/// answered with `headers`, unless the user gave a checksum or turned this off
/// (`DownloadOptions::remote_checksum`): at once in the headers, then, once the download lets it
/// (see [`go`]), in its checksum files, asked for with `auth`, and the Metalink they name
/// (`described`). The download's own requests come first: those are optional.
pub(crate) fn look_for(
    site: &Mutex<Site>,
    options: &DownloadOptions,
    client: &Client,
    auth: Option<Arc<Auth>>,
    url: &Url,
    headers: &HeaderMap,
    described: Option<(Url, Option<u64>)>,
) {
    let found = Arc::new(OnceLock::new());
    let (go, mut gate) = tokio::sync::watch::channel(false);
    let search = (options.remote_checksum && options.expected_checksum.is_none()).then(|| {
        // One in the headers is known at once, and nothing need be asked.
        if let Some(checksum) = from_headers(headers) {
            let _ = found.set(checksum.clone());
            return tokio::spawn(async move { Some(checksum) });
        }
        let (client, url, headers, limit, slot) = (client.clone(), url.clone(), headers.clone(), options.max_connections_per_host, Arc::clone(&found));
        tokio::spawn(async move {
            // A download that ends without letting it (see `go`) asks nothing.
            gate.wait_for(|&go| go).await.ok()?;
            let checksum = published(&client, auth.as_deref(), limit, &url, &headers, described).await;
            if let Some(checksum) = &checksum {
                let _ = slot.set(checksum.clone());
            }
            checksum
        })
    });
    // What a run before left of the mirrors (`primary`, `primary_only`) holds for this one.
    let mut site = site.lock();
    (site.search, site.go, site.found, site.note) = (search, Some(go), found, None);
}

/// Lets the search [`look_for`] started ask the site for the checksum files and Metalink: once
/// the download sent the requests it widens with, or has every byte. Those still go to no host
/// that limits how often it is asked or was seen to publish none (see `from_sidecars`).
pub(crate) fn go(site: &Mutex<Site>) {
    if let Some(go) = &site.lock().go {
        go.send_replace(true);
    }
}

/// Holds the download to `checksum` (`algo:hex`) in place of what [`look_for`] looks for: the
/// checksum of a file put together from an older version (see `crate::delta`).
pub(crate) fn expect(site: &Mutex<Site>, checksum: String) {
    let mut site = site.lock();
    if let Some(search) = site.search.take() {
        search.abort();
    }
    site.found = Arc::new(OnceLock::from(checksum.clone()));
    site.search = Some(tokio::spawn(async move { Some(checksum) }));
}

/// The checksum the download is to be held to besides the user's (see [`look_for`] and
/// [`expect`]), once it is known; the file is hashed with it as it arrives.
pub(crate) fn found(site: &Mutex<Site>) -> Option<String> {
    site.lock().found.get().cloned()
}

/// Why the engine does not keep a finished download (see [`finish`]).
pub(crate) enum Unfinished {
    /// The file could not be read; it stays for another try.
    Io(String),
    /// Its contents are wrong; it is discarded.
    Wrong(String),
    /// Pieces that failed the metalink's hashes are marked missing in its state, to be fetched
    /// again ([`REFETCH`]).
    Refetch,
    /// What other mirrors wrote is marked missing, to be fetched again from the one the download
    /// went by ([`FROM_PRIMARY`]).
    FromPrimary,
}

/// The last checks of the finished `.part` at `part`, once the engine checked it against the
/// expected checksum (`verified`, its BLAKE3 as hex): a metalink's piece hashes when the whole
/// file failed or has no hash, then the site's published checksum (see [`look_for`]), which a
/// download that finishes sooner waits for until its time is up. Also keeps the file's BLAKE3
/// piece hashes for history: from the blocks `writer` hashed as it wrote them, else from a read
/// the other checks make anyway (a download in one stream, which cannot be repaired by ranges
/// from its server, has none otherwise). The site's checksum comes from `writer` too when it
/// hashed the file with it as it arrived (see [`found`]). Reads the file once for all of them,
/// and only when one is needed. `quiet` is called once nothing more is to be asked of any host:
/// the search has its answer (or ran out of time), and what is left is read from disk. Gives up
/// with [`CANCELLED`](crate::engine::CANCELLED) as soon as `cancel` is, the file left as it is.
/// Returns its BLAKE3 as hex.
pub(crate) async fn finish(
    part: &Path,
    verified: Result<String, VerifyError>,
    options: &DownloadOptions,
    site: &Mutex<Site>,
    writer: Option<DiskWriter>,
    cancel: &tokio_util::sync::CancellationToken,
    quiet: impl FnOnce(),
) -> Result<String, Unfinished> {
    let cancelled = || Unfinished::Io(crate::engine::CANCELLED.to_string());
    let copies = {
        let mut site = site.lock();
        if site.primary_only { Vec::new() } else { std::mem::take(&mut site.copies) }
    };
    let (hash, whole_failed) = match verified {
        Ok(hash) => (Some(hash), None),
        Err(VerifyError::Io(e)) => return Err(Unfinished::Io(e)),
        Err(VerifyError::Mismatch(e)) if options.metalink_pieces.is_some() => (None, Some(e)),
        Err(VerifyError::Mismatch(e)) => return Err(from_primary(part, copies, e).await),
    };
    // Every byte is in: nothing of the download's own is left to ask for.
    go(site);
    let search = site.lock().search.take();
    let published = match search {
        Some(mut search) => tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                search.abort();
                return Err(cancelled());
            }
            found = &mut search => found.ok().flatten(),
        },
        None => None,
    };
    quiet();
    let metalink = options.metalink_pieces.clone().filter(|_| whole_failed.is_some() || options.expected_checksum.is_none());
    let (path, wanted, stop) = (part.to_path_buf(), published.clone(), cancel.clone());
    let read = tokio::task::spawn_blocking(move || {
        let size = std::fs::metadata(&path)?.len();
        let piece = crate::history::piece_size(size);
        let followed = wanted.as_deref().zip(writer.as_ref()).map(|(wanted, writer)| writer.followed(wanted)).transpose()?.flatten();
        let kept = piece.zip(writer).map(|(piece, writer)| writer.piece_hashes(piece)).transpose()?;
        let read_pieces = piece.filter(|_| kept.is_none() && (wanted.is_some() || metalink.is_some()));
        let mut pass = read_pass_until(&path, wanted.as_deref().filter(|_| followed.is_none()), metalink.as_ref(), read_pieces, &|| stop.is_cancelled())?;
        pass.published = followed.or(pass.published);
        if !pass.bad.is_empty() {
            return Ok(Err(refetch(&path, size, &pass.bad)));
        }
        pass.blake3 = kept.unwrap_or(pass.blake3);
        Ok(Ok((pass, piece)))
    });
    let read = read.await.map_err(|e| Unfinished::Io(format!("Background task failed: {e}")))?;
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let (pass, piece) = read.map_err(|e: io::Error| Unfinished::Io(format!("Failed to read {}: {}", part.display(), e)))??;
    if let Some(e) = whole_failed {
        return Err(Unfinished::Wrong(e));
    }
    let hash = hash.unwrap_or_default();
    if let (Some((algo, expected)), Some(actual)) = (published.as_deref().and_then(|c| c.split_once(':')), &pass.published) {
        if expected != actual {
            let e = format!("Checksum verification failed against the site's published {}: expected {}, got {}", algo_name(algo), expected, actual);
            return Err(from_primary(part, copies, e).await);
        }
        site.lock().note = Some(format!("the site's {}", algo_name(algo)));
    }
    if let Some(piece) = piece.filter(|_| !pass.blake3.is_empty()) {
        let (history, hash) =(crate::history::DownloadHistoryManager::default_history_path(), hash.clone());
        let saved = tokio::task::spawn_blocking(move || crate::history::save_pieces(&history, &hash, piece, &pass.blake3)).await;
        if let Ok(Err(e)) = saved {
            tracing::warn!("Could not keep the piece hashes of {}: {}", part.display(), e);
        }
    }
    Ok(hash)
}

/// Marks `bad` missing in the state of the `size`-byte `.part` at `part`, all else complete, for
/// the engine to fetch them again. Without a state to resume from, the file is wrong. Blocking.
fn refetch(part: &Path, size: u64, bad: &[ByteRange]) -> Unfinished {
    match mark_missing(part, size, bad) {
        Ok(true) => {
            tracing::warn!("{} piece(s) of {} do not match the metalink's hashes; fetching them again", bad.len(), part.display());
            Unfinished::Refetch
        }
        Ok(false) => {
            let bytes: u64 = bad.iter().map(ByteRange::len).sum();
            Unfinished::Wrong(format!("{} piece(s), {} bytes, do not match the metalink's hashes", bad.len(), bytes))
        }
        Err(e) => Unfinished::Io(e),
    }
}

/// Marks `bad` missing in the state of the `size`-byte `.part` at `part`, all else complete;
/// false without a state of that size. Blocking.
fn mark_missing(part: &Path, size: u64, bad: &[ByteRange]) -> Result<bool, String> {
    let state_path = DownloadState::state_file_path(part);
    let Some(mut state) = DownloadState::load_from_path(&state_path).ok().flatten().filter(|s| s.file_size == size) else {
        return Ok(false);
    };
    // The rest is on disk before the state says so.
    OpenOptions::new().write(true).open(part).and_then(|f| f.sync_data()).map_err(|e| format!("Failed to flush {}: {}", part.display(), e))?;
    state.completed_ranges = compute_gaps(size, bad);
    // Mirrors serve the file under validators of their own: the hashes hold it now.
    (state.etag, state.last_modified) = (None, None);
    state.save_atomic(&state_path).map_err(|e| format!("Failed to save download state: {}", e))?;
    Ok(true)
}

/// A file that failed its whole-file hash (`error`) with `copies`, ranges other mirrors than the
/// one the download went by wrote: those are marked missing, to be fetched again from that one
/// ([`Unfinished::FromPrimary`]), as a mirror may serve another version under the same size. With
/// none, or no state to mark them in, the file is wrong.
async fn from_primary(part: &Path, copies: Vec<ByteRange>, error: String) -> Unfinished {
    if copies.is_empty() {
        return Unfinished::Wrong(error);
    }
    let part = part.to_path_buf();
    let marked = tokio::task::spawn_blocking(move || {
        let size = std::fs::metadata(&part).map_err(|e| format!("Failed to read {}: {}", part.display(), e))?.len();
        let marked = mark_missing(&part, size, &copies)?;
        if marked {
            let bytes: u64 = copies.iter().map(ByteRange::len).sum();
            tracing::warn!("{} does not match its checksum: fetching the {} bytes other mirrors sent again from the first", part.display(), bytes);
        }
        Ok::<bool, String>(marked)
    });
    match marked.await {
        Ok(Ok(true)) => Unfinished::FromPrimary,
        Ok(Err(e)) => Unfinished::Io(e),
        _ => Unfinished::Wrong(error),
    }
}

/// The error a download ends with, once [`REFETCH`] rounds are used up.
pub(crate) fn refetch_failed(error: String) -> String {
    if error != REFETCH {
        return error;
    }
    "Pieces of the file still do not match the metalink's hashes after fetching them again; \
     the partial download was kept"
        .to_string()
}

/// A hash taken of each piece of a file read in order.
enum Hasher {
    /// The piece's BLAKE3 chaining value: a subtree of the file's hash.
    Tree(blake3::Hasher),
    Flat(Box<dyn DynDigest + Send>),
}

impl Hasher {
    fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Tree(h) => {
                h.update_rayon(data);
            }
            Hasher::Flat(h) => h.update(data),
        }
    }

    fn finish(self) -> Vec<u8> {
        match self {
            Hasher::Tree(h) => h.finalize_non_root().to_vec(),
            Hasher::Flat(h) => h.finalize().to_vec(),
        }
    }
}

/// A hasher of `algo` (one of `ALGOS`) for a piece at any offset.
fn flat(algo: &str) -> Option<fn(u64) -> Hasher> {
    let new: fn(u64) -> Hasher = match algo {
        "sha512" => |_| Hasher::Flat(Box::new(Sha512::new())),
        "sha256" => |_| Hasher::Flat(Box::new(Sha256::new())),
        "sha1" => |_| Hasher::Flat(Box::new(Sha1::new())),
        "md5" => |_| Hasher::Flat(Box::new(Md5::new())),
        _ => return None,
    };
    Some(new)
}

/// A BLAKE3 hasher for the piece at `offset` of a file of several pieces.
fn tree(offset: u64) -> Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.set_input_offset(offset);
    Hasher::Tree(hasher)
}

/// The digests of each `len`-byte piece of a `size`-byte file, fed its bytes in order.
struct Pieced {
    len: u64,
    size: u64,
    at: u64,
    new: fn(u64) -> Hasher,
    current: Option<Hasher>,
    done: Vec<Vec<u8>>,
}

impl Pieced {
    fn new(len: u64, size: u64, new: fn(u64) -> Hasher) -> Self {
        Self { len: len.max(1), size, at: 0, new, current: None, done: Vec::new() }
    }

    fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let end = ((self.at / self.len + 1) * self.len).min(self.size);
            let n = ((end - self.at) as usize).min(data.len());
            let start = self.at - self.at % self.len;
            self.current.get_or_insert_with(|| (self.new)(start)).update(&data[..n]);
            (self.at, data) = (self.at + n as u64, &data[n..]);
            if self.at == end {
                self.done.extend(self.current.take().map(Hasher::finish));
            }
        }
    }

    /// The pieces' digests; an empty file's one.
    fn finish(mut self) -> Vec<Vec<u8>> {
        if self.size == 0 {
            self.done.push((self.new)(0).finish());
        }
        self.done
    }
}

/// What one read of a finished file found (see [`read_pass`]).
#[derive(Default)]
pub(crate) struct Pass {
    /// The file's digest in the algorithm of the checksum asked for, as lowercase hex.
    pub(crate) published: Option<String>,
    /// The pieces that do not match the metalink's hashes.
    pub(crate) bad: Vec<ByteRange>,
    /// Its BLAKE3 piece hashes (chaining values), when asked for.
    pub(crate) blake3: Vec<[u8; 32]>,
}

/// Reads the file at `path` once, if anything is asked of it: its digest in the algorithm of the
/// checksum `published` (`sha256:<hex>`), which pieces fail the `metalink`'s hashes (unless those
/// do not fit its size), and its BLAKE3 hashes of `piece`-byte pieces, which must be a power of
/// two of at least 1 KiB, smaller than the file. Blocking.
pub(crate) fn read_pass(path: &Path, published: Option<&str>, metalink: Option<&Pieces>, piece: Option<u64>) -> io::Result<Pass> {
    read_pass_until(path, published, metalink, piece, &|| false)
}

/// [`read_pass`], given up with an `Interrupted` error once `stop` says so, looked at before each
/// block.
fn read_pass_until(path: &Path, published: Option<&str>, metalink: Option<&Pieces>, piece: Option<u64>, stop: &dyn Fn() -> bool) -> io::Result<Pass> {
    let whole_algo = published.and_then(|c| flat(c.split_once(':')?.0));
    if whole_algo.is_none() && metalink.is_none() && piece.is_none() {
        return Ok(Pass::default());
    }
    #[cfg(test)]
    crate::storage::READ_BACK.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(path.to_path_buf());
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut whole = whole_algo.map(|new| Pieced::new(size, size, new));
    let metalink = metalink.filter(|m| m.fit(size));
    let mut pieces = metalink.and_then(|m| Some(Pieced::new(m.length, size, flat(&m.algo)?)));
    let mut blake = piece.filter(|&p| p < size).map(|p| Pieced::new(p, size, tree));
    let mut buf = vec![0; READ_BLOCK];
    let mut at = 0;
    while at < size {
        if stop() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, crate::engine::CANCELLED));
        }
        let n = (size - at).min(READ_BLOCK as u64) as usize;
        file.read_exact(&mut buf[..n])?;
        for hashed in [&mut whole, &mut pieces, &mut blake].into_iter().flatten() {
            hashed.update(&buf[..n]);
        }
        at += n as u64;
    }
    let hex = |digest: &[u8]| digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut pass = Pass {
        published: whole.and_then(|w| w.finish().first().map(|d| hex(d))),
        blake3: blake.map_or_else(Vec::new, |b| b.finish().iter().map(|d| d.as_slice().try_into().unwrap_or_default()).collect()),
        ..Pass::default()
    };
    if let (Some(pieces), Some(metalink)) = (pieces, metalink) {
        let len = metalink.length;
        for (i, (digest, expected)) in pieces.finish().iter().zip(&metalink.hashes).enumerate() {
            if hex(digest) != *expected {
                let start = i as u64 * len;
                pass.bad.extend(ByteRange::from_len(start, len.min(size - start)).ok());
            }
        }
    }
    Ok(pass)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::broadcast;
    use crate::engine::{DownloadEngine, EngineSnapshot};

    const MIB: usize = 1024 * 1024;

    /// How [`serve`] answers.
    #[derive(Clone, Default)]
    struct Site {
        /// Header lines added to the file's answers.
        headers: Vec<String>,
        /// Files beside it: (path, content type, body).
        files: Vec<(String, String, String)>,
        /// Bytes of the file sent damaged (inverted) the first time they are asked for.
        damaged_once: Option<std::ops::Range<usize>>,
        /// HEAD and one-byte probes, a mirror's beside the first, go unanswered: it takes part
        /// only as the first (or only) mirror, whose probe asks for the file's start.
        only_as_first: bool,
        /// Sidecar answers, a 404 too, wait this long.
        sidecars_wait: Duration,
        /// The most sidecars that were waiting for their answer at once.
        sidecars_at_once: Arc<std::sync::atomic::AtomicUsize>,
        /// Every answer is sent 64 KiB at a time, each piece after this long.
        pace: Duration,
        /// Sidecars are refused with 429.
        sidecars_refused: bool,
    }

    /// Serves `content` at `/dir/file.bin` (HEAD, GET and ranges) and `site.files` beside it, any
    /// other path a 404. Returns the file's URL and every request's path and range, in order.
    async fn serve(content: Vec<u8>, site: Site) -> (Url, Arc<std::sync::Mutex<Vec<(String, Option<(u64, u64)>)>>>) {
        let listener = crate::hosts::unseen_listener().await;
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (content, site, seen) = (Arc::new(content), Arc::new(site), Arc::clone(&log));
        let damaged = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let busy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (content, site, seen, damaged) = (Arc::clone(&content), Arc::clone(&site), Arc::clone(&seen), Arc::clone(&damaged));
                let busy = Arc::clone(&busy);
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let req = String::from_utf8_lossy(&req).to_ascii_lowercase();
                    let mut words = req.split_whitespace();
                    let (method, path) = (words.next().unwrap_or("").to_string(), words.next().unwrap_or("").to_string());
                    let len = content.len() as u64;
                    let range = req.lines().find_map(|l| l.strip_prefix("range: bytes=")).and_then(|spec| {
                        let (s, e) = spec.trim().split_once('-')?;
                        Some((s.parse::<u64>().ok()?, e.parse::<u64>().map_or(len - 1, |e| e.min(len - 1))))
                    });
                    seen.lock().unwrap().push((path.clone(), range));
                    if site.only_as_first && (method == "head" || range == Some((0, 0))) {
                        // Held unanswered until the test is over (at most a minute).
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        return;
                    }
                    let resp = if path == "/dir/file.bin" {
                        let (s, e) = range.unwrap_or((0, len - 1));
                        let mut body = content[s as usize..=e as usize].to_vec();
                        if let Some(bad) = site.damaged_once.as_ref().filter(|_| method != "head") {
                            if (s as usize) < bad.end && e as usize >= bad.start && !damaged.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                for (i, b) in body.iter_mut().enumerate() {
                                    if bad.contains(&(s as usize + i)) {
                                        *b ^= 0xff;
                                    }
                                }
                            }
                        }
                        let status = if range.is_some() { "206 Partial Content" } else { "200 OK" };
                        let mut head = format!("HTTP/1.1 {status}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\n", body.len());
                        if range.is_some() {
                            head += &format!("Content-Range: bytes {s}-{e}/{len}\r\n");
                        }
                        for line in &site.headers {
                            head += &format!("{line}\r\n");
                        }
                        let mut resp = (head + "Connection: close\r\n\r\n").into_bytes();
                        if method != "head" {
                            resp.extend_from_slice(&body);
                        }
                        resp
                    } else {
                        let waiting = busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        site.sidecars_at_once.fetch_max(waiting, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(site.sidecars_wait).await;
                        busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        match site.files.iter().find(|(p, _, _)| p.to_ascii_lowercase() == path) {
                            _ if site.sidecars_refused => {
                                b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                            }
                            Some((_, kind, body)) => {
                                format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                                    .into_bytes()
                            }
                            None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        }
                    };
                    for piece in resp.chunks(if site.pace.is_zero() { resp.len().max(1) } else { 64 * 1024 }) {
                        tokio::time::sleep(site.pace).await;
                        if sock.write_all(piece).await.is_err() {
                            return;
                        }
                    }
                    let _ = sock.shutdown().await;
                });
            }
        });
        (Url::parse(&format!("http://{addr}/dir/file.bin")).unwrap(), log)
    }

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 ^ (i >> 16) as u8).collect()
    }

    fn b64(digest: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(digest)
    }

    fn sha256_hex(data: &[u8]) -> String {
        checksum("sha256", &Sha256::digest(data)).split_off(7)
    }

    /// The checksum the headers of the answer to a GET for `range` give, the sidecars aside.
    async fn from_answer(headers: &[String], range: Option<&str>) -> Option<String> {
        let (url, _) = serve(content(100), Site { headers: headers.to_vec(), ..Site::default() }).await;
        let client = Client::new();
        let mut request = client.get(url.clone());
        if let Some(range) = range {
            request = request.header("Range", range);
        }
        let resp = request.send().await.unwrap();
        published_checksum(&client, &url, resp.headers()).await
    }

    #[tokio::test]
    async fn every_header_form_gives_the_whole_files_checksum() {
        let data = content(100);
        let (sha256, sha512) = (Sha256::digest(&data).to_vec(), Sha512::digest(&data).to_vec());
        let (sha1, md5) = (Sha1::digest(&data).to_vec(), Md5::digest(&data).to_vec());
        let partial = Some("bytes=0-9");
        for (header, range, expected) in [
            (format!("Repr-Digest: sha-256=:{}:", b64(&sha256)), partial, checksum("sha256", &sha256)),
            (format!("Content-Digest: sha-512=:{}:", b64(&sha512)), None, checksum("sha512", &sha512)),
            (format!("Digest: SHA-256={}", b64(&sha256)), partial, checksum("sha256", &sha256)),
            (format!("Digest: SHA={},MD5={}", b64(&sha1), b64(&md5)), None, checksum("sha1", &sha1)),
            (format!("Content-MD5: {}", b64(&md5)), None, checksum("md5", &md5)),
            (format!("x-goog-hash: crc32c=n03x6A==, md5={}", b64(&md5)), partial, checksum("md5", &md5)),
            (format!("x-amz-checksum-sha256: {}", b64(&sha256)), None, checksum("sha256", &sha256)),
            (format!("x-amz-checksum-sha1: {}", b64(&sha1)), None, checksum("sha1", &sha1)),
            (format!("x-ms-blob-content-md5: {}", b64(&md5)), partial, checksum("md5", &md5)),
            // Hex where base64 belongs, as some servers send it.
            (format!("Content-MD5: {}", checksum("md5", &md5).split_off(4)), None, checksum("md5", &md5)),
        ] {
            assert_eq!(from_answer(std::slice::from_ref(&header), range).await, Some(expected), "{header}");
        }
        // The strongest of several wins.
        let both = [format!("Content-MD5: {}", b64(&md5)), format!("Repr-Digest: sha-256=:{}:", b64(&sha256))];
        assert_eq!(from_answer(&both, None).await, Some(checksum("sha256", &sha256)));
    }

    #[tokio::test]
    async fn digests_of_a_part_an_encoding_or_parts_and_etags_are_no_checksum() {
        let data = content(100);
        let (sha256, md5) = (Sha256::digest(&data).to_vec(), Md5::digest(&data).to_vec());
        let partial = Some("bytes=0-9");
        for (headers, range) in [
            (vec![format!("Content-Digest: sha-256=:{}:", b64(&sha256))], partial),
            (vec![format!("Content-MD5: {}", b64(&md5))], partial),
            (vec![format!("Digest: SHA-256={}", b64(&sha256)), "Content-Encoding: gzip".into()], None),
            (vec![format!("Repr-Digest: sha-256=:{}:", b64(&sha256)), "Content-Encoding: br".into()], partial),
            (vec![format!("x-amz-checksum-sha256: {}-3", b64(&sha256))], None),
            (vec!["x-goog-hash: crc32c=n03x6A==".into()], None),
            // A gzipped object served unzipped: its hash is of the gzip.
            (vec![format!("x-goog-hash: md5={}", b64(&md5)), "x-goog-stored-content-encoding: gzip".into()], None),
            (vec![format!("ETag: \"{}\"", sha256_hex(&data))], None),
            (vec!["Repr-Digest: sha-256=:dG9vIHNob3J0:".into(), "Digest: UNIXsum=30637".into()], None),
        ] {
            assert_eq!(from_answer(&headers, range).await, None, "{headers:?}");
        }
    }

    /// The checksum found for the file with only `files` beside it.
    async fn from_files(files: &[(&str, &str, &str)]) -> Option<String> {
        let files = files.iter().map(|(p, t, b)| (p.to_string(), t.to_string(), b.to_string())).collect();
        let (url, _) = serve(content(100), Site { files, ..Site::default() }).await;
        published_checksum(&Client::new(), &url, &HeaderMap::new()).await
    }

    #[tokio::test]
    async fn every_sidecar_form_gives_the_files_line() {
        let data = content(100);
        let (sha256, sha512) = (Sha256::digest(&data).to_vec(), Sha512::digest(&data).to_vec());
        let (sha1, md5) = (Sha1::digest(&data).to_vec(), Md5::digest(&data).to_vec());
        let hex = |c: String| c.split_once(':').unwrap().1.to_string();
        let (h256, h512, h1, h5) = (hex(checksum("sha256", &sha256)), hex(checksum("sha512", &sha512)), hex(checksum("sha1", &sha1)), hex(checksum("md5", &md5)));
        let text = "text/plain";
        for (files, expected) in [
            (vec![("/dir/file.bin.sha256", text, format!("{h256}  file.bin\n"))], checksum("sha256", &sha256)),
            (vec![("/dir/file.bin.sha512", text, format!("{}\n", h512.to_uppercase()))], checksum("sha512", &sha512)),
            (vec![("/dir/file.bin.sha1", text, format!("{h1} *file.bin"))], checksum("sha1", &sha1)),
            (vec![("/dir/file.bin.md5", "application/octet-stream", h5.clone())], checksum("md5", &md5)),
            (
                vec![("/dir/SHA256SUMS", text, format!("{}  other.bin\n{h256}  ./file.bin\n", "0".repeat(64)))],
                checksum("sha256", &sha256),
            ),
            (vec![("/dir/sha256sums.txt", text, format!("{h256} *sub/file.bin\r\n"))], checksum("sha256", &sha256)),
            // The file's own line wins over one for a file of its name in a folder.
            (
                vec![("/dir/SHA256SUMS", text, format!("{}  sub/file.bin\n{h256}  file.bin\n", "0".repeat(64)))],
                checksum("sha256", &sha256),
            ),
            (
                vec![("/dir/CHECKSUMS", text, format!("SHA256 (other.bin) = {}\nSHA512 (file.bin) = {h512}\n", "0".repeat(64)))],
                checksum("sha512", &sha512),
            ),
            // The file's own sidecars come before the folder's lists.
            (
                vec![("/dir/SHA256SUMS", text, format!("{h256}  file.bin")), ("/dir/file.bin.md5", text, h5.clone())],
                checksum("md5", &md5),
            ),
        ] {
            let files: Vec<(&str, &str, &str)> = files.iter().map(|(p, t, b)| (*p, *t, b.as_str())).collect();
            assert_eq!(from_files(&files).await, Some(expected), "{files:?}");
        }
    }

    #[tokio::test]
    async fn sidecars_that_are_pages_too_large_or_of_other_files_are_passed_over() {
        let h256 = sha256_hex(&content(100));
        for files in [
            vec![("/dir/file.bin.sha256", "text/html; charset=utf-8", format!("{h256}  file.bin"))],
            vec![("/dir/file.bin.sha256", "text/plain", format!("{h256}  file.bin\n{}", "#".repeat(MAX_SIDECAR)))],
            vec![("/dir/file.bin.sha256", "text/plain", format!("{h256}  other.bin"))],
            vec![("/dir/file.bin.sha256", "text/plain", h256[..40].to_string())],
            vec![("/dir/SHA256SUMS", "text/plain", h256.clone())],
            vec![("/dir/file.bin.md5", "text/plain", h256.clone())],
        ] {
            let files: Vec<(&str, &str, &str)> = files.iter().map(|(p, t, b)| (*p, *t, b.as_str())).collect();
            assert_eq!(from_files(&files).await, None, "{files:?}");
        }
    }

    #[tokio::test]
    async fn sidecars_get_three_seconds_in_all() {
        let line = format!("{}  file.bin", sha256_hex(&content(100)));
        let files = vec![("/dir/SHA256SUMS".to_string(), "text/plain".to_string(), line)];
        let (url, log) = serve(content(100), Site { files, sidecars_wait: Duration::from_secs(30), ..Site::default() }).await;
        let started = std::time::Instant::now();
        assert_eq!(published_checksum(&Client::new(), &url, &HeaderMap::new()).await, None);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(log.lock().unwrap().iter().all(|(path, range)| path != "/dir/file.bin" && range.is_none()), "GET of the sidecars only");
    }

    #[tokio::test]
    async fn sidecars_are_asked_for_two_at_a_time_and_the_first_in_order_wins() {
        let data = content(100);
        let hex = |c: String| c.split_once(':').unwrap().1.to_string();
        let (sha1, md5) = (checksum("sha1", &Sha1::digest(&data)), checksum("md5", &Md5::digest(&data)));
        let text = "text/plain".to_string();
        let files = vec![("/dir/file.bin.sha1".to_string(), text.clone(), hex(sha1.clone())), ("/dir/file.bin.md5".to_string(), text, hex(md5))];
        let at_once = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let site = Site { files, sidecars_wait: Duration::from_millis(200), sidecars_at_once: Arc::clone(&at_once), ..Site::default() };
        let (url, _) = serve(data, site).await;
        assert_eq!(published_checksum(&Client::new(), &url, &HeaderMap::new()).await, Some(sha1));
        assert_eq!(at_once.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn what_a_folder_does_not_publish_is_not_asked_for_again() {
        let client = Client::new();
        // How many requests the file `name` in the folder of `url` costs.
        let asked = |url: &Url, log: &Arc<std::sync::Mutex<Vec<(String, Option<(u64, u64)>)>>>, name: &str| {
            let (client, url, log) = (client.clone(), url.join(name).unwrap(), Arc::clone(log));
            async move {
                assert_eq!(published_checksum(&client, &url, &HeaderMap::new()).await, None, "{url}");
                std::mem::take(&mut *log.lock().unwrap()).len()
            }
        };
        // No checksum file on the host at all: the first file asks for each, the next from the host
        // none (see `hosts::checksums`).
        let (url, log) = serve(content(100), Site::default()).await;
        for (name, requests) in [("a.bin", 7), ("b.bin", 0), ("f.iso", 0)] {
            assert_eq!(asked(&url, &log, name).await, requests, "{name}");
        }
        // A list that is there is read for every file, whatever it lists.
        let files = vec![("/dir/SHA256SUMS".to_string(), "text/plain".to_string(), format!("{}  other.bin", "0".repeat(64)))];
        let (url, log) = serve(content(100), Site { files, ..Site::default() }).await;
        for (name, requests) in [("a.bin", 7), ("b.bin", 5), ("c.bin", 5), ("d.bin", 1)] {
            assert_eq!(asked(&url, &log, name).await, requests, "{name}");
        }
    }

    /// Files of one folder asked for at once take turns until the host is known: of a bare folder,
    /// the first asks for each checksum file and the others none; once one found its own, the
    /// others ask at once.
    #[tokio::test]
    async fn the_files_of_a_folder_take_turns_until_its_host_is_known() {
        let client = Client::new();
        let all = |url: &Url| {
            let asked = (0..12).map(|i| {
                let (client, url) = (&client, url.join(&format!("f{i}.bin")).unwrap());
                async move { published_checksum(client, &url, &HeaderMap::new()).await }
            });
            futures_util::future::join_all(asked)
        };
        let (url, log) = serve(content(100), Site { sidecars_wait: Duration::from_millis(50), ..Site::default() }).await;
        assert!(all(&url).await.iter().all(Option::is_none));
        assert_eq!(log.lock().unwrap().len(), 7);

        let data = content(100);
        let md5 = checksum("md5", &Md5::digest(&data));
        let files = (0..12).map(|i| (format!("/dir/f{i}.bin.md5"), "text/plain".to_string(), md5.clone().split_off(4))).collect();
        let (url, _) = serve(data.clone(), Site { files, sidecars_wait: Duration::from_millis(200), ..Site::default() }).await;
        let started = Instant::now();
        assert!(all(&url).await.iter().all(|found| found.as_ref() == Some(&md5)));
        // Each takes two rounds of 0.2 s: the first alone, then the others together.
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_checksum_file_found_or_a_server_that_fails_is_no_sign_of_a_bare_folder() {
        // The fourth file has one of its own, as the three before it had: they are asked for still.
        let data = content(100);
        let line = |name: &str| (format!("/dir/{name}.md5"), "text/plain".to_string(), checksum("md5", &Md5::digest(&data)).split_off(4));
        let (url, log) = serve(data.clone(), Site { files: ["a.bin", "b.bin", "c.bin", "d.bin"].map(line).to_vec(), ..Site::default() }).await;
        let client = Client::new();
        for name in ["a.bin", "b.bin", "c.bin", "d.bin"] {
            let found = published_checksum(&client, &url.join(name).unwrap(), &HeaderMap::new()).await;
            assert_eq!(found, Some(checksum("md5", &Md5::digest(&data))), "{name}");
        }
        assert_eq!(log.lock().unwrap().iter().filter(|(path, _)| path.ends_with(".md5")).count(), 4);

        // A server that does not answer tells nothing of its folder.
        let gone = Url::parse("http://127.0.0.1:9/dir/a.bin").unwrap();
        assert_eq!(published_checksum(&client, &gone, &HeaderMap::new()).await, None);
        assert!(!missing("http://127.0.0.1:9/dir/*.bin", 1) && !missing("http://127.0.0.1:9/dir/SHA256SUMS", 1));
    }

    /// A host that refuses a lookup (429) is asked for no more of them than were already on their
    /// way (two at a time), not all seven; one that refused a request lately is asked for none.
    #[tokio::test]
    async fn sidecars_stop_at_a_refusal_and_skip_a_host_that_refused_lately() {
        let sidecars = |log: &Arc<std::sync::Mutex<Vec<(String, Option<(u64, u64)>)>>>| log.lock().unwrap().len();
        let (url, log) = serve(content(100), Site { sidecars_refused: true, ..Site::default() }).await;
        assert_eq!(published_checksum(&Client::new(), &url, &HeaderMap::new()).await, None);
        assert_eq!(sidecars(&log), SIDECARS_AT_ONCE, "{:?}", log.lock().unwrap());

        let files = vec![("/dir/file.bin.sha256".to_string(), "text/plain".to_string(), format!("{}  file.bin", sha256_hex(&content(100))))];
        let (url, log) = serve(content(100), Site { files, ..Site::default() }).await;
        crate::hosts::acquire(&url, 0).await.throttled(Duration::ZERO);
        assert_eq!(published_checksum(&Client::new(), &url, &HeaderMap::new()).await, None);
        assert_eq!(sidecars(&log), 0);
    }

    /// Downloads `urls` (mirrors of one file) into `dir` with `options`; returns the result and
    /// what the last snapshot says the file was checked against.
    async fn download(urls: Vec<Url>, dir: &Path, options: DownloadOptions) -> (Result<std::path::PathBuf, String>, Option<String>) {
        let options = DownloadOptions { output_path: Some(dir.join("")), ..options };
        let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(1024);
        let engine = DownloadEngine::new(urls, options);
        let result = engine.run(Some(tx)).await;
        let mut last = None;
        while let Ok(snapshot) = rx.try_recv() {
            last = Some(snapshot);
        }
        let checked = last.and_then(|s| s.checked_against);
        assert_eq!(checked, engine.checked_against());
        (result, checked)
    }

    #[tokio::test]
    async fn a_download_is_checked_against_the_sites_checksum() {
        let data = content(300 * 1024);
        let dir = tempfile::tempdir().unwrap();
        let header = format!("Repr-Digest: sha-256=:{}:", b64(&Sha256::digest(&data)));
        let (url, _) = serve(data.clone(), Site { headers: vec![header], ..Site::default() }).await;
        let (done, checked) = download(vec![url], dir.path(), DownloadOptions::default()).await;
        assert_eq!(std::fs::read(done.unwrap()).unwrap(), data);
        assert_eq!(checked.as_deref(), Some("the site's SHA-256"));

        // A wrong sidecar fails it, saying where the checksum came from, and nothing is kept.
        let dir = tempfile::tempdir().unwrap();
        let files = vec![("/dir/file.bin.sha1".to_string(), "text/plain".to_string(), "0".repeat(40))];
        let (url, _) = serve(data.clone(), Site { files: files.clone(), ..Site::default() }).await;
        let (failed, _) = download(vec![url], dir.path(), DownloadOptions::default()).await;
        assert!(failed.as_ref().is_err_and(|e| e.contains("the site's published SHA-1")), "{failed:?}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "{:?}", std::fs::read_dir(dir.path()).unwrap().collect::<Vec<_>>());

        // Not when the user turned it off, nor looked for when the user gave a checksum.
        let (url, log) = serve(data.clone(), Site { files, ..Site::default() }).await;
        let off = DownloadOptions { remote_checksum: false, ..DownloadOptions::default() };
        let (done, checked) = download(vec![url.clone()], dir.path(), off).await;
        assert!(done.is_ok() && checked.is_none(), "{done:?} {checked:?}");
        let own = DownloadOptions { expected_checksum: Some(format!("sha256:{}", sha256_hex(&data))), ..DownloadOptions::default() };
        let (done, checked) = download(vec![url], &dir.path().join("own"), own).await;
        assert!(done.is_ok() && checked.is_none(), "{done:?} {checked:?}");
        assert!(log.lock().unwrap().iter().all(|(path, _)| path == "/dir/file.bin"), "{:?}", log.lock().unwrap());
    }

    #[tokio::test]
    async fn a_download_waits_for_a_sidecar_on_its_way() {
        let data = content(1000);
        let dir = tempfile::tempdir().unwrap();
        let files = vec![("/dir/SHA256SUMS".to_string(), "text/plain".to_string(), format!("{}  file.bin", sha256_hex(&data)))];
        let (url, _) = serve(data, Site { files, sidecars_wait: Duration::from_millis(500), ..Site::default() }).await;
        let (done, checked) = download(vec![url], dir.path(), DownloadOptions::default()).await;
        assert!(done.is_ok(), "{done:?}");
        assert_eq!(checked.as_deref(), Some("the site's SHA-256"));
    }

    /// Once its bytes are in, the download says it is being checked, the file still under its
    /// partial name with its resume state; it says it has nothing more to send once the site's
    /// checksum file has its answer, and asks for nothing after that. The file takes its name only
    /// with the verdict: one it fails (the site's, or the user's) fails the download and leaves
    /// nothing behind.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_keeps_its_partial_name_until_its_checks_are_done() {
        let data = content(1000);
        for (hex, own) in [(sha256_hex(&data), None), ("0".repeat(64), None), (sha256_hex(&data), Some("0".repeat(64)))] {
            let dir = tempfile::tempdir().unwrap();
            let files = vec![("/dir/SHA256SUMS".to_string(), "text/plain".to_string(), format!("{hex}  file.bin"))];
            let (url, log) = serve(data.clone(), Site { files, sidecars_wait: Duration::from_millis(300), ..Site::default() }).await;
            let options = DownloadOptions { output_path: Some(dir.path().join("")), expected_checksum: own.clone().map(|hex| format!("sha256:{hex}")), ..DownloadOptions::default() };
            let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(1024);
            let engine = DownloadEngine::new(vec![url], options);
            let run = tokio::spawn(async move { engine.run(Some(tx)).await });
            // What was on disk when the download said it was checking, and the requests out when it
            // said it had nothing more to send.
            let (mut checking, mut free) = (None, None);
            while let Ok(snapshot) = rx.recv().await {
                let target = snapshot.target_path.clone().unwrap_or_default();
                let part = crate::engine::part_path(&target);
                if snapshot.checking && checking.is_none() {
                    let state = crate::state::DownloadState::state_file_path(&part);
                    checking = Some((target.exists(), std::fs::read(&part).ok(), state.exists()));
                }
                if snapshot.place_free && free.is_none() {
                    free = Some((log.lock().unwrap().len(), target.exists() && snapshot.checking));
                }
            }
            let result = run.await.unwrap();
            let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
            let (named, on_disk, state) = checking.expect("it says when it checks");
            assert!(!named && on_disk.as_deref() == Some(&data[..]) && state, "{named} {state}");
            match (own, hex == sha256_hex(&data)) {
                (None, true) => {
                    let (asked, named) = free.expect("nothing more to send");
                    assert!(!named, "still under its partial name");
                    assert_eq!(asked, log.lock().unwrap().len(), "nothing asked after it said so");
                    assert!(result.is_ok() && left == ["file.bin"], "{result:?} {left:?}");
                }
                (None, false) => {
                    assert!(result.as_ref().is_err_and(|e| e.contains("the site's published SHA-256")), "{result:?}");
                    assert!(left.is_empty(), "nothing left: {left:?}");
                }
                (Some(_), _) => assert!(result.is_err() && left.is_empty(), "{result:?} {left:?}"),
            }
        }
    }

    /// A download stopped while its file is checked keeps the file under its partial name with its
    /// resume state, all of it there; run again, it only checks the file again: no request for its
    /// bytes but the probe's (its HEAD and the file's start), and no second copy.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_download_stopped_while_it_is_checked_only_checks_again() {
        let data = content(2 * MIB);
        let dir = tempfile::tempdir().unwrap();
        let wait = Duration::from_secs(2);
        let files = vec![("/dir/file.bin.sha256".to_string(), "text/plain".to_string(), sha256_hex(&data))];
        // 64 KiB a piece every 5 ms: about 160 ms for the file over one connection.
        let site = Site { files, sidecars_wait: wait, pace: Duration::from_millis(5), ..Site::default() };
        let (url, log) = serve(data.clone(), site).await;
        let options = || DownloadOptions { output_path: Some(dir.path().join("")), ..DownloadOptions::default() };
        let (tx, mut rx) = broadcast::channel::<EngineSnapshot>(1024);
        let engine = Arc::new(DownloadEngine::new(vec![url.clone()], options()));
        let run = tokio::spawn({
            let engine = Arc::clone(&engine);
            async move { engine.run(Some(tx)).await }
        });
        while let Ok(snapshot) = rx.recv().await {
            if snapshot.checking {
                break;
            }
        }
        let stopped = std::time::Instant::now();
        engine.cancel();
        let result = run.await.unwrap();
        assert!(stopped.elapsed() < wait, "it stops at once, not after the checksum file's {wait:?}: {:?}", stopped.elapsed());
        assert_eq!(result.as_deref().map_err(String::as_str), Err(crate::engine::CANCELLED));
        let part = crate::engine::part_path(&dir.path().join("file.bin"));
        let state = crate::state::DownloadState::load_from_path(&crate::state::DownloadState::state_file_path(&part)).unwrap().unwrap();
        assert_eq!(crate::range::merge_ranges(state.completed_ranges), [ByteRange::new(0, data.len() as u64 - 1).unwrap()]);
        assert!(!dir.path().join("file.bin").exists());

        let before = log.lock().unwrap().len();
        let (done, checked) = download(vec![url], dir.path(), options()).await;
        assert_eq!(done.unwrap(), dir.path().join("file.bin"));
        assert_eq!(checked.as_deref(), Some("the site's SHA-256"));
        assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), data);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["file.bin"], "no second copy");
        let asked: Vec<_> = log.lock().unwrap()[before..].iter().filter(|(path, _)| path == "/dir/file.bin").cloned().collect();
        assert!(asked.iter().all(|(_, range)| range.is_none_or(|(start, _)| start == 0)), "only the probe's: {asked:?}");
    }

    /// A newer version that fails the site's checksum, with `only_if_newer` (or a delta from the
    /// file it replaces), leaves the old file as it was.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_newer_version_that_fails_leaves_the_old_file() {
        let data = content(300 * 1024);
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("file.bin");
        std::fs::write(&old, b"the version the user has").unwrap();
        let files = vec![("/dir/SHA256SUMS".to_string(), "text/plain".to_string(), format!("{}  file.bin", "0".repeat(64)))];
        let (url, _) = serve(data, Site { files, sidecars_wait: Duration::from_millis(100), ..Site::default() }).await;
        for delta in [false, true] {
            let options = DownloadOptions {
                output_path: Some(old.clone()),
                only_if_newer: !delta,
                delta_from: delta.then(|| old.clone()),
                ..DownloadOptions::default()
            };
            let result = DownloadEngine::new(vec![url.clone()], options).run(None).await;
            assert!(result.as_ref().is_err_and(|e| e.contains("the site's published SHA-256")), "{result:?}");
            assert_eq!(std::fs::read(&old).unwrap(), b"the version the user has", "delta: {delta}");
            let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
            assert_eq!(left, ["file.bin"], "delta: {delta}");
        }
    }

    /// The site's checksum is taken as the file arrives, from the headers at once and from a
    /// checksum file as soon as that is found: the finished file is not read again for it.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_sites_checksum_is_taken_as_the_file_arrives() {
        let data = content(4 * MIB);
        let sums = |hex: &str| vec![("/dir/SHA256SUMS".to_string(), "text/plain".to_string(), format!("{hex}  file.bin"))];
        let header = format!("Repr-Digest: sha-256=:{}:", b64(&Sha256::digest(&data)));
        // 64 KiB every 25 ms over one connection: the file takes about 1.6 s. The list is asked
        // for after the file's own four checksum files, two at a time, so it is found after
        // about 80 ms, or 0.5 s when each answer waits 150 ms.
        let pace = Duration::from_millis(25);
        let one = || DownloadOptions { num_connections: 1, ..DownloadOptions::default() };
        let read_back = |dir: &Path| crate::storage::READ_BACK.lock().unwrap().iter().filter(|p| p.starts_with(dir)).cloned().collect::<Vec<_>>();
        for (label, site) in [
            ("in its headers", Site { headers: vec![header], pace, ..Site::default() }),
            ("in a list found at once", Site { files: sums(&sha256_hex(&data)), pace, ..Site::default() }),
            ("in a list found halfway", Site { files: sums(&sha256_hex(&data)), pace, sidecars_wait: Duration::from_millis(150), ..Site::default() }),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (url, _) = serve(data.clone(), site).await;
            let (done, checked) = download(vec![url], dir.path(), one()).await;
            assert!(std::fs::read(done.unwrap()).unwrap() == data, "{label}");
            assert_eq!(checked.as_deref(), Some("the site's SHA-256"), "{label}");
            assert_eq!(read_back(dir.path()), Vec::<std::path::PathBuf>::new(), "{label}");
        }
        // A wrong one fails the download as it did, giving the file's digest.
        let dir = tempfile::tempdir().unwrap();
        let (url, _) = serve(data.clone(), Site { files: sums(&"0".repeat(64)), pace, ..Site::default() }).await;
        let (failed, _) = download(vec![url], dir.path(), one()).await;
        let wrong = format!("against the site's published SHA-256: expected {}, got {}", "0".repeat(64), sha256_hex(&data));
        assert!(failed.as_ref().is_err_and(|e| e.contains(&wrong)), "{failed:?}");
        assert_eq!(read_back(dir.path()), Vec::<std::path::PathBuf>::new());
    }

    /// Hashes of `data` in `length`-byte pieces, as a metalink lists them.
    fn pieces_of(data: &[u8], length: usize) -> Pieces {
        let hashes = data.chunks(length).map(|piece| sha256_hex(piece)).collect();
        Pieces { length: length as u64, algo: "sha256".into(), hashes }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_pieces_that_fail_a_metalinks_hashes_are_fetched_again_from_another_mirror() {
        let data = content(3 * MIB);
        let dir = tempfile::tempdir().unwrap();
        // The first mirror damages the middle piece; the second never answers its probe beside
        // the first, so it shares none of the work: it is asked again on its own.
        let damaged = Site { damaged_once: Some(MIB + 10..MIB + 20), ..Site::default() };
        let (bad, _) = serve(data.clone(), damaged).await;
        let (good, log) = serve(data.clone(), Site { only_as_first: true, ..Site::default() }).await;
        // Pieces of 768 KiB: a piece fetched again is never mistaken for a probe's request.
        let piece = 3 * MIB / 4;
        let options = DownloadOptions {
            expected_checksum: Some(format!("sha256:{}", sha256_hex(&data))),
            metalink_pieces: Some(pieces_of(&data, piece)),
            ..DownloadOptions::default()
        };
        let (done, _) = download(vec![bad, good], dir.path(), options).await;
        assert!(std::fs::read(done.unwrap()).unwrap() == data);
        // Besides its probes (HEAD, and a GET of the file's first byte or of the first MiB it reads
        // ahead), as many as got out before the download no longer needed them, the second mirror
        // was asked for the damaged piece only.
        let log = log.lock().unwrap();
        let probe = |(start, end): (u64, u64)| start == 0 && (end == 0 || end == 1024 * 1024 - 1);
        let fetched: Vec<(u64, u64)> = log.iter().filter_map(|(_, range)| *range).filter(|&range| !probe(range)).collect();
        let damaged = piece as u64..2 * piece as u64;
        assert!(!fetched.is_empty() && fetched.iter().all(|&(s, e)| damaged.contains(&s) && damaged.contains(&e)), "{log:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pieces_check_a_download_without_a_whole_hash() {
        let data = content(3 * MIB);
        let dir = tempfile::tempdir().unwrap();
        let (url, _) = serve(data.clone(), Site { damaged_once: Some(2 * MIB + 5..2 * MIB + 6), ..Site::default() }).await;
        let options = DownloadOptions { metalink_pieces: Some(pieces_of(&data, MIB)), ..DownloadOptions::default() };
        let (done, _) = download(vec![url], dir.path(), options).await;
        assert!(std::fs::read(done.unwrap()).unwrap() == data, "one mirror is asked again");
    }

    #[test]
    fn a_pass_reads_pieces_of_every_kind_at_once() {
        let data = content(5 * MIB + 3);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, &data).unwrap();
        let mut metalink = pieces_of(&data, MIB);
        metalink.hashes[3] = "0".repeat(64);
        let wanted = format!("sha512:{}", "0".repeat(128));
        let pass = read_pass(&path, Some(&wanted), Some(&metalink), Some(4 * MIB as u64)).unwrap();
        assert_eq!(Some(checksum("sha512", &Sha512::digest(&data))), pass.published.map(|hex| format!("sha512:{hex}")));
        assert_eq!(pass.bad, vec![ByteRange::from_len(3 * MIB as u64, MIB as u64).unwrap()]);
        // BLAKE3 pieces are subtrees: merged, they give the file's own hash.
        assert_eq!(pass.blake3.len(), 2);
        let root = blake3::hazmat::merge_subtrees_root(&pass.blake3[0], &pass.blake3[1], blake3::hazmat::Mode::Hash);
        assert_eq!(root, blake3::hash(&data));
        // Pieces that do not fit the file check nothing.
        metalink.hashes.pop();
        assert!(read_pass(&path, None, Some(&metalink), None).unwrap().bad.is_empty());
    }

    #[test]
    fn a_writers_pieces_are_those_a_read_gives() {
        let data = content(5 * MIB + 5);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        let writer = DiskWriter::open_or_create(&path, data.len() as u64).unwrap();
        // The first 3 MiB as an earlier run left them, never hashed; the rest out of order.
        writer.write_chunk_slice(0, &data[..3 * MIB]).unwrap();
        writer.track_digest(&[ByteRange::from_len(0, 3 * MIB as u64).unwrap()], None);
        writer.write_chunk_slice(5 * MIB as u64, &data[5 * MIB..]).unwrap();
        writer.write_chunk_slice(3 * MIB as u64, &data[3 * MIB..5 * MIB]).unwrap();
        assert_eq!(writer.digest(None).unwrap().blake3_hex(), blake3::hash(&data).to_hex().as_str());

        let piece = crate::history::piece_size(data.len() as u64).unwrap();
        let read = read_pass(&path, None, None, Some(piece)).unwrap().blake3;
        assert_eq!(read.len(), 2);
        assert_eq!(writer.piece_hashes(piece).unwrap(), read);
    }
}
