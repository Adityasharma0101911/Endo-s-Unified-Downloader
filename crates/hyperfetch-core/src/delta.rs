//! Delta downloads: when an older version of a file is on disk, the blocks of the new version it
//! still holds are copied from it, and only the rest is downloaded. Which blocks those are is told
//! by checksums published for the new version, never guessed: a metalink's piece hashes (each
//! piece at the same place in both versions) or a zsync control file (a block anywhere in the old
//! version, found with its rolling checksum and checked with its MD4).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::metalink::Pieces;
use crate::range::{merge_ranges, ByteRange};

/// Bytes read from the old version at a time.
const READ: usize = 4 * 1024 * 1024;
/// Largest zsync block, so a window fits in memory many times over.
const MAX_BLOCK: usize = 1 << 20;

/// A zsync control file (see `zsyncmake`): the new version's length, its SHA-1, and for each of its
/// blocks a rolling checksum and the first bytes of its MD4, the last block padded with zeros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zsync {
    pub block: usize,
    pub length: u64,
    /// How many blocks in a row must match before one is taken (1 or 2).
    seq: usize,
    rsum_bytes: usize,
    strong_bytes: usize,
    /// The new version's SHA-1, as `--checksum` takes it.
    pub sha1: Option<String>,
    /// Each block's rolling checksum, its low `rsum_bytes`, and the start of its MD4.
    blocks: Vec<(u32, Vec<u8>)>,
}

/// Reads a zsync control file: its `Key: value` lines, an empty line, then each block's checksums.
pub fn parse_zsync(data: &[u8]) -> Result<Zsync, String> {
    let bad = |what: &str| format!("Not a zsync file: {what}");
    let end = data.windows(2).position(|w| w == b"\n\n").ok_or_else(|| bad("no end to its header"))?;
    let head = std::str::from_utf8(&data[..end]).map_err(|_| bad("its header is not text"))?;
    let field = |key: &str| head.lines().find_map(|line| line.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case(key)).map(|(_, v)| v.trim()));
    let number = |key: &str| field(key).and_then(|v| v.parse::<u64>().ok()).ok_or_else(|| bad(&format!("no {key}")));
    let (block, length) = (number("Blocksize")? as usize, number("Length")?);
    if !block.is_power_of_two() || !(16..=MAX_BLOCK).contains(&block) {
        return Err(bad("its block size is not a power of two from 16 bytes to 1 MiB"));
    }
    let lengths: Vec<usize> = field("Hash-Lengths").unwrap_or("1,4,16").split(',').filter_map(|v| v.trim().parse().ok()).collect();
    let [seq, rsum_bytes, strong_bytes] = lengths[..] else { return Err(bad("its Hash-Lengths")) };
    if !(1..=2).contains(&seq) || !(1..=4).contains(&rsum_bytes) || !(3..=16).contains(&strong_bytes) {
        return Err(bad("its Hash-Lengths"));
    }
    let count = usize::try_from(length.div_ceil(block as u64)).map_err(|_| bad("its Length"))?;
    let body = &data[end + 2..];
    let each = rsum_bytes + strong_bytes;
    if count.checked_mul(each) != Some(body.len()) {
        return Err(bad("its checksums do not cover its Length"));
    }
    let blocks = body
        .chunks_exact(each)
        .map(|sums| (sums[..rsum_bytes].iter().fold(0u32, |v, &b| (v << 8) | u32::from(b)), sums[rsum_bytes..].to_vec()))
        .collect();
    let sha1 = field("SHA-1").filter(|v| v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit())).map(|v| format!("sha1:{}", v.to_ascii_lowercase()));
    Ok(Zsync { block, length, seq, rsum_bytes, strong_bytes, sha1, blocks })
}

/// Where the blocks of the new version that `old` still holds are: (offset in the new version,
/// offset in `old`, length), as `pieces` (the metalink's, checked at the same place) or `zsync` (anywhere) tell.
pub fn plan(old: &Path, size: u64, pieces: Option<&Pieces>, zsync: Option<&Zsync>) -> io::Result<Vec<(u64, u64, u64)>> {
    match (zsync.filter(|z| z.length == size), pieces.filter(|p| p.fit(size))) {
        (Some(zsync), _) => zsync_plan(old, zsync),
        (None, Some(pieces)) => pieces_plan(old, size, pieces),
        (None, None) => Ok(Vec::new()),
    }
}

