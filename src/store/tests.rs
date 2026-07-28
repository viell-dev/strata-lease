use super::{SegmentId, Store};
use crate::Span;

fn parked(text: &str) -> (Store, SegmentId) {
    let mut store = Store::new();
    let id = store.park(0, text.as_bytes().to_vec());

    (store, id)
}

#[test]
fn parked_segment_resolves_span_bytes() {
    let (store, _) = parked("key=value\n");

    assert_eq!(store.len(), 1);
    assert!(!store.is_empty());
    assert_eq!(store.retained_len(), 10);
    assert_eq!(store.bytes(Span::new(4, 9)), Some(b"value".as_slice()));
    assert_eq!(store.bytes(Span::new(4, 20)), None);
}

#[test]
fn segment_bases_offset_span_lookup() {
    let mut store = Store::new();
    store.park(100, b"based".to_vec());

    assert_eq!(store.bytes(Span::new(101, 104)), Some(b"ase".as_slice()));
    assert_eq!(store.bytes(Span::new(99, 102)), None);
}

#[test]
fn bytes_requires_a_single_containing_segment() {
    let mut store = Store::new();
    store.park(0, b"alpha".to_vec());
    store.park(5, b"beta".to_vec());

    assert_eq!(store.bytes(Span::new(3, 7)), None);
    assert_eq!(store.bytes(Span::new(5, 9)), Some(b"beta".as_slice()));
}

#[test]
fn lease_pins_live_bytes_until_dropped() {
    let (mut store, id) = parked("0123456789");
    let Some(lease) = store.lease(Span::new(2, 6)) else {
        panic!("span should be retained");
    };

    assert!(lease.is_loaned());
    assert_eq!(&*lease.bytes(), b"2345");
    assert_eq!(store.live_bytes(id), Some(4));

    drop(lease);

    assert_eq!(store.live_bytes(id), Some(0));
}

#[test]
fn cloned_leases_share_one_pin() {
    let (mut store, id) = parked("0123456789");
    let Some(lease) = store.lease(Span::new(0, 3)) else {
        panic!("span should be retained");
    };
    let clone = lease.clone();

    assert_eq!(store.live_bytes(id), Some(3));

    drop(lease);

    assert_eq!(store.live_bytes(id), Some(3));
    assert_eq!(&*clone.bytes(), b"012");

    drop(clone);

    assert_eq!(store.live_bytes(id), Some(0));
}

#[test]
fn consumer_promotion_unpins_without_retiring() {
    let (mut store, id) = parked("consumer");
    let Some(lease) = store.lease(Span::new(0, 3)) else {
        panic!("span should be retained");
    };

    lease.promote();

    assert!(lease.is_owned());
    assert_eq!(store.live_bytes(id), Some(0));
    assert_eq!(store.retained_len(), 8);
    assert_eq!(&*lease.bytes(), b"con");
}

#[test]
fn retire_promotes_stragglers_and_releases_bytes() {
    let (mut store, id) = parked("prefix:value:suffix");
    let Some(value) = store.lease(Span::new(7, 12)) else {
        panic!("span should be retained");
    };
    let Some(suffix) = store.lease(Span::new(13, 19)) else {
        panic!("span should be retained");
    };

    assert!(store.retire(id));

    assert!(value.is_owned());
    assert!(suffix.is_owned());
    assert_eq!(&*value.bytes(), b"value");
    assert_eq!(&*suffix.bytes(), b"suffix");
    assert_eq!(store.retained_len(), 0);
    assert_eq!(store.live_bytes(id), Some(0));
    assert_eq!(store.is_retired(id), Some(true));
    assert_eq!(store.bytes(Span::new(7, 12)), None);
    assert!(!store.retire(id));
}

#[test]
fn retire_all_reports_and_empties() {
    let mut store = Store::new();
    store.park(0, b"alpha".to_vec());
    store.park(5, b"beta".to_vec());

    assert_eq!(store.retire_all(), 2);
    assert_eq!(store.retained_len(), 0);
    assert_eq!(store.retire_all(), 0);
}

#[test]
fn maintain_respects_waste_threshold() {
    let (mut store, id) = parked("0123456789");
    let Some(lease) = store.lease(Span::new(2, 4)) else {
        panic!("span should be retained");
    };

    assert_eq!(store.maintain(9), 0);
    assert!(lease.is_loaned());
    assert_eq!(store.is_retired(id), Some(false));

    assert_eq!(store.maintain(8), 1);
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"23");
    assert_eq!(store.retained_len(), 0);
}

#[test]
fn maintain_counts_dropped_leases_as_waste() {
    let (mut store, _) = parked("0123456789");
    let Some(lease) = store.lease(Span::new(0, 10)) else {
        panic!("span should be retained");
    };

    assert_eq!(store.maintain(1), 0);

    drop(lease);

    assert_eq!(store.maintain(1), 1);
}

#[test]
fn maintain_skips_open_segments_until_sealed() {
    let mut store = Store::new();
    let id = store.park_open(0, b"open".to_vec());

    assert_eq!(store.maintain(0), 0);
    assert_eq!(store.is_retired(id), Some(false));

    assert!(store.seal(id));

    assert_eq!(store.maintain(0), 1);
    assert_eq!(store.is_retired(id), Some(true));
}

#[test]
fn seal_of_unknown_segment_is_rejected() {
    let mut store = Store::<Vec<u8>>::new();

    assert!(!store.seal(SegmentId(7)));
    assert!(!store.retire(SegmentId(7)));
    assert_eq!(store.live_bytes(SegmentId(7)), None);
    assert_eq!(store.is_retired(SegmentId(7)), None);
}

#[test]
fn leases_can_cross_segment_boundaries() {
    let mut store = Store::new();
    let first = store.park(0, b"alpha".to_vec());
    let second = store.park(5, b"beta".to_vec());
    let Some(parts) = store.leases(Span::new(3, 7)) else {
        panic!("cross-segment span should be retained");
    };
    let [head, tail] = parts.as_slice() else {
        panic!("expected one lease per segment");
    };

    assert_eq!(&*head.bytes(), b"ha");
    assert_eq!(&*tail.bytes(), b"be");
    assert_eq!(store.live_bytes(first), Some(2));
    assert_eq!(store.live_bytes(second), Some(2));

    store.retire_all();

    assert_eq!(&*head.bytes(), b"ha");
    assert_eq!(&*tail.bytes(), b"be");
}

#[test]
fn leases_with_a_gap_resolve_to_none_and_unpin() {
    let mut store = Store::new();
    let first = store.park(0, b"alpha".to_vec());
    store.park(10, b"omega".to_vec());

    assert!(store.leases(Span::new(3, 12)).is_none());
    assert_eq!(store.live_bytes(first), Some(0));
}

#[test]
fn dropping_the_store_keeps_loaned_leases_readable() {
    let mut store = Store::new();
    store.park(0, b"outlives".to_vec());
    let Some(lease) = store.lease(Span::new(0, 8)) else {
        panic!("span should be retained");
    };

    drop(store);

    assert!(lease.is_loaned());
    assert_eq!(&*lease.bytes(), b"outlives");

    lease.promote();

    assert_eq!(&*lease.bytes(), b"outlives");
}
