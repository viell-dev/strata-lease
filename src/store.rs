//! The owner side: retained segments, the lease registry, and retirement.

use crate::lease::{Lease, Slot};
use crate::span::Span;
use std::collections::VecDeque;
use std::fmt::{self, Debug, Formatter};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

/// Identifier for a parked segment.
///
/// Identifiers are assigned in park order, are never reused, and stay valid for the lifetime of
/// the store that issued them — including after the segment is retired. They carry no store
/// identity: passing an identifier to a *different* store is a logic error that safely resolves
/// against whatever segment holds that position there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(usize);

impl SegmentId {
    /// Returns the numeric segment index.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

struct Segment<B> {
    base: u64,
    bytes: Option<Arc<B>>,
    sealed: bool,
    leases: Vec<Weak<Slot<B>>>,
    live_bytes: Arc<AtomicUsize>,
}

impl<B> Segment<B> {
    /// Drops registry entries whose lease is gone or already promoted.
    fn prune(&mut self) {
        self.leases
            .retain(|lease| lease.upgrade().is_some_and(|slot| !slot.is_promoted()));
    }

    /// Registers a loaned lease, pruning dead entries first when the registry is about to grow.
    ///
    /// Pruning at capacity keeps the registry proportional to the number of live loaned leases
    /// with amortized O(1) cost, even when the owner never calls
    /// [`maintain`](crate::Store::maintain).
    fn register(&mut self, registration: Weak<Slot<B>>) {
        if self.leases.len() == self.leases.capacity() {
            self.prune();
        }

        self.leases.push(registration);
    }
}

impl<B: AsRef<[u8]>> Segment<B> {
    fn len(&self) -> usize {
        self.bytes
            .as_ref()
            .map_or(0, |bytes| (**bytes).as_ref().len())
    }

    /// Maps `span` to a range inside this segment when the segment fully contains it.
    fn range_for(&self, span: Span) -> Option<Range<usize>> {
        if span.end < span.start || self.bytes.is_none() {
            return None;
        }

        let len = u64::try_from(self.len()).ok()?;
        let end = self.base.checked_add(len)?;

        if span.start < self.base || span.end > end {
            return None;
        }

        let start = usize::try_from(span.start.checked_sub(self.base)?).ok()?;
        let stop = usize::try_from(span.end.checked_sub(self.base)?).ok()?;

        Some(start..stop)
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
/// Segments are matched in park order. Bases are expected to be ascending and non-overlapping, as
/// produced by parking consecutive buffers of one input stream; [`Store::park`] does not validate
/// this. With out-of-order bases, single-segment lookups still resolve, but spans crossing
/// segment boundaries conservatively resolve to `None`. With overlapping bases, the first-parked
/// segment containing a span wins.
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
    compacted: usize,
}

impl<B> Store<B> {
    /// Creates an empty store.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            segments: VecDeque::new(),
            compacted: 0,
        }
    }

