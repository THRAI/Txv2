# SMP Scheduler And PELT Design

**Status:** approved in design dialogue on 2026-08-04; written-spec review
pending

**Goal:** turn the existing AP/reactor SMP bring-up into production-safe
multi-hart userspace scheduling, then improve placement and scalability with
per-hart ownership, idle-first pull stealing, PELT-style load estimation, IPI
coalescing, and measured task/wake sharding.

## Decision Summary

The target scheduler uses:

- one authoritative task execution state in reactor `TaskControl`;
- per-hart reactor-owned runqueues;
- scheduler-owned affinity, budget, wake-class, and PELT metadata colocated
  with the stable task slot but not duplicated as execution truth;
- static submit spreading before movable userspace is enabled;
- idle-first pull stealing from the cold end of `Preempted` only;
- event-driven PELT-lite for placement and victim choice, first in shadow mode;
- generation- and epoch-bearing physical queue/wake tokens;
- one coalesced Reschedule IPI per outstanding remote scheduling batch;
- a staged path from legacy pinned execution through static spreading,
  movable userspace, active PELT, and measured storage sharding.

Periodic push balancing, NUMA policy, RT classes, CFS/EEVDF ordering, cgroup
CPU control, CPU-frequency capacity scaling, and PMU-driven accounting are not
part of this design.

## Architectural Basis And Precedence

This design is a performance and SMP extension of:

- `docs/superpowers/specs/2026-07-14-reactor-refactor-design.md`;
- `docs/Txv3/10_SCHED_SMP_v1.md`;
- `docs/design/02_execution/REACTOR_v0.md`;
- `docs/design/02_execution/SCHEDULER_v0.md`;
- `docs/design/02_execution/reactor_scheduling.md`;
- `docs/design/02_execution/THREAD_RUNTIME_v1.md`.

The reactor-refactor design remains authoritative where it says that
`TaskControl` owns task lifecycle, execution owner, queue epoch, and poll
lease. The `TaskSchedCell` name used during design review denotes the
scheduler-owned sidecar colocated with that control state; it does not create
a second lifecycle or owner.

This design refines two parts of the earlier reactor-refactor design:

1. SMP wake, pick, steal, and forced migration may use the documented lock
   order `hart shard(s) -> TaskControl`. The earlier mostly-non-nested rule
   remains the default for all other reactor operations.
2. The earlier measured-storage phase remains conditional, but this design
   fixes the selected target shape if measurements justify promotion: a
   segmented generation-checked directory, per-task control locks, and
   per-hart wake ingress.

Before implementation changes behavior, the implementation plan must update
the canonical active docs where wording conflicts with this design. In
particular, `reactor_scheduling.md` must describe the handoff insertion point
as the local/hot end rather than a physical `front`, because the SMP contract
uses LIFO-local and FIFO-steal.

## Current Baseline And Blocking Gaps

The 2026-08-04 readiness audit established the current baseline:

- APs boot, enter reactor loops, receive IPIs, and execute synthetic reactor
  work.
- Host scheduler/reactor tests and RV64 `-smp 4` smoke and busybox boot pass.
- The guest smoke report remains `steal=0:rebalance=0` and does not prove
  userspace load distribution.
- a Reschedule IPI is acknowledged without making trap return reschedule the
  interrupted userspace task;
- steal removes queue membership and publishes owner metadata in separate
  critical sections;
- steal currently examines `Boosted`, `New`, and `Preempted` and removes from
  the wrong end relative to the active v3 contract;
- production userspace tasks remain submit-hart pinned;
- scheduler metadata, the reactor task table, and raw wake ingress retain
  shared locks that can become scalability bottlenecks after true userspace
  parallelism is enabled.

The design therefore treats correctness and userspace migration safety as
promotion prerequisites rather than performance follow-ups.

## Goals

- No lost wake, duplicate valid run token, or simultaneous polling of one
  task.
- Secondary harts run normal userspace tasks under static placement first and
  movable placement later.
- Cross-hart wake has bounded latency and cannot resume userspace directly
  after merely acknowledging a Reschedule IPI.
