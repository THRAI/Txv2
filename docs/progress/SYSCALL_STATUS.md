# txKernel Linux syscall implementation status

<!-- txdoc:SYSCALL-STATUS-1 -->

**Living record.** This is the canonical map of what's wired, what's stubbed,
and what's missing in `crates/tx-shims/src/linux_syscall/`. Update it any time
a syscall lane changes state (added, removed, classification changed). See
[`tx-ltp-syscall`](../../.agents/skills/tx-ltp-syscall/SKILL.md) for the
update workflow.

**Gold standard.** A syscall is "done" only when at least one **OSComp**
test (`cargo xtask oscomp run --target rv64-qemu`, harness at
`external/oscomp-autotest/`) and/or **LTP** test (Linux Test Project, run
inside the same QEMU image) moves from failed/skipped to passing because of
the change. Host unit tests and `cargo xtask ci` are sanity checks for the
inner loop; OSComp + LTP are the correctness bar. When a syscall lands,
record which specific OSComp/LTP test(s) closed it under "Currently
passing" below.

**Last refresh:** 2026-05-18 (6th pass — wired the 5
defined-no-arm RT_SIG entries: `sys_rt_sigtimedwait` is a real
async poll-and-yield loop on `payload.pending()`; `sigaltstack`
returns 0 instead of `-ENOSYS`; `sigpending`, `sigsuspend`, and
`sigqueueinfo` are now reachable. Headline counts:
dispatched 107 → **112**, defined-no-arm 13 → **8**. Plus
`populate_rootfs_tmp_dirs()` at boot creates `/tmp`, `/var`,
`/var/tmp` in the rootfs tmpfs — closes the first of two
lmbench blockers. Scoreboard: libctest no longer dies on
`sigtimedwait: ENOSYS` — every test now reaches the test body
but most still time out because the child `entry-static.exe`
invocations don't post `SIGCHLD` back to the parent in time
(next investigation). basic-musl 102/102, lua-musl 9/9,
busybox-musl 52/55, libcbench ~14/27, lmbench 0/36 unchanged in
the last recorded run; the `/var/tmp` change isn't end-to-end
verified yet in this worktree.)

## Headline counts

> **The dispatch table is the SSoT** for the counts and the per-syscall
> status. The values below are sourced from
> `cargo xtask syscall status` and refreshed by
> `cargo xtask syscall sync` (which rewrites
> [Auto-maintained syscall table](#auto-maintained-syscall-table) at
> the bottom of this file).

- `pub const NR_*` defined in `numbers.rs`: **120**
- Dispatched in `mod.rs` (unique match arms): **112**
- Currently stubbed (returns `-ENOSYS`): see "Already-partial" below for
  human-curated entries; the auto-table flags additional likely stubs
  by heuristic.
- `NR_*` defined but no dispatch arm: **8** — see the auto-table.
- Outright unwired (no `NR_*` definition, no arm): **~116** syscalls — see
  categorized list below.

To recount manually:

```sh
cargo xtask syscall status               # canonical counts
grep -c '^pub const NR_' crates/tx-shims/src/linux_syscall/numbers.rs
```

## High-stakes directions (prioritized by LTP impact ÷ effort)

The table below is the canonical "where to focus" map. Effort is in worktree
weeks (S ≤ 2w, M = 3–4w, L = 6–10w, XL ≥ 12w). LTP impact estimates the
number of additional LTP tests that move from skipped/failed to runnable.

| Gap | OSComp / LTP impact | Effort | Substrate status |
|---|---|---|---|
| Diagnose why libctest children don't reach `exit_group` within the 10 s libctest internal timeout (parent's `sigtimedwait` is **verified correct** by the new `sigtimedwait_dispatch.rs` test module; the wedge is upstream of the SIGCHLD post) | **+~220 OSComp** | M–L — needs trap-trace of `entry-static.exe` under QEMU; suspects: fork-without-execve COW path slowness, missing handler-return path, or per-test syscall hangs | wait-side **proven correct** end-to-end (`sigtimedwait_observes_sigchld_posted_by_child_exit` test exercises fork → `step_exit_group` → `post_sigchld_to_parent` → `sigtimedwait` returning SIGCHLD with the bit cleared) |
| lmbench unblock — `mkdir /var/tmp` at boot + diagnose `Simple read: -1` | up to +36 OSComp (lmbench-musl 0/36) | S–M (mkdir is hours; `Simple read` needs runtime triage) | `mkdir` is trivial; `read(2)` edge case to investigate |
| libcbench malloc / stdio gaps | up to +13 OSComp (`~14/27` → close to 27/27) | M (~3w) | malloc benches return 0 → allocator instrumentation; stdio failures suggest fd / buffering path |
| `preadv` / `pwritev` / `fallocate` / `readahead` | +20 LTP | S–M (3–4w) | `writev` loop + VFS hooks exist |
| `getrlimit` / `setrlimit` + `sched_getaffinity` | +15 LTP | S (~2w) | per-proc resource field |
| POSIX `mq_*` | +15 LTP | M (3–4w) | single kernel queue object, no namespace work |
| per-process timers (`timer_create` family, `setitimer`, `getrusage`) | +20 LTP | M (2–3w) | deadline tracking shared with `timerfd` |
| `sendfile` / `splice` / `copy_file_range` | +15 LTP | M (3–4w) | needs page-cache coherence path |
| ext4 file-content writeback (`flush_page` / `fsync_file`) | +ext4 LTP | M (~3w) | currently `-ENOSYS`; namespace writes already persist via the pager's direct-write path |
| SysV IPC (`msg` / `sem` / `shm`) | +40 LTP | L (6–10w) | new namespace-aware subsystem |
| Network stack (full socket API) | +60 LTP + iperf/netperf | XL (12–16w) | no subsystem exists — TCP/UDP state, sockaddr unions, sk_buff |
| `inotify` / `fanotify` | +10 LTP | M (~3w) | new event queue subsystem |
| `chroot` / `pivot_root` / `swap*` | +8 LTP | S–M (1–2w) | `chroot` is one field; bdev-fs lands swap |
| `seccomp` / capabilities / `keyctl` | +15 LTP | L (6–8w) | filter bytecode + keyring |
| `ptrace` | +20 LTP | XL (12w+) | parallel exec context — out of scope v1 |
| `bpf` / `perf_event_open` | +10 LTP | XL (12w+) | out of scope v1 |

