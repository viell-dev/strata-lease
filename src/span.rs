//! Non-empty absolute byte spans.
//!
//! A [`Span`] addresses bytes by their position in a logical stream, independently of which parked
//! segment holds them. The [`Store`](crate::Store) maps a span onto its retained segments through
//! each segment's base offset.
//!
//! # Example
//!
//! ```
//! use strata_lease::{Span, SpanError};
//!
//! // Half-open bounds: 4 is included, 9 is not
//! let span = Span::new(4, 9).unwrap();
//! assert_eq!(span.start(), 4);
//! assert_eq!(span.end(), 9);
//! assert_eq!(span.len(), 5);
//!
//! // Empty and reversed bounds are rejected at construction
//! assert_eq!(Span::new(4, 4), Err(SpanError::Empty { position: 4 }));
//! assert_eq!(Span::new(9, 4), Err(SpanError::Reversed { start: 9, end: 4 }));
//! ```

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::ops::Range;

/// An error returned when bounds do not form a non-empty, forward span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanError {
    /// The start and end offsets are equal.
    Empty {
        /// The shared start and end offset.
        position: u64,
    },
    /// The end offset precedes the start offset.
    Reversed {
        /// The inclusive start offset.
        start: u64,
        /// The exclusive end offset.
        end: u64,
    },
}

impl Display for SpanError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { position } => write!(f, "span at {position} is empty"),
            Self::Reversed { start, end } => {
                write!(f, "span end {end} precedes its start {start}")
            }
        }
    }
}

impl Error for SpanError {}

/// A non-empty absolute byte span in a logical input stream.
///
/// Spans use half-open bounds: `start` is inclusive and `end` is exclusive. Construction ensures
/// `start < end`, so empty and reversed spans are unrepresentable. [`Span::new`] rejects such
/// bounds up front instead of deferring the problem to lookup time.
///
/// Spans order lexicographically by their start offset, then their end offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    /// Inclusive start offset.
    start: u64,
    /// Exclusive end offset.
    end: u64,
}

impl Span {
    /// Creates a span from inclusive `start` and exclusive `end` offsets.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, SpanError};
    /// let span = Span::new(2, 6).unwrap();
    /// assert_eq!(span.len(), 4);
    ///
    /// assert_eq!(Span::new(3, 3), Err(SpanError::Empty { position: 3 }));
    /// assert_eq!(Span::new(6, 2), Err(SpanError::Reversed { start: 6, end: 2 }));
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`SpanError::Empty`] when the offsets are equal and [`SpanError::Reversed`] when
    /// `end` precedes `start`.
    #[inline]
    pub const fn new(start: u64, end: u64) -> Result<Self, SpanError> {
        if start == end {
            Err(SpanError::Empty { position: start })
        } else if start > end {
            Err(SpanError::Reversed { start, end })
        } else {
            Ok(Self { start, end })
        }
    }

    /// Returns the inclusive start offset.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::Span;
    /// assert_eq!(Span::new(2, 6).unwrap().start(), 2);
    /// ```
    #[inline]
    pub const fn start(&self) -> u64 {
        self.start
    }

    /// Returns the exclusive end offset.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::Span;
    /// assert_eq!(Span::new(2, 6).unwrap().end(), 6);
    /// ```
    #[inline]
    pub const fn end(&self) -> u64 {
        self.end
    }

    /// Returns the number of bytes covered by the span, which is always at least one.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::Span;
    /// assert_eq!(Span::new(2, 6).unwrap().len(), 4);
    /// assert_eq!(Span::new(0, u64::MAX).unwrap().len(), u64::MAX);
    /// ```
    #[expect(
        clippy::len_without_is_empty,
        reason = "Spans are non-empty by construction"
    )]
    #[expect(clippy::arithmetic_side_effects, reason = "Safe by invariant")]
    #[inline]
    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Maps this span into `segment`-relative bounds when `segment` fully contains it.
    ///
    /// Returns `None` when this span reaches outside `segment`, or, on platforms narrower than
    /// 64 bits, when the relative offsets exceed `usize`. This is the only constructor of
    /// [`SegmentRange`], so every value that exists indexes a validated containment.
    pub(crate) fn segment_range(&self, segment: &Self) -> Option<SegmentRange> {
        if self.start < segment.start || self.end > segment.end {
            return None;
        }

        let start = usize::try_from(self.start.checked_sub(segment.start)?).ok()?;
        let end = usize::try_from(self.end.checked_sub(segment.start)?).ok()?;

        Some(SegmentRange { start, end })
    }
}

impl TryFrom<Range<u64>> for Span {
    type Error = SpanError;

    /// Converts a half-open range into a span, rejecting empty and reversed ranges.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, SpanError};
    /// assert_eq!(Span::try_from(2..6), Span::new(2, 6));
    /// assert_eq!(Span::try_from(4..4), Err(SpanError::Empty { position: 4 }));
    /// ```
    #[inline]
    fn try_from(range: Range<u64>) -> Result<Self, Self::Error> {
        Self::new(range.start, range.end)
    }
}

impl From<Span> for Range<u64> {
    /// Converts a span back into the half-open range it covers.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::ops::Range;
    /// # use strata_lease::Span;
    /// assert_eq!(Range::from(Span::new(2, 6).unwrap()), 2..6);
    /// ```
    #[inline]
    fn from(span: Span) -> Self {
        span.start..span.end
    }
}

/// A non-empty, forward byte range inside one retained segment, relative to the segment's base.
///
/// This is the segment-local counterpart of [`Span`]: `usize` slice indices instead of absolute
/// `u64` stream offsets. Values only come from [`Span::segment_range`], so `start < end` and
/// `end` is within the segment length measured at construction. Reads through it still clamp
/// defensively, because an unstable buffer may have shrunk since the containment was validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentRange {
    /// Inclusive start index.
    start: usize,
    /// Exclusive end index.
    end: usize,
}

impl SegmentRange {
    /// Returns the number of bytes the range covers, which is always at least one.
    #[expect(clippy::arithmetic_side_effects, reason = "Safe by invariant")]
    #[inline]
    pub(crate) const fn len(&self) -> usize {
        self.end - self.start
    }

    /// Returns the range as slice bounds.
    #[inline]
    pub(crate) const fn bounds(&self) -> Range<usize> {
        self.start..self.end
    }
}

#[cfg(test)]
pub(crate) mod tests;
