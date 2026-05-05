# SIGCHLD edge in `step_process_exit` / `step_exit_group` (PROCESS_v1 §7.3.3 phase 5)

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from children container)
**Status:** Complete. CI green (11 gates). 297 tests pass (292 + 5 new).

## Goal

Materialise the `SIGCHLD` edge of the `step_process_exit` phase-5
cascade per `PROCESS_v1` §7.3.3:

> deliver_posix_signal(SignalTarget::Process(parent), SIGCHLD,
>     SigInfo { si_pid, si_uid, si_code: CLD_EXITED | CLD_KILLED | CLD_DUMPED,
>               si_status: exit_status, .. })

Day-1 scope: post `SIGCHLD` via the catchable shim. No `siginfo`
carrier yet (no `SigInfo` type in the day-1 signal surface), no
`exit_port` wake (no port machinery), no orphan-pgrp/SIGHUP cascade
(needs init-reparenting).

This pass picks the smallest, most independently-valuable item from
the children-container note's next-step list. Boot wiring is
deferred — it crosses the tx-kernel/tx-subsystems crate boundary and
needs zone init + AddressSpace allocation in the kernel boot path,
which is real architectural work. SIGCHLD edge needs neither.

## What landed

### `post_sigchld_to_parent` helper

```rust
fn post_sigchld_to_parent(process: &Cap<ProcessIdentity>) {
    if let Some(parent) = process.parent_cap() {
        let _ = crate::signal::step_kill_process(&parent, crate::signal::Signum::SIGCHLD);
    }
}
```

Goes through `signal::step_kill_process` — the catchable-signal shim
— rather than `route_gewalt`, because SIGCHLD is catchable. Inside,
`step_kill_process` dispatches to `post_signal` against the parent's
leader thread, populates `thread_pending`, and updates
`signal_summary.deliverable_signal` if the parent has SIGCHLD
unmasked (default mask is empty, so always unmasked at day-1).

Three None-tolerant cases all handled by short-circuits:
- `parent_cap()` returns `None` (init / orphan): outer `if let` skips.
- Parent is a zombie: `step_kill_process` returns
  `KillOutcome::NoLiveThread`; we discard the return.
- Parent's leader has SIGCHLD masked: `post_signal` queues to
  `thread_pending` but doesn't update `deliverable_signal` — handled
  inside the signal subsystem.

### Wired into both exit paths

```rust
pub fn step_exit_group(process: &Cap<ProcessIdentity>, status: ExitStatus) {
    sever_children(process);
    // ... zombify threads, drop payload, write exit_status ...
    post_sigchld_to_parent(process);     // §7.3.3 phase 5
}

pub(crate) fn step_process_exit(process: &Cap<ProcessIdentity>, status: ExitStatus) {
    sever_children(process);
    *process.exit_status.lock() = Some(status);
    *process.payload.lock() = None;
    post_sigchld_to_parent(process);     // §7.3.3 phase 5
}
```

Both paths produce zombies, both must produce SIGCHLD. The post runs
**after** zombification so the parent observes a complete zombie
when it acts on SIGCHLD (avoids a window where the parent's handler
or `wait(2)` walks children and sees the would-be reapable child as
not-yet-zombified).

Note: `sever_children` clears the *exiting process's children's*
parent slots (downward sever). The exiting process's *own* `parent`
slot is unchanged — that's how `post_sigchld_to_parent` resolves the
recipient.

### Tests (5 new)

- `child_exit_via_step_exit_group_posts_sigchld_to_parent` — happy
  path: `step_fork(parent) → step_exit_group(child)` ⇒ parent's
  leader has SIGCHLD pending.
- `child_exit_via_last_thread_cascade_posts_sigchld_to_parent` —
  same outcome through `step_thread_exit → step_process_exit`.
  Confirms both exit paths fire the producer.
- `bootstrap_init_exit_does_not_panic_with_no_parent` — init has no
  parent; SIGCHLD producer short-circuits cleanly.
- `orphaned_child_exit_does_not_post_sigchld` — parent exits first
  (severs child); child's later exit walks `parent_cap() = None` and
  short-circuits. No panic, no error.
- `zombie_parent_does_not_receive_sigchld` — pathological window
  where child still holds a Weak<parent> but parent is a zombie:
  `step_kill_process` returns `NoLiveThread`, we discard. Constructed
  by manually clearing `parent.children` before parent exit so
  sever doesn't run on the child (real flows always sever, so this
  state isn't reachable through normal paths — it's a defensive
  shape).

