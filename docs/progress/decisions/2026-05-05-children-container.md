# Children container on `ProcessIdentity` (PROCESS_v1 §2.1)

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from foreground-pgrp ratification)
**Status:** Complete. CI green (11 gates). 292 tests pass (285 + 7 new).

## Goal

Pair the `parent: Weak<ProcessIdentity>` field with the matching
downward materialization — `children` — and wire the day-1 reparent
stub into both exit paths. Per `PROCESS_v1` §2.1 the spec shape is
`children: DllContainer<ProcessIdentity>`; day-1 uses the same
`SpinMutex<Vec<Weak<...>>>` pattern as `pgrp.members` and
`session.members`.

This is the natural follow-up to the previous drift cleanup pass
(which added `parent`) — completes the bidirectional binding so
future reparenting (§8.1), `script_waitpid` (§7.4), orphan-pgrp
SIGHUP detection (§8.2), and the SIGCHLD edge of `step_process_exit`
(§7.3.3 phase 5) all have the structural slot they need.

## What landed

### `children` field on `ProcessIdentity`

```rust
pub struct ProcessIdentity {
    pub pid: Pid,
    pub(crate) parent: SpinMutex<Option<Weak<ProcessIdentity>>>,
    pub(crate) children: SpinMutex<Vec<Weak<ProcessIdentity>>>,  // NEW
    pub(crate) pgrp: SpinMutex<Cap<ProcessGroup>>,
    pub(crate) exit_status: SpinMutex<Option<ExitStatus>>,
    pub(crate) payload: SpinMutex<Option<PayloadCap<ProcessPayload>>>,
}
```

Members held weakly because retention runs through each child's own
Cap chain — children outlive the parent's exit (zombies can be reaped
later), so the parent must not pin them. The container is purely a
materialization; walkers re-validate via `Weak::upgrade` under an
epoch guard.

Two new public accessors:
- `child_slot_count() -> usize` — cheap raw count including stale
  Weak entries (mirrors `pgrp.member_slot_count()`).
- `live_children() -> Vec<Cap<ProcessIdentity>>` — snapshots the
  container, upgrades under a single epoch guard, filters out stale
  entries, returns owned `Cap`s. Lock is released before return.

### `step_fork` pushes child into parent.children

```rust
// Register child in parent's pgrp.
parent_pgrp.members.lock().push(child_proc.downgrade());

// Register child in parent's children list. Materialization of the
// upward `parent` binding per PROCESS_v1 §2.1.
parent.children.lock().push(child_proc.downgrade());
```

Pairs with the existing `parent.downgrade()` push into the child's
`parent` slot — bidirectional binding now established at fork time.

### `sever_children` helper for §8.1 day-1 stub

```rust
fn sever_children(process: &Cap<ProcessIdentity>) {
    let snapshot: Vec<Weak<ProcessIdentity>> = process.children.lock().clone();
    let guard = tx_substrate::epoch::guard();
    for weak in snapshot {
        if let Some(child) = weak.upgrade(&guard) {
            *child.parent.lock() = None;
        }
    }
}
```

Walks the children list under an epoch guard, clears each live
child's `parent` slot. Severs the upward link without moving the
child into init's children list — day-1 has no globally-addressable
init handle. After sever, a child's `parent_cap()` returns `None`
and `parent_pid()` returns `Pid::RESERVED`, identical to init itself.

The "real" reparent-to-init operation (or subreaper-ancestor lookup
per `PR_SET_CHILD_SUBREAPER`) lands when an init handle becomes
globally-addressable (boot wiring) — at that point `sever_children`
becomes `reparent_children_to(init_weak)` and pushes severed
children into init's children list.

Wired into both exit paths:
- `step_exit_group(proc, status)` — explicit group-exit syscall.
- `step_process_exit(proc, status)` — last-thread cascade from
  `step_thread_exit`.

