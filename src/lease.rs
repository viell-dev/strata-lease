//! Promotable lease handles into retained segments.

use std::fmt::{self, Debug, Formatter};
use std::ops::{Deref, Range};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

/// Adds `len` pinned bytes to a segment's live-byte counter, saturating at `usize::MAX`.
///
/// The live-byte counters are advisory: they only steer [`Store::maintain`](crate::Store)
/// decisions. Increments happen on the owner thread under `&mut Store` and decrements on
/// arbitrary consumer threads, so `Relaxed` suffices: a stale or saturated value only makes
/// maintenance conservative (a segment is retained longer than strictly necessary), never
/// unsound. Saturation instead of wrapping keeps a pathological aggregate (overlapping leases
/// summing past `usize::MAX`) from corrupting the counter into premature retirement.
fn pin(live_bytes: &AtomicUsize, len: usize) {
    let _ = live_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
        Some(live.saturating_add(len))
    });
}

/// Removes `len` pinned bytes from a segment's live-byte counter, saturating at zero.
fn unpin(live_bytes: &AtomicUsize, len: usize) {
    let _ = live_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
        Some(live.saturating_sub(len))
    });
}

/// Shared state behind one lease and all of its clones.
///
/// The pin on the source segment is the `Arc<B>` inside `source`. Exactly one path releases it:
/// whoever takes the `Option` out of the mutex also subtracts the range length from the shared
/// live-byte counter. `owned` is only ever initialized while that mutex is held and *before* the
/// source is taken, so a live slot always has a source, an owned copy, or both mid-promotion —
/// never neither, even when a buffer's `as_ref` panics mid-copy.
pub(crate) struct Slot<B> {
    owned: OnceLock<Box<[u8]>>,
    source: Mutex<Option<(Arc<B>, Range<usize>)>>,
    live_bytes: Arc<AtomicUsize>,
}

impl<B> Slot<B> {
    fn lock_source(&self) -> MutexGuard<'_, Option<(Arc<B>, Range<usize>)>> {
        self.source.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Returns whether this slot already holds its own copy of the bytes.
    ///
    /// A promoted slot no longer needs owner-side promotion, so the store prunes it from the
    /// lease registry even while consumers still hold it.
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
        let Some((segment, range)) = source.as_ref() else {
            return;
        };

        let bytes = (**segment).as_ref().get(range.clone()).unwrap_or(&[]);
        /* Setting can only fail if `owned` is already initialized, which cannot happen: every
        setter holds the source lock and requires the source to still be present, and it was
        still present above. */
        let _ = self.owned.set(Box::from(bytes));

        let Some((_, range)) = source.take() else {
            return;
        };
        drop(source);
        unpin(&self.live_bytes, range.len());
    }
}

impl<B> Drop for Slot<B> {
    fn drop(&mut self) {
        let source = self
            .source
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);

        if let Some((_, range)) = source.take() {
            unpin(&self.live_bytes, range.len());
        }
    }
}

/// A promotable zero-copy view into a segment retained by a [`Store`](crate::Store).
///
/// A lease is either *loaned* — it reads directly from the retained segment and pins it through
/// shared ownership — or *owned* — it holds its own copy of the bytes. Consumers read through
/// [`Lease::bytes`] in both states. Promotion happens in place, either from the consumer side via
/// [`Lease::promote`] or from the owner side when the store retires the segment. Clones share one
/// state and one pin, and dropping the last clone of a loaned lease releases its pin.
///
/// # Thread safety
///
/// `Lease<B>` is `Send` and `Sync` exactly when `B` is `Send + Sync` (which `Vec<u8>` and other
/// standard buffer types are). Reads, clones, promotion, and store-side retirement may race
/// freely; every interleaving yields the same bytes.
pub struct Lease<B = Vec<u8>> {
    slot: Arc<Slot<B>>,
}

impl<B> Clone for Lease<B> {
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
    /// Creates a loaned lease pinning `range` of `segment`, plus the registration the store keeps
    /// to find the lease again at retirement.
    pub(crate) fn loaned(
        segment: Arc<B>,
        range: Range<usize>,
        live_bytes: Arc<AtomicUsize>,
    ) -> (Self, Weak<Slot<B>>) {
        pin(&live_bytes, range.len());
        let slot = Arc::new(Slot {
            owned: OnceLock::new(),
            source: Mutex::new(Some((segment, range))),
            live_bytes,
        });
        let registration = Arc::downgrade(&slot);

        (Self { slot }, registration)
    }

