# Cred Snapshot Wiring

<!-- txdoc:02-EXECUTION-CRED-SNAPSHOT-WIRING-V-1 -->

## Status
<!-- txdoc:CSW-STATUS -->

Draft v1. Companion to [`cred_service_v_1`](<cred_service_v_1_draft (2).md>) — documents
how that contract is realised in the current code, the API surfaces that
landed, the bypasses the audit closed, and the open items.

Read this when planning new cred-consuming syscalls, auditing existing arms
for permission bypasses, or extending `cred::checks::*`. Read
`cred_service_v_1` first for the architectural contract.

---

## Purpose
<!-- txdoc:CSW-PURPOSE -->

`cred_service_v_1` defines the contract: a `Credential` ledger plus
publication-time grant minting at subsystem commit points, with a
canonical `cred::checks::*` authorization surface taking value-typed
foreign inputs and producing zero-sized guard-phantom witnesses.

This doc names the **shipped artefacts** that realise that contract:

- the by-value snapshot type and where it is captured;
- the witness types currently minted and the predicates that produce them;
- the combinators that fold the recurring snapshot+facts+check sequence;
- the syscall arms wired through the witnesses;
- the bypasses closed by the audit and the ones still open;
- the FS-layer cred checks that need consolidation.

It is not a re-derivation of the design — `cred_service_v_1` remains the
contract; this is the wiring map.

---

## Snapshot model
<!-- txdoc:CSW-SNAPSHOT-MODEL -->

Per `cred_service_v_1` §"In flight", the script-side credential is a
by-value metadata copy captured **once at syscall entry**, not a live
re-read of `ProcessPayload.cred` on every check.

### CredSnapshot type
<!-- txdoc:CSW-CREDSNAPSHOT-TYPE -->

```rust
// crates/tx-subsystems/src/cred/mod.rs
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[must_use = "the snapshot is the syscall-entry credential — \
              discarding it forces a live re-read elsewhere"]
pub struct CredSnapshot { cred: Cred }

impl CredSnapshot {
    pub const fn from_cred(cred: Cred) -> Self;
    pub const fn root() -> Self;              // defensive zombie fallback
    pub const fn cred(self) -> Cred;
    pub const fn as_cred(&self) -> &Cred;
    pub fn is_privileged_for(self, cap: Capability) -> bool;
}
impl From<Cred> for CredSnapshot;
impl AsRef<Cred> for CredSnapshot;
```

`Copy`, `must_use`, wraps a single `Cred` value. Future growth (generation
tag for racing-setuid, NOSUID mount hint, retained `Cap<Cred>`) lands here
without touching every consumer signature.

### Capture sites
<!-- txdoc:CSW-CAPTURE-SITES -->

```rust
// Subsystem layer — exposes the snapshot from a process Cap.
impl ProcessIdentity {
    pub fn cred_snapshot(&self) -> Option<CredSnapshot>;  // None → zombie
}
impl ProcessPayload {
    pub fn cred_snapshot(&self) -> CredSnapshot;          // infallible
}

// Syscall layer — captures exactly once in SyscallCtx::new.
pub struct SyscallCtx<'a> {
    // …
    cred_snapshot: CredSnapshot,
    // …
}
impl<'a> SyscallCtx<'a> {
    pub fn cred(&self) -> Cred             { self.cred_snapshot.cred() }
    pub fn cred_snapshot(&self) -> &CredSnapshot;
    pub fn walker_cred(&self) -> Credential;  // VFS DAC projection
}
```

`SyscallCtx::new(...)` calls `process.cred_snapshot().unwrap_or_else(CredSnapshot::root)`.
After that point, no `AtomicSlot<Cap<Cred>>` re-read happens inside the
syscall — every accessor reads from the cached snapshot.

A mid-syscall `setuid` on the same process (architecturally permitted in
phase 2 of cred service; not yet exercised) cannot perturb authorization
decisions already taken inside the script.

