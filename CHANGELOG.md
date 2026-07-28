# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Initial implementation of `strata-lease`, revocable zero-copy sharing of retained
byte buffers through promotable leases.

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
- `Span` for absolute byte addressing and `SegmentId` for parked segments.
- Project metadata following the `strata-reader` conventions: strict lint
  configuration, Mozilla Public License 2.0, and a Minimum Supported Rust
  Version of **1.87.0** on Rust edition 2024.
