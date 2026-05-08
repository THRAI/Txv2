---
date: 2026-05-06
topic: "DAC + setuid scaffolding (scoping research for the post-fork-clone-wait4 slice)"
status: complete
prior:
  - docs/progress/decisions/2026-05-06-fork-clone-wait4.md
  - docs/progress/decisions/2026-05-06-elf-loader-and-execve.md
  - docs/progress/research/2026-05-06-musl-ltp-execve-coverage.md
---

# DAC + setuid scaffolding research

This note scopes the slice that lands DAC (POSIX rwx mode-bit
checks) + the setuid/setgid family on top of the
trio + pre-ELF + ELF-loader + fork-clone-wait4 stack. The
fork-clone-wait4 decision note's follow-up list ranks this #2
(after CSPRNG, which is a one-line replacement). LTP unlock
target: `execve02` + the EACCES sub-case of `execve03` + the LTP
permissions cluster (`setuid*`, `setgid*`, `setreuid*`,
`setregid*`, `setresuid*`, `setresgid*`, `getuid*`, `geteuid*`,
`getgid*`, `getegid*`, `chmod*`, `chown*`, `access*`,
`faccessat*`).

## Spec summary (PROCESS_v1, VFS_CHECKS_V2.1, EXEC_v1)

The kernel-side credential model lives in the cred service per
`txdoc:PROCESS-CREDENTIAL-SERVICE-DRAFT-1` and the
`docs/design/02_execution/cred_service_v_1_draft (2).md`
companion. A process owns a `Cred { uid, euid, gid, egid,
effective_caps, permitted_caps }` snapshot on `ProcessPayload`
mutated only via `cred::step_setuid` / `cred::step_setgid`
helpers. POSIX privilege rule: a caller with euid 0 OR
`CAP_SETUID` may set arbitrary IDs across all three slots
(real / effective / saved); a non-privileged caller may only
swap effective among `(uid, euid)`. The day-1 `Cred` shape (cred
module preamble) explicitly elides `suid`/`sgid` saved-set IDs
with a documented "non-privileged setuid only swaps among
(uid, euid)" simplification; PROCESS_v1 §11.1 defers
`setresuid`-style saved-set IDs and the supplementary group
list to "Phase 2 cred spec".

