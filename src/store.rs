//! The owner side: retained segments, the lease registry, and retirement.
//!
//! A [`Store`] parks byte buffers as segments with absolute base offsets, resolves [`Span`]s
//! against them, hands out [`Lease`]s, and decides when a segment stops being worth retaining.
//!
//! # Example
//!
//! ```
//! use strata_lease::{Span, Store};
//!
//! let mut store = Store::new();
//!
//! // Park two consecutive chunks of a stream
//! let first = store.park(0, b"key=".to_vec()).unwrap();
//! let second = store.park(4, b"value\n".to_vec()).unwrap();
//!
//! // Spans resolve through the base offsets
//! assert_eq!(store.bytes(Span::new(4, 9).unwrap()), Some(b"value".as_slice()));
//!
//! // Leases pin their segment until dropped, promoted, or retired
//! let value = store.lease(Span::new(4, 9).unwrap()).unwrap();
//! assert_eq!(store.live_bytes(second), Some(5));
//!
//! // Retiring promotes the straggler and frees the segment
//! assert!(store.retire(second));
//! assert!(value.is_owned());
//! assert_eq!(&*value.bytes(), b"value");
//! assert_eq!(store.retained_len(), 4);
//! # let _ = first;
//! ```

use crate::lease::{Backing, Lease, Slot};
use crate::span::{SegmentRange, Span};
use std::collections::VecDeque;
use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

/// Identifier for a parked segment.
///
/// Identifiers are assigned in park order, are never reused, and stay valid for the lifetime of
/// the store that issued them, including after the segment is retired. The index is `u64` so
/// that no-reuse holds on every target: each parked segment covers at least one byte of the
/// `u64` address space, which bounds the number of parks a store can ever accept.
///
/// Identifiers carry no store identity: passing an identifier to a *different* store is a logic
/// error that safely resolves against whatever segment holds that position there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(u64);

impl SegmentId {
    /// Returns the numeric segment index.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::Store;
    /// let mut store = Store::new();
    /// let first = store.park(0, b"first".to_vec()).unwrap();
    /// let second = store.park(5, b"second".to_vec()).unwrap();
    ///
    /// assert_eq!(first.index(), 0);
    /// assert_eq!(second.index(), 1);
    /// ```
    #[inline]
    pub const fn index(self) -> u64 {
        self.0
    }
}

/// Lifecycle state of a parked segment, reported by [`Store::status`].
///
/// A segment only ever moves forward: `Open`, then `Sealed`, then `Retired`. [`Store::park`]
/// starts a segment at `Sealed`; [`Store::park_open`] starts it at `Open`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentStatus {
    /// Retaining bytes; exempt from [`Store::maintain`] until sealed.
    Open,
    /// Retaining bytes; eligible for retirement by [`Store::maintain`].
    Sealed,
    /// Bytes released; the settled, terminal state.
    Retired,
}

/// Why a buffer was rejected by [`Store::park`] or [`Store::park_open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkErrorKind {
    /// The buffer contains no bytes; an empty segment could never resolve any [`Span`].
    Empty,
    /// The base starts before the end of the previously parked segment, breaking the ascending,
    /// non-overlapping segment order.
    OutOfOrder {
        /// The rejected base offset.
        base: u64,
        /// The lowest base the store accepts next: the end of the last parked segment.
        min_base: u64,
    },
    /// `base + len` exceeds [`u64::MAX`], so the segment's tail bytes could never be addressed
    /// by a [`Span`].
    Unaddressable {
        /// The rejected base offset.
        base: u64,
        /// The buffer length in bytes, saturated at [`u64::MAX`].
        len: u64,
    },
}

impl Display for ParkErrorKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Empty => write!(f, "an empty buffer cannot be parked"),
            Self::OutOfOrder { base, min_base } => write!(
                f,
                "base {base} starts before the end {min_base} of the previously parked segment"
            ),
            Self::Unaddressable { base, len } => write!(
                f,
                "segment at base {base} with {len} bytes extends past the u64 address space"
            ),
        }
    }
}

/// An error returned when a buffer cannot be parked.
///
/// The rejected buffer rides along and is handed back through [`ParkError::into_bytes`], so a
/// failed park never costs the producer its data.
///
/// # Trait semantics
///
/// The `Display` output is that of the [`ParkErrorKind`]. The `Debug` output reports the kind
/// only; the buffer is not printed, so `B` needs no `Debug` implementation.
pub struct ParkError<B> {
    /// Why the buffer was rejected.
    kind: ParkErrorKind,
    /// The rejected buffer.
    bytes: B,
}

