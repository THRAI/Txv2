//! Retained and weak references to zone slots.
//!
//! `Cap<T>` owns identity retention. `Weak<T>` is only a generation-checked
//! lookup hint. `IdentRef<'g, T>` is a guard-scoped borrowed observation that
//! can be upgraded into a `Cap<T>` only while the slot is still live.

use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use crate::epoch::{self, Guard};

use super::meta::{SlotState, RETAIN_SENTINEL_DEAD};
use super::registry::{self, SlotKey};
use super::slot::{reclaim_slot, Slot};
use super::Dead;

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

    pub fn retain_count(&self) -> u32 {
        let Some(slot) = self.slot() else {
            debug_assert!(false, "zone Cap key no longer resolves to a slot");
            return 0;
        };
        unsafe { slot.as_ref().meta().load(Ordering::Acquire).retain() }
    }

    fn try_retire(slot: NonNull<Slot<T>>) {
        let meta = unsafe { slot.as_ref().meta() };
        loop {
            let cur = meta.load(Ordering::Acquire);
            if cur.retain() != RETAIN_SENTINEL_DEAD || cur.state() != SlotState::Dead {
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
                    let retry_result =
                        unsafe { epoch::retire_raw(slot.as_ptr() as *mut u8, reclaim_slot::<T>) };
                    if retry_result.is_ok() {
                        return;
                    }

                    // Once the slot is Retiring there is no remaining Cap that
                    // can safely retry later. Failing loudly is better than
                    // silently leaking a permanently unreachable slot.
                    panic!("zone EBR retirement failed after drain");
                }
                Err(_) => continue,
            }
        }
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
    pub fn from_cap(cap: Cap<T>) -> Self {
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

#[derive(Clone, Copy)]
pub struct Weak<T: 'static> {
    /// Compact logical slot identity. Stale keys are harmless because
    /// `generation` is checked under an epoch guard before exposing data.
    pub(crate) raw: u32,
    /// Generation captured when the weak handle was created.
    pub(crate) generation: u16,
    _marker: PhantomData<T>,
}

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
