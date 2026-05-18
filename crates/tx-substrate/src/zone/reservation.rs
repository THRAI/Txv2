//! Linear reservation and publication path for zone objects.
//!
//! Allocation is split into reserve and sign so higher-level steps can reserve
//! all resources first and publish only after every preparation has succeeded.

use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use super::cap::Cap;
use super::meta::SlotState;
use super::registry;
use super::slot::Slot;
use super::{runtime, Zone, ZoneError};

// ---------------------------------------------------------------------------
// L6 mutation emit gate (OBS-8)
// ---------------------------------------------------------------------------

/// Runtime gate for L6 `MutationZoneSign` observation events.
///
/// Defaults to **off** so the change is observable on demand without
/// perturbing existing benchmarks.  Flip to `true` at boot to enable.
///
/// High-frequency path: the gate is a single `Relaxed` `AtomicBool` load on
/// every `sign` call.  When `false`, the emit block is a ~1-ns dead branch.
pub static MUTATION_EMIT_ENABLED: AtomicBool = AtomicBool::new(false);

pub struct ZoneReservation<T: 'static> {
    /// Owning zone. Drop rollback returns the reserved slot here.
    zone: &'static Zone<T>,
    /// Slot currently in `Reserved` state.
    pub(crate) slot: NonNull<Slot<T>>,
    /// Cleared by `sign` so Drop does not roll back a published slot.
    active: bool,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<T: 'static> ZoneReservation<T> {
    pub(crate) fn new(zone: &'static Zone<T>, slot: NonNull<Slot<T>>) -> Self {
        Self {
            zone,
            slot,
            active: true,
            _not_send_sync: PhantomData,
        }
    }

    pub fn zone(&self) -> &'static Zone<T> {
        self.zone
    }
}

impl<T: 'static> Drop for ZoneReservation<T> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }

        // Rollback never bumps generation because the slot was never visible to
        // weak observers.
        let meta = unsafe { self.slot.as_ref().meta() };
        loop {
            let cur = meta.load(Ordering::Acquire);
            if cur.state() != SlotState::Reserved {
                return;
            }
            let new = cur.with_state(SlotState::Free);
            match meta.compare_exchange(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    self.zone.return_slot(self.slot);
                    return;
                }
                Err(_) => continue,
            }
        }
    }
}

pub fn reserve<T: 'static>(zone: &'static Zone<T>) -> Result<ZoneReservation<T>, ZoneError> {
    runtime::ensure_running()?;
    let zone_id = zone.id();
    if registry::lookup(zone_id).is_none() {
        return Err(ZoneError::NotRegistered);
    }
    let slot = zone.pop_free_slot()?;
    let meta = unsafe { slot.as_ref().meta() };
    loop {
        let cur = meta.load(Ordering::Acquire);
        if cur.state() != SlotState::Free || cur.retain() != 0 {
            zone.return_slot(slot);
            return Err(ZoneError::InvalidState);
        }
        let new = cur.with_state(SlotState::Reserved);
        if meta
            .compare_exchange(cur, new, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break;
        }
    }
    Ok(ZoneReservation::new(zone, slot))
}

pub fn sign<T: 'static>(mut reservation: ZoneReservation<T>, value: T) -> Cap<T> {
    let slot = reservation.slot;
    unsafe {
        slot.as_ref().write_value(value);
    }

    // Publication is infallible after reservation: write the value, then publish
    // the slot as Live with the initial retain count.
    let meta = unsafe { slot.as_ref().meta() };
    loop {
        let cur = meta.load(Ordering::Acquire);
        debug_assert_eq!(cur.state(), SlotState::Reserved);
        debug_assert_eq!(cur.retain(), 0);
        let new = cur.with_retain(1).with_state(SlotState::Live);
        if meta
            .compare_exchange(cur, new, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break;
        }
    }

    reservation.active = false;
    let cap = unsafe { Cap::from_slot(slot) };

    // L6 MutationZoneSign emit (OBS-8).
    //
    // Emitted after publication so the Cap is fully live.  Gated by
    // `MUTATION_EMIT_ENABLED` (default off) to avoid perturbing
    // benchmarks.  The emit is at a substrate convergence point, not
    // inside a `StepOp::step` body (OBS-A-1).
    if MUTATION_EMIT_ENABLED.load(Ordering::Relaxed) {
        if let Some(em) = tx_observe::current() {
            use tx_observe::encode::{encode_mutation_zone_sign, mutation_zone_sign_tag};
            use tx_observe::{EventNameId, TxTraceLevel};
            use tx_observe_types::PayloadMutationZoneSign;

            let object_id = cap.trace_id();
            let kind_byte = (object_id >> 56) as u8;
            let p = PayloadMutationZoneSign {
                object_id,
                kind: kind_byte,
                _pad: [0u8; 7],
            };
            let (payload_bytes, _) = encode_mutation_zone_sign(&p);
            em.instant(
                TxTraceLevel::Mutation,
                EventNameId::from_raw(0x4d5a5347u32), // "MZSG" — mutation.zone_sign
                tx_observe::SpanId::NONE,
                mutation_zone_sign_tag(),
                &payload_bytes,
            );
        }
    }

    cap
}
