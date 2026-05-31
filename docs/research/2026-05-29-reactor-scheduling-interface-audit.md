---
date: 2026-05-29
topic: "Reactor interfaces for explicit scheduling and publish acknowledgement"
status: draft
scope:
  - crates/tx-reactor
  - crates/tx-substrate/src/wake
  - crates/tx-kernel/src/thread_future.rs
  - crates/tx-kernel/src/init/reactor_submit.rs
  - docs/design/02_execution
  - docs/Txv3/10_SCHED_SMP_v1.md
---

# Research: Reactor Interfaces For Explicit Scheduling And Publish Acknowledgement

## Question

What reactor interfaces do we have now, and are they enough to support the
pthread lifecycle scheduling model we have been discussing: wake-class
placement, bounded handoff, child-publish acknowledgement, and later
fair/RT-style scheduling contexts?

## Short Answer

The current reactor interfaces are enough for the Phase 1 wake-class scheduler
that now exists in code: ordinary readiness wakes, futex wake handoff hints,
lifecycle wakes, signal delivery, boosted queues, aged preempted work,
same-hart userspace preempt markers, direct child submission, and per-hart
wake/IPI dispatch.

They are not enough for the next scheduling model. The missing surface is not
another wake hint. The missing surface is a first-class publish-ack and bounded
handoff interface that can prove "the cloned child task is published and
visible to the scheduler" without polling the child far enough to enter
userspace, run a syscall, or consume a full preempted userspace slice. The
current clone handoff uses `yield_now`, which creates a broad poll boundary; it
does not express the narrower acknowledgement we need.

For longer-term scheduler work, the current interfaces also lack virtual-time
accounting, explicit scheduling contexts or donation, deadline/latency policy
inputs, and a real pluggable policy boundary. Those are not needed to fix the
immediate pthread long tail, but they matter if Tx wants a Fuchsia-like fair
scheduler, a Linux EEVDF-like fairness model, or seL4-MCS-style scheduling
contexts rather than a tuned Phase 1 queue policy.

## Reading Set

Active docs and local source used:

- `docs/design/02_execution/REACTOR_v0.md`
- `docs/design/02_execution/SCHEDULER_v0.md`
- `docs/design/02_execution/THREAD_RUNTIME_v1.md`
- `docs/design/02_execution/reactor_scheduling.md`
- `docs/Txv3/10_SCHED_SMP_v1.md`
- `crates/tx-reactor/src/{runtime.rs,scheduler.rs,userspace.rs,preempt.rs,yield_now.rs}`
- `crates/tx-substrate/src/wake/{mailbox.rs,wait_source.rs}`
- `crates/tx-kernel/src/{thread_future.rs,init/reactor_submit.rs}`
- `crates/tx-subsystems/src/{thread_runtime/structure.rs,futex/mod.rs}`

External comparison anchors:

- Fuchsia/Zircon scheduler documentation:
  <https://fuchsia.dev/fuchsia-src/concepts/kernel/kernel_scheduling>
- Linux EEVDF scheduler documentation:
  <https://docs.kernel.org/scheduler/sched-eevdf.html>
- seL4 MCS tutorial and manual landing pages:
  <https://docs.sel4.systems/Tutorials/mcs.html>
  and <https://docs.sel4.systems/projects/sel4/manual.html>
- Managarm source tree for coroutine-kernel comparison:
  <https://github.com/managarm/managarm>

## Current Interface Inventory

### Reactor task submission

Current code exposes `Reactor::submit_task_with_meta` and
`Reactor::submit_task_with_meta_from_hart`. The latter returns
`(TaskKey, WakeDispatchReport)`, and it observes the chosen `TaskRunOwner` to
send a local marker or remote IPI if the initial placement is not the current
hart.

This is enough to publish a child task directly into the live reactor. The
clone path already uses it through `CoreInit::submit_child_thread_now`, and
submits the child with `userspace_thread_sched_meta_for(...).preempted_on_submit()`.
That keeps cloned children out of the `New` queue.

