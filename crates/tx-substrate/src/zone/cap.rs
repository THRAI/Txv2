//! Retained and weak references to zone slots.
//!
//! `Cap<T>` owns identity retention. `Weak<T>` is only a generation-checked
//! lookup hint. `IdentRef<'g, T>` is a guard-scoped borrowed observation that
//! can be upgraded into a `Cap<T>` only while the slot is still live.

use core::hash::{Hash, Hasher};
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use crate::epoch::{self, Guard};

use super::meta::{SlotState, RETAIN_SENTINEL_DEAD};
use super::policy::IsPayloadPolicy;
use super::registry::{self, SlotKey};
use super::slot::{reclaim_slot, Slot};
use super::{Dead, ZoneAllocated};

pub struct Cap<T: 'static> {
    /// Compact `zone_id + slot_id` encoding. `Cap` does not store generation
    /// because its retention prevents the slot from being reused.
    pub(crate) raw: u32,
    _marker: PhantomData<T>,
}

/// Payload evidence currently shares the same retention mechanics as `Cap`.
///
/// The wrapper keeps the public surface ready for split identity/payload
/// entities even though this slice uses the same underlying slot state.
pub struct PayloadCap<T: 'static> {
    inner: Cap<T>,
}

unsafe impl<T: Send + Sync> Send for Cap<T> {}
unsafe impl<T: Send + Sync> Sync for Cap<T> {}

impl<T: 'static> Cap<T> {
    pub(crate) fn try_retire_slot(slot: NonNull<Slot<T>>) {
        let meta = unsafe { slot.as_ref().meta() };
        loop {
            let cur = meta.load(Ordering::Acquire);
            if cur.retain() != RETAIN_SENTINEL_DEAD {
                return;
            }
            if cur.state() != SlotState::Dead {
                return;
            }

            // The sentinel blocks new upgrades. Moving to Retiring hands the
            // slot to EBR; only the EBR callback may run T's destructor and
            // return the slot to the Keg.
            let new = cur.with_retain(0).with_state(SlotState::Retiring);
            match meta.compare_exchange(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    let retire_result =
                        unsafe { epoch::retire_raw(slot.as_ptr() as *mut u8, reclaim_slot::<T>) };
                    if retire_result.is_ok() {
                        return;
                    }

                    let _ = epoch::drain_with_budget(64);
                    unsafe { epoch::retire_raw(slot.as_ptr() as *mut u8, reclaim_slot::<T>) }
                        .expect(
                            "zone slot retire enqueue failed after bounded drain; \
                             five-state zone design fail-fasts on retired-node pool exhaustion",
                        );
                    return;
                }
                Err(_) => continue,
            }
        }
    }

    pub(crate) unsafe fn from_slot(slot: NonNull<Slot<T>>) -> Self {
        let key = unsafe { slot.as_ref().key() };
        Self {
            raw: key.raw(),
            _marker: PhantomData,
        }
    }

    fn slot(&self) -> Option<NonNull<Slot<T>>> {
        registry::slot_for::<T>(self.key())
    }

    pub fn downgrade(&self) -> Weak<T> {
        let slot = self
            .slot()
            .expect("zone Cap key no longer resolves to a live slot");
        let meta = unsafe { slot.as_ref().meta() };
        let cur = meta.load(Ordering::Acquire);
        debug_assert_eq!(cur.state(), SlotState::Live);
        Weak {
            raw: self.raw,
            generation: cur.generation(),
            _marker: PhantomData,
        }
    }

    pub fn ident_ref<'g>(&self, guard: &'g Guard<'_>) -> IdentRef<'g, T> {
        let _ = guard;
        let slot = self
            .slot()
            .expect("zone Cap key no longer resolves to a live slot");
        let meta = unsafe { slot.as_ref().meta() };
        let cur = meta.load(Ordering::Acquire);
        debug_assert_eq!(cur.state(), SlotState::Live);
        IdentRef {
            slot,
            raw: self.raw,
            generation: cur.generation(),
            _guard: PhantomData,
        }
    }

    pub fn key(&self) -> SlotKey {
        SlotKey::from_raw(self.raw)
    }

    pub fn raw(&self) -> u32 {
        self.raw
    }

    /// Packed trace object id for observation payloads (OBS-4).
    ///
    /// Bit layout:
    /// ```text
    /// bits  0..32  — slot index (zone_id encoded in upper 8 of the 32 bits,
    ///                slot_id in the lower 24)
    /// bits 32..56  — generation (u16 from slot metadata, zero-extended)
    /// bits 56..64  — kind discriminant: FNV-1a hash of TypeId<T>, low 8 bits
    /// ```
    ///
    /// This accessor is the kernel-side mechanism that downstream payloads will
    /// use to refer to objects by identity.  Only the accessor is added in
    /// OBS-4; populating payloads with it is OBS-7/OBS-8 territory.
    ///
    /// # Note
    ///
    /// Reads the slot's generation from the live atomic metadata word.  The
    /// read is `Acquire`-ordered consistent with the rest of the cap API.
    pub fn trace_id(&self) -> u64 {
        let generation = self
            .slot()
            .map(|s| unsafe { s.as_ref().meta().load(Ordering::Acquire).generation() })
            .unwrap_or(0) as u64;
        let kind = cap_kind_byte::<T>() as u64;
        (kind << 56) | (generation << 32) | (self.raw as u64)
    }

    pub fn retain_count(&self) -> u32 {
        let Some(slot) = self.slot() else {
            debug_assert!(false, "zone Cap key no longer resolves to a slot");
            return 0;
        };
        unsafe { slot.as_ref().meta().load(Ordering::Acquire).retain() }
    }

    fn try_retire(slot: NonNull<Slot<T>>) {
        Self::try_retire_slot(slot);
    }
}

