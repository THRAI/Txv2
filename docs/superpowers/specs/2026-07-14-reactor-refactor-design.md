# Reactor Refactor Design

**Status:** approved design, implementation pending (2026-07-14)

## Purpose

Refactor `tx-reactor` into a correctness-first runtime whose in-memory state,
directory topology, lock ownership, and public interfaces follow one execution
model. The refactor must remove duplicate poll/commit paths, make task lifecycle
and scheduler ownership transitions coherent, separate scheduler policy from
reactor mechanism, and retire compatibility state that is no longer used by the
live kernel path.

This design is intentionally staged. The first implementation keeps the current
generation-checked `Vec<TaskSlot>` storage and the global wake ingress. A
segmented arena or per-hart wake sharding may follow only after measurements
show that allocation, table locking, or wake-ingress contention is material.

## Current Problems

The current reactor has four structural problems.

1. `runtime.rs` combines shared state, per-hart execution, wake routing, task
   polling, timer integration, userspace compatibility APIs, observability, and
   public facades. `scheduler.rs` combines policy, shared metadata, local
   queues, balancing, and compatibility-owned locals.
2. The production concurrent hart loop and the host-oriented
   `HartRuntimeView::run_until_idle*` path duplicate the take, poll, and commit
   algorithm. Timing and result reporting differ, but task state transitions
   should not.
3. Task lifecycle and scheduler execution ownership are stored under separate
   locks and updated through separate APIs. A wake can make a task runnable and
   queued after a pending poll commits `TaskStatus::Parked`, then the old poller
   can overwrite scheduler ownership back to `Parked`. The resulting runnable
   task has no valid queue owner.
4. Live userspace execution is per thread through
   `ThreadPayload.userspace_slot`, while `ReactorShared` still owns a global
   single-slot userspace facade. These are two representations of the same
   protocol with different ownership.

The current wake path also appends a task id for every wake even when the wake
bit is already set. Scheduler queues contain `TaskId` rather than a
generation-bearing token, so stale physical queue entries are not fully
isolated from slot reuse.

## Goals

- One authoritative task execution state per task.
- One poll acquisition and commit algorithm for all runtime facades.
- No reactor or scheduler lock held while polling a Future.
- Generation- and epoch-checked physical queue entries.
- Transition-sensitive wake ingress with bounded duplicate traffic.
- Explicit global, per-hart, and per-thread ownership boundaries.
- Scheduler policy that makes decisions without owning runqueues.
- Focused modules below the repository's 1,500-line authored-source limit.
- Compatibility exports that are narrow, named, and removable.
- Interfaces covering submission, wake, polling, cancellation, terminal drain,
  timer routing, AST handling, userspace rendezvous, and observability.

## Non-Goals

- Replacing reactor task storage with `Zone<Task>`.
- Adding semantic identity, capabilities, weak identity references, or EBR
  reclamation to temporal reactor tasks.
- Introducing a lock-free scheduler or lock-free task table in this refactor.
- Designing a new fair scheduler, deadline scheduler, or scheduler class model.
- Changing `TaskMailbox`, `WaitSource`, or `TimerEngine` ownership in their
  source crates.
- Moving POSIX thread, signal, VM fault, or syscall policy into `tx-reactor`.
- Sharding wait-source subscriber vectors without fanout measurements.
- Changing HAL trap-frame or return-to-user contracts.

## Architectural Basis

The module split follows four rules.

### Authoritative ownership

Each state has one owner. Task execution truth belongs to `core`; scheduling
policy belongs to `scheduler`; physical execution state belongs to `hart`;
wake coalescing and delivery belong to `wake`; a userspace request belongs to
the corresponding `ThreadPayload` instance.

### Policy versus mechanism

The scheduler chooses a target hart, queue class, slice, and migration policy.
It does not push or pop a runqueue. Reactor hart code owns the queues, applies
placement decisions, and sends reschedule IPIs.

