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
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
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

    /// Returns the total number of bytes in this range.
    #[inline]
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
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
    /// Returns the parsed `ByteRange` and optional total file size.
    pub fn parse_content_range(header: &str) -> Result<(ByteRange, Option<u64>), RangeError> {
        let trimmed = header.trim();
        let stripped = trimmed
            .strip_prefix("bytes ")
            .ok_or_else(|| RangeError::InvalidHeader(trimmed.to_string()))?;

        let mut parts = stripped.split('/');
        let range_part = parts.next().ok_or_else(|| RangeError::InvalidHeader(trimmed.to_string()))?;
        let total_part = parts.next().ok_or_else(|| RangeError::InvalidHeader(trimmed.to_string()))?;

        let mut range_bounds = range_part.split('-');
        let start_str = range_bounds.next().ok_or_else(|| RangeError::InvalidHeader(trimmed.to_string()))?;
        let end_str = range_bounds.next().ok_or_else(|| RangeError::InvalidHeader(trimmed.to_string()))?;

        let start: u64 = start_str.parse().map_err(|_| RangeError::InvalidHeader(trimmed.to_string()))?;
        let end: u64 = end_str.parse().map_err(|_| RangeError::InvalidHeader(trimmed.to_string()))?;

        let total = if total_part == "*" {
            None
        } else {
            Some(total_part.parse().map_err(|_| RangeError::InvalidHeader(trimmed.to_string()))?)
        };

        Ok((ByteRange::new(start, end)?, total))
    }

    /// Converts to standard Rust `RangeInclusive<u64>`.
    #[inline]
    pub fn as_inclusive_range(&self) -> RangeInclusive<u64> {
        self.start..=self.end
    }
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
    }
}
