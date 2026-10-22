//! What do promotion, retirement, and threshold maintenance cost?
//! Each iteration consumes a fresh fixture, because promoting or retiring the same one twice
//! would only time the no-op. Fixtures are built outside timing in batches of 16
//! (`iter_batched_ref`), and their final destruction is outside timing too. Timed work includes
//! each operation's copies, allocations, and registry walk, and freeing the segment buffer when
//! the operation drops the last pin on it.
//! - `promote`: a consumer promotes one loaned lease of 16 B or 4 KiB in a 64 KiB segment.
//! - `retire`: one 64 KiB segment with no leases, 16 or 256 independent 16 B leases, or 16
//!   leases of 2 KiB covering half the segment. `clones` and `overlapping` both cover one 16 B
//!   range 256 times: as clones sharing one registration and one copy, or as independent
//!   leases that each register and copy.
//! - `maintain`: 64 sealed segments of 4 KiB, alternately pinned by a 3 KiB lease and a 256 B
//!   one. The waste threshold retires none of them, the lightly pinned half, or all of them.

use criterion::{BatchSize, BenchmarkId, Criterion};
use std::collections::HashSet;
use std::hint::black_box;
use strata_lease::{Lease, SegmentId, Store};

/// Iterations per batch of fresh fixtures, bounding the memory held by unconsumed fixtures.
const BATCH: BatchSize = BatchSize::NumIterations(16);

/// Offset of the first leased byte, so an offset error cannot hide at the segment base.
const START: usize = 1000;

/// One parked segment and the leases a case attaches to it.
struct Fixture {
    store: Store,
    id: SegmentId,
    leases: Vec<Lease>,
}

/// Parks `data` and attaches `count` independent leases of `len` bytes, each starting `stride`
/// bytes after the previous one; a zero stride makes them all overlap.
fn independent(data: &[u8], count: usize, len: usize, stride: usize) -> Fixture {
    let mut store = Store::new();
    let id = store.park(0, data.to_vec()).unwrap();
    let leases = (0..count)
        .map(|index| {
            let start = START + index * stride;
            store.lease(super::span(start, start + len)).unwrap()
        })
        .collect();

    Fixture { store, id, leases }
}

/// Parks `data` and attaches one lease of `len` bytes plus clones, `count` handles in total.
fn cloned(data: &[u8], count: usize, len: usize) -> Fixture {
    let mut store = Store::new();
    let id = store.park(0, data.to_vec()).unwrap();
    let lease = store.lease(super::span(START, START + len)).unwrap();
    let leases = vec![lease; count];

    Fixture { store, id, leases }
}

/// Checks that every lease of a fixture is owned and holds the bytes its range had.
fn check_owned(fixture: &Fixture, data: &[u8], len: usize, stride: usize) {
    for (index, lease) in fixture.leases.iter().enumerate() {
        let start = START + index * stride;
        assert!(lease.is_owned());
        assert_eq!(*lease.bytes(), data[start..start + len]);
    }
}

/// Parks `data` as sealed segments of 4 KiB, pinned alternately by a 3 KiB and a 256 B lease.
fn pinned_segments(data: &[u8]) -> (Store, Vec<Lease>) {
    let mut store = Store::new();
    let leases = data
        .chunks_exact(4096)
        .enumerate()
        .map(|(index, chunk)| {
            let base = index * 4096;
            store
                .park(u64::try_from(base).unwrap(), chunk.to_vec())
                .unwrap();
            let len = if index % 2 == 0 { 3072 } else { 256 };
            store.lease(super::span(base, base + len)).unwrap()
        })
        .collect();

    (store, leases)
}

pub(super) fn bench(c: &mut Criterion) {
    let data = super::source(64 * 1024);

    let mut group = c.benchmark_group("promote");
    for len in [16, 4096] {
        // Promotion copies the range and unpins it, without involving the store
        let fixture = independent(&data, 1, len, 0);
        fixture.leases[0].promote();
        check_owned(&fixture, &data, len, 0);
        assert_eq!(fixture.store.live_bytes(fixture.id), Some(0));

        group.bench_function(BenchmarkId::from_parameter(len), |b| {
            b.iter_batched_ref(
                || independent(&data, 1, len, 0),
                |fixture| black_box(&fixture.leases[0]).promote(),
                BATCH,
            );
        });
    }
    group.finish();

    let mut group = c.benchmark_group("retire");
    // Name, leases, bytes per lease, and distance between lease starts
    for (name, count, len, stride) in [
        ("none", 0, 16, 16),
        ("16x16B", 16, 16, 16),
        ("256x16B", 256, 16, 16),
        ("16x2KiB", 16, 2048, 2048),
        ("clones", 256, 16, 0),
        ("overlapping", 256, 16, 0),
    ] {
        let setup = || {
            if name == "clones" {
                cloned(&data, count, len)
            } else {
                independent(&data, count, len, stride)
            }
        };

        // Retirement promotes every lease to its own bytes and releases the segment
        let mut fixture = setup();
        assert!(fixture.store.retire(fixture.id));
        check_owned(&fixture, &data, len, stride);
        assert_eq!(fixture.store.retained_len(), 0);

        // Clones share one promoted copy; independent leases each take their own
        let copies = fixture.leases.iter().map(|lease| lease.bytes().as_ptr());
        let distinct = copies.collect::<HashSet<_>>().len();
        let expected = if name == "clones" {
            1
        } else {
            fixture.leases.len()
        };
        assert_eq!(distinct, expected);

        group.bench_function(name, |b| {
            b.iter_batched_ref(
                setup,
                |fixture| black_box(fixture.store.retire(black_box(fixture.id))),
                BATCH,
            );
        });
    }
    group.finish();

    let data = super::source(64 * 4096);
    let mut group = c.benchmark_group("maintain");
    for (name, threshold, retired) in [("none", 3841, 0), ("half", 2048, 32), ("all", 1024, 64)] {
        // Waste is 1 KiB under a 3 KiB lease and 3.75 KiB under a 256 B one
        let (mut store, leases) = pinned_segments(&data);
        assert_eq!(store.maintain(threshold), retired);
        for (index, lease) in leases.iter().enumerate() {
            let base = index * 4096;
            let len = if index % 2 == 0 { 3072 } else { 256 };
            let promoted = retired == 64 || (retired == 32 && index % 2 == 1);
            assert_eq!(lease.is_owned(), promoted);
            assert_eq!(*lease.bytes(), data[base..base + len]);
        }

        group.bench_function(name, |b| {
            b.iter_batched_ref(
                || pinned_segments(&data),
                |(store, _)| black_box(store.maintain(black_box(threshold))),
                BATCH,
            );
        });
    }
    group.finish();
}
