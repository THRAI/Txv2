//! Integration tests for `tx_scripts::drive`.
//!
//! Pins the spec's algorithm (`docs/Txv3/03_STEP_MODEL_v2.md` §5) against
//! the four `StepOutcome` variants and the `DriveMode::classify` matrix.
//!
//! Tests use a minimal `MockStepOp` that returns outcomes from a scriptable
//! queue so each test exercises only `drive`'s contract, not any real shim
//! operation.
//!
//! `drive` is `async fn` but never parks in this PR (reactor wait wiring
//! is a future PR). All futures resolve in the first poll; `block_on` with
//! a noop waker is sufficient.
//!
//! All substrate types are imported through `tx_scripts::adapter::step_engine`
//! so this file stays inside the 0/0 boundary ratchet.
//!
//! txdoc cross-refs:
//! - txdoc:STEP-V2-DRIVER-1  (drive loop algorithm)
//! - txdoc:STEP-V2-DRIVER-MODE-1 (DriveMode classify matrix)

use std::future::Future;
use std::sync::{Arc, Mutex};
use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};
use tx_scripts::adapter::step_engine::{
    AgentCancelPolicy, Cap, Deadline, DelegateEndpoint, DelegateRequest, DelegateToken, DriveMode,
    Errno, InterestMask, NoProgress, ProcessIdentity, ScriptCtx, StepOp, StepOutcome,
    SubjectAuthority, SubjectContext, SubjectIdentity, TimerId, WaitSourceId, YieldShape,
};
use tx_scripts::adapter::wake::{
    register_source, unregister_source, MailboxEvent, SignalRouting, TaskMailbox, TimerToken,
    WaitGeneration, WaitSource,
};
use tx_substrate::bus::RawQueue;
use tx_subsystems::cred::placeholder_restrictions_cap;
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity as RealProcessIdentity};
use tx_subsystems::signal::{
    step_sigaction_entry, SaFlags, SigActionEntry, SigDisposition, SignalMask, Signum,
};
use tx_subsystems::thread_runtime::execution::{
    post_signal_with_post, step_sigprocmask, SigmaskHow,
};
use tx_subsystems::thread_runtime::ThreadIdentity as RealThreadIdentity;
use tx_subsystems::wait_source::{register_wait_queue, release_wait_source};
use tx_time::{
    DeadlineDomain, DeadlineNs, DeadlineRegistrarHandle, TimeError, TimerRole, TimerTarget,
};

struct RecordingDeadlineDomain {
    next_token: std::sync::atomic::AtomicU64,
    roles: Mutex<Vec<TimerRole>>,
}

impl RecordingDeadlineDomain {
    fn new() -> Self {
        Self {
            next_token: std::sync::atomic::AtomicU64::new(1),
            roles: Mutex::new(Vec::new()),
        }
    }

    fn roles(&self) -> Vec<TimerRole> {
        self.roles.lock().unwrap().clone()
    }
}

impl DeadlineDomain for RecordingDeadlineDomain {
    fn register_deadline(
        &self,
        _deadline_ns: DeadlineNs,
        role: TimerRole,
        _target: TimerTarget,
    ) -> Result<TimerToken, TimeError> {
        self.roles.lock().unwrap().push(role);
        Ok(TimerToken::new(
            self.next_token
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ))
    }

    fn cancel_deadline(&self, _token: TimerToken) -> bool {
        true
    }
}

struct FailingDeadlineDomain;

impl DeadlineDomain for FailingDeadlineDomain {
    fn register_deadline(
        &self,
        _deadline_ns: DeadlineNs,
        _role: TimerRole,
        _target: TimerTarget,
    ) -> Result<TimerToken, TimeError> {
        Err(TimeError::Hardware)
    }

    fn cancel_deadline(&self, _token: TimerToken) -> bool {
        false
    }
}
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

static DRIVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn deliver_signal_with_direct_post_for_test(
    thread: &Cap<RealThreadIdentity>,
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

struct StubPmap;

static NEXT_ROOT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

impl PmapIf for StubPmap {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let id = NEXT_ROOT_ID.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a future to completion using a noop waker. Since `drive` never parks
/// in this PR, the future resolves in the first poll. If it doesn't resolve
/// within 64 polls, the test panics.
fn block_on<F: core::future::Future>(future: F) -> F::Output {
    use core::task::{Context, Poll, Waker};
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = Box::pin(future);
    for _ in 0..64 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => continue,
        }
    }
    panic!("drive test block_on: future did not resolve");
}

