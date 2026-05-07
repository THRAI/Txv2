# Roadmap to interactive shell in QEMU

**Status:** proposed (planning only).
**Date:** 2026-05-07
**Scope:** Enumerate the gap between today's `feat/fd-ops` HEAD and
typing into a shell prompt over QEMU stdio. Decompose into
slice-shaped work with clear success criteria, dependency edges,
and rough LOC estimates.

## What is already wired (good news)

Grounded by code inspection, not assumption:

- **Reactor loop drives userspace.** `CoreInit::run_userspace_reactor_loop`
  (`crates/tx-kernel/src/init.rs:1146-1217`) is invoked from the
  boot flow when `P::SUBSTRATE_BOOT_READY`. Each iteration runs a
  `step_hart_loop_at` step, idles on WFI when the loop reports
  idle, and breaks when init zombifies. Layer B is **not** a
  missing primitive — what's deferred is the **end-to-end QEMU
  smoke test** that boots kernel + shell + watches serial for a
  prompt.
- **VM has all primitives.** `crates/tx-subsystems/src/vm/execution.rs`
  exposes `try_mmap`, `try_munmap`, `try_mprotect`, `try_mremap`,
  `madvise`, `msync`, `mincore`, `fork_aspace`, `exec_aspace`. The
  syscall arms are missing, not the substrate.
- **TTY ioctl primitives exist.**
  `tx_subsystems::tty::execution::{step_ioctl_tcgets, step_ioctl_tcsets,
  step_ioctl_tiocgpgrp, step_ioctl_tiocspgrp, step_ioctl_tiocgwinsz,
  step_ioctl_tiocswinsz, step_ioctl_tiocsctty, step_ioctl_tiocnotty}`.
  No `NR_IOCTL = 29` arm dispatches them.
- **`step_chdir`** lives at `process/execution.rs:694`;
  `process.cwd()` accessor at `process/structure.rs:297`. No
  `NR_CHDIR` / `NR_FCHDIR` / `NR_GETCWD` syscall arms.
- **HAL `UserAccessIf::{copy_from_user, copy_to_user}`** exists at
  `tx-hal/src/lib.rs:914`. Default returns `EFAULT`; platforms
  override with arch-specific fixup-table assembly. Used today by
  `page_backed::user_buffer` for read/write IO. **Not wired into
  any other syscall arm.**
- **`TX_BUSYBOX` / `TX_MUSL_LIBC`** env vars are read by
  `xtask/src/{image,doctor}.rs`. The image builder accepts them;
  the kernel side has no `register_busybox_into_tmpfs` helper.
- **CSPRNG via HAL `EntropyIf`** lands in this PR (commit `f2d8a67`).
  `getrandom(2)` syscall arm is not yet wired.

## Lacking interfaces — clean inventory

Categorised by depth of gap. "Plumbing" means primitives exist,
just need a syscall arm. "Step missing" means a new step function
or substrate primitive must be written.

### A. Pure plumbing (~50–80 LOC each)

| Linux | NR (RV64 generic) | Primitive | Rough LOC |
|---|---|---|---|
| `mmap` | 222 | `vm::execution::try_mmap` | ~100 (flag decode + UserRange shape + EFAULT mapping) |
| `munmap` | 215 | `vm::execution::try_munmap` | ~50 |
| `mprotect` | 226 | `vm::execution::try_mprotect` | ~50 |
| `madvise` | 233 | `vm::execution::madvise` | ~50 |
| `mremap` | 216 | `vm::execution::try_mremap` | ~70 |
| `msync` | 227 | `vm::execution::msync` | ~50 |
| `ioctl` | 29 | `tty::execution::step_ioctl_*` family | ~150 (request decode + 8 ioctl arms + ENOTTY default) |
| `chdir` | 49 | `process::execution::step_chdir` | ~60 |
| `fchdir` | 50 | needs new helper (resolve fd → DEntry) | ~40 |
| `umask` | 166 | new field on `ProcessIdentity` | ~40 |
| `getrandom` | 278 | `tx_hal::EntropyIf::generate_bytes` | ~40 |
| `uname` | 160 | static `utsname` build | ~50 |
| `prlimit64` | 261 | static `rlimit` table | ~80 |
| `getrlimit` | (legacy) | aliases prlimit64 | ~20 |
| `setrlimit` | (legacy) | aliases prlimit64 | ~20 |
| `clock_gettime` | 113 | `tx_hal::TimeIf::read_ns` | ~50 |
| `gettimeofday` | 169 | derived from `read_ns` | ~30 |
| `times` | 153 | derived from `read_ns` | ~30 |
| `kill` | 129 | `signal::script_kill` (exists) | ~30 |
| `tkill` | 130 | thread-targeted kill — needs new step | ~50 |
| `tgkill` | 131 | tgid+tid kill — needs new step | ~50 |
| `rt_sigreturn` | 139 | `tx_hal::SignalFrameIf` (exists) | ~30 |
| `getpgrp` | 81 | already wired but returns ENOSYS — fix | ~10 |

