//! What does a lease cost to create and read, compared with copying the bytes it covers?
//! One 64 KiB segment is parked once; ranges are 16 B or 4 KiB, starting 1000 bytes in. The
//! `lifecycle` pair keeps the same bytes for the same lifetime: `lease` creates a loaned lease,
//! reads it, and drops it; `copy` copies the range into a Vec, reads it, and drops it. Two
//! controls show the floor without equivalent retention: `arc` clones an `Arc<[u8]>` of the whole
//! buffer, which keeps all of it alive and can never be revoked, and `borrow` reads a slice that
//! pins the store itself. `read` acquires and drops one guard on an existing loaned or owned
//! lease, and `clone` clones and drops an existing lease. Setup and checks are outside timing.

use criterion::{BenchmarkId, Criterion};
use std::hint::black_box;
use std::sync::Arc;
use strata_lease::Store;

pub(super) fn bench(c: &mut Criterion) {
    let data = super::source(64 * 1024);
    let shared: Arc<[u8]> = Arc::from(data.as_slice());
    let mut store = Store::new();
    let id = store.park(0, data.clone()).unwrap();
    let start = 1000;

    let mut group = c.benchmark_group("lifecycle");
    for len in [16, 4096] {
        let span = super::span(start, start + len);
        let expected = &data[start..start + len];

        // Every variant reads exactly the expected bytes, and the lease unpins them when dropped
        let lease = store.lease(span).unwrap();
        assert!(lease.is_loaned());
        assert_eq!(&*lease.bytes(), expected);
        assert_eq!(store.live_bytes(id), Some(len));
        drop(lease);
        assert_eq!(store.live_bytes(id), Some(0));
        assert_eq!(store.bytes(span).unwrap().to_vec(), expected);
        assert_eq!(&shared[start..start + len], expected);

        group.bench_function(BenchmarkId::new("lease", len), |b| {
            b.iter(|| {
                let lease = store.lease(black_box(span)).unwrap();
                black_box(&*lease.bytes());
            });
        });
        group.bench_function(BenchmarkId::new("copy", len), |b| {
            b.iter(|| {
                let copy = store.bytes(black_box(span)).unwrap().to_vec();
                black_box(&copy);
            });
        });
        group.bench_function(BenchmarkId::new("arc", len), |b| {
            b.iter(|| {
                let view = Arc::clone(&shared);
                black_box(&view[black_box(start)..start + len]);
            });
        });
        group.bench_function(BenchmarkId::new("borrow", len), |b| {
            b.iter(|| black_box(store.bytes(black_box(span)).unwrap()));
        });
    }
    group.finish();

    let mut group = c.benchmark_group("read");
    for len in [16, 4096] {
        let span = super::span(start, start + len);
        let expected = &data[start..start + len];
        let loaned = store.lease(span).unwrap();
        let owned = store.lease(span).unwrap();
        owned.promote();

        // Both states read the same bytes
        assert!(loaned.is_loaned());
        assert!(owned.is_owned());
        assert_eq!(&*loaned.bytes(), expected);
        assert_eq!(&*owned.bytes(), expected);

        group.bench_function(BenchmarkId::new("loaned", len), |b| {
            b.iter(|| {
                let guard = black_box(&loaned).bytes();
                black_box(&*guard);
            });
        });
        group.bench_function(BenchmarkId::new("owned", len), |b| {
            b.iter(|| {
                let guard = black_box(&owned).bytes();
                black_box(&*guard);
            });
        });
    }
    group.finish();

    // A clone shares its original's state, so promoting one promotes both
    let lease = store.lease(super::span(start, start + 16)).unwrap();
    let clone = lease.clone();
    clone.promote();
    assert!(lease.is_owned());
    let lease = store.lease(super::span(start, start + 16)).unwrap();

    c.bench_function("clone", |b| {
        b.iter(|| drop(black_box(black_box(&lease).clone())));
    });
}
