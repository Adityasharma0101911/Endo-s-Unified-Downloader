//! The checksum a site publishes for a file, and the last checks a finished download goes
//! through: that checksum when the user gave none, a metalink's piece hashes, and the BLAKE3
//! piece hashes history keeps for `--verify` and repair (see `history::piece_size`).

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use blake3::hazmat::HasherExt;
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

/// The checksum the site publishes for the file at `url`, in the form `--checksum` takes
/// (`sha256:<hex>`, `sha512:`, `sha1:`, `md5:`), whose probe answered with `probe_headers`.
///
/// The headers come first, the strongest digest of the whole file they give: `Repr-Digest`,
/// `Content-Digest` (RFC 9530) and `Digest` (RFC 3230), `Content-MD5`, and the hashes cloud
/// storage sends (`x-goog-hash` md5, `x-amz-checksum-sha256`/`-sha1` unless they are of parts,
/// `x-ms-blob-content-md5`). Nothing is taken from an encoded answer, nor a content digest from a
/// partial one, and an ETag never counts as a hash. Then the checksum files beside it, from the
/// same site: `<url>.sha256`, `.sha512`, `.sha1`, `.md5`, then `SHA256SUMS`, `sha256sums.txt` and
/// `CHECKSUMS` in its folder, by the line for its name. Those are fetched one after the other,
/// with GET only, each at most 64 KiB, within 3 seconds in all.
pub async fn published_checksum(client: &Client, url: &Url, probe_headers: &HeaderMap) -> Option<String> {
    published(client, None, 0, url, probe_headers).await
}

/// [`published_checksum`], the checksum files asked for with `auth` as the download's own
/// requests are (see `worker::authorize`): its credentials go to its own hosts alone. Each takes
/// a slot of the host's `limit` (see `crate::hosts`), as every request of a download does.
async fn published(client: &Client, auth: Option<&Auth>, limit: usize, url: &Url, probe_headers: &HeaderMap) -> Option<String> {
    match from_headers(probe_headers) {
        Some(found) => Some(found),
        None => from_sidecars(client, auth, limit, url).await,
    }
}

