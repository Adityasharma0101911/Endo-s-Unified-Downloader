//! Qobuz catalog resolution and 24-bit / 192kHz Hi-Res lossless FLAC audio search.
//! Interfaces with Qobuz's public API to resolve ISRCs, track metadata, bit-depth, and sample rates.

use reqwest::Client;
use serde_json::Value;

/// Well-known public Qobuz application identifiers used for metadata and catalog lookup.
pub const QOBUZ_APP_ID: &str = "712109809";
pub const QOBUZ_APP_ID_FALLBACK: &str = "100000000";

/// Metadata for a resolved Qobuz track, including Hi-Res audio specifications.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QobuzTrack {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration: f64,
    pub isrc: Option<String>,
    pub hires: bool,
    pub bit_depth: Option<u32>,
    pub sampling_rate: Option<f64>,
    pub cover: Option<String>,
}

impl QobuzTrack {
    /// Description of the audio quality tier (e.g., "24-bit / 192.0 kHz Hi-Res FLAC" or "16-bit / 44.1 kHz FLAC").
    pub fn quality_description(&self) -> String {
        let bit = self.bit_depth.unwrap_or(if self.hires { 24 } else { 16 });
        let rate = self.sampling_rate.unwrap_or(44.1);
        if self.hires || bit > 16 {
            format!("{bit}-bit / {rate:.1} kHz Hi-Res FLAC")
        } else {
            format!("{bit}-bit / {rate:.1} kHz Lossless FLAC")
        }
    }
}

/// Parses a Qobuz track JSON representation into [`QobuzTrack`].
pub fn parse_track_json(track: &Value) -> Option<QobuzTrack> {
    let id = track["id"].as_u64()?;
    let title = track["title"].as_str()?.to_string();

    let artist = track["performer"]["name"]
        .as_str()
        .or_else(|| track["album"]["artist"]["name"].as_str())
        .or_else(|| track["artist"]["name"].as_str())
        .unwrap_or("Unknown Artist")
        .to_string();

    let album = track["album"]["title"].as_str().map(String::from);
    let duration = track["duration"].as_f64().unwrap_or(0.0);
    let isrc = track["isrc"].as_str().map(String::from);

    let hires = track["hires"].as_bool().unwrap_or(false)
        || track["hires_streamable"].as_bool().unwrap_or(false);

    let bit_depth = track["maximum_bit_depth"]
        .as_u64()
        .or_else(|| track["audio_info"]["bit_depth"].as_u64())
        .map(|b| b as u32);

    let sampling_rate = track["maximum_sampling_rate"]
        .as_f64()
        .or_else(|| track["audio_info"]["sampling_rate"].as_f64());

    let cover = track["album"]["image"]["large"]
        .as_str()
        .or_else(|| track["album"]["image"]["extralarge"].as_str())
        .or_else(|| track["album"]["image"]["small"].as_str())
        .or_else(|| track["album"]["cover"].as_str())
        .map(String::from);

    Some(QobuzTrack {
        id,
        title,
        artist,
        album,
        duration,
        isrc,
        hires: hires || bit_depth.is_some_and(|b| b > 16),
        bit_depth,
        sampling_rate,
        cover,
    })
}

/// Resolves a track on Qobuz by its track ID.
pub async fn resolve_track(http: &Client, track_id: u64) -> Result<Option<QobuzTrack>, String> {
    let url = format!(
        "https://www.qobuz.com/api.json/0.2/track/get?track_id={track_id}&app_id={QOBUZ_APP_ID}"
    );

    let resp = http
        .get(&url)
        .header(reqwest::header::USER_AGENT, crate::resolver::APP_USER_AGENT)
        .send()
        .await
        .map_err(|e| format!("Qobuz track request failed: {e}"))?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let bytes = resp.bytes().await.map_err(|e| format!("Qobuz read failed: {e}"))?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|e| format!("Qobuz invalid JSON: {e}"))?;

    if json["status"].as_str() == Some("error") {
        return Ok(None);
    }

    Ok(parse_track_json(&json))
}

