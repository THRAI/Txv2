//! Identity table slot storing retained `Cap<T>` evidence.
//!
//! Per `object_model_v2.md` §7.2, identity slots in namespace containers store
//! `Cap<Identity>`.  They promise addressability: as long as the slot is
//! populated, the identity is reachable through the stored `Cap`.
//!
//! `IdentitySlot<T>` is the canonical container type for identity-table
//! entries.  It is `#[repr(transparent)]` over `Cap<T>` so that table lookups
//! incur no indirection overhead.

use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::Deref;

use super::cap::{Cap, Weak};

/// Identity table slot storing a retained `Cap<T>`.
///
/// # Obligation guarantee
///
/// Identity slots enforce the `OBL-*` addressability invariant at the type
/// level: an identity table stores `IdentitySlot<T>`, which can only hold a
/// `Cap<T>` (strong retention).  A resolution-only cache stores `Weak<T>` or
/// `()`.  These are different types, so the compiler rejects accidental
/// weak-evidence storage in an addressability binding.
///
/// # Examples
///
/// ```ignore
/// struct MountTableEntry {
///     parent_payload_ptr: usize,
///     child_fs_object_id: FsObjectId,
///     mount: IdentitySlot<MountIdentity>,
/// }
///
/// let cap: Cap<MountIdentity> = sign(MountIdentity::new(...))?;
/// let slot = IdentitySlot::from_cap(cap);
/// let weak = slot.downgrade();
/// ```
#[repr(transparent)]
pub struct IdentitySlot<T: 'static> {
    cap: Cap<T>,
}

// Safety: IdentitySlot delegates to Cap, which is Send+Sync when T is.
unsafe impl<T: Send + Sync + 'static> Send for IdentitySlot<T> {}
unsafe impl<T: Send + Sync + 'static> Sync for IdentitySlot<T> {}

impl<T: 'static> IdentitySlot<T> {
    /// Wrap a `Cap<T>` into an identity table slot.
    #[inline]
    pub fn from_cap(cap: Cap<T>) -> Self {
        Self { cap }
    }

    /// Borrow the stored `Cap<T>`.
    #[inline]
    pub fn cap(&self) -> &Cap<T> {
        &self.cap
    }

    /// Clone the stored `Cap<T>`, incrementing the retain count.
    #[inline]
    pub fn clone_cap(&self) -> Cap<T> {
        self.cap.clone()
    }

    /// Produce a non-retaining `Weak<T>` hint for guard-scoped observation.
    #[inline]
    pub fn downgrade(&self) -> Weak<T> {
        self.cap.downgrade()
    }

    /// Consume the slot and return the inner `Cap<T>`.
    #[inline]
    pub fn into_cap(self) -> Cap<T> {
        self.cap
    }
}

// ---------------------------------------------------------------------------
// Standard trait impls — delegate to the inner Cap.
// ---------------------------------------------------------------------------

impl<T: 'static> Deref for IdentitySlot<T> {
    type Target = Cap<T>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.cap
    }
}

impl<T: 'static> Clone for IdentitySlot<T> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            cap: self.cap.clone(),
        }
    }
}

impl<T: 'static> PartialEq for IdentitySlot<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.cap == other.cap
    }
}

impl<T: 'static> Eq for IdentitySlot<T> {}

impl<T: 'static> Hash for IdentitySlot<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.cap.key().hash(state);
    }
}

impl<T: 'static> fmt::Debug for IdentitySlot<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentitySlot")
            .field("key", &self.cap.key())
            .finish()
    }
}

impl<T: 'static> From<Cap<T>> for IdentitySlot<T> {
    #[inline]
    fn from(cap: Cap<T>) -> Self {
        Self::from_cap(cap)
    }
}
