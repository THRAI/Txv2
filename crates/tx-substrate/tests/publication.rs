use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, MutexGuard};

use tx_hal::{CpuId, IrqIf, LocalExecutionGuard, PercpuIf, SmpIf};
use tx_substrate::epoch;
use tx_substrate::{testing, PublishError, Published};

static PUBLICATION_TEST_LOCK: Mutex<()> = Mutex::new(());

std::thread_local! {
    static PUBLICATION_CPU: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

struct TwoCpuPlatform;

fn set_publication_cpu(cpu: usize) {
    PUBLICATION_CPU.with(|current| current.set(cpu));
}

impl PercpuIf for TwoCpuPlatform {
    fn current_cpu_id() -> CpuId {
        CpuId(PUBLICATION_CPU.with(core::cell::Cell::get))
    }
}

unsafe fn restore_publication_execution(_saved: usize) {}

impl IrqIf for TwoCpuPlatform {
    fn exclude_local_execution() -> LocalExecutionGuard {
        unsafe { LocalExecutionGuard::new(0, restore_publication_execution) }
    }
}

impl SmpIf for TwoCpuPlatform {
    fn possible_cpu_count() -> usize {
        2
    }
}

#[derive(Debug)]
struct DropProbe {
    id: usize,
    drops: Arc<AtomicUsize>,
}

struct PanicDropProbe {
    panic: bool,
    drops: Arc<AtomicUsize>,
}

impl Drop for PanicDropProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::AcqRel);
        assert!(!self.panic, "injected publication destructor panic");
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Snapshot {
    sequence: usize,
    payload: [usize; 4],
}

fn isolate() -> MutexGuard<'static, ()> {
    let guard = PUBLICATION_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    testing::init_host_for_test_once();
    testing::fail_next_publication_allocations(0);
    for _ in 0..4 {
        let _ = epoch::drain_with_budget(usize::MAX);
    }
    guard
}

