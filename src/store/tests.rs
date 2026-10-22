//! Tests for the Store

#![expect(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "Okay in tests"
)]

use super::*;
use crate::span::tests::span;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Iteration budget that keeps Miri runs tractable while stressing native runs harder.
const fn budget(miri: usize, native: usize) -> usize {
    if cfg!(miri) { miri } else { native }
}

/// A buffer whose `as_ref` panics while `poisoned` is set, emulating a faulty custom buffer.
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

/// A buffer that violates the stability contract by shrinking to nothing on demand.
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

/// A buffer type from outside the crate, boxed instead of a vector.
struct Region {
    bytes: Box<[u8]>,
}

impl AsRef<[u8]> for Region {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// Test helper to park `text` at base 0 in a fresh store.
fn parked(text: &str) -> (Store, SegmentId) {
    let mut store = Store::new();
    let id = store.park(0, text.as_bytes().to_vec()).unwrap();

    (store, id)
}

/// Test helper to build the identifier `index` would have in any store.
const fn segment_id(index: u64) -> SegmentId {
    SegmentId(index)
}

/// Test helper counting the registry entries pruning visits while `kept` leases stay loaned and
/// one more is created `rounds` times, then dropped or promoted and kept alive.
///
/// `Retained::lease` prunes exactly when it registers into a full registry, and a prune visits
/// every registration, so the registry length at those moments sums to the visits.
#[expect(clippy::arithmetic_side_effects, reason = "Bounded test sizes")]
fn prune_visits(kept: usize, rounds: usize, promote: bool) -> usize {
    let mut store = Store::new();
    store.park(0, vec![0; kept + 1]).unwrap();
    let held: Vec<_> = (0..u64::try_from(kept).unwrap())
        .map(|index| store.lease(span(index, index + 1)).unwrap())
        .collect();
    let tail = u64::try_from(kept).unwrap();
    let range = span(tail, tail + 1)
        .segment_range(&span(0, tail + 1))
        .unwrap();
    let retained = store.segments[0].retained_mut().unwrap();
    let mut promoted = Vec::new();
    let mut visits = 0;

    for _ in 0..rounds {
        if retained.leases.len() == retained.leases.capacity() {
            visits += retained.leases.len();
        }

        let lease = retained.lease(range);
        if promote {
            lease.promote();
            promoted.push(lease);
        }
    }

    // However the churned leases end, the registry stays proportional to the loaned ones
    assert!(retained.leases.capacity() <= 4 * (kept + 1));
    assert_eq!(store.live_bytes(segment_id(0)), Some(kept));
    drop(held);

    visits
}

// -----------------------------------------------------------------------------
// SegmentId
// -----------------------------------------------------------------------------

#[test]
fn test_segment_id_index() {
    let mut store = Store::new();

    // Identifiers count parks from zero
    assert_eq!(store.park(0, b"a".to_vec()).unwrap().index(), 0);
    assert_eq!(store.park(1, b"b".to_vec()).unwrap().index(), 1);
    assert_eq!(segment_id(7).index(), 7);
}

#[test]
fn test_segment_id_ord() {
    // Identifiers order by park order
    assert!(segment_id(0) < segment_id(1));
    assert_eq!(segment_id(3), segment_id(3));
}

/* Coverage note: `SegmentStatus` is a plain enum reported by `Store::status`.
 * Its behavior is covered by the `status` tests.
 */

// -----------------------------------------------------------------------------
// ParkErrorKind and ParkError
// -----------------------------------------------------------------------------

#[test]
fn test_park_error_kind_display() {
    // Each variant reports the rejected values
    assert_eq!(
        ParkErrorKind::Empty.to_string(),
        "an empty buffer cannot be parked"
    );
    assert_eq!(
        ParkErrorKind::OutOfOrder {
            base: 3,
            min_base: 5
        }
        .to_string(),
        "base 3 starts before the end 5 of the previously parked segment"
    );
    assert_eq!(
        ParkErrorKind::Unaddressable { base: 9, len: 2 }.to_string(),
        "segment at base 9 with 2 bytes extends past the u64 address space"
    );
}

#[test]
fn test_park_error_kind() {
    let error = ParkError {
        kind: ParkErrorKind::Empty,
        bytes: Vec::<u8>::new(),
    };

    assert_eq!(error.kind(), ParkErrorKind::Empty);
}

#[test]
fn test_park_error_into_bytes() {
    let error = ParkError {
        kind: ParkErrorKind::Empty,
        bytes: b"returned".to_vec(),
    };

    // The rejected buffer comes back intact
    assert_eq!(error.into_bytes(), b"returned");
}

#[test]
fn test_park_error_debug() {
    let error = ParkError {
        kind: ParkErrorKind::Empty,
        bytes: b"hidden".to_vec(),
    };

    // Only the kind is printed, marked non-exhaustive with a trailing `..`
    assert_eq!(format!("{error:?}"), "ParkError { kind: Empty, .. }");
    assert!(!format!("{error:#?}").contains("hidden"));

    // `B` needs no `Debug` implementation
    let error = ParkError {
        kind: ParkErrorKind::Empty,
        bytes: Region {
            bytes: Box::from(b"".as_slice()),
        },
    };
    assert_eq!(format!("{error:?}"), "ParkError { kind: Empty, .. }");
}

#[test]
fn test_park_error_display() {
    let error = ParkError {
        kind: ParkErrorKind::OutOfOrder {
            base: 3,
            min_base: 5,
        },
        bytes: Vec::<u8>::new(),
    };

    // The message is the kind's message
    assert_eq!(error.to_string(), error.kind().to_string());
}

#[test]
fn test_park_error_error() {
    let error: Box<dyn Error> = Box::new(ParkError {
        kind: ParkErrorKind::Empty,
        bytes: Vec::<u8>::new(),
    });

    // The error is a leaf: it wraps no other error
    assert!(error.source().is_none());
}

// -----------------------------------------------------------------------------
// Segment
// -----------------------------------------------------------------------------

#[test]
fn test_segment_retained() {
    let (mut store, id) = parked("segment");

    // A retained segment exposes its state, both ways
    assert!(store.segments[0].retained().is_some());
    assert!(store.segments[0].retained_mut().is_some());
    assert!(!store.segments[0].is_retired());

    store.segments[0].state = State::Retired;

    // A retired one does not
    assert!(store.segments[0].retained().is_none());
    assert!(store.segments[0].retained_mut().is_none());
    assert!(store.segments[0].is_retired());
    assert_eq!(store.status(id), Some(SegmentStatus::Retired));
}

#[test]
fn test_segment_len() {
    let mut store = Store::new();
    store.park(100, b"0123456789".to_vec()).unwrap();

    // The length comes from the span, not the base
    assert_eq!(store.segments[0].len(), 10);

    // And survives retirement
    store.segments[0].state = State::Retired;
    assert_eq!(store.segments[0].len(), 10);
}

#[test]
fn test_segment_debug() {
    let (mut store, id) = parked("debug");
    let lease = store.lease(span(0, 3)).unwrap();

    // Bookkeeping is printed; the bytes are not
    assert_eq!(
        format!("{:?}", store.segments[0]),
        "Segment { span: Span { start: 0, end: 5 }, status: Sealed, registered_leases: 1, \
         live_bytes: 3, .. }"
    );
    assert!(!format!("{:#?}", store.segments[0]).contains("debug"));

    // The status reflects the seal flag
    store.segments[0].retained_mut().unwrap().sealed = false;
    assert!(format!("{:?}", store.segments[0]).contains("status: Open"));

    // A tombstone prints only its span and status
    drop(lease);
    store.retire(id);
    store.segments.push_back(Segment {
        span: span(5, 6),
        state: State::Retired,
    });
    assert_eq!(
        format!("{:?}", store.segments[0]),
        "Segment { span: Span { start: 5, end: 6 }, status: Retired }"
    );
}

// -----------------------------------------------------------------------------
// Retained
// -----------------------------------------------------------------------------

#[test]
fn test_retained_prune() {
    let (mut store, _) = parked("0123456789");
    let live = store.lease(span(0, 2)).unwrap();
    let dropped = store.lease(span(2, 4)).unwrap();
    let promoted = store.lease(span(4, 6)).unwrap();
    drop(dropped);
    promoted.promote();

    let retained = store.segments[0].retained_mut().unwrap();
    assert_eq!(retained.leases.len(), 3);

    retained.prune();

    // Only the live loaned lease keeps its registration
    assert_eq!(retained.leases.len(), 1);
    assert!(!retained.leases[0].upgrade().unwrap().is_promoted());
    drop(live);
    assert!(retained.leases[0].upgrade().is_none());
}

#[test]
fn test_retained_lease() {
    let (mut store, id) = parked("0123456789");
    let range = span(2, 6).segment_range(&span(0, 10)).unwrap();

    // Leasing pins the range and registers the lease
    let lease = store.segments[0].retained_mut().unwrap().lease(range);
    assert_eq!(&*lease.bytes(), b"2345");
    assert_eq!(store.live_bytes(id), Some(4));
    assert_eq!(store.segments[0].retained().unwrap().leases.len(), 1);

    // The registry only prunes when it is about to grow
    drop(lease);
    let retained = store.segments[0].retained_mut().unwrap();
    let capacity = retained.leases.capacity();
    while retained.leases.len() < capacity {
        drop(retained.lease(range));
    }
    assert_eq!(retained.leases.len(), capacity);

    // At capacity, the dead registrations are pruned before pushing instead of growing
    let lease = retained.lease(range);
    assert_eq!(retained.leases.len(), 1);
    assert_eq!(retained.leases.capacity(), capacity);
    assert!(retained.leases[0].upgrade().is_some());
    drop(lease);
    assert!(retained.leases[0].upgrade().is_none());

    // So churn without maintenance keeps the registry bounded
    for _ in 0..budget(500, 10_000) {
        drop(store.lease(span(0, 8)).unwrap());
    }
    assert!(store.segments[0].retained().unwrap().leases.len() <= 8);

    /* When most leases are long-lived, a prune frees almost nothing, so the registry also grows.
    Holding one fewer lease than a capacity boundary is the worst case without that growth: the
    registry is full again after every churned lease, and each one rescans every held lease.
    With it, each full scan is followed by at least half its length in pushes, so the churn
    costs at most two visits per round plus one final scan of the registry. */
    let boundary = budget(16, 1024);
    let rounds = budget(200, 10_000);
    for kept in [boundary - 1, boundary, boundary + 1] {
        for promote in [false, true] {
            let visits = prune_visits(kept, rounds, promote);
            assert!(
                visits <= 2 * rounds + 4 * (kept + 1),
                "kept={kept} promote={promote}: {visits} visits for {rounds} rounds"
            );
        }
    }
}

// -----------------------------------------------------------------------------
// Store - Custom Derive-ables
// -----------------------------------------------------------------------------

#[test]
fn test_store_default() {
    let store: Store = Store::default();

    // Default is a fresh store
    assert!(store.segments.is_empty());
    assert_eq!(store.compacted, 0);
    assert_eq!(store.high_water, 0);
}

#[test]
fn test_store_debug() {
    let mut store = Store::new();
    let first = store.park(0, b"hidden".to_vec()).unwrap();
    store.park(6, b"second".to_vec()).unwrap();
    store.retire(first);

    let debug = format!("{store:?}");

    // The bookkeeping is printed, including the compaction offset
    assert!(debug.starts_with("Store { segments: ["));
    assert!(debug.contains("Segment { span: Span { start: 6, end: 12 }, status: Sealed"));
    assert!(debug.ends_with("compacted: 1, high_water: 12 }"));

    // The bytes are not, and `B` needs no `Debug` implementation
    assert!(!debug.contains("hidden"));
    let mut store = Store::new();
    store
        .park(
            0,
            Region {
                bytes: Box::from(b"region".as_slice()),
            },
        )
        .unwrap();
    assert!(format!("{store:?}").contains("status: Sealed"));
}

// -----------------------------------------------------------------------------
// Store - Creation
// -----------------------------------------------------------------------------

#[test]
fn test_store_new() {
    let store: Store = Store::new();

    // Check internal state matches expectations
    assert!(store.segments.is_empty());
    assert_eq!(store.compacted, 0);
    assert_eq!(store.high_water, 0);
}

// -----------------------------------------------------------------------------
// Store - Accessors
// -----------------------------------------------------------------------------

#[test]
fn test_store_segment_count() {
    let mut store = Store::new();

    // Starts at zero
    assert_eq!(store.segment_count(), 0);

    // Counts parks
    let first = store.park(0, b"aa".to_vec()).unwrap();
    store.park(2, b"bb".to_vec()).unwrap();
    assert_eq!(store.segment_count(), 2);

    // Counts history: retirement and compaction do not decrease it
    store.retire(first);
    assert_eq!(store.compacted, 1);
    assert_eq!(store.segment_count(), 2);
    store.retire_all();
    assert_eq!(store.segments.len(), 0);
    assert_eq!(store.segment_count(), 2);
}

#[test]
fn test_store_retained_len() {
    let mut store = Store::new();

    // Starts at zero
    assert_eq!(store.retained_len(), 0);

    // Sums retained segment lengths
    let first = store.park(0, b"aaaa".to_vec()).unwrap();
    let second = store.park(10, b"bbbbbb".to_vec()).unwrap();
    assert_eq!(store.retained_len(), 10);

    // Retired segments no longer count, compacted or not
    store.retire(second);
    assert_eq!(store.retained_len(), 4);
    store.retire(first);
    assert_eq!(store.retained_len(), 0);
}

#[test]
fn test_store_live_bytes() {
    let (mut store, id) = parked("0123456789");

    // Nothing pinned yet
    assert_eq!(store.live_bytes(id), Some(0));

    // Leases pin their length, counted with multiplicity across overlapping leases
    let head = store.lease(span(0, 4)).unwrap();
    let overlap = store.lease(span(2, 6)).unwrap();
    assert_eq!(store.live_bytes(id), Some(8));

    // Clones share one pin
    let clone = head.clone();
    assert_eq!(store.live_bytes(id), Some(8));
    drop(head);
    assert_eq!(store.live_bytes(id), Some(8));
    drop(clone);
    assert_eq!(store.live_bytes(id), Some(4));

    // Promotion releases the pin
    overlap.promote();
    assert_eq!(store.live_bytes(id), Some(0));

    // Retired and compacted segments report zero; unknown identifiers report nothing
    let lease = store.lease(span(0, 4)).unwrap();
    store.retire(id);
    assert_eq!(store.compacted, 1);
    assert_eq!(store.live_bytes(id), Some(0));
    assert_eq!(store.live_bytes(SegmentId(1)), None);
    assert!(lease.is_owned());
}

#[test]
fn test_store_status() {
    let mut store = Store::new();
    let open = store.park_open(0, b"open".to_vec()).unwrap();
    let sealed = store.park(4, b"sealed".to_vec()).unwrap();

    // Parking chooses the initial state
    assert_eq!(store.status(open), Some(SegmentStatus::Open));
    assert_eq!(store.status(sealed), Some(SegmentStatus::Sealed));

    // The lifecycle only moves forward
    store.seal(open);
    assert_eq!(store.status(open), Some(SegmentStatus::Sealed));
    store.retire(sealed);
    assert_eq!(store.status(sealed), Some(SegmentStatus::Retired));
    store.retire(open);
    assert_eq!(store.status(open), Some(SegmentStatus::Retired));

    // Compacted identifiers still answer; unknown identifiers do not
    assert_eq!(store.compacted, 2);
    assert_eq!(store.status(open), Some(SegmentStatus::Retired));
    assert_eq!(store.status(segment_id(2)), None);
}

#[test]
fn test_store_segment_span() {
    let mut store = Store::new();
    let first = store.park(3, b"hello".to_vec()).unwrap();
    let second = store.park(8, b"world".to_vec()).unwrap();

    // Retained segments report the span validated at parking
    assert_eq!(store.segment_span(first), Some(span(3, 8)));
    assert_eq!(store.segment_span(second), Some(span(8, 13)));

    // Retired segments do not, whether or not they have compacted away
    store.retire(second);
    assert_eq!(store.segment_span(second), None);
    store.retire(first);
    assert_eq!(store.segment_span(first), None);

    // Neither do unknown identifiers
    assert_eq!(store.segment_span(segment_id(9)), None);
}

// -----------------------------------------------------------------------------
// Store - Parking
// -----------------------------------------------------------------------------

#[test]
fn test_store_park() {
    let mut store = Store::new();

    // A parked segment is sealed, addressable by its base, and advances the high-water mark
    let first = store.park(100, b"based".to_vec()).unwrap();
    assert_eq!(first.index(), 0);
    assert_eq!(store.status(first), Some(SegmentStatus::Sealed));
    assert_eq!(store.segments[0].span, span(100, 105));
    assert_eq!(store.high_water, 105);
    assert_eq!(store.bytes(span(101, 104)), Some(b"ase".as_slice()));
    assert_eq!(store.bytes(span(99, 102)), None);

    // A segment may follow directly or after a gap
    store.park(105, b"next".to_vec()).unwrap();
    let third = store.park(200, b"gap".to_vec()).unwrap();
    assert_eq!(third.index(), 2);
    assert_eq!(store.high_water, 203);
    assert_eq!(store.bytes(span(200, 203)), Some(b"gap".as_slice()));
    assert_eq!(store.bytes(span(150, 151)), None);

    // An empty buffer is rejected and handed back
    let error = store.park(203, Vec::new()).unwrap_err();
    assert_eq!(error.kind(), ParkErrorKind::Empty);
    assert_eq!(error.into_bytes(), Vec::<u8>::new());
    assert_eq!(store.segment_count(), 3);

    // Overlapping the previous segment is rejected and handed back
    let error = store.park(202, b"late".to_vec()).unwrap_err();
    assert_eq!(
        error.kind(),
        ParkErrorKind::OutOfOrder {
            base: 202,
            min_base: 203,
        }
    );
    assert_eq!(error.into_bytes(), b"late");
    assert_eq!(store.segment_count(), 3);
    assert_eq!(store.retained_len(), 12);

    // Order is judged against retired history too: those positions were spoken for
    store.retire_all();
    let error = store.park(150, b"late".to_vec()).unwrap_err();
    assert_eq!(
        error.kind(),
        ParkErrorKind::OutOfOrder {
            base: 150,
            min_base: 203,
        }
    );
    let fourth = store.park(203, b"resumed".to_vec()).unwrap();
    assert_eq!(fourth.index(), 3);

    // A tail past the address space is rejected and handed back
    let base = u64::MAX - 2;
    let error = store.park(base, b"abcdef".to_vec()).unwrap_err();
    assert_eq!(error.kind(), ParkErrorKind::Unaddressable { base, len: 6 });
    assert_eq!(error.into_bytes(), b"abcdef");
    assert_eq!(store.segment_count(), 4);

    // The very last bytes of the address space are still parkable
    store.park(base, b"ab".to_vec()).unwrap();
    assert_eq!(store.high_water, u64::MAX);
    assert_eq!(store.bytes(span(base, u64::MAX)), Some(b"ab".as_slice()));

    // But nothing after them
    let error = store.park(u64::MAX, b"x".to_vec()).unwrap_err();
    assert_eq!(
        error.kind(),
        ParkErrorKind::Unaddressable {
            base: u64::MAX,
            len: 1
        }
    );
}

#[test]
fn test_store_park_open() {
    let mut store = Store::new();

    // An open segment retains bytes like a sealed one, but sweeps leave it alone
    let id = store.park_open(0, b"open".to_vec()).unwrap();
    assert_eq!(store.status(id), Some(SegmentStatus::Open));
    assert_eq!(store.bytes(span(0, 4)), Some(b"open".as_slice()));
    assert_eq!(store.maintain(0), 0);
    assert_eq!(store.status(id), Some(SegmentStatus::Open));

    // The same validation applies
    assert_eq!(
        store.park_open(0, b"early".to_vec()).unwrap_err().kind(),
        ParkErrorKind::OutOfOrder {
            base: 0,
            min_base: 4
        }
    );
    assert_eq!(
        store.park_open(4, Vec::new()).unwrap_err().kind(),
        ParkErrorKind::Empty
    );

    // Explicit retirement ignores the seal state
    assert!(store.retire(id));
    assert_eq!(store.status(id), Some(SegmentStatus::Retired));
}

#[test]
fn test_store_seal() {
    let mut store = Store::new();
    let id = store.park_open(0, b"open".to_vec()).unwrap();

    // Sealing makes the segment retirable by sweeps
    assert!(store.seal(id));
    assert_eq!(store.status(id), Some(SegmentStatus::Sealed));
    assert_eq!(store.maintain(0), 1);

    // Sealing a retired segment is a no-op that reports success, compacted or not
    assert!(store.seal(id));
    assert_eq!(store.compacted, 1);
    let next = store.park(4, b"more".to_vec()).unwrap();
    let last = store.park(8, b"last".to_vec()).unwrap();
    store.retire(last);
    assert!(store.seal(last));
    assert_eq!(store.status(last), Some(SegmentStatus::Retired));
    assert!(store.seal(next));

    // Unknown identifiers are rejected
    assert!(!store.seal(segment_id(3)));
}

// -----------------------------------------------------------------------------
// Store - Lookup
// -----------------------------------------------------------------------------

#[test]
fn test_store_entry() {
    let mut store = Store::new();
    let first = store.park(0, b"aa".to_vec()).unwrap();
    let second = store.park(2, b"bb".to_vec()).unwrap();

    // Identifiers index the resident segments
    assert_eq!(store.entry(first).unwrap().span, span(0, 2));
    assert_eq!(store.entry_mut(second).unwrap().span, span(2, 4));
    assert!(store.entry(segment_id(2)).is_none());
    assert!(!store.is_compacted(first));

    // Compaction offsets the indexing
    store.retire(first);
    assert_eq!(store.compacted, 1);
    assert!(store.is_compacted(first));
    assert!(store.entry(first).is_none());
    assert_eq!(store.entry(second).unwrap().span, span(2, 4));
    assert_eq!(store.entry_mut(second).unwrap().span, span(2, 4));
}

#[test]
fn test_store_position_after() {
    let mut store = Store::new();
    store.park(0, b"alpha".to_vec()).unwrap();
    store.park(10, b"omega".to_vec()).unwrap();

    // Offsets inside or before a segment find it
    assert_eq!(store.position_after(0), 0);
    assert_eq!(store.position_after(4), 0);
    assert_eq!(store.position_after(5), 1);
    assert_eq!(store.position_after(7), 1);
    assert_eq!(store.position_after(10), 1);
    assert_eq!(store.position_after(14), 1);

    // Offsets past every segment land after the deque
    assert_eq!(store.position_after(15), 2);
    assert_eq!(store.position_after(u64::MAX), 2);

    // An empty store has no positions
    let store: Store = Store::new();
    assert_eq!(store.position_after(0), 0);
}

#[test]
fn test_store_containing() {
    let mut store = Store::new();
    store.park(0, b"alpha".to_vec()).unwrap();
    store.park(10, b"omega".to_vec()).unwrap();

    // A span inside one segment maps to that segment and its relative range
    let (position, range) = store.containing(span(1, 4)).unwrap();
    assert_eq!(position, 0);
    assert_eq!(range.bounds(), 1..4);
    let (position, range) = store.containing(span(12, 15)).unwrap();
    assert_eq!(position, 1);
    assert_eq!(range.bounds(), 2..5);

    // Spans reaching past a segment, into a gap, or past the end do not map
    assert!(store.containing(span(3, 7)).is_none());
    assert!(store.containing(span(5, 7)).is_none());
    assert!(store.containing(span(7, 12)).is_none());
    assert!(store.containing(span(15, 16)).is_none());
}

#[test]
fn test_store_bytes() {
    let mut store = Store::new();
    let first = store.park(0, b"alpha".to_vec()).unwrap();
    store.park(5, b"beta".to_vec()).unwrap();

    // Spans inside one segment resolve
    assert_eq!(store.bytes(span(0, 5)), Some(b"alpha".as_slice()));
    assert_eq!(store.bytes(span(1, 4)), Some(b"lph".as_slice()));
    assert_eq!(store.bytes(span(5, 9)), Some(b"beta".as_slice()));

    // Spans across segments or past them do not
    assert_eq!(store.bytes(span(3, 7)), None);
    assert_eq!(store.bytes(span(5, 20)), None);

    // Neither do spans into a retired segment
    store.retire(first);
    assert_eq!(store.bytes(span(0, 5)), None);
    assert_eq!(store.bytes(span(5, 9)), Some(b"beta".as_slice()));
}

#[test]
fn test_store_ranges() {
    let mut store = Store::new();
    let first = store.park(0, b"alpha".to_vec()).unwrap();
    let second = store.park(5, b"beta".to_vec()).unwrap();
    store.park(20, b"gamma".to_vec()).unwrap();

    // A span inside one segment maps to one range
    let ranges = store.ranges(span(0, 5)).unwrap();
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].0, 0);
    assert_eq!(ranges[0].1.bounds(), 0..5);
    let ranges = store.ranges(span(8, 9)).unwrap();
    assert_eq!(ranges[0].0, 1);
    assert_eq!(ranges[0].1.bounds(), 3..4);

    // A span across consecutive segments maps to one range per segment
    let ranges = store.ranges(span(3, 7)).unwrap();
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0].0, 0);
    assert_eq!(ranges[0].1.bounds(), 3..5);
    assert_eq!(ranges[1].0, 1);
    assert_eq!(ranges[1].1.bounds(), 0..2);

    // A span reaching into a gap, starting in one, or past the end does not map
    assert!(store.ranges(span(3, 12)).is_none());
    assert!(store.ranges(span(9, 10)).is_none());
    assert!(store.ranges(span(12, 22)).is_none());
    assert!(store.ranges(span(24, 26)).is_none());

    // A retired segment in the middle breaks the coverage
    store.retire(second);
    assert!(store.ranges(span(3, 7)).is_none());
    assert!(store.ranges(span(5, 6)).is_none());
    assert_eq!(store.ranges(span(0, 5)).unwrap()[0].0, 0);
    assert_eq!(store.ranges(span(20, 22)).unwrap()[0].0, 2);

    // Positions index the resident deque, so they shift with compaction
    store.retire(first);
    assert_eq!(store.segments.len(), 1);
    let ranges = store.ranges(span(20, 22)).unwrap();
    assert_eq!(ranges[0].0, 0);
    assert_eq!(ranges[0].1.bounds(), 0..2);
}

