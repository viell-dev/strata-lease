//! Integration tests: adversarial scenarios against the public API
//!
//! Where `consumer.rs` shows the crate used as intended, these scenarios try to break it: races
//! between consumers and owner reclamation with the interleaving forced as tight as possible, and
//! a randomized lifecycle checked step by step against a reference model. Each still reads top to
//! bottom as a story, but the point is the invariant every step asserts, not the usage.
//!
//! Like all example material in this crate, this file is additionally licensed under MIT-0.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "Okay in tests"
)]

use std::sync::{Arc, Barrier};
use std::thread;
use strata_lease::{Lease, SegmentId, SegmentStatus, Span, Store};

/// Iteration budget that keeps Miri runs tractable while stressing native runs harder.
const fn budget(miri: usize, native: usize) -> usize {
    if cfg!(miri) { miri } else { native }
}

/// Test helper to build a span that is known to be valid.
fn span(start: u64, end: u64) -> Span {
    Span::new(start, end).unwrap()
}

/// Deterministic xorshift generator so randomized failures replay exactly.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x.wrapping_shl(13);
        x ^= x.wrapping_shr(7);
        x ^= x.wrapping_shl(17);
        self.0 = x;

        x
    }

    /// Returns a value in `0..bound`, or `0` when `bound` is `0`.
    fn below(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound.max(1)).unwrap();
        usize::try_from(self.next() % bound).unwrap()
    }
}

// -----------------------------------------------------------------------------
// Retirement races readers, promoters, and cloners
// -----------------------------------------------------------------------------

#[test]
fn test_retirement_races_readers_promoters_and_cloners() {
    let spans = [span(0, 20), span(20, 40), span(40, 60), span(60, 80)];
    let expected = b"0123456789".repeat(2);

    // Many rounds, because the interesting interleavings are rare
    for _ in 0..budget(4, 64) {
        let mut store = Store::new();
        store.park(0, b"0123456789".repeat(8)).unwrap();
        let barrier = Arc::new(Barrier::new(spans.len() + 1));
        let mut workers = Vec::new();

        /* Each worker holds a lease and waits at the barrier, so that all of them and the owner
        start their work at the same instant */
        for span in spans {
            let lease = store.lease(span).unwrap();
            let barrier = Arc::clone(&barrier);

            workers.push(thread::spawn(move || {
                barrier.wait();

                // Clone, read, hold a guard across a promotion, then read through the clone
                let clone = lease.clone();
                let before = lease.to_vec();
                let guard = clone.bytes();
                lease.promote();
                let held_across_promotion = guard.to_vec();
                drop(guard);
                let after = clone.to_vec();

                (before, held_across_promotion, after)
            }));
        }

        // The owner reclaims everything while every worker is mid-read or mid-promote
        barrier.wait();
        store.retire_all();
        assert_eq!(store.retained_len(), 0);

        // Every interleaving yields the same bytes at every step
        for worker in workers {
            let (before, held, after) = worker.join().unwrap();

            assert_eq!(before, expected);
            assert_eq!(held, expected);
            assert_eq!(after, expected);
        }
    }
}

// -----------------------------------------------------------------------------
// A randomized lifecycle matches a reference model
// -----------------------------------------------------------------------------

/// One parked chunk in the reference model: its identifier and the stream range it covers.
struct Chunk {
    id: SegmentId,
    start: usize,
    end: usize,
}

/// Checks every held lease and the store's view of every chunk against the reference stream.
fn verify(store: &Store, chunks: &[Chunk], held: &[(Lease, Vec<u8>)], stream: &[u8]) {
    // Every held lease still answers with the bytes it was created for
    for (lease, expected) in held {
        assert_eq!(&*lease.bytes(), expected.as_slice());
        assert_eq!(lease.len(), expected.len());
    }

    // Every chunk is either fully readable or fully gone, and its identifier still answers
    for chunk in chunks {
        let span = span(
            u64::try_from(chunk.start).unwrap(),
            u64::try_from(chunk.end).unwrap(),
        );
        let expected = &stream[chunk.start..chunk.end];

        match store.status(chunk.id) {
            Some(SegmentStatus::Open | SegmentStatus::Sealed) => {
                assert_eq!(store.bytes(span), Some(expected));
                assert_eq!(store.copy_bytes(span), Some(expected.to_vec()));
            }
            Some(SegmentStatus::Retired) => {
                assert_eq!(store.bytes(span), None);
                assert_eq!(store.copy_bytes(span), None);
            }
            None => panic!("chunk identifiers should stay valid"),
        }
    }
}

