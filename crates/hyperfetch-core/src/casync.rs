//! Chunk stores (casync and desync): a `.caibx` index lists the chunks of one file, a `.caidx` those
//! of a folder tree packed as a catar archive, each by its SHA-512/256 (SHA-256 in older indexes);
//! a chunk store holds them compressed with zstd, as `<store>/<first 4 hex>/<id>.cacnk`, on a web
//! server or in a folder. The app reads the index (a link ending in `.caibx` or `.caidx`), takes
//! each chunk it can from what is on disk (the part file of an earlier run, at its place; the older
//! version given with `--delta-from`, cut into chunks as casync cuts them; the chunk cache), fetches
//! the others from the store several at once, checks each against its id, and puts the file
//! together; a folder tree's archive is then unpacked (its folders and files: links, devices and
//! the like are left out). The store is `--chunk-store` (a link or a folder), else `default.castr`
//! beside the index, as casync names it. Fetched chunks are kept in `--chunk-cache`, its oldest
//! dropped past `--chunk-cache-size`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures_util::stream::{self, StreamExt};
use sha2::{Digest, Sha256, Sha512_256};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{DownloadOptions, EngineSnapshot, CANCELLED};
use crate::worker::RateLimiter;

const INDEX: u64 = 0x96824d9c7b129ff9;
const TABLE: u64 = 0xe75b9e112f17417d;
const TABLE_TAIL: u64 = 0x4b4f050e5549ecd1;
const SHA512_256: u64 = 0x2000000000000000;
const ENTRY: u64 = 0x1396fabcea5bbb51;
const PAYLOAD: u64 = 0x8b9e1d93d6dcffc9;
const FILENAME: u64 = 0x6dbb6ebcb3161f0b;
const GOODBYE: u64 = 0xdfd35c5e8327c403;
/// The largest index read.
const MAX_INDEX: u64 = 64 << 20;
/// The rolling hash's window, and its table, as casync has them.
const WINDOW: usize = 48;
#[rustfmt::skip]
const BUZHASH: [u32; 256] = [
    0x458be752, 0xc10748cc, 0xfbbcdbb8, 0x6ded5b68, 0xb10a82b5, 0x20d75648, 0xdfc5665f, 0xa8428801,
    0x7ebf5191, 0x841135c7, 0x65cc53b3, 0x280a597c, 0x16f60255, 0xc78cbc3e, 0x294415f5, 0xb938d494,
    0xec85c4e6, 0xb7d33edc, 0xe549b544, 0xfdeda5aa, 0x882bf287, 0x3116737c, 0x05569956, 0xe8cc1f68,
    0x0806ac5e, 0x22a14443, 0x15297e10, 0x50d090e7, 0x4ba60f6f, 0xefd9f1a7, 0x5c5c885c, 0x82482f93,
    0x9bfd7c64, 0x0b3e7276, 0xf2688e77, 0x8fad8abc, 0xb0509568, 0xf1ada29f, 0xa53efdfe, 0xcb2b1d00,
    0xf2a9e986, 0x6463432b, 0x95094051, 0x5a223ad2, 0x9be8401b, 0x61e579cb, 0x1a556a14, 0x5840fdc2,
    0x9261ddf6, 0xcde002bb, 0x52432bb0, 0xbf17373e, 0x7b7c222f, 0x2955ed16, 0x9f10ca59, 0xe840c4c9,
    0xccabd806, 0x14543f34, 0x1462417a, 0x0d4a1f9c, 0x087ed925, 0xd7f8f24c, 0x7338c425, 0xcf86c8f5,
    0xb19165cd, 0x9891c393, 0x325384ac, 0x0308459d, 0x86141d7e, 0xc922116a, 0xe2ffa6b6, 0x53f52aed,
    0x2cd86197, 0xf5b9f498, 0xbf319c8f, 0xe0411fae, 0x977eb18c, 0xd8770976, 0x9833466a, 0xc674df7f,
    0x8c297d45, 0x8ca48d26, 0xc49ed8e2, 0x7344f874, 0x556f79c7, 0x6b25eaed, 0xa03e2b42, 0xf68f66a4,
    0x8e8b09a2, 0xf2e0e62a, 0x0d3a9806, 0x9729e493, 0x8c72b0fc, 0x160b94f6, 0x450e4d3d, 0x7a320e85,
    0xbef8f0e1, 0x21d73653, 0x4e3d977a, 0x1e7b3929, 0x1cc6c719, 0xbe478d53, 0x8d752809, 0xe6d8c2c6,
    0x275f0892, 0xc8acc273, 0x4cc21580, 0xecc4a617, 0xf5f7be70, 0xe795248a, 0x375a2fe9, 0x425570b6,
    0x8898dcf8, 0xdc2d97c4, 0x0106114b, 0x364dc22f, 0x1e0cad1f, 0xbe63803c, 0x5f69fac2, 0x4d5afa6f,
    0x1bc0dfb5, 0xfb273589, 0x0ea47f7b, 0x3c1c2b50, 0x21b2a932, 0x6b1223fd, 0x2fe706a8, 0xf9bd6ce2,
    0xa268e64e, 0xe987f486, 0x3eacf563, 0x1ca2018c, 0x65e18228, 0x2207360a, 0x57cf1715, 0x34c37d2b,
    0x1f8f3cde, 0x93b657cf, 0x31a019fd, 0xe69eb729, 0x8bca7b9b, 0x4c9d5bed, 0x277ebeaf, 0xe0d8f8ae,
    0xd150821c, 0x31381871, 0xafc3f1b0, 0x927db328, 0xe95effac, 0x305a47bd, 0x426ba35b, 0x1233af3f,
    0x686a5b83, 0x50e072e5, 0xd9d3bb2a, 0x8befc475, 0x487f0de6, 0xc88dff89, 0xbd664d5e, 0x971b5d18,
    0x63b14847, 0xd7d3c1ce, 0x7f583cf3, 0x72cbcb09, 0xc0d0a81c, 0x7fa3429b, 0xe9158a1b, 0x225ea19a,
    0xd8ca9ea3, 0xc763b282, 0xbb0c6341, 0x020b8293, 0xd4cd299d, 0x58cfa7f8, 0x91b4ee53, 0x37e4d140,
    0x95ec764c, 0x30f76b06, 0x5ee68d24, 0x679c8661, 0xa41979c2, 0xf2b61284, 0x4fac1475, 0x0adb49f9,
    0x19727a23, 0x15a7e374, 0xc43a18d5, 0x3fb1aa73, 0x342fc615, 0x924c0793, 0xbee2d7f0, 0x8a279de9,
    0x4aa2d70c, 0xe24dd37f, 0xbe862c0b, 0x177c22c2, 0x5388e5ee, 0xcd8a7510, 0xf901b4fd, 0xdbc13dbc,
    0x6c0bae5b, 0x64efe8c7, 0x48b02079, 0x80331a49, 0xca3d8ae6, 0xf3546190, 0xfed7108b, 0xc49b941b,
    0x32baf4a9, 0xeb833a4a, 0x88a3f1a5, 0x3a91ce0a, 0x3cc27da1, 0x7112e684, 0x4a3096b1, 0x3794574c,
    0xa3c8b6f3, 0x1d213941, 0x6e0a2e00, 0x233479f1, 0x0f4cd82f, 0x6093edd2, 0x5d7d209e, 0x464fe319,
    0xd4dcac9e, 0x0db845cb, 0xfb5e4bc3, 0xe0256ce1, 0x09fb4ed1, 0x0914be1e, 0xa5bdb2c3, 0xc6eb57bb,
    0x30320350, 0x3f397e91, 0xa67791bc, 0x86bc0e2c, 0xefa0a7e2, 0xe9ff7543, 0xe733612c, 0xd185897b,
    0x329e5388, 0x91dd236b, 0x2ecb0d93, 0xf4d82a3d, 0x35b5c03f, 0xe4e606f0, 0x05b21843, 0x37b45964,
    0x5eff22f4, 0x6027f4cc, 0x77178b3c, 0xae507131, 0x7bf7cabc, 0xf9c18d66, 0x593ade65, 0xd95ddf11,
];

