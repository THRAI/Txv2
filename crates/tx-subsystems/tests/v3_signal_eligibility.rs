//! D9-B: `step_kill_process` thread-eligibility scan pins.
//!
//! Per D9 §"Phase D9-B" the process-directed kill path must filter
//! the thread selection by `!signal_mask.is_blocked(sig)`. Previously
//! the routine picked the first non-zombie thread regardless of its
//! sigmask (POSIX-conformance gap called out in D9 §2.5).
//!
//! This binary pins the new two-pass selection:
//!
//! 1. **Eligible-first.** When at least one non-zombie thread has
//!    `sig` unblocked, that thread receives the post. Threads with
//!    `sig` blocked are skipped even when they appear earlier in
//!    `payload.threads`.
//! 2. **All-blocked fallback.** When every non-zombie thread has
//!    `sig` blocked, the first non-zombie thread is picked anyway
//!    and the bit is set in its `thread_pending` (Linux semantics:
//!    the signal remains pending in that thread's mask until the
//!    thread unblocks it). The mailbox post still fires; the
//!    summary `deliverable_signal` bit stays `false` because the
//!    target's mask blocks the signal.
//! 3. **Zombie skipping.** Zombified threads are passed over in
//!    both passes (their `payload_cap()` is `None`).
//! 4. **Single-thread sanity.** A one-thread process behaves as
//!    before: the thread is non-zombie and the signal is unblocked
//!    by default, so the post lands on the leader.
//! 5. **Coalescence on double-post.** Two `step_kill_process` calls
//!    with the same signum on the same target both pick the same
//!    eligible thread and set the bit once (bitset coalescence;
//!    POSIX standard-signal contract). Each call still posts to
//!    the mailbox.
//! 6. **All-zombie returns NoLiveThread.** A process with every
//!    thread zombified returns `KillOutcome::NoLiveThread` rather
//!    than panicking.

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};
use tx_subsystems::signal::adapter::step_engine::{Cap, MailboxEvent, SignalRouting, TaskMailbox};

use tx_subsystems::process::bootstrap_init_process;
use tx_subsystems::process::execution::spawn_sibling_thread_for_test;
use tx_subsystems::process::structure::ProcessIdentity;
use tx_subsystems::signal::{step_kill_process, KillOutcome, SignalMask, Signum};
use tx_subsystems::thread_runtime::execution::{
    mark_thread_zombie_for_test, step_sigprocmask, SigmaskHow,
};
use tx_subsystems::thread_runtime::ThreadIdentity;
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

/// Local stub pmap (`vm::TestPmap` is `pub(crate)`).
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

fn bind_mailbox(thread: &Cap<ThreadIdentity>, mailbox: &Arc<TaskMailbox>) {
    let payload = thread.payload_cap().expect("live thread");
    payload.bind_mailbox(Arc::downgrade(mailbox));
}

fn pending_snapshot(thread: &Cap<ThreadIdentity>) -> u64 {
    thread
        .payload_cap()
        .map(|p| p.pending().snapshot())
        .unwrap_or(0)
}

/// Drain all `SignalDelivered` events from `mailbox` whose signum
/// matches `sig`. Returns the count.
fn count_signal_delivered(mailbox: &TaskMailbox, sig: Signum) -> usize {
    let mut count = 0;
    while let Some(evt) = mailbox.poll() {
        match evt {
            MailboxEvent::SignalDelivered {
                signum,
                routing: SignalRouting::ProcessDirected,
            } if signum == sig.raw() as u32 => {
                count += 1;
            }
            _ => panic!("unexpected mailbox event: {evt:?}"),
        }
    }
    count
}

