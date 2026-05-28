//! Zone-backed object storage.
//!
//! This module is the first executable Zone slice: it exposes the public
//! `Zone`/`Cap`/`Weak`/`IdentRef` shape, supports reserve/sign publication, and
//! sends final object reclamation through EBR. The later Keg/per-CPU-bucket
//! layer can reuse the same slot metadata and reference semantics.

mod bucket;
mod cap;
mod error;
mod identity_slot;
mod keg;
mod meta;
mod payload;
mod policy;
mod registry;
mod reservation;
mod runtime;
mod slab;
mod slot;
mod sync;

use core::any::TypeId;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{CpuId, TxPlatform};

pub use bucket::{ZoneBucket, DEFAULT_BUCKET_CAPACITY};
pub use cap::{Cap, IdentRef, PayloadCap, Weak};
pub use error::{Dead, ZoneError};
pub use identity_slot::IdentitySlot;
pub use meta::{SlotState, SlotWord};
pub use payload::{CoLocatedEntity, Entity, OperationalCapExt, OperationalRefExt, PayloadBinding};
pub use policy::{
    CapProducingPolicy, IsPayloadPolicy, ObserverNodePolicy, PayloadPolicy, RetainedEntityPolicy,
    ZonePolicy,
};
pub use registry::{
    lookup, register_static_zone, registered_zone_count, snapshot, EmptySlabTrimStats, SlotKey,
    ZoneId, ZoneInfo,
};
pub use reservation::{reserve, ZoneReservation, MUTATION_EMIT_ENABLED};
pub use runtime::{
    freeze_for_shutdown, init_on_ap, init_on_bsp, is_initialized, state, ZoneRuntimeState,
};
pub use slab::ZoneSlab;

const ZONE_ID_INITIALIZING: usize = usize::MAX;

pub struct Zone<T: 'static> {
    /// Lazily assigned registry ID. Zero means not yet assigned.
    id: AtomicUsize,
    /// Number of slots represented by slabs currently owned by this zone.
    allocated_slots: AtomicUsize,
    /// Central slab manager for this object type.
    keg: keg::Keg<T>,
    /// Per-CPU free-slot caches. Each bucket may be touched only while the
    /// current CPU is pinned.
    buckets: [UnsafeCell<ZoneBucket<T>>; runtime::MAX_ZONE_CPUS],
    _marker: PhantomData<fn() -> T>,
}

unsafe impl<T: 'static> Sync for Zone<T> {}

impl<T: 'static> Zone<T> {
    pub const fn const_new() -> Self {
        Self {
            id: AtomicUsize::new(0),
            allocated_slots: AtomicUsize::new(0),
            keg: keg::Keg::const_new(),
            buckets: [const { UnsafeCell::new(ZoneBucket::new()) }; runtime::MAX_ZONE_CPUS],
            _marker: PhantomData,
        }
    }

    pub fn id(&self) -> ZoneId {
        loop {
            let existing = self.id.load(Ordering::Acquire);
            if existing != 0 && existing != ZONE_ID_INITIALIZING {
                return ZoneId(existing);
            }

            if existing == ZONE_ID_INITIALIZING {
                core::hint::spin_loop();
                continue;
            }

            if self
                .id
                .compare_exchange(0, ZONE_ID_INITIALIZING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Only the thread that wins the initializing CAS consumes a new
                // ZoneId, avoiding ID leaks under concurrent first access.
                let allocated = registry::allocate_zone_id();
                self.id.store(allocated.0, Ordering::Release);
                return allocated;
            }
        }
    }

    /// Initialize the static zone metadata during Zone core init.
    ///
    /// This does not allocate any object slab. The `Keg` and per-CPU buckets are
    /// already empty through `const_new`; this method assigns a stable `ZoneId`
    /// and publishes the zone to the debug registry.
    pub fn init_static(&'static self) -> ZoneInfo {
        registry::register_static_zone(self).expect("static zone registration failed")
    }

    pub fn info(&self) -> ZoneInfo {
        ZoneInfo {
            id: self.id(),
            type_id: TypeId::of::<T>(),
            type_name: core::any::type_name::<T>(),
            allocated_slots: self.allocated_slots(),
            slab_count: self.slab_count(),
            empty_slab_count: self.empty_slab_count(),
        }
    }

    pub fn allocated_slots(&self) -> usize {
        self.allocated_slots.load(Ordering::Acquire)
    }

    pub fn slab_count(&self) -> usize {
        self.keg.slab_count()
    }

    pub fn empty_slab_count(&self) -> usize {
        self.keg.empty_slab_count()
    }

    pub(crate) fn note_allocated_slots(&self, count: usize) {
        self.allocated_slots.fetch_add(count, Ordering::AcqRel);
    }

    pub(crate) fn note_released_slots(&self, count: usize) {
        self.allocated_slots.fetch_sub(count, Ordering::AcqRel);
    }

    pub(crate) fn pop_free_slot(
        &'static self,
    ) -> Result<core::ptr::NonNull<slot::Slot<T>>, ZoneError> {
        let cpu_pin = runtime::pin_current_cpu()?;
        let bucket = unsafe { &mut *self.buckets[cpu_pin.cpu_id().0].get() };
        if let Some(slot) = bucket.pop() {
            return Ok(slot);
        }
        self.keg.refill_bucket(self, bucket)?;
        bucket.pop().ok_or(ZoneError::AllocationFailed)
    }

    pub(crate) fn return_slot(&self, slot: core::ptr::NonNull<slot::Slot<T>>) {
        self.keg.return_slot(slot);
    }

    pub(crate) fn return_slot_from_reclaim(&self, slot: core::ptr::NonNull<slot::Slot<T>>) {
        self.keg.return_slot_without_slab_retire(slot);
    }

    pub(crate) fn trim_empty_slabs(&self, limit: usize) -> usize {
        self.keg.trim_empty_slabs(limit)
    }

    pub(crate) fn flush_current_cpu_bucket(&'static self) -> Result<(), ZoneError> {
        let cpu_pin = runtime::pin_current_cpu()?;
        let bucket = unsafe { &mut *self.buckets[cpu_pin.cpu_id().0].get() };
        self.drain_bucket_to_keg(bucket);
        Ok(())
    }

    pub(crate) fn slot_from_key(&self, key: SlotKey) -> Option<core::ptr::NonNull<slot::Slot<T>>> {
        if key.zone_id() != self.id() {
            return None;
        }
        self.keg.slot_from_key(key)
    }

    pub fn refill_bucket<const N: usize>(
        &'static self,
        bucket: &mut ZoneBucket<T, N>,
    ) -> Result<(), ZoneError> {
        self.keg.refill_bucket(self, bucket)
    }

    pub fn drain_bucket_to_keg<const N: usize>(&self, bucket: &mut ZoneBucket<T, N>) {
        while let Some(slot) = bucket.pop() {
            self.keg.return_slot(slot);
        }
    }

    pub(crate) fn init_cpu_bucket(&'static self, cpu: CpuId) -> Result<(), ZoneError> {
        if cpu.0 >= runtime::MAX_ZONE_CPUS {
            return Err(ZoneError::InvalidState);
        }
        let bucket = unsafe { &mut *self.buckets[cpu.0].get() };
        bucket.clear();
        Ok(())
    }
}

