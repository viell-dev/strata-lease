//! Promotable lease handles into retained segments.

use std::fmt::{self, Debug, Formatter};
use std::ops::{Deref, Range};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

/// Shared state behind one lease and all of its clones.
///
/// The pin on the source segment is the `Arc<B>` inside `source`. Exactly one path releases it:
/// whoever takes the `Option` out of the mutex also subtracts the range length from the shared
/// live-byte counter. `owned` is only ever initialized while that mutex is held, so a live slot
/// always has a source, an owned copy, or both mid-promotion — never neither.
pub(crate) struct Slot<B> {
    owned: OnceLock<Box<[u8]>>,
    source: Mutex<Option<(Arc<B>, Range<usize>)>>,
    live_bytes: Arc<AtomicUsize>,
}

impl<B> Slot<B> {
    fn lock_source(&self) -> MutexGuard<'_, Option<(Arc<B>, Range<usize>)>> {
        self.source.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<B: AsRef<[u8]>> Slot<B> {
    /// Copies the loaned range into owned storage and releases the pin on the source segment.
    ///
    /// Idempotent and race-safe: promotion happens under the source lock, and only the caller
    /// that takes the source decrements the live-byte counter.
    pub(crate) fn promote(&self) {
        let mut source = self.lock_source();
        let Some((segment, range)) = source.take() else {
            return;
        };

        let bytes = (*segment).as_ref().get(range.clone()).unwrap_or(&[]);
        /* Setting can only fail if `owned` is already initialized, which cannot happen: every
        setter first takes the source out of the mutex, and it was still present above. */
        let _ = self.owned.set(Box::from(bytes));
        drop(source);
        self.live_bytes.fetch_sub(range.len(), Ordering::Relaxed);
    }
}

impl<B> Drop for Slot<B> {
    fn drop(&mut self) {
        let source = self
            .source
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);

        if let Some((_, range)) = source.take() {
            self.live_bytes.fetch_sub(range.len(), Ordering::Relaxed);
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
        live_bytes.fetch_add(range.len(), Ordering::Relaxed);
        let slot = Arc::new(Slot {
            owned: OnceLock::new(),
            source: Mutex::new(Some((segment, range))),
            live_bytes,
        });
        let registration = Arc::downgrade(&slot);

        (Self { slot }, registration)
    }

    /// Returns whether the lease holds its own copy of the bytes.
    pub fn is_owned(&self) -> bool {
        self.slot.owned.get().is_some()
    }

    /// Returns whether the lease still reads from the retained segment.
    pub fn is_loaned(&self) -> bool {
        !self.is_owned()
    }
}

impl<B: AsRef<[u8]>> Lease<B> {
    /// Returns the number of bytes the lease covers.
    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    /// Returns `true` when the lease covers no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the lease bytes without copying, whether loaned or owned.
    ///
    /// While the returned guard is alive for a loaned lease, promotion of this lease (including a
    /// segment retirement that walks over it) blocks; drop the guard promptly.
    pub fn bytes(&self) -> LeaseBytes<'_, B> {
        if let Some(owned) = self.slot.owned.get() {
            return LeaseBytes {
                inner: Inner::Owned(owned),
            };
        }

        let source = self.slot.lock_source();

        if source.is_some() {
            return LeaseBytes {
                inner: Inner::Loaned(source),
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
    /// the [`Store`](crate::Store).
    pub fn promote(&self) {
        self.slot.promote();
    }

    /// Copies the current lease bytes into a vector.
    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes().to_vec()
    }
}

/// Zero-copy read guard returned by [`Lease::bytes`].
///
/// Dereferences to the lease's byte slice in both lease states.
pub struct LeaseBytes<'a, B = Vec<u8>> {
    inner: Inner<'a, B>,
}

enum Inner<'a, B> {
    Owned(&'a [u8]),
    Loaned(MutexGuard<'a, Option<(Arc<B>, Range<usize>)>>),
}

impl<B: AsRef<[u8]>> Deref for LeaseBytes<'_, B> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.inner {
            Inner::Owned(bytes) => bytes,
            Inner::Loaned(source) => source
                .as_ref()
                .and_then(|(segment, range)| (**segment).as_ref().get(range.clone()))
                .unwrap_or(&[]),
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