    /// Returns the number of segments ever parked, including retired ones.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.compacted.saturating_add(self.segments.len())
    }

    /// Returns `true` when no segments have ever been parked.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the segment for `id` when it has not been compacted away.
    fn entry(&self, id: SegmentId) -> Option<&Segment<B>> {
        self.segments.get(id.index().checked_sub(self.compacted)?)
    }

    /// Mutable counterpart of [`Self::entry`].
    fn entry_mut(&mut self, id: SegmentId) -> Option<&mut Segment<B>> {
        self.segments
            .get_mut(id.index().checked_sub(self.compacted)?)
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
        while self
            .segments
            .front()
            .is_some_and(|entry| entry.bytes.is_none())
        {
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

        self.entry(segment)
            .map(|entry| entry.live_bytes.load(Ordering::Relaxed))
    }

    /// Returns whether `segment` has been retired.
    #[must_use]
    pub fn is_retired(&self, segment: SegmentId) -> Option<bool> {
        if self.is_compacted(segment) {
            return Some(true);
        }

        self.entry(segment).map(|entry| entry.bytes.is_none())
    }

    /// Returns whether `segment` is sealed, and therefore eligible for [`Self::maintain`].
    ///
    /// Retired segments report `true`: there is nothing left to protect from a sweep.
    #[must_use]
    pub fn is_sealed(&self, segment: SegmentId) -> Option<bool> {
        if self.is_compacted(segment) {
            return Some(true);
        }

        self.entry(segment)
            .map(|entry| entry.sealed || entry.bytes.is_none())
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

        entry.sealed = true;
        true
    }

    /// Parks a segment with its absolute base offset.
    ///
    /// The segment is sealed: [`Self::maintain`] may retire it as soon as its waste threshold is
    /// met. Use [`Self::park_open`] to defer that.
    ///
    /// Bases are expected to be ascending and non-overlapping across parks; see the
    /// [type-level notes](Store#segment-ordering). `base + bytes.len()` must not exceed
    /// [`u64::MAX`]; a segment parked past that limit is retained but unaddressable, and every
    /// span lookup into it resolves to `None`.
    pub fn park(&mut self, base: u64, bytes: B) -> SegmentId {
        self.insert(base, bytes, true)
    }

    /// Parks a segment that [`Self::maintain`] must leave alone until [`Self::seal`].
    ///
    /// Use this when parking ahead of attaching leases, so a maintenance sweep between the two
    /// steps cannot retire the still-unleased segment. Explicit [`Self::retire`] and
    /// [`Self::retire_all`] calls ignore the seal state. The base expectations of [`Self::park`]
    /// apply.
    pub fn park_open(&mut self, base: u64, bytes: B) -> SegmentId {
        self.insert(base, bytes, false)
    }

    fn insert(&mut self, base: u64, bytes: B, sealed: bool) -> SegmentId {
        let id = SegmentId(self.len());
        self.segments.push_back(Segment {
            base,
            bytes: Some(Arc::new(bytes)),
            sealed,
            leases: Vec::new(),
            live_bytes: Arc::new(AtomicUsize::new(0)),
        });

        id
    }
}

impl<B: AsRef<[u8]>> Store<B> {
    /// Returns the total retained bytes across parked segments, saturating at `usize::MAX`.
    #[must_use]
    pub fn retained_len(&self) -> usize {
        self.segments
            .iter()
            .fold(0usize, |total, entry| total.saturating_add(entry.len()))
    }

    /// Returns the absolute span a still-retained segment covers.
    ///
    /// Returns `None` for unknown, retired, or unaddressable (base-plus-length overflow)
    /// segments.
    #[must_use]
    pub fn segment_span(&self, segment: SegmentId) -> Option<Span> {
        let entry = self.entry(segment)?;
        entry.bytes.as_ref()?;
        let len = u64::try_from(entry.len()).ok()?;
        let end = entry.base.checked_add(len)?;

        Some(Span::new(entry.base, end))
    }

    /// Returns bytes for `span` when one retained segment fully contains it.
    ///
    /// This is the zero-copy peek for owners and trusted callers; the borrow pins the store
    /// itself, not the segment. Spans crossing segment boundaries resolve to `None` — lease them
    /// through [`Self::leases`] or copy them through [`Self::copy_bytes`] instead. Empty spans
    /// (including reversed ones) resolve to empty bytes everywhere.
    #[must_use]
    pub fn bytes(&self, span: Span) -> Option<&[u8]> {
        if span.is_empty() {
            return Some(&[]);
        }

        self.segments.iter().find_map(|entry| {
            let range = entry.range_for(span)?;
            (**entry.bytes.as_ref()?).as_ref().get(range)
        })
    }

    /// Creates a lease for `span` when one retained segment fully contains it.
    ///
    /// The lease starts loaned: it reads from the retained segment and pins it until the lease is
    /// dropped, promoted, or the segment is retired. Empty spans (including reversed ones) yield
    /// an owned empty lease everywhere; it pins nothing.
    #[must_use]
    pub fn lease(&mut self, span: Span) -> Option<Lease<B>> {
        if span.is_empty() {
            return Some(Lease::empty());
        }

        let entry = self
            .segments
            .iter_mut()
            .find(|entry| entry.range_for(span).is_some())?;
        let range = entry.range_for(span)?;
        let bytes = Arc::clone(entry.bytes.as_ref()?);
        let (lease, registration) = Lease::loaned(bytes, range, Arc::clone(&entry.live_bytes));
        entry.register(registration);

        Some(lease)
    }

