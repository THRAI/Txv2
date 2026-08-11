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
use super::retired::{allocate_bag_page, BagQueue, RetiredEntry, SealedBag, RETIRED_BAG_CAPACITY};
use tx_hal::{CpuId, CpuPinGuard, CpuPinReason, IrqIf, PercpuIf, SmpIf};

const INITIAL_EPOCH: u64 = 1;
const COLLECT_STEPS: usize = 8;
const PERIODIC_COLLECT_BUDGET: usize = RETIRED_BAG_CAPACITY * COLLECT_STEPS;
const MAX_EPOCH_CPUS: usize = 64;

static GLOBAL_DOMAIN: EpochDomain = EpochDomain::new();
static RETIRE_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);
static DRAIN_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);
static RECLAIM_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpochError {
    AlreadyInitialized,
    NotInitialized,
    RetiredBagAllocationFailed,
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
    pub retired_count: usize,
    pub collection_requested: bool,
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
    /// Debug/accounting counter for currently pinned CPU participants.
    active_guards: CachePadded<AtomicUsize>,
    /// Raw CPU-ID mask for CPUs that may participate in EBR. Hardware hart
    /// IDs may be sparse (for example, VF2 application harts are 1..=4), so
    /// the population count is not a valid per-CPU array bound.
    possible_cpu_mask: CachePadded<AtomicU64>,
    /// Set by retirement/pin hot paths and consumed at a normal-stack drain
    /// point. Trap/IRQ paths may request collection but never execute
    /// destructor callbacks themselves.
    collection_requested: AtomicBool,
    /// Platform callbacks are installed once at BSP init and then read lock-free.
    hooks: UnsafeCell<PlatformHooks>,
    /// Per-CPU guard state and retired-node list heads.
    cpu_states: [CpuLocalEpochState; MAX_EPOCH_CPUS],
    /// Protects initialization and hook replacement.
    lock: SpinLock,
    /// Serializes short shared sealed-bag queue operations.
    queue_lock: SpinLock,
    /// Allows one collector to execute callbacks at a time without holding the
    /// queue lock across arbitrary destructors.
    collect_lock: SpinLock,
    /// Shared sealed-bag queue and recycled page-backed bag cache.
    state: UnsafeCell<DomainState>,
}

unsafe impl Sync for EpochDomain {}

