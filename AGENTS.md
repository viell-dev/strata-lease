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

- **`span`**, `Span` and `Error` (re-exported as `SpanError`), plus the crate-internal
  `SegmentRange`: non-empty absolute byte spans in the logical input stream, mapped onto retained
  segments through each segment's base offset. `Span::new` enforces `start < end`, so empty and
  reversed spans are unrepresentable; the store never has to special-case them. `SegmentRange` is
  the segment-local counterpart (`Copy`, non-empty, forward `usize` slice bounds) and is only
  constructible through `Span::segment_range`, the containment check itself.
- **`lease`**, `Lease` / `LeaseBytes` and the crate-internal `Slot`: the consumer-side promotable
  handle. A `Slot` holds `owned: OnceLock<Box<[u8]>>`,
  `source: Mutex<Option<(Arc<B>, SegmentRange)>>`, and a shared `Arc<AtomicUsize>` live-byte
  counter (saturating, advisory). The pin on a segment is the `Arc<B>` in `source`; whoever takes
  it out of the mutex (promotion or `Slot::drop`) decrements the counter exactly once. Promotion copies under the lock *before* taking the
  source, so a panicking buffer `as_ref` leaves the lease loaned and retryable. `LeaseBytes` is a
  snapshot guard: it clones the `Arc<B>` and range out of the mutex instead of holding it, so
  reads never block promotion or retirement and guards nest freely.
- **`store`**, `Store` / `SegmentId` / `ParkError` / `ParkErrorKind`: the owner side. Segments
  live in a `VecDeque` behind a `compacted` offset as a `Segment` enum: `Retained` (with
  `Arc<B>` bytes, a `Vec<Weak<Slot<B>>>` lease registry pruned amortized on insert and by
  `maintain`, the shared live-byte counter, and a `sealed` flag) or a bare `Retired` tombstone,
  so retired segments cannot carry stale bookkeeping. `park` validates against the store's
  `high_water` offset — buffers must be non-empty, in ascending non-overlapping base order, and
  `u64`-addressable — and rejects violations with a `ParkError` carrying the buffer back.
  `retire` promotes surviving leases *before* swapping in the tombstone (panic-safe, retryable),
  then compacts leading retired tombstones; `maintain` retires sealed segments by the
  wasted-bytes threshold using the O(1) counters. `SegmentId`s are stable: compacted identifiers
  keep answering with their settled state (retired, zero live bytes).

Key invariants: promotion is idempotent and race-safe because all owned-copy writes happen under
the source mutex; leases outlive the store safely because loaned slots share ownership of the
segment bytes; compaction only ever removes retired segments, so it is invisible through the
public API; spans and parked segments are validated at construction, so lookups never see an
empty span, a reversed span, or a segment out of order. Tests live in sibling `tests.rs` files
per module, plus consumer-facing coverage in `tests/consumer.rs` and adversarial/model coverage
in `tests/hardening.rs`.
