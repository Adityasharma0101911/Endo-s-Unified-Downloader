//! Deezer audio decryption and lossless FLAC resolution.
//! Implements Blowfish-CBC chunk decryption, AES-128-ECB CDN URL generation,
//! and ISRC/metadata lookup via Deezer's public API.

use aes::cipher::{BlockEncrypt, KeyInit};
use blowfish::Blowfish;
use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use md5::{Digest, Md5};
use serde_json::Value;

/// Static Blowfish secret key used across Deezer clients.
const BF_SECRET: &[u8; 16] = b"g4el58wc0zvf9na1";

/// Static initialization vector for Deezer Blowfish-CBC stream chunks.
const BF_IV: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

/// Static AES-128 key used to encrypt the CDN mobile streaming path.
const AES_URL_KEY: &[u8; 16] = b"jo6aey6haid2Teih";

/// Audio chunk size in bytes for Deezer stream decryption.
pub const CHUNK_SIZE: usize = 2048;

/// Metadata for a resolved Deezer track.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeezerTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration: f64,
    pub isrc: Option<String>,
    pub md5_origin: Option<String>,
    pub media_version: Option<String>,
    pub filesize_flac: Option<u64>,
    pub cover: Option<String>,
}

/// Computes the 16-byte Blowfish decryption key for a Deezer song ID.
pub fn calc_bf_key(song_id: &str) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(song_id.as_bytes());
    let hash = hasher.finalize();
    let hex = format!("{hash:x}");
    let hex_bytes = hex.as_bytes();
    let mut key = [0u8; 16];
    for i in 0..16 {
        key[i] = hex_bytes[i] ^ hex_bytes[i + 16] ^ BF_SECRET[i];
    }
    key
}

/// Decrypts a single 2048-byte chunk in-place using Blowfish-CBC with Deezer's static IV.
pub fn decrypt_chunk(chunk: &mut [u8], key: &[u8; 16]) -> Result<(), String> {
    if chunk.len() != CHUNK_SIZE {
        return Ok(());
    }
    type BfCbcDec = cbc::Decryptor<Blowfish>;
    let decryptor = BfCbcDec::new_from_slices(key, &BF_IV)
        .map_err(|e| format!("Invalid Blowfish key/iv: {e}"))?;
    decryptor
        .decrypt_padded_mut::<NoPadding>(chunk)
        .map_err(|e| format!("Blowfish-CBC chunk decryption failed: {e:?}"))?;
    Ok(())
}

