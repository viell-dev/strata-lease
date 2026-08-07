//! Revocable zero-copy sharing of retained byte buffers.
//!
//! A data owner hands out cheap views into memory it retains instead of copies. Plain borrowing
//! pins that memory for as long as any view exists, and reference counting keeps the whole
//! allocation alive until the last view dies, so one small straggler can pin megabytes. This crate
//! sits between the two: sharing is reference counted, but every view is a [`Lease`] that can be
//! *promoted* — switched in place from pointing into the retained segment to holding its own small
//! copy. That gives the owner an escape hatch neither plain borrowing nor plain reference counting
//! allows: promote the stragglers, then free the large allocation unconditionally.
//!
//! # How it works
//!
//! - A [`Store`] retains parked byte segments, each with an absolute base offset. Parking
//!   validates the segment invariants up front — non-empty, ascending non-overlapping bases,
//!   addressable within `u64` — and rejects anything else with a [`ParkError`] that hands the
//!   buffer back.
//! - [`Store::lease`] maps an absolute [`Span`] — non-empty by construction; [`Span::new`]
//!   rejects empty and reversed bounds with a [`SpanError`] — to a [`Lease`]: a cheap, cloneable,
//!   thread-safe
//!   handle that is either *loaned* (reads from the retained segment it pins) or *owned* (holds
//!   its own copy). Consumers read through [`Lease::bytes`] and never need to care which state the
//!   lease is in.
//! - The store tracks the live loaned bytes pinning each segment. When retaining a segment stops
//!   being worth it, [`Store::maintain`] (threshold-driven), [`Store::retire`], or
//!   [`Store::retire_all`] promote every surviving lease and release the segment.
//! - [`Lease::promote`] lets a consumer take its own copy early and release its pin without
//!   involving the store.
//!
//! Dropping the [`Store`] without retiring is safe: each loaned lease shares ownership of its
//! segment, so the bytes stay alive until the last loaned lease is dropped or promoted. Retirement
//! is a memory-management action, not a safety requirement.
//!
//! # Quick start
//!
//! ```
//! use strata_lease::{Span, Store};
//!
//! // The owner parks a buffer it has finished producing.
//! let mut store = Store::new();
//! store.park(0, b"key=value\n".to_vec()).unwrap();
//!
//! // Consumers hold cheap leases instead of copies.
//! let value = store.lease(Span::new(4, 9).unwrap()).unwrap();
//! assert!(value.is_loaned());
//! assert_eq!(&*value.bytes(), b"value");
//!
//! // Retaining the whole buffer for one small straggler is no longer worth it: retire.
//! // The lease takes its own copy in place and the store releases the buffer.
//! store.retire_all();
//! assert!(value.is_owned());
//! assert_eq!(&*value.bytes(), b"value");
//! assert_eq!(store.retained_len(), 0);
//! ```
//!
//! # Contracts
//!
//! Invalid inputs that can be rejected up front are: [`Span::new`] refuses empty and reversed
//! bounds, and [`Store::park`] refuses segments that would break the store's ordering and
//! addressability invariants. What remains below are runtime contracts; everything in this crate
//! is safe code, and violating one degrades behavior (empty or truncated reads, delayed
//! reclamation) but can never cause memory unsafety.
//!
//! - **Stable buffers.** `Store<B>` accepts any `B: AsRef<[u8]>` and trusts that `as_ref` returns
//!   the same slice — same address, same length, same contents — on every call. An impl that
//!   mutates or swaps its slice between calls safely degrades: reads whose validated range no
//!   longer fits clamp to empty bytes.
//! - **Thread safety.** [`Lease<B>`] and [`Store<B>`] are `Send`/`Sync` exactly when `B` is
//!   `Send + Sync`. Reads never block promotion or retirement: a [`LeaseBytes`] guard snapshots
//!   its own pin on the allocation, so guards nest freely and retirement completes immediately
//!   even while readers are active — the allocation itself is freed when the last outstanding
//!   guard drops.
//! - **Advisory accounting.** Per-segment live-byte counters count lease ranges with
//!   multiplicity (overlapping leases each contribute fully), saturate instead of wrapping, and
//!   only steer [`Store::maintain`]; they are not a precise measure of covered bytes.
//!
//! # Crate organization
//!
//! - [`Store`]: the owner side; parks segments, hands out leases, and decides when to retire.
//! - [`Lease`]: the consumer side; a promotable view that reads uniformly in both states.
//! - [`LeaseBytes`]: the zero-copy read guard returned by [`Lease::bytes`].
//! - [`Span`]: non-empty absolute byte spans used to address bytes across parked segments, with
//!   [`SpanError`] for rejected bounds.
//! - [`ParkError`] / [`ParkErrorKind`]: why [`Store::park`] rejected a buffer, carrying the
//!   buffer back to the caller.

mod lease;
mod span;
mod store;

pub use lease::{Lease, LeaseBytes};
pub use span::{Error as SpanError, Span};
pub use store::{ParkError, ParkErrorKind, SegmentId, Store};

/// Compile-checks the README examples alongside the doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}
