//! Runtime registry for static `Zone<T>` instances.
//!
//! The registry gives compact keys a way back to their owning zone without
//! exposing raw pointers in public handles. Each registered zone installs typed
//! callback shims that operate on an erased `*const ()`.

use core::any::TypeId;
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::CpuId;

use super::slot::Slot;
use super::sync::SpinLock;
use super::{Zone, ZoneError};

const MAX_REGISTERED_ZONES: usize = 256;
const ZONE_ID_BITS: u32 = 8;
const SLOT_ID_BITS: u32 = 32 - ZONE_ID_BITS;
const SLOT_ID_MASK: u32 = (1u32 << SLOT_ID_BITS) - 1;
pub(crate) const SLOTS_PER_SLAB: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZoneId(pub usize);

/// Logical address of one slot inside one registered zone.
///
/// Encoding layout:
///
/// - high 8 bits: `zone_id - 1`, supporting public ZoneId values 1..=256
/// - low 24 bits: dense `slot_id`
///
/// `slot_id` is derived from the current slab layout:
/// `(slab_id - 1) * SLOTS_PER_SLAB + slot_index`.
#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(transparent)]
pub struct SlotKey(u32);

impl SlotKey {
    pub(crate) fn new(zone_id: ZoneId, slab_id: usize, slot_index: usize) -> Option<Self> {
        if zone_id.0 == 0 || zone_id.0 > MAX_REGISTERED_ZONES {
            return None;
        }
        if slab_id == 0 || slot_index >= SLOTS_PER_SLAB {
            return None;
        }

        let slot_id = (slab_id - 1)
            .checked_mul(SLOTS_PER_SLAB)?
            .checked_add(slot_index)?;
        if slot_id > SLOT_ID_MASK as usize {
            return None;
        }

        let encoded_zone = ((zone_id.0 - 1) as u32) << SLOT_ID_BITS;
        Some(Self(encoded_zone | slot_id as u32))
    }

    pub(crate) const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn zone_id(self) -> ZoneId {
        ZoneId(((self.0 >> SLOT_ID_BITS) as usize) + 1)
    }

    pub const fn slot_id(self) -> u32 {
        self.0 & SLOT_ID_MASK
    }

    pub const fn slab_id(self) -> usize {
        (self.slot_id() as usize / SLOTS_PER_SLAB) + 1
    }

    pub const fn slot_index(self) -> usize {
        self.slot_id() as usize % SLOTS_PER_SLAB
    }
}

impl core::fmt::Debug for SlotKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SlotKey")
            .field("raw", &self.raw())
            .field("zone_id", &self.zone_id())
            .field("slab_id", &self.slab_id())
            .field("slot_index", &self.slot_index())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZoneInfo {
    pub id: ZoneId,
    pub type_id: TypeId,
    pub allocated_slots: usize,
    pub slab_count: usize,
}

#[derive(Clone, Copy)]
struct RegisteredZone {
    /// Stable ID used as the registry table index.
    zone_id: ZoneId,
    /// Rust type stored in this zone. Used to reject cross-type key resolution.
    type_id: TypeId,
    /// Erased `&'static Zone<T>`.
    erased: *const (),
    /// Rebuild current diagnostics for this zone.
    refresh_info: fn(*const ()) -> ZoneInfo,
    /// Initialize this zone's per-CPU bucket for one CPU.
    init_cpu_bucket: fn(*const (), CpuId) -> Result<(), ZoneError>,
    /// Resolve a logical key to a typed slot pointer, erased for storage.
    slot_from_key: fn(*const (), SlotKey) -> Option<*mut ()>,
}

static NEXT_ZONE_ID: AtomicUsize = AtomicUsize::new(1);
static REGISTRY: ZoneRegistry = ZoneRegistry::new();

pub(crate) fn allocate_zone_id() -> ZoneId {
    ZoneId(NEXT_ZONE_ID.fetch_add(1, Ordering::AcqRel))
}

pub fn register_static_zone<T: 'static>(zone: &'static Zone<T>) -> Result<ZoneInfo, ZoneError> {
    // `zone.info()` assigns a ZoneId on first use. Registration is idempotent
    // for the same static zone and type.
    let info = zone.info();
    let entry = RegisteredZone {
        zone_id: info.id,
        type_id: info.type_id,
        erased: zone as *const Zone<T> as *const (),
        refresh_info: refresh_info::<T>,
        init_cpu_bucket: init_cpu_bucket::<T>,
        slot_from_key: slot_from_key::<T>,
    };
    REGISTRY.register(entry)?;
    Ok(info)
}

pub fn lookup(zone_id: ZoneId) -> Option<ZoneInfo> {
    REGISTRY.lookup(zone_id)
}

pub fn registered_zone_count() -> usize {
    REGISTRY.count()
}

