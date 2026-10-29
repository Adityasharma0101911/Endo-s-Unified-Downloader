//! Stream filtering and track selection engine matching N_m3u8DL-RE `-sv`, `-sa`, `-ss`, `-dv`, `-da`, `-ds`.

use std::collections::HashMap;
use regex::Regex;
use crate::stream_spec::{MediaType, StreamSpec};

/// Stream filter criteria.
#[derive(Debug, Clone, Default)]
pub struct StreamFilter {
    pub group_id_reg: Option<Regex>,
    pub language_reg: Option<Regex>,
    pub name_reg: Option<Regex>,
    pub codecs_reg: Option<Regex>,
    pub resolution_reg: Option<Regex>,
    pub frame_rate_reg: Option<Regex>,
    pub channels_reg: Option<Regex>,
    pub video_range_reg: Option<Regex>,
    pub url_reg: Option<Regex>,
    pub period_id_reg: Option<Regex>,
    pub segments_min_count: Option<usize>,
    pub segments_max_count: Option<usize>,
    pub playlist_min_dur: Option<f64>,
    pub playlist_max_dur: Option<f64>,
    pub bandwidth_min_kbps: Option<u64>,
    pub bandwidth_max_kbps: Option<u64>,
    pub role: Option<String>,
    pub for_target: ForTarget,
}

/// The `for=` stream selection policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForTarget {
    Best(usize),
    Worst(usize),
    All,
}

impl Default for ForTarget {
    fn default() -> Self {
        Self::Best(1)
    }
}

