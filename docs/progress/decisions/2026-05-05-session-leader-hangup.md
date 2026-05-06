# Session-leader-tty hangup cascade (PROCESS_v1 §8.3)

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from boot wiring)
**Status:** Complete. CI green (11 gates). 317 tests pass (312 + 5 new).

## Goal

Materialise the session-leader-death cascade per `PROCESS_v1` §8.3
(amended in the P3 ratification pass to spell the two-hop weak
dereference). When a session leader exits, the kernel must:

1. SIGHUP+SIGCONT the foreground pgrp of the session's controlling
   tty (if any).
2. Clear the tty's `session_pgrp` slot (the authoritative side per
   OPA-3 TTY-CTL-1).
3. Clear the session's `controlling_tty` mirror.

The cascade was doc-spelled in the P3 pass but unimplemented. The
helper `Session::foreground_pgrp_cap()` (added in P3) does the
two-hop walk; the kernel-static init handle (added in boot wiring)
ensures children are reparented before the cascade fires. Both
prerequisites are now in place.

## What landed

### `session_leader_hangup_cascade` helper

```rust
fn session_leader_hangup_cascade(process: &Cap<ProcessIdentity>) {
    let pgrp = process.pgrp_cap();
    let session = pgrp.session_cap();

    // Day-1 single-namespace: session leader is the process whose
    // pid matches the session's sid (set at setsid time).
    if process.pid.0 != session.sid.0 {
        return;
    }

    // Phase 1 first hop: controlling tty.
    let Some(tty) = session.controlling_tty_cap() else {
        return; // No controlling tty — no-op.
    };

    // Phase 1 second hop: foreground pgrp.
    let fg_pgrp = tty.foreground_pgrp_cap();

    // Phase 2: SIGHUP + SIGCONT to fg pgrp (POSIX §11.1.3 — wake
    // stopped members so they can receive SIGHUP).
    if let Some(fg_pgrp) = fg_pgrp {
        let _ = signal::step_kill_pgrp(&fg_pgrp, Signum::SIGHUP);
        let _ = signal::step_kill_pgrp(&fg_pgrp, Signum::SIGCONT);
    }

    // Phase 3: clear tty's session_pgrp (authoritative).
    let _ = tty.clear_session_pgrp();

    // Phase 4: clear session.controlling_tty (mirror).
    *session.controlling_tty.lock() = None;
}
```

Wired into both exit paths *before* `sever_children` and payload
drop, so the cascade runs while the exiting process's signal-state
infrastructure is still observable:

```rust
pub fn step_exit_group(process, status) {
    session_leader_hangup_cascade(process);   // NEW
    sever_children(process);
    // ... drop payload + write exit_status ...
    post_sigchld_to_parent(process);
}

pub(crate) fn step_process_exit(process, status) {
    session_leader_hangup_cascade(process);   // NEW
    sever_children(process);
    *process.exit_status.lock() = Some(status);
    *process.payload.lock() = None;
    post_sigchld_to_parent(process);
}
```

### Cascade scenarios covered

The helper handles five distinct cases via short-circuits:

| Scenario | Cascade behavior |
|---|---|
| Non-session-leader exit | Skip entire cascade (early return at pid≠sid check) |
| Session leader, no controlling tty | Skip entire cascade (first-hop short-circuit) |
| Session leader, tty present, fg pgrp present | SIGHUP + SIGCONT to fg pgrp; clear both bindings |
| Session leader, tty present, fg pgrp absent (raw-id-only / reclaimed) | Skip SIGHUP step; **still** clear both bindings |
| Session leader is init itself | Same as session leader case — bootstrap init has sid=1=pid, so this fires whenever init exits with a controlling tty |

### Tests (5 new)

In [tty/tests/typed_session_pgrp.rs](../../../crates/tx-subsystems/src/tty/tests/typed_session_pgrp.rs)
(natural home — needs both TTY and process constructors):

- `session_leader_exit_with_controlling_tty_fires_sighup_sigcont_and_clears_binding`
  — full happy path: wire init's session ↔ tty ↔ fg-pgrp, exit init,
  assert tty.session_pgrp cleared and session.controlling_tty cleared.
- `session_leader_exit_with_live_fg_pgrp_member_delivers_sighup_and_sigcont`
  — fork a child to be a *surviving* fg-pgrp member (since init
  zombifies and can't be inspected after exit); exit init; assert
  child's leader has SIGHUP pending. SIGCONT is Gewalt (clears
  stop_requested rather than entering thread_pending), so we assert
  the Gewalt invariant (SIGCONT does NOT enter pending) too.
- `non_session_leader_exit_does_not_fire_cascade` — fork → setsid →
  fork-of-leader = grandchild (non-leader of new session); exit
  grandchild; assert tty + session bindings unchanged.
- `session_leader_exit_without_controlling_tty_is_noop` — bootstrap
  session has no tty; just exercises the no-op branch.
