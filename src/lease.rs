//! Promotable lease handles into retained segments.
//!
//! A [`Lease`] is the consumer side of the crate: a cheap, cloneable handle that reads from a
//! segment retained by a [`Store`](crate::Store) until it is *promoted* to its own copy, either by
//! the consumer or by the store when it retires the segment. [`Lease::bytes`] reads uniformly in
//! both states through a [`LeaseBytes`] guard.
//!
//! # Example
//!
//! ```
//! use strata_lease::{Span, Store};
//!
//! let mut store = Store::new();
//! store.park(0, b"key=value\n".to_vec()).unwrap();
//! let lease = store.lease(Span::new(4, 9).unwrap()).unwrap();
//!
//! // Loaned: reads straight from the retained segment
//! assert!(lease.is_loaned());
//! assert_eq!(&*lease.bytes(), b"value");
//!
//! // Owned: reads from a private copy, and the segment is no longer pinned
//! lease.promote();
//! assert!(lease.is_owned());
//! assert_eq!(&*lease.bytes(), b"value");
//! ```

use crate::span::SegmentRange;
use std::fmt::{self, Debug, Formatter};
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

/// Adds `len` pinned bytes to a segment's live-byte counter, saturating at `usize::MAX`.
///
/// The live-byte counters are advisory: they only steer [`Store::maintain`](crate::Store::maintain)
/// decisions. Increments happen on the owner side under `&mut Store` and decrements on arbitrary
/// consumer threads, so `Relaxed` suffices: a stale or saturated value only makes maintenance
/// conservative (a segment is retained longer than strictly necessary), never unsound. Saturation
/// instead of wrapping keeps a pathological aggregate (overlapping leases summing past
/// `usize::MAX`) from corrupting the counter into premature retirement.
fn pin(live_bytes: &AtomicUsize, len: usize) {
    let _ = live_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
        Some(live.saturating_add(len))
    });
}

/// Removes `len` pinned bytes from a segment's live-byte counter, saturating at zero.
///
/// Saturation is sticky: once the counter has clamped at `usize::MAX`, part of the pinned total
/// has been lost, and subtracting from the clamped value could drive the counter *below* the true
/// total. That would turn the advisory error non-conservative and retire segments whose bytes are
/// still loaned out. A saturated counter therefore stays saturated; the affected segment is
/// simply never retired by [`Store::maintain`](crate::Store::maintain), while explicit retirement
/// is unaffected.
fn unpin(live_bytes: &AtomicUsize, len: usize) {
    let _ = live_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
        (live != usize::MAX).then(|| live.saturating_sub(len))
    });
}

/// A retained segment's shared allocation: the buffer plus its live-byte counter.
///
/// The counter lives inside the same allocation as the bytes it accounts for, so a lease can
/// never be wired to one segment's bytes and another segment's counter, and pinning a segment is
/// one `Arc` instead of two.
pub(crate) struct Backing<B> {
    /// The parked buffer.
    pub(crate) bytes: B,
    /// Bytes currently pinned by loaned leases, counted with multiplicity.
    pub(crate) live_bytes: AtomicUsize,
}

/// Shared state behind one lease and all of its clones.
///
/// The pin on the source segment is the `Arc<Backing<B>>` inside `source`. Exactly one path
/// releases it: whoever takes the `Option` out of the mutex also subtracts the range length from
/// the backing's live-byte counter. `owned` is only ever initialized while that mutex is held and
/// *before* the source is taken, so a live slot always has a source, an owned copy, or both
/// mid-promotion, never neither, even when a buffer's `as_ref` panics mid-copy.
pub(crate) struct Slot<B> {
    /// The promoted copy, set exactly once.
    owned: OnceLock<Arc<[u8]>>,
    /// The pinned segment and the range within it, until promotion takes them.
    source: Mutex<Option<(Arc<Backing<B>>, SegmentRange)>>,
}

impl<B> Slot<B> {
    /// Locks the source, recovering from poisoning.
    ///
    /// The only code that runs under this lock is the buffer's `as_ref` and an allocation, so a
    /// poisoned lock means a buffer implementation panicked mid-promotion. The slot state is still
    /// consistent then (see the type documentation), so continuing is safe.
    fn lock_source(&self) -> MutexGuard<'_, Option<(Arc<Backing<B>>, SegmentRange)>> {
        self.source.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns whether this slot already holds its own copy of the bytes.
    ///
    /// A promoted slot no longer needs owner-side promotion, so the store prunes it from the
    /// lease registry even while consumers still hold it.
    #[inline]
    pub(crate) fn is_promoted(&self) -> bool {
        self.owned.get().is_some()
    }
}

