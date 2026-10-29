//! Native Microsoft Smooth Streaming (MSS .ism/Manifest) parser.

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use url::Url;

use crate::stream_spec::{DrmKeyInfo, MediaSegment, MediaType, StreamSpec};

/// Parse a Microsoft Smooth Streaming manifest from XML.
pub fn parse_mss_manifest(xml: &str, manifest_url: &str) -> Result<Vec<StreamSpec>, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let base_url = Url::parse(manifest_url).map_err(|e| format!("invalid manifest URL '{manifest_url}': {e}"))?;

    let mut global_timescale: u64 = 10_000_000;
    let mut _global_duration: u64 = 0;
    let mut _is_live = false;

    let mut drm_info = Vec::new();

    let mut streams = Vec::new();
    let mut current_stream_index: Option<StreamIndexContext> = None;
    let mut current_chunks: Vec<MssChunk> = Vec::new();
    let mut current_time: u64 = 0;

    let mut text_buf = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = tag_name.split(':').last().unwrap_or(&tag_name);

                match local {
                    "SmoothStreamingMedia" => {
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            if key.eq_ignore_ascii_case("TimeScale") {
                                global_timescale = val.parse().unwrap_or(10_000_000);
                            } else if key.eq_ignore_ascii_case("Duration") {
                                _global_duration = val.parse().unwrap_or(0);
                            } else if key.eq_ignore_ascii_case("IsLive") {
                                _is_live = val.eq_ignore_ascii_case("TRUE");
                            }
                        }
                    }
                    "ProtectionHeader" => {
                        let mut sys_id = "9A04F079-9840-4286-AB92-E65BE0885F95".to_string();
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            if key.eq_ignore_ascii_case("SystemID") {
                                sys_id = val;
                            }
                        }
                        // PSSH content will be collected in text
                        let drm = DrmKeyInfo {
                            system_id: sys_id,
                            default_kid: None,
                            pssh_base64: None,
                        };
                        drm_info.push(drm);
                    }
                    "StreamIndex" => {
                        let mut ctx = StreamIndexContext::default();
                        ctx.timescale = global_timescale;
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.to_ascii_lowercase().as_str() {
                                "type" => ctx.stream_type = val,
                                "name" => ctx.name = Some(val),
                                "subtype" => ctx.subtype = Some(val),
                                "url" => ctx.url_pattern = val,
                                "language" => ctx.language = Some(val),
                                "timescale" => ctx.timescale = val.parse().unwrap_or(global_timescale),
                                _ => {}
                            }
                        }
                        current_stream_index = Some(ctx);
                        current_chunks.clear();
                        current_time = 0;
                    }
                    "QualityLevel" => {
                        let mut ql = QualityLevelContext::default();
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.to_ascii_lowercase().as_str() {
                                "index" => ql.index = val.parse().unwrap_or(0),
                                "bitrate" => ql.bitrate = val.parse().unwrap_or(0),
                                "fourcc" => ql.fourcc = val,
                                "maxwidth" => ql.width = val.parse().ok(),
                                "maxheight" => ql.height = val.parse().ok(),
                                "samplingrate" => ql.sampling_rate = val.parse().ok(),
                                "channels" => ql.channels = val.parse().ok(),
                                "codecprivatedata" => ql.codec_private_data = Some(val),
                                _ => {}
                            }
                        }
                        if let Some(ref mut idx) = current_stream_index {
                            idx.quality_levels.push(ql);
                        }
                    }
                    "c" => {
                        let mut dur: u64 = 0;
                        let mut time: Option<u64> = None;
                        let mut repeat: i64 = 0;
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.as_str() {
                                "d" => dur = val.parse().unwrap_or(0),
                                "t" => time = val.parse().ok(),
                                "r" => repeat = val.parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                        if let Some(t) = time {
                            current_time = t;
                        }
                        let r_count = if repeat < 0 { 0 } else { repeat as usize };
                        for _ in 0..=r_count {
                            current_chunks.push(MssChunk {
                                start_time: current_time,
                                duration: dur,
                            });
                            current_time += dur;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                text_buf = e.unescape().unwrap_or_default().to_string();
            }
            Ok(Event::End(ref e)) => {
                let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local = tag_name.split(':').last().unwrap_or(&tag_name);

                if local == "ProtectionHeader" {
                    if let Some(last) = drm_info.last_mut() {
                        last.pssh_base64 = Some(text_buf.trim().to_string());
                    }
                } else if local == "StreamIndex" {
                    if let Some(stream_idx) = current_stream_index.take() {
                        let stream_type = stream_idx.stream_type.to_ascii_lowercase();
                        let media_type = if stream_type.contains("video") {
                            Some(MediaType::Video)
                        } else if stream_type.contains("audio") {
                            Some(MediaType::Audio)
                        } else if stream_type.contains("text") {
                            Some(MediaType::Subtitles)
                        } else {
                            None
                        };

                        let timescale = if stream_idx.timescale == 0 { 10_000_000.0 } else { stream_idx.timescale as f64 };

                        for ql in stream_idx.quality_levels {
                            let mut segments = Vec::new();
                            for (i, chunk) in current_chunks.iter().enumerate() {
                                let start_time_secs = chunk.start_time as f64 / timescale;
                                let duration_secs = chunk.duration as f64 / timescale;

                                let mut rel_url = stream_idx.url_pattern.clone();
                                rel_url = rel_url.replace("{bitrate}", &ql.bitrate.to_string());
                                rel_url = rel_url.replace("{Bitrate}", &ql.bitrate.to_string());
                                rel_url = rel_url.replace("{start time}", &chunk.start_time.to_string());
                                rel_url = rel_url.replace("{start_time}", &chunk.start_time.to_string());

                                if let Ok(seg_url) = base_url.join(&rel_url) {
                                    segments.push(MediaSegment {
                                        index: i,
                                        url: seg_url.to_string(),
                                        duration_secs,
                                        start_time_secs,
                                        byte_range: None,
                                        enc_info: None,
                                    });
                                }
                            }

                            let resolution = match (ql.width, ql.height) {
                                (Some(w), Some(h)) => Some(format!("{w}x{h}")),
                                _ => None,
                            };
                            let total_duration_secs: f64 = segments.iter().map(|s| s.duration_secs).sum();
                            let segments_count = segments.len();

                            let channels = ql.channels.map(|c| c.to_string());

                            streams.push(StreamSpec {
                                id: Some(format!("{}_{}", stream_idx.name.as_deref().unwrap_or("track"), ql.index)),
                                media_type,
                                group_id: stream_idx.name.clone(),
                                group_title: None,
                                language: stream_idx.language.clone(),
                                name: stream_idx.name.clone(),
                                codecs: Some(ql.fourcc),
                                resolution,
                                width: ql.width,
                                height: ql.height,
                                frame_rate: None,
                                channels,
                                bandwidth: ql.bitrate,
                                video_range: None,
                                url: manifest_url.to_string(),
                                original_url: Some(manifest_url.to_string()),
                                period_id: None,
                                segments_count,
                                total_duration_secs,
                                roles: Vec::new(),
                                segments,
                                init_segment: None,
                                drm_info: drm_info.clone(),
                            });
                        }
                    }
                }
                text_buf.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("error parsing MSS XML: {e}")),
            _ => {}
        }
    }

    Ok(streams)
}

