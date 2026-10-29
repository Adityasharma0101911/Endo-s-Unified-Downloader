//! Native MPEG-DASH MPD manifest parser and segment generator.

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use url::Url;

use crate::stream_spec::{DrmKeyInfo, MediaSegment, MediaType, StreamSpec};

/// Parse an MPEG-DASH MPD manifest from XML text and the manifest's base URL.
pub fn parse_dash_manifest(xml: &str, mpd_url: &str) -> Result<Vec<StreamSpec>, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let base_url = Url::parse(mpd_url).map_err(|e| format!("invalid MPD URL '{mpd_url}': {e}"))?;

    let mut streams = Vec::new();
    let mut mpd_base_url: Option<String> = None;
    let mut mpd_duration_secs: Option<f64> = None;
    let mut _is_dynamic = false;

    // Parser state
    let mut current_period_id: Option<String> = None;
    let mut current_period_base_url: Option<String> = None;
    let mut current_period_start = 0.0;
    let mut current_period_duration: Option<f64> = None;

    let mut current_adaptation_set: Option<AdaptationSetContext> = None;
    let mut current_representation: Option<RepresentationContext> = None;

    let mut text_buf = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local_name = tag_name.split(':').last().unwrap_or(&tag_name);

                match local_name {
                    "MPD" => {
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            if key == "type" && val == "dynamic" {
                                _is_dynamic = true;
                            } else if key == "mediaPresentationDuration" {
                                mpd_duration_secs = parse_iso_duration(&val);
                            }
                        }
                    }
                    "Period" => {
                        current_period_base_url = None;
                        current_period_duration = None;
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            if key == "id" {
                                current_period_id = Some(val);
                            } else if key == "start" {
                                if let Some(d) = parse_iso_duration(&val) {
                                    current_period_start = d;
                                }
                            } else if key == "duration" {
                                current_period_duration = parse_iso_duration(&val);
                            }
                        }
                    }
                    "AdaptationSet" => {
                        let mut ctx = AdaptationSetContext::default();
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.as_str() {
                                "id" => ctx.id = Some(val),
                                "contentType" => ctx.content_type = Some(val),
                                "mimeType" => ctx.mime_type = Some(val),
                                "lang" => ctx.lang = Some(val),
                                "group" => ctx.group = Some(val),
                                "codecs" => ctx.codecs = Some(val),
                                "width" => ctx.width = val.parse().ok(),
                                "height" => ctx.height = val.parse().ok(),
                                "frameRate" => ctx.frame_rate = parse_frame_rate(&val),
                                _ => {}
                            }
                        }
                        current_adaptation_set = Some(ctx);
                    }
                    "Representation" => {
                        let mut ctx = RepresentationContext::default();
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.as_str() {
                                "id" => ctx.id = val,
                                "bandwidth" => ctx.bandwidth = val.parse().unwrap_or(0),
                                "codecs" => ctx.codecs = Some(val),
                                "width" => ctx.width = val.parse().ok(),
                                "height" => ctx.height = val.parse().ok(),
                                "frameRate" => ctx.frame_rate = parse_frame_rate(&val),
                                "audioSamplingRate" => ctx.sampling_rate = Some(val),
                                _ => {}
                            }
                        }
                        current_representation = Some(ctx);
                    }
                    "SegmentTemplate" => {
                        let mut template = SegmentTemplateInfo::default();
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.as_str() {
                                "timescale" => template.timescale = val.parse().unwrap_or(1),
                                "initialization" => template.initialization = Some(val),
                                "media" => template.media = Some(val),
                                "startNumber" => template.start_number = val.parse().unwrap_or(1),
                                "duration" => template.duration = val.parse().ok(),
                                _ => {}
                            }
                        }
                        if let Some(ref mut rep) = current_representation {
                            rep.segment_template = Some(template);
                        } else if let Some(ref mut adapt) = current_adaptation_set {
                            adapt.segment_template = Some(template);
                        }
                    }
                    "S" => {
                        // SegmentTimeline element
                        let mut s_time: Option<u64> = None;
                        let mut s_duration: u64 = 0;
                        let mut s_repeat: i64 = 0;
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.as_str() {
                                "t" => s_time = val.parse().ok(),
                                "d" => s_duration = val.parse().unwrap_or(0),
                                "r" => s_repeat = val.parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                        let entry = TimelineEntry { t: s_time, d: s_duration, r: s_repeat };
                        if let Some(ref mut rep) = current_representation {
                            if let Some(ref mut tmpl) = rep.segment_template {
                                tmpl.timeline.push(entry);
                            }
                        } else if let Some(ref mut adapt) = current_adaptation_set {
                            if let Some(ref mut tmpl) = adapt.segment_template {
                                tmpl.timeline.push(entry);
                            }
                        }
                    }
                    "ContentProtection" => {
                        let mut sys_id = String::new();
                        let mut default_kid = None;
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            if key.eq_ignore_ascii_case("schemeIdUri") {
                                sys_id = val;
                            } else if key.ends_with("default_KID") || key == "default_kid" {
                                default_kid = Some(val.replace('-', "").to_ascii_lowercase());
                            }
                        }
                        let drm = DrmKeyInfo {
                            system_id: sys_id,
                            default_kid,
                            pssh_base64: None,
                        };
                        if let Some(ref mut rep) = current_representation {
                            rep.drm_info.push(drm);
                        } else if let Some(ref mut adapt) = current_adaptation_set {
                            adapt.drm_info.push(drm);
                        }
                    }
                    "Role" => {
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            if key == "value" {
                                if let Some(ref mut adapt) = current_adaptation_set {
                                    adapt.roles.push(val);
                                }
                            }
                        }
                    }
                    _ => {}
                }

                // If Representation was an Empty element, build stream spec immediately
                if local_name == "Representation" {
                    // Check if event was Empty
                    // Note: if it has no child nodes or is self-closing, we build spec
                    if let Some(rep) = current_representation.take() {
                        if let Some(ref adapt) = current_adaptation_set {
                            let spec = build_dash_stream_spec(
                                &base_url,
                                mpd_base_url.as_deref(),
                                current_period_base_url.as_deref(),
                                current_period_id.as_deref(),
                                current_period_start,
                                current_period_duration.or(mpd_duration_secs).unwrap_or(0.0),
                                adapt,
                                &rep,
                            );
                            streams.push(spec);
                        }
                    }
                }
            }
            Ok(Event::Text(ref e)) => {
                text_buf = e.unescape().unwrap_or_default().to_string();
            }
            Ok(Event::End(ref e)) => {
                let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let local_name = tag_name.split(':').last().unwrap_or(&tag_name);

                match local_name {
                    "BaseURL" => {
                        let text = text_buf.trim().to_string();
                        if !text.is_empty() {
                            if current_adaptation_set.is_some() {
                                if let Some(ref mut adapt) = current_adaptation_set {
                                    adapt.base_url = Some(text);
                                }
                            } else if current_period_id.is_some() {
                                current_period_base_url = Some(text);
                            } else {
                                mpd_base_url = Some(text);
                            }
                        }
                    }
                    "cenc:pssh" | "pssh" => {
                        let pssh = text_buf.trim().to_string();
                        if !pssh.is_empty() {
                            if let Some(ref mut rep) = current_representation {
                                if let Some(last) = rep.drm_info.last_mut() {
                                    last.pssh_base64 = Some(pssh);
                                }
                            } else if let Some(ref mut adapt) = current_adaptation_set {
                                if let Some(last) = adapt.drm_info.last_mut() {
                                    last.pssh_base64 = Some(pssh);
                                }
                            }
                        }
                    }
                    "Representation" => {
                        if let Some(rep) = current_representation.take() {
                            if let Some(ref adapt) = current_adaptation_set {
                                let spec = build_dash_stream_spec(
                                    &base_url,
                                    mpd_base_url.as_deref(),
                                    current_period_base_url.as_deref(),
                                    current_period_id.as_deref(),
                                    current_period_start,
                                    current_period_duration.or(mpd_duration_secs).unwrap_or(0.0),
                                    adapt,
                                    &rep,
                                );
                                streams.push(spec);
                            }
                        }
                    }
                    "AdaptationSet" => {
                        current_adaptation_set = None;
                    }
                    _ => {}
                }
                text_buf.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("error parsing MPD XML at position {}: {e}", reader.buffer_position())),
            _ => {}
        }
    }

    Ok(streams)
}