- `sched_setaffinity` and cpuset-style changes cannot execute a task on a
  disallowed hart after the migration safe point.
- Local scheduling keeps cache-hot work local; steal removes the oldest
  eligible preempted work.
- Load estimation does not add periodic task scans, allocations, or global
  locks.
- PELT failure or stale estimates affect placement quality only, never task
  correctness.
- Each rollout stage has a boot-time fallback and a focused evidence gate.

## Non-Goals

- Moving reactor tasks into Zone/Cap/Weak/EBR semantics.
- Moving the Future allocation when scheduler ownership migrates.
- Migrating a task during an active userspace machine round trip.
- Replacing the VM pmap-tail plan or claiming scheduler work fixes VM/TLB
  tails.
- A runtime hot switch from movable scheduling back to static scheduling.
- A periodic push balancer without evidence that idle-first pull stealing is
  insufficient.
- Exact Linux scheduler internals. PELT decay semantics are borrowed, but Tx
  retains its reactor/scheduler ownership split and queue policy.

## Ownership Model

| State | Authoritative owner | Physical/materialized form |
|---|---|---|
| Future, lifecycle, execution owner, poll lease | reactor `TaskControl` | stable task slot |
| Affinity, class, budget, wake class, task PELT | scheduler sidecar under task control | stable task slot |
| Runnable order | reactor `HartRunShard` | generation/epoch-bearing run tokens |
| Mailbox events and wake hint | `TaskMailbox` | bounded event/hint state |
| Wake coalescing and ingress membership | `TaskWakeState` | per-hart MPSC wake node |
| Per-hart utilization estimate | `HartRunShard` | PELT state plus atomic snapshots |
| Userspace registers and active request | `ThreadPayload` | per-hart trap slot plus run token |
| ASID residency | VM/pmap | hart residency mask governed by shootdown |

The scheduler returns placement, queue, slice, and migration decisions. It
does not send an IPI, poll a Future, enter userspace, or own a semantic thread.
The reactor applies decisions and owns physical queue mutation.

## Target Data Model

```rust
struct TaskSlot {
    generation: AtomicU32,
    current_hart: AtomicU16,
    lifecycle_hint: AtomicU8,
    control: SpinMutex<TaskControl>,
    mailbox: Arc<TaskMailbox>,
    wake: Arc<TaskWakeState>,
}

struct TaskControl {
    future: Option<TaskFuture>,
    lifecycle: TaskStatus,
    owner: TaskRunOwner,
    lease_epoch: u32,
    queue_epoch: u32,
    observed_wake_seq: u64,
    cancel_requested: bool,
    active_userspace: Option<UserspaceRunToken>,
    sched: TaskSchedState,
}

struct TaskSchedState {
    class: SchedClass,
    affinity: u64,
    migration: MigrationPolicy,
    flags: TaskSchedFlags,
    queue: Option<QueueKind>,
    remaining_budget_ns: u64,
    total_runtime_ns: u64,
    queued_turn: u64,
    wake_class: WakeClass,
    pelt: TaskPelt,
}

struct RunToken {
    key: TaskKey,
    queue_epoch: u32,
    queue: QueueKind,
}

struct HartRunShard {
    queues: SpinMutex<HartRunState>,
    wake_ingress: HartWakeIngress,
    need_resched: AtomicBool,
    userspace_preempt: AtomicBool,
    ipi_armed: AtomicBool,
    polling_idle: AtomicBool,
    util_snapshot: AtomicU64,
    runnable_snapshot: AtomicU64,
    nr_running_snapshot: AtomicU32,
}

struct HartRunState {
    kernel: VecDeque<RunToken>,
    boosted: VecDeque<RunToken>,
    new: VecDeque<RunToken>,
    preempted: VecDeque<RunToken>,
    nr_running: u32,
    pelt: HartPelt,
    next_victim: HartId,
    boosted_streak: u8,
    aged_streak: u8,
}
```

`current_hart` is the lock-free routing and migration linearization field. Its
Release updates occur while the required shard and task-control locks are
held. `TaskControl.owner` is the full authoritative execution state and must
match `current_hart` whenever the task-control lock is held. The lifecycle
atomic is an outside-lock diagnostic hint only.