#[test]
fn test_store_copy_bytes() {
    let mut store = Store::new();
    store.park(0, b"alpha".to_vec()).unwrap();
    let second = store.park(5, b"beta".to_vec()).unwrap();
    store.park(9, b"gamma".to_vec()).unwrap();

    // Copies follow spans across any number of segments
    assert_eq!(store.copy_bytes(span(0, 5)), Some(b"alpha".to_vec()));
    assert_eq!(store.copy_bytes(span(3, 7)), Some(b"habe".to_vec()));
    assert_eq!(store.copy_bytes(span(3, 11)), Some(b"habetaga".to_vec()));

    // Spans not fully retained do not copy
    assert_eq!(store.copy_bytes(span(3, 20)), None);
    store.retire(second);
    assert_eq!(store.copy_bytes(span(3, 11)), None);
    assert_eq!(store.copy_bytes(span(9, 14)), Some(b"gamma".to_vec()));
}

// -----------------------------------------------------------------------------
// Store - Leasing
// -----------------------------------------------------------------------------

#[test]
fn test_store_lease() {
    let (mut store, id) = parked("0123456789");

    // The lease starts loaned and pins its length
    let lease = store.lease(span(2, 6)).unwrap();
    assert!(lease.is_loaned());
    assert_eq!(&*lease.bytes(), b"2345");
    assert_eq!(store.live_bytes(id), Some(4));
    assert_eq!(store.segments[0].retained().unwrap().leases.len(), 1);

    // Dropping it releases the pin
    drop(lease);
    assert_eq!(store.live_bytes(id), Some(0));

    // Spans not inside one retained segment do not lease
    assert!(store.lease(span(8, 12)).is_none());
    assert!(store.lease(span(10, 11)).is_none());
    store.retire(id);
    assert!(store.lease(span(2, 6)).is_none());
}