#[derive(Debug, Clone, Default)]
struct AdaptationSetContext {
    id: Option<String>,
    content_type: Option<String>,
    mime_type: Option<String>,
    lang: Option<String>,
    group: Option<String>,
    codecs: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<f64>,
    roles: Vec<String>,
    drm_info: Vec<DrmKeyInfo>,
    segment_template: Option<SegmentTemplateInfo>,
    base_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct RepresentationContext {
    id: String,
    bandwidth: u64,
    codecs: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<f64>,
    sampling_rate: Option<String>,
    drm_info: Vec<DrmKeyInfo>,
    segment_template: Option<SegmentTemplateInfo>,
}

#[derive(Debug, Clone, Default)]
struct SegmentTemplateInfo {
    timescale: u64,
    initialization: Option<String>,
    media: Option<String>,
    start_number: u64,
    duration: Option<u64>,
    timeline: Vec<TimelineEntry>,
}

#[derive(Debug, Clone)]
struct TimelineEntry {
    t: Option<u64>,
    d: u64,
    r: i64,
}

fn build_dash_stream_spec(
    mpd_url: &Url,
    mpd_base: Option<&str>,
    period_base: Option<&str>,
    period_id: Option<&str>,
    period_start_secs: f64,
    period_duration_secs: f64,
    adapt: &AdaptationSetContext,
    rep: &RepresentationContext,
) -> StreamSpec {
    // Resolve hierarchy base URL
    let mut resolved = mpd_url.clone();
    if let Some(b) = mpd_base {
        if let Ok(u) = resolved.join(b) {
            resolved = u;
        }
    }
    if let Some(b) = period_base {
        if let Ok(u) = resolved.join(b) {
            resolved = u;
        }
    }
    if let Some(ref b) = adapt.base_url {
        if let Ok(u) = resolved.join(b) {
            resolved = u;
        }
    }

    // Determine media type
    let mime = adapt.mime_type.as_deref().unwrap_or("");
    let ctype = adapt.content_type.as_deref().unwrap_or("");
    let media_type = if mime.starts_with("video") || ctype.eq_ignore_ascii_case("video") || rep.width.is_some() || adapt.width.is_some() {
        Some(MediaType::Video)
    } else if mime.starts_with("audio") || ctype.eq_ignore_ascii_case("audio") {
        Some(MediaType::Audio)
    } else if mime.contains("ttml") || mime.contains("vtt") || ctype.eq_ignore_ascii_case("text") || ctype.eq_ignore_ascii_case("subtitles") {
        Some(MediaType::Subtitles)
    } else {
        None
    };

    let width = rep.width.or(adapt.width);
    let height = rep.height.or(adapt.height);
    let resolution = match (width, height) {
        (Some(w), Some(h)) => Some(format!("{w}x{h}")),
        _ => None,
    };
    let codecs = rep.codecs.clone().or_else(|| adapt.codecs.clone());
    let frame_rate = rep.frame_rate.or(adapt.frame_rate);

    // Merge DRM info
    let mut drm_info = adapt.drm_info.clone();
    drm_info.extend(rep.drm_info.clone());

    // Generate segments from template
    let tmpl = rep.segment_template.as_ref().or(adapt.segment_template.as_ref());
    let mut segments = Vec::new();
    let mut init_segment = None;

    if let Some(t) = tmpl {
        // Init segment
        if let Some(ref init_str) = t.initialization {
            let expanded_init = resolve_template(init_str, &rep.id, rep.bandwidth, 0, 0);
            if let Ok(init_url) = resolved.join(&expanded_init) {
                init_segment = Some(MediaSegment {
                    index: 0,
                    url: init_url.to_string(),
                    duration_secs: 0.0,
                    start_time_secs: 0.0,
                    byte_range: None,
                    enc_info: None,
                });
            }
        }

        // Media segments
        if let Some(ref media_str) = t.media {
            let timescale = if t.timescale == 0 { 1.0 } else { t.timescale as f64 };

            if !t.timeline.is_empty() {
                // SegmentTimeline calculation
                let mut current_time = 0u64;
                let mut seg_idx = 0usize;
                let mut number = t.start_number;

                for entry in &t.timeline {
                    if let Some(start_t) = entry.t {
                        current_time = start_t;
                    }
                    let repeat_count = if entry.r < 0 { 0 } else { entry.r as usize };
                    for _ in 0..=repeat_count {
                        let seg_url_rel = resolve_template(media_str, &rep.id, rep.bandwidth, number, current_time);
                        if let Ok(seg_url) = resolved.join(&seg_url_rel) {
                            let duration_secs = entry.d as f64 / timescale;
                            let start_time_secs = period_start_secs + (current_time as f64 / timescale);
                            segments.push(MediaSegment {
                                index: seg_idx,
                                url: seg_url.to_string(),
                                duration_secs,
                                start_time_secs,
                                byte_range: None,
                                enc_info: None,
                            });
                            seg_idx += 1;
                        }
                        current_time += entry.d;
                        number += 1;
                    }
                }
            } else if let Some(dur) = t.duration {
                // Fixed duration calculation
                let seg_dur_secs = dur as f64 / timescale;
                let count = if seg_dur_secs > 0.0 && period_duration_secs > 0.0 {
                    (period_duration_secs / seg_dur_secs).ceil() as usize
                } else {
                    16 // Default initial take count
                };

                for i in 0..count {
                    let number = t.start_number + i as u64;
                    let time = (i as u64) * dur;
                    let seg_url_rel = resolve_template(media_str, &rep.id, rep.bandwidth, number, time);
                    if let Ok(seg_url) = resolved.join(&seg_url_rel) {
                        segments.push(MediaSegment {
                            index: i,
                            url: seg_url.to_string(),
                            duration_secs: seg_dur_secs,
                            start_time_secs: period_start_secs + (i as f64 * seg_dur_secs),
                            byte_range: None,
                            enc_info: None,
                        });
                    }
                }
            }
        }
    }

    let segments_count = segments.len();
    let total_duration_secs = if period_duration_secs > 0.0 {
        period_duration_secs
    } else {
        segments.iter().map(|s| s.duration_secs).sum()
    };

    StreamSpec {
        id: Some(rep.id.clone()),
        media_type,
        group_id: adapt.group.clone().or_else(|| adapt.id.clone()),
        group_title: None,
        language: adapt.lang.clone(),
        name: None,
        codecs,
        resolution,
        width,
        height,
        frame_rate,
        channels: None,
        bandwidth: rep.bandwidth,
        video_range: None,
        url: resolved.to_string(),
        original_url: Some(mpd_url.to_string()),
        period_id: period_id.map(|s| s.to_string()),
        segments_count,
        total_duration_secs,
        roles: adapt.roles.clone(),
        segments,
        init_segment,
        drm_info,
    }
}

/// Substitute DASH template identifiers:
/// `$RepresentationID$` -> id
/// `$Bandwidth$` -> bandwidth
/// `$Number$` / `$Number%05d$` -> number
/// `$Time$` / `$Time%05d$` -> time
fn resolve_template(tmpl: &str, rep_id: &str, bandwidth: u64, number: u64, time: u64) -> String {
    let mut res = String::with_capacity(tmpl.len() + 16);
    let mut chars = tmpl.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' {
            if chars.peek() == Some(&'$') {
                chars.next();
                res.push('$');
                continue;
            }
            // Read until next '$'
            let mut var = String::new();
            while let Some(&nc) = chars.peek() {
                chars.next();
                if nc == '$' {
                    break;
                }
                var.push(nc);
            }

            if var == "RepresentationID" {
                res.push_str(rep_id);
            } else if var == "Bandwidth" {
                res.push_str(&bandwidth.to_string());
            } else if var.starts_with("Number") {
                format_placeholder(&mut res, &var, number);
            } else if var.starts_with("Time") {
                format_placeholder(&mut res, &var, time);
            } else {
                res.push('$');
                res.push_str(&var);
                res.push('$');
            }
        } else {
            res.push(c);
        }
    }

    res
}

