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

**Last refresh:** 2026-05-18 (initial seed; OSComp/LTP gold-standard
framing added same day).

## Headline counts

- `pub const NR_*` defined in `numbers.rs`: **119**
- Dispatched in `mod.rs` (per match arms): **~96**
- Currently stubbed (returns `-ENOSYS`): see "Already-partial" below.
- Outright unwired (no `NR_*` definition, no arm): **~116** syscalls — see
  categorized list below.

The numbers above are approximate at seed time. When you change the table,
recount with:

```sh
grep -c '^pub const NR_' crates/tx-shims/src/linux_syscall/numbers.rs
grep -cE 'NR_[A-Z_0-9]+ =>' crates/tx-shims/src/linux_syscall/mod.rs
```

## High-stakes directions (prioritized by LTP impact ÷ effort)

The table below is the canonical "where to focus" map. Effort is in worktree
weeks (S ≤ 2w, M = 3–4w, L = 6–10w, XL ≥ 12w). LTP impact estimates the
number of additional LTP tests that move from skipped/failed to runnable.

| Gap | LTP impact | Effort | Substrate status |
|---|---|---|---|
| `fork` + CLOEXEC bitmap | +50 fork/exec chain tests | S (~2w) | scaffolded in trio (commit `a70f4a0`); plumbing only |
| `preadv` / `pwritev` / `fallocate` / `readahead` | +20 | S–M (3–4w) | `writev` loop + VFS hooks exist |
| POSIX `mq_*` | +15 | M (3–4w) | single kernel queue object, no namespace work |
| per-process timers (`timer_create` family, `setitimer`, `getrusage`) | +20 | M (2–3w) | deadline tracking shared with `timerfd` |
| `getrlimit` / `setrlimit` + `sched_getaffinity` | +15 | S (~2w) | per-proc resource field |
| `sendfile` / `splice` / `copy_file_range` | +15 | M (3–4w) | needs page-cache coherence path |
| SysV IPC (`msg` / `sem` / `shm`) | +40 | L (6–10w) | new namespace-aware subsystem |
| Network stack (full socket API) | +60 | XL (12–16w) | no subsystem exists — TCP/UDP state, sockaddr unions, sk_buff |
| `inotify` / `fanotify` | +10 | M (~3w) | new event queue subsystem |
| `chroot` / `pivot_root` / `swap*` | +8 | S–M (1–2w) | `chroot` is one field; bdev-fs lands swap |
| `seccomp` / capabilities / `keyctl` | +15 | L (6–8w) | filter bytecode + keyring |
| `ptrace` | +20 | XL (12w+) | parallel exec context — out of scope v1 |
| `bpf` / `perf_event_open` | +10 | XL (12w+) | out of scope v1 |

**Reading the table.** Best LTP-impact-per-effort: `fork` + CLOEXEC,
`getrlimit`/`setrlimit`, then `preadv`/`pwritev`/`fallocate`. SysV IPC and the
network stack are larger but unlock the biggest LTP coverage jumps. `ptrace`
and `bpf` are explicitly **out of scope for v1** — flag them and move on
unless the user specifically chartered them.

## OSComp + LTP coverage (the gold standard)

The skill treats OSComp and LTP as the canonical correctness bar. Track
both here so the project knows what real-world tests pass on top of the
internal sanity checks.

### OSComp

Harness: `external/oscomp-autotest/` (git submodule). Runner:
`cargo xtask oscomp run --target rv64-qemu`.

**Currently passing (as of last refresh):** brk, chdir, close, mkdir,
unlink, sleep (see STATUS.md entries on 2026-05-13). The full passing set
should be enumerated here by name as each syscall lands; the
`cargo xtask oscomp` output is the source of truth.

**Known partial:** clone (partial impl), mmap/munmap (file creation
cascades needed), mount (ENOSYS), openat (dirfd≠AT_FDCWD not yet
supported), execve advanced cases.

### LTP

