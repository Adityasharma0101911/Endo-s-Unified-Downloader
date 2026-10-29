//! Range slicing for segments based on segment indices or timestamps (`--custom-range`).

use crate::stream_spec::MediaSegment;

#[derive(Debug, Clone, PartialEq)]
pub enum CustomRange {
    /// Segment index range (start, end inclusive). If start is None -> from 0. If end is None -> to the end.
    IndexRange { start: Option<usize>, end: Option<usize> },
    /// Time in seconds range (start_secs, end_secs inclusive).
    TimeRange { start_secs: Option<f64>, end_secs: Option<f64> },
}

impl CustomRange {
    /// Parse a range string such as:
    /// - `0-10`
    /// - `10-`
    /// - `-99`
    /// - `05:00-20:00`
    /// - `00:26-`
    /// - `-00:26`
    pub fn parse(input: &str) -> Result<Self, String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err("empty custom-range".to_string());
        }

        let (left_str, right_str) = match trimmed.split_once('-') {
            Some((l, r)) => (l.trim(), r.trim()),
            None => {
                // If single number, treat as exact index `N-N`
                if let Ok(idx) = trimmed.parse::<usize>() {
                    return Ok(Self::IndexRange { start: Some(idx), end: Some(idx) });
                }
                return Err(format!("invalid custom-range: {trimmed}"));
            }
        };

        // Check if either side contains a colon `:`, which indicates a time format (MM:SS or HH:MM:SS)
        if left_str.contains(':') || right_str.contains(':') {
            let start_secs = if left_str.is_empty() {
                None
            } else {
                Some(parse_time_to_secs(left_str)?)
            };
            let end_secs = if right_str.is_empty() {
                None
            } else {
                Some(parse_time_to_secs(right_str)?)
            };
            Ok(Self::TimeRange { start_secs, end_secs })
        } else {
            let start = if left_str.is_empty() {
                None
            } else {
                Some(left_str.parse::<usize>().map_err(|e| format!("invalid start index: {e}"))?)
            };
            let end = if right_str.is_empty() {
                None
            } else {
                Some(right_str.parse::<usize>().map_err(|e| format!("invalid end index: {e}"))?)
            };
            Ok(Self::IndexRange { start, end })
        }
    }

    /// Slice a list of segments matching this custom range.
    pub fn slice_segments(&self, segments: &[MediaSegment]) -> Vec<MediaSegment> {
        if segments.is_empty() {
            return Vec::new();
        }

        match self {
            Self::IndexRange { start, end } => {
                let start_idx = start.unwrap_or(0);
                let end_idx = end.unwrap_or(segments.len().saturating_sub(1));
                segments
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx >= start_idx && *idx <= end_idx)
                    .map(|(_, seg)| seg.clone())
                    .collect()
            }
            Self::TimeRange { start_secs, end_secs } => {
                let start_time = start_secs.unwrap_or(0.0);
                let end_time = end_secs.unwrap_or(f64::MAX);

                segments
                    .iter()
                    .filter(|seg| {
                        let seg_start = seg.start_time_secs;
                        let seg_end = seg.start_time_secs + seg.duration_secs;
                        // Include segment if it overlaps with [start_time, end_time]
                        seg_end > start_time && seg_start <= end_time
                    })
                    .cloned()
                    .collect()
            }
        }
    }
}

/// Parse `HH:MM:SS` or `MM:SS` or plain seconds into `f64` seconds.
fn parse_time_to_secs(s: &str) -> Result<f64, String> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => parts[0].parse::<f64>().map_err(|e| format!("invalid seconds '{s}': {e}")),
        2 => {
            let min: f64 = parts[0].parse().map_err(|e| format!("invalid minutes '{s}': {e}"))?;
            let sec: f64 = parts[1].parse().map_err(|e| format!("invalid seconds '{s}': {e}"))?;
            Ok(min * 60.0 + sec)
        }
        3 => {
            let hr: f64 = parts[0].parse().map_err(|e| format!("invalid hours '{s}': {e}"))?;
            let min: f64 = parts[1].parse().map_err(|e| format!("invalid minutes '{s}': {e}"))?;
            let sec: f64 = parts[2].parse().map_err(|e| format!("invalid seconds '{s}': {e}"))?;
            Ok(hr * 3600.0 + min * 60.0 + sec)
        }
        _ => Err(format!("invalid time format: {s}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_index_ranges() {
        assert_eq!(
            CustomRange::parse("0-10").unwrap(),
            CustomRange::IndexRange { start: Some(0), end: Some(10) }
        );
        assert_eq!(
            CustomRange::parse("10-").unwrap(),
            CustomRange::IndexRange { start: Some(10), end: None }
        );
        assert_eq!(
            CustomRange::parse("-99").unwrap(),
            CustomRange::IndexRange { start: None, end: Some(99) }
        );
    }

    #[test]
    fn test_parse_time_ranges() {
        assert_eq!(
            CustomRange::parse("05:00-20:00").unwrap(),
            CustomRange::TimeRange { start_secs: Some(300.0), end_secs: Some(1200.0) }
        );
        assert_eq!(
            CustomRange::parse("00:26-").unwrap(),
            CustomRange::TimeRange { start_secs: Some(26.0), end_secs: None }
        );
        assert_eq!(
            CustomRange::parse("-00:26").unwrap(),
            CustomRange::TimeRange { start_secs: None, end_secs: Some(26.0) }
        );
    }

    #[test]
    fn test_slice_segments() {
        let segs: Vec<MediaSegment> = (0..10).map(|i| MediaSegment {
            index: i,
            url: format!("http://example.com/seg_{i}.ts"),
            duration_secs: 2.0,
            start_time_secs: i as f64 * 2.0,
            byte_range: None,
            enc_info: None,
        }).collect();

        let range = CustomRange::parse("2-5").unwrap();
        let sliced = range.slice_segments(&segs);
        assert_eq!(sliced.len(), 4);
        assert_eq!(sliced[0].index, 2);
        assert_eq!(sliced[3].index, 5);

        let time_range = CustomRange::parse("00:04-00:08").unwrap();
        let time_sliced = time_range.slice_segments(&segs);
        // segments with start_time in range: seg 2 (4s-6s), seg 3 (6s-8s), seg 4 (8s-10s)
        assert_eq!(time_sliced.len(), 3);
        assert_eq!(time_sliced[0].index, 2);
    }
}