### Walker projection
<!-- txdoc:CSW-WALKER-PROJECTION -->

```rust
// crates/tx-subsystems/src/vfs/structure.rs
pub struct Credential {       // walker-side DAC projection
    pub uid: u32,             // = snapshot's euid
    pub gid: u32,             // = snapshot's egid
    pub effective_caps: CapabilitySet,
}
impl From<&Cred> for Credential;
impl From<&CredSnapshot> for Credential;  // direct, skip Cred copy
```

`SyscallCtx::walker_cred()` projects from the cached snapshot in one step.
Every path-walking syscall (open / stat / chmod / unlink / rename / link /
chdir / …) traces back to this single snapshot.

---

## cred::checks::* surface
<!-- txdoc:CSW-CRED-CHECKS-SURFACE -->

Per `cred_service_v_1` §"Checks surface" + §"Cred witnesses". Two layers
sit on top of `vfs::predicates` (which owns the bit-level POSIX math):

### Witness predicates (one input → one witness)
<!-- txdoc:CSW-WITNESS-PREDICATES -->

```rust
// crates/tx-subsystems/src/cred/checks.rs

// Path resolution & traversal
pub struct SearchAuthorized<'g>;
pub fn require_path_search<'g>(
    source: &CredSnapshot,
    meta: &InodeMeta,
    guard: &'g Guard<'_>,
) -> Result<SearchAuthorized<'g>, Errno>;

// Open
pub struct OpenAuthorized<'g>;
pub fn require_open<'g>(
    source: &CredSnapshot,
    meta: &InodeMeta,
    flags: OpenFileFlags,
    guard: &'g Guard<'_>,
) -> Result<OpenAuthorized<'g>, Errno>;

// Unlink / rmdir
pub struct UnlinkAuthorized<'g>;
pub fn require_unlink<'g>(
    source: &CredSnapshot,
    parent_meta: &InodeMeta,
    child_meta: &InodeMeta,
    guard: &'g Guard<'_>,
) -> Result<UnlinkAuthorized<'g>, Errno>;

// Link
pub struct LinkAuthorized<'g>;
pub fn require_link<'g>(
    source: &CredSnapshot,
    new_parent_meta: &InodeMeta,
    guard: &'g Guard<'_>,
) -> Result<LinkAuthorized<'g>, Errno>;

// Signal (re-exported from cred root for discoverability)
pub struct SignalAuthorized<'g>;
pub fn require_signal_send<'g>(
    source: &CredSnapshot,
    target: &TargetProcCred,
    sig: Signum,
    guard: &'g Guard<'_>,
) -> Result<SignalAuthorized<'g>, Errno>;
pub fn signal_permitted(source: &CredSnapshot, target: &TargetProcCred, sig: Signum) -> bool;
```

Each witness is **zero-sized**, `#[must_use]`, carries a `PhantomData<&'g ()>`,
and is constructible only through the matching `require_*`. The bit-level
math lives in [`vfs/predicates.rs`](../../../crates/tx-subsystems/src/vfs/predicates.rs)
(`check_descend_perm`, `check_open_perm`, `check_unlink_perm`,
`check_link_perm`). Cred owns the witness type — the publication subsystem
owns the structure that consumes it.

### Combinators (snapshot + facts + check)
<!-- txdoc:CSW-COMBINATORS -->

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "authorization outcomes must drive a commit-or-skip decision"]
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

The bare form takes its own epoch guard and drops it before returning, so
the caller's commit phase can take downstream guards (e.g. `post_signal`
for SigInfo storage, `upgrade_owner_proc` for thread→process resolution)
without nesting. The `_under_guard` variant lets fanout loops
(`script_kill_pgrp`) reuse one snapshot + outer guard across N iterations.

`AuthOutcome` is a 3-state enum (Authorized / NoLiveTarget / Err(Errno))
so POSIX kill's distinct outcomes — "delivered", "no live target",
"denied" — each map to a different `SyscallResult` branch with no
type-level ambiguity.

