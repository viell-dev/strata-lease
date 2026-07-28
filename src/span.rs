//! Absolute byte spans.

/// Absolute byte span in a logical input stream.
///
/// Spans address bytes independently of which parked segment holds them; the
/// [`Store`](crate::Store) maps a span onto retained segments via each segment's base offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// Inclusive start byte offset.
    pub start: u64,
    /// Exclusive end byte offset.
    pub end: u64,
}

impl Span {
    /// Creates a new byte span.
    #[inline]
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// Returns the number of bytes the span covers.
    ///
    /// Reversed spans (`end < start`) have length zero.
    #[inline]
    pub const fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Returns `true` when the span covers no bytes.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests;