Both paths produce zombies; both must sever. Sever runs **before**
payload drop / exit_status write so the parent identity is still
fully retained when children walk back. (Children's `parent` Weaks
upgrade to the parent's still-live Cap during the sever walk — but
we don't actually need to upgrade them; we just write `None` to
each child's `parent` slot directly.)

### Tests (7 new)

- `bootstrap_init_has_no_children` — invariant: pid=1 starts empty.
- `fork_pushes_child_into_parent_children_list` — basic happy path.
- `multiple_forks_accumulate_in_parent_children_list` — three forks,
  three live entries; confirms order-independent set membership.
- `live_children_drops_stale_weak_after_child_identity_drops` —
  child identity drop leaves stale slot; `child_slot_count` stays
  at 2 (no compaction) but `live_children().len()` returns 1.
- `parent_exit_via_step_exit_group_severs_children_parent_slot` —
  `step_exit_group` path: child's `parent_pid()` flips to
  `Pid::RESERVED` after parent exits.
- `parent_exit_via_last_thread_cascade_severs_children_parent_slot`
  — `step_thread_exit → step_process_exit` path: same outcome
  through the last-thread cascade.
- `child_severance_does_not_affect_grandchildren` — sever is
  shallow: parent's exit only severs direct children; grandchildren
  keep their parent (the now-orphaned child).

## Spec compliance

| Spec | Pre | Post |
|---|---|---|
| §2.1 `children: DllContainer<ProcessIdentity>` | ❌ absent | ✓ as `SpinMutex<Vec<Weak<...>>>` |
| §8.1 children severed at parent exit | ❌ absent | ✓ day-1 stub (sever; reparent-to-init future) |
| §7.3.3 phase 5 SIGCHLD/exit_port/orphan-pgrp/session-leader-tty | ❌ absent | ❌ still future (now structurally enabled) |
| §7.4 `script_waitpid` | ❌ absent | ❌ still future (now structurally enabled) |

## Deliberately deferred

- **Reparent-to-init.** Day-1 sever is a no-op move (parent slot
  cleared but no destination). Real reparent lands with boot wiring
  giving us a globally-addressable init handle.
- **§7.3.3 phase 5 cascade** — SIGCHLD-to-parent, `exit_port` wake,
  orphan-pgrp SIGHUP/SIGCONT (§8.2), session-leader-controlling-tty
  hangup (§8.3). All structurally enabled now (the children list
  exists, the §8.3 cascade has a doc-spelled `Session::foreground_pgrp_cap()`
  to walk through), but each requires its own producer landing.
- **`script_waitpid`.** Reads parent.children, finds zombies (children
  with `exit_status.is_some()`), reaps via `structural_withdraw`.
  Now buildable; deferred until a syscall driver consumes it.
- **Children-list compaction.** Stale-Weak entries accumulate
  forever (until parent reclaims). `live_children()` filters at
  read time. A future cleanup step could prune at fork-time or
  exit-time; not needed for correctness.
- **`structural_move` substrate primitive.** Spec §8.1 prescribes
  this for reparenting; we use direct lock+update. Substrate work.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 292
  tests pass (285 prior + 7 new).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `process: add children container + sever-on-exit (PROCESS_v1 §2.1 + §8.1 day-1 stub)`
- `<this commit>` — `docs(progress): record children container landing`

## Next step

The natural sequence from here:

1. **Boot wiring** to make pid=1 globally addressable. Lets us
   upgrade `sever_children` to `reparent_children_to_init`, and
   gives a kernel-static init handle for everything that needs one.
2. **§7.3.3 phase 5 SIGCHLD edge** — `step_process_exit` calls
   `deliver_posix_signal(SignalTarget::Process(parent), SIGCHLD, ...)`
   if a parent is still alive. Now possible: just walk
   `process.parent_cap()` and skip if None.
3. **`script_waitpid`** — first real waitpid surface. Walks
   `caller.children`, picks zombies, reaps. Pairs naturally with the
   SIGCHLD producer above.
4. **§8.3 session-leader-death cascade** — uses
   `Session::foreground_pgrp_cap()` (doc-spelled in the previous
   pass) to fanout SIGHUP+SIGCONT to fg pgrp, then severs both sides
   of the controlling-tty binding.

Of these, (1) is the most cross-cutting (touches `tx-kernel/src/init.rs`)
and unblocks (2)/(4); (3) can land independently.

## Blockers

None.
