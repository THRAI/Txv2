# step_exit_group_with_signal

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from Gewalt/event factoring)
**Status:** Complete. CI green (11 gates). 280 tests pass (+3 net).

## Goal

Materialise the SIGKILL control-op invocation that `SIGNAL_v1` §12.3
`route_sigkill` prescribes:

```rust
fn route_sigkill(target: SignalTarget) -> DeliveryOutcome {
    // ...
    invoke_group_exit_with_signal(proc, SIGKILL);
    // exit_status encodes "killed by SIGKILL".
    // ...
}
```

Before this pass, our `route_gewalt(SIGKILL)` set `summary.termination`
on every thread but didn't actually zombify the process. The previous
"day-1 collapses to summary bits" comment finally cashes out: SIGKILL
now invokes group exit on the spot.

## What landed

### `ProcessIdentity` extension

```rust
pub struct ProcessIdentity {
    // (existing fields unchanged)
    pub(crate) terminating_signal: SpinMutex<Option<Signum>>,   // NEW
}

impl ProcessIdentity {
    pub fn terminating_signal(&self) -> Option<Signum>;
}
```

`Some(sig)` if the process was killed by a signal; `None` for
explicit `step_exit_group(int)` exits and live processes. Future
`wait(2)` consults both `exit_status` and `terminating_signal` to
build the POSIX status word (WIFEXITED vs WIFSIGNALED).

### New `process::step_exit_group_with_signal`

```rust
pub fn step_exit_group_with_signal(process: &Cap<ProcessIdentity>, sig: Signum) {
    *process.terminating_signal.lock() = Some(sig);
    step_exit_group(process, signal_exit_status(sig));
}

const fn signal_exit_status(sig: Signum) -> i32 {
    128 + sig.raw() as i32   // shell convention; replaceable when wait(2) lands
}
```

Day-1 status convention: `128 + sig` (shell). Linux `wait(2)`
encodes as `(sig & 0x7f)` with the core-dump bit at 0x80; we'll
swap encoders when `wait(2)` lands and there's an authoritative
WIFSIGNALED/WTERMSIG decoder pair.

### `route_gewalt(SIGKILL)` rewired

```rust
pub fn route_gewalt(target: &Cap<ProcessIdentity>, sig: Signum) -> KillOutcome {
    debug_assert!(is_gewalt(sig));

    if sig == Signum::SIGKILL {
        if target.is_zombie() { return KillOutcome::NoLiveThread; }
        crate::process::execution::step_exit_group_with_signal(target, sig);
        return KillOutcome::Delivered;
    }
    // ... SIGSTOP / SIGCONT path unchanged: update summary.stop_requested
}
```

`summary.termination` is no longer set by SIGKILL. The bit remains
in `InterruptSummary` for `THREAD_RUNTIME_v1` §5.2's other producers
(fatal sync fault, ptrace fatal) which haven't landed yet.
`AstOutcome::InitiateTermination` is still the day-1 outcome when
the bit IS set on a still-live thread.

### Tests

Added (process side):
- `step_exit_group_with_signal_records_signum_and_status_encoding`
- `step_exit_group_with_signal_overrides_terminating_signal_on_double_call`
- `step_exit_group_does_not_set_terminating_signal`

Reshaped (signal side):
- `sigkill_post_sets_summary_termination` →
  `sigkill_via_step_kill_zombifies_process_with_status_encoding`.
  Asserts `is_zombie()`, `terminating_signal == Some(SIGKILL)`,
  `exit_status == Some(128 + SIGKILL)`.
- `route_gewalt_marks_target_threads_only` →
  `route_gewalt_sigkill_zombifies_target_only`. Asserts target
  is_zombie + sibling untouched.
- `ast_check_initiate_termination_for_summary_termination_bit`:
  no longer uses SIGKILL (which now zombifies before AST runs).
  Sets the bit directly via the crate-internal `update_summary`
  helper to exercise the AST priority-1 path on a still-live thread.

Removed:
- `sigkill_does_not_enter_thread_pending` — covered by the
  zombification test (after SIGKILL the thread's payload is gone,
  so there's literally no `thread_pending` to inspect).

## Spec compliance

| Spec | Pre | Post |
|---|---|---|
| §12.3 route_sigkill invokes invoke_group_exit_with_signal | ❌ set summary bit only | ✓ direct invocation |
| "exit_status encodes 'killed by SIGKILL'" | ❌ no encoding | ✓ 128 + sig (day-1 convention) |
| Process zombifies on SIGKILL | ❌ stayed alive with bit | ✓ |
| Terminating signum recoverable for `wait(2)` | ❌ not stored | ✓ via terminating_signal |

## Deliberately deferred

- **Linux-style WIFSIGNALED status encoding**: day-1 uses
  shell-convention `128 + sig`. Migrates with `wait(2)`.
- **`ProcessIdentity.exit_kind` enum**: a single `enum
  ExitKind { Exited(i32), Signaled(Signum) }` would unify
  `exit_status` + `terminating_signal`. Day-1 keeps both fields
  to avoid a breaking change to `exit_status()`'s shape.
- **`fatal_synchronous_fault` producer**: `THREAD_RUNTIME_v1` §5.2
  lists "fatal exit condition set" as one of the producers of
  `summary.termination`. SIGSEGV-with-default-Term + handle_fault
  routing land with HAL fault wiring.
- **`AstOutcome::DefaultTerminate` invoking the exit**: today the
  variant is recognised by tests but no caller actually calls
  `step_exit_group_with_signal` on the returned signum. The
  reactor's AST sweep (still future) will wire that.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 280
  tests pass (277 prior + 3 net).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `process+signal: step_exit_group_with_signal materialises route_sigkill (SIGNAL_v1 §12.3)`
- `<this commit>` — `docs(progress): record step_exit_group_with_signal landing`

## Next step

The branch now carries 9 commits. The signal/process boundary is
fully wired for SIGKILL. Logical next:

1. **`ast_check::DefaultTerminate` → `step_exit_group_with_signal`
   bridge** (~30 min). When the reactor AST sweep lands, the
   handler for `DefaultTerminate { sig }` should invoke
   `step_exit_group_with_signal(self_proc, sig)` to terminate the
   process. Currently the test verifies the variant is returned,
   but no caller closes the loop. Adding a small
   `signal::dispatch_ast_outcome(thread, outcome)` helper would
   make the round-trip testable end-to-end.
2. **Boot wiring** (~1 session).
3. **Saved-set IDs on `Cred`** (~1 session).
4. **`SaFlags` on `SigDisposition`** (~1 session).

## Blockers

None.