impl StreamFilter {
    /// Parse a filter string such as:
    /// - `best`
    /// - `all`
    /// - `res=^1920x:codecs=hvc1:for=best`
    /// - `plistDurMin=1h20m30s:for=best`
    /// - `bwMin=800:bwMax=1000`
    /// - `segsMax=2:for=all`
    pub fn parse(input: &str) -> Result<Self, String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(Self::default());
        }

        // Bare words
        if trimmed.eq_ignore_ascii_case("best") {
            return Ok(Self { for_target: ForTarget::Best(1), ..Default::default() });
        }
        if trimmed.eq_ignore_ascii_case("worst") {
            return Ok(Self { for_target: ForTarget::Worst(1), ..Default::default() });
        }
        if trimmed.eq_ignore_ascii_case("all") {
            return Ok(Self { for_target: ForTarget::All, ..Default::default() });
        }

        let mut filter = Self::default();
        let tokens = split_colon_separated(trimmed);

        for token in tokens {
            if let Some((k, v)) = token.split_once('=') {
                let key = k.trim();
                let val = unquote(v.trim());
                match key.to_ascii_lowercase().as_str() {
                    "id" => {
                        filter.group_id_reg = Some(Regex::new(&val).map_err(|e| format!("invalid id regex: {e}"))?);
                    }
                    "lang" | "language" => {
                        filter.language_reg = Some(Regex::new(&val).map_err(|e| format!("invalid lang regex: {e}"))?);
                    }
                    "name" => {
                        filter.name_reg = Some(Regex::new(&val).map_err(|e| format!("invalid name regex: {e}"))?);
                    }
                    "codecs" | "codec" => {
                        filter.codecs_reg = Some(Regex::new(&val).map_err(|e| format!("invalid codecs regex: {e}"))?);
                    }
                    "res" | "resolution" => {
                        filter.resolution_reg = Some(Regex::new(&val).map_err(|e| format!("invalid res regex: {e}"))?);
                    }
                    "frame" | "framerate" => {
                        filter.frame_rate_reg = Some(Regex::new(&val).map_err(|e| format!("invalid frame regex: {e}"))?);
                    }
                    "channel" | "channels" => {
                        filter.channels_reg = Some(Regex::new(&val).map_err(|e| format!("invalid channels regex: {e}"))?);
                    }
                    "range" => {
                        filter.video_range_reg = Some(Regex::new(&val).map_err(|e| format!("invalid range regex: {e}"))?);
                    }
                    "url" => {
                        filter.url_reg = Some(Regex::new(&val).map_err(|e| format!("invalid url regex: {e}"))?);
                    }
                    "period" => {
                        filter.period_id_reg = Some(Regex::new(&val).map_err(|e| format!("invalid period regex: {e}"))?);
                    }
                    "segsmin" => {
                        filter.segments_min_count = Some(val.parse::<usize>().map_err(|e| format!("invalid segsMin: {e}"))?);
                    }
                    "segsmax" => {
                        filter.segments_max_count = Some(val.parse::<usize>().map_err(|e| format!("invalid segsMax: {e}"))?);
                    }
                    "plistdurmin" => {
                        filter.playlist_min_dur = Some(parse_duration_hms(&val)?);
                    }
                    "plistdurmax" => {
                        filter.playlist_max_dur = Some(parse_duration_hms(&val)?);
                    }
                    "bwmin" => {
                        filter.bandwidth_min_kbps = Some(val.parse::<u64>().map_err(|e| format!("invalid bwMin: {e}"))?);
                    }
                    "bwmax" => {
                        filter.bandwidth_max_kbps = Some(val.parse::<u64>().map_err(|e| format!("invalid bwMax: {e}"))?);
                    }
                    "role" => {
                        filter.role = Some(val.to_string());
                    }
                    "for" => {
                        filter.for_target = parse_for_target(&val)?;
                    }
                    other => {
                        return Err(format!("unknown filter key: {other}"));
                    }
                }
            } else {
                let token_clean = token.trim();
                if token_clean.eq_ignore_ascii_case("all") {
                    filter.for_target = ForTarget::All;
                } else if token_clean.eq_ignore_ascii_case("best") {
                    filter.for_target = ForTarget::Best(1);
                } else if token_clean.eq_ignore_ascii_case("worst") {
                    filter.for_target = ForTarget::Worst(1);
                }
            }
        }

        Ok(filter)
    }

    /// Check if a single stream matches all non-ranking criteria of this filter.
    pub fn matches(&self, stream: &StreamSpec) -> bool {
        if let Some(ref reg) = self.group_id_reg {
            let gid = stream.group_id.as_deref().or(stream.id.as_deref()).unwrap_or("");
            if !reg.is_match(gid) {
                return false;
            }
        }
        if let Some(ref reg) = self.language_reg {
            let lang = stream.language.as_deref().unwrap_or("");
            if !reg.is_match(lang) {
                return false;
            }
        }
        if let Some(ref reg) = self.name_reg {
            let name = stream.name.as_deref().unwrap_or("");
            if !reg.is_match(name) {
                return false;
            }
        }
        if let Some(ref reg) = self.codecs_reg {
            let codecs = stream.codecs.as_deref().unwrap_or("");
            if !reg.is_match(codecs) {
                return false;
            }
        }
        if let Some(ref reg) = self.resolution_reg {
            let res = stream.resolution.as_deref().unwrap_or("");
            if !reg.is_match(res) {
                return false;
            }
        }
        if let Some(ref reg) = self.frame_rate_reg {
            let fr_str = stream.frame_rate.map(|f| f.to_string()).unwrap_or_default();
            if !reg.is_match(&fr_str) {
                return false;
            }
        }
        if let Some(ref reg) = self.channels_reg {
            let ch = stream.channels.as_deref().unwrap_or("");
            if !reg.is_match(ch) {
                return false;
            }
        }
        if let Some(ref reg) = self.video_range_reg {
            let vr = stream.video_range.as_deref().unwrap_or("");
            if !reg.is_match(vr) {
                return false;
            }
        }
        if let Some(ref reg) = self.url_reg {
            if !reg.is_match(&stream.url) {
                return false;
            }
        }
        if let Some(ref reg) = self.period_id_reg {
            let pid = stream.period_id.as_deref().unwrap_or("");
            if !reg.is_match(pid) {
                return false;
            }
        }
        if let Some(min_segs) = self.segments_min_count {
            if stream.segments_count > 0 && stream.segments_count < min_segs {
                return false;
            }
        }
        if let Some(max_segs) = self.segments_max_count {
            if stream.segments_count > 0 && stream.segments_count > max_segs {
                return false;
            }
        }
        if let Some(min_dur) = self.playlist_min_dur {
            if stream.total_duration_secs > 0.0 && stream.total_duration_secs < min_dur {
                return false;
            }
        }
        if let Some(max_dur) = self.playlist_max_dur {
            if stream.total_duration_secs > 0.0 && stream.total_duration_secs > max_dur {
                return false;
            }
        }
        if let Some(min_kbps) = self.bandwidth_min_kbps {
            let kbps = stream.bandwidth / 1000;
            if kbps < min_kbps {
                return false;
            }
        }
        if let Some(max_kbps) = self.bandwidth_max_kbps {
            let kbps = stream.bandwidth / 1000;
            if kbps > max_kbps {
                return false;
            }
        }
        if let Some(ref role) = self.role {
            if !stream.has_role(role) {
                return false;
            }
        }
        true
    }
}