#[test]
fn test_store_leases() {
    let mut store = Store::new();
    let first = store.park(0, b"alpha".to_vec()).unwrap();
    let second = store.park(5, b"beta".to_vec()).unwrap();
    store.park(20, b"omega".to_vec()).unwrap();

    // A span inside one segment yields one lease
    let parts = store.leases(span(5, 9)).unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(&*parts[0].bytes(), b"beta");
    drop(parts);

    // A span across segments yields one lease per segment, each pinning its piece
    let parts = store.leases(span(3, 7)).unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(&*parts[0].bytes(), b"ha");
    assert_eq!(&*parts[1].bytes(), b"be");
    assert_eq!(store.live_bytes(first), Some(2));
    assert_eq!(store.live_bytes(second), Some(2));

    // Retirement promotes every piece
    store.retire_all();
    assert!(parts.iter().all(Lease::is_owned));
    assert_eq!(&*parts[0].bytes(), b"ha");
    assert_eq!(&*parts[1].bytes(), b"be");

    // A span reaching into a gap yields nothing, and pins nothing
    let mut store = Store::new();
    let first = store.park(0, b"alpha".to_vec()).unwrap();
    store.park(10, b"omega".to_vec()).unwrap();
    assert!(store.leases(span(3, 12)).is_none());
    assert_eq!(store.live_bytes(first), Some(0));
    assert!(store.segments[0].retained().unwrap().leases.is_empty());
}

