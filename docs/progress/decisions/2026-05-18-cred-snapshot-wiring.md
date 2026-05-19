# Cred snapshot wiring + cred::checks API

**Date:** 2026-05-18
**Branch:** `cc/eager-morse-6538b1` (worktree)
**Status:** Complete. 8 commits. `cargo -q xtask unit` green (332). `cargo test -p tx-subsystems --lib` 655/655.

## Goal

`cred_service_v_1` §"In flight" promised that the script-side credential is
"a by-value metadata copy captured *once* at syscall entry." The slot for it
on `SyscallCtx` was reserved by an explicit comment at
[`ctx.rs:31`](../../../crates/tx-shims/src/linux_syscall/ctx.rs) — but the
type didn't exist, `ctx.cred()` re-read the `ProcessPayload.cred`
`AtomicSlot<Cap<Cred>>` on every call, and four signal scripts duplicated the
same auth-then-commit dance with manual guard scoping. Cred authorization
itself was canonical (`require_signal_send`); the *wiring around it* was
unfactored.

This batch finishes the wiring, extracts the patterns into APIs, and uses the
audit it forced to close three real cred-bypass paths in `sys_kill` /
`sys_tkill`.

## Spec ground truth

| Doc | Says |
|---|---|
| [`cred_service_v_1`](<../../design/02_execution/cred_service_v_1_draft (2).md>) §"In flight" | Script holds a by-value metadata copy captured once at syscall entry. |
| [`cred_service_v_1`](<../../design/02_execution/cred_service_v_1_draft (2).md>) §"Checks surface" | `cred::checks::require_*(snapshot, foreign_input, &guard) -> Result<Authorized<'g>, Errno>`. |
| [`cred_service_v_1`](<../../design/02_execution/cred_service_v_1_draft (2).md>) §"Cred witnesses" | Zero-sized provenance tokens, `#[must_use]`, guard-phantom-typed. |
| [`cred_service_v_1`](<../../design/02_execution/cred_service_v_1_draft (2).md>) §"Foreign inputs consumed by cred" | Cred consumes subsystem-exported value types (`&InodeMeta`, `&TargetProcCred`), not live nodes. |
| [`SIGNAL_v1`](../../design/04_process-signals/SIGNAL_v1.md) §32 | Permission rule for kill / tkill / tgkill. |
| [`EXEC_v1`](../../design/02_execution/EXEC_v1.md) §5 | Captures `cred_snapshot` at prelude, threads through phases for racing-setuid resilience. |

## What landed

### 1. `CredSnapshot` first-class type

[`cred/mod.rs`](../../../crates/tx-subsystems/src/cred/mod.rs) +
[`process/structure.rs`](../../../crates/tx-subsystems/src/process/structure.rs).

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[must_use]
pub struct CredSnapshot { cred: Cred }

impl CredSnapshot {
    pub const fn from_cred(cred: Cred) -> Self;
    pub const fn root() -> Self;
    pub const fn cred(self) -> Cred;
    pub const fn as_cred(&self) -> &Cred;
    pub fn is_privileged_for(self, cap: Capability) -> bool;
}
impl From<Cred> for CredSnapshot;
impl AsRef<Cred> for CredSnapshot;

impl ProcessIdentity {
    pub fn cred_snapshot(&self) -> Option<CredSnapshot>; // None for zombies
}
impl ProcessPayload {
    pub fn cred_snapshot(&self) -> CredSnapshot;         // infallible
}
```

The wrapper gives the architectural distinction a name and leaves room for
future fields (generation tag for racing-setuid, NOSUID hint, retained
`Cap<Cred>`) without touching every check signature.

### 2. `SyscallCtx` captures snapshot once

[`tx-shims/.../ctx.rs`](../../../crates/tx-shims/src/linux_syscall/ctx.rs).

```rust
pub struct SyscallCtx<'a> {
    pub process, thread, aspace, mailbox, timer_wheel, delegate_registry,
    cred_snapshot: CredSnapshot,   // captured exactly once in SyscallCtx::new
    pub _lifetime: PhantomData<&'a ()>,
}