#[derive(Debug, Clone, Default)]
struct StreamIndexContext {
    stream_type: String,
    name: Option<String>,
    subtype: Option<String>,
    url_pattern: String,
    language: Option<String>,
    timescale: u64,
    quality_levels: Vec<QualityLevelContext>,
}

#[derive(Debug, Clone, Default)]
struct QualityLevelContext {
    index: usize,
    bitrate: u64,
    fourcc: String,
    width: Option<u32>,
    height: Option<u32>,
    sampling_rate: Option<u32>,
    channels: Option<u32>,
    codec_private_data: Option<String>,
}

#[derive(Debug, Clone)]
struct MssChunk {
    start_time: u64,
    duration: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mss_manifest() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<SmoothStreamingMedia MajorVersion="2" MinorVersion="0" Duration="100000000" TimeScale="10000000">
  <StreamIndex Type="video" Name="video" Chunks="2" Url="QualityLevels({bitrate})/Fragments(video={start time})">
    <QualityLevel Index="0" Bitrate="2962000" FourCC="H264" MaxWidth="1280" MaxHeight="720" />
    <c d="20000000" />
    <c d="20000000" />
  </StreamIndex>
</SmoothStreamingMedia>"#;

        let streams = parse_mss_manifest(xml, "http://example.com/SuperSpeedway.ism/Manifest").unwrap();
        assert_eq!(streams.len(), 1);
        let s = &streams[0];
        assert_eq!(s.media_type, Some(MediaType::Video));
        assert_eq!(s.bandwidth, 2962000);
        assert_eq!(s.resolution, Some("1280x720".into()));
        assert_eq!(s.segments.len(), 2);
        assert_eq!(s.segments[0].url, "http://example.com/SuperSpeedway.ism/QualityLevels(2962000)/Fragments(video=0)");
        assert_eq!(s.segments[1].url, "http://example.com/SuperSpeedway.ism/QualityLevels(2962000)/Fragments(video=20000000)");
    }
}