/// A mock `StepOp` that returns outcomes from a fixed queue (front to back).
/// When the queue is exhausted, subsequent `step()` calls panic.
///
/// `Output = u32`, `Progress = NoProgress` keeps the generics minimal.
struct MockStepOp {
    queue: std::collections::VecDeque<StepOutcome<u32, NoProgress>>,
}

impl MockStepOp {
    fn new(outcomes: impl IntoIterator<Item = StepOutcome<u32, NoProgress>>) -> Self {
        Self {
            queue: outcomes.into_iter().collect(),
        }
    }
}

impl StepOp<ProcessIdentity> for MockStepOp {
    type Output = u32;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<ProcessIdentity>) -> StepOutcome<u32, NoProgress> {
        self.queue
            .pop_front()
            .expect("MockStepOp: queue exhausted unexpectedly")
    }
}

struct RetryThenDoneOp {
    source: WaitSourceId,
    interests: InterestMask,
    yielded: bool,
}

impl RetryThenDoneOp {
    fn new(source: WaitSourceId, interests: InterestMask) -> Self {
        Self {
            source,
            interests,
            yielded: false,
        }
    }
}

impl<I: SubjectIdentity> StepOp<I> for RetryThenDoneOp {
    type Output = u32;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<u32, NoProgress> {
        if self.yielded {
            StepOutcome::Done(77)
        } else {
            self.yielded = true;
            StepOutcome::Yield {
                progress: NoProgress,
                shape: YieldShape::OnWaitSource {
                    source: self.source,
                    interests: self.interests,
                },
            }
        }
    }
}

fn empty_ctx<I: SubjectIdentity>() -> ScriptCtx<I> {
    ScriptCtx::new()
}

fn fresh_aspace() -> Cap<AddressSpace> {
    AddressSpace::new_cap_for_platform::<StubPmap>().expect("fresh aspace")
}

fn bootstrap_subject() -> (
    Cap<RealProcessIdentity>,
    Cap<RealThreadIdentity>,
    Arc<TaskMailbox>,
    ScriptCtx<RealProcessIdentity>,
) {
    tx_test_support::init_host();
    reset_init_process();
    reset_pid_counter();
    reset_tid_counter();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();

    let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap init");
    let thread = proc_cap.nth_thread(0).expect("leader thread");
    let mailbox = Arc::new(TaskMailbox::new());
    thread
        .payload_cap()
        .expect("live thread")
        .bind_mailbox(Arc::downgrade(&mailbox));
    let authority = SubjectAuthority::new(
        proc_cap.cred_cap().expect("live cred"),
        placeholder_restrictions_cap().expect("restrictions cap"),
    );
    let subject = SubjectContext::from_thread(proc_cap.clone(), thread.clone(), authority);
    let ctx = ScriptCtx::new().with_subject(subject);
    (proc_cap, thread, mailbox, ctx)
}

fn on_wait_source_shape(source_id: u64, interests: u64) -> YieldShape {
    YieldShape::OnWaitSource {
        source: WaitSourceId::new(source_id),
        interests: InterestMask::new(interests),
    }
}

fn on_agent_shape() -> YieldShape {
    YieldShape::OnAgent {
        endpoint: DelegateEndpoint::placeholder(),
        request: DelegateRequest::Placeholder,
        token: DelegateToken::placeholder(),
        deadline: Deadline::NEVER,
        cancel: AgentCancelPolicy::BestEffort,
    }
}

// ---------------------------------------------------------------------------
// 1. drive over a StepOp that immediately returns Done — returns the value
// ---------------------------------------------------------------------------

#[test]
fn drive_done_immediately_returns_value() {
    let op = MockStepOp::new([StepOutcome::Done(42u32)]);
    let mut ctx = empty_ctx();
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        None,
        None,
        None,
    ));
    assert_eq!(result, Ok(42u32));
}