impl<'a> SyscallCtx<'a> {
    pub fn new(...) -> Self {
        let cred_snapshot = process.cred_snapshot().unwrap_or_else(CredSnapshot::root);
        Self { ..., cred_snapshot, ... }
    }
    pub fn cred(&self) -> Cred             { self.cred_snapshot.cred() }
    pub fn cred_snapshot(&self) -> &CredSnapshot { &self.cred_snapshot }
    pub fn walker_cred(&self) -> Credential { Credential::from(&self.cred_snapshot) }
}
```

`ctx.cred()` no longer re-reads the `AtomicSlot`. A mid-syscall `setuid` on
the same process cannot perturb authorization decisions already taken in the
script.

Also added `impl From<&CredSnapshot> for vfs::structure::Credential` so the
walker-side DAC projection goes directly from the snapshot, skipping an
intermediate `Cred` value-copy.

### 3. `cred::checks::*` authorization surface

New module [`cred/checks.rs`](../../../crates/tx-subsystems/src/cred/checks.rs).

**Witness predicates** — pure, one-input-one-witness, match design exactly:

```rust
pub struct SearchAuthorized<'g> { _guard: PhantomData<&'g ()>, _priv: () }
pub struct OpenAuthorized<'g>   { ...same shape... }
// (SignalAuthorized<'g> pre-existed; re-exported)

pub fn require_path_search<'g>(&CredSnapshot, &InodeMeta, &Guard<'_>) -> Result<SearchAuthorized<'g>, Errno>;
pub fn require_open<'g>(&CredSnapshot, &InodeMeta, OpenFileFlags, &Guard<'_>) -> Result<OpenAuthorized<'g>, Errno>;
pub fn require_signal_send<'g>(&CredSnapshot, &TargetProcCred, Signum, &Guard<'_>) -> Result<SignalAuthorized<'g>, Errno>;
```

Bit-level DAC math stays in `vfs::predicates` (`check_descend_perm`,
`check_open_perm`); this module is the authorization seam, owning the
witness type from the publication-authority side.

**Combinators** — wrap `require_*` with the snapshot-capture + foreign-value-
type resolution + guard-scope discipline that every cred-checked script
repeated:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum AuthOutcome { Authorized, NoLiveTarget }

pub fn authorize_signal_send(
    source: &Cap<ProcessIdentity>,
    target: &Cap<ProcessIdentity>,
    sig: Signum,
) -> Result<AuthOutcome, Errno>;

pub fn authorize_signal_send_under_guard(
    source_snapshot: &CredSnapshot,
    source: &Cap<ProcessIdentity>,
    target: &Cap<ProcessIdentity>,
    sig: Signum,
    guard: &Guard<'_>,
) -> Result<AuthOutcome, Errno>;
```

The `_under_guard` variant is for fanout loops (`script_kill_pgrp`) that
already hold one snapshot + guard across N iterations.

`AuthOutcome` is a 3-state enum (`Authorized` / `NoLiveTarget` /
`Err(Errno)`) because POSIX kill maps "no live target" → 0 or ESRCH and
"denied" → EPERM to different SyscallResult branches; folding them into one
`Result<(), Errno>` would have lost that distinction at the type level.

### 4. Signal-script reworks

[`signal/mod.rs`](../../../crates/tx-subsystems/src/signal/mod.rs).

Four scripts now go through the combinator (or its under-guard variant for
the fanout):

```rust
// Before:
{
    let guard = step_engine::guard();
    let source_snapshot = source.cred_snapshot().ok_or(Errno::ESRCH)?;
    let Some(target_facts) = target.target_proc_cred_for(source) else {
        return Ok(KillScriptOutcome::NoLiveThread);
    };
    let _auth = cred::require_signal_send(&source_snapshot, &target_facts, sig, &guard)?;
}
commit(target, sig, info);

// After:
match cred::checks::authorize_signal_send(source, target, sig)? {
    AuthOutcome::NoLiveTarget => return Ok(KillScriptOutcome::NoLiveThread),
    AuthOutcome::Authorized => {}
}
commit(target, sig, info);
```