/// Whether `url` is a chunk store's index.
pub(crate) fn handles(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    matches!(url.scheme(), "http" | "https") && (path.ends_with(".caibx") || path.ends_with(".caidx"))
}

/// An index: whether its ids are SHA-512/256 (else SHA-256), its chunk sizes and its chunks.
#[derive(Debug, PartialEq)]
struct Index {
    sha512: bool,
    min: u64,
    avg: u64,
    max: u64,
    chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, PartialEq)]
struct Chunk {
    start: u64,
    size: u64,
    id: [u8; 32],
}

/// The index in `bytes` (see casync's format: a header with the chunk sizes, then a table of each
/// chunk's end and id, then its tail).
fn parse_index(bytes: &[u8]) -> Result<Index, String> {
    let bad = || "not a casync index (.caibx or .caidx)".to_string();
    let word = |at: usize| bytes.get(at..at + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()));
    if word(0) != Some(48) || word(8) != Some(INDEX) || word(48) != Some(u64::MAX) || word(56) != Some(TABLE) {
        return Err(bad());
    }
    let (flags, min, avg, max) = (word(16).ok_or_else(bad)?, word(24).ok_or_else(bad)?, word(32).ok_or_else(bad)?, word(40).ok_or_else(bad)?);
    if min < WINDOW as u64 || min > avg || avg > max || max > 64 << 20 {
        return Err(format!("the index's chunk sizes ({min}, {avg}, {max}) are not ones casync makes"));
    }
    let (mut at, mut start, mut chunks) = (64, 0u64, Vec::new());
    loop {
        let end = word(at).ok_or_else(bad)?;
        if end == 0 {
            break;
        }
        if end <= start || end - start > max {
            return Err(format!("the index's chunk at byte {start} is larger than its chunks may be"));
        }
        let id = bytes.get(at + 8..at + 40).ok_or_else(bad)?.try_into().map_err(|_| bad())?;
        chunks.push(Chunk { start, size: end - start, id });
        (start, at) = (end, at + 40);
    }
    if word(at + 8) != Some(0) || word(at + 32) != Some(TABLE_TAIL) {
        return Err(bad());
    }
    Ok(Index { sha512: flags & SHA512_256 != 0, min, avg, max, chunks })
}

fn id_of(data: &[u8], sha512: bool) -> [u8; 32] {
    if sha512 {
        Sha512_256::digest(data).into()
    } else {
        Sha256::digest(data).into()
    }
}