The gap is that the report is wake-dispatch oriented, not publish-ack oriented.
It does not return a structured fact such as:

```rust
struct TaskPublishReport {
    task: TaskKey,
    target_hart: HartId,
    queue: Phase1QueueKind,
    queued_turn: u64,
    dispatch: WakeDispatchReport,
}
```

That missing report matters because clone currently proves publication by
causing a later scheduler boundary. The interface should let the caller prove
queue visibility immediately after submit, without needing the child to run.

### Scheduler metadata and queues

`InitialSchedMeta` currently carries class, nice, RT priority, affinity,
kernel/userspace classification, migration policy, spread-on-submit, and
`preempted_on_submit`. `Phase1QueueKind` has `Kernel`, `Boosted`, `New`, and
`Preempted`. `TaskRunOwner` records `Parked`, `Queued { hart, queue }`,
`Polling { hart }`, or `Terminal`.

The implemented pick order matches the new scheduling draft:

```text
Kernel -> Boosted -> aged Preempted -> New -> Preempted
```

`AGING_PROMOTION_TURNS` exists and is currently `8`. `New` tasks receive a
short 1 ms slice, while preempted-like queues use the remaining task budget or
a 10 ms preempted slice.

This is enough to encode the current Phase 1 policy. It is not enough for a
modern fair scheduler model because there is no virtual runtime, virtual
deadline, eligible time, lag, weight-derived slice, latency target, or policy
specific field in the implemented `TaskSchedMeta`. `SCHEDULER_v0` anticipated a
policy-specific extension area, but the code has not grown that boundary yet.

### Wake classes and mailbox hints

`MailboxSchedulerHint` and reactor `WakeHint` now encode:

```text
Normal < WakeHandoff < LifecycleWake < PriorityBoost < SignalDelivery
```

`TaskMailbox::post_with_scheduler_hint` exists. `TaskMailbox::post` maps
default `SourceFired` to `Normal`, and delivered signals to `SignalDelivery`.
`publish_scheduler_hint` latches the strongest hint monotonically until the
reactor drains the batch.

`WaitSource` exposes `notify_with_hint`, `notify_limit_with_hint`,
`notify_emit_with_hint`, and `notify_limit_emit_with_hint`, while the default
notify methods stay `Normal`.

This is enough to prevent the old collapse where all `SourceFired` posts looked
like priority wakes. It also gives futex and lifecycle wake producers the
necessary vocabulary.

### Futex wake producers

`step_futex_wake_in` and `step_futex_wake_masked_in` use
`MailboxSchedulerHint::WakeHandoff`. `step_futex_lifecycle_wake_in` uses
`MailboxSchedulerHint::LifecycleWake`. The unit tests assert that both hints
are latched.

This is enough for the current distinction:

- syscall futex wake: handoff hint, but not boosted;
- clear-child / lifecycle futex wake: boosted lifecycle wake;
- generic wait source wake: normal.

The futex surface is not the missing interface for clone publish-ack. Futex
wakes are about already-registered waiters. Clone publish-ack is about task
publication and scheduler visibility before the child has necessarily executed
a wait or a userspace entry.

### Runtime wake delivery

The runtime converts `MailboxSchedulerHint` to reactor `WakeHint`, drains wake
batches, marks the task runnable, applies local queue insertion, and sends the
reschedule signal when placement is remote. On same-hart wakes with hint at
least `WakeHandoff`, `mark_userspace_preempt_for_wake` marks
`UserspacePreempt`.

This is enough to cover the current futex handoff path. It is also explicitly
separate from `NeedResched`, which avoids consuming a generic dispatch marker
at userspace entry.

The gap is that the runtime only has wake-oriented handoff. There is no
equivalent "publish handoff" channel for a new task submission. A clone child
that has just been submitted is not a mailbox wake event, so representing it as
a wake hint would blur two different protocols.

