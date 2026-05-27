//! D9-A: `MailboxEvent::SignalDelivered` + per-thread mailbox
//! integration tests.
//!
//! Pins the new wake plumbing that runs **alongside** the
//! `InterruptSummary` denormalised view (D2-style coexistence per
//! `docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md`
//! Recommended Option A): `post_signal`, `route_gewalt`'s
//! per-thread loop, and `set_thread_zombie` each post a
//! [`MailboxEvent::SignalDelivered`] to the bound mailbox **after**
//! the existing summary update. The mailbox post is best-effort —
//! an unbound (`None`) slot or a dropped `Weak` is silently skipped.
//!
//! Invariants pinned (bundled into a single integration `#[test]`
//! per the cred-zone / exit-source precedent — `reset_*_for_test`
//! helpers are crate-private and unreachable from integration
//! tests):
//!
//! 1. **post_signal-bound-mailbox**. A thread with a bound
//!    `TaskMailbox` receives a
//!    `MailboxEvent::SignalDelivered { signum, routing:
//!    ProcessDirected }` per `post_signal` call. The summary side
//!    keeps working: `InterruptSummary.deliverable_signal` is set
//!    on the same pass.
//! 2. **route_gewalt-fanout**. `step_kill_process(target, SIGSTOP)`
//!    dispatches through `route_gewalt`, which iterates the
//!    process's threads list. Every live thread's bound mailbox
//!    receives a `SignalDelivered { signum: 19, routing:
//!    ProcessDirected }` event after `summary.stop_requested` is
//!    set. SIGCONT mirrors with signum 18 and clears stop.
//! 3. **set_thread_zombie-posts-terminal**. Before dropping the
//!    payload, `set_thread_zombie` posts a final
//!    `SignalDelivered { signum: SIGKILL, routing:
//!    ProcessDirected }` event so a parked future re-polls and
//!    observes payload-dropped state.
//! 4. **mailbox-unbound-no-post-no-crash**. A thread with no
//!    mailbox bound (the early-bring-up invariant) silently
//!    accepts `post_signal` / `route_gewalt` posts — the
//!    `InterruptSummary` side still updates, no panic.
//! 5. **dropped-weak-silent-skip**. A thread bound to a mailbox
//!    whose strong refs have all been dropped sees the mailbox
//!    post silently skipped: `Weak::upgrade` returns `None`, the
//!    event never queues anywhere, the summary still updates.

extern crate alloc;

use alloc::sync::{Arc, Weak as ArcWeak};
use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};
use tx_subsystems::signal::adapter::step_engine::{Cap, MailboxEvent, SignalRouting, TaskMailbox};

use tx_subsystems::process::bootstrap_init_process;
use tx_subsystems::process::structure::ProcessIdentity;
use tx_subsystems::signal::{step_kill_process, KillOutcome, Signum};
use tx_subsystems::thread_runtime::execution::post_signal;
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

/// Local stub pmap (this integration test cannot see `vm::TestPmap`,
/// which is `pub(crate)` and `cfg(test)`-gated). Mirrors the
/// exit-wait-source integration test's stub.
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

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<StubPmap>().expect("fresh aspace")
}

/// Bind `mailbox` (held as a `Weak` clone) to the thread's payload
/// via the public `bind_mailbox` accessor. Returns `false` if the
/// thread is a zombie (no payload to bind to).
fn bind(thread: &Cap<ThreadIdentity>, mailbox: &Arc<TaskMailbox>) -> bool {
    match thread.payload_cap() {
        Some(payload) => {
            payload.bind_mailbox(Arc::downgrade(mailbox));
            true
        }
        None => false,
    }
}

/// Pop and assert one `SignalDelivered` event matching the
/// expected signum + routing. Panics on mismatch or empty queue.
fn assert_signal_delivered(
    mailbox: &TaskMailbox,
    expected_signum: u32,
    expected_routing: SignalRouting,
) {
    let evt = mailbox
        .poll()
        .expect("mailbox should have a SignalDelivered event");
    match evt {
        MailboxEvent::SignalDelivered { signum, routing } => {
            assert_eq!(signum, expected_signum, "wrong signum in SignalDelivered");
            assert_eq!(
                routing, expected_routing,
                "wrong routing in SignalDelivered"
            );
        }
        other => panic!("expected SignalDelivered, got {other:?}"),
    }
}