// -----------------------------------------------------------------------------
// Store - Retirement
// -----------------------------------------------------------------------------

#[test]
fn test_store_retire() {
    let (mut store, id) = parked("prefix:value:suffix");
    let value = store.lease(span(7, 12)).unwrap();
    let suffix = store.lease(span(13, 19)).unwrap();
    let promoted = store.lease(span(0, 6)).unwrap();
    promoted.promote();
    let guard = value.bytes();

    // Retiring promotes the surviving leases and releases the bytes, without blocking on guards
    assert!(store.retire(id));
    assert!(value.is_owned());
    assert!(suffix.is_owned());
    assert_eq!(&*value.bytes(), b"value");
    assert_eq!(&*suffix.bytes(), b"suffix");
    assert_eq!(&*promoted.bytes(), b"prefix");
    assert_eq!(store.retained_len(), 0);
    assert_eq!(store.live_bytes(id), Some(0));
    assert_eq!(store.status(id), Some(SegmentStatus::Retired));
    assert_eq!(store.bytes(span(7, 12)), None);

    // The guard taken beforehand keeps its snapshot
    assert_eq!(&*guard, b"value");
    drop(guard);

    // Retiring again, or an unknown segment, reports nothing to do
    assert!(!store.retire(id));
    assert!(!store.retire(segment_id(1)));

    // Open segments retire too
    let open = store.park_open(19, b"open".to_vec()).unwrap();
    assert!(store.retire(open));
    assert_eq!(store.status(open), Some(SegmentStatus::Retired));
}

