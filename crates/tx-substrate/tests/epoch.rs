use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use static_assertions::assert_not_impl_any;
use tx_hal::{CpuId, EntropyIf, IpiKind, IrqIf, LocalExecutionGuard, PercpuIf, SmpIf};
use tx_substrate::epoch::{self, testing, EpochError};

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static RECLAIM_COUNT: AtomicUsize = AtomicUsize::new(0);
static REENTRANT_RECLAIM_COUNT: AtomicUsize = AtomicUsize::new(0);
static IRQ_ENABLED: [AtomicBool; 2] = [AtomicBool::new(true), AtomicBool::new(true)];
static IRQ_RESTORE_COUNT: AtomicUsize = AtomicUsize::new(0);
static IRQ_ENTRY_COUNT: AtomicUsize = AtomicUsize::new(0);
static RESTORE_SAW_RETIRE_ACTIVE: AtomicBool = AtomicBool::new(false);
static REENTRANT_INTRUSIVE_NODE: AtomicPtr<testing::IntrusiveTestNode> =
    AtomicPtr::new(core::ptr::null_mut());
static SAME_INTRUSIVE_NODE: AtomicPtr<testing::IntrusiveTestNode> =
    AtomicPtr::new(core::ptr::null_mut());
static SAME_NODE_REENQUEUE_REJECTED: AtomicBool = AtomicBool::new(false);
static MAINTENANCE_IPI_COUNT: AtomicUsize = AtomicUsize::new(0);

struct TestPlatform;

