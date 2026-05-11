# Reactor Wait/Wake Refactor — Runtime Landing Spec v4

**Status:** draft
**Target:** land the runtime side of `CONCEPTS_v5`
**Scope:** reactor task mailbox, object-owned wait sources, wake hints, wait generation, conditional registration, driver-local active waits, timer/deadline substrate, and minimum `OnAgent` runtime support.

------

## 1. Purpose

`CONCEPTS_v5` defines the semantic algebra:

```text
SubjectContext
StepOp
StepOutcome
StepProgress
YieldShape
OnBehalfOf
OnAgent
```

This spec makes that algebra executable in the reactor/runtime layer.

Runtime path:

```text
StepOp returns YieldShape
driver prepares ActiveWait
WaitSource / DelegateToken / TimerToken records waiter
semantic object / agent / timer publishes WakeHint
TaskMailbox receives WakeHint
reactor wakes task
driver validates generation
driver rechecks truth source
StepOp continues
```

This document does **not** expand the concept layer. It specifies the runtime interfaces required to execute the existing algebra safely.

------

## 2. Placement Rule

The old “port / wait queue / wake path” is factored into two ownership domains:

```text
Object side:
  WaitSource

Task side:
  TaskMailbox
```

The bridge is a yield registration:

```text
WaitRegistrationGuard
AgentTokenGuard
TimerGuard
```

The active suspension itself is not a globally-owned object. It is driver-local state:

```text
ActiveWait
```

Final placement:

```text
semantic object
  owns WaitSource

reactor task
  owns TaskMailbox

driver future
  owns ActiveWait and YieldRegistration guards
```

Key rule:

```text
WaitSource belongs to the object.
TaskMailbox belongs to the reactor task.
ActiveWait belongs to the driver future.
```

------

## 3. Terminology

### WaitSource

An object-owned publication point for wait-relevant state changes.

Examples:

```text
Pipe.read_source
Pipe.write_source
Socket.recv_source
Process.exit_source
Thread.exit_source
Tty.hangup_source
Timerfd.expire_source
```

A `WaitSource` emits wake hints. It is not truth.

Truth remains in the object:

```text
pipe ring
socket receive queue
process lifecycle state
timerfd expiration counter
thread stop state
```

### TaskMailbox

A task-owned wake delivery queue.

It belongs to the reactor task, not to any semantic subsystem object.

### WakeHint

A small mailbox event saying:

```text
something relevant may have changed;
look at this replayable truth source.
```

A `WakeHint` must not carry subsystem semantic payload as the only copy of truth.

### WaitGeneration

A per-task monotone generation number identifying one active wait.

It prevents late hints from an old wait from affecting a newer wait.

### ActiveWait

Driver-local state for the currently yielded operation.

It is not a zone object and should not be stored as a long-lived field on `ThreadPayload`.

### Deadline

A deadline is not owned by `OnAgent`.

Deadlines are attached by the driver’s wait protocol as timer guards. The same mechanism supports:

```text
OnWaitSource + timeout
OnAgent + timeout
OnTimer primary sleep
```

------

## 4. Reactor-Side Structures

### 4.1 ReactorTask

```rust
pub struct ReactorTask {
    pub id: TaskId,
    pub future: KernelFuture,
    pub mailbox: Cap<TaskMailbox>,
    pub state: AtomicTaskState,
}
```

The reactor task owns the mailbox. Subsystems never directly own task scheduling state.

------

### 4.2 TaskMailbox

```rust
pub struct TaskMailbox {
    pub task: TaskHandle,

    /// Bounded wake-hint queue.
    queue: BoundedMpsc<WakeHint, MAILBOX_CAP>,

    /// Sticky overflow flag.
    overflow: AtomicBool,

    /// Monotone per-task wait generation.
    generation: AtomicU64,
}
```

Default bound:

```rust
pub const MAILBOX_CAP: usize = 32;
```

Rationale:

```text
small enough to keep per-task memory bounded and cache-friendly;
large enough that overflow is rare under normal non-adversarial wake traffic.
```

Required operations:

```rust
impl TaskMailbox {
    pub fn next_generation(&self) -> WaitGeneration;

    pub fn post(&self, hint: WakeHint);

    pub async fn recv(&self) -> WakeHint;

    pub fn try_pop(&self) -> Option<WakeHint>;

    pub fn take_overflow(&self) -> bool;
}
```

------

### 4.3 WakeHint

```rust
pub enum WakeHint {
    SourceFired {
        generation: WaitGeneration,
        source: WaitSourceId,
        interests: InterestMask,
    },

    AgentReplied {
        generation: WaitGeneration,
        token: DelegateTokenId,
    },

    TimerFired {
        generation: WaitGeneration,
        timer: TimerId,
    },

    Abort {
        generation: Option<WaitGeneration>,
        reason: AbortReason,
    },
}
```

There is no `Overflow` variant.

Overflow is represented by the mailbox’s sticky `overflow` flag. If the queue is full, an overflow event cannot be reliably enqueued. Therefore overflow is a flag, not a queue event.

#### TimerFired vs SourceFired

`TimerFired` is for driver-owned timer tokens:

```text
nanosleep / clock_nanosleep
poll/select/epoll_wait timeout
OnAgent deadline
```

Fd-shaped timer readiness uses `SourceFired`:

```text
timerfd expiration
  -> Timerfd.expire_source.notify()
  -> WakeHint::SourceFired
```

------

### 4.4 Mailbox Memory Ordering

The mailbox must satisfy:

```text
A producer that posts a WakeHint or sets overflow must publish that fact
before waking the reactor task.

A consumer that is woken must acquire before reading the queue or overflow flag.
```

Required ordering:

```rust
impl TaskMailbox {
    pub fn post(&self, hint: WakeHint) {
        if self.queue.try_push_release(hint).is_err() {
            self.overflow.store(true, Ordering::Release);
        }

        self.task.wake_after_release();
    }

    pub fn try_pop(&self) -> Option<WakeHint> {
        self.queue.try_pop_acquire()
    }

    pub fn take_overflow(&self) -> bool {
        self.overflow.swap(false, Ordering::AcqRel)
    }
}
```

Required happens-before relation:

```text
queue slot publication or overflow flag store
  happens-before
reactor observes wake and polls task
```

------

## 5. Object-Side Interface: WaitSource

### 5.1 WaitSource

```rust
pub struct WaitSource {
    id: WaitSourceId,

    /// Optional level-triggered readiness bits.
    readiness: AtomicReadiness,

    /// Direct task subscribers for R1.
    subscribers: SubscriberIndex,
}
```

Required operations:

```rust
impl WaitSource {
    pub fn id(&self) -> WaitSourceId;

    pub fn notify(&self, interests: InterestMask);

    pub fn register_prepared(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
        interests: InterestMask,
        predicate: PreparedPredicate,
    ) -> Result<WaitRegistrationGuard, ConditionChanged>;
}
```

`notify()` is called only after the semantic commit that made the change visible.

Example:

```rust
fn pipe_write_commit(pipe: &Pipe, bytes: &[u8]) {
    pipe.ring.push(bytes);

    pipe.read_source.readiness.set(Readiness::HasData);
    pipe.read_source.notify(Interest::HasData);
}
```

