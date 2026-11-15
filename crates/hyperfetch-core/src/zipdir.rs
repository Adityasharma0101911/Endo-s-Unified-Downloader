//! Lists what a ZIP on a server holds without downloading it: a range request for its end, where
//! its central directory is found, and one more for that directory when it lies further back.
//! Downloads one of its files the same way (see [`extract`]): its local header, then its data,
//! unpacked as it arrives.

use crate::worker::Auth;
use flate2::{Decompress, FlushDecompress, Status};
use reqwest::header::{ACCEPT_ENCODING, CONTENT_RANGE, RANGE};
use reqwest::{Client, Response, StatusCode};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use url::Url;

/// Bytes read from the end of the file: the end of central directory record and its comment.
const TAIL: u64 = 64 * 1024;
/// Longest wait for an answer, and between two reads of its body.
const IDLE: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 30 });
/// Largest central directory read.
const MAX_DIRECTORY: u64 = 16 * 1024 * 1024;
/// Most entries listed.
const MAX_ENTRIES: u64 = 100_000;
const END_SIG: u32 = 0x0605_4b50;
const END_LEN: usize = 22;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_LOCATOR_LEN: usize = 20;
const ZIP64_END_SIG: u32 = 0x0606_4b50;
const ZIP64_END_LEN: u64 = 56;
const ENTRY_SIG: u32 = 0x0201_4b50;
const ENTRY_LEN: usize = 46;
const LOCAL_SIG: u32 = 0x0403_4b50;
const LOCAL_LEN: u64 = 30;
const STORED: u16 = 0;
const DEFLATED: u16 = 8;
/// Code page 437 from 0x80 on, which names without the UTF-8 flag use (below it is ASCII).
const CP437_HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";

/// One file or folder in a ZIP.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ZipEntry {
    /// Its path in the archive, '/'-separated; a folder's ends with '/'.
    pub name: String,
    /// Bytes once extracted.
    pub size: u64,
    /// Bytes in the archive.
    pub compressed: u64,
    /// When it was last changed, as the archive records it ("2024-05-17 13:45:30", local time).
    pub modified: Option<String>,
    /// How it is packed: 0 stored, 8 deflated (the two [`extract`] reads), or another method.
    pub method: u16,
    /// The CRC-32 of its extracted bytes.
    pub crc32: u32,
    /// Where its local header starts in the archive.
    pub offset: u64,
    /// Its data is encrypted.
    pub encrypted: bool,
}

impl ZipEntry {
    /// A folder, which has no data of its own.
    pub fn is_folder(&self) -> bool {
        self.name.ends_with('/')
    }

    /// Why this entry cannot be downloaded on its own, if it cannot: encrypted, or packed with a
    /// method other than store and deflate.
    pub fn unsupported(&self) -> Option<String> {
        if self.is_folder() {
            return None;
        }
        if self.encrypted {
            return Some(format!("{} is encrypted, which only a download of the whole ZIP can open", self.name));
        }
        let method = match self.method {
            STORED | DEFLATED => return None,
            9 => "Deflate64",
            12 => "bzip2",
            14 => "LZMA",
            93 => "Zstandard",
            95 => "XZ",
            98 => "PPMd",
            _ => "a method",
        };
        Some(format!("{} is packed with {} (method {}), which cannot be unpacked on its own: download the whole ZIP", self.name, method, self.method))
    }

    /// Where the entry goes below a folder: the parts of its name, each made a valid file name;
    /// None when one is ".." or nothing is left. A leading '/' or drive only names a folder.
    pub fn local_path(&self) -> Option<PathBuf> {
        let mut path = PathBuf::new();
        for part in self.name.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".") {
            let clean = crate::engine::sanitize_component(part);
            if part == ".." || clean.is_empty() {
                return None;
            }
            path.push(clean);
        }
        (!path.as_os_str().is_empty()).then_some(path)
    }
}

/// The entries of the ZIP at `url`, read from its end and central directory with range requests,
/// at most 64 KiB and 16 MiB of them. Fails when the server sends no parts of files, or the file
/// is no ZIP, a damaged one or one split over several files, or lists more than 100 000 entries.
pub async fn list_zip(client: &Client, url: &Url) -> Result<Vec<ZipEntry>, String> {
    let (tail, size) = fetch(client, None, url, None, TAIL).await?;
    let tail_start = size - tail.len() as u64;
    let within = |start: u64, len: u64| {
        let from = usize::try_from(start.checked_sub(tail_start)?).ok()?;
        tail.get(from..from.checked_add(usize::try_from(len).ok()?)?)
    };
    let dir = match end_of_directory(&tail)? {
        End::Directory(dir) => dir,
        End::Zip64At(at) => match within(at, ZIP64_END_LEN) {
            Some(record) => zip64_end(record)?,
            None => zip64_end(&fetch(client, None, url, Some(at), ZIP64_END_LEN).await?.0)?,
        },
    };
    dir.check(size)?;
    match within(dir.offset, dir.size) {
        Some(listing) => entries(listing, dir.entries),
        None => entries(&fetch(client, None, url, Some(dir.offset), dir.size).await?.0, dir.entries),
    }
}