### Userspace-run slot

`UserspaceRunSlot::start_request` creates the active request,
`dispatch` records userspace dispatch, `complete_interesting_trap` resolves
syscall/fault/fatal traps, and `checkpoint_userspace_entry` handles the
policy-neutral AST checkpoint. `ThreadPayload` stores the slot and the active
request token.

This is the right mechanism boundary for safe userspace entry. The key rule
from `reactor_scheduling.md` is preserved in `thread_future`: do not yield
after publishing an active userspace request and before entering userspace.

There is a doc/code mismatch to keep visible: `REACTOR_v0` says timer
preemption should not resolve the userspace-run future, but the current
`UserspaceRunSlot::record_timer_preemption` resolves the wait with
`UserspaceTrapInfo::TimerPreempt`, and `thread_future` then yields. That is not
the immediate pthread blocker, but it means the current implementation does not
fully match the preemption-transparency prose. Any future publish-ack interface
must avoid making this worse by introducing another unsafe pre-entry yield.

### Thread future clone handoff

`thread_future::run_thread` currently does the following:

1. On successful clone return, `syscall_return_needs_handoff` sets
   `syscall_handoff_pending`.
2. On the next syscall trap, before dispatching that syscall, the future emits
   `debug.thread.clone_handoff.yield` and awaits `yield_now`.
3. `yield_now` wakes the same task and returns `Pending` on first poll, then
   `Ready` on the next poll.

This is semantically valid because it creates a scheduler boundary after clone
publication. It is too broad because it gives the reactor permission to run the
child as an ordinary preempted userspace task. Previous observe runs showed the
cost of this path dominates the hot pthread create/join loop.

The interface gap is therefore precise: `yield_now` means "make a general
cooperative poll boundary." Clone publish needs "acknowledge the child task is
visible to scheduling, maybe give bounded kernel-only bookkeeping credit, but
do not let the child consume a full lifecycle/userspace slice."

### Observability

Current code emits low-level scheduler and submit markers such as
`debug.sched.submit.queue`, `debug.sched.runnable.queue`,
`debug.sched.runnable.front`, `debug.sched.runnable.hint`,
`debug.sched.stop.reason`, `debug.reactor.submit.*`, and
`debug.thread.clone_handoff.yield`.

This is enough to explain the current long tail, but not enough to prove a new
publish-ack contract. The new interface should emit one structured report when
the child is submitted and acknowledged:

- child task id / tid;
- target hart;
- queue kind;
- queued turn;
- whether a local marker or remote IPI was sent;
- whether bounded handoff credit was consumed;
- whether the child reached userspace entry before the parent resumed.

The last field should be a negative assertion in tests: a pure publish-ack
should not require userspace entry.

## Fit Against The Target Scheduling Models

### Current Tx Phase 1 wake-class model

Mostly supported.

Implemented surfaces:

- explicit wake classes;
- default `SourceFired` as `Normal`;
- futex wake as `WakeHandoff`;
- lifecycle wake as `LifecycleWake`;
- boosted queue for lifecycle/priority/signal;
- same-hart userspace preempt marker for handoff-strength wakes;
- aged preempted work before new work;
- direct child submit into the live reactor;
- `preempted_on_submit` for cloned userspace threads.

Missing for pthread lifecycle:

- first-class child publish acknowledgement;
- bounded handoff that does not run userspace;
- task publish report with queue/turn proof;
- tests that prove clone ack does not require child first syscall or exit.

### Managarm-like coroutine kernel shape

Tx has a compatible mechanical split: asynchronous subsystem waits publish to a
wake substrate, tasks are coroutine futures, and the scheduler remains separate
from semantic wait truth. That is the right direction.

The missing part is CPU policy. Managarm-style coroutine scheduling still needs
clear runnable accounting and queue policy. Tx has Phase 1 queues, but not a
first-class CPU fairness model or a bounded continuation handoff model.

