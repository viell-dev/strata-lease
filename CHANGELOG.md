# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Initial implementation of `strata-lease`, revocable zero-copy sharing of retained
byte buffers through promotable leases.

### Added

#### Core types

- `Store<B>`, the owner side: parks byte segments with absolute base offsets,
  resolves absolute `Span`s against them, hands out promotable leases, and
  decides when a segment stops being worth retaining. `B` is any
  `AsRef<[u8]>` buffer and defaults to `Vec<u8>`.
- `Lease<B>`, the consumer side: a cheap, cloneable, thread-safe handle that
  is either loaned (reads from the retained segment it pins) or owned (holds
  its own copy). Clones share one state and one pin; dropping the last clone
  of a loaned lease releases its pin.
- `LeaseBytes<B>`, the zero-copy read guard returned by `Lease::bytes`. It is
  a self-contained snapshot holding its own `Arc` on the segment allocation or
  the promoted copy, so guards nest freely, never block promotion or
  retirement, and may outlive the lease and the store.
- `Span`, a non-empty absolute byte span with half-open `u64` bounds.
  `Span::new` and `TryFrom<Range<u64>>` reject empty and reversed bounds with
  a `SpanError`, so every span that exists covers at least one byte.
  Conversion to `Range<u64>` and lexicographic ordering included.
- `SegmentId`, a stable identifier assigned in park order, never reused, and
  valid for the lifetime of the issuing store, including after retirement and
  compaction. `SegmentStatus` reports a segment's forward-only lifecycle
  (`Open`, `Sealed`, `Retired`).

#### Owner-side operations

- `park` and `park_open`, validating up front that a buffer is non-empty, in
  ascending non-overlapping base order, and addressable within `u64`.
  Rejected buffers are handed back inside a `ParkError` carrying a
  `ParkErrorKind`. Gaps between segments are allowed.
- `seal`, marking an open segment as eligible for threshold maintenance.
- `bytes`, the zero-copy lookup within one segment, and `copy_bytes`, the
  handle-free copy following spans across segment boundaries.
- `lease`, creating one loaned lease within one segment, and `leases`,
  creating one lease per segment for spans crossing segment boundaries.
- `retire`, `retire_all`, and the threshold-driven `maintain`, each promoting
  every surviving lease in place before releasing a segment. `retire`
  promotes before releasing anything, so a panicking buffer leaves the call
  retryable. `maintain` skips open segments, uses the O(1) live-byte counters
  for its threshold check, and prunes registry entries for dropped and
  promoted leases.
- Inspection through `segment_count`, `retained_len`, `live_bytes`, `status`,
  and `segment_span`.
- Segment lookups binary-search the parked segments by offset.
- Retired segments leave a tombstone so identifiers stay stable; leading
  tombstones compact away automatically, so a streaming park-and-retire loop
  runs in bounded memory. Lease registries prune themselves amortized as
  leases are handed out.

#### Consumer-side operations

- `Lease::bytes`, `len`, `is_empty`, and `to_vec` for reading uniformly in
  both states.
- `Lease::promote`, taking the copy early and releasing the pin without store
  access. Promotion is idempotent, reaches every clone, and copies before
  releasing the pin, so a panicking buffer leaves the lease loaned and
  readable.
- `Lease::is_owned` and `is_loaned` as snapshots of the promotion state.

#### Accounting and safety

- Per-segment live-byte counters, bundled with the bytes in one allocation,
  count loaned ranges with multiplicity and steer `maintain`. They saturate
  instead of wrapping, and saturation is sticky, so the accounting error is
  always conservative.
- No `unsafe` code and no dependencies. Dropping the store without retiring
  never dangles a lease. Buffers that violate the stable-buffer contract
  degrade to empty reads rather than panics or memory unsafety.
- `Lease<B>` and `Store<B>` are `Send` and `Sync` exactly when `B` is.

#### Project metadata

- Crate-level documentation, README, and a doctest on every public method.
- Unit tests in nested `tests.rs` submodules per module, plus integration
  tests under `tests/` written as commented public-API scenarios: intended
  use in `consumer.rs`, and the thread race and randomized reference-model
  lifecycle in `hardening.rs`. The whole suite is Miri-clean.
- `Cargo.toml` metadata including `repository`, `keywords`, `categories`,
  and a curated `include` list.
- Strict lint configuration: `unsafe_code = "deny"`, `missing_docs`,
  warn-level `clippy::pedantic` and `clippy::cargo`, and warnings on
  arithmetic, indexing, `unwrap`, and `expect`.
- Mozilla Public License 2.0, with example material additionally under MIT-0,
  the maintainers' statement of intent in `LICENSE-CLARIFICATION.md`, and the
  contribution policy in `CONTRIBUTING.md`.
- Minimum Supported Rust Version of **1.87.0** on Rust edition 2024.

[Unreleased]: https://github.com/viell-dev/strata-lease/commits/main