impl<B> ParkError<B> {
    /// Returns why the buffer was rejected.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{ParkErrorKind, Store};
    /// let mut store = Store::new();
    /// let error = store.park(0, Vec::new()).unwrap_err();
    ///
    /// assert_eq!(error.kind(), ParkErrorKind::Empty);
    /// ```
    #[inline]
    pub const fn kind(&self) -> ParkErrorKind {
        self.kind
    }

    /// Returns the rejected buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::Store;
    /// let mut store = Store::new();
    /// store.park(0, b"first".to_vec()).unwrap();
    ///
    /// // Overlapping the first segment is rejected, but the data is not lost
    /// let error = store.park(3, b"second".to_vec()).unwrap_err();
    /// assert_eq!(error.into_bytes(), b"second");
    /// ```
    #[inline]
    pub fn into_bytes(self) -> B {
        self.bytes
    }
}

impl<B> Debug for ParkError<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParkError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl<B> Display for ParkError<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.kind, f)
    }
}

impl<B> Error for ParkError<B> {}

/// A parked segment: the span it covers plus its retained state or a retired tombstone.
///
/// The span outlives retirement so that the segment deque stays searchable by offset. Retirement
/// swaps the whole retained state for the tombstone at once, so a retired segment cannot carry a
/// stale seal flag, lease registry, or live-byte counter.
struct Segment<B> {
    /// The absolute span the segment covers, validated at parking.
    span: Span,
    /// The retained bytes and bookkeeping, or the retired tombstone.
    state: State<B>,
}

/// The lifecycle state of a [`Segment`].
enum State<B> {
    /// The segment retains its bytes.
    Retained(Retained<B>),
    /// The segment has released its bytes.
    Retired,
}

/// The retained state of a [`Segment`]: its bytes plus lease bookkeeping.
struct Retained<B> {
    /// The shared allocation holding the bytes and the live-byte counter.
    backing: Arc<Backing<B>>,
    /// Whether [`Store::maintain`] may retire the segment.
    sealed: bool,
    /// Registrations of leases handed out on the segment, pruned lazily.
    leases: Vec<Weak<Slot<B>>>,
}

impl<B> Segment<B> {
    /// Returns the retained state, or `None` for a retired segment.
    #[inline]
    fn retained(&self) -> Option<&Retained<B>> {
        match &self.state {
            State::Retained(retained) => Some(retained),
            State::Retired => None,
        }
    }

    /// Mutable counterpart of [`Self::retained`].
    #[inline]
    fn retained_mut(&mut self) -> Option<&mut Retained<B>> {
        match &mut self.state {
            State::Retained(retained) => Some(retained),
            State::Retired => None,
        }
    }

    /// Returns whether the segment has been retired.
    #[inline]
    fn is_retired(&self) -> bool {
        matches!(self.state, State::Retired)
    }

    /// Segment length in bytes, from the span validated at parking.
    ///
    /// The length originated as a buffer's `usize` length at parking, so the narrowing cannot
    /// actually fail; the saturating fallback only satisfies the lint.
    #[inline]
    fn len(&self) -> usize {
        usize::try_from(self.span.len()).unwrap_or(usize::MAX)
    }
}

impl<B> Retained<B> {
    /// Drops registry entries whose lease is gone or already promoted.
    fn prune(&mut self) {
        self.leases
            .retain(|lease| lease.upgrade().is_some_and(|slot| !slot.is_promoted()));
    }

    /// Creates a loaned lease over `range` and registers it, pruning dead registry entries
    /// first when the registry is about to grow.
    ///
    /// A prune visits every registration, so it must buy room for proportionally many pushes
    /// before the next one. When fewer than half the registrations are pruned, the registry
    /// therefore also reserves room for as many again as survived. Either way at least half the
    /// visited length is free afterwards, so every full scan is paid for by that many pushes:
    /// the cost stays amortized O(1) per lease even when most leases are long-lived, and the
    /// registry stays proportional to the number of live loaned leases, even when the owner
    /// never calls [`Store::maintain`].
    fn lease(&mut self, range: SegmentRange) -> Lease<B> {
        let (lease, registration) = Lease::loaned(Arc::clone(&self.backing), range);

        if self.leases.len() == self.leases.capacity() {
            self.prune();

            let survivors = self.leases.len();
            if survivors > self.leases.capacity() / 2 {
                self.leases.reserve(survivors);
            }
        }

        self.leases.push(registration);
        lease
    }
}