impl<B: AsRef<[u8]>> Slot<B> {
    /// Copies the loaned range into owned storage and releases the pin on the source segment.
    ///
    /// Idempotent and race-safe: promotion happens under the source lock, and only the caller
    /// that takes the source decrements the live-byte counter. The copy happens *before* the
    /// source is taken, so a panicking buffer `as_ref` leaves the lease loaned and fully
    /// functional instead of silently empty.
    pub(crate) fn promote(&self) {
        let mut source = self.lock_source();
        let Some((backing, range)) = source.as_ref() else {
            return;
        };

        let bytes = backing.bytes.as_ref().get(range.bounds()).unwrap_or(&[]);
        /* Setting can only fail if `owned` is already initialized, which cannot happen: every
        setter holds the source lock and requires the source to still be present, and it was
        still present above. */
        let _ = self.owned.set(Arc::from(bytes));

        let Some((backing, range)) = source.take() else {
            return;
        };
        drop(source);
        unpin(&backing.live_bytes, range.len());
    }
}

impl<B> Drop for Slot<B> {
    fn drop(&mut self) {
        let source = self
            .source
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);

        if let Some((backing, range)) = source.take() {
            unpin(&backing.live_bytes, range.len());
        }
    }
}

/// A promotable zero-copy view into a segment retained by a [`Store`](crate::Store).
///
/// A lease is either *loaned*, reading directly from the retained segment and pinning it through
/// shared ownership, or *owned*, holding its own copy of the bytes. Consumers read through
/// [`Lease::bytes`] in both states. Promotion happens in place, either from the consumer side via
/// [`Lease::promote`] or from the owner side when the store retires the segment. Clones share one
/// state and one pin, and dropping the last clone of a loaned lease releases its pin.
///
/// # Thread safety
///
/// `Lease<B>` is `Send` and `Sync` exactly when `B` is `Send + Sync` (which `Vec<u8>` and other
/// standard buffer types are). Reads, clones, promotion, and store-side retirement may race
/// freely; every interleaving yields the same bytes.
///
/// # Trait semantics
///
/// Cloning shares the lease state: a promotion through any clone is visible through every clone,
/// and the clones together hold one pin on the segment.
///
/// The `Debug` output reports the promotion state only; the bytes are not printed.
pub struct Lease<B = Vec<u8>> {
    /// The state shared by every clone of this lease.
    slot: Arc<Slot<B>>,
}

impl<B> Clone for Lease<B> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            slot: Arc::clone(&self.slot),
        }
    }
}

impl<B> Debug for Lease<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("owned", &self.is_owned())
            .finish_non_exhaustive()
    }
}

impl<B> Lease<B> {
    /// Creates a loaned lease pinning `range` of `backing`, plus the registration the store
    /// keeps to find the lease again at retirement.
    pub(crate) fn loaned(backing: Arc<Backing<B>>, range: SegmentRange) -> (Self, Weak<Slot<B>>) {
        pin(&backing.live_bytes, range.len());
        let slot = Arc::new(Slot {
            owned: OnceLock::new(),
            source: Mutex::new(Some((backing, range))),
        });
        let registration = Arc::downgrade(&slot);

        (Self { slot }, registration)
    }

    /// Returns whether the lease holds its own copy of the bytes.
    ///
    /// This is a snapshot: under a concurrent promotion it can report `true` an instant before
    /// the pin on the segment is released. Treat it as a hint, not a synchronization point.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, b"owned".to_vec()).unwrap();
    /// let lease = store.lease(Span::new(0, 5).unwrap()).unwrap();
    ///
    /// assert!(!lease.is_owned());
    /// lease.promote();
    /// assert!(lease.is_owned());
    /// ```
    #[inline]
    pub fn is_owned(&self) -> bool {
        self.slot.is_promoted()
    }

    /// Returns whether the lease still reads from the retained segment.
    ///
    /// The inverse of [`Lease::is_owned`], with the same snapshot caveat.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, b"loaned".to_vec()).unwrap();
    /// let lease = store.lease(Span::new(0, 6).unwrap()).unwrap();
    ///
    /// assert!(lease.is_loaned());
    /// store.retire_all();
    /// assert!(!lease.is_loaned());
    /// ```
    #[inline]
    pub fn is_loaned(&self) -> bool {
        !self.is_owned()
    }
}