// ---------------------------------------------------------------------------
// 2. drive over a StepOp that returns Err — surfaces the error
// ---------------------------------------------------------------------------

#[test]
fn drive_err_surfaces_error() {
    let op = MockStepOp::new([StepOutcome::Err(Errno::ENOENT)]);
    let mut ctx = empty_ctx();
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        None,
        None,
        None,
    ));
    assert_eq!(result, Err(Errno::ENOENT));
}

#[test]
fn drive_err_surfaces_einval() {
    let op = MockStepOp::new([StepOutcome::Err(Errno::EINVAL)]);
    let mut ctx = empty_ctx();
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Nonblocking,
        None,
        None,
        None,
    ));
    assert_eq!(result, Err(Errno::EINVAL));
}

// ---------------------------------------------------------------------------
// 3. drive over a StepOp that yields on a wait source with Nonblocking mode
//    — mode classifies as Translate(Eagain), drive returns EAGAIN
//    (no progress → EAGAIN; progress → also EAGAIN in this PR since
//     PartialReturn synthesis is not yet implemented)
// ---------------------------------------------------------------------------

#[test]
fn drive_yield_on_wait_source_nonblocking_no_progress_returns_eagain() {
    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: on_wait_source_shape(1, 0b1),
    }]);
    let mut ctx = empty_ctx();
    // Nonblocking + no progress → Translate(Eagain) → Err(EAGAIN)
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Nonblocking,
        None,
        None,
        None,
    ));
    assert_eq!(result, Err(Errno::EAGAIN));
}

// ---------------------------------------------------------------------------
// 4. drive over a StepOp that yields on an agent with Nonblocking mode
//    — mode classifies as Translate(Eagain), drive returns EAGAIN
// ---------------------------------------------------------------------------

#[test]
fn drive_yield_on_agent_nonblocking_returns_eagain() {
    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: on_agent_shape(),
    }]);
    let mut ctx = empty_ctx();
    // Nonblocking + no progress → Translate(Eagain) → Err(EAGAIN)
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Nonblocking,
        None,
        None,
        None,
    ));
    assert_eq!(result, Err(Errno::EAGAIN));
}

// ---------------------------------------------------------------------------
// 5. drive over a StepOp that returns Continue then Done
//    — loop retries and eventually returns the done value
// ---------------------------------------------------------------------------

#[test]
fn drive_continue_then_done_retries_loop() {
    let op = MockStepOp::new([
        StepOutcome::Continue {
            progress: NoProgress,
        },
        StepOutcome::Continue {
            progress: NoProgress,
        },
        StepOutcome::Done(99u32),
    ]);
    let mut ctx = empty_ctx();
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        None,
        None,
        None,
    ));
    assert_eq!(result, Ok(99u32));
}

#[test]
fn wait_source_register_notify_delivers_to_mailbox() {
    let ws = Arc::new(WaitSource::new(WaitSourceId::new(99)));
    register_source(Arc::clone(&ws));

    let mb = Arc::new(TaskMailbox::new());
    let gen = mb.next_generation();
    let interests = InterestMask::new(0b1);

    // Register the task's mailbox with the WaitSource.
    let sub_id = ws.register(Arc::downgrade(&mb), gen, interests);
    assert_eq!(ws.subscriber_count(), 1);

    // Notify from the object side — should deliver to the mailbox.
    let posted = ws.notify(interests);
    assert_eq!(posted, 1);

    // Mailbox should have the event.
    let evt = mb.poll().expect("event should be delivered");
    match evt {
        MailboxEvent::SourceFired {
            generation,
            source,
            interests: evt_interests,
        } => {
            assert_eq!(generation, gen);
            assert_eq!(source, WaitSourceId::new(99));
            assert_eq!(evt_interests, interests);
        }
        other => panic!("expected SourceFired, got {other:?}"),
    }

    // Cleanup.
    ws.unregister(sub_id);
    assert_eq!(ws.subscriber_count(), 0);
    unregister_source(WaitSourceId::new(99));
}

// ---------------------------------------------------------------------------
// Bonus: UnsupportedShape via Selecting + OnAgent → ENOSYS
// ---------------------------------------------------------------------------