/// Retained byte segments that hand out promotable [`Lease`]s.
///
/// The store is the data owner's side of the sharing: it parks segments the producer has finished
/// with, maps absolute [`Span`]s onto them, and tracks every loaned lease through a weak registry
/// plus an O(1) live-byte counter per segment. Retiring a segment promotes each surviving lease to
/// its own copy, after which the retained allocation is released; the owner never has to wait for
/// stragglers.
///
/// # Segment ordering
///
/// Parking validates what lookups rely on: segments are non-empty, in ascending non-overlapping
/// base order, and fully addressable within `u64`. A buffer that would break this is rejected
/// with a [`ParkError`] that hands the buffer back, so the store never holds a segment those
/// invariants do not cover. Gaps between segments are fine; spans reaching into a gap resolve to
/// `None`. Lookups binary-search the segments by offset.
///
/// # Buffer contract
///
/// `B` may be any `AsRef<[u8]>` type: `Vec<u8>`, `Box<[u8]>`, a memory-mapped region, or a
/// reader's retained buffer. The store trusts that `as_ref` returns the same slice, meaning the
/// same address, length, and contents, on every call. Everything in this crate is safe code, so an
/// implementation that breaks the contract degrades behavior but can never cause memory
/// unsafety: reads whose validated range no longer fits clamp to empty bytes, and a panicking
/// `as_ref` leaves promotion and retirement retryable.
///
/// # Memory
///
/// Retired segments leave a small bookkeeping tombstone behind so that [`SegmentId`]s stay
/// stable. The store compacts automatically: once the *oldest* parked segments are retired, their
/// tombstones are removed, so a streaming producer that parks and retires in order runs in
/// bounded memory. Queries against compacted identifiers keep answering with their settled state
/// (retired, zero live bytes).
///
/// Compaction and lease-registry pruning keep the capacity they empty for reuse instead of
/// reallocating, so bookkeeping memory follows the historical peak: the most segments resident
/// at once, and for each retained segment the most leases registered at once. A steady loop
/// reuses that capacity. After a burst, [`Self::shrink_to_fit`] releases it without touching
/// identifiers or retained bytes.
///
/// Registry entries are weak, so dropping a lease releases its pin at once, but its small shared
/// state stays allocated until its entry is pruned: when the registry next fills up, during
/// [`Self::maintain`] or [`Self::shrink_to_fit`], or when the segment retires.
///
/// Dropping the store without retiring is safe: each loaned lease shares ownership of its segment,
/// so the bytes stay alive until the last loaned lease is dropped or promoted. Retirement is a
/// memory-management action, not a safety requirement.
///
/// # Thread safety
///
/// `Store<B>` is `Send` and `Sync` exactly when `B` is `Send + Sync`. All owner-side operations
/// take `&mut self`; leases handed out may be read, cloned, and promoted from any thread while
/// the owner parks, sweeps, and retires.
///
/// # Trait semantics
///
/// The `Debug` output reports per-segment bookkeeping (span, status, registered leases, live
/// bytes) and the compaction offset; the bytes are not printed, so `B` needs no `Debug`
/// implementation.
pub struct Store<B = Vec<u8>> {
    /// Resident segments in park order, sorted by span.
    segments: VecDeque<Segment<B>>,
    /// Number of leading retired segments compacted away; offsets [`SegmentId`] indexing.
    compacted: u64,
    /// End offset of the last parked segment; the lowest base the next park accepts.
    high_water: u64,
}

impl<B> Default for Store<B> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<B> Debug for Store<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("segments", &self.segments)
            .field("compacted", &self.compacted)
            .field("high_water", &self.high_water)
            .finish()
    }
}

impl<B> Debug for Segment<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut segment = f.debug_struct("Segment");
        segment.field("span", &self.span);

        match &self.state {
            State::Retained(retained) => segment
                .field(
                    "status",
                    &if retained.sealed {
                        SegmentStatus::Sealed
                    } else {
                        SegmentStatus::Open
                    },
                )
                .field("registered_leases", &retained.leases.len())
                .field(
                    "live_bytes",
                    &retained.backing.live_bytes.load(Ordering::Relaxed),
                )
                .finish_non_exhaustive(),
            State::Retired => segment.field("status", &SegmentStatus::Retired).finish(),
        }
    }
}

impl<B> Store<B> {
    /// Creates an empty store.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::Store;
    /// let store: Store = Store::new();
    /// assert_eq!(store.segment_count(), 0);
    /// assert_eq!(store.retained_len(), 0);
    /// ```
    #[inline]
    pub const fn new() -> Self {
        Self {
            segments: VecDeque::new(),
            compacted: 0,
            high_water: 0,
        }
    }

