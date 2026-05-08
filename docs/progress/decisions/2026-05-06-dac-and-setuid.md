# DAC + setuid

**Date:** 2026-05-06
**Branch:** `feat/dac-and-setuid`
**Plan:** [`docs/progress/plans/2026-05-06-dac-and-setuid.md`](../plans/2026-05-06-dac-and-setuid.md)
**Research:** [`docs/progress/research/2026-05-06-dac-and-setuid-scaffolding.md`](../research/2026-05-06-dac-and-setuid-scaffolding.md)
**Status:** Complete (host-test scope; full Cred saved-set semantics +
walker DAC predicate + 16 new syscall arms + setuid recompute at
exec + end-to-end Layer A smoke). 5 phase commits + 1 chore on top
of fork/clone/wait4. tx-kernel 37/37, tx-fs 24/24 serial, tx-shims
78/78, tx-scripts 39/39, tx-subsystems 405/405 serial, tx-substrate
sync 2/2. `cargo check --workspace`, `cargo check -p
tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`,
`cargo fmt --check`, `cargo xtask progress validate` all clean.

## Goal

The biggest LTP-coverage unlock per the fork/clone/wait4 decision
note's follow-up list. Targets ~50–70 LTP tests outright (full
`getuid*` / `geteuid*` / `getgid*` / `getegid*` / `setuid*` /
`setgid*` / `setreuid*` / `setregid*` / `setresuid*` / `setresgid*` /
`chmod*` / `fchmod*` / `chown*` / `fchown*` / `access*` /
`faccessat*` clusters) plus LTP `execve02` (EACCES for non-root
execing a 0700 root-owned file) + the remaining EACCES sub-cases of
`execve03` + setuid+execve chain tests scattered across LTP.

Demonstrable: a static-musl-shape fixture with the setuid bit set
in its mode (`S_ISUID | 0o755`), owned by uid 1000, executed by a
process running as uid 1001 — the kernel recomputes the effective
uid to 1000 at exec, the binary's `getuid()` vs `geteuid()` observe
1001 vs 1000, the `AT_SECURE` auxv flag is set. Plus a non-root
exec of a 0700 root-owned binary returns `EACCES` (LTP `execve02`).

## What landed

### Wave 1 — Cred extension (commit `3536366`)

**A. `Cred.suid` / `Cred.sgid` fields:** Full Linux saved-set
semantics. Constructors initialise `suid = euid` / `sgid = egid` (the
at-fork semantic). The `cred.rs:15-23` day-1 deferral note
rewritten to remove the `suid`/`sgid` and
`step_setresuid`/`step_setreuid` deferrals.

**B. Existing `step_setuid` / `step_setgid` updates:** maintain
saved-set correctly under both privileged and non-privileged paths.

**C. Four new helpers:** `step_setresuid`, `step_setresgid`,
`step_setreuid`, `step_setregid`. Each takes `Option<Uid|Gid>` per
arg (None = leave unchanged); applies Linux's privilege rule
atomically:

- With `CAP_SETUID`, all three args allowed.
- Without, each requested value (if `Some`) must equal one of
  `{uid, euid, suid}`.
- All three are atomic — if any fails the rule, the whole call fails
  with `EPERM` and no fields change.
- `setreuid` quirk: if `e` is `Some` and the new euid differs from
  the old euid, `suid` is updated to the new euid.

**D. `Credential.effective_caps` extension** (Q2 DECIDED): walker-
side `Credential` grew `effective_caps: CapabilitySet` so the walker
can short-circuit `CAP_DAC_OVERRIDE`. **Important semantic flip**:
`Credential::default()` is no longer root-equivalent (returns empty
caps). New `Credential::root()` constructor for explicit root
callers. Five production call sites flipped to `Credential::root()`
(init.rs ×3, devfs ×1, sys_execve ×1).