std::thread_local! {
    static CURRENT_CPU: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

fn set_current_cpu(cpu: usize) {
    CURRENT_CPU.with(|current| current.set(cpu));
}

fn current_cpu() -> usize {
    CURRENT_CPU.with(core::cell::Cell::get)
}

impl PercpuIf for TestPlatform {
    fn current_cpu_id() -> CpuId {
        CpuId(current_cpu())
    }
}
impl IrqIf for TestPlatform {
    fn interrupts_enabled() -> bool {
        IRQ_ENABLED[current_cpu()].load(Ordering::Acquire)
    }

    fn exclude_local_execution() -> LocalExecutionGuard {
        let cpu = current_cpu();
        let enabled = IRQ_ENABLED[cpu].swap(false, Ordering::AcqRel) as usize;
        let saved = (cpu << 1) | enabled;
        unsafe { LocalExecutionGuard::new(saved, restore_local_execution) }
    }
}
impl EntropyIf for TestPlatform {}
impl SmpIf for TestPlatform {
    fn possible_cpu_count() -> usize {
        2
    }

    fn is_cpu_online(cpu: CpuId) -> bool {
        cpu.0 < 2
    }

    fn send_ipi(target: CpuId, kind: IpiKind) {
        assert_eq!(target, CpuId(1));
        assert_eq!(kind, IpiKind::Maintenance);
        MAINTENANCE_IPI_COUNT.fetch_add(1, Ordering::AcqRel);
    }
}

unsafe fn count_reclaim(_ptr: *mut u8) {
    RECLAIM_COUNT.fetch_add(1, Ordering::AcqRel);
}

unsafe fn reclaim_and_retire_again(_ptr: *mut u8) {
    assert!(<TestPlatform as IrqIf>::interrupts_enabled());
    REENTRANT_RECLAIM_COUNT.fetch_add(1, Ordering::AcqRel);
    let node = REENTRANT_INTRUSIVE_NODE.swap(core::ptr::null_mut(), Ordering::AcqRel);
    assert!(!node.is_null());
    unsafe {
        testing::retire_intrusive_for_test(&mut *node).expect("callback recursive retire");
    }
    let _ = epoch::try_drain(usize::MAX);
}

unsafe fn reject_same_intrusive_reenqueue(_ptr: *mut u8) {
    let node = SAME_INTRUSIVE_NODE.swap(core::ptr::null_mut(), Ordering::AcqRel);
    assert!(!node.is_null());
    let error = unsafe { testing::retire_intrusive_for_test(&mut *node) }
        .expect_err("a detached node must remain marked queued during callback");
    SAME_NODE_REENQUEUE_REJECTED.store(error == EpochError::RetireBagOccupied, Ordering::Release);
}

unsafe fn restore_local_execution(saved: usize) {
    RESTORE_SAW_RETIRE_ACTIVE.store(testing::local_retire_active_for_test(), Ordering::Release);
    let cpu = saved >> 1;
    assert_eq!(current_cpu(), cpu, "local execution guard crossed CPUs");
    IRQ_ENABLED[cpu].store(saved & 1 != 0, Ordering::Release);
    IRQ_RESTORE_COUNT.fetch_add(1, Ordering::AcqRel);
}

fn try_run_same_cpu_irq(action: impl FnOnce()) -> bool {
    if !IRQ_ENABLED[current_cpu()].load(Ordering::Acquire) {
        return false;
    }
    IRQ_ENTRY_COUNT.fetch_add(1, Ordering::AcqRel);
    action();
    true
}

fn reset_epoch() {
    unsafe {
        testing::reset_for_test();
    }
    RECLAIM_COUNT.store(0, Ordering::Release);
    REENTRANT_RECLAIM_COUNT.store(0, Ordering::Release);
    set_current_cpu(0);
    for enabled in &IRQ_ENABLED {
        enabled.store(true, Ordering::Release);
    }
    IRQ_RESTORE_COUNT.store(0, Ordering::Release);
    IRQ_ENTRY_COUNT.store(0, Ordering::Release);
    RESTORE_SAW_RETIRE_ACTIVE.store(false, Ordering::Release);
    REENTRANT_INTRUSIVE_NODE.store(core::ptr::null_mut(), Ordering::Release);
    SAME_INTRUSIVE_NODE.store(core::ptr::null_mut(), Ordering::Release);
    SAME_NODE_REENQUEUE_REJECTED.store(false, Ordering::Release);
    MAINTENANCE_IPI_COUNT.store(0, Ordering::Release);
    epoch::init_on_bsp::<TestPlatform>().expect("epoch init");
    epoch::init_on_ap(CpuId(1)).expect("epoch AP init");
}

fn retired_ptr() -> *mut u8 {
    NonNull::<u8>::dangling().as_ptr()
}

#[test]
fn local_retire_guard_nested_save_restore() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();

    let outer = <TestPlatform as IrqIf>::exclude_local_execution();
    assert!(!<TestPlatform as IrqIf>::interrupts_enabled());
    let inner = <TestPlatform as IrqIf>::exclude_local_execution();
    assert!(!<TestPlatform as IrqIf>::interrupts_enabled());

    drop(inner);
    assert!(!<TestPlatform as IrqIf>::interrupts_enabled());
    assert_eq!(IRQ_RESTORE_COUNT.load(Ordering::Acquire), 1);
    drop(outer);
    assert!(<TestPlatform as IrqIf>::interrupts_enabled());
    assert_eq!(IRQ_RESTORE_COUNT.load(Ordering::Acquire), 2);
}