    /// Returns the number of segments ever parked, including retired and compacted ones.
    ///
    /// This counts history, not residency: it never decreases, and a store whose every segment
    /// has been retired still reports the full count. Use [`Self::retained_len`] for current
    /// memory residency.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::Store;
    /// let mut store = Store::new();
    /// store.park(0, b"first".to_vec()).unwrap();
    /// store.park(5, b"second".to_vec()).unwrap();
    /// assert_eq!(store.segment_count(), 2);
    ///
    /// store.retire_all();
    /// assert_eq!(store.segment_count(), 2);
    /// ```
    #[inline]
    pub fn segment_count(&self) -> u64 {
        /* The resident count converts losslessly: it never exceeds the total parked count,
        which the u64 address space bounds. */
        self.compacted
            .saturating_add(u64::try_from(self.segments.len()).unwrap_or(u64::MAX))
    }

    /// Returns the total retained bytes across parked segments, saturating at `usize::MAX`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::Store;
    /// let mut store = Store::new();
    /// let first = store.park(0, b"first".to_vec()).unwrap();
    /// store.park(5, b"second".to_vec()).unwrap();
    /// assert_eq!(store.retained_len(), 11);
    ///
    /// store.retire(first);
    /// assert_eq!(store.retained_len(), 6);
    /// ```
    pub fn retained_len(&self) -> usize {
        self.segments.iter().fold(0usize, |total, segment| {
            if segment.is_retired() {
                total
            } else {
                total.saturating_add(segment.len())
            }
        })
    }

    /// Returns the live loaned bytes currently pinning `segment`, or `None` for identifiers this
    /// store never issued.
    ///
    /// Live bytes are counted with multiplicity: overlapping leases each contribute their full
    /// range, so the value is the number of bytes retirement would copy, not the union of covered
    /// bytes. The counter is advisory (it steers [`Self::maintain`]) and saturates instead of
    /// wrapping under pathological aggregate lengths. Retired segments report zero.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// let id = store.park(0, b"0123456789".to_vec()).unwrap();
    /// assert_eq!(store.live_bytes(id), Some(0));
    ///
    /// let head = store.lease(Span::new(0, 4).unwrap()).unwrap();
    /// let overlap = store.lease(Span::new(2, 6).unwrap()).unwrap();
    /// assert_eq!(store.live_bytes(id), Some(8)); // 4 + 4, with multiplicity
    ///
    /// drop(head);
    /// assert_eq!(store.live_bytes(id), Some(4));
    ///
    /// overlap.promote();
    /// assert_eq!(store.live_bytes(id), Some(0));
    /// ```
    pub fn live_bytes(&self, segment: SegmentId) -> Option<usize> {
        if self.is_compacted(segment) {
            return Some(0);
        }

        self.entry(segment).map(|segment| {
            segment.retained().map_or(0, |retained| {
                retained.backing.live_bytes.load(Ordering::Relaxed)
            })
        })
    }

    /// Returns the lifecycle state of `segment`, or `None` for identifiers this store never
    /// issued.
    ///
    /// Compacted segments answer [`SegmentStatus::Retired`]: identifiers stay valid for the
    /// lifetime of the store that issued them.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{SegmentStatus, Store};
    /// let mut store = Store::new();
    /// let id = store.park_open(0, b"lifecycle".to_vec()).unwrap();
    /// assert_eq!(store.status(id), Some(SegmentStatus::Open));
    ///
    /// store.seal(id);
    /// assert_eq!(store.status(id), Some(SegmentStatus::Sealed));
    ///
    /// store.retire(id);
    /// assert_eq!(store.status(id), Some(SegmentStatus::Retired));
    /// ```
    pub fn status(&self, segment: SegmentId) -> Option<SegmentStatus> {
        if self.is_compacted(segment) {
            return Some(SegmentStatus::Retired);
        }

        self.entry(segment).map(|segment| match &segment.state {
            State::Retained(retained) if retained.sealed => SegmentStatus::Sealed,
            State::Retained(_) => SegmentStatus::Open,
            State::Retired => SegmentStatus::Retired,
        })
    }

    /// Returns the absolute span a still-retained segment covers.
    ///
    /// Returns `None` for unknown or retired segments; every retained segment carries the span
    /// validated at parking.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// let id = store.park(100, b"based".to_vec()).unwrap();
    /// assert_eq!(store.segment_span(id), Some(Span::new(100, 105).unwrap()));
    ///
    /// store.retire(id);
    /// assert_eq!(store.segment_span(id), None);
    /// ```
    pub fn segment_span(&self, segment: SegmentId) -> Option<Span> {
        let segment = self.entry(segment)?;

        (!segment.is_retired()).then_some(segment.span)
    }