#[test]
fn test_store_retire_all() {
    let mut store = Store::new();
    store.park(0, b"alpha".to_vec()).unwrap();
    let open = store.park_open(5, b"beta".to_vec()).unwrap();
    let lease = store.lease(span(0, 5)).unwrap();

    // Every segment retires regardless of seal state, and the count is reported
    assert_eq!(store.retire_all(), 2);
    assert_eq!(store.retained_len(), 0);
    assert_eq!(store.status(open), Some(SegmentStatus::Retired));
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"alpha");

    // Nothing is left to retire; new parks are retired on the next call
    assert_eq!(store.retire_all(), 0);
    store.park(9, b"more".to_vec()).unwrap();
    assert_eq!(store.retire_all(), 1);
}

#[test]
fn test_store_maintain() {
    let (mut store, id) = parked("0123456789");
    let lease = store.lease(span(2, 4)).unwrap();

    // Wasted bytes are the retained length minus the live loaned bytes: 10 - 2 = 8
    assert_eq!(store.maintain(9), 0);
    assert!(lease.is_loaned());
    assert_eq!(store.status(id), Some(SegmentStatus::Sealed));
    assert_eq!(store.maintain(8), 1);
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"23");
    assert_eq!(store.retained_len(), 0);

    // Dropped leases stop counting as live
    let (mut store, _) = parked("0123456789");
    let lease = store.lease(span(0, 10)).unwrap();
    assert_eq!(store.maintain(1), 0);
    drop(lease);
    assert_eq!(store.maintain(1), 1);

    // A zero threshold retires even fully loaned segments
    let (mut store, id) = parked("0123456789");
    let lease = store.lease(span(0, 10)).unwrap();
    assert_eq!(store.maintain(0), 1);
    assert_eq!(store.status(id), Some(SegmentStatus::Retired));
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"0123456789");

    // Open segments are skipped until sealed
    let mut store = Store::new();
    let id = store.park_open(0, b"open".to_vec()).unwrap();
    assert_eq!(store.maintain(0), 0);
    assert_eq!(store.status(id), Some(SegmentStatus::Open));
    store.seal(id);
    assert_eq!(store.maintain(0), 1);

    // Segments are judged independently, and the count covers only this call
    let mut store = Store::new();
    store.park(0, vec![b'a'; 100]).unwrap();
    store.park(100, vec![b'b'; 10]).unwrap();
    store.park(110, vec![b'c'; 100]).unwrap();
    assert_eq!(store.maintain(50), 2);
    assert_eq!(store.retained_len(), 10);
    assert_eq!(store.maintain(50), 0);

    // A sweep that retires nothing still prunes promoted and dropped registrations
    let (mut store, _) = parked("0123456789");
    let promoted = store.lease(span(0, 8)).unwrap();
    drop(store.lease(span(0, 8)).unwrap());
    promoted.promote();
    assert_eq!(store.segments[0].retained().unwrap().leases.len(), 2);
    assert_eq!(store.maintain(usize::MAX), 0);
    assert!(store.segments[0].retained().unwrap().leases.is_empty());
    assert_eq!(&*promoted.bytes(), b"01234567");

    // A saturated counter keeps its segment from retiring by any positive threshold
    let (mut store, id) = parked("0123456789");
    store.segments[0]
        .retained()
        .unwrap()
        .backing
        .live_bytes
        .store(usize::MAX, Ordering::Relaxed);
    assert_eq!(store.maintain(1), 0);
    assert_eq!(store.status(id), Some(SegmentStatus::Sealed));
    assert!(store.retire(id));
}