impl<T: 'static> Default for Zone<T> {
    fn default() -> Self {
        Self::const_new()
    }
}

/// Type-level binding between an object type and its single static zone.
///
/// # Safety
///
/// Implementations must always return the same process-wide static `Zone<Self>`.
/// Returning a different zone for the same type would make compact slot keys
/// resolve through the wrong registry entry and break `Cap`/`Weak` generation
/// safety.
pub unsafe trait ZoneAllocated: Sized + 'static {
    /// Slot lifecycle policy for this zone-backed type.
    ///
    /// Defaults to `RetainedEntityPolicy<Self>` so existing impls compile
    /// unchanged.  Payload types in split-entity layouts should declare
    /// `type Policy = PayloadPolicy<Self>` for audit visibility.
    /// Observer nodes use `ObserverNodePolicy<Self>`, which rejects
    /// `zone::sign` / `sign_for` at compile time.
    type Policy: ZonePolicy = RetainedEntityPolicy<Self>;

    /// Return the single static zone that stores values of this type.
    fn zone() -> &'static Zone<Self>;
}

pub fn reserve_for<T: ZoneAllocated>() -> Result<ZoneReservation<T>, ZoneError> {
    reserve(T::zone())
}

pub fn register_zone_for<T: ZoneAllocated>() -> Result<ZoneInfo, ZoneError> {
    registry::register_static_zone(T::zone())
}

pub fn sign_for<T: ZoneAllocated>(reservation: ZoneReservation<T>, value: T) -> Cap<T>
where
    T::Policy: CapProducingPolicy,
{
    reservation::sign(reservation, value)
}

/// Reserve a zone slot and sign a value into it in one step.
///
/// Convenience for the canonical adapter pattern
/// `reserve_for::<T>()? → sign_for(res, value)`.
/// Used by 2+ adapters; defined here so per-adapter wrappers can be
/// replaced with a direct call to `zone::sign`.
pub fn sign<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError>
where
    T::Policy: CapProducingPolicy,
{
    let res = reserve_for::<T>()?;
    Ok(sign_for(res, value))
}

pub fn init_ap_for_current_stage(cpu: CpuId) -> Result<(), ZoneError> {
    init_on_ap(cpu)
}

pub fn init_bsp_for_current_stage<P: TxPlatform>() -> Result<(), ZoneError> {
    init_on_bsp::<P>()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ZoneMaintenanceBudget {
    pub epoch_reclaim_budget: usize,
    pub empty_slab_budget: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ZoneMaintenanceStats {
    pub epoch: crate::epoch::DrainStats,
    pub empty_slabs: EmptySlabTrimStats,
}

pub fn return_empty_slabs(limit: usize) -> EmptySlabTrimStats {
    registry::trim_empty_slabs(limit)
}

pub fn maintenance_tick(budget: ZoneMaintenanceBudget) -> ZoneMaintenanceStats {
    let _ = registry::flush_current_cpu_buckets();
    ZoneMaintenanceStats {
        epoch: crate::epoch::try_drain(budget.epoch_reclaim_budget),
        empty_slabs: return_empty_slabs(budget.empty_slab_budget),
    }
}

#[doc(hidden)]
pub mod testing {
    pub unsafe fn reset_for_test() {
        super::registry::reset_for_test();
        super::runtime::reset_for_test();
    }

    pub fn init_for_test(page_size: usize, direct_map_base: usize) -> Result<(), super::ZoneError> {
        super::runtime::init_for_test(page_size, direct_map_base)
    }

    /// Return a `Cap` pointing at the reserved (not yet live) slot.
    ///
    /// # Safety
    ///
    /// The returned `Cap` must not be dereferenced until after `sign_for` completes
    /// for this reservation. Use only to break mutual-reference cycles during test
    /// fixture construction where neither side will be accessed before both are live.
    pub unsafe fn peek_reservation_cap<T: super::ZoneAllocated>(
        res: &super::ZoneReservation<T>,
    ) -> super::Cap<T> {
        unsafe { super::cap::Cap::from_slot(res.slot) }
    }
}
