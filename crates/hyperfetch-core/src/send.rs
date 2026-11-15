//! Send shares: the open-source Send's links (`https://host/download/<id>/#<key>`), on any server
//! that runs it. The key is in the link's fragment and never leaves this computer; the server
//! holds the file encrypted.
//!
//! The wire format, as Send's own code has it (app/keychain.js, app/ece.js, app/api.js and
//! server/middleware/auth.js): from the 16-byte key, HKDF-SHA-256 with an empty salt gives the
//! metadata key (info `metadata`, AES-128-GCM with a zero IV over the name and size) and the
//! authentication key (info `authentication`, 64 bytes); each request carries `Authorization:
//! send-v1 <HMAC-SHA-256 of the server's latest nonce>`, and each answer a new nonce in
//! `WWW-Authenticate`. The body is RFC 8188 `aes128gcm` with the key itself as input keying
//! material: a header (salt, record size, key id), then records of the record size, each
//! sealed on its own, padded with a 1 (a 2 on the last) and zeros.
//!
//! The server sends the whole body every time (no ranges), and counts a download only once it
//! has sent all of it, deleting the file at its download limit: so a share downloads over one
//! connection, in order, and each record's tag is checked before its plaintext is written. A
//! failed download starts again from the beginning; there is no resume, no delta and no piece
//! repair. A share with a password is refused, saying so.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use reqwest::{Client, StatusCode};
use ring::{aead, hkdf, hmac};
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{DownloadOptions, EngineSnapshot, CANCELLED};

/// The nonce Send's client starts with, before the server names one.
const FIRST_NONCE: &str = "yRCdyQ1EMSA3mo4rqSkuNQ==";
/// Largest record size taken: Send writes 64 KiB ones.
const MAX_RECORD: usize = 16 << 20;
const TAG: usize = 16;
/// How often progress is shown.
const SHOW_EVERY: Duration = Duration::from_millis(250);
const EXPIRED: &str = "The Send link has expired, or was downloaded as many times as it allows";

/// Whether `url` is a Send share's link: `/download/<id>` with its key in the fragment.
pub(crate) fn handles(url: &Url) -> bool {
    share(url).is_some()
}

/// The share's id and key of a Send link.
fn share(url: &Url) -> Option<(String, [u8; 16])> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let mut parts = url.path_segments()?.filter(|s| !s.is_empty());
    let (Some("download"), Some(id), None) = (parts.next(), parts.next(), parts.next()) else { return None };
    if !(10..=16).contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let key: [u8; 16] = b64(url.fragment()?)?.try_into().ok()?;
    Some((id.to_string(), key))
}

/// Base64 in either alphabet, padded or not, as Send's client reads it.
fn b64(text: &str) -> Option<Vec<u8>> {
    let plain: String = text.trim().trim_end_matches('=').chars().map(|c| match c {
        '-' => '+',
        '_' => '/',
        c => c,
    }).collect();
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(plain).ok()
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// HKDF-SHA-256 of `ikm` with `salt` and `info`, `len` bytes long.
fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    struct Len(usize);
    impl hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    let mut out = vec![0; len];
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    prk.expand(&[info], Len(len)).and_then(|okm| okm.fill(&mut out)).expect("HKDF lengths are fixed and small");
    out
}

/// The `Authorization` header that answers `nonce` with the share's authentication key.
fn signed(auth_key: &[u8], nonce: &str) -> Option<String> {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, auth_key), &b64(nonce)?);
    Some(format!("send-v1 {}", b64url(tag.as_ref())))
}

/// The nonce a Send answer gives for the next request.
fn nonce_of(answer: &reqwest::Response) -> Option<String> {
    let header = answer.headers().get(WWW_AUTHENTICATE)?.to_str().ok()?;
    header.strip_prefix("send-v1 ").map(|n| n.trim().to_string())
}

/// `sealed` opened with AES-128-GCM under `key` and a zero IV, as Send seals its metadata.
fn open_zero_iv(key: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, key).ok()?);
    let mut buf = sealed.to_vec();
    let plain = key.open_in_place(aead::Nonce::assume_unique_for_key([0; 12]), aead::Aad::empty(), &mut buf).ok()?;
    Some(plain.to_vec())
}