#[test]
fn signal_eligibility_pins() {
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();

    let proc_cap: Cap<ProcessIdentity> = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let leader = proc_cap.nth_thread(0).expect("leader");

    // ---- (4) Single-thread sanity ----
    // The leader has no signals blocked by default; SIGTERM lands
    // on it. This pin documents that the refactor preserves the
    // bootstrapped single-thread shape.
    let leader_mb = Arc::new(TaskMailbox::new());
    bind_mailbox(&leader, &leader_mb);
    assert_eq!(
        step_kill_process(&proc_cap, Signum::SIGTERM, None),
        KillOutcome::Delivered
    );
    assert!(
        leader
            .payload_cap()
            .unwrap()
            .pending()
            .is_pending(Signum::SIGTERM),
        "single-thread case: SIGTERM lands on leader"
    );
    assert_eq!(
        count_signal_delivered(&leader_mb, Signum::SIGTERM),
        1,
        "exactly one mailbox event per kill"
    );

    // Clear the leader's pending bit so it doesn't leak into the
    // next phase.
    leader
        .payload_cap()
        .unwrap()
        .pending()
        .clear(Signum::SIGTERM);

    // Now add two sibling threads: T2, T3. Bind a mailbox to each.
    let t2 = spawn_sibling_thread_for_test(&proc_cap).expect("t2");
    let t3 = spawn_sibling_thread_for_test(&proc_cap).expect("t3");
    let t2_mb = Arc::new(TaskMailbox::new());
    let t3_mb = Arc::new(TaskMailbox::new());
    bind_mailbox(&t2, &t2_mb);
    bind_mailbox(&t3, &t3_mb);

    // ---- (1) Eligible-first ----
    // Block SIGUSR-equivalent (we'll use SIGTERM) on the LEADER
    // and T3. T2 has it unblocked. Even though the leader is at
    // index 0, the post must land on T2.
    let mut block_term = SignalMask::EMPTY;
    block_term.block(Signum::SIGTERM);
    step_sigprocmask(&leader, SigmaskHow::SetMask, block_term);
    step_sigprocmask(&t3, SigmaskHow::SetMask, block_term);
    // T2 keeps the empty mask.

    assert_eq!(
        step_kill_process(&proc_cap, Signum::SIGTERM, None),
        KillOutcome::Delivered
    );
    assert_eq!(
        pending_snapshot(&leader) & Signum::SIGTERM.bit(),
        0,
        "leader blocks SIGTERM and must be skipped"
    );
    assert!(
        t2.payload_cap()
            .unwrap()
            .pending()
            .is_pending(Signum::SIGTERM),
        "T2 (unblocked) receives SIGTERM"
    );
    assert_eq!(
        pending_snapshot(&t3) & Signum::SIGTERM.bit(),
        0,
        "T3 blocks SIGTERM and must be skipped"
    );
    assert_eq!(
        count_signal_delivered(&leader_mb, Signum::SIGTERM),
        0,
        "leader mailbox not posted"
    );
    assert_eq!(
        count_signal_delivered(&t2_mb, Signum::SIGTERM),
        1,
        "T2 mailbox posted exactly once"
    );
    assert_eq!(
        count_signal_delivered(&t3_mb, Signum::SIGTERM),
        0,
        "T3 mailbox not posted"
    );

    // Clean up T2's pending bit so it doesn't leak.
    t2.payload_cap().unwrap().pending().clear(Signum::SIGTERM);

    // ---- (2) All-blocked fallback ----
    // Block SIGTERM on T2 as well. Now every non-zombie thread
    // blocks SIGTERM; the fallback must pick the first
    // non-zombie thread (the leader).
    step_sigprocmask(&t2, SigmaskHow::SetMask, block_term);

    assert_eq!(
        step_kill_process(&proc_cap, Signum::SIGTERM, None),
        KillOutcome::Delivered
    );
    assert!(
        leader
            .payload_cap()
            .unwrap()
            .pending()
            .is_pending(Signum::SIGTERM),
        "fallback: leader (first non-zombie) receives SIGTERM despite blocking"
    );
    assert!(
        !leader
            .payload_cap()
            .unwrap()
            .interrupt_summary()
            .deliverable_signal,
        "summary deliverable_signal stays false when target masks the signal"
    );
    assert_eq!(
        count_signal_delivered(&leader_mb, Signum::SIGTERM),
        1,
        "leader mailbox posted on fallback path (wake-hint regardless of mask)"
    );
    assert_eq!(
        count_signal_delivered(&t2_mb, Signum::SIGTERM),
        0,
        "T2 mailbox not posted in fallback path"
    );
    assert_eq!(
        count_signal_delivered(&t3_mb, Signum::SIGTERM),
        0,
        "T3 mailbox not posted in fallback path"
    );

    // ---- (5) Coalescence on double-post ----
    // Unblock SIGTERM on T2 so it becomes the eligible target;
    // post twice. The bit set in T2's pending is idempotent
    // (bitset coalescence) but the mailbox receives two events
    // (one per kill call) — the queue is a queue.
    step_sigprocmask(&t2, SigmaskHow::SetMask, SignalMask::EMPTY);
    // Clear the leader bit from the prior fallback so we can
    // distinguish.
    leader
        .payload_cap()
        .unwrap()
        .pending()
        .clear(Signum::SIGTERM);
    // Drain any residual mailbox events accumulated.
    while leader_mb.poll().is_some() {}
    while t2_mb.poll().is_some() {}
    while t3_mb.poll().is_some() {}

    assert_eq!(
        step_kill_process(&proc_cap, Signum::SIGTERM, None),
        KillOutcome::Delivered
    );
    assert_eq!(
        step_kill_process(&proc_cap, Signum::SIGTERM, None),
        KillOutcome::Delivered
    );
    assert!(
        t2.payload_cap()
            .unwrap()
            .pending()
            .is_pending(Signum::SIGTERM),
        "T2 still has the bit (idempotent)"
    );
    assert_eq!(
        pending_snapshot(&t2) & Signum::SIGTERM.bit(),
        Signum::SIGTERM.bit(),
        "exactly one bit set (coalescence)"
    );
    assert_eq!(
        count_signal_delivered(&t2_mb, Signum::SIGTERM),
        2,
        "two mailbox events for two kill calls"
    );

    // ---- (3) Zombie skipping ----
    // Zombify T2; the new eligible target should now be... well,
    // every thread still blocks SIGTERM via the leader+T3, so
    // we'll unblock the leader to make it the eligible target
    // after T2 is gone. First confirm zombify works mid-scan.
    mark_thread_zombie_for_test(&t2, 0);
    assert!(t2.is_zombie());
    // Drain residual events.
    while leader_mb.poll().is_some() {}
    while t3_mb.poll().is_some() {}

    step_sigprocmask(&leader, SigmaskHow::SetMask, SignalMask::EMPTY);
    // T3 still blocks SIGTERM. The scan order is [leader, T2, T3];
    // T2 is a zombie (skipped), leader is now unblocked, so the
    // post must land on leader.
    leader
        .payload_cap()
        .unwrap()
        .pending()
        .clear(Signum::SIGTERM);
    assert_eq!(
        step_kill_process(&proc_cap, Signum::SIGTERM, None),
        KillOutcome::Delivered
    );
    assert!(
        leader
            .payload_cap()
            .unwrap()
            .pending()
            .is_pending(Signum::SIGTERM),
        "leader is the eligible non-zombie target"
    );
    assert_eq!(
        pending_snapshot(&t3) & Signum::SIGTERM.bit(),
        0,
        "T3 (still blocks SIGTERM) untouched"
    );
    assert_eq!(
        count_signal_delivered(&leader_mb, Signum::SIGTERM),
        1,
        "leader mailbox got the wake-hint"
    );
    assert_eq!(
        count_signal_delivered(&t3_mb, Signum::SIGTERM),
        0,
        "T3 mailbox quiet"
    );

    // ---- (6) All-zombie returns NoLiveThread ----
    // Zombify the leader and T3 too. Now every thread is a
    // zombie; the call must return `NoLiveThread`.
    mark_thread_zombie_for_test(&leader, 0);
    mark_thread_zombie_for_test(&t3, 0);
    assert!(leader.is_zombie());
    assert!(t3.is_zombie());

    assert_eq!(
        step_kill_process(&proc_cap, Signum::SIGINT, None),
        KillOutcome::NoLiveThread,
        "all-zombie process returns NoLiveThread, not panic"
    );

    tx_test_support::drain_to_quiescence();
}