### Verification of no regressions

Full suite ran clean before adding new tests (292 → 292) — the
SIGCHLD post is observable only through `thread_pending` /
`signal_summary` reads, and no existing test asserts on those for
the bootstrap-init thread after a fork+child-exit sequence.

After adding the new tests: 292 + 5 = 297, all pass.

## Spec compliance

| Spec | Pre | Post |
|---|---|---|
| §7.3.3 phase 5 SIGCHLD to parent | ❌ absent | ✓ catchable post (no siginfo) |
| §7.3.3 phase 5 `exit_port` wake | ❌ absent | ❌ deferred (port machinery) |
| §7.3.3 phase 5 reparenting (full) | ❌ absent | ❌ deferred (sever-only stub from prior pass) |
| §7.3.3 phase 5 orphan-pgrp SIGHUP/SIGCONT (§8.2) | ❌ absent | ❌ deferred |
| §7.3.3 phase 5 session-leader-tty hangup (§8.3) | ❌ absent | ❌ deferred (doc-spelled in P3 pass) |

## Deliberately deferred

- **`SigInfo` carrier.** Spec wants `si_pid`, `si_uid`, `si_code`
  (`CLD_EXITED` / `CLD_KILLED` / `CLD_DUMPED`), `si_status`. Day-1
  signal surface has no `SigInfo` type — `post_signal` takes only
  `Signum`. When the carrier lands, `post_sigchld_to_parent` extends
  to populate it from `process.exit_status()` (Exited int → CLD_EXITED;
  Signaled sig → CLD_KILLED; +core_dump bit → CLD_DUMPED) and
  `process.pid` / `process.cred()`.
- **`exit_port` wake** for `pidfd` subscribers, ptrace tracers,
  direct waitpid observers. Needs the port machinery from
  `SIGNAL_ATTACHMENTS_v1`.
- **SIGCHLD ↔ `wait(2)` interaction.** Default `Ignore` action +
  `SA_NOCLDWAIT` semantics: when the parent has either, zombies
  should auto-reap. Not yet — zombies persist forever in day-1
  until the (future) `script_waitpid` reaps them.
- **Parent re-routing on death.** When the parent dies between
  child's zombification and `wait(2)`, spec §8.1 reparents the
  zombie to init; init then reaps. Day-1 sever-only stub leaves the
  child with no parent — `wait(2)` can't run. Ratifies when boot
  wiring lands and reparent-to-init replaces sever.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 297
  tests pass (292 prior + 5 new).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `process: post SIGCHLD to parent in step_exit_group / step_process_exit (PROCESS_v1 §7.3.3 phase 5)`
- `<this commit>` — `docs(progress): record SIGCHLD edge landing`

## Next step

The remaining items from the children-container note's next-steps
list, in approximate cost order:

1. **`script_waitpid`** (~half-day). Walks `caller.children`,
   identifies zombies (`exit_status.is_some()`), reaps:
   structural_withdraw from `parent.children` and from
   `pgrp.members`, drop the `Cap<ProcessIdentity>` retainer, return
   `(pid, exit_status)`. Day-1 WNOHANG only (blocking variant needs
   reactor wait integration). Now buildable on top of children
   container + SIGCHLD producer.
2. **Boot wiring** (cross-cutting, ~1 session). Adds tx-subsystems
   dependency to tx-kernel; calls `zones::register_all()` and
   `bootstrap_init_process()` from the kernel boot path; stashes the
   init Cap in a static. Lets `sever_children` upgrade to
   `reparent_children_to_init` (proper §8.1) and gives every
   subsystem a kernel-static init reference.
3. **`SigInfo` carrier** (signal subsystem work). Populates the
   SIGCHLD `si_*` fields per spec. Independent.
4. **§8.2 orphan-pgrp SIGHUP/SIGCONT detection.** When a process
   exits, scan its now-orphaned pgrps; if any contain stopped
   members, fanout SIGHUP+SIGCONT. Needs stop-state machinery
   (THREAD_RUNTIME_v1 §6) which is also deferred.
5. **§8.3 session-leader-tty hangup cascade.** Doc-spelled in the
   P3 pass; uses `Session::foreground_pgrp_cap()` for the two-hop
   dereference. Self-contained from this side; just needs to be
   added to `step_process_exit` when the exiting process is a
   session leader.

## Blockers

None.