/// The name and size the share's metadata gives, opened with `key`.
fn open_metadata(key: &[u8; 16], sealed: &[u8]) -> Result<(String, u64), String> {
    let unreadable = || "the Send link's key does not open its file's details: is the link whole?".to_string();
    let plain = open_zero_iv(&hkdf(&[], key, b"metadata", 16), sealed).ok_or_else(unreadable)?;
    let meta: serde_json::Value = serde_json::from_slice(&plain).map_err(|_| unreadable())?;
    let name = meta["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("send-file").to_string();
    Ok((name, meta["size"].as_u64().ok_or_else(unreadable)?))
}

/// An RFC 8188 `aes128gcm` body opened record by record as it arrives (see the module docs).
struct Records {
    ikm: Vec<u8>,
    /// The header's record size, content key and nonce base, once read.
    open: Option<(usize, aead::LessSafeKey, [u8; 12])>,
    pending: Vec<u8>,
    seq: u32,
}

impl Records {
    fn new(ikm: &[u8]) -> Self {
        Self { ikm: ikm.to_vec(), open: None, pending: Vec::new(), seq: 0 }
    }

    /// The plaintext of the records `bytes` completes; the last record waits for [`Self::finish`].
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self.pending.extend_from_slice(bytes);
        if self.open.is_none() {
            let Some(&idlen) = self.pending.get(20) else { return Ok(Vec::new()) };
            let header = 21 + idlen as usize;
            if self.pending.len() < header {
                return Ok(Vec::new());
            }
            let rs = u32::from_be_bytes(self.pending[16..20].try_into().expect("four bytes")) as usize;
            if !(TAG + 2..=MAX_RECORD).contains(&rs) {
                return Err(format!("the Send file's record size ({rs}) cannot be read"));
            }
            let salt = &self.pending[..16];
            let cek = hkdf(salt, &self.ikm, b"Content-Encoding: aes128gcm\0", 16);
            let base: [u8; 12] = hkdf(salt, &self.ikm, b"Content-Encoding: nonce\0", 12).try_into().expect("twelve bytes");
            let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).expect("a 16-byte key"));
            self.open = Some((rs, key, base));
            self.pending.drain(..header);
        }
        let rs = self.open.as_ref().map_or(0, |(rs, _, _)| *rs);
        let mut out = Vec::new();
        // A whole record with more after it is not the last.
        while self.pending.len() > rs {
            let mut record: Vec<u8> = self.pending.drain(..rs).collect();
            out.extend_from_slice(self.open_record(&mut record, false)?);
        }
        Ok(out)
    }

    /// The plaintext of the last record, once the body ended.
    fn finish(&mut self) -> Result<Vec<u8>, String> {
        if self.open.is_none() || self.pending.is_empty() {
            return Err("the Send file ended early".to_string());
        }
        let mut record = std::mem::take(&mut self.pending);
        Ok(self.open_record(&mut record, true)?.to_vec())
    }

    /// `record` opened in place and checked, without its padding.
    fn open_record<'a>(&mut self, record: &'a mut [u8], last: bool) -> Result<&'a [u8], String> {
        let (_, key, base) = self.open.as_ref().expect("opened by push");
        let mut nonce = *base;
        for (n, s) in nonce[8..].iter_mut().zip(self.seq.to_be_bytes()) {
            *n ^= s;
        }
        let at = self.seq;
        self.seq = self.seq.checked_add(1).ok_or("the Send file has too many records")?;
        let broken = || format!("the Send file's record {at} does not decrypt: it was changed or damaged, so nothing of it is kept");
        let plain = key.open_in_place(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), record).map_err(|_| broken())?;
        let end = plain.iter().rposition(|&b| b != 0).ok_or_else(broken)?;
        match (plain[end], last) {
            (1, false) | (2, true) => Ok(&plain[..end]),
            (1, true) => Err("the Send file ended early".to_string()),
            _ => Err(broken()),
        }
    }
}

