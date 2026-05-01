# Reactor Userspace-Entry AST Checkpoint

**Date:** 2026-05-01

## Decision

`tx_reactor::userspace` now owns a policy-neutral userspace-entry AST
checkpoint on the existing userspace-run shell. `UserspaceRunSlot` validates
the active request, drains a caller-supplied task-local `AstSlot` exactly once,
hands the drained `AstBatch` to a caller-owned policy closure, and applies only
the reactor-visible continuation. `Reactor::request_userspace_run` now exposes
the current single-slot userspace-run shell through the public reactor facade,
with reactor-owned driver methods for dispatch, timer preemption, interesting
trap completion, and status. `Reactor::checkpoint_task_userspace_entry` adds
the task-keyed adapter over real reactor task storage: it validates the
userspace-run request before consuming the generation-checked task's AST
markers.

- `EnterUserspace` records userspace dispatch;
- `RePollTask` preserves the request for the thread future to run again;
- `Resolve(trap)` resolves the userspace-run wait with a caller-supplied
  interesting trap outcome.

This keeps signal selection, VM page-fault policy, handler-frame construction,
`ThreadPayload` mutation, and final HAL `return_to_userspace` outside the
reactor shell.

## Verification

- `cargo test -p tx-reactor --test userspace_run`

## Next Step

ThreadRuntime still needs to define the real task-to-thread state that calls
this checkpoint before user entry. HAL/trap still needs the production
saved-frame return path, and signal/process still owns the policy that decides
whether the checkpoint proceeds, re-polls, or resolves the userspace wait.
The reactor-owned next slice is no longer the facade itself; it is replacing
the single-slot shell with per-task userspace-run state once ThreadRuntime can
name the owning task/thread mapping.

## Blockers

- Full ThreadRuntime-backed `Reactor::request_userspace_run` task integration
  is still pending.
- Real signal delivery and VM fault policy are not implemented.
- The permanent per-hart production loop and final userspace trampoline remain
  later runtime work.