### State-machine linearization

State that must change coherently is colocated in `TaskControl`. Physical
queues and wake inboxes are derived indexes. Their entries are validated
against authoritative state before use and may be discarded when stale.

### Dependency direction

The hart driver orchestrates lower-level components. Core task code does not
depend on timer, userspace, HAL, or semantic subsystem policy. Adapters translate
external vocabulary but own no runtime state.

## Ownership Model

| Concern | Authoritative owner | Does not own |
|---|---|---|
| Future and task lifecycle | `core::TaskControl` | placement policy, semantic thread state |
| Execution owner and poll lease | `core::TaskControl` | physical queue storage |
| Affinity, budget, priority | `scheduler::TaskSchedMeta` | Future, runqueue |
| Runnable queue | `hart::HartRunQueue` | task lifecycle truth |
| Wake sequence and ingress latch | `wake::TaskWakeState` | mailbox events, scheduler policy |
| Mailbox events | `tx_substrate::TaskMailbox` | runnable membership |
| Deadline order | `tx_time::TimerEngine` | expiry routing |
| Timer routes and drive | `timer::ReactorTimerDomain` | semantic timer state |
| Userspace request rendezvous | per-thread `UserspaceRunSlot` | thread registers and signal policy |
| AST markers | task-local reactor state | signal selection policy |
| Completion and token/ack protocols | `coord` | task execution ownership |

## Target Directory Topology

```text
crates/tx-reactor/src/
  lib.rs

  core/
    mod.rs
    key.rs             # TaskId, TaskGeneration, TaskKey, RunToken
    task.rs            # TaskCell and immutable task-owned attachments
    arena.rs           # generation-checked Vec slots and terminal recycling
    transition.rs      # authoritative lifecycle/owner transitions
    poll.rs            # PollLease, PollResult, PollCommit

  scheduler/
    mod.rs
    policy.rs          # policy trait and decision vocabulary
    metadata.rs        # affinity, priority, budget, placement history
    placement.rs       # submit/wake/stop placement decisions
    balance.rs         # pure victim and migration decisions

  hart/
    mod.rs
    local.rs           # HartReactorLocal and local execution state
    run_queue.rs       # kernel/boosted/new/preempted physical queues
    drive.rs           # sole hart execution loop
    dispatch.rs        # apply placement, markers, and reschedule IPI

  wake/
    mod.rs
    state.rs           # TaskWakeState wake sequence and inbox latch
    ingress.rs         # generation-bearing WakeToken queue
    route.rs           # parked/running wake transition and placement

  wait/
    mod.rs
    registration.rs    # prepared wait registration and stale-wake guards
    channel.rs         # reactor channel/readiness wrappers
    future.rs          # wait futures and outcome vocabulary
    timeout.rs         # deadline registration adapter

  timer/
    mod.rs
    domain.rs          # ReactorTimerDomain instance and due drive
    route.rs           # timer key to reactor-owned delivery target

  userspace/
    mod.rs
    rendezvous.rs      # UserspaceRunSlot protocol type
    compat.rs          # temporary global facade for migration tests only

  coord/
    mod.rs
    completion.rs      # completion counter plus wake channel
    sync.rs            # token/ack synchronous coordination
    delegate.rs        # delegate reply and abandonment integration

  adapter/
    mod.rs
    bus_wire.rs
    step_engine.rs

  observability.rs
  interrupt.rs
  yield_now.rs
  spin_lock.rs
```

Runqueues belong to `hart`, not `scheduler`. `scheduler` returns decisions;
`hart::dispatch` mutates local queues. This preserves the active contract that
the scheduler is policy consulted by the reactor rather than an entity-owning
or queue-owning subsystem.

## In-Memory Model

