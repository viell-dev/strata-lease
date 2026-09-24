# strata-lease

Revocable zero-copy sharing of retained byte buffers through promotable leases.

A data owner hands out cheap views into memory it retains instead of copies. Normally that pins the
memory: plain borrowing pins it for as long as any view exists, and reference counting (`Arc`,
`bytes::Bytes`) keeps the whole allocation alive until the last view dies, so one tiny straggler can
pin megabytes. This crate sits between the two. Sharing is reference counted, but every view is a
`Lease` that can be _promoted_: switched in place from pointing into the retained buffer to holding
its own small copy. That gives the owner an escape hatch neither model allows: promote the
stragglers, then free the large allocation unconditionally.

The trade in one line: zero-copy in the common case, and a small copy only for leases that outlive
the buffer's usefulness. The owner regains the right to free its memory, which plain borrowing or
reference counting never grants.

This is intended for parsers, tokenizers, protocol decoders, and other producers that retain large
input buffers whose consumers keep small pieces alive for an unpredictable time. The crate has no
dependencies and no `unsafe` code.

## Features

- **Promotable leases**: `Lease` is a cheap, cloneable handle that is either _loaned_ (reads from
  the retained segment it pins) or _owned_ (holds its own copy). `lease.bytes()` reads uniformly
  in both states; consumers never need to know which one they are in.
- **Owner-side reclamation**: `retire(id)` and `retire_all()` promote every surviving lease in
  place and release the segment unconditionally, without ever blocking on active readers.
  `maintain(min_wasted_bytes)` retires segments once their wasted (retained minus live loaned)
  bytes reach a threshold.
- **O(1) retirement checks**: each lease's pin is tracked in a shared per-segment live-byte
  counter, decremented automatically when a lease is dropped or promoted, so `maintain` never has
  to walk the lease registry to decide whether retiring pays off.
- **Consumer-side promotion**: `lease.promote()` takes the copy early and releases the pin without
  involving the store, which suits an ownership boundary such as a resolver or deserializer.
- **Non-blocking reads**: `lease.bytes()` returns a snapshot guard holding its own pin, so guards
  nest freely, never deadlock, and stay valid across concurrent promotion or retirement.
- **Absolute spans**: segments are parked with a base offset, and `Span`s address bytes across the
  whole logical stream. Lookups binary-search the parked segments, and `leases()` covers spans
  that straddle segment boundaries.
- **Invalid states are unrepresentable**: `Span::new` rejects empty and reversed bounds, and
  `park` rejects buffers that would break the store's invariants (empty, out of base order, or
  unaddressable within `u64`), handing the buffer back, so lookups never have to paper over a
  malformed span or a misparked segment.
- **Safe by reference counting**: dropping the `Store` without retiring never dangles a lease,
  because loaned leases share ownership of their segment. Retirement is a memory-management
  action, not a safety requirement.
- **Bounded bookkeeping**: lease registries prune themselves as leases are handed out, and the
  tombstones of retired segments compact away once everything parked before them retires, so a
  streaming park-and-retire loop runs in bounded memory.

## Usage

```rust
use strata_lease::{Span, Store};

// The owner parks a buffer it has finished producing (e.g. a consumed reader buffer)
let mut store = Store::new();
store.park(0, b"key=value\n".to_vec()).unwrap();

// Consumers hold cheap leases instead of copies
let value = store.lease(Span::new(4, 9).unwrap()).unwrap();
assert!(value.is_loaned());
assert_eq!(&*value.bytes(), b"value");

/* Retaining ten bytes for a five-byte straggler is no longer worth it: retire. The lease takes
its own copy in place and the store releases the buffer. */
store.retire_all();
assert!(value.is_owned());
assert_eq!(&*value.bytes(), b"value");
assert_eq!(store.retained_len(), 0);
```

## Threshold-driven maintenance

Instead of retiring unconditionally, the owner can sweep periodically and only retire segments
where the waste justifies the copies:

```rust
use strata_lease::{Span, Store};

let mut store = Store::new();
store.park(0, vec![b'x'; 8 * 1024]).unwrap();
let straggler = store.lease(Span::new(10, 26).unwrap()).unwrap();

// 8 KiB retained for 16 live bytes: wasted >= 4 KiB, so the segment retires
assert_eq!(store.maintain(4 * 1024), 1);
assert!(straggler.is_owned());
assert_eq!(store.retained_len(), 0);
```

`park_open` and `seal` cover streaming producers: an open segment is exempt from `maintain`, so a
sweep cannot retire a buffer that was parked ahead of attaching its leases.

## Buffer types

`Store<B>` and `Lease<B>` are generic over any `B: AsRef<[u8]>` and default to `Vec<u8>`, so the
crate works with plain vectors, `Box<[u8]>`, memory-mapped buffers, or reader-owned buffer types.

## Contracts and caveats

Everything in the crate is safe code; breaking a contract degrades behavior (empty or truncated
reads, delayed reclamation) but can never cause memory unsafety.

- **Stable buffers.** `B::as_ref` must return the same slice (same address, length, and
  contents) on every call. A misbehaving implementation safely degrades: reads whose validated
  range no longer fits clamp to empty bytes, and a panicking `as_ref` leaves promotion retryable.
- **Validated up front.** `Span::new` returns an error for empty or reversed bounds, and `park`
  returns an error (carrying the buffer back) for empty, out-of-order, or `u64`-overflowing
  segments, so a store only ever holds segments its lookups can honor. Gaps between segments are
  fine; spans reaching into a gap resolve to `None`.
- **Thread safety.** `Lease<B>` and `Store<B>` are `Send`/`Sync` exactly when `B` is
  `Send + Sync`. A `LeaseBytes` guard keeps the segment allocation alive on its own until dropped,
  even across retirement, so drop guards promptly rather than storing them.
- **Advisory accounting.** Live-byte counters count lease ranges with multiplicity (overlapping
  leases each contribute fully); they steer `maintain` and are not a precise measure of covered
  bytes. They saturate instead of wrapping, and saturation is sticky, so the accounting error is
  always conservative: a saturated segment is retained, never retired early.
- **Identifier scope.** A `SegmentId` is only meaningful for the store that issued it; it stays
  valid (reporting the segment's settled state) after retirement and compaction.

## Minimum Supported Rust Version

The MSRV is **1.87.0**.

## License

Licensed under the Mozilla Public License, Version 2.0 (see [LICENSE](LICENSE)
or <https://www.mozilla.org/en-US/MPL/2.0/>).

MPL-2.0 is a file-level weak copyleft license: its obligations attach to this crate's files (in
source and compiled form) and to modifications of them, not to your own source files that merely
use this crate. Distributing a binary that includes this crate obliges you to tell recipients
that the crate is included and how to obtain its source; for an unmodified crate, pointing them
at the exact crates.io release is a reasonable way to do that. Example material (documentation
code examples, `examples/`, integration tests, and benchmarks) is additionally licensed under
[MIT-0](LICENSE-MIT-0), so it can be copied freely. See
[LICENSE-CLARIFICATION.md](LICENSE-CLARIFICATION.md) for the maintainers' full statement of
intent regarding normal Rust usage (linking, monomorphization, macro expansion), binary
distribution, and generated code, and [CONTRIBUTING.md](CONTRIBUTING.md) for the contribution
policy.
