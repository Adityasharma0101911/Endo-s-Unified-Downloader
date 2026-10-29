//! IPTV M3U / M3U8 channel list parser matching N_m3u8DL-RE.

use std::collections::HashMap;
use regex::Regex;
use url::Url;

use crate::stream_spec::{MediaType, StreamSpec};

/// Check if raw text looks like an IPTV channel playlist rather than standard HLS.
pub fn is_iptv_playlist(text: &str) -> bool {
    let mut is_hls = false;
    let mut has_iptv_markers = false;

    for line in text.lines().map(str::trim) {
        if line.starts_with("#EXT-X-") {
            is_hls = true;
            break;
        }
        if line.starts_with("#EXTGRP:") {
            has_iptv_markers = true;
        }
        if line.starts_with("#EXTINF:") {
            let rest = &line[8..];
            if rest.starts_with("-1") || rest.contains("tvg-") || rest.contains("group-title") {
                has_iptv_markers = true;
            }
        }
    }

    !is_hls && has_iptv_markers
}

/// Parse an IPTV M3U channel list into stream specifications.
pub fn parse_iptv_playlist(text: &str, base_url_str: &str) -> Result<Vec<StreamSpec>, String> {
    let base_url = Url::parse(base_url_str).ok();
    let attr_re = Regex::new(r#"(?:^|\s)([\w-]+)=(?:"([^"]*)"|'([^']*)'|([^\s,]+))"#)
        .map_err(|e| e.to_string())?;

    let mut streams = Vec::new();
    let mut pending_spec: Option<StreamSpec> = None;

    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }

        if line.starts_with("#EXTINF:") {
            if pending_spec.is_some() {
                return Err("malformed IPTV playlist: consecutive #EXTINF without URL".to_string());
            }

            let comma_pos = find_title_separator(line);
            let (attr_part, title_part) = if comma_pos > 0 {
                (&line[..comma_pos], line[comma_pos + 1..].trim())
            } else {
                (line, "")
            };

            let mut attrs = HashMap::new();
            for cap in attr_re.captures_iter(attr_part) {
                let key = cap[1].to_ascii_lowercase();
                let val = cap.get(2).or_else(|| cap.get(3)).or_else(|| cap.get(4)).map(|m| m.as_str()).unwrap_or("");
                attrs.insert(key, val.to_string());
            }

            let name = if !title_part.is_empty() {
                Some(title_part.to_string())
            } else {
                attrs.get("tvg-name").cloned().or_else(|| attrs.get("tvg-id").cloned())
            };

            let mut spec = StreamSpec::new(String::new());
            spec.media_type = Some(MediaType::Video);
            spec.name = name;
            spec.group_id = attrs.get("tvg-id").cloned();
            spec.group_title = attrs.get("group-title").cloned();
            spec.language = attrs.get("tvg-language").cloned();
            spec.original_url = Some(base_url_str.to_string());

            pending_spec = Some(spec);
        } else if line.starts_with("#EXTGRP:") {
            if let Some(ref mut spec) = pending_spec {
                if spec.group_title.is_none() {
                    spec.group_title = Some(line[8..].trim().to_string());
                }
            }
        } else if !line.starts_with('#') {
            if let Some(mut spec) = pending_spec.take() {
                let final_url = if let Some(ref base) = base_url {
                    base.join(line).map(|u| u.to_string()).unwrap_or_else(|_| line.to_string())
                } else {
                    line.to_string()
                };

                if spec.name.is_none() {
                    spec.name = spec.group_id.clone().or_else(|| Some(line.to_string()));
                }
                spec.url = final_url;
                streams.push(spec);
            }
        }
    }

    if pending_spec.is_some() {
        return Err("malformed IPTV playlist: trailing #EXTINF without URL".to_string());
    }

    Ok(streams)
}

fn find_title_separator(line: &str) -> usize {
    let mut in_quote = '\0';
    for (i, c) in line.chars().enumerate().skip(8) {
        if in_quote != '\0' {
            if c == in_quote {
                in_quote = '\0';
            }
        } else if c == '\'' || c == '"' {
            in_quote = c;
        } else if c == ',' {
            return i;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_iptv() {
        let m3u = "#EXTM3U\n#EXTINF:-1 tvg-id=\"CNN\" group-title=\"News\",CNN International\nhttp://example.com/cnn.m3u8";
        assert!(is_iptv_playlist(m3u));

        let hls = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\nseg1.ts";
        assert!(!is_iptv_playlist(hls));
    }

    #[test]
    fn test_parse_iptv() {
        let text = r#"#EXTM3U
#EXTINF:-1 tvg-id="ch1" tvg-name="Channel One" tvg-language="en" group-title="Entertainment",Channel 1 HD
http://live.example.com/ch1.ts
#EXTINF:-1 tvg-id="ch2" group-title="Sports",Channel 2 Sports
http://live.example.com/ch2.ts
"#;
        let streams = parse_iptv_playlist(text, "http://live.example.com/channels.m3u").unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].name, Some("Channel 1 HD".into()));
        assert_eq!(streams[0].group_title, Some("Entertainment".into()));
        assert_eq!(streams[0].url, "http://live.example.com/ch1.ts");
        assert_eq!(streams[1].name, Some("Channel 2 Sports".into()));
        assert_eq!(streams[1].group_title, Some("Sports".into()));
    }
}