---

## Auth-then-commit discipline
<!-- txdoc:CSW-AUTH-THEN-COMMIT -->

Every cred-checked script follows the same shape:

```
{ auth-phase guard scope }
  capture snapshot (or use one passed in)
  resolve foreign-value inputs (target_proc_cred_for, inode meta, …)
  call require_* (or authorize_* combinator)
  guard drops here
commit phase
  fs/signal/process primitive call (which may take its own guard)
```

The auth-phase guard **must drop before the commit** — downstream commit
primitives take their own guards (e.g. `post_signal` for SigInfo,
`Weak::upgrade` inside `upgrade_owner_proc`) and the no-nested-guard
invariant (`txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`) forbids overlap.

This discipline is **enforced by the combinators**: callers using
`authorize_signal_send` do not see the guard at all; the function takes it
and drops it inside. Callers using `require_*` directly bracket the scope
manually — same shape, more flexibility for sites that need the witness
to live longer.

---

## SyscallResult bridge
<!-- txdoc:CSW-SYSCALLRESULT-BRIDGE -->

```rust
// crates/tx-shims/src/linux_syscall/result.rs

impl SyscallResult {
    pub fn error_from(errno: Errno) -> Self;     // folds errno_to_i32
}
impl From<Errno> for SyscallResult;

pub fn dispatch_errno<T, F>(
    result: Result<T, Errno>,
    on_ok: F,
) -> SyscallResult
where F: FnOnce(T) -> SyscallResult;
```

Lives alongside the dispatch outcome enum. Two helpers compress the
two recurring patterns:

- `error_from(errno)` — collapses every `SyscallResult::Error(errno_to_i32(X))`
  to `SyscallResult::error_from(X)`. 164 call sites swept.
- `dispatch_errno(result, on_ok)` — folds the
  `match script(...) { Ok(v) => map, Err(e) => SyscallResult::Error(errno_to_i32(e)) }`
  pattern. Used by the cred-checked kill-family syscall arms.

`errno_to_i32` stays as the canonical translation table — `error_from`
is a thin wrapper over it.

---

## Syscall-arm wiring map
<!-- txdoc:CSW-SYSCALL-ARM-WIRING -->

State of each cred-consuming syscall arm. `script_*` entry points all
take + drop their own auth guard internally.

| Syscall | Cred-check entry point | Witness / outcome | Status |
|---|---|---|---|
| `sys_kill` (pid > 0) | `signal::script_kill_process(snapshot via cap, target, sig, info)` | `KillScriptOutcome` | ✅ wired |
| `sys_kill` (pid == 0 pgrp) | `signal::script_kill_pgrp(snapshot via cap, pgrp, sig)` | `u32` count | ✅ wired |
| `sys_tkill` (thread) | `signal::script_deliver_signal(snapshot via cap, SignalTarget, sig)` | `KillOutcome` | ✅ wired |
| `sys_tgkill` | `ThreadKillOp` direct drive (no cred check) | n/a | ⚠️ tgid==caller-pid constraint trivially permits; comment notes future migration site |
| `sys_unlinkat` | `cred::checks::require_unlink(ctx.cred_snapshot(), parent_meta, child_meta, &guard)` | `UnlinkAuthorized<'g>` | ✅ wired |
| `sys_linkat` | `cred::checks::require_link(ctx.cred_snapshot(), new_parent_meta, &guard)` | `LinkAuthorized<'g>` | ✅ wired |
| `sys_renameat2` | `RenameOp` direct drive (no cred check) | n/a | ❌ **open audit item** — write-on-both-parents + sticky-on-old-parent not enforced |
| `sys_fchmodat` | `ChmodOp` → per-FS `step_chmod(cred)` (e.g. `tmpfs::step_chmod`) | owner/CAP_FOWNER check inline in each FS | ⚠️ checked but **wrong layer** — rule lives in each FS impl rather than at `cred::checks::require_chmod` |
| `sys_fchownat` | `ChownOp` → per-FS `step_chown(cred)` (e.g. `tmpfs::step_chown`) | privileged/own-uid+gid check inline in each FS | ⚠️ checked but **wrong layer** — same as chmod |
| `sys_openat` | Walker `check_open_perm` via VFS predicates | inline in walker | ⚠️ uses walker projection directly; could consume `OpenAuthorized` for witness chain |
| Path-walk syscalls (stat, chdir, access, …) | Walker `check_descend_perm` via VFS predicates | inline in walker | ⚠️ uses walker projection directly; could consume `SearchAuthorized` for witness chain |

