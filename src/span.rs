//! Non-empty absolute byte spans.

use std::error;
use std::fmt::{self, Display, Formatter};
use std::ops::Range;

/// An error returned when bounds do not form a non-empty, forward span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
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

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { position } => write!(f, "span at {position} is empty"),
            Self::Reversed { start, end } => {
                write!(f, "span end {end} precedes its start {start}")
            }
        }
    }
}

impl error::Error for Error {}

/// A non-empty absolute byte span in a logical input stream.
///
/// Spans address bytes independently of which parked segment holds them; the
/// [`Store`](crate::Store) maps a span onto retained segments via each segment's base offset.
///
/// Spans use half-open bounds: `start` is inclusive and `end` is exclusive. Construction ensures
/// `start < end`, so empty and reversed spans are unrepresentable: [`Span::new`] rejects such
/// bounds up front instead of deferring the problem to lookup time.
///
/// Spans order lexicographically by their start offset, then their end offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    start: u64,
    end: u64,
}

impl Span {
    /// Creates a span from inclusive `start` and exclusive `end` offsets.
    ///
    /// Returns [`Error::Empty`] when the offsets are equal and [`Error::Reversed`] when `end`
    /// precedes `start`.
    #[inline]
    pub const fn new(start: u64, end: u64) -> Result<Self, Error> {
        if start == end {
            Err(Error::Empty { position: start })
        } else if start > end {
            Err(Error::Reversed { start, end })
        } else {
            Ok(Self { start, end })
        }
    }

    /// Returns the inclusive start offset.
    #[inline]
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Returns the exclusive end offset.
    #[inline]
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Returns the number of bytes covered by the span; always at least one.
    #[inline]
    #[must_use]
    #[expect(
        clippy::len_without_is_empty,
        reason = "spans are non-empty by construction"
    )]
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "start < end by construction"
    )]
    pub const fn len(self) -> u64 {
        self.end - self.start
    }

    /// Maps this span into `segment`-relative bounds when `segment` fully contains it.
    ///
    /// Returns `None` when this span reaches outside `segment`, or — on platforms narrower than
    /// 64 bits — when the offsets exceed `usize`. This is the only constructor of
    /// [`SegmentRange`], so every value that exists indexes a validated containment.
    pub(crate) fn segment_range(self, segment: Self) -> Option<SegmentRange> {
        if self.start < segment.start || self.end > segment.end {
            return None;
        }

        let start = usize::try_from(self.start.checked_sub(segment.start)?).ok()?;
        let end = usize::try_from(self.end.checked_sub(segment.start)?).ok()?;

        Some(SegmentRange { start, end })
    }
}

/// A non-empty, forward byte range inside one retained segment, relative to the segment's base.
///
/// This is the segment-local counterpart of [`Span`]: `usize` slice indices instead of absolute
/// `u64` stream offsets. Values only come from [`Span::segment_range`], so `start < end` and
/// `end` is within the segment length measured at construction; unlike `Range<usize>` it is
/// `Copy`. Reads through it still clamp defensively, because an unstable buffer may have shrunk
/// since the containment was validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentRange {
    start: usize,
    end: usize,
}

impl SegmentRange {
    /// Returns the number of bytes the range covers; always at least one.
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "start < end by construction"
    )]
    pub(crate) const fn len(self) -> usize {
        self.end - self.start
    }

    /// Returns the range as slice bounds.
    pub(crate) const fn bounds(self) -> Range<usize> {
        self.start..self.end
    }
}

impl TryFrom<Range<u64>> for Span {
    type Error = Error;

    fn try_from(range: Range<u64>) -> Result<Self, Self::Error> {
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
