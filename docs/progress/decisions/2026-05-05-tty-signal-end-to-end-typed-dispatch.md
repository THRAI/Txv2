# TTY → signal end-to-end typed dispatch

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from kill-permission check)
**Status:** Complete. CI green (11 gates). 5 new tests pass; total suite 251.

## Goal

Close the last raw-id seam in TTY's job-control flow. Before this
pass:

1. TTY's `SessionPgrp` already carried typed `Weak<Session>` /
   `Weak<ProcessGroup>` (from the typed-rebinding pass), and
   `TtyIdentity` exposed `foreground_pgrp_cap()`.
2. `signal::script_kill_pgrp(source, &Cap<ProcessGroup>, sig)` ran the
   cred-checked fanout (from the kill-permission pass).

But the bridge in between was raw u32: `SignalTarget::ForegroundProcessGroup(u32)`,
`SignalTarget::CallerProcessGroup(u32)` etc. The ioctl/hangup steps
emitted u32-only dispatches; downstream code that actually wanted to
deliver had to re-resolve the pgrp from the id.

This pass makes `SignalDispatch` carry both the legacy id (for
comparison / observability) and a typed `Weak<ProcessGroup>` for
direct upgrade-and-route, then adds a single `signal::deliver_tty_dispatch`
helper that walks the bridge.

## What landed

### `SignalTarget` becomes hybrid struct variants

```rust
pub enum SignalTarget {
    ForegroundProcessGroup    { pgid: u32, pgrp: Option<Weak<ProcessGroup>> },
    SessionLeaderProcessGroup { pgid: u32, pgrp: Option<Weak<ProcessGroup>> },
    CallerProcessGroup        { pgid: u32, pgrp: Option<Weak<ProcessGroup>> },
}

impl SignalTarget {
    pub const fn pgid(&self) -> u32;
    pub fn pgrp_weak(&self) -> Option<&Weak<ProcessGroup>>;
}
```

`PartialEq` is manual: the typed `Weak` slot is intentionally not
compared (same rationale as `SessionPgrp` and now `IoctlCaller`).
Legacy raw-id tests continue matching.

### `IoctlCaller` gains a typed `pgrp` slot

```rust
pub struct IoctlCaller {
    pub session_id: u32,
    pub pgrp_id: u32,
    pub pgrp: Option<Weak<ProcessGroup>>,   // NEW
    pub is_session_leader: bool,
    pub has_controlling_tty: bool,
    pub in_foreground: bool,
    pub sigttin_ignored: bool,
    pub sigttou_ignored: bool,
}

impl IoctlCaller {
    pub fn with_pgrp_weak(self, pgrp: Weak<ProcessGroup>) -> Self;
    // ... existing builders unchanged
}
```

Existing `IoctlCaller::new(session_id, pgrp_id)` constructor leaves
`pgrp` as `None`; the new `with_pgrp_weak()` builder lets a future
syscall driver attach the caller's typed pgrp. `PartialEq` ignores
the typed slot for the same reason as above.

### TTY ioctl/hangup steps populate the typed slot

Three call sites now thread the typed Weak through:

- `step_ioctl_tiocswinsz` → `SignalTarget::ForegroundProcessGroup { pgrp: binding.foreground_pgrp, ... }`
- `deferred_signal_for_tty` (used by step_ingest VINTR/VQUIT/VSUSP) →
  same pattern
- `step_hangup` SIGCONT path → same pattern
- `step_hangup` SIGHUP path → leaves `pgrp: None`. The session
  leader's pgrp is conceptually `pgid == session_leader_pgid`; the
  binding doesn't carry it as a Weak. Resolves once a session→
  leader-pgrp lookup lands.
- `background_read_signal` / `background_write_signal` (require_fg_pgrp.rs):
  use `caller.pgrp` (the new typed slot) to populate
  `CallerProcessGroup`.

### `signal::deliver_tty_dispatch` bridge

```rust
pub fn deliver_tty_dispatch(
    source: &Cap<ProcessIdentity>,
    dispatch: SignalDispatch,
) -> Result<DispatchOutcome, Errno>;

pub enum DispatchOutcome {
    Delivered { count: u32 },
    PgrpDropped,
    NoTypedPgrp,
}
```

Walks the bridge in three steps:
1. Pull the dispatch's typed `Weak<ProcessGroup>` (`None` →
   `NoTypedPgrp` for legacy raw-id binders; caller falls back).
2. Upgrade under a single epoch guard (`None` → `PgrpDropped`).
3. Map `JobControlSignal` to `Signum` (centralised in
   `signum_for_job_control`) and call the cred-checked
   `script_kill_pgrp`.

`signum_for_job_control` is also published so callers that want to
post a single signum without going through the full bridge can do so:
- `Int → SIGINT`, `Quit → SIGQUIT`, `Tstp → SIGTSTP`,
  `Ttin → SIGTTIN`, `Ttou → SIGTTOU`, `Hup → SIGHUP`,
  `Cont → SIGCONT`, `Winch → Signum(28)` (Linux SIGWINCH).