✅ wired = cred check fires through `cred::checks::*` or `signal::script_*`, witness/outcome consumed at the FS commit site.
⚠️ checked but layer-wrong = enforcement exists but doesn't go through the canonical cred seam.
❌ open audit item = no cred check enforced; user-space gap.

---

## Closed bypass audit
<!-- txdoc:CSW-CLOSED-BYPASS-AUDIT -->

Five real cred-bypass paths existed before this batch. All five closed:

| # | Syscall | What was missing | Closure commit |
|---|---|---|---|
| 1 | `sys_kill` (pid > 0) | `KillProcessOp` drove `step_kill_process` directly, bypassing `cred::require_signal_send` | `32769d4` |
| 2 | `sys_kill` (pid == 0 pgrp) | `step_kill_pgrp` had no per-member cred check | `6d27c78` |
| 3 | `sys_tkill` (thread) | `DeliverSignalOp` drove `deliver_posix_signal` without cred check | `6d27c78` |
| 4 | `sys_unlinkat` | No write-on-parent enforcement; no `S_ISVTX` sticky-bit ownership rule | `a70a61f` |
| 5 | `sys_linkat` | No write-on-new-parent enforcement | `68a005b` |

Each closure is regression-locked by a dispatch test in
[`tx-shims/.../tests/fcntl_misc.rs`](../../../crates/tx-shims/src/linux_syscall/tests/fcntl_misc.rs)
or
[`tx-shims/.../tests/file_mutation.rs`](../../../crates/tx-shims/src/linux_syscall/tests/file_mutation.rs)
that exercises the syscall as a non-root caller and asserts the cred check
fires before the commit primitive.

---

## Open audit items
<!-- txdoc:CSW-OPEN-AUDIT-ITEMS -->

### `sys_renameat2` (real bypass)
<!-- txdoc:CSW-OPEN-RENAMEAT2 -->

POSIX rename rule is "unlink from old parent + create in new parent":

- W+X on **old** parent (to remove the entry).
- W+X on **new** parent (to add the entry).
- `S_ISVTX` on old parent → caller must own old child, or own old parent,
  or carry `CAP_FOWNER` (or be euid 0). EPERM otherwise.
- If the rename **displaces** an existing entry at the new path
  (POSIX-permitted same-type collision), `S_ISVTX` on new parent triggers
  the same ownership rule against the displaced child.

Today's `tmpfs::rename` enforces none of these. `sys_renameat2` and the
composite `RenameOp` walk paths but do no cred check.

Proposed shape:

```rust
pub struct RenameAuthorized<'g>;
pub fn require_rename<'g>(
    source: &CredSnapshot,
    old_parent_meta: &InodeMeta,
    old_child_meta: &InodeMeta,
    new_parent_meta: &InodeMeta,
    new_child_meta: Option<&InodeMeta>,   // Some if displacing
    guard: &'g Guard<'_>,
) -> Result<RenameAuthorized<'g>, Errno>;
```

Body: `check_unlink_perm(old_parent, old_child)` + `check_link_perm(new_parent)`
+ optional `check_unlink_perm(new_parent, new_child)` for the displaced
side.

