//! D9-C: end-to-end interrupt-wake integration pin.
//!
//! Per D9 §"Phase D9-C" the wake-substrate migration must demonstrate
//! that a thread parked inside `Channel::wait_event(_,
//! WaitProtocol::Interruptible, ...)` is resolved as
//! `WaitOutcome::Interrupted` when another thread calls
//! `step_kill_process(pid, SIGTERM)` — and within a bounded number
//! of reactor ticks. This is the lost-wake fix's primary regression
//! test (pre-D9-A, the parked future would miss the delivered
//! signal because the underlying `Channel` never fired).
//!
//! Setup:
//!
//! 1. Bootstrap a one-thread process and bind a `TaskMailbox` to
//!    its `ThreadPayload`.
//! 2. Submit a reactor task that awaits a custom future. The
//!    future:
//!    - On each poll, registers the task's `Waker` with the bound
//!      `TaskMailbox` so a `SignalDelivered` post calls
//!      `waker.wake_by_ref()`.
//!    - Polls a `Channel::wait_event_with_interrupts(..., interrupts,
//!      || false)` where `interrupts` is a `ThreadInterruptAdapter`
//!      that bridges the thread's `signal_summary` into the reactor's
//!      `InterruptSource` trait.
//!    - The underlying `Channel` is **never** fired in this test —
//!      the only wake path is the mailbox.
//! 3. Run the reactor once to park the task.
//! 4. Call `step_kill_process(proc, SIGTERM)`. This sets
//!    `summary.deliverable_signal` AND posts `SignalDelivered` to
//!    the mailbox, which wakes the task.
//! 5. Run the reactor again; the task re-polls,
//!    `classify_interrupt` observes the deliverable summary, and
//!    the future resolves `WaitOutcome::Interrupted`.
//! 6. Assert the task completes within 100 reactor ticks (the real
//!    path is two: park, then post-wake re-poll).
//!
//! Invariants pinned:
//!
//! - **Interrupted-outcome.** The future returns
//!   `WaitOutcome::Interrupted`, not `Ready` (condition was always
//!   `false`) and not `TimedOut` (no deadline).
//! - **Bounded reactor ticks.** Total polled count across all
//!   reactor passes is ≤ 100 (the implementation requires 2; the
//!   bound is generous to avoid flakes on future refactors).
//! - **The Channel was never fired.** No `channel.fire(...)` call
//!   appears in the test. The only wake comes from the mailbox.

extern crate alloc;

use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll};
use std::sync::Mutex;

use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};
use tx_subsystems::signal::adapter::step_engine::{Cap, PayloadCap, TaskMailbox};
use tx_subsystems::signal::adapter::wait_routing::{
    Channel, InterruptSource, InterruptSummary as ReactorSummary, Mask, Reactor, WaitOutcome,
    WaitProtocol,
};

use tx_subsystems::process::bootstrap_init_process;
use tx_subsystems::process::structure::ProcessIdentity;
use tx_subsystems::signal::{step_kill_process, KillOutcome, Signum};
use tx_subsystems::thread_runtime::structure::ThreadPayload;
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

const WAIT_EVENT_MASK: Mask = Mask::from_bits(0x1);

/// Bridge `ThreadPayload`'s signal-side `InterruptSummary` into the
/// reactor's `InterruptSource` trait. The reactor and signal
/// subsystems each carry an `InterruptSummary` type; this adapter
/// translates between them.
#[derive(Clone)]
struct ThreadInterruptAdapter {
    payload: PayloadCap<ThreadPayload>,
}

impl InterruptSource for ThreadInterruptAdapter {
    fn deliverable_signal_pending(&self) -> bool {
        self.payload.interrupt_summary().deliverable_signal
    }
    fn termination_in_force(&self) -> bool {
        self.payload.interrupt_summary().termination
    }
    fn stop_requested(&self) -> bool {
        self.payload.interrupt_summary().stop_requested
    }
    fn interrupt_summary(&self) -> ReactorSummary {
        let s = self.payload.interrupt_summary();
        ReactorSummary::new(s.deliverable_signal, s.termination, s.stop_requested)
    }
}

/// Wrapper future: on each poll, registers the task's `Waker` with
/// the bound `TaskMailbox` (so a `SignalDelivered` post wakes the
/// task) and then polls the inner `WaitEventFuture`.
///
/// This is the D9-C glue: it materialises the "parked future is
/// woken by a mailbox post" invariant. In production this binding
/// will live inside the reactor's thread-task wrapper; here we
/// inline it for the integration pin.
struct MailboxWakeAdapter<F> {
    inner: F,
    mailbox: Arc<TaskMailbox>,
}