#[test]
fn test_store_compact() {
    let mut store = Store::new();
    let first = store.park(0, b"aa".to_vec()).unwrap();
    let second = store.park(2, b"bb".to_vec()).unwrap();
    let third = store.park(4, b"cc".to_vec()).unwrap();

    // A middle tombstone stays until everything ahead of it retires too
    assert!(store.retire(second));
    assert_eq!(store.segments.len(), 3);
    assert_eq!(store.compacted, 0);

    // The front and the already retired middle compact away together
    assert!(store.retire(first));
    assert_eq!(store.segments.len(), 1);
    assert_eq!(store.compacted, 2);

    // Identifiers stay valid and answer with their settled state
    assert_eq!(store.segment_count(), 3);
    assert_eq!(store.status(first), Some(SegmentStatus::Retired));
    assert_eq!(store.status(second), Some(SegmentStatus::Retired));
    assert_eq!(store.status(third), Some(SegmentStatus::Sealed));
    assert_eq!(store.live_bytes(first), Some(0));
    assert!(store.seal(first));
    assert!(!store.retire(first));

    // New parks continue the identifier sequence past compacted history
    let fourth = store.park(6, b"dd".to_vec()).unwrap();
    assert_eq!(fourth.index(), 3);
    assert_eq!(store.bytes(span(4, 6)), Some(b"cc".as_slice()));
    assert_eq!(store.bytes(span(6, 8)), Some(b"dd".as_slice()));

    // So a streaming park-and-retire loop runs in bounded memory
    let mut store = Store::new();
    for round in 0..budget(20, 100) {
        let base = u64::try_from(round * 4).unwrap();
        let id = store.park(base, b"data".to_vec()).unwrap();
        assert!(store.retire(id));
        assert!(store.segments.is_empty());
    }
    assert_eq!(
        store.segment_count(),
        u64::try_from(budget(20, 100)).unwrap()
    );
    assert_eq!(store.retained_len(), 0);
}