The wake does not authorize a read. The reader must rerun the step and recheck the pipe state.

------

### 5.2 Conditional Registration

A task may park on a `WaitSource` only after conditional registration has linearized against the predicate that made the step unable to progress.

Forbidden sequence:

```text
step observes predicate false
step returns YieldShape
driver registers later without recheck
producer notifies in between
task parks after notification
lost wake
```

Required sequence:

```text
observe predicate false
prepare registration with still-blocked predicate
register_prepared checks predicate under WaitSource critical section
if install succeeds:
  yield
else:
  retry step
```

The predicate belongs to the subsystem. The generic driver must not interpret subsystem predicates.

------

### 5.3 PreparedWaitRegistration

`PreparedWaitRegistration` must be allocation-free on the hot path.

```rust
pub struct PreparedWaitRegistration {
    pub source: Weak<WaitSource>,
    pub interests: InterestMask,
    pub predicate: PreparedPredicate,
}
pub enum PreparedPredicate {
    Atomic {
        check: AtomicPredicateFn,
        data: AtomicPredicateData,
    },

    Sequenced {
        seq: Cap<SeqCounter>,
        observed: u64,
    },
}
pub type AtomicPredicateFn =
    fn(data: &AtomicPredicateData) -> bool;
```

`AtomicPredicateData` is fixed-size inline storage:

```rust
pub struct AtomicPredicateData {
    inline: [u8; 32],
}
```

Rules:

```text
AtomicPredicateData must not contain heap pointers to unbounded predicate state.
If a predicate does not fit in the fixed inline storage, use the sequence-counter pattern.
```

Installation:

```rust
impl PreparedWaitRegistration {
    pub fn install(
        self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<WaitRegistrationGuard, ConditionChanged> {
        let source = self.source.upgrade()
            .ok_or(ConditionChanged)?;

        source.register_prepared(
            mailbox,
            generation,
            self.interests,
            self.predicate,
        )
    }
}
```

------

### 5.4 Predicate Constraints

The `PreparedPredicate` is evaluated inside the `WaitSource` registration critical section.

Therefore the predicate must be:

```text
non-blocking
non-allocating
non-recursive
atomic-load only
no lock acquisition
no user memory access
no subsystem callback
```

Allowed R1 patterns:

#### Pattern A: Atomic Predicate

Example:

```text
pipe ring is empty AND writers still exist
```

The predicate reads atomic head/tail/writer counters only.

#### Pattern B: Sequence Counter

Used for compound predicates.

Flow:

```text
step observes predicate false
step records seq value
register_prepared checks seq unchanged under source lock
if seq changed:
  ConditionChanged
else:
  registration succeeds
```

A producer that can change the predicate increments the sequence before or during notify.

------

### 5.5 WaitRegistrationGuard

```rust
pub struct WaitRegistrationGuard {
    source: Weak<WaitSource>,
    sub_id: SubscriptionId,
    generation: WaitGeneration,
}
```

Drop unregisters:

```rust
impl Drop for WaitRegistrationGuard {
    fn drop(&mut self) {
        if let Some(source) = self.source.upgrade() {
            source.unregister(self.sub_id, self.generation);
        }
    }
}
```

Unregistering does not guarantee no old wake hint is in flight. `WaitGeneration` remains required.

------

## 6. Timer Substrate

The timer substrate is used by:

```text
YieldShape::OnTimer
wait protocol deadlines
OnAgent timeout
poll/select/epoll_wait timeout
```

### 6.1 TimerWheel

```rust
pub struct TimerWheel;

impl TimerWheel {
    pub fn arm(&self, deadline: Deadline) -> Result<Cap<TimerToken>, Errno>;
}
```

Implementation may use a timer wheel, hierarchical timer wheel, or per-CPU timer heap. The runtime contract is independent of the concrete structure.

------

### 6.2 TimerToken

```rust
pub struct TimerToken;

impl TimerToken {
    pub fn id(&self) -> TimerId;

    pub fn bind_waiter(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<(), Errno>;

    pub fn is_expired(&self) -> bool;

    /// Idempotent. Safe after expiry and safe after prior cancellation.
    pub fn cancel(&self);
}
```

When a timer expires, it posts:

```rust
WakeHint::TimerFired {
    generation,
    timer,
}
```

Timer truth lives in `TimerToken`, not in `WakeHint`.

Invariant:

```text
TimerToken::cancel() is idempotent.
Calling cancel on an already-expired or already-cancelled timer is a no-op.
```

------

## 7. Driver-Local ActiveWait

`ActiveWait` is local to the async driver future.

```rust
pub struct ActiveWait {
    pub generation: WaitGeneration,
    pub shape: ActiveYieldShape,
    pub registrations: SmallVec<[YieldRegistration; 4]>,
}
```

All yield-shape side state must be unwound through `ActiveWait::drop`.

```rust
pub enum YieldRegistration {
    WaitSource(WaitRegistrationGuard),
    AgentToken(AgentTokenGuard),
    Timer(TimerGuard),
}
```

------

### 7.1 ActiveYieldShape

`YieldShape` is pre-install. `ActiveYieldShape` is post-install.

```rust
pub enum ActiveYieldShape {
    OnWaitSource {
        source: WaitSourceId,
        interests: InterestMask,
    },

    OnAgent {
        endpoint: Cap<DelegateEndpoint>,
        token: Cap<DelegateToken>,
        cancel: AgentCancelPolicy,
    },

    OnTimer {
        timer: Cap<TimerToken>,
    },
}
```

This avoids placeholders for consumed `PreparedWaitRegistration` values.

------

### 7.2 TimerGuard

A timer guard can be a primary sleep, a generic deadline, or an `OnAgent` deadline.

```rust
pub enum TimerRole {
    PrimarySleep,

    /// Generic deadline for OnWaitSource / poll / select.
    DeadlineAbort,

    /// Deadline for OnAgent.
    /// Expiry attempts DelegateToken::mark_timed_out().
    DelegateTimeout {
        token: Cap<DelegateToken>,
    },
}
pub struct TimerGuard {
    token: Cap<TimerToken>,
    generation: WaitGeneration,
    role: TimerRole,
}
```

Constructor-only style:

```rust
impl TimerGuard {
    pub fn new(
        token: Cap<TimerToken>,
        generation: WaitGeneration,
        role: TimerRole,
    ) -> Self {
        Self {
            token,
            generation,
            role,
        }
    }

    pub fn token_id(&self) -> TimerId {
        self.token.id()
    }

    pub fn role(&self) -> &TimerRole {
        &self.role
    }
}
```

Drop cancels the timer:

```rust
impl Drop for TimerGuard {
    fn drop(&mut self) {
        self.token.cancel();
    }
}
```

------

### 7.3 ActiveWait Timer Lookup

```rust
impl ActiveWait {
    /// Return the timer role installed in this wait, if any.
    fn timer_role(&self, timer: TimerId) -> Option<&TimerRole> {
        for reg in &self.registrations {
            if let YieldRegistration::Timer(g) = reg {
                if g.token_id() == timer {
                    return Some(g.role());
                }
            }
        }

        None
    }
}
```

------

## 8. Step Driver Integration