### `sys_fchmodat` / `sys_fchownat` (layering, not security)
<!-- txdoc:CSW-OPEN-CHMOD-CHOWN-LAYERING -->

Cred is enforced — but the rule lives in each filesystem's `step_chmod` /
`step_chown` implementation (`tmpfs::step_chmod`, `devfs::step_chmod`,
`bdevfs::step_chmod`, `procfs::step_chmod`, and ext4-side equivalents).

Per `cred_service_v_1` §"Cred owns credential semantics" the rule belongs
in cred. The current FS-layer placement is a leftover from the
DAC + setuid slice (Wave 3).

Proposed shape:

```rust
pub struct ChmodAuthorized<'g>;
pub fn require_chmod<'g>(
    source: &CredSnapshot,
    target_meta: &InodeMeta,
    new_mode: u16,
    guard: &'g Guard<'_>,
) -> Result<ChmodAuthorized<'g>, Errno>;
// Rule: owner OR CAP_FOWNER OR euid 0.

pub struct ChownAuthorized<'g>;
pub fn require_chown<'g>(
    source: &CredSnapshot,
    target_meta: &InodeMeta,
    new_uid: Option<u32>,
    new_gid: Option<u32>,
    guard: &'g Guard<'_>,
) -> Result<ChownAuthorized<'g>, Errno>;
// Rule: arbitrary uid/gid → CAP_CHOWN / CAP_FOWNER / euid 0;
//       otherwise non-privileged → only own uid/gid.
```

Migration touches `FsOps::step_chmod` / `step_chown` signatures — those
take `&Credential` (walker projection) today and would need to either
keep that signature (and have `require_chmod` accept walker projection
too) or shift to `&CredSnapshot`. The shift broadens because
`Credential` is lossy (no suid/sgid/permitted_caps), so the FS-layer can't
recover the snapshot from what it currently receives.

Cleanest path: thread `CredSnapshot` through `FsOps::step_chmod` /
`step_chown` (and other ops that take cred), delete the per-FS cred check,
and add `require_chmod` / `require_chown` consumption at the syscall arm
(or composite op). Until that lands, the per-FS check is defence-in-depth
and not a security gap.

### Walker-side `require_path_search` / `require_open` adoption
<!-- txdoc:CSW-OPEN-WALKER-ADOPTION -->

`require_path_search` and `require_open` are landed and tested, but the
VFS walker (`vfs::walker::step_walk`) and `step_open` still call the
inline `vfs::predicates::check_descend_perm` / `check_open_perm` directly
rather than going through the cred witness. The bit math is shared, so
behaviour is identical; the difference is that the witness chain is not
intact at the walker mint sites.

Migration: the walker takes `&Credential` (walker projection) today and
would need either:
- the same projection-shaped overload of `require_*` (lossy → loses the
  full `Cred` info that some future predicate might need), or
- the syscall arm to pre-walk + check + thread the witness into the
  composite op.

Deferred. Not a security gap; an architectural alignment item.

### `sys_tgkill` (vacuous today, future site)
<!-- txdoc:CSW-OPEN-TGKILL -->

The `tgid != caller.pid → -ESRCH` short-circuit in `sys_tgkill`
guarantees source == target, so the cred check trivially passes. When
cross-process tgkill lands, route through `script_deliver_signal` like
`sys_tkill` does. A comment at the syscall arm marks the migration site.

---

## Invariants and contracts
<!-- txdoc:CSW-INVARIANTS-AND-CONTRACTS -->

### I-1: Snapshot is captured exactly once per syscall
<!-- txdoc:CSW-I-1-SNAPSHOT-CAPTURE-ONCE -->

`SyscallCtx::new` is the canonical capture site. Inside the syscall body,
`ctx.cred()`, `ctx.cred_snapshot()`, and `ctx.walker_cred()` all read
from the cached snapshot — they perform no `AtomicSlot` load.

