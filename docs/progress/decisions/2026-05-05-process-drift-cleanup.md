# Process subsystem — drift cleanup against `PROCESS_v1`

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from `signal::ast_dispatch`)
**Status:** Complete. CI green (11 gates). 282 tests pass.

## Goal

Close the mechanism-level drift the doc/impl coherence audit
(`docs/design/04_process-signals/PROCESS_v1.md`) flagged. Out of
scope: deferred features (children DLL, GroupExit, leader_exit_status,
Frame, nsproxy, Binding<T> primitive, foreground_pgrp homing) — those
are roadmap items, not drift.

## What landed

### 1. `step_zombie` → `step_process_exit` (§7.3.3)

`process::execution`'s last-thread cascade was named `step_zombie`,
emphasising the side-effect rather than the verb. Spec §7.3.3 names
it `step_process_exit` because zombification is the *consequence* of
the process-exit step. Rename only; signature still `(proc, status)`
with the new typed status (see §3 below).

```rust
pub(crate) fn step_process_exit(process: &Cap<ProcessIdentity>, status: ExitStatus) {
    *process.exit_status.lock() = Some(status);
    *process.payload.lock() = None;
}
```

The full §7.3.3 commit phase (SIGCHLD to parent, `exit_port` wake,
reparenting, orphan-pgrp SIGHUP, session-leader controlling-tty
hangup) is still deferred — names the right slot, leaves the
side-effects for the children-container pass.

### 2. `Session.groups` → `Session.members` (§2.4)

Spec calls the DLL `members` for both `ProcessGroup` (members are
processes) and `Session` (members are pgrps). Impl had drifted to
`groups` on Session. Mechanical rename:

```rust
pub struct Session {
    pub sid: Sid,
    pub(crate) controlling_tty: SpinMutex<Option<Weak<TtyIdentity>>>,
    pub(crate) members: SpinMutex<Vec<Weak<ProcessGroup>>>,  // was: groups
}
```

Also renamed accessor `group_slot_count()` → `member_slot_count()`
to match `ProcessGroup`'s already-correct shape.

### 3. `ExitStatus` enum unification (§6.2)

`ProcessIdentity` carried two parallel slots —
`exit_status: SpinMutex<Option<i32>>` and
`terminating_signal: SpinMutex<Option<Signum>>` — that the previous
pass's decision note had flagged for unification. Spec §6.2 uses an
`ExitStatus` ADT throughout the priority rule. Land it:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitStatus {
    Exited(i32),                     // explicit step_exit_group(int)
    Signaled(crate::signal::Signum), // killed by signal
}

impl ExitStatus {
    pub fn wait_status_word(self) -> i32 { ... }   // 128+sig encoding
    pub fn terminating_signal(self) -> Option<Signum> { ... }
}
```

Field collapse:

```rust
pub struct ProcessIdentity {
    pub pid: Pid,
    pub(crate) parent: SpinMutex<Option<Weak<ProcessIdentity>>>,
    pub(crate) pgrp: SpinMutex<Cap<ProcessGroup>>,
    pub(crate) exit_status: SpinMutex<Option<ExitStatus>>,   // was two slots
    pub(crate) payload: SpinMutex<Option<PayloadCap<ProcessPayload>>>,
}
```

Step signatures:

```rust
pub fn step_exit_group(process: &Cap<ProcessIdentity>, status: ExitStatus) { ... }
pub fn step_exit_group_with_signal(process: &Cap<ProcessIdentity>, sig: Signum) {
    step_exit_group(process, ExitStatus::Signaled(sig));     // no encoding here
}
pub(crate) fn step_process_exit(process: &Cap<ProcessIdentity>, status: ExitStatus) { ... }
```

The day-1 `128 + sig` shell-convention encoding moves from the
step body into `ExitStatus::wait_status_word()`. Threads still
zombify with the encoded int (per `THREAD_RUNTIME_v1` §7.2 thread
exit_status is an int). The thread-side last-thread cascade promotes
its `i32` to `ExitStatus::Exited(int)` because signal-driven
termination doesn't reach the cascade — it goes through
`step_exit_group_with_signal` which records `Signaled(sig)` directly
before fanning out.

Accessor surface:
- `ProcessIdentity::exit_status() -> Option<ExitStatus>` (was `Option<i32>`).
- `ProcessIdentity::terminating_signal() -> Option<Signum>` derived
  from `exit_status` (kept-shape accessor).

### 4. `parent_pid: Pid` → `parent: SpinMutex<Option<Weak<ProcessIdentity>>>` (§2.1)

Bare `parent_pid: Pid` was a dead field (no consumer). Spec §2.1
wants `parent: Binding<ProcessIdentity>` so children, reparenting,
and `getppid` rendering can resolve through the actual parent
identity. Day-1 doesn't have substrate `Binding<T>`, so use the same
mechanism the rest of the impl uses:

```rust
pub(crate) parent: SpinMutex<Option<Weak<ProcessIdentity>>>,
```

`None` only for `pid=1` init. Held weakly so parent exit doesn't
pin children — children outlive their parent and reparent to init at
`step_process_exit` (cascade still future). Two new accessors:

```rust
pub fn parent_cap(&self) -> Option<Cap<ProcessIdentity>> {
    let weak = (*self.parent.lock())?;
    let guard = tx_substrate::epoch::guard();
    weak.upgrade(&guard)
}

