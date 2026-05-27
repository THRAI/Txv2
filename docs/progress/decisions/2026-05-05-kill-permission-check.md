# Kill permission check (cred → signal)

**Date:** 2026-05-05
**Branch:** `process-topology` (continued from TTY pgrp typed rebind)
**Status:** Complete. CI green (11 gates). 12 new tests pass; total suite 246.

## Goal

Make `cred` and `signal` interlock at their canonical seam per
`SIGNAL_v1.md` §32 and `cred_service_v_1`. Specifically: the syscall-
shaped `kill(target, sig)` consults `cred::require_signal_send` for
permission and only invokes `deliver_posix_signal` (== our existing
`step_kill_process`) on success. This is the first real consumer of
`Cred` outside cred's own setuid/setgid tests, and the first time the
signal shim distinguishes "internal producer post" from
"permissioned syscall script".

## Spec ground truth

| Doc | Says |
|---|---|
| [`SIGNAL_v1.md`](../../design/04_process-signals/SIGNAL_v1.md) §32 | Permission per cred — caller's euid must match target's euid (or `CAP_KILL`), except `SIGCONT` within the same session. |
| [`SIGNAL_v1.md`](../../design/04_process-signals/SIGNAL_v1.md) §32 script | `check_signal_permission(...)?; deliver_posix_signal(...)` — composition. |
| [`cred_service_v_1`](<../../design/02_execution/cred_service_v_1_draft (2).md>) §"Checks surface" | Canonical entry point: `require_signal_send(&Cred, &TargetProcCred, sig, &Guard) -> Result<SignalAuthorized<'g>, Errno>`. |
| [`cred_service_v_1`](<../../design/02_execution/cred_service_v_1_draft (2).md>) §"Cred witnesses" | `SignalAuthorized<'g>` is a **zero-sized provenance token**, guard-phantom-typed. |
| [`cred_service_v_1`](<../../design/02_execution/cred_service_v_1_draft (2).md>) §"Foreign value types from subsystems" | `TargetProcCred` is a **process-exported value** (not a `Cap`); cred consumes only the facts it needs. |
| [`cred_service_v_1`](<../../design/02_execution/cred_service_v_1_draft (2).md>) §"Not every operation is tokenized" | kill is **live-checked, no minted grant** — every call re-runs the cred rule. |

## What landed

### New types

```rust
// crates/tx-subsystems/src/cred.rs
pub struct SignalAuthorized<'g> { _guard: PhantomData<&'g ()>, _priv: () }

pub fn signal_permitted(source: Cred, target: &TargetProcCred, sig: Signum) -> bool;
pub fn require_signal_send<'g>(
    source: Cred,
    target: &TargetProcCred,
    sig: Signum,
    guard: &'g Guard<'_>,
) -> Result<SignalAuthorized<'g>, Errno>;

// crates/tx-subsystems/src/process/structure.rs
pub struct TargetProcCred {
    pub uid: Uid,
    pub euid: Uid,
    pub gid: Gid,
    pub egid: Gid,
    pub same_session: bool,
}

impl ProcessIdentity {
    pub fn target_proc_cred_for(&self, source: &ProcessIdentity)
        -> Option<TargetProcCred>;
}

// crates/tx-subsystems/src/signal.rs
pub enum KillScriptOutcome { Delivered, NoLiveThread, Probed }

pub fn script_kill_process(
    source: &Cap<ProcessIdentity>,
    target: &Cap<ProcessIdentity>,
    sig: Signum,
) -> Result<KillScriptOutcome, Errno>;

pub fn script_kill_probe(
    source: &Cap<ProcessIdentity>,
    target: &Cap<ProcessIdentity>,
) -> Result<KillScriptOutcome, Errno>;

pub fn script_kill_pgrp(
    source: &Cap<ProcessIdentity>,
    pgrp: &Cap<ProcessGroup>,
    sig: Signum,
) -> Result<u32, Errno>;
```

### `Errno` extensions

`crates/tx-subsystems/src/execution.rs` Errno enum gains
`EPERM` (POSIX `kill(2)` denial) and `ESRCH` (zombie source / no
such process). Both standard POSIX, will be reused by every
forthcoming syscall script.

### Day-1 permission rule

```rust
fn signal_permitted(source: Cred, target: &TargetProcCred, sig: Signum) -> bool {
    if sig == Signum::SIGCONT && target.same_session { return true; }
    if source.is_privileged_for(Capability::KILL)   { return true; }
    source.uid  == target.uid  || source.euid == target.euid
        || source.uid  == target.euid || source.euid == target.uid
}
```

This collapses Linux's 4-way `(uid,euid) × (uid,suid,ruid)` match
to `(uid,euid) × (uid,euid)` because day-1 `Cred` doesn't carry
saved-set IDs yet. When `suid`/`ruid` arrive, the predicate extends
without reshaping its callers — the `TargetProcCred` shape grows
fields and the rule consults them.

