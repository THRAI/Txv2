# Reactor Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Converge `tx-reactor` on one generation-safe task state machine, one poll/commit path, hart-owned runqueues, transition-sensitive wake routing, and per-thread userspace rendezvous ownership.

**Architecture:** A generation-checked `TaskArena` stores stable `Arc<TaskCell>` values. Each `TaskCell` owns one locked `TaskControl` containing the Future, lifecycle, execution owner, lease epoch, queue epoch, cancellation state, and last observed wake sequence. Scheduler modules return policy decisions; hart modules own physical queues and dispatch; all production and host facades call one `drive_hart` loop.

**Tech Stack:** Rust `no_std` + `alloc`, txKernel `SpinLock`, Rust `Future`/`Waker`, `tx-time`, `tx-substrate` mailbox/wait primitives, `cargo xtask`, RV64 QEMU.

---

## Scope

This plan implements Phases 0 through 4 of
`docs/superpowers/specs/2026-07-14-reactor-refactor-design.md`. Measured
segmented task storage and per-hart wake-ingress sharding are explicitly
outside this plan; the completed design retains `Vec<TaskSlot>` and one shared,
transition-sensitive `WakeIngress`.

The checkout is dirty in Reactor, kernel trap/thread, ThreadRuntime, timer, and
progress files. Every task must preserve existing edits and stage only the
paths listed for that task. Do not reset, checkout, or rewrite unrelated work.

## File Map

### New runtime modules

- `crates/tx-reactor/src/core/mod.rs`: core facade and narrow re-exports.
- `crates/tx-reactor/src/core/key.rs`: `TaskId`, `TaskGeneration`, `TaskKey`,
  `QueueEpoch`, `LeaseEpoch`, and `RunToken`.
- `crates/tx-reactor/src/core/task.rs`: `TaskCell`, immutable attachments, and
  task snapshots.
- `crates/tx-reactor/src/core/arena.rs`: `Vec<TaskSlot>`, free list, terminal
  records, generation reuse, and cell lookup.
- `crates/tx-reactor/src/core/transition.rs`: `TaskControl`, legal state pairs,
  wake/submit/cancel/terminal transitions, and invariant checks.
- `crates/tx-reactor/src/core/poll.rs`: `PollLease`, acquisition, commit, poll
  context guard, and `CommitAction`.
- `crates/tx-reactor/src/core/tests.rs`: deterministic lifecycle, wake, cancel,
  token, and lease tests requiring crate-private access.
- `crates/tx-reactor/src/wake/mod.rs`: wake facade.
- `crates/tx-reactor/src/wake/state.rs`: transition-sensitive
  `TaskWakeState`.
- `crates/tx-reactor/src/wake/ingress.rs`: shared `WakeIngress` and `WakeToken`.
- `crates/tx-reactor/src/wake/route.rs`: wake-state reconciliation and
  placement requests.
- `crates/tx-reactor/src/hart/mod.rs`: hart facade.
- `crates/tx-reactor/src/hart/local.rs`: `ReactorLocals` and
  `HartReactorLocal`.
- `crates/tx-reactor/src/hart/run_queue.rs`: four physical queues containing
  `RunToken`.
- `crates/tx-reactor/src/hart/dispatch.rs`: apply placement, need-resched, and
  IPI actions.
- `crates/tx-reactor/src/hart/drive.rs`: sole poll loop and hart-step report.
- `crates/tx-reactor/src/scheduler/mod.rs`: scheduler facade.
- `crates/tx-reactor/src/scheduler/policy.rs`: policy trait and public
  vocabulary.
- `crates/tx-reactor/src/scheduler/metadata.rs`: affinity, priority, budget,
  and accounting, without authoritative run owner.
- `crates/tx-reactor/src/scheduler/placement.rs`: submit/wake/stop decisions.
- `crates/tx-reactor/src/scheduler/balance.rs`: victim and migration decisions.
- `crates/tx-reactor/src/userspace/mod.rs`: userspace facade.
- `crates/tx-reactor/src/userspace/rendezvous.rs`: per-thread request state.
- `crates/tx-reactor/src/userspace/compat.rs`: temporary global test facade,
  deleted before plan completion.
- `crates/tx-reactor/src/wait/{mod,registration,channel,future,timeout}.rs`:
  existing wait functionality split by responsibility.
- `crates/tx-reactor/src/timer/{mod,domain,route}.rs`: timer domain and route
  integration over `tx-time`.
- `crates/tx-reactor/src/coord/{mod,completion,sync,delegate}.rs`: completion,
  token/ack, and delegate protocols.
- `crates/tx-reactor/src/adapter/{mod,bus_wire,step_engine}.rs`: external
  vocabulary adapters.
- `crates/tx-reactor/src/observability.rs`: transition counters/events moved
  out of the runtime coordinator.

### Existing files retired or reduced

- `crates/tx-reactor/src/task.rs`: removed after `core` extraction.
- `crates/tx-reactor/src/waker.rs`: removed after `wake` extraction.
- `crates/tx-reactor/src/runtime.rs`: reduced to the public `Reactor` facade or
  removed after `hart::drive` extraction.
- `crates/tx-reactor/src/scheduler.rs`: removed after scheduler extraction.
- `crates/tx-reactor/src/hart_loop.rs`: reduced to step vocabulary and adapter
  calls into `hart::drive`.
- `crates/tx-reactor/src/userspace.rs`: removed after userspace extraction.
- `crates/tx-reactor/src/wait.rs`: removed after wait extraction.
- `crates/tx-reactor/src/deadline_registry.rs`: moved to `timer`.
- `crates/tx-reactor/src/{completion,sync_coord,agent_reply}.rs`: moved to
  `coord`.
- `crates/tx-reactor/src/{mailbox,wait_source}.rs`: deleted after root
  re-exports point directly at adapters/substrate.

### Tests and integration consumers

- `crates/tx-reactor/tests/task_lifecycle.rs`: public lifecycle and generation
  behavior.
- `crates/tx-reactor/tests/reactor_smoke.rs`: poll/wake/SMP/production-facade
  convergence.
- `crates/tx-reactor/tests/scheduler.rs`: policy-only tests using an explicit
  local queue harness.
