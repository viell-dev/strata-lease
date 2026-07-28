# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Initial implementation of `strata-lease`, revocable zero-copy sharing of retained
byte buffers through promotable leases, hardened by an adversarial pre-release
audit.

### Added

- `Store<B>`, the owner side: parks byte segments with absolute base offsets
  (`park`, `park_open`, `seal`), resolves absolute `Span`s to zero-copy bytes
  (`bytes`) or handle-free copies across segment boundaries (`copy_bytes`), and
  hands out promotable leases (`lease`, plus `leases` for spans crossing
  segment boundaries).
- `Lease<B>`, the consumer side: a cheap, cloneable, thread-safe handle that is
  either loaned (reads from the retained segment it pins) or owned (holds its
  own copy). `bytes()` reads uniformly in both states through the `LeaseBytes`
  guard; `promote()` takes the copy early and releases the pin without store
  access. Clones share one state and one pin, and dropped leases release their
  pin automatically.
- Owner-side reclamation: `retire(id)` and `retire_all()` promote every
  surviving lease in place and free the segment unconditionally;
  `maintain(min_wasted_bytes)` retires sealed segments once their wasted
  (retained minus live loaned) bytes reach the threshold, using O(1) shared
  live-byte counters instead of walking the registry.
- `Span` for absolute byte addressing (with `Range<u64>` conversions and
  ordering) and `SegmentId` for parked segments.
- Owner-side inspection: `is_sealed(id)` reports maintenance eligibility and
  `segment_span(id)` the absolute span a retained segment covers.
- Project metadata following the `strata-reader` conventions: strict lint
  configuration (including `missing_docs`), Mozilla Public License 2.0, and a
  Minimum Supported Rust Version of **1.87.0** on Rust edition 2024.

### Changed

- `LeaseBytes` is a snapshot guard: it takes its own pin on the segment
  allocation instead of holding the lease's internal lock, so guards nest
  freely, never deadlock, stay valid across promotion and retirement, and
  retirement never blocks on active readers.
- Empty spans (including reversed ones) resolve successfully against any store
  through every accessor and yield empty bytes; an empty lease starts owned and
  pins nothing.
- Retired segments compact away once every segment parked before them is
  retired too, so a streaming park-and-retire loop runs in bounded memory;
  `SegmentId`s stay valid and report the settled retired state.
- Lease registries prune dropped and promoted registrations amortized on
  insert, keeping them proportional to the live loaned leases even when
  `maintain` is never called.
- Live-byte counters saturate instead of wrapping, keeping pathological
  aggregate lease lengths from corrupting maintenance decisions.
- `Debug` for `Store` no longer requires `B: Debug` and reports per-segment
  bookkeeping (base, retired, sealed, registered leases, live bytes).

### Fixed

- Promotion copies the lease bytes before releasing its pin, so a panicking
  buffer `as_ref` implementation leaves the lease loaned, readable, and
  correctly accounted instead of silently empty with a leaked pin.
- `retire` promotes surviving leases before releasing the registry and the
  retained bytes, so a panicking promotion leaves the segment intact and the
  call retryable.
- `Cargo.lock` is tracked instead of ignored, matching its inclusion in the
  published package.
- The published package includes the test suite, so the crate can be audited
  and tested from the packaged sources.
