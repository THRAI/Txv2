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
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
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

/// One logical Zone retirement, including the occupant generation expected by
/// its EBR callback.
///
/// `SlotKey` alone names reusable storage. Carrying the generation prevents a
/// delayed callback from reclaiming a later occupant of the same physical
/// slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetiredSlot {
    key: SlotKey,
    generation: u16,
}

impl RetiredSlot {
    pub(crate) const fn new(key: SlotKey, generation: u16) -> Self {
        Self { key, generation }
    }

    pub(crate) const fn key(self) -> SlotKey {
        self.key
    }

    pub(crate) const fn generation(self) -> u16 {
        self.generation
    }

    pub(crate) const fn encode_link(self) -> u64 {
        (1 << 63) | ((self.generation as u64) << 32) | self.key.raw() as u64
    }

    pub(crate) const fn decode_link(raw: u64) -> Option<Self> {
        if raw & (1 << 63) == 0 {
            None
        } else {
            Some(Self::new(
                SlotKey::from_raw(raw as u32),
                ((raw >> 32) & u16::MAX as u64) as u16,
            ))
        }
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
    pub type_name: &'static str,
    pub allocated_slots: usize,
    pub slab_count: usize,
    pub empty_slab_count: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmptySlabTrimStats {
    pub scanned_zones: usize,
    pub retired_slabs: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SlotLookupDebug {
    pub reason: &'static str,
    pub requested_type: &'static str,
    pub registered_type: Option<&'static str>,
    pub key: SlotKey,
    pub allocated_slots: Option<usize>,
    pub slab_count: Option<usize>,
    pub empty_slab_count: Option<usize>,
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
    /// Flush the current CPU bucket back into the central keg.
    flush_current_cpu_bucket: fn(*const ()) -> Result<(), ZoneError>,
    /// Retire surplus empty slabs for runtime maintenance.
    trim_empty_slabs: fn(*const (), usize) -> usize,
    /// Resolve a logical key to a typed slot pointer, erased for storage.
    slot_from_key: fn(*const (), SlotKey) -> Option<*mut ()>,
    /// Read or replace the generation-bearing intrusive retirement link.
    retiring_next: fn(*const (), RetiredSlot) -> Option<RetiredSlot>,
    set_retiring_next: fn(*const (), RetiredSlot, Option<RetiredSlot>),
    /// Reclaim one typed slot selected from a mixed Zone retirement bag.
    reclaim_slot: unsafe fn(*const (), RetiredSlot, &mut crate::epoch::LocalRetireGuard),
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
        flush_current_cpu_bucket: flush_current_cpu_bucket::<T>,
        trim_empty_slabs: trim_empty_slabs_for::<T>,
        slot_from_key: slot_from_key::<T>,
        retiring_next: retiring_next_for::<T>,
        set_retiring_next: set_retiring_next_for::<T>,
        reclaim_slot: reclaim_slot_for::<T>,
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

pub fn trim_empty_slabs(limit: usize) -> EmptySlabTrimStats {
    REGISTRY.trim_empty_slabs(limit)
}

pub fn flush_current_cpu_buckets() -> Result<(), ZoneError> {
    REGISTRY.flush_current_cpu_buckets()
}

pub(crate) fn init_cpu_buckets(cpu: CpuId) -> Result<(), ZoneError> {
    REGISTRY.init_cpu_buckets(cpu)
}

pub(crate) fn slot_for<T: 'static>(key: SlotKey) -> Option<NonNull<Slot<T>>> {
    REGISTRY.slot_for::<T>(key)
}

pub(crate) fn retiring_next(retired: RetiredSlot) -> Option<RetiredSlot> {
    REGISTRY.retiring_next(retired)
}

pub(crate) fn set_retiring_next(retired: RetiredSlot, next: Option<RetiredSlot>) {
    REGISTRY.set_retiring_next(retired, next);
}

pub(crate) unsafe fn reclaim_slot(
    retired: RetiredSlot,
    local_guard: &mut crate::epoch::LocalRetireGuard,
) {
    unsafe { REGISTRY.reclaim_slot(retired, local_guard) }
}

pub(crate) fn slot_lookup_debug<T: 'static>(key: SlotKey) -> SlotLookupDebug {
    REGISTRY.slot_lookup_debug::<T>(key)
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

fn flush_current_cpu_bucket<T: 'static>(erased: *const ()) -> Result<(), ZoneError> {
    let zone = unsafe { &*(erased as *const Zone<T>) };
    zone.flush_current_cpu_bucket()
}

fn trim_empty_slabs_for<T: 'static>(erased: *const (), limit: usize) -> usize {
    let zone = unsafe { &*(erased as *const Zone<T>) };
    zone.trim_empty_slabs(limit)
}

fn slot_from_key<T: 'static>(erased: *const (), key: SlotKey) -> Option<*mut ()> {
    let zone = unsafe { &*(erased as *const Zone<T>) };
    zone.slot_from_key(key).map(|slot| slot.as_ptr() as *mut ())
}