**Reading the table.** The top row (env symlink) is by far the highest
LTP/OSComp impact per worktree-hour today: 229 OSComp tests are blocked
by a single missing interpreter on the rootfs. After that, lmbench
mkdir + libcbench triage are the cheap wins. Long-tail Linux ABI gaps
(`preadv`, `getrlimit`, `mq_*`, …) come next; SysV IPC and the network
stack are larger but unlock the biggest LTP coverage jumps. `ptrace`
and `bpf` are explicitly **out of scope for v1** — flag them and move
on unless the user specifically chartered them.

**Recently landed (removed from this table):**
- `fork` + CLOEXEC bitmap — audit 2026-05-18 found it was already
  wired; unblocked 32/32 basic-musl after orthogonal signal-frame /
  mount / dirfd fixes in PR #30 + #31.
- `utimensat` stub + auto-mounted `/proc` + `/proc/meminfo` + ext4
  `rename`/`rmdir` + `O_APPEND` `pc.set_size_bytes` + `NR_SYSLOG`
  stub — landed on `main` via PR #33 (busybox-musl 52/55, unblocked
  `touch`, `dmesg`, `free`, `ps`, `df`, `mv`, `rmdir`, and 6 append
  tests).
- New xtask scoring subcommands (`oscomp score`, `list-suites`, `test`)
  — landed in PR #33; let us produce the per-suite scoreboard above
  with one command (`cargo xtask oscomp test --target rv64-qemu`).
- Shebang shims (`/bin/sh`, `/bin/busybox`, `/usr/bin/env`) +
  kernel-side ENOEXEC fallback to `/bin/sh` — landed on this branch
  2026-05-18 (lua 0/9 → 9/9, basic 101/102 → 102/102, libctest now
  executes — see "Top-of-table fix landed" below).
- `populate_rootfs_tmp_dirs()` at boot — auto-create `/tmp`,
  `/var`, `/var/tmp` in the rootfs tmpfs. Closes the first of
  two lmbench blockers (the `/var/tmp` ENOENT during suite
  setup). The second blocker — `Simple read: -1` — is a runtime
  `read(2)` edge case and remains open. Landed on this branch
  2026-05-18 alongside the RT_SIG wiring. End-to-end OSComp QEMU
  verification pending (cpio image requires vendored busybox).
- 5 RT_SIG dispatch arms wired (`sys_rt_sigpending`,
  `sys_rt_sigsuspend`, `sys_rt_sigqueueinfo`, `sys_rt_sigtimedwait`,
  `sys_sigaltstack`) — landed on this branch 2026-05-18. The
  `sigtimedwait` body is a real async poll loop that consumes
  `payload.pending()` and `NanosleepOp`-yields between iterations;
  the others use existing handlers / a `Return(0)` for the altstack
  no-op. Headline: dispatched 107 → 112, defined-no-arm 13 → 8.
  Effect on libctest: `[signal Killed]` → `[timed out]` — the
  parent now correctly blocks in `sigtimedwait`, but child
  `entry-static.exe` processes don't appear to exit promptly
  enough; root cause still under investigation (see top high-stakes
  row).

## OSComp + LTP coverage (the gold standard)

The skill treats OSComp and LTP as the canonical correctness bar. Track
both here so the project knows what real-world tests pass on top of the
internal sanity checks.

### OSComp

