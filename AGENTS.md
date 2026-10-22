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
- `missing_docs = "warn"`, every public item carries documentation, and public methods carry a
  `# Examples` doctest

## Architecture

Single-crate library with three private modules, all re-exported at the crate root:

- **`span`**, `Span` + `SpanError`: non-empty absolute byte spans in a logical stream. `Span::new`
  enforces `start < end`, so empty and reversed spans are unrepresentable. The crate-internal
  `SegmentRange` is the segment-local counterpart (`usize` slice bounds relative to a segment
  base) and is only constructible through `Span::segment_range`, the containment check itself.
- **`lease`**, `Lease<B>` + `LeaseBytes<B>`: the consumer-side promotable handle. The
  crate-internal `Backing<B>` bundles a segment's bytes with its live-byte counter in one
  allocation; `Slot<B>` is the state shared by a lease and its clones, holding an
  `OnceLock<Arc<[u8]>>` owned copy and a mutex-protected `Option<(Arc<Backing<B>>, SegmentRange)>`
  source. The `Arc<Backing<B>>` in the source is the pin; whoever takes it out of the mutex
  (promotion or `Slot::drop`) decrements the counter exactly once. Promotion copies under the lock
  *before* taking the source, so a panicking buffer `as_ref` leaves the lease loaned and
  retryable. `LeaseBytes` is a lifetime-free snapshot guard: it clones an `Arc` instead of
  borrowing or locking, so reads never block promotion or retirement.
- **`store`**, `Store<B>` + `SegmentId`, `SegmentStatus`, `ParkError`, `ParkErrorKind`: the owner
  side. Segments live in a `VecDeque` behind a `compacted` offset, each carrying its `Span` plus
  either a `Retained` state (shared `Arc<Backing<B>>`, `sealed` flag, `Vec<Weak<Slot<B>>>` lease
  registry pruned amortized on insert and by `maintain`) or a `Retired` tombstone. Lookups
  binary-search the deque by offset (`position_after`), which is why tombstones keep their span.
  `park` validates against `high_water` (non-empty, ascending non-overlapping bases,
  `u64`-addressable) and hands rejected buffers back in a `ParkError`. `retire` promotes
  surviving leases *before* swapping in the tombstone, then compacts leading tombstones;
  `maintain` retires sealed segments by the wasted-bytes threshold using the O(1) counters.
  Compaction and pruning keep emptied capacity for reuse; only `shrink_to_fit` releases it.

Key design: the `Store` never blocks on consumers and consumers never need the `Store` to
promote; both sides synchronize only through the per-lease `Slot`. Live-byte counters are
advisory (`Relaxed`, saturating, sticky at saturation) and only ever err toward retaining.

## Tests

Two kinds of tests, both Miri-clean:

- **Unit tests** in nested `tests.rs` submodules per module (`src/<module>/tests.rs`), in the
  `strata-reader` style: one white-box `test_<type>_<method>` per method walking its branches
  with commented assertions, section banners, and coverage notes for thin wrappers.
- **Integration tests** under `tests/`, public API only. Each test is one scenario named
  `test_<scenario>` and told top to bottom: a `/* */` block sets the situation, then each step
  carries a comment saying what the owner or consumer does and what it can rely on. They double
  as usage examples (and are MIT-0 example material), so favor guidance over terseness.
  `consumer.rs` holds the intended-use scenarios; `hardening.rs` holds the adversarial ones (the
  barrier-forced retirement race and the randomized reference-model lifecycle).

## Benchmarks

- `benches/lease.rs` is a Criterion harness with modules under `benches/lease/`, one per
  question; `benches/memory.rs` is a separate allocation-counting harness without Criterion. Its
  counting `GlobalAlloc` is the repository's only `unsafe` code, under `#[expect(unsafe_code)]`.
- `benches/README.md` is the methodology, `benches/BASELINE.md` the measured results, and
  `benches/results.csv` the raw estimates. Validate outputs before timing; every timed case and
  the memory harness also run in smoke mode under `cargo test --all-targets`.
- Benchmarks are example material and ship in the package: keep the `include` list current.
