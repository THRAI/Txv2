//! D9-D: signalfd subsystem integration tests.
//!
//! Pins the per-process subscription registry and the
//! signal → signalfd routing path (`step_kill_process` →
//! `signalfd::notify_process_signal` → matching subscriptions push
//! onto their pending queues + fire their wait sources).
//!
//! Invariants pinned (bundled into a single integration `#[test]`
//! per the mailbox / userfaultfd-scaffold precedent — `reset_*_for_test`
//! helpers are crate-private and unreachable from integration
//! tests):
//!
//! 1. **signalfd-create-mints-cap**. `signalfd_create(proc, mask)`
//!    returns a `Cap<SignalFd>` with a fresh `sfd_id` and the
//!    requested mask installed; the cap is registered with the
//!    per-process subscription list.
//! 2. **kill-routes-to-signalfd**. `step_kill_process(proc, SIGUSR1)`
//!    with a signalfd registered for `SIGUSR1` pushes a signum onto
//!    the signalfd's pending queue and fires the wait source. The
//!    signalfd's `read(2)`-shape returns one `signalfd_siginfo`
//!    record with `ssi_signo == SIGUSR1`.
//! 3. **kill-filters-by-mask**. `step_kill_process(proc, SIGUSR2)`
//!    against a signalfd subscribed only to SIGUSR1 is dropped on
//!    the floor (the signalfd's pending queue stays empty).
//! 4. **eagain-on-empty-and-nonblock**. `signalfd_read` with an
//!    empty queue and `nonblocking = true` returns
//!    `StepOutcome::Err(EAGAIN)`.
//! 5. **yield-on-empty-and-blocking**. `signalfd_read` with an
//!    empty queue and `nonblocking = false` returns
//!    `StepOutcome::Yield { OnWaitSource }` carrying the per-fd
//!    wait source id — the dispatcher would park on it until a
//!    signal arrives.
//! 6. **drop-unregisters**. Dropping a `Cap<SignalFd>` releases the
//!    subscription entry so subsequent `step_kill_process` calls
//!    no longer route to the dropped cap. (Verified by counting
//!    `notify_process_signal`'s returned `delivered` count before
//!    and after drop.)

extern crate alloc;

use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};
use tx_substrate::epoch;
use tx_substrate::testing::init_host_for_test_once;
use tx_subsystems::signalfd::adapter::step_engine::{Cap, StepOutcome, V3Errno, YieldShape};

use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity};
use tx_subsystems::signal::{step_kill_process, KillOutcome, Signum};
use tx_subsystems::signalfd::{
    notify_process_signal, signalfd_create, signalfd_read, SignalFd, SIGNALFD_SIGINFO_SIZE,
};
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct StubPmap;

static NEXT_ROOT_ID: AtomicUsize = AtomicUsize::new(1);

impl PmapIf for StubPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let id = NEXT_ROOT_ID.fetch_add(1, Ordering::AcqRel);
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(id * 4096)),
            Asid(id as u16),
        ))
    }

    fn destroy_pmap_root(_root: PmapRoot) {}

    fn reserve_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }

    fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {}

    fn commit_mapping(
        _root: &PmapRoot,
        _reservation: PmapReservation,
        _permissions: PmapPermissions,
    ) {
    }

    fn unmap_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        Ok(Some(PmapUnmapResult::new(virt, PhysAddr(virt.0), kind)))
    }

    fn protect_mapping(
        _root: &PmapRoot,
        virt: VirtAddr,
        kind: PmapReserveKind,
        _permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        Ok(Some(PmapInvalidation::new(virt, kind.size())))
    }

    fn shootdown_kernel_mapping(_invalidation: PmapInvalidation) {}

    fn shootdown_mapping(_asid: Asid, _invalidation: PmapInvalidation) {}
}

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    init_host_for_test_once();
    let _ = zones::register_all();
    drain_to_quiescence();
    guard
}

fn drain_to_quiescence() {
    let mut quiet = 0u32;
    while quiet < 2 {
        let stats = epoch::drain_with_budget(usize::MAX);
        if stats.reclaimed == 0 {
            quiet += 1;
        } else {
            quiet = 0;
        }
    }
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<StubPmap>().expect("fresh aspace")
}

fn read_one_signo(sfd: &SignalFd) -> u32 {
    let mut buf = [0u8; SIGNALFD_SIGINFO_SIZE];
    match signalfd_read(sfd, &mut buf, /* nonblocking = */ true) {
        StepOutcome::Done(n) => {
            assert_eq!(n, SIGNALFD_SIGINFO_SIZE);
            u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
        }
        other => panic!("expected Done({SIGNALFD_SIGINFO_SIZE}), got {other:?}"),
    }
}

