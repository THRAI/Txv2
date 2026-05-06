# Signal delivery sweep day-1 (selection + ast_check)

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from TTY → signal end-to-end typed dispatch)
**Status:** Complete. CI green (11 gates). 21 new tests pass; total suite 272.

## Goal

Land the delivery-side machinery `SIGNAL_v1` §14 (selection
algorithm) and §15.1 (ast_check) describe, scoped to what works
without the (still-future) reactor wakers, wait-adapt, AST trap-
return, and signal-frame construction. After this pass, `SigDisposition`
finally has a real consumer; the `signal_summary` field that
`THREAD_RUNTIME_v1` §5.2 has been pointing at since the topology
pass is finally read; and the day-1 surface tests can simulate
site-B delivery end-to-end against fabricated thread/process state.

## Spec ground truth

| Doc | Section | Says |
|---|---|---|
| [`SIGNAL_v1`](../../design/04_process-signals/SIGNAL_v1.md) §14 | selection | thread_pending lowest first, then group_pending lowest, both intersected with `!mask` |
| [`SIGNAL_v1`](../../design/04_process-signals/SIGNAL_v1.md) §15.1 | ast_check | termination → return; else loop: select → dequeue → consult sig_actions → ignore-cases re-loop, terminal-cases return |
| [`THREAD_RUNTIME_v1`](../../design/02_execution/THREAD_RUNTIME_v1.md) §5.2 | summary | atomic `{deliverable_signal, termination, stop_requested}` kept current by `post_signal`, `sigprocmask`, `step_thread_exit`, SIGKILL routing |
| [`THREAD_RUNTIME_v1`](../../design/02_execution/THREAD_RUNTIME_v1.md) §5.4 | two sites | A=wait-adapt on wake; B=AST on userspace return. Day-1 only models site B as a synchronous probe |

## What landed

### `signal.rs` additions

```rust
pub struct InterruptSummary {
    pub deliverable_signal: bool,
    pub termination: bool,
    pub stop_requested: bool,
}
impl InterruptSummary {
    pub const EMPTY: Self;
    pub const fn pack(self) -> u8;
    pub const fn unpack(bits: u8) -> Self;
}

pub enum DefaultAction { Term, Core, Ignore, Stop, Cont }
pub fn default_action(sig: Signum) -> DefaultAction;

pub enum PendingSource { Thread, Group }
pub fn select_next_signal(thread: &Cap<ThreadIdentity>)
    -> Option<(Signum, PendingSource)>;

pub enum AstOutcome {
    Continue,
    InitiateTermination,
    DefaultTerminate { sig: Signum },
    DefaultStop      { sig: Signum },
    DefaultContinue  { sig: Signum },
    DeliverHandler   { sig: Signum, handler: usize },
}
pub fn ast_check(thread: &Cap<ThreadIdentity>) -> AstOutcome;
```

### `ThreadPayload` extension

```rust
pub struct ThreadPayload {
    // (existing fields unchanged)
    pub(crate) signal_summary: AtomicU8,   // NEW
}

impl ThreadPayload {
    pub fn interrupt_summary(&self) -> InterruptSummary;            // pub
    pub(crate) fn update_summary(&self, f: impl Fn(&mut InterruptSummary));
}
```

### Summary-maintenance hooks

`thread_runtime::execution::post_signal`:
- post the bit on `thread_pending`
- if unmasked → `summary.deliverable_signal = true`
- if `sig == SIGKILL` → `summary.termination = true`
- if `sig ∈ {SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU}` → `summary.stop_requested = true`
- if `sig == SIGCONT` → `summary.stop_requested = false`

`thread_runtime::execution::step_sigprocmask`:
- store the new mask
- recompute `summary.deliverable_signal` from `pending().deliverable_bits(new_mask) != 0`

### `default_action` table

