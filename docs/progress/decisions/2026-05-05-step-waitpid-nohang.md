# `step_waitpid_nohang` (PROCESS_v1 §7.4 — WNOHANG variant)

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from SIGCHLD edge)
**Status:** Complete. CI green (11 gates). 307 tests pass (297 + 10 net).

## Goal

Materialise the synchronous WNOHANG path of `script_waitpid` per
`PROCESS_v1` §7.4. Day-1 callers can now reap zombie children
instead of accumulating them forever — closes the longest-standing
gap in the process subsystem's day-1 surface.

The blocking variant (POSIX default `waitpid` without WNOHANG) is
deferred — it needs reactor channel integration to suspend the
caller until a child's state changes.

## What landed

### Retention fix: `parent.children: Vec<Cap>` (was `Vec<Weak>`)

Implementing waitpid surfaced a real bug in the children-container
pass: `parent.children` was `Vec<Weak<ProcessIdentity>>`, which
doesn't retain. So a zombie child whose only retainer was its parent
would reclaim before reap, and `waitpid` could never see it.

Per `PROCESS_v1` §8.5 ("zombies stay in pgrp.members and
session.members until reap. Withdrawn at reap, not at exit.") and
§2.1 (where `children: DllContainer<ProcessIdentity>` is the
addressability-for-waitid binding), the children container must
retain. Switched to `Vec<Cap<ProcessIdentity>>`.

Asymmetry with `pgrp.members`: per spec §2.3, pgrp's retention is
held by `session.members` and by each member's `pgrp` binding —
*not* by `pgrp.members` entries. So `pgrp.members` correctly stays
`Vec<Weak<ProcessIdentity>>`.

Accessor surface change:
- `child_slot_count()` → `child_count()` (rename — every entry is
  always a valid `Cap` now; "slot" was Weak-shaped vocabulary).
- `live_children()` → `children()` (rename — there are no stale
  entries to filter; every entry is a live `Cap`).

### `step_waitpid_nohang` step

```rust
pub fn step_waitpid_nohang(
    parent: &Cap<ProcessIdentity>,
    target: WaitTarget,
) -> Result<(Pid, ExitStatus), WaitError>
```

Phase 1 — find a reapable child:
1. Snapshot `parent.children.lock().clone()` (cheap clone — `Cap`s
   are reference-counted).
2. For each child Cap matching the selector: track `any_match` and,
   if `is_zombie()`, capture as `reapable`.
3. Return `Err(NoneReady)` if matches found but none zombie;
   `Err(NoChildren)` if no matches at all.

Phase 2 — reap:
1. Read `child.pid` and `child.exit_status()`.
2. Withdraw from `parent.children`: `retain(|c| c.key() != key)` —
   releases parent's retention.
3. Withdraw from `child.pgrp.members` (Weak walk + retain).
4. Drop the local Cap. Identity becomes reclaimable after epoch
   drain (no other strong retainer).
5. Return `Ok((pid, exit_status))`.

### Selectors

```rust
pub enum WaitTarget {
    Any,           // waitpid(-1, ...)
    Pid(Pid),      // waitpid(pid > 0, ...)
}
```

Pgrp selectors (`waitpid(0, ...)` for caller's pgroup, `waitpid(-pgid, ...)`)
deferred — they're filter-only additions on the children walk and
land naturally as a follow-up.

### Error model

```rust
pub enum WaitError {
    NoChildren,    // POSIX ECHILD — no matching children at all
    NoneReady,     // WNOHANG no-zombie — POSIX returns 0
}
```

The two failure modes are kept distinct because they map to
different POSIX returns. The future syscall driver translates
`NoneReady` to a "0" return (per the WNOHANG convention) and
`NoChildren` to `ECHILD`.

### Tests (10 new)

- `waitpid_with_no_children_returns_no_children` — no fork, no
  children → ECHILD-equivalent.
- `waitpid_with_live_child_returns_none_ready` — fork but no exit
  → WNOHANG no-ready.
- `waitpid_any_reaps_zombie_child_and_returns_status` — happy
  path with `WaitTarget::Any`; demonstrates parent.children retains
  child even after test drops its external Cap.
- `waitpid_specific_pid_reaps_only_that_child` — two zombie
  children, `Pid(c2)` reaps c2; subsequent `Any` reaps c1. Confirms
  selector precision.
- `waitpid_specific_pid_with_no_match_returns_no_children` — pid
  selector that names a nonexistent child returns ECHILD-equivalent.
- `waitpid_specific_pid_with_live_match_returns_none_ready` — pid
  selector that matches a live (non-zombie) child returns
  NoneReady.
- `waitpid_reap_withdraws_from_parent_children_list` — after reap,
  `parent.child_count() == 0`.
- `waitpid_reap_withdraws_from_pgrp_members_list` — after reap,
  pgrp.member_slot_count drops by 1.
- `waitpid_reap_returns_signaled_status` — `step_exit_group_with_signal(SIGTERM)`
  → reap returns `Ok((pid, ExitStatus::Signaled(SIGTERM)))`.
- `waitpid_after_reaping_all_children_returns_no_children` — second
  reap on an empty children list returns ECHILD-equivalent.

### Test maintenance from the retention switch

Two existing tests assumed Weak-based shapes:

- `live_children_drops_stale_weak_after_child_identity_drops`
  (renamed `dropping_test_child_cap_leaves_parent_children_list_intact`):
  rewritten to assert the *new* invariant — dropping the test's
  external Cap does NOT release parent's retention. Children leave
  `parent.children` only via reap or parent reclaim.
- `pgrp_member_weak_observation_returns_live_process_until_identity_drops`:
  the test wanted to demonstrate that `pgrp.members`'s Weak goes
  stale once child identity is fully released. With parent.children
  retaining, dropping the test's external Cap is no longer
  sufficient — must also reap to release parent's retention.
  Updated to: zombify → drop test Cap → waitpid reap → drain →
  assert pgrp.members's Weak now stale.

## Spec compliance

| Spec | Pre | Post |
|---|---|---|
| §7.4 `script_waitpid` WNOHANG path | ❌ absent | ✓ `step_waitpid_nohang` |
| §7.4 blocking variant | ❌ absent | ❌ deferred (reactor channel integration) |
| §8.5 zombies retained in parent.children until reap | ❌ Weak (didn't retain) | ✓ Cap (retains) |
| §8.5 zombies retained in pgrp.members until reap | ✓ (pgrp's retention is via member.pgrp; pgrp.members shape correct) | ✓ |
| §2.1 children = DllContainer (addressability binding) | ❌ Weak | ✓ Cap (functionally equivalent) |
| §7.4 WEXITED/WSTOPPED/WNOHANG flag handling | ❌ absent | ❌ deferred (only WNOHANG-equivalent today) |
| §7.4 PidName withdrawal at reap | ❌ absent | ❌ deferred (single-ns, no PidName yet) |
| §7.4 RLIMIT_NPROC release at reap | ❌ absent | ❌ deferred (no rlimits) |

## Deliberately deferred

- **Blocking `waitpid`.** Needs reactor channel integration to
  suspend the caller until a child fires its `exit_port` waker.
  When `exit_port` machinery + reactor wait protocol land, the
  WNOHANG step becomes the inner loop body of an async `script_waitpid`.
- **Pgrp selectors** — `WaitTarget::Pgrp(Pgid)` and
  `WaitTarget::CallerPgrp` (`waitpid(0)`). Filter-only additions on
  the children walk; can land alongside `getpgid` / `tcgetpgrp`
  surfaces.
- **WCONTINUED, WUNTRACED, __WALL, __WCLONE.** Need stop-state
  machinery (THREAD_RUNTIME §6) and full clone-flag support.
- **`WaitOptions` flag struct.** Day-1 has the single-purpose
  `step_waitpid_nohang` because options conflate poorly with the
  Result return shape. When more flags land, a `WaitOptions`
  struct can wrap them.
- **`siginfo`-style return.** `wait4(2)` and `waitid(2)` populate
  a `siginfo_t` with `si_code` (`CLD_EXITED` / `CLD_KILLED` /
  etc.) — same blocker as the SIGCHLD producer's deferred
  siginfo carrier.
- **Auto-reap on SIGCHLD ignored / SA_NOCLDWAIT.** When the parent
  has SIGCHLD set to `Ignore` or installs a handler with
  `SA_NOCLDWAIT`, POSIX requires zombies to auto-reap at exit
  rather than persist. Lands when `SaFlags` is wired.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 307
  tests pass (297 prior + 10 net new — the 10 new waitpid tests
  plus 0 net change from the two stale-Weak test rewrites).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `process: parent.children retains via Cap (PROCESS_v1 §8.5)`
- `<this commit>` — `process: step_waitpid_nohang reaps zombie children (PROCESS_v1 §7.4 WNOHANG)`
- `<this commit>` — `docs(progress): record waitpid landing`

## Branch summary so far

The `process-topology` branch now carries 15 commits across:
1. Topology (Process / Thread / ProcessGroup / Session)
2. Signal day-1 (post + observe)
3. Cred service stub
4. TTY pgrp typed `Cap<Session>` / `Cap<ProcessGroup>` rebinding
5. Kill permission check
6. TTY → signal end-to-end typed dispatch
7. Signal delivery sweep (selection + ast_check + summary)
8. Gewalt vs event factoring restored
9. `step_exit_group_with_signal` materialises route_sigkill
10. `ast_dispatch` materialises ast_check's DefaultTerminate
11. Process drift cleanup (4 mechanism fixes per spec)
12. Foreground-pgrp ratification (P3 — TTY-owned)
13. Children container + sever-on-exit
14. SIGCHLD edge in step_process_exit
15. **`step_waitpid_nohang`** + parent.children retention fix

The day-1 process subsystem now closes the full reap cycle:
fork → exit → SIGCHLD-to-parent → waitpid → reap. The remaining
spec gaps are all in well-bounded follow-up territory: blocking
waitpid (reactor wait), boot wiring (kernel boundary), §8.3
session-leader-tty hangup, §8.2 orphan-pgrp SIGHUP/SIGCONT (needs
stop-state machinery), and the `siginfo` carrier.

## Next step

Of the items in the SIGCHLD-edge note's next-step list, post-waitpid:

1. **Boot wiring** (~1 session, cross-crate). Now most-
   foundational: makes pid=1 globally-addressable, lets
   `sever_children` upgrade to `reparent_children_to_init`, makes
   `step_process_exit` post SIGCHLD to init for orphans rather
   than skipping.
2. **§8.3 session-leader-tty hangup cascade.** Doc-spelled in P3;
   self-contained except for the `step_process_exit` site —
   detect "process is session leader" (compare process.pid against
   session.sid via process.pgrp_cap().session_cap()), then run the
   four-step cascade described in §8.3.
3. **`SigInfo` carrier** for SIGCHLD si_pid/si_code/si_status.
4. **Pgrp selectors** for waitpid (`Pgrp(Pgid)` / `CallerPgrp`).

Boot wiring is the most-foundational and unblocks #2's "if process
is session leader" detection cleanly (init being addressable lets
the session-leader-death cascade reparent before clearing tty).
But it crosses the tx-kernel/tx-subsystems crate boundary and
needs zone init + AddressSpace allocation in the kernel boot path.

## Blockers

None.