The no-nested-guard discipline (auth guard must drop before
`post_signal`'s inner SigInfo-storage guard) is now a property of
`authorize_signal_send` itself, not a manual `{}` block at every site.

`script_kill_process` extended with `info: Option<SigInfo>` so sys_kill's
SI_USER block is preserved through the script path (the prior
`KillProcessOp` drive carried info; `script_kill_process` previously dropped
it).

New `script_deliver_signal(source, target, sig)` — cred-checked counterpart
to `deliver_posix_signal`. Resolves Thread → Process before taking the auth
guard scope (`upgrade_owner_proc` holds its own guard internally; nesting
would trip the no-nested-guard invariant). Commit-side passes
`SignalTarget::Process(target_proc)` to `deliver_posix_signal` rather than
the original `Thread` to avoid a *latent* nested-guard bug in that branch
that today isn't exercised (leader-thread tids aren't registered in
`PidName::Thread`).

### 5. Three real cred-bypasses closed

[`tx-shims/.../signal.rs`](../../../crates/tx-shims/src/linux_syscall/signal.rs).

| Syscall | Before | After |
|---|---|---|
| `sys_kill(pid > 0)` | `KillProcessOp::drive_oneshot` → `step_kill_process` (no cred check) | `script_kill_process` (cred-checked) |
| `sys_kill(pid == 0)` | `step_kill_pgrp` directly (no per-member check) | `script_kill_pgrp` (per-member cred check) |
| `sys_tkill(tid)` | `DeliverSignalOp::drive_oneshot` (no cred check) | `script_deliver_signal` |
| `sys_tgkill(tgid, tid)` | `ThreadKillOp::drive_oneshot` | **unchanged** — tgid==caller-pid constraint means trivially permitted today; comment notes future migration site. |

The bypasses came from a previous migration to StepOp wraps that didn't
re-wire the cred path. The 2026-05-05-kill-permission-check decision had
landed the cred-checked scripts (`script_kill_process` etc.); the syscall
arms just never adopted them. Stand-alone tests of the scripts kept passing,
so the gap survived the dispatch-migration commits.

POSIX-correct behaviour change at `sys_kill(0, sig)`: returns `-EPERM` when
no member was both live AND permitted (was `-ESRCH`). No existing tests
covered the 0-delivery case.

### 6. `SyscallResult` / `Errno` bridge

[`tx-shims/.../result.rs`](../../../crates/tx-shims/src/linux_syscall/result.rs).

```rust
impl SyscallResult {
    pub fn error_from(errno: Errno) -> Self;        // folds errno_to_i32 translation
}
impl From<Errno> for SyscallResult;

pub fn dispatch_errno<T>(
    Result<T, Errno>,
    impl FnOnce(T) -> SyscallResult,
) -> SyscallResult;
```

Mechanical sweep: 164 sites across 14 syscall-arm files collapsed from
`SyscallResult::Error(errno_to_i32(X))` to `SyscallResult::error_from(X)`.
`errno_to_i32` retained as the canonical translation table.

`dispatch_errno` consumed at the three sys_kill-family arms; future arms
with `Result<T, Errno>`-shaped returns adopt incrementally.

## Decisions

### D1: snapshot is a Copy wrapper, not a Cap

Considered making `CredSnapshot` retain a `Cap<Cred>` so the slab entry
stays alive while the snapshot is held. Rejected: `Cred` is `Copy` (small
POD), the value-copy already gives an independent snapshot, and retaining
a cap per syscall would couple every snapshot lifetime to the EBR reclamation
of the cred zone. Future growth can add a retained `Cap<Cred>` field
non-breakingly if a use case emerges.

### D2: `cred::checks::*` two-layered (require_* + authorize_*)

The design names only `require_*`. Adding `authorize_*` on top serves the
script-author audience: `require_signal_send` requires the caller to have
already captured the snapshot, resolved the target facts, and taken a guard.
That's exactly the recurring boilerplate. `authorize_signal_send` is a
combinator over `require_signal_send` — the underlying witness API is
unchanged.

### D3: `AuthOutcome` enum, not `Result<bool, Errno>`

POSIX kill maps `NoLiveTarget` and "denied" to different syscall returns
(0/ESRCH for the former, EPERM for the latter). Folding them into
`Result<(), Errno>` would lose the distinction at the type level. The 3-state
enum + `Result` together preserve the four outcomes (Authorized,
NoLiveTarget, Err(EPERM), Err(ESRCH)).

### D4: `script_deliver_signal` routes Thread → Process at commit