| Signums | Action |
|---|---|
| 1 (SIGHUP), 2 (SIGINT), 9 (SIGKILL), 13 (SIGPIPE), 15 (SIGTERM), unknown RT | `Term` |
| 3 (SIGQUIT), 4 (SIGILL), 6 (SIGABRT), 11 (SIGSEGV) | `Core` |
| 17 (SIGCHLD), 23 (SIGURG), 28 (SIGWINCH) | `Ignore` |
| 19 (SIGSTOP), 20 (SIGTSTP), 21 (SIGTTIN), 22 (SIGTTOU) | `Stop` |
| 18 (SIGCONT) | `Cont` |

### `select_next_signal` algorithm

1. Acquire thread payload; read `signal_mask`.
2. `thread_pending.deliverable_bits(mask)` — if any bit set, return
   `(lowest_signum_bit, PendingSource::Thread)`.
3. Drop thread payload lock; upgrade `owner_proc` Weak.
4. `proc_payload.group_pending().deliverable_bits(mask)` — if any
   bit set, return `(lowest_signum_bit, PendingSource::Group)`.
5. Else `None`.

`lowest_signum_bit` uses `bits.trailing_zeros() + 1`.

### `ast_check` algorithm (per §15.1)

1. Read `signal_summary`. If `termination` → `InitiateTermination`
   (no signal pop).
2. Upgrade `owner_proc`. If gone → `Continue`.
3. Loop:
   - `select_next_signal` — `None` ⇒ break with `Continue`.
   - Dequeue from the source queue (clear bit on
     `thread_pending` or `group_pending`).
   - Consult `proc.payload.sig_actions().get(sig)`.
   - Match disposition:
     - `Ignore` → `continue` (re-select).
     - `Default` → match `default_action(sig)`:
       - `Ignore` → `continue`.
       - `Term`/`Core` → return `DefaultTerminate { sig }`.
       - `Stop` → return `DefaultStop { sig }`.
       - `Cont` → return `DefaultContinue { sig }`.
     - `Handler(h)` → return `DeliverHandler { sig, handler: h }`.

`stop_requested` summary bit: per spec §15.1 "thread_future's next
poll handles the stop", `ast_check` returns `Continue` on stop-only
state. Day-1 doesn't have `thread_future`; `stop_requested` is set
by `post_signal` and observable via `interrupt_summary()` for tests.

### Tests (21 new)

In `signal/tests.rs::delivery`:

Pure (no zone setup):
1. `default_action_table_matches_spec`
2. `interrupt_summary_pack_unpack_round_trip`

`select_next_signal`:
3. `select_picks_lowest_signum_from_thread_pending`
4. `select_skips_masked_signals`
5. `select_prefers_thread_pending_over_group_pending`
6. `select_returns_none_when_all_masked_or_empty`
7. `select_falls_through_to_group_pending_when_thread_empty`

`ast_check` matrix:
8. `ast_check_continue_when_no_pending`
9. `ast_check_initiate_termination_for_summary_termination_bit`
10. `ast_check_default_terminate_for_sigterm`
11. `ast_check_default_ignore_for_sigchld_drops_and_continues`
12. `ast_check_default_stop_for_sigtstp`
13. `ast_check_default_continue_for_sigcont`
14. `ast_check_deliver_handler_when_handler_installed`
15. `ast_check_silent_ignore_disposition_drops_and_continues`

Summary maintenance:
16. `post_signal_marks_deliverable_when_unmasked`
17. `post_signal_skips_summary_deliverable_when_masked`
18. `sigprocmask_unblock_sets_deliverable_for_already_pending`
19. `sigkill_post_sets_summary_termination`
20. `sigstop_post_sets_summary_stop_requested`
21. `sigcont_post_clears_stop_requested`

## Spec compliance

| `SIGNAL_v1` says | Day-1 lands |
|---|---|
| §14: `select_next_signal` thread-pending first, then group-pending, both `!mask` | ✓ exact algorithm |
| §15.1: termination → return; loop dequeue + consult sig_actions | ✓ exact loop shape |
| §15.1: SIG_DFL Ignore + explicit SIG_IGN re-loop | ✓ |
| §15.1: stop_requested observed by thread_future, not AST | ✓ AST returns Continue on stop-only state |
| `THREAD_RUNTIME_v1` §5.2: summary atomic, kept current | ✓ AtomicU8, packed via InterruptSummary::{pack,unpack} |
| `THREAD_RUNTIME_v1` §5.2: post_signal updates summary | ✓ |
| `THREAD_RUNTIME_v1` §5.2: sigprocmask updates summary | ✓ |
| `THREAD_RUNTIME_v1` §5.2: SIGKILL sets termination on fan-out threads | ✓ at post_signal level |

