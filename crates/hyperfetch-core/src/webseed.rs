//! A torrent's web seeds (BEP 19) as BitTorrent peers on loopback, so that librqbit, which has no
//! web seeds of its own, takes pieces from them and from the swarm at once. Each of [`PEERS`]
//! listeners is one peer to librqbit: it says it has every piece the seeds serve and answers a
//! request for a block with that block of the whole piece, fetched over HTTP (in parts of up to
//! `PART`, at once) and checked against the torrent's SHA-1 before any of it is sent. librqbit
//! writes every piece itself and checks it again, so no piece is written by two writers and none
//! counts unchecked.
//!
//! A piece the seeds serve wrong (the file changed since the torrent was made) or not at all is
//! offered no more: the connection that asked for it is closed, librqbit asks the swarm for it,
//! and when it connects again (after its back-off, 10 s or more) the piece is not among those
//! this peer has. Requests go through `crate::hosts`, so a host's rest after a 429 holds them too.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OnceCell, Semaphore};
use tokio_util::sync::{CancellationToken, DropGuard};
use url::Url;

/// Peers the seeds are offered to librqbit as: it asks each for 128 blocks (2 MiB) at most at once.
pub(crate) const PEERS: usize = 4;
/// Most bytes one HTTP request of a piece fetches; a larger piece comes in parts at once.
const PART: u64 = 1 << 20;
/// Longest one part may take, every try included.
const PART_LIMIT: Duration = Duration::from_secs(120);
/// Tries of a part over each of its file's links.
const TRIES: usize = 3;
/// Pause after a failed try, or a 429 or 503 without Retry-After.
const PAUSE: Duration = Duration::from_secs(2);
/// Largest block a request may ask for (BEP 3's is 16 KiB; some clients ask for more).
const MAX_BLOCK: u32 = 128 * 1024;
/// Largest message read from librqbit: the bitfield of a torrent of 8 million pieces.
const MAX_MESSAGE: u32 = 1 << 20;
/// How long a fetched piece is kept for the rest of its blocks to be asked for.
const KEEP: Duration = Duration::from_secs(30);
/// Bytes of fetched pieces kept at most (and two pieces, however large they are).
const KEPT_BYTES: u64 = 256 << 20;
const PROTOCOL: &[u8; 20] = b"\x13BitTorrent protocol";
/// The block librqbit asks for: the unit of the requests it keeps out to a peer.
const BLOCK: u64 = 16 * 1024;

/// Pieces the web seeds served other bytes of than the torrent's, for tests to count.
#[cfg(test)]
pub(crate) static WRONG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Why `crate::p2p` found a torrent's web seeds could not be peers, for tests to read.
#[cfg(test)]
pub(crate) static NOT_PEERS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The web seeds of one torrent as peers, until dropped.
pub(crate) struct WebSeeds {
    /// Where librqbit reaches them.
    pub addrs: Vec<SocketAddr>,
    seeds: Arc<Seeds>,
    _stop: DropGuard,
}

impl WebSeeds {
    /// Bytes of checked pieces fetched from the seeds so far.
    pub fn fetched(&self) -> u64 {
        self.seeds.fetched.load(Ordering::Relaxed)
    }
}

struct SeedFile {
    start: u64,
    length: u64,
    urls: Vec<Url>,
}

struct Seeds {
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    piece_length: u64,
    stream_length: u64,
    /// Requests librqbit may keep out to each of these peers (BEP 10's `reqq`; its own most is
    /// 128): together they reserve at most half the torrent, so in a small one the swarm's peers
    /// still find pieces to fetch from the start.
    reqq: u64,
    hashes: Vec<[u8; 20]>,
    files: Vec<SeedFile>,
    client: Client,
    /// HTTP requests open at once over all hosts, and to one host (see `hosts::acquire`).
    requests: Semaphore,
    per_host: usize,
    /// Pieces not offered: a file of them has no seed, or the seeds failed them.
    lost: Mutex<Vec<bool>>,
    kept: Mutex<HashMap<u32, Kept>>,
    fetched: AtomicU64,
}

/// A piece being fetched or fetched, while its blocks are asked for.
struct Kept {
    piece: Arc<OnceCell<Option<Bytes>>>,
    used: Instant,
    /// Bytes of its blocks sent.
    sent: u64,
}

