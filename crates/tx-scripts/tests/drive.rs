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

use tx_scripts::adapter::step_engine::{
    AgentCancelPolicy, Deadline, DelegateEndpoint, DelegateRequest, DelegateToken, DriveMode,
    Errno, InterestMask, NoProgress, ProcessIdentity, ScriptCtx, StepOp, StepOutcome, WaitSourceId,
    YieldShape,
};

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

fn empty_ctx() -> ScriptCtx<ProcessIdentity> {
    ScriptCtx::new()
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
    let result = block_on(tx_scripts::drive(op, &mut ctx, DriveMode::Waiting, None, None, None));
    assert_eq!(result, Ok(42u32));
}

// ---------------------------------------------------------------------------
// 2. drive over a StepOp that returns Err — surfaces the error
// ---------------------------------------------------------------------------

#[test]
fn drive_err_surfaces_error() {
    let op = MockStepOp::new([StepOutcome::Err(Errno::ENOENT)]);
    let mut ctx = empty_ctx();
    let result = block_on(tx_scripts::drive(op, &mut ctx, DriveMode::Waiting, None, None, None));
    assert_eq!(result, Err(Errno::ENOENT));
}

#[test]
fn drive_err_surfaces_einval() {
    let op = MockStepOp::new([StepOutcome::Err(Errno::EINVAL)]);
    let mut ctx = empty_ctx();
    let result = block_on(tx_scripts::drive(op, &mut ctx, DriveMode::Nonblocking, None, None, None));
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
    let result = block_on(tx_scripts::drive(op, &mut ctx, DriveMode::Nonblocking, None, None, None));
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
    let result = block_on(tx_scripts::drive(op, &mut ctx, DriveMode::Nonblocking, None, None, None));
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
    let result = block_on(tx_scripts::drive(op, &mut ctx, DriveMode::Waiting, None, None, None));
    assert_eq!(result, Ok(99u32));
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
    let result = block_on(tx_scripts::drive(op, &mut ctx, DriveMode::Selecting, None, None, None));
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
    let result = block_on(tx_scripts::drive(op, &mut ctx, DriveMode::Waiting, None, None, None));
    assert_eq!(result, Ok(42));
}
