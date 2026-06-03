# Reactor Scheduling Update

<!-- txdoc:02-EXECUTION-REACTOR-SCHEDULING -->

## Status

<!-- txdoc:REACTOR-SCHEDULING-STATUS -->

Draft scheduling contract for pthread lifecycle scheduling fixes.

This document refines the Phase 1 reactor scheduler policy and the submit-side
publish contract without changing the Linux ABI or introducing Txv3
`OnHandoff`. It exists because libcbench pthread create/join exposes a long
tail when ordinary readiness wakes, lifecycle wakes, signal delivery, and clone
child publication are represented by overly broad scheduler boundaries.

## Boundary

<!-- txdoc:REACTOR-SCHEDULING-BOUNDARY -->

The reactor remains mechanism and the scheduler remains policy. Wake events are
not semantic truth: a wake only asks the runtime to poll a task so the task can
re-observe its condition under the owning subsystem's rules.

Kernel execution is cooperative between reactor poll boundaries. Userspace
execution is preemptive, but ordinary timer preemption is scheduler mechanism,
not semantic progress for the thread future. Syscall, page-fault, and fatal
traps are the events that advance the userspace-thread future.

Once a thread future publishes an active userspace request, it must not yield
before entering userspace. That interval owns trap-frame merge and active
request publication; yielding there can expose partially-published user
context.

`WakeHandoff` is intentionally not named `Handoff`. Txv3 reserves
`OnHandoff` for PI/RT ownership-transfer waits. This update only describes a
scheduler hint for waking an already-published waiter soon enough that the
waker does not overrun a userspace lifecycle protocol.

## Scheduling Contract

<!-- txdoc:REACTOR-SCHEDULING-CONTRACT -->

The Phase 1 contract is:

1. The reactor owns task polling, wake drainage, userspace-run waits, timer/IPI
   markers, and safe poll boundaries.
2. The scheduler owns runnable placement, queue order, budgets, affinity,
   fairness interpretation, and priority interpretation.
3. Subsystems own semantic truth. A woken task must re-observe the condition
   under the subsystem's rules before it assumes progress.
4. Wakes are advisory scheduling hints. They are not task-publication reports
   and they are not PI/RT ownership-transfer handoffs.
5. Task submission publishes a new reactor task. Clone child publication must
   be acknowledged by submit-side scheduler state, not by requiring the child to
   run.
6. A successful clone return requires child publish acknowledgement, not child
   execution acknowledgement.
7. A bounded handoff, if needed after publish acknowledgement, must be named as
   such and must be able to forbid userspace entry.

The contract explicitly does not promise:

- that a wake means the semantic condition is true;
- that clone return means the child has run;
- fair virtual-time scheduling;
- scheduling-context donation or replenishment;
- Txv3 `OnHandoff` semantics for `WakeHandoff`.

The boundary is intentionally split into three lanes:

```text
                         +-----------------------------+
                         |        Thread future        |
                         |  syscall/fault/exit driver  |
                         +--------------+--------------+
                                        |
                                        | poll / await / trap result
                                        v
+-------------------+       +----------+----------+       +-------------------+
| Semantic objects  | wake  |       Reactor       | place |     Scheduler     |
| futex, pipe, exit +------>| poll, mailbox drain +------>| queues, budget,   |
| signal, VM, ...   |       | userspace-run slot  |       | affinity, hints   |
+---------+---------+       +----------+----------+       +---------+---------+
          ^                            |                            |
          | re-observe                 | submit                     | pick/stop
          | semantic state             v                            v
          |                 +----------+----------+       +---------+---------+
          +-----------------+    Task table       |<------+  Run ownership    |
                            | TaskKey -> future   |       | Queued/Polling/...|
                            +---------------------+       +-------------------+
```

Read the arrows as ownership, not call-stack prescription:

- semantic objects publish wake facts, but keep semantic truth;
- the reactor turns wakes and submits into runnable task mechanics;
- the scheduler chooses where and when runnable tasks execute;
- the thread future advances only at reactor poll/trap boundaries.

## Wake Classes

<!-- txdoc:REACTOR-SCHEDULING-WAKE-CLASSES -->

Scheduler hints are ordered by strength:

```text
Normal < WakeHandoff < LifecycleWake < PriorityBoost < SignalDelivery
```

- `Normal`: default readiness notification from wait sources.
- `WakeHandoff`: futex syscall wake that posted at least one waiter. It does
  not enter the boosted queue.
- `LifecycleWake`: thread-exit wake such as `clear_child_tid` and robust-list
  exit wake.
- `PriorityBoost`: explicit internal priority wake.
- `SignalDelivery`: signal delivery posts.

Mailbox hint latching is monotonic within one wake batch: a stronger hinted
post wins over earlier weaker posts until the reactor consumes the batch.

## Queue Policy

<!-- txdoc:REACTOR-SCHEDULING-QUEUE-POLICY -->

The Phase 1 local queue order is:

```text
Kernel -> Boosted -> aged Preempted -> New -> Preempted
```

`Boosted` is reserved for `LifecycleWake`, `PriorityBoost`, and
`SignalDelivery`. `WakeHandoff` does not enter `Boosted`, but a futex wake
with remaining budget is placed at the front of `Preempted`. This gives the
published waiter one handoff opportunity without turning every futex wake into
a priority lane. `Normal` userspace wakes keep ordinary preempted placement
behind already-preempted peers.

Preempted work ages by scheduler turns. A preempted task queued for at least
`AGING_PROMOTION_TURNS` is eligible before `New`, preventing long tails where a
steady stream of new work indefinitely delays already-preempted userspace
threads.

## Comparison With Other Schedulers

<!-- txdoc:REACTOR-SCHEDULING-COMPARISON -->

Tx's current scheduler is a Phase 1 coroutine-kernel runqueue policy. It is not
yet a mature fair-share, deadline, or temporal-isolation scheduler. Its core
strength is the split between subsystem truth, reactor mechanics, and scheduler
placement; its core weakness is that CPU policy is still queue-based rather
than virtual-time or scheduling-context based.

```text
Tx today:
  wake classes + per-hart queues + budget remnants + aging

Linux / Fuchsia:
  virtual time + eligibility/deadline math + weighted fairness

seL4 MCS:
  explicit CPU-time objects + budget/period + donation/return

Coroutine executors / Managarm-like kernels:
  poll segments + wake mechanics + work stealing / fairness policy
```

| System | Scheduling core | Tx comparison |
|---|---|---|
| Tx Phase 1 | `Kernel -> Boosted -> aged Preempted -> New -> Preempted`; wake hints; per-hart queues; cooperative kernel polls; preemptive userspace | Good mechanism split, but policy is still coarse queue heuristics. |
| Linux EEVDF | Eligible fair tasks are ordered by virtual deadline, with lag/eligibility controlling who should run. | Tx lacks virtual runtime, lag, virtual deadline, and eligibility math. |
| Fuchsia/Zircon | Fair and deadline policies over per-CPU queues, with weight/deadline style accounting. | Tx has per-hart queues, affinity, IPIs, and consumed-time accounting, but not fair tree ordering, weights, or real deadline admission. |
| seL4 MCS | Scheduling contexts are explicit CPU-time objects with budget/period and donation/return behavior. | Tx has no sched-context object, no donation chain, no replenishment accounting, and no Txv3 `OnHandoff` implementation. |
| Tokio-style executor | Cooperative poll segments, local queues, wake-to-runnable mechanics, and work stealing. | Tx is conceptually close, but must preserve kernel safe points, userspace trap discipline, and semantic subsystem re-observation. |
| Managarm-like coroutine kernel | Async wait substrate plus separate scheduler policy. | Tx is closest here architecturally: wait truth and runnable policy are separate. |

Borrowing order should be pragmatic:

1. Borrow from coroutine kernels and executors for the immediate pthread fix:
   make runnable publication cheap, explicit, and observable.
2. Borrow from Linux/Fuchsia after that for normal-task fairness: add
   virtual-time or virtual-deadline state instead of tuning queue constants
   indefinitely.
3. Borrow from seL4 MCS only when Tx needs temporal isolation, PI/RT ownership
   transfer, or CPU-budget donation as a first-class kernel object.

Do not use fair scheduling or scheduling-context donation as prerequisites for
the libcbench pthread lifecycle fix. That fix needs submit-side publish
acknowledgement first.

## Submit Publish Acknowledgement

<!-- txdoc:REACTOR-SCHEDULING-PUBLISH-ACK -->

Task publication is distinct from wake delivery. A wake targets a task that has
already parked or become waitable. A submit publishes a new task into the task
table and scheduler queues. Clone child publication uses the submit protocol,
not the wait-source wake protocol.

The two paths differ:

```text
WAIT-SOURCE WAKE PATH

  object state changes
          |
          v
  WaitSource::notify_with_hint(mask, hint)
          |
          v
  TaskMailbox::post_with_scheduler_hint(SourceFired, hint)
          |
          v
  reactor drains mailbox batch
          |
          v
  scheduler.task_runnable(task, hint)
          |
          v
  later poll: task re-observes object state


SUBMIT PUBLISH PATH

  clone creates child ThreadIdentity + ThreadPayload
          |
          v
  reactor.submit_task_publish_ack(child future, meta)
          |
          +--> task table insert
          +--> scheduler metadata insert
          +--> owner = Queued { hart, queue }
          +--> local queue insert
          +--> local marker / remote IPI if needed
          |
          v
  TaskPublishReport returned to parent
          |
          v
  parent may return from clone; child has not necessarily run
```

A child publish acknowledgement is complete when all of these are true:

1. the task-table entry exists;
2. scheduler metadata exists for the task;
3. `TaskRunOwner` is `Queued { hart, queue }`;
4. the task id is inserted into the selected local queue;
5. local dispatch marker or remote reschedule IPI has been emitted if required;
6. the submit path returns a structured report naming the task, hart, queue,
   queued turn, and dispatch action.

This acknowledgement must not require polling the child. In particular, it must
not require the child to publish a userspace-run request, enter userspace, take
a first syscall, or run its exit path.

The current next-syscall clone `yield_now` handoff is a transitional scheduling
boundary, not the submit publish contract. Submit-side acknowledgement proves
the child is visible to the scheduler, but libcbench pthread evidence shows
publication alone is not sufficient: a parent can otherwise re-enter userspace
and issue the next create-side syscall while newly published children remain
behind the parent in `Preempted`, extending musl's `__thread_list_lock` convoy.

Until Tx has an explicit bounded handoff credit, every successful clone return
keeps one deferred handoff at the next syscall boundary. The replacement should
keep the submit-side acknowledgement above and replace the broad cooperative
yield with a policy field equivalent to `may_enter_userspace = false` for clone
publish.

The pthread clone/join lifetime target is:

```text
PARENT THREAD                                      CHILD THREAD

enter userspace
     |
     v
clone syscall trap
     |
     v
step_clone_thread()
     |
     +---------------- submit child --------------------+
     |                                                  |
     v                                                  v
TaskPublishReport                               Queued(Preempted)
     |                                                  |
     |  publish ack proves scheduler visibility         |
     |  it does not prove child execution               |
     v                                                  |
store clone return                               later scheduler pick
     |                                                  |
     v                                                  v
enter userspace                           start_request -> enter userspace
     |                                                  |
     v                                                  v
parent continues clone loop                child runs start routine / exit
     |                                                  |
     |                         futex/clear_child wake   |
     +<------------------------- join visibility -------+
     |
     v
join re-observes child state
```

The invalid shape for the long-tail fix is:

```text
clone success -> generic yield_now -> child consumes full userspace/lifecycle
                                   -> parent resumes much later
```

The current production shape is:

```text
clone success -> submit publish ack -> parent resumes userspace
                         |
                         +-- next syscall boundary yields once
                             child gets a scheduling opportunity before the
                             parent dispatches the next create-side syscall
```

The target replacement shape is:

```text
clone success -> submit publish ack -> bounded clone handoff credit
                         |
                         +-- max_ns / max_polls / may_enter_userspace = false
```

## Runtime Preemption

<!-- txdoc:REACTOR-SCHEDULING-RUNTIME-PREEMPTION -->

When a wake targets the current hart and its hint is at least `WakeHandoff`, the
runtime marks `UserspacePreempt`. Remote target wakes continue to send a
reschedule IPI. `NeedResched` remains a dispatch marker; it is not consumed as
a userspace-entry checkpoint.

Userspace-entry handoffs stay at safe points only. The thread future must not
yield after publishing an active userspace request and before `enter_userspace`.
The clone helper is currently a child-publish handoff: a successful `clone`
return asks for one deferred reactor handoff at the next syscall boundary so the
child task is visible and runnable before clone storms dominate runnable peers.
The handoff is not proof of publication; the submit-side publish report is.
The target contract is narrower: clone should observe that publish report and
use an explicit bounded clone handoff credit rather than a broad cooperative
yield.

## Producers

<!-- txdoc:REACTOR-SCHEDULING-PRODUCERS -->

- Generic `WaitSource::notify*` calls emit `Normal`.
- Futex `FUTEX_WAKE` and `FUTEX_WAKE_BITSET` syscall paths, including the
  direct-trap one-shot path, emit `WakeHandoff` when they post waiters.
- `clear_child_tid` and robust-list exit wakes use explicit lifecycle futex
  wrappers that emit `LifecycleWake`.
- Signal posting continues to emit `SignalDelivery`.

## Verification

<!-- txdoc:REACTOR-SCHEDULING-VERIFICATION -->

Host coverage should pin:

- default `SourceFired` as `Normal` and explicit hinted posts as strongest-wins;
- default wait-source notify as `Normal`, futex notify as `WakeHandoff`, and
  lifecycle notify as `LifecycleWake`;
- scheduler placement for `WakeHandoff`, boosted wake classes, aged preempted
  promotion, and duplicate wake coalescing;
- same-hart `WakeHandoff` marking `UserspacePreempt`;
- successful clone return requesting the child-publish handoff, while
  non-clone, clone-error, and no-return paths do not;
- submit-side child publish acknowledgement reporting task id, target hart,
  queue, queued turn, and dispatch action;
- cloned children submitted with `preempted_on_submit` reporting `Preempted`,
  not `New`;
- publish acknowledgement being available before any child poll;
- no yield after active userspace request publication and before userspace
  entry.

Guest validation should use bracketed observe windows around pthread-focused
libcbench runs, then score the saved serial and fault-decode every saved guest
log before claiming the pthread lifecycle long tail is fixed.
