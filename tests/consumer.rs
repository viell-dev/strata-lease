//! Integration tests: the lease lifecycle as a consumer sees it
//!
//! Each test is one scenario driven the way a downstream parser, document model, or resolver would
//! drive it, through the public API only. A scenario reads top to bottom as a story: the situation,
//! the steps the owner and its consumers take, and what each side can rely on afterwards. Read
//! them as worked usage examples. The per-method contracts they build on are pinned down by the
//! unit tests in `src/<module>/tests.rs`.
//!
//! Like all example material in this crate, this file is additionally licensed under MIT-0.

#![expect(clippy::unwrap_used, reason = "Okay in tests")]

use std::thread;
use strata_lease::{Lease, Span, Store};

/// A consumer that reads through the handle without knowing, or caring, which state it is in.
fn checksum(lease: &Lease) -> usize {
    lease.bytes().iter().map(|&byte| usize::from(byte)).sum()
}

/// Test helper to build a span that is known to be valid.
fn span(start: u64, end: u64) -> Span {
    Span::new(start, end).unwrap()
}

// -----------------------------------------------------------------------------
// A parser keeps values and reclaims its buffers
// -----------------------------------------------------------------------------

#[test]
fn test_parser_pipeline_keeps_values_and_reclaims_buffers() {
    /* A `key=value;` document arrives in three chunks. The parser parks each chunk as soon as it
    has consumed it, reads the key zero-copy and forgets it, and keeps the value as a long-lived
    lease in its document model. Each chunk records the bytes it holds, the absolute offset it was
    read at, and the spans of its key and value. */
    let chunks: [(&[u8], u64, Span, Span); 3] = [
        (b"alpha=1;", 0, span(0, 5), span(6, 7)),
        (b"beta=22;", 8, span(8, 12), span(13, 15)),
        (b"gamma=333;", 16, span(16, 21), span(22, 25)),
    ];

    let mut store = Store::new();
    let mut document = Vec::new();

    for (chunk, base, key_span, value_span) in chunks {
        /* Park the chunk open, so a maintenance sweep run from elsewhere cannot retire it before
        the parser has attached its leases */
        let id = store.park_open(base, chunk.to_vec()).unwrap();

        // The key is only needed for a moment: lease it, inspect it, let it go
        let key = store.lease(key_span).unwrap();
        assert!(!key.bytes().is_empty());
        drop(key);

        // The value outlives this loop, so its lease goes into the document model
        document.push(store.lease(value_span).unwrap());

        // The chunk is fully parsed; sweeps may now consider it
        store.seal(id);
    }

    // Every chunk is still retained in full, and the values read straight from them
    assert_eq!(store.retained_len(), 26);
    let sums: Vec<usize> = document.iter().map(checksum).collect();

    /* Each chunk wastes at least six bytes on its dead key and syntax, so a sweep at that
    threshold frees all three. The live values are promoted in place. */
    assert_eq!(store.maintain(6), 3);
    assert_eq!(store.retained_len(), 0);

    // The document model did not notice: same bytes, same checksums, now from private copies
    let expected: [&[u8]; 3] = [b"1", b"22", b"333"];
    for ((lease, expected), sum) in document.iter().zip(expected).zip(sums) {
        assert!(lease.is_owned());
        assert_eq!(&*lease.bytes(), expected);
        assert_eq!(checksum(lease), sum);
    }
}

// -----------------------------------------------------------------------------
// Reads are zero-copy until promotion
// -----------------------------------------------------------------------------

#[test]
fn test_loaned_reads_are_zero_copy_until_promotion() {
    let mut store = Store::new();
    store.park(0, b"zero-copy".to_vec()).unwrap();

    // The owner peeks at some bytes and a consumer leases the same bytes
    let lease = store.lease(span(0, 4)).unwrap();
    let direct = store.bytes(span(0, 4)).unwrap();

    // While loaned, the lease reads the retained allocation itself: the same address
    assert_eq!(lease.bytes().as_ptr(), direct.as_ptr());

    // The consumer decides to own its bytes
    lease.promote();

    // Now it reads a private copy at a different address, with identical contents
    assert_ne!(lease.bytes().as_ptr(), direct.as_ptr());
    assert_eq!(&*lease.bytes(), b"zero");
}

