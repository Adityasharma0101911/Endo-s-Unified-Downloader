//! Subtitle format conversion (VTT <-> SRT) and timestamp shifting (`--sub-format`, `--auto-subtitle-fix`).

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubtitleFormat {
    #[default]
    Srt,
    Vtt,
}

impl SubtitleFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "SRT" => Some(Self::Srt),
            "VTT" | "WEBVTT" => Some(Self::Vtt),
            _ => None,
        }
    }
}

/// A parsed subtitle cue.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleCue {
    pub start_secs: f64,
    pub end_secs: f64,
    pub text: String,
}

/// Shift subtitle cue timestamps by `offset_secs` and format to target format.
pub fn fix_and_convert_subtitles(
    raw_sub: &str,
    target_format: SubtitleFormat,
    offset_secs: f64,
) -> String {
    let cues = parse_subtitles(raw_sub);
    let mut shifted_cues = Vec::new();

    for mut cue in cues {
        cue.start_secs = (cue.start_secs + offset_secs).max(0.0);
        cue.end_secs = (cue.end_secs + offset_secs).max(0.0);
        if cue.end_secs > cue.start_secs {
            shifted_cues.push(cue);
        }
    }

    match target_format {
        SubtitleFormat::Srt => format_srt(&shifted_cues),
        SubtitleFormat::Vtt => format_vtt(&shifted_cues),
    }
}

/// Parse either WebVTT or SRT subtitle text into cues.
pub fn parse_subtitles(content: &str) -> Vec<SubtitleCue> {
    let time_re = Regex::new(
        r"(?:(\d{1,2}):)?(\d{2}):(\d{2})[,\.](\d{3})\s*-->\s*(?:(\d{1,2}):)?(\d{2}):(\d{2})[,\.](\d{3})"
    ).unwrap();

    let mut cues = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        if let Some(caps) = time_re.captures(line) {
            let start_h: f64 = caps.get(1).map(|m| m.as_str().parse().unwrap_or(0.0)).unwrap_or(0.0);
            let start_m: f64 = caps[2].parse().unwrap_or(0.0);
            let start_s: f64 = caps[3].parse().unwrap_or(0.0);
            let start_ms: f64 = caps[4].parse().unwrap_or(0.0);

            let end_h: f64 = caps.get(5).map(|m| m.as_str().parse().unwrap_or(0.0)).unwrap_or(0.0);
            let end_m: f64 = caps[6].parse().unwrap_or(0.0);
            let end_s: f64 = caps[7].parse().unwrap_or(0.0);
            let end_ms: f64 = caps[8].parse().unwrap_or(0.0);

            let start_secs = start_h * 3600.0 + start_m * 60.0 + start_s + (start_ms / 1000.0);
            let end_secs = end_h * 3600.0 + end_m * 60.0 + end_s + (end_ms / 1000.0);

            i += 1;
            let mut text_lines = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                text_lines.push(clean_subtitle_tags(lines[i].trim()));
                i += 1;
            }

            cues.push(SubtitleCue {
                start_secs,
                end_secs,
                text: text_lines.join("\n"),
            });
        }
        i += 1;
    }

    cues
}

/// Strip WebVTT voice/color tags like `<v speaker>` or `<c.red>...</c>`.
fn clean_subtitle_tags(text: &str) -> String {
    let tag_re = Regex::new(r"<[^>]+>").unwrap();
    tag_re.replace_all(text, "").to_string()
}

/// Format cues as standard SRT.
pub fn format_srt(cues: &[SubtitleCue]) -> String {
    let mut out = String::new();
    for (i, cue) in cues.iter().enumerate() {
        out.push_str(&(i + 1).to_string());
        out.push('\n');
        out.push_str(&format_hms_ms(cue.start_secs, ','));
        out.push_str(" --> ");
        out.push_str(&format_hms_ms(cue.end_secs, ','));
        out.push('\n');
        out.push_str(&cue.text);
        out.push_str("\n\n");
    }
    out
}

/// Format cues as standard WebVTT.
pub fn format_vtt(cues: &[SubtitleCue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        out.push_str(&format_hms_ms(cue.start_secs, '.'));
        out.push_str(" --> ");
        out.push_str(&format_hms_ms(cue.end_secs, '.'));
        out.push('\n');
        out.push_str(&cue.text);
        out.push_str("\n\n");
    }
    out
}

fn format_hms_ms(secs: f64, ms_sep: char) -> String {
    let total_ms = (secs * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_secs = total_ms / 1000;
    let s = total_secs % 60;
    let total_mins = total_secs / 60;
    let m = total_mins % 60;
    let h = total_mins / 60;

    format!("{h:02}:{m:02}:{s:02}{ms_sep}{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vtt_to_srt_conversion() {
        let vtt = r#"WEBVTT

00:00:01.500 --> 00:00:04.000
<v Speaker>Hello world!</v>
Second line.
"#;
        let srt = fix_and_convert_subtitles(vtt, SubtitleFormat::Srt, 0.0);
        assert!(srt.contains("00:00:01,500 --> 00:00:04,000"));
        assert!(srt.contains("Hello world!"));
        assert!(!srt.contains("<v Speaker>"));
    }

    #[test]
    fn test_timestamp_shifting() {
        let srt_in = "1\n00:00:01,000 --> 00:00:03,000\nHello\n\n";
        let srt_out = fix_and_convert_subtitles(srt_in, SubtitleFormat::Srt, 2.5);
        assert!(srt_out.contains("00:00:03,500 --> 00:00:05,500"));
    }
}
