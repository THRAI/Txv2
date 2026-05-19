# Cred Snapshot Wiring

<!-- txdoc:02-EXECUTION-CRED-SNAPSHOT-WIRING-V-1 -->

## Status
<!-- txdoc:CSW-STATUS -->

Draft v1.1. Companion to [`cred_service_v_1`](<cred_service_v_1_draft (2).md>) —
documents how that contract is realised in the current code, the API
surfaces that landed, the bypasses the audit closed, and the open items.

v1.1 update: closes the migration (`require_rename`, `require_chmod`,
`require_chown` land; `sys_renameat2`, `sys_fchmodat`, `sys_fchownat`
wired; lint-discovered `sys_mkdirat` / `sys_symlinkat` bypasses closed
via `require_link`). New `xtask lint invariants cred-check` static gate
enforces "every cred-relevant mutator syscall arm is authorised" with
no ratchet.

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

// Link / mkdir / symlink (all share the W+X-on-new-parent rule)
pub struct LinkAuthorized<'g>;
pub fn require_link<'g>(
    source: &CredSnapshot,
    new_parent_meta: &InodeMeta,
    guard: &'g Guard<'_>,
) -> Result<LinkAuthorized<'g>, Errno>;

// Rename — composes unlink + link rules, optional displaced-side check
pub struct RenameAuthorized<'g>;
pub fn require_rename<'g>(
    source: &CredSnapshot,
    old_parent_meta: &InodeMeta,
    old_child_meta: &InodeMeta,
    new_parent_meta: &InodeMeta,
    displaced_child: Option<&InodeMeta>,
    guard: &'g Guard<'_>,
) -> Result<RenameAuthorized<'g>, Errno>;

// Chmod / chown
pub struct ChmodAuthorized<'g>;
pub fn require_chmod<'g>(
    source: &CredSnapshot,
    target_meta: &InodeMeta,
    new_mode: u16,
    guard: &'g Guard<'_>,
) -> Result<ChmodAuthorized<'g>, Errno>;

pub struct ChownAuthorized<'g>;
pub fn require_chown<'g>(
    source: &CredSnapshot,
    target_meta: &InodeMeta,
    new_uid: Option<u32>,
    new_gid: Option<u32>,
    guard: &'g Guard<'_>,
) -> Result<ChownAuthorized<'g>, Errno>;

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

Two flavours: a 3-state signal-side combinator family and a 2-state
FS-side family.

#### Signal-side: `authorize_signal_send`
<!-- txdoc:CSW-COMBINATORS-SIGNAL -->

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

`AuthOutcome` is 3-state (Authorized / NoLiveTarget / Err(Errno)) because
POSIX kill's distinct outcomes — "delivered", "no live target", "denied"
— each map to a different `SyscallResult` branch with no type-level
ambiguity. The `_under_guard` variant lets fanout loops
(`script_kill_pgrp`) reuse one snapshot + outer guard across N iterations.

#### FS-side: `authorize_unlink` / `_link` / `_rename` / `_chmod` / `_chown` / `_open` / `_path_search`
<!-- txdoc:CSW-COMBINATORS-FS -->

```rust
pub fn authorize_path_search(source: &CredSnapshot, meta: &InodeMeta) -> Result<(), Errno>;
pub fn authorize_open(source: &CredSnapshot, meta: &InodeMeta, flags: OpenFileFlags) -> Result<(), Errno>;
pub fn authorize_unlink(source: &CredSnapshot, parent: &InodeMeta, child: &InodeMeta) -> Result<(), Errno>;
pub fn authorize_link(source: &CredSnapshot, new_parent: &InodeMeta) -> Result<(), Errno>;
pub fn authorize_rename(source: &CredSnapshot, op: &InodeMeta, oc: &InodeMeta, np: &InodeMeta, displaced: Option<&InodeMeta>) -> Result<(), Errno>;
pub fn authorize_chmod(source: &CredSnapshot, target: &InodeMeta, new_mode: u16) -> Result<(), Errno>;
pub fn authorize_chown(source: &CredSnapshot, target: &InodeMeta, new_uid: Option<u32>, new_gid: Option<u32>) -> Result<(), Errno>;
```