LTP binaries live inside the QEMU userspace image; the canonical invocation
is to launch `cargo xtask qemu --target rv64-qemu` and run the specific LTP
binary from the busybox shell (the exact entry point is whatever the
current image ships).

**Runnable today (~25 tests):** `execve01`/`04`/`06` basics, `stat`/`fstat`
family, `mkdir`/`unlink`/`rename`/`symlink`, `getdents64`, `epoll`/`futex`/
`timerfd`/`eventfd`/`signalfd` basics, `rt_sigaction`/`procmask` basics, AIO
core (PR-11).

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
- `userfaultfd` phases 2–5
- `futex` REQUEUE / PI variants
- `epoll_*` bodies (beyond core)
- `rt_sigreturn`

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

<!-- BEGIN AUTOGEN: syscall-table — `cargo xtask syscall-status --regen` -->

### Mechanical status (autogenerated)

_Counts read from `crates/tx-shims/src/linux_syscall/{numbers.rs, mod.rs}`._
_Run `cargo xtask syscall-status --regen` to refresh; `--check` to lint in CI._

- **`NR_*` defined:** 159
- **Dispatched (has a match arm):** 150
- **Defined but not dispatched:** 9 — see list below

#### Defined but not dispatched

These syscalls have a `pub const NR_*` in `numbers.rs` but no match arm in `dispatch()`. Closing them is usually cheaper than greenfield work — ABI numbering and one-line intent already exist.

| `NR_*` | # | Summary |
|---|---:|---|
| `NR_EPOLL_WAIT` | 232 | `epoll_wait(epfd, events, maxevents, timeout)`. The RV64/LA64 |
| `NR_GETPEERNAME` | 205 | `getpeername(sockfd, addr, addrlen)`. Linux generic ABI `__NR_getpeername`. |
| `NR_GETSOCKOPT` | 209 | `getsockopt(sockfd, level, optname, optval, optlen)`. Linux generic ABI `__NR_g… |
| `NR_PIDFD_OPEN` | 434 | `pidfd_open(pid, flags)` — Linux RV64. |
| `NR_PIDFD_SEND_SIGNAL` | 424 | `pidfd_send_signal(pidfd, sig, info, flags)` — Linux RV64. |
| `NR_SEMTIMEDOP` | 192 | `semtimedop(semid, sops, nsops, timeout)`. Linux generic uapi `__NR_semtimedop … |
| `NR_SHUTDOWN` | 210 | `shutdown(sockfd, how)`. Linux generic ABI `__NR_shutdown`. |
| `NR_SIGNALFD` | 282 | Historical `signalfd(fd, &mask, sizemask)` (no `flags`). x86_64 |
| `NR_SOCKETPAIR` | 199 | `socketpair(domain, type, protocol, sv)`. Linux generic ABI `__NR_socketpair`. |

<!-- END AUTOGEN: syscall-table -->

<!-- BEGIN syscall-auto-table (generated by `cargo xtask syscall sync`) -->

_This section is generated by `cargo xtask syscall sync` from
`crates/tx-shims/src/linux_syscall/{numbers,mod}.rs`. Do not edit
between the BEGIN/END sentinels by hand — your changes will be
overwritten by the next `sync`. The lint variant
`cargo xtask lint syscall-status` fails on drift._

### Counts (from dispatch table)

- `pub const NR_*` in numbers.rs: **159**
- dispatched in mod.rs: **149** (of which async: 41, likely-stub: 0)
- defined but not dispatched: **10**

### Defined in `numbers.rs` but no dispatch arm (10)

These have a syscall number constant but no match arm in `mod.rs`. Either wire them up or remove the constant.