### Fuchsia/Zircon-like fair scheduling

Tx has the skeleton pieces: task metadata, per-hart queues, affinity,
preemptive slices, consumed-time accounting, and stop reasons.

Tx lacks the policy inputs and accounting fields needed for the model:

- weight;
- virtual timeline position;
- eligible/deadline or finish time;
- target latency;
- fair-share lag;
- per-task policy object or class-specific accounting.

Adding those should be a later scheduler update. They are not the minimal fix
for pthread clone publish-ack.

### Linux EEVDF-like fairness

Tx does not currently expose enough scheduler state for EEVDF-style behavior.
The scheduler has `total_runtime_ns` and `remaining_budget_ns`, but no virtual
runtime, virtual deadline, eligible test, or lag calculation. Queue order is
class/age based, not virtual-deadline based.

Borrowable idea: once pthread lifecycle is fixed, represent ordinary fair tasks
by eligibility and virtual finish/deadline rather than tuning `New` versus
`Preempted` queue constants indefinitely.

### seL4-MCS-like scheduling contexts

This is the strongest mismatch. seL4 MCS makes CPU time a first-class
scheduling context that can be bound, donated, and returned. Tx currently has
budget fields inside scheduler metadata, but there is no capability-shaped
scheduling context, no donation, no yield-to/return path, and no `OnHandoff`
integration. Txv3 also explicitly reserves `OnHandoff` for PI/RT
ownership-transfer waits, while `WakeHandoff` is only a scheduler hint.

Borrowable idea for the immediate problem: clone publish-ack should look more
like a narrow scheduler protocol than a wake hint. Do not overload
`WakeHandoff` or Txv3 `OnHandoff`.

## Are The Interfaces Enough?

### Enough now

The existing interfaces are enough to:

- keep ordinary readiness wakes behind already-preempted userspace work;
- distinguish futex syscall wakes from lifecycle wakes;
- boost lifecycle/signal/explicit priority wakes;
- prevent a flood of `New` tasks from starving aged preempted tasks;
- submit clone children directly to the live reactor;
- keep clone children out of `New`;
- request same-hart userspace preemption for futex handoff-strength wakes;
- observe queue placement and stop reasons at a coarse level.

### Not enough

The existing interfaces are not enough to:

- acknowledge a submitted child without polling it;
- bound a publish handoff by polls, nanoseconds, or "kernel-only until
  userspace entry";
- distinguish "task is visible in the scheduler" from "task has run";
- prevent child publish-ack from becoming a full child lifecycle slice;
- query task queue state as a stable publish report;
- express fair virtual-time scheduling;
- express scheduling-context donation or return;
- plug in scheduler policies behind a stable trait as `SCHEDULER_v0`
  anticipated;
- prove the userspace preemption transparency model described in `REACTOR_v0`,
  because current timer preempt is represented as a resolved trap to
  `thread_future`.

## Recommended Interface Additions

### 1. Add a publish report for task submission

Extend `submit_task_with_meta_from_hart` or add a new narrower API:

```rust
pub struct TaskPublishReport {
    pub task: TaskKey,
    pub target_hart: HartId,
    pub queue: Phase1QueueKind,
    pub queued_turn: u64,
    pub dispatch: WakeDispatchReport,
}

pub fn submit_task_publish_ack<F, S>(
    &self,
    future: F,
    initial_meta: InitialSchedMeta,
    current_hart: HartId,
    signal: &mut S,
) -> TaskPublishReport
where
    F: Future<Output = ()> + Send + 'static,
    S: RescheduleSignal;
```

The important property is that this returns after task-table insertion,
scheduler metadata insertion, queue insertion, and local marker/IPI dispatch
have all been performed. It should not poll the child.

### 2. Add scheduler publish acknowledgement

Expose a scheduler-side helper used by the submit path:

