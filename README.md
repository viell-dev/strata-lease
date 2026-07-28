# strata-lease

Revocable zero-copy sharing: promotable leases into retained byte buffers.

A data owner hands out cheap views into memory it retains instead of copies. Normally that pins the
memory: plain borrowing pins it for as long as any view exists, and reference counting (`Arc`,
`bytes::Bytes`) keeps the whole allocation alive until the last view dies, so one tiny straggler can
pin megabytes. This crate sits between the two. Sharing is reference counted, but every view is a
`Lease` that can be _promoted_ — switched in place from pointing into the retained buffer to holding
its own small copy. That gives the owner an escape hatch neither model allows: promote the
stragglers, then free the large allocation unconditionally.

The trade in one line: zero-copy in the common case, and a small copy only for leases that outlive
the buffer's usefulness — the owner regains the right to free its memory, which plain borrowing or
refcounting never grants.

## Features

- **Promotable leases**: `Lease` is a cheap, cloneable, thread-safe handle that is either _loaned_
  (reads from the retained segment it pins) or _owned_ (holds its own copy). `lease.bytes()` reads
  uniformly in both states; consumers never need to know which one they are in.
- **Owner-side reclamation**: `retire(id)` and `retire_all()` promote every surviving lease in
  place and release the segment unconditionally. `maintain(min_wasted_bytes)` retires segments
  once their wasted (retained minus live loaned) bytes reach a threshold.
- **O(1) retirement checks**: each lease's pin is tracked in a shared per-segment live-byte
  counter, decremented automatically when a lease is dropped or promoted, so `maintain` never has
  to walk the lease registry to decide.
- **Consumer-side promotion**: `lease.promote()` takes the copy early and releases the pin without
  involving the store — useful at an ownership boundary such as a resolver or deserializer.
- **Absolute spans**: segments are parked with a base offset, and `Span`s address bytes across the
  whole logical stream; `leases()` covers spans that straddle segment boundaries.
- **Safe by refcounting**: dropping the `Store` without retiring never dangles a lease — loaned
  leases share ownership of their segment. Retirement is a memory-management action, not a safety
  requirement.

## Usage

```rust
use strata_lease::{Span, Store};

// The owner parks a buffer it has finished producing (e.g. a consumed reader buffer).
let mut store = Store::new();
store.park(0, b"key=value\n".to_vec());

// Consumers hold cheap leases instead of copies.
let value = store.lease(Span::new(4, 9)).unwrap();
assert!(value.is_loaned());
assert_eq!(&*value.bytes(), b"value");

// Retaining ten bytes for a five-byte straggler is no longer worth it: retire.
// The lease takes its own copy in place and the store releases the buffer.
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
store.park(0, vec![b'x'; 8 * 1024]);
let straggler = store.lease(Span::new(10, 26)).unwrap();

// 8 KiB retained for 16 live bytes: wasted ≥ 4 KiB, so the segment retires.
assert_eq!(store.maintain(4 * 1024), 1);
assert!(straggler.is_owned());
assert_eq!(store.retained_len(), 0);
```

`park_open` / `seal` cover streaming producers: an open segment is exempt from `maintain`, so a
sweep cannot retire a buffer that was parked ahead of attaching its leases.

## Buffer types

`Store<B>` and `Lease<B>` are generic over any `B: AsRef<[u8]>` and default to `Vec<u8>`, so the
crate has no dependencies and works with plain vectors, `Box<[u8]>`, memory-mapped buffers, or
reader-owned buffer types.

## Minimum Supported Rust Version

The MSRV is **1.87.0**.

## License

Licensed under the [Mozilla Public License Version 2.0](LICENSE).

MPL-2.0 is a file-level weak copyleft: modifications to MPL-licensed files must be released
under the MPL, but the license does not extend to separate files that merely use this crate.

For the avoidance of doubt regarding intent: normal Rust usage of this crate is not considered
modification of the Covered Software. This includes linking, generic instantiation
(monomorphization), inlining, and other artifacts produced by the Rust compiler from
downstream code. Only changes to the source files in this repository trigger MPL-2.0's source
disclosure obligations. This note describes intent only and does not modify the license text.
