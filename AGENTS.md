# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.
See `README.md` for project overview, features, usage, and MSRV.

## Lint Configuration

The crate uses strict lints configured in `Cargo.toml`. Key points:

- `unsafe_code = "deny"`, unsafe is forbidden except with `#[expect(unsafe_code, reason = "...")]`
- `clippy::allow_attributes_without_reason = "warn"`, always use `#[expect(lint, reason = "...")]`
  instead of `#[allow(lint)]`
- Arithmetic, indexing, unwrap, and expect are all warned, use checked/saturating operations and
  document safety with `#[expect(..., reason = "...")]`

## Architecture

Single-crate library with three private modules, all re-exported at the crate root:

- **`span`**, `Span`: absolute byte spans in the logical input stream, mapped onto retained
  segments through each segment's base offset.
- **`lease`**, `Lease` / `LeaseBytes` and the crate-internal `Slot`: the consumer-side promotable
  handle. A `Slot` holds `owned: OnceLock<Box<[u8]>>`, `source: Mutex<Option<(Arc<B>, Range)>>`,
  and a shared `Arc<AtomicUsize>` live-byte counter. The pin on a segment is the `Arc<B>` in
  `source`; whoever takes it out of the mutex (promotion or `Slot::drop`) decrements the counter
  exactly once. `owned` is only initialized under the source lock, so a live slot always has a
  source or an owned copy.
- **`store`**, `Store` / `SegmentId`: the owner side. Each segment keeps `Option<Arc<B>>` bytes, a
  `Vec<Weak<Slot<B>>>` lease registry, the shared live-byte counter, and a `sealed` flag.
  `retire` promotes surviving leases and drops the bytes; `maintain` retires sealed segments by
  the wasted-bytes threshold using the O(1) counters; retired segments stay in the vector so
  `SegmentId`s remain stable.

Key invariants: promotion is idempotent and race-safe because all owned-copy writes happen under
the source mutex; leases outlive the store safely because loaned slots share ownership of the
segment bytes. Tests live in sibling `tests.rs` files per module.