/// `len` bytes of `url` from `start`, or its last `len` bytes without one, and the file's size.
/// Redirects to another host are followed even on a client that stops at them (see
/// `worker::send`); a server quiet for `IDLE` fails it.
async fn fetch(client: &Client, auth: Option<&Auth>, url: &Url, start: Option<u64>, len: u64) -> Result<(Vec<u8>, u64), String> {
    let (mut resp, served, size) = ranged(client, auth, url, start, len).await?;
    let mut body = Vec::new();
    while let Some(chunk) = next(&mut resp, url).await? {
        body.extend_from_slice(&chunk);
        if body.len() as u64 > served {
            return Err(format!("{} sent more than the {} bytes asked for", url, served));
        }
    }
    if body.len() as u64 != served {
        return Err(format!("{} sent {} bytes for a range of {}", url, body.len(), served));
    }
    Ok((body, size))
}

/// The answer to a request for `len` bytes of `url` from `start` (its last `len` without one),
/// once its Content-Range is checked: with the bytes it holds and the file's size. `auth` goes
/// only where it covers (see `worker::authorize`).
async fn ranged(client: &Client, auth: Option<&Auth>, url: &Url, start: Option<u64>, len: u64) -> Result<(Response, u64, u64), String> {
    let range = match start {
        Some(start) => format!("bytes={}-{}", start, start + len.max(1) - 1),
        None => format!("bytes=-{}", len),
    };
    let fail = |e: reqwest::Error| format!("Cannot read {}: {}", url, e);
    let request = |url: &Url| crate::worker::authorize(client.get(url.clone()), auth, url).header(RANGE, range.as_str()).header(ACCEPT_ENCODING, "identity");
    let resp = timeout(IDLE, crate::worker::send(request, url)).await.map_err(|_| quiet(url))?.map_err(fail)?;
    match resp.status() {
        StatusCode::PARTIAL_CONTENT => {}
        status if status.is_success() => {
            return Err(format!("{} does not send parts of files, so what the ZIP holds cannot be read without downloading it whole", url));
        }
        status => return Err(format!("Cannot read {}: HTTP {}", url, status)),
    }
    let header = resp.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    match crate::range::ByteRange::parse_content_range(&header) {
        Ok((served, Some(size)))
            if served.len() <= len && start.map_or(served.end + 1 == size, |s| served.start == s) =>
        {
            Ok((resp, served.len(), size))
        }
        _ => Err(format!("{} answered a range request with Content-Range {:?}", url, header)),
    }
}

/// The next block of `resp`'s body; a server quiet for `IDLE` fails it.
async fn next(resp: &mut Response, url: &Url) -> Result<Option<bytes::Bytes>, String> {
    timeout(IDLE, resp.chunk()).await.map_err(|_| quiet(url))?.map_err(|e| format!("Cannot read {}: {}", url, e))
}

fn quiet(url: &Url) -> String {
    format!("Cannot read {}: no answer for {} seconds", url, IDLE.as_secs())
}