#[test]
fn drive_selecting_on_agent_returns_enosys() {
    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: on_agent_shape(),
    }]);
    let mut ctx = empty_ctx();
    // Selecting + OnAgent → Translate(UnsupportedShape) → Err(ENOSYS)
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Selecting,
        None,
        None,
        None,
    ));
    assert_eq!(result, Err(Errno::ENOSYS));
}

// ---------------------------------------------------------------------------
// Bonus: Waiting + OnWaitSource → Resolve → parks, wakes, then retries
// ---------------------------------------------------------------------------

#[test]
fn drive_waiting_on_wait_source_wake_retries() {
    struct CountWake(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }

    let mailbox = Arc::new(TaskMailbox::new());
    let source_id = WaitSourceId::new(7);
    let interests = InterestMask::new(0xff);
    let ws = Arc::new(WaitSource::new(source_id));
    register_source(Arc::clone(&ws));

    let op = MockStepOp::new([
        StepOutcome::Yield {
            progress: NoProgress,
            shape: YieldShape::OnWaitSource {
                source: source_id,
                interests,
            },
        },
        StepOutcome::Done(42),
    ]);

    let mut ctx = ScriptCtx::new().with_mailbox(Arc::clone(&mailbox));
    let fut = tx_scripts::drive(op, &mut ctx, DriveMode::Waiting, Some(&mailbox), None, None);
    let wake_count = Arc::new(CountWake(std::sync::atomic::AtomicUsize::new(0)));
    let waker = std::task::Waker::from(Arc::clone(&wake_count));
    let mut task_ctx = std::task::Context::from_waker(&waker);
    let mut pinned = Box::pin(fut);
    assert!(
        matches!(
            pinned.as_mut().poll(&mut task_ctx),
            std::task::Poll::Pending
        ),
        "first poll should park on the registered wait source"
    );

    assert_eq!(ws.notify(interests), 1);
    assert_eq!(
        wake_count.0.load(std::sync::atomic::Ordering::Acquire),
        1,
        "the wait-source event wakes the parked task once"
    );
    assert_eq!(
        pinned.as_mut().poll(&mut task_ctx),
        std::task::Poll::Ready(Ok(42))
    );
    assert!(
        mailbox.has_waker(),
        "a completed nested wait must retain the task-level mailbox route"
    );
    assert!(mailbox.post(MailboxEvent::SignalDelivered {
        signum: 15,
        routing: SignalRouting::ProcessDirected,
    }));
    assert_eq!(
        wake_count.0.load(std::sync::atomic::Ordering::Acquire),
        2,
        "a lifecycle event posted after wait completion still wakes the task"
    );
    unregister_source(source_id);
}

#[test]
fn drive_wait_source_reactor_handoff_parks_and_completes_without_poll_loop() {
    let source_id = WaitSourceId::new(0xd12e);
    let interests = InterestMask::new(0b1);
    let source = Arc::new(WaitSource::new(source_id));
    register_source(Arc::clone(&source));

    let reactor = tx_reactor::Reactor::new();
    let task = reactor.submit(async move {
        let mailbox = tx_reactor::current_task_mailbox(0)
            .expect("reactor publishes the current task mailbox while polling");
        let op = MockStepOp::new([
            StepOutcome::Yield {
                progress: NoProgress,
                shape: YieldShape::OnWaitSource {
                    source: source_id,
                    interests,
                },
            },
            StepOutcome::Done(42),
        ]);
        let mut ctx = ScriptCtx::new().with_mailbox(Arc::clone(&mailbox));
        assert_eq!(
            tx_scripts::drive(op, &mut ctx, DriveMode::Waiting, Some(&mailbox), None, None,).await,
            Ok(42)
        );
    });

    let parked = reactor.run_until_idle();
    assert_eq!(parked.polled, 1);
    assert_eq!(parked.completed, 0);
    assert_eq!(
        reactor.task_status(task),
        Some(tx_reactor::TaskStatus::Parked)
    );

    assert_eq!(source.notify(interests), 1);
    let completed = reactor.run_until_idle();
    assert_eq!(completed.polled, 1, "one source event needs one re-poll");
    assert_eq!(completed.completed, 1);
    assert_eq!(
        reactor.task_status(task),
        Some(tx_reactor::TaskStatus::Completed)
    );
    assert_eq!(
        reactor.run_until_idle().polled,
        0,
        "a consumed event must not leave a self-wake loop"
    );

    unregister_source(source_id);
}