Runqueues may contain stale physical tokens after cancellation or generation
invalidation, but at most one token can match the task's current generation,
queue epoch, owner hart, and queue class. Only that token counts as runnable
membership. A stale token can never acquire a poll lease.

## Core Invariants

1. One task has one authoritative execution owner and at most one active poll
   lease.
2. One task has at most one valid run token. Duplicate stale tokens are
   discardable mechanical debris, not runnable membership.
3. A cross-hart wake rechecks `current_hart` while holding the selected owner
   shard and task-control lock before creating or upgrading a placement.
4. A movable userspace task is stealable only after its previous userspace
   round trip has fully unwound and its old per-hart trap slots are clear.
5. Kernel-class tasks are pinned and unstealable.
6. `New`, `Boosted`, and `Kernel` queues are unstealable. Only `Preempted` is
   exposed to steal.
7. Local preempted dispatch is LIFO from the local/hot end. Steal is FIFO from
   the remote/cold end.
8. PELT snapshots, queue-depth snapshots, and owner hints are advisory. Stale
   values cannot override affinity, generation, owner, lifecycle, or poll
   lease validation.
9. No queue, task-control, scheduler, mailbox, or userspace-slot lock is held
   while polling a Future, sending an IPI, entering userspace, or yielding.
10. No RangeLock, epoch guard, sidecar guard, queue guard, or task-control
    guard crosses a `StepOp` yield.

## Locking Protocol

The only allowed nested scheduler/reactor order is:

```text
mailbox publication completes and releases its lock
    -> HartRunShard locks in ascending HartId order
        -> TaskControl lock
```

Scheduler policy runs on immutable snapshots while the owning task-control
lock is held or after copying the required values. It does not acquire another
reactor lock.

- Same-hart wake/pick uses one shard, then one task-control lock.
- Steal uses victim shard, then candidate task control. It never blocks on a
  victim; `try_lock` failure moves to the next victim.
- Queued forced migration locks source and destination in ascending HartId
  order, then task control.
- Running forced migration changes an atomic request flag and reschedules the
  owner; the actual move occurs at stop commit.
- IPI send and wake forwarding occur after every lock is released.
- Wake uses `try_lock` for both the target shard and task control. Failure of
  either lock releases any lock already acquired for that attempt. After at
  most `WAKE_LOCK_RETRY_LIMIT = 32` failed attempts, wake publishes to the
  target wake ingress.

Debug builds must include lock-order assertions for the allowed nested cases.

## Task State Machine

```text
submit:                 New -> Runnable/Queued
local pick:             Runnable/Queued -> Polling
direct steal:           Runnable/Queued(Preempted, victim) -> Polling(thief)
poll Pending + no wake: Polling -> Parked
poll Pending + wake:    Polling -> Runnable/Queued
yield or preempt:       Polling -> Runnable/Queued(Preempted)
wake:                   Parked -> Runnable/Queued
queued affinity move:   Runnable/Queued(src) -> Runnable/Queued(dst)
running affinity move:  Polling(src) -> Runnable/Queued(dst) at commit
complete/cancel:        any live state -> Terminal
```

Queue pop and poll acquisition are one bounded transaction under shard then
task-control lock. A token that fails generation, queue epoch, owner, queue,
affinity, or lifecycle validation is removed as stale and never polled.

## Wake Algorithm

Mailbox publication happens before mechanical scheduling. Wake hints latch
monotonically within one batch:

```text
Normal < WakeHandoff < LifecycleWake < PriorityBoost < SignalDelivery
```

The fast path is:

```text
post_wake(task, hint, producer_hart):
    publish mailbox event and stronger hint

    loop:
        target = current_hart.load(Acquire)
        try_lock target shard
        try_lock TaskControl
        on either contention: release acquired locks and retry
        on repeated contention: enqueue one ingress token and arm target
        if current_hart changed: unlock and retry
        apply state transition or queue upgrade
        unlock all
        arm_reschedule(target, producer_hart)
        return
```

State handling under lock is:

| State | Wake action |
|---|---|
| `Parked` | Create one new queue epoch and enqueue once. |
| `Runnable/Queued` | Do not duplicate. Upgrade queue placement only if the new hint is stronger. |
| `Polling` | Leave the owner unchanged. Poll commit observes the wake sequence. |
| terminal/stale | Discard the mechanical wake and count it. |

Queue selection is:

| Condition | Queue/action |
|---|---|
| kernel task | `Kernel` |
| lifecycle, priority, or signal wake | `Boosted` |
| `WakeHandoff` with remaining budget | `Preempted` local/hot end with one-shot latency marker |
| ordinary wake with remaining budget | `Preempted` local/hot end |
| no remaining budget | `New` |

## Per-Hart Wake Ingress

The contended fallback and final sharded raw-waker path use one single-consumer
MPSC ingress per hart. `TaskWakeState` is the intrusive node and contains the
full generation-bearing key, monotonic wake sequence, ingress latch, routing
hint, and queue link.

```rust
struct TaskWakeState {
    key: TaskKey,
    wake_seq: AtomicU64,
    ingress_queued: AtomicBool,
    owner_hint: AtomicU16,
    next: AtomicPtr<TaskWakeState>,
}
```

The queue uses the standard intrusive MPSC exchange-and-link shape with a
per-hart stub node and one consumer. It does not use a Treiber CAS stack and
therefore does not depend on an untagged-pointer ABA assumption. On the
`ingress_queued: false -> true` transition, ingress takes one `Arc` strong
reference to the wake state; the consumer releases it after processing.

The consumer reads and detaches the next pointer before clearing the ingress
latch. It then rereads `wake_seq`; if a new wake raced with latch clearing, it
requeues one node. Owner changes cause forwarding based on a fresh
`current_hart` read. `owner_hint` is only a cache.

Task recycling invalidates generation before reuse. A stale Waker can publish
only its old `TaskKey`; generation validation prevents it from affecting the
new task in that slot.

## Reschedule And IPI Coalescing

```text
arm_reschedule(target, producer):
    target.need_resched.store(true, Release)

    if target == producer:
        return

    paired ordering barrier

    if target.polling_idle.load(Acquire):
        return

    if target.ipi_armed.compare_exchange(false, true) succeeds:
        send Reschedule IPI after dropping all locks
```

The Reschedule IPI handler performs only bounded trap-context work:

```text
ack IPI
ipi_armed.store(false, Release)
need_resched.store(true, Release)
return TrapAction::Reschedule
```

Trap-return arbitration consumes `need_resched`. An interrupted userspace task
snapshots context and unwinds to its reactor future. A kernel task observes the
marker at the next cooperative `StepOp` boundary.

The idle handshake distinguishes active polling from the final sleep window:

```text
polling_idle.store(true, Release)
paired ordering barrier

if need_resched || wake ingress nonempty || local valid work exists:
    polling_idle.store(false, Release)
    return to scheduler loop

polling_idle.store(false, Release)
paired ordering barrier

if need_resched || wake ingress nonempty || local valid work exists:
    return to scheduler loop

pause_for_ipi/WFI
```

While `polling_idle` is true, the target is still executing the polling
protocol and a producer may suppress the IPI because the first or second
recheck must observe its publication. Before the target can execute WFI it
clears `polling_idle` and rechecks after the paired barrier. A producer that
races after that recheck observes false and sends one coalesced interrupt.
This closes the check-to-WFI missed-wakeup window.

## Queue Direction And Local Selection

Physical endpoints are named by purpose:

```text
front = steal/old end
back  = local/hot end
```

Local selection order is:

```text
Kernel FIFO
-> Boosted FIFO
-> aged Preempted oldest
-> WakeHandoff-marked Preempted
-> New FIFO
-> Preempted LIFO
```

Normal preempt/yield appends to the preempted local/hot end. Local ordinary
selection removes from that end. Steal removes from the opposite end. New work
is never stolen.

The initial constants are:

```text
BASE_SLICE_NS = 10 ms
NEW_QUEUE_SLICE_NS = 1 ms
AGING_PROMOTION_TURNS = 8
AGED_STREAK_LIMIT = 1
BOOST_BURST_LIMIT = 4
STEAL_SCAN_LIMIT = 8
```

