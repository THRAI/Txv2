//! Reactor-owned userspace-run shell.
//!
//! This module is the dispatch seam for the first production-runtime shard.
//! It must remain a reactor mechanism: no VM policy, ThreadRuntime payload
//! ownership, signal routing, or architecture-specific trap-frame restore.

use alloc::sync::Arc;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use crate::{
    ast::{AstBatch, AstSlot},
    spin_lock::SpinLock,
    task::TaskLifecycleError,
};

/// Userspace virtual address reported by a userspace trap.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct UserAddr(u64);

impl UserAddr {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Raw syscall request payload captured at the reactor boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyscallRequest {
    pub nr: u64,
    pub args: [u64; 6],
}

impl SyscallRequest {
    pub const fn new(nr: u64, args: [u64; 6]) -> Self {
        Self { nr, args }
    }
}

/// Access class for a userspace page fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageFaultAccess {
    Read,
    Write,
    Execute,
    Unknown,
}

/// Page-fault trap payload captured by the reactor shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageFaultInfo {
    pub addr: UserAddr,
    pub access: PageFaultAccess,
    pub present: bool,
}

/// Fatal hardware trap payload.
///
/// The numbers are intentionally raw at this layer. Architecture-specific
/// decoding belongs below this shell; semantic policy belongs above it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FatalTrapInfo {
    pub cause: u64,
    pub value: u64,
}

impl FatalTrapInfo {
    pub const fn new(cause: u64, value: u64) -> Self {
        Self { cause, value }
    }
}

/// Interesting userspace trap that resolves a userspace-run wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserspaceTrapInfo {
    Syscall(SyscallRequest),
    PageFault(PageFaultInfo),
    TimerPreempt,
    Fatal(FatalTrapInfo),
}

/// Generation-checked identity for an in-flight userspace-run request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct UserspaceRunRequest(u64);

impl UserspaceRunRequest {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Reactor-visible phase of a userspace-run request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserspaceRunPhase {
    Pending,
    Running,
    Preempted,
    Resolved,
}

/// Snapshot of the active userspace-run request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserspaceRunStatus {
    pub request: UserspaceRunRequest,
    pub phase: UserspaceRunPhase,
    pub dispatches: u64,
    pub preemptions: u64,
    pub trap: Option<UserspaceTrapInfo>,
}

/// Rejection reason for userspace-run slot operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserspaceRunError {
    Busy(UserspaceRunStatus),
    NoActiveRequest,
    StaleRequest {
        attempted: UserspaceRunRequest,
        active: UserspaceRunRequest,
    },
    AlreadyResolved(UserspaceRunRequest),
    NotRunning(UserspaceRunRequest),
    RequestIdExhausted,
}

/// Reactor facts handed to the userspace-entry AST policy hook.
///
/// The checkpoint carries only the active userspace-run request and the drained
/// task-local AST markers. Signal selection, handler frames, VM policy, and
/// trap-frame restoration are intentionally outside this type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserspaceEntryCheckpoint {
    pub request: UserspaceRunRequest,
    pub ast: AstBatch,
}

/// Policy-neutral decision returned by a userspace-entry AST hook.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserspaceEntryDecision {
    /// Continue to the platform userspace dispatch path.
    EnterUserspace,
    /// Do not enter userspace yet; let the thread future run again.
    RePollTask,
    /// Resolve the userspace-run wait with a caller-supplied trap outcome.
    Resolve(UserspaceTrapInfo),
}

/// Concrete reactor action taken after a userspace-entry AST checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserspaceEntryAction {
    Entered(UserspaceRunStatus),
    RePollTask(UserspaceRunStatus),
    Resolved(UserspaceRunStatus),
}

/// Result of a userspace-entry AST checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserspaceEntryOutcome {
    pub checkpoint: UserspaceEntryCheckpoint,
    pub action: UserspaceEntryAction,
}

/// Error returned by task-keyed userspace-entry checkpoint helpers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserspaceEntryTaskError {
    Task(TaskLifecycleError),
    Run(UserspaceRunError),
}