#[test]
fn drive_waiting_on_registered_raw_queue_wake_retries() {
    let mailbox = Arc::new(TaskMailbox::new());
    let queue = RawQueue::new();
    let source_id = register_wait_queue(queue.clone());
    let op = MockStepOp::new([
        StepOutcome::Yield {
            progress: NoProgress,
            shape: on_wait_source_shape(source_id, 0b1),
        },
        StepOutcome::Done(42),
    ]);
    let mut ctx = ScriptCtx::new().with_mailbox(Arc::clone(&mailbox));
    let future = tx_scripts::drive(op, &mut ctx, DriveMode::Waiting, Some(&mailbox), None, None);
    let waker = std::task::Waker::noop().clone();
    let mut task_ctx = std::task::Context::from_waker(&waker);
    let mut future = Box::pin(future);

    assert!(matches!(
        future.as_mut().poll(&mut task_ctx),
        std::task::Poll::Pending
    ));
    assert_eq!(queue.subscriber_count(), 1);
    assert_eq!(queue.fire(0b1), 1);
    assert_eq!(
        future.as_mut().poll(&mut task_ctx),
        std::task::Poll::Ready(Ok(42))
    );

    release_wait_source(source_id);
}

#[test]
fn registered_raw_queue_wait_preserves_unrelated_mailbox_events() {
    let mailbox = Arc::new(TaskMailbox::new());
    let queue = RawQueue::new();
    let source_id = register_wait_queue(queue.clone());
    assert!(mailbox.post(MailboxEvent::SourceFired {
        generation: WaitGeneration::new(99),
        source: WaitSourceId::new(0xfeed),
        interests: InterestMask::new(0b1),
    }));
    let op = MockStepOp::new([
        StepOutcome::Yield {
            progress: NoProgress,
            shape: on_wait_source_shape(source_id, 0b1),
        },
        StepOutcome::Done(42),
    ]);
    let mut ctx = ScriptCtx::new().with_mailbox(Arc::clone(&mailbox));
    let future = tx_scripts::drive(op, &mut ctx, DriveMode::Waiting, Some(&mailbox), None, None);
    let waker = std::task::Waker::noop().clone();
    let mut task_ctx = std::task::Context::from_waker(&waker);
    let mut future = Box::pin(future);

    assert!(matches!(
        future.as_mut().poll(&mut task_ctx),
        std::task::Poll::Pending
    ));
    assert_eq!(queue.fire(0b1), 1);
    assert_eq!(
        future.as_mut().poll(&mut task_ctx),
        std::task::Poll::Ready(Ok(42))
    );
    assert!(matches!(
        mailbox.poll(),
        Some(MailboxEvent::SourceFired {
            source,
            generation,
            ..
        }) if source == WaitSourceId::new(0xfeed) && generation == WaitGeneration::new(99)
    ));

    release_wait_source(source_id);
}

#[test]
fn drive_waiting_on_ready_registered_raw_queue_retries_without_parking() {
    let mailbox = Arc::new(TaskMailbox::new());
    let queue = RawQueue::new();
    let source_id = register_wait_queue(queue.clone());
    queue.fire(0b1);
    let op = MockStepOp::new([
        StepOutcome::Yield {
            progress: NoProgress,
            shape: on_wait_source_shape(source_id, 0b1),
        },
        StepOutcome::Done(42),
    ]);
    let mut ctx = ScriptCtx::new().with_mailbox(Arc::clone(&mailbox));

    assert_eq!(
        block_on(tx_scripts::drive(
            op,
            &mut ctx,
            DriveMode::Waiting,
            Some(&mailbox),
            None,
            None,
        )),
        Ok(42)
    );
    assert_eq!(queue.subscriber_count(), 0);

    release_wait_source(source_id);
}