fn hex(id: &[u8; 32]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where a chunk is in a store: `<first 4 hex>/<id>.cacnk`.
fn chunk_path(id: &[u8; 32]) -> String {
    let hex = hex(id);
    format!("{}/{hex}.cacnk", &hex[..4])
}

/// Where `buf` is cut into its first chunk, as casync cuts: where the rolling hash of the 48 bytes
/// before is one less than a multiple of the discriminator, but not before `min` bytes nor after
/// `max`.
fn cut(buf: &[u8], min: usize, max: usize, discriminator: u32) -> usize {
    if buf.len() <= min {
        return buf.len();
    }
    let end = buf.len().min(max);
    let boundary = |hash: u32| hash.wrapping_add(1) % discriminator == 0;
    let mut hash = 0u32;
    for (i, &byte) in buf[min - WINDOW..min].iter().enumerate() {
        hash ^= BUZHASH[byte as usize].rotate_left((WINDOW - i - 1) as u32);
    }
    if boundary(hash) {
        return min;
    }
    for at in min..end {
        hash = hash.rotate_left(1) ^ BUZHASH[buf[at - WINDOW] as usize].rotate_left(WINDOW as u32) ^ BUZHASH[buf[at] as usize];
        if boundary(hash) {
            return at + 1;
        }
    }
    end
}

/// Cuts what `reader` holds into chunks as casync does with these sizes, calling `found` with the
/// offset and bytes of each.
fn chunks(mut reader: impl Read, min: u64, avg: u64, max: u64, mut found: impl FnMut(u64, &[u8])) -> io::Result<()> {
    let discriminator = ((avg as f64 / (-1.42888852e-7 * avg as f64 + 1.33237515)) as u32).max(1);
    let (min, max) = (min as usize, max as usize);
    let (mut buf, mut start, mut eof) = (Vec::new(), 0u64, false);
    loop {
        while !eof && buf.len() < max {
            let had = buf.len();
            buf.resize(had + max.max(1 << 16), 0);
            let n = reader.read(&mut buf[had..])?;
            buf.truncate(had + n);
            eof = n == 0;
        }
        if buf.is_empty() {
            return Ok(());
        }
        let at = cut(&buf, min, max, discriminator);
        found(start, &buf[..at]);
        start += at as u64;
        buf.drain(..at);
    }
}

/// Where chunks are fetched from.
enum Store {
    Web(Url),
    Folder(PathBuf),
}

fn store_of(index: &Url, given: Option<&str>) -> Result<Store, String> {
    match given.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) if s.starts_with("http://") || s.starts_with("https://") => Url::parse(&format!("{}/", s.trim_end_matches('/'))).map(Store::Web).map_err(|e| format!("--chunk-store: {e}")),
        Some(s) => Ok(Store::Folder(PathBuf::from(s))),
        None => index.join("default.castr/").map(Store::Web).map_err(|e| e.to_string()),
    }
}

/// The chunk in `packed` (zstd) when it is `chunk`, checked against its id.
fn unpack(packed: &[u8], chunk: &Chunk, sha512: bool) -> Option<Vec<u8>> {
    let mut data = Vec::with_capacity(chunk.size as usize);
    ruzstd::decoding::StreamingDecoder::new(packed).ok()?.take(chunk.size + 1).read_to_end(&mut data).ok()?;
    (data.len() as u64 == chunk.size && id_of(&data, sha512) == chunk.id).then_some(data)
}

/// What chunks are fetched with.
struct Fetcher<'a> {
    http: &'a reqwest::Client,
    store: &'a Store,
    cache: Option<&'a Path>,
    limiter: Option<&'a RateLimiter>,
    sha512: bool,
}

impl Fetcher<'_> {
    /// `chunk` from the cache, else from the store (kept in the cache then), checked.
    async fn fetch(&self, chunk: &Chunk) -> Result<Vec<u8>, String> {
        let name = chunk_path(&chunk.id);
        let cached = self.cache.map(|dir| dir.join(&name));
        if let Some(data) = cached.as_ref().and_then(|file| std::fs::read(file).ok()).and_then(|packed| unpack(&packed, chunk, self.sha512)) {
            return Ok(data);
        }
        let failed = |e: String| format!("chunk {}: {e}", hex(&chunk.id));
        let packed = match self.store {
            Store::Web(base) => {
                let url = base.join(&name).map_err(|e| e.to_string())?;
                let mut answer = self.http.get(url).send().await.and_then(|a| a.error_for_status()).map_err(|e| failed(e.without_url().to_string()))?;
                let mut packed = Vec::new();
                while let Some(part) = answer.chunk().await.map_err(|e| failed(e.without_url().to_string()))? {
                    packed.extend_from_slice(&part);
                    // zstd never makes a chunk much larger than it is.
                    if packed.len() as u64 > chunk.size + chunk.size / 8 + (1 << 16) {
                        return Err(failed("larger in the store than it can be".to_string()));
                    }
                }
                packed
            }
            Store::Folder(dir) => tokio::fs::read(dir.join(&name)).await.map_err(|e| failed(e.to_string()))?,
        };
        if let Some(limiter) = self.limiter {
            limiter.acquire(packed.len() as u64).await;
        }
        let data = unpack(&packed, chunk, self.sha512).ok_or_else(|| failed("damaged in the store (it does not match its id)".to_string()))?;
        if let Some(file) = cached {
            let part = file.with_extension("cacnk.part");
            let kept = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&part, &packed)).and_then(|()| std::fs::rename(&part, &file));
            if kept.is_err() {
                let _ = std::fs::remove_file(&part);
            }
        }
        Ok(data)
    }
}