impl<T: 'static> Clone for Cap<T> {
    fn clone(&self) -> Self {
        let slot = self
            .slot()
            .expect("zone Cap key no longer resolves to a live slot");
        let meta = unsafe { slot.as_ref().meta() };
        loop {
            let cur = meta.load(Ordering::Acquire);
            debug_assert_eq!(cur.state(), SlotState::Live);
            let new = cur
                .inc_retain()
                .expect("zone Cap retain count overflowed during clone");
            match meta.compare_exchange(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    return Self {
                        raw: self.raw,
                        _marker: PhantomData,
                    };
                }
                Err(_) => continue,
            }
        }
    }
}

impl<T: 'static> PartialEq for Cap<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T: 'static> Eq for Cap<T> {}

impl<T: 'static> Drop for Cap<T> {
    fn drop(&mut self) {
        let Some(slot) = self.slot() else {
            debug_assert!(false, "zone Cap key no longer resolves to a slot");
            return;
        };
        let meta = unsafe { slot.as_ref().meta() };
        let old = loop {
            let cur = meta.load(Ordering::Acquire);
            debug_assert_eq!(cur.state(), SlotState::Live);
            debug_assert!(cur.retain() > 0);
            let new = if cur.retain() == 1 {
                // Last retention installs the no-upgrade barrier. The slot is
                // not reusable yet; EBR must first let old IdentRefs quiesce.
                cur.with_retain(RETAIN_SENTINEL_DEAD)
                    .with_state(SlotState::Dead)
            } else {
                cur.dec_retain()
                    .expect("zone Cap retain count underflowed during drop")
            };
            match meta.compare_exchange(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(previous) => break previous,
                Err(_) => continue,
            }
        };

        if old.retain() > 1 {
            return;
        }

        Self::try_retire(slot);
    }
}

impl<T: 'static> Deref for Cap<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        let slot = self
            .slot()
            .expect("zone Cap key no longer resolves to a live slot");
        unsafe { slot.as_ref().data_ref() }
    }
}

impl<T: 'static> PayloadCap<T> {
    pub fn from_cap(cap: Cap<T>) -> Self
    where
        T: ZoneAllocated,
        T::Policy: IsPayloadPolicy,
    {
        Self { inner: cap }
    }

    /// Unchecked constructor for tests and internal substrate code.
    #[doc(hidden)]
    pub fn from_cap_unchecked(cap: Cap<T>) -> Self {
        Self { inner: cap }
    }

    pub fn into_cap(self) -> Cap<T> {
        self.inner
    }

    pub fn downgrade(&self) -> Weak<T> {
        self.inner.downgrade()
    }

    pub fn ident_ref<'g>(&self, guard: &'g Guard<'_>) -> IdentRef<'g, T> {
        self.inner.ident_ref(guard)
    }

    pub fn key(&self) -> SlotKey {
        self.inner.key()
    }

    /// Packed trace object id.  Delegates to [`Cap::trace_id`].
    pub fn trace_id(&self) -> u64 {
        self.inner.trace_id()
    }
}

impl<T: 'static> Clone for PayloadCap<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: 'static> Deref for PayloadCap<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

unsafe impl<T: Send + Sync> Send for PayloadCap<T> {}
unsafe impl<T: Send + Sync> Sync for PayloadCap<T> {}

pub struct Weak<T: 'static> {
    /// Compact logical slot identity. Stale keys are harmless because
    /// `generation` is checked under an epoch guard before exposing data.
    pub(crate) raw: u32,
    /// Generation captured when the weak handle was created.
    pub(crate) generation: u16,
    _marker: PhantomData<T>,
}

