//! Tests for the Span

#![expect(clippy::unwrap_used, reason = "Okay in tests")]

use super::*;
use std::error::Error;

/// Test helper to build a span that is known to be valid.
///
/// # Panics
///
/// Panics if the bounds are empty or reversed, which is a bug in the test itself.
pub(crate) fn span(start: u64, end: u64) -> Span {
    Span::new(start, end).unwrap()
}

// -----------------------------------------------------------------------------
// SpanError
// -----------------------------------------------------------------------------

#[test]
fn test_span_error_display() {
    // Each variant reports the offending bounds
    assert_eq!(
        SpanError::Empty { position: 3 }.to_string(),
        "span at 3 is empty"
    );
    assert_eq!(
        SpanError::Reversed { start: 6, end: 2 }.to_string(),
        "span end 2 precedes its start 6"
    );
}

#[test]
fn test_span_error_error() {
    let error: Box<dyn Error> = Box::new(SpanError::Empty { position: 3 });

    // The error is a leaf: it wraps no other error
    assert!(error.source().is_none());
}

// -----------------------------------------------------------------------------
// Span - Creation
// -----------------------------------------------------------------------------

#[test]
fn test_span_new() {
    // Forward bounds construct a span with the given offsets
    let span = Span::new(2, 6).unwrap();
    assert_eq!(span.start, 2);
    assert_eq!(span.end, 6);

    // A single byte is the smallest valid span
    let span = Span::new(5, 6).unwrap();
    assert_eq!(span.start, 5);
    assert_eq!(span.end, 6);

    // The full address space is a valid span
    let span = Span::new(0, u64::MAX).unwrap();
    assert_eq!(span.start, 0);
    assert_eq!(span.end, u64::MAX);

    // Equal bounds are rejected as empty
    assert_eq!(Span::new(3, 3), Err(SpanError::Empty { position: 3 }));
    assert_eq!(Span::new(0, 0), Err(SpanError::Empty { position: 0 }));

    // Reversed bounds are rejected, even by one
    assert_eq!(
        Span::new(6, 2),
        Err(SpanError::Reversed { start: 6, end: 2 })
    );
    assert_eq!(
        Span::new(3, 2),
        Err(SpanError::Reversed { start: 3, end: 2 })
    );
}

#[test]
fn test_span_try_from_range() {
    // Forward ranges convert
    assert_eq!(Span::try_from(2..6), Ok(span(2, 6)));

    // Empty and reversed ranges are rejected with the same errors as `new`
    assert_eq!(Span::try_from(4..4), Err(SpanError::Empty { position: 4 }));
    let reversed = Range { start: 9, end: 4 };
    assert_eq!(
        Span::try_from(reversed),
        Err(SpanError::Reversed { start: 9, end: 4 })
    );
}

// -----------------------------------------------------------------------------
// Span - Accessors
// -----------------------------------------------------------------------------

#[test]
fn test_span_start() {
    assert_eq!(span(2, 6).start(), 2);
    assert_eq!(span(0, 1).start(), 0);
}

#[test]
fn test_span_end() {
    assert_eq!(span(2, 6).end(), 6);
    assert_eq!(span(0, 1).end(), 1);
}

#[test]
fn test_span_len() {
    assert_eq!(span(2, 6).len(), 4);

    // The smallest span covers one byte
    assert_eq!(span(0, 1).len(), 1);

    // The widest span covers the whole address space
    assert_eq!(span(0, u64::MAX).len(), u64::MAX);
}

// -----------------------------------------------------------------------------
// Span - Conversions and ordering
// -----------------------------------------------------------------------------

#[test]
fn test_span_into_range() {
    // The range round-trips the half-open bounds
    assert_eq!(Range::from(span(2, 6)), 2..6);
    assert_eq!(Span::try_from(Range::from(span(2, 6))), Ok(span(2, 6)));
}

#[test]
fn test_span_ord() {
    // Ordering is by start offset first
    assert!(span(1, 9) < span(2, 3));

    // Then by end offset
    assert!(span(2, 3) < span(2, 4));

    // Equal spans are equal in every respect
    assert_eq!(span(2, 4), span(2, 4));
    assert_eq!(span(2, 4).cmp(&span(2, 4)), std::cmp::Ordering::Equal);
}

// -----------------------------------------------------------------------------
// Span - segment_range
// -----------------------------------------------------------------------------

#[test]
fn test_span_segment_range() {
    let segment = span(100, 105);

    // A contained span maps to bounds relative to the segment base
    let range = span(102, 104).segment_range(&segment).unwrap();
    assert_eq!(range.bounds(), 2..4);
    assert_eq!(range.len(), 2);

    // A segment maps onto itself completely
    let range = segment.segment_range(&segment).unwrap();
    assert_eq!(range.bounds(), 0..5);
    assert_eq!(range.len(), 5);

    // The first and last bytes map individually
    assert_eq!(
        span(100, 101).segment_range(&segment).unwrap().bounds(),
        0..1
    );
    assert_eq!(
        span(104, 105).segment_range(&segment).unwrap().bounds(),
        4..5
    );

    // Starting before the segment does not map
    assert_eq!(span(99, 102).segment_range(&segment), None);

    // Ending past the segment does not map
    assert_eq!(span(103, 106).segment_range(&segment), None);

    // A disjoint span does not map, on either side
    assert_eq!(span(0, 1).segment_range(&segment), None);
    assert_eq!(span(105, 106).segment_range(&segment), None);
}

// -----------------------------------------------------------------------------
// SegmentRange
// -----------------------------------------------------------------------------

#[test]
fn test_segment_range_len() {
    let range = SegmentRange { start: 2, end: 4 };

    assert_eq!(range.len(), 2);
}

#[test]
fn test_segment_range_bounds() {
    let range = SegmentRange { start: 2, end: 4 };

    assert_eq!(range.bounds(), 2..4);
}
