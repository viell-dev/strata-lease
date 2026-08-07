//! The owner side: retained segments, the lease registry, and retirement.

use crate::lease::{Backing, Lease, Slot};
use crate::span::{SegmentRange, Span};
use std::collections::VecDeque;
use std::error;
use std::fmt::{self, Debug, Display, Formatter};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

/// Identifier for a parked segment.
///
/// Identifiers are assigned in park order, are never reused, and stay valid for the lifetime of
/// the store that issued them — including after the segment is retired. The index is `u64` so
/// that no-reuse holds on every target: each parked segment covers at least one byte of the
/// `u64` address space, which bounds the number of parks a store can ever accept. Identifiers
/// carry no store identity: passing an identifier to a *different* store is a logic error that
/// safely resolves against whatever segment holds that position there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(u64);

impl SegmentId {
    /// Returns the numeric segment index.
    #[inline]
    #[must_use]
    pub const fn index(self) -> u64 {
        self.0
    }
}

/// Lifecycle state of a parked segment, reported by [`Store::status`].
///
/// A segment only ever moves forward: `Open` → `Sealed` → `Retired`. [`Store::park`] starts a
/// segment at `Sealed`; [`Store::park_open`] starts it at `Open`.
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

/// An error returned when a buffer cannot be parked.
///
/// The rejected buffer rides along and is handed back through [`ParkError::into_bytes`], so a
/// failed park never costs the producer its data.
pub struct ParkError<B> {
    kind: ParkErrorKind,
    bytes: B,
}

impl<B> ParkError<B> {
    /// Returns why the buffer was rejected.
    #[inline]
    #[must_use]
    pub const fn kind(&self) -> ParkErrorKind {
        self.kind
    }

    /// Returns the rejected buffer.
    #[inline]
    #[must_use]
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

impl<B> Display for ParkError<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.kind, f)
    }
}

impl<B> error::Error for ParkError<B> {}

/// A parked segment: retained bytes plus lease bookkeeping, or a retired tombstone.
///
/// Retirement swaps the whole retained state for the tombstone at once, so a retired segment
/// cannot carry a stale seal flag, lease registry, or live-byte counter.
enum Segment<B> {
    Retained(Retained<B>),
    Retired,
}

impl<B> Segment<B> {
    fn retained(&self) -> Option<&Retained<B>> {
        match self {
            Self::Retained(retained) => Some(retained),
            Self::Retired => None,
        }
    }

    fn retained_mut(&mut self) -> Option<&mut Retained<B>> {
        match self {
            Self::Retained(retained) => Some(retained),
            Self::Retired => None,
        }
    }
}

struct Retained<B> {
    /// The absolute span the segment covers, validated at parking.
    span: Span,
    backing: Arc<Backing<B>>,
    sealed: bool,
    leases: Vec<Weak<Slot<B>>>,
}

impl<B> Retained<B> {
    /// Segment length in bytes, from the span validated at parking.
    ///
    /// The length originated as a buffer's `usize` length at parking, so the narrowing cannot
    /// actually fail; the saturating fallback only satisfies the lint.
    fn len(&self) -> usize {
        usize::try_from(self.span.len()).unwrap_or(usize::MAX)
    }

    /// Drops registry entries whose lease is gone or already promoted.
    fn prune(&mut self) {
        self.leases
            .retain(|lease| lease.upgrade().is_some_and(|slot| !slot.is_promoted()));
    }