/// Keep streams matching the filter and apply `for=` ranking.
pub fn filter_keep(streams: &[StreamSpec], filter: &StreamFilter) -> Vec<StreamSpec> {
    let matched: Vec<&StreamSpec> = streams.iter().filter(|s| filter.matches(s)).collect();
    if matched.is_empty() {
        return Vec::new();
    }

    match filter.for_target {
        ForTarget::All => matched.into_iter().cloned().collect(),
        ForTarget::Best(n) => {
            // Group by base language
            let mut groups: HashMap<String, Vec<&StreamSpec>> = HashMap::new();
            for s in matched {
                let key = s.base_language().unwrap_or_else(|| "".to_string());
                groups.entry(key).or_default().push(s);
            }
            let mut result = Vec::new();
            for (_, mut list) in groups {
                list.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
                for s in list.into_iter().take(n) {
                    result.push(s.clone());
                }
            }
            result
        }
        ForTarget::Worst(n) => {
            let mut groups: HashMap<String, Vec<&StreamSpec>> = HashMap::new();
            for s in matched {
                let key = s.base_language().unwrap_or_else(|| "".to_string());
                groups.entry(key).or_default().push(s);
            }
            let mut result = Vec::new();
            for (_, mut list) in groups {
                list.sort_by(|a, b| a.bandwidth.cmp(&b.bandwidth));
                for s in list.into_iter().take(n) {
                    result.push(s.clone());
                }
            }
            result
        }
    }
}

/// Drop streams matching the filter.
pub fn filter_drop(streams: &[StreamSpec], filter: &StreamFilter) -> Vec<StreamSpec> {
    let kept_set: Vec<StreamSpec> = filter_keep(streams, filter);
    streams
        .iter()
        .filter(|s| !kept_set.iter().any(|k| k.url == s.url && k.bandwidth == s.bandwidth))
        .cloned()
        .collect()
}

/// Automatically select best video, best audio(s), and subtitle tracks.
pub fn auto_select(streams: &[StreamSpec]) -> Vec<StreamSpec> {
    let mut selected = Vec::new();

    // 1. Best video
    let mut videos: Vec<&StreamSpec> = streams
        .iter()
        .filter(|s| s.media_type == Some(MediaType::Video) || (s.media_type.is_none() && s.resolution.is_some()))
        .collect();
    if !videos.is_empty() {
        videos.sort_by(|a, b| {
            let res_a = a.width.unwrap_or(0) * a.height.unwrap_or(0);
            let res_b = b.width.unwrap_or(0) * b.height.unwrap_or(0);
            res_b.cmp(&res_a).then_with(|| b.bandwidth.cmp(&a.bandwidth))
        });
        selected.push((*videos[0]).clone());
    }

    // 2. Best audio per language
    let audios: Vec<&StreamSpec> = streams
        .iter()
        .filter(|s| s.media_type == Some(MediaType::Audio))
        .collect();
    let mut audio_groups: HashMap<String, Vec<&StreamSpec>> = HashMap::new();
    for a in audios {
        let key = a.base_language().unwrap_or_else(|| "".to_string());
        audio_groups.entry(key).or_default().push(a);
    }
    for (_, mut list) in audio_groups {
        list.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
        if let Some(best_a) = list.first() {
            selected.push((*best_a).clone());
        }
    }

    // 3. Subtitles
    for sub in streams.iter().filter(|s| s.media_type == Some(MediaType::Subtitles)) {
        selected.push(sub.clone());
    }

    // If nothing matched by media_type, fall back to best overall stream
    if selected.is_empty() && !streams.is_empty() {
        let mut sorted: Vec<&StreamSpec> = streams.iter().collect();
        sorted.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
        selected.push((*sorted[0]).clone());
    }

    selected
}

