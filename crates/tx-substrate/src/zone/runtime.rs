//! Runtime hooks needed by the zone allocator.
//!
//! Zone itself is platform-neutral, but slab allocation needs the platform page
//! size, direct-map base, and CPU-pinning hook. BSP init installs those facts
//! once before any zone reservation can run.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use tx_hal::{
    CpuId, CpuPinGuard, CpuPinReason, IrqIf, LocalExecutionGuard, PercpuIf, PhysAddr, SmpIf,
    TxPlatform,
};

use super::ZoneError;

pub const MAX_ZONE_CPUS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZoneRuntimeState {
    NotReady = 0,
    Running = 1,
    FrozenForShutdown = 2,
}

static ZONE_RUNTIME_INITIALIZED: AtomicBool = AtomicBool::new(false);
static ZONE_RUNTIME_STATE: AtomicUsize = AtomicUsize::new(ZoneRuntimeState::NotReady as usize);
/// Raw CPU-ID mask for CPUs that may access per-CPU buckets. The mask may be
/// sparse, so its population count cannot be used as an array bound.
static POSSIBLE_CPU_MASK: AtomicU64 = AtomicU64::new(1);
/// Runtime page size used for slab layout and page-base recovery.
static PAGE_SIZE: AtomicUsize = AtomicUsize::new(4096);
/// Base virtual address of the direct map.
static DIRECT_MAP_BASE: AtomicUsize = AtomicUsize::new(0);
static HOOKS: RuntimeHookCell = RuntimeHookCell::new();

struct RuntimeHookCell(UnsafeCell<RuntimeHooks>);

unsafe impl Sync for RuntimeHookCell {}

impl RuntimeHookCell {
    const fn new() -> Self {
        Self(UnsafeCell::new(RuntimeHooks::default()))
    }

    unsafe fn get(&self) -> *mut RuntimeHooks {
        self.0.get()
    }
}

pub fn init_on_bsp<P: TxPlatform>() -> Result<(), ZoneError> {
    let possible_cpu_mask = P::possible_cpus();
    let possible_cpu_count = possible_cpu_mask.count();
    if possible_cpu_count == 0
        || possible_cpu_count > MAX_ZONE_CPUS
        || !possible_cpu_mask.contains(<P as PercpuIf>::current_cpu_id())
    {
        return Err(ZoneError::InvalidState);
    }

    ZONE_RUNTIME_INITIALIZED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| ZoneError::AlreadyInitialized)?;

    POSSIBLE_CPU_MASK.store(possible_cpu_mask.bits(), Ordering::Release);
    PAGE_SIZE.store(P::PAGE_SIZE, Ordering::Release);
    DIRECT_MAP_BASE.store(P::DIRECT_MAP_BASE.0, Ordering::Release);
    unsafe {
        *HOOKS.get() = RuntimeHooks::for_platform::<P>();
    }
    // The registry is a boot-time directory; reset it whenever the zone runtime
    // is initialized from a clean BSP boot.
    super::registry::init_registry();
    ZONE_RUNTIME_STATE.store(ZoneRuntimeState::Running as usize, Ordering::Release);
    Ok(())
}

#[doc(hidden)]
pub fn init_for_test(page_size: usize, direct_map_base: usize) -> Result<(), ZoneError> {
    init_for_test_with_possible_cpus(page_size, direct_map_base, 1)
}

#[doc(hidden)]
pub fn init_for_test_with_possible_cpus(
    page_size: usize,
    direct_map_base: usize,
    possible_cpus: usize,
) -> Result<(), ZoneError> {
    if possible_cpus == 0 || possible_cpus > MAX_ZONE_CPUS {
        return Err(ZoneError::InvalidState);
    }
    ZONE_RUNTIME_INITIALIZED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| ZoneError::AlreadyInitialized)?;

    let possible_cpu_bits = if possible_cpus == MAX_ZONE_CPUS {
        u64::MAX
    } else {
        (1u64 << possible_cpus) - 1
    };
    POSSIBLE_CPU_MASK.store(possible_cpu_bits, Ordering::Release);
    PAGE_SIZE.store(page_size, Ordering::Release);
    DIRECT_MAP_BASE.store(direct_map_base, Ordering::Release);
    unsafe {
        *HOOKS.get() = RuntimeHooks::default();
    }
    super::registry::init_registry();
    ZONE_RUNTIME_STATE.store(ZoneRuntimeState::Running as usize, Ordering::Release);
    Ok(())
}

pub fn init_on_ap(cpu: CpuId) -> Result<(), ZoneError> {
    // `Running` is published only after the mask, address facts, hooks, and
    // registry have all been installed by the BSP.
    ensure_running()?;
    if !is_possible_cpu(cpu) {
        return Err(ZoneError::InvalidState);
    }
    // Once all known zones are registered, each AP gets an empty local bucket
    // for every zone.
    super::registry::init_cpu_buckets(cpu)
}