**Plumbing totals: ~22 syscall arms, ~1100 LOC.**

### B. Step missing (~100–250 LOC each)

| Linux | NR | What's missing | Rough LOC |
|---|---|---|---|
| `getdents64` | 61 | per-fd readdir cursor on `OpenFile`; `linux_dirent64` marshalling; `FsOps::readdir` cursor surface (exists, needs threading) | ~200 |
| `fstat` | 80 | `FsOps::stat` step + `struct stat64` marshalling | ~150 |
| `newfstatat` | 79 | as above + walker entry | ~180 |
| `lstat`-shape | (via 79 with AT_SYMLINK_NOFOLLOW) | rides newfstatat | ~30 marginal |
| `statx` | 291 | richer `struct statx` — punt to a sibling slice; not on day-1 path | (deferred) |
| `getcwd` | 17 | path serialisation walking up the dentry tree | ~120 |
| `readlinkat` | 78 | `FsOps::readlink` step (exists for `Symlink` backing) + buffer copy | ~80 |
| `unlinkat` | 35 | `FsOps::unlink` / `rmdir` step (~exists, partial) | ~150 |
| `mkdirat` | 34 | `FsOps::mkdir` step (~exists, partial) | ~100 |
| `renameat2` | 276 | `FsOps::rename` step (cross-dir moves; new) | ~250 |
| `symlinkat` | 36 | `FsOps::symlink` step (new) | ~100 |
| `utimensat` | 88 | `InodeMeta` atime/mtime fields + step | ~80 |
| `linkat` | 37 | `FsOps::link` step (new; refcount on inode) | ~150 |
| `truncate` / `ftruncate` | 45 / 46 | `FsPageBacking::truncate` (exists; one is a path resolve) | ~80 |
| `pipe` (legacy) | absent on RV64 | musl never emits | (skip) |
| `dup2` (legacy) | absent on RV64 | musl never emits | (skip) |

**Step-missing totals: ~14 arms, ~1700 LOC.**

### C. Cross-cutting infrastructure

#### C.1 — User-VA sweep (~400 LOC churn, no new primitives)

Today 18 `TODO(phase-userva)` markers in
`crates/tx-shims/src/linux_syscall/mod.rs`: every syscall arm
reads/writes user pointers via **direct kernel-pointer deref**
under a "bootstrap kernel-buffer exemption" rather than through
`UserAccessIf::{copy_from_user, copy_to_user}`. This is fine for
the bake-in fixture (whose pointers are kernel-VA in the direct
map) but breaks the moment a real userspace program passes a
genuine user pointer. Migrate every arm:

- `read_user_cstr` helper (already used for execve path) extended
  to handle EFAULT properly.
- `read_user_bytes` / `write_user_bytes` helpers.
- Replace direct deref in: `sys_write`, `sys_read`, `sys_openat`
  (path), `sys_pipe2` (pipefd write), `sys_clone` (parent_tidptr,
  child_tidptr, tls), `sys_set_tid_address`, `sys_set_robust_list`,
  `sys_rt_sigaction` (act/oldact), `sys_rt_sigprocmask` (set/oldset),
  `sys_wait4` (status, rusage), `sys_getresuid` / `sys_getresgid`
  (out pointers), `sys_faccessat2` (path).

Risk: each migration requires deciding the EFAULT contract for
that arm (most short-circuit; some, like `sys_write`, do partial
progress). The fork/clone/wait4 slice's `_user_cstr` decode at
execve is the template.

#### C.2 — Pipe lifecycle Drop hook (~80 LOC)

Wave 3 carryover. Wire `Cap<OpenFile>` Drop (or an explicit
`OpenFile::on_close` hook called from `sys_close`,
`sys_dup3`-replace, fork's CLOEXEC sweep, and the exit-cleanup
path) to call `pipe::PipePayload::decr_reader` /
`decr_writer` for `StructPayload::Pipe { side, .. }` files.
Today's `decr_*` helpers already wake the opposite-side carrier;
the hook just connects them.