    /// Marks an open segment as retirable by [`Self::maintain`].
    ///
    /// Returns `false` when the segment does not exist. Sealing an already sealed or retired
    /// segment is a no-op that reports success.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{SegmentStatus, Store};
    /// let mut store = Store::new();
    /// let id = store.park_open(0, b"open".to_vec()).unwrap();
    ///
    /// assert!(store.seal(id));
    /// assert_eq!(store.status(id), Some(SegmentStatus::Sealed));
    ///
    /// // Sealing again is a no-op that still reports success
    /// assert!(store.seal(id));
    ///
    /// // Identifiers this store never issued are rejected
    /// let mut other = Store::new();
    /// other.park(0, b"a".to_vec()).unwrap();
    /// let unknown = other.park(1, b"b".to_vec()).unwrap();
    /// assert!(!store.seal(unknown));
    /// ```
    pub fn seal(&mut self, segment: SegmentId) -> bool {
        if self.is_compacted(segment) {
            return true;
        }

        let Some(segment) = self.entry_mut(segment) else {
            return false;
        };

        if let Some(retained) = segment.retained_mut() {
            retained.sealed = true;
        }

        true
    }

    /// Releases spare bookkeeping capacity left behind by earlier bursts.
    ///
    /// The segment list and every lease registry shrink to what they currently hold, after
    /// pruning registry entries for dropped or already promoted leases. Nothing is retired and no
    /// segment bytes are freed: [`SegmentId`]s, retained segments, and loaned leases are
    /// unaffected (see the [memory notes](Store#memory)). Each call reallocates whatever it
    /// shrinks, so call it after a burst rather than after every park or retirement.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{SegmentStatus, Store};
    /// let mut store = Store::new();
    /// let first = store.park(0, b"x".to_vec()).unwrap();
    /// for base in 1..1000 {
    ///     store.park(base, b"x".to_vec()).unwrap();
    /// }
    ///
    /// // The burst retires, but the store keeps capacity sized for 1000 segments
    /// assert_eq!(store.retire_all(), 1000);
    /// store.shrink_to_fit();
    ///
    /// // Identifiers still answer with their settled state, and numbering continues
    /// assert_eq!(store.status(first), Some(SegmentStatus::Retired));
    /// assert_eq!(store.park(1000, b"x".to_vec()).unwrap().index(), 1000);
    /// ```
    pub fn shrink_to_fit(&mut self) {
        self.segments.shrink_to_fit();

        for segment in &mut self.segments {
            if let Some(retained) = segment.retained_mut() {
                retained.prune();
                retained.leases.shrink_to_fit();
            }
        }
    }

    /// Returns the segment for `id` when it has not been compacted away.
    fn entry(&self, id: SegmentId) -> Option<&Segment<B>> {
        let offset = id.index().checked_sub(self.compacted)?;
        self.segments.get(usize::try_from(offset).ok()?)
    }

    /// Mutable counterpart of [`Self::entry`].
    fn entry_mut(&mut self, id: SegmentId) -> Option<&mut Segment<B>> {
        let offset = id.index().checked_sub(self.compacted)?;
        self.segments.get_mut(usize::try_from(offset).ok()?)
    }

    /// Returns whether `id` refers to a segment compacted away after retirement.
    #[inline]
    fn is_compacted(&self, id: SegmentId) -> bool {
        id.index() < self.compacted
    }

    /// Returns the position of the first resident segment that ends after `offset`.
    ///
    /// Segments are sorted and non-overlapping, so this is the only segment that can contain
    /// `offset`, and every segment from here on lies at or after it. The position is
    /// `self.segments.len()` when no segment ends after `offset`.
    #[inline]
    fn position_after(&self, offset: u64) -> usize {
        self.segments
            .partition_point(|segment| segment.span.end() <= offset)
    }

    /// Returns the position of the resident segment that fully contains `span`, plus the range
    /// of the span within it.
    fn containing(&self, span: Span) -> Option<(usize, SegmentRange)> {
        let position = self.position_after(span.start());
        let range = span.segment_range(&self.segments.get(position)?.span)?;

        Some((position, range))
    }

    /// Removes the tombstones of leading retired segments.
    ///
    /// Only ever removes retired segments, whose observable state is settled, so compaction is
    /// invisible through the public API. The emptied deque capacity is kept for later parks;
    /// [`Self::shrink_to_fit`] releases it.
    fn compact(&mut self) {
        while self.segments.front().is_some_and(Segment::is_retired) {
            self.segments.pop_front();
            self.compacted = self.compacted.saturating_add(1);
        }
    }
}