- `crates/tx-reactor/tests/hart_loop.rs`: one-driver timing and idle behavior.
- `crates/tx-reactor/tests/userspace_run.rs`: rendezvous state machine and
  compatibility retirement.
- `crates/tx-reactor/tests/{completion,sync_coord,wait_bus,wait_interrupt,
  timer_idle,v3_pr7b_timer_routing,v3_timer_surface}.rs`: import and facade
  migration.
- `crates/tx-kernel/src/init.rs`: production `drive_hart` caller.
- `crates/tx-kernel/src/thread_future.rs`: timer-preempt-transparent per-thread
  rendezvous use.
- `crates/tx-kernel/src/trap_handoff.rs`: strict running-request completion.
- `crates/tx-kernel/src/thread_future/tests.rs`: userspace round-trip tests.
- `crates/tx-subsystems/src/thread_runtime/{structure,execution,tests}.rs`:
  active request publication and cancellation.
- `docs/progress/STATUS.md`: completion catch-up.
- `docs/progress/plans/2026-07-14-reactor-refactor.json`: operational progress
  record created when execution starts.

## Task 1: Restore the Reactor Test Baseline

**Files:**
- Modify: `crates/tx-reactor/tests/reactor_smoke.rs:203,332,573`
- Verify: `crates/tx-substrate/src/step/agent.rs:658`

- [ ] **Step 1: Confirm the focused green baseline**

Run:

```bash
cargo test -p tx-reactor --test task_lifecycle --test userspace_run
```

Expected: `task_lifecycle` reports 8 passed and `userspace_run` reports 16
passed.

- [ ] **Step 2: Confirm the broad test compile failure**

Run:

```bash
cargo test -p tx-reactor --test reactor_smoke --no-run
```

Expected: three `E0061` errors at lines near 203, 332, and 573 because
`DelegateRegistry::install_request` accepts five arguments.

- [ ] **Step 3: Remove the obsolete sixth argument**

At each failing call, change:

```rust
let guard = registry.install_request(
    token,
    endpoint,
    deadline,
    operation,
    Arc::downgrade(&mailbox),
    None,
)?;
```

to:

```rust
let guard = registry.install_request(
    token,
    endpoint,
    deadline,
    operation,
    Arc::downgrade(&mailbox),
)?;
```

Preserve the actual local variable names and surrounding error handling.

- [ ] **Step 4: Rebuild the broad test**

Run:

```bash
cargo test -p tx-reactor --test reactor_smoke --no-run
```

Expected: compilation succeeds and emits the test executable path.

- [ ] **Step 5: Commit the baseline repair**

```bash
git add crates/tx-reactor/tests/reactor_smoke.rs
git commit -m "test(reactor): align delegate request fixtures"
```

## Task 2: Add Deterministic State-Machine Witnesses

**Files:**
- Create: `crates/tx-reactor/src/runtime/tests.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/src/task.rs`
- Modify: `crates/tx-reactor/src/scheduler.rs`

- [ ] **Step 1: Add a crate-private runtime test module**

Append to `runtime.rs`:

```rust
#[cfg(test)]
mod tests;
```

- [ ] **Step 2: Write the lost-wake regression**

Create `runtime/tests.rs` with a test that constructs a `TaskTable`, submits a
pending Future, creates matching scheduler metadata, takes the Future for
polling, commits it parked, routes a wake to queued, then simulates the old
poller's blocked stop notification:

```rust
#[test]
fn wake_after_task_park_before_scheduler_stop_remains_runnable() {
    let mut tasks = TaskTable::new();
    let key = tasks.submit(core::future::pending::<()>());
    let mut scheduler = Phase1Scheduler::new();
    scheduler.task_submitted(
        key.id(),
        TaskHandle::new(key.id()),
        InitialSchedMeta::fair(),
    );
    let _ = scheduler.pick_next(HartId(0)).expect("submitted task");

    let (_, future, _, _) = tasks
        .take_runnable_future_by_id(key.id())
        .expect("runnable task");
    assert_eq!(
        tasks.finish_polled_pending(key, future),
        Ok(PendingPollCommit::Parked)
    );

    let (woken, _) = tasks
        .make_parked_owner_runnable_with_hint(key.id(), key.generation())
        .expect("parked task wakes");
    scheduler.task_runnable(woken.id(), WakeHint::Normal);
    scheduler.task_stopped(woken.id(), StopReason::Blocked, 0, HartId(0));

    assert!(matches!(
        scheduler.task_owner(woken.id()),
        Some(TaskRunOwner::Queued { .. })
    ));
}
```

Adjust only constructor parameters that differ in the current checkout; retain
the exact interleaving and final assertion.

- [ ] **Step 3: Write cancellation-during-poll and stale-token witnesses**

Add tests with these assertions:

```rust
#[test]
fn cancelling_a_polled_task_defers_slot_reuse_until_commit() {
    let mut tasks = TaskTable::new();
    let first = tasks.submit(core::future::pending::<()>());
    let first_token = tasks.place_for_test(first, HartId(0), Phase1QueueKind::New);
    let lease = tasks
        .acquire_poll(HartId(0), first_token)
        .expect("first poll lease");

    assert!(matches!(
        tasks.cancel_task(first),
        Ok(CancelOutcome::DeferredToLease(_))
    ));
    let while_polling = tasks.submit(core::future::pending::<()>());
    assert_ne!(while_polling.id(), first.id());

    assert_eq!(
        tasks.commit_poll(
            lease,
            PollResult::Pending {
                stop: StopReason::Blocked,
                consumed_ns: 0,
                slice_expired: false,
            },
        ),
        Ok(CommitAction::Cancelled)
    );
    assert_eq!(tasks.drain_cancelled().len(), 1);

    let after_drain = tasks.submit(core::future::pending::<()>());
    assert_eq!(after_drain.id(), first.id());
    assert_ne!(after_drain.generation(), first.generation());
}

#[test]
fn a_queue_token_from_an_old_generation_cannot_poll_a_reused_slot() {
    let mut tasks = TaskTable::new();
    let first = tasks.submit(core::future::ready(()));
    let stale_token = tasks.place_for_test(first, HartId(0), Phase1QueueKind::New);
    let lease = tasks
        .acquire_poll(HartId(0), stale_token)
        .expect("first poll lease");
    assert_eq!(
        tasks.commit_poll(lease, PollResult::Ready),
        Ok(CommitAction::Completed)
    );
    assert_eq!(tasks.drain_completed().len(), 1);

    let replacement = tasks.submit(core::future::pending::<()>());
    assert_eq!(replacement.id(), first.id());
    assert_ne!(replacement.generation(), first.generation());
    assert!(matches!(
        tasks.acquire_poll(HartId(0), stale_token),
        Err(AcquireError::StaleHandle)
    ));
}
```

