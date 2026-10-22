//! Compare lease costs with copying, and measure lease churn and segment reclamation.
//! Each module states its question and timing boundary, then checks outputs before measuring.
//! Run `cargo bench --bench lease`; use `-- --test` to run the correctness checks without timing.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "Bounded benchmark fixtures; a failed check must stop the benchmark"
)]

use criterion::{criterion_group, criterion_main};
use strata_lease::Span;

#[path = "lease/access.rs"]
mod access;
#[path = "lease/churn.rs"]
mod churn;
#[path = "lease/reclaim.rs"]
mod reclaim;

/// Returns `len` pseudo-random bytes, so a shifted or misplaced range cannot pass a content check
/// the way it could against a repeating pattern.
fn source(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

/// Returns the span `start..end`.
fn span(start: usize, end: usize) -> Span {
    Span::new(u64::try_from(start).unwrap(), u64::try_from(end).unwrap()).unwrap()
}

criterion_group!(benches, access::bench, churn::bench, reclaim::bench);
criterion_main!(benches);
