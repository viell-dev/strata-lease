use crate::{Lease, Span, Store};
use std::thread;

fn span(start: u64, end: u64) -> Span {
    match Span::new(start, end) {
        Ok(span) => span,
        Err(error) => panic!("test span {start}..{end} should be valid: {error}"),
    }
}

fn leased(text: &str, start: u64, end: u64) -> (Store, Lease) {
    let mut store = Store::new();
    if let Err(error) = store.park(0, text.as_bytes().to_vec()) {
        panic!("test park should succeed: {error}");
    }
    let Some(lease) = store.lease(span(start, end)) else {
        panic!("span should be retained");
    };

    (store, lease)
}

#[test]
fn lease_reads_uniformly_before_and_after_promotion() {
    let (_store, lease) = leased("key=value\n", 4, 9);

    assert!(lease.is_loaned());
    assert!(!lease.is_owned());
    assert_eq!(lease.len(), 5);
    assert!(!lease.is_empty());
    assert_eq!(lease.bytes().as_ref(), b"value");

    lease.promote();
    lease.promote();

    assert!(lease.is_owned());
    assert!(!lease.is_loaned());
    assert_eq!(lease.len(), 5);
    assert_eq!(&*lease.bytes(), b"value");
    assert_eq!(lease.to_vec(), b"value".to_vec());
}

#[test]
fn nested_guards_on_one_lease_do_not_deadlock() {
    let (_store, lease) = leased("nested", 0, 6);

    let first = lease.bytes();
    let second = lease.bytes();

    assert_eq!(&*first, b"nested");
    assert_eq!(&*second, b"nested");
    // Reads that internally take another guard stay legal while guards are held.
    assert_eq!(lease.len(), 6);
    assert_eq!(lease.to_vec(), b"nested".to_vec());
}

#[test]
fn promotion_does_not_block_or_invalidate_active_guards() {
    let (_store, lease) = leased("guarded", 0, 7);
    let guard = lease.bytes();

    lease.promote();

    assert!(lease.is_owned());
    assert_eq!(&*guard, b"guarded");

    drop(guard);

    assert_eq!(&*lease.bytes(), b"guarded");
}

#[test]
fn promotion_reaches_every_clone() {
    let (_store, lease) = leased("cloned", 0, 6);
    let clone = lease.clone();

    lease.promote();

    assert!(clone.is_owned());
    assert_eq!(&*clone.bytes(), b"cloned");
}

#[test]
fn leases_move_across_threads() {
    let (mut store, lease) = leased("shared-bytes", 0, 6);

    let reader = thread::spawn(move || lease.to_vec());
    store.retire_all();
    let Ok(bytes) = reader.join() else {
        panic!("reader thread should complete");
    };

    assert_eq!(bytes, b"shared".to_vec());
}

#[test]
fn leases_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<Lease>();
    assert_send_sync::<Store>();
}

#[test]
fn lease_debug_reports_promotion_state() {
    let (_store, lease) = leased("debug", 0, 5);

    assert!(format!("{lease:?}").contains("owned: false"));
    lease.promote();
    assert!(format!("{lease:?}").contains("owned: true"));
}