- `session_leader_exit_with_tty_but_no_fg_pgrp_clears_binding_without_signal`
  — install raw-id-only SessionPgrp (foreground_pgrp_cap returns
  None); exit session leader; assert tty.session_pgrp still cleared
  per spec ("must clear regardless of fg-pgrp upgrade").

### Existing test stability

All 27 pre-existing process tests stay green. Bootstrap tests have
no controlling tty (`assert!(!session.has_controlling_tty())` is a
common assertion); the cascade short-circuits at the first hop. No
test infrastructure changes needed.

## Spec compliance

| Spec | Pre | Post |
|---|---|---|
| §8.3 step 1: foreground-pgrp resolution via two-hop | ❌ absent | ✓ |
| §8.3 step 2: SIGHUP + SIGCONT to fg pgrp | ❌ absent | ✓ |
| §8.3 step 3: clear tty.session_pgrp | ❌ absent | ✓ |
| §8.3 step 4: clear session.controlling_tty (mirror) | ❌ absent | ✓ |
| §8.3 None-tolerance at each hop | ❌ N/A | ✓ |
| §8.3 atomicity note (class-3 compositional) | ❌ N/A | ✓ doc-acknowledged |
| §8.3 SIGHUP `siginfo` carrier | ❌ absent | ❌ deferred (no SigInfo type yet) |

## Deliberately deferred

- **`SigInfo` carrier for SIGHUP/SIGCONT.** Same blocker as the
  SIGCHLD producer's deferred siginfo. POSIX `si_code = SI_KERNEL`,
  no `si_pid`/`si_uid` — but the type doesn't exist yet.
- **Atomic ordering for the four substeps.** Per the doc note,
  steps are class-3 compositional. A future hardening could batch
  them into one publication boundary, but POSIX permits the
  intermediate-state visibility.
- **Non-session-leader exit clearing fg-pgrp slot.** If a fg-pgrp
  member exits, the pgrp's member-slot count drops via natural
  Weak staleness, but the tty's `foreground_pgrp` Weak doesn't
  recompact. This is the same "stale Weak" pattern that exists for
  every materialization in the impl; cleanup is on read-time,
  on-demand.
- **Stopped-member SIGHUP wake** (POSIX §11.1.3 "send SIGHUP and
  SIGCONT to every process in the orphaned process group" — the
  *orphaned* pgrp variant). Day-1 doesn't have stop-state machinery
  (THREAD_RUNTIME_v1 §6), so SIGCONT here is a defensive issue
  (Gewalt → clears summary.stop_requested, even if no member is
  stopped). Becomes meaningful when stop_state lands.
- **§8.2 orphan-pgrp SIGHUP/SIGCONT** — different cascade from §8.3.
  When a process exits orphans a pgrp (all members lose their
  parent's-pgrp tether), the orphaned pgrp gets SIGHUP+SIGCONT.
  Needs orphan detection (pgroup-wide parent-tether walk) AND
  stop_state to know which pgrps have stopped members.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 317
  tests pass (312 prior + 5 new).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `process: session-leader-tty hangup cascade in step_exit_group / step_process_exit (PROCESS_v1 §8.3)`
- `<this commit>` — `docs(progress): record §8.3 cascade landing`

## Next step

The day-1 process subsystem now closes:

- ✓ Topology (Process / Thread / ProcessGroup / Session)
- ✓ Signal day-1 (post + observe + Gewalt routing + ast_dispatch)
- ✓ Cred + permission check
- ✓ TTY pgrp typed dispatch + foreground-pgrp homing (P3)
- ✓ step_exit_group_with_signal materialising route_sigkill
- ✓ Process drift cleanup (4 mechanism fixes per spec)
- ✓ Children container + parent binding bidirectional
- ✓ SIGCHLD producer in step_process_exit / step_exit_group
- ✓ step_waitpid_nohang reaping
- ✓ Boot wiring + reparent-to-init
- ✓ §8.3 session-leader-tty hangup cascade

Remaining items are smaller, well-bounded:

1. **Pgrp selectors for waitpid** (`Pgrp(Pgid)` / `CallerPgrp`) —
   ~30min filter additions on the children walk in
   `step_waitpid_nohang`.
2. **`SigInfo` carrier** for the SIGCHLD/SIGHUP producers' deferred
   `si_*` populators. Independent.
3. **§8.2 orphan-pgrp SIGHUP** detection — when a non-session-leader
   parent exits, the cascade needs to scan members of the now-orphaned
   pgrp and SIGHUP+SIGCONT them if any member is stopped. Blocked
   on stop-state machinery (THREAD_RUNTIME_v1 §6).
4. **Blocking `waitpid`** — needs reactor channel integration to
   suspend the caller until a child fires its `exit_port` waker.
5. **First-userspace task submission** — init currently has no
   executing thread; the leader thread is constructed but not
   submitted to the reactor. Blocked on EXEC_v1 + userspace-stub.

## Blockers

None.
