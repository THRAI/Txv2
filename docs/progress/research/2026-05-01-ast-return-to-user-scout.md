# AST Return-To-User Scout

**Date:** 2026-05-01

**Scope:** define the future AST return-to-user hook boundary for the reactor
runtime dispatch plan without implementing signal policy.

## Current state

- `REACTOR_v0` names AST as the reactor-owned moment that exists between two
  polls of the same task. It also names userspace-run as a wait whose future
  resolves only on an interesting trap, not on timer preemption.
- Current `tx-reactor` code implements task-local AST marker storage:
  `AstMarker`, `AstSlot`, `AstBatch`, key-checked queue/consume APIs, and
  pre-poll consumption in `Reactor::run_until_idle_on_hart_with_reschedule`.
- `crates/tx-reactor/tests/ast_runtime.rs` proves marker coalescing, task
  locality, pre-poll consumption, and stale/terminal-key rejection.
- The original scout found only poll-boundary mechanism. A follow-up code slice
  now adds a userspace-entry AST checkpoint to the userspace-run shell, but no
  code selects a signal, builds a signal frame, touches `ThreadPayload`, or
  invokes a real HAL return-to-userspace path.
- `crates/tx-reactor/src/userspace.rs` now provides a reactor-local
  userspace-run wait shell with raw interesting-trap outcomes, but it is not
  yet wired into task state, ThreadRuntime, VM, or the trap shell. The current
  HAL crate exposes trap classification snapshots, while the active HAL doc
  still treats full `RawTrapFrame`, `KernelTrapSink`,
  `SignalFrameIf`, and `return_to_userspace` as future trap-shell work.

## Reactor-owned hook boundary

The reactor can own a policy-neutral userspace-entry checkpoint:

1. When a scheduler-selected task is in a future userspace-run wait, and before
   the platform trap shell returns to user mode, the reactor reaches an AST
   checkpoint for that task.
2. The reactor drains that task's AST markers exactly once for this checkpoint,
   preserving the same task-local coalescing/order rules as current
   poll-boundary AST.
3. The reactor hands only reactor facts to an adjacent owner: task identity,
   the drained `AstBatch`, scheduling/dispatch context as needed, and a
   policy-neutral continuation point.
4. The adjacent owner returns a narrow decision the reactor can obey without
   understanding signal semantics:
   - proceed with userspace dispatch;
   - do not enter userspace yet and make/preserve the task runnable for its
     thread-runtime future to run;
   - resolve the userspace-run wait with an already-defined interesting-trap
     outcome, once that outcome type exists.

The reactor should not read `signal_summary`, inspect pending queues, choose a
signal, apply default actions, write user stacks, rewrite trap PCs, or decide
whether stop/termination/handler delivery wins. Its ownership is the temporal
site, marker batch, runqueue effect, and loss-free handoff.

## Adjacent owners and required contracts

- **ThreadRuntime** owns the userspace thread future, the mapping from reactor
  task to `Cap<ThreadPayload>`, `ThreadPayload.regs`, `signal_mask`,
  `thread_pending`, `signal_summary`, `stop_state`, and the site-B loop that
  runs before every userspace entry. It must define how a userspace-run wait is
  represented in reactor task state and how a non-entry AST decision re-enters
  the thread future.
- **Signal/process** own `deliver_posix_signal`, process vs thread pending
  queues, deliverability refresh, signal selection, default actions,
  synchronous-fault signal handling, handler-frame construction, and sigreturn
  state restoration. They decide what the AST checkpoint means.
- **HAL/trap** owns saved-register trap entry, trap classification,
  `KernelTrapSink<P>`, mutable trap-frame views, `SignalFrameIf` frame ABI
  helpers, and the unsafe final `return_to_userspace` operation. It must also
  distinguish timer/IPI/device traps from interesting traps that resolve the
  userspace-run wait.
- **VM/page-fault handling** owns whether a user fault is resolved and resumed
  or becomes a synchronous-fault signal/fatal trap outcome.

## Minimal implemented slice

The safe first code slice is now a host-testable hook in `tx-reactor` that
still carries no POSIX policy:

- `UserspaceRunSlot::checkpoint_userspace_entry` validates an active
  userspace-run request before draining AST markers;
- `Reactor::request_userspace_run` exposes the current single-slot
  userspace-run shell through the public reactor facade, with driver methods
  for dispatch, timer preemption, interesting trap completion, and status;
- `Reactor::checkpoint_task_userspace_entry` consumes the real task-table AST
  slot behind a generation-checked `TaskKey` after validating the active
  userspace-run request;
- reuse `AstBatch` draining semantics rather than adding a second AST queue;
- define `UserspaceEntryDecision` as enter userspace, re-poll task, or resolve
  userspace-run with a caller-supplied outcome;
- prove in tests that the checkpoint runs before mocked userspace dispatch,
  consumes each marker batch once, preserves coalescing, can resolve the wait,
  does not run on timer preemption alone, rejects stale userspace requests
  without draining task AST, rejects terminal tasks without dispatching, and
  never inspects signal payload state.

That slice makes the reactor checkpoint executable without claiming signal
delivery, handler ABI, or real trap-frame return.

## Blockers/non-goals

- Blocked on real `request_userspace_run` task state and `TrapInfo`/equivalent
  interesting-trap outcome types.
- Blocked on `ThreadIdentity`/`ThreadPayload` implementation and the
  ThreadRuntime future that owns site-B policy.
- Blocked on full saved-register trap shell, `KernelTrapSink<P>`,
  `SignalFrameIf`, and real `return_to_userspace`.
- Blocked on signal/process implementation for pending queues, signal
  selection, handler/default action policy, and sigreturn.
- Non-goal for this shard: POSIX signal routing, handler-frame construction, VM
  fault policy, scheduling policy, or any claim that return-to-userspace AST
  delivery is implemented today.