After four Boosted selections, one ready fair task must run. While `New` is
ready, at most one aged or handoff-preempted task runs before one New task.
These limits affect order only; they do not alter affinity, owner, or budget.

Kernel tasks remain strict and cooperative. The class is reserved for bounded
mechanism work with safe points. Unbounded background work must use a fair or
background policy rather than relying on the Kernel queue.

## Budget Accounting

- A New task receives the 1 ms probe slice.
- If it blocks early, unused budget follows it through sleep and wake.
- If it consumes the probe, its next preempted run receives a full 10 ms
  budget.
- Slice expiry exhausts the current budget; the next fair turn starts a new
  base slice.
- Remote reschedule, migration, and steal preserve remaining budget.
- Wake hints change queue position but never refill or multiply budget.
- A handoff marker is consumed after one dispatch.

PELT does not choose a slice or priority. Adaptive slices are a later,
independent shadow experiment after movable SMP is accepted. Its candidate
formula is `clamp(target_latency / local_fair_nr_running, MIN_SLICE,
BASE_SLICE)`, and it cannot be promoted in the same change as movable SMP.

## PELT-Lite

Tx uses event-driven fixed-point PELT-style decay:

```text
period = 1.024 ms
half_life = 32 periods
capacity = 1024 for every hart in this version

avg' = avg * decay(dt) + signal * (1 - decay(dt))
```

Decay factors use a Q32 lookup. Every complete group of 32 periods halves the
old contribution; partial periods are carried forward. No floating point,
allocation, periodic task scan, or global lock is allowed.

Task signals are:

| Task state | util signal | runnable signal |
|---|---:|---:|
| `Polling` | 1024 | 1024 |
| `Runnable/Queued` | 0 | 1024 |
| `Parked` or terminal | 0 | 0 |

`TaskPelt` integrates the table above at each task-state transition. For
`HartPelt`, the util input is 1024 while that hart is executing a task and 0
otherwise. Its runnable input is the saturating sum of 1024 for the executing
task, if any, plus 1024 for every authoritative queued task on that hart.
Consequently runnable average may exceed one hart's capacity and represent a
backlog; stale physical queue tokens never contribute to the signal.

`TaskPelt` follows the task. `HartPelt` independently integrates the local
running and runnable signals; migration updates source and destination signals
but does not transfer a historical hart average. Published per-hart snapshots
are advisory and use saturating arithmetic. A regressing clock or invalid
sample keeps the last valid estimate and forces the current decision to use
queue depth.

Hart pressure is:

```text
pressure(h) = util_avg(h) + max(0, runnable_avg(h) - capacity(h))
```

The runnable term distinguishes a saturated hart with a backlog from a
saturated hart with one running task.

## Initial Placement

```text
allowed = affinity & online_harts
if allowed is empty: reject with EINVAL

best = allowed hart with minimum
       (pressure, nr_running, round_robin_distance)

if preferred hart is allowed,
   preferred.pressure <= best.pressure + capacity/8,
   and preferred.nr_running <= best.nr_running + 1:
    choose preferred
else if parent hart is allowed,
        parent.pressure <= best.pressure + capacity/8,
        and parent.nr_running <= best.nr_running + 1:
    choose parent
else:
    choose best
```

`nr_running` changes immediately on enqueue and prevents a submission burst
from waiting for PELT convergence. Parent preference preserves cache and clone
locality only within the explicit pressure and runnable margins.

In static mode the selected hart becomes `PinnedAfterPlacement`. In movable
mode it is the initial owner only.

Static mode is an internal acceptance stage, not a Linux-affinity-complete
production mode. Before S3 migration safety passes, an affinity update that
would exclude the pinned owner returns `ENOSYS` through the not-installed
migration seam and leaves the old mask unchanged. A narrowing that still
contains the owner is allowed. The default compatibility mode cannot move from
legacy to static merely on the basis of S2 evidence.

## Idle-First Direct Steal

Steal runs only after all local queues and the local wake ingress are empty.

