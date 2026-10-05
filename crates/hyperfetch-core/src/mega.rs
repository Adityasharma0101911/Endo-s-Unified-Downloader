//! MEGA file and folder links: read through MEGA's API, downloaded from where it says, and
//! decrypted on the way to disk.
//!
//! A file link carries the file's 256-bit key after `#`; a folder link carries the folder's
//! 128-bit key, which decrypts the keys of the files in it. MEGA stores a file encrypted with
//! AES-128-CTR, so any range of it decrypts on its own (see [`Cipher::apply`]): the engine asks for
//! ranges as for any file, writes them decrypted (see `engine::DownloadEngine::fetch_mega`), and
//! checks the MAC the key carries once the file is whole (see [`Cipher::mac_matches`]).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::{Aes128, Block};
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{json, Value};
use url::Url;

use crate::ingest::Task;
use crate::resolver::ResolverError;

/// MEGA's API.
const API: &str = "https://g.api.mega.co.nz/cs";

/// What a download MEGA's transfer quota stopped says.
pub const QUOTA: &str = "MEGA's free transfer quota is used up; try later or use a debrid service";

/// How long a folder's listing serves the downloads of its files (see [`folder`]).
const LISTING_KEPT: Duration = Duration::from_secs(600);

/// MEGA's base64: the URL-safe alphabet, without padding.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// What a MEGA link names.
enum Link {
    /// A file shared on its own: its public handle and 256-bit key.
    File { id: String, key: [u8; 32] },
    /// A shared folder: its public handle, its key (as the link spells it, and decoded), and the
    /// node in it the link names, if any, with whether the link says that node is a file.
    Folder { id: String, key_text: String, key: [u8; 16], node: Option<String>, file: bool },
}

/// The MEGA link `url`: `mega.nz/file/<id>#<key>` (or `/embed/`), `mega.nz/folder/<id>#<key>`
/// with `/file/<node>` or `/folder/<node>` after the key, or the older `mega.nz/#!<id>!<key>` and
/// `mega.nz/#F!<id>!<key>[!<node>]`. None for anything else, a key of the wrong length included.
fn parse(url: &Url) -> Option<Link> {
    let host = url.host_str()?.trim_end_matches('.');
    if !matches!(host.strip_prefix("www.").unwrap_or(host), "mega.nz" | "mega.co.nz") {
        return None;
    }
    let fragment = url.fragment().unwrap_or_default();
    let path: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let (folder, id, key, node, file) = match path.as_slice() {
        ["file" | "embed", id] => (false, *id, fragment, None, false),
        ["folder", id] => {
            let mut parts = fragment.split('/');
            let key = parts.next()?;
            let (kind, node) = (parts.next(), parts.next());
            (true, *id, key, node.filter(|_| matches!(kind, Some("file" | "folder"))), kind == Some("file"))
        }
        [] => match fragment.strip_prefix("F!") {
            Some(rest) => {
                let mut parts = rest.split('!');
                (true, parts.next()?, parts.next()?, parts.next(), false)
            }
            None => {
                let mut parts = fragment.strip_prefix('!')?.split('!');
                (false, parts.next()?, parts.next()?, None, false)
            }
        },
        _ => return None,
    };
    let token = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !token(id) || node.is_some_and(|n| !token(n)) {
        return None;
    }
    let (id, decoded, node) = (id.to_string(), BASE64.decode(key).ok()?, node.map(str::to_string));
    Some(if folder {
        Link::Folder { id, key_text: key.to_string(), key: decoded.try_into().ok()?, node, file }
    } else {
        Link::File { id, key: decoded.try_into().ok()? }
    })
}

/// Whether `url` is a MEGA folder link [`list`] reads: one that does not name a file in it.
pub fn lists(url: &Url) -> bool {
    matches!(parse(url), Some(Link::Folder { file: false, .. }))
}

/// Whether `url` is a MEGA link, which the engine downloads through [`file`] (a folder's fails
/// there with a message: [`list`] takes those).
pub fn handles(url: &Url) -> bool {
    parse(url).is_some()
}

/// The MEGA link `url` itself: MEGA's files come encrypted, so the engine downloads them through
/// their link (see [`file`]) and decrypts them on the way to disk.
pub async fn resolve(_client: &reqwest::Client, url: &Url) -> Result<Vec<Url>, ResolverError> {
    Ok(vec![url.clone()])
}

