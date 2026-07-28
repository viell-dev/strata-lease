//! Absolute byte spans.

use std::ops::Range;

/// Absolute byte span in a logical input stream.
///
/// Spans address bytes independently of which parked segment holds them; the
/// [`Store`](crate::Store) maps a span onto retained segments via each segment's base offset.
///
/// A reversed span (`end < start`) is treated as empty everywhere: it has length zero, and every
/// store lookup resolves it exactly like an empty span. Spans order lexicographically by `start`,
/// then `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    /// Inclusive start byte offset.
    pub start: u64,
    /// Exclusive end byte offset.
    pub end: u64,
}

impl Span {
    /// Creates a new byte span.
    #[inline]
    #[must_use]
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// Returns the number of bytes the span covers.
    ///
    /// Reversed spans (`end < start`) have length zero.
    #[inline]
    #[must_use]
    pub const fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Returns `true` when the span covers no bytes.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }
}

impl From<Range<u64>> for Span {
    fn from(range: Range<u64>) -> Self {
        Self::new(range.start, range.end)
    }
}

impl From<Span> for Range<u64> {
    fn from(span: Span) -> Self {
        span.start..span.end
    }
}

#[cfg(test)]
mod tests;