```rust
pub struct QueuedTaskReport {
    pub task: TaskId,
    pub hart: HartId,
    pub queue: Phase1QueueKind,
    pub queued_turn: u64,
}
```

This should be generated at the same linearization point that sets
`TaskRunOwner::Queued { hart, queue }`.

### 3. Replace clone `yield_now` with publish ack first

The immediate candidate fix should be conservative:

1. keep direct child submit;
2. keep `preempted_on_submit`;
3. after successful clone, require the submit path's publish report to exist;
4. remove or narrow the next-syscall `yield_now` only after host and guest tests
   prove the report is sufficient.

If pure publish ack is not enough, add a second, explicit bounded handoff rather
than falling back to generic `yield_now`.

### 4. Add bounded handoff credit only if publish ack is insufficient

If child visibility alone is not enough, add an explicit bounded credit:

```rust
pub struct HandoffCredit {
    pub max_polls: u8,
    pub max_ns: u64,
    pub may_enter_userspace: bool,
}
```

For clone publish, `may_enter_userspace` should start as `false`. The handoff
may allow scheduler bookkeeping or child future setup through a defined
pre-entry checkpoint, but should not run a userspace slice.

This needs a thread-runtime checkpoint such as:

```rust
enum ThreadFutureCheckpoint {
    Submitted,
    PreUserspaceRequest,
    UserspaceRequestPublished,
    EnteringUserspace,
}
```

The publish-ack test should assert that clone ack stops before
`EnteringUserspace`.

### 5. Keep wake handoff separate from publish ack

Do not add `PublishAck` to `WakeHint`. Wakes and submits are different
protocols:

- wake: a previously parked task should re-observe a semantic condition;
- publish ack: a newly created task has become visible to the scheduler.

Conflating them would make futex behavior and clone lifecycle harder to reason
about.

### 6. Later: add real scheduler policy state

After pthread lifecycle is fixed, add a policy-specific scheduler state layer:

```rust
pub enum PolicyState {
    Phase1,
    Fair {
        weight: u32,
        vruntime_ns: u64,
        vdeadline_ns: u64,
        lag_ns: i64,
    },
    Deadline {
        runtime_ns: u64,
        deadline_ns: u64,
        period_ns: u64,
    },
}
```

This is separate from publish ack. Do not block the pthread fix on a fair
scheduler rewrite.

## Test Plan For The Interface Update

Host tests:

- `submit_task_publish_ack` reports task id, target hart, queue, queued turn,
  and dispatch action.
- A child submitted with `preempted_on_submit` reports `Preempted`, not `New`.
- The publish report is available before any child poll.
- Successful clone records a child publish ack.
- Clone error/no-return paths do not request publish ack.
- Replacing clone `yield_now` does not introduce a yield between
  `start_request()` and `enter_userspace`.
- Same-hart futex `WakeHandoff` still marks `UserspacePreempt`.
- Lifecycle and signal wakes still enter `Boosted`.

Guest tests:

- Build `rv64-qemu`.
- Run the tailored `pthread-minimal2` with bracketed observe.
- Run full libcbench after the tailored case improves.
- Score the saved serial.
- Run `fault-decode --all --brief` on saved logs.

Observability checks:

- publish ack event exists for each successful clone;
- child task queue is reported without child userspace entry;
- parent resumes after publish ack without the prior multi-hundred-microsecond
  clone handoff tax;
- no trap lines and no direct-trap context corruption.

## Conclusion

The current reactor/wake/scheduler interfaces are adequate for the wake-class
mitigation already implemented. They are not adequate for the stronger
publish-ack scheduling model needed to remove the pthread lifecycle long tail.

The next fix should introduce a submit-side publish report and use it to
replace the broad clone `yield_now` handoff. Only if that report is not enough
should Tx add a bounded, kernel-only handoff credit. Fair scheduling and
scheduling-context donation are important later work, but they are larger than
the immediate pthread blocker.