## Deliberately deferred

- **Site A (wait-adapt) integration**: `THREAD_RUNTIME_v1` §5.4
  describes wait-adapt consulting `InterruptSource` predicate on
  every wake. Day-1 has no wait-adapt; `interrupt_summary()` is
  read-only-from-test today.
- **`build_signal_frame` / `SignalFrameInfo`**: SIGNAL_v1 §16 +
  HAL `SignalFrameIf`. `DeliverHandler` returns just `{sig,
  handler: usize}`; full frame layout, alternate stack, trampoline
  placement land with the AST trap-return wiring.
- **`invoke_group_exit_with_signal`**: SIGNAL_v1 §15.1 has
  `DefaultTerminate` actually invoke `process::step_exit_group`
  with an exit-status that encodes "killed by sig". Day-1 returns
  the intent; the caller (still-future) does the invocation. Our
  `step_exit_group` doesn't yet take a signum.
- **`sa_mask` accumulation during handler**: SIGNAL_v1 §16.1 — when
  a handler enters, the mask gains `sa_mask + sig` (unless
  `SA_NODEFER`). Day-1 records `Disposition::Handler` flat without
  `SaFlags`; SaFlags lands when sigaction grows the full POSIX
  shape.
- **SA_RESETHAND**: SIGNAL_v1 §18.1 — first delivery resets
  disposition to Default. Same dependency on full SaFlags.
- **Gewalt routing for SIGSTOP/SIGCONT**: SIGNAL_v1 §12.3's
  `route_sigstop` / `route_sigcont` invoke control ops directly
  (not pending queue). Day-1 collapses both into post_signal +
  default_action; the result is observable via `summary.stop_requested`
  but no actual `stop_state` machinery exists.
- **`select_next_signal` per-iteration mask re-read**: spec §14
  re-reads the mask every iteration; day-1 reads once per call (the
  inner loop in `ast_check` calls `select_next_signal` afresh, so
  the mask IS re-read across loop iterations). Per-call internal
  consistency is the same.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 272
  tests pass (251 prior + 21 new).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `signal: delivery sweep day-1 (select_next_signal + ast_check + summary maintenance)`
- `<this commit>` — `docs(progress): record signal delivery sweep day-1`

## Next step

The `process-topology` branch now carries 7 stacked commits:

1. Process / Thread / ProcessGroup / Session topology
2. Signal day-1 (post + observe)
3. Cred service stub
4. TTY pgrp typed `Cap<Session>` / `Cap<ProcessGroup>` rebinding
5. Kill permission check (`script_kill_*`)
6. TTY → signal end-to-end typed dispatch (`deliver_tty_dispatch`)
7. Signal delivery sweep day-1 (`select_next_signal`, `ast_check`,
   `signal_summary`)

Recommended follow-ups:

1. **`step_exit_group_with_signal(proc, sig)`** (~30 min). Lets
   `ast_check`'s `DefaultTerminate` callers actually invoke
   group-exit with the encoding "killed by sig". Single new field
   on `ProcessIdentity.exit_status` (or a separate `terminating_signal`
   slot). First real consumer of the `DefaultTerminate` outcome.
2. **Boot wiring** (~1 session). Thread `bootstrap_init_process`
   through `tx-kernel/src/init.rs`.
3. **Saved-set IDs on `Cred`** (~1 session). Adds `suid`/`sgid`,
   extends kill rule to Linux 4-way.
4. **Session→leader-pgrp index** (~30 min). Lets SIGHUP-on-hangup
   route via the typed bridge.
5. **`SaFlags` on `SigDisposition`** (~1 session). Unblocks
   SA_NOCLDSTOP, SA_NOCLDWAIT, SA_RESETHAND, SA_SIGINFO,
   SA_RESTART.

## Blockers

None.