impl<B: AsRef<[u8]>> Lease<B> {
    /// Returns the number of bytes the lease covers.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, b"key=value\n".to_vec()).unwrap();
    /// let lease = store.lease(Span::new(4, 9).unwrap()).unwrap();
    ///
    /// assert_eq!(lease.len(), 5);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    /// Returns `true` when the lease covers no bytes.
    ///
    /// A lease created from a [`Span`](crate::Span) always covers at least one byte, so this
    /// only reports `true` when the parked buffer violated the stable-buffer contract and shrank
    /// underneath a loaned lease (see [`Store`](crate::Store)).
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, b"key=value\n".to_vec()).unwrap();
    /// let lease = store.lease(Span::new(4, 9).unwrap()).unwrap();
    ///
    /// assert!(!lease.is_empty());
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the lease bytes without copying, whether loaned or owned.
    ///
    /// The returned guard is a snapshot: for a loaned lease it takes its own pin on the retained
    /// segment, so reads never block promotion or retirement, not even on the same thread, and
    /// guards nest freely. A guard created before a promotion or retirement keeps reading the
    /// original allocation until it is dropped, with bytes identical to the promoted copy.
    ///
    /// Because a loaned guard keeps the whole segment allocation alive on its own, drop it once
    /// the read is done rather than storing it long-term; the lease itself is the cheap handle to
    /// keep.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, b"key=value\n".to_vec()).unwrap();
    /// let lease = store.lease(Span::new(4, 9).unwrap()).unwrap();
    ///
    /// // The guard dereferences to the byte slice
    /// let bytes = lease.bytes();
    /// assert_eq!(&*bytes, b"value");
    /// assert_eq!(bytes.first(), Some(&b'v'));
    ///
    /// // Guards nest, and stay valid across promotion
    /// let again = lease.bytes();
    /// lease.promote();
    /// assert_eq!(&*bytes, b"value");
    /// assert_eq!(&*again, b"value");
    /// ```
    pub fn bytes(&self) -> LeaseBytes<B> {
        if let Some(owned) = self.slot.owned.get() {
            return LeaseBytes {
                inner: Inner::Owned(Arc::clone(owned)),
            };
        }

        let source = self.slot.lock_source();

        if let Some((backing, range)) = source.as_ref() {
            return LeaseBytes {
                inner: Inner::Loaned {
                    backing: Arc::clone(backing),
                    range: *range,
                },
            };
        }

        drop(source);

        /* The source was already released, so the promotion that released it has completed and
        `owned` is initialized; the empty fallback is unreachable. */
        LeaseBytes {
            inner: Inner::Owned(self.slot.owned.get().cloned().unwrap_or_default()),
        }
    }

    /// Copies the lease bytes into their own storage and releases the pin on the segment.
    ///
    /// Promotion is idempotent, affects every clone of this lease, and does not require access to
    /// the [`Store`](crate::Store). It is safe to call while [`LeaseBytes`] guards are alive,
    /// including guards on this same lease from this same thread; existing guards keep reading
    /// the original allocation until dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// let id = store.park(0, b"key=value\n".to_vec()).unwrap();
    /// let lease = store.lease(Span::new(4, 9).unwrap()).unwrap();
    /// assert_eq!(store.live_bytes(id), Some(5));
    ///
    /// // The consumer takes its copy early; the segment is no longer pinned
    /// lease.promote();
    /// assert!(lease.is_owned());
    /// assert_eq!(store.live_bytes(id), Some(0));
    /// assert_eq!(&*lease.bytes(), b"value");
    ///
    /// // Promoting again is a no-op
    /// lease.promote();
    /// assert_eq!(&*lease.bytes(), b"value");
    /// ```
    #[inline]
    pub fn promote(&self) {
        self.slot.promote();
    }

    /// Copies the current lease bytes into a vector.
    ///
    /// This does not change the lease state; use [`Lease::promote`] to release the pin.
    ///
    /// # Examples
    ///
    /// ```
    /// # use strata_lease::{Span, Store};
    /// let mut store = Store::new();
    /// store.park(0, b"key=value\n".to_vec()).unwrap();
    /// let lease = store.lease(Span::new(4, 9).unwrap()).unwrap();
    ///
    /// assert_eq!(lease.to_vec(), b"value");
    /// assert!(lease.is_loaned());
    /// ```
    #[inline]
    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes().to_vec()
    }
}

/// Zero-copy read guard returned by [`Lease::bytes`].
///
/// Dereferences to the lease's byte slice in both lease states. The guard is a self-contained
/// snapshot, a pin on the segment allocation for a loaned lease or a share of the promoted copy
/// for an owned one (an `Arc` clone either way, never a borrow or a lock), so it stays valid
/// across promotion and retirement, and even outlives the lease and the store.
///
/// # Trait semantics
///
/// The `Debug` output is that of the byte slice.
pub struct LeaseBytes<B = Vec<u8>> {
    /// The allocation the guard reads from.
    inner: Inner<B>,
}

/// The allocation a [`LeaseBytes`] guard pins.
enum Inner<B> {
    /// A share of the promoted copy.
    Owned(Arc<[u8]>),
    /// A pin on the retained segment and the range within it.
    Loaned {
        backing: Arc<Backing<B>>,
        range: SegmentRange,
    },
}

impl<B: AsRef<[u8]>> Deref for LeaseBytes<B> {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        match &self.inner {
            Inner::Owned(bytes) => bytes,
            Inner::Loaned { backing, range } => {
                backing.bytes.as_ref().get(range.bounds()).unwrap_or(&[])
            }
        }
    }
}

impl<B: AsRef<[u8]>> AsRef<[u8]> for LeaseBytes<B> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl<B: AsRef<[u8]>> Debug for LeaseBytes<B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(&**self, f)
    }
}

#[cfg(test)]
mod tests;