#[test]
fn dropping_parked_registered_raw_queue_wait_unregisters_subscription() {
    let mailbox = Arc::new(TaskMailbox::new());
    let queue = RawQueue::new();
    let source_id = register_wait_queue(queue.clone());
    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: on_wait_source_shape(source_id, 0b1),
    }]);
    let mut ctx = ScriptCtx::new().with_mailbox(Arc::clone(&mailbox));
    let future = tx_scripts::drive(op, &mut ctx, DriveMode::Waiting, Some(&mailbox), None, None);
    let waker = std::task::Waker::noop().clone();
    let mut task_ctx = std::task::Context::from_waker(&waker);
    let mut future = Box::pin(future);

    assert!(matches!(
        future.as_mut().poll(&mut task_ctx),
        std::task::Poll::Pending
    ));
    assert_eq!(queue.subscriber_count(), 1);
    drop(future);
    assert_eq!(queue.subscriber_count(), 0);

    release_wait_source(source_id);
}

// ---------------------------------------------------------------------------
// drive-taskmb: basic TaskMailbox round-trip
// ---------------------------------------------------------------------------

#[test]
fn mailbox_post_poll_roundtrip() {
    let mb = TaskMailbox::new();
    let gen = mb.next_generation();
    // TaskMailbox counter starts at 1, so first next_generation() is 1.
    assert_eq!(gen.raw(), 1);
    let evt = MailboxEvent::SourceFired {
        generation: gen,
        source: WaitSourceId::new(1),
        interests: InterestMask::new(0b1),
    };
    assert!(mb.post(evt));
    assert_eq!(mb.len(), 1);
    let polled = mb.poll();
    assert!(polled.is_some());
    let polled = polled.unwrap();
    assert!(matches!(polled, MailboxEvent::SourceFired { .. }));
    assert_eq!(mb.len(), 0);
}

#[test]
fn mailbox_active_wait_matches() {
    let mb = TaskMailbox::new();
    let source = WaitSourceId::new(7);
    let interests = InterestMask::new(0b101);
    let gen = mb.next_generation();
    let active = tx_substrate::wake::mailbox::ActiveWait::new(gen, source, interests);
    let evt = MailboxEvent::SourceFired {
        generation: gen,
        source,
        interests: InterestMask::new(0b001),
    };
    assert!(active.matches(&evt), "overlapping interests should match");
    let evt2 = MailboxEvent::SourceFired {
        generation: gen,
        source: WaitSourceId::new(99),
        interests,
    };
    assert!(!active.matches(&evt2), "different source should not match");
}

// ---------------------------------------------------------------------------
// drive-taskmb: yield OnWaitSource with real TaskMailbox resolves on pre-posted event
// ---------------------------------------------------------------------------

#[test]
fn drive_yield_on_wait_source_with_mailbox_resolves_on_pre_posted_event() {
    let mailbox = Arc::new(TaskMailbox::new());
    let source_id = WaitSourceId::new(42);
    let interests = InterestMask::new(0b1);

    // resolve_on_wait_source calls mailbox.next_generation() internally
    // (counter starts at 1, so first call returns generation 1).
    // Pre-post an event with generation 1 so they match.
    assert!(
        mailbox.post(MailboxEvent::SourceFired {
            generation: WaitGeneration::new(1),
            source: source_id,
            interests,
        }),
        "mailbox should accept event"
    );

    // StepOp: yield OnWaitSource, then Done.
    let op = MockStepOp::new([
        StepOutcome::Yield {
            progress: NoProgress,
            shape: YieldShape::OnWaitSource {
                source: source_id,
                interests,
            },
        },
        StepOutcome::Done(99u32),
    ]);

    let mut ctx = ScriptCtx::new().with_mailbox(Arc::clone(&mailbox));
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        Some(&mailbox),
        None,
        None,
    ));
    assert_eq!(result, Ok(99u32));
}