Task 3 must provide the crate-test-only `place_for_test` helper with exactly
this signature; production code must not call it. Do not introduce sleeps or
timing-based synchronization.

- [ ] **Step 4: Run the tests and verify the red state**

Run:

```bash
cargo test -p tx-reactor --lib runtime::tests -- --nocapture
```

Expected: the lost-wake assertion fails with owner `Parked`; cancellation
reuse or stale-token coverage also fails until Task 3 defines the new types.

- [ ] **Step 5: Commit only the failing witnesses**

```bash
git add crates/tx-reactor/src/runtime.rs crates/tx-reactor/src/runtime/tests.rs
git commit -m "test(reactor): pin poll wake and cancel races"
```

## Task 3: Introduce Stable TaskCell, TaskControl, and PollLease

**Files:**
- Modify: `crates/tx-reactor/src/task.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/src/scheduler.rs`
- Test: `crates/tx-reactor/src/runtime/tests.rs`
- Test: `crates/tx-reactor/tests/task_lifecycle.rs`

- [ ] **Step 1: Add epoch and token vocabulary**

Define next to `TaskKey`:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LeaseEpoch(u32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct QueueEpoch(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunToken {
    pub key: TaskKey,
    pub queue_epoch: QueueEpoch,
    pub queue: Phase1QueueKind,
}
```

Provide private checked increment helpers that return lifecycle errors on
exhaustion rather than wrapping.

- [ ] **Step 2: Define authoritative state pairs**

Move execution ownership beside lifecycle:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskRunOwner {
    Parked,
    Queued {
        hart: HartId,
        queue: Phase1QueueKind,
        epoch: QueueEpoch,
    },
    Polling {
        hart: HartId,
        lease: LeaseEpoch,
    },
    Terminal,
}

struct TaskControl {
    future: Option<TaskFuture>,
    status: TaskStatus,
    owner: TaskRunOwner,
    lease_epoch: LeaseEpoch,
    queue_epoch: QueueEpoch,
    observed_wake_seq: u64,
    cancel_requested: bool,
    last_stop_reason: Option<StopReason>,
}
```

Add `debug_assert_valid()` matching only the five legal state/owner pairs in
the design spec.

- [ ] **Step 3: Replace slot-owned Task with stable TaskCell**

Use stable cell allocation while retaining the vector/free-list arena:

```rust
struct TaskCell {
    key: TaskKey,
    control: SpinLock<TaskControl>,
    wake_state: Arc<TaskWakeState>,
    mailbox: Arc<TaskMailbox>,
    ast: SpinLock<AstSlot>,
    last_ast_batch: SpinLock<AstBatch>,
}

struct TaskSlot {
    generation: TaskGeneration,
    task: Option<Arc<TaskCell>>,
}
```

`TaskTable::resolve(key)` clones the `Arc<TaskCell>` under the table lock and
validates generation before returning it.

- [ ] **Step 4: Define PollLease and acquisition**

```rust
pub(crate) struct PollLease {
    cell: Arc<TaskCell>,
    key: TaskKey,
    hart: HartId,
    lease_epoch: LeaseEpoch,
    wake_seq_at_acquire: u64,
    future: Option<TaskFuture>,
    mailbox: Arc<TaskMailbox>,
}
```

`TaskTable::acquire_poll(hart, token)` resolves the cell, locks control,
validates runnable/queued key+epoch+hart+queue, moves out the Future, increments
the lease, records the current wake sequence, and commits `Polling + Polling`.

- [ ] **Step 5: Define a single commit result**

```rust
pub(crate) enum PollResult {
    Ready,
    Pending {
        stop: StopReason,
        consumed_ns: u64,
        slice_expired: bool,
    },
}

pub(crate) enum CommitAction {
    Parked,
    Enqueue(PlacementRequest),
    Completed,
    Cancelled,
    Stale,
}
```

`commit_poll` validates generation and lease, accounts the scheduler stop
decision, reconciles wake sequence and cancellation, restores or drops the
Future, and changes lifecycle+owner through one match.

- [ ] **Step 6: Replace both existing take/finish call pairs**

In both loops, replace:

```rust
tasks.take_runnable_future_by_id(handle.id())
future.as_mut().poll(&mut context)
tasks.finish_polled_pending(handle, future)
scheduler.task_stopped(handle.id(), stop_reason, consumed_ns, hart)
```

with:

```rust
let mut lease = self.acquire_poll(hart, token)?;
let result = lease.poll(&mut context);
let action = self.commit_poll(lease, result)?;
self.apply_commit_action(action, signal);
```

At this task, the loops may remain duplicated; only the state transition
implementation becomes shared.

- [ ] **Step 7: Run the state and lifecycle tests**

```bash
cargo test -p tx-reactor --lib runtime::tests -- --nocapture
cargo test -p tx-reactor --test task_lifecycle -- --nocapture
```

Expected: lost-wake, active-lease cancellation, stale generation, reuse, and
existing lifecycle tests pass.

- [ ] **Step 8: Commit the authoritative transition core**

```bash
git add crates/tx-reactor/src/task.rs crates/tx-reactor/src/runtime.rs \
  crates/tx-reactor/src/scheduler.rs crates/tx-reactor/src/runtime/tests.rs \
  crates/tx-reactor/tests/task_lifecycle.rs
git commit -m "refactor(reactor): centralize task poll transitions"
```

## Task 4: Make Wake Ingress Transition-Sensitive

**Files:**
- Modify: `crates/tx-reactor/src/waker.rs`
- Modify: `crates/tx-reactor/src/task.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Test: `crates/tx-reactor/tests/task_lifecycle.rs`
- Test: `crates/tx-reactor/src/runtime/tests.rs`

- [ ] **Step 1: Extend the public wake coalescing test**

Change `repeated_wakes_coalesce_before_drain` to assert both semantic and
physical coalescing through a crate-private counter/snapshot:

```rust
for _ in 0..64 {
    waker.wake_by_ref();
}
assert_eq!(reactor.wake_ingress_depth_for_test(), 1);
assert_eq!(reactor.run_until_idle().polled, 1);
assert_eq!(reactor.wake_ingress_depth_for_test(), 0);
```

- [ ] **Step 2: Write a clear-versus-wake test**

Use a deterministic drain hook or a unit-test-only ingress method to interleave
one wake after the router records `processed_seq` and before it clears the
latch. Assert exactly one replacement token remains and the task is polled.

- [ ] **Step 3: Replace the raw wake queue**

Implement:

```rust
pub(crate) struct WakeToken {
    pub key: TaskKey,
}

pub(crate) struct WakeIngress {
    queue: SpinLock<VecDeque<WakeToken>>,
}

pub(crate) struct TaskWakeState {
    key: TaskKey,
    wake_seq: AtomicU64,
    ingress_queued: AtomicBool,
    ingress: Weak<WakeIngress>,
}
```

`wake()` increments `wake_seq` and pushes only after a successful
`false -> true` compare-exchange on `ingress_queued`.

- [ ] **Step 4: Implement race-safe drain completion**

After routing one token:

```rust
let processed = state.wake_seq();
route_wake(token, processed);
state.clear_ingress_latch();
if state.wake_seq() != processed && state.try_latch_ingress() {
    ingress.push(token);
}
```

Use Acquire/Release ordering and document the linearization points in
`wake/state.rs` when the later file move occurs.

- [ ] **Step 5: Run wake tests**

```bash
cargo test -p tx-reactor --test task_lifecycle repeated_wakes -- --nocapture
cargo test -p tx-reactor --lib runtime::tests -- --nocapture
cargo test -p tx-reactor --test wait_bus -- --nocapture
```

Expected: one ingress token per unobserved wake batch, clear-race wake retained,
and wait/mailbox behavior unchanged.

- [ ] **Step 6: Commit wake convergence**

```bash
git add crates/tx-reactor/src/waker.rs crates/tx-reactor/src/task.rs \
  crates/tx-reactor/src/runtime.rs crates/tx-reactor/src/runtime/tests.rs \
  crates/tx-reactor/tests/task_lifecycle.rs
git commit -m "fix(reactor): coalesce task wake ingress"
```

## Task 5: Put Generation-Bearing RunToken in Hart Queues

**Files:**
- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Test: `crates/tx-reactor/tests/scheduler.rs`
- Test: `crates/tx-reactor/tests/reactor_smoke.rs`
- Test: `crates/tx-reactor/src/runtime/tests.rs`

- [ ] **Step 1: Change physical queue element types**

Replace each `VecDeque<TaskId>` in `HartRunQueues` with
`VecDeque<RunToken>`. Queue helpers accept and return tokens:

```rust
pub(crate) fn push_to_local_queue(
    local: &HartSchedulerLocal,
    token: RunToken,
    front: bool,
);

pub(crate) fn pop_from_local_queue(
    local: &HartSchedulerLocal,
    queue: Phase1QueueKind,
) -> Option<RunToken>;
```

- [ ] **Step 2: Return placement without storing authoritative owner in policy**

Change `LocalEnqueueRequest` to carry:

```rust
pub struct LocalEnqueueRequest {
    pub hart: HartId,
    pub token: RunToken,
    pub front: bool,
}
```

Task control creates queue epoch and owner; scheduler returns target hart,
queue class, insertion direction, and slice/accounting decisions.

- [ ] **Step 3: Validate tokens after queue pop**

The hart loop releases the queue lock, then calls `acquire_poll(hart, token)`.
Treat `StaleHandle`, wrong owner, wrong queue epoch, and cancellation as stale
queue entries and continue without polling.

- [ ] **Step 4: Migrate scheduler tests to a key allocator harness**

Replace direct `TaskId(raw)` queue submissions with:

```rust
struct SchedulerHarness {
    next_id: usize,
    generation: TaskGeneration,
    scheduler: Phase1Scheduler,
    local: HartSchedulerLocal,
}

impl SchedulerHarness {
    fn submit(&mut self, meta: InitialSchedMeta) -> TaskKey {
        let key = TaskKey::new_for_test(self.next_id, self.generation);
        self.next_id += 1;
        self.scheduler.register_task(key, meta);
        key
    }
}
```

Keep `new_for_test` crate-private by moving scheduler policy tests that need it
into `scheduler/tests.rs`; integration tests cover only public behavior.

- [ ] **Step 5: Add stale token and duplicate token assertions**

Assert an old generation cannot acquire a reused slot, and two physical copies
of one token result in one poll plus one stale-token drop.

- [ ] **Step 6: Run scheduler and smoke tests**

```bash
cargo test -p tx-reactor --test scheduler -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
cargo test -p tx-reactor --lib runtime::tests -- --nocapture
```

Expected: all scheduler queue-order tests pass using full tokens; stale and
duplicate entries never poll twice.

- [ ] **Step 7: Commit tokenized queues**

```bash
git add crates/tx-reactor/src/scheduler.rs crates/tx-reactor/src/runtime.rs \
  crates/tx-reactor/src/runtime/tests.rs crates/tx-reactor/tests/scheduler.rs \
  crates/tx-reactor/tests/reactor_smoke.rs
git commit -m "refactor(reactor): validate generation-bearing run tokens"
```

## Task 6: Finish Two-Phase Cancellation and Terminal Recycling

**Files:**
- Modify: `crates/tx-reactor/src/task.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Test: `crates/tx-reactor/tests/task_lifecycle.rs`
- Test: `crates/tx-reactor/src/runtime/tests.rs`

- [ ] **Step 1: Add cancellation outcome vocabulary**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelOutcome {
    Committed,
    DeferredToLease(LeaseEpoch),
    AlreadyTerminal(TaskStatus),
}
```

- [ ] **Step 2: Implement state-specific cancellation**

Under `TaskControl`:

```rust
match control.owner {
    TaskRunOwner::Polling { lease, .. } => {
        control.cancel_requested = true;
        CancelOutcome::DeferredToLease(lease)
    }
    TaskRunOwner::Queued { .. } | TaskRunOwner::Parked => {
        control.queue_epoch = control.queue_epoch.next()?;
        control.future.take();
        control.status = TaskStatus::Cancelled;
        control.owner = TaskRunOwner::Terminal;
        CancelOutcome::Committed
    }
    TaskRunOwner::Terminal => CancelOutcome::AlreadyTerminal(control.status),
}
```

- [ ] **Step 3: Gate terminal drain and slot reuse**

Drain only `Completed/Cancelled + Terminal` cells with no Future and no active
lease. Remove the `Arc<TaskCell>` from the slot, then place the id on the free
list. Increment generation before constructing the next cell.

- [ ] **Step 4: Run lifecycle witnesses**

```bash
cargo test -p tx-reactor --test task_lifecycle -- --nocapture
cargo test -p tx-reactor --lib runtime::tests cancelling -- --nocapture
```

Expected: queued/parked cancellation is immediate, polling cancellation is
deferred, and no slot is reused before commit and drain.

- [ ] **Step 5: Commit cancellation semantics**

```bash
git add crates/tx-reactor/src/task.rs crates/tx-reactor/src/runtime.rs \
  crates/tx-reactor/src/runtime/tests.rs crates/tx-reactor/tests/task_lifecycle.rs
git commit -m "fix(reactor): defer cancellation across active poll leases"
```

## Task 7: Extract Core and Wake Modules Without Behavior Changes

**Files:**
- Create: `crates/tx-reactor/src/core/{mod,key,task,arena,transition,poll,tests}.rs`
- Create: `crates/tx-reactor/src/wake/{mod,state,ingress,route}.rs`
- Modify: `crates/tx-reactor/src/lib.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Delete: `crates/tx-reactor/src/task.rs`
- Delete: `crates/tx-reactor/src/waker.rs`

- [ ] **Step 1: Move key and arena vocabulary**

Move types without renaming public variants. `core/mod.rs` uses narrow exports:

```rust
pub use key::{TaskGeneration, TaskId, TaskKey};
pub use task::{TaskDrainRecord, TaskLifecycleError, TaskSnapshot, TaskStatus};
pub(crate) use key::{LeaseEpoch, QueueEpoch, RunToken};
pub(crate) use poll::{CommitAction, PollLease, PollResult};
pub(crate) use transition::{TaskControl, TaskRunOwner};
```

- [ ] **Step 2: Move wake implementation**

`wake/mod.rs` exports no raw lock or queue fields:

```rust
pub(crate) use ingress::{WakeIngress, WakeToken};
pub(crate) use route::{WakeRouteAction, WakeRouter};
pub(crate) use state::TaskWakeState;
```

- [ ] **Step 3: Move private tests with the implementation**

Move `runtime/tests.rs` state-machine tests to `core/tests.rs`; leave driver
tests in the runtime/hart test module.

- [ ] **Step 4: Update imports and delete old files**

Use explicit `crate::core` and `crate::wake` paths. Do not add a broad prelude.

- [ ] **Step 5: Verify the mechanical move**

```bash
cargo fmt --check
cargo test -p tx-reactor --lib
cargo test -p tx-reactor --test task_lifecycle --test wait_bus
git diff --check
```

Expected: behavior and test counts are unchanged from Task 6.

- [ ] **Step 6: Commit core/wake extraction**

```bash
git add crates/tx-reactor/src/core crates/tx-reactor/src/wake \
  crates/tx-reactor/src/lib.rs crates/tx-reactor/src/runtime.rs \
  crates/tx-reactor/src/task.rs crates/tx-reactor/src/waker.rs
git commit -m "refactor(reactor): extract task core and wake routing"
```

## Task 8: Separate Scheduler Policy from Hart-Owned Queues

**Files:**
- Create: `crates/tx-reactor/src/scheduler/{mod,policy,metadata,placement,balance,tests}.rs`
- Create: `crates/tx-reactor/src/hart/{mod,local,run_queue,dispatch}.rs`
- Modify: `crates/tx-reactor/src/lib.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/tests/scheduler.rs`
- Delete: `crates/tx-reactor/src/scheduler.rs`

- [ ] **Step 1: Move policy vocabulary and metadata**

Move `HartId`, `InitialSchedMeta`, `SliceConfig`, `StopReason`, `WakeHint`,
affinity errors, accounting, placement, and balance decisions into scheduler
children. Do not move `HartRunQueues` or queue locks into scheduler.

- [ ] **Step 2: Move physical queues into hart**

```rust
pub(crate) struct HartRunQueue {
    kernel: VecDeque<RunToken>,
    boosted: VecDeque<RunToken>,
    new: VecDeque<RunToken>,
    preempted: VecDeque<RunToken>,
}

pub struct HartReactorLocal {
    hart: HartId,
    run_queue: SpinLock<HartRunQueue>,
    markers: PreemptMarkers,
    drive_active: AtomicBool,
    stats: HartRunStatsCell,
}
```

- [ ] **Step 3: Make policy methods decision-only**

Policy methods return `Placement`/`StopDecision`; only
`hart::dispatch::apply_placement` pushes tokens and marks/sends reschedule.

- [ ] **Step 4: Remove `compat_locals`**

Migrate private policy tests to use an explicit `HartReactorLocal` or pure
metadata fixture. Remove `Phase1Scheduler.compat_locals`,
`ensure_compat_hart`, `apply_compat_enqueue`, and all queue-mutating legacy
methods.

- [ ] **Step 5: Add lock-boundary assertions**

Ensure queue pop releases the queue lock before task-control validation and
that balance scans never hold a hart queue lock while acquiring scheduler
metadata.

- [ ] **Step 6: Run policy, queue, and SMP tests**

```bash
cargo test -p tx-reactor --test scheduler -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
cargo test -p tx-reactor --test dispatch -- --nocapture
```

Expected: queue ordering, affinity, wake placement, steal validation, and
remote dispatch tests pass without scheduler-owned locals.

- [ ] **Step 7: Commit scheduler/hart separation**

```bash
git add crates/tx-reactor/src/scheduler crates/tx-reactor/src/hart \
  crates/tx-reactor/src/scheduler.rs crates/tx-reactor/src/runtime.rs \
  crates/tx-reactor/src/lib.rs crates/tx-reactor/tests/scheduler.rs \
  crates/tx-reactor/tests/reactor_smoke.rs crates/tx-reactor/tests/dispatch.rs
git commit -m "refactor(reactor): separate scheduler policy from hart queues"
```

## Task 9: Replace Duplicate Poll Loops with One Hart Driver

**Files:**
- Create: `crates/tx-reactor/src/hart/drive.rs`
- Modify: `crates/tx-reactor/src/hart/mod.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/src/hart_loop.rs`
- Modify: `crates/tx-reactor/tests/hart_loop.rs`
- Modify: `crates/tx-reactor/tests/reactor_smoke.rs`
- Modify: `crates/tx-kernel/src/init.rs`

- [ ] **Step 1: Add same-hart reentry regression**

Use a Future that calls the host drive facade recursively on the same hart and
assert the nested call returns `HartDriveError::Busy(HartId(0))` or an explicit
busy `HartLoopStep` without polling a second task or replacing task-local
mailbox context.

- [ ] **Step 2: Define driver inputs**

```rust
pub struct PollBudget {
    pub max_polls: usize,
    pub stop_when_idle: bool,
}

pub enum HartDriveError {
    Busy(HartId),
    UnknownHart(HartId),
}
```

- [ ] **Step 3: Move one complete loop into `hart::drive`**

The sole loop order is timer drive, wake drain, marker consume, token pop,
lease acquire, poll, accounting, commit, dispatch, repeat, deadline restore.
Use an RAII guard around `HartReactorLocal.drive_active.compare_exchange` so
all returns clear the flag.

- [ ] **Step 4: Replace public facade bodies**

`SharedReactor::run_hart_loop_concurrent*`,
`HartRuntimeView::run_until_idle*`, and `Reactor::run_until_idle*` become
parameter adapters over:

```rust
self.drive_hart(hart, budget, clock, signal)
```

They contain no Future take, poll, status, owner, or queue transitions.

- [ ] **Step 5: Migrate the production caller**

Change `crates/tx-kernel/src/init.rs` to invoke the stable driver facade while
preserving the current slice clock, reschedule signal, deadline arm, and
`HartLoopStep` interpretation.

- [ ] **Step 6: Prove facade equivalence**

Add a test that runs the same scripted tasks through host `run_until_idle` and
the concurrent facade and compares poll count, completion count, final task
snapshot, queue depths, and next deadline.

- [ ] **Step 7: Run driver tests**

```bash
cargo test -p tx-reactor --test hart_loop -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
cargo test -p tx-kernel thread_future -- --nocapture
```

Expected: one implementation serves both facades; same-hart reentry is
rejected; production integration compiles.

- [ ] **Step 8: Commit the single driver**

```bash
git add crates/tx-reactor/src/hart/drive.rs crates/tx-reactor/src/hart/mod.rs \
  crates/tx-reactor/src/runtime.rs crates/tx-reactor/src/hart_loop.rs \
  crates/tx-reactor/tests/hart_loop.rs crates/tx-reactor/tests/reactor_smoke.rs \
  crates/tx-kernel/src/init.rs
git commit -m "refactor(reactor): unify hart polling through one driver"
```

## Task 10: Extract Wait, Timer, Coordination, Adapter, and Observability

**Files:**
- Create: `crates/tx-reactor/src/wait/{mod,registration,channel,future,timeout}.rs`
- Create: `crates/tx-reactor/src/timer/{mod,domain,route}.rs`
- Create: `crates/tx-reactor/src/coord/{mod,completion,sync,delegate}.rs`
- Create: `crates/tx-reactor/src/adapter/{mod,bus_wire,step_engine}.rs`
- Create: `crates/tx-reactor/src/observability.rs`
- Modify: `crates/tx-reactor/src/lib.rs`
- Modify: affected Reactor integration tests
- Delete: old single-file modules after moves

- [ ] **Step 1: Split wait by maintained state**

Move prepared registration and stale-wake guards to `registration`, channel
state to `channel`, Future implementations/outcomes to `future`, and deadline
adapters to `timeout`. Preserve public root re-exports.

- [ ] **Step 2: Move timer integration without changing `tx-time` ownership**

Move `ReactorTimerDomain`, driver claim, deadline-change state, and routes into
`timer`. Continue extracting due keys before callbacks and routing expiry
through ordinary mailbox/wake paths.

- [ ] **Step 3: Move reusable coordination protocols**

Move completion counter, sync token/ack, and delegate reply/abandonment code to
`coord`. Keep counter/state truth separate from wake channels.

- [ ] **Step 4: Split adapters and observability**

Adapters translate substrate/step types only. Move debug event emitters and
snapshots to `observability`; do not let metrics determine task state.

- [ ] **Step 5: Run focused module suites**

```bash
cargo test -p tx-reactor --test wait_bus --test wait_interrupt
cargo test -p tx-reactor --test timer_idle --test v3_pr7b_timer_routing --test v3_timer_surface
cargo test -p tx-reactor --test completion --test sync_coord
cargo xtask lint arch
cargo xtask lint unused
```

Expected: all moved public surfaces remain available and architecture/unused
checks report no new findings.

- [ ] **Step 6: Commit the responsibility split**

Stage only the moved Reactor modules and their affected tests, then commit:

```bash
git commit -m "refactor(reactor): group wait timer and coordination modules"
```

## Task 11: Correct and Localize Userspace Rendezvous

**Files:**
- Create: `crates/tx-reactor/src/userspace/{mod,rendezvous,compat}.rs`
- Modify: `crates/tx-reactor/src/lib.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/tests/userspace_run.rs`
- Modify: `crates/tx-kernel/src/thread_future.rs`
- Modify: `crates/tx-kernel/src/trap_handoff.rs`
- Modify: `crates/tx-kernel/src/thread_future/tests.rs`
- Modify: `crates/tx-subsystems/src/thread_runtime/structure.rs`
- Modify: `crates/tx-subsystems/src/thread_runtime/tests.rs`
- Delete: `crates/tx-reactor/src/userspace.rs`

- [ ] **Step 1: Rewrite tests to the approved state machine**

Replace tests expecting timer preemption to resolve the wait with tests for:

```rust
#[test]
fn timer_preemption_does_not_resolve_or_wake_userspace_wait() {
    let slot = UserspaceRunSlot::new();
    let mut wait = Box::pin(slot.start_request().expect("request"));
    let request = wait.request();
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);

    assert!(wait.as_mut().poll(&mut cx).is_pending());
    slot.dispatch(request).expect("running request");
    let status = slot
        .record_timer_preemption(request)
        .expect("record preemption");

    assert_eq!(status.phase, UserspaceRunPhase::Running);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert!(wait.as_mut().poll(&mut cx).is_pending());
}

#[test]
fn cancel_resolves_wait_with_cancelled_and_wakes_once() {
    let slot = UserspaceRunSlot::new();
    let mut wait = Box::pin(slot.start_request().expect("request"));
    let request = wait.request();
    let wakes = Arc::new(AtomicUsize::new(0));
    let waker = counting_waker(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&waker);

    assert!(wait.as_mut().poll(&mut cx).is_pending());
    slot.cancel(request).expect("cancel request");
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(
        wait.as_mut().poll(&mut cx),
        Poll::Ready(UserspaceRunResult::Cancelled)
    );
}

#[test]
fn duplicate_dispatch_is_rejected() {
    let slot = UserspaceRunSlot::new();
    let wait = slot.start_request().expect("request");
    let request = wait.request();

    slot.dispatch(request).expect("first dispatch");
    assert!(matches!(
        slot.dispatch(request),
        Err(UserspaceRunError::InvalidPhase {
            expected: UserspaceRunPhase::Pending,
            actual: UserspaceRunPhase::Running,
            ..
        })
    ));
}

#[test]
fn interesting_trap_requires_running_request() {
    let slot = UserspaceRunSlot::new();
    let wait = slot.start_request().expect("request");
    let request = wait.request();

    assert!(matches!(
        slot.complete_interesting_trap(request, syscall_trap(63)),
        Err(UserspaceRunError::InvalidPhase {
            expected: UserspaceRunPhase::Running,
            actual: UserspaceRunPhase::Pending,
            ..
        })
    ));
}
```

Use a `UserspaceRunResult` output so cancellation is represented explicitly:

```rust
pub enum UserspaceRunResult {
    Trap(UserspaceTrapInfo),
    Cancelled,
}
```

- [ ] **Step 2: Run tests and verify the red state**

```bash
cargo test -p tx-reactor --test userspace_run -- --nocapture
```

Expected: timer, cancellation, duplicate dispatch, and pending-completion tests
fail against the old slot behavior.

- [ ] **Step 3: Implement strict rendezvous transitions**

Implement:

```rust
pub enum UserspaceRunError {
    NoActiveRequest,
    Busy(UserspaceRunStatus),
    StaleRequest {
        attempted: UserspaceRunRequest,
        active: UserspaceRunRequest,
    },
    InvalidPhase {
        request: UserspaceRunRequest,
        expected: UserspaceRunPhase,
        actual: UserspaceRunPhase,
    },
    RequestIdExhausted,
}

enum ActivePhase {
    Pending,
    Running,
    Resolved(UserspaceRunResult),
}
```

- `dispatch`: only `Pending -> Running`.
- `complete_interesting_trap`: only `Running -> Resolved(Trap)`.
- `cancel`: `Pending|Running -> Resolved(Cancelled)` and wake once.
- `record_timer_preemption`: validate request is Running, increment diagnostic
  count, leave phase Running, and do not take/wake the waiter.

- [ ] **Step 4: Make active request publication one ThreadRuntime protocol**

Replace independent slot/token calls with methods such as:

```rust
pub fn begin_userspace_run(&self) -> Result<UserspaceRunWait, UserspaceRunError>;
pub fn resolve_userspace_trap(
    &self,
    request: UserspaceRunRequest,
    trap: UserspaceTrapInfo,
) -> Result<(), UserspaceRunError>;
pub fn cancel_userspace_run(&self) -> Result<(), UserspaceRunError>;
```

Each method updates the slot and `active_request` under one documented
ThreadRuntime protocol so exceptional exits cannot leave a stale token.

- [ ] **Step 5: Update `run_thread` and trap handoff**

Handle `UserspaceRunResult::Cancelled` as thread teardown/interruption policy
owned by ThreadRuntime. Timer expiry records the hart preemption marker and
returns to the hart driver without resolving the userspace wait solely for
slice expiry.

- [ ] **Step 6: Move the implementation and retain a temporary compat module**

Move slot types to `userspace/rendezvous.rs`. Keep global Reactor facade calls
inside `userspace/compat.rs` only while migrating the two compatibility tests.

- [ ] **Step 7: Run userspace and thread-runtime tests**

```bash
cargo test -p tx-reactor --test userspace_run -- --nocapture
cargo test -p tx-kernel thread_future -- --nocapture
cargo test -p tx-kernel trap_handoff -- --nocapture
cargo test -p tx-subsystems thread_runtime -- --nocapture
```

Expected: strict generation transitions, cancellation wake, timer transparency,
and live per-thread trap round-trip tests pass.

- [ ] **Step 8: Commit userspace convergence**

Stage only the listed Reactor, kernel, and ThreadRuntime files, then commit:

```bash
git commit -m "refactor(reactor): make userspace rendezvous thread owned"
```

## Task 12: Retire Compatibility Paths and Stabilize the Public Facade

**Files:**
- Modify: `crates/tx-reactor/src/lib.rs`
- Modify: `crates/tx-reactor/src/runtime.rs` or its replacement facade
- Modify: workspace consumers found by `rg`
- Delete: `crates/tx-reactor/src/userspace/compat.rs`
- Delete: `crates/tx-reactor/src/mailbox.rs`
- Delete: `crates/tx-reactor/src/wait_source.rs`
- Test: all Reactor integration tests

- [ ] **Step 1: Prove compatibility consumers are gone**

Run:

```bash
rg -n "tx_reactor::(mailbox|wait_source)::|request_userspace_run|dispatch_userspace_run|record_userspace_timer_preemption|complete_userspace_run|compat_locals" crates boards xtask
```

Expected: only the compatibility definitions/tests remain. Migrate each listed
consumer before deletion.

- [ ] **Step 2: Point root re-exports directly at owning modules**

Keep stable root names for mailbox/wait types while deleting forwarding source
files:

```rust
pub use adapter::bus_wire::{PreparedWaitRegistration, WaitRegistrationGuard};
pub use tx_substrate::wake::{TaskMailbox, WaitSource};
```

Use the exact existing root export set rather than widening it.

- [ ] **Step 3: Delete global userspace facade and compatibility tests**

All live and test callers use a per-thread or directly constructed
`UserspaceRunSlot`. Remove `ReactorShared.userspace` and its request/dispatch/
complete/checkpoint forwarding methods.

- [ ] **Step 4: Remove legacy TaskId submission**

Run `rg -n "\.submit\(" crates boards` and classify Reactor submissions by
receiver type. Migrate every `Reactor::submit` consumer to `submit_task ->
TaskKey`, update variables and registries to retain `TaskKey`, then delete the
legacy `Reactor::submit -> TaskId` method. Non-Reactor methods named `submit`
are unchanged.

- [ ] **Step 5: Run the whole Reactor package**

```bash
cargo test -p tx-reactor
cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo xtask lint arch
cargo xtask lint unused
```

Expected: Reactor package tests pass, RV64 kernel target checks, and no retired
compatibility path is imported.

- [ ] **Step 6: Commit compatibility retirement**

```bash
git add crates/tx-reactor crates/tx-kernel crates/tx-subsystems
git commit -m "refactor(reactor): retire duplicate runtime compatibility paths"
```

Before committing, inspect `git diff --cached --name-only` and unstage any
unrelated dirty-tree path.

## Task 13: Add Transition Observability and Final Correctness Gates

**Files:**
- Modify: `crates/tx-reactor/src/observability.rs`
- Modify: focused Reactor tests
- Modify: `docs/progress/STATUS.md`
- Create/modify: `docs/progress/plans/2026-07-14-reactor-refactor.json`

- [ ] **Step 1: Add typed transition counters**

Record, without affecting decisions:

```text
poll_lease_acquired
poll_lease_rejected_stale
poll_commit_parked
poll_commit_requeued_wake
wake_ingress_enqueued
wake_ingress_coalesced
run_token_stale_generation
run_token_stale_epoch
cancel_deferred_to_lease
hart_drive_reentry_rejected
userspace_stale_request
```

- [ ] **Step 2: Assert metrics do not change behavior**

Run the same scripted task sequence with observability enabled and disabled;
compare final task snapshots, poll counts, queue depths, and terminal records.

- [ ] **Step 3: Run the full local gate ladder**

```bash
cargo fmt --check
cargo test -p tx-reactor
cargo test -p tx-kernel thread_future -- --nocapture
cargo test -p tx-kernel trap_handoff -- --nocapture
cargo test -p tx-subsystems thread_runtime -- --nocapture
cargo xtask lint arch
cargo xtask lint unused
cargo xtask lint docs
git diff --check
```

Expected: every command exits 0. If unrelated dirty-tree failures remain,
record the exact command and failure rather than weakening a gate.

- [ ] **Step 4: Run target and SMP witnesses**

```bash
cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo xtask full-build --target rv64-qemu
cargo xtask test smoke --target rv64-qemu
```

Expected: target check and full build pass; QEMU smoke contains the boot and SMP
owner-wake sentinels required by the active Reactor/SMP docs.

- [ ] **Step 5: Verify structural acceptance criteria**

```bash
wc -l crates/tx-reactor/src/**/*.rs crates/tx-reactor/src/*.rs
rg -n "compat_locals|request_userspace_run|record_userspace_timer_preemption" crates/tx-reactor/src crates/tx-reactor/tests
rg -n "VecDeque<TaskId>|TaskHandle::new\(TaskId" crates/tx-reactor/src
```

Expected: no authored source file exceeds 1,500 lines; retired compatibility
symbols and raw-TaskId physical queues are absent.

- [ ] **Step 6: Complete progress catch-up**

Update the plan JSON and `STATUS.md` with changed surfaces, exact verification,
next performance-measurement action, and blockers. Run:

```bash
cargo xtask progress validate
```

Expected: all progress records validate. Do not modify unrelated progress JSON
to hide a pre-existing schema error; report it separately if still present.

- [ ] **Step 7: Commit observability and completion records**

```bash
git add crates/tx-reactor/src/observability.rs crates/tx-reactor/tests \
  docs/progress/STATUS.md docs/progress/plans/2026-07-14-reactor-refactor.json
git commit -m "observe(reactor): pin refactor transition invariants"
```

## Final Review Checklist

- [ ] One `PollLease` acquisition and commit implementation serves every
  facade.
- [ ] `TaskControl` is the only authority for lifecycle and execution owner.
- [ ] Physical runqueues contain `TaskKey + QueueEpoch` tokens.
- [ ] Wake ingress inserts once per unobserved wake batch.
- [ ] Cancellation cannot recycle a slot with an active lease.
- [ ] Scheduler policy owns no physical queue or compatibility locals.
- [ ] `ReactorShared` contains no userspace slot.
- [ ] Timer preemption does not resolve a userspace wait solely for slice
  expiry.
- [ ] Forwarding mailbox/wait-source modules and global userspace facade are
  gone.
- [ ] All Reactor source files are at or below 1,500 lines.
- [ ] Focused, package, target, docs/arch/unused, progress, and QEMU gates have
  fresh recorded evidence.