/// One task per file of the MEGA folder at `url` (or of the folder or file in it the link names),
/// in folders named after the shared folder and its subfolders, each downloading through its link
/// in the folder (`.../file/<node>`).
pub async fn list(http: &reqwest::Client, url: &Url) -> Result<Vec<Task>, String> {
    let Some(Link::Folder { id, key_text, key, node, .. }) = parse(url) else {
        return Err("not a MEGA folder link".to_string());
    };
    let nodes = folder(http, &id, &key).await?;
    let by_handle: HashMap<&str, &Node> = nodes.iter().map(|n| (n.handle.as_str(), n)).collect();
    let top = match &node {
        Some(node) => *by_handle.get(node.as_str()).ok_or("the MEGA folder holds no such file or folder")?,
        // The shared folder: the node whose parent is not shared with it.
        None => nodes.iter().find(|n| !by_handle.contains_key(n.parent.as_str())).ok_or("the MEGA folder is empty")?,
    };
    let clean = |n: &Node| Some(crate::engine::sanitize_component(&n.name)).filter(|c| !c.is_empty()).unwrap_or_else(|| n.handle.clone());
    let tasks: Vec<Task> = nodes
        .iter()
        .filter(|n| !n.folder)
        .filter_map(|file| {
            let folders = path_from(top, file, &by_handle)?;
            let link = format!("https://mega.nz/folder/{}#{}/file/{}", id, key_text, file.handle);
            Some(Task {
                urls: vec![Url::parse(&link).ok()?],
                folder: (!folders.is_empty()).then(|| folders.into_iter().map(clean).collect::<PathBuf>()),
                name: Some(PathBuf::from(clean(file))),
                size: Some(file.size),
                from_document: true,
                ..Task::default()
            })
        })
        .collect();
    if tasks.is_empty() {
        return Err("the MEGA folder holds no files".to_string());
    }
    Ok(tasks)
}

/// The folders from `top` down to the one `node` is in, when `node` is in `top` (none when it is
/// `top`): None for a node elsewhere, or in a loop of parents.
fn path_from<'n>(top: &Node, node: &'n Node, nodes: &HashMap<&str, &'n Node>) -> Option<Vec<&'n Node>> {
    let (mut path, mut at) = (Vec::new(), node);
    while at.handle != top.handle {
        at = nodes.get(at.parent.as_str())?;
        path.push(at);
        if path.len() > nodes.len() {
            return None;
        }
    }
    path.reverse();
    Some(path)
}

/// A MEGA file to download: where MEGA serves it for now, its name, and its key.
pub struct Source {
    pub url: Url,
    pub name: String,
    pub cipher: Cipher,
}

/// Asks MEGA's API for the file the link `link` names: a file's own link, or one of a file in a
/// shared folder (whose key comes from the folder's listing).
pub async fn file(http: &reqwest::Client, link: &Url) -> Result<Source, String> {
    let (key, answer) = match parse(link) {
        Some(Link::File { id, key }) => (key, api(http, json!({"a": "g", "g": 1, "ssl": 2, "p": id}), None).await?),
        Some(Link::Folder { id, key, node: Some(node), .. }) => {
            let nodes = folder(http, &id, &key).await?;
            let found = nodes.iter().find(|n| n.handle == node).ok_or("The MEGA folder holds no such file")?;
            let key = found.key.as_slice().try_into().map_err(|_| FOLDER.to_string())?;
            (key, api(http, json!({"a": "g", "g": 1, "ssl": 2, "n": node}), Some(&id)).await?)
        }
        Some(Link::Folder { .. }) => return Err(FOLDER.to_string()),
        None => return Err("Not a MEGA link".to_string()),
    };
    if let Some(code) = answer["e"].as_i64().filter(|&code| code < 0) {
        return Err(error(code));
    }
    let cipher = Cipher::new(&key);
    let name = answer["at"]
        .as_str()
        .and_then(|at| attributes_name(at, &cipher.aes))
        .ok_or("The key in the MEGA link does not open the file: check that the link is complete")?;
    let url = match answer["g"].as_str().and_then(|g| Url::parse(g).ok()) {
        Some(url) => url,
        None if answer["tl"].as_u64().is_some_and(|wait| wait > 0) => return Err(QUOTA.to_string()),
        None => return Err("MEGA gave no address to download the file from".to_string()),
    };
    Ok(Source { url, name, cipher })
}

const FOLDER: &str = "This MEGA link is a folder: add it as it is to download the files in it";

/// A file or folder of a shared MEGA folder, decrypted.
struct Node {
    handle: String,
    parent: String,
    folder: bool,
    name: String,
    key: Vec<u8>,
    size: u64,
}

/// The last folder listed, which the downloads of its files share.
// ponytail: one folder kept; files of several MEGA folders downloading at once list theirs again.
#[allow(clippy::type_complexity)]
static LISTED: Mutex<Option<(String, [u8; 16], Instant, Arc<Vec<Node>>)>> = Mutex::new(None);

/// The nodes of the shared MEGA folder `id`, decrypted with its `key`; those that do not decrypt
/// are left out.
async fn folder(http: &reqwest::Client, id: &str, key: &[u8; 16]) -> Result<Arc<Vec<Node>>, String> {
    if let Some((listed, listed_key, at, nodes)) = &*LISTED.lock() {
        if listed == id && listed_key == key && at.elapsed() < LISTING_KEPT {
            return Ok(Arc::clone(nodes));
        }
    }
    let answer = api(http, json!({"a": "f", "c": 1, "ca": 1, "r": 1}), Some(id)).await?;
    let raw = answer["f"].as_array().ok_or("MEGA's API sent no folder listing")?;
    let handles: HashSet<&str> = raw.iter().filter_map(|n| n["h"].as_str()).collect();
    let nodes: Vec<Node> = raw.iter().filter_map(|n| node(n, key, &handles)).collect();
    if nodes.is_empty() {
        return Err(if raw.is_empty() {
            "the MEGA folder is empty".to_string()
        } else {
            "the key in the MEGA link does not open the folder: check that the link is complete".to_string()
        });
    }
    let nodes = Arc::new(nodes);
    *LISTED.lock() = Some((id.to_string(), *key, Instant::now(), Arc::clone(&nodes)));
    Ok(nodes)
}