#### C.3 — Real per-task AST signal stacks (~250 LOC)

Audit Tier-1 #9. Today signal delivery routes through the
process-level pending queue; the per-thread AST check inside
`enter_userspace_with_context` is the day-1 stub. Multi-threaded
processes won't deliver the right signal to the right thread at
the right hart. **Single-threaded shells** (the day-1 target)
work without this; defer until needed.

#### C.4 — Futex (~400 LOC)

`futex(uaddr, FUTEX_WAIT|FUTEX_WAKE, ...)`. New
`tx_subsystems::futex` subsystem mirroring `pipe.rs` shape:

- Per-uaddr `BTreeMap<UserVa, FutexBucket>` global registry, or a
  hashed bucket array.
- Each bucket carries a `Channel` registered with `wait_carrier`.
- `FUTEX_WAIT(uaddr, val, timeout)`: `copy_from_user(uaddr) ==
  val` check; if matches, `Blocked(WaitToken)` until woken or
  timeout.
- `FUTEX_WAKE(uaddr, n)`: fire bucket's carrier with mask
  matching `n` waiters.
- `FUTEX_PRIVATE_FLAG` recognised but ignored (per-process
  semantics fall out for free with shared aspace).

musl uses futex internally for `pthread_once`-style guards even
in single-threaded programs (libc startup hits
`__lock_acquire_recursive` paths). Without futex, **musl-built
shells panic at libc init**. This is non-negotiable for booting
busybox.

#### C.5 — `nanosleep` + timer wait carrier (~150 LOC)

`nanosleep(req, rem)` and `clock_nanosleep`. Reactor already
exposes a deadline API (`P::set_deadline_ns`); just need a
per-task timer wait carrier and a `step_nanosleep` arm that
parks on it. Shells use `sleep` builtin; bash uses
`nanosleep(0)` for yields.

### D. Integration work (no kernel code; build + smoke)

#### D.1 — Busybox bake-in (~200 LOC)

A `register_busybox_into_tmpfs()` helper mirroring
`register_init_fixture_into_tmpfs` but reading bytes from a
build-time `include_bytes!` against the path in `TX_BUSYBOX` env
var (or, more cleanly, an xtask-generated `pub static
BUSYBOX_BYTES: &[u8]` blob).

Plus an "init shell wrapper" — a 50-byte assembly shim that
`execve("/bin/sh", argv, envp)`s busybox. Or skip the shim and
register busybox as `/init` directly with a hard-coded
`argv=["sh"]`.

#### D.2 — QEMU shell smoke test (~150 LOC)

`cargo xtask qemu-shell-smoke`:
1. Spawn QEMU with the kernel + tmpfs initramfs containing busybox.
2. Watch serial output for the configured shell prompt sentinel
   (e.g. `# `).
3. Send `echo hello\n` over stdin.
4. Watch for `hello` in serial output.
5. Send `exit\n`; watch for kernel `:userspace:exited:0` sentinel.
6. PASS.

The xtask `qemu` module already has timeout + sentinel-match
plumbing. This is wiring, not new infrastructure.

#### D.3 — Cross-toolchain doc (~10 LOC)

`docs/DEVELOPMENT.md` already says "BusyBox initramfs requires
TX_BUSYBOX". Add a riscv64 cross-toolchain section + a
`cargo xtask doctor riscv64` check.

### E. Page-substrate user-readiness (separate work)

STATUS.md open blockers carried from earlier slices:

> RV64 QEMU still needs superpage/multi-frame map-count batching,
> production remote-hart shootdown coordination, and VM/syscall/user-return
> trap policy before the page substrate is user/VM-ready.

Hard to estimate without deeper investigation. **Single-process
single-hart shell smoke** likely doesn't need it (one cwd, one
aspace, no fork bombs). Multi-hart busybox `top` definitely
does.

Triage: defer to a sibling "page-substrate user-readiness"
slice, run after the shell smoke is green on a single hart.

## Slice ordering

Dependency-respecting ordering with success criteria. Each slice
is one PR, tracked the same way fd-ops was.

### Slice 1 — Pipe lifecycle hook (~1 day)

Smallest first. Closes Wave 3 carryover. Unblocks shell pipelines
(`ls | grep`).