Harness: `external/oscomp-autotest/` (git submodule). Runners (after PR #33):

```sh
cargo xtask oscomp qemu        --target rv64-qemu          # boot + run
cargo xtask oscomp list-suites --target rv64-qemu          # available suites
cargo xtask oscomp score       --target rv64-qemu          # score the last run
cargo xtask oscomp test        --target rv64-qemu          # qemu → score combo
cargo xtask oscomp test        --target rv64-qemu --suite busybox-musl
```

#### Scoreboard (rv64-qemu, last full-suite run)

| Suite | Score | Δ | Status |
|---|---:|---:|---|
| `basic-musl`   | **102/102** | +1 | full pass |
| `busybox-musl` | **52/55**   | 0  | landed; 3 non-kernel (`hwclock`, `kill 10`, `which ls`) |
| `libcbench-musl` | **~13.9/27** | 0 | partial; malloc benches + stdio tests fail |
| `libctest-musl`  | **14/220 (and counting)** | **+14** | TimerToken-mismatch fix in `tx_scripts::drive::resolve_on_timer` lets parent's `OnTimer` yield resume correctly; argv/basename/clocale_mbfuncs/clock_gettime/dirname/env/fdopen/fnmatch/fscanf/fwscanf/iconv_open/inet_pton/mbc/memstream all pass. Next FAIL is `pthread_cancel_points` (SIGSEGV — expected without pthread support); kernel then trapped on a user-pointer `read_volatile` during `pthread_cancel` (separate kernel-side EFAULT bug, pre-existing) |
| `lua-musl`     | **9/9**     | +9 | full pass |
| `lmbench-musl` | **0/36**    | 0  | needs `/var/tmp` + `Simple read: -1` triage |

#### Top-of-table fix landed (2026-05-18, this branch)

Two cooperating changes unblocked the shebang/exec wrapper failures
that had been blocking 229 OSComp tests:

1. **`populate_rootfs_shebang_shims()`** at kernel boot — auto-create
   `/bin/`, `/usr/`, `/usr/bin/` in the tmpfs rootfs and symlink:
   - `/bin/busybox` → `/musl/musl/busybox`
   - `/bin/sh`      → `/musl/musl/busybox`
   - `/usr/bin/env` → `/musl/musl/busybox`

   Carved into [`crates/tx-kernel/src/init/rootfs_shims.rs`](../../crates/tx-kernel/src/init/rootfs_shims.rs).
   Invoked from the boot sequence between `mount_sdcard_at_musl()`
   and `bind_init_cwd_and_root()`.

2. **Kernel-side ENOEXEC fallback** in
   [`crates/tx-scripts/src/process/exec/script.rs`](../../crates/tx-scripts/src/process/exec/script.rs) —
   when a file is **not** ELF and does **not** start with `#!`,
   exec_script now synthesizes `#!/bin/sh <path> [argv…]` instead of
   returning `ExecError::NotExecutable`. This matches every userspace
   shell's standard ENOEXEC fallback, but is needed at the kernel
   layer because busybox ash's fallback only fires for files whose
   first character looks "script-like" — and the libctest wrappers
   `run-static.sh` / `run-dynamic.sh` start with `./runtest.exe …`
   (no shebang) which ash gives up on.

   Two unit tests were inverted to assert the new behaviour:
   `exec_script_non_elf_non_shebang_falls_back_to_bin_sh` and
   `dispatch_execve_non_elf_non_shebang_falls_back_to_bin_sh`.

Result: **lua-musl 0/9 → 9/9**, **basic-musl 101/102 → 102/102**,
and **libctest now actually runs all 220 tests** — followed by
the RT_SIG dispatch wiring later in this branch (see "Third
move" section below) which moved libctest from `[signal Killed]`
to `[timed out]`. Score still 0/220 pending child-exit / SIGCHLD
investigation.

#### Second move: lmbench (36 tests)

Two blockers; (1) **landed this branch 2026-05-18** via
`populate_rootfs_tmp_dirs()` (see Recently landed), (2) **still
open**:

- ~~`/var/tmp/` doesn't exist; `mkdir` it at boot.~~ Landed:
  `populate_rootfs_tmp_dirs()` in
  [`crates/tx-kernel/src/init/rootfs_shims.rs`](../../crates/tx-kernel/src/init/rootfs_shims.rs)
  now creates `/tmp` (0o777), `/var` (0o755), and `/var/tmp`
  (0o777) in the rootfs tmpfs at boot.
- `Simple read: -1` is a timing/IO issue and needs runtime triage
  before the rest of the suite can be scored. Likely a `read(2)`
  return-value or `pread`-style edge case.

#### Third move: wire `sys_rt_sigtimedwait` for libctest (landed)

`NR_RT_SIGTIMEDWAIT=137` was one of 13 entries in `defined-but-no-arm`.
Wired on this branch 2026-05-18 as a real async poll loop:

```rust
loop {
    let pending = payload.pending().snapshot() & set_bits;
    if pending != 0 {
        let signum_raw = (pending.trailing_zeros() + 1) as u8;
        payload.pending().clear(sig);
        return SyscallResult::Return(signum_raw as i64);
    }
    if read_ns() >= deadline_ns { return -EAGAIN; }
    NanosleepOp { nanos: 5ms, .. }.await;  // yield to reactor
}
```

Plus four cooperating arms (`sigpending`, `sigsuspend`,
`sigqueueinfo`, `sigaltstack` — last returns 0 instead of `-ENOSYS`).

**Effect on libctest:** every test now reaches the test body
(no more `ENOSYS` / `[signal Killed]`). Score still 0/220 because
children `entry-static.exe` invocations time out — the parent
correctly blocks in `sigtimedwait` but the child doesn't appear
to exit / post `SIGCHLD` in the allotted window. This is the new
top high-stakes row: the gap shifted from the wait-side to the
child-exit / signal-post path.

**Wait-side proven correct.** Five new host tests in
`crates/tx-shims/src/linux_syscall/tests/sigtimedwait_dispatch.rs`
exercise the dispatch arm directly:

| Test | What it asserts |
|---|---|
| `sigtimedwait_returns_pending_signum_and_clears_bit` | A pre-loaded SIGCHLD on the calling thread's `payload.pending()` is observed and cleared; `Return(17)` |
| `sigtimedwait_wrong_sigsetsize_returns_neg_einval` | musl-style 8 byte sigset is required (glibc-style 16 → -EINVAL) |
| `sigtimedwait_null_set_returns_neg_efault` | NULL `set` returns -EFAULT, not -EINVAL or panic |
| `sigtimedwait_ignores_signals_outside_set_returns_neg_eagain` | Pending SIGTERM with a `set = {SIGCHLD}` doesn't get confused; -EAGAIN and SIGTERM remains pending |
| `sigtimedwait_observes_sigchld_posted_by_child_exit` | End-to-end: fork → `step_exit_group(child)` → `post_sigchld_to_parent` → `sigtimedwait` returns SIGCHLD with the parent's pending bit cleared |

The last test is the load-bearing one — it exercises the **exact**
kernel-side post → read path libctest relies on, including
`step_exit_group`'s call into `post_sigchld_to_parent` →
`step_kill_process(parent, SIGCHLD)` → `post_signal(thread,
SIGCHLD)` → `payload.pending().post(SIGCHLD)`. All five pass on
host. So:

- The bit encoding is right (`Signum(17).bit() == 1 << 16`).
- The sigset_t / timespec userspace pointers decode correctly.
- The `payload.pending().snapshot() & set_bits` poll observes
  bits posted by the canonical SIGCHLD-post path.
- The clear-bit-on-consume path works.
- The 8 byte sigsetsize check matches musl's RV64 ABI.

The libctest `[timed out]` therefore cannot be the
`sys_rt_sigtimedwait` wait body. The wedge is **upstream** of
the SIGCHLD post — most likely:

1. The child takes longer than libctest's 10 s internal
   timeout to reach `exit_group` (fork-without-execve COW path,
   demand-paging, or scheduler artefacts).
2. The child hangs on a syscall the kernel responds to with the
   wrong value or never resumes (futex, pthread, or a
   conditional branch).
3. The kernel doesn't schedule the child between the parent's
   `NanosleepOp` chunks (single-task reactor starvation —
   unlikely given the test harness has been running multi-task
   workloads in QEMU for months, but worth ruling out under
   trap-trace).

**Next step.** Run `cargo xtask qemu --target rv64-qemu` with a
`/bin/sh -c "./run-static.sh argv"` invocation isolated to one
libctest test, capture the trap log, then `cargo xtask
fault-decode --target rv64-qemu --serial <log> --user-elf
.../entry-static.exe --summary` to see where the child is
spending its time. The 10 s internal timeout means we should
see plenty of forward progress before the deadline if the child
is genuinely slow; if the child wedges on one specific syscall,
the log will show many repeats of that syscall just before the
parent's `SIGKILL`.

#### basic-musl 32/32 detail (carries over)

The full 32 basic-musl binaries reach `END test_*` markers within a
single QEMU run:

`brk`, `chdir`, `clone`, `close`, `dup`, `dup2`, `execve`, `exit`,
`fork`, `fstat`, `getcwd`, `getdents`, `getpid`, `getppid`,
`gettimeofday`, `mkdir_`, `mmap`, `mount`, `munmap`, `open`, `openat`,
`pipe`, `read`, `sleep`, `times`, `umount`, `uname`, `unlink`, `wait`,
`waitpid`, `write`, `yield`.

(`test_echo` is a fixture invoked by `test_execve`, not counted as a
separate test. The 101/102 score above is per-assertion across the
suite, with a single `mmap` assertion still partial.)

#### busybox-musl 52/55 fix manifest (PR #33 / commit `ebe6803`)

| Fix | File | Effect |
|---|---|---|
| `O_APPEND` on ext4 — `pc.set_size_bytes(meta.size)` after `PageContainer::new_cap()` | `crates/tx-ext4/src/namespace.rs` | unblocks 6 append tests (`echo "…" >> test.txt`) |
| `utimensat` stub returns 0 instead of -ENOSYS | `crates/tx-shims/src/linux_syscall/fs_mut.rs` | `touch(1)` |
| `syslog` / `dmesg` — `NR_SYSLOG=116` dispatch returning 0 | `fs_mut.rs`, `numbers.rs`, `mod.rs` | `dmesg(1)` |
| ext4 `rename` via `lookup` + `append_dir_entry` + `remove_dir_entry` | `crates/tx-ext4/src/namespace.rs` | `mv(1)` |
| ext4 `rmdir` via `remove_dir_entry` | `crates/tx-ext4/src/namespace.rs` | `rmdir(1)` |
| `/proc/meminfo` wired into procfs `lookup` / `readdir` / `render` | `crates/tx-fs/src/procfs/{mod,read}.rs` | `free(1)` |
| `mount_procfs_at_proc()` at kernel init | `crates/tx-kernel/src/init.rs` | `free(1)`, `ps(1)`, `df(1)` |

#### Not yet run

`cyclictest`, `iperf`, `netperf`, `iozone` — no end-to-end run
recorded. Network suites (`iperf` / `netperf`) require the absent
socket stack (see high-stakes table).

### LTP

LTP binaries live inside the QEMU userspace image; the canonical invocation
is to launch `cargo xtask qemu --target rv64-qemu` and run the specific LTP
binary from the busybox shell (the exact entry point is whatever the
current image ships).

**Runnable today (~25 tests, syscall-shape implied by current dispatch
table — no end-to-end LTP-in-QEMU run yet recorded for this kernel).**
Family-by-family expected coverage:
- `execve01` / `04` / `06`: should pass (execve wired end-to-end per
  basic-musl).
- `stat` / `fstat` / `newfstatat` / `statx`: pass when the live size path
  is exercised (PR #30 wired this; basic-musl `test_fstat` validates).
- `mkdir` / `unlink` / `rename` / `symlink`: wired.
- `getdents64`: wired.
- `epoll_*` / `futex` / `timerfd` / `eventfd` / `signalfd` basics: wired;
  full `epoll_*` bodies still partial (see Already-partial).
- `rt_sigaction` / `rt_sigprocmask`: wired.
- `rt_sigreturn`: wired in PR #30 — moves from -ENOSYS to functional.
- AIO core (PR-11): wired.
- `clone01` / `fork01` / `fork02`: should pass (PR #30 wired libc-style
  `newsp` handling).

**Standing follow-up:** capture an actual LTP run against the kernel and
enumerate the passing/failing tests by name in a `Currently passing
(LTP)` subsection. Right now the doc records the *shape* of LTP coverage
implied by syscall wiring; an explicit list of named passing LTP tests
needs an LTP image inside the QEMU userspace.

### When a syscall lands

Add a line under the relevant subsection naming the specific OSComp/LTP
test(s) that newly pass — e.g. *"2026-05-19: `getrlimit01`, `getrlimit02`
pass after rlimit field landed (commit `<sha>`)"*. The skill's "Done Means"
requires this entry before the work counts as complete.

## Unwired by topic (~116 syscalls)

### File I/O & VFS extras (18)

`preadv`, `pwritev`, `preadv2`, `pwritev2`, `sendfile`, `copy_file_range`,
`splice`, `tee`, `sync_file_range`, `readahead`, `fallocate`,
`name_to_handle_at`, `open_by_handle_at`, `fanotify_init` / `_mark`,
`inotify_init1` / `_add_watch` / `_rm_watch`.

### Network — entire socket API (20)

`socket`, `socketpair`, `bind`, `listen`, `accept`, `accept4`, `connect`,
`getsockname`, `getpeername`, `send`, `sendto`, `sendmsg`, `sendmmsg`,
`recv`, `recvfrom`, `recvmsg`, `recvmmsg`, `shutdown`, `setsockopt`,
`getsockopt`.

### IPC SysV (12)

`msgget` / `msgsnd` / `msgrcv` / `msgctl`, `semget` / `semop` / `semctl` /
`semtimedop`, `shmget` / `shmat` / `shmctl` / `shmdt`.

### IPC POSIX queues (10)

`mq_open` / `_close` / `_unlink` / `_getattr` / `_setattr` / `_send` /
`_receive` / `_timedsend` / `_timedreceive` / `_notify`.

### Memory extended (8)

`mlock2`, `mbind`, `migrate_pages`, `get_mempolicy`, `set_mempolicy`,
`process_vm_readv` / `_writev`, `memfd_create`.

### Process / sched / limits (22)

`setns`, `unshare`, `pidfd_getfd`, `getrlimit`, `setrlimit`, `getrusage`,
`sched_*` family (`setparam` / `getparam` / `setscheduler` / `getscheduler` /
`get_priority_max` / `_min` / `setaffinity` / `getaffinity` / `yield` /
`setattr` / `getattr` / `rr_get_interval`), `capget`, `capset`, `prctl`.

### Timer / time (7)

`timer_create` / `_settime` / `_gettime` / `_getoverrun` / `_delete`,
`clock_adjtime`, `adjtimex`.

### Security & keys (7)

`keyctl`, `add_key`, `request_key`, `seccomp`, `landlock_create_ruleset` /
`_add_rule` / `_restrict_self`.

### Filesystem misc / mount (4)

`pivot_root`, `chroot`, `swapon`, `swapoff`.

### Misc / debug (8)

`reboot`, `kexec_load`, `syslog`, `perf_event_open`, `bpf`, `ptrace`,
`process_madvise`, `close_range`.

## Already-partial (existing arms returning `-ENOSYS`)

These syscalls have an `NR_*` defined and a dispatch arm wired, but the body
is stubbed. Closing them is usually cheaper than greenfield work because
classification, ctx threading, and tests are already in place.

- `io_uring_enter` user-ring path
- `userfaultfd` phases 2–5 (UFFDIO_REGISTER / fault interception / reply)
- `futex` REQUEUE / CMP_REQUEUE / PI variants (WAIT/WAKE wired)
- `epoll_*` bodies beyond core (create1/ctl/wait/pwait dispatch arms exist;
  bodies return -ENOSYS pending select/poll integration)
- `mount` fstypes beyond `tmpfs` / `devfs` / `proc` / `ext4` / `vfat`-alias
- `umount2` flag bits beyond the bare path-based unmount
- `utimensat` (busybox-musl `df` / `ps` / `free` / `touch` blocker)

**Recently closed:**
- `rt_sigreturn` — wired in PR #30 (`take_saved_signal_context()` →
  `store_saved_user_context()`; unblocks every signal-handler return).
- `sys_clone` `newsp` rejection — fixed in PR #30 (libc-style `clone(fn,
  NULL, stack, …)` now works).
- `pipe`/`eventfd`/`timerfd`/process adapter `register_source` gaps —
  fixed in PR #31 (`drive()`-driven pipe reads no longer park forever).

## How to update this file

When you implement or change a syscall:

1. If a new `NR_*` was added, increment the "defined" count in **Headline
   counts**. If a new dispatch arm was wired, increment the "dispatched"
   count. Use the `grep` commands above to recount — eyeball counts drift.
2. If the change closes a stubbed arm, remove it from **Already-partial**.
3. If the change adds a syscall from the unwired list, remove it from the
   matching topic section and bump the section heading count (e.g. "18" → "17").
4. If the change shifts the high-stakes table — a row landed, an effort
   estimate changed, a substrate prerequisite cleared — update the row or
   delete it. Add a brief justification in the commit / catch-up.
5. **Record the OSComp/LTP test(s) that newly pass** in the relevant
   subsection of "OSComp + LTP coverage". This is the gold-standard
   correctness bar — the skill's "Done Means" requires at least one named
   test, with a date, commit, and the syscall it exercises.
6. Update **Last refresh** at the top.
7. The skill's "Done Means" section also requires a one-line entry in
   `docs/progress/STATUS.md` pointing at this file and the commit.

## Auto-maintained syscall table

<!-- BEGIN syscall-auto-table (generated by `cargo xtask syscall sync`) -->

_This section is generated by `cargo xtask syscall sync` from
`crates/tx-shims/src/linux_syscall/{numbers,mod}.rs`. Do not edit
between the BEGIN/END sentinels by hand — your changes will be
overwritten by the next `sync`. The lint variant
`cargo xtask lint syscall-status` fails on drift._

### Counts (from dispatch table)

- `pub const NR_*` in numbers.rs: **120**
- dispatched in mod.rs: **112** (of which async: 43, likely-stub: 0)
- defined but not dispatched: **8**

### Defined in `numbers.rs` but no dispatch arm (8)

These have a syscall number constant but no match arm in `mod.rs`. Either wire them up or remove the constant.

- `NR_EPOLL_CREATE1` (nr=291)
- `NR_EPOLL_CTL` (nr=233)
- `NR_EPOLL_PWAIT` (nr=281)
- `NR_EPOLL_WAIT` (nr=232)
- `NR_IO_URING_ENTER` (nr=426)
- `NR_PIDFD_OPEN` (nr=434)
- `NR_PIDFD_SEND_SIGNAL` (nr=424)
- `NR_SIGNALFD` (nr=282)

### Dispatched syscalls (112) — name → handler

Sorted by syscall number. `*` marks `async` handlers; `[stub]` marks bodies the heuristic flagged.

| NR | Name | Handler | Lane |
|---:|---|---|---|
| 17 | `NR_GETCWD` | `sys_getcwd` | sync |
| 23 | `NR_DUP` | `sys_dup` | sync |
| 24 | `NR_DUP3` | `sys_dup3` | sync |
| 25 | `NR_FCNTL` | `sys_fcntl` | sync |
| 29 | `NR_IOCTL` | `sys_ioctl` | sync |
| 32 | `NR_FLOCK` | `sys_flock` | async |
| 33 | `NR_MKNODAT` | `sys_mknodat` | async |
| 34 | `NR_MKDIRAT` | `sys_mkdirat` | async |
| 35 | `NR_UNLINKAT` | `sys_unlinkat` | async |
| 36 | `NR_SYMLINKAT` | `sys_symlinkat` | async |
| 37 | `NR_LINKAT` | `sys_linkat` | async |
| 39 | `NR_UMOUNT2` | `sys_umount2` | async |
| 40 | `NR_MOUNT` | `sys_mount` | async |
| 43 | `NR_STATFS` | `sys_statfs` | async |
| 44 | `NR_FSTATFS` | `sys_fstatfs` | async |
| 45 | `NR_TRUNCATE` | `sys_truncate` | async |
| 46 | `NR_FTRUNCATE` | `sys_ftruncate` | async |
| 48 | `NR_FACCESSAT` | `sys_faccessat` | sync |
| 49 | `NR_CHDIR` | `sys_chdir` | async |
| 50 | `NR_FCHDIR` | `sys_fchdir` | async |
| 53 | `NR_FCHMODAT` | `sys_fchmodat` | sync |
| 54 | `NR_FCHOWNAT` | `sys_fchownat` | sync |
| 56 | `NR_OPENAT` | `sys_openat` | async |
| 57 | `NR_CLOSE` | `sys_close` | sync |
| 59 | `NR_PIPE2` | `sys_pipe2` | sync |
| 61 | `NR_GETDENTS64` | `sys_getdents64` | async |
| 62 | `NR_LSEEK` | `sys_lseek` | sync |
| 63 | `NR_READ` | `sys_read` | sync |
| 64 | `NR_WRITE` | `sys_write` | sync |
| 65 | `NR_READV` | `sys_readv` | async |
| 66 | `NR_WRITEV` | `sys_writev` | async |
| 73 | `NR_PPOLL` | `sys_ppoll` | async |
| 74 | `NR_SIGNALFD4` | `sys_signalfd4` | sync |
| 78 | `NR_READLINKAT` | `sys_readlinkat` | async |
| 79 | `NR_NEWFSTATAT` | `sys_newfstatat` | async |
| 80 | `NR_FSTAT` | `sys_fstat` | sync |
| 81 | `NR_GETPGRP` | `sys_getpgrp` | sync |
| 81 | `NR_SYNC` | `sys_sync` | async |
| 82 | `NR_FSYNC` | `sys_fsync` | async |
| 83 | `NR_FDATASYNC` | `sys_fdatasync` | async |
| 88 | `NR_UTIMENSAT` | `sys_utimensat` | sync |
| 93 | `NR_EXIT` | `sys_exit` | sync |
| 94 | `NR_EXIT_GROUP` | `sys_exit_group` | sync |
| 96 | `NR_SET_TID_ADDRESS` | `sys_set_tid_address` | sync |
| 98 | `NR_FUTEX` | `sys_futex` | async |
| 99 | `NR_SET_ROBUST_LIST` | `sys_set_robust_list` | sync |
| 101 | `NR_NANOSLEEP` | `sys_nanosleep` | async |
| 113 | `NR_CLOCK_GETTIME` | `sys_clock_gettime` | sync |
| 115 | `NR_CLOCK_NANOSLEEP` | `sys_clock_nanosleep` | async |
| 116 | `NR_SYSLOG` | `sys_syslog` | sync |
| 129 | `NR_KILL` | `sys_kill` | sync |
| 130 | `NR_TKILL` | `sys_tkill` | sync |
| 131 | `NR_TGKILL` | `sys_tgkill` | sync |
| 132 | `NR_SIGALTSTACK` | `sys_sigaltstack` | sync |
| 133 | `NR_RT_SIGSUSPEND` | `sys_rt_sigsuspend` | sync |
| 134 | `NR_RT_SIGACTION` | `sys_rt_sigaction` | sync |
| 135 | `NR_RT_SIGPROCMASK` | `sys_rt_sigprocmask` | sync |
| 136 | `NR_RT_SIGPENDING` | `sys_rt_sigpending` | sync |
| 137 | `NR_RT_SIGTIMEDWAIT` | `sys_rt_sigtimedwait` | async |
| 138 | `NR_RT_SIGQUEUEINFO` | `sys_rt_sigqueueinfo` | sync |
| 139 | `NR_RT_SIGRETURN` | `sys_rt_sigreturn` | sync |
| 143 | `NR_SETREGID` | `sys_setregid` | sync |
| 144 | `NR_SETGID` | `sys_setgid` | sync |
| 145 | `NR_SETREUID` | `sys_setreuid` | sync |
| 146 | `NR_SETUID` | `sys_setuid` | sync |
| 147 | `NR_SETRESUID` | `sys_setresuid` | sync |
| 148 | `NR_GETRESUID` | `sys_getresuid` | sync |
| 149 | `NR_SETRESGID` | `sys_setresgid` | sync |
| 150 | `NR_GETRESGID` | `sys_getresgid` | sync |
| 153 | `NR_TIMES` | `sys_times` | sync |
| 154 | `NR_SETPGID` | `sys_setpgid` | sync |
| 155 | `NR_GETPGID` | `sys_getpgid` | sync |
| 156 | `NR_GETSID` | `sys_getsid` | sync |
| 157 | `NR_SETSID` | `sys_setsid` | sync |
| 160 | `NR_UNAME` | `sys_uname` | sync |
| 166 | `NR_UMASK` | `sys_umask` | sync |
| 169 | `NR_GETTIMEOFDAY` | `sys_gettimeofday` | sync |
| 172 | `NR_GETPID` | `sys_getpid` | sync |
| 173 | `NR_GETPPID` | `sys_getppid` | sync |
| 174 | `NR_GETUID` | `sys_getuid` | sync |
| 175 | `NR_GETEUID` | `sys_geteuid` | sync |
| 176 | `NR_GETGID` | `sys_getgid` | sync |
| 177 | `NR_GETEGID` | `sys_getegid` | sync |
| 178 | `NR_GETTID` | `sys_gettid` | sync |
| 206 | `NR_IO_SETUP` | `sys_io_setup` | sync |
| 207 | `NR_IO_DESTROY` | `sys_io_destroy` | sync |
| 208 | `NR_IO_GETEVENTS` | `sys_io_getevents` | async |
| 209 | `NR_IO_SUBMIT` | `sys_io_submit` | sync |
| 214 | `NR_BRK` | `sys_brk` | async |
| 215 | `NR_MUNMAP` | `sys_munmap` | async |
| 216 | `NR_MREMAP` | `sys_mremap` | async |
| 220 | `NR_CLONE` | `sys_clone` | async |
| 221 | `NR_EXECVE` | `sys_execve` | async |
| 222 | `NR_MMAP` | `sys_mmap` | async |
| 226 | `NR_MPROTECT` | `sys_mprotect` | async |
| 227 | `NR_MSYNC` | `sys_msync` | async |
| 228 | `NR_MLOCK` | `sys_mlock` | async |
| 229 | `NR_MUNLOCK` | `sys_munlock` | async |
| 233 | `NR_MADVISE` | `sys_madvise` | sync |
| 260 | `NR_WAIT4` | `sys_wait4` | async |
| 261 | `NR_PRLIMIT64` | `sys_prlimit64` | sync |
| 267 | `NR_SYNCFS` | `sys_syncfs` | async |
| 276 | `NR_RENAMEAT2` | `sys_renameat2` | async |
| 278 | `NR_GETRANDOM` | `sys_getrandom` | sync |
| 282 | `NR_USERFAULTFD` | `sys_userfaultfd` | sync |
| 283 | `NR_TIMERFD_CREATE` | `sys_timerfd_create` | sync |
| 286 | `NR_TIMERFD_SETTIME` | `sys_timerfd_settime` | sync |
| 287 | `NR_TIMERFD_GETTIME` | `sys_timerfd_gettime` | sync |
| 290 | `NR_EVENTFD2` | `sys_eventfd2` | sync |
| 291 | `NR_STATX` | `sys_statx` | async |
| 425 | `NR_IO_URING_SETUP` | `sys_io_uring_setup` | sync |
| 439 | `NR_FACCESSAT2` | `sys_faccessat2` | sync |

<!-- END syscall-auto-table -->

## Doc cross-references

- Dispatch-lane classification: `docs/Txv3/04_SYSCALL_SHAPE_v1.md`,
  `docs/Txv3/03_STEP_MODEL_v2.md`, and the
  [`tx-syscall-dispatch`](../../.agents/skills/tx-syscall-dispatch/SKILL.md)
  skill.
- Step-op migration when porting a v4 syscall to the v5 typed `StepOp`:
  [`tx-step-migration`](../../.agents/skills/tx-step-migration/SKILL.md).
- Observe→fix→verify loop for shell-driven syscall failures:
  [`tx-shell-syscall-fixup`](../../.agents/skills/tx-shell-syscall-fixup/SKILL.md).
- Subsystem ownership of the entity a syscall acts on:
  [`tx-subsystem-manifest`](../../.agents/skills/tx-subsystem-manifest/SKILL.md).
- Canonical syscall phasing source: `docs/progress/plans/2026-05-05-trio-trap-syscall-tmpfs-devfs.md`.