/// The bytes of `chunk` in `file`, when they are there.
fn read_chunk(file: &mut File, chunk: &Chunk) -> Option<Vec<u8>> {
    let mut data = vec![0; chunk.size as usize];
    file.seek(SeekFrom::Start(chunk.start)).ok()?;
    file.read_exact(&mut data).ok()?;
    Some(data)
}

/// Puts into `out` each chunk of `index` it does not hold yet that `seed` holds anywhere (found by
/// cutting the seed as casync cuts), and gives back the chunks still missing and the bytes `out`
/// held already. Blocking.
fn from_disk(out: &mut File, index: &Index, seed: Option<&Path>) -> io::Result<(Vec<Chunk>, u64)> {
    let (mut missing, mut held) = (Vec::new(), 0);
    for chunk in &index.chunks {
        if read_chunk(out, chunk).is_some_and(|data| id_of(&data, index.sha512) == chunk.id) {
            held += chunk.size;
        } else {
            missing.push(chunk.clone());
        }
    }
    let Some(seed) = seed.filter(|_| !missing.is_empty()) else { return Ok((missing, held)) };
    // Each chunk found goes to its places at once, so one chunk of the seed is in memory at a time.
    let mut wanted: HashMap<[u8; 32], Vec<u64>> = HashMap::new();
    for chunk in &missing {
        wanted.entry(chunk.id).or_default().push(chunk.start);
    }
    let mut failed = None;
    chunks(io::BufReader::new(File::open(seed)?), index.min, index.avg, index.max, |_, data| {
        let Some(starts) = wanted.remove(&id_of(data, index.sha512)).filter(|_| failed.is_none()) else { return };
        for start in starts {
            if let Err(e) = out.seek(SeekFrom::Start(start)).and_then(|_| out.write_all(data)) {
                failed = Some(e);
                return;
            }
        }
    })?;
    if let Some(e) = failed {
        return Err(e);
    }
    missing.retain(|c| wanted.contains_key(&c.id));
    Ok((missing, held))
}

/// Drops the oldest chunks of the cache `dir` until it holds at most `max` bytes. Only chunks count
/// (`<first 4 hex>/<id>.cacnk`): any other file there, the user's, is left alone. Blocking.
fn trim_cache(dir: &Path, max: u64) {
    let hex = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let chunk = |e: &std::fs::DirEntry, prefix: &str| {
        let name = e.file_name();
        let id = name.to_str().and_then(|n| n.strip_suffix(".cacnk")).unwrap_or_default();
        hex(id, 64) && id.starts_with(prefix)
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().filter(|n| hex(n, 4)).map(str::to_string))
        .filter_map(|prefix| std::fs::read_dir(dir.join(&prefix)).ok().map(|files| (prefix, files)))
        .flat_map(|(prefix, files)| files.flatten().filter(move |e| chunk(e, &prefix)))
        .filter_map(|e| e.metadata().ok().filter(std::fs::Metadata::is_file).map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path())))
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, size, path) in files {
        if total <= max {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
        }
    }
}

/// Unpacks the catar archive `archive` into the folder `into`: its folders and files; links,
/// devices and the like are left out, and counted. Blocking.
fn unpack_catar(archive: &Path, into: &Path) -> Result<usize, String> {
    let bad = || format!("{} is not a catar archive casync made", archive.display());
    let error = |e: io::Error| format!("Cannot unpack {}: {e}", archive.display());
    let mut reader = io::BufReader::new(File::open(archive).map_err(error)?);
    let (mut folders, mut name, mut file, mut skipped): (Vec<PathBuf>, Option<PathBuf>, Option<File>, usize) = (Vec::new(), None, None, 0);
    loop {
        let mut head = [0u8; 16];
        match reader.read_exact(&mut head) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            other => other.map_err(error)?,
        }
        let size = u64::from_le_bytes(head[..8].try_into().unwrap_or_default());
        let kind = u64::from_le_bytes(head[8..].try_into().unwrap_or_default());
        let body = size.checked_sub(16).ok_or_else(bad)?;
        match kind {
            ENTRY if body == 48 => {
                let mut entry = [0u8; 48];
                reader.read_exact(&mut entry).map_err(error)?;
                let mode = u64::from_le_bytes(entry[8..16].try_into().unwrap_or_default());
                let path = match (name.take(), folders.last()) {
                    (Some(name), Some(folder)) => folder.join(name),
                    (None, None) => into.to_path_buf(),
                    _ => return Err(bad()),
                };
                file = None;
                match mode & 0o170000 {
                    0o040000 => {
                        std::fs::create_dir_all(&path).map_err(error)?;
                        folders.push(path);
                    }
                    // Never over a file that is there: the tree goes into a folder of its own.
                    0o100000 => file = Some(std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(error)?),
                    _ => skipped += 1,
                }
            }
            FILENAME if body <= 4096 => {
                let mut raw = vec![0u8; body as usize];
                reader.read_exact(&mut raw).map_err(error)?;
                let text = String::from_utf8_lossy(raw.strip_suffix(&[0]).unwrap_or(&raw)).into_owned();
                // One name, never a path: what would climb out or is unusable gets a plain one.
                name = Some(crate::ingest::clean_path([text.as_str()]).ok().filter(|p| p.components().count() == 1).unwrap_or_else(|| PathBuf::from("_")));
            }
            PAYLOAD => match file.take() {
                Some(mut out) => {
                    if io::copy(&mut (&mut reader).take(body), &mut out).map_err(error)? != body {
                        return Err(bad());
                    }
                }
                None => return Err(bad()),
            },
            _ => {
                if kind == GOODBYE {
                    folders.pop();
                }
                if io::copy(&mut (&mut reader).take(body), &mut io::sink()).map_err(error)? != body {
                    return Err(bad());
                }
            }
        }
    }
    if !folders.is_empty() {
        return Err(bad());
    }
    Ok(skipped)
}