#[test]
fn drive_yield_on_wait_source_with_deadline_returns_etimedout() {
    let mailbox = Arc::new(TaskMailbox::new());
    let domain = Arc::new(RecordingDeadlineDomain::new());
    let source_id = WaitSourceId::new(45);
    let interests = InterestMask::new(0b1);

    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: YieldShape::OnWaitSource {
            source: source_id,
            interests,
        },
    }]);

    let timer_registrar = DeadlineRegistrarHandle::from_domain(domain.clone());
    let mut ctx = ScriptCtx::new()
        .with_mailbox(Arc::clone(&mailbox))
        .with_deadline(Deadline::from_raw(10));
    let fut = tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        Some(&mailbox),
        None,
        Some(&timer_registrar),
    );
    let waker = std::task::Waker::noop().clone();
    let mut task_ctx = std::task::Context::from_waker(&waker);
    let mut pinned = Box::pin(fut);
    assert!(
        matches!(
            pinned.as_mut().poll(&mut task_ctx),
            std::task::Poll::Pending
        ),
        "first poll should park on the wait source"
    );
    assert_eq!(domain.roles(), vec![TimerRole::DeadlineAbort]);
    let _ = mailbox.post(MailboxEvent::TimerFired {
        token: TimerToken::new(1),
    });
    assert_eq!(
        pinned.as_mut().poll(&mut task_ctx),
        std::task::Poll::Ready(Err(Errno::ETIMEDOUT))
    );
}

#[test]
fn drive_finite_wait_aborts_when_deadline_registration_fails() {
    let mailbox = Arc::new(TaskMailbox::new());
    let source_id = WaitSourceId::new(46);
    let interests = InterestMask::new(0b1);
    let registrar = DeadlineRegistrarHandle::from_domain(Arc::new(FailingDeadlineDomain));
    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: YieldShape::OnWaitSource {
            source: source_id,
            interests,
        },
    }]);
    let mut ctx = ScriptCtx::new()
        .with_mailbox(Arc::clone(&mailbox))
        .with_deadline(Deadline::from_raw(10));
    let mut future = Box::pin(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        Some(&mailbox),
        None,
        Some(&registrar),
    ));
    let waker = std::task::Waker::noop().clone();
    let mut task_ctx = std::task::Context::from_waker(&waker);
    assert!(matches!(
        future.as_mut().poll(&mut task_ctx),
        std::task::Poll::Ready(Err(Errno::ETIMEDOUT))
    ));
}

#[test]
fn drive_timer_aborts_when_deadline_registration_fails() {
    let mailbox = Arc::new(TaskMailbox::new());
    let registrar = DeadlineRegistrarHandle::from_domain(Arc::new(FailingDeadlineDomain));
    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: YieldShape::OnTimer {
            token: TimerId::new(1),
            deadline: Deadline::from_raw(10),
        },
    }]);
    let mut ctx = ScriptCtx::new().with_mailbox(Arc::clone(&mailbox));
    let mut future = Box::pin(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        Some(&mailbox),
        None,
        Some(&registrar),
    ));
    let waker = std::task::Waker::noop().clone();
    let mut task_ctx = std::task::Context::from_waker(&waker);
    assert!(matches!(
        future.as_mut().poll(&mut task_ctx),
        std::task::Poll::Ready(Err(Errno::ETIMEDOUT))
    ));
}

#[test]
fn dropping_parked_wait_unregisters_wait_source_subscription() {
    let mailbox = Arc::new(TaskMailbox::new());
    let source = Arc::new(WaitSource::new(WaitSourceId::new(47)));
    register_source(Arc::clone(&source));
    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: YieldShape::OnWaitSource {
            source: source.id(),
            interests: InterestMask::new(0b1),
        },
    }]);
    let mut ctx = ScriptCtx::new().with_mailbox(Arc::clone(&mailbox));
    let future = tx_scripts::drive(op, &mut ctx, DriveMode::Waiting, Some(&mailbox), None, None);
    let waker = std::task::Waker::noop().clone();
    let mut task_ctx = std::task::Context::from_waker(&waker);
    let mut future = Box::pin(future);
    assert!(matches!(
        future.as_mut().poll(&mut task_ctx),
        std::task::Poll::Pending
    ));
    assert_eq!(source.subscriber_count(), 1);
    drop(future);
    assert_eq!(source.subscriber_count(), 0);
    unregister_source(source.id());
}