    /// Creates a lease that starts owned with no bytes and pins nothing.
    ///
    /// Empty spans resolve to this: there is nothing to loan, so the lease never holds a pin on
    /// any segment.
    pub(crate) fn empty() -> Self {
        let owned = OnceLock::new();
        let _ = owned.set(Box::from(&[][..]));

        Self {
            slot: Arc::new(Slot {
                owned,
                source: Mutex::new(None),
                live_bytes: Arc::new(AtomicUsize::new(0)),
            }),
        }
    }

    /// Returns whether the lease holds its own copy of the bytes.
    ///
    /// This is a snapshot: under a concurrent promotion it can report `true` an instant before
    /// the pin on the segment is released. Treat it as a hint, not a synchronization point.
    #[must_use]
    pub fn is_owned(&self) -> bool {
        self.slot.owned.get().is_some()
    }

    /// Returns whether the lease still reads from the retained segment.
    ///
    /// The inverse of [`Lease::is_owned`], with the same snapshot caveat.
    #[must_use]
    pub fn is_loaned(&self) -> bool {
        !self.is_owned()
    }
}

impl<B: AsRef<[u8]>> Lease<B> {
    /// Returns the number of bytes the lease covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    /// Returns `true` when the lease covers no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the lease bytes without copying, whether loaned or owned.
    ///
    /// The returned guard is a snapshot: for a loaned lease it takes its own pin on the retained
    /// segment, so reads never block promotion or retirement — not even on the same thread — and
    /// guards nest freely. A guard created before a promotion or retirement keeps reading the
    /// original allocation until it is dropped, with bytes identical to the promoted copy.
    ///
    /// Because a loaned guard keeps the whole segment allocation alive on its own, drop it once
    /// the read is done rather than storing it long-term; the lease itself is the cheap handle to
    /// keep.
    #[must_use]
    pub fn bytes(&self) -> LeaseBytes<'_, B> {
        if let Some(owned) = self.slot.owned.get() {
            return LeaseBytes {
                inner: Inner::Owned(owned),
            };
        }

        let source = self.slot.lock_source();

        if let Some((segment, range)) = source.as_ref() {
            return LeaseBytes {
                inner: Inner::Loaned {
                    segment: Arc::clone(segment),
                    range: range.clone(),
                },
            };
        }

        drop(source);

        /* The source was already released, so the promotion that released it has completed and
        `owned` is initialized; the empty fallback is unreachable. */
        LeaseBytes {
            inner: Inner::Owned(self.slot.owned.get().map_or(&[][..], |bytes| bytes)),
        }
    }

    /// Copies the lease bytes into their own storage and releases the pin on the segment.
    ///
    /// Promotion is idempotent, affects every clone of this lease, and does not require access to
    /// the [`Store`](crate::Store). It is safe to call while [`LeaseBytes`] guards are alive,
    /// including guards on this same lease from this same thread; existing guards keep reading
    /// the original allocation until dropped.
    pub fn promote(&self) {
        self.slot.promote();
    }

    /// Copies the current lease bytes into a vector.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes().to_vec()
    }
}

/// Zero-copy read guard returned by [`Lease::bytes`].
///
/// Dereferences to the lease's byte slice in both lease states. For a loaned lease the guard
/// holds its own pin on the segment allocation (an `Arc` clone, not a lock), so it stays valid —
/// and keeps that allocation alive — across promotion, retirement, and even dropping the lease
/// and the store.
pub struct LeaseBytes<'a, B = Vec<u8>> {
    inner: Inner<'a, B>,
}

enum Inner<'a, B> {
    Owned(&'a [u8]),
    Loaned {
        segment: Arc<B>,
        range: Range<usize>,
    },
}

impl<B: AsRef<[u8]>> Deref for LeaseBytes<'_, B> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.inner {
            Inner::Owned(bytes) => bytes,
            Inner::Loaned { segment, range } => {
                (**segment).as_ref().get(range.clone()).unwrap_or(&[])
            }
        }
    }
}

impl<B: AsRef<[u8]>> AsRef<[u8]> for LeaseBytes<'_, B> {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl<B: AsRef<[u8]>> Debug for LeaseBytes<'_, B> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Debug::fmt(&**self, f)
    }
}

#[cfg(test)]
mod tests;