fn retiring_next_for<T: 'static>(erased: *const (), retired: RetiredSlot) -> Option<RetiredSlot> {
    let zone = unsafe { &*(erased as *const Zone<T>) };
    let slot = zone.slot_from_key(retired.key())?;
    let slot = unsafe { slot.as_ref() };
    let current = slot.meta().load(Ordering::Acquire);
    if current.state() != super::SlotState::Retiring
        || current.generation() != retired.generation()
        || current.reclaim_claimed()
    {
        return None;
    }
    slot.retiring_next()
}

fn set_retiring_next_for<T: 'static>(
    erased: *const (),
    retired: RetiredSlot,
    next: Option<RetiredSlot>,
) {
    let zone = unsafe { &*(erased as *const Zone<T>) };
    let Some(slot) = zone.slot_from_key(retired.key()) else {
        return;
    };
    let slot = unsafe { slot.as_ref() };
    let current = slot.meta().load(Ordering::Acquire);
    if current.state() == super::SlotState::Retiring
        && current.generation() == retired.generation()
        && !current.reclaim_claimed()
    {
        slot.set_retiring_next(next);
    }
}

unsafe fn reclaim_slot_for<T: 'static>(
    erased: *const (),
    retired: RetiredSlot,
    local_guard: &mut crate::epoch::LocalRetireGuard,
) {
    let zone = unsafe { &*(erased as *const Zone<T>) };
    let Some(slot) = zone.slot_from_key(retired.key()) else {
        return;
    };
    unsafe { super::slot::reclaim_slot(slot, retired.generation(), local_guard) };
}

struct ZoneRegistry {
    lock: SpinLock,
    entries: [UnsafeCell<Option<RegisteredZone>>; MAX_REGISTERED_ZONES],
    /// Lock-free read gate per entry. `register` publishes with `Release`
    /// after writing the entry under `lock`; `entry_at` reads the entry
    /// without the lock once it observes `true` with `Acquire`. Entries are
    /// write-once for the kernel's lifetime (`clear` is boot/test-only), so
    /// a published entry is immutable and the unguarded read is sound. This
    /// keeps the global registry SpinLock off the per-Cap resolution path —
    /// it was acquired on every Cap deref/clone/drop in every subsystem.
    published: [core::sync::atomic::AtomicBool; MAX_REGISTERED_ZONES],
}

unsafe impl Sync for ZoneRegistry {}

impl ZoneRegistry {
    const fn new() -> Self {
        Self {
            lock: SpinLock::new(),
            entries: [const { UnsafeCell::new(None) }; MAX_REGISTERED_ZONES],
            published: [const { core::sync::atomic::AtomicBool::new(false) }; MAX_REGISTERED_ZONES],
        }
    }

    fn register(&self, entry: RegisteredZone) -> Result<(), ZoneError> {
        if entry.zone_id.0 == 0 || entry.zone_id.0 > MAX_REGISTERED_ZONES {
            return Err(ZoneError::AllocationFailed);
        }

        let _guard = self.lock.lock();
        unsafe {
            let index = entry.zone_id.0 - 1;
            let slot = &mut *self.entries[index].get();
            match *slot {
                Some(existing)
                    if existing.erased == entry.erased && existing.type_id == entry.type_id =>
                {
                    Ok(())
                }
                Some(_) => Err(ZoneError::InvalidState),
                None => {
                    *slot = Some(entry);
                    self.published[index].store(true, Ordering::Release);
                    Ok(())
                }
            }
        }
    }

    fn lookup(&self, zone_id: ZoneId) -> Option<ZoneInfo> {
        if zone_id.0 == 0 || zone_id.0 > MAX_REGISTERED_ZONES {
            return None;
        }
        let entry = self.entry_at(zone_id.0 - 1)?;
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
        let mut written = 0;
        for index in 0..MAX_REGISTERED_ZONES {
            if written == out.len() {
                break;
            }
            let Some(entry) = self.entry_at(index) else {
                continue;
            };
            out[written] = Some((entry.refresh_info)(entry.erased));
            written += 1;
        }
        written
    }

    fn init_cpu_buckets(&self, cpu: CpuId) -> Result<(), ZoneError> {
        for index in 0..MAX_REGISTERED_ZONES {
            let Some(entry) = self.entry_at(index) else {
                continue;
            };
            (entry.init_cpu_bucket)(entry.erased, cpu)?;
        }
        Ok(())
    }

    fn flush_current_cpu_buckets(&self) -> Result<(), ZoneError> {
        for index in 0..MAX_REGISTERED_ZONES {
            let Some(entry) = self.entry_at(index) else {
                continue;
            };
            (entry.flush_current_cpu_bucket)(entry.erased)?;
        }
        Ok(())
    }

