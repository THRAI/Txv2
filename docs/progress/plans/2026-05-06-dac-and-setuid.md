# DAC + setuid

Status: proposed (planning only). Closes ~50-70 LTP test
directories (`getuid` / `geteuid` / `getgid` / `getegid` /
`setuid` / `setgid` / `setreuid` / `setregid` / `setresuid` /
`setresgid` / `chmod` / `fchmod` / `chown` / `fchown` /
`access` / `faccessat`) plus LTP `execve02` + the remaining
EACCES sub-cases of `execve03` + setuid binary support. The
breadth slice — many small syscall arms wrapping
already-existing `cred` helpers, plus walker DAC plumbing,
plus a single new exec phase-7 helper. Companion to
[`docs/progress/plans/2026-05-06-fork-clone-wait4.md`](2026-05-06-fork-clone-wait4.md)
(the slice that just landed on `feat/fork-clone-wait4`); this
slice is ranked #2 on the fork/clone/wait4 decision note's
follow-up list (CSPRNG was #1 but is a 1-line chore, not a
slice; explicitly skipped per user direction).

Builds on (already shipped):

- `crates/tx-subsystems/src/cred.rs:135-143` — `Cred { uid,
  euid, gid, egid, effective_caps, permitted_caps }`. Already
  on `ProcessPayload.cred: SpinMutex<Cred>` per
  `process/structure.rs:541` (verified mid-research).
- `crates/tx-subsystems/src/cred.rs:184-240` —
  `step_setuid` / `step_setgid` with day-1 privilege rules + 9
  unit tests. **No syscall arms consume them today.**
- `crates/tx-subsystems/src/cred.rs:160-162` —
  `Cred::is_privileged_for(cap)` short-circuits on
  `euid == 0`. Re-used unchanged.
- `crates/tx-subsystems/src/vfs/structure.rs:62-66` —
  walker-side `Credential { uid, gid }`.
- `crates/tx-subsystems/src/vfs/walker.rs:99-102` — the
  `let _ = cred;` deferral the slice replaces.
- `crates/tx-subsystems/src/vfs/structure.rs:110-121` —
  `InodeMeta { mode: u16, uid: u32, gid: u32, ... }`. Already
  carries everything DAC needs.
- `crates/tx-fs/src/tmpfs.rs:283-286` (file create), `:437-439`
  (mkdir), `:527-529` (symlink) — already store
  `meta.uid = cred.uid; meta.gid = cred.gid` from the FsOps
  `&Credential` passthrough. **No new tmpfs storage work.**
- `crates/tx-shims/src/linux_syscall/mod.rs:201-227` —
  `SyscallCtx` with the documented "future fields (cred
  snapshot)" comment. **No `cred()` accessor today.**
- `crates/tx-shims/src/linux_syscall/mod.rs:872-878` —
  `sys_execve`'s `Credential::default()` with the
  `TODO(phase-cred-on-ctx)` marker the slice retires.
- `crates/tx-scripts/src/process/exec/stack.rs:81-91` —
  `AuxvFacts` with 4 fields (`at_phdr`/`at_phent`/`at_phnum`/
  `at_pagesz`). Auxv emits 6 entries today; the slice grows
  this to 11.
- `crates/tx-scripts/src/process/exec/script.rs:392-453` —
  Phase 6/7 commit block. The setuid recompute lands as a
  pre-Phase-6 helper (mutate cred *before* PoNR).
- `crates/tx-shims/src/linux_syscall/numbers.rs` — verified
  the 10 syscall numbers below are absent.

Inputs:
[`docs/progress/research/2026-05-06-dac-and-setuid-scaffolding.md`](../research/2026-05-06-dac-and-setuid-scaffolding.md)
fully (the slice's gap list, LTP coverage matrix, and musl
`__init_security` table are cited inline below);
[`docs/progress/decisions/2026-05-06-fork-clone-wait4.md`](../decisions/2026-05-06-fork-clone-wait4.md)
for the `SyscallCtx`-shape and the existing seams the slice
extends.

## Goal

