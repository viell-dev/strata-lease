use super::Span;

#[test]
fn span_reports_length_and_emptiness() {
    assert_eq!(Span::new(2, 6).len(), 4);
    assert!(!Span::new(2, 6).is_empty());
    assert!(Span::new(3, 3).is_empty());
}

#[test]
fn reversed_span_length_saturates_to_zero() {
    assert_eq!(Span::new(6, 2).len(), 0);
    assert!(Span::new(6, 2).is_empty());
}

#[test]
fn spans_convert_to_and_from_ranges() {
    assert_eq!(Span::from(2..6), Span::new(2, 6));
    assert_eq!(std::ops::Range::from(Span::new(2, 6)), 2..6);
}

#[test]
fn spans_order_by_start_then_end() {
    assert!(Span::new(1, 9) < Span::new(2, 3));
    assert!(Span::new(2, 3) < Span::new(2, 4));
}