impl From<TaskLifecycleError> for UserspaceEntryTaskError {
    fn from(error: TaskLifecycleError) -> Self {
        Self::Task(error)
    }
}

impl From<UserspaceRunError> for UserspaceEntryTaskError {
    fn from(error: UserspaceRunError) -> Self {
        Self::Run(error)
    }
}

/// Reactor-local userspace-run request slot.
#[derive(Clone)]
pub struct UserspaceRunSlot {
    state: Arc<SpinLock<SlotState>>,
}

/// Future returned by [`UserspaceRunSlot::start_request`].
pub struct UserspaceRunWait {
    slot: UserspaceRunSlot,
    request: UserspaceRunRequest,
    finished: bool,
}

struct SlotState {
    next_request: u64,
    active: Option<ActiveRun>,
}

struct ActiveRun {
    request: UserspaceRunRequest,
    phase: ActivePhase,
    dispatches: u64,
    preemptions: u64,
    waker: Option<Waker>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivePhase {
    Pending,
    Running,
    Resolved(UserspaceTrapInfo),
}

impl UserspaceRunSlot {
    pub fn new() -> Self {
        Self {
            state: Arc::new(SpinLock::new(SlotState {
                next_request: 0,
                active: None,
            })),
        }
    }

    /// Start a userspace-run wait in the pending phase.
    pub fn start_request(&self) -> Result<UserspaceRunWait, UserspaceRunError> {
        let mut state = self.state.lock();
        if let Some(active) = &state.active {
            return Err(UserspaceRunError::Busy(active.status()));
        }

        let request = UserspaceRunRequest(state.next_request);
        state.next_request = state
            .next_request
            .checked_add(1)
            .ok_or(UserspaceRunError::RequestIdExhausted)?;
        state.active = Some(ActiveRun {
            request,
            phase: ActivePhase::Pending,
            dispatches: 0,
            preemptions: 0,
            waker: None,
        });

        Ok(UserspaceRunWait {
            slot: self.clone(),
            request,
            finished: false,
        })
    }

    pub fn status(&self) -> Option<UserspaceRunStatus> {
        self.state.lock().active.as_ref().map(ActiveRun::status)
    }

    pub fn is_idle(&self) -> bool {
        self.status().is_none()
    }

    /// Return the status for a specific active request.
    ///
    /// This is the generation check used by reactor task-state adapters before
    /// they consume task-local AST markers.
    pub fn status_for_request(
        &self,
        request: UserspaceRunRequest,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        self.active_status(request)
    }

    /// Record a scheduler dispatch that grants userspace execution.
    ///
    /// Dispatch is reactor-internal and does not wake or resolve the waiting
    /// future.
    pub fn dispatch(
        &self,
        request: UserspaceRunRequest,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        self.with_active(request, |active| {
            if let ActivePhase::Resolved(_) = active.phase {
                return Err(UserspaceRunError::AlreadyResolved(request));
            }

            active.phase = ActivePhase::Running;
            active.dispatches += 1;
            Ok(active.status())
        })
    }

    /// Resolve the wait with a timer-preemption event and wake the task.
    ///
    /// Timer preemption participates in the same request-completion protocol as
    /// syscall and page-fault traps. This keeps ownership of the active request
    /// with the thread future: the trap shell only snapshots context and
    /// publishes an event; the future consumes that event and decides whether to
    /// yield, re-enter, or terminate.
    pub fn record_timer_preemption(
        &self,
        request: UserspaceRunRequest,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        let (status, waker) = {
            let mut state = self.state.lock();
            let active = state
                .active
                .as_mut()
                .ok_or(UserspaceRunError::NoActiveRequest)?;
            if active.request != request {
                return Err(UserspaceRunError::StaleRequest {
                    attempted: request,
                    active: active.request,
                });
            }
            if let ActivePhase::Resolved(_) = active.phase {
                return Err(UserspaceRunError::AlreadyResolved(request));
            }
            if active.phase != ActivePhase::Running {
                return Err(UserspaceRunError::NotRunning(request));
            }

            active.phase = ActivePhase::Resolved(UserspaceTrapInfo::TimerPreempt);
            active.preemptions += 1;
            (active.status(), active.waker.take())
        };

        if let Some(waker) = waker {
            waker.wake();
        }

        Ok(status)
    }