```text
try_steal(thief):
    order online victims by
        (runnable pressure, nr_running, round-robin distance) descending

    for victim in that order:
        skip if preempted depth is zero
        try_lock victim; on contention continue

        inspect at most STEAL_SCAN_LIMIT tokens from Preempted.front
        validate generation, epoch, owner, lifecycle, affinity,
                 migration mode, and userspace migration-safe state

        on success:
            remove token
            update victim signal/accounting
            update TaskControl.owner to Polling(thief)
            current_hart.store(thief, Release)
            acquire a PollLease owned by thief
            release locks
            poll directly without enqueueing on thief

        restore rejected valid tokens in their original order
```

PELT selects the victim, not the candidate ordering. With PELT disabled,
unsettled, or invalid, victim ordering falls back to preempted depth plus the
per-hart round-robin cursor. A failed estimate can select a suboptimal victim
but cannot make an ineligible task stealable.

No periodic push balancer runs in this design. It requires a separate design
and evidence of sustained non-idle imbalance that idle-first pull cannot fix.

## Userspace Migration Safety

Static mode enables userspace on APs without movement after initial placement:

```text
affinity = online_harts
target = placement policy
migration = PinnedAfterPlacement
```

Movable mode adds a generation-bearing run token for each machine userspace
round trip:

```rust
struct UserspaceRunToken {
    task: TaskKey,
    run_seq: u64,
    hart: HartId,
}
```

Before entering userspace, the runtime:

1. validates `current_hart`, affinity, generation, and poll lease;
2. installs matching identity, payload, and run token in the local hart slot;
3. activates the pmap and records ASID residency for that hart;
4. creates the hart-local kernel resume context;
5. marks the task not migration-safe;
6. enters userspace.

Before a user task becomes stealable after a syscall, fault, timer preemption,
or Reschedule IPI, the runtime:

1. saves registers into `ThreadPayload`;
2. returns fully to the reactor/thread future;
3. consumes and closes the active userspace token;
4. clears the old hart identity, payload, and token slots;
5. proves the old kernel resume context is inactive;
6. marks the task migration-safe;
7. only then publishes a valid Preempted token.

Timer preemption cannot preserve the old userspace slot in movable mode. ASID
residency, however, is not cleared merely because the task stopped running;
VM shootdown and ASID reuse remain responsible for clearing that history.

Trap handoff requires exact TaskKey generation, run sequence, and hart match.
A mismatch cannot return to userspace.

## Affinity And Forced Migration

`sched_setaffinity` first computes `effective = requested & online_harts`. An
empty result is `EINVAL`. Once the new mask is published:

| State | Action when current hart is disallowed |
|---|---|
| Parked | Lock old/destination shards in order, recheck, update owner hart. |
| Runnable/Queued | Lock source/destination in order, remove current valid token, update owner, enqueue destination token. |
| Polling | Set `MUST_MIGRATE`, reschedule owner, and move during poll commit. |
| terminal/stale | Return `ESRCH`. |

The destination is the allowed online hart with minimum PELT pressure, using
the queue-depth fallback when required. Forced affinity always overrides
locality and PELT preference.

A thread that changes its own mask to exclude the current hart cannot return
to userspace there. The userspace-entry checkpoint sees `MUST_MIGRATE` or an
affinity mismatch and returns preserve-and-reschedule. Internal lock
contention uses bounded retry/yield and is not exposed as `EBUSY`.

Race closure is:

- wake first: it creates/upgrades source placement; forced migration then
  moves that placement;
- migration first: wake rechecks owner under the old shard and retries;
- steal first: the task becomes Polling on thief, then forced migration marks
  thief for stop-boundary movement;
- forced migration first: steal cannot acquire the source shard and selects a
  different victim.

## Stable Task Storage And Measured Sharding

The behavior work does not require moving a Future with hart ownership. The
first correctness stages may retain the current generation-checked arena while
introducing authoritative `TaskControl` and valid `RunToken` semantics.

If task-table lock measurements justify S5, storage becomes:

```rust
struct TaskDirectory {
    segments: [AtomicPtr<TaskSegment>; MAX_SEGMENTS],
    alloc_shards: [SpinMutex<FreeList>; ALLOC_SHARDS],
}
```