impl<B: AsRef<[u8]>> Store<B> {
    /// Parks a segment with its absolute base offset.
    ///
    /// The segment is sealed: [`Self::maintain`] may retire it as soon as its waste threshold is
    /// met. Use [`Self::park_open`] to defer that.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{ParkErrorKind, Span, Store};
    /// let mut store = Store::new();
    /// let id = store.park(0, b"alpha".to_vec()).unwrap();
    /// assert_eq!(id.index(), 0);
    /// assert_eq!(store.bytes(Span::new(0, 5).unwrap()), Some(b"alpha".as_slice()));
    ///
    /// // Segments must follow each other; gaps are fine
    /// store.park(10, b"omega".to_vec()).unwrap();
    /// assert_eq!(store.bytes(Span::new(5, 7).unwrap()), None);
    ///
    /// // Going backwards is not
    /// let error = store.park(7, b"late".to_vec()).unwrap_err();
    /// assert_eq!(error.kind(), ParkErrorKind::OutOfOrder { base: 7, min_base: 15 });
    /// ```
    ///
    /// # Errors
    ///
    /// Parking validates the segment invariants up front instead of storing a segment lookups
    /// cannot honor. The buffer must be non-empty ([`ParkErrorKind::Empty`]), `base` must not
    /// precede the end of the previously parked segment ([`ParkErrorKind::OutOfOrder`]), and
    /// `base + len` must not exceed [`u64::MAX`] ([`ParkErrorKind::Unaddressable`]). A rejected
    /// buffer is handed back inside the [`ParkError`].
    #[inline]
    pub fn park(&mut self, base: u64, bytes: B) -> Result<SegmentId, ParkError<B>> {
        self.insert(base, bytes, true)
    }

    /// Parks a segment that [`Self::maintain`] must leave alone until [`Self::seal`].
    ///
    /// Use this when parking ahead of attaching leases, so a maintenance sweep between the two
    /// steps cannot retire the still-unleased segment. Explicit [`Self::retire`] and
    /// [`Self::retire_all`] calls ignore the seal state.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{SegmentStatus, Span, Store};
    /// let mut store = Store::new();
    /// let id = store.park_open(0, b"key=value\n".to_vec()).unwrap();
    ///
    /// // A sweep leaves the open segment alone, even with nothing pinning it
    /// assert_eq!(store.maintain(0), 0);
    /// assert_eq!(store.status(id), Some(SegmentStatus::Open));
    ///
    /// // Attach the leases, then seal
    /// let value = store.lease(Span::new(4, 9).unwrap()).unwrap();
    /// store.seal(id);
    /// assert_eq!(store.maintain(0), 1);
    /// assert!(value.is_owned());
    /// ```
    ///
    /// # Errors
    ///
    /// The validation of [`Self::park`] applies.
    #[inline]
    pub fn park_open(&mut self, base: u64, bytes: B) -> Result<SegmentId, ParkError<B>> {
        self.insert(base, bytes, false)
    }

    /// Validates and appends a segment; see [`Self::park`] for the rules.
    fn insert(&mut self, base: u64, bytes: B, sealed: bool) -> Result<SegmentId, ParkError<B>> {
        let Ok(len) = u64::try_from(bytes.as_ref().len()) else {
            /* Unreachable outside hypothetical platforms wider than 64 bits; such a length
            cannot fit the u64 address space either way. */
            return Err(ParkError {
                kind: ParkErrorKind::Unaddressable {
                    base,
                    len: u64::MAX,
                },
                bytes,
            });
        };

        if len == 0 {
            return Err(ParkError {
                kind: ParkErrorKind::Empty,
                bytes,
            });
        }

        if base < self.high_water {
            return Err(ParkError {
                kind: ParkErrorKind::OutOfOrder {
                    base,
                    min_base: self.high_water,
                },
                bytes,
            });
        }

        /* `len >= 1` forces `base < base + len`, so once the addition fits, the span bounds
        are valid by construction; both failure paths funnel into the same rejection. */
        let Some(span) = base
            .checked_add(len)
            .and_then(|end| Span::new(base, end).ok())
        else {
            return Err(ParkError {
                kind: ParkErrorKind::Unaddressable { base, len },
                bytes,
            });
        };

        let id = SegmentId(self.segment_count());
        self.high_water = span.end();
        self.segments.push_back(Segment {
            span,
            state: State::Retained(Retained {
                backing: Arc::new(Backing {
                    bytes,
                    live_bytes: AtomicUsize::new(0),
                }),
                sealed,
                leases: Vec::new(),
            }),
        });

        Ok(id)
    }