#[test]
fn signal_mailbox_phase_a_plumbing() {
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();

    let proc_cap: Cap<ProcessIdentity> = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let leader = proc_cap.nth_thread(0).expect("leader thread");
    assert!(!leader.is_zombie());

    // ---- (4) mailbox-unbound-no-post-no-crash ----------------------
    // Before binding, post_signal must update the summary and not
    // crash on the missing mailbox.
    post_signal(
        &leader,
        Signum::SIGTERM,
        SignalRouting::ProcessDirected,
        None,
    );
    let summary_before_bind = leader
        .payload_cap()
        .expect("live leader has payload")
        .interrupt_summary();
    assert!(
        summary_before_bind.deliverable_signal,
        "summary must still set deliverable_signal even without a bound mailbox",
    );

    // ---- (1) post_signal-bound-mailbox -----------------------------
    let leader_mailbox = Arc::new(TaskMailbox::new());
    assert!(bind(&leader, &leader_mailbox));
    assert!(leader_mailbox.is_empty(), "no events before any post");

    // SIGCHLD is catchable and not yet pending, so this is a fresh
    // post; the mailbox should receive one event.
    post_signal(
        &leader,
        Signum::SIGCHLD,
        SignalRouting::ProcessDirected,
        None,
    );
    assert_signal_delivered(
        &leader_mailbox,
        Signum::SIGCHLD.raw() as u32,
        SignalRouting::ProcessDirected,
    );
    assert!(
        leader_mailbox.is_empty(),
        "exactly one event per post_signal call",
    );

    // ---- (2) route_gewalt-fanout -----------------------------------
    // SIGSTOP routes through `step_kill_process` → `route_gewalt`,
    // which iterates every live thread of the process. The
    // bootstrapped init has one live thread; the loop posts to it
    // after `summary.stop_requested` is set.
    assert_eq!(
        step_kill_process(&proc_cap, Signum::SIGSTOP, None),
        KillOutcome::Delivered,
    );
    let summary_after_stop = leader.payload_cap().expect("alive").interrupt_summary();
    assert!(
        summary_after_stop.stop_requested,
        "route_gewalt sets stop_requested on each live thread (D2 coexistence)"
    );
    assert_signal_delivered(
        &leader_mailbox,
        Signum::SIGSTOP.raw() as u32,
        SignalRouting::ProcessDirected,
    );
    assert!(leader_mailbox.is_empty(), "exactly one event per thread");

    // SIGCONT clears the stop bit and also posts. This pins both
    // the SIGCONT path and that route_gewalt fires on every Gewalt
    // signum (not just SIGSTOP).
    assert_eq!(
        step_kill_process(&proc_cap, Signum::SIGCONT, None),
        KillOutcome::Delivered,
    );
    let summary_after_cont = leader.payload_cap().expect("alive").interrupt_summary();
    assert!(
        !summary_after_cont.stop_requested,
        "SIGCONT clears stop_requested",
    );
    assert_signal_delivered(
        &leader_mailbox,
        Signum::SIGCONT.raw() as u32,
        SignalRouting::ProcessDirected,
    );
    assert!(leader_mailbox.is_empty());

    // ---- (5) dropped-weak-silent-skip ------------------------------
    // Drop the strong ref to the mailbox; the payload's `Weak` will
    // fail to upgrade on the next post. The summary side still
    // updates; no panic / UB.
    drop(leader_mailbox);
    post_signal(
        &leader,
        Signum::SIGINT,
        SignalRouting::ProcessDirected,
        None,
    );
    let summary_after_dangling = leader.payload_cap().expect("alive").interrupt_summary();
    assert!(
        summary_after_dangling.deliverable_signal,
        "summary still updates when the bound mailbox's strong refs are gone",
    );
    // No mailbox to inspect — the pin is "no panic," which we
    // reached by getting here.

    // ---- (3) set_thread_zombie-posts-terminal ----------------------
    // Re-bind a fresh mailbox to the leader, then trigger
    // set_thread_zombie via `step_exit_group_with_signal`. The
    // mailbox should receive a SIGKILL/ProcessDirected event before
    // payload drop.
    let terminal_mailbox = Arc::new(TaskMailbox::new());
    assert!(bind(&leader, &terminal_mailbox));
    assert!(terminal_mailbox.is_empty());

    // Snapshot the Weak so we can observe the strong-ref dance
    // separately from the mailbox's queue.
    let weak_clone: ArcWeak<TaskMailbox> = Arc::downgrade(&terminal_mailbox);

    tx_subsystems::process::execution::step_exit_group_with_signal(&proc_cap, Signum::SIGKILL);

    // Per D9-A: set_thread_zombie posts SIGKILL/ProcessDirected
    // before dropping the payload.
    assert_signal_delivered(
        &terminal_mailbox,
        Signum::SIGKILL.raw() as u32,
        SignalRouting::ProcessDirected,
    );
    assert!(terminal_mailbox.is_empty(), "exactly one terminal post");

    // Payload is gone — leader is now a zombie.
    assert!(leader.is_zombie(), "leader zombified after exit_group");

    // The Weak we cloned out still upgrades (we still hold the Arc
    // strong ref), pinning that the terminal-mailbox post landed on
    // the live mailbox, not on a phantom.
    assert!(
        weak_clone.upgrade().is_some(),
        "weak clone upgrades while strong refs are live",
    );

    // Drop the strong ref; the Weak now fails to upgrade.
    drop(terminal_mailbox);
    assert!(weak_clone.upgrade().is_none());

    // EBR drain to let zone caps recycle.
    tx_test_support::drain_to_quiescence();
}