pub fn parent_pid(&self) -> Pid {
    self.parent_cap().map(|p| p.pid).unwrap_or(Pid::RESERVED)
}
```

`bootstrap_init_process` constructs with `parent = None`; `step_fork`
constructs with `parent = Some(parent.downgrade())`. Tests now
exercise both `parent_pid()` (numeric) and `parent_cap()` (typed),
proving the binding actually resolves.

This unblocks the future children-DLL pass: children list is added
on `ProcessIdentity` and walkers re-validate via `child.parent_cap()`
matching the parent identity.

## Spec compliance

| Spec | Pre | Post |
|---|---|---|
| §7.3.3 step_process_exit name | ❌ `step_zombie` | ✓ |
| §2.4 Session.members | ❌ `groups` | ✓ |
| §6.2 ExitStatus ADT | ❌ split slots | ✓ |
| §2.1 parent binding | ❌ bare `Pid` | ✓ (Weak; Binding<T> when substrate lands) |

## Deliberately deferred

These remain gaps; this pass moved only the four mechanism items.

- **Children container** (§2.1 `children: DllContainer<ProcessIdentity>`).
  Now buildable on top of `parent: Weak<ProcessIdentity>` — next
  natural step.
- **`step_process_exit` side-effects** (§7.3.3 phase-5: SIGCHLD,
  exit_port wake, reparenting, orphan-pgrp SIGHUP, session-leader
  controlling-tty hangup). Blocked on children container.
- **GroupExit coordination** (§5). Blocked on substrate `AtomicOneShot<T>`.
- **`leader_exit_status`** (§6.1). Pairs with GroupExit.
- **Frame container** (§3). Blocked on VFS `Shared<T>` and fd_table.
- **`nsproxy` / `PidName`** (§9). Single-namespace day-1.
- **`Binding<T>` mechanism** (substrate primitive). Until then,
  `SpinMutex<Cap<...>>` / `SpinMutex<Option<Weak<...>>>` carry the
  same retention semantics.
- **`Session.foreground_pgrp` homing** (§2.4 vs typed-rebinding work).
  User-deferred.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 282
  tests pass (no test count change; renames + type swaps preserved
  every prior assertion, plus added `parent_cap` coverage).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `process: rename step_zombie → step_process_exit (PROCESS_v1 §7.3.3)`
- `<this commit>` — `process: rename Session.groups → Session.members (PROCESS_v1 §2.4)`
- `<this commit>` — `process+signal: unify exit_status into ExitStatus enum (PROCESS_v1 §6.2)`
- `<this commit>` — `process: parent_pid → parent: Weak<ProcessIdentity> (PROCESS_v1 §2.1)`
- `<this commit>` — `docs(progress): record process drift cleanup`

## Branch summary so far

The `process-topology` branch now carries 11 commits (signal pass
through ast_dispatch + drift cleanup). Process subsystem types are
now spec-faithful at the field-name and primitive-mechanism level
for everything that doesn't depend on missing substrate primitives
(`Binding<T>`, `AtomicOneShot<T>`, intrusive DLL).

## Next step

Per the audit's implementation entry order, the natural next item is
the children container — spec-faithful field on `ProcessIdentity`
(`children: SpinMutex<Vec<Weak<ProcessIdentity>>>` matching the
existing pgrp.members pattern), wired into `step_fork` (push child
into parent's children) and `step_process_exit` (walk + reparent to
init). This unblocks `script_waitpid` and the rest of §7.3.3's
phase-5 cascade.

After that, the spec-foreground question (`Session.foreground_pgrp`
homing — TTY vs Session) needs a decision note before more code
lands on either side.

## Blockers

None.