**Success:** `close(reader_fd)` flips `reader_count` to 0;
existing test transition logic stops needing the explicit
`decr_*` calls; one new test
`pipe_close_last_reader_fd_wakes_writer_for_sigpipe`.

### ~~Slice 2 — User-VA sweep~~ — **deferred to Slice 10** (re-ordered 2026-05-07)

**Original framing:** "Foundation for everything else."

**Why deferred:** investigation during Slice 1 commit showed there
is **no production `UserAccessIf` impl for RV64** — only the
default-trait EFAULT stub plus test-only impls in
`page_backed/user_buffer_tests.rs`. Migrating the 18
`TODO(phase-userva)` sites today would just turn every
user-pointer-using syscall into a hard EFAULT against the bake-in
fixture, which is actively kernel-VA mapped via the bootstrap
exemption.

**New shape:** the user-VA sweep happens as a single slice later in
the roadmap (Slice 10, formerly Slice 2), bundled with the
production RV64 `UserAccessIf` impl. Slices 2–9 continue the
existing `TODO(phase-userva)` pattern; one PR migrates them all
at once.

### Slice 2 — VM syscalls (~2–3 days) [was Slice 3]

`mmap`, `munmap`, `mprotect`, `madvise`, `mremap`, `msync`. All
pure plumbing — primitives exist. Unblocks musl libc startup
(thread stack alloc, malloc heap).

**Success:** Anonymous-mmap → write → munmap round-trip dispatch
test. File-mmap of a tmpfs file dispatch test (read-back via
direct user-VA after the mapping installs).

### Slice 3 — Futex (~4–7 days) [was Slice 4]

The largest single piece in the roadmap. Without it, musl
panics at libc init. Mirrors fd-ops Wave 3's pipe pattern:
new `tx_subsystems::futex` module + `wait_carrier` integration.

**Success:** `FUTEX_WAIT(uaddr, val)` parks; `FUTEX_WAKE(uaddr,
1)` wakes. Timeout via `set_deadline_ns`. PRIVATE flag
recognised. Single-thread musl-init smoke (a hand-built fixture
that does the libc-startup futex dance) reaches `_start`'s first
`write(2)`.

### Slice 4 — Time syscalls (~2 days) [was Slice 5]

`clock_gettime`, `gettimeofday`, `nanosleep`,
`clock_nanosleep`. Quick. `nanosleep` reuses futex's wait-carrier
pattern.

**Success:** `clock_gettime(CLOCK_MONOTONIC)` returns increasing
values; `nanosleep(100ms)` parks for ≥100ms; nightly LTP
`nanosleep01` passes.

### Slice 5 — IOCTL + TTY routing (~2 days) [was Slice 6]

Pure plumbing. Eight TTY ioctl arms exist as step functions; one
syscall arm decodes `request` and routes. Without this, **musl's
isatty(stdin) check returns false**, the shell starts in
non-interactive mode, no prompt is printed.

**Success:** `ioctl(0, TCGETS, &termios)` returns 0; `ioctl(0,
TIOCGWINSZ, &winsize)` returns 0; `isatty(0)` is true; bash/dash
prints a prompt over QEMU stdio.

### Slice 6 — Stat family (~3 days) [was Slice 7]

`fstat`, `newfstatat`, `getdents64`, `getcwd`, `chdir`,
`fchdir`, `umask`. Mostly mechanical given existing primitives.

**Success:** `ls -l /` works; `cd /tmp; pwd` works; `getdents64`
returns entries for tmpfs root.

### Slice 7 — fcntl extension + remaining day-1 misc (~2 days) [was Slice 8]

`F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETFL`, `F_SETFL`. Plus
`uname`, `prlimit64`, `getrandom`, `getpgrp`-fix, `kill`,
`tkill`, `tgkill`, `rt_sigreturn` if not already wired.

**Success:** Each LTP `fcntl*` test that's not behind an unrelated
gap passes.

### Slice 8 — File-mutation syscalls (~4 days) [was Slice 9]

`unlinkat`, `mkdirat`, `renameat2`, `symlinkat`, `linkat`,
`readlinkat`, `truncate`, `ftruncate`, `utimensat`. Larger because
each needs a new `FsOps` step on the tmpfs side.

**Success:** Shell can `rm -r /tmp/x; mkdir /tmp/x; cp /etc/hosts
/tmp/x/`. Each LTP file-mutation cluster passes its day-1 sub-set.