/// Starts the web seeds of the torrent `torrent` (its .torrent bytes) as peers, fetching with
/// `client`, at most `connections` requests at once and `per_host` to one host; why not when it
/// has none, or no piece hashes to check what they serve against.
pub(crate) async fn start(torrent: &[u8], info_hash: [u8; 20], client: Client, connections: usize, per_host: usize) -> Result<WebSeeds, String> {
    let info = crate::torrent::parse_torrent_bytes(torrent)?;
    if info.piece_hashes.is_empty() || info.piece_length == 0 || info.piece_length > u64::from(u32::MAX) {
        return Err(format!("no piece hashes to check them by ({} pieces of {} bytes)", info.num_pieces, info.piece_length));
    }
    let mut lost = vec![false; info.piece_hashes.len()];
    for file in info.files.iter().filter(|f| f.urls.is_empty() && f.length > 0) {
        let (first, last) = (file.start / info.piece_length, (file.start + file.length - 1) / info.piece_length);
        lost[first as usize..=last as usize].fill(true);
    }
    if lost.iter().all(|&lost| lost) {
        return Err("no web seeds".into());
    }
    let mut peer_id = *b"-EU2000-000000000000";
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut peer_id[8..]).map_err(|_| "no random peer id")?;
    let seeds = Arc::new(Seeds {
        info_hash,
        peer_id,
        piece_length: info.piece_length,
        stream_length: info.stream_length,
        reqq: (info.stream_length / BLOCK / (2 * PEERS as u64)).clamp(16, 128),
        hashes: info.piece_hashes,
        files: info.files.into_iter().map(|f| SeedFile { start: f.start, length: f.length, urls: f.urls }).collect(),
        client,
        requests: Semaphore::new(connections.max(1)),
        per_host,
        lost: Mutex::new(lost),
        kept: Mutex::default(),
        fetched: AtomicU64::new(0),
    });
    let stop = CancellationToken::new();
    let mut addrs = Vec::new();
    for _ in 0..PEERS {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(|e| format!("no local port: {e}"))?;
        addrs.push(listener.local_addr().map_err(|e| e.to_string())?);
        tokio::spawn(listen(listener, seeds.clone(), stop.clone()));
    }
    Ok(WebSeeds { addrs, seeds, _stop: stop.drop_guard() })
}

async fn listen(listener: TcpListener, seeds: Arc<Seeds>, stop: CancellationToken) {
    loop {
        let socket = tokio::select! {
            _ = stop.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((socket, _)) => socket,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let (seeds, stop) = (seeds.clone(), stop.clone());
        tokio::spawn(async move {
            tokio::select! {
                _ = stop.cancelled() => {}
                _ = peer(socket, seeds) => {}
            }
        });
    }
}

fn invalid(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string())
}

