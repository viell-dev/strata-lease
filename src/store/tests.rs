use super::{SegmentId, Store};
use crate::Span;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
fn copy_bytes_follows_fragmented_spans() {
    let mut store = Store::new();
    store.park(0, b"alpha".to_vec());
    store.park(5, b"beta".to_vec());

    assert_eq!(store.copy_bytes(Span::new(3, 7)), Some(b"habe".to_vec()));
    assert_eq!(store.copy_bytes(Span::new(0, 5)), Some(b"alpha".to_vec()));
    assert!(store.copy_bytes(Span::new(3, 20)).is_none());
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
fn empty_spans_resolve_everywhere() {
    let mut store = Store::new();

    // Against an empty store, including reversed spans.
    assert_eq!(store.bytes(Span::new(7, 7)), Some(&[][..]));
    assert_eq!(store.bytes(Span::new(9, 4)), Some(&[][..]));
    assert_eq!(store.copy_bytes(Span::new(7, 7)), Some(Vec::new()));
    assert_eq!(store.copy_bytes(Span::new(9, 4)), Some(Vec::new()));

    let Some(lease) = store.lease(Span::new(7, 7)) else {
        panic!("empty span should resolve");
    };
    assert!(lease.is_owned());
    assert!(lease.is_empty());

    let Some(parts) = store.leases(Span::new(9, 4)) else {
        panic!("reversed span should resolve as empty");
    };
    assert!(parts.is_empty());

    // In a gap between segments.
    let id = store.park(0, b"alpha".to_vec());
    store.park(10, b"omega".to_vec());

    assert_eq!(store.bytes(Span::new(7, 7)), Some(&[][..]));
    assert!(store.lease(Span::new(7, 7)).is_some());
    assert_eq!(store.live_bytes(id), Some(0));
}

#[test]
fn out_of_order_bases_resolve_single_segments_but_not_crossings() {
    let mut store = Store::new();
    store.park(5, b"beta".to_vec());
    store.park(0, b"alpha".to_vec());

    assert_eq!(store.bytes(Span::new(0, 5)), Some(b"alpha".as_slice()));
    assert_eq!(store.bytes(Span::new(5, 9)), Some(b"beta".as_slice()));

    // Cross-segment spans conservatively resolve to `None` instead of guessing.
    assert!(store.copy_bytes(Span::new(3, 7)).is_none());
    assert!(store.leases(Span::new(3, 7)).is_none());
}

#[test]
fn overlapping_bases_resolve_to_the_first_parked_segment() {
    let mut store = Store::new();
    store.park(0, b"aaaa".to_vec());
    store.park(2, b"bbbb".to_vec());

    assert_eq!(store.bytes(Span::new(2, 4)), Some(b"aa".as_slice()));
}

#[test]
fn unaddressable_base_overflow_resolves_to_none_without_panicking() {
    let mut store = Store::new();
    let id = store.park(u64::MAX.saturating_sub(2), b"abcdef".to_vec());
    let span = Span::new(u64::MAX.saturating_sub(2), u64::MAX);

    assert_eq!(store.bytes(span), None);
    assert!(store.lease(span).is_none());
    assert!(store.copy_bytes(span).is_none());
    assert_eq!(store.segment_span(id), None);
    assert_eq!(store.retained_len(), 6);
}

#[test]
fn segment_accessors_report_span_and_seal_state() {
    let mut store = Store::new();
    let id = store.park_open(3, b"hello".to_vec());

    assert_eq!(store.is_sealed(id), Some(false));
    assert_eq!(store.segment_span(id), Some(Span::new(3, 8)));

    assert!(store.seal(id));
    assert_eq!(store.is_sealed(id), Some(true));

    assert!(store.retire(id));
    assert_eq!(store.segment_span(id), None);
    assert_eq!(store.is_sealed(id), Some(true));
    assert_eq!(store.is_sealed(SegmentId(9)), None);
    assert_eq!(store.segment_span(SegmentId(9)), None);
}

#[test]
fn maintain_zero_threshold_retires_fully_loaned_segments() {
    let (mut store, id) = parked("0123456789");
    let Some(lease) = store.lease(Span::new(0, 10)) else {
        panic!("span should be retained");
    };

    assert_eq!(store.maintain(0), 1);
    assert_eq!(store.is_retired(id), Some(true));
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"0123456789");
}

#[test]
fn retiring_in_order_compacts_tombstones_and_keeps_ids_stable() {
    let mut store = Store::new();
    let first = store.park(0, b"aa".to_vec());
    let second = store.park(2, b"bb".to_vec());
    let third = store.park(4, b"cc".to_vec());

    assert!(store.retire(second));
    // A middle tombstone stays until everything ahead of it retires too.
    assert_eq!(store.segments.len(), 3);

    assert!(store.retire(first));
    // The front and the already retired middle compact away together.
    assert_eq!(store.segments.len(), 1);

    // Identifiers stay valid and answer with their settled state.
    assert_eq!(store.len(), 3);
    assert_eq!(store.is_retired(first), Some(true));
    assert_eq!(store.is_retired(second), Some(true));
    assert_eq!(store.is_retired(third), Some(false));
    assert_eq!(store.live_bytes(first), Some(0));
    assert_eq!(store.is_sealed(first), Some(true));
    assert!(store.seal(first));
    assert!(!store.retire(first));

    // New parks continue the identifier sequence past compacted history.
    let fourth = store.park(6, b"dd".to_vec());
    assert_eq!(fourth.index(), 3);
    assert_eq!(store.bytes(Span::new(4, 6)), Some(b"cc".as_slice()));
    assert_eq!(store.bytes(Span::new(6, 8)), Some(b"dd".as_slice()));
}