/// Decrypts a Deezer stream from reader to writer, where every 3rd chunk (i % 3 == 0)
/// of 2048 bytes is decrypted with Blowfish-CBC, and all other bytes are passed through untouched.
pub fn decrypt_stream<R: std::io::Read, W: std::io::Write>(
    mut reader: R,
    mut writer: W,
    key: &[u8; 16],
) -> std::io::Result<u64> {
    let mut chunk_index = 0usize;
    let mut total_written = 0u64;
    let mut buf = vec![0u8; CHUNK_SIZE];

    loop {
        let mut read = 0;
        while read < CHUNK_SIZE {
            let n = reader.read(&mut buf[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }

        if read == 0 {
            break;
        }

        if read == CHUNK_SIZE && chunk_index % 3 == 0 {
            decrypt_chunk(&mut buf[..read], key)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        }

        writer.write_all(&buf[..read])?;
        total_written += read as u64;
        chunk_index += 1;
    }

    writer.flush()?;
    Ok(total_written)
}

/// Builds the CDN streaming URL for a Deezer track given its md5_origin hash, song ID,
/// format (e.g. "9" for FLAC, "3" for 320k MP3, "1" for 128k MP3), and media version.
pub fn get_track_url(
    md5_origin: &str,
    song_id: &str,
    format_id: &str,
    media_version: &str,
) -> Result<String, String> {
    if md5_origin.is_empty() {
        return Err("md5_origin is empty".to_string());
    }
    let shard = &md5_origin[..1];
    let sep = '\u{00a4}'; // 0xA4 delimiter
    let step1 = format!("{md5_origin}{sep}{format_id}{sep}{song_id}{sep}{media_version}");

    let mut hasher = Md5::new();
    hasher.update(step1.as_bytes());
    let hash = format!("{:x}", hasher.finalize());

    let step2 = format!("{hash}{sep}{step1}{sep}");
    let mut padded = step2.into_bytes();
    // Pad to multiple of 16 bytes, at least 80 bytes
    let target_len = if padded.len() < 80 {
        80
    } else {
        (padded.len() + 15) / 16 * 16
    };
    padded.resize(target_len, b' ');

    let cipher = aes::Aes128::new(AES_URL_KEY.into());
    for block in padded.chunks_exact_mut(16) {
        let block_ref = aes::cipher::generic_array::GenericArray::from_mut_slice(block);
        cipher.encrypt_block(block_ref);
    }

    let encrypted_hex: String = padded.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("https://e-cdns-proxy-{shard}.dzcdn.net/mobile/{format_id}/{encrypted_hex}"))
}

/// Parses a Deezer track JSON object into [`DeezerTrack`].
pub fn parse_track_json(track: &Value) -> Option<DeezerTrack> {
    let id = track["id"].as_u64()?.to_string();
    let title = track["title"].as_str()?.to_string();
    let artist = track["artist"]["name"].as_str().unwrap_or("Unknown Artist").to_string();
    let album = track["album"]["title"].as_str().map(String::from);
    let duration = track["duration"].as_f64().unwrap_or(0.0);
    let isrc = track["isrc"].as_str().map(String::from);
    let md5_origin = track["md5_origin"].as_str().map(String::from);
    let media_version = track["media_version"].as_str().or_else(|| track["media_version"].as_u64().map(|_| "4")).map(String::from);
    let filesize_flac = track["filesize_flac"].as_u64();
    let cover = track["album"]["cover_xl"]
        .as_str()
        .or_else(|| track["album"]["cover_big"].as_str())
        .or_else(|| track["album"]["cover_medium"].as_str())
        .map(String::from);

    Some(DeezerTrack {
        id,
        title,
        artist,
        album,
        duration,
        isrc,
        md5_origin,
        media_version,
        filesize_flac,
        cover,
    })
}

/// Resolves an ISRC to a Deezer track via Deezer's public API.
pub async fn resolve_isrc(http: &reqwest::Client, isrc: &str) -> Result<Option<DeezerTrack>, String> {
    let clean_isrc = isrc.trim().to_uppercase();
    if clean_isrc.is_empty() {
        return Ok(None);
    }
    let url = format!("https://api.deezer.com/track/isrc:{clean_isrc}");
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Deezer ISRC request failed: {e}"))?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let bytes = resp.bytes().await.map_err(|e| format!("Deezer read failed: {e}"))?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|e| format!("Deezer invalid JSON: {e}"))?;
    if json["error"].is_object() {
        return Ok(None);
    }

    Ok(parse_track_json(&json))
}

/// Searches Deezer for a track by artist and title via Deezer's public API.
pub async fn search_track(http: &reqwest::Client, artist: &str, title: &str) -> Result<Option<DeezerTrack>, String> {
    let query = format!("{artist} {title}").trim().to_string();
    if query.is_empty() {
        return Ok(None);
    }
    let mut url = url::Url::parse("https://api.deezer.com/search/track").map_err(|e| e.to_string())?;
    url.query_pairs_mut().append_pair("q", &query);

    let resp = http
        .get(url.as_str())
        .send()
        .await
        .map_err(|e| format!("Deezer search request failed: {e}"))?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let bytes = resp.bytes().await.map_err(|e| format!("Deezer read failed: {e}"))?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|e| format!("Deezer invalid JSON: {e}"))?;
    let data = json["data"].as_array();
    if let Some(items) = data {
        if let Some(first) = items.first() {
            return Ok(parse_track_json(first));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calc_bf_key() {
        // Track ID 3135556
        let key = calc_bf_key("3135556");
        assert_eq!(key.len(), 16);
        // Verify deterministic key derivation
        let key2 = calc_bf_key("3135556");
        assert_eq!(key, key2);

        // Different track ID gives different key
        let key3 = calc_bf_key("3135557");
        assert_ne!(key, key3);
    }

    #[test]
    fn test_decrypt_chunk_roundtrip() {
        use cbc::cipher::BlockEncryptMut;
        type BfCbcEnc = cbc::Encryptor<Blowfish>;

        let key = calc_bf_key("12345678");
        let original_data: Vec<u8> = (0..CHUNK_SIZE).map(|i| (i % 256) as u8).collect();

        // Encrypt with Blowfish-CBC
        let mut encrypted = original_data.clone();
        let encryptor = BfCbcEnc::new_from_slices(&key, &BF_IV).unwrap();
        let len = encrypted.len();
        encryptor.encrypt_padded_mut::<NoPadding>(&mut encrypted, len).unwrap();

        assert_ne!(encrypted, original_data);

        // Decrypt with decrypt_chunk
        decrypt_chunk(&mut encrypted, &key).unwrap();
        assert_eq!(encrypted, original_data);
    }

    #[test]
    fn test_decrypt_stream() {
        use cbc::cipher::BlockEncryptMut;
        type BfCbcEnc = cbc::Encryptor<Blowfish>;

        let key = calc_bf_key("87654321");
        // Create 3 full chunks: chunk 0 encrypted, chunk 1 clear, chunk 2 clear
        let mut stream_data = Vec::with_capacity(CHUNK_SIZE * 3);

        let chunk0: Vec<u8> = (0..CHUNK_SIZE).map(|i| (i % 250) as u8).collect();
        let mut chunk0_enc = chunk0.clone();
        let encryptor = BfCbcEnc::new_from_slices(&key, &BF_IV).unwrap();
        let len = chunk0_enc.len();
        encryptor.encrypt_padded_mut::<NoPadding>(&mut chunk0_enc, len).unwrap();

        let chunk1: Vec<u8> = (0..CHUNK_SIZE).map(|i| ((i + 1) % 250) as u8).collect();
        let chunk2: Vec<u8> = (0..CHUNK_SIZE).map(|i| ((i + 2) % 250) as u8).collect();

        stream_data.extend_from_slice(&chunk0_enc);
        stream_data.extend_from_slice(&chunk1);
        stream_data.extend_from_slice(&chunk2);

        let mut output = Vec::new();
        let written = decrypt_stream(&stream_data[..], &mut output, &key).unwrap();
        assert_eq!(written, (CHUNK_SIZE * 3) as u64);

        // Verify chunk 0 was decrypted back to original
        assert_eq!(&output[..CHUNK_SIZE], &chunk0[..]);
        // Verify chunk 1 and 2 stayed unchanged
        assert_eq!(&output[CHUNK_SIZE..CHUNK_SIZE * 2], &chunk1[..]);
        assert_eq!(&output[CHUNK_SIZE * 2..], &chunk2[..]);
    }

    #[test]
    fn test_get_track_url_format() {
        let md5 = "71eebebcd1234567890abcdef1234567";
        let url = get_track_url(md5, "3135556", "9", "4").unwrap();
        assert!(url.starts_with("https://e-cdns-proxy-7.dzcdn.net/mobile/9/"));
        assert!(url.len() > 50);
    }
}