```text
Reactor
├── ReactorShared
│   ├── TaskArena
│   │   └── Vec<TaskSlot>
│   │       └── TaskCell
│   │           ├── TaskKey
│   │           ├── TaskControl lock
│   │           ├── Arc<TaskWakeState>
│   │           ├── Arc<TaskMailbox>
│   │           └── AST state
│   ├── SchedulerMetadata
│   ├── WakeIngress
│   ├── ReactorTimerDomain
│   ├── DelegateRegistry
│   └── Observability
└── ReactorLocals
    └── HartReactorLocal[hart]
        ├── HartRunQueue
        ├── preemption markers
        ├── current-slice and idle state
        └── per-hart statistics
```

`ReactorShared` contains cross-hart truth and shared services.
`HartReactorLocal` contains state used only to execute work on one hart. A
`UserspaceRunSlot` is not a field of either structure after compatibility
retirement; it is stored in `ThreadPayload`.

## Task Identity and Storage

The first implementation retains:

```rust
pub struct TaskKey {
    id: TaskId,
    generation: TaskGeneration,
}

struct TaskArena {
    slots: Vec<TaskSlot>,
    free: Vec<TaskId>,
}
```

The task Future remains `Pin<Box<dyn Future<Output = ()> + Send>>`. Moving a
`TaskSlot` during `Vec` growth moves the box pointer, not the pinned Future
allocation. Generation protects slot reuse. Terminal recycling is synchronous
and does not require EBR because no identity-shaped task reference survives
after generation invalidation and active lease release.

Every physical scheduler entry carries the full temporal identity:

```rust
pub struct RunToken {
    key: TaskKey,
    queue_epoch: u32,
    queue: QueueClass,
}
```

`queue_epoch` changes whenever a task receives a new runnable placement. A
queue pop succeeds only when key, generation, epoch, queue class, target hart,
and authoritative owner all match.

## TaskControl

`TaskControl` contains state that must transition coherently:

```rust
struct TaskControl {
    future: Option<TaskFuture>,
    lifecycle: TaskStatus,
    owner: TaskRunOwner,
    lease_epoch: u32,
    queue_epoch: u32,
    observed_wake_seq: u64,
    cancel_requested: bool,
    last_stop_reason: Option<StopReason>,
}
```

Legal stable combinations are:

| Lifecycle | Owner | Meaning |
|---|---|---|
| `Runnable` | `Queued { hart, queue, epoch }` | one current placement exists |
| `Polling` | `Polling { hart, lease_epoch }` | one poll lease owns the Future |
| `Parked` | `Parked` | no runnable placement exists |
| `Completed` | `Terminal` | Future completed and awaits drain/recycle |
| `Cancelled` | `Terminal` | cancellation committed and awaits drain/recycle |

No public or crate-private API may independently set lifecycle or owner. All
changes go through `core::transition`.

## PollLease Protocol

The executor exposes one poll transaction:

```rust
fn acquire_poll(
    &self,
    hart: HartId,
    token: RunToken,
) -> Result<PollLease, AcquireError>;

fn commit_poll(
    &self,
    lease: PollLease,
    result: PollResult,
) -> Result<CommitAction, CommitError>;
```

Acquisition performs the following under the task-control lock:

1. Validate `TaskKey`, generation, queue epoch, owner hart, and queue class.
2. Verify that no active poll lease exists.
3. Increment `lease_epoch`.
4. Move the Future out of `TaskControl`.
5. Set `(lifecycle, owner)` to matching `Polling` states.
6. Capture the current wake sequence.

The lock is released before `Future::poll`. Task-local mailbox, hart, AST, and
other poll context are installed through an RAII `PollContextGuard` and are
always restored when the poll ends or unwinds in host tests.

Commit reacquires the task-control lock and validates the lease epoch and
generation. It then commits one result:

- `Ready`: set `Completed + Terminal`, drop the Future, publish terminal drain.
- `Pending` with cancellation requested: set `Cancelled + Terminal`, drop the
  Future, publish terminal drain.