/// Downloads `entry` of the ZIP at `url` into `out`, unpacked as it arrives, and checks it against
/// the size and CRC-32 the ZIP's directory gives: a range request for its local header, which says
/// where its data starts, then one for the data. `limiter` holds the packed bytes to the download's
/// speed; `written` is told how many unpacked bytes are out so far. Fails for an entry
/// [`ZipEntry::unsupported`] names, and when the data runs past what the directory says.
pub async fn extract<W: AsyncWrite + Unpin>(
    client: &Client,
    auth: Option<&Auth>,
    url: &Url,
    entry: &ZipEntry,
    out: &mut W,
    limiter: Option<&crate::worker::RateLimiter>,
    written: &mut (dyn FnMut(u64) + Send),
) -> Result<(), String> {
    if let Some(why) = entry.unsupported() {
        return Err(why);
    }
    let (header, size) = fetch(client, auth, url, Some(entry.offset), LOCAL_LEN).await?;
    if u32_at(&header, 0) != Some(LOCAL_SIG) {
        return Err(format!("The ZIP has no file where its directory puts {}", entry.name));
    }
    let skip = |at| u16_at(&header, at).map(u64::from).ok_or_else(damaged);
    let start = entry.offset + LOCAL_LEN + skip(26)? + skip(28)?;
    if start.checked_add(entry.compressed).is_none_or(|end| end > size) {
        return Err(damaged());
    }
    let damaged_data = |e: flate2::DecompressError| format!("{}'s data is damaged: {}", entry.name, e);
    let too_long = || format!("{} unpacks to more than the {} bytes the ZIP says", entry.name, entry.size);
    let mut inflate = (entry.method == DEFLATED).then(|| Decompress::new(false));
    let (mut crc, mut buf) = (flate2::Crc::new(), vec![0u8; 256 * 1024]);
    let (mut read, mut made, mut ended) = (0u64, 0u64, entry.method == STORED);
    let mut resp = match entry.compressed {
        0 => None,
        len => Some(ranged(client, auth, url, Some(start), len).await?.0),
    };
    while let Some(chunk) = match resp.as_mut() {
        Some(resp) => next(resp, url).await?,
        None => None,
    } {
        read += chunk.len() as u64;
        if read > entry.compressed {
            return Err(format!("{} sent more than the {} bytes of {}", url, entry.compressed, entry.name));
        }
        if let Some(limiter) = limiter {
            limiter.acquire(chunk.len() as u64).await;
        }
        let Some(z) = inflate.as_mut() else {
            made += chunk.len() as u64;
            if made > entry.size {
                return Err(too_long());
            }
            crc.update(&chunk);
            out.write_all(&chunk).await.map_err(|e| format!("Write error: {}", e))?;
            written(made);
            continue;
        };
        let mut input = &chunk[..];
        while !ended {
            let (taken, given) = (z.total_in(), z.total_out());
            ended = z.decompress(input, &mut buf, FlushDecompress::None).map_err(damaged_data)? == Status::StreamEnd;
            let (used, out_now) = ((z.total_in() - taken) as usize, (z.total_out() - given) as usize);
            input = &input[used..];
            made += out_now as u64;
            if made > entry.size {
                return Err(too_long());
            }
            crc.update(&buf[..out_now]);
            out.write_all(&buf[..out_now]).await.map_err(|e| format!("Write error: {}", e))?;
            // A full buffer may leave more to come out of what was already put in.
            if out_now < buf.len() && (input.is_empty() || (used == 0 && out_now == 0)) {
                break;
            }
        }
        written(made);
    }
    out.flush().await.map_err(|e| format!("Write error: {}", e))?;
    if read != entry.compressed || !ended {
        return Err(format!("{} ended after {} of the {} bytes of {}", url, read, entry.compressed, entry.name));
    }
    if made != entry.size {
        return Err(format!("{} unpacks to {} bytes, not the {} the ZIP says", entry.name, made, entry.size));
    }
    match crc.sum() {
        sum if sum == entry.crc32 => Ok(()),
        sum => Err(format!("{} fails its CRC-32 check: the ZIP says {:08x}, its data gives {:08x}", entry.name, entry.crc32, sum)),
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

fn damaged() -> String {
    "The ZIP's directory is damaged".to_string()
}

/// Where a ZIP's central directory is, and how many entries it lists.
#[derive(Debug, PartialEq)]
struct Directory {
    offset: u64,
    size: u64,
    entries: u64,
}

impl Directory {
    /// Whether the directory fits the caps and a `size`-byte file.
    fn check(&self, size: u64) -> Result<(), String> {
        if self.size > MAX_DIRECTORY || self.entries > MAX_ENTRIES {
            return Err(format!(
                "The ZIP lists {} entries in {} bytes, more than can be shown ({} entries, {} bytes)",
                self.entries, self.size, MAX_ENTRIES, MAX_DIRECTORY
            ));
        }
        // Every entry takes `ENTRY_LEN` bytes at least.
        let room = self.entries.checked_mul(ENTRY_LEN as u64).is_some_and(|need| need <= self.size);
        if !room || self.offset.checked_add(self.size).is_none_or(|end| end > size) {
            return Err(damaged());
        }
        Ok(())
    }
}

/// What the end of a ZIP says.
#[derive(Debug, PartialEq)]
enum End {
    Directory(Directory),
    /// A ZIP64 archive, whose end of central directory record is at this offset.
    Zip64At(u64),
}

/// Finds the end of central directory record in `tail`, the file's last bytes: the last one whose
/// comment fits.
fn end_of_directory(tail: &[u8]) -> Result<End, String> {
    let at = (0..=tail.len().saturating_sub(END_LEN))
        .rev()
        .find(|&at| {
            u32_at(tail, at) == Some(END_SIG)
                && u16_at(tail, at + 20).is_some_and(|comment| at + END_LEN + usize::from(comment) <= tail.len())
        })
        .ok_or("The file is not a ZIP (or its comment is too long to find its end)")?;
    let field = |offset| u16_at(tail, at + offset).ok_or_else(damaged);
    if field(4)? != 0 || field(6)? != 0 {
        return Err("The ZIP is split over several files".to_string());
    }
    if let Some(locator) = at.checked_sub(ZIP64_LOCATOR_LEN).filter(|&l| u32_at(tail, l) == Some(ZIP64_LOCATOR_SIG)) {
        return u64_at(tail, locator + 8).map(End::Zip64At).ok_or_else(damaged);
    }
    let (entries, size, offset) = (field(10)?, u32_at(tail, at + 12).ok_or_else(damaged)?, u32_at(tail, at + 16).ok_or_else(damaged)?);
    if entries == u16::MAX || size == u32::MAX || offset == u32::MAX {
        return Err(damaged());
    }
    Ok(End::Directory(Directory { offset: u64::from(offset), size: u64::from(size), entries: u64::from(entries) }))
}

/// The directory a ZIP64 end of central directory record names.
fn zip64_end(record: &[u8]) -> Result<Directory, String> {
    if u32_at(record, 0) != Some(ZIP64_END_SIG) {
        return Err(damaged());
    }
    if u32_at(record, 16) != Some(0) || u32_at(record, 20) != Some(0) {
        return Err("The ZIP is split over several files".to_string());
    }
    let field = |at| u64_at(record, at).ok_or_else(damaged);
    Ok(Directory { entries: field(32)?, size: field(40)?, offset: field(48)? })
}

/// The `count` entries of the central directory `dir`.
fn entries(dir: &[u8], count: u64) -> Result<Vec<ZipEntry>, String> {
    let mut list = Vec::with_capacity(count.min(MAX_ENTRIES) as usize);
    let mut at = 0;
    for _ in 0..count {
        if u32_at(dir, at) != Some(ENTRY_SIG) {
            return Err(damaged());
        }
        let short = |offset| u16_at(dir, at + offset).map(usize::from).ok_or_else(damaged);
        let long = |offset| u32_at(dir, at + offset).ok_or_else(damaged);
        let (flags, method, time, date, crc32) = (short(8)?, short(10)?, short(12)?, short(14)?, long(16)?);
        let (mut compressed, mut size) = (u64::from(long(20)?), u64::from(long(24)?));
        let (name_len, extra_len, comment_len) = (short(28)?, short(30)?, short(32)?);
        let mut offset = u64::from(long(42)?);
        let name_at = at + ENTRY_LEN;
        let name = dir.get(name_at..name_at + name_len).ok_or_else(damaged)?;
        let extra = dir.get(name_at + name_len..name_at + name_len + extra_len).ok_or_else(damaged)?;
        // The ZIP64 extra field holds, in order, the sizes and offset too large for their own fields.
        let mut zip64 = zip64_sizes(extra).into_iter();
        for field in [&mut size, &mut compressed, &mut offset] {
            if *field == u64::from(u32::MAX) {
                *field = zip64.next().ok_or_else(damaged)?;
            }
        }
        let name = if flags & 0x800 != 0 {
            String::from_utf8_lossy(name).into_owned()
        } else {
            name.iter().map(|&b| if b < 0x80 { char::from(b) } else { CP437_HIGH.chars().nth(usize::from(b - 0x80)).unwrap_or('?') }).collect()
        };
        let (method, encrypted) = (method as u16, flags & 1 != 0);
        list.push(ZipEntry { name, size, compressed, modified: dos_time(date as u16, time as u16), method, crc32, offset, encrypted });
        at = name_at + name_len + extra_len + comment_len;
    }
    Ok(list)
}

/// The values of the ZIP64 extended information field (0x0001) in an entry's `extra` fields.
fn zip64_sizes(extra: &[u8]) -> Vec<u64> {
    let mut at = 0;
    while let (Some(id), Some(len)) = (u16_at(extra, at), u16_at(extra, at + 2)) {
        let data = extra.get(at + 4..at + 4 + usize::from(len)).unwrap_or_default();
        if id == 1 {
            return data.chunks_exact(8).filter_map(|v| v.try_into().ok().map(u64::from_le_bytes)).collect();
        }
        at += 4 + usize::from(len);
    }
    Vec::new()
}

/// An MS-DOS date and time as "YYYY-MM-DD HH:MM:SS"; None for no date or an impossible one.
fn dos_time(date: u16, time: u16) -> Option<String> {
    let (year, month, day) = (1980 + (date >> 9), (date >> 5) & 0xf, date & 0x1f);
    let (hour, minute, second) = (time >> 11, (time >> 5) & 0x3f, (time & 0x1f) * 2);
    let valid = (1..=12).contains(&month) && day >= 1 && hour < 24 && minute < 60 && second < 60;
    valid.then(|| format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const DATE: u16 = ((2024 - 1980) << 9) | (5 << 5) | 17;
    const TIME: u16 = (13 << 11) | (45 << 5) | 15;

    /// A stored (uncompressed) ZIP of `files` (name, UTF-8 flag, contents) with `comment`; in
    /// ZIP64 form with `zip64`, where the first entry claims `huge` bytes.
    fn zip(files: &[(&[u8], bool, &[u8])], comment: &[u8], zip64: Option<u64>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut directory = Vec::new();
        for (i, (name, utf8, data)) in files.iter().enumerate() {
            let offset = out.len() as u32;
            let flags: u16 = if *utf8 { 0x800 } else { 0 };
            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend([20u16, flags, 0, TIME, DATE].iter().flat_map(|v| v.to_le_bytes()));
            out.extend([0u32, data.len() as u32, data.len() as u32].iter().flat_map(|v| v.to_le_bytes()));
            out.extend([name.len() as u16, 0].iter().flat_map(|v| v.to_le_bytes()));
            out.extend_from_slice(name);
            out.extend_from_slice(data);
            let huge = zip64.filter(|_| i == 0);
            let extra: Vec<u8> = match huge {
                Some(huge) => [1u16.to_le_bytes(), 16u16.to_le_bytes()].concat().into_iter().chain(huge.to_le_bytes()).chain((huge / 2).to_le_bytes()).collect(),
                None => Vec::new(),
            };
            let (size, packed) = if huge.is_some() { (u32::MAX, u32::MAX) } else { (data.len() as u32, data.len() as u32) };
            directory.extend(ENTRY_SIG.to_le_bytes());
            directory.extend([45u16, 20, flags, 0, TIME, DATE].iter().flat_map(|v| v.to_le_bytes()));
            directory.extend([0u32, packed, size].iter().flat_map(|v| v.to_le_bytes()));
            directory.extend([name.len() as u16, extra.len() as u16, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()));
            directory.extend([0u32, offset].iter().flat_map(|v| v.to_le_bytes()));
            directory.extend_from_slice(name);
            directory.extend(extra);
        }
        let (offset, size, count) = (out.len() as u64, directory.len() as u64, files.len() as u64);
        out.extend(directory);
        if zip64.is_some() {
            let record = out.len() as u64;
            out.extend(ZIP64_END_SIG.to_le_bytes());
            out.extend(44u64.to_le_bytes());
            out.extend([45u16, 45].iter().flat_map(|v| v.to_le_bytes()));
            out.extend([0u32, 0].iter().flat_map(|v| v.to_le_bytes()));
            out.extend([count, count, size, offset].iter().flat_map(|v| v.to_le_bytes()));
            out.extend(ZIP64_LOCATOR_SIG.to_le_bytes());
            out.extend(0u32.to_le_bytes());
            out.extend(record.to_le_bytes());
            out.extend(1u32.to_le_bytes());
        }
        let small = |v: u64, max: u64| if zip64.is_some() { max } else { v };
        out.extend(END_SIG.to_le_bytes());
        out.extend([0u16, 0, small(count, 0xffff) as u16, small(count, 0xffff) as u16].iter().flat_map(|v| v.to_le_bytes()));
        out.extend([small(size, 0xffff_ffff) as u32, small(offset, 0xffff_ffff) as u32].iter().flat_map(|v| v.to_le_bytes()));
        out.extend((comment.len() as u16).to_le_bytes());
        out.extend_from_slice(comment);
        out
    }

    /// What `list_zip` gives for the file `data`, without a server: the same steps on its bytes.
    fn list_bytes(data: &[u8]) -> Result<Vec<ZipEntry>, String> {
        let size = data.len() as u64;
        let tail_start = size.saturating_sub(TAIL);
        let read = |start: u64, len: u64| {
            let end = start.checked_add(len).filter(|&end| end <= size).ok_or("out of range")?;
            Ok::<_, String>(&data[start as usize..end as usize])
        };
        let dir = match end_of_directory(&data[tail_start as usize..])? {
            End::Directory(dir) => dir,
            End::Zip64At(at) => zip64_end(read(at, ZIP64_END_LEN)?)?,
        };
        dir.check(size)?;
        entries(read(dir.offset, dir.size)?, dir.entries)
    }

    /// Serves `data` with ranges (suffix ranges too) unless `ranges` is off; counts requests.
    async fn serve(data: Vec<u8>, ranges: bool) -> (Url, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (data, count) = (Arc::new(data), Arc::new(AtomicUsize::new(0)));
        let requests = Arc::clone(&count);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (data, requests) = (Arc::clone(&data), Arc::clone(&requests));
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    requests.fetch_add(1, Ordering::SeqCst);
                    let req = String::from_utf8_lossy(&req).to_ascii_lowercase();
                    let len = data.len() as u64;
                    let spec = req.lines().find_map(|l| l.strip_prefix("range: bytes=")).map(str::trim).filter(|_| ranges);
                    let range = spec.and_then(|spec| match spec.split_once('-')? {
                        ("", n) => Some((len.saturating_sub(n.parse().ok()?), len - 1)),
                        (s, e) => Some((s.parse().ok()?, e.parse::<u64>().ok()?.min(len - 1))),
                    });
                    let resp = match range {
                        Some((s, e)) => {
                            let body = &data[s as usize..=e as usize];
                            let head = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{len}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            [head.as_bytes(), body].concat()
                        }
                        None => {
                            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n");
                            [head.as_bytes(), &data[..]].concat()
                        }
                    };
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (Url::parse(&format!("http://{addr}/a.zip")).unwrap(), count)
    }

    #[test]
    fn cp437_covers_the_high_half() {
        assert_eq!(CP437_HIGH.chars().count(), 128);
    }

    #[tokio::test]
    async fn lists_a_zip_from_its_end_in_one_request() {
        let data = zip(&[(b"docs/", true, b""), ("caf\u{e9}.txt".as_bytes(), true, b"hello"), (b"\x81ber.txt", false, b"cp437")], b"made by a test", None);
        let (url, requests) = serve(data, true).await;
        let listed = list_zip(&Client::new(), &url).await.unwrap();
        let when = Some("2024-05-17 13:45:30".to_string());
        assert_eq!(
            listed,
            [
                ZipEntry { name: "docs/".into(), size: 0, compressed: 0, modified: when.clone(), ..Default::default() },
                ZipEntry { name: "caf\u{e9}.txt".into(), size: 5, compressed: 5, modified: when.clone(), offset: 35, ..Default::default() },
                ZipEntry { name: "\u{fc}ber.txt".into(), size: 5, compressed: 5, modified: when, offset: 79, ..Default::default() },
            ]
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_directory_further_back_takes_a_second_request() {
        let names: Vec<String> = (0..2000).map(|i| format!("folder/a-rather-long-file-name-{i:05}.bin")).collect();
        let files: Vec<(&[u8], bool, &[u8])> = names.iter().map(|n| (n.as_bytes(), false, &b"x"[..])).collect();
        let data = zip(&files, b"", None);
        let (url, requests) = serve(data, true).await;
        let listed = list_zip(&Client::new(), &url).await.unwrap();
        assert_eq!(listed.len(), 2000);
        assert_eq!(listed[1999].name, names[1999]);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reads_zip64_sizes_and_counts() {
        let data = zip(&[(b"disc.img", true, b"abc"), (b"b.txt", true, b"b")], b"", Some(5 << 30));
        let (url, _) = serve(data.clone(), true).await;
        let listed = list_zip(&Client::new(), &url).await.unwrap();
        assert_eq!((listed[0].size, listed[0].compressed), (5 << 30, 5 << 29));
        assert_eq!((listed[1].name.as_str(), listed[1].size), ("b.txt", 1));
        assert_eq!(list_bytes(&data).unwrap(), listed);
    }

    #[tokio::test]
    async fn needs_ranges_and_a_zip() {
        let (url, requests) = serve(zip(&[(b"a", true, b"a")], b"", None), false).await;
        let err = list_zip(&Client::new(), &url).await.unwrap_err();
        assert!(err.contains("does not send parts of files"), "{err}");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        let (url, _) = serve(b"just some text, no archive at all".to_vec(), true).await;
        assert!(list_zip(&Client::new(), &url).await.unwrap_err().contains("not a ZIP"));
    }

    /// Serves every request with `answer` (taking `wait` first) on a port of its own.
    async fn answering(answer: String, wait: Duration) -> Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let answer = answer.clone();
                tokio::spawn(async move {
                    let _ = sock.read(&mut [0u8; 1024]).await;
                    tokio::time::sleep(wait).await;
                    let _ = sock.write_all(answer.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Url::parse(&format!("http://{addr}/a.zip")).unwrap()
    }

    /// A client carrying the user's headers stops at a redirect to another host; the listing
    /// follows it (as a download does), without those headers.
    #[tokio::test]
    async fn follows_a_redirect_to_another_host() {
        let (zip_url, _) = serve(zip(&[(b"a.txt", true, b"a")], b"", None), true).await;
        let moved = answering(format!("HTTP/1.1 302 Found\r\nLocation: {zip_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"), Duration::ZERO).await;
        let options = crate::engine::DownloadOptions { extra_headers: vec![("X-Team".into(), "t".into())], ..Default::default() };
        let client = crate::engine::build_client(&options).unwrap();
        let listed = list_zip(&client, &moved).await.unwrap();
        assert_eq!(listed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a.txt"]);
    }

    /// A server that never answers fails the listing instead of hanging it.
    #[tokio::test]
    async fn a_silent_server_fails_in_time() {
        let silent = answering(String::new(), Duration::from_secs(10)).await;
        let started = std::time::Instant::now();
        let err = list_zip(&Client::new(), &silent).await.unwrap_err();
        assert!(err.contains("no answer"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn hostile_directories_are_refused_before_they_are_read() {
        let mut data = zip(&[(b"a", true, b"a")], b"", None);
        let end = data.len() - END_LEN;
        // A directory claimed to be 32 MiB, at the start of a 32 MiB-and-a-bit file.
        data[end + 12..end + 16].copy_from_slice(&(32u32 << 20).to_le_bytes());
        let end_record = data.split_off(end);
        data.resize(data.len() + (32 << 20), 0);
        data.extend(end_record);
        let (url, requests) = serve(data, true).await;
        let err = list_zip(&Client::new(), &url).await.unwrap_err();
        assert!(err.contains("more than can be shown"), "{err}");
        assert_eq!(requests.load(Ordering::SeqCst), 1, "the directory was never asked for");

        let mut data = zip(&[(b"a", true, b"a"), (b"b", true, b"b")], b"", Some(1));
        let record = data.len() - END_LEN - ZIP64_LOCATOR_LEN - ZIP64_END_LEN as usize;
        data[record + 32..record + 40].copy_from_slice(&200_000u64.to_le_bytes());
        assert!(list_bytes(&data).unwrap_err().contains("more than can be shown"));
    }

    #[test]
    fn damaged_directories_fail_cleanly() {
        let good = zip(&[(b"one.txt", true, b"1"), (b"two.txt", true, b"22")], b"", None);
        let end = good.len() - END_LEN;
        let with = |at: usize, bytes: &[u8]| {
            let mut data = good.clone();
            data[at..at + bytes.len()].copy_from_slice(bytes);
            data
        };
        for (what, data) in [
            ("more entries than are there", with(end + 10, &9u16.to_le_bytes())),
            ("a directory past the end", with(end + 16, &u32::MAX.to_le_bytes()[..3])),
            ("a directory larger than the file", with(end + 12, &4096u32.to_le_bytes())),
            ("split archives", with(end + 4, &1u16.to_le_bytes())),
            // The second entry's name length: it is the directory's last 46 + 7 bytes.
            ("a name running out of the directory", with(end - ENTRY_LEN - 7 + 28, &500u16.to_le_bytes())),
        ] {
            assert!(list_bytes(&data).is_err(), "{what}");
        }
        assert_eq!(list_bytes(&good).unwrap().len(), 2);
    }

    #[test]
    fn mangled_zips_never_panic() {
        let names: Vec<String> = (0..40).map(|i| format!("dir/{i}.txt")).collect();
        let files: Vec<(&[u8], bool, &[u8])> = names.iter().map(|n| (n.as_bytes(), true, &b"data"[..])).collect();
        let samples = [zip(&files, b"comment", None), zip(&files[..3], b"", Some(7 << 30))];
        // A small linear congruential generator keeps the run the same every time.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |below: usize| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as usize % below.max(1)
        };
        for round in 0..4000 {
            let mut data = samples[round % 2].clone();
            for _ in 0..1 + next(8) {
                // Mostly the end and the directory, where the parser looks.
                let near = if next(4) == 0 { usize::MAX } else { 900 };
                let at = data.len() - 1 - next(data.len().min(near));
                data[at] = next(256) as u8;
            }
            if next(5) == 0 {
                data.truncate(next(data.len()));
            }
            if let Ok(listed) = list_bytes(&data) {
                assert!(listed.len() as u64 <= MAX_ENTRIES);
            }
        }
    }

    #[test]
    fn dos_times() {
        assert_eq!(dos_time(DATE, TIME).as_deref(), Some("2024-05-17 13:45:30"));
        assert_eq!(dos_time(0, 0), None);
        assert_eq!(dos_time((44 << 9) | (13 << 5) | 1, 0), None, "month 13");
    }

    /// A ZIP of `files` (name, contents, method, flags) with their true CRC-32s, deflated where the
    /// method is 8, each local header with an extra field the directory lacks; with `zip64`, every
    /// entry's sizes and offset are in its ZIP64 extra field.
    fn packed(files: &[(&str, &[u8], u16, u16)], zip64: bool) -> Vec<u8> {
        use std::io::Write;
        let (mut out, mut directory) = (Vec::new(), Vec::new());
        let le16 = |values: &[u16]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let le32 = |values: &[u32]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        for &(name, data, method, flags) in files {
            let offset = out.len() as u64;
            let body = match method {
                DEFLATED => {
                    let mut packer = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                    packer.write_all(data).unwrap();
                    packer.finish().unwrap()
                }
                _ => data.to_vec(),
            };
            let mut crc = flate2::Crc::new();
            crc.update(data);
            out.extend(LOCAL_SIG.to_le_bytes());
            out.extend(le16(&[20, flags | 0x800, method, TIME, DATE]));
            out.extend(le32(&[crc.sum(), body.len() as u32, data.len() as u32]));
            out.extend(le16(&[name.len() as u16, 4]));
            out.extend(name.as_bytes());
            out.extend(le16(&[0xcafe, 0]));
            out.extend(&body);
            let small = |v: u64| if zip64 { u32::MAX } else { v as u32 };
            let extra: Vec<u8> = match zip64 {
                true => le16(&[1, 24]).into_iter().chain([data.len() as u64, body.len() as u64, offset].iter().flat_map(|v| v.to_le_bytes())).collect(),
                false => Vec::new(),
            };
            directory.extend(ENTRY_SIG.to_le_bytes());
            directory.extend(le16(&[45, 20, flags | 0x800, method, TIME, DATE]));
            directory.extend(le32(&[crc.sum(), small(body.len() as u64), small(data.len() as u64)]));
            directory.extend(le16(&[name.len() as u16, extra.len() as u16, 0, 0, 0]));
            directory.extend(le32(&[0, small(offset)]));
            directory.extend(name.as_bytes());
            directory.extend(extra);
        }
        let (at, size, count) = (out.len() as u32, directory.len() as u32, files.len() as u16);
        out.extend(directory);
        out.extend(END_SIG.to_le_bytes());
        out.extend(le16(&[0, 0, count, count]));
        out.extend(le32(&[size, at]));
        out.extend(le16(&[0]));
        out
    }

    /// What `extract` gives for `entry`, or why it failed.
    async fn extracted(url: &Url, entry: &ZipEntry) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let mut shown = Vec::new();
        extract(&Client::new(), None, url, entry, &mut out, None, &mut |n| shown.push(n)).await?;
        assert!(shown.windows(2).all(|w| w[0] < w[1]) && shown.last().is_none_or(|&n| n == out.len() as u64), "{shown:?}");
        Ok(out)
    }

    /// Text that deflates well, so one packed block unpacks to more than the buffer holds.
    fn text(len: usize) -> Vec<u8> {
        (0..len).map(|i| b"the quick brown fox jumps over the lazy dog "[i % 44]).collect()
    }

    /// Stored and deflated files come out whole, each in two range requests (its local header,
    /// then its data); in a ZIP64 archive too, whose offsets are in the extra field.
    #[tokio::test]
    async fn extracts_stored_and_deflated_files_by_range() {
        let (big, small) = (text(900_000), b"hello, zip".to_vec());
        let noise: Vec<u8> = (0..70_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for zip64 in [false, true] {
            let files: [(&str, &[u8], u16, u16); 5] =
                [("docs/", b"", 0, 0), ("docs/a.txt", &small, 0, 0), ("big.txt", &big, 8, 0), ("noise.bin", &noise, 8, 0), ("empty", b"", 0, 0)];
            let (url, requests) = serve(packed(&files, zip64), true).await;
            let listed = list_zip(&Client::new(), &url).await.unwrap();
            assert_eq!(listed.iter().map(|e| (e.method, e.size)).collect::<Vec<_>>(), [(0, 0), (0, 10), (8, 900_000), (8, 70_000), (0, 0)]);
            for (entry, (_, data, _, _)) in listed.iter().zip(&files).skip(1) {
                let before = requests.load(Ordering::SeqCst);
                assert_eq!(extracted(&url, entry).await.as_deref(), Ok(*data), "{} in ZIP64 {zip64}", entry.name);
                let asked = if entry.compressed == 0 { 1 } else { 2 };
                assert_eq!(requests.load(Ordering::SeqCst) - before, asked, "{}", entry.name);
            }
            assert!(listed[2].compressed < 10_000, "the text deflated");
        }
    }

    /// Data that fails its CRC-32, unpacks past or short of its size, or is cut short fails;
    /// encrypted files and other methods say why before any request.
    #[tokio::test]
    async fn bad_data_and_unsupported_files_fail() {
        let data = text(50_000);
        let (url, requests) = serve(packed(&[("a.txt", &data, 8, 0), ("s.txt", &data[..100], 0, 0)], false), true).await;
        let listed = list_zip(&Client::new(), &url).await.unwrap();
        let with = |change: &dyn Fn(&mut ZipEntry)| {
            let mut entry = listed[0].clone();
            change(&mut entry);
            entry
        };
        for (entry, says) in [
            (with(&|e| e.crc32 ^= 1), "fails its CRC-32 check"),
            (with(&|e| e.size -= 1), "unpacks to more than"),
            (with(&|e| e.size += 1), "unpacks to 50000 bytes, not the 50001"),
            (with(&|e| e.compressed -= 5), "ended after"),
            (ZipEntry { size: 99, ..listed[1].clone() }, "unpacks to more than"),
            (with(&|e| e.offset += 1), "has no file where"),
        ] {
            let err = extracted(&url, &entry).await.unwrap_err();
            assert!(err.contains(says), "{says}: {err}");
        }
        let before = requests.load(Ordering::SeqCst);
        for (entry, says) in [(with(&|e| e.encrypted = true), "is encrypted"), (with(&|e| e.method = 12), "packed with bzip2 (method 12)")] {
            assert!(entry.unsupported().is_some_and(|why| why.contains(says)), "{says}");
            assert!(extracted(&url, &entry).await.unwrap_err().contains(says));
        }
        assert_eq!(requests.load(Ordering::SeqCst), before, "nothing was asked for them");
        let encrypted = list_bytes(&packed(&[("x", b"x", 0, 1)], false)).unwrap();
        assert!(encrypted[0].encrypted && encrypted[0].unsupported().is_some());
    }

    /// An entry's name becomes a path below the folder, never above it.
    #[test]
    fn entry_paths_stay_in_the_folder() {
        let path = |name: &str| ZipEntry { name: name.into(), ..Default::default() }.local_path();
        let parts = |parts: &[&str]| Some(parts.iter().collect::<PathBuf>());
        assert_eq!(path("docs/a.txt"), parts(&["docs", "a.txt"]));
        assert_eq!(path("docs/"), parts(&["docs"]));
        assert_eq!(path("/etc/./passwd"), parts(&["etc", "passwd"]));
        assert_eq!(path("C:\\Windows\\x.dll"), parts(&["C_", "Windows", "x.dll"]));
        assert_eq!(path("a/b?.txt"), parts(&["a", "b_.txt"]));
        for escape in ["../x", "a/../../x", "a\\..\\x", "", "/", "./"] {
            assert_eq!(path(escape), None, "{escape}");
        }
    }

    /// A download of one entry (`DownloadOptions::zip_entry`) puts it at its path under the
    /// folder, numbered beside a file of that name; a folder entry is made.
    #[tokio::test]
    async fn a_download_of_an_entry_keeps_its_folders() {
        let data = text(300_000);
        let (url, _) = serve(packed(&[("sub/", b"", 0, 0), ("sub/deep/t.txt", &data, 8, 0)], false), true).await;
        let listed = list_zip(&Client::new(), &url).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let download = |entry: &ZipEntry| {
            let options = crate::engine::DownloadOptions {
                output_path: Some(dir.path().join("")),
                zip_entry: Some(entry.clone()),
                ..Default::default()
            };
            crate::engine::DownloadEngine::new(vec![url.clone()], options)
        };
        assert_eq!(download(&listed[0]).run(None).await.unwrap(), dir.path().join("sub"));
        assert!(dir.path().join("sub").is_dir());
        let (tx, mut rx) = tokio::sync::broadcast::channel(1024);
        let first = download(&listed[1]).run(Some(tx)).await.unwrap();
        assert_eq!(first, dir.path().join("sub").join("deep").join("t.txt"));
        assert_eq!(std::fs::read(&first).unwrap(), data);
        let mut last = None;
        while let Ok(snapshot) = rx.try_recv() {
            last = Some(snapshot);
        }
        assert_eq!(last.map(|s| (s.downloaded_bytes, s.total_bytes)), Some((300_000, 300_000)));
        let second = download(&listed[1]).run(None).await.unwrap();
        assert_eq!(second, dir.path().join("sub").join("deep").join("t (1).txt"));
        assert_eq!(std::fs::read(&second).unwrap(), data);
        let names: Vec<_> = std::fs::read_dir(dir.path().join("sub").join("deep")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "no part or lock files are left: {names:?}");
    }
}
