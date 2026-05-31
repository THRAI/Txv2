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
use super::retired::{append_index, PerCpuRetiredPool};
use tx_hal::{CpuId, CpuPinGuard, IrqIf, PercpuIf, SmpIf};

const INITIAL_EPOCH: u64 = 1;
const RETIRE_THRESHOLD: usize = 512;
const DEFAULT_DRAIN_BATCH: usize = 128;
const MAX_EPOCH_CPUS: usize = 64;

pub use super::retired::RETIRED_NODE_POOL_CAPACITY;

static GLOBAL_DOMAIN: EpochDomain = EpochDomain::new();
static RETIRE_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);
static DRAIN_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);
static RECLAIM_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuEpochSummary {
    pub cpu_id: CpuId,
    pub initialized: bool,
    pub local_epoch: u64,
    pub retired_count: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EpochSummary {
    pub initialized: bool,
    pub global_epoch: u64,
    pub active_guards: usize,
    pub possible_cpus: usize,
}

#[repr(align(64))]
struct CachePadded<T>(T);

impl<T> CachePadded<T> {
    const fn new(value: T) -> Self {
        Self(value)
    }
}

pub(crate) struct EpochDomain {
    /// Becomes true once BSP initialization has installed platform hooks.
    initialized: AtomicBool,
    /// Monotonic epoch used to decide when retired nodes become reclaimable.
    global_epoch: CachePadded<AtomicU64>,
    /// Debug/accounting counter for currently live guards.
    active_guards: CachePadded<AtomicUsize>,
    /// Number of CPUs the platform says may participate in EBR.
    possible_cpus: CachePadded<AtomicUsize>,
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
            global_epoch: CachePadded::new(AtomicU64::new(INITIAL_EPOCH)),
            active_guards: CachePadded::new(AtomicUsize::new(0)),
            possible_cpus: CachePadded::new(AtomicUsize::new(1)),
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

        self.global_epoch.0.store(INITIAL_EPOCH, Ordering::Release);
        self.active_guards.0.store(0, Ordering::Release);
        self.possible_cpus.0.store(possible_cpus, Ordering::Release);

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
        self.global_epoch.0.store(INITIAL_EPOCH, Ordering::Release);
        self.active_guards.0.store(0, Ordering::Release);
        self.possible_cpus.0.store(1, Ordering::Release);

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
        if cpu.0 >= self.possible_cpus.0.load(Ordering::Acquire) {
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

        let current_epoch = self.global_epoch.0.load(Ordering::Acquire);
        local.enter(current_epoch);
        self.active_guards.0.fetch_add(1, Ordering::AcqRel);
        // Publish the local epoch before any protected load can float above the
        // guard acquisition. This is the core EBR reader-side ordering rule.
        fence(Ordering::SeqCst);
        Guard::new(self, local, cpu_id, current_epoch, cpu_pin)
    }

    pub(crate) fn leave_guard(&'static self) {
        self.active_guards.0.fetch_sub(1, Ordering::AcqRel);
    }

    /// Return a borrow-mode guard for the current CPU if one is already active
    /// (local_epoch != 0).  The returned guard does not call `local.enter()` or
    /// increment `active_guards`; its Drop is a no-op.  Returns `None` when no
    /// guard is held.
    fn borrow_guard(&'static self) -> Option<Guard<'static>> {
        if !self.initialized.load(Ordering::Acquire) {
            return None;
        }
        let hooks = self.hooks();
        let cpu_pin = (hooks.pin_current_cpu)();
        let cpu_id = cpu_pin.cpu_id();
        let local = self.cpu_state(cpu_id)?;
        let local_epoch = local.current();
        if local_epoch == 0 {
            return None;
        }
        Some(Guard::new_borrowed(
            self,
            local,
            cpu_id,
            local_epoch,
            cpu_pin,
        ))
    }

    unsafe fn retire_raw(
        &'static self,
        ptr: *mut u8,
        reclaim_fn: unsafe fn(*mut u8),
    ) -> Result<(), EpochError> {
        let trace_seq = epoch_trace_sample(&RETIRE_TRACE_SAMPLE);
        if let Some(seq) = trace_seq {
            emit_epoch_trace(b"debug.epoch.retire.enter", seq);
            emit_epoch_trace(b"debug.epoch.retire.reclaim_fn", reclaim_fn as usize as i64);
        }
        if ptr.is_null() {
            if let Some(seq) = trace_seq {
                emit_epoch_trace(b"debug.epoch.retire.null", seq);
            }
            return Err(EpochError::NullPointer);
        }
        if !self.initialized.load(Ordering::Acquire) {
            if let Some(seq) = trace_seq {
                emit_epoch_trace(b"debug.epoch.retire.not_initialized", seq);
            }
            return Err(EpochError::NotInitialized);
        }

        let hooks = self.hooks();
        let cpu_pin = (hooks.pin_current_cpu)();
        let cpu_id = cpu_pin.cpu_id();
        let local = self.cpu_state(cpu_id).ok_or(EpochError::InvalidCpu)?;
        if !local.is_initialized() {
            return Err(EpochError::CpuNotInitialized);
        }

        let retired_at_epoch = self.global_epoch.0.load(Ordering::Relaxed);
        let mut state = unsafe { &mut *self.state.get() };
        // The current CPU is pinned, so this allocation touches only the
        // current CPU's independent retired pool.
        let mut index = state.alloc_node(cpu_id);
        drop(cpu_pin);

        if index.is_none() {
            if let Some(seq) = trace_seq {
                emit_epoch_trace(b"debug.epoch.retire.alloc_miss", seq);
            }
            let _ = self.try_drain(DEFAULT_DRAIN_BATCH);

            let cpu_pin = (hooks.pin_current_cpu)();
            let cpu_id = cpu_pin.cpu_id();
            let local = self.cpu_state(cpu_id).ok_or(EpochError::InvalidCpu)?;
            if !local.is_initialized() {
                return Err(EpochError::CpuNotInitialized);
            }
            state = unsafe { &mut *self.state.get() };
            index = state.alloc_node(cpu_id);

            let index = index.ok_or(EpochError::RetiredNodePoolExhausted)?;
            let pool = state
                .pool_mut(cpu_id)
                .expect("retire_raw CPU must have a retired pool");
            pool.nodes[index].ptr = ptr;
            pool.nodes[index].reclaim_fn = reclaim_fn;
            pool.nodes[index].retired_at_epoch = retired_at_epoch;

            let retired = unsafe { &mut *local.retired_ptr() };
            retired.push(&mut pool.nodes, index);
            let retired_count = retired.count;
            if let Some(seq) = trace_seq {
                emit_epoch_trace(b"debug.epoch.retire.queued", seq);
                emit_epoch_trace(b"debug.epoch.retire.local_count", retired_count as i64);
            }
            let should_try_drain = retired_count > RETIRE_THRESHOLD;
            drop(cpu_pin);
            if should_try_drain {
                if let Some(seq) = trace_seq {
                    emit_epoch_trace(b"debug.epoch.retire.threshold", seq);
                }
                let _ = self.try_drain(DEFAULT_DRAIN_BATCH);
            }
            return Ok(());
        }

        let index = index.expect("checked above");
        let pool = state
            .pool_mut(cpu_id)
            .expect("retire_raw CPU must have a retired pool");
        pool.nodes[index].ptr = ptr;
        pool.nodes[index].reclaim_fn = reclaim_fn;
        pool.nodes[index].retired_at_epoch = retired_at_epoch;

        let retired = unsafe { &mut *local.retired_ptr() };
        retired.push(&mut pool.nodes, index);
        let retired_count = retired.count;
        if let Some(seq) = trace_seq {
            emit_epoch_trace(b"debug.epoch.retire.queued", seq);
            emit_epoch_trace(b"debug.epoch.retire.local_count", retired_count as i64);
        }
        let should_try_drain = retired_count > RETIRE_THRESHOLD;

        if should_try_drain {
            if let Some(seq) = trace_seq {
                emit_epoch_trace(b"debug.epoch.retire.threshold", seq);
            }
            let _ = self.try_drain(DEFAULT_DRAIN_BATCH);
        }
        Ok(())
    }

    fn try_drain(&'static self, budget: usize) -> DrainStats {
        let trace_seq = epoch_trace_sample(&DRAIN_TRACE_SAMPLE);
        if let Some(seq) = trace_seq {
            emit_epoch_trace(b"debug.epoch.drain.enter", seq);
            emit_epoch_trace(b"debug.epoch.drain.budget", budget as i64);
        }
        if !self.initialized.load(Ordering::Acquire) {
            return DrainStats::default();
        }

        let mut stats = DrainStats {
            active_guards: self.active_guards.0.load(Ordering::Acquire),
            ..DrainStats::default()
        };

        if self.try_advance_epoch() {
            stats.advanced_epochs += 1;
        }

        // Nodes retired in epoch E are reclaimable only once the global epoch
        // reaches at least E + 2.
        let safe_epoch = self.global_epoch.0.load(Ordering::Acquire);
        let mut reclaim_head = None;
        let mut reclaim_tail = None;
        let mut reclaim_count = 0usize;

        let hooks = self.hooks();
        let cpu_pin = (hooks.pin_current_cpu)();
        let cpu_id = cpu_pin.cpu_id();
        if let Some(_seq) = trace_seq {
            emit_epoch_trace(b"debug.epoch.drain.cpu", cpu_id.0 as i64);
            emit_epoch_trace(b"debug.epoch.drain.safe_epoch", safe_epoch as i64);
        }
        if cpu_id.0 >= self.possible_cpus.0.load(Ordering::Acquire)
            || !(hooks.is_cpu_online)(cpu_id)
            || !self.cpu_states[cpu_id.0].is_initialized()
        {
            return stats;
        }

        {
            let state = unsafe { &mut *self.state.get() };
            let pool = state
                .pool_mut(cpu_id)
                .expect("try_drain CPU must have a retired pool");
            let retired = unsafe { &mut *self.cpu_states[cpu_id.0].retired_ptr() };
            let mut remaining_head = None;
            let mut remaining_tail = None;
            let mut remaining_count = 0usize;

            // Drain only the current CPU's retired list. Other CPUs reclaim
            // their own nodes when they hit their drain path.
            while let Some(index) = retired.pop(&mut pool.nodes) {
                let expired = safe_epoch >= pool.nodes[index].retired_at_epoch.saturating_add(2);
                if reclaim_count < budget && expired {
                    append_index(&mut pool.nodes, &mut reclaim_head, &mut reclaim_tail, index);
                    reclaim_count += 1;
                } else {
                    append_index(
                        &mut pool.nodes,
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
        if let Some(seq) = trace_seq {
            emit_epoch_trace(b"debug.epoch.drain.exit", seq);
            emit_epoch_trace(b"debug.epoch.drain.advanced", stats.advanced_epochs as i64);
            emit_epoch_trace(b"debug.epoch.drain.reclaimed", stats.reclaimed as i64);
            emit_epoch_trace(b"debug.epoch.drain.remaining", stats.remaining as i64);
            emit_epoch_trace(
                b"debug.epoch.drain.active_guards",
                stats.active_guards as i64,
            );
        }
        stats
    }

    fn try_advance_epoch(&'static self) -> bool {
        let current = self.global_epoch.0.load(Ordering::Acquire);
        let hooks = self.hooks();

        // Epoch advance is allowed only if every online initialized CPU is
        // either outside a guard (`local == 0`) or already in the current epoch.
        for cpu in 0..self.possible_cpus.0.load(Ordering::Acquire) {
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
            .0
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
            let pool = state
                .pool_mut(cpu)
                .expect("reclaim_list CPU must have a retired pool");
            debug_assert!(index < RETIRED_NODE_POOL_CAPACITY);
            let node = &mut pool.nodes[index];
            let ptr = node.ptr;
            let reclaim_fn = node.reclaim_fn;
            let next = node.next;
            node.next = None;

            // The callback owns object-specific destruction. The domain only
            // decides when it is safe to call it.
            let trace_seq = epoch_trace_sample(&RECLAIM_TRACE_SAMPLE);
            if let Some(seq) = trace_seq {
                emit_epoch_trace(b"debug.epoch.reclaim.begin", seq);
                emit_epoch_trace(b"debug.epoch.reclaim.fn", reclaim_fn as usize as i64);
            }
            unsafe { reclaim_fn(ptr) };
            if let Some(seq) = trace_seq {
                emit_epoch_trace(b"debug.epoch.reclaim.end", seq);
            }

            let state = unsafe { &mut *self.state.get() };
            state.free_node(cpu, index);

            reclaimed += 1;
            head = next;
        }
        reclaimed
    }

    fn cpu_state(&'static self, cpu: CpuId) -> Option<&'static CpuLocalEpochState> {
        if cpu.0 < self.possible_cpus.0.load(Ordering::Acquire) {
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
        self.global_epoch.0.store(INITIAL_EPOCH, Ordering::Release);
        self.active_guards.0.store(0, Ordering::Release);
        self.possible_cpus.0.store(1, Ordering::Release);
        let _guard = self.lock.lock();
        *self.hooks.get() = PlatformHooks::default();
        (*self.state.get()).reset();
        for cpu in 0..MAX_EPOCH_CPUS {
            self.cpu_states[cpu].reset();
        }
    }

    fn summary(&'static self) -> EpochSummary {
        EpochSummary {
            initialized: self.initialized.load(Ordering::Acquire),
            global_epoch: self.global_epoch.0.load(Ordering::Acquire),
            active_guards: self.active_guards.0.load(Ordering::Acquire),
            possible_cpus: self.possible_cpus.0.load(Ordering::Acquire),
        }
    }

    fn cpu_summary(&'static self, cpu: CpuId) -> Option<CpuEpochSummary> {
        let state = self.cpu_state(cpu)?;
        Some(CpuEpochSummary {
            cpu_id: cpu,
            initialized: state.is_initialized(),
            local_epoch: state.current(),
            retired_count: state.retired_count(),
        })
    }
}

fn epoch_trace_sample(counter: &AtomicU64) -> Option<i64> {
    let seq = counter.fetch_add(1, Ordering::Relaxed);
    (seq < 128 || seq.is_power_of_two()).then_some(seq as i64)
}

fn emit_epoch_trace(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
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
    /// One independent retired-node pool per CPU.
    pools: [PerCpuRetiredPool; MAX_EPOCH_CPUS],
}

impl DomainState {
    const fn new() -> Self {
        Self {
            pools: [const { PerCpuRetiredPool::new() }; MAX_EPOCH_CPUS],
        }
    }

    fn reset(&mut self) {
        for cpu in 0..MAX_EPOCH_CPUS {
            self.pools[cpu].reset();
        }
    }

    fn alloc_node(&mut self, cpu: CpuId) -> Option<usize> {
        self.pools.get_mut(cpu.0)?.alloc_node()
    }

    fn free_node(&mut self, cpu: CpuId, index: usize) {
        self.pools[cpu.0].free_node(index);
    }

    fn pool_mut(&mut self, cpu: CpuId) -> Option<&mut PerCpuRetiredPool> {
        self.pools.get_mut(cpu.0)
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

pub(crate) fn borrow_guard() -> Option<Guard<'static>> {
    GLOBAL_DOMAIN.borrow_guard()
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

pub fn summary() -> EpochSummary {
    GLOBAL_DOMAIN.summary()
}

pub fn cpu_summary(cpu: CpuId) -> Option<CpuEpochSummary> {
    GLOBAL_DOMAIN.cpu_summary(cpu)
}

#[doc(hidden)]
pub unsafe fn reset_for_test() {
    GLOBAL_DOMAIN.reset_for_test();
}

#[doc(hidden)]
pub fn init_for_test() {
    GLOBAL_DOMAIN.init_for_test();
}