/// One connection from librqbit: the handshake, what this peer has, then a block for each request
/// until the connection ends or a piece it asks for cannot be served.
async fn peer(socket: TcpStream, seeds: Arc<Seeds>) -> std::io::Result<()> {
    let _ = socket.set_nodelay(true);
    let (mut from, mut to) = socket.into_split();
    let mut hello = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(10), from.read_exact(&mut hello)).await.map_err(|_| invalid("no handshake"))??;
    if hello[..20] != PROTOCOL[..] || hello[28..48] != seeds.info_hash {
        return Ok(());
    }
    // Extensions (BEP 10) only to say how many requests to keep out to this peer (`Seeds::reqq`).
    let extended = hello[25] & 0x10 != 0;
    let bitfield = seeds.bitfield();
    let mut reply = Vec::with_capacity(68 + 5 + bitfield.len() + 32 + 5);
    reply.extend_from_slice(PROTOCOL);
    reply.extend_from_slice(&[0, 0, 0, 0, 0, if extended { 0x10 } else { 0 }, 0, 0]);
    reply.extend_from_slice(&seeds.info_hash);
    reply.extend_from_slice(&seeds.peer_id);
    // Before the bitfield, as librqbit sends its own: it reserves pieces as soon as it has that.
    if extended {
        let handshake = format!("d1:mde4:reqqi{}ee", seeds.reqq);
        reply.extend_from_slice(&(2 + handshake.len() as u32).to_be_bytes());
        reply.extend_from_slice(&[20, 0]);
        reply.extend_from_slice(handshake.as_bytes());
    }
    reply.extend_from_slice(&(1 + bitfield.len() as u32).to_be_bytes());
    reply.push(5);
    reply.extend_from_slice(&bitfield);
    // Unchoked from the start: this peer serves whoever asks.
    reply.extend_from_slice(&1u32.to_be_bytes());
    reply.push(1);
    to.write_all(&reply).await?;

    let (blocks, mut sending) = tokio::sync::mpsc::channel::<Bytes>(32);
    let failed = CancellationToken::new();
    let mut asked = tokio::task::JoinSet::new();
    let writer = async {
        while let Some(frame) = sending.recv().await {
            to.write_all(&frame).await?;
        }
        Ok::<(), std::io::Error>(())
    };
    let reader = async {
        loop {
            while asked.try_join_next().is_some() {}
            let len = from.read_u32().await?;
            if len > MAX_MESSAGE {
                return Err::<(), _>(invalid("message too long"));
            }
            let mut message = vec![0u8; len as usize];
            from.read_exact(&mut message).await?;
            // Only requests matter: choke, interest, have, cancel (a block sent anyway is taken as a
            // late one) and extensions are left alone.
            let [6, request @ ..] = &message[..] else { continue };
            let field = |at: usize| request.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
            let (Some(index), Some(begin), Some(length), 12) = (field(0), field(4), field(8), request.len()) else {
                return Err(invalid("bad request"));
            };
            if !seeds.holds(index, begin, length) {
                return Err(invalid("request outside the torrent"));
            }
            let (seeds, blocks, failed) = (seeds.clone(), blocks.clone(), failed.clone());
            asked.spawn(async move {
                let Some(piece) = seeds.piece(index).await else { return failed.cancel() };
                let block = &piece[begin as usize..(begin + length) as usize];
                let mut frame = BytesMut::with_capacity(13 + block.len());
                frame.put_u32(9 + length);
                frame.put_u8(7);
                frame.put_u32(index);
                frame.put_u32(begin);
                frame.put_slice(block);
                if blocks.send(frame.freeze()).await.is_ok() {
                    seeds.sent(index, u64::from(length));
                }
            });
        }
    };
    let ended = tokio::select! {
        ended = reader => ended,
        ended = writer => ended,
        _ = failed.cancelled() => Ok(()),
    };
    asked.abort_all();
    ended
}

impl Seeds {
    fn piece_len(&self, index: u32) -> u64 {
        self.piece_length.min(self.stream_length - u64::from(index) * self.piece_length)
    }

    /// Whether a block of `length` bytes at `begin` of piece `index` is in the torrent.
    fn holds(&self, index: u32, begin: u32, length: u32) -> bool {
        (index as usize) < self.hashes.len() && length > 0 && length <= MAX_BLOCK && u64::from(begin) + u64::from(length) <= self.piece_len(index)
    }

    /// The pieces offered, one bit each from the high bit of the first byte.
    fn bitfield(&self) -> Vec<u8> {
        let lost = self.lost.lock();
        let mut bits = vec![0u8; lost.len().div_ceil(8)];
        for index in (0..lost.len()).filter(|&i| !lost[i]) {
            bits[index / 8] |= 0x80 >> (index % 8);
        }
        bits
    }

    /// Piece `index`, fetched once for all its blocks; None when the seeds cannot serve it, which
    /// then is offered no more.
    async fn piece(&self, index: u32) -> Option<Bytes> {
        if self.lost.lock()[index as usize] {
            return None;
        }
        let cell = {
            let mut kept = self.kept.lock();
            let now = Instant::now();
            kept.retain(|_, k| now - k.used < KEEP);
            // The oldest go first once they hold more than `KEPT_BYTES`.
            let most = (KEPT_BYTES / self.piece_length).max(2) as usize;
            while kept.len() >= most && !kept.contains_key(&index) {
                let Some(oldest) = kept.iter().min_by_key(|(_, k)| k.used).map(|(&i, _)| i) else { break };
                kept.remove(&oldest);
            }
            let k = kept.entry(index).or_insert_with(|| Kept { piece: Arc::default(), used: now, sent: 0 });
            k.used = now;
            k.piece.clone()
        };
        let piece = cell.get_or_init(|| self.fetch(index)).await.clone();
        if piece.is_none() {
            self.lost.lock()[index as usize] = true;
            self.kept.lock().remove(&index);
        }
        piece
    }

