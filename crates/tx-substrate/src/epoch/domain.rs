//! Global epoch domain.
//!
//! The domain owns the global epoch counter, per-CPU local epoch records, and
//! delayed reclamation queues. Readers enter the domain by creating a `Guard`;
//! destructors for retired objects run only after every online CPU has either
//! left its guard or advanced past the retire epoch.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::guard::Guard;
use super::local::{
    CpuLocalEpochState, CpuMembership, LOCAL_RETIRE_RESERVATION_CAPACITY as LOCAL_RETIRE_CAPACITY,
};
use super::RcuHead;
use tx_hal::{CpuId, CpuPinGuard, IrqIf, LocalExecutionGuard, PercpuIf, SmpIf};

const INITIAL_EPOCH: u64 = 1;
pub(crate) const MAX_EPOCH_CPUS: usize = 64;

static GLOBAL_DOMAIN: EpochDomain = EpochDomain::new();
static DRAIN_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);

/// Last EBR destructor invocation observed on one CPU.
///
/// The fields are deliberately plain integers backed by atomics. Fatal-trap
/// diagnostics can therefore inspect them without allocating or taking an EBR
/// lock while the failing hart may still be inside the callback itself.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReclaimTrace {
    pub sequence: u64,
    /// Zero after a callback returned, one while it is executing.
    pub active: bool,
    /// 1 = intrusive EBR head, 2 = zone slot, 3 = deferred Published drop.
    pub kind: usize,
    pub object: usize,
    pub callback: usize,
    pub next: usize,
}

#[repr(align(64))]
struct ReclaimTraceCell {
    sequence: AtomicU64,
    active: AtomicBool,
    kind: AtomicUsize,
    object: AtomicUsize,
    callback: AtomicUsize,
    next: AtomicUsize,
}

impl ReclaimTraceCell {
    const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            active: AtomicBool::new(false),
            kind: AtomicUsize::new(0),
            object: AtomicUsize::new(0),
            callback: AtomicUsize::new(0),
            next: AtomicUsize::new(0),
        }
    }
}

static RECLAIM_TRACE: [ReclaimTraceCell; MAX_EPOCH_CPUS] =
    [const { ReclaimTraceCell::new() }; MAX_EPOCH_CPUS];

pub(crate) const RECLAIM_KIND_INTRUSIVE: usize = 1;
pub(crate) const RECLAIM_KIND_ZONE: usize = 2;
pub(crate) const RECLAIM_KIND_DEFERRED_PUBLICATION: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpochError {
    AlreadyInitialized,
    NotInitialized,
    TooManyCpus,
    InvalidCpu,
    CpuNotInitialized,
    CpuAlreadyOnline,
    CpuNotOnline,
    LocalRetireReentered,
    RetireBagOccupied,
    LocalRetireExhausted,
    LocalRetireOutstanding,
}