#[test]
fn local_retire_guard_preserves_pre_disabled_state() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    IRQ_ENABLED[0].store(false, Ordering::Release);

    let guard = <TestPlatform as IrqIf>::exclude_local_execution();
    drop(guard);

    assert!(!<TestPlatform as IrqIf>::interrupts_enabled());
    assert_eq!(IRQ_RESTORE_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn local_retire_guard_clears_active_before_restoring_execution() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();

    testing::with_local_retire_guard_for_test(|| {
        assert!(testing::local_retire_active_for_test());
        assert!(!<TestPlatform as IrqIf>::interrupts_enabled());
        assert_eq!(IRQ_RESTORE_COUNT.load(Ordering::Acquire), 0);
    })
    .expect("local retire guard");

    assert!(!RESTORE_SAW_RETIRE_ACTIVE.load(Ordering::Acquire));
    assert!(<TestPlatform as IrqIf>::interrupts_enabled());
    assert_eq!(IRQ_RESTORE_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn local_retire_guard_blocks_same_cpu_simulated_irq_retire() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut node = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        count_reclaim,
    ));

    testing::with_local_retire_guard_for_test(|| {
        assert!(!try_run_same_cpu_irq(|| unsafe {
            testing::retire_intrusive_for_test(&mut node).expect("IRQ retire")
        }));
        assert_eq!(IRQ_ENTRY_COUNT.load(Ordering::Acquire), 0);
    })
    .expect("local retire guard");

    assert!(try_run_same_cpu_irq(|| unsafe {
        testing::retire_intrusive_for_test(&mut node).expect("IRQ retire")
    }));
    assert_eq!(IRQ_ENTRY_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn local_retire_guard_blocks_epoch_advance_and_nested_drain() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let before = epoch::summary().global_epoch;

    testing::with_local_retire_guard_for_test(|| {
        let stats = epoch::try_drain(usize::MAX);
        assert_eq!(stats.advanced_epochs, 0);
        assert_eq!(epoch::summary().global_epoch, before);
        assert!(testing::local_retire_active_for_test());
    })
    .expect("local retire guard");
}

#[test]
fn local_retire_guard_reclaim_callback_allows_recursive_retire_and_drain() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut second = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        count_reclaim,
    ));
    let mut first = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        reclaim_and_retire_again,
    ));
    REENTRANT_INTRUSIVE_NODE.store(&mut *second, Ordering::Release);
    unsafe {
        testing::retire_intrusive_for_test(&mut first).expect("initial retire");
    }

    let first = epoch::try_drain(usize::MAX);
    let second = epoch::try_drain(usize::MAX);

    assert_eq!(first.bag_reclaimed, 0);
    assert_eq!(second.bag_reclaimed, 1);
    assert_eq!(REENTRANT_RECLAIM_COUNT.load(Ordering::Acquire), 1);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 0);

    for _ in 0..3 {
        let _ = epoch::try_drain(usize::MAX);
    }
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn local_retire_guard_remote_cpu_active_blocks_epoch_advance() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
    let release = std::sync::Arc::new(std::sync::Barrier::new(2));
    let remote_entered = entered.clone();
    let remote_release = release.clone();

    let remote = std::thread::spawn(move || {
        set_current_cpu(1);
        testing::with_local_retire_guard_for_test(|| {
            remote_entered.wait();
            remote_release.wait();
        })
        .expect("remote local retire guard");
    });

    entered.wait();
    let before = epoch::summary().global_epoch;
    let blocked = epoch::try_drain(0);
    assert_eq!(blocked.advanced_epochs, 0);
    assert_eq!(epoch::summary().global_epoch, before);

    release.wait();
    remote.join().expect("remote retire thread");
    let advanced = epoch::try_drain(0);
    assert_eq!(advanced.advanced_epochs, 1);
}

#[test]
fn local_retire_guard_is_not_send_or_sync() {
    assert_not_impl_any!(LocalExecutionGuard: Send, Sync);
}

#[test]
fn intrusive_bag_reclaims_only_after_two_epoch_advances() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut node = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        count_reclaim,
    ));
    unsafe {
        testing::retire_intrusive_for_test(&mut node).expect("intrusive retire");
    }

    let first = epoch::try_drain(usize::MAX);
    assert_eq!(first.bag_reclaimed, 0);
    assert_eq!(first.bag_remaining, 1);
    let second = epoch::try_drain(usize::MAX);
    assert_eq!(second.bag_reclaimed, 1);
    assert_eq!(second.bag_remaining, 0);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn intrusive_singleton_rejects_double_enqueue() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut node = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        count_reclaim,
    ));

    unsafe {
        testing::retire_intrusive_for_test(&mut node).expect("first intrusive retire");
    }
    let error = unsafe { testing::retire_intrusive_for_test(&mut node) }
        .expect_err("a queued singleton must reject a second enqueue");

    assert_eq!(error, EpochError::RetireBagOccupied);
    let summary = epoch::cpu_summary(CpuId(0)).expect("CPU summary");
    assert_eq!(summary.bag_retired, 1);
    assert_eq!(summary.publication_pending, 0);
}

