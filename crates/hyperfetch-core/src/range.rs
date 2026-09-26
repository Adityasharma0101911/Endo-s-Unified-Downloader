use std::fmt;
use std::ops::RangeInclusive;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum RangeError {
    #[error("Invalid range: start ({0}) is greater than end ({1})")]
    InvalidRange(u64, u64),
    #[error("Empty range cannot be split")]
    EmptyRange,
    #[error("Split point {0} is outside range [{1}, {2}]")]
    SplitOutOfBounds(u64, u64, u64),
    #[error("Invalid Content-Range header: {0}")]
    InvalidHeader(String),
}

/// Represents an inclusive byte range [start, end].
/// E.g., `ByteRange::new(0, 999)` contains 1000 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawByteRange")]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

/// Unvalidated wire form, so deserialized ranges go through `ByteRange::new`.
#[derive(Deserialize)]
struct RawByteRange {
    start: u64,
    end: u64,
}

impl TryFrom<RawByteRange> for ByteRange {
    type Error = RangeError;

    fn try_from(raw: RawByteRange) -> Result<Self, Self::Error> {
        ByteRange::new(raw.start, raw.end)
    }
}

impl ByteRange {
    /// Creates a new inclusive ByteRange [start, end].
    /// Returns an error if start > end.
    pub fn new(start: u64, end: u64) -> Result<Self, RangeError> {
        if start > end {
            return Err(RangeError::InvalidRange(start, end));
        }
        Ok(Self { start, end })
    }

    /// Creates a new ByteRange from an offset and length in bytes.
    pub fn from_len(start: u64, len: u64) -> Result<Self, RangeError> {
        if len == 0 {
            return Err(RangeError::EmptyRange);
        }
        let end = start.checked_add(len - 1).ok_or(RangeError::InvalidRange(start, u64::MAX))?;
        Ok(Self { start, end })
    }

    /// Returns the total number of bytes in this range (saturating at `u64::MAX` for `[0, u64::MAX]`).
    #[inline]
    pub fn len(&self) -> u64 {
        (self.end - self.start).saturating_add(1)
    }

    /// Returns true if length is 0 (which is impossible for valid ranges, but provided for convention).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start > self.end
    }

    /// Checks whether an offset is contained in this range.
    #[inline]
    pub fn contains(&self, offset: u64) -> bool {
        offset >= self.start && offset <= self.end
    }

    /// Checks whether another range is completely contained within this range.
    #[inline]
    pub fn contains_range(&self, other: &ByteRange) -> bool {
        self.start <= other.start && self.end >= other.end
    }

    /// Checks whether this range overlaps with another range.
    #[inline]
    pub fn intersects(&self, other: &ByteRange) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    /// Returns the overlapping range if they intersect.
    pub fn intersection(&self, other: &ByteRange) -> Option<ByteRange> {
        if self.intersects(other) {
            let start = self.start.max(other.start);
            let end = self.end.min(other.end);
            Some(ByteRange { start, end })
        } else {
            None
        }
    }

    /// Splits this range into two non-overlapping ranges at `split_point`.
    /// The first range will be [start, split_point - 1] and the second will be [split_point, end].
    pub fn split_at(&self, split_point: u64) -> Result<(ByteRange, ByteRange), RangeError> {
        if split_point <= self.start || split_point > self.end {
            return Err(RangeError::SplitOutOfBounds(split_point, self.start, self.end));
        }
        let first = ByteRange::new(self.start, split_point - 1)?;
        let second = ByteRange::new(split_point, self.end)?;
        Ok((first, second))
    }

    /// Splits this range at its midpoint. Useful for work stealing.
    /// E.g. [0, 9] (len 10) -> midpoint is 5: [0, 4] and [5, 9].
    pub fn split_midpoint(&self) -> Result<(ByteRange, ByteRange), RangeError> {
        if self.len() < 2 {
            return Err(RangeError::EmptyRange);
        }
        let mid = self.start + (self.len() / 2);
        self.split_at(mid)
    }

    /// Converts into a standard HTTP `Range: bytes=start-end` header value.
    pub fn to_http_header(&self) -> String {
        format!("bytes={}-{}", self.start, self.end)
    }

    /// Parses an HTTP `Content-Range` header (e.g. `bytes 0-999/5000` or `bytes 0-999/*`).
    /// Returns the parsed `ByteRange` and optional total file size. Anything that is not exactly
    /// `bytes <digits>-<digits>/<digits or *>` with `start <= end < total` is rejected.
    pub fn parse_content_range(header: &str) -> Result<(ByteRange, Option<u64>), RangeError> {
        let invalid = || RangeError::InvalidHeader(header.to_string());
        let number = |s: &str| -> Result<u64, RangeError> {
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            s.parse().map_err(|_| invalid())
        };

        let (unit, spec) = header.trim().split_once(' ').ok_or_else(invalid)?;
        if !unit.eq_ignore_ascii_case("bytes") {
            return Err(invalid());
        }
        let (range_part, total_part) = spec.trim().split_once('/').ok_or_else(invalid)?;
        let (start_str, end_str) = range_part.split_once('-').ok_or_else(invalid)?;

        let range = ByteRange::new(number(start_str)?, number(end_str)?)?;
        let total = if total_part == "*" { None } else { Some(number(total_part)?) };
        if total.is_some_and(|t| range.end >= t) {
            return Err(invalid());
        }
        Ok((range, total))
    }

    /// Converts to standard Rust `RangeInclusive<u64>`.
    #[inline]
    pub fn as_inclusive_range(&self) -> RangeInclusive<u64> {
        self.start..=self.end
    }

    /// Merges two ranges if they overlap or are directly contiguous.
    pub fn merge(&self, other: &ByteRange) -> Option<ByteRange> {
        if self.intersects(other) || self.end.saturating_add(1) == other.start || other.end.saturating_add(1) == self.start {
            Some(ByteRange {
                start: self.start.min(other.start),
                end: self.end.max(other.end),
            })
        } else {
            None
        }
    }
}

