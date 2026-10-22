//! How much memory does a store keep, and when is it actually released?
//! A counting global allocator records the bytes and blocks still allocated at each checkpoint,
//! relative to the start of each scenario. It counts requested sizes, not allocator overhead or
//! process RSS. `retained_len()` reports only parked segment bytes, so it cannot show bookkeeping,
//! lease handles, or a buffer kept alive by an outstanding read guard; these scenarios separate
//! them. Each checkpoint's expectation is asserted before the table prints. The counts are
//! deterministic for a given compiler and target, so nothing is timed and Criterion is not used.
//! Run `cargo bench --bench memory`.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    reason = "Bounded scenario sizes; a failed check must stop the harness"
)]
#![expect(clippy::print_stdout, reason = "The harness prints its measurements")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use strata_lease::{SegmentId, Span, Store};

/// Bytes currently allocated through the global allocator.
static BYTES: AtomicUsize = AtomicUsize::new(0);
/// Blocks currently allocated through the global allocator.
static BLOCKS: AtomicUsize = AtomicUsize::new(0);

/// Forwards to the system allocator, counting what is outstanding.
struct Counting;

// SAFETY: every method passes its arguments unchanged to `System`, which upholds the
// `GlobalAlloc` contract; the counters only change after successful calls.
#[expect(
    unsafe_code,
    reason = "Allocation counting for this harness; the library itself has no unsafe code"
)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc` is passed on unchanged
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            BLOCKS.fetch_add(1, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract for `dealloc` is passed on unchanged
        unsafe { System.dealloc(pointer, layout) };
        BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        BLOCKS.fetch_sub(1, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller's contract for `realloc` is passed on unchanged
        let resized = unsafe { System.realloc(pointer, layout, size) };
        if !resized.is_null() {
            BYTES.fetch_add(size, Ordering::Relaxed);
            BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        resized
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Outstanding bytes and blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Usage {
    bytes: usize,
    blocks: usize,
}

/// Nothing outstanding.
const NONE: Usage = Usage {
    bytes: 0,
    blocks: 0,
};

/// One measured point of a scenario.
struct Row {
    scenario: &'static str,
    checkpoint: &'static str,
    usage: Usage,
}

/// Collects checkpoints into preallocated rows, so recording one never allocates.
struct Report {
    rows: Vec<Row>,
    scenario: &'static str,
    base: Usage,
}

impl Report {
    fn now() -> Usage {
        Usage {
            bytes: BYTES.load(Ordering::Relaxed),
            blocks: BLOCKS.load(Ordering::Relaxed),
        }
    }

    /// Starts a scenario: its checkpoints count what was allocated since.
    fn start(&mut self, scenario: &'static str) {
        self.scenario = scenario;
        self.base = Self::now();
    }

    /// Records and returns what the running scenario holds now.
    fn checkpoint(&mut self, checkpoint: &'static str) -> Usage {
        let now = Self::now();
        let usage = Usage {
            bytes: now.bytes - self.base.bytes,
            blocks: now.blocks - self.base.blocks,
        };
        assert!(self.rows.len() < self.rows.capacity());
        self.rows.push(Row {
            scenario: self.scenario,
            checkpoint,
            usage,
        });
        usage
    }
}

/// A buffer that counts its own destruction, showing when a segment is actually freed.
struct Tracked {
    bytes: Vec<u8>,
    drops: Arc<AtomicUsize>,
}

impl AsRef<[u8]> for Tracked {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// Number of segments or leases in a burst.
const BURST: usize = 100_000;

fn span(start: usize, end: usize) -> Span {
    Span::new(u64::try_from(start).unwrap(), u64::try_from(end).unwrap()).unwrap()
}

/// Parks a burst of one-byte static segments, recording their identifiers into `ids`.
///
/// No payload is allocated, so everything the store allocates is bookkeeping. `ids` must have
/// room for the burst, so recording identifiers does not allocate either.
fn static_burst(ids: &mut Vec<SegmentId>) -> Store<&'static [u8]> {
    ids.clear();
    let mut store = Store::new();
    for base in 0..u64::try_from(BURST).unwrap() {
        ids.push(store.park(base, b"x".as_slice()).unwrap());
    }
    store
}

/// A burst of segments retires completely, the newest survives, or the oldest survives.
fn segment_burst(report: &mut Report) {
    let mut ids = Vec::with_capacity(BURST);

    report.start("segment burst, all retired");
    let mut store = static_burst(&mut ids);
    let parked = report.checkpoint("parked");
    assert_eq!(store.retire_all(), BURST);
    assert_eq!(store.retained_len(), 0);

    // Compaction removed every tombstone but kept the deque's capacity for reuse
    let retired = report.checkpoint("retired and compacted");
    assert_eq!(retired.blocks, 1);
    assert!(retired.bytes < parked.bytes);
    store.shrink_to_fit();
    assert_eq!(report.checkpoint("shrink_to_fit"), NONE);
    assert_eq!(store.segment_count(), u64::try_from(BURST).unwrap());
    drop(store);
    assert_eq!(report.checkpoint("store dropped"), NONE);

    // Retiring everything older than the newest compacts all of it away
    report.start("segment burst, newest kept");
    let mut store = static_burst(&mut ids);
    for &id in ids.iter().take(BURST - 1) {
        assert!(store.retire(id));
    }
    let retired = report.checkpoint("others retired and compacted");
    assert_eq!(retired.blocks, 2);
    store.shrink_to_fit();
    let shrunk = report.checkpoint("shrink_to_fit");
    assert_eq!(shrunk.blocks, 2);
    assert!(shrunk.bytes < 1024);
    drop(store);

    // Behind a surviving oldest segment the tombstones are identifier history and must stay
    report.start("segment burst, oldest kept");
    let mut store = static_burst(&mut ids);
    for &id in ids.iter().skip(1) {
        assert!(store.retire(id));
    }
    assert_eq!(report.checkpoint("others retired").blocks, 2);
    store.shrink_to_fit();
    let shrunk = report.checkpoint("shrink_to_fit");
    assert_eq!(shrunk.blocks, 2);
    assert!(shrunk.bytes >= BURST * size_of::<Span>());
    drop(store);
}

/// A burst of leases on one segment ends; its registry entries linger until pruned and shrunk.
fn lease_burst(report: &mut Report) {
    let mut leases = Vec::with_capacity(BURST);

    report.start("lease burst");
    let mut store = Store::new();
    store.park(0, vec![0; 64 * 1024]).unwrap();
    let parked = report.checkpoint("64 KiB parked");
    leases.extend((0..BURST).map(|index| {
        let start = index % 1024;
        store.lease(span(start, start + 16)).unwrap()
    }));
    report.checkpoint("leases alive");

    // A dead registration still holds its lease's small shared allocation until it is pruned
    leases.clear();
    let dropped = report.checkpoint("leases dropped");
    assert_eq!(dropped.blocks, parked.blocks + BURST + 1);

    // Pruning frees those allocations and keeps the registry's capacity
    assert_eq!(store.maintain(usize::MAX), 0);
    let pruned = report.checkpoint("maintain pruned");
    assert_eq!(pruned.blocks, parked.blocks + 1);
    store.shrink_to_fit();
    let shrunk = report.checkpoint("shrink_to_fit");
    assert_eq!(shrunk.blocks, parked.blocks);
    assert!(shrunk.bytes <= parked.bytes);
    drop(store);
}

/// A read guard outlives retirement and keeps the whole buffer alive until it is dropped.
fn guard_across_retirement(report: &mut Report) {
    let drops = Arc::new(AtomicUsize::new(0));
    let buffer = || Tracked {
        bytes: vec![0; 1024 * 1024],
        drops: Arc::clone(&drops),
    };

    report.start("guard across retirement");
    let mut store = Store::new();
    let id = store.park(0, buffer()).unwrap();
    let lease = store.lease(span(0, 16)).unwrap();
    report.checkpoint("1 MiB parked, 16 B leased");
    let guard = lease.bytes();
    assert!(store.retire(id));
    assert!(lease.is_owned());
    assert_eq!(store.retained_len(), 0);

    // The store let go and the lease owns its copy, but the guard still pins the buffer
    let retired = report.checkpoint("retired, guard alive");
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert!(retired.bytes > 1024 * 1024);
    drop(guard);
    let released = report.checkpoint("guard dropped");
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(released.bytes < 1024);
    drop((store, lease));

    // Without a guard, retirement frees the buffer at once
    report.start("retirement without a guard");
    let mut store = Store::new();
    let id = store.park(0, buffer()).unwrap();
    let lease = store.lease(span(0, 16)).unwrap();
    assert!(store.retire(id));
    let retired = report.checkpoint("retired");
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    assert!(retired.bytes < 1024);
    drop((store, lease));
}

/// A producer parks a segment per round; leases outlive their round, then maintenance retires it.
///
/// Each round parks 4 KiB open, leases a 3 KiB record kept for 8 rounds and a 16 B straggler kept
/// for 64, then seals and sweeps with a 2 KiB waste threshold. A segment is worth keeping while
/// its record lives and retires the round it dies, promoting its straggler.
fn streaming(report: &mut Report) {
    let mut records = VecDeque::with_capacity(9);
    let mut stragglers = VecDeque::with_capacity(65);

    report.start("streaming");
    let mut store = Store::new();
    let mut early = None;
    for round in 0..10_000 {
        let base = round * 4096;
        let id = store
            .park_open(u64::try_from(base).unwrap(), vec![0; 4096])
            .unwrap();
        records.push_back(store.lease(span(base, base + 3072)).unwrap());
        stragglers.push_back(store.lease(span(base + 3072, base + 3088)).unwrap());
        if records.len() > 8 {
            records.pop_front();
        }
        if stragglers.len() > 64 {
            stragglers.pop_front();
        }
        assert!(store.seal(id));
        store.maintain(2048);

        // The same state recurs every round once the rings are full
        if round == 999 {
            early = Some(report.checkpoint("after 1,000 rounds"));
        }
    }
    let steady = report.checkpoint("after 10,000 rounds");
    assert_eq!(Some(steady), early);
    assert_eq!(store.retained_len(), 8 * 4096);
    // The rings were allocated before the scenario started, so empty them without freeing
    records.clear();
    stragglers.clear();
    store.retire_all();
    drop(store);
    assert_eq!(report.checkpoint("everything dropped"), NONE);
}

fn main() {
    let mut report = Report {
        rows: Vec::with_capacity(64),
        scenario: "",
        base: Report::now(),
    };

    segment_burst(&mut report);
    lease_burst(&mut report);
    guard_across_retirement(&mut report);
    streaming(&mut report);

    println!("| Scenario | Checkpoint | Bytes | Blocks |");
    println!("| --- | --- | ---: | ---: |");
    for row in &report.rows {
        println!(
            "| {} | {} | {} | {} |",
            row.scenario, row.checkpoint, row.usage.bytes, row.usage.blocks
        );
    }
}