/// Copies what `plan` found from `old` into the `size`-byte `part` (created, sparse elsewhere) and
/// returns the ranges of `part` it filled.
pub fn seed(old: &Path, part: &Path, size: u64, found: &[(u64, u64, u64)]) -> io::Result<Vec<ByteRange>> {
    let (mut from, to) = (File::open(old)?, std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(part)?);
    to.set_len(size)?;
    let mut buf = Vec::new();
    for &(at, source, len) in found {
        buf.resize(len as usize, 0);
        read_at(&mut from, source, &mut buf)?;
        crate::storage::write_all_at(&to, &buf, at)?;
    }
    to.sync_data()?;
    Ok(merge_ranges(found.iter().filter_map(|&(at, _, len)| ByteRange::from_len(at, len).ok()).collect()))
}

fn read_at(file: &mut File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)
}

/// The metalink's pieces whose hash the old version's bytes at the same place have.
fn pieces_plan(old: &Path, size: u64, pieces: &Pieces) -> io::Result<Vec<(u64, u64, u64)>> {
    let mut file = File::open(old)?;
    let have = file.metadata()?.len();
    let mut found = Vec::new();
    let mut buf = Vec::new();
    for (n, want) in pieces.hashes.iter().enumerate() {
        let at = n as u64 * pieces.length;
        let len = pieces.length.min(size - at);
        if at + len > have {
            break;
        }
        buf.resize(len as usize, 0);
        read_at(&mut file, at, &mut buf)?;
        let got: String = match pieces.algo.as_str() {
            "sha256" => Sha256::digest(&buf).iter().map(|b| format!("{b:02x}")).collect(),
            _ => Sha1::digest(&buf).iter().map(|b| format!("{b:02x}")).collect(),
        };
        if got.eq_ignore_ascii_case(want) {
            found.push((at, at, len));
        }
    }
    Ok(found)
}

/// zsync's rolling checksum of `data`: the sum of its bytes and the sum of each byte times its
/// distance from the end, each 16 bits.
fn rsum(data: &[u8]) -> (u16, u16) {
    data.iter().enumerate().fold((0u16, 0u16), |(a, b), (i, &c)| {
        (a.wrapping_add(c.into()), b.wrapping_add(((data.len() - i) as u16).wrapping_mul(c.into())))
    })
}

/// The blocks of `zsync` found anywhere in `old`, read once with the rolling checksum: a block
/// whose checksum matches is taken when the start of its MD4 does too, and with `seq` 2 the block
/// after it (if any) as well, as zsync takes it.
fn zsync_plan(old: &Path, zsync: &Zsync) -> io::Result<Vec<(u64, u64, u64)>> {
    let (block, count) = (zsync.block, zsync.blocks.len());
    let mask = if zsync.rsum_bytes == 4 { u32::MAX } else { (1u32 << (8 * zsync.rsum_bytes)) - 1 };
    let mut by_rsum: HashMap<u32, Vec<usize>> = HashMap::new();
    for (n, (sum, _)) in zsync.blocks.iter().enumerate() {
        by_rsum.entry(*sum).or_default().push(n);
    }
    let strong = |data: &[u8], n: usize| md4(data)[..zsync.strong_bytes] == zsync.blocks[n].1[..];
    let mut found: Vec<Option<u64>> = vec![None; count];
    let (mut left, mut file) = (count, File::open(old)?);
    let old_len = file.metadata()?.len();
    let len_of = |n: usize| (block as u64).min(zsync.length - n as u64 * block as u64);
    // The old version's bytes from `base`, then zeros: a last block is padded with them.
    let (mut buf, mut base, mut eof) = (Vec::new(), 0u64, false);
    let mut at = 0u64;
    let mut sums: Option<(u16, u16)> = None;
    loop {
        // Two blocks from `at` in the buffer, for `seq` 2.
        while !eof && (at - base) as usize + 2 * block > buf.len() {
            buf.drain(..(at - base) as usize);
            base = at;
            let had = buf.len();
            buf.resize(had + READ, 0);
            let read = read_full(&mut file, &mut buf[had..])?;
            buf.truncate(had + read);
            if read == 0 {
                eof = true;
                buf.resize(buf.len() + 2 * block, 0);
            }
        }
        let from = (at - base) as usize;
        // Past the old version's last byte nothing more starts.
        if left == 0 || from >= buf.len() - if eof { 2 * block } else { 0 } {
            break;
        }
        let window = &buf[from..from + block];
        let (a, b) = *sums.get_or_insert_with(|| rsum(window));
        let mut taken = false;
        if let Some(candidates) = by_rsum.get(&(((u32::from(a) << 16) | u32::from(b)) & mask)) {
            for &n in candidates {
                // Its bytes must all be in the old version, past which only padding is.
                if found[n].is_some() || at + len_of(n) > old_len {
                    continue;
                }
                let next = n + 1;
                let follows = zsync.seq == 1 || next >= count || {
                    let after = &buf[from + block..from + 2 * block];
                    (((u32::from(rsum(after).0) << 16) | u32::from(rsum(after).1)) & mask) == zsync.blocks[next].0 && strong(after, next)
                };
                if follows && strong(window, n) {
                    found[n] = Some(at);
                    left -= 1;
                    taken = true;
                }
            }
        }
        if taken {
            // zsync goes on after the block it took.
            at += block as u64;
            sums = None;
            continue;
        }
        // One byte on: the byte leaving and the one coming in change the sums.
        let (out, inn) = (buf[from], buf[from + block]);
        let a2 = a.wrapping_sub(out.into()).wrapping_add(inn.into());
        let b2 = b.wrapping_sub((block as u16).wrapping_mul(out.into())).wrapping_add(a2);
        sums = Some((a2, b2));
        at += 1;
    }
    Ok(found.iter().enumerate().filter_map(|(n, source)| source.map(|source| (n as u64 * block as u64, source, len_of(n)))).collect())
}