/// Merges a list of byte ranges into contiguous, non-overlapping ranges.
pub fn merge_ranges(mut ranges: Vec<ByteRange>) -> Vec<ByteRange> {
    if ranges.is_empty() {
        return Vec::new();
    }
    ranges.sort();
    let mut merged = Vec::with_capacity(ranges.len());
    let mut current = ranges[0];

    for next in ranges.into_iter().skip(1) {
        if let Some(m) = current.merge(&next) {
            current = m;
        } else {
            merged.push(current);
            current = next;
        }
    }
    merged.push(current);
    merged
}

/// Computes the missing gap ranges in `[0, total_size - 1]` that are not covered by `completed`.
pub fn compute_gaps(total_size: u64, completed: &[ByteRange]) -> Vec<ByteRange> {
    if total_size == 0 {
        return Vec::new();
    }
    let merged = merge_ranges(completed.to_vec());
    let mut gaps = Vec::new();
    let mut cursor = 0u64;

    for range in &merged {
        if range.start >= total_size {
            break;
        }
        if range.start > cursor {
            if let Ok(gap) = ByteRange::new(cursor, range.start - 1) {
                gaps.push(gap);
            }
        }
        cursor = (range.end.saturating_add(1)).max(cursor);
        if cursor >= total_size {
            break;
        }
    }

    if cursor < total_size {
        if let Ok(gap) = ByteRange::new(cursor, total_size - 1) {
            gaps.push(gap);
        }
    }

    gaps
}