/// The node `n` of a folder listing: its key decrypted with the folder's `key` (AES-128-ECB), its
/// name with its own key. Its key is `<handle>:<key>` for each share it is in, the folder's being
/// the one whose handle the listing holds.
fn node(n: &Value, key: &[u8; 16], handles: &HashSet<&str>) -> Option<Node> {
    let folder = match n["t"].as_i64()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let shares: Vec<(&str, &str)> = n["k"].as_str()?.split('/').filter_map(|e| e.split_once(':')).collect();
    let (_, wrapped) = shares.iter().find(|(h, _)| handles.contains(h)).or(shares.first())?;
    let mut node_key = BASE64.decode(wrapped).ok()?;
    if node_key.len() != if folder { 16 } else { 32 } {
        return None;
    }
    let aes = Aes128::new(key.into());
    node_key.chunks_exact_mut(16).for_each(|block| aes.decrypt_block(Block::from_mut_slice(block)));
    let own = if folder {
        Aes128::new_from_slice(&node_key).ok()?
    } else {
        Cipher::new(node_key.as_slice().try_into().ok()?).aes
    };
    Some(Node {
        handle: n["h"].as_str()?.to_string(),
        parent: n["p"].as_str().unwrap_or_default().to_string(),
        folder,
        name: attributes_name(n["a"].as_str()?, &own)?,
        key: node_key,
        size: n["s"].as_u64().unwrap_or(0),
    })
}

/// The name in a node's attributes `at`: `MEGA{"n":"<name>",...}`, zero-padded and encrypted
/// with AES-128-CBC under a zero IV. None when they do not decrypt to that (a wrong key).
fn attributes_name(at: &str, aes: &Aes128) -> Option<String> {
    let data = BASE64.decode(at).ok()?;
    let (mut plain, mut previous) = (Vec::with_capacity(data.len()), Block::default());
    for chunk in data.chunks_exact(16) {
        let mut block = Block::clone_from_slice(chunk);
        aes.decrypt_block(&mut block);
        plain.extend(block.iter().zip(&previous).map(|(p, c)| p ^ c));
        previous = Block::clone_from_slice(chunk);
    }
    let json = std::str::from_utf8(plain.strip_prefix(b"MEGA")?).ok()?.trim_end_matches('\0');
    serde_json::from_str::<Value>(json).ok()?["n"].as_str().map(str::to_string)
}

/// MEGA's answer to `command`, asked of the shared folder `folder` if given. Its error codes
/// become messages; "try again" (-3) is tried again a few times first.
async fn api(http: &reqwest::Client, command: Value, folder: Option<&str>) -> Result<Value, String> {
    let mut url = Url::parse(&api_base()).map_err(|e| e.to_string())?;
    let sequence = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros());
    url.query_pairs_mut().append_pair("id", &(sequence % 10_000_000_000).to_string());
    if let Some(folder) = folder {
        url.query_pairs_mut().append_pair("n", folder);
    }
    let body = json!([command]).to_string();
    for wait in [1, 2, 4, 8, 0] {
        let request = http.post(url.clone()).header(reqwest::header::CONTENT_TYPE, "application/json").body(body.clone());
        let response = request.send().await.map_err(|e| format!("MEGA's API could not be reached: {}", e))?;
        match response.status().as_u16() {
            509 => return Err(QUOTA.to_string()),
            status if !(200..300).contains(&status) => return Err(format!("MEGA's API answered HTTP {}", status)),
            _ => {}
        }
        let bytes = response.bytes().await.map_err(|e| format!("MEGA's API answer broke off: {}", e))?;
        let answer: Value = serde_json::from_slice(&bytes).map_err(|e| format!("MEGA's API sent an unreadable answer: {}", e))?;
        // A bare number fails the whole request, a number in place of the command's answer that.
        match answer.as_i64().or_else(|| answer.get(0)?.as_i64()) {
            Some(-3) if wait > 0 => tokio::time::sleep(Duration::from_secs(wait)).await,
            Some(code) => return Err(error(code)),
            None => return answer.get(0).filter(|a| a.is_object()).cloned().ok_or("MEGA's API sent an unexpected answer".to_string()),
        }
    }
    Err(error(-3))
}

fn api_base() -> String {
    #[cfg(test)]
    if let Some(base) = tests::API.with(|api| api.borrow().clone()) {
        return base;
    }
    API.to_string()
}