/// Handle format specs like `%05d` in `$Number%05d$`.
fn format_placeholder(out: &mut String, placeholder: &str, val: u64) {
    if let Some((_, fmt_spec)) = placeholder.split_once('%') {
        if let Some(width_str) = fmt_spec.strip_suffix('d') {
            if width_str.starts_with('0') {
                if let Ok(width) = width_str[1..].parse::<usize>() {
                    let s = format!("{:0width$}", val, width = width);
                    out.push_str(&s);
                    return;
                }
            }
        }
    }
    out.push_str(&val.to_string());
}

/// Parse ISO 8601 duration string like "PT1H20M30.5S" to seconds.
fn parse_iso_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    if !s.starts_with('P') {
        return None;
    }
    let mut total = 0.0;
    let mut num_buf = String::new();
    let mut in_time = false;

    for c in s[1..].chars() {
        match c {
            'T' => in_time = true,
            'Y' => {
                let n: f64 = num_buf.parse().ok()?;
                total += n * 365.0 * 86400.0;
                num_buf.clear();
            }
            'M' if !in_time => {
                let n: f64 = num_buf.parse().ok()?;
                total += n * 30.0 * 86400.0;
                num_buf.clear();
            }
            'D' => {
                let n: f64 = num_buf.parse().ok()?;
                total += n * 86400.0;
                num_buf.clear();
            }
            'H' => {
                let n: f64 = num_buf.parse().ok()?;
                total += n * 3600.0;
                num_buf.clear();
            }
            'M' if in_time => {
                let n: f64 = num_buf.parse().ok()?;
                total += n * 60.0;
                num_buf.clear();
            }
            'S' => {
                let n: f64 = num_buf.parse().ok()?;
                total += n;
                num_buf.clear();
            }
            digit if digit.is_ascii_digit() || digit == '.' => {
                num_buf.push(digit);
            }
            _ => {}
        }
    }
    Some(total)
}