impl fmt::Display for ByteRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{} ({} B)", self.start, self.end, self.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_range() {
        let r = ByteRange::new(0, 99).unwrap();
        assert_eq!(r.len(), 100);
        assert_eq!(r.to_http_header(), "bytes=0-99");
        assert!(r.contains(0));
        assert!(r.contains(99));
        assert!(!r.contains(100));
    }

    #[test]
    fn test_from_len() {
        let r = ByteRange::from_len(100, 50).unwrap();
        assert_eq!(r.start, 100);
        assert_eq!(r.end, 149);
        assert_eq!(r.len(), 50);
    }

    #[test]
    fn test_invalid_range() {
        assert_eq!(ByteRange::new(100, 50), Err(RangeError::InvalidRange(100, 50)));
        assert_eq!(ByteRange::from_len(0, 0), Err(RangeError::EmptyRange));
    }

    #[test]
    fn test_intersection() {
        let r1 = ByteRange::new(0, 100).unwrap();
        let r2 = ByteRange::new(50, 150).unwrap();
        let r3 = ByteRange::new(101, 200).unwrap();

        assert!(r1.intersects(&r2));
        assert_eq!(r1.intersection(&r2), Some(ByteRange::new(50, 100).unwrap()));
        assert!(!r1.intersects(&r3));
        assert_eq!(r1.intersection(&r3), None);
    }

    #[test]
    fn test_split_midpoint() {
        let r = ByteRange::new(0, 9).unwrap(); // len 10
        let (first, second) = r.split_midpoint().unwrap();
        assert_eq!(first, ByteRange::new(0, 4).unwrap());
        assert_eq!(second, ByteRange::new(5, 9).unwrap());
        assert_eq!(first.len() + second.len(), r.len());

        let r_odd = ByteRange::new(0, 10).unwrap(); // len 11
        let (first_odd, second_odd) = r_odd.split_midpoint().unwrap();
        assert_eq!(first_odd, ByteRange::new(0, 4).unwrap());
        assert_eq!(second_odd, ByteRange::new(5, 10).unwrap());
        assert_eq!(first_odd.len() + second_odd.len(), r_odd.len());
    }

    #[test]
    fn test_parse_content_range() {
        let (range, total) = ByteRange::parse_content_range("bytes 0-499/1234").unwrap();
        assert_eq!(range, ByteRange::new(0, 499).unwrap());
        assert_eq!(total, Some(1234));

        let (range_star, total_star) = ByteRange::parse_content_range("bytes 100-200/*").unwrap();
        assert_eq!(range_star, ByteRange::new(100, 200).unwrap());
        assert_eq!(total_star, None);

        assert!(ByteRange::parse_content_range("invalid").is_err());
        assert_eq!(ByteRange::parse_content_range("Bytes 5-9/10").unwrap().1, Some(10));
    }

    #[test]
    fn test_parse_content_range_is_strict() {
        for bad in [
            "bytes +0-499/1234",   // sign accepted by u64::from_str
            "bytes 0-499/1234/5",  // trailing garbage
            "bytes 0-4-9/1234",    // extra dash
            "bytes 0-1234/1234",   // end beyond total
            "bytes 500-499/1234",  // start > end
            "bytes */1234",        // unsatisfied-range form carries no range
            "bytes 0-/1234",
            "items 0-499/1234",
        ] {
            assert!(ByteRange::parse_content_range(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn test_len_does_not_overflow_at_u64_max() {
        assert_eq!(ByteRange::new(0, u64::MAX).unwrap().len(), u64::MAX);
        assert_eq!(ByteRange::new(u64::MAX, u64::MAX).unwrap().len(), 1);
    }

    #[test]
    fn test_deserialize_rejects_inverted_range() {
        let good = bincode::serialize(&(10u64, 20u64)).unwrap();
        assert_eq!(bincode::deserialize::<ByteRange>(&good).unwrap(), ByteRange::new(10, 20).unwrap());
        let bad = bincode::serialize(&(20u64, 10u64)).unwrap();
        assert!(bincode::deserialize::<ByteRange>(&bad).is_err());
        assert!(serde_json::from_str::<ByteRange>(r#"{"start":5,"end":1}"#).is_err());
    }

    #[test]
    fn test_merge_and_merge_ranges() {
        let r1 = ByteRange::new(0, 99).unwrap();
        let r2 = ByteRange::new(100, 199).unwrap();
        let r3 = ByteRange::new(250, 300).unwrap();
        let r4 = ByteRange::new(280, 400).unwrap();

        assert_eq!(r1.merge(&r2), Some(ByteRange::new(0, 199).unwrap()));
        assert_eq!(r3.merge(&r4), Some(ByteRange::new(250, 400).unwrap()));
        assert_eq!(r1.merge(&r3), None);

        let merged = merge_ranges(vec![r3, r1, r4, r2]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], ByteRange::new(0, 199).unwrap());
        assert_eq!(merged[1], ByteRange::new(250, 400).unwrap());
    }

    #[test]
    fn test_compute_gaps() {
        let total = 1000u64;
        let completed = vec![
            ByteRange::new(100, 199).unwrap(),
            ByteRange::new(400, 599).unwrap(),
        ];
        let gaps = compute_gaps(total, &completed);
        assert_eq!(gaps.len(), 3);
        assert_eq!(gaps[0], ByteRange::new(0, 99).unwrap());
        assert_eq!(gaps[1], ByteRange::new(200, 399).unwrap());
        assert_eq!(gaps[2], ByteRange::new(600, 999).unwrap());

        // Entirely completed
        let all_done = vec![ByteRange::new(0, 999).unwrap()];
        assert!(compute_gaps(total, &all_done).is_empty());

        // Nothing completed
        let none_done: Vec<ByteRange> = Vec::new();
        let gaps_none = compute_gaps(total, &none_done);
        assert_eq!(gaps_none.len(), 1);
        assert_eq!(gaps_none[0], ByteRange::new(0, 999).unwrap());
    }
}