Segments are allocated only on submit, published once, and never moved.
Lookup is lock-free after the segment pointer load and generation check.
Future ownership remains protected by the per-task control lock. Slot
allocation and recycling use sharded slow-path free lists.

Poll acquisition moves the Future out under task-control lock and releases the
lock before polling. Poll commit validates generation and lease epoch before
restoring or dropping it. Terminal recycling requires:

- terminal authoritative state;
- no valid run token or active poll lease;
- no active userspace run token;
- wake ingress no longer owns a token for that generation;
- Future and completion/drain state released.

Old Wakers remain harmless because they carry the old generation.

## Rollout Modes

Behavior is immutable after boot:

```text
tx.sched.smp  = legacy | static | movable
tx.sched.load = depth | pelt-shadow | pelt
```

Shared versus segmented storage is initially a compile-time build choice
because it changes layout. There is no live global downgrade from movable to
static mode.

| Stage | Change | Promotion evidence |
|---|---|---|
| S0 Legacy | Current pinned behavior. | Preserved fallback baseline. |
| S1 Correctness | Authoritative transitions, trap reschedule, queue/owner linearization, IPI coalescing, wake fallback. | Race/model tests, SMP4 smoke, zero lost wake or double poll. |
| S2 Static | AP userspace plus placement shadow; pinned after initial placement. | Four CPU tasks spread across four harts; AP trap and exit clean. |
| S3 Movable | Run-token cleanup, direct steal, affinity movement. | Load-skew, pipe wake, timer-preempt migration, affinity tests. |
| S4 PELT | PELT placement/victim decisions become active. | At least five interleaved A/B pairs; pressure-imbalance area falls by at least 20%, median throughput is at least 98% of depth mode, and wake p99 is at most 105% of depth mode. |
| S5 Sharded | Segmented task directory and all raw wakers route directly to per-hart ingress. | At least five interleaved A/B pairs; targeted shared-lock total wait falls by at least 30%, median throughput is at least 98% of S4, and correctness receipts are identical. |

PELT shadow records the legacy and candidate choices for the same event. It
does not alter placement until the S4 promotion decision.

The per-hart ingress introduced in S1 is the correctness fallback for a
contended direct wake. Before S5, compatibility raw Wakers may still enter the
shared ingress and be routed from there. S5 removes that shared hot path only
after its lock metrics satisfy the promotion gate.

## Fail-Closed Rules

| Condition | Required action |
|---|---|
| PELT time regression, overflow, or invalid state | Saturate where possible and use queue-depth decision for the event. |
| Owner changes during a locked recheck | Release and reroute. |
| Wake shard remains contended after 32 attempts | Publish one per-hart ingress token. |
| Stale generation/queue/lease token | Discard and count. |
| Empty effective affinity | Return `EINVAL`. |
| Internal migration lock contention | Retry/yield without exposing an internal errno. |
| Userspace task/run-seq/hart mismatch | Do not `sret`; panic in debug, terminate the affected task with durable diagnostics in release. |
| Duplicate valid run token or simultaneous poll lease | Kernel invariant failure; do not continue polling. |

Automatic runtime mode downgrade is forbidden because it cannot define
ownership for already migrated tasks. Operational rollback selects the prior
mode at the next boot.

## Observability

Emit paths must use fixed-size records, remain bounded, allocate nothing, and
support sampling. Required stable families are:

```text
debug.sched.hart.util_avg
debug.sched.hart.runnable_avg
debug.sched.runqueue.depth
debug.sched.placement.choice
debug.sched.pelt.shadow_disagree

debug.sched.wake.route
debug.sched.wake.duplicate
debug.sched.wake_to_dispatch_ns

debug.sched.ipi.sent
debug.sched.ipi.coalesced
debug.sched.ipi.suppressed_idle
debug.sched.ipi_to_dispatch_ns

debug.sched.steal.attempt
debug.sched.steal.success
debug.sched.steal.reject_reason
debug.sched.migration.reason
debug.sched.migration.duration_ns

debug.sched.token.stale
debug.sched.owner.retry
debug.lock.sched.*
debug.lock.task_table.*
debug.lock.runqueue.*
```

