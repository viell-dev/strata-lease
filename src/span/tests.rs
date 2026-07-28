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