impl<F: Future + Unpin> Future for MailboxWakeAdapter<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.mailbox.register_waker(cx.waker().clone());
        Pin::new(&mut this.inner).poll(cx)
    }
}

#[test]
fn pselect_style_wait_resolves_interrupted_via_signal_mailbox() {
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();

    let proc_cap: Cap<ProcessIdentity> = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
    let leader = proc_cap.nth_thread(0).expect("leader");
    let leader_payload = leader.payload_cap().expect("live leader");

    // Wake mailbox bound to the leader's payload. D9-A's
    // `post_signal_mailbox` will route `SignalDelivered` events
    // here.
    let mailbox = Arc::new(TaskMailbox::new());
    leader_payload.bind_mailbox(Arc::downgrade(&mailbox));

    // The channel is never fired in this test — its only role is
    // to give us a real `WaitEventFuture` shape.
    let channel = Channel::new();

    // The interrupt source reads truth from the thread's
    // `signal_summary`.
    let interrupts = ThreadInterruptAdapter {
        payload: leader_payload.clone(),
    };

    // Outcome slot the task fills in once the wait resolves.
    let outcome: Arc<Mutex<Option<WaitOutcome>>> = Arc::new(Mutex::new(None));

    let reactor = Reactor::new();

    {
        let channel = channel.clone();
        let mailbox = Arc::clone(&mailbox);
        let outcome = Arc::clone(&outcome);
        reactor.submit(async move {
            // Condition is permanently false — the only way to
            // exit the wait is via a deliverable-signal classify.
            let inner = channel.wait_event_with_interrupts(
                WAIT_EVENT_MASK,
                WaitProtocol::Interruptible,
                interrupts,
                || false,
            );
            let result = MailboxWakeAdapter {
                inner,
                mailbox: Arc::clone(&mailbox),
            }
            .await;
            *outcome.lock().expect("outcome slot poisoned") = Some(result);
        });
    }

    // ---- Tick budget: 100 reactor passes -----------------------
    // The real path: tick 1 parks the task (the future polls
    // once, observes `condition() == false` and no summary bits,
    // returns Pending). After we send the signal, tick 2 sees
    // the task runnable, re-polls, and `classify_interrupt`
    // returns `Some(Interrupted)`. So the test budget is 100 to
    // catch any future spin without flagging the legitimate path.
    const MAX_TICKS: usize = 100;
    let mut total_polled = 0usize;

    // ---- Phase 1: park the task --------------------------------
    let stats_park = reactor.run_until_idle();
    total_polled += stats_park.polled;
    assert!(
        stats_park.polled >= 1,
        "first run polls the task at least once"
    );
    assert_eq!(
        stats_park.completed, 0,
        "task must park, not complete (channel never fires, no summary set)"
    );
    assert!(
        outcome.lock().expect("outcome poisoned").is_none(),
        "outcome must be unset while parked"
    );

    // ---- Phase 2: deliver the signal ---------------------------
    // This is the single semantic action under test: post a
    // process-directed SIGTERM. Per D9-A, this:
    //   1. Sets `thread_pending` for SIGTERM.
    //   2. Sets `summary.deliverable_signal` (mask is empty so
    //      the signal is unmasked).
    //   3. Posts `MailboxEvent::SignalDelivered` to the bound
    //      mailbox, which calls `waker.wake_by_ref()`.
    // The reactor's task waker, registered via
    // `MailboxWakeAdapter::poll`, marks the task runnable.
    assert_eq!(
        step_kill_process(&proc_cap, Signum::SIGTERM, None),
        KillOutcome::Delivered
    );
    assert!(
        leader_payload.interrupt_summary().deliverable_signal,
        "D9-A: summary.deliverable_signal set after post_signal",
    );

    // ---- Phase 3: re-poll resolves the wait --------------------
    let stats_resume = reactor.run_until_idle();
    total_polled += stats_resume.polled;
    assert!(
        total_polled <= MAX_TICKS,
        "wake path resolved within {MAX_TICKS} ticks (got {total_polled})",
    );
    assert_eq!(
        stats_resume.completed, 1,
        "task completes on the post-wake re-poll",
    );

    assert_eq!(
        *outcome.lock().expect("outcome poisoned"),
        Some(WaitOutcome::Interrupted),
        "Channel::wait_event resolves to Interrupted via signal mailbox wake",
    );

    tx_test_support::drain_to_quiescence();
}