- `Pending` with a wake since acquisition: restore the Future, update scheduler
  accounting, create one new queue epoch and placement.
- `Pending` without a wake: restore the Future and set `Parked + Parked`.
- explicit yield or slice expiry: restore the Future and create one new
  placement according to the scheduler decision.

A wake after the final wake-sequence observation still enqueues a `WakeToken`.
If commit has made the task parked, wake routing transitions it back to
runnable. A wake observed before commit causes commit itself to requeue the
task. Therefore neither ordering loses the wake.

## Wake Protocol

`TaskWakeState` stores a task key, a monotonic wake sequence, and an ingress
latch:

```rust
struct TaskWakeState {
    key: TaskKey,
    wake_seq: AtomicU64,
    ingress_queued: AtomicBool,
    ingress: Weak<WakeIngress>,
}
```

`wake()` increments `wake_seq` and inserts one `WakeToken { key }` only on the
`ingress_queued: false -> true` transition. Repeated wakes before observation
do not add physical queue entries.

When the router drains a token it records the processed wake sequence, performs
the authoritative task transition, clears `ingress_queued`, and rereads the
wake sequence. If another wake arrived during drain, it reacquires the latch
and requeues one token. This closes the clear-versus-wake race without allowing
an unbounded duplicate wake queue.

Wake routing handles task state as follows:

| State observed | Action |
|---|---|
| `Parked` | transition to runnable, ask policy for placement, enqueue |
| `Polling` | leave ownership unchanged; poll commit observes wake sequence |
| `Runnable/Queued` | update wake hint if stronger; do not duplicate placement |
| terminal or stale generation | discard token |

The initial implementation keeps one shared `WakeIngress`. Per-hart ingress is
deferred until contention metrics justify the added owner/migration protocol.

## Scheduler Boundary

Scheduler APIs are decision-shaped and do not mutate runqueues:

```rust
trait SchedulerPolicy {
    fn place_submit(&self, task: TaskKey, meta: &TaskSchedMeta) -> Placement;
    fn place_wake(
        &self,
        task: TaskKey,
        meta: &TaskSchedMeta,
        hint: WakeHint,
        current: OwnerSnapshot,
    ) -> Placement;
    fn account_stop(
        &self,
        task: TaskKey,
        meta: &mut TaskSchedMeta,
        stop: StopObservation,
    ) -> StopDecision;
}
```

`Placement` names a target hart, queue class, front/back insertion, slice, and
whether remote rescheduling is required. `hart::dispatch` applies it to the
target local queue and sends an IPI when necessary.

Scheduler metadata no longer stores the authoritative execution owner. It may
retain a last-hart hint and migration history, but those values cannot validate
queue membership or poll ownership.

## Hart Execution

`hart::drive` is the only implementation of the execution loop. Production,
host tests, and compatibility facades call the same primitive:

```rust
fn drive_hart<S, C>(
    &self,
    hart: HartId,
    budget: PollBudget,
    clock: &mut C,
    signal: &mut S,
) -> HartLoopStep
where
    C: SliceClock,
    S: RescheduleSignal;
```

The loop performs:

1. Drive due timers and route their outputs.
2. Drain wake ingress and apply placements.
3. Consume local preemption and reschedule markers.
4. Pop one generation-bearing `RunToken`.
5. Acquire a poll lease; discard invalid stale tokens.
6. Poll without reactor locks.
7. Account timing and commit through the single poll transaction.
8. Repeat until budget exhaustion or idle.
9. Restore the current-hart hardware deadline and return `HartLoopStep`.

`run_until_idle`, `run_until_idle_on_hart`, and the concurrent boot loop become
thin parameter presets over `drive_hart`. They contain no task transitions.

Each hart has a CAS-protected drive guard. Reentrant execution of the same hart
returns an explicit `HartBusy` or no-progress step rather than overwriting
task-local poll context.

## Locking Rules

