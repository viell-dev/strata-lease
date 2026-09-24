//! Tests for the Lease

#![expect(clippy::unwrap_used, reason = "Okay in tests")]

use super::*;
use crate::span::tests::span;
use crate::{Span, Store};
use std::thread;

/// Test helper to build a backing over a copy of `bytes` with no live bytes.
fn backing(bytes: &[u8]) -> Arc<Backing<Vec<u8>>> {
    Arc::new(Backing {
        bytes: bytes.to_vec(),
        live_bytes: AtomicUsize::new(0),
    })
}

/// Test helper to build a segment range `start..end` inside a segment of `len` bytes based at 0.
fn range(start: u64, end: u64, len: u64) -> SegmentRange {
    span(start, end).segment_range(&span(0, len)).unwrap()
}

/// Test helper to park `text` at base 0 and lease `start..end` from it.
fn leased(text: &str, start: u64, end: u64) -> (Store, Lease) {
    let mut store = Store::new();
    store.park(0, text.as_bytes().to_vec()).unwrap();
    let lease = store.lease(Span::new(start, end).unwrap()).unwrap();

    (store, lease)
}

// -----------------------------------------------------------------------------
// Live-byte counter helpers
// -----------------------------------------------------------------------------

#[test]
fn test_pin() {
    let counter = AtomicUsize::new(0);

    // Pins accumulate
    pin(&counter, 5);
    assert_eq!(counter.load(Ordering::Relaxed), 5);
    pin(&counter, 7);
    assert_eq!(counter.load(Ordering::Relaxed), 12);

    // Overflow saturates instead of wrapping
    pin(&counter, usize::MAX);
    assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
}

#[test]
fn test_unpin() {
    let counter = AtomicUsize::new(12);

    // Unpins subtract
    unpin(&counter, 5);
    assert_eq!(counter.load(Ordering::Relaxed), 7);

    // Underflow saturates at zero
    unpin(&counter, 100);
    assert_eq!(counter.load(Ordering::Relaxed), 0);

    // A saturated counter stays saturated: the true total is unknown, so dipping is unsafe
    let counter = AtomicUsize::new(usize::MAX - 1);
    pin(&counter, 5);
    assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
    unpin(&counter, 5);
    assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
}

// -----------------------------------------------------------------------------
// Slot
// -----------------------------------------------------------------------------

#[test]
fn test_slot_is_promoted() {
    let (lease, _registration) = Lease::loaned(backing(b"slot"), range(0, 4, 4));

    // A fresh slot is loaned
    assert!(!lease.slot.is_promoted());

    lease.slot.promote();

    // A promoted slot reports so
    assert!(lease.slot.is_promoted());
}

#[test]
fn test_slot_promote() {
    let backing = backing(b"0123456789");
    let (lease, _registration) = Lease::loaned(Arc::clone(&backing), range(2, 6, 10));

    // Leasing pinned the range length
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 4);
    assert_eq!(Arc::strong_count(&backing), 2);

    lease.slot.promote();

    // The copy holds the range bytes, the source is gone, and the pin is released
    assert_eq!(
        lease.slot.owned.get().map(|owned| &**owned),
        Some(b"2345".as_slice())
    );
    assert!(lease.slot.lock_source().is_none());
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&backing), 1);

    lease.slot.promote();

    // Promoting again changes nothing and does not unpin twice
    assert_eq!(
        lease.slot.owned.get().map(|owned| &**owned),
        Some(b"2345".as_slice())
    );
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn test_slot_drop() {
    let backing = backing(b"0123456789");
    let (lease, registration) = Lease::loaned(Arc::clone(&backing), range(2, 6, 10));
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 4);

    drop(lease);

    // Dropping the last handle releases the pin and the registration goes dead
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&backing), 1);
    assert!(registration.upgrade().is_none());

    // A promoted slot has already released its pin, so dropping it does not unpin again
    let (lease, _registration) = Lease::loaned(Arc::clone(&backing), range(0, 3, 10));
    lease.slot.promote();
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 0);
    drop(lease);
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 0);
}