### 8.1 StepOutcome

```rust
pub enum StepOutcome<T, P> {
    Continue {
        progress: P,
    },

    Yield {
        progress: P,
        shape: YieldShape,
    },

    Done(T),

    Err(Errno),
}
```

------

### 8.2 YieldShape Minimum Set

```rust
pub enum YieldShape {
    OnWaitSource {
        source: WaitSourceId,
        interests: InterestMask,
        registration: PreparedWaitRegistration,
    },

    OnAgent {
        endpoint: Cap<DelegateEndpoint>,
        request: DelegateRequest,
        token: Cap<DelegateToken>,
        cancel: AgentCancelPolicy,
    },

    /// Timer-only wait, e.g. nanosleep / clock_nanosleep.
    OnTimer {
        deadline: Deadline,
    },
}
```

Reserved for future work:

```text
OnEdge
OnHandoff
```

Do not implement `OnEdge` or `OnHandoff` in this refactor.

------

### 8.3 Interruptibility and WaitProtocol

```rust
pub enum Interruptibility {
    Uninterruptible,
    Interruptible,
    Killable,
}
pub struct WaitProtocol {
    pub interruptibility: Interruptibility,
    pub deadline: Option<Deadline>,
}
impl WaitProtocol {
    pub fn signal_errno(&self) -> Errno {
        match self.interruptibility {
            Interruptibility::Interruptible => Errno::EINTR,

            // Fatal-only path. Non-fatal signals should not reach this branch.
            Interruptibility::Killable => Errno::EINTR,

            Interruptibility::Uninterruptible => {
                unreachable!("uninterruptible waits cannot return signal errno")
            }
        }
    }
}
```

Semantics:

```text
Interruptible:
  ordinary signal aborts the wait and maps through signal_errno / restart logic.

Killable:
  only fatal or teardown-class aborts are delivered to the wait.

Uninterruptible:
  ordinary signal abort hints are ignored by the driver.
```

Examples:

```text
read(pipe):
  YieldShape::OnWaitSource
  WaitProtocol { deadline: None }

poll(timeout=100ms):
  YieldShape::OnWaitSource
  WaitProtocol { deadline: Some(100ms) }

FUSE request with timeout:
  YieldShape::OnAgent
  WaitProtocol { deadline: Some(policy_timeout) }

nanosleep:
  YieldShape::OnTimer { deadline }
  WaitProtocol { deadline: None }
```

Rule:

```text
Deadline is a driver protocol attachment, not an OnAgent-only field.
```

------

### 8.4 StepOp

```rust
pub trait StepOp {
    type Output;
    type Progress: StepProgress;

    fn step(&mut self, ctx: &mut ScriptCtx)
        -> StepOutcome<Self::Output, Self::Progress>;

    /// Called after wait_active returns and before the next step() call.
    ///
    /// The StepOp may stash resume payload in its own state.
    /// For ResumeOutcome::Retry, this usually does nothing.
    /// For ResumeOutcome::WithReply, this stores the delegate reply so
    /// the next step() can consume it under a fresh guard.
    fn apply_resume(&mut self, resume: ResumeOutcome)
        -> Result<(), Errno>;
}
```

------

### 8.5 ResumeOutcome

```rust
pub enum ResumeOutcome {
    /// Rerun step. Used by OnWaitSource.
    Retry,

    /// Delegate reply available. Used by OnAgent.
    WithReply(DelegateReply),

    /// Primary timer wait expired.
    TimerExpired(TimerId),

    /// Wait aborted.
    Aborted(AbortReason),
}
```

------

### 8.6 Driver Loop

```rust
pub async fn drive<O: StepOp>(
    mut op: O,
    ctx: &mut ScriptCtx,
    mode: DriveMode,
    protocol: WaitProtocol,
) -> Result<O::Output, Errno> {
    let mut acc = O::Progress::EMPTY;

    loop {
        match op.step(ctx) {
            StepOutcome::Continue { progress } => {
                acc.extend(progress);
                continue;
            }

            StepOutcome::Done(value) => {
                return Ok(value);
            }

            StepOutcome::Err(errno) => {
                return mode.finish_error(errno, acc);
            }

            StepOutcome::Yield { progress, shape } => {
                acc.extend(progress);

                match mode.disposition(&shape, &acc) {
                    YieldDisposition::Resolve => {
                        let mut active =
                            prepare_active_wait(shape, ctx, &protocol)?;

                        let resume =
                            wait_active(&mut active, ctx, &protocol).await?;

                        op.apply_resume(resume)?;
                        continue;
                    }

                    YieldDisposition::ReturnPartial => {
                        return mode.return_partial(acc);
                    }

                    YieldDisposition::ReturnErr(errno) => {
                        return Err(errno);
                    }

                    YieldDisposition::Unsupported => {
                        // POSIX convention: "operation not supported on this object/mode."
                        // Modes wanting a different errno should classify as
                        // YieldDisposition::ReturnErr(custom) instead of Unsupported.
                        return Err(Errno::EOPNOTSUPP);
                    }
                }
            }
        }
    }
}
```

`DriveMode` must not be a boolean `accepts()` check.

It must support POSIX translation:

```text
nonblocking read/write:
  no progress + would block -> EAGAIN
  some progress + would block -> partial return
```

------

## 9. Preparing ActiveWait

```rust
fn prepare_active_wait(
    shape: YieldShape,
    ctx: &mut ScriptCtx,
    protocol: &WaitProtocol,
) -> Result<ActiveWait, Errno> {
    if matches!(shape, YieldShape::OnTimer { .. }) && protocol.deadline.is_some() {
        return Err(Errno::EINVAL);
    }

    let generation = ctx.task_mailbox.next_generation();
    let mut registrations = SmallVec::<[YieldRegistration; 4]>::new();

    let active_shape = match shape {
        YieldShape::OnWaitSource {
            source,
            interests,
            registration,
        } => {
            let guard = registration.install(
                ctx.task_mailbox.downgrade(),
                generation,
            )?;

            registrations.push(YieldRegistration::WaitSource(guard));

            ActiveYieldShape::OnWaitSource {
                source,
                interests,
            }
        }

        YieldShape::OnAgent {
            endpoint,
            request,
            token,
            cancel,
        } => {
            // Move the request envelope onto the token before enqueue.
            token.install_request(request)?;

            token.bind_waiter(
                ctx.task_mailbox.downgrade(),
                generation,
            )?;

            // Derive token-drop policy from the agent-cancel policy.
            // (BestEffort + Synchronous map to CancelOnDrop; Detached maps to Abandon.)
            let drop_policy = TokenDropPolicy::from_agent_cancel(cancel);

            let guard = AgentTokenGuard::new(
                token.clone(),
                generation,
                cancel,
                drop_policy,
            );

            endpoint.enqueue_request(token.clone())?;

            registrations.push(YieldRegistration::AgentToken(guard));

            ActiveYieldShape::OnAgent {
                endpoint,
                token,
                cancel,
            }
        }

        YieldShape::OnTimer {
            deadline,
        } => {
            let timer = ctx.timer_wheel.arm(deadline)?;

            timer.bind_waiter(
                ctx.task_mailbox.downgrade(),
                generation,
            )?;

            registrations.push(YieldRegistration::Timer(
                TimerGuard::new(
                    timer.clone(),
                    generation,
                    TimerRole::PrimarySleep,
                )
            ));

            ActiveYieldShape::OnTimer {
                timer,
            }
        }
    };

    if let Some(deadline) = protocol.deadline {
        let timer = ctx.timer_wheel.arm(deadline)?;

        timer.bind_waiter(
            ctx.task_mailbox.downgrade(),
            generation,
        )?;

        let role = match &active_shape {
            ActiveYieldShape::OnAgent { token, .. } => {
                TimerRole::DelegateTimeout {
                    token: token.clone(),
                }
            }

            _ => TimerRole::DeadlineAbort,
        };

        registrations.push(YieldRegistration::Timer(
            TimerGuard::new(
                timer,
                generation,
                role,
            )
        ));
    }

    Ok(ActiveWait {
        generation,
        shape: active_shape,
        registrations,
    })
}
```