Demonstrate a static-musl-shape RV64 fixture binary with the
setuid bit set in its mode (`mode = S_ISUID | 0755`), owned by
`uid = 1000`, executed by a process running as `uid = 1001`.
The kernel recomputes the executing process's effective uid to
1000 at exec phase 7, the binary observes `getuid() == 1001`
vs `geteuid() == 1000` via the new syscall arms, and reads
`AT_SECURE = 1` from the auxv slice. **Plus** a non-root
process executing a `0700` root-owned binary returns
`-EACCES` from `execve` (LTP `execve02`). End state: both
control-flow paths reach zombie cleanly, console captured the
expected lines, the production reactor loop drained both
threads. Smoke target — host test in
`crates/tx-kernel/src/init/tests.rs` extending the existing
`boot_smoke_*` shape (Layer A: production-paths-up-to-divergence,
matches the fork/clone/wait4 plan's choice).

## Doc anchors

- `txdoc:PROCESS-CREDENTIAL-SERVICE-DRAFT-1`
  (`docs/design/04_process-signals/PROCESS_v1.md` line 1369) —
  cred service shape; the `suid`/`sgid` extension in this slice
  closes the §11.1 "Phase 2 cred spec" deferral.
- `txdoc:VFS-CHECKS-TRANSITION-RULES-1`
  (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md` line 254)
  rule #5 ("Permission denied → `Error(TraverseDenied)`"); the
  walker DAC predicate produces this error today via
  `Errno::EACCES`.
- `txdoc:VFS-CHECKS-WALKCAUSE-1`
  (`VFS_CHECKS_V2.1.md` line 279) — `WalkCause::TraverseDenied`
  has no production emitter today; this slice adds the first.
- `txdoc:EXEC-3-3-CRED-AUTHORIZES-EXECUTE-COMPUTES-NEW-CREDENTIALS`
  (`docs/design/02_execution/EXEC_v1.md` line 202) —
  `compute_exec_credentials` Phase 2 work; v1 returns
  `caller_cred` unchanged. The slice lands the Phase 2 setuid
  application.
- `txdoc:EXEC-12-3-INSTALL-NEW-CREDENTIAL`
  (`EXEC_v1.md` line 1240) — Phase 7's "store the new cred
  back" commit. v1 stores the same value; the slice changes
  this when `S_ISUID` / `S_ISGID` is set on the mode.
- `txdoc:EXEC-12-PHASE-7-INFALLIBLE-POST-SWAP-COMMITS`
  (`EXEC_v1.md` line 1183) — the infallibility invariant that
  forces the cred-recompute helper to live *before* Phase 6's
  `replace_aspace` PoNR (mutation must be reversible if the
  recompute fails; in practice it never fails, but the
  ordering keeps Phase 7 a straight-line block).

## Part 1 — Cred extension (suid/sgid + effective_caps bridge)

The cred-service-side groundwork. Each sub-item is a single
file edit; total is ~250 LOC across `cred.rs` plus 6-8 new
unit tests.

### A. `Cred.suid` / `Cred.sgid` fields

Per Q1 DECIDED (full Linux semantics; the cred module already
flags this at `cred.rs:21-23` as the deferred-but-coming
day-1 simplification).

- Extend `Cred` (`crates/tx-subsystems/src/cred.rs:135`):
  ```text
  pub struct Cred {
      pub uid: Uid,
      pub euid: Uid,
      pub suid: Uid,   // NEW — saved-set UID
      pub gid: Gid,
      pub egid: Gid,
      pub sgid: Gid,   // NEW — saved-set GID
      pub effective_caps: CapabilitySet,
      pub permitted_caps: CapabilitySet,
  }
  ```
- `Cred::root()` (`cred.rs:147`): set `suid = Uid::ROOT`,
  `sgid = Gid::ROOT`.
- `Cred::default()` (derived): `Default::default()` produces
  `Uid(0)` / `Gid(0)` for `suid`/`sgid` — matches the
  bootstrap-init-as-root pattern.
- Update `cred.rs:15-23`'s "Deliberately deferred" comment:
  remove the `suid`/`sgid` line; add a one-line note that the
  saved-set IDs landed in this slice.

### B. Existing `step_setuid` / `step_setgid` get saved-set updates

The day-1 helpers already enforce the privilege rules; the
slice extends their commit blocks to keep `suid`/`sgid` in
sync.

- `step_setuid` (`cred.rs:184-211`): on the privileged branch,
  set `new.suid = new_uid`. On the non-privileged branch,
  `suid` is preserved (Linux: only privileged callers update
  saved-set; non-privileged setuid leaves `suid` alone).
  Same for `step_setgid` and `sgid`.
- Existing 9 unit tests in `crates/tx-subsystems/src/cred/tests.rs`
  re-validate; add 2 tests:
  - `step_setuid_privileged_writes_suid_to_new_uid`.
  - `step_setuid_non_privileged_preserves_suid`.

### C. Cred-side `step_set*` helpers (4 new)

Land the 4 helpers `cred.rs:26-28` flagged as deferred. Each
mirrors the existing `step_setuid` shape (lock payload → lock
cred → enforce rule → write → fence → return `CredChange`).

1. `step_setresuid(target, ruid: Option<Uid>, euid: Option<Uid>, suid: Option<Uid>) -> CredChange`.
   `None` means "leave alone" (the syscall encodes this as
   `(u32) -1`; the arm translates before calling). Privileged:
   any combination. Non-privileged: each *requested* value
   (the `Some(_)` ones) must equal one of the existing
   `(uid, euid, suid)`. If `euid` changes, no implicit `suid`
   bump (`setresuid` is the *explicit* form).
2. `step_setresgid(target, rgid: Option<Gid>, egid: Option<Gid>, sgid: Option<Gid>) -> CredChange`.
   Same shape with `Gid` and `CAP_SETGID`.
3. `step_setreuid(target, ruid: Option<Uid>, euid: Option<Uid>) -> CredChange`.
   Linux's two-arg form. Privileged: arbitrary. Non-privileged:
   the requested `ruid` must equal current `(uid, euid)`; the
   requested `euid` must equal current `(uid, euid, suid)`.
   **Saved-set bump**: if `ruid` is set OR `euid` ends up
   different from `prev.uid`, then `suid := euid_after`. This
   is the Linux quirk that distinguishes `setreuid` from
   `setresuid` and the reason `setresuid01..05` need the
   explicit form.
4. `step_setregid(target, rgid: Option<Gid>, egid: Option<Gid>) -> CredChange`.
   Same shape.

`CredChange` enum is unchanged
(`Replaced { prev, new } | Zombie | PermissionDenied`).

#### Tests
(in `crates/tx-subsystems/src/cred/tests.rs`):

- `step_setresuid_privileged_sets_all_three`.
- `step_setresuid_non_privileged_rejects_unrelated_uid`.
- `step_setresuid_non_privileged_swap_within_existing_set_succeeds`.
- `step_setresuid_negone_sentinel_means_leave_alone`.
- `step_setreuid_non_privileged_bumps_suid_when_euid_changes`.
- `step_setreuid_non_privileged_does_not_bump_suid_when_euid_unchanged`.
- 4 `setresgid` / `setregid` companions (same shape with
  `Gid`).

### D. `Credential.effective_caps` extension (Q2 DECIDED)

The walker-side projection grows a `CapabilitySet` so the
walker can short-circuit on `CAP_DAC_OVERRIDE` without
reaching back into the per-process `Cred` lock.

- Extend `vfs/structure.rs:62-66`:
  ```text
  pub struct Credential {
      pub uid: u32,
      pub gid: u32,
      pub effective_caps: CapabilitySet,  // NEW
  }
  ```
- `Credential::default()` (derived): `effective_caps =
  CapabilitySet::EMPTY`. **Behaviour change**: today the
  walker's deferral makes the value irrelevant; once the DAC
  predicate (Part 2A) lands, a `Credential::default()` caller
  is *no longer root* — it's an unprivileged uid-0 caller
  with no caps. Existing `Credential::default()` call sites
  that need root-equivalence migrate to a new helper:
- New constructor: `Credential::root() -> Self` returns
  `{ uid: 0, gid: 0, effective_caps: CapabilitySet::FULL }`.
- New constructor: `Credential::for_walker(cred: &Cred) -> Self`
  produces the projection from a full `Cred`. Uses **euid /
  egid** (POSIX rule for DAC checks), not `uid` / `gid`. This
  is the bridge Part 1D names.
- `tx-subsystems::vfs::Credential` re-export
  (`vfs/mod.rs:21`) — verify `CapabilitySet` is reachable from
  the `vfs` boundary; if it lives in `crate::cred` only, add
  a `pub use crate::cred::CapabilitySet;` re-export at the
  `vfs` mod level so the walker can name the type without
  bringing all of `cred` into its module path.

#### Tests

- `credential_root_has_full_caps_and_uid_zero`.
- `credential_for_walker_pulls_euid_egid_not_real`.
- `credential_default_has_empty_caps_and_zero_uid` — pins the
  behaviour change; the slice must update existing tests that
  assumed `Credential::default()` was "root for walker
  purposes" (audit `vfs/walker/tests.rs`,
  `tx-fs/src/tmpfs.rs::tests` — both call
  `Credential::default()` heavily; most should migrate to
  `Credential::root()`).

### E. Cred → Credential bridge (call-site sweep)

After 1D, all the `Credential::default()` call sites in the
production code paths get audited. Three categories:

1. **Bootstrap init** (`crates/tx-kernel/src/init.rs:651` and
   `:741`) — the bootstrap-exec path. The bootstrap process
   *is* root by construction; replace with
   `Credential::root()`. No behaviour change.
2. **`sys_execve`** (`linux_syscall/mod.rs:878`) — replace
   with `Credential::for_walker(&ctx.cred())` (after Part 7
   adds `ctx.cred()`). This is the slice's load-bearing
   change.
3. **Tests** — case-by-case: walker tests that assert "always
   allowed today" should move to
   `Credential::root()` to keep the existing behaviour;
   tests that should now exercise the EACCES path move to a
   non-root `Credential` constructed inline.

The bridge has zero new types — it's purely the `for_walker`
constructor in 1D.

## Part 2 — VFS permission-check plumbing

### A. `step_walk` DAC predicate

Replace the `let _ = cred;` deferral at
`vfs/walker.rs:99-102` with a real predicate.

#### Surface

- New free function in `vfs/walker.rs`:
  ```text
  fn check_perm(meta: &InodeMeta, cred: &Credential, want: PermBit) -> bool
  ```
- New enum in `vfs/walker.rs` (or `vfs/structure.rs`; pick
  the lower-disruption site):
  ```text
  pub enum PermBit { Read, Write, Execute }
  ```
- Body — the standard Linux DAC algorithm:
  1. Short-circuit: if
     `cred.effective_caps.contains(Capability::DAC_OVERRIDE)`
     OR `cred.uid == 0`, return `true` for `Read`/`Write`.
     For `Execute`, **only** short-circuit when at least one
     of the three execute bits (`S_IXUSR | S_IXGRP | S_IXOTH`)
     is set on the mode (POSIX exception: root cannot execute
     a file with no execute bits — Linux honours this; LTP
     `execve03` checks). This is one of two `CAP_DAC_OVERRIDE`
     subtleties; the other is `CAP_DAC_READ_SEARCH` which the
     slice does **not** ship (no LTP test gates on it).
  2. If `cred.uid == meta.uid`: check the user triplet
     (`(meta.mode >> 6) & 7`).
  3. Else if `cred.gid == meta.gid`: check the group triplet
     (`(meta.mode >> 3) & 7`).
  4. Else: check the other triplet (`meta.mode & 7`).
- Mode constants: `S_IXUSR = 0o100`, `S_IXGRP = 0o010`,
  `S_IXOTH = 0o001`, etc. Add to `vfs/structure.rs`'s
  S_IFMT constants block (`structure.rs:83-90`); they're
  already absent.

#### Integration into `step_walk`

`walker.rs::walk_inner` (`walker.rs:147`) carries the inner
component-by-component traversal. The predicate is called at
each *intermediate* component descent with `PermBit::Execute`
(POSIX traverse rule); at the terminal, the caller
(`step_open` / `step_walk` consumers) applies the per-op
predicate. Concretely:

- Plumb `cred: &Credential` from `step_walk` into
  `walk_inner` (currently it stops at the outer `step_walk`
  per the deferral comment).
- After loading the parent directory's `InodeMeta`, before
  calling `lookup`, run
  `check_perm(&parent_meta, cred, PermBit::Execute)`. On
  `false`, return `StepOutcome::Err(Errno::EACCES)`.
- Map the new error site to `WalkCause::TraverseDenied`
  (`VFS_CHECKS_V2.1.md` line 281-290) — the spec already
  names this cause, but the walker has no emitter today.
  Slice ships the first.

#### Tests
(in `crates/tx-subsystems/src/vfs/walker/tests.rs`):

- `step_walk_denies_traverse_on_zero_x_intermediate_dir` —
  build a tmpfs with `dir1` having mode `0o600` (owner has
  no X), uid `cred.uid`; walk `/dir1/file`; expect EACCES.
- `step_walk_allows_root_to_traverse_zero_x_intermediate_dir`
  — same fixture; walker called with `Credential::root()`;
  expect success.
- `step_walk_allows_cap_dac_override_to_traverse` — same
  fixture; walker called with a non-root `Credential` whose
  `effective_caps` contains `Capability::DAC_OVERRIDE`;
  expect success.
- `step_walk_uses_other_triplet_when_uid_and_gid_mismatch` —
  dir mode `0o001` (only "other" can X); walker uid 1001,
  dir uid 1000, gid 1000; expect success.
- `step_walk_denies_other_triplet_when_zero_x_in_other` —
  dir mode `0o770` (no X for other); walker uid 9999;
  expect EACCES.

### B. `step_open` mode validation

At final dentry materialise time
(`walker.rs::step_open:108-137`), check the requested
`OpenFileFlags` (read/write/executable) against the inode's
mode bits + cred. The current implementation drops `mode`
explicitly with `let _ = mode;` (`walker.rs:122`); the slice
extends the validation.

- After resolving the terminal `dentry` (line 124), load the
  `InodeMeta` for the terminal RNode (the rnode already has
  it accessible via `dentry.rnode().meta()` — verify the
  accessor exists; if not, add it).
- Compose the requested permission bits from `OpenFileFlags`:
  ```text
  let want = match (flags.read, flags.write) {
      (true, true)  => &[PermBit::Read, PermBit::Write][..],
      (true, false) => &[PermBit::Read][..],
      (false, true) => &[PermBit::Write][..],
      (false, false) => &[][..],
  };
  ```
- For each `want` bit, run `check_perm`; on the first `false`,
  return `StepOutcome::Err(Errno::EACCES)`.
- For `O_TRUNC` (not in OpenFileFlags today; deferred), the
  Linux rule adds W requirement; out of scope.
- The exec_script consumer (Phase 1 of the script) opens with
  `OpenFileFlags { read: true, ... }` and the terminal's
  mode must include X for the *exec* check — but that check
  lives in the `cred::checks::require_executable` Phase 2 of
  EXEC_v1, not in `step_open`. Slice runs the X check inline
  in `exec_script` Phase 1 instead (Part 5A).

#### Tests

- `step_open_allows_read_on_owner_readable_file`.
- `step_open_denies_read_on_owner_no_read_file`.
- `step_open_denies_write_on_read_only_file`.
- `step_open_allows_root_to_open_zero_perm_file_for_read`.

### C. FsOps::step_chmod / step_chown

New trait methods on `FsOps`
(`vfs/execution.rs:20-156`); default returns `ENOSYS`; tmpfs
overrides; devfs inherits the default.

- New trait methods (extending the existing `FsOps`):
  ```text
  fn step_chmod(
      &self,
      fs_object_id: FsObjectId,
      new_mode: u16,
      cred: &Credential,
      guard: &Guard<'_>,
  ) -> StepOutcome<()> {
      let _ = (fs_object_id, new_mode, cred, guard);
      StepOutcome::Err(Errno::ENOSYS)
  }
  fn step_chown(
      &self,
      fs_object_id: FsObjectId,
      new_uid: Option<u32>,
      new_gid: Option<u32>,
      cred: &Credential,
      guard: &Guard<'_>,
  ) -> StepOutcome<()> {
      let _ = (fs_object_id, new_uid, new_gid, cred, guard);
      StepOutcome::Err(Errno::ENOSYS)
  }
  ```
- Tmpfs override (`crates/tx-fs/src/tmpfs.rs`): mutates the
  in-place `InodeMeta` inside `state.inodes:
  BTreeMap<FsObjectId, TmpfsInode>` under the existing
  `SpinMutex`. Permission rules:
  - `step_chmod`: caller is owner (`cred.uid == meta.uid`)
    OR has `CAP_FOWNER`. Errno: `EPERM` (not EACCES — this
    is Linux's chmod-specific rule).
  - `step_chmod`: preserve the S_IFMT bits
    (`new_mode & !S_IFMT | meta.mode & S_IFMT`); the caller
    cannot change file kind via chmod.
  - `step_chown`: only `CAP_CHOWN` (root) can change
    ownership. `chown(uid=-1, gid=-1)` is a no-op even for
    non-root (POSIX). `EPERM` otherwise.
- Devfs override: `EROFS` (devfs nodes are kernel-owned);
  out of scope to extend devfs to support these.

#### Tests
(`crates/tx-fs/src/tmpfs/tests.rs`):

- `step_chmod_owner_can_clear_setuid_bit`.
- `step_chmod_non_owner_returns_eperm`.
- `step_chmod_root_can_modify_any_file`.
- `step_chmod_preserves_s_ifmt_bits`.
- `step_chown_root_can_change_ownership`.
- `step_chown_non_root_returns_eperm`.
- `step_chown_negone_sentinel_means_no_change_for_that_field`.

## Part 3 — Process-side syscall arms (10 new)

The cred-mutation arms. Each is a small wrapper around an
already-existing or Part 1C-added cred helper.

### Numbers

Verified against Linux's RV64 generic ABI
(`asm-generic/unistd.h`); these are NOT all in tree today:

```text
NR_GETUID    = 174
NR_GETEUID   = 175
NR_GETGID    = 176
NR_GETEGID   = 177
NR_SETUID    = 146
NR_SETGID    = 144
NR_SETREUID  = 145    // NB: collides with the research
                       // note's preliminary "RV64 generic uses
                       // 145 = setregid" — verify against the
                       // actual asm-generic/unistd.h head before
                       // commit. See Open Question #1.
NR_SETREGID  = 143
NR_SETRESUID = 147
NR_SETRESGID = 149
NR_GETRESUID = 148   // optional — nice to have for round-trip tests
NR_GETRESGID = 150   // optional
```

Add to
`crates/tx-shims/src/linux_syscall/numbers.rs` with one-line
docstrings citing
`txdoc:PROCESS-CREDENTIAL-SERVICE-DRAFT-1` and the relevant
LTP cluster.

### Behaviour

Each arm reads `ctx.cred()` (Part 7) for the privilege
check — returning `EPERM` on `CredChange::PermissionDenied`,
`Return(0)` on `Replaced`, `ESRCH` on `Zombie` (impossible
in practice since the caller is by definition alive; defensive).

Read-side arms (one-liners):

- `sys_getuid(ctx) -> Return(ctx.cred().uid.0 as i64)`.
- `sys_geteuid(ctx) -> Return(ctx.cred().euid.0 as i64)`.
- `sys_getgid(ctx) -> Return(ctx.cred().gid.0 as i64)`.
- `sys_getegid(ctx) -> Return(ctx.cred().egid.0 as i64)`.

Single-arg setters:

- `sys_setuid(args, ctx)`: `step_setuid(&ctx.process,
  Uid(args[0] as u32))` → map outcome.
- `sys_setgid(args, ctx)`: `step_setgid(&ctx.process,
  Gid(args[0] as u32))` → map outcome.

Two-arg setters:

- `sys_setreuid(args, ctx)`: translate `(u32) -1` sentinel
  (`args[i] == 0xFFFF_FFFF`) to `None`, others to
  `Some(Uid(...))`; call `step_setreuid(&ctx.process, ruid,
  euid)`.
- `sys_setregid(args, ctx)`: same with `Gid`.

Three-arg setters:

- `sys_setresuid(args, ctx)`: translate the three sentinels;
  call `step_setresuid(&ctx.process, ruid, euid, suid)`.
- `sys_setresgid(args, ctx)`: same with `Gid`.

(Optional) read-side getters:

- `sys_getresuid(args, ctx)`: writes `*ruid_ptr`,
  `*euid_ptr`, `*suid_ptr`; returns `0`. Same kernel-buffer
  bootstrap-exemption SAFETY comment as `sys_write` /
  `sys_read` (`linux_syscall/mod.rs:336`). Verifies the
  three IDs round-trip.
- `sys_getresgid(args, ctx)`: same.

### Dispatch arms

Add to `dispatch::<P>` (`linux_syscall/mod.rs:272-296`),
matching the existing `nr if nr == NR_*` shape:

```text
nr if nr == NR_GETUID    => sys_getuid(ctx),
nr if nr == NR_GETEUID   => sys_geteuid(ctx),
nr if nr == NR_GETGID    => sys_getgid(ctx),
nr if nr == NR_GETEGID   => sys_getegid(ctx),
nr if nr == NR_SETUID    => sys_setuid(req.args, ctx),
nr if nr == NR_SETGID    => sys_setgid(req.args, ctx),
nr if nr == NR_SETREUID  => sys_setreuid(req.args, ctx),
nr if nr == NR_SETREGID  => sys_setregid(req.args, ctx),
nr if nr == NR_SETRESUID => sys_setresuid(req.args, ctx),
nr if nr == NR_SETRESGID => sys_setresgid(req.args, ctx),
```

(Plus the optional `getresuid`/`getresgid` arms.)

### Tests
(`crates/tx-shims/src/linux_syscall/tests.rs`):

- `sys_getuid_returns_zero_for_root_init`.
- `sys_geteuid_returns_zero_for_root_init`.
- `sys_setuid_root_to_1000_changes_all_three_ids`.
- `sys_setuid_non_privileged_swap_succeeds`.
- `sys_setuid_non_privileged_unrelated_uid_returns_eperm`.
- `sys_setresuid_negone_sentinel_translates_to_none`.
- `sys_setreuid_non_privileged_bumps_suid_when_euid_changes`.
- `sys_getresuid_round_trip_after_setresuid`.
- 4 `gid` companions of the above shape.

## Part 4 — File-mode syscall arms

### Numbers

Verified against Linux's RV64 generic ABI:

```text
NR_FCHMODAT  = 53
NR_FCHMOD    = 52
NR_FCHOWNAT  = 54
NR_FCHOWN    = 55
NR_FACCESSAT = 48
NR_FACCESSAT2 = 439
```

Note: `NR_CHMOD`, `NR_CHOWN`, `NR_LCHOWN`, `NR_ACCESS` are
**not** present in the RV64 generic ABI — Linux RV64 only
ships the `*at` variants. glibc/musl implement the
non-`*at` calls as wrappers over the `*at` form with
`AT_FDCWD = -100`. The slice ships only the kernel-side
`*at` arms; `AT_FDCWD` (-100 cast to `i32`) routes to
`process.cwd()`.

### Behaviour

- `sys_fchmodat(args, ctx)`:
  - `args[0]` = dirfd; the slice supports only `AT_FDCWD`
    (-100). Other dirfds (positive) need a
    `dirfd → Cap<DEntry>` lookup the slice doesn't add;
    return `-EBADF` for now. Open Question below.
  - `args[1]` = path uaddr.
  - `args[2]` = mode.
  - `args[3]` = flags (`AT_SYMLINK_NOFOLLOW = 0x100`); v1
    accepts both 0 and `AT_SYMLINK_NOFOLLOW`, walker handles.
  - Walk the path → terminal RNode → call
    `fs_ops.step_chmod(fs_object_id, mode, &cred, &guard)`.
- `sys_fchmod(args, ctx)`:
  - `args[0]` = fd; resolve via `resolve_fd`
    (`linux_syscall/mod.rs:322` already exists).
  - `args[1]` = mode.
  - Read the openfile's RNode → call `step_chmod`.
- `sys_fchownat(args, ctx)`:
  - `args[0]` = dirfd (only `AT_FDCWD`).
  - `args[1]` = path uaddr.
  - `args[2]` = uid (u32).
  - `args[3]` = gid (u32).
  - `args[4]` = flags.
  - Translate the `(u32) -1` sentinel for "no change". Walk;
    call `step_chown`.
- `sys_fchown(args, ctx)`:
  - `args[0]` = fd; resolve.
  - `args[1]` = uid; `args[2]` = gid.
  - Same sentinel translation.
- `sys_faccessat(args, ctx)`:
  - `args[0]` = dirfd.
  - `args[1]` = path uaddr.
  - `args[2]` = mode bits to test (R_OK=4, W_OK=2, X_OK=1,
    F_OK=0).
  - `args[3]` = flags (`AT_EACCESS = 0x200` — use euid/egid;
    default to ruid/rgid; v1 always uses ruid/rgid for
    simplicity since Linux's distinction matters mostly for
    setuid binaries running `access` to check on behalf of
    the real user — closing in a follow-up if LTP fails).
  - Walk; on success run `check_perm` for each requested bit
    against `meta`.
  - F_OK alone (`mode == 0`) is "exists?" — walker success
    is enough; return `0`.
- `sys_faccessat2(args, ctx)`: same as `sys_faccessat` plus
  honours `AT_EACCESS` properly. Linux added this syscall
  precisely to fix the AT_EACCESS-honouring gap; v1 ships it
  as an alias for `faccessat` since the slice's `faccessat`
  already accepts the flag.

### Dispatch arms

```text
nr if nr == NR_FCHMOD    => sys_fchmod(req.args, ctx),
nr if nr == NR_FCHMODAT  => sys_fchmodat(req.args, ctx).await,
nr if nr == NR_FCHOWN    => sys_fchown(req.args, ctx),
nr if nr == NR_FCHOWNAT  => sys_fchownat(req.args, ctx).await,
nr if nr == NR_FACCESSAT => sys_faccessat(req.args, ctx).await,
nr if nr == NR_FACCESSAT2 => sys_faccessat2(req.args, ctx).await,
```

The `*at` arms are `async` because they call into
`step_walk` which is `async fn` (matches `sys_execve`'s
shape).

### Tests

- `sys_fchmod_owner_can_change_mode`.
- `sys_fchmod_non_owner_returns_eperm`.
- `sys_fchmodat_atfdcwd_walks_relative_path`.
- `sys_fchmodat_clears_setuid_bit_via_explicit_mode`.
- `sys_fchown_root_changes_ownership`.
- `sys_fchown_non_root_returns_eperm`.
- `sys_faccessat_f_ok_returns_zero_for_existing_file`.
- `sys_faccessat_r_ok_returns_eacces_for_unreadable_file`.
- `sys_faccessat_x_ok_returns_zero_for_executable`.

## Part 5 — `exec_script` setuid handling

### A. Pre-Phase-6 exec auth (X-bit + ownership)

`exec_script::Phase 1` opens the file with `OpenFileFlags
{ read: true, ... }`; the **execute** authorisation is a
separate check that today is absent. Add one site in
Phase 1 (after `step_open` succeeds, before `step_open`'s
return is consumed), or as the first action of Phase 2 (the
parser block, line 290).

- Read the inode meta: `let exec_meta = openfile.rnode().meta();`
- Run `check_perm(&exec_meta, &cred, PermBit::Execute)`. On
  `false`, return `Err(ExecError::AccessDenied)` (a new
  `ExecError` variant if not present today; map to
  `-EACCES` via `execve_errno_magnitude`).
- This closes LTP `execve02` (non-root-can't-execute-0700-file)
  and the EACCES sub-cases of `execve03` (no-execute-bit on
  binary).

### B. Setuid recompute helper (cred mutation pre-PoNR)

Add a new helper colocated with the cred service:

- `crates/tx-subsystems/src/cred.rs`:
  ```text
  pub struct ExecCredOutcome {
      pub at_secure: bool,
  }
  pub fn step_apply_suid_for_exec(
      target: &Cap<ProcessIdentity>,
      file_uid: Uid,
      file_gid: Gid,
      file_mode: u16,
  ) -> ExecCredOutcome
  ```
- Body:
  1. Lock payload + cred under the same discipline as
     `step_setuid`.
  2. Snapshot `prev_euid = cred.euid`, `prev_egid = cred.egid`.
  3. If `file_mode & S_ISUID != 0`: `cred.euid = file_uid`.
  4. If `file_mode & S_ISGID != 0`: `cred.egid = file_gid`.
  5. Always update saved-set: `cred.suid = cred.euid`,
     `cred.sgid = cred.egid` (Linux: post-exec, saved-set is
     copied from new effective).
  6. Compute `at_secure = (cred.euid != prev_euid) ||
     (cred.egid != prev_egid)` — Linux's short-form rule.
     (Real Linux's rule is more nuanced — file caps,
     `nosuid` mounts, etc. — but for the slice's surface this
     captures the LTP-relevant case.)
  7. Fence + return.
- New `S_ISUID`, `S_ISGID` constants on
  `vfs/structure.rs`'s S_IFMT block:
  - `S_ISUID = 0o4000`, `S_ISGID = 0o2000`,
    `S_ISVTX = 0o1000` (sticky bit; not used by the slice
    but document for completeness).

### C. Wire the helper into `exec_script`

The recompute MUST happen pre-Phase 6 (the
`replace_aspace` PoNR at `script.rs:420`). The
`AuxvFacts.at_secure` value is needed at Phase 5
(`build_initial_user_stack` at `script.rs:365`); so the
helper actually has to fire **before** Phase 5.

Concrete placement: between Phase 3 (parse + validate, line
326-327) and Phase 4 (build aspace, line 345). At that point
`exec_meta` is in scope, parsing succeeded, and no
irreversible commit has happened.

```text
// === New: Phase 3.5 — apply setuid/setgid file mode bits ===
let exec_outcome = cred::step_apply_suid_for_exec(
    process,
    Uid(exec_meta.uid),
    Gid(exec_meta.gid),
    exec_meta.mode,
);
let at_secure = if exec_outcome.at_secure { 1 } else { 0 };
```

Then the Phase 5 `AuxvFacts` builder consumes the post-recompute
cred via a fresh `ctx.cred()` snapshot (or a returned tuple
from the helper) for `at_uid` / `at_euid` / `at_gid` / `at_egid`.

### Tests
(`crates/tx-subsystems/src/cred/tests.rs` and
`crates/tx-scripts/src/process/exec/tests.rs`):

- `step_apply_suid_for_exec_set_isuid_changes_euid_to_file_owner`.
- `step_apply_suid_for_exec_clear_isuid_preserves_euid`.
- `step_apply_suid_for_exec_at_secure_set_when_euid_elevated`.
- `step_apply_suid_for_exec_at_secure_clear_when_no_change`.
- `step_apply_suid_for_exec_post_exec_suid_equals_new_euid`.
- `exec_script_clears_at_secure_for_non_setuid_binary`.
- `exec_script_sets_at_secure_for_setuid_binary_with_different_owner`.
- `exec_script_phase_3_5_runs_pre_phase_6_ponr` — race smoke:
  inject a failing parser; assert cred is unchanged
  (the helper hasn't run yet because Phase 3.5 is *after*
  parse). Inject a panicking `replace_aspace` (test-only
  feature flag); assert cred IS changed pre-PoNR — that is
  the desired ordering, since the recompute is pre-PoNR;
  the cred mutation is committed but the aspace swap aborts.
  This is fine: Linux's exec failure mode here is "process
  has fresh cred but old aspace" — equivalent to a
  fork-then-immediate-fail; the LTP suite doesn't check this
  edge (it's not user-observable: a Phase 6 failure means
  the process is heading to a panic anyway, since `replace_aspace`
  is infallible).

## Part 6 — `AuxvFacts` extension

Extend `AuxvFacts` with the 5 fields the setuid path needs.
`build_initial_user_stack` grows from emitting 6 auxv pairs
to 11.

### Surface

- Extend `AuxvFacts`
  (`crates/tx-scripts/src/process/exec/stack.rs:81-91`):
  ```text
  pub struct AuxvFacts {
      pub at_phdr: u64,
      pub at_phent: u64,
      pub at_phnum: u64,
      pub at_pagesz: u64,
      pub at_uid: u64,    // NEW
      pub at_euid: u64,   // NEW
      pub at_gid: u64,    // NEW
      pub at_egid: u64,   // NEW
      pub at_secure: u64, // NEW (0 or 1)
  }
  ```
- Add the 5 auxv `a_type` constants (`stack.rs:38-44`):
  ```text
  const AT_UID:    u64 = 11;
  const AT_EUID:   u64 = 12;
  const AT_GID:    u64 = 13;
  const AT_EGID:   u64 = 14;
  const AT_SECURE: u64 = 23;
  ```
- Update `AUXV_PAIR_COUNT` (`stack.rs:60`) from 6 to 11.
- `build_initial_user_stack` body emits the 5 new pairs in
  the same order as the Linux kernel
  (`fs/binfmt_elf.c::create_elf_tables`): AT_PHDR, AT_PHENT,
  AT_PHNUM, AT_PAGESZ, AT_UID, AT_EUID, AT_GID, AT_EGID,
  AT_SECURE, AT_RANDOM, AT_NULL.

### Plumbing

- `exec_script::Phase 5` (`script.rs:359-364`) constructs
  `AuxvFacts` — extend it to read post-recompute cred values.
  The cred snapshot for the auxv has to come from
  `process.payload_cap()?.cred.lock()` after Phase 3.5 (Part
  5C) has run.
- For the bootstrap exec (`init.rs:651` /
  `:741`): the bootstrap process is root, so
  `at_uid = at_euid = at_gid = at_egid = 0`,
  `at_secure = 0`. Trivial.

### Tests
(`crates/tx-scripts/src/process/exec/tests.rs` and
`crates/tx-scripts/src/process/exec/stack/tests.rs` if it
exists — else inline in `stack.rs`):

- `auxv_emits_five_security_entries_in_correct_order`.
- `auxv_at_uid_matches_post_recompute_cred`.
- `auxv_at_secure_set_for_setuid_binary`.
- `auxv_at_secure_clear_for_non_setuid_binary`.
- `auxv_table_byte_layout_matches_expected_pair_count` —
  pin the byte count; defends against accidental drift.

## Part 7 — `SyscallCtx::cred()` accessor

Wires `ctx.cred()` so `sys_execve` and the new arms reach the
current process's cred without going through
`Credential::default()`. This is the one cross-cutting piece;
naming it Part 7 makes the call-site sweep mechanical (every
arm in Parts 3, 4, and 5 reads through it).

### Surface

- New method on `SyscallCtx`
  (`linux_syscall/mod.rs:201-227`):
  ```text
  impl<'a> SyscallCtx<'a> {
      /// Snapshot the current process's `Cred`. Atomic snapshot:
      /// the field on `ProcessPayload` is `SpinMutex<Cred>` and
      /// `Cred` is `Copy`, so readers get a coherent view under
      /// one lock acquisition.
      ///
      /// Returns `Cred::root()` if the process is a zombie
      /// (impossible in practice from inside a syscall arm; the
      /// caller is by definition alive). Defensive default keeps
      /// the syscall arms' Optional-noise low.
      pub fn cred(&self) -> Cred {
          self.process
              .payload_cap()
              .map(|p| *p.cred.lock())
              .unwrap_or_else(Cred::root)
      }
  }
  ```
- Sibling helper for the walker:
  ```text
  pub fn walker_cred(&self) -> Credential {
      Credential::for_walker(&self.cred())
  }
  ```
  (Reused by every `*at` arm in Part 4 and by `sys_execve`.)

### Sweep

- `linux_syscall/mod.rs:878` (`sys_execve`): replace
  `let cred = Credential::default();` with
  `let cred = ctx.walker_cred();`. Remove the
  `TODO(phase-cred-on-ctx)` marker.
- `init.rs:651`, `:741` (bootstrap): keep as
  `Credential::root()` (the bootstrap process is root by
  construction; `ctx.cred()` isn't reachable since the
  bootstrap-exec calls `exec_script` directly without a
  `SyscallCtx`).

### Tests

- `syscall_ctx_cred_returns_payload_cred`.
- `syscall_ctx_cred_returns_root_for_zombie` — defensive case;
  zombie payload returns `None` from `payload_cap`.
- `syscall_ctx_walker_cred_uses_euid_egid` — pin the
  POSIX-DAC rule (DAC checks consult euid/egid, not real
  uid/gid).
- `sys_execve_uses_caller_cred_not_default` — fork a child
  with non-root cred (after Part 3 lands `setresuid`); call
  `sys_execve` from the child; assert the walker received
  the child's cred (smoke via a test-only side-channel slot
  in `vfs/walker/tests.rs`).

## Part 8 — End-to-end smoke

Layer A (production-paths-up-to-divergence; matches the
fork/clone/wait4 plan's choice). Build a fixture binary with
the setuid bit set; bootstrap-exec a "switcher" process that
runs as a non-root uid and execve's the setuid fixture;
assert the post-exec cred matches the file owner; assert
`AT_SECURE = 1` in the auxv stack.

### Decision: extend `init_fixture.rs` once more

The fork/clone/wait4 slice already grew the fixture to a
fork+wait shape. This slice's smoke needs a *different*
shape (drop privs → execve setuid binary → observe new euid).
Two options:

1. **Extend `init_fixture.rs` again** — pile on. Total
   instruction count creeps toward ~120; readable but the
   single-fixture-per-fork-of-a-fork pattern is starting to
   strain.
2. **Sibling fixture: `init_setuid_fixture.rs`** — second
   fixture under a separate const blob, second host-test
   bootstrap. The trio's fixture stays as the
   fork/wait/exit smoke; the new fixture is a setuid drop
   target.

**Decision: (2).** The fork/wait fixture is a stable smoke
target that's been pinned to ~7 byte-tests; rewriting it
again invites churn. A sibling fixture keeps both smokes
intact.

### Fixture sketch (`init_setuid_fixture.rs`)

The fixture is a "switcher" + a "target":

- **Switcher** (the fixture's `_start`): a binary that
  drops privs (calls `setresuid` to non-zero), reads its
  euid back to confirm, then `execve`s the target. The
  switcher binary is owned by root with mode `0o755`.
- **Target** (a second const blob): a binary with mode
  `S_ISUID | 0o755`, owned by `uid = 1000`. Reads its uid
  vs euid via `getuid` / `geteuid`, writes the result to
  fd 1, exits.

For the host smoke, the bootstrap can execute the target
*directly* with a synthesized non-root caller cred — no need
for the switcher binary. The switcher path is "more
realistic" but the slice's smoke doesn't gate on it; the
direct path is sufficient.

```text
target:
    li   a7, NR_GETUID
    ecall
    ; a0 now holds ruid (= 1001 if executed by uid-1001 caller,
    ; *not* changed by the setuid bit — Linux preserves ruid)
    mv   t0, a0
    li   a7, NR_GETEUID
    ecall
    ; a0 now holds euid (= 1000 if setuid bit took effect)
    ; (in the smoke: write 8 bytes "ru:NN eu:MM\n" to fd 1)
    ...
    li   a7, NR_EXIT_GROUP
    li   a0, 0
    ecall
```

### Smoke shape (`crates/tx-kernel/src/init/tests.rs`)

`boot_smoke_setuid_binary_recomputes_effective_uid_at_exec`:

1. Drive `drive_boot_wiring`.
2. Build a tmpfs fixture: write the setuid binary at
   `/setuid-target` with mode `S_ISUID | 0o755`, uid 1000.
3. Build init's process; manually set its cred to
   `{ uid: 1001, euid: 1001, ... }` via `step_setresuid`.
4. Drive `run_bootstrap_exec_for_init` with path
   `/setuid-target`; the exec_script's Phase 3.5 should
   recompute the cred to `euid = 1000`.
5. Spawn the production reactor loop.
6. Drain. Assert:
   - Post-exec, `init.cred().uid == 1001` (real uid
     preserved).
   - Post-exec, `init.cred().euid == 1000` (setuid bit
     applied).
   - Post-exec, `init.cred().suid == 1000` (saved-set
     copied from new euid per Linux semantics).
   - The auxv stack image's `AT_SECURE` slot is `1`
     (assertion via reading the populated stack range
     post-exec).
   - Console captured `b"ru:1001 eu:1000\n"` (or whatever
     the fixture writes).
   - The process reaches zombie cleanly with
     `ExitStatus::Exited(0)`.

`boot_smoke_execve_returns_eacces_for_non_executable_binary`:

1. Drive `drive_boot_wiring`.
2. Tmpfs fixture: a regular file at `/no-exec` with mode
   `0o600` (no execute bits), owned by uid 0.
3. Build init's process; cred = `uid: 1001` (non-root).
4. Drive `run_bootstrap_exec_for_init` with path
   `/no-exec`; expect `ExecError::AccessDenied` →
   `Err(-EACCES)` from the bootstrap helper. (This is LTP
   `execve02`'s shape.)

## Cross-cutting risks

1. **`Cred` mutability vs walker re-entry.** The slice's
   `step_setresuid` family mutates `payload.cred` under the
   same `SpinMutex`. A concurrent `step_walk` running for
   the same process holds a `Credential` *snapshot* (taken
   via `ctx.walker_cred()` at syscall entry, not a borrow of
   the lock). Since `Credential` is `Copy` and the snapshot
   is taken once, no live reference into the cred lock
   persists across the walker's `.await` points. Mitigation
   already encoded by the design; the slice just has to
   keep `walker_cred` returning a `Credential` value, never
   a `&Credential` reference into the lock.
2. **Test scaffolding: TestPlatform's "user" cred.** The
   trio's `TestPlatform` (and the bootstrap test fixtures)
   defaults all processes to `Cred::root()` via
   `bootstrap_init_process`. The slice's EACCES tests need a
   real non-root cred — Mitigation: the test scaffolding
   adds a helper
   `crate::init::tests::with_non_root_cred(&cap, uid)` that
   calls `step_setresuid(cap, Some(Uid(uid)), Some(Uid(uid)),
   Some(Uid(uid)))` after bootstrap, before the
   tested step runs. Documented in the smoke test module.
3. **`AT_SECURE` timing.** `at_secure` must be set
   **before** `build_initial_user_stack` composes the auxv
   table at Phase 5 (`script.rs:365`). This means
   cred-recompute (Phase 3.5) precedes stack-build. The
   plan's Part 5C explicitly orders Phase 3.5 between Phase
   3 and Phase 4 — the slice ships this ordering. Grep
   target: any future refactor that moves Phase 3.5 must
   leave it pre-Phase-5.
4. **`chmod` clearing the setuid bit silently.** Linux's
   `chmod(2)` rule: if the caller is non-root and the file's
   group is one the caller doesn't belong to, the kernel
   silently clears `S_ISGID` on the chmod even if the
   caller didn't request it. (`S_ISUID` is *not* cleared on
   non-owner chmod — the kernel returns EPERM first.) The
   slice's `step_chmod` honours the basic permission check
   (owner or CAP_FOWNER); the silent-clear-S_ISGID nuance is
   a Linux quirk. Mitigation: the slice's `step_chmod`
   ignores it for v1; LTP `chmod07` (the silent-clear test)
   stays gated. Flag for follow-up; document under "Out of
   scope".
5. **`step_apply_suid_for_exec` failure window.** The helper
   is documented as infallible (the cred mutation is a lock
   + bitwise op + fence); however, if the parser's
   `parse_image_plan` succeeds but a later phase
   (Phase 4 build_aspace) fails, the cred has already been
   mutated. Linux's behaviour at this point is undefined —
   in practice, the process keeps the new cred and panics
   on the next userspace touch (because the aspace was
   never replaced). v1 absorbs this: `replace_aspace` is
   infallible; Phase 4 failure modes are limited to ENOMEM
   from `vm_scripts::build_aspace_from_image`. Open Question
   below: should the helper run *after* Phase 4
   (post-aspace-build, pre-Phase 6 PoNR)? Trade-off: later
   placement is safer (more failure modes pre-PoNR); earlier
   placement (Phase 3.5) lets Phase 5's auxv read the
   post-recompute cred without a re-snapshot. The plan
   defaults to Phase 3.5 with a one-line accommodation
   (Phase 5 reads `process.payload_cap()?.cred.lock()`
   fresh, so a later phase placement also works). Decided
   below.
6. **`Capability::CAP_FOWNER` is missing today.** The
   research note's `Capability` enumeration
   (`cred.rs:71-127`) lists `CHOWN, DAC_OVERRIDE, KILL,
   SETGID, SETUID, NET_ADMIN, SYS_ADMIN`. **`FOWNER` is
   absent.** The slice adds it as a new `const FOWNER: Self
   = Self(3);` entry — Linux's POSIX cap number is 3.
   Backwards-compatible (additions to the enum don't break
   existing code); document in the cred module's preamble.
7. **`ECHILD` / `EPERM` / `EACCES` errno constants.** The
   `linux_syscall/mod.rs` errno table already has `EBADF`,
   `E2BIG`, `EINVAL`, `ENOSYS` and (post fork/clone/wait4)
   `ECHILD`. The slice adds `EPERM = 1` and `EACCES = 13`
   if not already in tree (the `Errno` enum already has
   them; the `*_VALUE` u-32 mapping needs verification
   pre-commit).
8. **Build-time order of test crates.** The cred extension
   (Part 1A) is in `tx-subsystems`; the cred unit tests
   live in `crates/tx-subsystems/src/cred/tests.rs`. The
   syscall arms (Part 3) are in `tx-shims`. The walker DAC
   plumbing (Part 2) is in `tx-subsystems`. Inter-crate
   build order is already settled (`tx-shims` depends on
   `tx-subsystems`); the slice just has to land in part
   order to keep CI green per-PR.
9. **`Credential::default()` semantic shift (Part 1D).** The
   walker DAC predicate makes `Credential::default()` mean
   "uid 0, gid 0, no caps" — *not* root. Existing tests
   that synthesise `Credential::default()` expecting "always
   allowed" will start hitting `EACCES` once Part 2A lands.
   Mitigation: PR ordering — Part 1D (walker shape change)
   ships *with* a sweep migrating
   `Credential::default()` to `Credential::root()` in
   walker tests. The single PR keeps CI green; the
   compile-without-tests-passing window is zero.
10. **Optional `getresuid` / `getresgid` arms.** Listed as
    "optional" in Part 3 because they're not on the LTP
    coverage gating list (LTP gates on the *setters*). The
    plan ships them anyway because they make round-trip
    testing one-arm — without them, the test would have to
    re-call `setresuid` and check via `getuid`/`geteuid`
    only (no way to read `suid`). Decision: ship.

## Out of scope (deliberately deferred)

- **Capability-syscall family** (`capset(2)`, `capget(2)`,
  `prctl(PR_CAPBSET_*)`, ambient capabilities, file
  capabilities via xattrs). The cred shape carries
  `effective_caps` + `permitted_caps` already; the syscalls
  to read/write them aren't wired. **Defer** to a capability
  slice.
- **POSIX ACLs** (`getxattr` / `setxattr` for `system.posix_acl_*`).
  Orthogonal subsystem. **Defer.**
- **MAC** (SELinux, AppArmor, etc.). Orthogonal subsystem;
  not on any txKernel roadmap. **Defer.**
- **`prctl(PR_SET_KEEPCAPS)` and friends.** Tied to the
  capability slice. **Defer.**
- **Capabilities-on-exec** (file-cap xattr application + the
  ambient/inheritable interplay with `CAP_FILE_CAP_*`).
  Tied to file capabilities. **Defer.**
- **Filesystems other than tmpfs for chmod/chown.** Devfs
  nodes are kernel-owned (return `EROFS`); ext4 is its own
  slice (already separately planned). **Defer.**
- **`chmod` silent-clear-S_ISGID semantic.** Linux quirk
  documented under risk #4. **Defer.**
- **Mount `nosuid` flag.** EXEC_v1 §3.2 pins the spec;
  `MountIdentity` doesn't track flags today. The slice
  ignores `nosuid` (always honours setuid). **Defer with
  the mount-flags slice.**
- **POSIX supplementary group list** (`setgroups(2)` /
  `getgroups(2)`, group permission via membership). `Cred`
  doesn't carry one. The walker's group check uses
  `egid == file.gid` only. **Defer with a supplementary-
  groups slice.**
- **`setfsuid(2)` / `setfsgid(2)`.** Linux-specific
  filesystem-uid distinction; PROCESS_v1 §11.1 defers.
  **Defer.**
- **`AT_EACCESS` honouring in `faccessat`.** v1 always uses
  ruid/rgid; the LTP suite mostly doesn't gate on the
  distinction. If a specific LTP test fails, follow-up adds
  honouring (5-line change). **Defer.**
- **Per-fd permission inheritance / `O_PATH` discipline.**
  Linux's `O_PATH` lets you open a file you have only
  search-X on but no R/W. `OpenFileFlags` has no `path`
  variant. **Defer.**
- **`lchown` (separate syscall) and the symlink-no-follow
  variant of all `*at` arms.** The walker handles the
  no-follow flag; the slice routes it for the `*at` arms
  but `lchown` is a separate syscall not in the RV64 generic
  ABI. **Defer.**
- **Cross-pid `chmod` / `fchmod` paths.** Need a
  `dirfd → Cap<DEntry>` lookup table for non-AT_FDCWD
  dirfds. The slice ships AT_FDCWD only. **Defer with a
  dirfd-table slice.**

## Phasing

Each step is a self-contained PR. Land in this order; later
parts depend on earlier ones (Part 7 is the bridge that lets
Parts 3-5 actually plug in to ctx.cred()).

1. **S — Part 1A `Cred.suid/sgid` fields + B step_setuid/gid
   updates + 1C `step_set*` helpers.** Touches
   `crates/tx-subsystems/src/cred.rs` only. Pure logic + 6-8
   new unit tests. Estimated ~250 LOC.
2. **S — Part 1D `Credential.effective_caps` extension +
   sweep.** Touches `crates/tx-subsystems/src/vfs/structure.rs`,
   `vfs/mod.rs`, `crates/tx-fs/src/tmpfs.rs` (test sweep),
   `crates/tx-subsystems/src/vfs/walker/tests.rs` (test
   sweep), `crates/tx-kernel/src/init.rs` (production
   `Credential::default()` → `Credential::root()` migration).
   Estimated ~100 LOC.
3. **M — Part 2A walker DAC predicate + B step_open mode
   validation.** Touches
   `crates/tx-subsystems/src/vfs/walker.rs`,
   `crates/tx-subsystems/src/vfs/structure.rs`. Cross-cutting
   risk #9 mitigation lives here (the
   `Credential::default()` → `Credential::root()` migration
   in production code paths is part of this PR). 9 tests.
   Estimated ~300 LOC.
4. **M — Part 2C FsOps::step_chmod / step_chown + tmpfs
   override.** Touches
   `crates/tx-subsystems/src/vfs/execution.rs` (trait),
   `crates/tx-fs/src/tmpfs.rs` (override + tests). 7 tests.
   Estimated ~250 LOC.
5. **S — Part 7 `SyscallCtx::cred()` accessor.** Touches
   `crates/tx-shims/src/linux_syscall/mod.rs`. Replaces
   `Credential::default()` in `sys_execve`. 4 tests.
   Estimated ~80 LOC.
6. **M — Part 3 cred syscall arms (10 new).** Touches
   `crates/tx-shims/src/linux_syscall/numbers.rs` and
   `mod.rs`. ~12 tests including round-trips.
   Estimated ~400 LOC.
7. **M — Part 4 file-mode syscall arms (6 new).** Touches
   the same. ~9 tests.
   Estimated ~350 LOC.
8. **M — Part 5 exec_script setuid handling (5A exec auth
   + 5B helper + 5C wire).** Touches
   `crates/tx-subsystems/src/cred.rs` (helper),
   `crates/tx-subsystems/src/vfs/structure.rs` (S_ISUID/GID
   constants), `crates/tx-scripts/src/process/exec/script.rs`
   (Phase 1 + Phase 3.5 wire). 8 tests including the pre-
   PoNR ordering smoke. Estimated ~300 LOC.
9. **S — Part 6 `AuxvFacts` extension.** Touches
   `crates/tx-scripts/src/process/exec/stack.rs`. 5 tests.
   Estimated ~150 LOC.
10. **M — Part 8 end-to-end smoke (2 host tests + 1 sibling
    fixture).** Touches
    `crates/tx-kernel/src/init/tests.rs` and
    `crates/tx-kernel/src/init/init_setuid_fixture.rs` (new).
    Estimated ~500 LOC including the fixture bytes + assert
    harness.

Total: 10 PRs, ~2700 LOC. Comparable to the
fork/clone/wait4 slice (8 PRs / ~2000 LOC) but slightly
broader because the syscall arm count is higher (16 new
arms vs 8 in fork/clone/wait4).

## Open questions

**All seven `Recommend` defaults confirmed by user 2026-05-06.**
The defaults below are the operative decisions; the discussion is
preserved for reviewer context.

1. **NR_SETREUID and NR_SETREGID exact RV64 generic ABI
   numbers.** The research note flagged uncertainty:
   "RV64 generic uses 145 = setregid, with no dedicated
   setreuid (consolidated under setresuid for glibc/musl)."
   But Linux's `asm-generic/unistd.h` actually defines
   `__NR_setregid = 143` and `__NR_setreuid = 145` — they
   *are* present in the generic ABI. The plan uses 143/145
   above; verify against
   `include/uapi/asm-generic/unistd.h` head before commit.
   If `NR_SETREUID = 145` collides with another in-tree
   constant, the slice has a bug that the type checker
   won't catch (numbers.rs is bare `pub const`s).
   **Recommend**: cargo xtask check that `numbers.rs`
   constants are unique; ship the grep-for-duplicates as
   part of Part 3's PR.
2. **Phase 3.5 vs post-Phase-4 placement for
   `step_apply_suid_for_exec`.** Cross-cutting risk #5
   names the trade-off. The plan defaults to Phase 3.5
   (between parse and build_aspace) because cred mutation
   is reversible (the helper could compose a "previous
   cred" return value if rollback became necessary), and
   parse failures shouldn't have already mutated cred.
   Counter-argument: post-Phase-4 placement is safer
   (only Phase 6's `replace_aspace` panicking can leave
   cred-mutated-but-aspace-unchanged, and `replace_aspace`
   is documented infallible). **Recommend**: Phase 3.5,
   per default. User input wanted only if a stricter
   ordering is preferred.
3. **`Credential::default()` semantic flip — single PR or
   two?** Cross-cutting risk #9. The plan ships it as a
   single PR (Part 1D) bundling the walker test sweep with
   the type extension. Splitting would require an interim
   PR where `Credential::default()` is "deprecated but
   still root-equivalent"; that's 2x the churn for zero
   benefit. **Recommend**: single PR per the plan. User
   input wanted only if a different staging is preferred.
4. **Optional `getresuid` / `getresgid` arms — ship or
   defer?** Cross-cutting risk #10. The plan ships them
   because round-trip testing of `setresuid` benefits;
   they're 10 LOC each. **Recommend**: ship per the plan.
5. **`AT_SECURE` rule shape — short-form or full Linux?**
   The plan uses Linux's short-form rule (`euid != prev_euid
   || egid != prev_egid`) per Part 5B. Real Linux's rule
   is more nuanced (file capabilities, nosuid mounts, etc.).
   The short form is correct for the slice's surface (no
   file caps, no nosuid). **Recommend**: short-form per
   the plan. If future capability/mount-flag slices add
   more sources of "exec elevated privilege", this rule
   gets revisited.
6. **`O_TRUNC` permission check — in scope?** Linux's
   `step_open` for `O_TRUNC` requires write permission on
   the file; the slice's `OpenFileFlags` has no `truncate`
   variant. Adding the variant + the perm check would land
   `O_TRUNC` ergonomics but needs a tmpfs `truncate` op.
   **Recommend**: defer; not in any LTP test on the
   immediate-unlock list.
7. **Where should `Capability::FOWNER` land?** Cross-cutting
   risk #6 names the absence. The plan adds it inline in
   Part 1A as a new `Capability::FOWNER = Self(3);` const.
   Linux's POSIX cap-FOWNER number is 3. **Recommend**:
   inline addition per the plan; trivial.