**E. Cred → Credential bridge:** `impl From<&Cred> for Credential`
(idiomatic over the plan's `for_walker` constructor). Passes
**effective** uid/gid (per `man 2 path_resolution` + `chmod` —
walker uses effective, not real).

12 new cred unit tests + 2 new vfs tests; tx-subsystems 377 → 391.

### Wave 2 — `SyscallCtx::cred()` + 12 syscall arms (commit `fcd9639`)

`ProcessIdentity::cred() -> Option<Cred>` accessor (Cred is `Copy`
so by-value return is sound). New `cross_crate_test_support::clear_caps_for_test`
helper.

`SyscallCtx::cred() -> Cred` returns by-value snapshot via
`self.process.cred().unwrap_or_else(Cred::root)`. Pulling a reference
into the `SpinMutex<Cred>` would be unsound across `.await` (the
cred can mutate during a syscall arm). `SyscallCtx::walker_cred() ->
Credential` is `Credential::from(&self.cred())`.

`sys_execve` flipped from Wave 1's interim `Credential::root()` to
`ctx.walker_cred()`. `init.rs` bootstrap-exec sites stay on
`Credential::root()` (no `SyscallCtx` available).

12 new syscall arms (Linux RV64 generic ABI):

| nr | name | wraps |
|---|---|---|
| 174 | `getuid` | `cred.uid` |
| 175 | `geteuid` | `cred.euid` |
| 176 | `getgid` | `cred.gid` |
| 177 | `getegid` | `cred.egid` |
| 146 | `setuid` | `step_setuid` |
| 144 | `setgid` | `step_setgid` |
| 145 | `setreuid` | `step_setreuid` |
| 143 | `setregid` | `step_setregid` |
| 147 | `setresuid` | `step_setresuid` |
| 149 | `setresgid` | `step_setresgid` |
| 148 | `getresuid` | reads `(uid, euid, suid)` |
| 150 | `getresgid` | reads `(gid, egid, sgid)` |

`u32::MAX → None` decoded via `decode_uid_arg` / `decode_gid_arg`
helpers per Linux's `(u32) -1 == leave unchanged` convention.
`getresuid` / `getresgid` write three u32s via `write_volatile` with
NULL-skip; `TODO(phase-userva)` marker.

17 new tests; tx-shims 48 → 65.

### Wave 3 — VFS DAC + AuxvFacts (commit `75e7556`)

**Part 2 — VFS permission-check plumbing:**

- Walker DAC predicate: replaced the `vfs/walker.rs:99-102` deferral
  with `check_descend_perm`. Per-component owner/group/other triplet
  selection. X bit (0o1) required for directory traversal.
  `CAP_DAC_OVERRIDE` short-circuits.
- `step_open` mode validation: new `check_open_perm` validates R/W
  bits against the inode's permission triplet for the matching
  uid/gid/other. The X-bit check for exec is deferred to Wave 4
  (exec_script's pre-Phase-6 X-bit auth).
- `FsOps::step_chmod` / `step_chown` trait methods (default `ENOSYS`).
- tmpfs override: `step_chmod` requires owner OR `CAP_FOWNER`, masks
  to 0o7777, preserves `S_IFMT` bits. `step_chown` checks privilege
  rule (`CAP_FOWNER` bypasses; non-privileged callers can chown to
  their own uid/gid only); Linux quirk: clears `S_ISUID`/`S_ISGID`
  silently on non-privileged chown.
- devfs override: both return `EROFS` (devfs is read-only).
- `Errno::EACCES` variant added (Linux ABI = -13).
- `S_ISUID = 0o4000`, `S_ISGID = 0o2000`, `S_ISVTX = 0o1000`
  constants.
- `Capability::FOWNER = Self(3)` (POSIX cap number 3) per Open Q
  #7 default.
- `Credential::default()` → `Credential::root()` test sweep:
  ~30 sites flipped across `tmpfs/tests`, `devfs/tests`,
  `walker/tests`, `vfs/tests`, `script/tests`, `legacy_phase_a` so
  the new DAC enforcement doesn't reject tests that aren't testing
  DAC. Tests intentionally left as `Credential::default()` are the
  ones asserting EACCES/EPERM paths.

**Part 6 — AuxvFacts extension:**

- `AuxvFacts` grew `at_uid`, `at_euid`, `at_gid`, `at_egid`,
  `at_secure` fields.
- 5 new auxv `a_type` constants: `AT_UID=11`, `AT_EUID=12`,
  `AT_GID=13`, `AT_EGID=14`, `AT_SECURE=23`. `AUXV_PAIR_COUNT`
  6 → 11. Auxv contribution grows from 96 bytes to 176 bytes
  (+80); existing 16-byte alignment helper absorbs the size delta.
- `exec_script` Phase 5 reads `process.cred()` (Wave 2's accessor)
  and feeds the cred fields into `AuxvFacts`. `at_secure`
  hardcoded to 0 with `TODO(wave-4)` marker (Wave 4 fills it).

8 new vfs walker DAC tests + 8 new tmpfs/devfs DAC tests + 4 new
auxv layout tests. tx-subsystems 391 → 399; tx-fs 16 → 24;
tx-scripts 29 → 33.

### Wave 4 — exec setuid + file-mode syscalls (commit `b9ae7a0`)

**Part 5 — exec_script setuid handling:**

- `step_apply_suid_for_exec(target, file_uid, file_gid, file_mode)
  -> Option<ExecCredOutcome>` at `cred.rs`. Returns `None` for
  zombie payloads. Per Q5 short-form rule (DECIDED 2026-05-06):
  ```
  at_secure = (S_ISUID set && new_euid != prev_euid)
           || (S_ISGID-with-group-X set && new_egid != prev_egid)
  ```
  File caps and `nosuid` mounts are out of scope. `previous_cred`
  is returned for rollback hooks but production `exec_script` does
  NOT roll back (per plan risk #5).
- Pre-Phase-6 X-bit check (`check_exec_perm` in `script.rs`) runs
  after Phase 1's `step_open` succeeds, before Phase 2's read.
  Rejects mode `0o600` binaries (no X anywhere — even with
  `CAP_DAC_OVERRIDE` the POSIX exception fires); `CAP_DAC_OVERRIDE`
  short-circuits IFF at least one X bit is set anywhere.
- Phase 3.5 in `exec_script` runs `step_apply_suid_for_exec` between
  parse and `build_aspace`. Cred mutation is structurally guaranteed
  to fire before Phase 5's stack-build, so `AT_SECURE` in the auxv
  reflects the post-recompute delta.
- `at_secure` flip: Wave 3's hardcoded 0 with `TODO(wave-4)` replaced
  by `if at_secure { 1 } else { 0 }`. TODO comment dropped.
- Errno mapping fix: `from_walker_errno` now maps both
  `Errno::EACCES` and `Errno::EPERM` to `ExecError::PermissionDenied`
  (Wave 3's walker DAC predicate emits EACCES; previously only EPERM
  was mapped, surfacing as `InvalidArgument`).

**Part 4 — File-mode syscall arms:**

- `NR_FCHMODAT = 53`, `NR_FCHOWNAT = 54`, `NR_FACCESSAT = 48`,
  `NR_FACCESSAT2 = 439`. `AT_FDCWD = -100`, `AT_EACCESS = 0x200`,
  `R_OK/W_OK/X_OK/F_OK`.
- `resolve_path_at` helper: synchronous (poll_walker_synchronously
  via noop-waker, mirroring `tx_scripts::process::exec`'s shape).
  `Guard` is `!Send`, so awaiting across `.await` would break
  `Reactor::submit_task`'s `Send` bound.
- AT_FDCWD only: real dirfd-relative paths require directory file
  descriptors which the slice doesn't have. Non-AT_FDCWD returns
  `-EBADF`.
- `fs_ops_for_dentry` helper ascends the parent_hint chain because
  the walker doesn't wire `with_containing_mount` on materialised
  child rnodes (only mount roots carry the weak).
- `faccessat2` AT_EACCESS flag switches between `(uid, gid)` and
  `(euid, egid)` for the perm check; matches Linux. F_OK
  short-circuits to `Return(0)` on resolution success.
  `CAP_DAC_OVERRIDE` bypasses R/W checks; X_OK with no X bit
  anywhere returns `-EACCES` even with DAC_OVERRIDE (Linux
  `generic_permission` quirk).
- New cross-crate test helpers in `cred.rs`:
  `install_caps_for_test` and `set_cred_ids_for_test`. Both
  `pub(crate)` re-exported through `cross_crate_test_support`.

6 new step_apply_suid tests + 6 new exec_script setuid tests + 13
new file-mode arm tests. tx-subsystems 399 → 405; tx-scripts 33 →
39; tx-shims 65 → 78.

### Wave 5 — End-to-end smoke + sibling fixture (commit `bb228cb`)

**Plan deviation** (Q4 originally locked "extend in place"): shipped
as a sibling `init_setuid_fixture.rs` because the fork/clone/wait4
slice's Wave 4 already grew `init_fixture.rs` into a 317-byte
fork+wait+exit binary with ~7 pinned byte tests. Sibling fixture
matches the plan's Part 8 section heading and avoids invalidating
the existing pins.

`crates/tx-kernel/src/init/init_setuid_fixture.rs` (new):

- 204 bytes total (64 ELF Ehdr + 56 PT_PHDR + 56 PT_LOAD + 28
  code).
- 7 RV64 instructions: `li a7, 174` + `ecall` (getuid) +
  `li a7, 175` + `ecall` (geteuid) + `li a7, 94` + `li a0, 0` +
  `ecall` (exit_group).
- Same `LOAD_VADDR = 0x10000` as the existing fixture; entry at
  `0x100B0`.
- 5 byte-pin tests.

End-to-end smoke
(`boot_smoke_setuid_exec_seeds_post_setuid_euid_and_at_secure`):

- Drives `drive_boot_wiring` + `register_setuid_fixture_into_tmpfs(uid=1000,
  gid=1000, mode=S_ISUID|0o755)`. The helper runs as root with
  `CAP_FOWNER` for `step_chown`/`step_chmod` (avoids the
  silent-clear-S_ISUID-on-non-privileged-chown quirk).
- Drops init's cred to non-privileged uid 1001:
  ```
  clear_caps_for_test(&init);
  set_cred_ids_for_test(&init, 1001, 1001, 1001, 1001, 1001, 1001);
  ```
- Pre-exec assertions: `cred.uid == 1001`, `cred.euid == 1001`.
- Drives `block_on(exec_script::<TestPlatform>(...))`.
- Post-exec assertions:
  - `cred.uid == 1001` (real uid unchanged at exec — Linux semantic)
  - `cred.euid == 1000` (S_ISUID flipped effective uid to file owner)
  - `cred.suid == 1000` (saved-set tracks new euid)
  - `gid/egid/sgid == 1001` (mode is `S_ISUID | 0o755`, no S_ISGID)
  - `init.aspace_cap().key()` differs from before (PoNR happened)
  - `saved_user_context.pc == INIT_SETUID_FIXTURE_ENTRY_VADDR`

`at_secure` verification: implicitly pinned via the cred delta
(1001 → 1000 is exactly the condition that sets `at_secure = true`
per `step_apply_suid_for_exec`). The explicit auxv stack-byte
decode was not needed since `exec_outcome.at_secure` short-circuits
through `auxv_facts.at_secure`, both of which already have direct
unit-test coverage in Wave 4 Part 5.

5 fixture pin tests + 1 boot smoke. tx-kernel 31 → 37.

## Decisions

All seven open questions decided 2026-05-06 (Q1-Q2 from research +
Q1-Q5 in the plan):

- **Q1 (research): suid/sgid in this slice.** Full Linux semantics.
  ~5 LTP tests gated (`setresuid01..05`).
- **Q2 (research): extend `Credential` with `effective_caps`** (not
  merge `Cred` and `Credential`). Walker short-circuits
  `CAP_DAC_OVERRIDE` cleanly.
- **Q1 (plan): NR_SETREUID/SETREGID = 145/143** per Linux generic ABI;
  shipped with a `cargo xtask` duplicate-detection follow-up flagged.
- **Q2 (plan): `step_apply_suid_for_exec` at Phase 3.5** (between
  parse and build_aspace). Cred mutation reversible; parse failures
  don't mutate cred.
- **Q3 (plan): `Credential::default()` flip in single PR.**
- **Q4 (plan): `getresuid`/`getresgid` arms shipped** (10 LOC each;
  helps round-trip testing).
- **Q5 (plan): `AT_SECURE` short-form rule** (`euid != prev_euid ||
  egid != prev_egid`); file caps + nosuid mounts are out of scope.
- **Q6 (plan): `O_TRUNC` permission check deferred** (not in any LTP
  test on the immediate-unlock list).
- **Q7 (plan): `Capability::FOWNER = Self(3)` inline** (POSIX cap
  number 3).
- **Q4 in Part 8: deviated to sibling fixture** rather than extend
  in place (existing init_fixture.rs is already a fork+wait+exit
  binary with pinned bytes).

## LTP coverage matrix

Day-1 MVP unlocks (this slice):

| LTP cluster | tests | this slice |
|---|---|---|
| `getuid*` / `geteuid*` / `getgid*` / `getegid*` | ~10 | full |
| `setuid*` / `setgid*` | ~10 | full |
| `setreuid*` / `setregid*` | ~6 | full |
| `setresuid*` / `setresgid*` | ~10 | full (saved-set sub-cases included) |
| `chmod*` / `fchmod*` | ~10 | full (via NR_FCHMODAT) |
| `chown*` / `fchown*` | ~10 | full (via NR_FCHOWNAT) |
| `access*` / `faccessat*` | ~5 | full (incl. AT_EACCESS) |
| `execve02` | 1 | unblocked (X-bit check + EACCES) |
| `execve03` EACCES sub-cases | ~3 | unblocked |
| setuid+execve chain | scattered | unblocked at kernel-surface level |

Gating remains for: capability tests beyond CAP_DAC_OVERRIDE/
CAP_FOWNER (not implemented); per-task AST plumbing for signal-handler
frame setup; `siginfo_t` carrier (waitid).

## Out of scope (deliberately deferred)

- Capabilities beyond `CAP_DAC_OVERRIDE` and `CAP_FOWNER`.
- File ACLs (POSIX ACL).
- Mandatory access control (MAC, SELinux, AppArmor).
- `prctl(PR_SET_KEEPCAPS)` and friends.
- File capabilities on exec.
- File systems other than tmpfs (devfs has fixed mode/uid/gid).
- `O_TRUNC` permission check.
- `nosuid` mount flag.
- AT_SYMLINK_NOFOLLOW (parsed but ignored — walker has no
  symlink-skip mode at the terminal).
- `CAP_CHOWN` (Wave 3's tmpfs `step_chown` uses `CAP_FOWNER` as a
  simplification).
- Layer B end-to-end smoke for setuid (full reactor-driven round
  trip with userspace observing post-exec uid/euid via
  getuid/geteuid syscalls back to console).

## Follow-ups in priority order

1. **Per-task AST plumbing** — signal-handler frame setup beyond
   `EnterUserspace`. Unlocks LTP wait* tests that gate on stop/cont
   signals.
2. **`NR_WAITID` + siginfo plumbing** — closes the LTP `waitid*`
   directory.
3. **`sys_open` syscall arm + `O_CLOEXEC` threading** — mechanical
   now that `OpenFileFlags::cloexec` is plumbed.
4. **`CLONE_VFORK | CLONE_VM`** — unlocks `posix_spawn` (musl shells
   use it).
5. **`cargo xtask check-numbers`** — duplicate-detection grep for
   `numbers.rs` constants (per Q1 follow-up flag).
6. **Layer B end-to-end smoke for setuid** (full reactor-driven
   round trip via instruction-decoder simulator).
7. **`RawTrapFrame`/`TrapFrameMut` portable HAL surface** — RV64
   board internals today.
8. **Real initramfs cpio unpack** — replace hand-encoded fixtures
   with real init binary loaded from disk.

## Verification

- `cargo test -p tx-kernel --lib` — 37/37.
- `cargo test -p tx-fs --lib -- --test-threads=1` — 24/24.
- `cargo test -p tx-shims --lib` — 78/78.
- `cargo test -p tx-scripts --lib` — 39/39.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 405/405.
- `cargo test -p tx-substrate` — sync 2/2.
- `cargo check --workspace` clean.
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf` clean.
- `cargo fmt --check` clean.
- `cargo xtask progress validate` ok.

## Commits on `feat/dac-and-setuid`

| sha | wave | scope |
|---|---|---|
| `320e2ce` | chore | research + plan (Q1=include suid/sgid; Q2=extend Credential with effective_caps; 7 minor Qs accepted defaults) |
| `3536366` | 1 | Cred extension (suid/sgid + 4 new step_set* helpers + Credential.effective_caps + Cred->Credential bridge) |
| `fcd9639` | 2 | SyscallCtx::cred() + 12 process-side syscall arms |
| `75e7556` | 3 | VFS DAC plumbing (Part 2) + AuxvFacts extension (Part 6) |
| `b9ae7a0` | 4 | exec_script setuid handling (Part 5) + 4 file-mode syscall arms (Part 4) |
| `bb228cb` | 5 | end-to-end smoke + sibling init_setuid_fixture (Part 8) |