Invariant:

```text
active wait generation
registration generation
token generation
timer generation
mailbox hint generation
must match.
```

Rule:

```text
OnTimer is already a primary timer wait.
Composing it with WaitProtocol.deadline is invalid in R1.
```

------

## 10. Waiting and Resume Classification

### 10.1 wait_active

```rust
async fn wait_active(
    active: &mut ActiveWait,
    ctx: &mut ScriptCtx,
    protocol: &WaitProtocol,
) -> Result<ResumeOutcome, Errno> {
    loop {
        if ctx.task_mailbox.take_overflow() {
            if let Some(resume) = rescan_active_sources(active, ctx)? {
                return Ok(resume);
            }
        }

        let hint = ctx.task_mailbox.recv().await;

        if !hint_matches_generation(&hint, active.generation) {
            continue;
        }

        match classify_hint(active, hint, ctx)? {
            ResumeClass::Ready(resume) => {
                return Ok(resume);
            }

            ResumeClass::StillBlocked => {
                continue;
            }

            ResumeClass::Aborted(reason) => {
                return Err(reason.into_errno_for(protocol));
            }
        }
    }
}
```

Overflow is checked before waiting for the next event.

------

### 10.2 Generation Filtering

```rust
fn hint_matches_generation(
    hint: &WakeHint,
    active: WaitGeneration,
) -> bool {
    match hint {
        WakeHint::Abort {
            generation: None,
            ..
        } => true,

        WakeHint::Abort {
            generation: Some(g),
            ..
        } => *g == active,

        WakeHint::SourceFired {
            generation,
            ..
        } => *generation == active,

        WakeHint::AgentReplied {
            generation,
            ..
        } => *generation == active,

        WakeHint::TimerFired {
            generation,
            ..
        } => *generation == active,
    }
}
```

Rule:

```text
Abort { generation: None } is global-to-task and bypasses generation filtering.
Abort { generation: Some(g) } applies only to active wait generation g.
```

------

### 10.3 ResumeClass

```rust
pub enum ResumeClass {
    Ready(ResumeOutcome),
    StillBlocked,
    Aborted(AbortReason),
}
```

------

### 10.4 classify_hint

```rust
fn classify_hint(
    active: &ActiveWait,
    hint: WakeHint,
    ctx: &mut ScriptCtx,
) -> Result<ResumeClass, Errno> {
    match hint {
        WakeHint::TimerFired { timer, .. } => {
            handle_timer_fired(active, timer)
        }

        WakeHint::SourceFired { source: hint_source, .. } => {
            match &active.shape {
                ActiveYieldShape::OnWaitSource { source, .. }
                    if *source == hint_source =>
                {
                    Ok(ResumeClass::Ready(
                        ResumeOutcome::Retry
                    ))
                }

                _ => Ok(ResumeClass::StillBlocked),
            }
        }

        WakeHint::AgentReplied { token: hint_token, .. } => {
            match &active.shape {
                ActiveYieldShape::OnAgent { token, .. }
                    if token.id() == hint_token =>
                {
                    classify_delegate_token(token)
                }

                _ => Ok(ResumeClass::StillBlocked),
            }
        }

        WakeHint::Abort { reason, .. } => {
            Ok(ResumeClass::Aborted(reason))
        }
    }
}
```

------

### 10.5 handle_timer_fired

```rust
fn handle_timer_fired(
    active: &ActiveWait,
    timer: TimerId,
) -> Result<ResumeClass, Errno> {
    match active.timer_role(timer) {
        Some(TimerRole::PrimarySleep) => {
            Ok(ResumeClass::Ready(
                ResumeOutcome::TimerExpired(timer)
            ))
        }

        Some(TimerRole::DeadlineAbort) => {
            Ok(ResumeClass::Aborted(
                AbortReason::TimedOut
            ))
        }

        Some(TimerRole::DelegateTimeout { token }) => {
            token.mark_timed_out();
            classify_delegate_token(token)
        }

        None => {
            Ok(ResumeClass::StillBlocked)
        }
    }
}
```

For `OnAgent`, timeout and reply race on `DelegateToken.state`.

Mailbox delivery order does not define semantic order. The token state transition defines semantic order.

------

### 10.6 classify_delegate_token

```rust
fn classify_delegate_token(
    token: &Cap<DelegateToken>,
) -> Result<ResumeClass, Errno> {
    match token.state() {
        DelegateState::Replied => {
            let reply = token.take_reply()?;
            Ok(ResumeClass::Ready(
                ResumeOutcome::WithReply(reply)
            ))
        }

        DelegateState::Pending |
        DelegateState::ReplyInstalling => {
            Ok(ResumeClass::StillBlocked)
        }

        DelegateState::Canceled => {
            Ok(ResumeClass::Aborted(AbortReason::Canceled))
        }

        DelegateState::TimedOut => {
            Ok(ResumeClass::Aborted(AbortReason::TimedOut))
        }

        DelegateState::AgentDied => {
            Ok(ResumeClass::Aborted(AbortReason::AgentDied))
        }
    }
}
```

------

### 10.7 Overflow Rescan

```rust
fn rescan_active_sources(
    active: &ActiveWait,
    ctx: &mut ScriptCtx,
) -> Result<Option<ResumeOutcome>, Errno> {
    for reg in &active.registrations {
        if let YieldRegistration::Timer(timer_guard) = reg {
            if timer_guard.token.is_expired() {
                match timer_guard.role() {
                    TimerRole::PrimarySleep => {
                        return Ok(Some(
                            ResumeOutcome::TimerExpired(
                                timer_guard.token_id()
                            )
                        ));
                    }

                    TimerRole::DeadlineAbort => {
                        return Ok(Some(
                            ResumeOutcome::Aborted(
                                AbortReason::TimedOut
                            )
                        ));
                    }

                    TimerRole::DelegateTimeout { token } => {
                        token.mark_timed_out();

                        return match classify_delegate_token(token)? {
                            ResumeClass::Ready(r) => Ok(Some(r)),

                            ResumeClass::Aborted(a) => {
                                Ok(Some(ResumeOutcome::Aborted(a)))
                            }

                            ResumeClass::StillBlocked => Ok(None),
                        };
                    }
                }
            }
        }
    }

    match &active.shape {
        ActiveYieldShape::OnWaitSource { .. } => {
            Ok(Some(ResumeOutcome::Retry))
        }

        ActiveYieldShape::OnAgent { token, .. } => {
            match classify_delegate_token(token)? {
                ResumeClass::Ready(r) => Ok(Some(r)),

                ResumeClass::Aborted(a) => {
                    Ok(Some(ResumeOutcome::Aborted(a)))
                }

                ResumeClass::StillBlocked => Ok(None),
            }
        }

        ActiveYieldShape::OnTimer { timer } => {
            if timer.is_expired() {
                Ok(Some(ResumeOutcome::TimerExpired(timer.id())))
            } else {
                Ok(None)
            }
        }
    }
}
```