/// What MEGA's error `code` means.
fn error(code: i64) -> String {
    match code {
        -17 => QUOTA.to_string(),
        -9 => "The MEGA file or folder does not exist: it was deleted, or the link is incomplete".to_string(),
        -16 => "MEGA has taken this file or folder down".to_string(),
        -11 => "MEGA denied access to this link".to_string(),
        -2 => "MEGA rejected the link as malformed".to_string(),
        -3 | -4 | -6 | -18 => "MEGA is busy or limiting requests right now; try again later".to_string(),
        code => format!("MEGA's API answered with error {}", code),
    }
}

/// The key of one MEGA file: AES-128 in CTR mode with a 64-bit nonce, and the MAC of its
/// plaintext. Never printed: it opens the file.
pub struct Cipher {
    aes: Aes128,
    nonce: [u8; 8],
    mac: [u8; 8],
}

impl Cipher {
    /// From the file's 256-bit key: the AES key is its two halves XORed, the nonce its third
    /// quarter, the MAC its fourth.
    fn new(key: &[u8; 32]) -> Self {
        let aes_key: [u8; 16] = std::array::from_fn(|i| key[i] ^ key[i + 16]);
        let quarter = |at: usize| -> [u8; 8] { std::array::from_fn(|i| key[at + i]) };
        Self { aes: Aes128::new(&aes_key.into()), nonce: quarter(16), mac: quarter(24) }
    }

    /// Decrypts `data`, the bytes at `offset` of the file, in place (encrypts plaintext alike):
    /// XORs it with the keystream there, the AES of the nonce and each 16-byte block's index.
    pub fn apply(&self, offset: u64, data: &mut [u8]) {
        let (first, skip) = (offset / 16, (offset % 16) as usize);
        let mut stream: Vec<Block> = (first..first + (skip + data.len()).div_ceil(16) as u64)
            .map(|index| {
                let mut block = Block::default();
                block[..8].copy_from_slice(&self.nonce);
                block[8..].copy_from_slice(&index.to_be_bytes());
                block
            })
            .collect();
        self.aes.encrypt_blocks(&mut stream);
        data.iter_mut().zip(stream.iter().flatten().skip(skip)).for_each(|(byte, key)| *byte ^= key);
    }

