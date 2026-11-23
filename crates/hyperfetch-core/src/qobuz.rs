//! What Qobuz's API says of a track (title, artist, album, ISRC, length), for reading a Qobuz
//! link's songs in [`crate::catalog`].

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
    }
}