    /// Maps `span` onto retained segments as `(position, range)` pairs covering the whole span in
    /// park order. Positions index `self.segments` directly and are only valid until the next
    /// mutation.
    fn ranges(&self, span: Span) -> Option<Vec<(usize, Range<usize>)>> {
        if span.is_empty() {
            return Some(Vec::new());
        }

        let mut cursor = span.start;
        let mut ranges = Vec::new();

        for (position, entry) in self.segments.iter().enumerate() {
            if cursor >= span.end {
                break;
            }

            let Some(bytes) = entry.bytes.as_ref() else {
                continue;
            };
            let len = u64::try_from((**bytes).as_ref().len()).ok()?;
            let end = entry.base.checked_add(len)?;

            if cursor >= end || span.end <= entry.base {
                continue;
            }

            if cursor < entry.base {
                return None;
            }

            let stop = span.end.min(end);
            let start = usize::try_from(cursor.checked_sub(entry.base)?).ok()?;
            let stop_offset = usize::try_from(stop.checked_sub(entry.base)?).ok()?;
            ranges.push((position, start..stop_offset));
            cursor = stop;
        }

        (cursor == span.end).then_some(ranges)
    }

    /// Copies bytes for `span` into a vector, following spans across segment boundaries.
    ///
    /// This is the read-only counterpart of [`Self::leases`] for callers that want bytes without
    /// creating a handle. Returns `None` when any part of the span is not retained; empty spans
    /// (including reversed ones) yield an empty vector everywhere.
    #[must_use]
    pub fn copy_bytes(&self, span: Span) -> Option<Vec<u8>> {
        let ranges = self.ranges(span)?;
        let mut bytes = Vec::with_capacity(usize::try_from(span.len()).unwrap_or(0));

        for (position, range) in ranges {
            let entry = self.segments.get(position)?;
            bytes.extend_from_slice((**entry.bytes.as_ref()?).as_ref().get(range)?);
        }

        Some(bytes)
    }

    /// Creates leases covering `span` across consecutive retained segments.
    ///
    /// Spans that fit one segment yield a single lease. Returns `None` when any part of the span
    /// is not retained; empty spans (including reversed ones) yield an empty vector everywhere.
    #[must_use]
    pub fn leases(&mut self, span: Span) -> Option<Vec<Lease<B>>> {
        let ranges = self.ranges(span)?;
        let mut collected = Vec::with_capacity(ranges.len());

        for (position, range) in ranges {
            let entry = self.segments.get_mut(position)?;
            let bytes = Arc::clone(entry.bytes.as_ref()?);
            let (lease, registration) = Lease::loaned(bytes, range, Arc::clone(&entry.live_bytes));
            entry.register(registration);
            collected.push(lease);
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

        if entry.bytes.is_none() {
            return false;
        }

        for registration in &entry.leases {
            if let Some(slot) = registration.upgrade() {
                slot.promote();
            }
        }

        entry.leases = Vec::new();
        entry.bytes = None;
        self.compact();
        true
    }

    /// Retires every remaining segment, promoting all surviving leases.
    ///
    /// This is the owner's unconditional escape hatch: afterwards the store retains no bytes,
    /// regardless of seal state or waste thresholds. Returns the number of segments retired by
    /// this call.
    pub fn retire_all(&mut self) -> usize {
        (self.compacted..self.len())
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

        for (position, entry) in self.segments.iter_mut().enumerate() {
            entry.prune();

            if !entry.sealed || entry.bytes.is_none() {
                continue;
            }

            let live = entry.live_bytes.load(Ordering::Relaxed);

            if entry.len().saturating_sub(live) >= min_wasted_bytes {
                retirable.push(self.compacted.saturating_add(position));
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
            .finish()
    }
}

impl<B> Debug for Segment<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Segment")
            .field("base", &self.base)
            .field("retired", &self.bytes.is_none())
            .field("sealed", &self.sealed)
            .field("registered_leases", &self.leases.len())
            .field("live_bytes", &self.live_bytes.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