    /// `length` bytes of piece `index` went out: once all of it has, it is not kept.
    fn sent(&self, index: u32, length: u64) {
        let mut kept = self.kept.lock();
        if let Some(k) = kept.get_mut(&index) {
            k.sent += length;
            if k.sent >= self.piece_len(index) {
                kept.remove(&index);
            }
        }
    }

    /// Fetches piece `index` from its files' seeds and checks it: None when a part cannot be had
    /// or the piece is not the torrent's. Padding (bytes in no file) is zeros.
    async fn fetch(&self, index: u32) -> Option<Bytes> {
        let start = u64::from(index) * self.piece_length;
        let len = self.piece_len(index);
        let mut parts = Vec::new();
        for file in self.files.iter().filter(|f| f.length > 0 && f.start < start + len && f.start + f.length > start) {
            let (mut at, to) = (start.max(file.start), (start + len).min(file.start + file.length));
            while at < to {
                let n = PART.min(to - at);
                parts.push((file, at - file.start, (at - start) as usize, n));
                at += n;
            }
        }
        let fetched = futures_util::future::join_all(parts.iter().map(|&(file, offset, _, n)| self.get(file, offset, n))).await;
        let mut piece = vec![0u8; len as usize];
        for (&(_, _, at, n), bytes) in parts.iter().zip(fetched) {
            piece[at..at + n as usize].copy_from_slice(&bytes?);
        }
        if Sha1::digest(&piece)[..] != self.hashes[index as usize] {
            tracing::warn!("a web seed serves other bytes than the torrent's in piece {index}: the swarm brings it");
            #[cfg(test)]
            WRONG.fetch_add(1, Ordering::SeqCst);
            return None;
        }
        self.fetched.fetch_add(len, Ordering::Relaxed);
        Some(piece.into())
    }

    /// `n` bytes at `offset` of `file`, from the first of its seeds that serves them.
    async fn get(&self, file: &SeedFile, offset: u64, n: u64) -> Option<Bytes> {
        let tries = async {
            for url in &file.urls {
                for _ in 0..TRIES {
                    match self.try_get(url, offset, n).await {
                        Ok(bytes) => return Some(bytes),
                        // This seed does not serve the part (gone, or no ranges): the next one.
                        Err(None) => break,
                        Err(Some(pause)) => tokio::time::sleep(pause).await,
                    }
                }
            }
            None
        };
        tokio::time::timeout(PART_LIMIT, tries).await.ok().flatten()
    }

    /// One request: the bytes, or the pause before trying again (None: not again from `url`).
    async fn try_get(&self, url: &Url, offset: u64, n: u64) -> Result<Bytes, Option<Duration>> {
        let _open = self.requests.acquire().await.map_err(|_| None::<Duration>)?;
        let slot = crate::hosts::acquire(url, self.per_host).await;
        let range = format!("bytes={}-{}", offset, offset + n - 1);
        let response = match self.client.get(url.clone()).header(RANGE, range).send().await {
            Ok(response) => response,
            Err(_) => return Err(Some(slot.unanswered().unwrap_or(PAUSE))),
        };
        let status = response.status();
        if matches!(status, StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE) {
            let pause = crate::worker::retry_after(response.headers()).unwrap_or(PAUSE);
            slot.throttled(pause);
            return Err(Some(pause));
        }
        slot.accepted();
        let from = response.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(range_start);
        if !(status == StatusCode::PARTIAL_CONTENT && from == Some(offset) || status == StatusCode::OK && offset == 0) {
            return Err(if status.is_server_error() { Some(PAUSE) } else { None });
        }
        let (mut body, mut stream) = (BytesMut::with_capacity(n as usize), response.bytes_stream());
        while (body.len() as u64) < n {
            match stream.next().await {
                Some(Ok(chunk)) => body.extend_from_slice(&chunk[..chunk.len().min(n as usize - body.len())]),
                _ => return Err(Some(PAUSE)),
            }
        }
        Ok(body.freeze())
    }
}