The design uses narrow locks and validated derived indexes.

1. Never hold a reactor lock while polling a Future or invoking semantic
   callbacks.
2. Pop from a hart queue under the queue lock, release it, then validate the
   token under the task-control lock.
3. Never acquire scheduler metadata while holding a hart queue lock.
4. Task transitions may read scheduler decisions, but queue mutation is
   returned as an action and applied after the task-control lock is released.
5. Timer algorithms return due keys before timer delivery callbacks run.
6. Mailbox publication runs before owner routing; mailbox locks are not held
   while task control, scheduler metadata, or hart queues are locked.
7. Terminal slot recycling requires terminal state and no active poll lease.

The intended lock order is therefore mostly non-nested. Where a lock pair
cannot be avoided, the allowed order must be documented in the owning module
and covered by a debug lock-order assertion.

## Cancellation and Terminal Drain

Cancellation is a two-phase transition.

- Parked or queued task: mark cancelled, invalidate the queue epoch, remove the
  Future, and publish a terminal record. Any physical queue token becomes
  stale.
- Polling task: set `cancel_requested`; the active lease remains valid until
  commit. Commit drops the Future and publishes cancellation.
- Terminal task: cancellation is idempotent or returns `AlreadyTerminal`.

`drain_completed` and `drain_cancelled` remove terminal records, clear the
slot, and add the id to the free list only after no active lease exists. Slot
reuse increments generation before constructing the next task.

## Shared and Local State

`ReactorShared` owns:

- task arena and task-control locks;
- scheduler policy metadata;
- global wake ingress;
- timer domain and route table;
- delegate registry;
- aggregate observability.

`HartReactorLocal` owns:

- physical runnable queues;
- current-slice and idle state;
- preemption and reschedule markers;
- drive reentry guard;
- per-hart execution statistics.

Cross-hart wake is:

```text
shared task transition
  -> scheduler placement decision
  -> target HartRunQueue
  -> target need_resched marker
  -> remote IPI when target != current
  -> target hart validates RunToken and polls
```

The unused local wake-inbox path is removed during compatibility retirement so
only one wake-routing model remains. If later measurements justify per-hart
wake ingress, that work introduces a new generation-bearing implementation and
its migration protocol rather than reviving the compatibility inbox.

## Userspace Rendezvous

`UserspaceRunSlot` remains a reactor mechanism type but is owned per thread:

```text
run_thread
  -> ThreadPayload.userspace_slot.start_request()
  -> publish active request token
  -> enter userspace
  -> trap shell resolves current ThreadPayload
  -> trap_handoff validates token and completes the same slot
  -> slot wakes run_thread Future
  -> next poll handles trap and prepares return to userspace
```

The rendezvous state machine is:

```text
Idle -> Pending -> Running -> Resolved -> Idle
          \-----------> Cancelled -> Idle
                      ^
                      |
                  from Running
```

Rules:

- Only the current request generation may dispatch or resolve the slot.
- Dispatch requires `Pending`; duplicate `Running -> Running` is rejected.
- Interesting trap completion requires `Running`.
- Cancellation stores a terminal result and wakes the waiter.
- Timer preemption records a hart preemption marker and returns control to the
  hart driver without resolving the userspace rendezvous or advancing the
  `run_thread` Future solely because its slice expired.
- Slot state and the thread's active token are published and cleared through
  one ThreadRuntime-owned protocol.

`ReactorShared.userspace` and the global request/dispatch/complete facade move
to `userspace::compat` for test migration and are then deleted. The live
per-thread path does not migrate through the global slot.

## Wait, Timer, and Coordination Boundaries

`wait` owns task suspension mechanics and stale-wake protection. It does not
own semantic readiness. After wake, the Future re-observes the semantic object
or signal state.

`timer` owns the concrete reactor timer domain and delivery routes. Deadline
ordering remains in `tx-time`; expiry routing occurs outside the timer-engine
lock and enters the ordinary mailbox/wake path.

