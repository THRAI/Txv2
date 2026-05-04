# Process / Thread topology day-1

**Date:** 2026-05-05
**Branch:** `process-topology`
**Status:** Complete. CI green (11 gates). 18/18 new tests pass.

## Goal

Land the entity graph (Process / Thread / ProcessGroup / Session) and
the lifecycle steps (`fork`, `exit`, `setpgid`, `setsid`) without
signal state, credentials, rlimits, or fd-table coupling. Topology
first, signals second so the entity shapes are not compromised by
signal semantics. After this pass, the next steps fill in slots on
`ThreadPayload` / `ProcessPayload` rather than reshape the entity
graph.

## What landed

### Module layout

Replaced the `pub mod process {}` / `pub mod thread_runtime {}` empty
stubs in [crates/tx-subsystems/src/lib.rs](../../../crates/tx-subsystems/src/lib.rs) with two real subsystems:

```
crates/tx-subsystems/src/
├── process/
│   ├── mod.rs                facade + re-exports
│   ├── structure.rs          types: ProcessIdentity, ProcessPayload,
│   │                                ProcessGroup, Session, Pid, Pgid, Sid
│   ├── execution.rs          steps: bootstrap_init_process, step_fork,
│   │                                step_exit_group, step_setpgid,
│   │                                step_setsid, step_zombie (internal)
│   └── tests.rs              12 tests
├── thread_runtime/
│   ├── mod.rs
│   ├── structure.rs          types: ThreadIdentity, ThreadPayload, Tid
│   ├── execution.rs          steps: step_thread_exit, set_thread_zombie
│   │                                (internal helper)
│   └── tests.rs              6 tests
```

`zones.rs` registers the six new zones (ProcessIdentity, ProcessPayload,
ProcessGroup, Session, ThreadIdentity, ThreadPayload).

### Ownership graph

```
ProcessIdentity ──Cap──▶ ProcessGroup ──Cap──▶ Session
    │  ▲                       │ ▲                  │ ▲
    │  └──Weak (members)───────┘ └──Weak (groups)───┘ │
    │                                                  │
    └──PayloadCap──▶ ProcessPayload                    │
                         │  ▲                          │
                         │  └──Weak (owner_proc)───────┘  (from ThreadIdentity)
                         │
                         └──Cap──▶ ThreadIdentity ──PayloadCap──▶ ThreadPayload
                                         │
                                         └──Weak──▶ ProcessIdentity
```

The cycle between `ProcessPayload.threads: Vec<Cap<ThreadIdentity>>`
and `ThreadIdentity.owner_proc` is broken with `Weak` on the
thread-side reverse pointer. Container entities (ProcessGroup, Session)
hold `Weak<ProcessIdentity>` / `Weak<ProcessGroup>` member lists so
containers do not retain their own contents.

### Identity-payload split

- `ProcessIdentity` keeps `pid`, `parent_pid`, `pgrp`, `exit_status`,
  `payload: SpinMutex<Option<PayloadCap<ProcessPayload>>>`. When the
  process zombifies (last thread exits or `step_exit_group`), payload
  is set to `None`; the identity persists until reaped. Pgrp
  membership lives on the **identity** so zombies remain in their pgrp
  per Linux semantics.
- `ThreadIdentity` keeps `tid`, `owner_proc` (Weak), `exit_status`,
  `payload: SpinMutex<Option<PayloadCap<ThreadPayload>>>`. Same
  zombie-retains-identity pattern.

### Step set