pub const LOCAL_RETIRE_RESERVATION_CAPACITY: usize = LOCAL_RETIRE_CAPACITY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LocalRetireSlot(usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservedRetireInvariant {
    WrongCpu,
    SlotNotReserved,
    SlotAlreadyFilled,
}

#[must_use = "local-retire reservations must be committed or canceled"]
#[derive(Debug)]
pub struct LocalRetireReservation {
    cpu_id: CpuId,
    slot: LocalRetireSlot,
    armed: bool,
    _not_send_sync: PhantomData<*mut ()>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrainStats {
    pub advanced_epochs: usize,
    pub bag_reclaimed: usize,
    pub bag_remaining: usize,
    pub active_guards: usize,
    pub examined: usize,
    pub publication_dropped: usize,
    pub publication_remaining: usize,
    /// Aggregate reclamation count retained for kernel maintenance callers.
    pub reclaimed: usize,
    /// Aggregate pending count retained for kernel maintenance callers.
    pub remaining: usize,
}

impl DrainStats {
    fn with_compat_totals(mut self) -> Self {
        self.reclaimed = self.bag_reclaimed.saturating_add(self.publication_dropped);
        self.remaining = self
            .bag_remaining
            .saturating_add(self.publication_remaining);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuEpochSummary {
    pub cpu_id: CpuId,
    pub initialized: bool,
    pub local_epoch: u64,
    pub bag_retired: usize,
    pub publication_pending: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EpochSummary {
    pub initialized: bool,
    pub global_epoch: u64,
    pub active_guards: usize,
    pub possible_cpus: usize,
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdvanceAttempt {
    pub advanced: bool,
    pub scan_attempts: usize,
    pub version_before: u64,
    pub version_after: u64,
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
    /// Number of CPUs the platform says may participate in EBR.
    possible_cpus: CachePadded<AtomicUsize>,
    /// Changes whenever a CPU enters or leaves the participating set.
    membership_version: CachePadded<AtomicU64>,
    /// Platform callbacks are installed once at BSP init and then read lock-free.
    hooks: UnsafeCell<PlatformHooks>,
    /// Per-CPU guard state and retired-node list heads.
    cpu_states: [CpuLocalEpochState; MAX_EPOCH_CPUS],
    /// Protects initialization and hook replacement; hot retire/drain paths use
    /// CPU pinning and per-CPU node ranges instead of this lock.
    lock: SpinLock,
}

unsafe impl Sync for EpochDomain {}

pub(crate) struct LocalRetireGuard {
    domain: &'static EpochDomain,
    local: &'static CpuLocalEpochState,
    cpu_id: CpuId,
    local_execution: Option<LocalExecutionGuard>,
    _cpu_pin: CpuPinGuard,
}

struct CallbackActiveGuard(&'static CpuLocalEpochState);

impl Drop for CallbackActiveGuard {
    fn drop(&mut self) {
        self.0.end_callback();
    }
}

impl LocalRetireGuard {
    pub(crate) fn cpu_id(&self) -> CpuId {
        self.cpu_id
    }

    pub(crate) fn sample_epoch_after_barrier(&self) -> u64 {
        // The caller establishes its no-new-reader barrier before sampling.
        // AcqRel RMW observes the immediately preceding modification-order
        // value and prevents a stale ordinary load from tagging the bag.
        self.domain.global_epoch.0.fetch_add(0, Ordering::AcqRel)
    }

    fn try_enqueue_head_after_barrier(
        &mut self,
        head: NonNull<RcuHead>,
        epoch: u64,
    ) -> Result<(), EpochError> {
        let state = unsafe { &mut *self.local.retire_state_ptr() };
        unsafe { state.try_enqueue_rcu(epoch, head) }?;
        self.local.publish_retire_summary(state);
        Ok(())
    }

    pub(crate) fn preflight_head_bags_after_barrier(
        &mut self,
        epoch: u64,
    ) -> Result<(), EpochError> {
        // This is the final publication preflight. It must remain O(1) and
        // callback-free so callers can keep it inside a short commit window.
        // Reclaimable collisions are maintenance work for the caller to run
        // before retrying.
        let state = unsafe { &mut *self.local.retire_state_ptr() };
        let next = epoch.saturating_add(1);
        for candidate in [epoch, next] {
            let bag = &state.bags[candidate as usize % super::bag::EPOCH_BAG_COUNT];
            if !bag.is_empty() && bag.epoch != candidate {
                return Err(EpochError::RetireBagOccupied);
            }
        }
        for candidate in [epoch, next] {
            let bag = &mut state.bags[candidate as usize % super::bag::EPOCH_BAG_COUNT];
            let prepared = bag.reset_if_empty(candidate);
            debug_assert!(prepared);
        }
        Ok(())
    }

    fn reclaim_reclaimable_bags(
        &mut self,
        safe_epoch: u64,
        budget: usize,
    ) -> (usize, usize, usize) {
        let mut zone_reclaim_head = None;
        let mut intrusive_reclaim_head: *mut RcuHead = core::ptr::null_mut();
        let mut detach_count = 0usize;
        let mut examined = 0usize;

        {
            let state = unsafe { &mut *self.local.retire_state_ptr() };
            for bag in &mut state.bags {
                examined += 1;
                if bag.is_empty() || safe_epoch < bag.epoch.saturating_add(2) {
                    continue;
                }
                while detach_count < budget {
                    if let Some(key) = bag.zone_head {
                        bag.zone_head = crate::zone::retiring_next(key);
                        crate::zone::set_retiring_next(key, zone_reclaim_head);
                        zone_reclaim_head = Some(key);
                        bag.zone_count -= 1;
                    } else if !bag.rcu_head.is_null() {
                        let head = bag.rcu_head;
                        let next = unsafe { (*head).next };
                        debug_assert!(!next.is_null());
                        bag.rcu_head = if next == head {
                            core::ptr::null_mut()
                        } else {
                            next
                        };
                        unsafe {
                            (*head).next = if intrusive_reclaim_head.is_null() {
                                head
                            } else {
                                intrusive_reclaim_head
                            };
                        }
                        intrusive_reclaim_head = head;
                        bag.rcu_count -= 1;
                    } else {
                        break;
                    }
                    detach_count += 1;
                    examined += 1;
                }
                if bag.is_empty() {
                    bag.epoch = 0;
                }
            }
            self.local.publish_retire_summary(state);
        }

        let reclaimed = self.domain.reclaim_zone_list(self, zone_reclaim_head)
            + self
                .domain
                .reclaim_intrusive_list(self, intrusive_reclaim_head);
        let remaining = self.local.bag_only_retired_count();
        (reclaimed, remaining, examined)
    }

    fn reclaim_reclaimable_reserved(
        &mut self,
        safe_epoch: u64,
        budget: usize,
    ) -> (usize, usize, usize) {
        let (head, detached, examined) = unsafe {
            self.local
                .reserved_retire()
                .detach_reclaimable(safe_epoch, budget)
        };
        let reclaimed = self.domain.reclaim_intrusive_list(self, head);
        debug_assert_eq!(reclaimed, detached);
        (
            reclaimed,
            self.local.reserved_retire().ready_count(),
            examined,
        )
    }

    pub(crate) fn head_bag_summary(&self) -> [(u64, usize, usize); super::bag::EPOCH_BAG_COUNT] {
        let state = unsafe { &*self.local.retire_state_ptr() };
        core::array::from_fn(|index| {
            let bag = &state.bags[index];
            (bag.epoch, bag.zone_count, bag.rcu_count)
        })
    }

    pub(crate) unsafe fn enqueue_prepared_head_after_barrier(
        &mut self,
        mut head: NonNull<RcuHead>,
        epoch: u64,
    ) {
        let state = unsafe { &mut *self.local.retire_state_ptr() };
        let bag = &mut state.bags[epoch as usize % super::bag::EPOCH_BAG_COUNT];
        let previous = bag.rcu_head;
        unsafe {
            head.as_mut().next = if previous.is_null() {
                head.as_ptr()
            } else {
                previous
            };
        }
        bag.rcu_head = head.as_ptr();
        bag.rcu_count += 1;
        self.local.publish_retire_summary(state);
    }

    pub(crate) unsafe fn enqueue_head_after_barrier(&mut self, head: NonNull<RcuHead>, epoch: u64) {
        self.try_enqueue_head_after_barrier(head, epoch)
            .expect("post-barrier RCU head enqueue must be infallible");
    }

    pub(crate) unsafe fn try_enqueue_slot_after_barrier(
        &mut self,
        key: crate::zone::SlotKey,
        epoch: u64,
        link: impl FnOnce(Option<crate::zone::SlotKey>),
    ) -> Result<(), EpochError> {
        let state = unsafe { &mut *self.local.retire_state_ptr() };
        let bag = state
            .bag_for_epoch_mut(epoch)
            .ok_or(EpochError::RetireBagOccupied)?;
        let previous_head = bag.zone_head;
        link(previous_head);
        bag.zone_head = Some(key);
        bag.zone_count += 1;
        self.local.publish_retire_summary(state);
        Ok(())
    }

    pub(crate) fn with_local_execution_open<R>(
        &mut self,
        action: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.local.end_retire();
        self.local.begin_callback();
        let callback_active = CallbackActiveGuard(self.local);
        drop(self.local_execution.take());

        let result = action(self);

        let hooks = self.domain.hooks();
        assert_eq!(
            (hooks.pin_current_cpu)().cpu_id(),
            self.cpu_id,
            "epoch reclaim callback crossed a CPU migration point"
        );
        let local_execution = (hooks.exclude_local_execution)();
        drop(callback_active);
        assert!(
            self.local.try_begin_retire(),
            "local retire state re-entered while reopening exclusion"
        );
        self.local_execution = Some(local_execution);
        result
    }
}

impl Drop for LocalRetireGuard {
    fn drop(&mut self) {
        self.local.end_retire();
        drop(self.local_execution.take());
        self.local.unpin();
    }
}

impl LocalRetireReservation {
    pub(crate) fn validate_for(
        &self,
        guard: &LocalRetireGuard,
    ) -> Result<(), ReservedRetireInvariant> {
        if guard.cpu_id() != self.cpu_id {
            return Err(ReservedRetireInvariant::WrongCpu);
        }
        match guard.local.reserved_retire().validate_reserved(self.slot.0) {
            Ok(()) if self.armed => Ok(()),
            Ok(()) | Err(false) => Err(ReservedRetireInvariant::SlotNotReserved),
            Err(true) => Err(ReservedRetireInvariant::SlotAlreadyFilled),
        }
    }

    pub(crate) unsafe fn enqueue_after_barrier(
        mut self,
        guard: &mut LocalRetireGuard,
        head: NonNull<RcuHead>,
        retired_at: u64,
    ) -> Result<(), ReservedRetireInvariant> {
        self.validate_for(guard)?;
        unsafe {
            guard
                .local
                .reserved_retire()
                .fill_reserved(self.slot.0, head, retired_at);
        }
        self.armed = false;
        Ok(())
    }
}

impl Drop for LocalRetireReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let cpu_pin = (GLOBAL_DOMAIN.hooks().pin_current_cpu)();
        assert_eq!(
            cpu_pin.cpu_id(),
            self.cpu_id,
            "local-retire reservation dropped on another CPU"
        );
        let local = GLOBAL_DOMAIN
            .cpu_state(self.cpu_id)
            .expect("local-retire reservation CPU remains initialized");
        local.reserved_retire().cancel(self.slot.0);
        self.armed = false;
    }
}

impl EpochDomain {
    const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            global_epoch: CachePadded::new(AtomicU64::new(INITIAL_EPOCH)),
            possible_cpus: CachePadded::new(AtomicUsize::new(1)),
            membership_version: CachePadded::new(AtomicU64::new(0)),
            hooks: UnsafeCell::new(PlatformHooks::default()),
            cpu_states: [const { CpuLocalEpochState::new() }; MAX_EPOCH_CPUS],
            lock: SpinLock::new(),
        }
    }

    fn init_on_bsp<P>(&'static self) -> Result<(), EpochError>
    where
        P: PercpuIf + SmpIf + IrqIf,
    {
        self.init_on_bsp_with::<P>(|| {})
    }

    fn init_on_bsp_with<P>(&'static self, admission_hook: impl FnOnce()) -> Result<(), EpochError>
    where
        P: PercpuIf + SmpIf + IrqIf,
    {
        let possible_cpus = P::possible_cpu_count();
        if possible_cpus == 0 || possible_cpus > MAX_EPOCH_CPUS {
            return Err(EpochError::TooManyCpus);
        }

        let _guard = self.lock.lock();
        if self.initialized.load(Ordering::Acquire) {
            return Err(EpochError::AlreadyInitialized);
        }
        admission_hook();
        self.global_epoch.0.store(INITIAL_EPOCH, Ordering::Release);
        self.possible_cpus.0.store(possible_cpus, Ordering::Release);
        self.membership_version.0.store(0, Ordering::Release);
        unsafe {
            *self.hooks.get() = PlatformHooks::for_platform::<P>();
        }

        for cpu in 0..MAX_EPOCH_CPUS {
            self.cpu_states[cpu].reset();
        }

        self.admit_cpu_locked(<P as PercpuIf>::current_cpu_id())?;
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    fn init_for_test(&'static self) {
        let _guard = self.lock.lock();
        self.initialized.store(false, Ordering::Release);
        self.global_epoch.0.store(INITIAL_EPOCH, Ordering::Release);
        self.possible_cpus.0.store(1, Ordering::Release);
        self.membership_version.0.store(0, Ordering::Release);
        unsafe {
            *self.hooks.get() = PlatformHooks::default();
        }

        for cpu in 0..MAX_EPOCH_CPUS {
            self.cpu_states[cpu].reset();
        }
        self.admit_cpu_locked(CpuId(0))
            .expect("test BSP CPU admission");
        self.initialized.store(true, Ordering::Release);
    }

    fn init_on_ap(&'static self, cpu: CpuId) -> Result<(), EpochError> {
        let _guard = self.lock.lock();
        if !self.initialized.load(Ordering::Acquire) {
            return Err(EpochError::NotInitialized);
        }
        self.admit_cpu_locked(cpu)
    }

    fn admit_cpu_locked(&'static self, cpu: CpuId) -> Result<(), EpochError> {
        if cpu.0 >= self.possible_cpus.0.load(Ordering::Acquire) {
            return Err(EpochError::InvalidCpu);
        }
        let local = &self.cpu_states[cpu.0];
        if local.membership() != CpuMembership::Offline {
            return Err(EpochError::CpuAlreadyOnline);
        }
        local.set_membership(CpuMembership::Admitting);
        local.prepare_admission();
        local.set_membership(CpuMembership::Online);
        self.membership_version.0.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn local_retire_guard(&'static self) -> Result<LocalRetireGuard, EpochError> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(EpochError::NotInitialized);
        }

        let hooks = self.hooks();
        if (hooks.in_irq_context)() {
            return Err(EpochError::CpuNotOnline);
        }
        let cpu_pin = (hooks.pin_current_cpu)();
        let cpu_id = cpu_pin.cpu_id();
        let local_execution = (hooks.exclude_local_execution)();
        let Some(local) = self.cpu_state(cpu_id) else {
            drop(local_execution);
            return Err(EpochError::InvalidCpu);
        };
        if !local.try_pin_for_retire() {
            drop(local_execution);
            return Err(EpochError::CpuNotOnline);
        }
        if !local.try_begin_retire() {
            drop(local_execution);
            local.unpin();
            return Err(EpochError::LocalRetireReentered);
        }

        Ok(LocalRetireGuard {
            domain: self,
            local,
            cpu_id,
            local_execution: Some(local_execution),
            _cpu_pin: cpu_pin,
        })
    }

    fn try_reserve_local_retire(&'static self) -> Result<LocalRetireReservation, EpochError> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(EpochError::NotInitialized);
        }
        let hooks = self.hooks();
        if (hooks.in_irq_context)() {
            return Err(EpochError::CpuNotOnline);
        }
        let cpu_pin = (hooks.pin_current_cpu)();
        let cpu_id = cpu_pin.cpu_id();
        let local = self.cpu_state(cpu_id).ok_or(EpochError::InvalidCpu)?;
        if !local.try_pin_online() {
            return Err(EpochError::CpuNotOnline);
        }
        let slot = local
            .reserved_retire()
            .try_reserve()
            .ok_or(EpochError::LocalRetireExhausted);
        local.unpin();
        drop(cpu_pin);
        Ok(LocalRetireReservation {
            cpu_id,
            slot: LocalRetireSlot(slot?),
            armed: true,
            _not_send_sync: PhantomData,
        })
    }

    #[track_caller]
    fn guard(&'static self) -> Guard<'static> {
        self.guard_with_admission_hook(|| {})
    }

    /// Enter the reader epoch after publishing a value which was stable across
    /// the admission window.
    ///
    /// Loading the global epoch and publishing it into the CPU-local slot are
    /// separate operations.  Without the validation load below, two remote
    /// drainers can advance the domain twice between those operations and make
    /// an old root reclaimable immediately before this reader dereferences it.
    /// No protected load has happened when the validation fails.  The slow
    /// path therefore repairs the published value while holding the same lock
    /// that serialises the epoch CAS.  It deliberately keeps the local epoch
    /// non-zero throughout: leaving and retrying would repeatedly expose a
    /// quiescent window to aggressive remote drainers and can livelock a
    /// reader on an otherwise idle SMP system.
    fn guard_with_admission_hook(&'static self, admission_hook: impl FnOnce()) -> Guard<'static> {
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
            local.try_pin_online(),
            "epoch::guard current CPU is not online in the epoch domain"
        );

        let active_epoch = local.current();
        if active_epoch != 0 {
            local.unpin();
            panic!(
                "epoch::guard nested on CPU {} with active epoch {}; use borrow_current_guard()",
                cpu_id.0, active_epoch
            );
        }
        let current_epoch = self.global_epoch.0.load(Ordering::Acquire);
        admission_hook();
        local.enter(current_epoch);
        // Publish the local epoch before any protected load can float above the
        // guard acquisition, then validate that no complete epoch advance
        // raced ahead of the publication.
        fence(Ordering::SeqCst);
        let observed_epoch = self.global_epoch.0.load(Ordering::Acquire);
        let entered_epoch = if observed_epoch == current_epoch {
            current_epoch
        } else {
            // `try_advance_epoch_with` performs its final membership check and
            // epoch CAS under this lock.  Once held, every earlier scan has
            // either committed or become stale.  Republish the newest epoch
            // without ever advertising quiescence; a scan racing after this
            // store may advance at most once, which is covered by the domain's
            // two-epoch reclamation grace period.
            let _epoch_commit = self.lock.lock();
            let latest_epoch = self.global_epoch.0.load(Ordering::Acquire);
            local.republish(latest_epoch);
            fence(Ordering::SeqCst);
            latest_epoch
        };
        Guard::new(local, cpu_id, entered_epoch, cpu_pin)
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
        if local.membership() != CpuMembership::Online {
            return None;
        }
        let local_epoch = local.current();
        if local_epoch == 0 {
            return None;
        }
        Some(Guard::new_borrowed(local, cpu_id, local_epoch, cpu_pin))
    }

    unsafe fn retire_intrusive(&'static self, head: NonNull<RcuHead>) -> Result<(), EpochError> {
        super::with_local_retire_guard(|local_guard| {
            if unsafe { head.as_ref().is_queued() } {
                return Err(EpochError::RetireBagOccupied);
            }
            let retired_at_epoch = local_guard.sample_epoch_after_barrier();
            unsafe {
                local_guard.enqueue_head_after_barrier(head, retired_at_epoch);
            }
            Ok(())
        })?
    }

    fn try_drain(&'static self, budget: usize) -> DrainStats {
        let trace_seq =
            epoch_trace_sample_if_enabled(tx_observe::is_enabled(), &DRAIN_TRACE_SAMPLE);
        if let Some(seq) = trace_seq {
            emit_epoch_trace(b"debug.epoch.drain.enter", seq);
            emit_epoch_trace(b"debug.epoch.drain.budget", budget as i64);
        }
        if !self.initialized.load(Ordering::Acquire) {
            return DrainStats::default();
        }

        let publication = crate::publication::drain_deferred_drops(budget);

        let mut stats = DrainStats {
            active_guards: self.active_guard_count(),
            publication_dropped: publication.dropped,
            publication_remaining: publication.remaining,
            ..DrainStats::default()
        };

        if self.try_advance_epoch() {
            stats.advanced_epochs += 1;
        }

        // Nodes retired in epoch E are reclaimable only once the global epoch
        // reaches at least E + 2.
        let safe_epoch = self.global_epoch.0.load(Ordering::Acquire);
        let mut local_guard = match self.local_retire_guard() {
            Ok(guard) => guard,
            Err(_) => return stats.with_compat_totals(),
        };
        let cpu_id = local_guard.cpu_id();
        let (bag_reclaimed, bag_remaining, bag_examined) =
            local_guard.reclaim_reclaimable_bags(safe_epoch, budget);
        let reserved_budget = budget.saturating_sub(bag_reclaimed);
        let (reserved_reclaimed, reserved_remaining, reserved_examined) =
            local_guard.reclaim_reclaimable_reserved(safe_epoch, reserved_budget);
        stats.bag_reclaimed = bag_reclaimed + reserved_reclaimed;
        stats.bag_remaining = bag_remaining + reserved_remaining;
        stats.examined += bag_examined + reserved_examined;
        stats.publication_remaining = crate::publication::deferred_drop_count(cpu_id);
        drop(local_guard);
        if let Some(seq) = trace_seq {
            emit_epoch_trace(b"debug.epoch.drain.cpu", cpu_id.0 as i64);
            emit_epoch_trace(b"debug.epoch.drain.safe_epoch", safe_epoch as i64);
            emit_epoch_trace(b"debug.epoch.drain.exit", seq);
            emit_epoch_trace(b"debug.epoch.drain.advanced", stats.advanced_epochs as i64);
            emit_epoch_trace(b"debug.epoch.drain.reclaimed", stats.bag_reclaimed as i64);
            emit_epoch_trace(b"debug.epoch.drain.remaining", stats.bag_remaining as i64);
            emit_epoch_trace(
                b"debug.epoch.drain.active_guards",
                stats.active_guards as i64,
            );
        }
        stats.with_compat_totals()
    }

    fn try_advance_epoch(&'static self) -> bool {
        self.try_advance_epoch_with(|| {}).advanced
    }

    fn try_advance_epoch_with(&'static self, membership_change: impl FnOnce()) -> AdvanceAttempt {
        let version_before = self.membership_version.0.load(Ordering::Acquire);
        let mut membership_change = Some(membership_change);
        let mut scan_attempts = 0usize;
        let hooks = self.hooks();
        let current_cpu = (hooks.pin_current_cpu)().cpu_id();
        loop {
            scan_attempts += 1;
            let version = self.membership_version.0.load(Ordering::Acquire);
            let current = self.global_epoch.0.load(Ordering::Acquire);
            let next = current.saturating_add(1);
            let mut blocked = false;

            // Pair with the reader-side SeqCst publication fence. Without a
            // scanner fence, a weakly ordered hart may observe a participant
            // as quiescent, then commit an epoch advance after that reader has
            // published its local epoch. Two such stale scans can otherwise
            // advance through the complete grace period and reclaim a root
            // while the newly admitted reader is cloning it.
            fence(Ordering::SeqCst);

            for cpu in 0..self.possible_cpus.0.load(Ordering::Acquire) {
                let state = &self.cpu_states[cpu];
                if !matches!(
                    state.membership(),
                    CpuMembership::Online | CpuMembership::Draining
                ) {
                    continue;
                }

                let local = state.current();
                if state.retire_active() || (local != 0 && local < current) {
                    blocked = true;
                }
                if !state.can_advance_to(next) {
                    if cpu != current_cpu.0 && state.request_drain() {
                        (hooks.request_maintenance)(CpuId(cpu));
                    }
                    blocked = true;
                }
            }

            if let Some(change) = membership_change.take() {
                change();
            }
            // Admission/offline also takes this lock. The scan stays lock-free,
            // while the final version check and epoch CAS form one membership
            // snapshot with no change window between them.
            let _membership = self.lock.lock();
            let version_after = self.membership_version.0.load(Ordering::Acquire);
            if version_after != version {
                continue;
            }
            let advanced = !blocked
                && self
                    .global_epoch
                    .0
                    .compare_exchange(current, next, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok();
            return AdvanceAttempt {
                advanced,
                scan_attempts,
                version_before,
                version_after,
            };
        }
    }

    fn service_local_drain_request(&'static self, budget: usize) -> Option<DrainStats> {
        if !self.initialized.load(Ordering::Acquire) {
            return None;
        }
        let hooks = self.hooks();
        let cpu_pin = (hooks.pin_current_cpu)();
        let local = self.cpu_state(cpu_pin.cpu_id())?;
        if local.membership() != CpuMembership::Online || !local.take_drain_request() {
            return None;
        }
        let stats = self.try_drain(budget);
        // One pass normally advances at most one epoch. Objects retired in E
        // cannot be reclaimed until E+2, and a bounded budget can also leave
        // ready callbacks behind. Keep maintenance armed until this CPU's
        // complete deferred-work chain is empty.
        if stats.remaining != 0 {
            local.request_drain();
        }
        drop(cpu_pin);
        Some(stats)
    }

    fn offline_cpu(&'static self, target: CpuId) -> Result<(), EpochError> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(EpochError::NotInitialized);
        }
        let hooks = self.hooks();
        let coordinator_pin = (hooks.pin_current_cpu)();
        let coordinator_cpu = coordinator_pin.cpu_id();
        if coordinator_cpu == target {
            return Err(EpochError::InvalidCpu);
        }
        let coordinator = self
            .cpu_state(coordinator_cpu)
            .ok_or(EpochError::InvalidCpu)?;
        let target_state = self.cpu_state(target).ok_or(EpochError::InvalidCpu)?;
        if !coordinator.try_pin_online() {
            return Err(EpochError::CpuNotOnline);
        }

        {
            let _membership = self.lock.lock();
            if !target_state.transition_membership(CpuMembership::Online, CpuMembership::Draining) {
                coordinator.unpin();
                return Err(EpochError::CpuNotOnline);
            }
            self.membership_version.0.fetch_add(1, Ordering::AcqRel);
        }

        while target_state.pin_count() != 0
            || target_state.current() != 0
            || target_state.retire_active()
        {
            core::hint::spin_loop();
        }

        let local_execution = (hooks.exclude_local_execution)();
        if !coordinator.try_begin_retire() {
            let _membership = self.lock.lock();
            if target_state.membership() == CpuMembership::Draining {
                assert!(target_state
                    .transition_membership(CpuMembership::Draining, CpuMembership::Online));
                self.membership_version.0.fetch_add(1, Ordering::AcqRel);
            }
            drop(local_execution);
            coordinator.unpin();
            return Err(EpochError::LocalRetireReentered);
        }

        let transfer_result = {
            let _membership = self.lock.lock();
            if target_state.membership() != CpuMembership::Draining
                || target_state.pin_count() != 0
                || target_state.current() != 0
                || target_state.retire_active()
            {
                Err(EpochError::CpuNotOnline)
            } else if target_state.reserved_retire().unfilled_count() != 0 {
                assert!(target_state
                    .transition_membership(CpuMembership::Draining, CpuMembership::Online));
                self.membership_version.0.fetch_add(1, Ordering::AcqRel);
                Err(EpochError::LocalRetireOutstanding)
            } else {
                let target_reserved = target_state.reserved_retire();
                let coordinator_reserved = coordinator.reserved_retire();
                let transfer_count = target_reserved.ready_count();
                let mut transfer_slots = [usize::MAX; LOCAL_RETIRE_CAPACITY];
                if !coordinator_reserved.reserve_transfer_slots(transfer_count, &mut transfer_slots)
                {
                    assert!(target_state
                        .transition_membership(CpuMembership::Draining, CpuMembership::Online));
                    self.membership_version.0.fetch_add(1, Ordering::AcqRel);
                    Err(EpochError::LocalRetireExhausted)
                } else {
                    let coordinator_retire = unsafe { &mut *coordinator.retire_state_ptr() };
                    let target_retire = unsafe { &mut *target_state.retire_state_ptr() };
                    match unsafe { coordinator_retire.merge_from(target_retire) } {
                        Ok(()) => {
                            unsafe {
                                target_reserved.transfer_ready_to_reserved(
                                    coordinator_reserved,
                                    &transfer_slots[..transfer_count],
                                );
                                crate::publication::transfer_deferred_drops(
                                    target,
                                    coordinator_cpu,
                                );
                            }
                            coordinator.publish_retire_summary(coordinator_retire);
                            target_state.publish_retire_summary(target_retire);
                            if transfer_count != 0
                                || crate::publication::deferred_drop_count(coordinator_cpu) != 0
                            {
                                coordinator.request_drain();
                            }
                            assert!(target_state.transition_membership(
                                CpuMembership::Draining,
                                CpuMembership::Offline,
                            ));
                            self.membership_version.0.fetch_add(1, Ordering::AcqRel);
                            Ok(())
                        }
                        Err(error) => {
                            coordinator_reserved
                                .cancel_transfer_slots(&transfer_slots[..transfer_count]);
                            assert!(target_state.transition_membership(
                                CpuMembership::Draining,
                                CpuMembership::Online,
                            ));
                            self.membership_version.0.fetch_add(1, Ordering::AcqRel);
                            Err(error)
                        }
                    }
                }
            }
        };

        coordinator.end_retire();
        drop(local_execution);
        coordinator.unpin();
        drop(coordinator_pin);
        transfer_result
    }

    fn reclaim_intrusive_list(
        &'static self,
        local_guard: &mut LocalRetireGuard,
        mut head: *mut RcuHead,
    ) -> usize {
        let mut reclaimed = 0usize;
        while !head.is_null() {
            let current = head;
            let next = unsafe { (*current).next };
            head = if next == current {
                core::ptr::null_mut()
            } else {
                next
            };
            unsafe {
                // Reclaim is one-shot. Keep the node marked queued while its
                // callback runs so a recursive retire cannot reinsert it.
                (*current).next = current;
            }
            let reclaim = unsafe { (*current).reclaim };
            let trace_sequence = begin_reclaim_trace(
                local_guard.cpu_id(),
                RECLAIM_KIND_INTRUSIVE,
                current as usize,
                reclaim as usize,
                head as usize,
            );
            local_guard.with_local_execution_open(|local_guard| unsafe {
                reclaim(current, local_guard);
            });
            finish_reclaim_trace(local_guard.cpu_id(), trace_sequence);
            reclaimed += 1;
        }
        reclaimed
    }

    fn reclaim_zone_list(
        &'static self,
        local_guard: &mut LocalRetireGuard,
        mut head: Option<crate::zone::SlotKey>,
    ) -> usize {
        let mut reclaimed = 0usize;
        while let Some(current) = head {
            head = crate::zone::retiring_next(current);
            crate::zone::set_retiring_next(current, Some(current));
            let trace_sequence = begin_reclaim_trace(
                local_guard.cpu_id(),
                RECLAIM_KIND_ZONE,
                current.raw() as usize,
                crate::zone::reclaim_retired_slot as usize,
                head.map_or(0, |next| next.raw() as usize),
            );
            local_guard.with_local_execution_open(|_| unsafe {
                crate::zone::reclaim_retired_slot(current);
            });
            finish_reclaim_trace(local_guard.cpu_id(), trace_sequence);
            reclaimed += 1;
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
        self.possible_cpus.0.store(1, Ordering::Release);
        self.membership_version.0.store(0, Ordering::Release);
        let _guard = self.lock.lock();
        *self.hooks.get() = PlatformHooks::default();
        for cpu in 0..MAX_EPOCH_CPUS {
            self.cpu_states[cpu].reset();
        }
    }

    fn summary(&'static self) -> EpochSummary {
        EpochSummary {
            initialized: self.initialized.load(Ordering::Acquire),
            global_epoch: self.global_epoch.0.load(Ordering::Acquire),
            active_guards: self.active_guard_count(),
            possible_cpus: self.possible_cpus.0.load(Ordering::Acquire),
        }
    }

    fn cpu_summary(&'static self, cpu: CpuId) -> Option<CpuEpochSummary> {
        let state = self.cpu_state(cpu)?;
        Some(CpuEpochSummary {
            cpu_id: cpu,
            initialized: state.is_initialized(),
            local_epoch: state.current(),
            bag_retired: state.bag_retired_count(),
            publication_pending: crate::publication::deferred_drop_count(cpu),
        })
    }

    fn local_retire_active_for_current_cpu(&'static self) -> bool {
        if !self.initialized.load(Ordering::Acquire) {
            return false;
        }
        let hooks = self.hooks();
        let cpu_pin = (hooks.pin_current_cpu)();
        self.cpu_state(cpu_pin.cpu_id())
            .is_some_and(CpuLocalEpochState::retire_active)
    }

    fn active_guard_count(&'static self) -> usize {
        (0..self.possible_cpus.0.load(Ordering::Acquire))
            .map(|cpu| self.cpu_states[cpu].active_guard_count())
            .sum()
    }

    fn membership_version(&'static self) -> u64 {
        self.membership_version.0.load(Ordering::Acquire)
    }
}

fn epoch_trace_sample(counter: &AtomicU64) -> Option<i64> {
    let seq = counter.fetch_add(1, Ordering::Relaxed);
    (seq < 128 || seq.is_power_of_two()).then_some(seq as i64)
}

#[inline]
fn epoch_trace_sample_if_enabled(enabled: bool, counter: &AtomicU64) -> Option<i64> {
    enabled.then(|| epoch_trace_sample(counter)).flatten()
}

fn emit_epoch_trace(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
    }
}

pub(crate) fn begin_reclaim_trace(
    cpu: CpuId,
    kind: usize,
    object: usize,
    callback: usize,
    next: usize,
) -> u64 {
    // Reclaim tracing is diagnostic-only.  Keeping it permanently enabled
    // adds several atomic stores around every EBR destructor; persistent VM
    // roots can retire a very large number of nodes during BuildStorm.
    if !cfg!(tx_ds_metrics) {
        return 0;
    }
    let Some(trace) = RECLAIM_TRACE.get(cpu.0) else {
        return 0;
    };
    let sequence = trace.sequence.load(Ordering::Relaxed).wrapping_add(1);
    trace.kind.store(kind, Ordering::Relaxed);
    trace.object.store(object, Ordering::Relaxed);
    trace.callback.store(callback, Ordering::Relaxed);
    trace.next.store(next, Ordering::Relaxed);
    trace.sequence.store(sequence, Ordering::Relaxed);
    trace.active.store(true, Ordering::Release);
    sequence
}

pub(crate) fn finish_reclaim_trace(cpu: CpuId, sequence: u64) {
    if !cfg!(tx_ds_metrics) {
        return;
    }
    let Some(trace) = RECLAIM_TRACE.get(cpu.0) else {
        return;
    };
    if trace.sequence.load(Ordering::Relaxed) == sequence {
        trace.active.store(false, Ordering::Release);
    }
}

pub fn reclaim_trace(cpu: CpuId) -> Option<ReclaimTrace> {
    if !cfg!(tx_ds_metrics) {
        return None;
    }
    let trace = RECLAIM_TRACE.get(cpu.0)?;
    let active = trace.active.load(Ordering::Acquire);
    Some(ReclaimTrace {
        sequence: trace.sequence.load(Ordering::Relaxed),
        active,
        kind: trace.kind.load(Ordering::Relaxed),
        object: trace.object.load(Ordering::Relaxed),
        callback: trace.callback.load(Ordering::Relaxed),
        next: trace.next.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[test]
    fn disabled_drain_trace_does_not_touch_the_shared_sample_counter() {
        let counter = AtomicU64::new(17);

        assert_eq!(epoch_trace_sample_if_enabled(false, &counter), None);
        assert_eq!(counter.load(Ordering::Relaxed), 17);
    }

    #[test]
    fn enabled_drain_trace_keeps_the_existing_first_sample() {
        let counter = AtomicU64::new(0);

        assert_eq!(epoch_trace_sample_if_enabled(true, &counter), Some(0));
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }
}

#[derive(Clone, Copy)]
struct PlatformHooks {
    pin_current_cpu: fn() -> CpuPinGuard,
    exclude_local_execution: fn() -> LocalExecutionGuard,
    in_irq_context: fn() -> bool,
    request_maintenance: fn(CpuId),
}

impl PlatformHooks {
    const fn default() -> Self {
        Self {
            pin_current_cpu: default_pin_current_cpu,
            exclude_local_execution: default_exclude_local_execution,
            in_irq_context: default_in_irq_context,
            request_maintenance: default_request_maintenance,
        }
    }

    fn for_platform<P>() -> Self
    where
        P: PercpuIf + SmpIf + IrqIf,
    {
        Self {
            pin_current_cpu: P::pin_current_cpu,
            exclude_local_execution: P::exclude_local_execution,
            in_irq_context: P::in_irq_context,
            request_maintenance: |cpu| P::send_ipi(cpu, tx_hal::IpiKind::Maintenance),
        }
    }
}

fn default_pin_current_cpu() -> CpuPinGuard {
    CpuPinGuard::new(CpuId(0))
}

fn default_exclude_local_execution() -> LocalExecutionGuard {
    unsafe { LocalExecutionGuard::new(0, default_restore_local_execution) }
}

unsafe fn default_restore_local_execution(_saved_state: usize) {}

fn default_in_irq_context() -> bool {
    false
}

fn default_request_maintenance(_cpu: CpuId) {}

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

#[doc(hidden)]
pub fn init_on_bsp_with_admission_hook_for_test<P>(
    admission_hook: impl FnOnce(),
) -> Result<(), EpochError>
where
    P: PercpuIf + SmpIf + IrqIf,
{
    GLOBAL_DOMAIN.init_on_bsp_with::<P>(admission_hook)
}

pub fn init_on_ap(cpu: CpuId) -> Result<(), EpochError> {
    GLOBAL_DOMAIN.init_on_ap(cpu)
}

#[track_caller]
pub(crate) fn guard() -> Guard<'static> {
    GLOBAL_DOMAIN.guard()
}

#[doc(hidden)]
pub fn guard_with_admission_hook_for_test(admission_hook: impl FnOnce()) -> Guard<'static> {
    GLOBAL_DOMAIN.guard_with_admission_hook(admission_hook)
}

pub(crate) fn borrow_guard() -> Option<Guard<'static>> {
    GLOBAL_DOMAIN.borrow_guard()
}

pub(crate) unsafe fn retire_intrusive(head: NonNull<RcuHead>) -> Result<(), EpochError> {
    unsafe { GLOBAL_DOMAIN.retire_intrusive(head) }
}

pub(crate) fn with_local_retire_guard<R>(
    action: impl FnOnce(&mut LocalRetireGuard) -> R,
) -> Result<R, EpochError> {
    let mut guard = GLOBAL_DOMAIN.local_retire_guard()?;
    Ok(action(&mut guard))
}

pub fn try_reserve_local_retire() -> Result<LocalRetireReservation, EpochError> {
    GLOBAL_DOMAIN.try_reserve_local_retire()
}

pub(crate) fn local_head_bag_summary(
) -> Result<[(u64, usize, usize); super::bag::EPOCH_BAG_COUNT], EpochError> {
    with_local_retire_guard(|local_guard| local_guard.head_bag_summary())
}

pub fn try_drain(budget: usize) -> DrainStats {
    GLOBAL_DOMAIN.try_drain(budget)
}

pub fn service_local_drain_request(budget: usize) -> Option<DrainStats> {
    GLOBAL_DOMAIN.service_local_drain_request(budget)
}

pub fn drain_requested_with_budget(budget: usize) -> DrainStats {
    service_local_drain_request(budget).unwrap_or_default()
}

pub fn offline_cpu(cpu: CpuId) -> Result<(), EpochError> {
    GLOBAL_DOMAIN.offline_cpu(cpu)
}

pub fn summary() -> EpochSummary {
    GLOBAL_DOMAIN.summary()
}

pub fn cpu_summary(cpu: CpuId) -> Option<CpuEpochSummary> {
    GLOBAL_DOMAIN.cpu_summary(cpu)
}

#[doc(hidden)]
pub fn with_local_retire_guard_for_test<R>(action: impl FnOnce() -> R) -> Result<R, EpochError> {
    let guard = GLOBAL_DOMAIN.local_retire_guard()?;
    let result = action();
    drop(guard);
    Ok(result)
}

#[doc(hidden)]
pub fn local_retire_active_for_test() -> bool {
    GLOBAL_DOMAIN.local_retire_active_for_current_cpu()
}

#[doc(hidden)]
pub fn try_advance_with_membership_change_for_test(
    membership_change: impl FnOnce(),
) -> AdvanceAttempt {
    GLOBAL_DOMAIN.try_advance_epoch_with(membership_change)
}

#[doc(hidden)]
pub fn drain_requested_for_test(cpu: CpuId) -> bool {
    GLOBAL_DOMAIN
        .cpu_state(cpu)
        .is_some_and(CpuLocalEpochState::drain_requested)
}

#[doc(hidden)]
pub fn membership_for_test(cpu: CpuId) -> Option<CpuMembership> {
    GLOBAL_DOMAIN
        .cpu_state(cpu)
        .map(CpuLocalEpochState::membership)
}

#[doc(hidden)]
pub fn membership_version_for_test() -> u64 {
    GLOBAL_DOMAIN.membership_version()
}

#[doc(hidden)]
pub unsafe fn reset_for_test() {
    GLOBAL_DOMAIN.reset_for_test();
}

#[doc(hidden)]
pub fn init_for_test() {
    GLOBAL_DOMAIN.init_for_test();
}