`coord` contains reusable protocols built on wait/wake:

- completion counter plus wake notification;
- targeted token/ack synchronous coordination;
- delegate reply, timeout, and abandonment integration.

These protocols do not participate in task ownership transitions except by
posting through the ordinary wake interface.

## Adapter and Observability Rules

`adapter` is the only reactor module that translates substrate bus/wake or
step-engine vocabulary. It does not own queues, task state, timer state, or
compatibility runtime instances.

Observability receives typed transition events after the authoritative state
change. Metrics and traces must not become scheduling truth. Required events
include:

- poll lease acquire/reject/commit;
- stale run token and stale wake token drops;
- wake coalesced/enqueued/rerouted;
- queue placement and remote IPI;
- cancel requested/committed;
- hart drive reentry rejection;
- userspace rendezvous generation mismatch and cancellation.

## Public Interface Shape

The crate facade exposes role-shaped APIs rather than internal storage:

```rust
pub trait ReactorSubmit {
    fn submit<F>(&self, future: F, meta: InitialSchedMeta) -> TaskKey;
    fn cancel(&self, task: TaskKey) -> Result<CancelOutcome, TaskError>;
}

pub trait ReactorDrive {
    fn drive_hart<S, C>(
        &self,
        hart: HartId,
        budget: PollBudget,
        clock: &mut C,
        signal: &mut S,
    ) -> HartLoopStep;
}

pub trait ReactorInspect {
    fn task_snapshot(&self, task: TaskKey) -> Option<TaskSnapshot>;
    fn scheduler_snapshot(&self) -> SchedulerSnapshot;
    fn hart_snapshot(&self, hart: HartId) -> Option<HartSnapshot>;
}
```

Mailbox, wait-source, timer registrar, and userspace rendezvous types may retain
root re-exports during migration. Internal module paths are not compatibility
contracts.

## Compatibility Retirement

The following paths are temporary:

- scheduler-owned `compat_locals` and queue-mutating legacy scheduler APIs;
- duplicate host `run_until_idle*` implementation;
- `mailbox.rs` and `wait_source.rs` back-compat modules;
- global `ReactorShared.userspace` and its facade;
- unused local wake inbox as a parallel delivery route;
- queue entries containing only `TaskId`.

Compatibility removal requires first migrating tests and workspace consumers
to the new role-shaped APIs. Root re-exports may remain for one migration phase
even after the forwarding source module is deleted.

## Migration Plan

### Phase 0: correctness witnesses

Add deterministic or model-based tests for:

- pending commit racing with wake routing;
- repeated wake coalescing and clear-versus-wake races;
- cancellation while a poll lease is active;
- stale run token after slot reuse;
- duplicate queue entries and queue-epoch validation;
- same-hart drive reentry;
- userspace cancel, duplicate dispatch, and stale trap generation.

Repair any current compile drift in the affected focused tests before using
them as a gate.

### Phase 1: authoritative transition core

Introduce `TaskControl`, `RunToken`, queue and lease epochs, transition-sensitive
wake ingress, and two-phase cancellation in the existing file layout. Route all
task lifecycle changes through the new transition API.

### Phase 2: behavior-preserving module extraction

Move code into `core`, `scheduler`, `hart`, `wake`, `wait`, `timer`, `userspace`,
`coord`, and `adapter`. Preserve public exports. Do not combine this mechanical
move with scheduler algorithm changes.

### Phase 3: single hart driver

Create `hart::drive::drive_hart` and make production and test facades invoke it.
Remove duplicate take/poll/commit code. Inject time, budget, and reschedule
behavior through explicit parameters.

### Phase 4: compatibility retirement

Migrate scheduler tests to explicit `HartReactorLocal`, remove
`compat_locals`, delete forwarding modules whose workspace consumers are gone,
and retire the global userspace slot after all tests use per-thread ownership.