### Slice 9 — User-VA sweep + RV64 UserAccessIf (~5–7 days) [was Slice 2]

The deferred foundation. Now lands as a unified slice with three
parts:

**Part A — RV64 production `UserAccessIf` impl.** Set `mstatus.SUM
= 1` around the copy. Use a fixup-table approach:
`copy_from_user` / `copy_to_user` are short asm sequences whose
fault-PC range is registered with the kernel trap handler; on a
kernel-mode page fault inside that range, the handler resumes at
the fixup PC with EFAULT in the result register instead of
panicking. ~400–500 LOC including the fault-handler integration.

**Part B — Migrate the 18 `TODO(phase-userva)` sites + any added
by slices 2–8.** ~300 LOC.

**Part C — EFAULT contract test per arm.** Each migrated arm gets
a "-EFAULT on unmapped user pointer" test using a `FaultingHal`
stand-in.

**Success:** Zero `TODO(phase-userva)` markers in
`linux_syscall/mod.rs`. Workspace tests prove EFAULT is propagated
through every arm. RV64 `UserAccessIf` impl passes a host-level
unit test against a synthetic page-table.

### Slice 10 — Busybox bake-in + boot wire (~2 days)

`register_busybox_into_tmpfs()` + `/init` shim. Establishes the
boot path that the QEMU smoke will exercise.

**Success:** `cargo xtask image rv64-qemu-virt` produces a
bootable image. Boot-time serial shows the kernel + busybox init
banner.

### Slice 11 — QEMU shell smoke (~2 days)

The capstone test. Boots kernel + busybox over QEMU, sends
`echo hello`, watches for `hello`.

**Success:** `cargo xtask qemu-shell-smoke` PASSES on CI.

### Slice 12+ — Page-substrate user-readiness (separate roadmap)

Deferred. Triggered when multi-hart shell or LTP stress tests
surface superpage / shootdown / trap-policy gaps.

## Total estimate

- Slices 1–11: **~25–35 dev-days** of focused work.
- LOC: **~3500–4500** new + churn (excluding the busybox blob).
- New syscall arms: **~36** (roughly doubles the current 33).
- New subsystems: **1** (`futex`).

The shape is heavy on plumbing and integration; the only large
new substrate piece is futex. Everything else is wiring existing
primitives to syscall arms.

## What "shell prompt" actually means at the end

After Slice 11:

```
$ cargo xtask qemu-shell-smoke
[...]
txkernel:rv64-qemu-virt:userspace:up
~ # echo hello
hello
~ # exit
txkernel:rv64-qemu-virt:userspace:exited:0
PASS
```

That's the bar. LTP coverage past this point is incremental and
slice-driven, not roadmap-blocking.

## Out of scope (deferred, deliberate)

- Networking syscalls (`socket` family, `sendmsg`/`recvmsg`).
  Shells don't need them; LTP has a separate net cluster.
- `epoll`, `pselect6` for I/O multiplexing. busybox shell builtins
  don't use them; a future slice for `tail -f`-shape work owns them.
- `inotify` / `fanotify`. Out for v1.
- `splice` / `tee` / `vmsplice`. Wave 3 deferred.
- Named FIFOs (`mkfifo`). Wave 3 deferred.
- Process namespaces (`unshare`, `setns`). Out for v1.
- `seccomp`. Out for v1.
- Real swap / `swapon`. Out for v1.
- `io_uring`. Out for v1.

## Open questions

1. **Init shape.** Does init exec busybox directly via the
   bake-in fixture, or do we land a tiny C shim built against
   musl that does `execve("/bin/sh", argv, envp)`? Default: use
   the bake-in fixture path, no shim — busybox supports
   `BB_RUN_AS_INIT` pid-1 mode.
2. **Cross-toolchain provisioning.** Should `cargo xtask doctor`
   block the smoke test if no riscv64 cross-toolchain is found,
   or download one to a workspace cache? Default: doctor warns
   only; CI provisions in a separate setup step.
3. **Static busybox vs musl-dynamic.** Static is simpler (no
   ld.so), smaller test surface for slices 2–8. Default: static
   only for v1; musl-dynamic is a sibling roadmap.
4. **Test parallelism on tx-subsystems.** `--test-threads=1` is
   carryover; not a shell-prompt blocker, but worth a
   "global-state cleanup" mini-slice once the rest lands.