pub fn snapshot(out: &mut [Option<ZoneInfo>]) -> usize {
    REGISTRY.snapshot(out)
}

pub(crate) fn init_cpu_buckets(cpu: CpuId) -> Result<(), ZoneError> {
    REGISTRY.init_cpu_buckets(cpu)
}

pub(crate) fn slot_for<T: 'static>(key: SlotKey) -> Option<NonNull<Slot<T>>> {
    REGISTRY.slot_for::<T>(key)
}

pub(crate) fn init_registry() {
    REGISTRY.clear();
    NEXT_ZONE_ID.store(1, Ordering::Release);
}

pub(crate) fn reset_for_test() {
    init_registry();
}

fn refresh_info<T: 'static>(erased: *const ()) -> ZoneInfo {
    let zone = unsafe { &*(erased as *const Zone<T>) };
    zone.info()
}

fn init_cpu_bucket<T: 'static>(erased: *const (), cpu: CpuId) -> Result<(), ZoneError> {
    let zone = unsafe { &*(erased as *const Zone<T>) };
    zone.init_cpu_bucket(cpu)
}

fn slot_from_key<T: 'static>(erased: *const (), key: SlotKey) -> Option<*mut ()> {
    let zone = unsafe { &*(erased as *const Zone<T>) };
    zone.slot_from_key(key).map(|slot| slot.as_ptr() as *mut ())
}

struct ZoneRegistry {
    lock: SpinLock,
    entries: [UnsafeCell<Option<RegisteredZone>>; MAX_REGISTERED_ZONES],
}

unsafe impl Sync for ZoneRegistry {}

impl ZoneRegistry {
    const fn new() -> Self {
        Self {
            lock: SpinLock::new(),
            entries: [const { UnsafeCell::new(None) }; MAX_REGISTERED_ZONES],
        }
    }

    fn register(&self, entry: RegisteredZone) -> Result<(), ZoneError> {
        if entry.zone_id.0 == 0 || entry.zone_id.0 > MAX_REGISTERED_ZONES {
            return Err(ZoneError::AllocationFailed);
        }

        let _guard = self.lock.lock();
        unsafe {
            let slot = &mut *self.entries[entry.zone_id.0 - 1].get();
            match *slot {
                Some(existing)
                    if existing.erased == entry.erased && existing.type_id == entry.type_id =>
                {
                    Ok(())
                }
                Some(_) => Err(ZoneError::InvalidState),
                None => {
                    *slot = Some(entry);
                    Ok(())
                }
            }
        }
    }

    fn lookup(&self, zone_id: ZoneId) -> Option<ZoneInfo> {
        if zone_id.0 == 0 || zone_id.0 > MAX_REGISTERED_ZONES {
            return None;
        }
        let _guard = self.lock.lock();
        let entry = unsafe { *self.entries[zone_id.0 - 1].get() }?;
        Some((entry.refresh_info)(entry.erased))
    }

    fn count(&self) -> usize {
        let _guard = self.lock.lock();
        let mut count = 0;
        for entry in &self.entries {
            if unsafe { (*entry.get()).is_some() } {
                count += 1;
            }
        }
        count
    }

    fn snapshot(&self, out: &mut [Option<ZoneInfo>]) -> usize {
        let _guard = self.lock.lock();
        let mut written = 0;
        for entry in &self.entries {
            if written == out.len() {
                break;
            }
            let Some(entry) = (unsafe { *entry.get() }) else {
                continue;
            };
            out[written] = Some((entry.refresh_info)(entry.erased));
            written += 1;
        }
        written
    }

    fn init_cpu_buckets(&self, cpu: CpuId) -> Result<(), ZoneError> {
        let _guard = self.lock.lock();
        for entry in &self.entries {
            let Some(entry) = (unsafe { *entry.get() }) else {
                continue;
            };
            (entry.init_cpu_bucket)(entry.erased, cpu)?;
        }
        Ok(())
    }

    fn slot_for<T: 'static>(&self, key: SlotKey) -> Option<NonNull<Slot<T>>> {
        let zone_id = key.zone_id();
        if zone_id.0 == 0 || zone_id.0 > MAX_REGISTERED_ZONES {
            return None;
        }

        let _guard = self.lock.lock();
        let entry = unsafe { *self.entries[zone_id.0 - 1].get() }?;
        // A stale or forged key must not resolve across zone/type boundaries.
        if entry.zone_id != zone_id || entry.type_id != TypeId::of::<T>() {
            return None;
        }

        let ptr = (entry.slot_from_key)(entry.erased, key)?;
        NonNull::new(ptr.cast::<Slot<T>>())
    }

    fn clear(&self) {
        let _guard = self.lock.lock();
        for entry in &self.entries {
            unsafe {
                *entry.get() = None;
            }
        }
    }
}