/// Split colon-separated parameters, allowing escaped colons `\:`.
fn split_colon_separated(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(&':') = chars.peek() {
                    chars.next();
                    current.push(':');
                } else {
                    current.push('\\');
                }
            }
            '"' | '\'' => {
                in_quote = !in_quote;
                current.push(c);
            }
            ':' if !in_quote => {
                if !current.trim().is_empty() {
                    tokens.push(current.trim().to_string());
                }
                current.clear();
            }
            other => current.push(other),
        }
    }
    if !current.trim().is_empty() {
        tokens.push(current.trim().to_string());
    }
    tokens
}

/// Unquote outer quotes if present.
fn unquote(s: &str) -> String {
    let trimmed = s.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Parse `for=` value (e.g. `best`, `best2`, `worst`, `worst3`, `all`).
fn parse_for_target(val: &str) -> Result<ForTarget, String> {
    let lower = val.to_ascii_lowercase();
    if lower == "all" {
        return Ok(ForTarget::All);
    }
    if lower == "best" {
        return Ok(ForTarget::Best(1));
    }
    if lower == "worst" {
        return Ok(ForTarget::Worst(1));
    }
    if let Some(num_str) = lower.strip_prefix("best") {
        if let Ok(n) = num_str.parse::<usize>() {
            return Ok(ForTarget::Best(n));
        }
    }
    if let Some(num_str) = lower.strip_prefix("worst") {
        if let Ok(n) = num_str.parse::<usize>() {
            return Ok(ForTarget::Worst(n));
        }
    }
    Err(format!("invalid for= value: {val}"))
}

/// Parse human-readable duration like `1h20m30s`, `90s`, `1.5m`.
fn parse_duration_hms(s: &str) -> Result<f64, String> {
    let trimmed = s.trim();
    if let Ok(secs) = trimmed.parse::<f64>() {
        return Ok(secs);
    }
    let re = Regex::new(r"(?:(\d+(?:\.\d+)?)h)?(?:(\d+(?:\.\d+)?)m)?(?:(\d+(?:\.\d+)?)s)?").unwrap();
    if let Some(caps) = re.captures(trimmed) {
        let mut total = 0.0;
        let mut matched_any = false;
        if let Some(h) = caps.get(1) {
            total += h.as_str().parse::<f64>().map_err(|e| e.to_string())? * 3600.0;
            matched_any = true;
        }
        if let Some(m) = caps.get(2) {
            total += m.as_str().parse::<f64>().map_err(|e| e.to_string())? * 60.0;
            matched_any = true;
        }
        if let Some(s) = caps.get(3) {
            total += s.as_str().parse::<f64>().map_err(|e| e.to_string())?;
            matched_any = true;
        }
        if matched_any {
            return Ok(total);
        }
    }
    Err(format!("invalid duration format: {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_filter_simple() {
        let filter = StreamFilter::parse("best").unwrap();
        assert_eq!(filter.for_target, ForTarget::Best(1));

        let filter = StreamFilter::parse("all").unwrap();
        assert_eq!(filter.for_target, ForTarget::All);
    }

    #[test]
    fn test_parse_filter_complex() {
        let filter = StreamFilter::parse(r#"res="^1920x":codecs=hvc1:for=best"#).unwrap();
        assert!(filter.resolution_reg.is_some());
        assert!(filter.codecs_reg.is_some());
        assert_eq!(filter.for_target, ForTarget::Best(1));
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration_hms("90s").unwrap(), 90.0);
        assert_eq!(parse_duration_hms("1h20m30s").unwrap(), 4830.0);
    }

    #[test]
    fn test_filter_keep_and_drop() {
        let mut s1 = StreamSpec::new("http://example.com/1080.m3u8".into());
        s1.resolution = Some("1920x1080".into());
        s1.codecs = Some("avc1.640028".into());
        s1.bandwidth = 5_000_000;
        s1.language = Some("en".into());

        let mut s2 = StreamSpec::new("http://example.com/720.m3u8".into());
        s2.resolution = Some("1280x720".into());
        s2.codecs = Some("avc1.64001f".into());
        s2.bandwidth = 2_500_000;
        s2.language = Some("en".into());

        let streams = vec![s1.clone(), s2.clone()];
        let filter = StreamFilter::parse("res=^1920x:for=best").unwrap();
        let kept = filter_keep(&streams, &filter);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].bandwidth, 5_000_000);

        let dropped = filter_drop(&streams, &filter);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].bandwidth, 2_500_000);
    }
}