    /// Whether the file at `path`, decrypted, matches the MAC its key carries. One sequential
    /// read; blocking. An empty file has nothing to check.
    pub fn mac_matches(&self, path: &Path) -> std::io::Result<bool> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Ok(len == 0 || self.mac_of(file, len)? == self.mac)
    }

    /// The MAC of the `len` bytes of plaintext `data`, as MEGA's clients take it: the CBC-MAC of
    /// each chunk (128 KiB, 256 KiB, ... up to 1 MiB, then 1 MiB each; zero-padded, from the nonce
    /// twice), chained in another CBC-MAC, its halves' words XORed.
    fn mac_of(&self, mut data: impl Read, len: u64) -> std::io::Result<[u8; 8]> {
        let mut start = Block::default();
        start[..8].copy_from_slice(&self.nonce);
        start[8..].copy_from_slice(&self.nonce);
        let (mut meta, mut buf) = (Block::default(), vec![0u8; 1 << 20]);
        let (mut done, mut size) = (0u64, 128u64 << 10);
        while done < len {
            let chunk = &mut buf[..size.min(len - done) as usize];
            data.read_exact(chunk)?;
            let mut mac = start;
            for block in chunk.chunks(16) {
                mac.iter_mut().zip(block).for_each(|(m, b)| *m ^= b);
                self.aes.encrypt_block(&mut mac);
            }
            meta.iter_mut().zip(&mac).for_each(|(m, c)| *m ^= c);
            self.aes.encrypt_block(&mut meta);
            done += chunk.len() as u64;
            size = (size + (128 << 10)).min(1 << 20);
        }
        Ok(std::array::from_fn(|i| {
            let i = i + i / 4 * 4;
            meta[i] ^ meta[i + 4]
        }))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::{DownloadEngine, DownloadOptions};
    use std::cell::RefCell;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    thread_local! {
        /// Where [`api`] asks: the test's own mock (each test runs on one thread).
        pub(crate) static API: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    const AES_KEY: [u8; 16] = *b"0123456789abcdef";
    const NONCE: [u8; 8] = *b"noncenon";
    const FOLDER_KEY: [u8; 16] = *b"folder-key-16byt";

    /// A file's 256-bit key for `plain`: [`AES_KEY`] and [`NONCE`], with the MAC of `plain`.
    fn file_key(plain: &[u8]) -> [u8; 32] {
        let compose = |mac: [u8; 8]| -> [u8; 32] {
            let tail: Vec<u8> = NONCE.iter().chain(&mac).copied().collect();
            std::array::from_fn(|i| if i < 16 { AES_KEY[i] ^ tail[i] } else { tail[i - 16] })
        };
        let mac = Cipher::new(&compose([0; 8])).mac_of(plain, plain.len() as u64).unwrap();
        compose(mac)
    }

    fn encrypted(key: &[u8; 32], plain: &[u8]) -> Vec<u8> {
        let mut data = plain.to_vec();
        Cipher::new(key).apply(0, &mut data);
        data
    }

    /// Attributes naming `name`, as MEGA encrypts them with `aes`.
    fn attributes(name: &str, aes: &Aes128) -> String {
        let mut plain = format!("MEGA{}", json!({"n": name})).into_bytes();
        plain.resize(plain.len().div_ceil(16) * 16, 0);
        let mut previous = Block::default();
        for chunk in plain.chunks_exact_mut(16) {
            chunk.iter_mut().zip(&previous).for_each(|(p, c)| *p ^= c);
            aes.encrypt_block(Block::from_mut_slice(chunk));
            previous = Block::clone_from_slice(chunk);
        }
        BASE64.encode(plain)
    }

    /// `key` wrapped with the folder key (AES-128-ECB), as a node of the folder carries it.
    fn wrapped(key: &[u8]) -> String {
        let aes = Aes128::new(&FOLDER_KEY.into());
        let mut key = key.to_vec();
        key.chunks_exact_mut(16).for_each(|block| aes.encrypt_block(Block::from_mut_slice(block)));
        BASE64.encode(key)
    }

    fn plaintext(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    /// The ranges the storage of a [`mock`] served.
    type Served = Arc<Mutex<Vec<(u64, u64)>>>;

    /// A mock of MEGA that [`api`] asks from now on: its API at `/cs` answers each command as
    /// `answer` says (given the mock's address and the `n=` folder), its storage serves `file`
    /// at `/dl` by range (with HTTP 509, as over quota, when there is none).
    async fn mock(file: Option<Vec<u8>>, answer: impl Fn(&Url, &Value, Option<&str>) -> Value + Send + Sync + 'static) -> (Url, Served) {
        let listener = crate::hosts::unseen_listener().await;
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        API.with(|api| *api.borrow_mut() = Some(base.join("cs").unwrap().to_string()));
        let (file, answer, served) = (Arc::new(file), Arc::new(answer), Served::default());
        let (mock, log) = (base.clone(), Arc::clone(&served));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (file, answer, log, base) = (Arc::clone(&file), Arc::clone(&answer), Arc::clone(&log), mock.clone());
                tokio::spawn(async move {
                    let (mut data, mut buf) = (Vec::new(), [0u8; 4096]);
                    let end = loop {
                        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                            break at + 4;
                        }
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => data.extend_from_slice(&buf[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&data[..end]).into_owned();
                    let lower = head.to_ascii_lowercase();
                    let header = |name: &str| lower.lines().find_map(|l| l.strip_prefix(name)).map(|v| v.trim().to_string());
                    let mut words = head.split_whitespace();
                    let (method, target) = (words.next().unwrap_or_default(), words.next().unwrap_or("/"));
                    let (head, body) = if method == "POST" {
                        let len: usize = header("content-length:").and_then(|v| v.parse().ok()).unwrap_or(0);
                        while data.len() < end + len {
                            match socket.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => data.extend_from_slice(&buf[..n]),
                            }
                        }
                        let commands: Value = serde_json::from_slice(&data[end..end + len]).unwrap();
                        let folder = base.join(target).unwrap().query_pairs().find(|(k, _)| k == "n").map(|(_, v)| v.into_owned());
                        let body = answer(&base, &commands[0], folder.as_deref()).to_string().into_bytes();
                        (format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", body.len()), body)
                    } else {
                        match file.as_deref() {
                            None => ("HTTP/1.1 509 Bandwidth Limit Exceeded\r\nContent-Length: 0\r\n".to_string(), Vec::new()),
                            Some(file) if method == "HEAD" => {
                                (format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n", file.len()), Vec::new())
                            }
                            Some(file) => {
                                let last = file.len() as u64 - 1;
                                let (start, stop) = header("range: bytes=")
                                    .and_then(|r| {
                                        let (a, b) = r.split_once('-')?;
                                        Some((a.parse().ok()?, b.parse::<u64>().map_or(last, |b| b.min(last))))
                                    })
                                    .unwrap_or((0, last));
                                log.lock().push((start, stop));
                                let range = format!("Content-Range: bytes {}-{}/{}\r\n", start, stop, file.len());
                                let body = file[start as usize..=stop as usize].to_vec();
                                (format!("HTTP/1.1 206 Partial Content\r\n{}Content-Length: {}\r\n", range, body.len()), body)
                            }
                        }
                    };
                    if socket.write_all(format!("{}Connection: close\r\n\r\n", head).as_bytes()).await.is_err() {
                        return;
                    }
                    for piece in body.chunks(64 * 1024) {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        if socket.write_all(piece).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (base, served)
    }

    /// The API of a mock serving the file `id` named `name` with `key`, at the mock's `/dl`.
    fn file_api(
        id: &'static str,
        name: &'static str,
        key: [u8; 32],
        size: usize,
    ) -> impl Fn(&Url, &Value, Option<&str>) -> Value + Send + Sync + 'static {
        move |base: &Url, command: &Value, _: Option<&str>| match command["p"].as_str() {
            Some(p) if p == id && command["a"] == "g" => {
                json!([{"s": size, "at": attributes(name, &Cipher::new(&key).aes), "g": base.join("dl").unwrap().as_str()}])
            }
            _ => json!([-9]),
        }
    }

    fn file_link(id: &str, key: &[u8; 32]) -> Url {
        Url::parse(&format!("https://mega.nz/file/{}#{}", id, BASE64.encode(key))).unwrap()
    }

    fn engine(link: Url, dir: &Path) -> DownloadEngine {
        let options = DownloadOptions {
            num_connections: 8,
            base_chunk_size: 256 * 1024,
            output_path: Some(dir.to_path_buf()),
            ..Default::default()
        };
        DownloadEngine::new(vec![link], options)
    }

    #[test]
    fn links_of_every_form_are_read() {
        let (file_key, folder_key) = (BASE64.encode([7u8; 32]), BASE64.encode([9u8; 16]));
        let read = |link: String| parse(&Url::parse(&link).unwrap());
        for link in [
            format!("https://mega.nz/file/AbC-_1#{file_key}"),
            format!("https://mega.nz/embed/AbC-_1#{file_key}"),
            format!("https://mega.nz/#!AbC-_1!{file_key}"),
            format!("https://www.mega.co.nz/#!AbC-_1!{file_key}"),
        ] {
            assert!(matches!(read(link.clone()), Some(Link::File { id, key }) if id == "AbC-_1" && key == [7; 32]), "{link}");
        }
        for (link, wanted, says_file) in [
            (format!("https://mega.nz/folder/F1#{folder_key}"), None, false),
            (format!("https://mega.nz/folder/F1#{folder_key}/file/N1"), Some("N1"), true),
            (format!("https://mega.nz/folder/F1#{folder_key}/folder/N1"), Some("N1"), false),
            (format!("https://mega.nz/#F!F1!{folder_key}"), None, false),
            (format!("https://mega.nz/#F!F1!{folder_key}!N1"), Some("N1"), false),
        ] {
            let Some(Link::Folder { id, key, node, file, key_text }) = read(link.clone()) else { panic!("{link}") };
            assert_eq!((id.as_str(), key, node.as_deref(), file, key_text), ("F1", [9; 16], wanted, says_file, folder_key.clone()), "{link}");
            let url = Url::parse(&link).unwrap();
            assert_eq!((lists(&url), handles(&url)), (!says_file, true), "{link}");
        }
        for link in [
            format!("https://mega.nz/file/AbC#{folder_key}"),
            format!("https://mega.nz/folder/F1#{file_key}"),
            "https://mega.nz/file/AbC".to_string(),
            format!("https://megalodon.nz/file/AbC#{file_key}"),
            format!("https://mega.nz/file/A.b#{file_key}"),
            "https://mega.nz/".to_string(),
        ] {
            assert!(read(link.clone()).is_none(), "{link}");
            assert!(!handles(&Url::parse(&link).unwrap()), "{link}");
        }
    }

    #[test]
    fn any_range_decrypts_on_its_own() {
        let plain = plaintext(10_000);
        let key = file_key(&plain);
        let cipher = encrypted(&key, &plain);
        assert_ne!(cipher, plain);
        let mut pieces = cipher.clone();
        let mut at = 0;
        for len in [1, 15, 16, 17, 333, 4096, 5 * 1024].into_iter().cycle() {
            let end = (at + len).min(pieces.len());
            Cipher::new(&key).apply(at as u64, &mut pieces[at..end]);
            at = end;
            if at == pieces.len() {
                break;
            }
        }
        assert!(pieces == plain);
        // The keystream is the AES of the nonce and the block's index, big-endian.
        let mut block = Block::default();
        block[..8].copy_from_slice(&NONCE);
        block[8..].copy_from_slice(&3u64.to_be_bytes());
        Aes128::new(&AES_KEY.into()).encrypt_block(&mut block);
        assert!(cipher[48..64].iter().zip(&plain[48..64]).zip(&block).all(|((c, p), k)| c ^ p == *k));
    }

    #[test]
    fn the_mac_of_a_file_is_its_chunks_chained() {
        let dir = tempfile::tempdir().unwrap();
        // Past the chunks that grow, into those of 1 MiB, ending in a partial block.
        let plain = plaintext((128 + 256 + 384 + 512 + 640 + 768 + 896 + 1024 + 1024 + 3) * 1024 + 5);
        let key = file_key(&plain);
        let path = dir.path().join("file");
        std::fs::write(&path, &plain).unwrap();
        assert!(Cipher::new(&key).mac_matches(&path).unwrap());
        let mut damaged = plain;
        damaged[3 * 1024 * 1024] ^= 1;
        std::fs::write(&path, &damaged).unwrap();
        assert!(!Cipher::new(&key).mac_matches(&path).unwrap());
        // A file of one short block, as one of exactly one.
        for len in [5, 16] {
            let plain = plaintext(len);
            std::fs::write(&path, &plain).unwrap();
            assert!(Cipher::new(&file_key(&plain)).mac_matches(&path).unwrap());
        }
    }

    #[tokio::test]
    async fn a_file_downloads_decrypted_over_several_connections() {
        let plain = plaintext(5 * 1024 * 1024 + 777);
        let key = file_key(&plain);
        let (_, served) = mock(Some(encrypted(&key, &plain)), file_api("Fa", "Holiday video.mp4", key, plain.len())).await;
        let dir = tempfile::tempdir().unwrap();
        let path = engine(file_link("Fa", &key), dir.path()).run(None).await.unwrap();
        assert_eq!(path, dir.path().join("Holiday video.mp4"));
        assert!(std::fs::read(&path).unwrap() == plain);
        assert!(served.lock().iter().filter(|(start, _)| *start > 0).count() > 1, "{:?}", served.lock());
        // Only the link is kept: MEGA's address serves ciphertext, for a while.
        let history = crate::history::DownloadHistoryManager::load();
        let entry = history.entries().iter().find(|e| e.file_path == std::path::absolute(&path).unwrap()).unwrap();
        assert_eq!(entry.urls, vec![file_link("Fa", &key).to_string()]);
    }

    #[tokio::test]
    async fn a_download_resumes_mid_file() {
        let plain = plaintext(3 * 1024 * 1024 + 101);
        let key = file_key(&plain);
        let (_, served) = mock(Some(encrypted(&key, &plain)), file_api("Fr", "resumed.bin", key, plain.len())).await;
        let dir = tempfile::tempdir().unwrap();
        // An earlier run wrote (decrypted) the first part, ending inside a block.
        let half = 2 * 1024 * 1024 + 9;
        let part = dir.path().join("resumed.bin.part");
        let mut on_disk = plain[..half].to_vec();
        on_disk.resize(plain.len(), 0);
        std::fs::write(&part, &on_disk).unwrap();
        let mut state = crate::state::DownloadState::new(
            "resumed.bin".to_string(),
            plain.len() as u64,
            256 * 1024,
            vec![file_link("Fr", &key).to_string()],
        );
        state.completed_ranges = vec![crate::range::ByteRange::new(0, half as u64 - 1).unwrap()];
        state.save_atomic(&crate::state::DownloadState::state_file_path(&part)).unwrap();

        let path = engine(file_link("Fr", &key), dir.path()).run(None).await.unwrap();
        assert_eq!(path, dir.path().join("resumed.bin"));
        assert!(std::fs::read(&path).unwrap() == plain);
        let served = served.lock().clone();
        assert!(served.iter().filter(|(start, _)| *start > 0).all(|(start, _)| *start >= half as u64), "{served:?}");
        assert!(served.iter().any(|(start, _)| *start == half as u64), "{served:?}");
    }

    #[tokio::test]
    async fn a_file_that_does_not_match_its_mac_fails() {
        let plain = plaintext(300 * 1024);
        let key = file_key(&plain);
        let mut damaged = encrypted(&key, &plain);
        damaged[200_000] ^= 0x40;
        mock(Some(damaged), file_api("Fm", "damaged.bin", key, plain.len())).await;
        let dir = tempfile::tempdir().unwrap();
        let error = engine(file_link("Fm", &key), dir.path()).run(None).await.unwrap_err();
        assert!(error.contains("MAC"), "{error}");
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[tokio::test]
    async fn quota_errors_say_so() {
        let plain = plaintext(1000);
        let key = file_key(&plain);
        let dir = tempfile::tempdir().unwrap();
        mock(None, file_api("Fq", "quota.bin", key, plain.len())).await;
        assert_eq!(engine(file_link("Fq", &key), dir.path()).run(None).await.unwrap_err(), QUOTA);
        mock(Some(plain), |_, _, _| json!([-17])).await;
        assert_eq!(engine(file_link("Fq", &key), dir.path()).run(None).await.unwrap_err(), QUOTA);
        // A key that does not open the file says so.
        let wrong = [1u8; 32];
        mock(Some(Vec::new()), file_api("Fk", "x.bin", key, 1000)).await;
        let error = engine(file_link("Fk", &wrong), dir.path()).run(None).await.unwrap_err();
        assert!(error.contains("does not open the file"), "{error}");
    }

    #[tokio::test]
    async fn a_folder_lists_its_files_and_each_downloads() {
        let (one, two) = (plaintext(70_000), plaintext(1234));
        let (one_key, two_key) = (file_key(&one), file_key(&two[..]));
        let sub_key = [5u8; 16];
        let nodes = json!([
            {"h": "Root", "p": "Owner", "t": 1, "a": attributes("Photos ../", &Aes128::new(&FOLDER_KEY.into())), "k": format!("Root:{}", wrapped(&FOLDER_KEY))},
            {"h": "Sub", "p": "Root", "t": 1, "a": attributes("2024", &Aes128::new(&sub_key.into())), "k": format!("Root:{}", wrapped(&sub_key))},
            {"h": "One", "p": "Root", "t": 0, "s": one.len(), "a": attributes("one.jpg", &Cipher::new(&one_key).aes), "k": format!("Usr:AAAA/Root:{}", wrapped(&one_key))},
            {"h": "Two", "p": "Sub", "t": 0, "s": two.len(), "a": attributes("../two.jpg", &Cipher::new(&two_key).aes), "k": format!("Root:{}", wrapped(&two_key))},
            {"h": "Bad", "p": "Root", "t": 0, "s": 1, "a": "AAAA", "k": "Root:AAAA"},
        ]);
        let two_cipher = encrypted(&two_key, &two);
        let api = move |base: &Url, command: &Value, folder: Option<&str>| match (command["a"].as_str(), folder) {
            (Some("f"), Some("Shared")) => json!([{"f": nodes.clone()}]),
            (Some("g"), Some("Shared")) if command["n"] == "Two" => {
                json!([{"s": 1234, "at": attributes("../two.jpg", &Cipher::new(&two_key).aes), "g": base.join("dl").unwrap().as_str()}])
            }
            _ => json!([-9]),
        };
        mock(Some(two_cipher), api).await;
        let key_text = BASE64.encode(FOLDER_KEY);
        let link = Url::parse(&format!("https://mega.nz/folder/Shared#{key_text}")).unwrap();
        let http = reqwest::Client::new();
        let mut tasks = list(&http, &link).await.unwrap();
        tasks.sort_by(|a, b| a.name.cmp(&b.name));
        let listed: Vec<_> = tasks.iter().map(|t| (t.folder.clone(), t.name.clone(), t.size, t.urls[0].to_string())).collect();
        let file = |node: &str| format!("https://mega.nz/folder/Shared#{key_text}/file/{node}");
        assert_eq!(
            listed,
            vec![
                (Some(PathBuf::from("Photos .._").join("2024")), Some(PathBuf::from(".._two.jpg")), Some(1234), file("Two")),
                (Some(PathBuf::from("Photos .._")), Some(PathBuf::from("one.jpg")), Some(70_000), file("One")),
            ]
        );
        assert!(tasks.iter().all(|t| t.from_document));
        // A subfolder's link lists only what is in it.
        let sub = Url::parse(&format!("{link}/folder/Sub")).unwrap();
        assert_eq!(list(&http, &sub).await.unwrap().len(), 1);

        let dir = tempfile::tempdir().unwrap();
        let path = engine(tasks[0].urls[0].clone(), dir.path()).run(None).await.unwrap();
        // Named by the engine, which drops leading dots, unlike the listing (whose name goes with the task).
        assert_eq!(path, dir.path().join("_two.jpg"));
        assert!(std::fs::read(&path).unwrap() == two);
        let error = engine(link, dir.path()).run(None).await.unwrap_err();
        assert!(error.contains("is a folder"), "{error}");
    }

    // rmega's public test files and folder (github.com/topac/rmega, spec/integration).

    #[tokio::test]
    #[ignore = "downloads from MEGA"]
    async fn live_a_file_and_a_folder_download() {
        let dir = tempfile::tempdir().unwrap();
        let small = Url::parse("https://mega.nz/file/muAVRRbb#zp9dvPvoVck8-4IwTazqsUqol6yiUK7kwLWOwrD8Jqo").unwrap();
        let path = engine(small, dir.path()).run(None).await.unwrap();
        assert_eq!(path, dir.path().join("testfile.txt"));
        assert_eq!(std::fs::read(&path).unwrap(), b"helloworld!
");

        let folder = Url::parse("https://mega.nz/folder/GvgkUIIK#v2hd_5GSvciGKazNeWSa6A").unwrap();
        let tasks = list(&reqwest::Client::new(), &folder).await.unwrap();
        let paths: Vec<PathBuf> = tasks.iter().map(|t| t.folder.clone().unwrap_or_default().join(t.name.as_ref().unwrap())).collect();
        for wanted in ["another_test_folder/b.txt", "another_test_folder/c/c.txt"] {
            assert!(paths.contains(&PathBuf::from(wanted)), "{paths:?}");
        }
        let b = &tasks[paths.iter().position(|p| p.ends_with("b.txt")).unwrap()];
        let path = engine(b.urls[0].clone(), dir.path()).run(None).await.unwrap();
        assert_eq!(path, dir.path().join("b.txt"));
    }

    #[tokio::test]
    #[ignore = "downloads 15 MB from MEGA"]
    async fn live_a_large_file_downloads_over_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let link = Url::parse("https://mega.nz/file/3zpE1ToL#B1L4o8POE4tER4h1tyVoGNxaXFhbjwfxhe3Eyp9nrN8").unwrap();
        let options = DownloadOptions {
            num_connections: 8,
            output_path: Some(dir.path().to_path_buf()),
            expected_checksum: Some("md5:a92ec9994911866e3ea31aa1d914ac23".to_string()),
            ..Default::default()
        };
        let path = DownloadEngine::new(vec![link], options).run(None).await.unwrap();
        assert_eq!(path, dir.path().join("testfile_big_15mb.binary"));
    }
}