// -----------------------------------------------------------------------------
// Store - Buffer contract
// -----------------------------------------------------------------------------

#[test]
fn test_store_custom_buffer() {
    let mut store = Store::new();
    store
        .park(
            0,
            Region {
                bytes: Box::from(b"custom-buffer".as_slice()),
            },
        )
        .unwrap();

    // Any stable `AsRef<[u8]>` works like a vector
    let lease = store.lease(span(0, 6)).unwrap();
    assert_eq!(store.bytes(span(7, 13)), Some(b"buffer".as_slice()));
    assert_eq!(store.retire_all(), 1);
    assert_eq!(&*lease.bytes(), b"custom");
}

#[test]
fn test_store_panicking_buffer() {
    let poisoned = Arc::new(AtomicBool::new(false));
    let mut store = Store::new();
    let id = store
        .park(
            0,
            FlakyBuffer {
                bytes: b"fragile".to_vec(),
                poisoned: Arc::clone(&poisoned),
            },
        )
        .unwrap();
    let lease = store.lease(span(0, 4)).unwrap();

    poisoned.store(true, Ordering::Relaxed);

    // Consumer-side promotion panics but must not corrupt the lease or its accounting
    assert!(catch_unwind(AssertUnwindSafe(|| lease.promote())).is_err());
    assert!(lease.is_loaned());
    assert_eq!(store.live_bytes(id), Some(4));

    // Owner-side retirement panics but must leave the segment retryable
    assert!(catch_unwind(AssertUnwindSafe(|| store.retire(id))).is_err());
    assert_eq!(store.status(id), Some(SegmentStatus::Sealed));
    assert_eq!(store.retained_len(), 7);

    poisoned.store(false, Ordering::Relaxed);

    // After the fault clears, everything proceeds normally despite the poisoned lock
    assert_eq!(&*lease.bytes(), b"frag");
    assert!(store.retire(id));
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"frag");
    assert_eq!(store.live_bytes(id), Some(0));
}