    /// Resolve the wait with an interesting trap and wake the registered task.
    pub fn complete_interesting_trap(
        &self,
        request: UserspaceRunRequest,
        trap: UserspaceTrapInfo,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        let (status, waker) = {
            let mut state = self.state.lock();
            let active = state
                .active
                .as_mut()
                .ok_or(UserspaceRunError::NoActiveRequest)?;
            if active.request != request {
                return Err(UserspaceRunError::StaleRequest {
                    attempted: request,
                    active: active.request,
                });
            }
            if let ActivePhase::Resolved(_) = active.phase {
                if !active.phase.replace_timer_preempt_with(trap) {
                    return Err(UserspaceRunError::AlreadyResolved(request));
                }
                (active.status(), active.waker.take())
            } else {
                active.phase = ActivePhase::Resolved(trap);
                (active.status(), active.waker.take())
            }
        };

        if let Some(waker) = waker {
            waker.wake();
        }

        Ok(status)
    }

    /// Resolve a trap produced by an already-dispatched userspace run.
    ///
    /// A pending request has not crossed the platform userspace-entry boundary,
    /// so accepting a syscall, page fault, or fatal trap for it would attach a
    /// stale hart-local context to the next run. A real trap may still replace
    /// a timer-preemption placeholder recorded for the same running request.
    pub fn complete_running_trap(
        &self,
        request: UserspaceRunRequest,
        trap: UserspaceTrapInfo,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        let (status, waker) = {
            let mut state = self.state.lock();
            let active = state
                .active
                .as_mut()
                .ok_or(UserspaceRunError::NoActiveRequest)?;
            if active.request != request {
                return Err(UserspaceRunError::StaleRequest {
                    attempted: request,
                    active: active.request,
                });
            }
            match active.phase {
                ActivePhase::Pending => return Err(UserspaceRunError::NotRunning(request)),
                ActivePhase::Running => {
                    active.phase = ActivePhase::Resolved(trap);
                    (active.status(), active.waker.take())
                }
                ActivePhase::Resolved(_) => {
                    if !active.phase.replace_timer_preempt_with(trap) {
                        return Err(UserspaceRunError::AlreadyResolved(request));
                    }
                    (active.status(), active.waker.take())
                }
            }
        };

        if let Some(waker) = waker {
            waker.wake();
        }

        Ok(status)
    }

    pub fn cancel(&self, request: UserspaceRunRequest) -> Result<(), UserspaceRunError> {
        let mut state = self.state.lock();
        let active = state
            .active
            .as_ref()
            .ok_or(UserspaceRunError::NoActiveRequest)?;
        if active.request != request {
            return Err(UserspaceRunError::StaleRequest {
                attempted: request,
                active: active.request,
            });
        }

        state.active = None;
        Ok(())
    }

    /// Run the policy-neutral AST checkpoint before userspace entry.
    ///
    /// This helper validates the active userspace-run request, drains the
    /// supplied task-local AST slot exactly once, asks the caller-owned policy
    /// hook for a narrow continuation decision, and then applies only the
    /// reactor-owned part of that decision.
    pub fn checkpoint_userspace_entry(
        &self,
        request: UserspaceRunRequest,
        ast: &mut AstSlot,
        decide: impl FnOnce(&UserspaceEntryCheckpoint) -> UserspaceEntryDecision,
    ) -> Result<UserspaceEntryOutcome, UserspaceRunError> {
        self.active_status(request)?;

        self.checkpoint_userspace_entry_batch(request, ast.consume(), decide)
    }