For `OnWaitSource`, overflow causes retry. The step must recheck the object predicate.

------

## 11. Delegate Runtime

### 11.1 DelegateEndpoint

```rust
pub struct DelegateEndpoint {
    id: DelegateEndpointId,
    scope: EndpointScope,

    /// Tokens, not envelopes. The request envelope lives on the token.
    request_queue: MpscQueue<Cap<DelegateToken>>,

    state: AtomicEndpointState,

    /// Readiness for agents waiting for requests.
    request_source: WaitSource,
}

pub enum EndpointScope {
    /// Endpoint dies when the named thread exits (ptrace per-thread).
    Thread(Cap<ThreadIdentity>),

    /// Endpoint dies when the named process exits (FUSE, userfaultfd,
    /// fanotify-permission, seccomp-user-notification).
    Process(Cap<ProcessIdentity>),
}
```

Required operations:

```rust
impl DelegateEndpoint {
    pub fn enqueue_request(
        &self,
        token: Cap<DelegateToken>,
    ) -> Result<(), DelegateError>;

    pub fn dequeue_request(
        &self,
    ) -> Result<Cap<DelegateToken>, DelegateError>;

    /// Walks in_flight and calls mark_agent_died on each token, releasing
    /// every script waiter with Aborted(AgentDied).
    pub fn abort_all(
        &self,
        reason: AgentAbortReason,
    );
}
```

When a request is enqueued (after `token.install_request(req)` has placed the envelope on the token):

```text
request_queue.push(token)
request_source.notify(HasRequest)
```

The agent dequeues a `Cap<DelegateToken>`, reads `token.request()` to get the envelope, computes, and replies via `token.reply(reply)`.

Agents that block on `/dev/fuse`, userfaultfd, ptrace endpoint, or equivalent wait on `request_source`.

**Endpoint-scope abandonment.** The endpoint subscribes to its scope's exit_source via the runtime. When the scope's owning thread or process exits, the runtime calls `abort_all(AgentDied)` on every endpoint of that scope. Each in-flight token then transitions `Pending → AgentDied` (CAS), wakes its bound script waiter with `Aborted(AgentDied)`, and the script returns `Err(EOWNERDEAD)`.

------

### 11.2 DelegateToken

```rust
pub struct DelegateToken {
    id: DelegateTokenId,

    state: AtomicDelegateState,

    /// Request envelope, installed by install_request before enqueue.
    request: OnceCell<DelegateRequest>,

    /// Reply envelope, written only during ReplyInstalling.
    reply: OnceCell<DelegateReply>,

    waiter: AtomicOption<Weak<TaskMailbox>>,

    generation: AtomicU64,
}
```

The token holds both the request envelope (placed by the script before enqueue) and the reply envelope (placed by the agent during the `ReplyInstalling` phase). The endpoint queue carries only `Cap<DelegateToken>`; the request envelope never travels through the queue separately.

`AgentCancelPolicy` and `TokenDropPolicy` are *not* stored on the token. The token has no agent-side or token-side use for either: `AgentCancelPolicy` is consulted by the runtime when posting cancellation to the agent; `TokenDropPolicy` is consulted by `AgentTokenGuard` (§11.6) when the script's `ActiveWait` drops. Both live on `AgentTokenGuard` and are accessed only there.

State machine:

```rust
pub enum DelegateState {
    Pending,
    ReplyInstalling,
    Replied,
    Canceled,
    AgentDied,
    TimedOut,
}
```

Invariant:

```text
DelegateToken.reply is populated iff DelegateToken.state == Replied.
Only ReplyInstalling may write the reply slot.
```

------

### 11.3 bind_waiter

```rust
impl DelegateToken {
    pub fn bind_waiter(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<(), Errno> {
        self.waiter.store(Some(mailbox), Ordering::Release);
        self.generation.store(generation.raw(), Ordering::Release);

        Ok(())
    }
}
```

Deadlines are not passed to `bind_waiter`. Deadlines are implemented by driver-installed `TimerGuard`s.

Neither `AgentCancelPolicy` nor `TokenDropPolicy` is passed to `bind_waiter`. Both live on `AgentTokenGuard`: `AgentCancelPolicy` controls how the kernel notifies the agent on cancellation (BestEffort/Synchronous/Detached); `TokenDropPolicy` controls what happens when the script's `ActiveWait` drops (CancelOnDrop/Abandon). The token itself has no use for either.

------

### 11.4 reply

```rust
impl DelegateToken {
    pub fn reply(
        &self,
        reply: DelegateReply,
    ) -> Result<(), LateReply> {
        if self.state
            .compare_exchange(
                DelegateState::Pending,
                DelegateState::ReplyInstalling,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(LateReply);
        }

        self.reply
            .set(reply)
            .expect("ReplyInstalling owns the reply slot");

        self.state.store(DelegateState::Replied, Ordering::Release);

        self.wake_waiter(WakeHintKind::AgentReplied);

        Ok(())
    }
}
```

------

### 11.5 cancel and Terminal Transitions

Every transition from `Pending` to a terminal non-pending state must wake the bound waiter if present.

```rust
impl DelegateToken {
    /// Script-side cancel.  CASes the state to Canceled and wakes the waiter;
    /// `agent_cancel` controls how the agent is notified (BestEffort posts a
    /// CANCEL on the request carrier; Synchronous additionally awaits ack;
    /// Detached drops without notification).
    pub fn cancel_with(
        &self,
        reason: CancelReason,
        agent_cancel: AgentCancelPolicy,
    ) {
        if self.state
            .compare_exchange(
                DelegateState::Pending,
                DelegateState::Canceled,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.notify_agent_cancel(agent_cancel);
            self.wake_waiter(WakeHintKind::Abort(
                reason.into_abort_reason()
            ));
        }
    }

    pub fn mark_agent_died(&self) {
        if self.state
            .compare_exchange(
                DelegateState::Pending,
                DelegateState::AgentDied,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.wake_waiter(WakeHintKind::Abort(
                AbortReason::AgentDied
            ));
        }
    }

    pub fn mark_timed_out(&self) {
        if self.state
            .compare_exchange(
                DelegateState::Pending,
                DelegateState::TimedOut,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.wake_waiter(WakeHintKind::Abort(
                AbortReason::TimedOut
            ));
        }
    }
}
```

Wake helper:

```rust
enum WakeHintKind {
    AgentReplied,
    Abort(AbortReason),
}

impl DelegateToken {
    fn wake_waiter(&self, kind: WakeHintKind) {
        let generation = WaitGeneration::from_raw(
            self.generation.load(Ordering::Acquire)
        );

        if let Some(mailbox) =
            self.waiter.load(Ordering::Acquire).upgrade()
        {
            match kind {
                WakeHintKind::AgentReplied => {
                    mailbox.post(WakeHint::AgentReplied {
                        generation,
                        token: self.id,
                    });
                }

                WakeHintKind::Abort(reason) => {
                    mailbox.post(WakeHint::Abort {
                        generation: Some(generation),
                        reason,
                    });
                }
            }
        }
    }
}
```

------

### 11.6 AgentTokenGuard

```rust
pub struct AgentTokenGuard {
    token: Cap<DelegateToken>,
    generation: WaitGeneration,

    /// What the kernel tells the agent on cancellation.
    agent_cancel: AgentCancelPolicy,

    /// What ActiveWait drop does to the token.
    drop_policy: TokenDropPolicy,
}
```

Drop:

```rust
impl Drop for AgentTokenGuard {
    fn drop(&mut self) {
        match self.drop_policy {
            TokenDropPolicy::CancelOnDrop => {
                // Cancel the token; the agent_cancel policy controls how the
                // agent is notified (BestEffort posts a CANCEL on the request
                // carrier; Synchronous additionally awaits an ack).
                self.token.cancel_with(CancelReason::Abandoned, self.agent_cancel);
                self.token.unbind_waiter(self.generation);
            }

            TokenDropPolicy::Abandon => {
                self.token.unbind_waiter(self.generation);
                // No cancel; agent's pending request resolves on its own. Late
                // reply will land on a dead waiter and be dropped by generation
                // filtering.
            }
        }
    }
}
```

`OnAgent` must always install an `AgentTokenGuard` into `ActiveWait.registrations`.

`cancel_with()` and `unbind_waiter()` must be idempotent with respect to already-terminal token states.

------

### 11.7 OnAgent Reply-vs-Timeout Race

Rule:

```text
For OnAgent waits with a deadline, the DelegateToken is the truth source.

Agent reply attempts:
  Pending -> ReplyInstalling -> Replied

Deadline expiry attempts:
  Pending -> TimedOut

Whichever state transition linearizes first determines the outcome.
A later competing transition is rejected as late.
```

Therefore:

```text
Mailbox order does not define semantic order.
DelegateToken.state defines semantic order.
```

------

### 11.8 DelegateReply Cross-Yield Safety

`DelegateReply` crosses yield because it is stored in `DelegateToken`.

Invariant:

```text
DELEGATE-3:
  DelegateReply and all of its constituents must satisfy YIELD-1.
```

Allowed:

```text
owned descriptors
Cap<T>
OperationalEvidence
replayable signifiers
fd injection descriptors
plain data
```

Forbidden:

```text
IdentRef
Witness
epoch::Guard
reservation guard
borrowed user slice
guard-bound reference
```

------

## 12. Abort and Cancel Enums

### 12.1 AbortReason

```rust
pub enum AbortReason {
    Signal,
    Canceled,
    TimedOut,
    AgentDied,

    /// Future use for OnBehalfOf / borrowed-subject teardown.
    BorrowerExited,
}
```

Errno mapping is protocol-dependent:

```rust
impl AbortReason {
    pub fn into_errno_for(&self, protocol: &WaitProtocol) -> Errno {
        match self {
            AbortReason::Signal => protocol.signal_errno(),
            AbortReason::Canceled => Errno::EINTR,
            AbortReason::TimedOut => Errno::ETIMEDOUT,
            AbortReason::AgentDied => Errno::EOWNERDEAD,
            AbortReason::BorrowerExited => Errno::EOWNERDEAD,
        }
    }
}
```

Do not hard-code one global errno mapping for every wait type if POSIX restart/interrupt behavior differs.

------

### 12.2 Cancellation Policies

Cancellation has two orthogonal axes. Each axis has its own closed enum so the two cannot be silently confused.

#### `AgentCancelPolicy` — what the kernel tells the agent

```rust
pub enum AgentCancelPolicy {
    /// Notify the endpoint that the request is canceled; give up after a grace.
    BestEffort,

    /// Notify and wait for the agent's cancellation acknowledgement on a
    /// per-token cancel-ack carrier.  The token state still CASes
    /// Pending → Canceled immediately; the kernel additionally yields until
    /// the agent acks before releasing scope-owned resources.  No new token
    /// state is needed.
    Synchronous,

    /// Fire-and-forget. Only valid for idempotent agent operations whose
    /// pending work has no observable side effects on the kernel.
    Detached,
}
```

Carried in `YieldShape::OnAgent::cancel`.

#### `TokenDropPolicy` — what `ActiveWait` drop does

```rust
pub enum TokenDropPolicy {
    /// Drop ⇒ token.cancel_with(Abandoned, agent_cancel); waiter unbound.
    CancelOnDrop,

    /// Drop ⇒ unbind waiter only. Late reply lands on a dead waiter and is
    /// rejected by generation filtering.
    Abandon,

    // Reserved (not R2):
    // /// Drop unbinds waiter but preserves the token for another consumer.
    // /// Requires a concrete use case to justify; not yet implemented.
    // KeepAlive,
}
```

Held internally by `AgentTokenGuard` (§11.6); never read during normal resume. Derived from `AgentCancelPolicy` at `prepare_active_wait` time:

```rust
impl TokenDropPolicy {
    pub fn from_agent_cancel(agent: AgentCancelPolicy) -> Self {
        match agent {
            AgentCancelPolicy::BestEffort | AgentCancelPolicy::Synchronous
                => TokenDropPolicy::CancelOnDrop,
            AgentCancelPolicy::Detached
                => TokenDropPolicy::Abandon,
        }
    }
}
```

The composition is meaningful: `CancelOnDrop + Synchronous` means "on drop, cancel and wait for agent ack." Splitting the policies makes that expressible without overloading a single enum.

------

## 13. Thread Runtime Changes

`ThreadPayload` should not store a full `WaitFrame`.

Minimum additions:

```rust
pub struct ThreadPayload {
    pub task: TaskHandle,
    pub mailbox: Cap<TaskMailbox>,

    pub signal_summary: AtomicSignalSummary,
    pub cancel_state: AtomicCancelState,
}
```

Signal delivery wakes via mailbox abort hint:

```rust
fn mark_deliverable_signal(thread: &Cap<ThreadIdentity>, sig: Signum) {
    let payload = thread.payload()?;

    payload.signal_summary.set_deliverable(sig);

    payload.mailbox.post(WakeHint::Abort {
        generation: None,
        reason: AbortReason::Signal,
    });
}
```

`generation: None` means wake the task regardless of active wait generation.

The driver interprets this according to the wait protocol:

```text
Interruptible -> EINTR / restart
Killable -> abort only for fatal
Uninterruptible -> ignore normal signal and keep waiting
```

------

## 14. Invariants

### WS-1: WaitSource Ownership

```text
Every wait-relevant publication point is owned by exactly one semantic object
or one explicitly declared aggregate object.
```

### WS-2: WaitSource Is Not Truth

```text
WaitSource notification is never sufficient authority to proceed.
The next step must re-observe the corresponding truth source.
```

