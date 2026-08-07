//! Consumer-facing integration coverage: the full lease lifecycle driven the way a downstream
//! parser, DOM, or resolver would drive it, using only the public API.

use std::thread;
use strata_lease::{Lease, SegmentId, Span, Store};

fn span(start: u64, end: u64) -> Span {
    match Span::new(start, end) {
        Ok(span) => span,
        Err(error) => panic!("test span {start}..{end} should be valid: {error}"),
    }
}

fn park<B: AsRef<[u8]>>(store: &mut Store<B>, base: u64, bytes: B) -> SegmentId {
    match store.park(base, bytes) {
        Ok(id) => id,
        Err(error) => panic!("test park at {base} should succeed: {error}"),
    }
}

/// A consumer that reads through the handle without knowing (or caring) which state it is in.
fn checksum(lease: &Lease) -> usize {
    lease.bytes().iter().map(|&byte| usize::from(byte)).sum()
}

#[test]
fn parser_pipeline_keeps_values_and_reclaims_buffers() {
    // A `key=value;` document arriving in three chunks, parked as it is consumed. Keys are read
    // zero-copy and discarded; values become long-lived DOM leases.
    let chunks: [(&[u8], u64, Span, Span); 3] = [
        (b"alpha=1;", 0, span(0, 5), span(6, 7)),
        (b"beta=22;", 8, span(8, 12), span(13, 15)),
        (b"gamma=333;", 16, span(16, 21), span(22, 25)),
    ];

    let mut store = Store::new();
    let mut dom = Vec::new();

    for (chunk, base, key_span, value_span) in chunks {
        // Parked open ahead of leasing, so a maintenance sweep cannot race the parser here.
        let Ok(id) = store.park_open(base, chunk.to_vec()) else {
            panic!("test park at {base} should succeed");
        };

        let Some(key) = store.lease(key_span) else {
            panic!("key span should be retained");
        };
        assert!(!key.bytes().is_empty());
        drop(key);

        let Some(value) = store.lease(value_span) else {
            panic!("value span should be retained");
        };
        dom.push(value);

        store.seal(id);
    }

    assert_eq!(store.retained_len(), 26);
    let sums: Vec<usize> = dom.iter().map(checksum).collect();

    // Every chunk wastes at least six bytes on its dead key and syntax, so a sweep frees them
    // all; the three live values are promoted in place.
    assert_eq!(store.maintain(6), 3);
    assert_eq!(store.retained_len(), 0);

    let expected: [&[u8]; 3] = [b"1", b"22", b"333"];

    for ((lease, expected), sum) in dom.iter().zip(expected).zip(sums) {
        assert!(lease.is_owned());
        assert_eq!(&*lease.bytes(), expected);
        assert_eq!(checksum(lease), sum);
    }
}

#[test]
fn loaned_reads_are_zero_copy_until_promotion() {
    let mut store = Store::new();
    park(&mut store, 0, b"zero-copy".to_vec());

    let Some(lease) = store.lease(span(0, 4)) else {
        panic!("span should be retained");
    };
    let Some(direct) = store.bytes(span(0, 4)) else {
        panic!("span should be retained");
    };

    // While loaned, the lease reads the retained allocation itself.
    assert_eq!(lease.bytes().as_ptr(), direct.as_ptr());

    lease.promote();

    // Once owned, the lease reads its private copy.
    assert_ne!(lease.bytes().as_ptr(), direct.as_ptr());
    assert_eq!(&*lease.bytes(), b"zero");
}

#[test]
fn concurrent_consumers_survive_owner_reclamation() {
    let mut store = Store::new();
    park(&mut store, 0, b"0123456789".repeat(4));

    let spans = [span(0, 10), span(10, 20), span(20, 30), span(30, 40)];
    let mut workers = Vec::new();

    for span in spans {
        let Some(lease) = store.lease(span) else {
            panic!("span should be retained");
        };

        workers.push(thread::spawn(move || {
            let before = lease.to_vec();
            lease.promote();
            let after = lease.to_vec();

            (before, after)
        }));
    }

    // The owner reclaims while the workers are reading and promoting.
    store.retire_all();
    assert_eq!(store.retained_len(), 0);

    for worker in workers {
        let Ok((before, after)) = worker.join() else {
            panic!("worker should complete");
        };

        assert_eq!(before, b"0123456789".to_vec());
        assert_eq!(after, b"0123456789".to_vec());
    }
}

#[test]
fn fragmented_leases_read_across_parked_chunks() {
    let mut store = Store::new();
    park(&mut store, 0, b"alpha".to_vec());
    park(&mut store, 5, b"beta".to_vec());

    let Some(parts) = store.leases(span(3, 7)) else {
        panic!("cross-chunk span should be retained");
    };
    let joined: Vec<u8> = parts.iter().flat_map(Lease::to_vec).collect();

    assert_eq!(joined, b"habe".to_vec());

    store.retire_all();
    let rejoined: Vec<u8> = parts.iter().flat_map(Lease::to_vec).collect();

    assert_eq!(rejoined, b"habe".to_vec());
}

#[test]
fn owner_can_vanish_without_retiring() {
    // Without retirement the crate degrades to plain refcounting: the last loaned lease keeps
    // the parked buffer alive, so nothing dangles.
    let mut store = Store::new();
    park(&mut store, 0, b"refcounted".to_vec());
    let Some(lease) = store.lease(span(0, 3)) else {
        panic!("span should be retained");
    };

    drop(store);

    assert!(lease.is_loaned());
    assert_eq!(lease.to_vec(), b"ref".to_vec());
}

struct Region {
    bytes: Box<[u8]>,
}

impl AsRef<[u8]> for Region {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

#[test]
fn store_accepts_custom_buffer_types() {
    let mut store = Store::new();
    park(
        &mut store,
        0,
        Region {
            bytes: Box::from(b"custom-buffer".as_slice()),
        },
    );

    let Some(lease) = store.lease(span(0, 6)) else {
        panic!("span should be retained");
    };

    assert_eq!(store.retire_all(), 1);
    assert_eq!(&*lease.bytes(), b"custom");
}