#[test]
fn drain_examines_only_budgeted_nodes() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut nodes: Vec<_> = (0..10)
        .map(|_| {
            Box::new(testing::IntrusiveTestNode::new(
                retired_ptr(),
                count_reclaim,
            ))
        })
        .collect();
    for node in &mut nodes {
        unsafe {
            testing::retire_intrusive_for_test(node).expect("intrusive retire");
        }
    }

    let _ = epoch::try_drain(0);
    let drained = epoch::try_drain(2);
    assert_eq!(drained.bag_reclaimed, 2);
    assert!(drained.examined <= 2 + 3, "examined={}", drained.examined);
    assert_eq!(drained.bag_remaining, 8);
}

#[test]
fn drain_callback_can_retire_reentrantly() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut second = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        count_reclaim,
    ));
    let mut first = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        reclaim_and_retire_again,
    ));
    REENTRANT_INTRUSIVE_NODE.store(&mut *second, Ordering::Release);
    unsafe {
        testing::retire_intrusive_for_test(&mut first).expect("first intrusive retire");
    }

    let _ = epoch::try_drain(usize::MAX);
    let reclaimed_first = epoch::try_drain(usize::MAX);
    assert_eq!(reclaimed_first.bag_reclaimed, 1);
    assert_eq!(reclaimed_first.bag_remaining, 1);
    assert_eq!(REENTRANT_RECLAIM_COUNT.load(Ordering::Acquire), 1);

    for _ in 0..3 {
        let _ = epoch::try_drain(usize::MAX);
    }
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn detached_intrusive_node_stays_queued_during_callback() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut node = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        reject_same_intrusive_reenqueue,
    ));
    SAME_INTRUSIVE_NODE.store(&mut *node, Ordering::Release);
    unsafe {
        testing::retire_intrusive_for_test(&mut node).expect("intrusive retire");
    }

    let _ = epoch::try_drain(usize::MAX);
    let drained = epoch::try_drain(usize::MAX);

    assert_eq!(drained.bag_reclaimed, 1);
    assert!(SAME_NODE_REENQUEUE_REJECTED.load(Ordering::Acquire));
    assert_eq!(drained.bag_remaining, 0);
}

#[test]
fn remote_cpu_summary_reads_atomic_retire_snapshot() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let queued = std::sync::Arc::new(std::sync::Barrier::new(2));
    let release = std::sync::Arc::new(std::sync::Barrier::new(2));
    let remote_queued = std::sync::Arc::clone(&queued);
    let remote_release = std::sync::Arc::clone(&release);

    let remote = std::thread::spawn(move || {
        set_current_cpu(1);
        let mut node = Box::new(testing::IntrusiveTestNode::new(
            retired_ptr(),
            count_reclaim,
        ));
        unsafe {
            testing::retire_intrusive_for_test(&mut node).expect("remote intrusive retire");
        }
        remote_queued.wait();
        remote_release.wait();
        let _ = epoch::try_drain(usize::MAX);
        let drained = epoch::try_drain(usize::MAX);
        assert_eq!(drained.bag_reclaimed, 1);
    });

    queued.wait();
    let summary = epoch::cpu_summary(CpuId(1)).expect("remote CPU summary");
    assert_eq!(summary.bag_retired, 1);
    assert_eq!(summary.publication_pending, 0);
    release.wait();
    remote.join().expect("remote retire thread");
}

#[test]
fn epoch_advance_restarts_on_membership_change() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    unsafe {
        testing::reset_for_test();
    }
    set_current_cpu(0);
    epoch::init_on_bsp::<TestPlatform>().expect("epoch init");

    let result = testing::try_advance_with_membership_change_for_test(|| {
        epoch::init_on_ap(CpuId(1)).expect("AP admission during epoch scan");
    });

    assert!(result.advanced);
    assert_eq!(result.scan_attempts, 2);
    assert_eq!(result.version_after, result.version_before + 1);
}

