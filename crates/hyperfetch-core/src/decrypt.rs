//! Decryption engine and DRM key handling.
//! Supports CENC/ClearKey with external tools (mp4decrypt, shaka-packager, ffmpeg)
//! and custom HLS ciphers (AES-128-CBC, AES-128-ECB, ChaCha20).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use cbc::Decryptor;
use aes::Aes128;
use chacha20::ChaCha20;
use chacha20::cipher::StreamCipher;

type Aes128CbcDec = Decryptor<Aes128>;

/// Supported decryption engine tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecryptEngine {
    #[default]
    Mp4Decrypt,
    ShakaPackager,
    FFmpeg,
}

impl DecryptEngine {
    pub fn parse(s: &str) -> Option<Self> {
        let upper = s.to_ascii_uppercase();
        match upper.as_str() {
            "MP4DECRYPT" => Some(Self::Mp4Decrypt),
            "SHAKA_PACKAGER" | "SHAKA" | "PACKAGER" => Some(Self::ShakaPackager),
            "FFMPEG" => Some(Self::FFmpeg),
            _ => None,
        }
    }
}

/// Custom HLS encryption methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CustomHlsMethod {
    #[default]
    None,
    Aes128,
    Aes128Ecb,
    Cenc,
    ChaCha20,
    SampleAes,
    SampleAesCtr,
}

impl CustomHlsMethod {
    pub fn parse(s: &str) -> Option<Self> {
        let upper = s.to_ascii_uppercase().replace('-', "_");
        match upper.as_str() {
            "NONE" => Some(Self::None),
            "AES_128" | "AES128" => Some(Self::Aes128),
            "AES_128_ECB" | "AES128ECB" => Some(Self::Aes128Ecb),
            "CENC" => Some(Self::Cenc),
            "CHACHA20" => Some(Self::ChaCha20),
            "SAMPLE_AES" | "SAMPLEAES" => Some(Self::SampleAes),
            "SAMPLE_AES_CTR" => Some(Self::SampleAesCtr),
            _ => None,
        }
    }
}

/// Scope of custom HLS parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CustomHlsScope {
    #[default]
    All,
    Video,
    Audio,
}

impl CustomHlsScope {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "ALL" => Some(Self::All),
            "VIDEO" => Some(Self::Video),
            "AUDIO" => Some(Self::Audio),
            _ => None,
        }
    }
}

/// A parsed KID:KEY pair (both formatted as 32 hex chars or 16-byte raw).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmKeyPair {
    pub kid: Option<String>,
    pub key: String,
    pub key_bytes: Vec<u8>,
    pub kid_bytes: Option<Vec<u8>>,
}

/// Decryption configuration.
#[derive(Debug, Clone, Default)]
pub struct DecryptionConfig {
    pub keys: Vec<DrmKeyPair>,
    pub engine: DecryptEngine,
    pub binary_path: Option<PathBuf>,
    pub custom_hls_method: Option<CustomHlsMethod>,
    pub custom_hls_key: Option<Vec<u8>>,
    pub custom_hls_iv: Option<Vec<u8>>,
    pub custom_hls_scope: CustomHlsScope,
}

impl DecryptionConfig {
    /// Add a key in "KID:KEY" or "KEY" format (hex or base64).
    pub fn add_key_str(&mut self, s: &str) -> Result<(), String> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        if let Some((kid_part, key_part)) = trimmed.split_once(':') {
            let kid_bytes = parse_hex_or_base64(kid_part.trim())?;
            let key_bytes = parse_hex_or_base64(key_part.trim())?;
            self.keys.push(DrmKeyPair {
                kid: Some(hex::encode(&kid_bytes)),
                key: hex::encode(&key_bytes),
                key_bytes,
                kid_bytes: Some(kid_bytes),
            });
        } else {
            let key_bytes = parse_hex_or_base64(trimmed)?;
            self.keys.push(DrmKeyPair {
                kid: None,
                key: hex::encode(&key_bytes),
                key_bytes,
                kid_bytes: None,
            });
        }
        Ok(())
    }

    /// Load keys from a text file.
    pub fn load_key_file(&mut self, path: &Path) -> Result<(), String> {
        let text = fs::read_to_string(path).map_err(|e| format!("cannot read key file: {e}"))?;
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            self.add_key_str(line)?;
        }
        Ok(())
    }
}

/// Parse a string which may be hex or base64.
pub fn parse_hex_or_base64(s: &str) -> Result<Vec<u8>, String> {
    let clean = s.trim().replace('-', "");
    // Try hex first if valid hex characters and even length
    if clean.chars().all(|c| c.is_ascii_hexdigit()) && clean.len() % 2 == 0 {
        if let Ok(bytes) = hex_decode(&clean) {
            return Ok(bytes);
        }
    }
    // Try base64
    use base64::Engine;
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(clean.trim_end_matches('=')) {
        return Ok(bytes);
    }
    if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE.decode(clean.trim_end_matches('=')) {
        return Ok(bytes);
    }
    // If it is a file path
    if let Ok(bytes) = fs::read(s) {
        return Ok(bytes);
    }

    Err(format!("cannot parse key or IV from '{s}'"))
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(s, "{:02x}", b);
        }
        s
    }
}