### `same_session` computation

`ProcessIdentity::target_proc_cred_for(source)` upgrades `self.pgrp`
and `source.pgrp` to `Cap<ProcessGroup>` then compares
`session_cap().key()`. The `Cap::key()` comparison is a pure slot-id
match — no upgrade-of-a-Weak risk. Returns `false` when either side's
pgrp resolves to a different session.

### Composition shape

`script_kill_process` matches the doc's `script_kill` shape:

```rust
let _auth = cred::require_signal_send(source_cred, &target_facts, sig, &guard)?;
match step_kill_process(target, sig) {
    KillOutcome::Delivered    => KillScriptOutcome::Delivered,
    KillOutcome::NoLiveThread => KillScriptOutcome::NoLiveThread,
}
```

The witness is consumed (dropped) immediately; cred's authority is
"provenance receipt", not "retained authorization".

`script_kill_pgrp` iterates `pgrp.members` per `SIGNAL_v1` §12.2,
runs the cred check per member, and posts on permitted ones. Per-
member denials and zombies are independent — they don't fail the
whole call. Returns the count of delivered processes.

`script_kill_probe` is POSIX `kill(pid, 0)` — the cred rule runs but
no signal is posted.

## Tests (12)

In `signal/tests.rs::kill_permission`:

Pure-rule tests (no zone setup):
1. `same_euid_passes`
2. `different_euid_fails_without_capability`
3. `cap_kill_overrides_euid_mismatch`
4. `root_overrides_euid_mismatch`
5. `sigcont_same_session_passes_regardless_of_uid`
6. `sigcont_different_session_still_requires_cred_match`

Integration tests against the real Process / PGroup graph:
7. `script_kill_process_same_uid_delivers`
8. `script_kill_process_different_uid_returns_eperm`
9. `script_kill_process_zombie_target_returns_no_live_thread`
10. `script_kill_process_zombie_source_returns_esrch`
11. `script_kill_pgrp_partial_permission_returns_count_of_permitted`
    (parent uid=1000, child_a uid=1000, child_b uid=2000 → 2 delivered)
12. `signal_zero_is_permission_probe_no_delivery`

Tests set creds directly via the test-only `set_cred` helper instead
of routing through `step_setuid`. Day-1 `step_setuid` retains
capabilities on deprivilege (the cap-drop transition machinery is
phase-2 per the cred doc); the kill rule is independent of how a
process arrived at its cred, so the tests construct cred state
directly.

## Deliberately deferred

- **Saved-set IDs (`suid`, `sgid`)**: when these land on `Cred`,
  `TargetProcCred` extends to carry them and the rule grows the
  full Linux 4-way match. No caller changes.
- **`dumpable` flag**: ptrace permission consults it; not part of
  the kill rule.
- **Numeric `step_kill(pid_t)` syscall shim**: needs a pid registry.
  Day-1 callers pass `Cap<ProcessIdentity>` directly.
- **Capability-drop on deprivilege in `step_setuid`**: phase-2 per
  the cred doc; signal tests construct creds directly to avoid
  depending on this behavior.
- **Migration to `subsystems/cred/checks/`**: the cred doc places
  `require_*` functions under `checks/`. Day-1 keeps them flat in
  `cred.rs`. Mechanical follow-up.

## Verification

- `cargo xtask ci` — 11/11 gates green.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 246
  tests pass (234 prior + 12 new).
- `cargo xtask progress validate` — ok.
- `cargo xtask lint docs` — ok.

## Commit ledger

- `<this commit>` — `signal+cred: kill permission check (script_kill_*)`
- `<this commit>` — `docs(progress): record kill permission check completion`

## Next step

The `process-topology` branch now carries 5 stacked commits:
1. Process / Thread / ProcessGroup / Session topology
2. Signal day-1 (post-and-observe shims)
3. Cred service stub (setuid/setgid + cred-on-payload)
4. TTY pgrp typed Cap<Session>/Cap<ProcessGroup> rebinding
5. Kill permission check (script_kill_process / pgrp / probe)

Recommended follow-ups:

1. **Migrate `IoctlCaller`/`SignalTarget` to typed refs** (~1 session).
   TTY job-control flow can route the foreground pgrp `Cap` directly
   into `signal::script_kill_pgrp` once `IoctlCaller` carries
   `Cap<ProcessIdentity>` instead of `caller.pgrp_id: u32`.
2. **Signal delivery sweep** (~2 sessions). Reactor-side step that
   consults `signal_mask`, `pending`, `sig_actions` and either
   invokes a handler or applies the default action. First real
   consumer of `SigDisposition`.
3. **Boot wiring** (~1 session). Thread `bootstrap_init_process`
   through `tx-kernel/src/init.rs`.
4. **Saved-set IDs on `Cred`** (~1 session). Adds `suid`/`sgid` and
   extends the kill rule to the full Linux 4-way match.

## Blockers

None.