#[test]
fn streaming_park_and_retire_runs_in_bounded_memory() {
    let mut store = Store::new();

    for round in 0..100u64 {
        let id = store.park(round.saturating_mul(4), b"data".to_vec());
        assert!(store.retire(id));
        assert!(store.segments.len() <= 1);
    }

    assert_eq!(store.len(), 100);
    assert_eq!(store.retained_len(), 0);
}

#[test]
fn registry_stays_bounded_without_maintenance() {
    let mut store = Store::new();
    store.park(0, vec![b'x'; 64]);

    let churn = if cfg!(miri) { 500 } else { 10_000 };

    for _ in 0..churn {
        let lease = store.lease(Span::new(0, 8));
        assert!(lease.is_some());
        drop(lease);
    }

    let Some(entry) = store.segments.front() else {
        panic!("segment should be parked");
    };
    assert!(
        entry.leases.len() <= 8,
        "registry should stay bounded, grew to {}",
        entry.leases.len()
    );
}

#[test]
fn maintenance_prunes_promoted_leases_from_the_registry() {
    let mut store = Store::new();
    store.park(0, vec![b'x'; 64]);
    let Some(lease) = store.lease(Span::new(0, 8)) else {
        panic!("span should be retained");
    };

    lease.promote();

    // The threshold is unreachable, so nothing retires, but the sweep still prunes the
    // promoted lease's registration while the lease itself stays alive.
    assert_eq!(store.maintain(usize::MAX), 0);
    let Some(entry) = store.segments.front() else {
        panic!("segment should be parked");
    };
    assert!(entry.leases.is_empty());
    assert_eq!(&*lease.bytes(), b"xxxxxxxx");
}

#[test]
fn retire_completes_while_read_guards_are_held() {
    let (mut store, id) = parked("guarded-read");
    let Some(lease) = store.lease(Span::new(0, 7)) else {
        panic!("span should be retained");
    };
    let guard = lease.bytes();

    // Retirement must not block on the same-thread guard.
    assert!(store.retire(id));
    assert_eq!(store.retained_len(), 0);

    // The guard snapshot outlives retirement; fresh reads see the owned copy.
    assert_eq!(&*guard, b"guarded");
    drop(guard);
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"guarded");
}

/// Buffer whose `as_ref` panics while `poisoned` is set, emulating a faulty custom buffer.
struct FlakyBuffer {
    bytes: Vec<u8>,
    poisoned: Arc<AtomicBool>,
}

impl AsRef<[u8]> for FlakyBuffer {
    fn as_ref(&self) -> &[u8] {
        assert!(
            !self.poisoned.load(Ordering::Relaxed),
            "buffer read while poisoned"
        );
        &self.bytes
    }
}

#[test]
fn panicking_buffer_leaves_promotion_and_retirement_retryable() {
    let poisoned = Arc::new(AtomicBool::new(false));
    let mut store = Store::new();
    let id = store.park(
        0,
        FlakyBuffer {
            bytes: b"fragile".to_vec(),
            poisoned: Arc::clone(&poisoned),
        },
    );
    let Some(lease) = store.lease(Span::new(0, 4)) else {
        panic!("span should be retained");
    };

    poisoned.store(true, Ordering::Relaxed);

    // Consumer-side promotion panics but must not corrupt the lease or its accounting.
    assert!(catch_unwind(AssertUnwindSafe(|| lease.promote())).is_err());
    assert!(lease.is_loaned());
    assert_eq!(store.live_bytes(id), Some(4));

    // Owner-side retirement panics but must leave the segment retryable.
    assert!(catch_unwind(AssertUnwindSafe(|| store.retire(id))).is_err());
    assert_eq!(store.is_retired(id), Some(false));

    poisoned.store(false, Ordering::Relaxed);

    // After the fault clears, everything proceeds normally despite the poisoned lock.
    assert_eq!(&*lease.bytes(), b"frag");
    assert!(store.retire(id));
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"frag");
    assert_eq!(store.live_bytes(id), Some(0));
}

/// Buffer that violates the stability contract by shrinking to nothing on demand.
struct ShrinkingBuffer {
    bytes: Vec<u8>,
    shrunk: Arc<AtomicBool>,
}

impl AsRef<[u8]> for ShrinkingBuffer {
    fn as_ref(&self) -> &[u8] {
        if self.shrunk.load(Ordering::Relaxed) {
            &[]
        } else {
            &self.bytes
        }
    }
}

#[test]
fn unstable_buffer_degrades_to_empty_reads_without_panicking() {
    let shrunk = Arc::new(AtomicBool::new(false));
    let mut store = Store::new();
    let id = store.park(
        0,
        ShrinkingBuffer {
            bytes: b"unstable".to_vec(),
            shrunk: Arc::clone(&shrunk),
        },
    );
    let Some(lease) = store.lease(Span::new(0, 8)) else {
        panic!("span should be retained");
    };

    shrunk.store(true, Ordering::Relaxed);

    // The validated range no longer fits; reads clamp to empty instead of panicking.
    assert_eq!(&*lease.bytes(), b"");
    assert_eq!(lease.len(), 0);

    lease.promote();

    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"");
    assert_eq!(store.live_bytes(id), Some(0));
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