impl EpochDomain {
    const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            global_epoch: CachePadded::new(AtomicU64::new(INITIAL_EPOCH)),
            active_guards: CachePadded::new(AtomicUsize::new(0)),
            possible_cpu_mask: CachePadded::new(AtomicU64::new(1)),
            collection_requested: AtomicBool::new(false),
            hooks: UnsafeCell::new(PlatformHooks::default()),
            cpu_states: [const { CpuLocalEpochState::new() }; MAX_EPOCH_CPUS],
            lock: SpinLock::new(),
            queue_lock: SpinLock::new(),
            collect_lock: SpinLock::new(),
            state: UnsafeCell::new(DomainState::new()),
        }
    }

    fn init_on_bsp<P>(&'static self) -> Result<(), EpochError>
    where
        P: PercpuIf + SmpIf + IrqIf,
    {
        let possible_cpu_mask = P::possible_cpus();
        let possible_cpu_count = possible_cpu_mask.count();
        if possible_cpu_count == 0 || possible_cpu_count > MAX_EPOCH_CPUS {
            return Err(EpochError::TooManyCpus);
        }
        let current_cpu = <P as PercpuIf>::current_cpu_id();
        if current_cpu.0 >= MAX_EPOCH_CPUS || !possible_cpu_mask.contains(current_cpu) {
            return Err(EpochError::InvalidCpu);
        }

        let _guard = self.lock.lock();
        if self.initialized.load(Ordering::Acquire) {
            return Err(EpochError::AlreadyInitialized);
        }

        self.global_epoch.0.store(INITIAL_EPOCH, Ordering::Release);
        self.active_guards.0.store(0, Ordering::Release);
        self.possible_cpu_mask
            .0
            .store(possible_cpu_mask.bits(), Ordering::Release);
        self.collection_requested.store(false, Ordering::Release);

        unsafe {
            *self.hooks.get() = PlatformHooks::for_platform::<P>();
            (*self.state.get()).reset();
        }

        for cpu in 0..MAX_EPOCH_CPUS {
            self.cpu_states[cpu].reset();
        }

        self.init_cpu(current_cpu)?;
        // Publish only after the mask, hooks, queue, and BSP-local state are
        // fully installed. AP initialization and guard acquisition use
        // Acquire loads of this flag.
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    fn init_for_test(&'static self) {
        let _guard = self.lock.lock();
        self.initialized.store(false, Ordering::Release);
        self.global_epoch.0.store(INITIAL_EPOCH, Ordering::Release);
        self.active_guards.0.store(0, Ordering::Release);
        self.possible_cpu_mask.0.store(1, Ordering::Release);
        self.collection_requested.store(false, Ordering::Release);

        unsafe {
            *self.hooks.get() = PlatformHooks::default();
            (*self.state.get()).reset();
        }

        for cpu in 0..MAX_EPOCH_CPUS {
            self.cpu_states[cpu].reset();
        }
        self.cpu_states[0].init();
        self.initialized.store(true, Ordering::Release);
    }

    fn init_on_ap(&'static self, cpu: CpuId) -> Result<(), EpochError> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(EpochError::NotInitialized);
        }
        self.init_cpu(cpu)
    }

    fn init_cpu(&'static self, cpu: CpuId) -> Result<(), EpochError> {
        if !self.is_possible_cpu(cpu) {
            return Err(EpochError::InvalidCpu);
        }
        self.cpu_states[cpu.0].init();
        Ok(())
    }

    #[track_caller]
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

        let cpu_pin = (hooks.pin_current_cpu)(CpuPinReason::EpochGuard);
        let cpu_id = cpu_pin.cpu_id();
        let local = self
            .cpu_state(cpu_id)
            .expect("epoch::guard current CPU is outside initialized epoch range");
        assert!(
            local.is_initialized(),
            "epoch::guard current CPU has not called epoch::init_on_ap/init_on_bsp"
        );

        let current_epoch = self.global_epoch.0.load(Ordering::Acquire);
        let (entered_epoch, outermost) = local.pin(current_epoch);
        let should_collect = if outermost {
            self.active_guards.0.fetch_add(1, Ordering::AcqRel);
            // Publish the local epoch before any protected load can float above
            // the guard acquisition. Nested guards reuse this published window.
            fence(Ordering::SeqCst);
            local.note_pin_and_should_collect()
        } else {
            false
        };
        let guard = Guard::new(self, local, cpu_id, entered_epoch, cpu_pin);
        if should_collect {
            // Crossbeam performs this cold-path collection on an ordinary
            // userspace thread stack. Txv2 can enter guard() from a syscall
            // trap stack, so only publish a maintenance request here. The
            // reactor consumes it after the trap longjmp returns to its normal
            // kernel stack.
            self.request_collection();
        }
        guard
    }

    pub(crate) fn leave_guard(&'static self) {
        self.active_guards.0.fetch_sub(1, Ordering::AcqRel);
    }

    /// Return a real nested guard for the current CPU if one is already active.
    /// The nested guard increments/decrements the CPU-local pin depth but does
    /// not republish the epoch, increment `active_guards`, or trigger periodic
    /// collection. Returns `None` when no guard is held.
    fn borrow_guard(&'static self) -> Option<Guard<'static>> {
        if !self.initialized.load(Ordering::Acquire) {
            return None;
        }
        let hooks = self.hooks();
        let cpu_pin = (hooks.pin_current_cpu)(CpuPinReason::EpochBorrow);
        let cpu_id = cpu_pin.cpu_id();
        let local = self.cpu_state(cpu_id)?;
        if !local.is_pinned() {
            return None;
        }
        let current_epoch = self.global_epoch.0.load(Ordering::Acquire);
        let (entered_epoch, outermost) = local.pin(current_epoch);
        assert!(
            !outermost,
            "active epoch participant became quiescent while CPU-pinned"
        );
        Some(Guard::new(self, local, cpu_id, entered_epoch, cpu_pin))
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
        let mut retried_after_drain = false;
        loop {
            let cpu_pin = (hooks.pin_current_cpu)(CpuPinReason::EpochRetire);
            let cpu_id = cpu_pin.cpu_id();
            let local = self.cpu_state(cpu_id).ok_or(EpochError::InvalidCpu)?;
            if !local.is_initialized() {
                return Err(EpochError::CpuNotInitialized);
            }

            let local_bag = unsafe { &mut *local.bag_ptr() };
            if local_bag.is_full() {
                let seal_result = self.seal_local_bag(cpu_id, local_bag);
                if seal_result.is_err() && !retried_after_drain {
                    drop(cpu_pin);
                    if let Some(seq) = trace_seq {
                        emit_epoch_trace(b"debug.epoch.retire.alloc_miss", seq);
                    }
                    let _ = self.try_drain_inner(PERIODIC_COLLECT_BUDGET, false);
                    retried_after_drain = true;
                    continue;
                }
                seal_result?;
            }

            local_bag.push(RetiredEntry { ptr, reclaim_fn });
            local.note_retired();
            let retired_count = local.retired_count();
            if let Some(seq) = trace_seq {
                emit_epoch_trace(b"debug.epoch.retire.queued", seq);
                emit_epoch_trace(b"debug.epoch.retire.local_count", retired_count as i64);
            }
            return Ok(());
        }
    }

    fn seal_local_bag(
        &'static self,
        cpu_id: CpuId,
        local_bag: &mut super::retired::LocalBag,
    ) -> Result<(), EpochError> {
        if local_bag.is_empty() {
            return Ok(());
        }

        let cached = {
            let _guard = self.queue_lock.lock();
            unsafe { &mut *self.state.get() }.queue.take_cached()
        };
        let bag = match cached {
            Some(bag) => bag,
            None => allocate_bag_page().map_err(|_| EpochError::RetiredBagAllocationFailed)?,
        };

        // Crossbeam seals a local bag only after a SeqCst publication fence,
        // then stamps it with the current global epoch.
        fence(Ordering::SeqCst);
        let epoch = self.global_epoch.0.load(Ordering::Relaxed);
        unsafe {
            SealedBag::fill_from_local(bag, local_bag, epoch, cpu_id.0);
        }
        let _guard = self.queue_lock.lock();
        unsafe { &mut *self.state.get() }.queue.push_back(bag);
        self.request_collection();
        Ok(())
    }

    fn try_drain(&'static self, budget: usize) -> DrainStats {
        self.try_drain_inner(budget, true)
    }

    fn drain_requested(&'static self, budget: usize) -> DrainStats {
        if !self.collection_requested.swap(false, Ordering::AcqRel) {
            return DrainStats::default();
        }

        let stats = self.try_drain_inner(budget, true);
        if stats.remaining > 0 {
            self.request_collection();
        }
        stats
    }

    fn try_drain_inner(&'static self, budget: usize, flush_local: bool) -> DrainStats {
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

        let hooks = self.hooks();
        if (hooks.in_irq_context)() || (hooks.in_trap_context)() {
            // Never run arbitrary Rust destructors on a bounded IRQ/trap
            // stack. Preserve the request for the reactor's normal-stack
            // maintenance point.
            self.request_collection();
            stats.remaining = self.total_retired_count();
            return stats;
        }

        let cpu_pin = (hooks.pin_current_cpu)(CpuPinReason::EpochDrain);
        let cpu_id = cpu_pin.cpu_id();
        if !self.is_possible_cpu(cpu_id)
            || !(hooks.is_cpu_online)(cpu_id)
            || !self.cpu_states[cpu_id.0].is_initialized()
        {
            return stats;
        }

        let local = &self.cpu_states[cpu_id.0];
        let flush_failed = flush_local
            && self
                .seal_local_bag(cpu_id, unsafe { &mut *local.bag_ptr() })
                .is_err();

        if self.try_advance_epoch() {
            stats.advanced_epochs += 1;
        }

        // Nodes retired in epoch E are reclaimable only once the global epoch
        // reaches at least E + 2.
        let safe_epoch = self.global_epoch.0.load(Ordering::Acquire);
        if let Some(_seq) = trace_seq {
            emit_epoch_trace(b"debug.epoch.drain.cpu", cpu_id.0 as i64);
            emit_epoch_trace(b"debug.epoch.drain.safe_epoch", safe_epoch as i64);
        }

        let Some(_collector) = self.collect_lock.try_lock() else {
            stats.remaining = self.total_retired_count();
            return stats;
        };

        let mut collected_bags = 0usize;
        while stats.reclaimed < budget && collected_bags < COLLECT_STEPS {
            let bag = {
                let _guard = self.queue_lock.lock();
                unsafe { &mut *self.state.get() }
                    .queue
                    .pop_expired(safe_epoch)
            };
            let Some(mut bag) = bag else {
                break;
            };
            collected_bags += 1;

            while stats.reclaimed < budget {
                let next = unsafe {
                    let bag_ref = bag.as_mut();
                    if bag_ref.cursor == bag_ref.len {
                        None
                    } else {
                        let entry = bag_ref.entries[bag_ref.cursor];
                        bag_ref.cursor += 1;
                        Some((entry, bag_ref.owner_cpu, bag_ref.cursor == bag_ref.len))
                    }
                };
                let Some((entry, owner_cpu, complete)) = next else {
                    break;
                };
                let reclaim_seq = epoch_trace_sample(&RECLAIM_TRACE_SAMPLE);
                if let Some(seq) = reclaim_seq {
                    emit_epoch_trace(b"debug.epoch.reclaim.begin", seq);
                    emit_epoch_trace(b"debug.epoch.reclaim.fn", entry.reclaim_fn as usize as i64);
                }
                unsafe { (entry.reclaim_fn)(entry.ptr) };
                if let Some(seq) = reclaim_seq {
                    emit_epoch_trace(b"debug.epoch.reclaim.end", seq);
                }
                self.cpu_states[owner_cpu].note_reclaimed(1);
                stats.reclaimed += 1;
                if complete {
                    break;
                }
            }

            if unsafe { bag.as_ref().remaining() } == 0 {
                let _guard = self.queue_lock.lock();
                unsafe { &mut *self.state.get() }.queue.recycle(bag);
            } else {
                let _guard = self.queue_lock.lock();
                unsafe { &mut *self.state.get() }.queue.push_front(bag);
                break;
            }
        }

        if flush_failed {
            let _ = self.seal_local_bag(cpu_id, unsafe { &mut *local.bag_ptr() });
        }
        stats.remaining = self.total_retired_count();
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

    #[inline]
    fn request_collection(&self) {
        self.collection_requested.store(true, Ordering::Release);
    }

    fn try_advance_epoch(&'static self) -> bool {
        let current = self.global_epoch.0.load(Ordering::Acquire);
        let hooks = self.hooks();

        // Pairs with guard publication and bag sealing. This is the same
        // synchronization point Crossbeam places before scanning participants.
        fence(Ordering::SeqCst);

        // Epoch advance is allowed only if every online initialized CPU is
        // either outside a guard (`local == 0`) or already in the current epoch.
        let possible_cpu_mask = self.possible_cpu_mask();
        for cpu in 0..MAX_EPOCH_CPUS {
            let cpu_id = CpuId(cpu);
            if !possible_cpu_mask.contains(cpu_id)
                || !(hooks.is_cpu_online)(cpu_id)
                || !self.cpu_states[cpu].is_initialized()
            {
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

    fn total_retired_count(&self) -> usize {
        let possible_cpu_mask = self.possible_cpu_mask();
        (0..MAX_EPOCH_CPUS)
            .filter(|cpu| possible_cpu_mask.contains(CpuId(*cpu)))
            .map(|cpu| self.cpu_states[cpu].retired_count())
            .sum()
    }

    fn cpu_state(&'static self, cpu: CpuId) -> Option<&'static CpuLocalEpochState> {
        if self.is_possible_cpu(cpu) {
            Some(&self.cpu_states[cpu.0])
        } else {
            None
        }
    }

    fn possible_cpu_mask(&self) -> tx_hal::CpuMask {
        tx_hal::CpuMask::from_bits(self.possible_cpu_mask.0.load(Ordering::Acquire))
    }

    fn is_possible_cpu(&self, cpu: CpuId) -> bool {
        cpu.0 < MAX_EPOCH_CPUS && self.possible_cpu_mask().contains(cpu)
    }

    fn hooks(&'static self) -> PlatformHooks {
        unsafe { *self.hooks.get() }
    }

    unsafe fn reset_for_test(&'static self) {
        let _guard = self.lock.lock();
        self.initialized.store(false, Ordering::Release);
        self.global_epoch.0.store(INITIAL_EPOCH, Ordering::Release);
        self.active_guards.0.store(0, Ordering::Release);
        self.possible_cpu_mask.0.store(1, Ordering::Release);
        self.collection_requested.store(false, Ordering::Release);
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
            possible_cpus: self.possible_cpu_mask().count(),
            retired_count: self.total_retired_count(),
            collection_requested: self.collection_requested.load(Ordering::Acquire),
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
    pin_current_cpu: fn(CpuPinReason) -> CpuPinGuard,
    in_irq_context: fn() -> bool,
    in_trap_context: fn() -> bool,
    is_cpu_online: fn(CpuId) -> bool,
}

impl PlatformHooks {
    const fn default() -> Self {
        Self {
            pin_current_cpu: default_pin_current_cpu,
            in_irq_context: default_in_irq_context,
            in_trap_context: default_in_trap_context,
            is_cpu_online: default_is_cpu_online,
        }
    }

    fn for_platform<P>() -> Self
    where
        P: PercpuIf + SmpIf + IrqIf,
    {
        Self {
            pin_current_cpu: P::pin_current_cpu_for,
            in_irq_context: P::in_irq_context,
            in_trap_context: P::in_trap_context,
            is_cpu_online: P::is_cpu_online,
        }
    }
}

fn default_pin_current_cpu(reason: CpuPinReason) -> CpuPinGuard {
    CpuPinGuard::new(CpuId(0)).with_reason(reason)
}

fn default_in_irq_context() -> bool {
    false
}

fn default_in_trap_context() -> bool {
    false
}

fn default_is_cpu_online(cpu: CpuId) -> bool {
    cpu.0 == 0
}

struct DomainState {
    queue: BagQueue,
}

impl DomainState {
    const fn new() -> Self {
        Self {
            queue: BagQueue::new(),
        }
    }

    fn reset(&mut self) {
        self.queue.reset();
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

    fn try_lock(&self) -> Option<SpinGuard<'_>> {
        self.held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinGuard { lock: self })
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

#[track_caller]
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

pub fn drain_requested(budget: usize) -> DrainStats {
    GLOBAL_DOMAIN.drain_requested(budget)
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
