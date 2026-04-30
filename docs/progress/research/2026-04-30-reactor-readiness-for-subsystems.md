---
date: 2026-04-30
topic: "Reactor readiness for subsystem development"
status: complete
---

# Research: Reactor Readiness For Subsystem Development

## Question

How far is the current reactor design and implementation from a solid reactor
that subsystem authors can build against?

## Conclusion

The active reactor design is mostly ready as an architectural contract for
subsystem authors. It clearly preserves the core boundaries: steps are
synchronous, scripts adapt `Blocked` outcomes into reactor waits, wakes are
hints rather than truth, tasks are temporal handles rather than semantic
entities, and thread runtime owns userspace thread semantics.

The implementation is earlier. `tx-reactor` is a host-testable cooperative
executor skeleton with task-local wakers, parked/runnable task state, wait
channels, timeout waits, and a first `Phase1Scheduler` shell. That is useful
for prototyping kernel-only async flows and for exercising wait discipline, but
it is not yet the runtime substrate that Process, ThreadRuntime, VM, VFS, TTY,
and signal delivery can depend on directly.

## Solid Pieces

- `REACTOR_v0` pins the reactor as infrastructure: schedule tasks, mediate
  wait/wake, own AST and cross-core sync carve-outs, and avoid semantic entity
  ownership.
- `STEP_MODEL_v1` and `THREAD_RUNTIME_v1` give subsystem authors a usable
  composition model: `thread_future` contains scripts, scripts compose bounded
  synchronous steps, and wait-adapt is the only blocking mechanism between
  steps.
- `SCHEDULER_v0` defines the policy boundary and a Phase 1 round-robin/two-queue
  scheduler contract.
- `crates/tx-reactor` implements task status, task-local waker coalescing,
  mask-based wait channels, `wait_event` re-observation, timeout wake driving,
  scheduler-facing task/hart/slice/stop/wake types, and `Phase1Scheduler`.

## Blocking Gaps

- Real reactor loop: current `run_until_idle` exits when no runnable task
  exists. There is no long-running idle/WFI/interrupt-driven kernel loop.
- Userspace runtime: no `request_userspace_run`, saved-register userspace
  dispatch, interesting-trap resolution, or trap-to-future handoff exists yet.
- Signal classification: `Interrupted` and `Killed` are API variants, but
  wait-adapt does not consult a thread-runtime interrupt source.
- AST and return-to-user delivery: `AstSlot` is only a placeholder.
- Hardware time: the HAL `TimeIf` surface exists, but reactor timeouts are
  still host-driven through `advance_time_to`.
- Bus/substrate integration: current wait channels are local queues, not yet
  substrate bus subscriptions with lost-wake linearization and epoch discipline.
- Cross-hart coordination: no IPI reschedule path, shootdown rendezvous surface,
  or multi-hart reactor operation exists yet.
- Thread drain/cancel: `TaskHandle` is a copyable id today; thread-runtime
  ownership and payload drain semantics remain to be implemented.

## Design Gaps To Close Before Freezing The API

- Choose one canonical wait shape. `REACTOR_v0` exposes
  `wait(channel, mask, protocol) -> Ready`, while Concepts/Bus also discuss
  `channel + condition + protocol -> ConditionTrue`. The implementation should
  pin where the recheck lives.
- Spell the lost-wake linearization rule for subscribe/recheck/park precisely.
  The docs require atomic register-or-recheck, but the current prose leaves
  room for unsafe check-then-subscribe implementations.
- Reconcile one-shot completion semantics: broadcast-like `done: AtomicBool`
  versus credit-consuming completion unless explicitly declared broadcast.
- Close API spelling for task drain/cancel, `yield_now`, AST queues, and
  synchronous coordination.

## Readiness Rating

Ready for subsystem design and step/script skeleton work: mostly.

Ready for executable subsystem development that blocks on real runtime waits,
signals, userspace scheduling, or cross-hart coordination: no, not yet.

Practical distance: about one solid implementation slice from a useful
kernel-only cooperative reactor, and several slices from the full subsystem
runtime. The next useful sequence is bus-backed wait linearization, HAL-driven
timer/idle loop, interruptible/killable wait classification, task drain/cancel,
then userspace-run and AST integration once trap/thread-runtime pieces exist.

## Verification

Subagents read the active reactor design docs, current `tx-reactor` code/tests,
and reactor progress records. The main session also ran:

```text
cargo test -p tx-reactor
```

Result: 17 tests passed.