#[test]
fn ap_admission_waits_for_in_progress_bsp_initialization() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    unsafe {
        testing::reset_for_test();
    }
    let bsp_locked = std::sync::Arc::new(std::sync::Barrier::new(2));
    let release_bsp = std::sync::Arc::new(std::sync::Barrier::new(2));
    let ap_started = std::sync::Arc::new(std::sync::Barrier::new(2));
    let ap_done = std::sync::Arc::new(AtomicBool::new(false));

    let bsp_locked_thread = bsp_locked.clone();
    let release_bsp_thread = release_bsp.clone();
    let bsp = std::thread::spawn(move || {
        set_current_cpu(0);
        testing::init_on_bsp_with_admission_hook_for_test::<TestPlatform>(|| {
            bsp_locked_thread.wait();
            release_bsp_thread.wait();
        })
        .expect("BSP epoch init");
    });
    bsp_locked.wait();

    let ap_started_thread = ap_started.clone();
    let ap_done_thread = ap_done.clone();
    let ap = std::thread::spawn(move || {
        set_current_cpu(1);
        ap_started_thread.wait();
        epoch::init_on_ap(CpuId(1)).expect("AP waits for BSP init");
        ap_done_thread.store(true, Ordering::Release);
    });
    ap_started.wait();
    for _ in 0..1_000 {
        std::thread::yield_now();
    }
    assert!(!ap_done.load(Ordering::Acquire));

    release_bsp.wait();
    bsp.join().expect("BSP init thread");
    ap.join().expect("AP init thread");
    assert!(ap_done.load(Ordering::Acquire));
}

#[test]
fn blocked_ring_reuse_requests_owner_local_drain_once() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut node = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        count_reclaim,
    ));

    set_current_cpu(1);
    unsafe {
        testing::retire_intrusive_for_test(&mut node).expect("remote intrusive retire");
    }
    set_current_cpu(0);
    assert_eq!(epoch::try_drain(0).advanced_epochs, 1);
    assert_eq!(epoch::try_drain(0).advanced_epochs, 1);
    assert_eq!(epoch::try_drain(0).advanced_epochs, 0);
    assert!(testing::drain_requested_for_test(CpuId(1)));
    assert_eq!(MAINTENANCE_IPI_COUNT.load(Ordering::Acquire), 1);
    assert_eq!(epoch::try_drain(0).advanced_epochs, 0);
    assert_eq!(MAINTENANCE_IPI_COUNT.load(Ordering::Acquire), 1);

    set_current_cpu(1);
    let serviced = epoch::service_local_drain_request(usize::MAX)
        .expect("owner CPU must consume its maintenance request");
    assert_eq!(serviced.bag_reclaimed, 1);
    assert!(epoch::service_local_drain_request(usize::MAX).is_none());
    assert!(!testing::drain_requested_for_test(CpuId(1)));
}

