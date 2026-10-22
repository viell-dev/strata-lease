# Benchmarks

[Results and conclusions](BASELINE.md) group measurements by the decision being compared.
The source modules below contain the inputs, correctness checks, and timing loops.

```bash
cargo bench --bench lease -- --test   # Check every timed workload, without timing
cargo bench --bench lease             # Measure every timed workload
cargo bench --bench lease -- churn    # Measure one group (here churn and churn_batch)
cargo bench --bench memory            # Count outstanding allocations at each checkpoint
```

Source bytes are pseudo-random rather than a repeating pattern, so a lease reading a shifted or
misplaced range fails the content checks that run before timing.

## Lifecycle and reads: what does a lease cost compared with a copy?

[Source](lease/access.rs). One 64 KiB segment is parked once. Ranges are 16 B or 4 KiB and start
1000 bytes into the segment. The store and its lease registry persist across iterations, so
amortized registry pruning is part of `lease`.

| Variant | One iteration | What the reader is left holding |
| --- | --- | --- |
| `lifecycle/lease` | Create a loaned lease, read it through one guard, drop both | A revocable view: the owner may still retire the segment |
| `lifecycle/copy` | Copy the range out of `Store::bytes` into a `Vec`, read it, drop it | Its own copy of just the range |
| `lifecycle/arc` | Clone an `Arc<[u8]>` of the whole buffer, slice the range, drop the clone | A share that keeps the whole buffer alive and cannot be revoked |
| `lifecycle/borrow` | Slice the range through `Store::bytes` | A borrow that pins the store itself |
| `read/loaned` | Acquire and drop one guard on an existing loaned lease | |
| `read/owned` | Acquire and drop one guard on an existing promoted lease | |
| `clone` | Clone and drop an existing lease | |

`lease` and `copy` keep the same bytes for the same lifetime, so they are the direct comparison.
`arc` and `borrow` are overhead controls, not alternatives: neither lets the owner reclaim the
buffer while the view exists. Compare the read variants with each other, not with the lifecycle
cases, which include creation and destruction.

## Churn: does creating a lease stay cheap next to long-lived ones?

[Source](lease/churn.rs). One segment holds `kept` single-byte leases for the whole case; they
stay loaned, so pruning can never free their registrations. Each iteration creates one more
lease on the spare byte after them.

| Variant | One iteration | Its registration when the next prune runs |
| --- | --- | --- |
| `churn/drop/<kept>` | Create a lease and drop it | Dead |
| `churn/promote/<kept>` | Create a lease, promote it, and keep it until the next iteration | Promoted, still alive |
| `churn_batch/<n>` | With 1023 kept, create `n` leases, then drop all of them | Dead |

The `kept` counts are 0 and the values on either side of registry capacity boundaries
(2^k - 1, 2^k, 2^k + 1). Just below a boundary, a registry whose prune frees only the single
churned registration would rescan every persistent lease for each new one; that is the
regression these cases preserve. Compare times across `kept`: the per-lease cost should not
depend on how many other leases are alive. `churn_batch` reports elements per second, so its
cases compare per lease.

Fixture setup is outside timing. Registry state carries over between iterations, so each
estimate is a steady-state average that includes amortized pruning, the lease's allocation, and
its destruction.

## Reclamation: what do promotion, retirement, and maintenance cost?

[Source](lease/reclaim.rs). Each iteration consumes a fresh fixture: promoting or retiring the
same one twice would only time the no-op. Fixtures are built outside timing in batches of 16
(`iter_batched_ref`), and their final destruction is excluded. Each operation's own copies,
allocations, and registry walk are timed, as is freeing the segment buffer when the operation
drops the last pin on it.

| Group | Fixture | Timed operation |
| --- | --- | --- |
| `promote/<len>` | One loaned lease of 16 B or 4 KiB in a 64 KiB segment | `Lease::promote` |
| `retire/none`, `16x16B`, `256x16B`, `16x2KiB` | A 64 KiB segment with that many independent leases of that size; `16x2KiB` keeps half the segment live | `Store::retire` |
| `retire/clones` | One 16 B lease plus 255 clones: one registration, one copy | `Store::retire` |
| `retire/overlapping` | 256 independent leases over the same 16 B: 256 registrations and copies | `Store::retire` |
| `maintain/none`, `half`, `all` | 64 sealed 4 KiB segments, alternately pinned by a 3 KiB and a 256 B lease | `Store::maintain` with a threshold retiring none, the lightly pinned half, or all |

Before timing, each fixture is checked once: retired leases are owned and hold their original
bytes, clones share one promoted copy while independent leases hold their own, and maintenance
retires exactly the expected segments.

## Memory: what stays allocated, and when is it released?

[Source](memory.rs). A counting global allocator records the bytes and blocks still allocated
at each checkpoint, relative to the start of the scenario. It counts requested sizes, not
allocator overhead or process RSS. Every checkpoint's expectation is asserted before the table
prints. Nothing is timed, so this harness does not use Criterion; its counts are deterministic
for a given compiler and target.

| Scenario | What it separates |
| --- | --- |
| Segment burst | 100,000 one-byte static segments, so every allocation is bookkeeping. Retire all, all but the newest, or all but the oldest, then `shrink_to_fit` |
| Lease burst | 100,000 leases on one 64 KiB segment, dropped, then pruned by `maintain`, then `shrink_to_fit` |
| Guard across retirement | A 1 MiB buffer that counts its own destruction stays alive behind a read guard after retirement, and is freed when the guard drops |
| Streaming | Park, lease, seal, and maintain per round for 10,000 rounds; usage after 1,000 and 10,000 rounds must match |

`retained_len()` cannot show any of this: it counts parked segment bytes only, not bookkeeping,
lease handles, or a buffer kept alive by an outstanding guard.

## Reproduce and interpret

The timed harness uses [Criterion 0.8.2](https://docs.rs/criterion/0.8.2/criterion/) as a
development-only dependency, with plotting and parallel analysis disabled. Exact output checks
run before timing; timed loops pass inputs and results through `std::hint::black_box`. All 41
timed cases run once in smoke mode under `cargo test --all-targets --all-features`, which also
runs the memory harness with its assertions. The tracked `Cargo.lock` records the dependency
resolution.

To compare a library change on the same machine:

```bash
cargo bench --bench lease -- --save-baseline before
# Change the library, keeping the workloads and configuration the same.
cargo bench --bench lease -- --baseline before
```

These are warm, in-memory, single-threaded workloads; they do not measure contention between
consumers promoting or reading while the owner retires. Small cases include harness overhead.
Record source revision, compiler, hardware, power settings, and competing load with results.
Two runs check repeatability on one host; they do not establish other machines' performance or
a CI threshold.
