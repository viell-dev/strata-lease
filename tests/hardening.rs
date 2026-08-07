//! Adversarial coverage: barrier-controlled races between consumers and owner reclamation, and a
//! deterministic randomized lifecycle exercised against a reference model.

use std::sync::{Arc, Barrier};
use std::thread;
use strata_lease::{Lease, SegmentId, Span, Store};

/// Iteration budget that keeps Miri runs tractable while stressing native runs harder.
fn budget(miri: usize, native: usize) -> usize {
    if cfg!(miri) { miri } else { native }
}

fn span(start: u64, end: u64) -> Span {
    match Span::new(start, end) {
        Ok(span) => span,
        Err(error) => panic!("test span {start}..{end} should be valid: {error}"),
    }
}

fn park(store: &mut Store, base: u64, bytes: Vec<u8>) -> SegmentId {
    match store.park(base, bytes) {
        Ok(id) => id,
        Err(error) => panic!("test park at {base} should succeed: {error}"),
    }
}

#[test]
fn retirement_races_readers_promoters_and_cloners() {
    let spans = [span(0, 20), span(20, 40), span(40, 60), span(60, 80)];
    let expected = b"0123456789".repeat(2);

    for _ in 0..budget(4, 64) {
        let mut store = Store::new();
        park(&mut store, 0, b"0123456789".repeat(8));
        let barrier = Arc::new(Barrier::new(spans.len().saturating_add(1)));
        let mut workers = Vec::new();

        for span in spans {
            let Some(lease) = store.lease(span) else {
                panic!("span should be retained");
            };
            let barrier = Arc::clone(&barrier);

            workers.push(thread::spawn(move || {
                barrier.wait();
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

        // The owner reclaims everything while every worker is mid-read/promote.
        barrier.wait();
        store.retire_all();
        assert_eq!(store.retained_len(), 0);

        for worker in workers {
            let Ok((before, held, after)) = worker.join() else {
                panic!("worker should complete");
            };

            assert_eq!(before, expected);
            assert_eq!(held, expected);
            assert_eq!(after, expected);
        }
    }
}

/// Deterministic xorshift generator so failures replay exactly.
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

    fn below(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound.max(1)).unwrap_or(u64::MAX);
        usize::try_from(self.next().checked_rem(bound).unwrap_or(0)).unwrap_or(0)
    }
}

/// One parked chunk in the reference model: its identifier and the stream range it covers.
struct Chunk {
    id: SegmentId,
    start: usize,
    end: usize,
}

#[test]
fn randomized_lifecycle_matches_reference_model() {
    let stream: Vec<u8> = (0u64..)
        .map(|index| u8::try_from(index.wrapping_mul(31) % 251).unwrap_or(0))
        .take(budget(512, 8 * 1024))
        .collect();

    for seed in [
        0x9E37_79B9_7F4A_7C15_u64,
        0x0000_0000_0000_002A,
        0x517C_C1B7_2722_0A95,
    ] {
        let mut rng = XorShift(seed);
        let mut store = Store::new();
        let mut chunks: Vec<Chunk> = Vec::new();

        // Park the stream as consecutive chunks of random length.
        let mut offset = 0usize;
        while offset < stream.len() {
            let len = rng
                .below(96)
                .saturating_add(1)
                .min(stream.len().saturating_sub(offset));
            let end = offset.saturating_add(len);
            let Some(bytes) = stream.get(offset..end) else {
                panic!("chunk range should be inside the stream");
            };
            let id = park(
                &mut store,
                u64::try_from(offset).unwrap_or(u64::MAX),
                bytes.to_vec(),
            );
            chunks.push(Chunk {
                id,
                start: offset,
                end,
            });
            offset = end;
        }

        // Held leases with the bytes they must keep answering, whatever happens around them.
        let mut held: Vec<(Lease, Vec<u8>)> = Vec::new();

        for step in 0..budget(256, 4096) {
            match rng.next() % 8 {
                // Lease a random subspan of a random chunk.
                0..=2 => {
                    let Some(chunk) = chunks.get(rng.below(chunks.len())) else {
                        continue;
                    };
                    let len = chunk.end.saturating_sub(chunk.start);
                    let a = chunk.start.saturating_add(rng.below(len));
                    let b = a
                        .saturating_add(rng.below(24).saturating_add(1))
                        .min(chunk.end);
                    let span = span(
                        u64::try_from(a).unwrap_or(u64::MAX),
                        u64::try_from(b).unwrap_or(u64::MAX),
                    );
                    let Some(expected) = stream.get(a..b) else {
                        panic!("model range should be inside the stream");
                    };

                    match store.lease(span) {
                        Some(lease) => held.push((lease, expected.to_vec())),
                        // The chunk was already retired; the span must really be gone.
                        None => assert_eq!(store.is_retired(chunk.id), Some(true)),
                    }
                }
                // Promote a random held lease.
                3 => {
                    if let Some((lease, _)) = held.get(rng.below(held.len())) {
                        lease.promote();
                    }
                }
                // Clone a random held lease; the clone answers for the same bytes.
                4 => {
                    if let Some((lease, expected)) = held.get(rng.below(held.len())) {
                        let (lease, expected) = (lease.clone(), expected.clone());
                        held.push((lease, expected));
                    }
                }
                // Drop a random held lease.
                5 => {
                    if !held.is_empty() {
                        let index = rng.below(held.len());
                        held.swap_remove(index);
                    }
                }
                // Sweep with a random threshold.
                6 => {
                    let _ = store.maintain(rng.below(128));
                }
                // Retire a random chunk outright.
                7 => {
                    if let Some(chunk) = chunks.get(rng.below(chunks.len())) {
                        let _ = store.retire(chunk.id);
                    }
                }
                _ => unreachable!("op selector is modulo 8"),
            }

            // Periodically verify every held lease and the store's own view.
            if step % 16 == 0 {
                verify(&store, &chunks, &held, &stream);
            }
        }

        store.retire_all();
        assert_eq!(store.retained_len(), 0);

        for (lease, expected) in &held {
            assert!(lease.is_owned());
            assert_eq!(&*lease.bytes(), expected.as_slice());
        }
    }
}

fn verify(store: &Store, chunks: &[Chunk], held: &[(Lease, Vec<u8>)], stream: &[u8]) {
    for (lease, expected) in held {
        assert_eq!(&*lease.bytes(), expected.as_slice());
        assert_eq!(lease.len(), expected.len());
    }

    for chunk in chunks {
        let span = span(
            u64::try_from(chunk.start).unwrap_or(u64::MAX),
            u64::try_from(chunk.end).unwrap_or(u64::MAX),
        );
        let Some(expected) = stream.get(chunk.start..chunk.end) else {
            panic!("chunk range should be inside the stream");
        };

        match store.is_retired(chunk.id) {
            Some(false) => {
                assert_eq!(store.bytes(span), Some(expected));
                assert_eq!(store.copy_bytes(span), Some(expected.to_vec()));
            }
            Some(true) => {
                assert_eq!(store.bytes(span), None);
                assert_eq!(store.copy_bytes(span), None);
            }
            None => panic!("chunk identifiers should stay valid"),
        }
    }
}