/// Decrypt an MP4 file using mp4decrypt, shaka-packager, or ffmpeg.
pub fn decrypt_mp4_file(
    engine: DecryptEngine,
    bin_path: Option<&Path>,
    keys: &[DrmKeyPair],
    input_path: &Path,
    output_path: &Path,
) -> Result<(), String> {
    if keys.is_empty() {
        return Err("no decryption keys provided".to_string());
    }

    match engine {
        DecryptEngine::Mp4Decrypt => {
            let bin = bin_path.unwrap_or(Path::new("mp4decrypt"));
            let mut cmd = Command::new(bin);
            for k in keys {
                if let Some(ref kid) = k.kid {
                    cmd.arg("--key").arg(format!("{kid}:{}", k.key));
                } else {
                    cmd.arg("--key").arg(format!("1:{}", k.key));
                }
            }
            cmd.arg(input_path).arg(output_path);

            let status = cmd.status().map_err(|e| format!("failed to invoke mp4decrypt: {e}"))?;
            if !status.success() {
                return Err(format!("mp4decrypt exited with status {status}"));
            }
        }
        DecryptEngine::ShakaPackager => {
            let bin = bin_path.unwrap_or(Path::new("packager"));
            let mut cmd = Command::new(bin);
            cmd.arg("--quiet").arg("--enable_raw_key_decryption");
            cmd.arg(format!("input={},stream=0,output={}", input_path.display(), output_path.display()));

            let mut keys_arg = Vec::new();
            for k in keys {
                let kid = k.kid.as_deref().unwrap_or("00000000000000000000000000000000");
                keys_arg.push(format!("key_id={kid}:key={}", k.key));
            }
            cmd.arg("--keys").arg(keys_arg.join(","));

            let status = cmd.status().map_err(|e| format!("failed to invoke shaka-packager: {e}"))?;
            if !status.success() {
                return Err(format!("shaka-packager exited with status {status}"));
            }
        }
        DecryptEngine::FFmpeg => {
            let bin = bin_path.unwrap_or(Path::new("ffmpeg"));
            let mut cmd = Command::new(bin);
            cmd.arg("-y");
            if let Some(first_key) = keys.first() {
                cmd.arg("-decryption_key").arg(&first_key.key);
            }
            cmd.arg("-i").arg(input_path);
            cmd.arg("-c").arg("copy");
            cmd.arg(output_path);

            let status = cmd.status().map_err(|e| format!("failed to invoke ffmpeg decryption: {e}"))?;
            if !status.success() {
                return Err(format!("ffmpeg decryption exited with status {status}"));
            }
        }
    }

    Ok(())
}

/// Decrypt HLS segment in-memory with AES-128-CBC.
pub fn decrypt_aes_128_cbc(ciphertext: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>, String> {
    if ciphertext.is_empty() {
        return Ok(Vec::new());
    }
    if ciphertext.len() % 16 != 0 {
        return Err("ciphertext length is not a multiple of 16".to_string());
    }

    let decryptor = Aes128CbcDec::new(key.into(), iv.into());
    let mut buf = ciphertext.to_vec();
    match decryptor.decrypt_padded_mut::<Pkcs7>(&mut buf) {
        Ok(plaintext) => Ok(plaintext.to_vec()),
        Err(e) => {
            // Some non-standard HLS streams omit PKCS7 padding; decrypt raw blocks if unpadded
            let decryptor_raw = Aes128CbcDec::new(key.into(), iv.into());
            let mut raw_buf = ciphertext.to_vec();
            use aes::cipher::block_padding::NoPadding;
            decryptor_raw
                .decrypt_padded_mut::<NoPadding>(&mut raw_buf)
                .map(|pt| pt.to_vec())
                .map_err(|_| format!("AES-128-CBC decryption failed: {e:?}"))
        }
    }
}

/// Decrypt HLS segment in-memory with ChaCha20.
pub fn decrypt_chacha20(ciphertext: &[u8], key: &[u8; 32], nonce: &[u8; 12]) -> Vec<u8> {
    let mut cipher = ChaCha20::new(key.into(), nonce.into());
    let mut buffer = ciphertext.to_vec();
    cipher.apply_keystream(&mut buffer);
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_str() {
        let mut config = DecryptionConfig::default();
        config.add_key_str("eb676abb88e14366a4fb98ce33fb10bc:8563777d1000f07da1b0a090f40a18ca").unwrap();
        assert_eq!(config.keys.len(), 1);
        assert_eq!(config.keys[0].kid, Some("eb676abb88e14366a4fb98ce33fb10bc".into()));
        assert_eq!(config.keys[0].key, "8563777d1000f07da1b0a090f40a18ca");
    }

    #[test]
    fn test_aes_128_cbc_roundtrip() {
        use cbc::Encryptor;
        use aes::cipher::BlockEncryptMut;
        type Aes128CbcEnc = Encryptor<Aes128>;

        let key = [0x42u8; 16];
        let iv = [0x24u8; 16];
        let plaintext = b"Hello, MPEG-DASH / HLS stream encryption testing!";

        let mut buf = vec![0u8; plaintext.len() + 16];
        buf[..plaintext.len()].copy_from_slice(plaintext);
        let encryptor = Aes128CbcEnc::new(&key.into(), &iv.into());
        let ciphertext = encryptor.encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len()).unwrap();

        let decrypted = decrypt_aes_128_cbc(ciphertext, &key, &iv).unwrap();
        assert_eq!(decrypted, plaintext);
    }
}