    /// Returns bytes for `span` when one retained segment fully contains it.
    ///
    /// This is the zero-copy peek for owners and trusted callers; the borrow pins the store
    /// itself, not the segment. Spans crossing segment boundaries resolve to `None`: lease them
    /// through [`Self::leases`] or copy them through [`Self::copy_bytes`] instead.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, b"alpha".to_vec()).unwrap();
    /// store.park(5, b"beta".to_vec()).unwrap();
    ///
    /// assert_eq!(store.bytes(Span::new(1, 4).unwrap()), Some(b"lph".as_slice()));
    /// assert_eq!(store.bytes(Span::new(5, 9).unwrap()), Some(b"beta".as_slice()));
    ///
    /// // Straddling both segments does not resolve here
    /// assert_eq!(store.bytes(Span::new(3, 7).unwrap()), None);
    /// ```
    pub fn bytes(&self, span: Span) -> Option<&[u8]> {
        let (position, range) = self.containing(span)?;
        let retained = self.segments.get(position)?.retained()?;

        retained.backing.bytes.as_ref().get(range.bounds())
    }

    /// Creates a lease for `span` when one retained segment fully contains it.
    ///
    /// The lease starts loaned: it reads from the retained segment and pins it until the lease is
    /// dropped, promoted, or the segment is retired.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// let id = store.park(0, b"key=value\n".to_vec()).unwrap();
    ///
    /// let value = store.lease(Span::new(4, 9).unwrap()).unwrap();
    /// assert!(value.is_loaned());
    /// assert_eq!(&*value.bytes(), b"value");
    /// assert_eq!(store.live_bytes(id), Some(5));
    ///
    /// // Spans outside the retained segments do not resolve
    /// assert!(store.lease(Span::new(4, 20).unwrap()).is_none());
    /// ```
    pub fn lease(&mut self, span: Span) -> Option<Lease<B>> {
        let (position, range) = self.containing(span)?;
        let retained = self.segments.get_mut(position)?.retained_mut()?;

        Some(retained.lease(range))
    }

    /// Maps `span` onto retained segments as `(position, range)` pairs covering the whole span in
    /// park order.
    ///
    /// Positions index `self.segments` directly and are only valid until the next mutation.
    /// Returns `None` when any byte of the span falls in a gap or a retired segment.
    fn ranges(&self, span: Span) -> Option<Vec<(usize, SegmentRange)>> {
        let mut cursor = span.start();
        let mut ranges = Vec::new();
        let first = self.position_after(cursor);

        for (position, segment) in self.segments.iter().enumerate().skip(first) {
            if cursor >= span.end() {
                break;
            }

            /* Every segment from `first` on ends after the cursor, so the cursor is either
            inside this segment or in the gap before it. */
            if cursor < segment.span.start() || segment.is_retired() {
                return None;
            }

            // Non-empty by the loop guard (`cursor < span.end()`) and `cursor < segment.end()`
            let stop = span.end().min(segment.span.end());
            let piece = Span::new(cursor, stop).ok()?;
            ranges.push((position, piece.segment_range(&segment.span)?));
            cursor = stop;
        }

        (cursor == span.end()).then_some(ranges)
    }

    /// Copies bytes for `span` into a vector, following spans across segment boundaries.
    ///
    /// This is the read-only counterpart of [`Self::leases`] for callers that want bytes without
    /// creating a handle. Returns `None` when any part of the span is not retained, or, on
    /// platforms narrower than 64 bits, when the copy could not fit in memory to begin with.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, b"alpha".to_vec()).unwrap();
    /// store.park(5, b"beta".to_vec()).unwrap();
    ///
    /// assert_eq!(store.copy_bytes(Span::new(3, 7).unwrap()), Some(b"habe".to_vec()));
    /// assert_eq!(store.copy_bytes(Span::new(3, 20).unwrap()), None);
    /// ```
    pub fn copy_bytes(&self, span: Span) -> Option<Vec<u8>> {
        let capacity = usize::try_from(span.len()).ok()?;
        let ranges = self.ranges(span)?;
        let mut bytes = Vec::with_capacity(capacity);

        for (position, range) in ranges {
            let retained = self.segments.get(position)?.retained()?;
            bytes.extend_from_slice(retained.backing.bytes.as_ref().get(range.bounds())?);
        }

        Some(bytes)
    }

    /// Creates leases covering `span` across consecutive retained segments.
    ///
    /// Spans that fit one segment yield a single lease. Returns `None` when any part of the span
    /// is not retained; a successful result always contains at least one lease.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, b"alpha".to_vec()).unwrap();
    /// store.park(5, b"beta".to_vec()).unwrap();
    ///
    /// let parts = store.leases(Span::new(3, 7).unwrap()).unwrap();
    /// assert_eq!(parts.len(), 2);
    /// assert_eq!(&*parts[0].bytes(), b"ha");
    /// assert_eq!(&*parts[1].bytes(), b"be");
    ///
    /// // A span inside one segment yields one lease
    /// let parts = store.leases(Span::new(5, 9).unwrap()).unwrap();
    /// assert_eq!(parts.len(), 1);
    /// ```
    pub fn leases(&mut self, span: Span) -> Option<Vec<Lease<B>>> {
        let ranges = self.ranges(span)?;
        let mut collected = Vec::with_capacity(ranges.len());

        for (position, range) in ranges {
            let retained = self.segments.get_mut(position)?.retained_mut()?;
            collected.push(retained.lease(range));
        }

        Some(collected)
    }