fn drain_all_publication_work() {
    for _ in 0..6 {
        let _ = epoch::drain_with_budget(usize::MAX);
    }
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn published_snapshot_is_send_sync_when_payload_is_send_sync() {
    assert_send_sync::<Published<Snapshot>>();
}

#[test]
fn try_new_reports_allocation_failure_and_drops_input() {
    let _isolation = isolate();
    let drops = Arc::new(AtomicUsize::new(0));
    testing::fail_next_publication_allocations(1);

    let result = Published::try_new(DropProbe {
        id: 1,
        drops: Arc::clone(&drops),
    });

    assert_eq!(result.err(), Some(PublishError::Allocation));
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn prepare_replace_reports_allocation_failure_without_changing_root() {
    let _isolation = isolate();
    let drops = Arc::new(AtomicUsize::new(0));
    let cell = Published::try_new(DropProbe {
        id: 1,
        drops: Arc::clone(&drops),
    })
    .expect("initial publication");
    testing::fail_next_publication_allocations(1);

    let result = cell.prepare_replace(DropProbe {
        id: 2,
        drops: Arc::clone(&drops),
    });

    assert_eq!(result.err(), Some(PublishError::Allocation));
    let guard = epoch::guard();
    assert_eq!(cell.read(&guard).id, 1);
    drop(guard);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    drop(cell);
    assert_eq!(drops.load(Ordering::Acquire), 2);
}

#[test]
fn uncommitted_reservation_rolls_back_node_and_writer_claim() {
    let _isolation = isolate();
    let drops = Arc::new(AtomicUsize::new(0));
    let cell = Published::try_new(DropProbe {
        id: 1,
        drops: Arc::clone(&drops),
    })
    .expect("initial publication");

    drop(
        cell.prepare_replace(DropProbe {
            id: 2,
            drops: Arc::clone(&drops),
        })
        .expect("prepared rollback"),
    );
    assert_eq!(drops.load(Ordering::Acquire), 1);

    cell.prepare_replace(DropProbe {
        id: 3,
        drops: Arc::clone(&drops),
    })
    .expect("writer claim released by rollback")
    .commit();
    let guard = epoch::guard();
    assert_eq!(cell.read(&guard).id, 3);
    drop(guard);

    drop(cell);
    drain_all_publication_work();
    assert_eq!(drops.load(Ordering::Acquire), 3);
}

#[test]
fn acquire_read_observes_fully_initialized_snapshot() {
    let _isolation = isolate();
    let cell = Published::try_new(Snapshot {
        sequence: 7,
        payload: [11, 13, 17, 19],
    })
    .expect("initial publication");

    let guard = epoch::guard();
    assert_eq!(
        *cell.read(&guard),
        Snapshot {
            sequence: 7,
            payload: [11, 13, 17, 19],
        }
    );
}

#[test]
fn commit_makes_the_prepared_snapshot_visible() {
    let _isolation = isolate();
    let cell = Published::try_new(Snapshot {
        sequence: 1,
        payload: [1; 4],
    })
    .expect("initial publication");

    cell.prepare_replace(Snapshot {
        sequence: 2,
        payload: [2, 3, 5, 7],
    })
    .expect("prepared replacement")
    .commit();

    let guard = epoch::guard();
    assert_eq!(cell.read(&guard).sequence, 2);
    assert_eq!(cell.read(&guard).payload, [2, 3, 5, 7]);
    drop(guard);
    drop(cell);
    drain_all_publication_work();
}

#[test]
fn old_root_survives_reader_and_two_epoch_grace() {
    let _isolation = isolate();
    let drops = Arc::new(AtomicUsize::new(0));
    let cell = Published::try_new(DropProbe {
        id: 1,
        drops: Arc::clone(&drops),
    })
    .expect("initial publication");
    let guard = epoch::guard();
    let old = cell.read(&guard);

    cell.prepare_replace(DropProbe {
        id: 2,
        drops: Arc::clone(&drops),
    })
    .expect("prepared replacement")
    .commit();
    let retired_epoch = epoch::summary().global_epoch;

    let blocked = epoch::drain_with_budget(usize::MAX);
    assert_eq!(blocked.bag_reclaimed, 0);
    assert_eq!(old.id, 1);
    assert_eq!(drops.load(Ordering::Acquire), 0);

    drop(guard);
    let grace_complete = epoch::drain_with_budget(usize::MAX);
    assert_eq!(grace_complete.bag_reclaimed, 1);
    assert_eq!(grace_complete.publication_remaining, 1);
    assert!(epoch::summary().global_epoch >= retired_epoch + 2);
    assert_eq!(drops.load(Ordering::Acquire), 0);

    let deferred = epoch::drain_with_budget(1);
    assert_eq!(deferred.publication_dropped, 1);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    drop(cell);
    assert_eq!(drops.load(Ordering::Acquire), 2);
}

#[test]
fn concurrent_reader_keeps_old_snapshot_while_writer_commits() {
    let _isolation = isolate();
    let cell = Arc::new(
        Published::try_new(Snapshot {
            sequence: 1,
            payload: [10; 4],
        })
        .expect("initial publication"),
    );
    let reader_entered = Arc::new(Barrier::new(2));
    let writer_committed = Arc::new(Barrier::new(2));

    let reader_cell = Arc::clone(&cell);
    let reader_entered_remote = Arc::clone(&reader_entered);
    let writer_committed_remote = Arc::clone(&writer_committed);
    let reader = std::thread::spawn(move || {
        let guard = epoch::guard();
        let observed = reader_cell.read(&guard);
        reader_entered_remote.wait();
        writer_committed_remote.wait();
        assert_eq!(observed.sequence, 1);
        assert_eq!(observed.payload, [10; 4]);
    });

    reader_entered.wait();
    cell.prepare_replace(Snapshot {
        sequence: 2,
        payload: [20; 4],
    })
    .expect("prepared replacement")
    .commit();
    writer_committed.wait();
    reader.join().expect("reader thread");

    let guard = epoch::guard();
    assert_eq!(cell.read(&guard).sequence, 2);
    drop(guard);
    drop(cell);
    drain_all_publication_work();
}

#[test]
fn published_drop_synchronously_destroys_exclusive_current_root() {
    let _isolation = isolate();
    let drops = Arc::new(AtomicUsize::new(0));
    let cell = Published::try_new(DropProbe {
        id: 1,
        drops: Arc::clone(&drops),
    })
    .expect("initial publication");

    drop(cell);

    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn commit_has_no_post_swap_error_or_allocation_step() {
    let _isolation = isolate();
    let cell = Published::try_new(Snapshot {
        sequence: 1,
        payload: [1; 4],
    })
    .expect("initial publication");
    let reservation = cell
        .prepare_replace(Snapshot {
            sequence: 2,
            payload: [2; 4],
        })
        .expect("prepared replacement");
    testing::fail_next_publication_allocations(1);

    let committed: () = reservation.commit();
    assert_eq!(committed, ());
    assert_eq!(
        Published::try_new(Snapshot {
            sequence: 3,
            payload: [3; 4],
        })
        .err(),
        Some(PublishError::Allocation),
        "commit must not consume the injected allocation failure"
    );
    let guard = epoch::guard();
    assert_eq!(cell.read(&guard).sequence, 2);
    drop(guard);
    drop(cell);
    drain_all_publication_work();
}

#[test]
fn commit_recovers_when_prior_retire_bags_need_full_drain() {
    let _isolation = isolate();
    let cell = Published::try_new(Snapshot {
        sequence: 0,
        payload: [0; 4],
    })
    .expect("initial publication");

    for sequence in 1..=3 {
        cell.prepare_replace(Snapshot {
            sequence,
            payload: [sequence; 4],
        })
        .expect("prepared replacement")
        .commit();
        let _ = epoch::drain_with_budget(0);
    }

    cell.prepare_replace(Snapshot {
        sequence: 4,
        payload: [4; 4],
    })
    .expect("prepared replacement after budget-limited drains")
    .commit();

    let guard = epoch::guard();
    assert_eq!(cell.read(&guard).sequence, 4);
    drop(guard);
    drop(cell);
    drain_all_publication_work();
}

#[test]
fn maintenance_budget_limits_deferred_type_drops() {
    let _isolation = isolate();
    let drops = Arc::new(AtomicUsize::new(0));
    let cell = Published::try_new(DropProbe {
        id: 0,
        drops: Arc::clone(&drops),
    })
    .expect("initial publication");

    for id in 1..=3 {
        cell.prepare_replace(DropProbe {
            id,
            drops: Arc::clone(&drops),
        })
        .expect("prepared replacement")
        .commit();
    }

    assert_eq!(epoch::drain_with_budget(usize::MAX).bag_reclaimed, 0);
    let grace_complete = epoch::drain_with_budget(usize::MAX);
    assert_eq!(grace_complete.bag_reclaimed, 3);
    assert_eq!(grace_complete.publication_dropped, 0);
    assert_eq!(grace_complete.publication_remaining, 3);
    assert_eq!(drops.load(Ordering::Acquire), 0);

    for expected in 1..=3 {
        let stats = epoch::drain_with_budget(1);
        assert_eq!(stats.publication_dropped, 1);
        assert_eq!(stats.publication_remaining, 3 - expected);
        assert_eq!(drops.load(Ordering::Acquire), expected);
    }

    drop(cell);
    assert_eq!(drops.load(Ordering::Acquire), 4);
}

#[test]
fn writer_claim_serializes_until_uncommitted_reservation_rolls_back() {
    let _isolation = isolate();
    let cell = Arc::new(
        Published::try_new(Snapshot {
            sequence: 1,
            payload: [1; 4],
        })
        .expect("initial publication"),
    );
    let held = cell
        .prepare_replace(Snapshot {
            sequence: 2,
            payload: [2; 4],
        })
        .expect("first writer reservation");
    let attempting = Arc::new(Barrier::new(2));
    let finished = Arc::new(core::sync::atomic::AtomicBool::new(false));
    let remote_cell = Arc::clone(&cell);
    let remote_attempting = Arc::clone(&attempting);
    let remote_finished = Arc::clone(&finished);
    let writer = std::thread::spawn(move || {
        remote_attempting.wait();
        remote_cell
            .prepare_replace(Snapshot {
                sequence: 3,
                payload: [3; 4],
            })
            .expect("second writer reservation")
            .commit();
        remote_finished.store(true, Ordering::Release);
    });

    attempting.wait();
    for _ in 0..1_000 {
        std::thread::yield_now();
    }
    assert!(!finished.load(Ordering::Acquire));
    drop(held);
    writer.join().expect("second writer");
    assert!(finished.load(Ordering::Acquire));
    let guard = epoch::guard();
    assert_eq!(cell.read(&guard).sequence, 3);
    drop(guard);
    drop(cell);
    drain_all_publication_work();
}

#[test]
fn cpu_offline_transfers_publication_deferred_drop_list() {
    let _isolation = isolate();
    unsafe {
        epoch::testing::reset_for_test();
    }
    set_publication_cpu(0);
    epoch::init_on_bsp::<TwoCpuPlatform>().expect("two-CPU epoch init");
    epoch::init_on_ap(CpuId(1)).expect("CPU 1 epoch admission");
    let drops = Arc::new(AtomicUsize::new(0));

    set_publication_cpu(1);
    let cell = Published::try_new(DropProbe {
        id: 1,
        drops: Arc::clone(&drops),
    })
    .expect("initial publication");
    cell.prepare_replace(DropProbe {
        id: 2,
        drops: Arc::clone(&drops),
    })
    .expect("replacement")
    .commit();
    let _ = epoch::drain_with_budget(usize::MAX);
    let grace = epoch::drain_with_budget(usize::MAX);
    assert_eq!(grace.bag_reclaimed, 1);
    assert_eq!(grace.publication_remaining, 1);
    assert_eq!(drops.load(Ordering::Acquire), 0);

    set_publication_cpu(0);
    epoch::offline_cpu(CpuId(1)).expect("offline CPU 1");
    let transferred = epoch::drain_with_budget(1);
    assert_eq!(transferred.publication_dropped, 1);
    assert_eq!(transferred.publication_remaining, 0);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    drop(cell);
    assert_eq!(drops.load(Ordering::Acquire), 2);

    unsafe {
        epoch::testing::reset_for_test();
    }
    epoch::testing::init_for_test();
}

#[test]
fn deferred_drop_list_grows_to_1088_nodes() {
    let _isolation = isolate();
    let drops = Arc::new(AtomicUsize::new(0));
    let cell = Published::try_new(DropProbe {
        id: 0,
        drops: Arc::clone(&drops),
    })
    .expect("initial publication");
    let replacements = 1088;
    for id in 1..=replacements {
        cell.prepare_replace(DropProbe {
            id,
            drops: Arc::clone(&drops),
        })
        .expect("unbounded publication replacement")
        .commit();
    }

    let _ = epoch::drain_with_budget(usize::MAX);
    let grace = epoch::drain_with_budget(usize::MAX);
    assert_eq!(grace.bag_reclaimed, replacements);
    assert_eq!(grace.publication_remaining, replacements);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    let deferred = epoch::drain_with_budget(usize::MAX);
    assert_eq!(deferred.publication_dropped, replacements);
    assert_eq!(deferred.publication_remaining, 0);
    assert_eq!(drops.load(Ordering::Acquire), replacements);
    drop(cell);
    assert_eq!(drops.load(Ordering::Acquire), replacements + 1);
}

#[test]
fn deferred_destructor_panic_preserves_undropped_tail() {
    let _isolation = isolate();
    let drops = Arc::new(AtomicUsize::new(0));
    let cell = Published::try_new(PanicDropProbe {
        panic: false,
        drops: Arc::clone(&drops),
    })
    .expect("initial publication");
    cell.prepare_replace(PanicDropProbe {
        panic: true,
        drops: Arc::clone(&drops),
    })
    .expect("panic replacement")
    .commit();
    cell.prepare_replace(PanicDropProbe {
        panic: false,
        drops: Arc::clone(&drops),
    })
    .expect("current replacement")
    .commit();
    let _ = epoch::drain_with_budget(usize::MAX);
    let grace = epoch::drain_with_budget(usize::MAX);
    assert_eq!(grace.publication_remaining, 2);

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = epoch::drain_with_budget(1);
    }));
    assert!(panic.is_err());
    assert_eq!(drops.load(Ordering::Acquire), 1);

    let recovered = epoch::drain_with_budget(usize::MAX);
    assert_eq!(recovered.publication_dropped, 1);
    assert_eq!(recovered.publication_remaining, 0);
    assert_eq!(drops.load(Ordering::Acquire), 2);
    drop(cell);
    assert_eq!(drops.load(Ordering::Acquire), 3);
}
