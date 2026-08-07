use super::{Error, Span};

fn span(start: u64, end: u64) -> Span {
    match Span::new(start, end) {
        Ok(span) => span,
        Err(error) => panic!("test span {start}..{end} should be valid: {error}"),
    }
}

#[test]
fn span_reports_bounds_and_length() {
    assert_eq!(span(2, 6).start(), 2);
    assert_eq!(span(2, 6).end(), 6);
    assert_eq!(span(2, 6).len(), 4);
}

#[test]
fn empty_bounds_are_rejected() {
    assert_eq!(Span::new(3, 3), Err(Error::Empty { position: 3 }));
}

#[test]
fn reversed_bounds_are_rejected() {
    assert_eq!(Span::new(6, 2), Err(Error::Reversed { start: 6, end: 2 }));
}

#[test]
fn spans_convert_to_and_from_ranges() {
    assert_eq!(Span::try_from(2..6), Ok(span(2, 6)));
    assert_eq!(Span::try_from(4..4), Err(Error::Empty { position: 4 }));

    let reversed = std::ops::Range { start: 9, end: 4 };
    assert_eq!(
        Span::try_from(reversed),
        Err(Error::Reversed { start: 9, end: 4 })
    );

    assert_eq!(std::ops::Range::from(span(2, 6)), 2..6);
}

#[test]
fn spans_order_by_start_then_end() {
    assert!(span(1, 9) < span(2, 3));
    assert!(span(2, 3) < span(2, 4));
}

#[test]
fn errors_display_their_bounds() {
    assert_eq!(
        Error::Empty { position: 3 }.to_string(),
        "span at 3 is empty"
    );
    assert_eq!(
        Error::Reversed { start: 6, end: 2 }.to_string(),
        "span end 2 precedes its start 6"
    );
}

#[test]
fn segment_range_maps_contained_spans() {
    let Some(range) = span(102, 104).segment_range(100, 5) else {
        panic!("contained span should map");
    };

    assert_eq!(range.bounds(), 2..4);
    assert_eq!(range.len(), 2);
}

#[test]
fn segment_range_rejects_uncontained_spans() {
    // Starts before the segment.
    assert_eq!(span(99, 102).segment_range(100, 5), None);
    // Ends past the segment.
    assert_eq!(span(103, 106).segment_range(100, 5), None);
    // Segment end would overflow the address space.
    assert_eq!(span(0, 1).segment_range(u64::MAX, 2), None);
}