/// A Send server asked about one share: each request answers the latest nonce with the share's
/// authentication key, and its answer names the next nonce.
struct Session<'a> {
    http: &'a Client,
    base: String,
    auth_key: Vec<u8>,
    nonce: String,
}

impl Session<'_> {
    async fn get(&mut self, path: &str) -> Result<reqwest::Response, String> {
        for _ in 0..2 {
            let header = signed(&self.auth_key, &self.nonce).ok_or("the Send server's nonce cannot be read")?;
            let answer = self.http.get(format!("{}{path}", self.base)).header(AUTHORIZATION, header).send().await;
            let answer = answer.map_err(|e| format!("Cannot reach the Send server: {}", e.without_url()))?;
            let fresh = nonce_of(&answer);
            // A nonce the server no longer expects is answered again with the one it names.
            let again = answer.status() == StatusCode::UNAUTHORIZED && fresh.as_ref().is_some_and(|f| *f != self.nonce);
            if let Some(fresh) = fresh {
                self.nonce = fresh;
            }
            if !again {
                return Ok(answer);
            }
        }
        Err(REFUSED.to_string())
    }
}

const REFUSED: &str = "the Send server refused the link's key: is the link whole?";

/// Downloads the Send share `link` with `http`, decrypting it, into the file
/// `options.output_path` names, else one named as the share says in `save_dir`.
pub(crate) async fn run(
    link: &Url,
    http: &Client,
    options: &DownloadOptions,
    save_dir: &Path,
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    cancel: CancellationToken,
) -> Result<(PathBuf, bool), String> {
    let (id, key) = share(link).ok_or("not a Send link with its key")?;
    let base = link.origin().ascii_serialization();
    let exists = http.get(format!("{base}/api/exists/{id}")).send().await.map_err(|e| format!("Cannot reach the Send server: {}", e.without_url()))?;
    if exists.status() == StatusCode::NOT_FOUND {
        return Err(EXPIRED.to_string());
    }
    let nonce = nonce_of(&exists).unwrap_or_else(|| FIRST_NONCE.to_string());
    let about: serde_json::Value = small_json(exists).await.unwrap_or_default();
    if about["requiresPassword"].as_bool() == Some(true) {
        return Err("The Send link has a password, which the app cannot open yet: download it in the browser".to_string());
    }
    let mut session = Session { http, base, auth_key: hkdf(&[], &key, b"authentication", 64), nonce };
    let meta = session.get(&format!("/api/metadata/{id}")).await?;
    match meta.status() {
        StatusCode::NOT_FOUND => return Err(EXPIRED.to_string()),
        StatusCode::UNAUTHORIZED => return Err(REFUSED.to_string()),
        status if !status.is_success() => return Err(format!("the Send server answered HTTP {}", status.as_u16())),
        _ => {}
    }
    let meta = small_json(meta).await.ok_or("the Send server's answer cannot be read")?;
    let sealed = meta["metadata"].as_str().and_then(b64).ok_or("the Send server gave no file details")?;
    let (name, size) = open_metadata(&key, &sealed)?;
    if options.max_filesize.is_some_and(|max| size > max) {
        return Err(format!("{name} is larger than the most allowed ({size} bytes)"));
    }
    let target = match &options.output_path {
        Some(path) if !crate::engine::is_dir_target(path) => path.clone(),
        _ => crate::engine::free_path(&save_dir.join(crate::engine::sanitize_component(&name))),
    };
    let part = crate::engine::part_path(&target);
    if let Some(dir) = part.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    }
    let answer = session.get(&format!("/api/download/{id}")).await?;
    match answer.status() {
        StatusCode::NOT_FOUND => return Err(EXPIRED.to_string()),
        status if !status.is_success() => return Err(format!("the Send server answered HTTP {} for the file", status.as_u16())),
        _ => {}
    }
    let shown = Shown { snapshots, size, target: target.clone() };
    if let Err(e) = receive(answer, &key, &part, &shown, options, &cancel).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    tokio::fs::rename(&part, &target).await.map_err(|e| format!("Cannot rename {}: {e}", part.display()))?;
    Ok((target, true))
}