/// The first byte of a `Content-Range: bytes a-b/c`.
fn range_start(value: &str) -> Option<u64> {
    value.trim().strip_prefix("bytes ")?.split('-').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_content_range_gives_its_first_byte() {
        assert_eq!(range_start("bytes 100-199/1000"), Some(100));
        assert_eq!(range_start("bytes */1000"), None);
        assert_eq!(range_start("items 1-2/3"), None);
    }

    /// A peer that speaks to librqbit as these do: handshake, bitfield, unchoke, then blocks of
    /// checked pieces; a piece the seed serves wrong closes the connection and is offered no more.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_blocks_come_from_checked_pieces() {
        let size = 3 * 64 * 1024 + 1000;
        let content = crate::netsim::file(size);
        let (web, _) = crate::netsim::serve(crate::netsim::Server { size, ..Default::default() }).await;
        let mut wrong = content.clone();
        wrong[70_000] ^= 1;
        let pieces: Vec<u8> = wrong.chunks(64 * 1024).flat_map(|p| Sha1::digest(p).to_vec()).collect();
        let torrent = [
            format!("d8:url-list{}:{web}4:infod6:lengthi{size}e4:name5:f.bin12:piece lengthi65536e6:pieces{}:", web.as_str().len(), pieces.len()).as_bytes(),
            &pieces,
            b"ee",
        ]
        .concat();
        let hash = [7u8; 20];
        let client = Client::builder().http1_only().build().unwrap();
        let seeds = start(&torrent, hash, client, 4, 0).await.unwrap();
        let mut socket = TcpStream::connect(seeds.addrs[0]).await.unwrap();
        socket.write_all(&[&PROTOCOL[..], &[0; 8], &hash, &[1; 20]].concat()).await.unwrap();
        let mut hello = [0u8; 68 + 5 + 1 + 5];
        socket.read_exact(&mut hello).await.unwrap();
        assert_eq!(hello[28..48], hash);
        assert_eq!(hello[68..], [0, 0, 0, 2, 5, 0b1111_0000, 0, 0, 0, 1, 1], "every piece, unchoked");
        // A block of the last piece, which the seed serves as the torrent says.
        let request = |index: u32, begin: u32, length: u32| [&13u32.to_be_bytes()[..], &[6], &index.to_be_bytes(), &begin.to_be_bytes(), &length.to_be_bytes()].concat();
        socket.write_all(&request(3, 500, 500)).await.unwrap();
        let mut block = [0u8; 13 + 500];
        tokio::time::timeout(Duration::from_secs(10), socket.read_exact(&mut block)).await.unwrap().unwrap();
        assert_eq!(block[..13], [&509u32.to_be_bytes()[..], &[7], &3u32.to_be_bytes(), &500u32.to_be_bytes()].concat()[..]);
        assert_eq!(block[13..], content[3 * 65536 + 500..3 * 65536 + 1000]);
        // Piece 1 is not what the torrent says: the connection ends, and it is offered no more.
        socket.write_all(&request(1, 0, 16384)).await.unwrap();
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut rest)).await.unwrap().unwrap();
        assert!(rest.is_empty());
        assert_eq!(seeds.seeds.bitfield(), [0b1011_0000]);
        assert_eq!(seeds.fetched(), 1000);
        // A peer that speaks extensions (BEP 10) is told how many requests to keep out. A request
        // outside the torrent ends the connection too.
        let mut socket = TcpStream::connect(seeds.addrs[1]).await.unwrap();
        socket.write_all(&[&PROTOCOL[..], &[0, 0, 0, 0, 0, 0x10, 0, 0], &hash, &[1; 20]].concat()).await.unwrap();
        let said = b"d1:mde4:reqqi16ee";
        let mut hello = [0u8; 68 + 6 + 6 + 17 + 5];
        socket.read_exact(&mut hello).await.unwrap();
        assert_eq!(hello[25], 0x10);
        assert_eq!(hello[68..74], [0, 0, 0, 19, 20, 0]);
        assert_eq!(hello[74..91], said[..]);
        assert_eq!(hello[91..], [0, 0, 0, 2, 5, 0b1011_0000, 0, 0, 0, 1, 1]);
        socket.write_all(&request(4, 0, 16384)).await.unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut rest)).await.unwrap();
        assert!(ended.is_err() || rest.is_empty());
    }
}
