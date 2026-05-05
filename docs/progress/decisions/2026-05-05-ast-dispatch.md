# `signal::ast_dispatch` — closing the AstOutcome → exit loop

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from `step_exit_group_with_signal`)
**Status:** Complete. CI green (11 gates). 4 new tests; suite 284.

## Goal

Cash out the round-trip the previous passes laid down: a catchable
fatal signal (e.g. SIGTERM with default disposition) should now
actually terminate the target process when site B (AST) processes it.

Before this pass: `ast_check` returned `AstOutcome::DefaultTerminate
{ sig }` but no caller closed the loop. Tests verified the variant
was recognised but the process stayed alive.

## What landed

### `signal::ast_dispatch`

```rust
pub fn ast_dispatch(thread: &Cap<ThreadIdentity>) -> AstOutcome {
    let outcome = ast_check(thread);
    if let AstOutcome::DefaultTerminate { sig } = outcome {
        let guard = epoch::guard();
        if let Some(proc) = thread.owner_proc.upgrade(&guard) {
            drop(guard);
            process::execution::step_exit_group_with_signal(&proc, sig);
        }
    }
    outcome
}
```

Thin wrapper over `ast_check`. Returns the same `AstOutcome` so the
caller can dispatch on the variant after the side-effect (if any)
has run.

| `AstOutcome` variant | Day-1 side-effect |
|---|---|
| `Continue` | none — proceed to userspace |
| `InitiateTermination` | none yet — observed by future `thread_future` poll per spec §15.1; producer of `summary.termination` is responsible for recording the signum |
| `DefaultTerminate { sig }` | **`step_exit_group_with_signal(owner_proc, sig)`** — process zombifies with `terminating_signal = Some(sig)` and shell-convention exit status |
| `DefaultStop { sig }` | none — stop-state machinery deferred |
| `DefaultContinue { sig }` | none — continue control op deferred |
| `DeliverHandler { sig, h }` | none — signal-frame construction deferred |

### End-to-end flow now testable

```
post_signal(leader, SIGTERM)
   ├── thread_pending bit set
   └── summary.deliverable_signal = true
        ↓
ast_dispatch(leader)
   ├── ast_check(leader)
   │      ├── select_next_signal → (SIGTERM, Thread)
   │      ├── dequeue from thread_pending
   │      ├── consult sig_actions → Default
   │      └── default_action(SIGTERM) = Term ⇒ DefaultTerminate { SIGTERM }
   └── if DefaultTerminate { sig } ⇒ step_exit_group_with_signal(proc, sig)
        ├── proc.terminating_signal = Some(SIGTERM)
        └── step_exit_group(proc, 128 + 15 = 143)
             ├── threads drained → all zombies
             ├── proc.payload = None
             └── proc.exit_status = Some(143)
```

### Tests (4 new)

- `ast_dispatch_default_terminate_zombifies_owner_with_signum` —
  asserts `is_zombie() && terminating_signal == Some(SIGTERM) &&
  exit_status == Some(143)`.
- `ast_dispatch_continue_has_no_side_effects`.
- `ast_dispatch_default_stop_recognised_but_unrealised` — SIGTSTP
  default Stop returns the variant but the process stays alive
  (regression test for the not-yet-wired stop-state).
- `ast_dispatch_deliver_handler_recognised_but_unrealised` — handler
  installed for SIGTERM, post SIGTERM, ast_dispatch returns
  `DeliverHandler { handler: 0xFEED }` and the process is NOT
  terminated (handler installation overrides the default-Term path).

## Spec compliance

| Spec | Pre | Post |
|---|---|---|
| §15.1 ast_check produces DefaultTerminate variant | ✓ | ✓ |
| §15.1 "Term/Core ⇒ invoke_group_exit_with_signal" | ❌ no caller | ✓ via ast_dispatch |
| §15.1 InitiateTermination observed by thread_future | future | future (no thread_future yet) |
| §15.1 Handler ⇒ build_signal_frame, redirect | future | future (no frame builder) |
| §15.1 Default Stop ⇒ invoke_group_stop | future | future (no stop_state) |

## Deliberately deferred

- **`InitiateTermination` materialisation**: spec routes via
  thread_future's next poll. Day-1 has no thread_future loop. The
  producer of `summary.termination` (a future fatal-sync-fault
  handler, ptrace fatal, etc.) should set `terminating_signal` on
  the process directly before setting the bit; ast_dispatch can
  then invoke `step_exit_group_with_signal` if needed. No producer
  yet, so no day-1 wiring.
- **`DefaultStop` materialisation**: needs `THREAD_RUNTIME_v1` §6
  stop_state and the stop control op.
- **`DefaultContinue` materialisation**: needs continue control op.
- **`DeliverHandler` materialisation**: needs `SIGNAL_v1` §16
  build_signal_frame, sigaltstack, trampoline placement, plus HAL
  `SignalFrameIf`.
- **AST integration with HAL trap return**: this pass simulates
  site B as a synchronous probe. The actual HAL hook calling
  `ast_dispatch` lands with the trap-return wiring.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 284
  tests pass (280 prior + 4 new).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `signal: ast_dispatch wires DefaultTerminate to step_exit_group_with_signal (SIGNAL_v1 §15.1)`
- `<this commit>` — `docs(progress): record ast_dispatch landing`

## Branch summary so far

The `process-topology` branch now carries 10 commits:

1. Process / Thread / ProcessGroup / Session topology
2. Signal day-1 (post + observe)
3. Cred service stub
4. TTY pgrp typed `Cap<Session>` / `Cap<ProcessGroup>` rebinding
5. Kill permission check
6. TTY → signal end-to-end typed dispatch
7. Signal delivery sweep day-1 (selection + ast_check + summary)
8. Gewalt vs event factoring restored
9. `step_exit_group_with_signal` materialises route_sigkill
10. **`ast_dispatch` materialises ast_check's DefaultTerminate**

Catchable signals with default `Term`/`Core` and SIGKILL are now both
fully end-to-end testable (target zombifies with the correct signum
and exit status). The remaining day-1 spec gaps (`DefaultStop`,
`DefaultContinue`, `DeliverHandler`, `InitiateTermination`,
synchronous-fault routing, AST-via-HAL-trap-return) all need
machinery in other subsystems, not signal itself.

## Next step

The signal subsystem's "what we can do without a real reactor /
HAL trap return / stop-state machinery" story is now closed. Next
work depends on which subsystem comes online first:

1. **HAL trap-return wiring** (touches kernel + HAL) — lets
   `ast_dispatch` actually run on real userspace returns.
2. **Stop-state machinery in `THREAD_RUNTIME_v1` §6** — materialises
   `DefaultStop` / `DefaultContinue`.
3. **Signal-frame construction** — materialises `DeliverHandler`.
4. **Boot wiring** — pid=1 in `tx-kernel/src/init.rs`.
5. **Saved-set IDs on Cred** — extends kill rule to Linux 4-way.

## Blockers

None.
