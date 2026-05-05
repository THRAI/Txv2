# Signal Gewalt/event factoring restored

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from signal delivery sweep day-1)
**Status:** Complete. CI green (11 gates). 5 new tests added; total suite 277.

## Goal

Audit found day-1 collapsed the spec's two signal categories into a
single `post_signal` pipeline: SIGKILL/SIGSTOP/SIGCONT entered
`thread_pending` alongside catchable signals, with `post_signal`
special-cases doing the summary updates. Per `SIGNAL_v1` §1 (the
Gewalt/event factoring) and §2 Consequence 2 (*"Pending queues carry
only catchable signals. SIGKILL / SIGSTOP / SIGCONT do not enter
pending queues."*), the Gewalt signums must bypass pending entirely
and route directly to control-op summaries. This pass restores the
spec's structural separation.

## Spec ground truth

| Doc | Section | Says |
|---|---|---|
| [`SIGNAL_v1`](../../design/04_process-signals/SIGNAL_v1.md) §1 | factoring | Gewalt = SIGKILL/SIGSTOP/SIGCONT (+ sync faults with SIG_DFL); Event = everything else; Hybrid = sync faults with handler |
| [`SIGNAL_v1`](../../design/04_process-signals/SIGNAL_v1.md) §2 Cons. 2 | bypass | "Pending queues carry only catchable signals. SIGKILL / SIGSTOP / SIGCONT do not enter pending queues. They are routed directly to control ops." |
| [`SIGNAL_v1`](../../design/04_process-signals/SIGNAL_v1.md) §12.1 | dispatch | `match sig { SIGKILL => route_sigkill, SIGSTOP => route_sigstop, SIGCONT => route_sigcont, _ => /* catchable path */ }` |
| [`SIGNAL_v1`](../../design/04_process-signals/SIGNAL_v1.md) §12.3 | route_sig* | route_sigkill/stop directly invoke control ops; route_sigcont invokes continue + may enqueue for handler if installed |

## What landed

### New API in `signal.rs`

```rust
/// True for SIGKILL / SIGSTOP / SIGCONT.
pub const fn is_gewalt(sig: Signum) -> bool;

/// Apply a Gewalt signal to every live thread of `target`. Updates
/// signal_summary directly; does NOT touch thread_pending or
/// group_pending. Per SIGNAL_v1 §12.3 / §2 Consequence 2.
pub fn route_gewalt(target: &Cap<ProcessIdentity>, sig: Signum) -> KillOutcome;
```

### Dispatch in `step_kill_process`

```rust
pub fn step_kill_process(target: &Cap<ProcessIdentity>, sig: Signum) -> KillOutcome {
    if is_gewalt(sig) {
        return route_gewalt(target, sig);    // bypass pending entirely
    }
    // catchable: pick leader, post_signal
    // ...
}
```

### Tightened `post_signal` contract

```rust
pub fn post_signal(thread: &Cap<ThreadIdentity>, sig: Signum) {
    debug_assert!(!matches!(sig, SIGKILL | SIGSTOP | SIGCONT),
        "post_signal must not be called with Gewalt signums; use route_gewalt");
    // ... only handle catchable: set pending bit + summary.deliverable_signal
}
```

The previous body's special-cases (SIGKILL → termination, SIGSTOP-family →
stop_requested, SIGCONT → clear stop_requested) all moved to
`route_gewalt`. Note that SIGTSTP/SIGTTIN/SIGTTOU stay catchable
(only SIGSTOP is Gewalt); their stop intent is materialised by
`ast_check` returning `DefaultStop`, not by a summary bit at post.

### Pgrp shim fixes

`step_kill_pgrp` and `script_kill_pgrp` previously mirrored every
delivered signum onto `group_pending`. Now they skip the mirror
for Gewalt:

```rust
if step_kill_process(member, sig) == KillOutcome::Delivered {
    if !is_gewalt(sig) {
        if let Some(payload) = member.payload.lock().as_ref() {
            payload.group_pending().post(sig);
        }
    }
    delivered += 1;
}
```

### Tests

Updated:
- `sigstop_post_sets_summary_stop_requested` →
  `sigstop_routed_via_step_kill_sets_summary_stop_requested`
- `sigcont_post_clears_stop_requested` →
  `sigcont_routed_via_step_kill_clears_stop_requested`

Both now use `step_kill_process` (which dispatches to `route_gewalt`)
instead of calling `post_signal` directly.

Added:
- `sigkill_does_not_enter_thread_pending`
- `sigstop_does_not_enter_thread_pending`
- `sigcont_does_not_enter_thread_pending`
- `route_gewalt_marks_target_threads_only`
- `step_kill_pgrp_does_not_mirror_gewalt_to_group_pending`

Removed:
- `ast_check_default_continue_for_sigcont` — SIGCONT is Gewalt and
  bypasses AST; the `AstOutcome::DefaultContinue` variant remains in
  the enum for the future case where SIGCONT-with-handler is enqueued
  to `group_pending` per §12.3's hybrid path, but day-1 never reaches
  it. `sigcont_does_not_enter_thread_pending` covers the bypass.

Net: 21 → 26 delivery tests (-1 +5), and `post_signal` contract is
no longer "happens to work for Gewalt", it's "never called with
Gewalt".

## Spec compliance

| Spec claim | Pre-refactor | Post-refactor |
|---|---|---|
| §2 Consequence 2: SIGKILL/SIGSTOP/SIGCONT not in pending | ❌ all 3 entered thread_pending | ✓ all bypass via route_gewalt |
| §12.1: dispatch by signum at producer entry | ❌ uniform post_signal | ✓ step_kill_process dispatches |
| §12.3: route_sigkill sets termination on every thread | ⚠ only on leader | ✓ on every member thread (multi-thread future-ready) |
| §1 Hybrid SIGCONT enqueue-for-handler | not modelled | not modelled (still future) |
| group_pending only for catchable | ❌ Gewalt also mirrored | ✓ Gewalt skips mirror |

## Deliberately deferred

- **SIGCONT enqueue-for-handler half**: §12.3 `route_sigcont`
  invokes continue *and* if a handler is installed, enqueues SIGCONT
  on `group_pending` so the handler runs after the continue. Day-1
  only does the continue half (clears stop_requested). When SIGCONT-
  with-handler is wired, `route_gewalt`'s SIGCONT branch grows the
  enqueue and `AstOutcome::DefaultContinue` becomes reachable.
- **Synchronous-fault Hybrid path**: §1 lists "synchronous faults
  with handler installed" as Hybrid. Lands with HAL fault routing.
- **`route_gewalt` SIGKILL fan-out across thread group**: today
  walks `payload.threads` which is currently 1-thread per process.
  When fork/clone produces multi-threaded processes, the existing
  loop already handles them — no further change needed.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 277
  tests pass (272 prior + 5 net).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `signal: factor Gewalt vs event per SIGNAL_v1 §1, §2 (route_gewalt + post_signal contract)`
- `<this commit>` — `docs(progress): record signal Gewalt/event factoring`

## Next step

The `process-topology` branch now carries 8 stacked commits. The
delivery layer now matches the spec's structural shape. Next on the
follow-up list:

1. **`step_exit_group_with_signal(proc, sig)`** (~30 min). Materialises
   `AstOutcome::DefaultTerminate` and `route_gewalt`'s SIGKILL
   summary into actual group exit with an exit-status that encodes
   "killed by sig".
2. **Boot wiring** (~1 session).
3. **Saved-set IDs on `Cred`** (~1 session).
4. **Session→leader-pgrp index** (~30 min).
5. **`SaFlags` on `SigDisposition`** (~1 session).

## Blockers

None.
