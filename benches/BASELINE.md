# Lease benchmark results

These measurements answer four groups of questions: what a lease costs compared with copying,
whether lease creation stays cheap next to long-lived leases, what reclamation costs, and which
memory stays allocated until when. The timed comparisons use two complete runs of the same code
on one machine; the churn section adds one run of the library before its registry-pruning fix.

**How to read the tables:** times are point estimates from run A, per iteration as stated in
each section, unless a column names another run. Ratio columns show **A / B**, the same
comparison repeated in run B. A time ratio above 1 means the numerator took longer. Ratios use
unrounded estimates. [results.csv](results.csv) contains all 99 estimates and their 95%
confidence intervals in nanoseconds; the displayed A/B ratios are repeat observations, not
confidence intervals.

## 1. Lease lifecycle and reads

**Question A:** what does keeping a range through a lease cost, compared with copying it? Each
iteration creates the view, reads it once, and drops it. Only `lease` and `copy` keep the same
bytes with the same independence from the buffer; `arc` and `borrow` are overhead controls.

| Range | Lease | Copy | Arc control | Borrow control | Lease / copy (A / B) |
| --- | ---: | ---: | ---: | ---: | ---: |
| 16 B | 63.7 ns | 9.0 ns | 8.2 ns | 5.6 ns | 7.05× / 7.08× |
| 4 KiB | 63.3 ns | 48.3 ns | 8.3 ns | 5.6 ns | 1.31× / 1.26× |

**Conclusion:** at both sizes, a lease costs more time to create, read, and drop than copying
the range does: about seven times as much for 16 B and about 30% more for 4 KiB. The lease
cost does not depend on the range size, while the copy grows with it, so the lines would cross
above 4 KiB, but these runs do not locate that point. A lease's advantage is what it lets the
owner do later (keep zero-copy views it can still revoke), not a faster short-lived read. For
ranges this small that are kept only briefly, copying is the cheaper recipe in time.

**Question B:** what does reading an existing lease cost? Each iteration acquires one
`LeaseBytes` guard and drops it.

| Range | Loaned | Owned | Loaned / owned (A / B) |
| --- | ---: | ---: | ---: |
| 16 B | 17.5 ns | 9.7 ns | 1.80× / 1.78× |
| 4 KiB | 17.6 ns | 9.6 ns | 1.83× / 1.84× |

Cloning and dropping an existing lease took 8.2 ns (run B: 8.4 ns).

**Conclusion:** a loaned read takes the lease's mutex to snapshot its pin, so it costs about
1.8 times an owned read, which only clones the promoted `Arc`. Neither depends on range size.

## 2. Lease churn next to long-lived leases

**Question:** does creating a short-lived lease stay cheap when many other leases on the
segment stay alive? Each iteration creates one lease next to `kept` persistent ones and either
drops it, or promotes it and keeps it until the next iteration. "Before" is the library source
from `76adcf2`, before the registry-pruning fix, measured with the same workloads.

| Persistent leases | Drop, before | Drop, A | Drop, B | Promote, before | Promote, A | Promote, B |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 47.4 ns | 46.9 ns | 47.6 ns | 86.7 ns | 85.8 ns | 88.6 ns |
| 1,023 | 8.39 µs | 60.0 ns | 66.3 ns | 8.44 µs | 98.6 ns | 107.2 ns |
| 1,024 | 59.3 ns | 59.6 ns | 66.2 ns | 99.4 ns | 100.0 ns | 107.7 ns |
| 1,025 | 59.7 ns | 53.9 ns | 58.2 ns | 98.9 ns | 94.4 ns | 98.2 ns |
| 16,383 | 133.57 µs | 61.0 ns | 64.1 ns | 133.95 µs | 104.0 ns | 106.9 ns |
| 16,384 | 60.9 ns | 61.2 ns | 65.3 ns | 99.8 ns | 100.1 ns | 103.0 ns |
| 16,385 | 60.2 ns | 55.1 ns | 58.4 ns | 99.9 ns | 95.9 ns | 102.5 ns |

With 1023 persistent leases, creating `n` short-lived leases before dropping them together
cost, per lease:

| Batch | Before | A | B |
| ---: | ---: | ---: | ---: |
| 1 | 8.32 µs | 62.0 ns | 62.3 ns |
| 16 | 60.4 ns | 60.1 ns | 60.1 ns |
| 256 | 59.9 ns | 60.3 ns | 59.8 ns |

**Conclusion:** before the fix, one persistent lease fewer than a capacity boundary made each
new lease rescan the whole registry: about 140 times slower at 1,023 and about 2,200 times
slower at 16,383 than at the next count up. After the fix, every count from 1,023 to 16,385
costs 54–67 ns to drop and 94–108 ns to promote, within the variation between runs; only the
empty registry is cheaper. The batches show why the cliff needed one lease at a time: a batch
of 16 already leaves enough dead registrations for each prune to pay for itself.

## 3. Promotion, retirement, and maintenance

**Question A:** what does a consumer pay to promote one lease? The operation copies the range
into its own allocation and releases the pin.

| Range | A | B |
| --- | ---: | ---: |
| 16 B | 51.6 ns | 50.7 ns |
| 4 KiB | 184.2 ns | 185.5 ns |

**Question B:** what does retiring one 64 KiB segment cost, by the leases still on it? The
time includes promoting every lease and freeing the segment buffer.

| Leases on the segment | A | B |
| --- | ---: | ---: |
| None | 40.1 ns | 41.3 ns |
| 16 × 16 B | 838.3 ns | 840.4 ns |
| 256 × 16 B | 12.45 µs | 12.54 µs |
| 16 × 2 KiB (half the segment) | 2.01 µs | 2.19 µs |
| One 16 B lease and 255 clones | 94.6 ns | 94.0 ns |
| 256 independent leases over the same 16 B | 12.49 µs | 12.42 µs |