The DAC contract sits in `VFS_CHECKS_V2.1`
(`txdoc:VFS-CHECKS-TRANSITION-RULES-1` rule #5: "Permission
denied → `Error(TraverseDenied)`"; rule #3 names "traverse
permission check" as part of the named-component step). Each
`InodeMeta` carries `mode: u16` (POSIX rwxrwxrwx + S_IFMT bits)
+ `uid: u32` + `gid: u32` (`vfs/structure.rs:110-121`); the
walker's `kernel_step` is supposed to apply the standard
DAC rule (owner → owner triplet; group-membership → group
triplet; else → other triplet) plus `CAP_DAC_OVERRIDE` /
`CAP_DAC_READ_SEARCH` short-circuits per Linux. Component-by-
component: directory traversal needs the **X** bit on every
intermediate; the terminal needs the bit appropriate to the
op (R for read, W for write, X for exec).

Setuid-on-exec semantics live in EXEC_v1 §3.3
(`txdoc:EXEC-3-3-CRED-AUTHORIZES-EXECUTE-COMPUTES-NEW-CREDENTIALS`):
`cred::checks::require_executable(rnode_w, exec_mount_w,
caller_cred) -> Result<ExecAuthWitness, Errno>` (X mode-bit +
ownership; EACCES on no-execute, EPERM on capability-denial),
followed by `cred::execution::compute_exec_credentials(auth_w,
suid_w, caller_cred) -> NewCredential` (v1 returns
`caller_cred` unchanged; "Phase 2 applies setuid/setgid file
mode bits subject to SuidWitness; applies file capabilities;
computes the post-exec credential"). Linux's
`execve(2)` man page pins the rule: when S_ISUID is set on the
program file, `euid := file_owner_uid`; the *new* effective
UID is then copied to saved-set UID; real UID stays. Same
for S_ISGID. The `AT_SECURE` auxv entry is set to 1 whenever
the exec changed any UID/GID in a way that elevated privilege
(`euid != ruid` post-exec is the simplest form); musl reads it
to enter "secure mode" (see musl-needs section).

## Existing scaffolding

A surprising amount has already landed under the cred module
shape — the slice extends rather than greenfields.

- **`Cred` type**: `crates/tx-subsystems/src/cred.rs:135-143`. Fields:
  `uid: Uid`, `euid: Uid`, `gid: Gid`, `egid: Gid`,
  `effective_caps: CapabilitySet`, `permitted_caps:
  CapabilitySet`. **No `suid`/`sgid` saved-set IDs**; comment
  at `cred.rs:21-23` calls this out as the day-1
  simplification. Stored on `ProcessPayload.cred:
  SpinMutex<Cred>` (`process/structure.rs:541`).
- **`Capability` set**: `cred.rs:71-127`. Already enumerates
  `CHOWN`, `DAC_OVERRIDE`, `KILL`, `SETGID`, `SETUID`,
  `NET_ADMIN`, `SYS_ADMIN`. `CapabilitySet` is a `u64`
  bitset with `contains` / `add` / `remove`.
- **`step_setuid` / `step_setgid` helpers**: `cred.rs:184-211`
  and `cred.rs:215-240`. Already enforce the day-1 rule
  ("privileged: arbitrary; non-privileged: swap among
  `(uid, euid)`"). Output is `CredChange { Replaced { prev,
  new }, Zombie, PermissionDenied }`. **No syscall arms
  consume them today** — the only current call sites are the
  cred test module (`cred/tests.rs`).
- **`step_setresuid`, `step_setreuid`, `step_seteuid`,
  `step_capset`, `step_setfsuid`**: explicitly listed at
  `cred.rs:26-28` as "land when the syscall script driver
  consumes them". Not present in tree.
- **`Cred::root()`** (`cred.rs:146-156`) — returns a fully
  capable root credential. Used by the cred tests; not yet
  by the bootstrap path (which still uses `Credential::default
  ()` — see below).
- **`Credential` (separate type)**: `vfs/structure.rs:62-66`.
  This is the *VFS-side* cred shape — `pub struct Credential
  { pub uid: u32, pub gid: u32 }`. Note the impedance
  mismatch: the cred service's `Cred` carries 4 IDs +
  capabilities; the VFS's `Credential` carries 2. The walker
  takes `&Credential`; `step_setuid` etc. mutate `Cred`. The
  slice has to bridge these (or unify).
- **VFS permission-check plumbing**: deferred per pre-ELF
  Phase 4. `crates/tx-subsystems/src/vfs/walker.rs:55-59` (the
  "Permissions" module-level rustdoc) reads:
  > `Credential` is threaded through but the slice defers
  > mode-bit checking; matches the existing
  > `bind_init_cwd_and_root` "always allow for init" surface.
  The deferral lands at `walker.rs:99-102` inside
  `step_walk`:
  ```rust
  // TODO(phase-vfs-perms): implement DAC mode check via cred
  // and inode meta. The slice always allows; the parameter
  // stays in the signature so the future check has a place to
  // land.
  let _ = cred;
  ```
  The walker returns `Errno::EACCES` from no other site today
  — `WalkCause::TraverseDenied` exists in the spec
  (`VFS_CHECKS_V2.1.md:281-290`) but has no production
  emitter.
- **`InodeMeta` uid/gid storage**: present.
  `vfs/structure.rs:110-121` already carries `mode: u16`,
  `uid: u32`, `gid: u32`, plus `nlinks`, `blocks`, `flags`.
  `InodeMeta::new(kind, mode)` defaults uid/gid to 0
  (`structure.rs:134-135`).
- **Tmpfs uid/gid storage**: present and **already wired to
  `cred`**. `crates/tx-fs/src/tmpfs.rs:283-286` (file
  create), `tmpfs.rs:437-439` (dir create), `tmpfs.rs:527-
  529` (symlink create) all set `meta.uid = cred.uid; meta.gid
  = cred.gid` from the `Credential` argument the FsOps trait
  passes through. **No new tmpfs storage work is needed for
  the slice.** Devfs's `create_inode` returns `ENOSYS` (no
  user-created devfs entries yet); the bootstrap path uses
  `register_console_inode` shapes that hard-code device-mode
  metadata.
- **`exec_script`'s open-binary path uses `Credential::default
  ()`**: at the orchestrator entry, `process/exec/script.rs:
  226` declares the `cred: &Credential` parameter — the caller
  is what passes `Credential::default()`. `init.rs:651` and
  `init.rs:741` (`drive_bootstrap_exec`) explicitly construct
  `Credential::default()` for the bootstrap exec, which is
  fine. `linux_syscall/mod.rs:872-878` (the NR_EXECVE arm) is
  the load-bearing site:
  ```rust
  // Default-credential path — Phase 6 reads cred from the
  // syscall context once a `cred` field is plumbed onto
  // `SyscallCtx`.
  // For now `Credential::default()` matches the bootstrap
  // process (uid=0, gid=0).
  // TODO(phase-cred-on-ctx): consume cred from ctx once the
  // field lands.
  let cred = Credential::default();
  ```
  The slice replaces this with `ctx.cred()` (after adding
  the accessor; see "Gaps").
- **`SyscallCtx`**: `linux_syscall/mod.rs:201-208` — has
  `process`, `thread`, `aspace`, plus a `_lifetime`
  PhantomData with the documented preamble:
  > Sliced lifetime so future fields (signal-mask snapshot,
  > cred snapshot) can be added without ripping every call
  > site.
  No `cred()` accessor today; the slice adds one (probably
  proxying through `ctx.process.payload_cap()?.cred()` ->
  `Cred` and projecting to a VFS `Credential` for the walker).
- **`AuxvFacts`**: `crates/tx-scripts/src/process/exec/stack.rs:
  81-91`. Carries `at_phdr`, `at_phent`, `at_phnum`,
  `at_pagesz`. **No `at_uid`, `at_euid`, `at_gid`, `at_egid`,
  `at_secure`** today. The auxv table written by
  `build_initial_user_stack` (`stack.rs:146`) currently emits
  the 6-entry minimum (`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`,
  `AT_PAGESZ`, `AT_RANDOM`, `AT_NULL`) per the ELF loader
  decision Q#1 / musl-ltp coverage research.
- **`ProcessPayload.cred` mutability**: already
  `SpinMutex<Cred>` (`structure.rs:541`); a setuid arm just
  takes the mutex, mutates, drops. No `AtomicSlot` flip
  needed (unlike the ELF loader's aspace flip).
- **NR_GETUID / NR_GETEUID / NR_GETGID / NR_GETEGID / NR_SETUID
  / NR_SETGID / NR_SETREUID / NR_SETREGID / NR_SETRESUID /
  NR_SETRESGID**: **all absent** from
  `crates/tx-shims/src/linux_syscall/numbers.rs` (verified —
  the file lists 25, 56, 60, 63, 64, 81, 96, 99, 154, 155,
  156, 157, 172, 173, 214, 220, 221, 260, plus the ENOSYS-
  default catch-all). RV64 generic ABI numbers are 174
  (getuid), 175 (geteuid), 176 (getgid), 177 (getegid), 144
  (setgid), 146 (setuid), 147 (setresuid), 148 (getresuid),
  149 (setresgid), 150 (getresgid), 151 (setfsuid), 152
  (setfsgid). `setreuid` / `setregid` are `prlimit`-clustered
  Linux-specific; RV64 numbers are 145 (setreuid? — verify
  against `asm-generic/unistd.h` during planning) — actually
  RV64 generic uses 145 = `setregid`, 144 = `setgid`, with no
  dedicated `setreuid` (consolidated under setresuid for
  glibc/musl). Plan must verify exact numbers from
  `asm-generic/unistd.h` head; LTP's `setreuid01.c` uses the
  glibc wrapper which on RV64 maps to setresuid.

## Gaps the slice has to build

- **`SyscallCtx::cred()` accessor.** Read
  `ctx.process.payload_cap()?.cred()`; project to VFS-side
  `Credential { uid: cred.euid.raw(), gid: cred.egid.raw() }`
  (DAC checks consult **euid/egid**, not uid/gid — POSIX rule).
  Missing today; multiple syscall arms downstream need this.
- **NR_GETUID, NR_GETEUID, NR_GETGID, NR_GETEGID arms.**
  Read-only; trivial. Each pulls
  `ctx.process.payload_cap()?.cred()` and returns the
  appropriate field as `Return(uid as i64)`. Five-line
  arms.
- **NR_SETUID, NR_SETGID arms.** Wrap `cred::step_setuid` /
  `cred::step_setgid`. Map `CredChange::Replaced → Return(0)`;
  `Zombie → Error(-ESRCH)`; `PermissionDenied → Error(-EPERM)`.
- **NR_SETREUID, NR_SETREGID arms.** No `step_setreuid`
  helper exists today; explicitly listed at `cred.rs:26-28`
  as deferred. The slice **adds** it. Linux semantics: set
  ruid + euid with -1 sentinel for "leave alone"; saved-set
  uid bumps to euid if euid changes. **Cred has no `suid`
  field today** — this is the load-bearing decision (see
  Open Questions).
- **NR_SETRESUID, NR_SETRESGID arms.** Same; explicitly listed
  as deferred. Sets all three explicitly; -1 means "leave
  alone". **Requires `suid`/`sgid` saved-set fields on `Cred`.**
- **NR_GETRESUID, NR_GETRESGID arms.** Trivial reads of all
  three; same `suid`/`sgid` storage dependency.
- **VFS permission check at every walker step.** Replace the
  `let _ = cred;` deferral at `vfs/walker.rs:99-102`. The
  natural shape: a `check_perm(meta: &InodeMeta, cred:
  &Credential, want: PermBit) -> bool` predicate inside
  `walker.rs`, called before each component descent (X bit
  for traverse), then again at the terminal in
  `step_open` (R / W / X based on `OpenFileFlags`).
  `CAP_DAC_OVERRIDE` / `CAP_DAC_READ_SEARCH` short-circuit
  the `is_root || cap_dac_override` cred via the existing
  `Cred::is_privileged_for(Capability::DAC_OVERRIDE)`. Errno
  on deny: `EACCES` (terminal, R/W/X) and `EACCES` for
  intermediate-X-deny per POSIX. The walker emits
  `Errno::EACCES` already from no site today, so adding the
  emit is mechanical.
- **`exec_script` open-binary path uses `ctx.cred()`.** Replace
  the `Credential::default()` at `linux_syscall/mod.rs:878`.
  Note: bootstrap exec at `init.rs:741` keeps
  `Credential::default()` (root-owned init) — only the
  syscall arm needs the change.
- **Setuid bit handling at exec phase 7.** Today the
  exec_script (`process/exec/script.rs:392-453`) executes the
  Phase 6/7 commit block with no cred recompute. The slice
  adds a Phase 7 step `step_apply_suid_for_exec(process,
  inode_meta)` that:
  1. Reads `inode_meta.mode & S_ISUID` and `S_ISGID`.
  2. If set, calls a new `cred::step_apply_suid_exec(process,
     file_uid, file_gid, suid_set, sgid_set) -> NewCredOutcome`
     that mutates payload.cred under the lock per Linux
     rules: `euid := file_uid` if `suid_set`; `egid :=
     file_gid` if `sgid_set`; `suid := euid`, `sgid := egid`
     post-mutation (saved-set update). Returns the boolean
     `at_secure` flag for the new auxv field.
  3. The `inode_meta` snapshot has to be threaded from
     Phase 1 (`step_open`) to Phase 7. Today the openfile's
     rnode carries it via `openfile.rnode().meta()` —
     accessible without the walker remembering it. Just add
     a local before Phase 6 and consult it in Phase 7. **PRE-
     PoNR**: the cred decision and the file_uid/file_gid
     snapshot have to happen **before** Phase 6's irreversible
     replace_aspace store, otherwise the Phase 7
     "infallible" rule breaks.
- **`AuxvFacts.at_secure`, `at_uid`, `at_euid`, `at_gid`,
  `at_egid`.** Add 5 fields to `AuxvFacts` (`stack.rs:75-91`).
  The setuid-exec path computes `at_secure = (new_euid !=
  pre_exec_uid) || (new_egid != pre_exec_gid)` (Linux's
  short-form rule); the static path leaves it 0. `at_uid` /
  `at_euid` / `at_gid` / `at_egid` carry the **post-exec**
  cred IDs. `build_initial_user_stack` extends from 6 to 11
  auxv entries. **Required for setuid-binary correctness**
  per musl needs section.
- **VFS's `Credential` ↔ cred service's `Cred` bridge.**
  Today `Credential` is `{ uid, gid }`; cred service's `Cred`
  is `{ uid, euid, gid, egid, caps, ... }`. Two options:
  (a) extend `Credential` to carry `effective_caps` so the
  walker can short-circuit on CAP_DAC_OVERRIDE without
  pulling the whole Cred lock; (b) walker takes `&Cred`
  directly and the VFS-side `Credential` retires. Option (a)
  is the lower-disruption change. Either way the slice has
  to bridge. (See Open Questions.)
- **NR_FACCESSAT / NR_ACCESS arms.** Mechanical: walk the
  path, then run the same `check_perm` predicate against the
  caller's cred + the resolved inode meta + the requested
  bits. Errno: `EACCES` (denied), `ENOENT` (missing),
  `EROFS` (W on read-only mount — mount RO bit isn't tracked
  today; defer or stub).
- **NR_CHMOD / NR_FCHMOD / NR_FCHMODAT arms.** Mechanical:
  walk path → terminal RNode → `inode_meta.mode := new_mode
  & 07777 | (existing & S_IFMT)`. Permission rule: caller is
  owner OR has `CAP_FOWNER`. Mutator on `InodeMeta` doesn't
  exist today (`InodeMeta` is `Copy` and stored inside
  `RNode`/tmpfs's `TmpfsInode`); the slice adds an FsOps trait
  method `step_chmod(fs_object_id, new_mode, &cred, &guard)`
  with a tmpfs implementation that mutates the meta in
  `inodes: BTreeMap<FsObjectId, TmpfsInode>` under the
  existing `SpinMutex`.
- **NR_CHOWN / NR_FCHOWN / NR_FCHOWNAT / NR_LCHOWN arms.**
  Same shape; needs `step_chown` FsOps method. Permission:
  only `CAP_CHOWN` (root) can change ownership; ownership-
  preserving `chown(uid=-1, gid=-1)` no-op for non-root.
- **`ProcessPayload.cred` write-side discipline.** Already
  `SpinMutex<Cred>` so no AtomicSlot flip required. The
  slice uses the existing lock; the only concern is making
  sure the setuid-exec path mutates **before** Phase 6's
  PoNR (so failure is reversible and Phase 7 stays
  infallible).

## musl needs

Verified against `git.musl-libc.org/cgit/musl/tree/src/env/__libc_start_main.c`
lines 30-43:

```c
if (aux[AT_UID]==aux[AT_EUID] && aux[AT_GID]==aux[AT_EGID]
    && !aux[AT_SECURE]) return;
```

Musl's `__init_security` (the function the above lives in) reads
all five auxv security entries directly from the on-stack
`size_t aux[AUX_CNT]` buffer — **no syscall fallback**. If the
auxv lacks them they default to 0 (musl's stack-allocated
`size_t aux[38] = {0}` zero-init). The all-zero case
trivially satisfies the early-return condition (uid==euid==0,
gid==egid==0, secure==0) and musl does **nothing** for
"secure mode" — i.e., the trio's current 6-entry auxv
incidentally produces the "harmless root running normally"
shape and musl's secure-mode code never fires.

| musl field          | source          | currently in trio | slice wires |
|---------------------|-----------------|-------------------|-------------|
| AT_UID              | auxv            | absent (defaults 0) | yes (post-exec ruid) |
| AT_EUID             | auxv            | absent (defaults 0) | yes (post-exec euid) |
| AT_GID              | auxv            | absent (defaults 0) | yes (post-exec rgid) |
| AT_EGID             | auxv            | absent (defaults 0) | yes (post-exec egid) |
| AT_SECURE           | auxv            | absent (defaults 0) | yes (1 iff exec elevated privilege) |

Secure-mode action (musl): poll fds 0/1/2 with a no-op syscall
to detect invalid descriptors and re-`open("/dev/null")` if
needed; set `libc.secure = 1`. Musl does **not** scrub env vars
itself — that's the dynamic-linker (`ldso`)'s job for dynamic
binaries; static-musl's secure-mode is mostly the fd-validation
step.

For `getuid` / `geteuid` etc. **as syscalls**: musl's
`getuid()` libc wrapper (`src/unistd/getuid.c`) is a thin
syscall stub. Static binaries don't call it at startup —
`__libc_start_main` reads from auxv only. But a binary that
calls `getuid()` later (e.g., LTP `getuid01.c` literally
calls the syscall) needs the kernel-side arm to exist; the
trio's current dispatcher returns `-ENOSYS` for unknown nrs,
which fails the test.

**Bottom line for the slice's musl correctness**: even before
setuid binaries work end-to-end, plumbing AT_UID / AT_EUID /
AT_GID / AT_EGID into the auxv (post-exec values) is
load-bearing for any future `setuid()` syscall to behave
correctly across an exec, since musl caches the auxv values
in `libc.{uid,euid,gid,egid}`-equivalents at startup. In
practice for v1: hardcoding them to 0 (matching the bootstrap
init) is fine until LTP `setuid01` runs — at which point
`AT_*` and the syscall arms have to land together.

## LTP coverage matrix

| dir | est. tests | gates on (kernel surface) | day-1 slice MVP outcome |
|---|---|---|---|
| `getuid` | 1-2 | NR_GETUID arm | ✓ outright |
| `geteuid` | 1-2 | NR_GETEUID arm | ✓ outright |
| `getgid` | 1-2 | NR_GETGID arm | ✓ outright |
| `getegid` | 1-2 | NR_GETEGID arm | ✓ outright |
| `setuid` | 3 (01, 03, 04) | NR_SETUID + cred mutation + CAP_SETUID rule | ✓ outright (uses existing `step_setuid`) |
| `setgid` | ~4 | NR_SETGID + cred mutation | ✓ outright |
| `setreuid` | ~4 | NR_SETREUID + saved-set update | needs new `step_setreuid` + `suid` field on `Cred` |
| `setregid` | ~4 | NR_SETREGID + saved-set update | same |
| `setresuid` | 5 (01..05) | NR_SETRESUID + explicit saved-set | needs new `step_setresuid` + `suid`/`sgid` fields |
| `setresgid` | ~5 | NR_SETRESGID | same |
| `chmod` | 7 (01,03,05-09) | NR_CHMOD arm + FsOps::step_chmod + tmpfs mutator | ✓ outright (pure tmpfs path) |
| `fchmod` | ~3 | NR_FCHMOD arm | ✓ outright |
| `chown` | ~5 | NR_CHOWN arm + FsOps::step_chown + CAP_CHOWN | ✓ outright (root-init path; non-root fails with EPERM) |
| `fchown` | ~3 | NR_FCHOWN arm | ✓ outright |
| `access` | 4 (01..04) | NR_ACCESS arm + DAC predicate | ✓ outright |
| `faccessat` | ~3 | NR_FACCESSAT arm | ✓ outright |
| `execve02` | 1 | DAC X-bit + non-root caller (setuid-then-execve) | ✓ outright (after setuid + walker DAC checks) |
| `execve03` EACCES | 1 sub-case | DAC R/X-bit | ✓ outright (closes 5/6 of execve03; 6th is EFAULT, separate) |
| `chmod` setuid-bit subtests | within chmod cluster | needs S_ISUID / S_ISGID storage on inode (which exists; mode is u16) + chmod arm | ✓ as a side effect |
| LTP setuid+execve chain tests | scattered | post-exec cred carryover (already correct: `ProcessPayload` survives exec, only Frame is replaced) + setuid-bit at exec | ✓ once setuid-bit at exec lands |

Non-exhaustive headcount: 50–70 LTP tests directly target the
permissions cluster; another ~30 indirectly depend on
setuid-binary semantics for `execve*` chain coverage.

Tests **not** unlocked by this slice:

- `lchown01` etc. — needs symlink-aware path resolution
  (walker handles, but lchown is a separate syscall arm; this
  slice covers, not separately listed).
- `capset` / `capget` / `prctl(PR_CAP_*)` — full POSIX
  capability syscalls. `Cred` has the storage; the syscalls
  don't. Out of slice (see capabilities section).
- `fanotify` / `inotify` / ACL-cluster (`getxattr`,
  `setxattr`) — orthogonal subsystems.
- LTP `mount*` `nosuid` / `noexec` flag tests — needs mount-
  flag storage (`MountIdentity` doesn't track these today).
- LTP `setfsuid` / `setfsgid` — Linux-specific filesystem-uid;
  PROCESS_v1 §11.1 defers to "Phase 2 cred spec".

## Slice sizing for txKernel

### MVP — basic DAC + getuid/geteuid + EACCES at step_open

What lands:

- `SyscallCtx::cred()` accessor.
- `NR_GETUID`, `NR_GETEUID`, `NR_GETGID`, `NR_GETEGID` arms
  (4 trivial reads).
- DAC `check_perm` helper in `walker.rs`; integrate into
  `step_walk` (X bit per intermediate component) and
  `step_open` (R/W/X per `OpenFileFlags`).
- `Credential` extension to carry `effective_caps` (so
  walker can short-circuit on CAP_DAC_OVERRIDE) — or
  walker takes `&Cred`. (Open question.)
- `exec_script`'s open path uses `ctx.cred()` not
  `Credential::default()`.
- `WalkCause::TraverseDenied → Errno::EACCES` mapping in the
  walker's error projection.

Unlocks: LTP `getuid` / `geteuid` / `getgid` / `getegid`
tests; LTP `execve02` (after the setuid arm lands — see
beyond-MVP); LTP `execve03` 5/6 sub-cases (already pass for
ENAMETOOLONG / ENOENT / ENOTDIR / ENOEXEC; gains EACCES);
LTP `access` / `faccessat` (after their syscall arms).

### Beyond MVP — full setuid family + setuid-bit at exec

Adds:

- `Cred` extension with `suid: Uid`, `sgid: Gid` (saved-set).
- `cred::step_setresuid`, `step_setresgid`, `step_setreuid`,
  `step_setregid`, `step_seteuid` helpers (PROCESS_v1 §11.1
  "Phase 2 cred spec" lands here).
- `NR_SETUID`, `NR_SETGID`, `NR_SETREUID`, `NR_SETREGID`,
  `NR_SETRESUID`, `NR_SETRESGID`, `NR_GETRESUID`,
  `NR_GETRESGID` syscall arms.
- Setuid-bit handling in `exec_script` Phase 7
  (`step_apply_suid_for_exec`): consults `inode_meta.mode &
  (S_ISUID | S_ISGID)`; mutates `payload.cred` pre-PoNR.
- `AuxvFacts` extension: `at_uid`, `at_euid`, `at_gid`,
  `at_egid`, `at_secure` (5 new fields); `build_initial_user_stack`
  emits 11 auxv entries (was 6).

Unlocks: LTP `setuid` / `setgid` / `setreuid` / `setregid` /
`setresuid` / `setresgid` (~25 tests); the setuid-binary
half of `execve02` and `execve03` EACCES; the setuid+execve
chain tests scattered across LTP.

### Beyond MVP — `chmod` / `chown` family

Adds:

- `FsOps::step_chmod(fs_object_id, new_mode, &cred, &guard)
  -> StepOutcome<()>` and `step_chown(fs_object_id,
  new_uid, new_gid, &cred, &guard) -> StepOutcome<()>`
  trait methods.
- Tmpfs implementations mutating the meta inside
  `tmpfs.rs:95 inodes: BTreeMap<FsObjectId, TmpfsInode>`
  under the existing mutex.
- Devfs implementations: `EROFS` (devfs nodes are kernel-
  owned).
- `NR_CHMOD`, `NR_FCHMOD`, `NR_FCHMODAT`, `NR_CHOWN`,
  `NR_FCHOWN`, `NR_FCHOWNAT`, `NR_LCHOWN` syscall arms.
- `NR_ACCESS`, `NR_FACCESSAT`, `NR_FACCESSAT2` syscall arms
  (DAC predicate runs in-kernel; no fs mutation).

Unlocks: LTP `chmod` / `fchmod` / `chown` / `fchown` /
`access` / `faccessat` (~25 tests).

### Beyond that — capabilities

`capset(2)` / `capget(2)` / `prctl(PR_CAPBSET_*)` /
ambient capabilities / file capabilities (xattr-stored) live
in a future capability slice. The current `Cred` shape carries
`effective_caps` + `permitted_caps` but no `inheritable_caps`
/ `bounding_caps` / `ambient_caps`; the cred module's
preamble explicitly defers these (`cred.rs:24-28`). Out of
this slice.

## Implementation readiness verdict

**Ready to plan a phased slice.** The cred subsystem already
ships `Cred`, `Capability`, `step_setuid`, `step_setgid`, +
9 days of test coverage in `cred/tests.rs`. Tmpfs already
stores uid/gid from `cred` at `create_inode` time — the wiring
predates this research. The walker has a single deferred site
(`walker.rs:99-102`) that the slice replaces. `AuxvFacts`
extension is mechanical (5 new fields, 5 new auxv-table
emits). The setuid-bit-at-exec wiring is a single new helper
(`step_apply_suid_for_exec`) that respects the EXEC-PONR
boundary because it lives in the loader's pre-PoNR phase.

The biggest scope decisions are (1) whether to extend `Cred`
with saved-set IDs in this slice (gates `setresuid`-family
arms; LTP `setresuid01..05` need it) and (2) whether to
extend `Credential` to carry caps for the walker fast-path.

## Open questions for the planning session

- **Saved-set UID/GID handling: full Linux semantics this
  slice or simplified?** Full `(uid, euid, suid)` + `(gid,
  egid, sgid)` adds 8 bytes to `Cred` and the explicit
  `step_setresuid` helper. Simplified — keep day-1's "swap
  among (uid, euid)" — fails LTP `setresuid01..05` (5 tests)
  and the saved-set-uid sub-cases of LTP `setreuid` /
  `setregid`. Recommend: do it now. The cred module already
  flags this as deferred at `cred.rs:21-23`; closing it
  here keeps the slice complete relative to its LTP-coverage
  goal.
- **Capability model: any `CAP_*` enforcement in this slice or
  all deferred?** `Cred` already carries `effective_caps`;
  `Capability::DAC_OVERRIDE`, `CAP_FOWNER`, `CAP_SETUID`,
  `CAP_SETGID`, `CAP_CHOWN` are the load-bearing bits. The
  existing `Cred::is_privileged_for(cap)` shortcuts on
  `euid == 0`, so root-init "just works"; the deferred bit is
  whether `capset(2)` / `capget(2)` / file-caps land. Recommend:
  enforcement in this slice (already-shipped `is_privileged_for`
  is enough); `capset` / `capget` / file caps deferred to a
  capability slice.
- **`Credential` ↔ `Cred` unification or extension?** Walker
  takes `&Credential { uid, gid }`; cred service mutates `Cred
  { uid, euid, gid, egid, caps }`. Three options: (a)
  walker takes `&Cred` directly (full info, breaks the
  vfs-only abstraction); (b) extend `Credential` with
  `effective_caps: CapabilitySet` (walker fast-path retains
  the small projection); (c) leave the projection at 2 fields
  and walker pulls cred via a context handle for cap checks.
  Recommend (b): minimal disruption, walker stays VFS-local.
- **`AT_UID` / `AT_EUID` / `AT_GID` / `AT_EGID` / `AT_SECURE`
  in auxv — add now or defer?** Add now. Five fields in
  `AuxvFacts` + 5 emits in `build_initial_user_stack` is ~30
  lines. Without `AT_SECURE`, LTP setuid-binary tests
  silently regress (musl's secure-mode fd-validation never
  fires; OK for non-setuid, broken for `execve02` once
  setuid-bit handling at exec lands). Tightly coupled to the
  setuid-bit-at-exec helper.
- **Mount `nosuid` / `noexec` flags: in scope?** EXEC_v1 §3.2
  (`txdoc:EXEC-3-2-MOUNT-SUPPLIES-NOEXEC-AND-NOSUID-POLICY`)
  pins the spec; `MountIdentity` has no flag storage today.
  Recommend defer: add `MountFlags { nosuid, noexec, ro }` in
  a small follow-up; LTP's `mount*` flag tests aren't on the
  immediate post-DAC unlock path.
- **POSIX supplementary group list: in scope?** `Cred` doesn't
  carry one. Linux's `setgroups(2)` / `getgroups(2)` need it.
  LTP has `setgroups` / `getgroups` clusters (~6 tests). Recommend
  defer — the supplementary list is a `Vec<Gid>` allocation
  on `Cred` and adds a write-side discipline question
  (allocation under the SpinMutex); a dedicated mini-slice
  later. Group permission checks in the walker fall back to
  "egid == file.gid" only (matches the day-1 LTP unlock
  shape).