The required report includes per-hart runtime/utilization, queue-depth time
series, wake source/current/target/hint, wake-to-dispatch percentiles, IPI
sent/coalesced/suppressed counts, steal attempts/success/rejections, migration
reason/latency, PELT shadow disagreement, and lock wait/service percentiles.

Score and metrics runs are separate. A trace is promotion evidence only when
its `runtime.json` reports complete capture with zero loss, overwrite, and
framing error.

## Verification Matrix

### Host and model tests

- Reschedule IPI returns scheduler arbitration rather than Resume.
- Wake before/after park commit cannot be lost.
- Wake racing steal cannot duplicate a valid run token.
- Wake racing queued forced migration routes to exactly one owner.
- Affinity racing direct steal migrates at the thief stop boundary.
- Duplicate wake ingress coalesces and survives latch-clear races.
- Local LIFO and remote FIFO queue directions are exact.
- Boost and aging burst limits prevent fair-lane starvation.
- PELT fixed-point vectors cover zero, one period, 32 periods, long decay,
  saturation, and regressing time.
- Stale generation, queue epoch, lease epoch, and userspace run sequence are
  rejected.
- Terminal recycle cannot occur with an active lease or wake-ingress owner.

### RV64 QEMU SMP witnesses

1. Four independent CPU-bound userspace tasks: compare SMP1 and SMP4 total
   throughput and per-hart runtime.
2. Load skew with four tasks initially on hart 0: at least two harts execute
   within 10 ms and all four within 100 ms.
3. Cross-hart pipe ping-pong: no lost wake; report wake-to-dispatch p50/p99.
4. Repeated `sched_setaffinity` movement: userspace executes only on allowed
   harts after each migration boundary.
5. Repeated timer-preempt A/B migration: an old per-hart slot never consumes a
   later trap.
6. pthread create/join: report clone handoff, futex wake, remote IPI, scheduler
   migration, and total time.
7. Mixed CPU and blocking I/O: no fair-task starvation and no material
   throughput collapse.
8. Wake/steal/affinity stress: no duplicate valid token, double poll, owner
   mismatch, or generation confusion.

Every guest log is fault-decoded before promotion. Performance comparisons use
the same parent/candidate pair, image, hart count, workload loop count, and
controlled host load. Existing results from a different image or metrics mode
are context, not matched A/B evidence.

## Implementation Dependency Order

This is the dependency order for the later implementation plan, not permission
to implement before written-spec review:

1. Align canonical docs and introduce authoritative TaskControl/RunToken
   transitions without changing placement behavior.
2. Repair Reschedule IPI trap-return arbitration and idle/IPI handshake.
3. Implement lock-and-recheck wake and queue/owner linearization.
4. Align local/steal queue direction and restrict stealable scope.
5. Enable static AP userspace placement behind boot mode.
6. Add userspace run-token cleanup and migration-safe proofs.
7. Enable direct steal and forced affinity migration behind movable mode.
8. Add PELT fixed-point accounting and shadow receipts, then consider active
   promotion.
9. Measure shared locks; implement segmented storage/per-hart raw wake only if
   S5 gates justify it.
10. Run the full acceptance matrix and update active docs/progress evidence.

## Scope Boundary With VM And pthread Tails

The scheduler exposes wake, IPI, migration, lock, and userspace-runtime
correlation needed by pthread analysis. It does not absorb the independent VM
pmap materialization, resident-index, range-shootdown, or ASID-reuse work.
Those remain governed by the active VM pmap-tail plan and its crash/cold-fault
gates. Scheduler and VM candidate runs must be attributed separately before a
combined benchmark is interpreted.

## Completion Criteria

This design is implemented only when:

- S1 through S3 correctness and guest witnesses pass;
- default userspace mode is deliberately selected based on those receipts;
- PELT active mode, if promoted, beats or matches the queue-depth baseline on
  the declared balance/throughput/tail metrics;
- any S5 sharding is justified by before/after lock evidence;
- active docs describe the shipped queue endpoints, wake protocol, task
  authority, migration-safe boundary, and rollback mode;
- progress records name the exact commands, artifacts, next step, and any
  remaining blocker.