#[test]
fn test_randomized_lifecycle_matches_reference_model() {
    // The reference stream: a deterministic byte pattern the model can always recompute from
    let stream: Vec<u8> = (0u64..)
        .map(|index| u8::try_from(index.wrapping_mul(31) % 251).unwrap())
        .take(budget(512, 8 * 1024))
        .collect();

    // Several seeds, each a fully deterministic run; a failure replays exactly from its seed
    for seed in [
        0x9E37_79B9_7F4A_7C15_u64,
        0x0000_0000_0000_002A,
        0x517C_C1B7_2722_0A95,
    ] {
        let mut rng = XorShift(seed);
        let mut store = Store::new();
        let mut chunks: Vec<Chunk> = Vec::new();

        // Park the stream as consecutive chunks of random length
        let mut offset = 0usize;
        while offset < stream.len() {
            let len = (rng.below(96) + 1).min(stream.len() - offset);
            let end = offset + len;
            let id = store
                .park(u64::try_from(offset).unwrap(), stream[offset..end].to_vec())
                .unwrap();
            chunks.push(Chunk {
                id,
                start: offset,
                end,
            });
            offset = end;
        }

        // Held leases, each with the bytes it must keep answering whatever happens around it
        let mut held: Vec<(Lease, Vec<u8>)> = Vec::new();

        for step in 0..budget(256, 4096) {
            match rng.next() % 8 {
                // Lease a random subspan of a random chunk
                0..=2 => {
                    let chunk = &chunks[rng.below(chunks.len())];
                    let a = chunk.start + rng.below(chunk.end - chunk.start);
                    let b = (a + rng.below(24) + 1).min(chunk.end);
                    let span = span(u64::try_from(a).unwrap(), u64::try_from(b).unwrap());

                    match store.lease(span) {
                        Some(lease) => held.push((lease, stream[a..b].to_vec())),
                        // The chunk was already retired; the span must really be gone
                        None => {
                            assert_eq!(store.status(chunk.id), Some(SegmentStatus::Retired));
                        }
                    }
                }
                // Promote a random held lease
                3 => {
                    if let Some((lease, _)) = held.get(rng.below(held.len())) {
                        lease.promote();
                    }
                }
                // Clone a random held lease; the clone answers for the same bytes
                4 => {
                    if let Some((lease, expected)) = held.get(rng.below(held.len())) {
                        let (lease, expected) = (lease.clone(), expected.clone());
                        held.push((lease, expected));
                    }
                }
                // Drop a random held lease
                5 => {
                    if !held.is_empty() {
                        let index = rng.below(held.len());
                        held.swap_remove(index);
                    }
                }
                // Sweep with a random threshold
                6 => {
                    let _ = store.maintain(rng.below(128));
                }
                // Retire a random chunk outright
                7 => {
                    let _ = store.retire(chunks[rng.below(chunks.len())].id);
                }
                _ => unreachable!("op selector is modulo 8"),
            }

            // Periodically compare the whole state against the model
            if step % 16 == 0 {
                verify(&store, &chunks, &held, &stream);
            }
        }

        // Retiring everything leaves every held lease owned, with its original bytes
        store.retire_all();
        assert_eq!(store.retained_len(), 0);
        verify(&store, &chunks, &held, &stream);

        for (lease, expected) in &held {
            assert!(lease.is_owned());
            assert_eq!(&*lease.bytes(), expected.as_slice());
        }
    }
}