    /// Creates a loaned lease over `range` and registers it, pruning dead registry entries
    /// first when the registry is about to grow.
    ///
    /// Pruning at capacity keeps the registry proportional to the number of live loaned leases
    /// with amortized O(1) cost, even when the owner never calls
    /// [`maintain`](crate::Store::maintain).
    fn lease(&mut self, range: SegmentRange) -> Lease<B> {
        let (lease, registration) = Lease::loaned(Arc::clone(&self.backing), range);

        if self.leases.len() == self.leases.capacity() {
            self.prune();
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
/// its own copy, after which the retained allocation is released — the owner never has to wait for
/// stragglers.
///
/// # Segment ordering
///
/// Parking validates what lookups rely on: segments are non-empty, in ascending non-overlapping
/// base order, and fully addressable within `u64`. A buffer that would break this is rejected
/// with a [`ParkError`] that hands the buffer back, so the store never holds a segment those
/// invariants do not cover. Gaps between segments are fine; spans reaching into a gap resolve to
/// `None`.
///
/// # Memory
///
/// Retired segments leave a small bookkeeping tombstone behind so that [`SegmentId`]s stay
/// stable. The store compacts automatically: once the *oldest* parked segments are retired, their
/// tombstones are freed, so a streaming producer that parks and retires in order runs in bounded
/// memory. Queries against compacted identifiers keep answering with their settled state
/// (retired, zero live bytes).
pub struct Store<B = Vec<u8>> {
    segments: VecDeque<Segment<B>>,
    /// Number of leading retired segments compacted away; offsets [`SegmentId`] indexing.
    compacted: u64,
    /// End offset of the last parked segment; the lowest base the next park accepts.
    high_water: u64,
}

impl<B> Store<B> {
    /// Creates an empty store.
    #[inline]
    #[must_use]
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
    /// has been retired still reports the full count.
    #[inline]
    #[must_use]
    pub fn segment_count(&self) -> u64 {
        /* The resident count converts losslessly: it never exceeds the total parked count,
        which the u64 address space bounds. */
        self.compacted
            .saturating_add(u64::try_from(self.segments.len()).unwrap_or(u64::MAX))
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
    fn is_compacted(&self, id: SegmentId) -> bool {
        id.index() < self.compacted
    }

    /// Frees the bookkeeping of leading retired segments.
    ///
    /// Only ever removes retired segments, whose observable state is settled, so compaction is
    /// invisible through the public API apart from the memory it returns.
    fn compact(&mut self) {
        while matches!(self.segments.front(), Some(Segment::Retired)) {
            self.segments.pop_front();
            self.compacted = self.compacted.saturating_add(1);
        }
    }

    /// Returns the live loaned bytes currently pinning `segment`.
    ///
    /// Live bytes are counted with multiplicity: overlapping leases each contribute their full
    /// range, so the value is the number of bytes retirement would copy, not the union of covered
    /// bytes. The counter is advisory (it steers [`Self::maintain`]) and saturates instead of
    /// wrapping under pathological aggregate lengths.
    #[must_use]
    pub fn live_bytes(&self, segment: SegmentId) -> Option<usize> {
        if self.is_compacted(segment) {
            return Some(0);
        }

        self.entry(segment).map(|entry| {
            entry.retained().map_or(0, |retained| {
                retained.backing.live_bytes.load(Ordering::Relaxed)
            })
        })
    }

    /// Returns the lifecycle state of `segment`, or `None` for identifiers this store never
    /// issued.
    ///
    /// Compacted segments answer [`SegmentStatus::Retired`]: identifiers stay valid for the
    /// lifetime of the store that issued them.
    #[must_use]
    pub fn status(&self, segment: SegmentId) -> Option<SegmentStatus> {
        if self.is_compacted(segment) {
            return Some(SegmentStatus::Retired);
        }

        self.entry(segment).map(|entry| match entry {
            Segment::Retained(retained) if retained.sealed => SegmentStatus::Sealed,
            Segment::Retained(_) => SegmentStatus::Open,
            Segment::Retired => SegmentStatus::Retired,
        })
    }

    /// Marks an open segment as retirable by [`Self::maintain`].
    ///
    /// Returns `false` when the segment does not exist. Sealing an already sealed or retired
    /// segment is a no-op that reports success.
    pub fn seal(&mut self, segment: SegmentId) -> bool {
        if self.is_compacted(segment) {
            return true;
        }

        let Some(entry) = self.entry_mut(segment) else {
            return false;
        };

        if let Some(retained) = entry.retained_mut() {
            retained.sealed = true;
        }

        true
    }

    /// Returns the total retained bytes across parked segments, saturating at `usize::MAX`.
    #[must_use]
    pub fn retained_len(&self) -> usize {
        self.segments.iter().fold(0usize, |total, entry| {
            total.saturating_add(entry.retained().map_or(0, Retained::len))
        })
    }

    /// Returns the absolute span a still-retained segment covers.
    ///
    /// Returns `None` for unknown or retired segments; every retained segment carries the span
    /// validated at parking.
    #[must_use]
    pub fn segment_span(&self, segment: SegmentId) -> Option<Span> {
        self.entry(segment)?
            .retained()
            .map(|retained| retained.span)
    }
}

impl<B: AsRef<[u8]>> Store<B> {
    /// Parks a segment with its absolute base offset.
    ///
    /// The segment is sealed: [`Self::maintain`] may retire it as soon as its waste threshold is
    /// met. Use [`Self::park_open`] to defer that.
    ///
    /// Parking validates the segment invariants up front instead of storing a segment lookups
    /// cannot honor: the buffer must be non-empty, `base` must not precede the end of the
    /// previously parked segment, and `base + len` must not exceed [`u64::MAX`]. A rejected
    /// buffer is handed back inside the [`ParkError`].
    pub fn park(&mut self, base: u64, bytes: B) -> Result<SegmentId, ParkError<B>> {
        self.insert(base, bytes, true)
    }

    /// Parks a segment that [`Self::maintain`] must leave alone until [`Self::seal`].
    ///
    /// Use this when parking ahead of attaching leases, so a maintenance sweep between the two
    /// steps cannot retire the still-unleased segment. Explicit [`Self::retire`] and
    /// [`Self::retire_all`] calls ignore the seal state. The validation of [`Self::park`]
    /// applies.
    pub fn park_open(&mut self, base: u64, bytes: B) -> Result<SegmentId, ParkError<B>> {
        self.insert(base, bytes, false)
    }

    fn insert(&mut self, base: u64, bytes: B, sealed: bool) -> Result<SegmentId, ParkError<B>> {
        let Ok(len) = u64::try_from(bytes.as_ref().len()) else {
            // Unreachable outside hypothetical >64-bit platforms; such a length cannot fit the
            // u64 address space either way.
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
        self.segments.push_back(Segment::Retained(Retained {
            span,
            backing: Arc::new(Backing {
                bytes,
                live_bytes: AtomicUsize::new(0),
            }),
            sealed,
            leases: Vec::new(),
        }));

        Ok(id)
    }

    /// Returns bytes for `span` when one retained segment fully contains it.
    ///
    /// This is the zero-copy peek for owners and trusted callers; the borrow pins the store
    /// itself, not the segment. Spans crossing segment boundaries resolve to `None` — lease them
    /// through [`Self::leases`] or copy them through [`Self::copy_bytes`] instead.
    #[must_use]
    pub fn bytes(&self, span: Span) -> Option<&[u8]> {
        self.segments.iter().find_map(|entry| {
            let retained = entry.retained()?;
            let range = span.segment_range(retained.span)?;
            retained.backing.bytes.as_ref().get(range.bounds())
        })
    }

    /// Creates a lease for `span` when one retained segment fully contains it.
    ///
    /// The lease starts loaned: it reads from the retained segment and pins it until the lease is
    /// dropped, promoted, or the segment is retired.
    #[must_use]
    pub fn lease(&mut self, span: Span) -> Option<Lease<B>> {
        self.segments.iter_mut().find_map(|entry| {
            let retained = entry.retained_mut()?;
            let range = span.segment_range(retained.span)?;

            Some(retained.lease(range))
        })
    }

    /// Maps `span` onto retained segments as `(position, range)` pairs covering the whole span in
    /// park order. Positions index `self.segments` directly and are only valid until the next
    /// mutation.
    fn ranges(&self, span: Span) -> Option<Vec<(usize, SegmentRange)>> {
        let mut cursor = span.start();
        let mut ranges = Vec::new();

        for (position, entry) in self.segments.iter().enumerate() {
            if cursor >= span.end() {
                break;
            }

            let Some(retained) = entry.retained() else {
                continue;
            };
            let segment = retained.span;

            if cursor >= segment.end() || span.end() <= segment.start() {
                continue;
            }

            if cursor < segment.start() {
                return None;
            }

            // Non-empty by the loop guard (`cursor < span.end()`) and `cursor < segment.end()`.
            let stop = span.end().min(segment.end());
            let piece = Span::new(cursor, stop).ok()?;
            ranges.push((position, piece.segment_range(segment)?));
            cursor = stop;
        }

        (cursor == span.end()).then_some(ranges)
    }

    /// Copies bytes for `span` into a vector, following spans across segment boundaries.
    ///
    /// This is the read-only counterpart of [`Self::leases`] for callers that want bytes without
    /// creating a handle. Returns `None` when any part of the span is not retained.
    #[must_use]
    pub fn copy_bytes(&self, span: Span) -> Option<Vec<u8>> {
        let ranges = self.ranges(span)?;
        let mut bytes = Vec::with_capacity(usize::try_from(span.len()).unwrap_or(0));

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
    #[must_use]
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
    pub fn retire(&mut self, segment: SegmentId) -> bool {
        let Some(entry) = self.entry_mut(segment) else {
            return false;
        };

        let Some(retained) = entry.retained() else {
            return false;
        };

        for registration in &retained.leases {
            if let Some(slot) = registration.upgrade() {
                slot.promote();
            }
        }

        *entry = Segment::Retired;
        self.compact();
        true
    }

    /// Retires every remaining segment, promoting all surviving leases.
    ///
    /// This is the owner's unconditional escape hatch: afterwards the store retains no bytes,
    /// regardless of seal state or waste thresholds. Returns the number of segments retired by
    /// this call.
    pub fn retire_all(&mut self) -> usize {
        (self.compacted..self.segment_count())
            .filter(|&index| self.retire(SegmentId(index)))
            .count()
    }

    /// Retires sealed segments whose wasted bytes meet `min_wasted_bytes`, and prunes registry
    /// entries for dropped or already promoted leases.
    ///
    /// Wasted bytes are the segment length minus its live loaned bytes, so a segment with no live
    /// leases wastes its entire length and a threshold of `0` retires every sealed segment — even
    /// one whose bytes are all still loaned. The threshold check is O(1) per segment through the
    /// shared live-byte counters; the pruning pass touches each registry entry. Returns the
    /// number of segments retired.
    pub fn maintain(&mut self, min_wasted_bytes: usize) -> usize {
        let mut retirable = Vec::new();
        let compacted = self.compacted;

        for (position, entry) in self.segments.iter_mut().enumerate() {
            let Some(retained) = entry.retained_mut() else {
                continue;
            };

            retained.prune();

            if !retained.sealed {
                continue;
            }

            let live = retained.backing.live_bytes.load(Ordering::Relaxed);

            if retained.len().saturating_sub(live) >= min_wasted_bytes {
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

impl<B> Default for Store<B> {
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
        match self {
            Self::Retained(retained) => f
                .debug_struct("Retained")
                .field("span", &retained.span)
                .field("sealed", &retained.sealed)
                .field("registered_leases", &retained.leases.len())
                .field(
                    "live_bytes",
                    &retained.backing.live_bytes.load(Ordering::Relaxed),
                )
                .finish_non_exhaustive(),
            Self::Retired => f.write_str("Retired"),
        }
    }
}

#[cfg(test)]
mod tests;