/// Resolves an ISRC to a Qobuz track via Qobuz API.
pub async fn resolve_isrc(http: &Client, isrc: &str) -> Result<Option<QobuzTrack>, String> {
    let clean_isrc = isrc.trim().to_uppercase();
    if clean_isrc.is_empty() {
        return Ok(None);
    }

    // Attempt direct track/get by isrc
    let direct_url = format!(
        "https://www.qobuz.com/api.json/0.2/track/get?isrc={clean_isrc}&app_id={QOBUZ_APP_ID}"
    );

    if let Ok(resp) = http
        .get(&direct_url)
        .header(reqwest::header::USER_AGENT, crate::resolver::APP_USER_AGENT)
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(bytes) = resp.bytes().await {
                if let Ok(json) = serde_json::from_slice::<Value>(&bytes) {
                    if json["status"].as_str() != Some("error") {
                        if let Some(track) = parse_track_json(&json) {
                            return Ok(Some(track));
                        }
                    }
                }
            }
        }
    }

    // Fall back to catalog/search with isrc query
    let search_url = format!(
        "https://www.qobuz.com/api.json/0.2/catalog/search?query={clean_isrc}&type=tracks&limit=5&app_id={QOBUZ_APP_ID}"
    );

    let resp = http
        .get(&search_url)
        .header(reqwest::header::USER_AGENT, crate::resolver::APP_USER_AGENT)
        .send()
        .await
        .map_err(|e| format!("Qobuz search request failed: {e}"))?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let bytes = resp.bytes().await.map_err(|e| format!("Qobuz read failed: {e}"))?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|e| format!("Qobuz invalid JSON: {e}"))?;

    if let Some(items) = json["tracks"]["items"].as_array() {
        for item in items {
            if let Some(track) = parse_track_json(item) {
                if track.isrc.as_deref().map(|s| s.eq_ignore_ascii_case(&clean_isrc)).unwrap_or(false) {
                    return Ok(Some(track));
                }
            }
        }
        if let Some(first) = items.first() {
            return Ok(parse_track_json(first));
        }
    }

    Ok(None)
}

/// Searches Qobuz for a track by artist and title.
pub async fn search_track(http: &Client, artist: &str, title: &str) -> Result<Option<QobuzTrack>, String> {
    let query = format!("{artist} {title}").trim().to_string();
    if query.is_empty() {
        return Ok(None);
    }

    let mut url = url::Url::parse("https://www.qobuz.com/api.json/0.2/catalog/search")
        .map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("query", &query)
        .append_pair("type", "tracks")
        .append_pair("limit", "5")
        .append_pair("app_id", QOBUZ_APP_ID);

    let resp = http
        .get(url.as_str())
        .header(reqwest::header::USER_AGENT, crate::resolver::APP_USER_AGENT)
        .send()
        .await
        .map_err(|e| format!("Qobuz search request failed: {e}"))?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let bytes = resp.bytes().await.map_err(|e| format!("Qobuz read failed: {e}"))?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|e| format!("Qobuz invalid JSON: {e}"))?;

    if let Some(items) = json["tracks"]["items"].as_array() {
        if let Some(first) = items.first() {
            return Ok(parse_track_json(first));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_qobuz_hires_track() {
        let sample = json!({
            "id": 12345678,
            "title": "Get Lucky",
            "duration": 248.5,
            "isrc": "FR6V81300040",
            "hires": true,
            "maximum_bit_depth": 24,
            "maximum_sampling_rate": 88.2,
            "performer": {
                "name": "Daft Punk"
            },
            "album": {
                "title": "Random Access Memories",
                "image": {
                    "large": "https://static.qobuz.com/images/covers/large.jpg"
                }
            }
        });

        let track = parse_track_json(&sample).unwrap();
        assert_eq!(track.id, 12345678);
        assert_eq!(track.title, "Get Lucky");
        assert_eq!(track.artist, "Daft Punk");
        assert_eq!(track.album.as_deref(), Some("Random Access Memories"));
        assert_eq!(track.duration, 248.5);
        assert_eq!(track.isrc.as_deref(), Some("FR6V81300040"));
        assert!(track.hires);
        assert_eq!(track.bit_depth, Some(24));
        assert_eq!(track.sampling_rate, Some(88.2));
        assert_eq!(track.quality_description(), "24-bit / 88.2 kHz Hi-Res FLAC");
        assert_eq!(
            track.cover.as_deref(),
            Some("https://static.qobuz.com/images/covers/large.jpg")
        );
    }

    #[test]
    fn test_parse_qobuz_cd_quality_track() {
        let sample = json!({
            "id": 99999,
            "title": "Standard Track",
            "duration": 180.0,
            "isrc": "US1234567890",
            "hires": false,
            "maximum_bit_depth": 16,
            "maximum_sampling_rate": 44.1,
            "performer": {
                "name": "Artist Name"
            }
        });

        let track = parse_track_json(&sample).unwrap();
        assert_eq!(track.id, 99999);
        assert!(!track.hires);
        assert_eq!(track.quality_description(), "16-bit / 44.1 kHz Lossless FLAC");
    }
}