// -----------------------------------------------------------------------------
// Lease - Custom Derive-ables
// -----------------------------------------------------------------------------

#[test]
fn test_lease_clone() {
    let backing = backing(b"cloned");
    let (lease, _registration) = Lease::loaned(Arc::clone(&backing), range(0, 6, 6));
    let clone = lease.clone();

    // Clones share the slot, and thereby one pin
    assert!(Arc::ptr_eq(&lease.slot, &clone.slot));
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 6);

    // Dropping one clone keeps the pin
    drop(lease);
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 6);
    assert_eq!(&*clone.bytes(), b"cloned");

    // Promotion through one clone reaches every clone
    let lease = clone.clone();
    clone.promote();
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"cloned");
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn test_lease_debug() {
    let (_store, lease) = leased("debug", 0, 5);

    // Only the promotion state is exposed, marked non-exhaustive with a trailing `..`
    assert_eq!(format!("{lease:?}"), "Lease { owned: false, .. }");
    lease.promote();
    assert_eq!(format!("{lease:?}"), "Lease { owned: true, .. }");

    // The bytes never leak into the output
    assert!(!format!("{lease:#?}").contains("debug"));
}

// -----------------------------------------------------------------------------
// Lease - Creation
// -----------------------------------------------------------------------------

#[test]
fn test_lease_loaned() {
    let backing = backing(b"0123456789");
    let (lease, registration) = Lease::loaned(Arc::clone(&backing), range(2, 6, 10));

    // The lease starts loaned over the given range, pinning it
    assert!(lease.is_loaned());
    assert_eq!(&*lease.bytes(), b"2345");
    assert_eq!(backing.live_bytes.load(Ordering::Relaxed), 4);

    // The registration finds the live slot without keeping it alive
    assert!(Arc::ptr_eq(&registration.upgrade().unwrap(), &lease.slot));
    assert_eq!(Arc::strong_count(&lease.slot), 1);
}

// -----------------------------------------------------------------------------
// Lease - State
// -----------------------------------------------------------------------------

#[test]
fn test_lease_is_owned() {
    let (mut store, lease) = leased("owned", 0, 5);

    // Loaned at first
    assert!(!lease.is_owned());

    // Owned after owner-side retirement
    store.retire_all();
    assert!(lease.is_owned());
}

#[test]
fn test_lease_is_loaned() {
    let (_store, lease) = leased("loaned", 0, 6);

    // Loaned at first
    assert!(lease.is_loaned());

    // Not after consumer-side promotion
    lease.promote();
    assert!(!lease.is_loaned());
}

// -----------------------------------------------------------------------------
// Lease - Reads
// -----------------------------------------------------------------------------

#[test]
fn test_lease_len() {
    let (_store, lease) = leased("key=value\n", 4, 9);

    // The length is that of the leased span, loaned or owned
    assert_eq!(lease.len(), 5);
    lease.promote();
    assert_eq!(lease.len(), 5);
}

#[test]
fn test_lease_is_empty() {
    let (_store, lease) = leased("key=value\n", 4, 9);

    // A lease over a span is never empty
    assert!(!lease.is_empty());
    lease.promote();
    assert!(!lease.is_empty());
}

#[test]
fn test_lease_bytes() {
    let (store, lease) = leased("nested", 0, 6);
    let direct = store.bytes(Span::new(0, 6).unwrap()).unwrap();

    // While loaned, the guard reads the retained allocation itself
    let first = lease.bytes();
    assert_eq!(&*first, b"nested");
    assert_eq!(first.as_ptr(), direct.as_ptr());

    // Guards nest on one lease, and reads that take another guard stay legal meanwhile
    let second = lease.bytes();
    assert_eq!(&*second, b"nested");
    assert_eq!(lease.len(), 6);
    assert_eq!(lease.to_vec(), b"nested");

    // Promotion neither blocks on nor invalidates the live guards
    lease.promote();
    assert!(lease.is_owned());
    assert_eq!(&*first, b"nested");
    assert_eq!(&*second, b"nested");
    assert_eq!(first.as_ptr(), direct.as_ptr());

    // Once owned, fresh guards read the private copy
    let third = lease.bytes();
    assert_eq!(&*third, b"nested");
    assert_ne!(third.as_ptr(), direct.as_ptr());

    // A guard outlives the lease and the store
    drop(first);
    drop(second);
    drop(lease);
    drop(store);
    assert_eq!(&*third, b"nested");
}