/// The first of `base`, `base (1)`, ... (numbered as the engine numbers names) that no file, folder
/// or other download holds, its blob (`.casync.part`, or `.catar.part` for a tree) and the claim on
/// it, held for the whole download: a name whose blob an earlier run left is taken up again, one
/// with another download's `.part` skipped. Blocking.
fn target_of(base: &Path, tree: bool) -> Result<(PathBuf, PathBuf, crate::engine::TargetClaim), String> {
    let mut n = 0;
    loop {
        let target = crate::engine::numbered(base, n);
        let mut blob = target.clone().into_os_string();
        blob.push(if tree { ".catar.part" } else { ".casync.part" });
        let blob = PathBuf::from(blob);
        let taken = std::fs::symlink_metadata(&target).is_ok() || (!blob.exists() && crate::engine::part_path(&target).exists());
        if !taken {
            if let Some(claim) = crate::engine::claim_target(&target)? {
                return Ok((target, blob, claim));
            }
        }
        n += 1;
    }
}

/// Downloads what the index `url` lists (see the module) into `save_dir`: a `.caibx`'s file, named
/// after it, or a `.caidx`'s folder tree; numbered (`name (1)`) when that name is taken.
pub(crate) async fn download(
    url: &Url,
    options: &DownloadOptions,
    limiter: Option<Arc<RateLimiter>>,
    save_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let http = crate::engine::build_client(options)?;
    let unread = |e: reqwest::Error| format!("Cannot read the index: {}", e.without_url());
    let mut answer = http.get(url.clone()).send().await.and_then(|a| a.error_for_status()).map_err(unread)?;
    let mut bytes = Vec::new();
    while let Some(part) = answer.chunk().await.map_err(unread)? {
        bytes.extend_from_slice(&part);
        if bytes.len() as u64 > MAX_INDEX {
            return Err("the index is larger than an index can be".to_string());
        }
    }
    let index = Arc::new(parse_index(&bytes)?);
    let tree = url.path().to_ascii_lowercase().ends_with(".caidx");
    let last = url.path_segments().and_then(|mut s| s.next_back()).unwrap_or_default();
    let stem = percent_encoding::percent_decode_str(&last[..last.len().saturating_sub(6)]).decode_utf8_lossy().into_owned();
    let name = crate::ingest::clean_path([stem.as_str()]).unwrap_or_else(|_| PathBuf::from("blob"));
    let total = index.chunks.last().map_or(0, |c| c.start + c.size);
    if let Some(max) = options.max_filesize.filter(|&max| total > max) {
        return Err(crate::engine::too_large(max, Some(total)));
    }
    // The name and the room for it, as the engine takes them, before anything is written; a tree
    // needs room for its archive and its files.
    let (base, dir) = (save_dir.join(&name), save_dir.to_path_buf());
    let (target, blob, _claim) = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot write {}: {e}", dir.display()))?;
        let (target, blob, claim) = target_of(&base, tree)?;
        crate::engine::check_space(&dir, if tree { total.saturating_mul(2) } else { total }, 0, &blob)?;
        Ok::<_, String>((target, blob, claim))
    })
    .await
    .map_err(|e| e.to_string())??;
    let error = |e: io::Error| format!("Cannot write {}: {e}", blob.display());

    // What is on disk: the part file of an earlier run, and the older version.
    let mut out = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&blob).map_err(error)?;
    out.set_len(total).map_err(error)?;
    let (seed, local) = (options.delta_from.clone(), Arc::clone(&index));
    let (mut out, missing, held) = tokio::task::spawn_blocking(move || from_disk(&mut out, &local, seed.as_deref()).map(|(missing, held)| (out, missing, held)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(error)?;
    let fresh = held < total;
    let mut done = total - missing.iter().map(|c| c.size).sum::<u64>();

    // The rest from the store, several at once.
    let store = store_of(url, options.chunk_store.as_deref())?;
    let fetcher = Fetcher { http: &http, store: &store, cache: options.chunk_cache.as_deref(), limiter: limiter.as_deref(), sha512: index.sha512 };
    let fetcher = &fetcher;
    let (started, from_disk_bytes) = (Instant::now(), done);
    let mut fetched = stream::iter(missing).map(|chunk| async move { (fetcher.fetch(&chunk).await, chunk) }).buffer_unordered(options.max_connections_per_host.clamp(1, 16));
    loop {
        let next = tokio::select! {
            () = cancel.cancelled() => return Err(CANCELLED.to_string()),
            next = fetched.next() => next,
        };
        let Some((data, chunk)) = next else { break };
        let data = data?;
        out.seek(SeekFrom::Start(chunk.start)).and_then(|_| out.write_all(&data)).map_err(error)?;
        done += chunk.size;
        if let Some(tx) = &snapshots {
            let speed = (done - from_disk_bytes) as f64 / started.elapsed().as_secs_f64().max(0.001);
            let _ = tx.send(crate::helpers::snapshot(&crate::helpers::Progress { done, total, ratio: None, speed: Some(speed) }, &target));
        }
    }
    drop(fetched);
    out.sync_all().map_err(error)?;
    drop(out);
    if let Some(cache) = options.chunk_cache.clone() {
        let max = options.chunk_cache_max;
        let _ = tokio::task::spawn_blocking(move || trim_cache(&cache, max)).await;
    }

    if tree {
        let (archive, into) = (blob.clone(), target.clone());
        let skipped = tokio::task::spawn_blocking(move || unpack_catar(&archive, &into)).await.map_err(|e| e.to_string())??;
        if skipped > 0 {
            tracing::warn!("{skipped} link(s), device(s) or the like left out of {}", target.display());
        }
        let _ = std::fs::remove_file(&blob);
    } else {
        std::fs::rename(&blob, &target).map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
    }
    Ok((target, fresh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::tests::serve;

    /// A pseudo-random blob (a fixed xorshift), the same on every run.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// The index of `data` cut with these sizes, as casync writes it, and its chunks.
    fn index_of(data: &[u8], sizes: (u64, u64, u64)) -> (Vec<u8>, Vec<(Chunk, Vec<u8>)>) {
        index_with(data, sizes, true)
    }

    /// [`index_of`], its ids SHA-512/256 or (older indexes) SHA-256.
    fn index_with(data: &[u8], (min, avg, max): (u64, u64, u64), sha512: bool) -> (Vec<u8>, Vec<(Chunk, Vec<u8>)>) {
        let flags = if sha512 { SHA512_256 } else { 0 };
        let mut out: Vec<u8> = [48, INDEX, flags, min, avg, max, u64::MAX, TABLE].iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut found = Vec::new();
        chunks(data, min, avg, max, |start, bytes| {
            let id = id_of(bytes, sha512);
            out.extend((start + bytes.len() as u64).to_le_bytes());
            out.extend(id);
            found.push((Chunk { start, size: bytes.len() as u64, id }, bytes.to_vec()));
        })
        .unwrap();
        let table = 16 + 40 * (found.len() as u64 + 1);
        out.extend([0u64, 0, 48, table, TABLE_TAIL].iter().flat_map(|w| w.to_le_bytes()));
        (out, found)
    }

    const SIZES: (u64, u64, u64) = (1024, 4096, 16384);
    /// Chunks are cut where the content says, between min and max, so that bytes put in front move
    /// the cuts after them along with the content; an index reads back as written.
    #[test]
    fn chunks_are_cut_by_content_and_indexes_read_back() {
        let data = noise(200_000, 7);
        let (index, found) = index_of(&data, SIZES);
        assert!(found.len() > 20 && found.iter().rev().skip(1).all(|(c, _)| (1024..=16384).contains(&c.size)), "{}", found.len());
        assert_eq!(found.iter().map(|(c, _)| c.size).sum::<u64>(), data.len() as u64);
        let parsed = parse_index(&index).unwrap();
        assert_eq!((parsed.sha512, parsed.min, parsed.avg, parsed.max), (true, 1024, 4096, 16384));
        assert_eq!(parsed.chunks, found.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>());

        let mut shifted = noise(777, 9);
        shifted.extend_from_slice(&data);
        let (_, moved) = index_of(&shifted, SIZES);
        let ids: std::collections::HashSet<[u8; 32]> = moved.iter().map(|(c, _)| c.id).collect();
        let kept = found.iter().filter(|(c, _)| ids.contains(&c.id)).count();
        assert!(kept + 3 >= found.len(), "only the first chunks change: {kept} of {}", found.len());
        assert!(parse_index(&index[..index.len() - 8]).is_err() && parse_index(b"not an index").is_err());
    }

    /// A blob from a web store: the part file of an earlier run and an older version are used, the
    /// rest fetched and checked (a damaged chunk is an error) and kept in the cache.
    #[tokio::test]
    async fn a_blob_is_put_together_from_disk_and_the_store() {
        let data = noise(120_000, 3);
        let (index, found) = index_of(&data, SIZES);
        let pack = |bytes: &[u8]| ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest);
        let mut files: HashMap<String, Vec<u8>> = found.iter().map(|(c, bytes)| (format!("/store/{}", chunk_path(&c.id)), pack(bytes))).collect();
        files.insert("/os.caibx".to_string(), index.clone());
        files.insert("/bad/os.caibx".to_string(), index.clone());
        for (c, bytes) in &found {
            files.insert(format!("/bad/default.castr/{}", chunk_path(&c.id)), if c.id == found[2].0.id { b"junk".to_vec() } else { pack(bytes) });
        }
        let (addr, hits) = serve(move |path, _| match files.get(path) {
            Some(body) => (200, String::new(), body.clone()),
            None => (404, String::new(), Vec::new()),
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        // An older version holding the first half (moved along by a few bytes), and a part file
        // holding the last chunk at its place.
        let seed = dir.path().join("old");
        std::fs::write(&seed, [&b"older"[..], &data[..60_000]].concat()).unwrap();
        let last = &found.last().unwrap().0;
        let mut part = vec![0u8; data.len()];
        part[last.start as usize..].copy_from_slice(&data[last.start as usize..]);
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("os.casync.part"), &part).unwrap();
        let cache = dir.path().join("cache");
        let options = DownloadOptions { chunk_store: Some(format!("http://{addr}/store")), delta_from: Some(seed), chunk_cache: Some(cache.clone()), ..DownloadOptions::default() };
        let url = Url::parse(&format!("http://{addr}/os.caibx")).unwrap();
        let (file, fresh) = download(&url, &options, None, &out, None, CancellationToken::new()).await.unwrap();
        assert_eq!((file.clone(), fresh), (out.join("os"), true));
        assert_eq!(std::fs::read(&file).unwrap(), data);
        let asked = hits.lock().keys().filter(|k| k.starts_with("/store/")).count();
        assert!(asked > 0 && asked + 4 < found.len(), "the older version's chunks and the last are not fetched: {asked} of {}", found.len());
        let cached: usize = std::fs::read_dir(&cache).unwrap().flatten().map(|d| std::fs::read_dir(d.path()).unwrap().count()).sum();
        assert_eq!(cached, asked);

        // From the cache alone, with a store that does not exist, the chunks not cached fail.
        let again = DownloadOptions { chunk_store: Some(dir.path().join("none").display().to_string()), chunk_cache: Some(cache.clone()), ..DownloadOptions::default() };
        assert!(download(&url, &again, None, &dir.path().join("other"), None, CancellationToken::new()).await.is_err());
        let bad = Url::parse(&format!("http://{addr}/bad/os.caibx")).unwrap();
        let refused = download(&bad, &DownloadOptions::default(), None, &dir.path().join("bad"), None, CancellationToken::new()).await.unwrap_err();
        assert!(refused.contains("damaged"), "{refused}");
    }

    /// A catar archive as casync packs it: `docs/a.txt` ("hello"), a file named `..` ("up") and a
    /// link.
    fn sample_archive() -> Vec<u8> {
        fn record(kind: u64, body: &[u8]) -> Vec<u8> {
            [&(16 + body.len() as u64).to_le_bytes()[..], &kind.to_le_bytes(), body].concat()
        }
        let entry = |mode: u64| record(ENTRY, &[0u64, mode, 0, 0, 0, 0].iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<_>>());
        let name = |n: &str| record(FILENAME, &[n.as_bytes(), &[0]].concat());
        [
            entry(0o040755),
            name("docs"),
            entry(0o040755),
            name("a.txt"),
            entry(0o100644),
            record(PAYLOAD, b"hello"),
            record(GOODBYE, &[0u8; 24]),
            name(".."),
            entry(0o100644),
            record(PAYLOAD, b"up"),
            name("link"),
            entry(0o120777),
            record(0x664a6fb6830e0d6c, b"/etc/passwd\0"),
            record(GOODBYE, &[0u8; 24]),
        ]
        .concat()
    }

    /// A catar archive's folders and files unpack; a name that would climb out cannot, and a link
    /// is left out.
    #[test]
    fn a_folder_tree_unpacks_from_its_archive() {
        let archive = sample_archive();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.catar"), &archive).unwrap();
        let into = dir.path().join("tree");
        assert_eq!(unpack_catar(&dir.path().join("t.catar"), &into).unwrap(), 1, "the link is left out");
        assert_eq!(std::fs::read(into.join("docs").join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(into.join("_")).unwrap(), b"up");
        assert!(!dir.path().join("up").exists() && !into.join("link").exists());
        std::fs::write(dir.path().join("cut.catar"), &archive[..archive.len() - 30]).unwrap();
        assert!(unpack_catar(&dir.path().join("cut.catar"), &dir.path().join("cut")).is_err());
    }

    /// A file or folder already there under the index's name stays as it is: the download takes the
    /// next free name, as other downloads do, and leaves no part or lock file. A `.caidx` downloads
    /// whole into its folder, and an older SHA-256 index works as a SHA-512/256 one does.
    #[tokio::test]
    async fn a_name_that_is_taken_is_numbered_and_a_tree_downloads_whole() {
        let data = noise(50_000, 5);
        let pack = |bytes: &[u8]| ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest);
        let mut files = HashMap::new();
        for (index, blob, sha512) in [("/os.caibx", data.clone(), true), ("/tree.caidx", sample_archive(), true), ("/old/os.caibx", data.clone(), false)] {
            let (bytes, found) = index_with(&blob, SIZES, sha512);
            let store = index.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
            files.insert(index.to_string(), bytes);
            for (c, chunk) in found {
                files.insert(format!("{store}/default.castr/{}", chunk_path(&c.id)), pack(&chunk));
            }
        }
        let (addr, _) = serve(move |path, _| match files.get(path) {
            Some(body) => (200, String::new(), body.clone()),
            None => (404, String::new(), Vec::new()),
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("os"), b"mine").unwrap();
        std::fs::create_dir(dir.path().join("tree")).unwrap();
        std::fs::write(dir.path().join("tree").join("a.txt"), b"mine too").unwrap();
        let get = |link: &str, into: PathBuf| {
            let url = Url::parse(&format!("http://{addr}/{link}")).unwrap();
            async move { download(&url, &DownloadOptions::default(), None, &into, None, CancellationToken::new()).await.unwrap() }
        };

        let (file, fresh) = get("os.caibx", dir.path().to_path_buf()).await;
        assert_eq!((file.clone(), fresh), (dir.path().join("os (1)"), true));
        assert_eq!(std::fs::read(&file).unwrap(), data);
        let (tree, _) = get("tree.caidx", dir.path().to_path_buf()).await;
        assert_eq!(tree, dir.path().join("tree (1)"));
        assert_eq!(std::fs::read(tree.join("docs").join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(tree.join("_")).unwrap(), b"up");
        assert_eq!(std::fs::read(dir.path().join("os")).unwrap(), b"mine");
        assert_eq!(std::fs::read(dir.path().join("tree").join("a.txt")).unwrap(), b"mine too");
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["os", "os (1)", "tree", "tree (1)"]);

        let (old, _) = get("old/os.caibx", dir.path().join("old")).await;
        assert_eq!(std::fs::read(&old).unwrap(), data);
    }

    /// An index whose blob is larger than the free disk space, or than `max_filesize`, is refused
    /// before anything is written.
    #[tokio::test]
    async fn an_index_larger_than_the_disk_or_the_limit_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (step, count) = (64u64 << 20, crate::media::free_space(dir.path()).unwrap() / (64 << 20) + 1024);
        let mut huge: Vec<u8> = [48, INDEX, SHA512_256, 16 << 20, 32 << 20, 64 << 20, u64::MAX, TABLE].iter().flat_map(|w| w.to_le_bytes()).collect();
        for n in 1..=count {
            huge.extend((n * step).to_le_bytes());
            huge.extend([7u8; 32]);
        }
        huge.extend([0u64, 0, 48, 16 + 40 * (count + 1), TABLE_TAIL].iter().flat_map(|w| w.to_le_bytes()));
        let small = index_of(&noise(5000, 1), SIZES).0;
        let (addr, _) = serve(move |path, _| match path {
            "/huge.caibx" => (200, String::new(), huge.clone()),
            "/small.caibx" => (200, String::new(), small.clone()),
            _ => (404, String::new(), Vec::new()),
        })
        .await;
        let out = dir.path().join("out");
        let url = |name: &str| Url::parse(&format!("http://{addr}/{name}")).unwrap();
        let refused = download(&url("huge.caibx"), &DownloadOptions::default(), None, &out, None, CancellationToken::new()).await.unwrap_err();
        assert!(refused.starts_with("Not enough disk space"), "{refused}");
        let limited = DownloadOptions { max_filesize: Some(1000), ..DownloadOptions::default() };
        let refused = download(&url("small.caibx"), &limited, None, &out, None, CancellationToken::new()).await.unwrap_err();
        assert!(refused.starts_with("File is larger than the limit"), "{refused}");
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "nothing is left on disk");
    }

    /// The cache drops its oldest chunks past its size, and never a file of the user's that sits
    /// in the same folder.
    #[test]
    fn the_cache_trims_only_its_own_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let mine = ["notes.txt", "docs/notes.txt", "0101/readme.txt", &format!("abcd/{}.cacnk", hex(&[3; 32]))].map(|p| dir.path().join(p));
        for path in &mine {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, [0u8; 5000]).unwrap();
        }
        let chunks = [[1u8; 32], [2; 32], [3; 32]].map(|id| dir.path().join(chunk_path(&id)));
        for path in &chunks {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, [0u8; 1000]).unwrap();
        }
        trim_cache(dir.path(), 1500);
        assert_eq!(chunks.iter().filter(|p| p.exists()).count(), 1);
        trim_cache(dir.path(), 0);
        assert!(chunks.iter().all(|p| !p.exists()));
        assert!(mine.iter().all(|p| p.exists()), "the user's files stay");
    }

    /// The chunker's cuts and ids for a fixed input with casync's default sizes. They pin the
    /// implementation that was checked, outside the repository, against desync's published test
    /// vector (its 1 MiB input file is not kept here).
    #[test]
    fn the_chunker_cuts_a_fixed_input_where_it_always_has() {
        let data = noise(1 << 20, 1);
        let mut cuts = Vec::new();
        chunks(&data[..], 16 * 1024, 64 * 1024, 256 * 1024, |start, bytes| cuts.push((start, bytes.len(), hex(&id_of(bytes, true))))).unwrap();
        let first: Vec<(u64, usize, &str)> = cuts.iter().take(3).map(|(s, l, id)| (*s, *l, id.as_str())).collect();
        assert_eq!(first, CUTS);
    }
    const CUTS: [(u64, usize, &str); 3] = [
        (0, 95399, "5f59f82217d24f50583e7a5d9d8476fd82139da7f996d45aa0fcdc4538220ee8"),
        (95399, 19035, "836115cb3538575e6f7fa55f8d09510eb4023c2d4ba1976de4aefa261cda9024"),
        (114434, 74097, "77693b813b78e1a73c72a70fa0c039269fc9efae99ba1d50b3b04644e3249338"),
    ];
}