    /// Retires `segment`: every surviving lease is promoted to its own copy in place, then the
    /// retained bytes are released.
    ///
    /// Returns `false` when the segment does not exist or was already retired. Plain
    /// [`Self::bytes`] lookups into the segment resolve to `None` afterwards. Retirement never
    /// blocks on readers: outstanding [`LeaseBytes`](crate::LeaseBytes) guards keep their own pin
    /// on the allocation and release it when dropped.
    ///
    /// Promotion runs before anything is released, so if a promotion panics (a panicking buffer
    /// `as_ref` implementation), the registry and retained bytes are intact and `retire` can
    /// simply be called again.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// let id = store.park(0, b"prefix:value:suffix".to_vec()).unwrap();
    /// let value = store.lease(Span::new(7, 12).unwrap()).unwrap();
    ///
    /// assert!(store.retire(id));
    /// assert!(value.is_owned());
    /// assert_eq!(&*value.bytes(), b"value");
    /// assert_eq!(store.retained_len(), 0);
    /// assert_eq!(store.bytes(Span::new(7, 12).unwrap()), None);
    ///
    /// // Retiring twice reports nothing to do
    /// assert!(!store.retire(id));
    /// ```
    pub fn retire(&mut self, segment: SegmentId) -> bool {
        let Some(segment) = self.entry_mut(segment) else {
            return false;
        };

        let Some(retained) = segment.retained() else {
            return false;
        };

        for registration in &retained.leases {
            if let Some(slot) = registration.upgrade() {
                slot.promote();
            }
        }

        segment.state = State::Retired;
        self.compact();
        true
    }

    /// Retires every remaining segment, promoting all surviving leases.
    ///
    /// This is the owner's unconditional escape hatch: afterwards the store retains no bytes,
    /// regardless of seal state or waste thresholds. Returns the number of segments retired by
    /// this call.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, b"alpha".to_vec()).unwrap();
    /// store.park_open(5, b"beta".to_vec()).unwrap();
    /// let lease = store.lease(Span::new(0, 5).unwrap()).unwrap();
    ///
    /// assert_eq!(store.retire_all(), 2);
    /// assert_eq!(store.retained_len(), 0);
    /// assert!(lease.is_owned());
    /// assert_eq!(store.retire_all(), 0);
    /// ```
    pub fn retire_all(&mut self) -> usize {
        (self.compacted..self.segment_count())
            .filter(|&index| self.retire(SegmentId(index)))
            .count()
    }

    /// Retires sealed segments whose wasted bytes meet `min_wasted_bytes`, and prunes registry
    /// entries for dropped or already promoted leases.
    ///
    /// Wasted bytes are the segment length minus its live loaned bytes, so a segment with no live
    /// leases wastes its entire length and a threshold of `0` retires every sealed segment, even
    /// one whose bytes are all still loaned. The threshold check is O(1) per segment through the
    /// shared live-byte counters; the pruning pass touches each registry entry. Returns the
    /// number of segments retired.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, vec![b'x'; 8 * 1024]).unwrap();
    /// let straggler = store.lease(Span::new(10, 26).unwrap()).unwrap();
    ///
    /// // 8 KiB retained for 16 live bytes: not worth it past a 4 KiB waste threshold
    /// assert_eq!(store.maintain(8 * 1024), 0);
    /// assert!(straggler.is_loaned());
    /// assert_eq!(store.maintain(4 * 1024), 1);
    /// assert!(straggler.is_owned());
    /// assert_eq!(store.retained_len(), 0);
    /// ```
    pub fn maintain(&mut self, min_wasted_bytes: usize) -> usize {
        let mut retirable = Vec::new();
        let compacted = self.compacted;

        for (position, segment) in self.segments.iter_mut().enumerate() {
            let len = segment.len();
            let Some(retained) = segment.retained_mut() else {
                continue;
            };

            retained.prune();

            if !retained.sealed {
                continue;
            }

            let live = retained.backing.live_bytes.load(Ordering::Relaxed);

            if len.saturating_sub(live) >= min_wasted_bytes {
                let position = u64::try_from(position).unwrap_or(u64::MAX);
                retirable.push(compacted.saturating_add(position));
            }
        }

        retirable
            .into_iter()
            .filter(|&index| self.retire(SegmentId(index)))
            .count()
    }
}

#[cfg(test)]
mod tests;