#[test]
fn drive_signal_delivered_interrupt_consumes_mailbox_hint() {
    let mailbox = Arc::new(TaskMailbox::new());
    let source_id = WaitSourceId::new(43);
    let interests = InterestMask::new(0b1);

    assert!(
        mailbox.post(MailboxEvent::SignalDelivered {
            signum: 32,
            routing: SignalRouting::ThreadDirected { tid: 7 },
        }),
        "mailbox should accept signal wake hint"
    );

    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: YieldShape::OnWaitSource {
            source: source_id,
            interests,
        },
    }]);

    let mut ctx = ScriptCtx::new().with_mailbox(Arc::clone(&mailbox));
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        Some(&mailbox),
        None,
        None,
    ));
    assert_eq!(result, Err(Errno::EINTR));
    assert!(
        mailbox.is_empty(),
        "SignalDelivered is a wake hint; drive must not re-post it into the next wait"
    );
}

#[test]
fn drive_masked_signal_hint_retries_instead_of_eintr() {
    let _guard = DRIVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_proc, thread, mailbox, mut ctx) = bootstrap_subject();
    let source_id = WaitSourceId::new(44);
    let interests = InterestMask::new(0b1);

    let mut block = SignalMask::EMPTY;
    block.block(Signum::SIGTERM);
    let _ = step_sigprocmask(&thread, SigmaskHow::SetMask, block);

    let op = RetryThenDoneOp::new(source_id, interests);
    deliver_signal_with_direct_post_for_test(
        &thread,
        Signum::SIGTERM,
        SignalRouting::ThreadDirected {
            tid: thread.tid.0 as u64,
        },
        None,
    );
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        Some(&mailbox),
        None,
        None,
    ));
    assert_eq!(
        result,
        Ok(77),
        "masked signal wake hints must only re-poll the wait predicate"
    );
    assert!(
        mailbox.is_empty(),
        "masked SignalDelivered hints are consumed after forcing a re-poll"
    );
}

#[test]
fn drive_sa_restart_signal_hint_retries_instead_of_eintr() {
    let _guard = DRIVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (proc, thread, mailbox, mut ctx) = bootstrap_subject();
    let source_id = WaitSourceId::new(46);
    let interests = InterestMask::new(0b1);

    let action = SigActionEntry {
        disposition: SigDisposition::Handler(0xCAFE),
        flags: SaFlags::RESTART,
        sa_mask: SignalMask::EMPTY,
        restorer: 0,
    };
    let _ = step_sigaction_entry(&proc, Signum::SIGTERM, action);

    let op = RetryThenDoneOp::new(source_id, interests);
    deliver_signal_with_direct_post_for_test(
        &thread,
        Signum::SIGTERM,
        SignalRouting::ThreadDirected {
            tid: thread.tid.0 as u64,
        },
        None,
    );
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        Some(&mailbox),
        None,
        None,
    ));
    assert_eq!(
        result,
        Ok(77),
        "SA_RESTART signal wake hints must re-poll instead of surfacing EINTR"
    );
    assert!(
        mailbox.is_empty(),
        "SA_RESTART SignalDelivered hint is consumed after forcing a re-poll"
    );
}

#[test]
fn drive_libc_sigcancel_hint_interrupts_even_with_sa_restart() {
    let _guard = DRIVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (proc, thread, mailbox, mut ctx) = bootstrap_subject();
    let source_id = WaitSourceId::new(47);
    let interests = InterestMask::new(0b1);

    let action = SigActionEntry {
        disposition: SigDisposition::Handler(0xCA11CE1),
        flags: SaFlags::RESTART,
        sa_mask: SignalMask::EMPTY,
        restorer: 0,
    };
    let _ = step_sigaction_entry(&proc, Signum::GLIBC_SIGCANCEL, action);

    let op = RetryThenDoneOp::new(source_id, interests);
    deliver_signal_with_direct_post_for_test(
        &thread,
        Signum::GLIBC_SIGCANCEL,
        SignalRouting::ThreadDirected {
            tid: thread.tid.0 as u64,
        },
        None,
    );
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        Some(&mailbox),
        None,
        None,
    ));
    assert_eq!(
        result,
        Err(Errno::EINTR),
        "libc cancellation signals must interrupt cancelable waits despite SA_RESTART"
    );
    assert!(
        mailbox.is_empty(),
        "SIGCANCEL SignalDelivered hint is consumed after interrupting the wait"
    );
}