| Step | Notes |
|---|---|
| `bootstrap_init_process(aspace)` | Creates pid=1, fresh `Session` + `ProcessGroup` rooted at pid=1, single leader thread bound to the supplied aspace. |
| `step_fork::<P>(parent)` | Clones aspace via `AddressSpace::fork_aspace::<P>`, allocates new pid + leader tid, **inherits** parent's pgrp (registers in `pgrp.members`). Returns `ParentZombie` if parent's payload is gone. |
| `step_exit_group(process, status)` | Drains the threads list, marks each thread zombie, drops the process payload, sets process exit status. Identity persists. |
| `step_thread_exit(thread, status)` | Marks the thread zombie, removes it from the parent's threads list, calls `step_zombie` on the parent if this was the last thread. |
| `step_setpgid(target, new_pgid)` | Day-1 only supports `new_pgid == target.pid` (creates a fresh group inside the target's current session). Joining an existing group requires a session-walk; tracked as follow-up. |
| `step_setsid(target)` | Creates a fresh `Session` + leader `ProcessGroup` rooted at `target.pid`. |

### Tests (18)

`process/tests.rs`:
1. `bootstrap_init_creates_pid_1_with_session_and_pgrp`
2. `fork_creates_child_with_leader_thread_and_inherits_pgrp`
3. `fork_clones_address_space_into_distinct_cap`
4. `fork_registers_child_in_parent_pgrp_member_list`
5. `fork_on_zombie_parent_returns_parent_zombie`
6. `last_thread_exit_zombifies_process_keeps_identity`
7. `exit_group_zombifies_process_at_once_and_records_status`
8. `setpgid_to_target_pid_creates_new_group_in_same_session`
9. `setpgid_with_existing_group_id_is_unimplemented`
10. `setsid_creates_fresh_session_and_pgrp_at_target_pid`
11. `pid_pgid_sid_share_value_space_but_are_distinct_types`
12. `pgrp_member_weak_observation_returns_live_process_until_identity_drops`

`thread_runtime/tests.rs`:
1. `thread_exit_sets_status_and_drops_thread_payload`
2. `last_thread_exit_zombifies_owner_process`
3. `weak_owner_proc_upgrades_while_process_lives`
4. `weak_owner_proc_survives_payload_drop`
5. `weak_owner_proc_flips_dead_after_identity_drop`
6. `fork_assigns_distinct_tids_to_parent_and_child_leader_threads`

Tests serialise on `EPOCH_TEST_LOCK` because they share zone-allocated
state. Each test resets the pid/tid counters before running so
assertions on PID values are stable.

### Test-only re-export

`vm/mod.rs` gains `#[cfg(test)] pub(crate) use pmap::TestPmap;` so
`process/tests.rs` can call `step_fork::<TestPmap>` without touching
`crate::vm::pmap` directly.

## Deliberately deferred

- Signal state on `ThreadPayload` (`signal_mask`, `signal_summary`,
  `thread_pending`) and `ProcessPayload` (`sig_actions`, `group_pending`).
  Land in the signal pass.
- Credentials (`cred: CredSnapshotRef`), rlimits (`rlimits: RLimitBagRef`),
  fd table (`fd_table: FdTable`). Land with their respective services.
- Reactor coupling: `ThreadPayload.task: SpinMutex<Option<TaskKey>>` is
  always `None` for now. Wired in β4.
- `step_setpgid` joining an existing group by id (requires session-walk).
- TTY `session_pgrp` triplet conversion from raw IDs to typed
  `Weak<Session>` / `Weak<ProcessGroup>`. Small follow-up before β3.
- Boot wiring: `tx-kernel/src/init.rs` does not yet construct an init
  process. `bootstrap_init_process` is test-only for this pass.
- `step_clone3`, `step_execve`, `step_wait4`, `step_kill`. All depend on
  signal state, fd table, or ExecImage.
- `exec_aspace` rebuild half (still blocked on ExecImage).
- Reaping (`step_wait`): identity drops when the last `Cap` is released;
  zombie reap-by-parent semantics land with the wait step.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1`: 215 tests
  pass (197 prior + 18 new).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `process: land Process/Thread/ProcessGroup/Session topology day-1`
- `<this commit>` — `docs(progress): record process-topology day-1 completion`

## Blockers

None. The branch is ready to merge once reviewed.

## Next step

Pick one of:
1. **TTY pgrp typed-rebinding** (small): change `TtyIdentity.session_pgrp` raw-id triplet to typed `Weak<Session>` + `Weak<ProcessGroup>`. Required before signal job control fanout. ~1 session.
2. **Signal pass (β3)**: add `signal_mask`, `signal_summary`, `thread_pending`, `group_pending` fields and the `deliver_posix_signal` shim. Activates TTY job control. ~2 sessions.
3. **Cred service (β2-cred)**: small synchronous service feeding `cred: CredSnapshotRef` into `ProcessPayload`. ~1 session.
4. **Boot wiring**: thread the `bootstrap_init_process` call through `tx-kernel/src/init.rs` so the kernel actually has a pid=1 at runtime. Touches the init spine. ~1 session.

Recommended order: 1 → 2 → 3 → 4 (small TTY follow-up first, then
signals to activate job control, then cred for fork/exec, then boot
wiring once there's something for the boot spine to do).