// Manual Clone/Copy impls — the derives would synthesise
// `where T: Clone` bounds (the standard derive quirk for types
// containing `PhantomData<T>`), which prevents `Weak<NonClone>`
// from being Clone even though it's just an integer pair.
impl<T: 'static> Clone for Weak<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: 'static> Copy for Weak<T> {}

const _: () = {
    assert!(core::mem::size_of::<Cap<()>>() == 4);
    assert!(core::mem::size_of::<Weak<()>>() == 8);
};

unsafe impl<T: Send + Sync> Send for Weak<T> {}
unsafe impl<T: Send + Sync> Sync for Weak<T> {}

impl<T: 'static> Weak<T> {
    pub fn observe<'g>(&self, guard: &'g Guard<'_>) -> Option<IdentRef<'g, T>> {
        let _ = guard;
        let key = self.key();
        let slot = registry::slot_for::<T>(key)?;
        let meta = unsafe { slot.as_ref().meta() };
        let cur = meta.load(Ordering::Acquire);
        // Generation equality rules out ABA reuse of the same slot key.
        if cur.generation() == self.generation && cur.state() == SlotState::Live {
            Some(IdentRef {
                slot,
                raw: self.raw,
                generation: self.generation,
                _guard: PhantomData,
            })
        } else {
            None
        }
    }

    pub fn upgrade(&self, guard: &Guard<'_>) -> Option<Cap<T>> {
        self.observe(guard)?.to_cap().ok()
    }

    pub fn generation(&self) -> u16 {
        self.generation
    }

    pub fn key(&self) -> SlotKey {
        SlotKey::from_raw(self.raw)
    }

    pub fn raw(&self) -> u32 {
        self.raw
    }
}

pub struct IdentRef<'g, T: 'static> {
    /// Raw pointer is allowed only inside the guard-scoped borrowed reference.
    pub(crate) slot: NonNull<Slot<T>>,
    pub(crate) raw: u32,
    pub(crate) generation: u16,
    _guard: PhantomData<&'g ()>,
}

impl<'g, T: 'static> IdentRef<'g, T> {
    pub fn to_cap(&self) -> Result<Cap<T>, Dead> {
        let meta = unsafe { self.slot.as_ref().meta() };
        loop {
            let cur = meta.load(Ordering::Acquire);
            if cur.generation() != self.generation || cur.state() != SlotState::Live {
                return Err(Dead);
            }
            if cur.retain() == RETAIN_SENTINEL_DEAD {
                return Err(Dead);
            }

            // Upgrade is a CAS that both revalidates generation/state and adds
            // one retained reference. If another thread kills the slot first,
            // the generation/state/sentinel checks fail on retry.
            let new = cur.inc_retain().map_err(|_| Dead)?;
            match meta.compare_exchange(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    return Ok(Cap {
                        raw: self.raw,
                        _marker: PhantomData,
                    });
                }
                Err(_) => continue,
            }
        }
    }

    pub fn generation(&self) -> u16 {
        self.generation
    }

    pub fn key(&self) -> SlotKey {
        SlotKey::from_raw(self.raw)
    }

    pub fn raw(&self) -> u32 {
        self.raw
    }
}

impl<T: 'static> Deref for IdentRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.slot.as_ref().data_ref() }
    }
}

impl<T: 'static> core::fmt::Debug for Cap<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cap")
            .field("key", &self.key())
            .finish_non_exhaustive()
    }
}

impl<T: 'static> core::fmt::Debug for Weak<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Weak")
            .field("key", &self.key())
            .field("generation", &self.generation)
            .finish()
    }
}

impl<T: 'static> core::fmt::Debug for IdentRef<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IdentRef")
            .field("key", &self.key())
            .field("generation", &self.generation)
            .finish()
    }
}

impl<T: 'static> core::fmt::Debug for PayloadCap<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PayloadCap")
            .field("key", &self.key())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Cap::trace_id helpers
// ---------------------------------------------------------------------------

/// FNV-1a hasher for `#[no_std]` environments.
struct Fnv1aHasher(u64);

impl Hasher for Fnv1aHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        const FNV_PRIME: u64 = 0x00000100000001B3;
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        let mut h = if self.0 == 0 { FNV_OFFSET } else { self.0 };
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        self.0 = h;
    }
}

/// Stable type discriminant byte for `T`, used in `Cap::trace_id`.
///
/// Derives the low 8 bits of the FNV-1a hash of `TypeId::of::<T>()`.
/// Collisions across types in the low byte are benign for observation
/// payloads — the full (slot, generation, kind) triple is used for
/// disambiguation by the daemon.
#[inline]
pub(super) fn cap_kind_byte<T: 'static>() -> u8 {
    let mut h = Fnv1aHasher(0);
    core::any::TypeId::of::<T>().hash(&mut h);
    h.finish() as u8
}