The implication: a mid-syscall `setuid` on the same process produces a
new cred cap in the slot, but the syscall's authorization decisions
remain coherent against the snapshot it captured at entry.

### I-2: Witnesses are short-lived
<!-- txdoc:CSW-I-2-WITNESSES-SHORT-LIVED -->

`require_*` witnesses carry `'g` from the auth guard. The witness is
consumed inside the same script frame at the commit site; cred mints no
reusable grant for these operations (§"Not every operation is
tokenized"). Holding a witness past the guard's scope is a lifetime
error caught at compile time.

### I-3: Auth guard scopes do not nest
<!-- txdoc:CSW-I-3-AUTH-GUARD-SCOPES-DO-NOT-NEST -->

The auth-phase epoch guard must drop before the commit phase. Downstream
commit primitives (`post_signal` for SigInfo, `upgrade_owner_proc` for
Weak resolution, the per-FS step bodies' internal guards) take their own
guards; nesting trips `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`.

The `authorize_*` combinators enforce this by taking + dropping the
auth guard internally. Direct `require_*` consumers bracket the scope
manually with `{ … }`.

### I-4: cred owns authorization; subsystems own publication
<!-- txdoc:CSW-I-4-CRED-OWNS-AUTHORIZATION-SUBSYSTEMS-OWN-PUBLICATION -->

Per `cred_service_v_1` §"Minting rule" — cred authorises; the owning
subsystem publishes. The witness mints from `cred::checks::require_*`
and is consumed at the subsystem's commit site (`FsOps::unlink`,
`FsOps::link`, `step_kill_process`, …). Cred does not publish any
foreign-subsystem object.

Today's chmod / chown are the layering exception (see §Open audit
items): the rule lives in each FS impl rather than at the cred seam.

---

## Test inventory
<!-- txdoc:CSW-TEST-INVENTORY -->

| Module | Tests | Coverage |
|---|---|---|
| [`cred::tests`](../../../crates/tx-subsystems/src/cred/tests.rs) | 11 new (snapshot 3 + path/open 3 + unlink 7 + link 4 = 17, minus the suite's pre-existing) | snapshot stability, root constructor, zombie behaviour, all `require_*` predicate branches |
| [`signal::tests::kill_permission`](../../../crates/tx-subsystems/src/signal/tests/kill_permission.rs) | 3 new (`script_deliver_signal_*` ×2, `authorize_signal_send_yields_three_state_outcome`) | combinator 4-outcome contract, thread-target authorization |
| [`tx-shims/.../fcntl_misc.rs`](../../../crates/tx-shims/src/linux_syscall/tests/fcntl_misc.rs) | `dispatch_kill_different_uid_returns_neg_eperm` | sys_kill EPERM on uid mismatch |
| [`tx-shims/.../file_mutation.rs`](../../../crates/tx-shims/src/linux_syscall/tests/file_mutation.rs) | `dispatch_unlinkat_without_parent_write_returns_neg_eacces`, `dispatch_linkat_without_new_parent_write_returns_neg_eacces` | unlink / link EACCES at the dispatch boundary |

Verification after the full batch:

- `cargo -q xtask unit` — host suite: tx-shims **232**, tx-kernel 44, tx-ext4 8, tx-scripts 50 (total 334).
- `cargo test -p tx-subsystems --lib` — **666** passed, 11 ignored, 0 failed.

---

## Short version
<!-- txdoc:CSW-SHORT-VERSION -->

> Snapshot captured once at `SyscallCtx::new`; cred consumed via
> `cred::checks::require_*` witnesses + `authorize_*` combinators;
> auth-phase guard scoped to drop before commit; five real
> permission bypasses closed (`sys_kill` pid>0 / pgrp, `sys_tkill`,
> `sys_unlinkat`, `sys_linkat`); `sys_renameat2` and chmod/chown
> layering remain as open audit items; the witness chain is intact
> at four FS / signal commit sites and the API is ready for further
> adoption.