### Tests (5 new)

In `signal/tests.rs::tty_bridge`:

1. `signum_for_job_control_maps_known_signals` — round-trip of every
   `JobControlSignal` variant.
2. `deliver_tty_dispatch_with_no_typed_pgrp_returns_no_typed_pgrp` —
   legacy raw-id dispatch path returns sentinel; caller routes
   elsewhere.
3. `typed_tty_vintr_routes_sigint_to_foreground_pgrp` —
   end-to-end: bind TTY typed-style → emit dispatch with typed Weak →
   `deliver_tty_dispatch` posts SIGINT to all 2 members of the fg
   pgrp; both leader threads observe SIGINT pending.
4. `deliver_tty_dispatch_skips_members_when_source_lacks_permission` —
   parent at uid=1000 (no caps); fg pgrp = {parent, child=root}; SIGINT
   delivers only to parent because cred check denies child. Per-member
   independence per `SIGNAL_v1` §12.2.
5. `deliver_tty_dispatch_zombie_source_returns_esrch` — composes
   correctly with `script_kill_pgrp`'s zombie-source check.

The 6 existing legacy `SignalTarget::Foreground/Session/CallerProcessGroup(u32)`
construction sites in `tty/tests/legacy_phase_a.rs` were converted to
`{ pgid: ..., pgrp: None }` — semantic-equivalent (PartialEq ignores
the typed slot).

## Spec compliance

| `SIGNAL_v1` says | Day-1 lands |
|---|---|
| TTY hangup → `SignalTarget::ProcessGroup(session_leader_pgrp)` | ✓ shape; `pgrp` left `None` until session→leader-pgrp index lands |
| TTY VINTR → `SignalTarget::ProcessGroup(fg_pgrp)` | ✓ with typed Weak |
| TTY background-IO → `SignalTarget::ProcessGroup(caller_pgrp)` | ✓ via `IoctlCaller.pgrp` |
| Per-member delivery is independent (§12.2) | ✓ permission denials per-member, no fail-fast |
| Producer catalog: `deliver_posix_signal(...)` | ✓ via `script_kill_pgrp` (which is the day-1 deliver_posix_signal for the pgrp variant) |

## Deliberately deferred

- **Session leader pgrp lookup**: SIGHUP-on-hangup wants the
  session-leader's pgrp specifically (`pgid == session_leader_pgid`).
  Day-1 `SessionPgrp` only exposes the *foreground* pgrp Weak.
  Adding a `session.leader_pgrp` accessor (walks `session.groups`
  for the matching pgid) is a small follow-up; until then SIGHUP
  dispatches with `pgrp: None` and consumers fall back to the
  numeric pgid.
- **`IoctlCaller` driver**: the syscall driver that holds a real
  `Cap<ProcessIdentity>` and constructs `IoctlCaller` with
  `with_pgrp_weak(caller_proc.pgrp_cap().downgrade())` lands when
  scripts/ exists.
- **SIGWINCH constant on `Signum`**: synthesised at the `signum_for_job_control`
  call site (`Signum::new(28)`). Promoting to a named constant is
  cosmetic.
- **`sys_kill(pid_t, ...)` syscall**: still no pid registry.
- **Numeric-pgid fallback path** for `DispatchOutcome::NoTypedPgrp`:
  no consumer exists yet to need one.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 251
  tests pass (246 prior + 5 new).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `tty+signal: typed SignalTarget + deliver_tty_dispatch bridge`
- `<this commit>` — `docs(progress): record TTY → signal end-to-end typed dispatch`

## Next step

The `process-topology` branch now carries 6 stacked commits:

1. Process / Thread / ProcessGroup / Session topology
2. Signal day-1 (post + observe)
3. Cred service stub (setuid/setgid + cred-on-payload)
4. TTY pgrp typed `Cap<Session>` / `Cap<ProcessGroup>` rebinding
5. Kill permission check (script_kill_*)
6. TTY → signal end-to-end typed dispatch (this pass)

Recommended follow-ups:

1. **Signal delivery sweep** (~2 sessions). Reactor-side step that
   consults `signal_mask`, `pending`, `sig_actions` to resolve a
   deliverable signal, optionally invokes a handler, applies the
   default action. First real consumer of `SigDisposition`.
2. **Boot wiring** (~1 session). Thread `bootstrap_init_process`
   through `tx-kernel/src/init.rs`.
3. **Saved-set IDs on `Cred`** (~1 session). Adds `suid`/`sgid`,
   extends kill rule to Linux 4-way.
4. **Session→leader-pgrp index** (~30 min). Lets SIGHUP-on-hangup
   route via the typed bridge instead of returning `pgrp: None`.

## Blockers

None.