### Phase 5: measured storage optimization

Add metrics for task-table lock wait/service time, task count, slot growth,
wake-ingress queue depth, duplicate wake ratio, stale-token drops, runqueue
contention, and remote wake rate. Only then choose among:

- retaining `Vec<TaskSlot>`;
- a reactor-private segmented generational arena;
- per-hart wake-ingress shards;
- per-hart task-table partitions.

None of these options introduces Zone semantics. Phase 5 is a separate design
decision and is not required to complete Phases 0 through 4.

## Reference Set

This design is grounded in the following active contracts and current-tree
audits:

- `docs/design/02_execution/REACTOR_v0.md`: reactor mechanism, task handles,
  AST, wake routing, timer-preemption transparency, and userspace boundary.
- `docs/design/02_execution/SCHEDULER_v0.md`: scheduler policy boundary,
  temporal task handles, and reactor-owned runqueues.
- `docs/Txv3/10_SCHED_SMP_v1.md`: owner validation, migration, wake, and
  cross-hart scheduling requirements.
- `docs/design/02_execution/THREAD_RUNTIME_v1.md`: per-thread userspace state,
  trap handoff, and return-to-user ownership.
- `docs/design/00_meta-framework/object_model_v2.md` and
  `docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md`: semantic entity versus
  temporal-handle boundary.
- `docs/progress/research/2026-07-13-reactor-architecture-survey.md` and
  `docs/progress/research/2026-07-13-reactor-fanout-readers-audit.md`: current
  poll, wake, mailbox, fanout, timer, and SMP implementation findings.
- Current implementation anchors in `crates/tx-reactor/src/{task,runtime,
  scheduler,userspace}.rs`, `crates/tx-substrate/src/wake/`,
  `crates/tx-kernel/src/{thread_future,trap_handoff}.rs`, and
  `crates/tx-subsystems/src/thread_runtime/`.

## Verification Matrix

| Area | Required witness |
|---|---|
| Lifecycle | submit, park, wake, yield, complete, cancel, drain, reuse |
| Poll correctness | one active lease, no lock during poll, wake-before/after commit |
| Token correctness | stale generation and stale queue epoch are rejected |
| Wake | transition-sensitive insertion, no lost wake, bounded duplicate traffic |
| SMP | remote placement, IPI, migration race, steal validation, same-hart guard |
| Scheduler | policy decisions independent from queue ownership |
| Timer | due batch outside lock, task route uses ordinary wake path |
| Userspace | per-thread slot, stale trap, cancel wake, timer-preempt transparency |
| Compatibility | old facade tests migrated before deletion |
| Observability | transition counters do not alter behavior |

Implementation gates, run narrowly before broadly:

```text
cargo fmt --check
cargo test -p tx-reactor <focused-test>
cargo test -p tx-reactor
cargo test -p tx-kernel <thread-runtime-focused-filter>
cargo xtask lint arch
cargo xtask lint unused
cargo xtask lint docs
cargo xtask progress validate
git diff --check
```

When runtime, trap, or remote-hart behavior changes, add the relevant RV64
target check and QEMU SMP owner-wake witness before claiming completion.

## Acceptance Criteria

The refactor is complete only when:

1. There is one task poll/commit implementation.
2. Lifecycle and execution owner have one transition authority.
3. Every runqueue token carries task generation and queue epoch.
4. Wake ingress is transition-sensitive and the wake/commit race is covered.
5. Cancellation cannot recycle a slot while its Future is being polled.
6. Scheduler policy owns no physical runqueue.
7. Shared, per-hart, and per-thread state are represented by different owner
   types with no global userspace slot in the live path.
8. Compatibility locals and duplicate runtime facades are removed.
9. No authored Reactor source file exceeds 1,500 lines.
10. Focused concurrency tests, package tests, architecture/docs lints, progress
    validation, and the required target/QEMU witnesses pass.