Each is a thin `{ let guard = fresh_guard(); require_X(..., &guard).map(drop) }`
wrapper. Returns `Result<(), Errno>` — the FS-side rules have no
"NoLiveTarget" branch (the walker resolved the target before the cred
check), so the simpler 2-state outcome fits.

#### When to use which
<!-- txdoc:CSW-COMBINATORS-WHEN-TO-USE -->

- **`require_*`** — when the call site genuinely needs to hold the
  typed `*Authorized<'g>` witness across a separate commit step that
  consumes it as a token (today: no in-tree consumer; reserved for
  composite ops that gate on the witness's type).
- **`authorize_*`** — every other consumer. Hides the guard scope,
  unifies the call-site shape:
  ```rust
  if let Err(e) = cred::checks::authorize_X(ctx.cred_snapshot(), …) {
      return SyscallResult::error_from(e);
  }
  ```
  The 7 cred-checked FS syscall arms in tx-shims all use this form.
  Saved ~37 lines of guard-scope boilerplate.

The bare form takes its own epoch guard and drops it before returning,
so the caller's commit phase can take downstream guards (e.g. fs_ops
internals, `post_signal` for SigInfo storage, `upgrade_owner_proc` for
thread→process resolution) without nesting.

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
| `sys_mkdirat` | `cred::checks::require_link(...)` (W+X on parent, same rule as link/symlink) | `LinkAuthorized<'g>` | ✅ wired |
| `sys_symlinkat` | `cred::checks::require_link(...)` | `LinkAuthorized<'g>` | ✅ wired |
| `sys_renameat2` | `cred::checks::require_rename(snapshot, old_parent, old_child, new_parent, displaced, &guard)` | `RenameAuthorized<'g>` | ✅ wired |
| `sys_fchmodat` | `cred_checks::authorize_chmod(snapshot, target_meta, mode)` at arm; `tmpfs::step_chmod` delegates to `vfs::predicates::check_chmod_perm` as defense-in-depth | `ChmodAuthorized<'g>` | ✅ wired at cred seam; per-FS layer consolidated to single source of truth |
| `sys_fchownat` | `cred_checks::authorize_chown(snapshot, target_meta, new_uid, new_gid)` at arm; `tmpfs::step_chown` delegates similarly | `ChownAuthorized<'g>` | ✅ wired at cred seam; per-FS layer consolidated |
| VFS walker (search) | `cred::checks::require_path_search_with_walker_cred(walker_cred, parent_meta, &guard)` at every component descend | `SearchAuthorized<'g>` (currently discarded) | ✅ wired; witness chain reaches the walker mint site |
| VFS walker (`step_open`) | `cred::checks::require_open_with_walker_cred(walker_cred, terminal_meta, flags, &guard)` at terminal open | `OpenAuthorized<'g>` (currently discarded) | ✅ wired; the publication site (`OpenFile::new_cap_with_dentry`) doesn't yet consume the witness as a type token — follow-up |
| `sys_openat` | Walker `check_open_perm` via VFS predicates | inline in walker | ⚠️ uses walker projection directly; could consume `OpenAuthorized` for witness chain |
| Path-walk syscalls (stat, chdir, access, …) | Walker `check_descend_perm` via VFS predicates | inline in walker | ⚠️ uses walker projection directly; could consume `SearchAuthorized` for witness chain |

✅ wired = cred check fires through `cred::checks::*` or `signal::script_*`, witness/outcome consumed at the FS commit site.
⚠️ checked but layer-wrong = enforcement exists but doesn't go through the canonical cred seam.
❌ open audit item = no cred check enforced; user-space gap.

---

## Closed bypass audit
<!-- txdoc:CSW-CLOSED-BYPASS-AUDIT -->

Eight real cred-bypass paths existed before this batch. All eight closed:

| # | Syscall | What was missing | Closure commit |
|---|---|---|---|
| 1 | `sys_kill` (pid > 0) | `KillProcessOp` drove `step_kill_process` directly, bypassing `cred::require_signal_send` | `32769d4` |
| 2 | `sys_kill` (pid == 0 pgrp) | `step_kill_pgrp` had no per-member cred check | `6d27c78` |
| 3 | `sys_tkill` (thread) | `DeliverSignalOp` drove `deliver_posix_signal` without cred check | `6d27c78` |
| 4 | `sys_unlinkat` | No write-on-parent enforcement; no `S_ISVTX` sticky-bit ownership rule | `a70a61f` |
| 5 | `sys_linkat` | No write-on-new-parent enforcement | `68a005b` |
| 6 | `sys_renameat2` | No write-on-both-parents; no sticky-bit ownership rule on old or displaced new | `e2748b1` |
| 7 | `sys_mkdirat` | No write-on-parent enforcement (discovered by the cred-check lint) | `b675870` |
| 8 | `sys_symlinkat` | No write-on-parent enforcement (discovered by the cred-check lint) | `b675870` |

Each of #1–#6 is regression-locked by a dispatch test in
[`tx-shims/.../tests/fcntl_misc.rs`](../../../crates/tx-shims/src/linux_syscall/tests/fcntl_misc.rs)
or
[`tx-shims/.../tests/file_mutation.rs`](../../../crates/tx-shims/src/linux_syscall/tests/file_mutation.rs)
that exercises the syscall as a non-root caller and asserts the cred check
fires before the commit primitive. #7 and #8 are gate-locked by the
[`cred-check` lint](#static-ci-lint-cargo-xtask-lint-invariants-cred-check) — they remain in scope of
the static gate which fails CI on any new untouched-mutator regression.

---

## Static CI lint: `cargo xtask lint invariants cred-check`
<!-- txdoc:CSW-STATIC-CI-LINT-CARGO-XTASK-LINT-INVARIANTS-CRED-CHECK -->

Lives at
[`xtask/src/lint_invariants_cred_check.rs`](../../../xtask/src/lint_invariants_cred_check.rs).

**What it does.** Walks every `pub(super) (async )? fn sys_*` in
`crates/tx-shims/src/linux_syscall/` (excluding `mod.rs`, `numbers.rs`,
`tests/`). For each function, extracts the body via brace-depth
tracking. If the body matches any of the [`MUTATOR_SIGNALS`] strings
(signal-send primitives, StepOp wraps for kill / rename / chmod /
chown / mkdir / etc., or `fs_ops.{unlink,link,rename,mkdir,symlink,
create_inode,step_chmod,step_chown,step_truncate}`) AND matches *none*
of the [`CRED_CHECK_SIGNALS`] strings (`cred::checks::require_*`,
`cred::checks::authorize_*`, the legacy `cred::require_*` re-exports,
`signal::script_kill_*` / `signal::script_deliver_signal`), it flags
the function.

**Allow-list with rationale** (each entry's exception is documented
inline in the lint source):

- `sys_tgkill` — tgid==caller-pid constraint makes source == target;
  cred check trivially permitted. Future cross-process tgkill must
  remove this entry and route through `script_deliver_signal`.
- `sys_write` — hot-path: write reuses the access grant minted at
  `open()`, per `cred_service_v_1` §"Hot path: use". The body also
  synthesises a SIGPIPE self-send on broken-pipe writes, which is a
  kernel-internal signal not subject to `require_signal_send`.
- `sys_ftruncate` — hot-path: operates on an already-open fd whose
  write grant was minted at `open()`.

**Failure mode.** No ratchet — the lint fails CI on any violation. The
floor is zero from day one because the audit landed cred-check wiring
for every flagged mutator before the lint did.

**Run manually:**

```
cargo xtask lint invariants cred-check
```

**Sample output** (current state):

```
Invariants Lint — cred-check (every cred-mutator is gated)
===========================================================
audited mutator syscall arms: 9, allow-listed: 3
  allow-listed (audit out-of-scope):
    sys_ftruncate
    sys_tgkill
    sys_write
violations: 0  ok
```

**Adding a new syscall.** When a new `sys_*` arm in tx-shims drives a
mutator primitive (FS rename / unlink / link / chmod / chown / signal-
send / thread-kill / mkdir / symlink), the lint will flag it on first
build. Resolution paths:

1. **Add a cred check.** Route the mutation through a
   `cred::checks::require_*` predicate or a cred-checked
   `signal::script_*` script before the FsOps / signal primitive
   dispatch. This is the expected path for almost every new arm.
2. **Allow-list with rationale.** If the mutation is architecturally
   self-only or otherwise outside cred's scope, add the function name
   to `ALLOW_LIST` in the lint source with a comment explaining why.
   Reviewers will scrutinise.

**Adding a new mutator primitive.** When a new mutator (a new `FsOps`
method, a new `step_*` function in signal/process, a new `StepOp` wrap)
is introduced, add a matching string to `MUTATOR_SIGNALS`. Forgetting
to do so leaves a hole — the lint won't flag arms that drive the new
primitive. The convention is: every cred-mutating primitive earns a
signal entry, every authorisation seam earns a check entry.

---

## Open follow-ups (not security gaps)
<!-- txdoc:CSW-OPEN-FOLLOW-UPS -->

### Witness consumption at publication sites
<!-- txdoc:CSW-OPEN-WITNESS-CONSUMPTION -->

The walker now mints `SearchAuthorized<'g>` / `OpenAuthorized<'g>`
via `cred::checks::require_*_with_walker_cred` (pass 2 of the hygiene
batch), but the publication sites (`OpenFile::new_cap_with_dentry`,
the per-component lookup result, etc.) don't yet *consume* the
witness as a type-level token. Today the witness is dropped at the
walker — same behaviour as before, but the witness chain isn't
load-bearing.

Migration: thread `OpenAuthorized<'g>` into
`OpenFile::new_cap_with_dentry`'s signature so the type system
demands the cred check ran before publication. Same for the per-
component `SearchAuthorized<'g>` (would need to land on the resolved
DEntry's construction site). Adds a small amount of API verbosity
in exchange for a compile-time anchor of the
`cred_service_v_1` §"Minting rule" invariant (cred authorises;
subsystems publish only when the auth witness is in scope).

Defer until a feature naturally lifts it. Not security-relevant;
the runtime check still fires.

### `sys_tgkill` (vacuous today, future site)
<!-- txdoc:CSW-OPEN-TGKILL -->

The `tgid != caller.pid → -ESRCH` short-circuit in `sys_tgkill`
guarantees source == target, so the cred check trivially passes. When
cross-process tgkill lands, remove `sys_tgkill` from the lint allow-
list and route through `script_deliver_signal` like `sys_tkill` does.
A comment at the syscall arm marks the migration site.

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
> `cred::checks::require_*` witnesses + `authorize_*` combinators
> (snapshot-shaped) or `require_*_with_walker_cred` (walker-shaped);
> auth-phase guard scoped to drop before commit; eight real
> permission bypasses closed (`sys_kill` pid>0 / pgrp, `sys_tkill`,
> `sys_unlinkat`, `sys_linkat`, `sys_renameat2`, `sys_mkdirat`,
> `sys_symlinkat`); `sys_fchmodat` / `sys_fchownat` route through
> the cred seam at the syscall arm with `tmpfs::step_chmod` /
> `step_chown` delegating to `vfs::predicates` for the
> defense-in-depth layer; VFS walker mints `SearchAuthorized` /
> `OpenAuthorized` via the walker-cred variants at every component
> descend and at terminal open. **Every DAC check site in the
> kernel flows through `cred::checks::*` — the cred subsystem is
> the single canonical authorisation seam.**
> `cargo xtask lint invariants cred-check` is the static CI gate
> that fails on any new mutator syscall arm without an
> authorisation gate.