**Conclusion:** retirement cost is dominated by the per-lease promotions, at roughly 49 ns per
small lease, close to a consumer promoting it. Retiring a segment nothing leases is cheap. The
two 256-handle cases differ by more than 100 times because clones share one registration and
one copy, while independent leases over the same bytes each register and copy: overlapping
ranges multiply the work of retirement, as they multiply the live-byte accounting.

**Question C:** what does a maintenance sweep cost over 64 sealed 4 KiB segments, by how many
of them its threshold retires?

| Retired | A | B |
| --- | ---: | ---: |
| None | 712.8 ns | 720.5 ns |
| The 32 lightly pinned segments | 4.45 µs | 4.53 µs |
| All 64 | 12.76 µs | 16.78 µs |

**Conclusion:** a sweep that retires nothing costs about 11 ns per segment for the threshold
check and registry pruning. Each retirement adds roughly 115–250 ns for its promotion, the
freed buffer, and compaction. Retiring all 64 segments varied by about 30% between runs, more
than any other case; treat its absolute value with caution.

## 4. Memory

`cargo bench --bench memory` counts the bytes and blocks still allocated at each checkpoint,
relative to the start of each scenario. These counts are identical in debug and release builds.

| Scenario | Checkpoint | Bytes | Blocks |
| --- | --- | ---: | ---: |
| segment burst, all retired | parked | 11340032 | 100001 |
| segment burst, all retired | retired and compacted | 7340032 | 1 |
| segment burst, all retired | shrink_to_fit | 0 | 0 |
| segment burst, all retired | store dropped | 0 | 0 |
| segment burst, newest kept | others retired and compacted | 7340072 | 2 |
| segment burst, newest kept | shrink_to_fit | 96 | 2 |
| segment burst, oldest kept | others retired | 7340072 | 2 |
| segment burst, oldest kept | shrink_to_fit | 5600040 | 2 |
| lease burst | 64 KiB parked | 65808 | 3 |
| lease burst | leases alive | 8314384 | 100004 |
| lease burst | leases dropped | 8314384 | 100004 |
| lease burst | maintain pruned | 1114384 | 4 |
| lease burst | shrink_to_fit | 65640 | 3 |
| guard across retirement | 1 MiB parked, 16 B leased | 1048960 | 5 |
| guard across retirement | retired, guard alive | 1048960 | 5 |
| guard across retirement | guard dropped | 328 | 3 |
| retirement without a guard | retired | 328 | 3 |
| streaming | after 1,000 rounds | 41280 | 153 |
| streaming | after 10,000 rounds | 41280 | 153 |
| streaming | everything dropped | 0 | 0 |

**Conclusions:**

- **Bookkeeping follows the peak until shrunk.** After 100,000 segments retire, compaction
  leaves the 7 MiB segment deque empty but allocated; `shrink_to_fit` returns it. When only the
  newest segment survives, it shrinks to one entry. When the oldest survives, the 100,000
  tombstones behind it are identifier history, so shrinking only trims spare capacity.
- **Dropped leases are not freed until pruned.** Dropping 100,000 leases released their pins
  but no memory: each registry entry is a weak reference, which keeps its lease's 72-byte shared
  state allocated. A prune (here `maintain`) frees those, leaving the registry's capacity, which
  `shrink_to_fit` then releases.
- **A guard outlives retirement.** With a read guard held, retirement promoted the lease and the
  store let go, yet the 1 MiB buffer stayed allocated, and its drop counter confirms it was
  destroyed only when the guard dropped. Without a guard, retirement freed it at once.
- **Streaming stays bounded.** The park/lease/seal/maintain loop held the same 41,280 bytes in
  153 blocks after 1,000 and after 10,000 rounds, and nothing after everything was dropped.

## Reproduce these observations

Measured code: the library with the registry-pruning fix and `Store::shrink_to_fit`, as on this
branch, and the workloads in `lease.rs`, `lease/`, and `memory.rs`. The "before" churn
measurements use the library source from `76adcf2` with the same workloads. Documentation-only
changes do not affect these numbers. Re-measure when the library code or a workload changes.

- Date: 2026-10-22. CPU: Intel Core i9-11900K, advertised 3.50 GHz.
- OS/target: Linux `7.2.9-zen1-1-zen`, `x86_64-unknown-linux-gnu`.
- Compiler: `rustc 1.97.1 (8bab26f4f 2026-07-14)`, LLVM 22.1.6; standard optimized bench
  profile.
- Criterion 0.8.2, dependency resolution in the tracked `Cargo.lock`.
- CPU 0 governor reported `powersave`. No affinity or frequency controls were applied.
  Background load was not controlled; other builds and checks were kept outside the timed runs.

```bash
cargo bench --bench lease -- --warm-up-time 1 --measurement-time 2 \
  --sample-size 30 --save-baseline run-a
cargo bench --bench lease -- --warm-up-time 1 --measurement-time 2 \
  --sample-size 30 --save-baseline run-b
cargo bench --bench memory
```

The "before" run used the churn filter on the older library source:

```bash
cargo bench --bench lease -- churn --warm-up-time 1 --measurement-time 2 \
  --sample-size 30 --save-baseline before-fix
```

All 41 timed workloads passed their output checks in both runs, and the 17 churn workloads in
the "before" run. CSV estimates use Criterion's slope when available and its mean otherwise,
with the per-run 95% intervals. Those intervals do not capture all cross-run variation; the
full-retirement sweep shows why repeated runs matter. Read the [methodology](README.md) for
precise timing and ownership boundaries. These are warm, single-threaded, in-memory workloads,
not concurrent promotion or retirement, and the memory counts are requested allocation sizes,
not RSS.
