//! Stream specification and media metadata across HLS, DASH, MSS, and IPTV streams.

use std::fmt;
use serde::{Deserialize, Serialize};

/// High-level media category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MediaType {
    Video,
    Audio,
    Subtitles,
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Video => write!(f, "VIDEO"),
            Self::Audio => write!(f, "AUDIO"),
            Self::Subtitles => write!(f, "SUBTITLES"),
        }
    }
}

/// A media segment belonging to a stream rendition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaSegment {
    /// Zero-based sequence index.
    pub index: usize,
    /// Absolute or relative URL.
    pub url: String,
    /// Duration of the segment in seconds.
    pub duration_secs: f64,
    /// Start time in seconds along the stream timeline.
    pub start_time_secs: f64,
    /// Optional byte range (start, length or start, end).
    pub byte_range: Option<(u64, u64)>,
    /// Optional encryption method and key information.
    pub enc_info: Option<SegmentEncryption>,
}

/// Encryption parameters for a segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEncryption {
    pub method: String,
    pub key_url: Option<String>,
    pub key_bytes: Option<Vec<u8>>,
    pub iv: Option<Vec<u8>>,
    pub key_id: Option<String>,
}

/// DRM Protection Information (CENC, Widevine, PlayReady, ClearKey).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmKeyInfo {
    pub system_id: String,
    pub default_kid: Option<String>,
    pub pssh_base64: Option<String>,
}

/// Common representation of a stream track across protocols.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSpec {
    pub id: Option<String>,
    pub media_type: Option<MediaType>,
    pub group_id: Option<String>,
    pub group_title: Option<String>,
    pub language: Option<String>,
    pub name: Option<String>,
    pub codecs: Option<String>,
    pub resolution: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    pub channels: Option<String>,
    pub bandwidth: u64,
    pub video_range: Option<String>,
    pub url: String,
    pub original_url: Option<String>,
    pub period_id: Option<String>,
    pub segments_count: usize,
    pub total_duration_secs: f64,
    pub roles: Vec<String>,
    pub segments: Vec<MediaSegment>,
    pub init_segment: Option<MediaSegment>,
    pub drm_info: Vec<DrmKeyInfo>,
}

impl StreamSpec {
    pub fn new(url: String) -> Self {
        Self {
            id: None,
            media_type: None,
            group_id: None,
            group_title: None,
            language: None,
            name: None,
            codecs: None,
            resolution: None,
            width: None,
            height: None,
            frame_rate: None,
            channels: None,
            bandwidth: 0,
            video_range: None,
            url,
            original_url: None,
            period_id: None,
            segments_count: 0,
            total_duration_secs: 0.0,
            roles: Vec::new(),
            segments: Vec::new(),
            init_segment: None,
            drm_info: Vec::new(),
        }
    }

    /// Check if this stream has a specific role (case-insensitive).
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r.eq_ignore_ascii_case(role))
    }

    /// Extract base language (e.g., "en-US" -> "en", "zh-CN" -> "zh").
    pub fn base_language(&self) -> Option<String> {
        self.language.as_ref().map(|lang| {
            let s = lang.trim();
            if let Some((base, _)) = s.split_once(['-', '_']) {
                base.to_ascii_lowercase()
            } else {
                s.to_ascii_lowercase()
            }
        })
    }

    /// Display string summarizing track details.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(t) = self.media_type {
            parts.push(t.to_string());
        }
        if let Some(ref res) = self.resolution {
            parts.push(res.clone());
        }
        if self.bandwidth > 0 {
            if self.bandwidth >= 1_000_000 {
                parts.push(format!("{:.1} Mbps", self.bandwidth as f64 / 1_000_000.0));
            } else {
                parts.push(format!("{} Kbps", self.bandwidth / 1000));
            }
        }
        if let Some(ref codecs) = self.codecs {
            parts.push(codecs.clone());
        }
        if let Some(ref lang) = self.language {
            parts.push(format!("lang: {lang}"));
        }
        if let Some(ref ch) = self.channels {
            parts.push(format!("{ch} ch"));
        }
        if let Some(ref name) = self.name {
            parts.push(format!("\"{name}\""));
        }
        if let Some(ref vr) = self.video_range {
            parts.push(vr.clone());
        }
        if parts.is_empty() {
            self.url.clone()
        } else {
            parts.join(" | ")
        }
    }
}