- `NR_EPOLL_WAIT` (nr=232)
- `NR_GETPEERNAME` (nr=205)
- `NR_GETSOCKOPT` (nr=209)
- `NR_IO_URING_ENTER` (nr=426)
- `NR_PIDFD_OPEN` (nr=434)
- `NR_PIDFD_SEND_SIGNAL` (nr=424)
- `NR_SEMTIMEDOP` (nr=192)
- `NR_SHUTDOWN` (nr=210)
- `NR_SIGNALFD` (nr=282)
- `NR_SOCKETPAIR` (nr=199)

### Dispatched syscalls (149) — name → handler

Sorted by syscall number. `*` marks `async` handlers; `[stub]` marks bodies the heuristic flagged.

| NR | Name | Handler | Lane |
|---:|---|---|---|
| 0 | `NR_IO_SETUP` | `sys_io_setup` | sync |
| 1 | `NR_IO_DESTROY` | `sys_io_destroy` | sync |
| 2 | `NR_IO_SUBMIT` | `sys_io_submit` | sync |
| 4 | `NR_IO_GETEVENTS` | `sys_io_getevents` | async |
| 17 | `NR_GETCWD` | `sys_getcwd` | sync |
| 20 | `NR_EPOLL_CREATE1` | `sys_epoll_create1` | sync |
| 21 | `NR_EPOLL_CTL` | `sys_epoll_ctl` | sync |
| 22 | `NR_EPOLL_PWAIT` | `sys_epoll_wait` | sync |
| 23 | `NR_DUP` | `sys_dup` | sync |
| 24 | `NR_DUP3` | `sys_dup3` | sync |
| 25 | `NR_FCNTL` | `sys_fcntl` | sync |
| 29 | `NR_IOCTL` | `sys_ioctl` | sync |
| 32 | `NR_FLOCK` | `sys_flock` | sync |
| 33 | `NR_MKNODAT` | `sys_mknodat` | async |
| 34 | `NR_MKDIRAT` | `sys_mkdirat` | async |
| 35 | `NR_UNLINKAT` | `sys_unlinkat` | async |
| 36 | `NR_SYMLINKAT` | `sys_symlinkat` | async |
| 37 | `NR_LINKAT` | `sys_linkat` | async |
| 39 | `NR_UMOUNT2` | `sys_umount2` | async |
| 40 | `NR_MOUNT` | `sys_mount` | async |
| 43 | `NR_STATFS` | `sys_statfs` | sync |
| 44 | `NR_FSTATFS` | `sys_fstatfs` | sync |
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
| 61 | `NR_GETDENTS64` | `sys_getdents64` | sync |
| 62 | `NR_LSEEK` | `sys_lseek` | sync |
| 63 | `NR_READ` | `sys_read` | sync |
| 64 | `NR_WRITE` | `sys_write` | sync |
| 65 | `NR_READV` | `sys_readv` | async |
| 66 | `NR_WRITEV` | `sys_writev` | async |
| 67 | `NR_PREAD64` | `sys_pread64` | async |
| 71 | `NR_SENDFILE64` | `sys_sendfile64` | async |
| 73 | `NR_PPOLL` | `sys_ppoll` | async |
| 74 | `NR_SIGNALFD4` | `sys_signalfd4` | sync |
| 78 | `NR_READLINKAT` | `sys_readlinkat` | async |
| 79 | `NR_NEWFSTATAT` | `sys_newfstatat` | async |
| 80 | `NR_FSTAT` | `sys_fstat` | sync |
| 81 | `NR_GETPGRP` | `sys_getpgrp` | sync |
| 81 | `NR_SYNC` | `sys_sync` | sync |
| 82 | `NR_FSYNC` | `sys_fsync` | sync |
| 83 | `NR_FDATASYNC` | `sys_fdatasync` | sync |
| 85 | `NR_TIMERFD_CREATE` | `sys_timerfd_create` | sync |
| 86 | `NR_TIMERFD_SETTIME` | `sys_timerfd_settime` | sync |
| 87 | `NR_TIMERFD_GETTIME` | `sys_timerfd_gettime` | sync |
| 88 | `NR_UTIMENSAT` | `sys_utimensat` | sync |
| 93 | `NR_EXIT` | `sys_exit` | sync |
| 94 | `NR_EXIT_GROUP` | `sys_exit_group` | sync |
| 96 | `NR_SET_TID_ADDRESS` | `sys_set_tid_address` | sync |
| 98 | `NR_FUTEX` | `sys_futex` | async |
| 99 | `NR_SET_ROBUST_LIST` | `sys_set_robust_list` | sync |
| 100 | `NR_GET_ROBUST_LIST` | `sys_get_robust_list` | sync |
| 101 | `NR_NANOSLEEP` | `sys_nanosleep` | async |
| 113 | `NR_CLOCK_GETTIME` | `sys_clock_gettime` | sync |
| 115 | `NR_CLOCK_NANOSLEEP` | `sys_clock_nanosleep` | async |
| 116 | `NR_SYSLOG` | `sys_syslog` | sync |
| 119 | `NR_SCHED_SETSCHEDULER` | `sys_sched_setscheduler` | sync |
| 122 | `NR_SCHED_SETAFFINITY` | `sys_sched_setaffinity` | sync |
| 123 | `NR_SCHED_GETAFFINITY` | `sys_sched_getaffinity` | sync |
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
| 180 | `NR_MQ_OPEN` | `sys_mq_open` | sync |
| 181 | `NR_MQ_UNLINK` | `sys_mq_unlink` | sync |
| 182 | `NR_MQ_TIMEDSEND` | `sys_mq_timedsend` | async |
| 183 | `NR_MQ_TIMEDRECEIVE` | `sys_mq_timedreceive` | async |
| 184 | `NR_MQ_NOTIFY` | `sys_mq_notify` | sync |
| 185 | `NR_MQ_GETSETATTR` | `sys_mq_getsetattr` | sync |
| 186 | `NR_MSGGET` | `sys_msgget` | sync |
| 187 | `NR_MSGCTL` | `sys_msgctl` | sync |
| 188 | `NR_MSGRCV` | `sys_msgrcv` | sync |
| 189 | `NR_MSGSND` | `sys_msgsnd` | sync |
| 190 | `NR_SEMGET` | `sys_semget` | sync |
| 191 | `NR_SEMCTL` | `sys_semctl` | sync |
| 193 | `NR_SEMOP` | `sys_semop` | sync |
| 194 | `NR_SHMGET` | `sys_shmget` | sync |
| 195 | `NR_SHMCTL` | `sys_shmctl` | sync |
| 196 | `NR_SHMAT` | `sys_shmat` | async |
| 197 | `NR_SHMDT` | `sys_shmdt` | async |
| 198 | `NR_SOCKET` | `sys_socket` | sync |
| 200 | `NR_BIND` | `sys_bind` | sync |
| 201 | `NR_LISTEN` | `sys_listen` | sync |
| 202 | `NR_ACCEPT` | `sys_accept` | sync |
| 203 | `NR_CONNECT` | `sys_connect` | sync |
| 204 | `NR_GETSOCKNAME` | `sys_getsockname` | sync |
| 206 | `NR_SENDTO` | `sys_sendto` | sync |
| 207 | `NR_RECVFROM` | `sys_recvfrom` | sync |
| 208 | `NR_SETSOCKOPT` | `sys_setsockopt` | sync |
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
| 242 | `NR_ACCEPT4` | `sys_accept4` | sync |
| 260 | `NR_WAIT4` | `sys_wait4` | async |
| 261 | `NR_PRLIMIT64` | `sys_prlimit64` | sync |
| 267 | `NR_SYNCFS` | `sys_syncfs` | sync |
| 276 | `NR_RENAMEAT2` | `sys_renameat2` | async |
| 278 | `NR_GETRANDOM` | `sys_getrandom` | sync |
| 282 | `NR_USERFAULTFD` | `sys_userfaultfd` | sync |
| 283 | `NR_MEMBARRIER` | `sys_membarrier` | sync |
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