pub fn ensure_initialized() -> Result<(), ZoneError> {
    if ZONE_RUNTIME_INITIALIZED.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(ZoneError::NotInitialized)
    }
}

pub fn ensure_running() -> Result<(), ZoneError> {
    ensure_initialized()?;
    match state() {
        ZoneRuntimeState::Running => Ok(()),
        ZoneRuntimeState::FrozenForShutdown => Err(ZoneError::FrozenForShutdown),
        ZoneRuntimeState::NotReady => Err(ZoneError::NotInitialized),
    }
}

pub fn is_initialized() -> bool {
    ZONE_RUNTIME_INITIALIZED.load(Ordering::Acquire)
}

pub fn state() -> ZoneRuntimeState {
    match ZONE_RUNTIME_STATE.load(Ordering::Acquire) {
        x if x == ZoneRuntimeState::Running as usize => ZoneRuntimeState::Running,
        x if x == ZoneRuntimeState::FrozenForShutdown as usize => {
            ZoneRuntimeState::FrozenForShutdown
        }
        _ => ZoneRuntimeState::NotReady,
    }
}

pub fn freeze_for_shutdown() -> Result<(), ZoneError> {
    ensure_initialized()?;
    loop {
        match state() {
            ZoneRuntimeState::NotReady => return Err(ZoneError::NotInitialized),
            ZoneRuntimeState::FrozenForShutdown => return Ok(()),
            ZoneRuntimeState::Running => {
                if ZONE_RUNTIME_STATE
                    .compare_exchange(
                        ZoneRuntimeState::Running as usize,
                        ZoneRuntimeState::FrozenForShutdown as usize,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return Ok(());
                }
            }
        }
    }
}

pub fn pin_current_cpu(reason: CpuPinReason) -> Result<CpuPinGuard, ZoneError> {
    ensure_running()?;
    let guard = unsafe { ((*HOOKS.get()).pin_current_cpu)(reason) };
    if is_possible_cpu(guard.cpu_id()) {
        Ok(guard)
    } else {
        Err(ZoneError::InvalidState)
    }
}

pub(crate) fn exclude_local_execution() -> LocalExecutionGuard {
    unsafe { ((*HOOKS.get()).exclude_local_execution)() }
}

pub fn page_size() -> usize {
    PAGE_SIZE.load(Ordering::Acquire)
}

pub fn direct_map_ptr(phys: PhysAddr) -> Result<*mut u8, ZoneError> {
    let base = DIRECT_MAP_BASE.load(Ordering::Acquire);
    if base == 0 {
        return Err(ZoneError::NotInitialized);
    }
    // ZoneSlab pages are accessed through the permanent direct map.
    Ok(base
        .checked_add(phys.0)
        .ok_or(ZoneError::AllocationFailed)? as *mut u8)
}

#[doc(hidden)]
pub fn set_direct_map_base_for_test(direct_map_base: usize) -> Result<(), ZoneError> {
    ensure_running()?;
    if direct_map_base == 0 {
        return Err(ZoneError::InvalidState);
    }
    DIRECT_MAP_BASE.store(direct_map_base, Ordering::Release);
    Ok(())
}

#[doc(hidden)]
pub unsafe fn reset_for_test() {
    ZONE_RUNTIME_INITIALIZED.store(false, Ordering::Release);
    ZONE_RUNTIME_STATE.store(ZoneRuntimeState::NotReady as usize, Ordering::Release);
    POSSIBLE_CPU_MASK.store(1, Ordering::Release);
    PAGE_SIZE.store(4096, Ordering::Release);
    DIRECT_MAP_BASE.store(0, Ordering::Release);
    unsafe {
        *HOOKS.get() = RuntimeHooks::default();
    }
}

struct RuntimeHooks {
    pin_current_cpu: fn(CpuPinReason) -> CpuPinGuard,
    exclude_local_execution: fn() -> LocalExecutionGuard,
}

impl RuntimeHooks {
    const fn default() -> Self {
        Self {
            pin_current_cpu: default_pin_current_cpu,
            exclude_local_execution: default_exclude_local_execution,
        }
    }

    fn for_platform<P>() -> Self
    where
        P: IrqIf + PercpuIf + SmpIf,
    {
        Self {
            pin_current_cpu: P::pin_current_cpu_for,
            exclude_local_execution: P::exclude_local_execution,
        }
    }
}

fn default_pin_current_cpu(reason: CpuPinReason) -> CpuPinGuard {
    CpuPinGuard::new(CpuId(0)).with_reason(reason)
}

fn default_exclude_local_execution() -> LocalExecutionGuard {
    unsafe { LocalExecutionGuard::new(0, default_restore_local_execution) }
}

unsafe fn default_restore_local_execution(_saved_state: usize) {}

fn is_possible_cpu(cpu: CpuId) -> bool {
    cpu.0 < MAX_ZONE_CPUS
        && tx_hal::CpuMask::from_bits(POSSIBLE_CPU_MASK.load(Ordering::Acquire)).contains(cpu)
}