#[test]
fn cpu_offline_transfers_all_bag_heads() {
    let _isolation = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut nodes: Vec<_> = (0..3)
        .map(|_| {
            Box::new(testing::IntrusiveTestNode::new(
                retired_ptr(),
                count_reclaim,
            ))
        })
        .collect();

    let last_node = nodes.len() - 1;
    for (index, node) in nodes.iter_mut().enumerate() {
        set_current_cpu(1);
        unsafe {
            testing::retire_intrusive_for_test(node).expect("remote intrusive retire");
        }
        set_current_cpu(0);
        if index != last_node {
            assert_eq!(epoch::try_drain(0).advanced_epochs, 1);
        }
    }
    assert_eq!(
        epoch::cpu_summary(CpuId(1))
            .expect("CPU 1 summary")
            .bag_retired,
        3
    );

    let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
    let release = std::sync::Arc::new(std::sync::Barrier::new(2));
    let remote_entered = entered.clone();
    let remote_release = release.clone();
    let reader = std::thread::spawn(move || {
        set_current_cpu(1);
        let guard = epoch::guard();
        remote_entered.wait();
        remote_release.wait();
        drop(guard);
    });
    entered.wait();

    let offlined = std::sync::Arc::new(AtomicBool::new(false));
    let offlined_result = offlined.clone();
    let coordinator = std::thread::spawn(move || {
        set_current_cpu(0);
        epoch::offline_cpu(CpuId(1)).expect("offline CPU 1");
        offlined_result.store(true, Ordering::Release);
    });
    while testing::cpu_membership_for_test(CpuId(1)) != testing::CpuMembershipForTest::Draining {
        core::hint::spin_loop();
    }
    assert!(!offlined.load(Ordering::Acquire));
    set_current_cpu(1);
    let mut rejected = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        count_reclaim,
    ));
    assert_eq!(
        unsafe { testing::retire_intrusive_for_test(&mut rejected) },
        Err(EpochError::CpuNotOnline)
    );
    let rejected_guard = std::panic::catch_unwind(epoch::guard);
    assert!(rejected_guard.is_err());

    release.wait();
    reader.join().expect("remote reader");
    coordinator.join().expect("offline coordinator");
    assert!(offlined.load(Ordering::Acquire));
    assert_eq!(
        testing::cpu_membership_for_test(CpuId(1)),
        testing::CpuMembershipForTest::Offline
    );
    assert_eq!(
        epoch::cpu_summary(CpuId(0))
            .expect("coordinator summary")
            .bag_retired,
        3
    );
    assert_eq!(
        epoch::cpu_summary(CpuId(1))
            .expect("offlined CPU summary")
            .bag_retired,
        0
    );

    set_current_cpu(0);
    for _ in 0..4 {
        let _ = epoch::try_drain(usize::MAX);
    }
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 3);
}

#[test]
fn guard_delays_reclaim_until_drop() {
    let _guard = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut node = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        count_reclaim,
    ));

    let epoch_guard = epoch::guard();
    unsafe {
        testing::retire_intrusive_for_test(&mut node).expect("retire");
    }

    let blocked = epoch::try_drain(usize::MAX);
    assert_eq!(blocked.bag_reclaimed, 0);
    assert_eq!(blocked.active_guards, 1);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 0);

    drop(epoch_guard);

    let drained = epoch::try_drain(usize::MAX);
    assert_eq!(drained.bag_reclaimed, 1);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn drain_budget_limits_reclaim_work() {
    let _guard = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    reset_epoch();
    let mut nodes: Vec<_> = (0..3)
        .map(|_| {
            Box::new(testing::IntrusiveTestNode::new(
                retired_ptr(),
                count_reclaim,
            ))
        })
        .collect();

    for node in &mut nodes {
        unsafe {
            testing::retire_intrusive_for_test(node).expect("retire");
        }
    }

    let not_yet = epoch::try_drain(2);
    assert_eq!(not_yet.bag_reclaimed, 0);

    let first = epoch::try_drain(2);
    assert_eq!(first.bag_reclaimed, 2);
    assert_eq!(first.bag_remaining, 1);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 2);

    let second = epoch::try_drain(2);
    assert_eq!(second.bag_reclaimed, 1);
    assert_eq!(second.bag_remaining, 0);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 3);
}

#[test]
fn retire_requires_initialization() {
    let _guard = EPOCH_TEST_LOCK.lock().expect("epoch test lock");
    unsafe {
        testing::reset_for_test();
    }
    let mut node = Box::new(testing::IntrusiveTestNode::new(
        retired_ptr(),
        count_reclaim,
    ));

    let not_initialized = unsafe { testing::retire_intrusive_for_test(&mut node) }
        .expect_err("retire before init should fail");
    assert_eq!(not_initialized, EpochError::NotInitialized);
}
