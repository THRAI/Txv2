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
use std::sync::Arc;
use tx_hal::{
    Asid, PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PtNode, VirtAddr,
};
use tx_scripts::adapter::step_engine::{
    AgentCancelPolicy, Cap, Deadline, DelegateEndpoint, DelegateRequest, DelegateToken, DriveMode,
    Errno, InterestMask, NoProgress, ProcessIdentity, ScriptCtx, StepOp, StepOutcome,
    SubjectIdentity, WaitSourceId, YieldShape,
};
use tx_substrate::step::{SubjectAuthority, SubjectContext};
use tx_substrate::wake::mailbox::{MailboxEvent, SignalRouting, TaskMailbox, WaitGeneration};
use tx_substrate::wake::timer::TimerWheel;
use tx_substrate::wake::wait_source::{register_source, unregister_source, WaitSource};
use tx_subsystems::cred::placeholder_restrictions_cap;
use tx_subsystems::cross_crate_test_support::{
    reset_init_process, reset_pid_counter, reset_tid_counter,
};
use tx_subsystems::process::{bootstrap_init_process, ProcessIdentity as RealProcessIdentity};
use tx_subsystems::signal::{SignalMask, Signum};
use tx_subsystems::thread_runtime::execution::{post_signal, step_sigprocmask, SigmaskHow};
use tx_subsystems::thread_runtime::ThreadIdentity as RealThreadIdentity;
use tx_subsystems::vm::AddressSpace;
use tx_subsystems::zones;

static DRIVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
// Bonus: Waiting + OnWaitSource → Resolve → parks then retries
// ---------------------------------------------------------------------------

#[test]
fn drive_waiting_on_wait_source_unregistered_token_retries() {
    // When the wait source id is not registered (test placeholder),
    // wait_on_token returns None and drive retries immediately.
    // The op yields, drive skips the await, applies ResumeOutcome::Retry,
    // loops, and step() returns Done.
    let op = MockStepOp::new([
        StepOutcome::Yield {
            progress: NoProgress,
            shape: on_wait_source_shape(7, 0xff),
        },
        StepOutcome::Done(42),
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
    assert_eq!(result, Ok(42));
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
    let wheel = TimerWheel::new();
    let source_id = WaitSourceId::new(45);
    let interests = InterestMask::new(0b1);

    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: YieldShape::OnWaitSource {
            source: source_id,
            interests,
        },
    }]);

    let mut ctx = ScriptCtx::new()
        .with_mailbox(Arc::clone(&mailbox))
        .with_timer_wheel(wheel.clone())
        .with_deadline(Deadline::from_raw(10));
    let fut = tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        Some(&mailbox),
        None,
        Some(&wheel),
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
    assert_eq!(wheel.fire_due(10), 1);
    assert_eq!(
        pinned.as_mut().poll(&mut task_ctx),
        std::task::Poll::Ready(Err(Errno::ETIMEDOUT))
    );
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
    post_signal(
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