#[test]
fn test_lease_promote() {
    let mut store = Store::new();
    let id = store.park(0, b"prefix:value:suffix".to_vec()).unwrap();
    let lease = store.lease(Span::new(7, 12).unwrap()).unwrap();
    let guard = lease.bytes();
    assert_eq!(store.live_bytes(id), Some(5));

    // Promotion takes the copy and releases the pin without store access
    lease.promote();
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"value");
    assert_eq!(store.live_bytes(id), Some(0));
    assert_eq!(store.retained_len(), 19);

    // The guard taken beforehand keeps reading the original allocation
    assert_eq!(&*guard, b"value");

    // Promotion is idempotent
    lease.promote();
    assert!(lease.is_owned());
    assert_eq!(&*lease.bytes(), b"value");

    // Owner-side retirement leaves an already promoted lease alone
    assert!(store.retire(id));
    assert_eq!(&*lease.bytes(), b"value");
}

#[test]
fn test_lease_to_vec() {
    let (_store, lease) = leased("key=value\n", 4, 9);

    // The copy matches the bytes and leaves the state alone
    assert_eq!(lease.to_vec(), b"value");
    assert!(lease.is_loaned());

    lease.promote();
    assert_eq!(lease.to_vec(), b"value");
}

// -----------------------------------------------------------------------------
// Lease - Threads
// -----------------------------------------------------------------------------

#[test]
fn test_lease_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<Lease>();
    assert_send_sync::<LeaseBytes>();
    assert_send_sync::<Store>();
}

#[test]
fn test_lease_threads() {
    let (mut store, lease) = leased("shared-bytes", 0, 6);
    let clone = lease.clone();

    // A lease moves to another thread and reads while the owner retires underneath it
    let reader = thread::spawn(move || {
        let before = lease.to_vec();
        lease.promote();
        let after = lease.to_vec();

        (before, after)
    });
    store.retire_all();
    let (before, after) = reader.join().unwrap();

    assert_eq!(before, b"shared");
    assert_eq!(after, b"shared");
    assert!(clone.is_owned());
    assert_eq!(&*clone.bytes(), b"shared");
}

// -----------------------------------------------------------------------------
// LeaseBytes
// -----------------------------------------------------------------------------

#[test]
fn test_lease_bytes_deref() {
    let (mut store, lease) = leased("key=value\n", 4, 9);

    // Loaned guards dereference to the segment bytes
    let loaned = lease.bytes();
    assert_eq!(&*loaned, b"value");
    assert_eq!(loaned.len(), 5);
    assert_eq!(loaned.first(), Some(&b'v'));

    // Owned guards dereference to the copy
    store.retire_all();
    let owned = lease.bytes();
    assert_eq!(&*owned, b"value");
    assert_ne!(owned.as_ptr(), loaned.as_ptr());
}

#[test]
fn test_lease_bytes_as_ref() {
    let (_store, lease) = leased("key=value\n", 4, 9);
    let guard = lease.bytes();

    // `AsRef` agrees with `Deref`
    assert_eq!(guard.as_ref(), b"value");
    assert_eq!(guard.as_ref().as_ptr(), guard.as_ptr());
}

#[test]
fn test_lease_bytes_debug() {
    let (_store, lease) = leased("ab", 0, 2);

    // The guard prints as the byte slice it dereferences to
    assert_eq!(format!("{:?}", lease.bytes()), "[97, 98]");
    lease.promote();
    assert_eq!(format!("{:?}", lease.bytes()), "[97, 98]");
}