`deliver_posix_signal`'s `SignalTarget::Thread` branch calls
`upgrade_owner_proc()` under a held guard, which would nest with the upgrade's
own internal guard. Today this latent bug isn't exercised (leader-thread
tids aren't registered in `PidName::Thread`, so the only path is the
`SignalTarget::Process` branch via `sys_tkill`'s fallback to `sys_kill`).
`script_deliver_signal` sidesteps it by passing `SignalTarget::Process` to
the commit, matching the effective behaviour today (thread targeting is a
future phase).

### D5: sys_tgkill left unchanged

The tgid==caller-pid constraint means source==target, so the cred check
trivially passes. Adding it would be a no-op today. A comment at the dispatch
arm notes the future migration site for when cross-process tgkill lands.

## Tests (8 new)

In [`cred/tests.rs`](../../../crates/tx-subsystems/src/cred/tests.rs):
- `cred_snapshot_freezes_value_against_later_mutation` — snapshot stays
  stable while the canonical cred advances via `step_setuid`.
- `cred_snapshot_root_constructor_matches_root_cred`.
- `cred_snapshot_returns_none_for_zombie`.
- `require_path_search_passes_for_dac_override`.
- `require_path_search_denies_without_x_bit`.
- `require_open_honors_read_and_write_bits`.

In [`signal/tests/kill_permission.rs`](../../../crates/tx-subsystems/src/signal/tests/kill_permission.rs):
- `script_deliver_signal_to_thread_denied_for_mismatched_uid`.
- `script_deliver_signal_to_thread_delivers_when_authorized`.
- `authorize_signal_send_yields_three_state_outcome` — pins the 4 outcomes
  (Authorized / EPERM / NoLiveTarget / ESRCH).

In [`tx-shims/.../fcntl_misc.rs`](../../../crates/tx-shims/src/linux_syscall/tests/fcntl_misc.rs):
- `dispatch_kill_different_uid_returns_neg_eperm` — caller uid=1000 no
  CAP_KILL → target uid=2000 → `-EPERM`, target stays live. Locks in the
  fix for the `sys_kill` cred bypass.

## Verification

- `cargo -q xtask unit`: tx-shims **230** (was 229, +1 EPERM dispatch test),
  tx-kernel 44, tx-ext4 8, tx-scripts 50. Total **332**.
- `cargo test -p tx-subsystems --lib`: **655** (was 649, +6), 11 ignored,
  0 failed.

## Commit chain

```
68c5499 cred: lift syscall-entry CredSnapshot to first-class type
0b45030 cred: thread CredSnapshot through signal authorization checks
446b553 cred: project CredSnapshot directly into VFS walker Credential
a2dbf80 cred: land cred::checks witness API per cred_service_v_1
32769d4 cred: enforce kill permission at sys_kill via script_kill_process
6d27c78 cred: close remaining kill-family permission bypasses
b737739 cred: extract authorize_signal_send combinator (snapshot + facts + check)
4ac80d8 tx-shims: land SyscallResult::error_from + dispatch_errno bridges
```

## Scope gaps (deferred, not regressed)

- Supplementary group list, `fsuid` / `fsgid`, capability
  bounding/inheritable/ambient sets — day-1 elisions per
  `cred_service_v_1` deferred list.
- `RestrictionStackHandle` is still a unit-typed placeholder zone (PR-K
  territory).
- `cred::checks::*` covers path_search / open / signal_send.
  `require_unlink` / `require_chmod` / `require_setuid` etc. land
  alongside the syscall arms that need them.
- `deliver_posix_signal::SignalTarget::Thread` nested-guard issue documented
  but not fixed (not exercised today; the script-level fix is in
  `script_deliver_signal`).
- `sys_tgkill` cred check trivially permitted by tgid==caller-pid constraint;
  add the explicit check when cross-process tgkill lands.

## Blockers

None.

## Next step

Two candidate follow-ups:

1. **`require_unlink` + first VFS witness migration** — add the
   `unlink`-specific DAC predicate to `cred::checks` (sticky-bit /
   owner-matches rule) and migrate one VFS execution site to consume the
   `UnlinkAuthorized<'g>` witness end-to-end. Demonstrates the witness
   discipline reaches an actual mint point.

2. **`require_chown` / `require_chmod`** — same shape, different rules.

Either is a tight scope (~150 lines + tests). Pick when the next exec /
DAC slice picks up.