/// A short JSON answer (a share's details), at most 1 MiB.
async fn small_json(answer: reqwest::Response) -> Option<serde_json::Value> {
    if answer.content_length().is_some_and(|n| n > 1 << 20) {
        return None;
    }
    let bytes = answer.bytes().await.ok()?;
    serde_json::from_slice(&bytes).ok().filter(|_| bytes.len() <= 1 << 20)
}

/// Where progress goes: of `size` bytes, into `target`.
struct Shown {
    snapshots: Option<broadcast::Sender<EngineSnapshot>>,
    size: u64,
    target: PathBuf,
}

/// Decrypts `answer`'s body into `part`, record by record: what the metadata says, no more.
async fn receive(answer: reqwest::Response, key: &[u8; 16], part: &Path, shown: &Shown, options: &DownloadOptions, cancel: &CancellationToken) -> Result<(), String> {
    let mut file = tokio::fs::File::create(part).await.map_err(|e| format!("Cannot create {}: {e}", part.display()))?;
    let limiter = options.max_speed.filter(|&s| s > 0).map(crate::worker::RateLimiter::new);
    let (mut records, mut body, mut done) = (Records::new(key), answer.bytes_stream(), 0u64);
    let (started, mut last_shown) = (Instant::now(), Instant::now());
    let idle = Duration::from_secs(options.stall_timeout_secs.max(1));
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CANCELLED.to_string()),
            chunk = tokio::time::timeout(idle, body.next()) => chunk.map_err(|_| format!("the Send server sent nothing for {}s", idle.as_secs()))?,
        };
        let (plain, ended) = match chunk {
            Some(bytes) => {
                let bytes = bytes.map_err(|e| format!("the download from the Send server broke: {}", e.without_url()))?;
                if let Some(limiter) = &limiter {
                    limiter.acquire(bytes.len() as u64).await;
                }
                (records.push(&bytes)?, false)
            }
            None => (records.finish()?, true),
        };
        done += plain.len() as u64;
        if done > shown.size {
            return Err("the Send file is larger than its details say".to_string());
        }
        file.write_all(&plain).await.map_err(|e| format!("Disk write error: {e}"))?;
        if let Some(tx) = shown.snapshots.as_ref().filter(|_| ended || last_shown.elapsed() >= SHOW_EVERY) {
            last_shown = Instant::now();
            let _ = tx.send(EngineSnapshot {
                total_bytes: shown.size,
                downloaded_bytes: done,
                speed_bytes_per_sec: done as f64 / started.elapsed().as_secs_f64().max(0.001),
                progress_ratio: if shown.size > 0 { done as f64 / shown.size as f64 } else { 1.0 },
                active_workers: 1,
                target_path: Some(shown.target.clone()),
                ..Default::default()
            });
        }
        if ended {
            break;
        }
    }
    file.flush().await.map_err(|e| format!("Disk write error: {e}"))?;
    if done != shown.size {
        return Err(format!("the Send file holds {done} bytes, not the {} its details say", shown.size));
    }
    Ok(())
}