#[test]
fn signalfd_d9d_pins_create_route_and_drain() {
    let _g = setup();

    let proc_cap: Cap<ProcessIdentity> = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let proc_key = proc_cap.key().raw();

    // SIGUSR1 = 10 on Linux generic; SIGUSR2 = 12.
    let sigusr1 = Signum::new(10).expect("SIGUSR1");
    let sigusr2 = Signum::new(12).expect("SIGUSR2");

    // ---- (1) signalfd-create-mints-cap ------------------------------
    let sfd_a = signalfd_create(&proc_cap, sigusr1.bit()).expect("create sfd_a");
    assert!(sfd_a.sfd_id() > 0, "sfd_id must be a positive monotonic id");
    assert_eq!(sfd_a.mask(), sigusr1.bit());

    let sfd_b = signalfd_create(&proc_cap, sigusr2.bit()).expect("create sfd_b");
    assert_ne!(
        sfd_a.sfd_id(),
        sfd_b.sfd_id(),
        "each SignalFd must mint a fresh sfd_id"
    );

    // ---- (2) kill-routes-to-signalfd --------------------------------
    // SIGUSR1 covers sfd_a (mask = SIGUSR1) but not sfd_b (mask =
    // SIGUSR2). `step_kill_process` posts to the thread-eligibility
    // first, then fans out to every matching signalfd subscription.
    assert_eq!(
        step_kill_process(&proc_cap, sigusr1),
        KillOutcome::Delivered,
    );
    assert_eq!(
        sfd_a.pending_count(),
        1,
        "sfd_a's mask covers SIGUSR1; one entry pushed"
    );

    // Drain the entry — `read(2)` returns one siginfo record.
    let signo = read_one_signo(&sfd_a);
    assert_eq!(signo, sigusr1.raw() as u32);
    assert_eq!(sfd_a.pending_count(), 0, "queue drained");

    // ---- (3) kill-filters-by-mask -----------------------------------
    // sfd_b's mask is SIGUSR2 only; sfd_a's mask is SIGUSR1 only.
    // SIGUSR1 delivered above must have left sfd_b alone.
    assert_eq!(
        sfd_b.pending_count(),
        0,
        "SIGUSR1 must not have pushed to a SIGUSR2-masked signalfd"
    );

    // Now deliver SIGUSR2 — sfd_b receives it, sfd_a does not.
    assert_eq!(
        step_kill_process(&proc_cap, sigusr2),
        KillOutcome::Delivered,
    );
    assert_eq!(sfd_b.pending_count(), 1, "sfd_b's mask covers SIGUSR2");
    assert_eq!(sfd_a.pending_count(), 0, "sfd_a still empty");
    let signo_b = read_one_signo(&sfd_b);
    assert_eq!(signo_b, sigusr2.raw() as u32);

    // ---- (4) eagain-on-empty-and-nonblock ---------------------------
    let mut buf = [0u8; SIGNALFD_SIGINFO_SIZE];
    match signalfd_read(&sfd_a, &mut buf, /* nonblocking = */ true) {
        StepOutcome::Err(V3Errno::EAGAIN) => {}
        other => panic!("expected EAGAIN, got {other:?}"),
    }

    // ---- (5) yield-on-empty-and-blocking ----------------------------
    let mut buf = [0u8; SIGNALFD_SIGINFO_SIZE];
    match signalfd_read(&sfd_a, &mut buf, /* nonblocking = */ false) {
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { source, interests },
            ..
        } => {
            assert_eq!(
                source.raw(),
                sfd_a.wait_source_id(),
                "yield carries the per-fd wait source id",
            );
            assert!(
                interests.raw() != 0,
                "yield interest mask must include the readable bit",
            );
        }
        other => panic!("expected Yield on WaitSource, got {other:?}"),
    }

    // ---- (6) drop-unregisters ---------------------------------------
    // Two signalfds registered against `proc_key` cover SIGUSR1 +
    // SIGUSR2 respectively. SIGUSR1 should route to *exactly one*
    // (sfd_a). Now drop sfd_a; the next SIGUSR1 should route to zero.
    let delivered_before = notify_process_signal(proc_key, sigusr1);
    // Drop one queued entry (we just pushed via notify_process_signal).
    let _ = sfd_a.pop_pending();
    assert_eq!(
        delivered_before, 1,
        "exactly one subscription covers SIGUSR1 before drop",
    );

    drop(sfd_a);
    drain_to_quiescence();

    let delivered_after = notify_process_signal(proc_key, sigusr1);
    assert_eq!(
        delivered_after, 0,
        "after dropping sfd_a, SIGUSR1 routes to zero subscriptions",
    );

    // sfd_b is still alive and covers SIGUSR2; verify it still
    // receives.
    let delivered_b = notify_process_signal(proc_key, sigusr2);
    assert_eq!(delivered_b, 1);
    let _ = sfd_b.pop_pending();
    drop(sfd_b);

    drain_to_quiescence();
}
