use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tx_hal::PmapIf;
use tx_substrate::{SpinMutex, SpinWait};

std::thread_local! {
    static COUNT_PLATFORM_PROGRESS: Cell<bool> = const { Cell::new(false) };
}

static PLATFORM_PROGRESS_CALLS: AtomicUsize = AtomicUsize::new(0);
static PLATFORM_PROGRESS_REENTERED: AtomicBool = AtomicBool::new(false);
static PROGRESS_SIDE_LOCK: SpinMutex<()> = SpinMutex::new(());

struct ProgressTestPmap;

impl PmapIf for ProgressTestPmap {
    fn service_pending_tlb_shootdown() {
        COUNT_PLATFORM_PROGRESS.with(|enabled| {
            if !enabled.get() {
                return;
            }
            let guard = PROGRESS_SIDE_LOCK
                .try_lock()
                .expect("progress hook must not hold the independent side lock");
            PLATFORM_PROGRESS_REENTERED.store(true, Ordering::Release);
            drop(guard);
            PLATFORM_PROGRESS_CALLS.fetch_add(1, Ordering::AcqRel);
        });
    }
}

#[test]
fn spinmutex_default_type_has_lock_metrics_disabled() {
    let mutex = SpinMutex::new(0u64);

    assert!(!mutex.lock_metrics_enabled());
}

#[test]
fn spinmutex_observed_constructor_preserves_lock_api() {
    let mutex = tx_substrate::SpinMutex::new_observed(
        0u64,
        tx_substrate::LockMetricsOn::new(b"debug.lock.test"),
    );

    assert_eq!(mutex.lock_metrics_enabled(), cfg!(tx_lock_metrics));
    {
        let mut guard = mutex.lock();
        *guard = 11;
    }
    assert_eq!(*mutex.lock(), 11);
}

#[test]
fn spinmutex_lock_unlock_round_trip() {
    // Construct via the public `const fn new`, then take and mutate
    // through the guard. Drop the guard, re-lock, and confirm the
    // previous mutation is observed — proves both `Deref`/`DerefMut`
    // and the release path on drop.
    let mutex = SpinMutex::new(0u64);
    {
        let mut guard = mutex.lock();
        *guard = 7;
    }
    {
        let guard = mutex.lock();
        assert_eq!(*guard, 7);
    }
    // Re-lock once more to confirm the lock is releaseable across
    // multiple acquire/release cycles.
    {
        let mut guard = mutex.lock();
        *guard += 1;
    }
    assert_eq!(*mutex.lock(), 8);
}

#[test]
fn spinmutex_holds_send_payload() {
    // `T: Send` lets `SpinMutex<T>` be `Sync`. Use a thread to prove
    // the `unsafe impl<T: Send> Sync` is honoured at the type level.
    let mutex = std::sync::Arc::new(SpinMutex::new(0i32));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let m = std::sync::Arc::clone(&mutex);
            std::thread::spawn(move || {
                for _ in 0..1_000 {
                    let mut g = m.lock();
                    *g += 1;
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread join");
    }
    assert_eq!(*mutex.lock(), 4_000);
}

#[test]
fn spin_progress_is_bounded_and_precedes_contended_acquisition() {
    COUNT_PLATFORM_PROGRESS.with(|enabled| enabled.set(true));
    PLATFORM_PROGRESS_CALLS.store(0, Ordering::Release);
    PLATFORM_PROGRESS_REENTERED.store(false, Ordering::Release);

    // Before substrate installs a platform, the default tick is a no-op.
    let mut uninstalled = SpinWait::new();
    uninstalled.tick();
    assert_eq!(PLATFORM_PROGRESS_CALLS.load(Ordering::Acquire), 0);

    // An explicit hook runs on failed attempts 1, 64, 128, ... rather than on
    // every iteration.
    let explicit_calls = Cell::new(0usize);
    let mut explicit = SpinWait::new();
    for _ in 0..63 {
        explicit.tick_with(|| explicit_calls.set(explicit_calls.get() + 1));
    }
    assert_eq!(explicit_calls.get(), 1);
    explicit.tick_with(|| explicit_calls.set(explicit_calls.get() + 1));
    assert_eq!(explicit_calls.get(), 2);

    tx_substrate::sync::install_platform_spin_progress::<ProgressTestPmap>();

    // An uncontended acquisition never calls the installed callback.
    let uncontended = SpinMutex::new(());
    let guard = uncontended.lock();
    assert_eq!(PLATFORM_PROGRESS_CALLS.load(Ordering::Acquire), 0);
    drop(guard);

    // Hold the target lock until the waiter has completed its first callback.
    // The callback takes only an independent try-lock, proving it does not need
    // the lock whose acquisition it is helping to make progress around.
    let target = Arc::new(SpinMutex::new(()));
    let owner = target.lock();
    let waiter_acquired = Arc::new(AtomicBool::new(false));
    let waiter_target = Arc::clone(&target);
    let waiter_acquired_flag = Arc::clone(&waiter_acquired);
    let waiter = std::thread::spawn(move || {
        COUNT_PLATFORM_PROGRESS.with(|enabled| enabled.set(true));
        let _guard = waiter_target.lock();
        waiter_acquired_flag.store(true, Ordering::Release);
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    while PLATFORM_PROGRESS_CALLS.load(Ordering::Acquire) == 0 {
        assert!(
            Instant::now() < deadline,
            "contended progress hook did not run"
        );
        std::thread::yield_now();
    }
    assert!(PLATFORM_PROGRESS_REENTERED.load(Ordering::Acquire));
    assert!(!waiter_acquired.load(Ordering::Acquire));

    drop(owner);
    waiter.join().expect("contended waiter");
    assert!(waiter_acquired.load(Ordering::Acquire));
    COUNT_PLATFORM_PROGRESS.with(|enabled| enabled.set(false));
}