    fn trim_empty_slabs(&self, limit: usize) -> EmptySlabTrimStats {
        let mut stats = EmptySlabTrimStats::default();
        let mut remaining = limit;

        for index in 0..MAX_REGISTERED_ZONES {
            let Some(entry) = self.entry_at(index) else {
                continue;
            };
            stats.scanned_zones += 1;
            if remaining == 0 {
                continue;
            }
            let retired = (entry.trim_empty_slabs)(entry.erased, remaining);
            stats.retired_slabs += retired;
            remaining = remaining.saturating_sub(retired);
        }

        stats
    }

    fn slot_for<T: 'static>(&self, key: SlotKey) -> Option<NonNull<Slot<T>>> {
        let zone_id = key.zone_id();
        if zone_id.0 == 0 || zone_id.0 > MAX_REGISTERED_ZONES {
            return None;
        }

        let entry = self.entry_at(zone_id.0 - 1)?;
        // A stale or forged key must not resolve across zone/type boundaries.
        if entry.zone_id != zone_id || entry.type_id != TypeId::of::<T>() {
            return None;
        }

        let ptr = (entry.slot_from_key)(entry.erased, key)?;
        NonNull::new(ptr.cast::<Slot<T>>())
    }

    fn retiring_next(&self, retired: RetiredSlot) -> Option<RetiredSlot> {
        let zone_id = retired.key().zone_id();
        let index = zone_id.0.checked_sub(1)?;
        let entry = self.entry_at(index)?;
        (entry.retiring_next)(entry.erased, retired)
    }

    fn set_retiring_next(&self, retired: RetiredSlot, next: Option<RetiredSlot>) {
        let zone_id = retired.key().zone_id();
        let Some(index) = zone_id.0.checked_sub(1) else {
            return;
        };
        let Some(entry) = self.entry_at(index) else {
            return;
        };
        (entry.set_retiring_next)(entry.erased, retired, next);
    }

    unsafe fn reclaim_slot(
        &self,
        retired: RetiredSlot,
        local_guard: &mut crate::epoch::LocalRetireGuard,
    ) {
        let zone_id = retired.key().zone_id();
        let Some(index) = zone_id.0.checked_sub(1) else {
            return;
        };
        let Some(entry) = self.entry_at(index) else {
            return;
        };
        unsafe { (entry.reclaim_slot)(entry.erased, retired, local_guard) };
    }

    fn slot_lookup_debug<T: 'static>(&self, key: SlotKey) -> SlotLookupDebug {
        let requested_type = core::any::type_name::<T>();
        let zone_id = key.zone_id();
        if zone_id.0 == 0 || zone_id.0 > MAX_REGISTERED_ZONES {
            return SlotLookupDebug {
                reason: "zone-id-out-of-range",
                requested_type,
                registered_type: None,
                key,
                allocated_slots: None,
                slab_count: None,
                empty_slab_count: None,
            };
        }

        let Some(entry) = self.entry_at(zone_id.0 - 1) else {
            return SlotLookupDebug {
                reason: "registry-entry-unpublished",
                requested_type,
                registered_type: None,
                key,
                allocated_slots: None,
                slab_count: None,
                empty_slab_count: None,
            };
        };

        let info = (entry.refresh_info)(entry.erased);
        if entry.zone_id != zone_id {
            return SlotLookupDebug {
                reason: "registry-zone-id-mismatch",
                requested_type,
                registered_type: Some(info.type_name),
                key,
                allocated_slots: Some(info.allocated_slots),
                slab_count: Some(info.slab_count),
                empty_slab_count: Some(info.empty_slab_count),
            };
        }
        if entry.type_id != TypeId::of::<T>() {
            return SlotLookupDebug {
                reason: "registry-type-mismatch",
                requested_type,
                registered_type: Some(info.type_name),
                key,
                allocated_slots: Some(info.allocated_slots),
                slab_count: Some(info.slab_count),
                empty_slab_count: Some(info.empty_slab_count),
            };
        }

        let reason = if (entry.slot_from_key)(entry.erased, key).is_some() {
            "slot-resolved-on-debug-retry"
        } else {
            "keg-slab-or-slot-miss"
        };
        SlotLookupDebug {
            reason,
            requested_type,
            registered_type: Some(info.type_name),
            key,
            allocated_slots: Some(info.allocated_slots),
            slab_count: Some(info.slab_count),
            empty_slab_count: Some(info.empty_slab_count),
        }
    }

    fn entry_at(&self, index: usize) -> Option<RegisteredZone> {
        if index >= MAX_REGISTERED_ZONES {
            return None;
        }
        // Lock-free fast path: a published entry is write-once immutable
        // (see `published` field docs), so the unguarded read after the
        // Acquire gate observes the fully-written entry from `register`.
        if !self.published[index].load(Ordering::Acquire) {
            return None;
        }
        unsafe { *self.entries[index].get() }
    }

    fn clear(&self) {
        let _guard = self.lock.lock();
        for (index, entry) in self.entries.iter().enumerate() {
            self.published[index].store(false, Ordering::Release);
            unsafe {
                *entry.get() = None;
            }
        }
    }
}