### WAIT-1: Conditional Registration

```text
A task may park on a WaitSource only after conditional registration has
linearized against the predicate that made the operation unable to progress.
```

### WAIT-2: Predicate Constraints

```text
The still-blocked predicate used by registration must be non-blocking,
non-allocating, atomic-load-only, and must not acquire locks or call
subsystem callbacks.
```

### WAIT-3: Stale Hint Filtering

```text
Every wait-specific WakeHint carries the active WaitGeneration.
Hints with stale generations are ignored.
```

### ABORT-1: Generationless Abort

```text
Abort hints with generation=None bypass generation filtering and wake the
task regardless of the active wait generation.
```

### MAIL-1: Mailbox Is Delivery, Not Truth

```text
TaskMailbox stores wake hints only. It never stores subsystem semantic
payload as the sole copy of truth.
```

### MAIL-2: Publish Before Wake

```text
A WakeHint or overflow flag must be published with release ordering before
the reactor task is woken. The task must acquire before consuming mailbox
contents or overflow state.
```

### MAIL-3: Overflow Is Recoverable

```text
Mailbox overflow is correct only because every active wait has a replayable
truth source. Overflow forces the driver to rescan the active wait source.
```

### TIMER-1: Timer Cancel Idempotency

```text
TimerToken::cancel() is idempotent.
Calling cancel on an already-expired or already-cancelled timer is a no-op.
```

### DTOK-1: Delegate Reply Coherence

```text
DelegateToken.reply is populated iff DelegateToken.state == Replied.
ReplyInstalling is the only state allowed to write the reply slot.
```

### DTOK-2: Terminal Transition Wakes Waiter

```text
Any DelegateToken transition from Pending to a terminal non-pending state
must post a WakeHint to the bound waiter if present.
```

### DTOK-3: Delegate Deadline Linearization

```text
OnAgent reply and OnAgent timeout race on DelegateToken.state.
The first successful state transition determines the result.
```

### DELEGATE-3: Delegate Reply Cross-Yield Safety

```text
DelegateReply and all of its constituents must satisfy YIELD-1.
```

### YIELD-1: Cross-Yield Safety

```text
YieldShape, ActiveWait, WakeHint, YieldRegistration, TimerToken, DelegateToken,
and DelegateReply must not contain IdentRef, Witness, epoch::Guard,
reservation guard, borrowed user slice, or any guard-bound reference.
```

Allowed across yield:

```text
Cap<T>
OperationalEvidence
owned descriptors
UserPtr + len as replayable descriptor
DelegateToken
TimerToken
WaitSourceId
WaitGeneration
owned kernel buffer
plain data
```

### CLEANUP-1: ActiveWait Owns Yield Cleanup

```text
All side state installed for a yield must be represented by a YieldRegistration
inside ActiveWait and unwound by ActiveWait drop.
```

------

## 15. Required Interfaces to Land

ID and tag types (`WaitSourceId`, `TimerId`, `DelegateTokenId`, `DelegateEndpointId`, `SubscriptionId`, `TaskId`) are newtype `u64` unless noted otherwise; the implementation may choose representation as long as `Eq`, `Copy`, and `Hash` are preserved. Bag tags (`InterestMask`, `Deadline`, `AtomicReadiness`, `AtomicTaskState`, `AtomicSignalSummary`, `AtomicCancelState`, `AtomicEndpointState`, `AtomicDelegateState`) are subsystem-local types whose concrete shape is implementation-defined.

### Reactor

```rust
pub struct TaskMailbox;
pub enum WakeHint;
pub struct WaitGeneration;

impl TaskMailbox {
    pub fn next_generation(&self) -> WaitGeneration;
    pub fn post(&self, hint: WakeHint);
    pub async fn recv(&self) -> WakeHint;
    pub fn try_pop(&self) -> Option<WakeHint>;
    pub fn take_overflow(&self) -> bool;
}
```

------

### Wait Source

```rust
pub struct WaitSource;
pub struct PreparedWaitRegistration;
pub enum PreparedPredicate;
pub struct AtomicPredicateData;
pub struct WaitRegistrationGuard;

impl WaitSource {
    pub fn notify(&self, interests: InterestMask);

    pub fn register_prepared(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
        interests: InterestMask,
        predicate: PreparedPredicate,
    ) -> Result<WaitRegistrationGuard, ConditionChanged>;
}
```

------

### Timer

```rust
pub struct TimerWheel;
pub struct TimerToken;

impl TimerWheel {
    pub fn arm(&self, deadline: Deadline) -> Result<Cap<TimerToken>, Errno>;
}

impl TimerToken {
    pub fn id(&self) -> TimerId;

    pub fn bind_waiter(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<(), Errno>;

    pub fn is_expired(&self) -> bool;

    /// Idempotent.
    pub fn cancel(&self);
}
```

------

### Step Driver

```rust
pub trait StepOp {
    type Output;
    type Progress: StepProgress;

    fn step(&mut self, ctx: &mut ScriptCtx)
        -> StepOutcome<Self::Output, Self::Progress>;

    fn apply_resume(&mut self, resume: ResumeOutcome)
        -> Result<(), Errno>;
}
pub enum YieldShape {
    OnWaitSource {
        source: WaitSourceId,
        interests: InterestMask,
        registration: PreparedWaitRegistration,
    },

    OnAgent {
        endpoint: Cap<DelegateEndpoint>,
        request: DelegateRequest,
        token: Cap<DelegateToken>,
        cancel: AgentCancelPolicy,
    },

    OnTimer {
        deadline: Deadline,
    },
}
pub enum ActiveYieldShape {
    OnWaitSource {
        source: WaitSourceId,
        interests: InterestMask,
    },

    OnAgent {
        endpoint: Cap<DelegateEndpoint>,
        token: Cap<DelegateToken>,
        cancel: AgentCancelPolicy,
    },

    OnTimer {
        timer: Cap<TimerToken>,
    },
}
pub struct ActiveWait {
    generation: WaitGeneration,
    shape: ActiveYieldShape,
    registrations: SmallVec<[YieldRegistration; 4]>,
}

pub enum YieldRegistration {
    WaitSource(WaitRegistrationGuard),
    AgentToken(AgentTokenGuard),
    Timer(TimerGuard),
}
pub enum ResumeOutcome {
    Retry,
    WithReply(DelegateReply),
    TimerExpired(TimerId),
    Aborted(AbortReason),
}
pub enum Interruptibility {
    Uninterruptible,
    Interruptible,
    Killable,
}

pub struct WaitProtocol {
    pub interruptibility: Interruptibility,
    pub deadline: Option<Deadline>,
}
pub enum AbortReason {
    Signal,
    Canceled,
    TimedOut,
    AgentDied,
    BorrowerExited,
}

pub enum AgentCancelPolicy {
    /// Notify the endpoint, give up after grace.
    BestEffort,
    /// Notify and wait for agent ack on a per-token cancel-ack carrier.
    Synchronous,
    /// Fire-and-forget. Only safe for idempotent agent ops.
    Detached,
}

pub enum TokenDropPolicy {
    /// ActiveWait drop ⇒ token.cancel_with(Abandoned, agent_cancel); waiter unbound.
    CancelOnDrop,
    /// ActiveWait drop ⇒ unbind waiter only.
    Abandon,
    // Reserved (not R2): KeepAlive — preserves token for another consumer.
}

impl TokenDropPolicy {
    pub fn from_agent_cancel(agent: AgentCancelPolicy) -> Self;
}
```