/// Whether `url` has a Send share's path (`/download/<id>`) and a fragment: the fragment is its
/// key, which `history::redact_url` takes out.
pub(crate) fn keyed_path(url: &Url) -> bool {
    let mut parts = url.path_segments().into_iter().flatten().filter(|s| !s.is_empty());
    let shape = matches!((parts.next(), parts.next(), parts.next()), (Some("download"), Some(id), None) if (10..=16).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit()));
    shape && url.fragment().is_some_and(|f| !f.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::DownloadEngine;
    use std::num::NonZeroU32;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;

    fn unb64(text: &str) -> Vec<u8> {
        b64(text).unwrap()
    }

    fn opened(ikm: &[u8], body: &[u8], step: usize) -> Result<Vec<u8>, String> {
        let mut records = Records::new(ikm);
        let mut out = Vec::new();
        for piece in body.chunks(step) {
            out.extend(records.push(piece)?);
        }
        out.extend(records.finish()?);
        Ok(out)
    }

    /// RFC 8188's own examples (sections 3.1 and 3.2): one record, and two records of 25 octets
    /// with a key id and a padding octet, however the body arrives.
    #[test]
    fn the_rfc_examples_decrypt() {
        let one = unb64("I1BsxtFttlv3u_Oo94xnmwAAEAAA-NAVub2qFgBEuQKRapoZu-IxkIva3MEB1PD-ly8Thjg");
        let two = unb64("uNCkWiNYzKTnBN9ji3-qWAAAABkCYTHOG8chz_gnvgOqdGYovxyjuqRyJFjEDyoF1Fvkj6hQPdPHI51OEUKEpgz3SsLWIqS_uA");
        for step in [1, 7, 1000] {
            assert_eq!(opened(&unb64("yqdlZ-tYemfogSmv7Ws5PQ"), &one, step).unwrap(), b"I am the walrus");
            assert_eq!(opened(&unb64("BO3ZVPxUlnLORbVGMpbT1Q"), &two, step).unwrap(), b"I am the walrus");
        }
        assert!(opened(&unb64("BO3ZVPxUlnLORbVGMpbT1Q"), &two[..two.len() - 1], 9).unwrap_err().contains("does not decrypt"));
        let mut changed = two.clone();
        changed[30] ^= 1;
        assert!(opened(&unb64("BO3ZVPxUlnLORbVGMpbT1Q"), &changed, 9).unwrap_err().contains("record 0 does not decrypt"));
        // The first record alone: its delimiter says more was to come.
        let first = &two[..23 + 25];
        assert_eq!(opened(&unb64("BO3ZVPxUlnLORbVGMpbT1Q"), first, 9).unwrap_err(), "the Send file ended early");
    }

    /// Send's own crypto test (test/frontend/tests/crypto-tests.js): its HKDF keys from a secret,
    /// and, from its PBKDF2 keys, an HMAC signature and AES-GCM with a zero IV as the metadata has.
    #[test]
    fn keys_match_sends_own_test_vectors() {
        let secret = unb64("SPIfAlwbnncIFw3hEHYihw");
        assert_eq!(b64url(&hkdf(&[], &secret, b"encryption", 16)), "g7okjWWO9yueDz16-owShQ");
        let auth = hkdf(&[], &secret, b"authentication", 64);
        assert_eq!(b64url(&auth), "TQOGtmQ8-ZfnWu6Iq-U1IAVBVREFuI17xqsW1shiC8eMCa-a5qeYTvoX3-5kCoCha8R59ycnPDnTz75clLBmbQ");
        let pbkdf2 = |salt: &[u8], len: usize| {
            let mut out = vec![0; len];
            ring::pbkdf2::derive(ring::pbkdf2::PBKDF2_HMAC_SHA256, NonZeroU32::new(100).unwrap(), salt, &secret, &mut out);
            out
        };
        let signing = pbkdf2(b"authentication", 64);
        assert_eq!(b64url(&signing), "wxXDmHgmMgrcDVD8zbDLRl2yNa8jSAQgsaeIBZ4vueygpxzaTK6ZE_6X-XHvllBly6pSuFNbSxcve0ZHhVdcEA");
        let sig = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &signing), b"test");
        assert_eq!(b64url(sig.as_ref()), "AOi4HcoCJxQ4nUYxlmHB1rlcxQBn-zVjrSHz-VW7S-I");
        assert_eq!(signed(&signing, &base64::engine::general_purpose::STANDARD.encode(b"test")).unwrap(), "send-v1 AOi4HcoCJxQ4nUYxlmHB1rlcxQBn-zVjrSHz-VW7S-I");
        let sealed = unb64("UXQQ4yVf55TRk9AZtz5QCwFofRvh-HdWJyxSCQ");
        assert_eq!(open_zero_iv(&pbkdf2(b"metadata", 16), &sealed).unwrap(), b"hello world!");
    }

    #[test]
    fn send_links_are_recognised() {
        let link = |s: &str| Url::parse(s).unwrap();
        assert!(handles(&link("https://send.example/download/1a2b3c4d5e6f7a8b/#yqdlZ-tYemfogSmv7Ws5PQ")));
        assert!(handles(&link("https://send.example/download/1a2b3c4d5e/#yqdlZ-tYemfogSmv7Ws5PQ")));
        assert!(!handles(&link("https://send.example/download/1a2b3c4d5e6f7a8b/")), "no key");
        assert!(!handles(&link("https://send.example/download/notahexid/#yqdlZ-tYemfogSmv7Ws5PQ")));
        assert!(!handles(&link("https://send.example/files/1a2b3c4d5e6f7a8b/#yqdlZ-tYemfogSmv7Ws5PQ")));
        let redacted = crate::history::redact_url("https://send.example/download/1a2b3c4d5e6f7a8b/#yqdlZ-tYemfogSmv7Ws5PQ");
        assert_eq!(redacted, "https://send.example/download/1a2b3c4d5e6f7a8b/#REDACTED");
        assert!(crate::history::is_redacted(&redacted));
    }

    /// `plain` as Send uploads it: RFC 8188 records of `rs` bytes, each full but the last.
    fn encrypt(ikm: &[u8], salt: [u8; 16], rs: usize, plain: &[u8]) -> Vec<u8> {
        let cek = hkdf(&salt, ikm, b"Content-Encoding: aes128gcm\0", 16);
        let base = hkdf(&salt, ikm, b"Content-Encoding: nonce\0", 12);
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
        let mut out = [&salt[..], &(rs as u32).to_be_bytes(), &[0]].concat();
        let pieces: Vec<&[u8]> = plain.chunks(rs - 17).collect();
        for (seq, piece) in pieces.iter().enumerate() {
            let last = seq + 1 == pieces.len();
            let mut record = piece.to_vec();
            record.push(if last { 2 } else { 1 });
            if !last {
                record.resize(rs - TAG, 0);
            }
            let mut nonce: [u8; 12] = base.clone().try_into().unwrap();
            for (n, s) in nonce[8..].iter_mut().zip((seq as u32).to_be_bytes()) {
                *n ^= s;
            }
            key.seal_in_place_append_tag(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut record).unwrap();
            out.extend(record);
        }
        out
    }

    /// A Send server as its own code answers (server/middleware/auth.js, routes/download.js): a
    /// nonce per share that each authorised request replaces, and a share gone once downloaded
    /// `limit` times.
    struct Share {
        id: &'static str,
        key: [u8; 16],
        body: Vec<u8>,
        metadata: String,
        password: bool,
        nonce: Mutex<String>,
        downloads: Mutex<usize>,
        limit: usize,
    }

    async fn send_server(shares: Vec<Share>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let shares = Arc::new(shares);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let shares = shares.clone();
                tokio::spawn(async move {
                    let (mut head, mut buf) = (Vec::new(), [0u8; 4096]);
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let target = head.split(' ').nth(1).unwrap_or_default().to_string();
                    let reply = |status: &str, nonce: Option<&str>, body: &[u8]| {
                        let auth = nonce.map(|n| format!("WWW-Authenticate: send-v1 {n}\r\n")).unwrap_or_default();
                        [format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n{auth}Connection: close\r\n\r\n", body.len()).as_bytes(), body].concat()
                    };
                    let found = shares.iter().find(|s| target.ends_with(&format!("/{}", s.id)) && *s.downloads.lock().unwrap() < s.limit);
                    let Some(share) = found else {
                        let _ = socket.write_all(&reply("404 Not Found", None, b"")).await;
                        return;
                    };
                    let current = share.nonce.lock().unwrap().clone();
                    if target.starts_with("/api/exists/") {
                        let body = format!(r#"{{"requiresPassword":{}}}"#, share.password);
                        let _ = socket.write_all(&reply("200 OK", Some(&current), body.as_bytes())).await;
                        return;
                    }
                    let prefix = "authorization: send-v1 ";
                    let sent = head.lines().find_map(|l| l.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| l[prefix.len()..].trim().to_string()));
                    let expected = signed(&hkdf(&[], &share.key, b"authentication", 64), &current).unwrap();
                    if sent.map(|s| format!("send-v1 {s}")) != Some(expected) {
                        let _ = socket.write_all(&reply("401 Unauthorized", Some(&current), b"")).await;
                        return;
                    }
                    let next = base64::engine::general_purpose::STANDARD.encode((current.len() as u64 * 7919 + target.len() as u64).to_be_bytes());
                    *share.nonce.lock().unwrap() = next.clone();
                    let answer = if target.starts_with("/api/metadata/") {
                        reply("200 OK", Some(&next), format!(r#"{{"metadata":"{}","finalDownload":true,"ttl":3600}}"#, share.metadata).as_bytes())
                    } else {
                        *share.downloads.lock().unwrap() += 1;
                        reply("200 OK", Some(&next), &share.body)
                    };
                    let _ = socket.write_all(&answer).await;
                });
            }
        });
        base
    }

    fn share(id: &'static str, key: [u8; 16], name: &str, plain: &[u8], body: Option<Vec<u8>>, password: bool) -> Share {
        let meta = serde_json::json!({"name": name, "size": plain.len(), "type": "application/octet-stream", "manifest": {}});
        let meta_key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &hkdf(&[], &key, b"metadata", 16)).unwrap());
        let mut sealed = serde_json::to_vec(&meta).unwrap();
        meta_key.seal_in_place_append_tag(aead::Nonce::assume_unique_for_key([0; 12]), aead::Aad::empty(), &mut sealed).unwrap();
        Share {
            id,
            key,
            body: body.unwrap_or_else(|| encrypt(&key, [9; 16], 1024, plain)),
            metadata: b64url(&sealed),
            password,
            nonce: Mutex::new(FIRST_NONCE.replace('y', "z")),
            downloads: Mutex::new(0),
            limit: 1,
        }
    }

    /// A share downloads through the engine with its nonces and is decrypted whole, its key kept
    /// out of the history; downloaded once, it is gone. A changed record, a body cut short, a
    /// password and a wrong key each fail without leaving a file.
    #[tokio::test]
    async fn a_share_downloads_and_decrypts() {
        let plain = crate::netsim::file(100_000);
        let key = [7u8; 16];
        let mut changed = encrypt(&key, [9; 16], 1024, &plain);
        changed[21 + 1024 * 5 + 3] ^= 0x40;
        let full = encrypt(&key, [9; 16], 1024, &plain);
        // Without its last record, the record before it says more was to come.
        let cut = full[..full.len() - (TAG + 1 + plain.len() % 1007)].to_vec();
        let base = send_server(vec![
            share("aaaaaaaaaaaaaaaa", key, "report.pdf", &plain, None, false),
            share("bbbbbbbbbbbbbbbb", key, "x.bin", &plain, Some(changed), false),
            share("cccccccccccccccc", key, "x.bin", &plain, Some(cut), false),
            share("dddddddddddddddd", key, "x.bin", &plain, None, true),
            share("eeeeeeeeeeeeeeee", [8; 16], "x.bin", &plain, None, false),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let get = |id: &str| {
            let link = Url::parse(&format!("{base}/download/{id}/#{}", b64url(&key))).unwrap();
            let options = DownloadOptions { output_path: Some(dir.path().join("")), ..Default::default() };
            async move { DownloadEngine::new(vec![link], options).run(None).await }
        };
        let path = get("aaaaaaaaaaaaaaaa").await.unwrap();
        assert_eq!(path, dir.path().join("report.pdf"));
        assert_eq!(std::fs::read(&path).unwrap(), plain);
        let history = std::fs::read_to_string(crate::history::DownloadHistoryManager::default_history_path()).unwrap_or_default();
        assert!(!history.contains(&b64url(&key)), "the key stays out of the history");
        assert_eq!(get("aaaaaaaaaaaaaaaa").await.unwrap_err(), EXPIRED, "downloaded as many times as it allows");

        let changed = get("bbbbbbbbbbbbbbbb").await.unwrap_err();
        assert!(changed.contains("record 5 does not decrypt"), "{changed}");
        assert!(get("cccccccccccccccc").await.unwrap_err().contains("ended early"));
        assert!(get("dddddddddddddddd").await.unwrap_err().contains("has a password"));
        assert_eq!(get("eeeeeeeeeeeeeeee").await.unwrap_err(), REFUSED);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["report.pdf"], "nothing of a failed share is kept");
    }
}