    /// Run the userspace-entry AST checkpoint with an already drained batch.
    pub fn checkpoint_userspace_entry_batch(
        &self,
        request: UserspaceRunRequest,
        ast: AstBatch,
        decide: impl FnOnce(&UserspaceEntryCheckpoint) -> UserspaceEntryDecision,
    ) -> Result<UserspaceEntryOutcome, UserspaceRunError> {
        self.active_status(request)?;

        let checkpoint = UserspaceEntryCheckpoint { request, ast };
        let action = match decide(&checkpoint) {
            UserspaceEntryDecision::EnterUserspace => {
                UserspaceEntryAction::Entered(self.dispatch(request)?)
            }
            UserspaceEntryDecision::RePollTask => {
                UserspaceEntryAction::RePollTask(self.active_status(request)?)
            }
            UserspaceEntryDecision::Resolve(trap) => {
                UserspaceEntryAction::Resolved(self.complete_interesting_trap(request, trap)?)
            }
        };

        Ok(UserspaceEntryOutcome { checkpoint, action })
    }

    fn with_active<R>(
        &self,
        request: UserspaceRunRequest,
        f: impl FnOnce(&mut ActiveRun) -> Result<R, UserspaceRunError>,
    ) -> Result<R, UserspaceRunError> {
        let mut state = self.state.lock();
        let active = state
            .active
            .as_mut()
            .ok_or(UserspaceRunError::NoActiveRequest)?;
        if active.request != request {
            return Err(UserspaceRunError::StaleRequest {
                attempted: request,
                active: active.request,
            });
        }

        f(active)
    }

    fn active_status(
        &self,
        request: UserspaceRunRequest,
    ) -> Result<UserspaceRunStatus, UserspaceRunError> {
        let state = self.state.lock();
        let active = state
            .active
            .as_ref()
            .ok_or(UserspaceRunError::NoActiveRequest)?;
        if active.request != request {
            return Err(UserspaceRunError::StaleRequest {
                attempted: request,
                active: active.request,
            });
        }
        if let ActivePhase::Resolved(_) = active.phase {
            return Err(UserspaceRunError::AlreadyResolved(request));
        }

        Ok(active.status())
    }
}

impl Default for UserspaceRunSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl UserspaceRunWait {
    pub const fn request(&self) -> UserspaceRunRequest {
        self.request
    }
}

impl Future for UserspaceRunWait {
    type Output = UserspaceTrapInfo;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Pending;
        }

        let mut state = this.slot.state.lock();
        let Some(active) = state.active.as_mut() else {
            return Poll::Pending;
        };
        if active.request != this.request {
            return Poll::Pending;
        }

        match active.phase {
            ActivePhase::Resolved(trap) => {
                state.active = None;
                this.finished = true;
                Poll::Ready(trap)
            }
            ActivePhase::Pending | ActivePhase::Running => {
                if active
                    .waker
                    .as_ref()
                    .is_none_or(|waker| !waker.will_wake(cx.waker()))
                {
                    active.waker = Some(cx.waker().clone());
                }
                Poll::Pending
            }
        }
    }
}

impl Drop for UserspaceRunWait {
    fn drop(&mut self) {
        if self.finished {
            return;
        }

        let mut state = self.slot.state.lock();
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.request == self.request)
        {
            state.active = None;
        }
    }
}

impl ActiveRun {
    fn status(&self) -> UserspaceRunStatus {
        UserspaceRunStatus {
            request: self.request,
            phase: self.phase.public_phase(),
            dispatches: self.dispatches,
            preemptions: self.preemptions,
            trap: self.phase.trap(),
        }
    }
}

impl ActivePhase {
    fn replace_timer_preempt_with(&mut self, trap: UserspaceTrapInfo) -> bool {
        match (*self, trap) {
            (Self::Resolved(UserspaceTrapInfo::TimerPreempt), UserspaceTrapInfo::TimerPreempt) => {
                false
            }
            (Self::Resolved(UserspaceTrapInfo::TimerPreempt), trap) => {
                *self = Self::Resolved(trap);
                true
            }
            _ => false,
        }
    }

    const fn public_phase(self) -> UserspaceRunPhase {
        match self {
            Self::Pending => UserspaceRunPhase::Pending,
            Self::Running => UserspaceRunPhase::Running,
            Self::Resolved(_) => UserspaceRunPhase::Resolved,
        }
    }

    const fn trap(self) -> Option<UserspaceTrapInfo> {
        match self {
            Self::Resolved(trap) => Some(trap),
            Self::Pending | Self::Running => None,
        }
    }
}
