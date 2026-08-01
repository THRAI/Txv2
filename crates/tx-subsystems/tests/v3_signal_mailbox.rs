//! D9-A: `MailboxEvent::SignalDelivered` + per-thread mailbox
//! integration tests.
//!
//! Pins the new wake plumbing that runs **alongside** the
//! `InterruptSummary` denormalised view (D2-style coexistence per
//! `docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md`
//! Recommended Option A): `post_signal_with_post`, Gewalt routing's
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
//! 1. **catchable-signal-bound-mailbox**. A thread with a bound
//!    `TaskMailbox` receives a
//!    `MailboxEvent::SignalDelivered { signum, routing:
//!    ProcessDirected }` per catchable signal post. The summary side
//!    keeps working: `InterruptSummary.deliverable_signal` is set
//!    on the same pass.
//! 2. **Gewalt fanout**. The process-directed `_with_post` helper
//!    dispatches through `route_gewalt_with_post`, which iterates the
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
//!    accepts `post_signal_with_post` / Gewalt routing posts — the
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

use tx_substrate::step::InterestMask;
use tx_subsystems::process::bootstrap_init_process;
use tx_subsystems::process::structure::ProcessIdentity;
use tx_subsystems::signal::{
    step_kill_process_with_post, step_kill_process_with_posts, KillOutcome, Signum,
};
use tx_subsystems::signalfd::SignalFd;
use tx_subsystems::thread_runtime::execution::post_signal_with_post;
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

fn deliver_signal_with_direct_post_for_test(
    thread: &Cap<ThreadIdentity>,
    sig: Signum,
    routing: SignalRouting,
    info: Option<tx_subsystems::signal::SigInfo>,
) {
    post_signal_with_post(thread, sig, routing, info, |weak, event| {
        let Some(mailbox) = weak.upgrade() else {
            return;
        };
        let _ = mailbox.post(event);
    });
}