#[test]
fn test_store_unstable_buffer() {
    let shrunk = Arc::new(AtomicBool::new(false));
    let mut store = Store::new();
    let id = store
        .park(
            0,
            ShrinkingBuffer {
                bytes: b"unstable".to_vec(),
                shrunk: Arc::clone(&shrunk),
            },
        )
        .unwrap();
    let lease = store.lease(span(0, 8)).unwrap();
    assert_eq!(&*lease.bytes(), b"unstable");

    shrunk.store(true, Ordering::Relaxed);

    // The validated range no longer fits; reads clamp to empty instead of panicking
    assert_eq!(&*lease.bytes(), b"");
    assert_eq!(lease.len(), 0);
    assert!(lease.is_empty());
    assert_eq!(store.bytes(span(0, 8)), None);
    assert_eq!(store.copy_bytes(span(0, 8)), None);

    // Promotion copies what is there, and the accounting stays consistent
    lease.promote();
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"");
    assert_eq!(store.live_bytes(id), Some(0));
    assert_eq!(store.retained_len(), 8);
}

#[test]
fn test_store_drop() {
    let mut store = Store::new();
    store.park(0, b"outlives".to_vec()).unwrap();
    let lease = store.lease(span(0, 8)).unwrap();
    let guard = lease.bytes();

    drop(store);

    // Without retirement the crate degrades to plain refcounting: nothing dangles
    assert!(lease.is_loaned());
    assert_eq!(&*lease.bytes(), b"outlives");
    assert_eq!(&*guard, b"outlives");

    lease.promote();
    assert_eq!(&*lease.bytes(), b"outlives");
}