// -----------------------------------------------------------------------------
// Consumers on other threads survive owner reclamation
// -----------------------------------------------------------------------------

#[test]
fn test_concurrent_consumers_survive_owner_reclamation() {
    let mut store = Store::new();
    store.park(0, b"0123456789".repeat(4)).unwrap();

    // Four workers each take a lease and go off to read and promote it on their own thread
    let spans = [span(0, 10), span(10, 20), span(20, 30), span(30, 40)];
    let mut workers = Vec::new();

    for span in spans {
        let lease = store.lease(span).unwrap();

        workers.push(thread::spawn(move || {
            let before = lease.to_vec();
            lease.promote();
            let after = lease.to_vec();

            (before, after)
        }));
    }

    // Meanwhile the owner reclaims everything, without waiting for the workers
    store.retire_all();
    assert_eq!(store.retained_len(), 0);

    // Whatever the interleaving, every worker read the same bytes before and after promotion
    for worker in workers {
        let (before, after) = worker.join().unwrap();

        assert_eq!(before, b"0123456789");
        assert_eq!(after, b"0123456789");
    }
}

// -----------------------------------------------------------------------------
// A span across parked chunks reads as one value
// -----------------------------------------------------------------------------

#[test]
fn test_fragmented_leases_read_across_parked_chunks() {
    let mut store = Store::new();
    store.park(0, b"alpha".to_vec()).unwrap();
    store.park(5, b"beta".to_vec()).unwrap();

    // A value straddles the chunk boundary: `leases` hands back one lease per chunk
    let parts = store.leases(span(3, 7)).unwrap();
    assert_eq!(parts.len(), 2);

    // The consumer concatenates the pieces to read the value
    let joined: Vec<u8> = parts.iter().flat_map(Lease::to_vec).collect();
    assert_eq!(joined, b"habe");

    // Retirement promotes each piece, and the value still reads the same
    store.retire_all();
    let rejoined: Vec<u8> = parts.iter().flat_map(Lease::to_vec).collect();
    assert_eq!(rejoined, b"habe");
}

// -----------------------------------------------------------------------------
// The owner can vanish without retiring
// -----------------------------------------------------------------------------

#[test]
fn test_owner_can_vanish_without_retiring() {
    let mut store = Store::new();
    store.park(0, b"refcounted".to_vec()).unwrap();
    let lease = store.lease(span(0, 3)).unwrap();

    // The owner goes away without ever calling `retire`
    drop(store);

    /* The crate degrades to plain reference counting: the last loaned lease keeps the parked
    buffer alive, so nothing dangles and the lease is still loaned */
    assert!(lease.is_loaned());
    assert_eq!(lease.to_vec(), b"ref");
}

// -----------------------------------------------------------------------------
// Any stable buffer type can be parked
// -----------------------------------------------------------------------------

/// A buffer type from outside the crate: a boxed slice instead of a vector.
struct Region {
    bytes: Box<[u8]>,
}

impl AsRef<[u8]> for Region {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

#[test]
fn test_store_accepts_custom_buffer_types() {
    // The store is generic over the buffer: anything stable that is `AsRef<[u8]>`
    let mut store = Store::new();
    store
        .park(
            0,
            Region {
                bytes: Box::from(b"custom-buffer".as_slice()),
            },
        )
        .unwrap();

    // Leases and retirement work exactly as they do for `Vec<u8>`
    let lease = store.lease(span(0, 6)).unwrap();
    assert_eq!(store.retire_all(), 1);
    assert_eq!(&*lease.bytes(), b"custom");
}
