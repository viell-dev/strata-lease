//! Does creating a short-lived lease stay cheap next to many long-lived ones?
//! One segment holds `kept` single-byte leases for the whole case, which pruning can never free.
//! In `drop`, each iteration creates one more lease and drops it. In `promote`, each iteration
//! creates one, promotes it, and keeps it until the next iteration replaces it, so pruning finds
//! its registration promoted rather than dead. The persistent counts sit around registry capacity
//! boundaries (2^k - 1, 2^k, 2^k + 1): there, a registry whose prune freed only one slot would
//! rescan every persistent lease for each new one. `churn_batch` holds 1023 leases and creates
//! 1, 16, or 256 short-lived leases per iteration before dropping them together. Setup is outside
//! timing. The registry carries over between iterations, so each estimate is a steady-state
//! average that includes amortized pruning.

use criterion::{BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use strata_lease::{Lease, SegmentId, Store};

/// A segment whose first `kept` bytes each carry a persistent lease, with one spare byte after.
struct Fixture {
    store: Store,
    id: SegmentId,
    held: Vec<Lease>,
    data: Vec<u8>,
}

fn fixture(kept: usize) -> Fixture {
    let data = super::source(kept + 1);
    let mut store = Store::new();
    let id = store.park(0, data.clone()).unwrap();
    let held = (0..kept)
        .map(|index| store.lease(super::span(index, index + 1)).unwrap())
        .collect();

    Fixture {
        store,
        id,
        held,
        data,
    }
}

/// Checks that every persistent lease is still loaned with its own byte, and that only they pin
/// the segment.
fn check_held(fixture: &Fixture) {
    for (index, lease) in fixture.held.iter().enumerate() {
        assert!(lease.is_loaned());
        assert_eq!(*lease.bytes(), fixture.data[index..=index]);
    }
    assert_eq!(
        fixture.store.live_bytes(fixture.id),
        Some(fixture.held.len())
    );
}

pub(super) fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("churn");
    for kept in [0, 1023, 1024, 1025, 16383, 16384, 16385] {
        let mut fixture = fixture(kept);
        let tail = super::span(kept, kept + 1);
        let expected = fixture.data[kept..].to_vec();

        // Enough rounds to fill and prune the registry; each churned lease reads the spare byte
        for _ in 0..kept + 64 {
            let lease = fixture.store.lease(tail).unwrap();
            assert_eq!(*lease.bytes(), expected);
        }
        let mut previous = None;
        for _ in 0..kept + 64 {
            let lease = fixture.store.lease(tail).unwrap();
            lease.promote();
            assert!(lease.is_owned());
            assert_eq!(*lease.bytes(), expected);
            previous = Some(lease);
        }
        check_held(&fixture);

        group.bench_function(BenchmarkId::new("drop", kept), |b| {
            b.iter(|| drop(black_box(fixture.store.lease(black_box(tail)).unwrap())));
        });
        group.bench_function(BenchmarkId::new("promote", kept), |b| {
            b.iter(|| {
                let lease = fixture.store.lease(black_box(tail)).unwrap();
                lease.promote();
                previous = Some(black_box(lease));
            });
        });

        // Timing changed nothing the persistent leases rely on
        drop(previous);
        check_held(&fixture);
    }
    group.finish();

    let mut group = c.benchmark_group("churn_batch");
    for batch in [1, 16, 256] {
        let mut fixture = fixture(1023);
        let tail = super::span(1023, 1024);
        let mut leases = Vec::with_capacity(batch);

        // A batch pins one byte per lease until it is dropped together
        leases.extend((0..batch).map(|_| fixture.store.lease(tail).unwrap()));
        assert!(
            leases
                .iter()
                .all(|lease| *lease.bytes() == fixture.data[1023..])
        );
        assert_eq!(fixture.store.live_bytes(fixture.id), Some(1023 + batch));
        leases.clear();
        check_held(&fixture);

        group.throughput(Throughput::Elements(u64::try_from(batch).unwrap()));
        group.bench_function(BenchmarkId::from_parameter(batch), |b| {
            b.iter(|| {
                leases.extend((0..batch).map(|_| fixture.store.lease(black_box(tail)).unwrap()));
                black_box(&leases);
                leases.clear();
            });
        });
        check_held(&fixture);
    }
    group.finish();
}