fn kill_process_direct_for_test(target: &Cap<ProcessIdentity>, sig: Signum) -> KillOutcome {
    step_kill_process_with_post(target, sig, None, |weak, event| {
        let Some(mailbox) = weak.upgrade() else {
            return;
        };
        let _ = mailbox.post(event);
    })
}

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
    // Before binding, the catchable signal helper must update the summary and not
    // crash on the missing mailbox.
    deliver_signal_with_direct_post_for_test(
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

    // ---- (1) catchable-signal-bound-mailbox ------------------------
    let leader_mailbox = Arc::new(TaskMailbox::new());
    assert!(bind(&leader, &leader_mailbox));
    assert!(leader_mailbox.is_empty(), "no events before any post");

    // SIGCHLD is catchable and not yet pending, so this is a fresh
    // post; the mailbox should receive one event.
    deliver_signal_with_direct_post_for_test(
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
        "exactly one event per catchable signal post",
    );

    // ---- (1b) catchable-signal-with-injected-post ------------------
    // The owner-aware wake convergence path keeps signal semantic
    // mutation here but lets a caller with reactor context inject the
    // mailbox post operation. The injected closure below still lands
    // exactly one SignalDelivered event and the summary remains the
    // truth-bearing side.
    let injected_posts = AtomicUsize::new(0);
    post_signal_with_post(
        &leader,
        Signum::SIGQUIT,
        SignalRouting::ProcessDirected,
        None,
        |weak, event| {
            injected_posts.fetch_add(1, Ordering::SeqCst);
            let mailbox = weak.upgrade().expect("bound mailbox is live");
            let _ = mailbox.post(event);
        },
    );
    assert_eq!(
        injected_posts.load(Ordering::SeqCst),
        1,
        "post_signal_with_post must invoke the injected post once"
    );
    assert_signal_delivered(
        &leader_mailbox,
        Signum::SIGQUIT.raw() as u32,
        SignalRouting::ProcessDirected,
    );
    assert!(
        leader
            .payload_cap()
            .expect("alive")
            .interrupt_summary()
            .deliverable_signal,
        "injected-post path still updates interrupt summary before waking",
    );

    // ---- (1c) process-kill-with-injected-post ----------------------
    // Process-directed signal routing owns target-thread selection,
    // but the final mailbox post must still be injectable so a caller
    // with reactor context can route through the owner-aware wake path.
    let process_injected_posts = AtomicUsize::new(0);
    assert_eq!(
        step_kill_process_with_post(&proc_cap, Signum::SIGPIPE, None, |weak, event| {
            process_injected_posts.fetch_add(1, Ordering::SeqCst);
            let mailbox = weak.upgrade().expect("bound mailbox is live");
            let _ = mailbox.post(event);
        }),
        KillOutcome::Delivered,
    );
    assert_eq!(
        process_injected_posts.load(Ordering::SeqCst),
        1,
        "step_kill_process_with_post must inject the catchable-signal post",
    );
    assert_signal_delivered(
        &leader_mailbox,
        Signum::SIGPIPE.raw() as u32,
        SignalRouting::ProcessDirected,
    );

    // ---- (1d) process-kill-with-injected-signalfd-post ------------
    // signalfd readiness is a wait-source event, so it must use the
    // mailbox-ref post seam while the primary signal mailbox event
    // keeps using the weak-mailbox seam.
    let sigusr1 = Signum::new(10).expect("SIGUSR1");
    let signalfd =
        SignalFd::new_cap_for_process(&proc_cap, sigusr1.bit()).expect("signalfd cap for process");
    let signalfd_mailbox = Arc::new(TaskMailbox::new());
    let signalfd_generation = signalfd_mailbox.next_generation();
    let signalfd_registration = signalfd
        .read_endpoint()
        .prepare(
            Arc::downgrade(&signalfd_mailbox),
            signalfd_generation,
            InterestMask::new(0x1),
        )
        .install_if(|| true)
        .expect("signalfd wait-source registration");
    let signal_posts = AtomicUsize::new(0);
    let signalfd_posts = AtomicUsize::new(0);
    assert_eq!(
        step_kill_process_with_posts(
            &proc_cap,
            sigusr1,
            None,
            |weak, event| {
                signal_posts.fetch_add(1, Ordering::SeqCst);
                let mailbox = weak.upgrade().expect("bound mailbox is live");
                let _ = mailbox.post(event);
            },
            |mailbox, event| {
                signalfd_posts.fetch_add(1, Ordering::SeqCst);
                mailbox.post(event)
            },
        ),
        KillOutcome::Delivered,
    );
    assert_eq!(
        signal_posts.load(Ordering::SeqCst),
        1,
        "process signal delivery must still use the weak-mailbox post seam",
    );
    assert_eq!(
        signalfd_posts.load(Ordering::SeqCst),
        1,
        "signalfd readiness must use the mailbox-ref post seam",
    );
    assert_eq!(
        signalfd.pending_count(),
        1,
        "signalfd owns the pending signal queue before waking readers",
    );
    assert_signal_delivered(
        &leader_mailbox,
        sigusr1.raw() as u32,
        SignalRouting::ProcessDirected,
    );
    let event = signalfd_mailbox
        .poll()
        .expect("signalfd wait-source should receive SourceFired");
    match event {
        MailboxEvent::SourceFired {
            generation,
            source,
            interests,
        } => {
            assert_eq!(generation, signalfd_generation);
            assert_eq!(source.raw(), signalfd.wait_source_id());
            assert_eq!(interests.raw(), 0x1);
        }
        other => panic!("expected signalfd SourceFired, got {other:?}"),
    }
    drop(signalfd_registration);

    // ---- (2) Gewalt fanout -----------------------------------------
    // SIGSTOP routes through the process-directed helper -> `route_gewalt_with_post`,
    // which iterates every live thread of the process. The
    // bootstrapped init has one live thread; the loop posts to it
    // after `summary.stop_requested` is set.
    assert_eq!(
        kill_process_direct_for_test(&proc_cap, Signum::SIGSTOP),
        KillOutcome::Delivered,
    );
    let summary_after_stop = leader.payload_cap().expect("alive").interrupt_summary();
    assert!(
        summary_after_stop.stop_requested,
        "Gewalt routing sets stop_requested on each live thread (D2 coexistence)"
    );
    assert_signal_delivered(
        &leader_mailbox,
        Signum::SIGSTOP.raw() as u32,
        SignalRouting::ProcessDirected,
    );
    assert!(leader_mailbox.is_empty(), "exactly one event per thread");

    // SIGCONT clears the stop bit and also posts. This pins both
    // the SIGCONT path and that Gewalt routing fires on every Gewalt
    // signum (not just SIGSTOP).
    assert_eq!(
        kill_process_direct_for_test(&proc_cap, Signum::SIGCONT),
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

    // The same Gewalt fanout must be injectable at the process
    // producer boundary, not only at the lower mailbox helper.
    let stop_injected_posts = AtomicUsize::new(0);
    assert_eq!(
        step_kill_process_with_post(&proc_cap, Signum::SIGSTOP, None, |weak, event| {
            stop_injected_posts.fetch_add(1, Ordering::SeqCst);
            let mailbox = weak.upgrade().expect("bound mailbox is live");
            let _ = mailbox.post(event);
        }),
        KillOutcome::Delivered,
    );
    assert_eq!(
        stop_injected_posts.load(Ordering::SeqCst),
        1,
        "step_kill_process_with_post must inject SIGSTOP fanout posts",
    );
    assert_signal_delivered(
        &leader_mailbox,
        Signum::SIGSTOP.raw() as u32,
        SignalRouting::ProcessDirected,
    );
    assert!(leader_mailbox.is_empty());

    // ---- (5) dropped-weak-silent-skip ------------------------------
    // Drop the strong ref to the mailbox; the payload's `Weak` will
    // fail to upgrade on the next post. The summary side still
    // updates; no panic / UB.
    drop(leader_mailbox);
    deliver_signal_with_direct_post_for_test(
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
    // set_thread_zombie via fatal group exit. The
    // mailbox should receive a SIGKILL/ProcessDirected event before
    // payload drop.
    let terminal_mailbox = Arc::new(TaskMailbox::new());
    assert!(bind(&leader, &terminal_mailbox));
    assert!(terminal_mailbox.is_empty());

    // Snapshot the Weak so we can observe the strong-ref dance
    // separately from the mailbox's queue.
    let weak_clone: ArcWeak<TaskMailbox> = Arc::downgrade(&terminal_mailbox);

    let terminal_injected_posts = AtomicUsize::new(0);
    assert_eq!(
        step_kill_process_with_post(&proc_cap, Signum::SIGKILL, None, |weak, event| {
            terminal_injected_posts.fetch_add(1, Ordering::SeqCst);
            let mailbox = weak.upgrade().expect("bound mailbox is live");
            let _ = mailbox.post(event);
        }),
        KillOutcome::Delivered,
    );
    assert_eq!(
        terminal_injected_posts.load(Ordering::SeqCst),
        1,
        "SIGKILL terminal zombify path must inject the final wake post",
    );

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