/// `algo:hex` for `digest` of `algo` (one of `ALGOS`).
fn checksum(algo: &str, digest: &[u8]) -> String {
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

/// The checksum the files beside `url` give for it (see [`published_checksum`]).
async fn from_sidecars(client: &Client, auth: Option<&Auth>, limit: usize, url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let name = url.path_segments()?.next_back().filter(|n| !n.is_empty())?;
    let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy().into_owned();
    let mut file = url.clone();
    file.set_query(None);
    file.set_fragment(None);
    let beside = ["sha256", "sha512", "sha1", "md5"].map(|ext| {
        let mut sidecar = file.clone();
        sidecar.set_path(&format!("{}.{ext}", file.path()));
        (sidecar, Some(ext))
    });
    let folder = ["SHA256SUMS", "sha256sums.txt", "CHECKSUMS"].map(|list| file.join(list).ok().map(|u| (u, None)));
    // One at a time, over the connection the last one left open: a host that takes few
    // connections keeps them for the download.
    let lookups = async {
        for (sidecar, ext) in beside.into_iter().chain(folder.into_iter().flatten()) {
            if let Some(found) = fetch_small(client, auth, limit, sidecar).await.and_then(|text| checksum_line(&text, &name, ext)) {
                return Some(found);
            }
        }
        None
    };
    tokio::time::timeout(SIDECAR_BUDGET, lookups).await.ok().flatten()
}

/// The body of `url`, unless it is no success, a web page or larger than `MAX_SIDECAR`.
async fn fetch_small(client: &Client, auth: Option<&Auth>, limit: usize, url: Url) -> Option<String> {
    let _slot = crate::hosts::acquire(&url, limit).await;
    let request = |url: &Url| authorize(client.get(url.clone()), auth, url).header(ACCEPT_ENCODING, "identity");
    let mut resp = send(request, &url).await.ok()?;
    if !resp.status().is_success() || crate::resolver::html_type(resp.headers()) {
        return None;
    }
    if resp.content_length().is_some_and(|len| len > MAX_SIDECAR as u64) {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.ok()? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_SIDECAR {
            return None;
        }
    }
    Some(String::from_utf8_lossy(&body).into_owned())
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
    pub(crate) note: Option<String>,
}

/// Starts looking for the site's published checksum of the download at `url`, whose probe
/// answered with `headers`, alongside the download (its checksum files asked for with `auth`),
/// unless the user gave a checksum or turned this off (`DownloadOptions::remote_checksum`).
pub(crate) fn look_for(
    site: &Mutex<Site>,
    options: &DownloadOptions,
    client: &Client,
    auth: Option<Arc<Auth>>,
    url: &Url,
    headers: &HeaderMap,
) {
    let search = (options.remote_checksum && options.expected_checksum.is_none()).then(|| {
        let (client, url, headers, limit) = (client.clone(), url.clone(), headers.clone(), options.max_connections_per_host);
        tokio::spawn(async move { published(&client, auth.as_deref(), limit, &url, &headers).await })
    });
    *site.lock() = Site { search, note: None };
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
}

/// The last checks of the finished `.part` at `part`, once the engine checked it against the
/// expected checksum (`verified`, its BLAKE3 as hex): a metalink's piece hashes when the whole
/// file failed or has no hash, then the site's published checksum (see [`look_for`]), which a
/// download that finishes sooner waits for until its time is up. Also keeps the file's BLAKE3
/// piece hashes for history: from the blocks `writer` hashed as it wrote them, else from a read
/// the other checks make anyway (a download in one stream, which cannot be repaired by ranges
/// from its server, has none otherwise). Reads the file once for all of them, and only when one
/// is needed. Returns its BLAKE3 as hex.
pub(crate) async fn finish(
    part: &Path,
    verified: Result<String, VerifyError>,
    options: &DownloadOptions,
    site: &Mutex<Site>,
    writer: Option<DiskWriter>,
) -> Result<String, Unfinished> {
    let (hash, whole_failed) = match verified {
        Ok(hash) => (Some(hash), None),
        Err(VerifyError::Io(e)) => return Err(Unfinished::Io(e)),
        Err(VerifyError::Mismatch(e)) if options.metalink_pieces.is_some() => (None, Some(e)),
        Err(VerifyError::Mismatch(e)) => return Err(Unfinished::Wrong(e)),
    };
    let search = site.lock().search.take();
    let published = match search {
        Some(search) => search.await.ok().flatten(),
        None => None,
    };
    let metalink = options.metalink_pieces.clone().filter(|_| whole_failed.is_some() || options.expected_checksum.is_none());
    let (path, wanted) = (part.to_path_buf(), published.clone());
    let read = tokio::task::spawn_blocking(move || {
        let size = std::fs::metadata(&path)?.len();
        let piece = crate::history::piece_size(size);
        let kept = piece.zip(writer).map(|(piece, writer)| writer.piece_hashes(piece)).transpose()?;
        let read_pieces = piece.filter(|_| kept.is_none() && (wanted.is_some() || metalink.is_some()));
        let mut pass = read_pass(&path, wanted.as_deref(), metalink.as_ref(), read_pieces)?;
        if !pass.bad.is_empty() {
            return Ok(Err(refetch(&path, size, &pass.bad)));
        }
        pass.blake3 = kept.unwrap_or(pass.blake3);
        Ok(Ok((pass, piece)))
    });
    let read = read.await.map_err(|e| Unfinished::Io(format!("Background task failed: {e}")))?;
    let (pass, piece) = read.map_err(|e: io::Error| Unfinished::Io(format!("Failed to read {}: {}", part.display(), e)))??;
    if let Some(e) = whole_failed {
        return Err(Unfinished::Wrong(e));
    }
    let hash = hash.unwrap_or_default();
    if let (Some((algo, expected)), Some(actual)) = (published.as_deref().and_then(|c| c.split_once(':')), &pass.published) {
        if expected != actual {
            return Err(Unfinished::Wrong(format!(
                "Checksum verification failed against the site's published {}: expected {}, got {}",
                algo_name(algo),
                expected,
                actual
            )));
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
    let wrong = || {
        let bytes: u64 = bad.iter().map(ByteRange::len).sum();
        Unfinished::Wrong(format!("{} piece(s), {} bytes, do not match the metalink's hashes", bad.len(), bytes))
    };
    let state_path = DownloadState::state_file_path(part);
    let Some(mut state) = DownloadState::load_from_path(&state_path).ok().flatten().filter(|s| s.file_size == size) else {
        return wrong();
    };
    // The rest is on disk before the state says so.
    if let Err(e) = OpenOptions::new().write(true).open(part).and_then(|f| f.sync_data()) {
        return Unfinished::Io(format!("Failed to flush {}: {}", part.display(), e));
    }
    state.completed_ranges = compute_gaps(size, bad);
    // Mirrors serve the file under validators of their own: the pieces' hashes hold it now.
    (state.etag, state.last_modified) = (None, None);
    match state.save_atomic(&state_path) {
        Ok(()) => {
            tracing::warn!("{} piece(s) of {} do not match the metalink's hashes; fetching them again", bad.len(), part.display());
            Unfinished::Refetch
        }
        Err(e) => Unfinished::Io(format!("Failed to save download state: {}", e)),
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
    let whole_algo = published.and_then(|c| flat(c.split_once(':')?.0));
    if whole_algo.is_none() && metalink.is_none() && piece.is_none() {
        return Ok(Pass::default());
    }
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut whole = whole_algo.map(|new| Pieced::new(size, size, new));
    let metalink = metalink.filter(|m| m.fit(size));
    let mut pieces = metalink.and_then(|m| Some(Pieced::new(m.length, size, flat(&m.algo)?)));
    let mut blake = piece.filter(|&p| p < size).map(|p| Pieced::new(p, size, tree));
    let mut buf = vec![0; READ_BLOCK];
    let mut at = 0;
    while at < size {
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
        /// Sidecar answers wait this long.
        sidecars_wait: Duration,
    }

    /// Serves `content` at `/dir/file.bin` (HEAD, GET and ranges) and `site.files` beside it, any
    /// other path a 404. Returns the file's URL and every request's path and range, in order.
    async fn serve(content: Vec<u8>, site: Site) -> (Url, Arc<std::sync::Mutex<Vec<(String, Option<(u64, u64)>)>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (content, site, seen) = (Arc::new(content), Arc::new(site), Arc::clone(&log));
        let damaged = Arc::new(std::sync::atomic::AtomicBool::new(false));
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (content, site, seen, damaged) = (Arc::clone(&content), Arc::clone(&site), Arc::clone(&seen), Arc::clone(&damaged));
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
                    } else if let Some((_, kind, body)) = site.files.iter().find(|(p, _, _)| p.to_ascii_lowercase() == path) {
                        tokio::time::sleep(site.sidecars_wait).await;
                        format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                            .into_bytes()
                    } else {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                    };
                    let _ = sock.write_all(&resp).await;
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
        let options = DownloadOptions {
            expected_checksum: Some(format!("sha256:{}", sha256_hex(&data))),
            metalink_pieces: Some(pieces_of(&data, MIB)),
            ..DownloadOptions::default()
        };
        let (done, _) = download(vec![bad, good], dir.path(), options).await;
        assert!(std::fs::read(done.unwrap()).unwrap() == data);
        // The second mirror's probe, then the damaged piece only.
        let fetched: Vec<(u64, u64)> = log.lock().unwrap().iter().filter_map(|(_, range)| *range).skip(2).collect();
        assert!(!fetched.is_empty() && fetched.iter().all(|&(s, e)| s >= MIB as u64 && e < 2 * MIB as u64), "{:?}", log.lock().unwrap());    }

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