/// Reads until `buf` is full or the file ends; how many bytes came.
fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match file.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Ok(got)
}

/// MD4 (RFC 1320), which zsync's block checksums use.
fn md4(data: &[u8]) -> [u8; 16] {
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend(((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in message.chunks_exact(64) {
        let x: Vec<u32> = chunk.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let [mut a, mut b, mut c, mut d] = state;
        let f = |x: u32, y: u32, z: u32| (x & y) | (!x & z);
        let g = |x: u32, y: u32, z: u32| (x & y) | (x & z) | (y & z);
        let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
        for &i in &[0, 4, 8, 12] {
            a = a.wrapping_add(f(b, c, d)).wrapping_add(x[i]).rotate_left(3);
            d = d.wrapping_add(f(a, b, c)).wrapping_add(x[i + 1]).rotate_left(7);
            c = c.wrapping_add(f(d, a, b)).wrapping_add(x[i + 2]).rotate_left(11);
            b = b.wrapping_add(f(c, d, a)).wrapping_add(x[i + 3]).rotate_left(19);
        }
        for &i in &[0, 1, 2, 3] {
            a = a.wrapping_add(g(b, c, d)).wrapping_add(x[i]).wrapping_add(0x5a82_7999).rotate_left(3);
            d = d.wrapping_add(g(a, b, c)).wrapping_add(x[i + 4]).wrapping_add(0x5a82_7999).rotate_left(5);
            c = c.wrapping_add(g(d, a, b)).wrapping_add(x[i + 8]).wrapping_add(0x5a82_7999).rotate_left(9);
            b = b.wrapping_add(g(c, d, a)).wrapping_add(x[i + 12]).wrapping_add(0x5a82_7999).rotate_left(13);
        }
        for &i in &[0, 2, 1, 3] {
            a = a.wrapping_add(h(b, c, d)).wrapping_add(x[i]).wrapping_add(0x6ed9_eba1).rotate_left(3);
            d = d.wrapping_add(h(a, b, c)).wrapping_add(x[i + 8]).wrapping_add(0x6ed9_eba1).rotate_left(9);
            c = c.wrapping_add(h(d, a, b)).wrapping_add(x[i + 4]).wrapping_add(0x6ed9_eba1).rotate_left(11);
            b = b.wrapping_add(h(c, d, a)).wrapping_add(x[i + 12]).wrapping_add(0x6ed9_eba1).rotate_left(15);
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d]) {
            *s = s.wrapping_add(v);
        }
    }
    let mut out = [0u8; 16];
    for (i, s) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A zsync control file for `data` with blocks of `block` bytes, as zsyncmake writes one.
    pub(crate) fn zsync_of(data: &[u8], block: usize, seq: usize, rsum_bytes: usize, strong_bytes: usize) -> Vec<u8> {
        let sha1: String = Sha1::digest(data).iter().map(|b| format!("{b:02x}")).collect();
        let mut out = format!(
            "zsync: 0.6.2\nFilename: f.bin\nBlocksize: {block}\nLength: {}\nHash-Lengths: {seq},{rsum_bytes},{strong_bytes}\nURL: f.bin\nSHA-1: {sha1}\n\n",
            data.len()
        )
        .into_bytes();
        for chunk in data.chunks(block) {
            let mut padded = chunk.to_vec();
            padded.resize(block, 0);
            let (a, b) = rsum(&padded);
            out.extend(&((u32::from(a) << 16) | u32::from(b)).to_be_bytes()[4 - rsum_bytes..]);
            out.extend(&md4(&padded)[..strong_bytes]);
        }
        out
    }

    #[test]
    fn md4_matches_rfc_1320() {
        let hex = |data: &[u8]| md4(data).iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(b""), "31d6cfe0d16ae931b73c59d7e0c089c0");
        assert_eq!(hex(b"abc"), "a448017aaf21d8525fc10ae87aa6729d");
        assert_eq!(hex(b"message digest"), "d9130a8164549fe818874806e1c7014b");
        let long = b"12345678901234567890123456789012345678901234567890123456789012345678901234567890";
        assert_eq!(hex(long), "e33b4ddc9c38f2199c3e7b164fcc0536");
    }

    /// The new version: 200 KB of text-like bytes that never repeat a block.
    fn version(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    }

    /// The old version has bytes inserted near its start and a stretch changed further on: zsync
    /// finds the blocks before and after both, shifted, and every block it finds is the new one's.
    #[test]
    fn zsync_finds_shifted_blocks() {
        let new = version(200_003, 7);
        let mut old = new.clone();
        old.splice(10_000..10_000, version(777, 9));
        old[120_000..130_000].fill(0xee);
        let dir = tempfile::tempdir().unwrap();
        let (old_path, part) = (dir.path().join("old"), dir.path().join("new.part"));
        std::fs::write(&old_path, &old).unwrap();
        for (seq, rsum_bytes, strong_bytes) in [(1, 4, 16), (2, 2, 5), (1, 3, 8)] {
            let zsync = parse_zsync(&zsync_of(&new, 2048, seq, rsum_bytes, strong_bytes)).unwrap();
            assert_eq!((zsync.length, zsync.blocks.len()), (200_003, 98));
            let found = plan(&old_path, new.len() as u64, None, Some(&zsync)).unwrap();
            let copied: u64 = found.iter().map(|&(_, _, len)| len).sum();
            // All but the blocks the insertion and the change touch: two and six of them.
            assert!(copied >= 200_003 - 10 * 2048 && copied < 200_003 - 6 * 2048, "{seq}: {copied}");
            let ranges = seed(&old_path, &part, new.len() as u64, &found).unwrap();
            let got = std::fs::read(&part).unwrap();
            for range in &ranges {
                assert!(got[range.start as usize..=range.end as usize] == new[range.start as usize..=range.end as usize]);
            }
            assert_eq!(zsync.sha1.as_deref().map(str::len), Some(45));
        }
        // The last block, shorter than the others, is found padded with zeros as zsync pads it.
        let found = plan(&old_path, new.len() as u64, None, Some(&parse_zsync(&zsync_of(&new, 2048, 1, 4, 16)).unwrap())).unwrap();
        assert!(found.iter().any(|&(at, _, len)| at == 97 * 2048 && len == 200_003 - 97 * 2048));
    }

    /// Metalink pieces are compared at the same place: a change spoils its pieces alone, and an
    /// insertion every piece after it.
    #[test]
    fn metalink_pieces_are_checked_in_place() {
        let new = version(100_000, 3);
        let hashes = new.chunks(16_384).map(|piece| Sha256::digest(piece).iter().map(|b| format!("{b:02x}")).collect()).collect();
        let pieces = Pieces { length: 16_384, algo: "sha256".into(), hashes };
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("old");
        let mut old = new.clone();
        old[40_000] ^= 1;
        std::fs::write(&old_path, &old).unwrap();
        let found = plan(&old_path, 100_000, Some(&pieces), None).unwrap();
        assert_eq!(found.iter().map(|&(at, _, _)| at / 16_384).collect::<Vec<_>>(), [0, 1, 3, 4, 5, 6]);
        assert_eq!(found.last(), Some(&(6 * 16_384, 6 * 16_384, 100_000 - 6 * 16_384)));
        old.insert(50_000, 0);
        std::fs::write(&old_path, &old).unwrap();
        assert_eq!(plan(&old_path, 100_000, Some(&pieces), None).unwrap().len(), 2);
        // Pieces of another size, or none at all, find nothing: nothing is guessed.
        assert!(plan(&old_path, 120_000, Some(&pieces), None).unwrap().is_empty());
        assert!(plan(&old_path, 100_000, None, None).unwrap().is_empty());
    }

    #[test]
    fn bad_zsync_files_are_refused() {
        let good = zsync_of(&version(5000, 1), 1024, 1, 4, 16);
        assert!(parse_zsync(&good).is_ok());
        let text = String::from_utf8_lossy(&good[..good.windows(2).position(|w| w == b"\n\n").unwrap()]).into_owned();
        for (from, to) in [("Blocksize: 1024", "Blocksize: 1000"), ("Length: 5000", "Length: 9000"), ("Hash-Lengths: 1,4,16", "Hash-Lengths: 3,4,16")] {
            let mut bad = text.replace(from, to).into_bytes();
            bad.extend(&good[text.len()..]);
            assert!(parse_zsync(&bad).is_err(), "{to}");
        }
        assert!(parse_zsync(b"just text").is_err());
    }
}