/// Parse frame rate like "24" or "24000/1001".
fn parse_frame_rate(s: &str) -> Option<f64> {
    if let Some((num, den)) = s.split_once('/') {
        let n: f64 = num.parse().ok()?;
        let d: f64 = den.parse().ok()?;
        if d > 0.0 {
            Some(n / d)
        } else {
            None
        }
    } else {
        s.parse::<f64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_iso_duration() {
        assert_eq!(parse_iso_duration("PT1H20M30.5S").unwrap(), 4830.5);
        assert_eq!(parse_iso_duration("PT30S").unwrap(), 30.0);
        assert_eq!(parse_iso_duration("P1DT2H").unwrap(), 93600.0);
    }

    #[test]
    fn test_resolve_template() {
        assert_eq!(
            resolve_template("$RepresentationID$_$Number%05d$.m4s", "v1", 2000000, 42, 0),
            "v1_00042.m4s"
        );
        assert_eq!(
            resolve_template("$RepresentationID$_$Time$.m4s", "v1", 2000000, 1, 90000),
            "v1_90000.m4s"
        );
    }

    #[test]
    fn test_parse_simple_mpd() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" mediaPresentationDuration="PT10S" type="static">
  <Period id="p0" duration="PT10S">
    <AdaptationSet contentType="video" mimeType="video/mp4" width="1920" height="1080">
      <SegmentTemplate timescale="1000" duration="2000" initialization="init.mp4" media="seg_$Number$.m4s" startNumber="1" />
      <Representation id="rep1" bandwidth="5000000" codecs="avc1.640028" />
    </AdaptationSet>
  </Period>
</MPD>"#;
        let streams = parse_dash_manifest(xml, "http://example.com/stream.mpd").unwrap();
        assert_eq!(streams.len(), 1);
        let s = &streams[0];
        assert_eq!(s.media_type, Some(MediaType::Video));
        assert_eq!(s.resolution, Some("1920x1080".into()));
        assert_eq!(s.bandwidth, 5000000);
        assert_eq!(s.segments.len(), 5);
        assert_eq!(s.segments[0].url, "http://example.com/seg_1.m4s");
        assert_eq!(s.init_segment.as_ref().unwrap().url, "http://example.com/init.mp4");
    }
}
