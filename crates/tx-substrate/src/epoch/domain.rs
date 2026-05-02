//! Global epoch domain.
//!
//! The domain owns the global epoch counter, per-CPU local epoch records, and
//! delayed reclamation queues. Readers enter the domain by creating a `Guard`;
//! destructors for retired objects run only after every online CPU has either
//! left its guard or advanced past the retire epoch.

use core::cell::UnsafeCell;
use core::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::guard::Guard;
use super::local::CpuLocalEpochState;
use super::retired::{append_index, RetiredNode};
use tx_hal::{CpuId, CpuPinGuard, IrqIf, PercpuIf, SmpIf};

const INITIAL_EPOCH: u64 = 1;
const RETIRE_THRESHOLD: usize = 64;
const DEFAULT_DRAIN_BATCH: usize = 32;
const MAX_EPOCH_CPUS: usize = 64;

pub use super::retired::RETIRED_NODE_POOL_CAPACITY;

static GLOBAL_DOMAIN: EpochDomain = EpochDomain::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpochError {
    AlreadyInitialized,
    NotInitialized,
    RetiredNodePoolExhausted,
    NullPointer,
    TooManyCpus,
    InvalidCpu,
    CpuNotInitialized,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrainStats {
    pub advanced_epochs: usize,
    pub reclaimed: usize,
    pub remaining: usize,
    pub active_guards: usize,
}

pub(crate) struct EpochDomain {
    /// Becomes true once BSP initialization has installed platform hooks.
    initialized: AtomicBool,
    /// Monotonic epoch used to decide when retired nodes become reclaimable.
    global_epoch: AtomicU64,
    /// Debug/accounting counter for currently live guards.
    active_guards: AtomicUsize,
    /// Number of CPUs the platform says may participate in EBR.
    possible_cpus: AtomicUsize,
    /// Platform callbacks are installed once at BSP init and then read lock-free.
    hooks: UnsafeCell<PlatformHooks>,
    /// Per-CPU guard state and retired-node list heads.
    cpu_states: [CpuLocalEpochState; MAX_EPOCH_CPUS],
    /// Protects initialization and hook replacement; hot retire/drain paths use
    /// CPU pinning and per-CPU node ranges instead of this lock.
    lock: SpinLock,
    /// Backing storage for retired nodes. It is physically one array, but split
    /// into fixed per-CPU slices so each CPU mutates only its own freelist.
    state: UnsafeCell<DomainState>,
}

unsafe impl Sync for EpochDomain {}

impl EpochDomain {
    const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            global_epoch: AtomicU64::new(INITIAL_EPOCH),
            active_guards: AtomicUsize::new(0),
            possible_cpus: AtomicUsize::new(1),
            hooks: UnsafeCell::new(PlatformHooks::default()),
            cpu_states: [const { CpuLocalEpochState::new() }; MAX_EPOCH_CPUS],
            lock: SpinLock::new(),
            state: UnsafeCell::new(DomainState::new()),
        }
    }

    fn init_on_bsp<P>(&'static self) -> Result<(), EpochError>
    where
        P: PercpuIf + SmpIf + IrqIf,
    {
        let possible_cpus = P::possible_cpu_count();
        if possible_cpus == 0 || possible_cpus > MAX_EPOCH_CPUS {
            return Err(EpochError::TooManyCpus);
        }

        self.initialized
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| EpochError::AlreadyInitialized)?;

        self.global_epoch.store(INITIAL_EPOCH, Ordering::Release);
        self.active_guards.store(0, Ordering::Release);
        self.possible_cpus.store(possible_cpus, Ordering::Release);

        let _guard = self.lock.lock();
        unsafe {
            *self.hooks.get() = PlatformHooks::for_platform::<P>();
            (*self.state.get()).reset();
        }

        for cpu in 0..MAX_EPOCH_CPUS {
            self.cpu_states[cpu].reset();
        }

        self.init_cpu(<P as PercpuIf>::current_cpu_id())
    }

    fn init_for_test(&'static self) {
        self.initialized.store(true, Ordering::Release);
        self.global_epoch.store(INITIAL_EPOCH, Ordering::Release);
        self.active_guards.store(0, Ordering::Release);
        self.possible_cpus.store(1, Ordering::Release);

        let _guard = self.lock.lock();
        unsafe {
            *self.hooks.get() = PlatformHooks::default();
            (*self.state.get()).reset();
        }

        for cpu in 0..MAX_EPOCH_CPUS {
            self.cpu_states[cpu].reset();
        }
        self.cpu_states[0].init();
    }

    fn init_on_ap(&'static self, cpu: CpuId) -> Result<(), EpochError> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(EpochError::NotInitialized);
        }
        self.init_cpu(cpu)
    }

    fn init_cpu(&'static self, cpu: CpuId) -> Result<(), EpochError> {
        if cpu.0 >= self.possible_cpus.load(Ordering::Acquire) {
            return Err(EpochError::InvalidCpu);
        }
        self.cpu_states[cpu.0].init();
        Ok(())
    }

    fn guard(&'static self) -> Guard<'static> {
        assert!(
            self.initialized.load(Ordering::Acquire),
            "epoch::guard called before epoch::init_on_bsp"
        );

        let hooks = self.hooks();
        debug_assert!(
            !(hooks.in_irq_context)(),
            "epoch guards must not be created in IRQ context"
        );

        let cpu_pin = (hooks.pin_current_cpu)();
        let cpu_id = cpu_pin.cpu_id();
        let local = self
            .cpu_state(cpu_id)
            .expect("epoch::guard current CPU is outside initialized epoch range");
        assert!(
            local.is_initialized(),
            "epoch::guard current CPU has not called epoch::init_on_ap/init_on_bsp"
        );

        let current_epoch = self.global_epoch.load(Ordering::Acquire);
        local.enter(current_epoch);
        self.active_guards.fetch_add(1, Ordering::AcqRel);
        // Publish the local epoch before any protected load can float above the
        // guard acquisition. This is the core EBR reader-side ordering rule.
        fence(Ordering::SeqCst);
        Guard::new(self, local, cpu_id, current_epoch, cpu_pin)
    }

    pub(crate) fn leave_guard(&'static self) {
        self.active_guards.fetch_sub(1, Ordering::AcqRel);
    }

    unsafe fn retire_raw(
        &'static self,
        ptr: *mut u8,
        reclaim_fn: unsafe fn(*mut u8),
    ) -> Result<(), EpochError> {
        if ptr.is_null() {
            return Err(EpochError::NullPointer);
        }
        if !self.initialized.load(Ordering::Acquire) {
            return Err(EpochError::NotInitialized);
        }

        let hooks = self.hooks();
        let cpu_pin = (hooks.pin_current_cpu)();
        let cpu_id = cpu_pin.cpu_id();
        let local = self.cpu_state(cpu_id).ok_or(EpochError::InvalidCpu)?;
        if !local.is_initialized() {
            return Err(EpochError::CpuNotInitialized);
        }

        let retired_at_epoch = self.global_epoch.load(Ordering::Relaxed);
        let state = unsafe { &mut *self.state.get() };
        // The current CPU is pinned, so this allocation touches only the
        // current CPU's fixed node slice.
        let index = state
            .alloc_node(cpu_id)
            .ok_or(EpochError::RetiredNodePoolExhausted)?;
        state.nodes[index].ptr = ptr;
        state.nodes[index].reclaim_fn = reclaim_fn;
        state.nodes[index].retired_at_epoch = retired_at_epoch;

        let retired = unsafe { &mut *local.retired_ptr() };
        retired.push(&mut state.nodes, index);
        let should_try_drain = retired.count > RETIRE_THRESHOLD;

        drop(cpu_pin);
        if should_try_drain {
            let _ = self.try_drain(DEFAULT_DRAIN_BATCH);
        }
        Ok(())
    }

    fn try_drain(&'static self, budget: usize) -> DrainStats {
        if !self.initialized.load(Ordering::Acquire) {
            return DrainStats::default();
        }

        let mut stats = DrainStats {
            active_guards: self.active_guards.load(Ordering::Acquire),
            ..DrainStats::default()
        };

        if self.try_advance_epoch() {
            stats.advanced_epochs += 1;
        }

        // Nodes retired in epoch E are reclaimable only once the global epoch
        // reaches at least E + 2.
        let safe_epoch = self.global_epoch.load(Ordering::Acquire);
        let mut reclaim_head = None;
        let mut reclaim_tail = None;
        let mut reclaim_count = 0usize;

        let hooks = self.hooks();
        let cpu_pin = (hooks.pin_current_cpu)();
        let cpu_id = cpu_pin.cpu_id();
        if cpu_id.0 >= self.possible_cpus.load(Ordering::Acquire)
            || !(hooks.is_cpu_online)(cpu_id)
            || !self.cpu_states[cpu_id.0].is_initialized()
        {
            return stats;
        }

        {
            let state = unsafe { &mut *self.state.get() };
            let retired = unsafe { &mut *self.cpu_states[cpu_id.0].retired_ptr() };
            let mut remaining_head = None;
            let mut remaining_tail = None;
            let mut remaining_count = 0usize;

            // Drain only the current CPU's retired list. Other CPUs reclaim
            // their own nodes when they hit their drain path.
            while let Some(index) = retired.pop(&mut state.nodes) {
                let expired = safe_epoch >= state.nodes[index].retired_at_epoch.saturating_add(2);
                if reclaim_count < budget && expired {
                    append_index(
                        &mut state.nodes,
                        &mut reclaim_head,
                        &mut reclaim_tail,
                        index,
                    );
                    reclaim_count += 1;
                } else {
                    append_index(
                        &mut state.nodes,
                        &mut remaining_head,
                        &mut remaining_tail,
                        index,
                    );
                    remaining_count += 1;
                }
            }

            retired.head = remaining_head;
            retired.count = remaining_count;
            stats.remaining = remaining_count;
        }

        stats.reclaimed = self.reclaim_list(cpu_id, reclaim_head);
        stats
    }

    fn try_advance_epoch(&'static self) -> bool {
        let current = self.global_epoch.load(Ordering::Acquire);
        let hooks = self.hooks();

        // Epoch advance is allowed only if every online initialized CPU is
        // either outside a guard (`local == 0`) or already in the current epoch.
        for cpu in 0..self.possible_cpus.load(Ordering::Acquire) {
            let cpu_id = CpuId(cpu);
            if !(hooks.is_cpu_online)(cpu_id) || !self.cpu_states[cpu].is_initialized() {
                continue;
            }

            let local = self.cpu_states[cpu].current();
            if local != 0 && local < current {
                return false;
            }
        }

        self.global_epoch
            .compare_exchange(
                current,
                current.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    fn reclaim_list(&'static self, cpu: CpuId, mut head: Option<usize>) -> usize {
        let mut reclaimed = 0usize;
        while let Some(index) = head {
            let state = unsafe { &mut *self.state.get() };
            debug_assert!(state.index_belongs_to_cpu(cpu, index));
            let node = &mut state.nodes[index];
            let ptr = node.ptr;
            let reclaim_fn = node.reclaim_fn;
            let next = node.next;
            node.next = None;

            // The callback owns object-specific destruction. The domain only
            // decides when it is safe to call it.
            unsafe { reclaim_fn(ptr) };

            let state = unsafe { &mut *self.state.get() };
            state.free_node(cpu, index);

            reclaimed += 1;
            head = next;
        }
        reclaimed
    }

    fn cpu_state(&'static self, cpu: CpuId) -> Option<&'static CpuLocalEpochState> {
        if cpu.0 < self.possible_cpus.load(Ordering::Acquire) {
            Some(&self.cpu_states[cpu.0])
        } else {
            None
        }
    }

    fn hooks(&'static self) -> PlatformHooks {
        unsafe { *self.hooks.get() }
    }

    unsafe fn reset_for_test(&'static self) {
        self.initialized.store(false, Ordering::Release);
        self.global_epoch.store(INITIAL_EPOCH, Ordering::Release);
        self.active_guards.store(0, Ordering::Release);
        self.possible_cpus.store(1, Ordering::Release);
        let _guard = self.lock.lock();
        *self.hooks.get() = PlatformHooks::default();
        (*self.state.get()).reset();
        for cpu in 0..MAX_EPOCH_CPUS {
            self.cpu_states[cpu].reset();
        }
    }
}

#[derive(Clone, Copy)]
struct PlatformHooks {
    pin_current_cpu: fn() -> CpuPinGuard,
    in_irq_context: fn() -> bool,
    is_cpu_online: fn(CpuId) -> bool,
}

impl PlatformHooks {
    const fn default() -> Self {
        Self {
            pin_current_cpu: default_pin_current_cpu,
            in_irq_context: default_in_irq_context,
            is_cpu_online: default_is_cpu_online,
        }
    }

    fn for_platform<P>() -> Self
    where
        P: PercpuIf + SmpIf + IrqIf,
    {
        Self {
            pin_current_cpu: P::pin_current_cpu,
            in_irq_context: P::in_irq_context,
            is_cpu_online: P::is_cpu_online,
        }
    }
}

fn default_pin_current_cpu() -> CpuPinGuard {
    CpuPinGuard::new(CpuId(0))
}

fn default_in_irq_context() -> bool {
    false
}

fn default_is_cpu_online(cpu: CpuId) -> bool {
    cpu.0 == 0
}

struct DomainState {
    /// Retired-node storage split into `MAX_EPOCH_CPUS` equal ranges.
    nodes: [RetiredNode; RETIRED_NODE_POOL_CAPACITY * MAX_EPOCH_CPUS],
    /// One freelist head per CPU range.
    free_heads: [Option<usize>; MAX_EPOCH_CPUS],
}

impl DomainState {
    const fn new() -> Self {
        Self {
            nodes: [const { RetiredNode::empty() }; RETIRED_NODE_POOL_CAPACITY * MAX_EPOCH_CPUS],
            free_heads: [None; MAX_EPOCH_CPUS],
        }
    }

    fn reset(&mut self) {
        for cpu in 0..MAX_EPOCH_CPUS {
            let start = Self::cpu_start(CpuId(cpu));
            let end = start + RETIRED_NODE_POOL_CAPACITY;
            for index in start..end {
                self.nodes[index].reset();
                self.nodes[index].next = if index + 1 < end {
                    Some(index + 1)
                } else {
                    None
                };
            }
            self.free_heads[cpu] = Some(start);
        }
    }

    fn alloc_node(&mut self, cpu: CpuId) -> Option<usize> {
        let index = self.free_heads.get(cpu.0).copied().flatten()?;
        debug_assert!(self.index_belongs_to_cpu(cpu, index));
        self.free_heads[cpu.0] = self.nodes[index].next;
        self.nodes[index].reset();
        Some(index)
    }

    fn free_node(&mut self, cpu: CpuId, index: usize) {
        debug_assert!(self.index_belongs_to_cpu(cpu, index));
        self.nodes[index].reset();
        self.nodes[index].next = self.free_heads[cpu.0];
        self.free_heads[cpu.0] = Some(index);
    }

    const fn cpu_start(cpu: CpuId) -> usize {
        cpu.0 * RETIRED_NODE_POOL_CAPACITY
    }

    fn index_belongs_to_cpu(&self, cpu: CpuId, index: usize) -> bool {
        let start = Self::cpu_start(cpu);
        index >= start && index < start + RETIRED_NODE_POOL_CAPACITY
    }
}

struct SpinLock {
    held: AtomicBool,
}

impl SpinLock {
    const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> SpinGuard<'_> {
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self }
    }
}

struct SpinGuard<'a> {
    lock: &'a SpinLock,
}

impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}

pub fn init_on_bsp<P>() -> Result<(), EpochError>
where
    P: PercpuIf + SmpIf + IrqIf,
{
    GLOBAL_DOMAIN.init_on_bsp::<P>()
}

pub fn init_on_ap(cpu: CpuId) -> Result<(), EpochError> {
    GLOBAL_DOMAIN.init_on_ap(cpu)
}

pub(crate) fn guard() -> Guard<'static> {
    GLOBAL_DOMAIN.guard()
}

pub(crate) unsafe fn retire_raw(
    ptr: *mut u8,
    reclaim_fn: unsafe fn(*mut u8),
) -> Result<(), EpochError> {
    GLOBAL_DOMAIN.retire_raw(ptr, reclaim_fn)
}

pub fn try_drain(budget: usize) -> DrainStats {
    GLOBAL_DOMAIN.try_drain(budget)
}

#[doc(hidden)]
pub unsafe fn reset_for_test() {
    GLOBAL_DOMAIN.reset_for_test();
}

#[doc(hidden)]
pub fn init_for_test() {
    GLOBAL_DOMAIN.init_for_test();
}