------

### Delegate

The runtime layer specifies the linearization of `DelegateToken` and the wake-routing path. The concrete shapes of requests, replies, and per-kind error / abort taxonomies are subsystem-defined and live in each `EndpointKind`'s own subsystem doc (FUSE, userfaultfd, fanotify-permission, ptrace, seccomp-user-notification). The runtime treats the following as opaque:

```rust
// Subsystem-defined; each EndpointKind specifies concrete shapes.
pub enum DelegateRequest    { /* per-kind */ }
pub enum DelegateReply      { /* per-kind */ }
pub enum DelegateError      { /* per-kind */ }
pub enum AgentAbortReason   { /* per-kind */ }
pub struct LateReply;
```

Cancellation and abort vocabulary used by the runtime:

```rust
pub enum CancelReason {
    /// ActiveWait dropped with TokenDropPolicy::CancelOnDrop.
    Abandoned,

    /// Script-side explicit cancel.
    Explicit,

    // Reserved: ScopeExited (for OnBehalfOf teardown).
}

impl CancelReason {
    pub fn into_abort_reason(self) -> AbortReason {
        match self {
            CancelReason::Abandoned | CancelReason::Explicit => AbortReason::Canceled,
        }
    }
}
```

```rust
pub struct DelegateEndpoint;
pub struct DelegateToken;

pub enum DelegateState {
    Pending,
    ReplyInstalling,
    Replied,
    Canceled,
    AgentDied,
    TimedOut,
}

impl DelegateEndpoint {
    pub fn scope(&self) -> &EndpointScope;

    pub fn enqueue_request(
        &self,
        token: Cap<DelegateToken>,
    ) -> Result<(), DelegateError>;

    pub fn dequeue_request(
        &self,
    ) -> Result<Cap<DelegateToken>, DelegateError>;

    pub fn abort_all(
        &self,
        reason: AgentAbortReason,
    );
}

pub enum EndpointScope {
    Thread(Cap<ThreadIdentity>),
    Process(Cap<ProcessIdentity>),
}

impl DelegateToken {
    /// Install the request envelope on the token before enqueue.
    pub fn install_request(
        &self,
        request: DelegateRequest,
    ) -> Result<(), Errno>;

    pub fn request(&self) -> Option<&DelegateRequest>;

    pub fn bind_waiter(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<(), Errno>;

    pub fn reply(
        &self,
        reply: DelegateReply,
    ) -> Result<(), LateReply>;

    /// Script-side cancel. Cooperates with the agent per `agent_cancel`.
    pub fn cancel_with(
        &self,
        reason: CancelReason,
        agent_cancel: AgentCancelPolicy,
    );

    pub fn mark_agent_died(&self);

    pub fn mark_timed_out(&self);

    pub fn unbind_waiter(
        &self,
        generation: WaitGeneration,
    );
}
```

------

## 16. Minimal Implementation Phases

### Phase R1 — Direct Wait Sources and Timer Substrate

Land:

```text
TaskMailbox
WakeHint
WaitGeneration
WaitSource
PreparedWaitRegistration
PreparedPredicate
WaitRegistrationGuard
YieldShape::OnWaitSource
YieldShape::OnTimer
TimerWheel / TimerToken
driver-local ActiveWait
generation filtering
mailbox overflow handling
protocol deadline via TimerGuard
```

Target users:

```text
pipe
basic socket readiness
timerfd
pidfd exit readiness
basic futex wait
nanosleep / clock_nanosleep
poll/select timeout substrate
```

------

### Phase R2 — Delegate Runtime

Land:

```text
DelegateEndpoint
DelegateToken
DelegateState::ReplyInstalling
YieldShape::OnAgent
AgentReplied WakeHint
AgentTokenGuard
agent cancel/death/timeout states using the R1 protocol-deadline mechanism
```

Target users:

```text
FUSE skeleton
userfaultfd skeleton
fanotify-permission skeleton
ptrace stop skeleton
seccomp user notification skeleton
```

------

### Phase R3 — Aggregators and Edge Semantics

Defer:

```text
EpollInstance as WakeSink
RawEdge
EPOLLET
inotify/fanotify edge buffers
```

------

### Phase R4 — Handoff

Defer:

```text
OnHandoff
Owned<T>
PI futex
rtmutex
scheduler donation
```

------

## 17. Non-Goals

This refactor must not do the following:

```text
Do not move all semantic ports into ThreadPayload.
Do not introduce a heap/zone-allocated WaitFrame object.
Do not implement generic WakeSink universe in R1.
Do not implement RawEdge in R1.
Do not implement OnHandoff in R1.
Do not implement priority donation in R1.
Do not make mailbox events carry semantic payload.
Do not let YieldShape resolution change SubjectContext.
Do not let agent replies become authority.
```

------

## 18. Naming Alignment

Use the runtime name consistently:

```text
WaitSource
OnWaitSource
SourceFired
```

Do not use `Carrier` in the runtime spec.

If concept-layer documents still use `OnCarrier`, update them in the same PR:

```text
OnCarrier -> OnWaitSource
```

Rationale:

```text
The implementation object is a WaitSource.
The yield shape waits on a WaitSource.
The mailbox hint says a source fired.
```

------

## 19. Summary

The structural shape is:

```text
SubjectContext decides who executes.

StepOp decides how semantic state advances.

YieldShape decides why execution cannot continue synchronously.

WaitSource is the object-owned waitable publication point.

TaskMailbox is the task-owned wake delivery queue.

WakeHint is a hint, not truth.

WaitGeneration rejects stale hints.

ActiveWait is driver-local yielded state.

YieldRegistration provides uniform cleanup.

TimerGuard provides primary sleep and protocol deadlines.

DelegateToken is the truth source for OnAgent.
```

Most important rule:

```text
Object state remains authoritative.
Wake delivery is centralized.
Resume always revalidates.
```

Runtime flow:

```text
object commit
  -> WaitSource.notify()
  -> TaskMailbox.post(WakeHint)
  -> reactor wakes task
  -> driver validates generation
  -> driver rechecks truth source
  -> StepOp continues
```

Delegate flow:

```text
requester step
  -> YieldShape::OnAgent
  -> DelegateToken bound to TaskMailbox + generation
  -> optional protocol deadline installs TimerGuard
  -> agent replies
  -> DelegateToken transitions Pending -> ReplyInstalling -> Replied
  -> TaskMailbox.post(AgentReplied)
  -> driver validates generation and token
  -> StepOp consumes reply under fresh step
```

Deadline flow:

```text
agent never replies or wait times out
  -> deadline TimerToken expires
  -> TaskMailbox.post(TimerFired)
  -> TimerRole::DelegateTimeout calls token.mark_timed_out()
  -> token races Pending -> TimedOut against reply's Pending -> ReplyInstalling
  -> first token state transition wins
  -> driver returns reply or ETIMEDOUT accordingly
```