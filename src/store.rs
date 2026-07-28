//! The owner side: retained segments, the lease registry, and retirement.

use crate::lease::{Lease, Slot};
use crate::span::Span;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

/// Identifier for a parked segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SegmentId(usize);

impl SegmentId {
    /// Returns the numeric segment index.
    #[inline]
    pub const fn index(self) -> usize {
        self.0
    }
}

#[derive(Debug)]
struct Segment<B> {
    base: u64,
    bytes: Option<Arc<B>>,
    sealed: bool,
    leases: Vec<Weak<Slot<B>>>,
    live_bytes: Arc<AtomicUsize>,
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
/// Segments are matched in park order; bases are expected to be ascending and non-overlapping,
/// as produced by parking consecutive buffers of one input stream.
#[derive(Debug)]
pub struct Store<B = Vec<u8>> {
    segments: Vec<Segment<B>>,
}

impl<B> Store<B> {
    /// Creates an empty store.
    #[inline]
    pub const fn new() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    /// Returns the number of parked segments, including retired ones.
    #[inline]
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// Returns `true` when no segments are parked.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Returns the live loaned bytes currently pinning `segment`.
    pub fn live_bytes(&self, segment: SegmentId) -> Option<usize> {
        self.segments
            .get(segment.index())
            .map(|entry| entry.live_bytes.load(Ordering::Relaxed))
    }

    /// Returns whether `segment` has been retired.
    pub fn is_retired(&self, segment: SegmentId) -> Option<bool> {
        self.segments
            .get(segment.index())
            .map(|entry| entry.bytes.is_none())
    }

    /// Marks an open segment as retirable by [`Self::maintain`].
    ///
    /// Returns `false` when the segment does not exist.
    pub fn seal(&mut self, segment: SegmentId) -> bool {
        let Some(entry) = self.segments.get_mut(segment.index()) else {
            return false;
        };

        entry.sealed = true;
        true
    }

    /// Parks a segment with its absolute base offset.
    ///
    /// The segment is sealed: [`Self::maintain`] may retire it as soon as its waste threshold is
    /// met. Use [`Self::park_open`] to defer that.
    pub fn park(&mut self, base: u64, bytes: B) -> SegmentId {
        self.insert(base, bytes, true)
    }

    /// Parks a segment that [`Self::maintain`] must leave alone until [`Self::seal`].
    ///
    /// Use this when parking ahead of attaching leases, so a maintenance sweep between the two
    /// steps cannot retire the still-unleased segment. Explicit [`Self::retire`] and
    /// [`Self::retire_all`] calls ignore the seal state.
    pub fn park_open(&mut self, base: u64, bytes: B) -> SegmentId {
        self.insert(base, bytes, false)
    }

    fn insert(&mut self, base: u64, bytes: B, sealed: bool) -> SegmentId {
        let id = SegmentId(self.segments.len());
        self.segments.push(Segment {
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
    /// Returns the total retained bytes across parked segments.
    pub fn retained_len(&self) -> usize {
        self.segments.iter().map(Segment::len).sum()
    }

    /// Returns bytes for `span` when one retained segment fully contains it.
    ///
    /// This is the zero-copy peek for owners and trusted callers; the borrow pins the store
    /// itself, not the segment. Spans crossing segment boundaries resolve to `None` — lease them
    /// through [`Self::leases`] instead.
    pub fn bytes(&self, span: Span) -> Option<&[u8]> {
        self.segments.iter().find_map(|entry| {
            let range = entry.range_for(span)?;
            (**entry.bytes.as_ref()?).as_ref().get(range)
        })
    }

    /// Creates a lease for `span` when one retained segment fully contains it.
    ///
    /// The lease starts loaned: it reads from the retained segment and pins it until the lease is
    /// dropped, promoted, or the segment is retired.
    pub fn lease(&mut self, span: Span) -> Option<Lease<B>> {
        let index = self
            .segments
            .iter()
            .position(|entry| entry.range_for(span).is_some())?;
        let entry = self.segments.get_mut(index)?;
        let range = entry.range_for(span)?;
        let bytes = Arc::clone(entry.bytes.as_ref()?);
        let (lease, registration) = Lease::loaned(bytes, range, Arc::clone(&entry.live_bytes));
        entry.leases.push(registration);

        Some(lease)
    }

    /// Creates leases covering `span` across consecutive retained segments.
    ///
    /// Spans that fit one segment yield a single lease. Returns `None` when any part of the span
    /// is not retained; an empty span yields an empty vector.
    pub fn leases(&mut self, span: Span) -> Option<Vec<Lease<B>>> {
        if span.end < span.start {
            return None;
        }

        let mut cursor = span.start;
        let mut collected = Vec::new();

        for index in 0..self.segments.len() {
            if cursor >= span.end {
                break;
            }

            let Some(entry) = self.segments.get_mut(index) else {
                break;
            };
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
            let (lease, registration) = Lease::loaned(
                Arc::clone(bytes),
                start..stop_offset,
                Arc::clone(&entry.live_bytes),
            );
            entry.leases.push(registration);
            collected.push(lease);
            cursor = stop;
        }

        (cursor == span.end).then_some(collected)
    }

    /// Retires `segment`: every surviving lease is promoted to its own copy in place, then the
    /// retained bytes are released.
    ///
    /// Returns `false` when the segment does not exist or was already retired. Plain
    /// [`Self::bytes`] lookups into the segment resolve to `None` afterwards.
    pub fn retire(&mut self, segment: SegmentId) -> bool {
        let Some(entry) = self.segments.get_mut(segment.index()) else {
            return false;
        };
        let Some(bytes) = entry.bytes.take() else {
            return false;
        };

        for registration in entry.leases.drain(..) {
            if let Some(slot) = registration.upgrade() {
                slot.promote();
            }
        }

        drop(bytes);
        true
    }

    /// Retires every remaining segment, promoting all surviving leases.
    ///
    /// This is the owner's unconditional escape hatch: afterwards the store retains no bytes,
    /// regardless of seal state or waste thresholds. Returns the number of segments retired by
    /// this call.
    pub fn retire_all(&mut self) -> usize {
        (0..self.segments.len())
            .filter(|&index| self.retire(SegmentId(index)))
            .count()
    }

    /// Retires sealed segments whose wasted bytes meet `min_wasted_bytes`, and prunes registry
    /// entries for dropped leases.
    ///
    /// Wasted bytes are the segment length minus its live loaned bytes, so a segment with no live
    /// leases wastes its entire length and a threshold of `0` retires every sealed segment. The
    /// check is O(1) per segment through the shared live-byte counters. Returns the number of
    /// segments retired.
    pub fn maintain(&mut self, min_wasted_bytes: usize) -> usize {
        let mut retirable = Vec::new();

        for (index, entry) in self.segments.iter_mut().enumerate() {
            entry.leases.retain(|lease| lease.strong_count() > 0);

            if !entry.sealed || entry.bytes.is_none() {
                continue;
            }

            let live = entry.live_bytes.load(Ordering::Relaxed);

            if entry.len().saturating_sub(live) >= min_wasted_bytes {
                retirable.push(index);
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

#[cfg(test)]
mod tests;
