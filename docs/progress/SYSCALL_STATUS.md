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

**Last refresh:** 2026-07-21 (mechanical tables regenerated after the compound
dispatch-guard parser was fixed; no syscall behavior changed in this refresh).

## Headline counts

- `pub const NR_*` defined in `numbers.rs`: **243**
- Dispatched in `mod.rs` (per match arms): **243**
- Defined but not dispatched: **0**.
- True missing from local `numbers.rs` vs Linux RV64 v6.17: **82**.
- Number mismatches vs Linux RV64 v6.17: **0**.
- Local `NR_*` extras not in the Linux RV64 v6.17 reference: **5**.

The generated sections below are the mechanical source of truth. Recount with
`cargo xtask syscall-status` or refresh with
`cargo xtask syscall-status --regen && cargo xtask syscall sync`.

## High-stakes directions (prioritized by LTP impact ÷ effort)

The table below is the canonical "where to focus" map. Effort is in worktree
weeks (S ≤ 2w, M = 3–4w, L = 6–10w, XL ≥ 12w). LTP impact estimates the
number of additional LTP tests that move from skipped/failed to runnable.

| Gap | LTP impact | Effort | Substrate status |
|---|---|---|---|
| Timer/time tail (`timer_create` family, `getitimer`, `setitimer`, `clock_adjtime`, `adjtimex`) | +18 timer tests | M (3–4w) | wallclock set/get policy, vDSO conversion state, `timerfd`, and `nanosleep` exist; per-process timer ids/signals plus adjustment/slew policy still needed |
| Lightweight process/sysinfo tail (`waitid`, `clone3`, `pidfd_getfd`, `setns`, `unshare`, `sysinfo`) | +20 process/namespace tests | M–L (4–8w) | wait/clone/pidfd scaffolding exists; namespace view and sysinfo accounting semantics need care |
| Network semantic-depth slice (socket options, ancillary data, protocol-specific behavior, blocking/cancellation) | +25 socket tests | M–L (4–8w) | the socket and message-vector syscall families are defined and dispatched; remaining work is Linux semantic coverage rather than dispatch-table closure |
| Metadata/xattr tail (`xattr*`) | +15 fs metadata tests | M (3–5w) | chmod/chown fd variants are wired; xattr still needs filesystem-facing storage policy |
| Modern path/mount APIs (`openat2`, `execveat`, `open_tree`, `move_mount`, `fsopen`/`fsconfig`/`fsmount`/`fspick`, `mount_setattr`) | +15 fs namespace tests | L (6–10w) | path walk/mount objects exist, but Linux open/mount attr ABI needs policy decisions |
| Event notification depth (`inotify_*`, `fanotify_*`) | +12 event-loop tests | M (3–5w) | `epoll_pwait2` is wired and inotify/fanotify numbers dispatch to scaffold validation; real queues still need VFS fsnotify sources and fanotify-permission policy |
| `io_uring`/AIO tail (`io_uring_register`, `io_cancel`, `io_pgetevents`, user-mmapped ring depth) | +10 async I/O tests | M (3–5w) | setup, raw AIO core, and a nonblocking `io_uring_enter` scaffold exist; real user-mmapped SQ/CQ parsing and registration remain |
| Memory policy/advice tail (`mincore`, `mlock2`, `mlockall`, `process_vm_*`, `memfd_create`, memory policy syscalls) | +15 mm tests | L (6–10w) | VM/pagebacked primitives exist; policy and cross-process access need design |
| Security/observability (`capget`, `capset`, `seccomp`, `keyctl`, `landlock_*`, `bpf`, `perf_event_open`, LSM syscalls) | +15 security/tooling tests | XL (12w+) | mostly new policy engines; keep behind explicit charter |
| `ptrace` | +20 debugger/process-control tests | XL (12w+) | parallel exec-control model; out of scope for v1 unless explicitly chartered |

**Reading the table.** The no-new-design tranche landed on 2026-05-24:
`close_range`, resource/scheduler query aliases, positioned/vector file I/O,
file allocation/cache hints, `copy_file_range`, and fd chmod/chown variants
are now defined and dispatched. The follow-up easy ABI query/no-op sweep also
landed on 2026-05-24: `clock_getres`, `getcpu`, `personality`, `getgroups`,
`restart_syscall`, `sched_setparam`, `getpriority`, `setpriority`,
`ioprio_get`, and `ioprio_set`. Best remaining impact-per-effort is now
timer/time probes, lightweight process/sysinfo calls, and network completion.
The pipe/splice tail also landed on 2026-05-24: `splice`, `tee`, and
`vmsplice` now use Linux RV64 v6.17 numbers and dispatch through a
lease-capable pipe/page-backed staging path, with pipe-to-pipe `splice`,
non-consuming `tee`, `vmsplice` iovec writes, Linux pipe offset-pointer
`ESPIPE`, known splice flag validation, file/pipe offset-pointer preservation,
resizable `F_GETPIPE_SZ`/`F_SETPIPE_SZ` pipe capacity, and full-page
PageBacked lease transfer covered by host tests. `SPLICE_F_GIFT` remains
explicit tech debt until VM user-page pin/adoption exists.
The wallclock/vDSO slice also landed on 2026-05-24:
`clock_settime(CLOCK_REALTIME)`, `settimeofday`, realtime offset state,
seqlock-protected vDSO conversion snapshots, realtime absolute sleep
revalidation, and `timerfd` cancel-on-set/rearm semantics. SysV IPC and POSIX mq
are no longer high-stakes missing rows
because their syscall numbers are defined and dispatched; remaining work there
is semantic depth, not missing-table closure. Network is now a semantic-depth
slice rather than "no subsystem exists": `socketpair`, `shutdown`,
`getpeername`, `getsockopt`, and the message-vector syscall family are all
defined and dispatched, while protocol-specific behavior and edge-case ABI
coverage still block a broad LTP socket tier. `ptrace`, `bpf`, Landlock,
keyrings, and perf remain
explicit v1 non-goals unless separately chartered.

The event-notification numbering slice also landed on 2026-05-24:
`epoll_pwait2` now shares the mailbox-backed epoll wait path with
nanosecond `timespec` timeout parsing, while `inotify_init1`,
`inotify_add_watch`, `inotify_rm_watch`, `fanotify_init`, and
`fanotify_mark` are defined and dispatched as deliberate scaffolds. The
inotify/fanotify arms validate obvious init flag errors and otherwise return
`ENOSYS` until VFS fsnotify event queues and fanotify permission delegation
are designed.

The already-partial cleanup on 2026-05-24 closed the stale partial list:
`io_uring_enter` now resolves ring fds and drains the in-kernel SQ scaffold
into CQEs, epoll no longer returns `ENOSYS` for nonzero-timeout no-ready waits
in the host path and reports pending userfaultfd faults as readable, and the
manual status now matches the already-landed futex REQUEUE/PI,
userfaultfd phase 2–5, and `rt_sigreturn` implementations.

### Easy Sweep Landed

The no-new-design easy sweep now has host coverage for the fixed v1 behavior:

| Syscall(s) | Landed behavior |
|---|---|
| `clock_getres` | validates the same clock IDs as `clock_gettime`, writes fixed `{tv_sec=0, tv_nsec=1}` resolution, and accepts a null result pointer after clock validation |
| `getcpu` | writes CPU `0` and node `0` for non-null output pointers and ignores the obsolete cache pointer |
| `personality` | returns the Linux default personality, accepts query/default no-op set, and rejects unsupported changes |
| `getgroups` | returns 0 groups for non-negative `gidsetsize`, writes no entries, and rejects negative size |
| `restart_syscall` | explicit `ENOSYS` arm distinguishes it from unknown missing until interrupted-sleep restart exists |
| `sched_setparam` | validates pid and `sched_param`; accepts priority 0 for current fixed `SCHED_OTHER`; rejects nonzero priority |
| `getpriority`, `setpriority` | supports `PRIO_PROCESS` self/current process; returns Linux raw nice-0 value 20 and accepts no-op set within Linux nice range |
| `ioprio_get`, `ioprio_set` | returns default best-effort priority; accepts no-op default self/current sets and rejects unsupported classes |

The following are tempting but **not** easy without design/policy: xattrs
(filesystem storage contract), `chroot`/modern mount APIs (namespace/path-root
policy), `vmsplice(SPLICE_F_GIFT)` real user-page gifting beyond the current
PageBacked file lease path, `waitid`
(full `siginfo_t`/rusage wait semantics), `setfsuid`/`setfsgid` and
`capget`/`capset` (credential/security policy), `sysinfo` (global accounting),
and advanced socket ancillary/protocol semantics.

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

## Unwired by topic (manual grouping)

This hand-maintained grouping is for planning only. The generated Linux RV64
v6.17 table below is authoritative for the exact true-missing list and source
locations.

### Easy ABI query/no-op tail

Fixed-model coverage landed for `sched_setparam`, `getpriority`,
`setpriority`, `getgroups`, `ioprio_get`, `ioprio_set`,
`restart_syscall`, `clock_getres`, `getcpu`, and `personality` on
2026-05-24. The current generated counts are `189` defined, `185`
dispatched, `4` defined-but-no-arm, and `131` true missing. After the
wallclock/vDSO slice the current generated counts are `191` defined, `187`
dispatched, `4` defined-but-no-arm, and `129` true missing. After the
event-notification numbering slice the current generated counts are `197`
defined, `193` dispatched, `4` defined-but-no-arm, and `123` true missing.
After the pipe/splice tail the generated counts reached `200` defined, `196`
dispatched, `4` defined-but-no-arm, and `120` true missing. The 2026-07-21
refresh reports the current state as `243` defined, `243` dispatched, no
defined-but-no-arm entries, and `82` true missing.

### File I/O & VFS extras

`name_to_handle_at`, `open_by_handle_at`, `quotactl`, `acct`, `vhangup`.

### Network semantic depth

`socketpair`, `shutdown`, `getpeername`, `getsockopt`, `sendmsg`, `recvmsg`,
`sendmmsg`, and `recvmmsg` are all defined and dispatched. Remaining work is
protocol-specific behavior, ancillary-data coverage, and Linux blocking/error
semantics rather than missing dispatch arms.

### Metadata and filesystem events

`setxattr`, `lsetxattr`, `fsetxattr`, `getxattr`, `lgetxattr`, `fgetxattr`,
`listxattr`, `llistxattr`, `flistxattr`, `removexattr`, `lremovexattr`,
`fremovexattr`. Event-notification numbers are no longer true-missing, but
inotify/fanotify remain semantic scaffolds until filesystem event publication
exists.

### Memory extended

`mlockall`, `munlockall`, `mincore`, `remap_file_pages`, `mbind`,
`migrate_pages`, `get_mempolicy`, `set_mempolicy`, `move_pages`,
`process_vm_readv`, `process_vm_writev`, `memfd_create`, `process_madvise`.

### Process / namespace / sysinfo

`waitid`, `unshare`, `setns`, `clone3`, `pidfd_getfd`, `setgroups`,
`sethostname`, `setdomainname`, `prctl`, `sysinfo`, `kcmp`, `riscv_hwprobe`,
`riscv_flush_icache`.

### Timer / time

`getitimer`, `setitimer`, `timer_create`, `timer_settime`, `timer_gettime`,
`timer_getoverrun`, `timer_delete`, `clock_adjtime`, `adjtimex`.

### Security & keys

`capget`, `capset`, `keyctl`, `add_key`, `request_key`, `seccomp`,
`landlock_create_ruleset`, `landlock_add_rule`, `landlock_restrict_self`.

### Filesystem misc / mount

`pivot_root`, `chroot`, `open_tree`, `move_mount`, `fsopen`, `fsconfig`,
`fsmount`, `fspick`, `mount_setattr`, `openat2`, `execveat`, `swapon`,
`swapoff`.

### Misc / debug / observability

`reboot`, `kexec_load`, `init_module`, `delete_module`, `finit_module`,
`perf_event_open`, `bpf`, `ptrace`, `rt_tgsigqueueinfo`.

## Already-partial (existing arms returning `-ENOSYS`)

These syscalls have an `NR_*` defined and a dispatch arm wired, but the body
is stubbed. Closing them is usually cheaper than greenfield work because
classification, ctx threading, and tests are already in place.

None currently curated. The remaining `restart_syscall` `ENOSYS` is explicit
unsupported restart-state policy, not an already-partial body.

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

_Counts read from `crates/tx-shims/src/linux_syscall/{numbers.rs, mod.rs}` and checked against Linux RV64 v6.17 from `xtask/data/syscalls/riscv/64/rv64/linux-6.17-table.json` (source: https://syscalls.mebeim.net/db/riscv/64/rv64/latest/table.json). Linux file/line references point into `external/linux-rv-6.17`._
_Run `cargo xtask syscall-status --regen` to refresh; `--check` to lint in CI._

- **`NR_*` defined:** 243
- **Linux RV64 reference syscalls:** 320
- **Dispatched (has a match arm):** 243
- **Defined but not dispatched:** 0 — see list below

- **True missing vs Linux RV64 reference:** 82
- **Number mismatches vs Linux RV64 reference:** 0
- **Local `NR_*` not in Linux RV64 reference:** 5

_All defined `NR_*` are wired in `dispatch()`._

#### True missing from local `numbers.rs`

Linux RV64 v6.17 syscalls that have no local `NR_*` constant. This is the greenfield backlog; it is distinct from defined-but-not-dispatched.

| Linux # | Name | Signature | Linux source |
|---:|---|---|---|
| 3 | `io_cancel` | `aio_context_t ctx_id, struct iocb *iocb, struct io_event *result` | `fs/aio.c`:2176 |
| 5 | `setxattr` | `const char *pathname, const char *name, const void *value, size_t size, int flags` | `fs/xattr.c`:743 |
| 6 | `lsetxattr` | `const char *pathname, const char *name, const void *value, size_t size, int flags` | `fs/xattr.c`:750 |
| 7 | `fsetxattr` | `int fd, const char *name, const void *value, size_t size, int flags` | `fs/xattr.c`:758 |
| 8 | `getxattr` | `const char *pathname, const char *name, void *value, size_t size` | `fs/xattr.c`:888 |
| 9 | `lgetxattr` | `const char *pathname, const char *name, void *value, size_t size` | `fs/xattr.c`:894 |
| 10 | `fgetxattr` | `int fd, const char *name, void *value, size_t size` | `fs/xattr.c`:901 |
| 11 | `listxattr` | `const char *pathname, char *list, size_t size` | `fs/xattr.c`:998 |
| 12 | `llistxattr` | `const char *pathname, char *list, size_t size` | `fs/xattr.c`:1004 |
| 13 | `flistxattr` | `int fd, char *list, size_t size` | `fs/xattr.c`:1010 |
| 14 | `removexattr` | `const char *pathname, const char *name` | `fs/xattr.c`:1097 |
| 15 | `lremovexattr` | `const char *pathname, const char *name` | `fs/xattr.c`:1103 |
| 16 | `fremovexattr` | `int fd, const char *name` | `fs/xattr.c`:1109 |
| 41 | `pivot_root` | `const char *new_root, const char *put_old` | `fs/namespace.c`:4661 |
| 51 | `chroot` | `const char *filename` | `fs/open.c`:598 |
| 58 | `vhangup` | `` | `fs/open.c`:1606 |
| 60 | `quotactl` | `unsigned int cmd, const char *special, qid_t id, void *addr` | `fs/quota/quota.c`:917 |
| 89 | `acct` | `const char *name` | `kernel/acct.c`:314 |
| 95 | `waitid` | `int which, pid_t upid, struct siginfo *infop, int options, struct rusage *ru` | `kernel/exit.c`:1797 |
| 104 | `kexec_load` | `unsigned long entry, unsigned long nr_segments, struct kexec_segment *segments, unsigne…` | `kernel/kexec.c`:242 |
| 105 | `init_module` | `void *umod, unsigned long len, const char *uargs` | `kernel/module/main.c`:3569 |
| 106 | `delete_module` | `const char *name_user, unsigned int flags` | `kernel/module/main.c`:776 |
| 117 | `ptrace` | `long request, long pid, unsigned long addr, unsigned long data` | `kernel/ptrace.c`:1387 |
| 142 | `reboot` | `int magic1, int magic2, unsigned int cmd, void *arg` | `kernel/reboot.c`:728 |
| 151 | `setfsuid` | `uid_t uid` | `kernel/sys.c`:940 |
| 152 | `setfsgid` | `gid_t gid` | `kernel/sys.c`:984 |
| 162 | `setdomainname` | `char *name, int len` | `kernel/sys.c`:1473 |
| 171 | `adjtimex` | `struct __kernel_timex *txc_p` | `kernel/time/time.c`:269 |
| 179 | `sysinfo` | `struct sysinfo *info` | `kernel/sys.c`:2896 |
| 217 | `add_key` | `const char *_type, const char *_description, const void *_payload, size_t plen, key_ser…` | `security/keys/keyctl.c`:74 |
| 218 | `request_key` | `const char *_type, const char *_description, const char *_callout_info, key_serial_t de…` | `security/keys/keyctl.c`:167 |
| 219 | `keyctl` | `int option, unsigned long arg2, unsigned long arg3, unsigned long arg4, unsigned long a…` | `security/keys/keyctl.c`:1874 |
| 224 | `swapon` | `const char *specialfile, int swap_flags` | `mm/swapfile.c`:3259 |
| 225 | `swapoff` | `const char *specialfile` | `mm/swapfile.c`:2674 |
| 235 | `mbind` | `unsigned long start, unsigned long len, unsigned long mode, const unsigned long *nmask,…` | `mm/mempolicy.c`:1752 |
| 237 | `set_mempolicy` | `int mode, const unsigned long *nmask, unsigned long maxnode` | `mm/mempolicy.c`:1779 |
| 238 | `migrate_pages` | `pid_t pid, unsigned long maxnode, const unsigned long *old_nodes, const unsigned long *…` | `mm/mempolicy.c`:1872 |
| 239 | `move_pages` | `pid_t pid, unsigned long nr_pages, const void **pages, const int *nodes, int *status, i…` | `mm/migrate.c`:2590 |
| 240 | `rt_tgsigqueueinfo` | `pid_t tgid, pid_t pid, int sig, siginfo_t *uinfo` | `kernel/signal.c`:4251 |
| 258 | `riscv_hwprobe` | `struct riscv_hwprobe *pairs, size_t pair_count, size_t cpusetsize, unsigned long *cpus,…` | `arch/riscv/kernel/sys_hwprobe.c`:511 |
| 259 | `riscv_flush_icache` | `uintptr_t start, uintptr_t end, uintptr_t flags` | `arch/riscv/kernel/sys_riscv.c`:59 |
| 266 | `clock_adjtime` | `const clockid_t which_clock, struct __kernel_timex *utx` | `kernel/time/posix-timers.c`:1165 |
| 270 | `process_vm_readv` | `pid_t pid, const struct iovec *lvec, unsigned long liovcnt, const struct iovec *rvec, u…` | `mm/process_vm_access.c`:292 |
| 271 | `process_vm_writev` | `pid_t pid, const struct iovec *lvec, unsigned long liovcnt, const struct iovec *rvec, u…` | `mm/process_vm_access.c`:299 |
| 273 | `finit_module` | `int fd, const char *uargs, int flags` | `kernel/module/main.c`:3723 |
| 277 | `seccomp` | `unsigned int op, unsigned int flags, void *uargs` | `kernel/seccomp.c`:2110 |
| 281 | `execveat` | `int fd, const char *filename, const char *const *argv, const char *const *envp, int fla…` | `fs/exec.c`:2013 |
| 292 | `io_pgetevents` | `aio_context_t ctx_id, long min_nr, long nr, struct io_event *events, struct __kernel_ti…` | `fs/aio.c`:2276 |
| 293 | `rseq` | `struct rseq *rseq, u32 rseq_len, int flags, u32 sig` | `kernel/rseq.c`:474 |
| 294 | `kexec_file_load` | `int kernel_fd, int initrd_fd, unsigned long cmdline_len, const char *cmdline_ptr, unsig…` | `kernel/kexec_file.c`:363 |
| 427 | `io_uring_register` | `unsigned int fd, unsigned int opcode, void *arg, unsigned int nr_args` | `io_uring/register.c`:906 |
| 429 | `move_mount` | `int from_dfd, const char *from_pathname, int to_dfd, const char *to_pathname, unsigned …` | `fs/namespace.c`:4531 |
| 431 | `fsconfig` | `int fd, unsigned int cmd, const char *_key, const void *_value, int aux` | `fs/fsopen.c`:344 |
| 432 | `fsmount` | `int fs_fd, unsigned int flags, unsigned int attr_flags` | `fs/namespace.c`:4392 |
| 435 | `clone3` | `struct clone_args *uargs, size_t size` | `kernel/fork.c`:2888 |
| 437 | `openat2` | `int dfd, const char *filename, struct open_how *how, size_t usize` | `fs/open.c`:1469 |
| 440 | `process_madvise` | `int pidfd, const struct iovec *vec, size_t vlen, int behavior, unsigned int flags` | `mm/madvise.c`:2057 |
| 442 | `mount_setattr` | `int dfd, const char *path, unsigned int flags, struct mount_attr *uattr, size_t usize` | `fs/namespace.c`:5130 |
| 443 | `quotactl_fd` | `unsigned int fd, unsigned int cmd, qid_t id, void *addr` | `fs/quota/quota.c`:973 |
| 444 | `landlock_create_ruleset` | `const struct landlock_ruleset_attr *const attr, const size_t size, const __u32 flags` | `security/landlock/syscalls.c`:195 |
| 445 | `landlock_add_rule` | `const int ruleset_fd, const enum landlock_rule_type rule_type, const void *const rule_a…` | `security/landlock/syscalls.c`:418 |
| 446 | `landlock_restrict_self` | `const int ruleset_fd, const __u32 flags` | `security/landlock/syscalls.c`:478 |
| 448 | `process_mrelease` | `int pidfd, unsigned int flags` | `mm/oom_kill.c`:1204 |
| 449 | `futex_waitv` | `struct futex_waitv *waiters, unsigned int nr_futexes, unsigned int flags, struct __kern…` | `kernel/futex/syscalls.c`:290 |
| 450 | `set_mempolicy_home_node` | `unsigned long start, unsigned long len, unsigned long home_node, unsigned long flags` | `mm/mempolicy.c`:1685 |
| 451 | `cachestat` | `unsigned int fd, struct cachestat_range *cstat_range, struct cachestat *cstat, unsigned…` | `mm/filemap.c`:4571 |
| 454 | `futex_wake` | `void *uaddr, unsigned long mask, int nr, unsigned int flags` | `kernel/futex/syscalls.c`:338 |
| 455 | `futex_wait` | `void *uaddr, unsigned long val, unsigned long mask, unsigned int flags, struct __kernel…` | `kernel/futex/syscalls.c`:370 |
| 456 | `futex_requeue` | `struct futex_waitv *waiters, unsigned int flags, int nr_wake, int nr_requeue` | `kernel/futex/syscalls.c`:414 |
| 457 | `statmount` | `const struct mnt_id_req *req, struct statmount *buf, size_t bufsize, unsigned int flags` | `fs/namespace.c`:5925 |
| 458 | `listmount` | `const struct mnt_id_req *req, u64 *mnt_ids, size_t nr_mnt_ids, unsigned int flags` | `fs/namespace.c`:6032 |
| 459 | `lsm_get_self_attr` | `unsigned int attr, struct lsm_ctx *ctx, u32 *size, u32 flags` | `security/lsm_syscalls.c`:77 |
| 460 | `lsm_set_self_attr` | `unsigned int attr, struct lsm_ctx *ctx, u32 size, u32 flags` | `security/lsm_syscalls.c`:55 |
| 461 | `lsm_list_modules` | `u64 *ids, u32 *size, u32 flags` | `security/lsm_syscalls.c`:96 |
| 462 | `mseal` | `unsigned long start, size_t len, unsigned long flags` | `mm/mseal.c`:187 |
| 463 | `setxattrat` | `int dfd, const char *pathname, unsigned int at_flags, const char *name, const struct xa…` | `fs/xattr.c`:719 |
| 464 | `getxattrat` | `int dfd, const char *pathname, unsigned int at_flags, const char *name, struct xattr_ar…` | `fs/xattr.c`:863 |
| 465 | `listxattrat` | `int dfd, const char *pathname, unsigned int at_flags, char *list, size_t size` | `fs/xattr.c`:991 |
| 466 | `removexattrat` | `int dfd, const char *pathname, unsigned int at_flags, const char *name` | `fs/xattr.c`:1091 |
| 467 | `open_tree_attr` | `int dfd, const char *filename, unsigned flags, struct mount_attr *uattr, size_t usize` | `fs/namespace.c`:5172 |
| 468 | `file_getattr` | `int dfd, const char *filename, struct file_attr *ufattr, size_t usize, unsigned int at_…` | `fs/file_attr.c`:382 |
| 469 | `file_setattr` | `int dfd, const char *filename, struct file_attr *ufattr, size_t usize, unsigned int at_…` | `fs/file_attr.c`:437 |

#### Local `NR_*` not in Linux RV64 reference

These constants are not present by syscall name in the Linux RV64 v6.17 table. They are usually compatibility aliases, stale cross-arch constants, or local scaffolding and should be justified or removed.

| `NR_*` | Local # |
|---|---:|
| `NR_PSELECT6_TIME64` | 413 |
| `NR_SIGNALFD` | 282 |
| `NR_TX_OBSERVE_BEGIN` | 333 |
| `NR_TX_OBSERVE_TRACE_OFF` | 335 |
| `NR_TX_OBSERVE_TRACE_ON` | 334 |

<!-- END AUTOGEN: syscall-table -->

<!-- BEGIN syscall-auto-table (generated by `cargo xtask syscall sync`) -->

_This section is generated by `cargo xtask syscall sync` from
`crates/tx-shims/src/linux_syscall/{numbers,mod}.rs` and the Linux RV64 v6.17 reference
at `xtask/data/syscalls/riscv/64/rv64/linux-6.17-table.json` (source: https://syscalls.mebeim.net/db/riscv/64/rv64/latest/table.json). Linux file/line references point into `external/linux-rv-6.17`. Do not edit
between the BEGIN/END sentinels by hand — your changes will be
overwritten by the next `sync`. The lint variant
`cargo xtask lint syscall-status` fails on drift or number mismatch._

### Counts (from dispatch table)

- `pub const NR_*` in numbers.rs: **243**
- Linux RV64 reference syscalls: **320**
- dispatched in mod.rs: **243** (of which async: 58, likely-stub: 2)
- defined but not dispatched: **0**

- true missing vs Linux RV64 reference: **82**
- number mismatches vs Linux RV64 reference: **0**
- local `NR_*` not in Linux RV64 reference: **5**

### Likely stubs (2)

Heuristic — body ≤14 non-comment lines mentioning `ENOSYS`/`unimplemented!`/`todo!`. The human-curated `## Already-partial` section above is the authoritative classification; this list highlights candidates for cleanup or for moving into the curated catalog.

- `NR_FANOTIFY_MARK` (263) → `sys_fanotify_mark`
- `NR_RESTART_SYSCALL` (128) → `(inline)`

### True missing from local `numbers.rs` (82)

These are Linux RV64 v6.17 syscalls with no local `NR_*` constant. This is the greenfield backlog; it is distinct from defined-but-not-dispatched.

| Linux # | Name | Signature | Linux source |
|---:|---|---|---|
| 3 | `io_cancel` | `aio_context_t ctx_id, struct iocb *iocb, struct io_event *result` | `fs/aio.c`:2176 |
| 5 | `setxattr` | `const char *pathname, const char *name, const void *value, size_t size, int flags` | `fs/xattr.c`:743 |
| 6 | `lsetxattr` | `const char *pathname, const char *name, const void *value, size_t size, int flags` | `fs/xattr.c`:750 |
| 7 | `fsetxattr` | `int fd, const char *name, const void *value, size_t size, int flags` | `fs/xattr.c`:758 |
| 8 | `getxattr` | `const char *pathname, const char *name, void *value, size_t size` | `fs/xattr.c`:888 |
| 9 | `lgetxattr` | `const char *pathname, const char *name, void *value, size_t size` | `fs/xattr.c`:894 |
| 10 | `fgetxattr` | `int fd, const char *name, void *value, size_t size` | `fs/xattr.c`:901 |
| 11 | `listxattr` | `const char *pathname, char *list, size_t size` | `fs/xattr.c`:998 |
| 12 | `llistxattr` | `const char *pathname, char *list, size_t size` | `fs/xattr.c`:1004 |
| 13 | `flistxattr` | `int fd, char *list, size_t size` | `fs/xattr.c`:1010 |
| 14 | `removexattr` | `const char *pathname, const char *name` | `fs/xattr.c`:1097 |
| 15 | `lremovexattr` | `const char *pathname, const char *name` | `fs/xattr.c`:1103 |
| 16 | `fremovexattr` | `int fd, const char *name` | `fs/xattr.c`:1109 |
| 41 | `pivot_root` | `const char *new_root, const char *put_old` | `fs/namespace.c`:4661 |
| 51 | `chroot` | `const char *filename` | `fs/open.c`:598 |
| 58 | `vhangup` | `` | `fs/open.c`:1606 |
| 60 | `quotactl` | `unsigned int cmd, const char *special, qid_t id, void *addr` | `fs/quota/quota.c`:917 |
| 89 | `acct` | `const char *name` | `kernel/acct.c`:314 |
| 95 | `waitid` | `int which, pid_t upid, struct siginfo *infop, int options, struct rusage *ru` | `kernel/exit.c`:1797 |
| 104 | `kexec_load` | `unsigned long entry, unsigned long nr_segments, struct kexec_segment *segments, unsigned …` | `kernel/kexec.c`:242 |
| 105 | `init_module` | `void *umod, unsigned long len, const char *uargs` | `kernel/module/main.c`:3569 |
| 106 | `delete_module` | `const char *name_user, unsigned int flags` | `kernel/module/main.c`:776 |
| 117 | `ptrace` | `long request, long pid, unsigned long addr, unsigned long data` | `kernel/ptrace.c`:1387 |
| 142 | `reboot` | `int magic1, int magic2, unsigned int cmd, void *arg` | `kernel/reboot.c`:728 |
| 151 | `setfsuid` | `uid_t uid` | `kernel/sys.c`:940 |
| 152 | `setfsgid` | `gid_t gid` | `kernel/sys.c`:984 |
| 162 | `setdomainname` | `char *name, int len` | `kernel/sys.c`:1473 |
| 171 | `adjtimex` | `struct __kernel_timex *txc_p` | `kernel/time/time.c`:269 |
| 179 | `sysinfo` | `struct sysinfo *info` | `kernel/sys.c`:2896 |
| 217 | `add_key` | `const char *_type, const char *_description, const void *_payload, size_t plen, key_seria…` | `security/keys/keyctl.c`:74 |
| 218 | `request_key` | `const char *_type, const char *_description, const char *_callout_info, key_serial_t dest…` | `security/keys/keyctl.c`:167 |
| 219 | `keyctl` | `int option, unsigned long arg2, unsigned long arg3, unsigned long arg4, unsigned long arg5` | `security/keys/keyctl.c`:1874 |
| 224 | `swapon` | `const char *specialfile, int swap_flags` | `mm/swapfile.c`:3259 |
| 225 | `swapoff` | `const char *specialfile` | `mm/swapfile.c`:2674 |
| 235 | `mbind` | `unsigned long start, unsigned long len, unsigned long mode, const unsigned long *nmask, u…` | `mm/mempolicy.c`:1752 |
| 237 | `set_mempolicy` | `int mode, const unsigned long *nmask, unsigned long maxnode` | `mm/mempolicy.c`:1779 |
| 238 | `migrate_pages` | `pid_t pid, unsigned long maxnode, const unsigned long *old_nodes, const unsigned long *ne…` | `mm/mempolicy.c`:1872 |
| 239 | `move_pages` | `pid_t pid, unsigned long nr_pages, const void **pages, const int *nodes, int *status, int…` | `mm/migrate.c`:2590 |
| 240 | `rt_tgsigqueueinfo` | `pid_t tgid, pid_t pid, int sig, siginfo_t *uinfo` | `kernel/signal.c`:4251 |
| 258 | `riscv_hwprobe` | `struct riscv_hwprobe *pairs, size_t pair_count, size_t cpusetsize, unsigned long *cpus, u…` | `arch/riscv/kernel/sys_hwprobe.c`:511 |
| 259 | `riscv_flush_icache` | `uintptr_t start, uintptr_t end, uintptr_t flags` | `arch/riscv/kernel/sys_riscv.c`:59 |
| 266 | `clock_adjtime` | `const clockid_t which_clock, struct __kernel_timex *utx` | `kernel/time/posix-timers.c`:1165 |
| 270 | `process_vm_readv` | `pid_t pid, const struct iovec *lvec, unsigned long liovcnt, const struct iovec *rvec, uns…` | `mm/process_vm_access.c`:292 |
| 271 | `process_vm_writev` | `pid_t pid, const struct iovec *lvec, unsigned long liovcnt, const struct iovec *rvec, uns…` | `mm/process_vm_access.c`:299 |
| 273 | `finit_module` | `int fd, const char *uargs, int flags` | `kernel/module/main.c`:3723 |
| 277 | `seccomp` | `unsigned int op, unsigned int flags, void *uargs` | `kernel/seccomp.c`:2110 |
| 281 | `execveat` | `int fd, const char *filename, const char *const *argv, const char *const *envp, int flags` | `fs/exec.c`:2013 |
| 292 | `io_pgetevents` | `aio_context_t ctx_id, long min_nr, long nr, struct io_event *events, struct __kernel_time…` | `fs/aio.c`:2276 |
| 293 | `rseq` | `struct rseq *rseq, u32 rseq_len, int flags, u32 sig` | `kernel/rseq.c`:474 |
| 294 | `kexec_file_load` | `int kernel_fd, int initrd_fd, unsigned long cmdline_len, const char *cmdline_ptr, unsigne…` | `kernel/kexec_file.c`:363 |
| 427 | `io_uring_register` | `unsigned int fd, unsigned int opcode, void *arg, unsigned int nr_args` | `io_uring/register.c`:906 |
| 429 | `move_mount` | `int from_dfd, const char *from_pathname, int to_dfd, const char *to_pathname, unsigned in…` | `fs/namespace.c`:4531 |
| 431 | `fsconfig` | `int fd, unsigned int cmd, const char *_key, const void *_value, int aux` | `fs/fsopen.c`:344 |
| 432 | `fsmount` | `int fs_fd, unsigned int flags, unsigned int attr_flags` | `fs/namespace.c`:4392 |
| 435 | `clone3` | `struct clone_args *uargs, size_t size` | `kernel/fork.c`:2888 |
| 437 | `openat2` | `int dfd, const char *filename, struct open_how *how, size_t usize` | `fs/open.c`:1469 |
| 440 | `process_madvise` | `int pidfd, const struct iovec *vec, size_t vlen, int behavior, unsigned int flags` | `mm/madvise.c`:2057 |
| 442 | `mount_setattr` | `int dfd, const char *path, unsigned int flags, struct mount_attr *uattr, size_t usize` | `fs/namespace.c`:5130 |
| 443 | `quotactl_fd` | `unsigned int fd, unsigned int cmd, qid_t id, void *addr` | `fs/quota/quota.c`:973 |
| 444 | `landlock_create_ruleset` | `const struct landlock_ruleset_attr *const attr, const size_t size, const __u32 flags` | `security/landlock/syscalls.c`:195 |
| 445 | `landlock_add_rule` | `const int ruleset_fd, const enum landlock_rule_type rule_type, const void *const rule_att…` | `security/landlock/syscalls.c`:418 |
| 446 | `landlock_restrict_self` | `const int ruleset_fd, const __u32 flags` | `security/landlock/syscalls.c`:478 |
| 448 | `process_mrelease` | `int pidfd, unsigned int flags` | `mm/oom_kill.c`:1204 |
| 449 | `futex_waitv` | `struct futex_waitv *waiters, unsigned int nr_futexes, unsigned int flags, struct __kernel…` | `kernel/futex/syscalls.c`:290 |
| 450 | `set_mempolicy_home_node` | `unsigned long start, unsigned long len, unsigned long home_node, unsigned long flags` | `mm/mempolicy.c`:1685 |
| 451 | `cachestat` | `unsigned int fd, struct cachestat_range *cstat_range, struct cachestat *cstat, unsigned i…` | `mm/filemap.c`:4571 |
| 454 | `futex_wake` | `void *uaddr, unsigned long mask, int nr, unsigned int flags` | `kernel/futex/syscalls.c`:338 |
| 455 | `futex_wait` | `void *uaddr, unsigned long val, unsigned long mask, unsigned int flags, struct __kernel_t…` | `kernel/futex/syscalls.c`:370 |
| 456 | `futex_requeue` | `struct futex_waitv *waiters, unsigned int flags, int nr_wake, int nr_requeue` | `kernel/futex/syscalls.c`:414 |
| 457 | `statmount` | `const struct mnt_id_req *req, struct statmount *buf, size_t bufsize, unsigned int flags` | `fs/namespace.c`:5925 |
| 458 | `listmount` | `const struct mnt_id_req *req, u64 *mnt_ids, size_t nr_mnt_ids, unsigned int flags` | `fs/namespace.c`:6032 |
| 459 | `lsm_get_self_attr` | `unsigned int attr, struct lsm_ctx *ctx, u32 *size, u32 flags` | `security/lsm_syscalls.c`:77 |
| 460 | `lsm_set_self_attr` | `unsigned int attr, struct lsm_ctx *ctx, u32 size, u32 flags` | `security/lsm_syscalls.c`:55 |
| 461 | `lsm_list_modules` | `u64 *ids, u32 *size, u32 flags` | `security/lsm_syscalls.c`:96 |
| 462 | `mseal` | `unsigned long start, size_t len, unsigned long flags` | `mm/mseal.c`:187 |
| 463 | `setxattrat` | `int dfd, const char *pathname, unsigned int at_flags, const char *name, const struct xatt…` | `fs/xattr.c`:719 |
| 464 | `getxattrat` | `int dfd, const char *pathname, unsigned int at_flags, const char *name, struct xattr_args…` | `fs/xattr.c`:863 |
| 465 | `listxattrat` | `int dfd, const char *pathname, unsigned int at_flags, char *list, size_t size` | `fs/xattr.c`:991 |
| 466 | `removexattrat` | `int dfd, const char *pathname, unsigned int at_flags, const char *name` | `fs/xattr.c`:1091 |
| 467 | `open_tree_attr` | `int dfd, const char *filename, unsigned flags, struct mount_attr *uattr, size_t usize` | `fs/namespace.c`:5172 |
| 468 | `file_getattr` | `int dfd, const char *filename, struct file_attr *ufattr, size_t usize, unsigned int at_fl…` | `fs/file_attr.c`:382 |
| 469 | `file_setattr` | `int dfd, const char *filename, struct file_attr *ufattr, size_t usize, unsigned int at_fl…` | `fs/file_attr.c`:437 |

### Local `NR_*` not in Linux RV64 reference (5)

These constants do not match a syscall name in the Linux RV64 v6.17 reference. They are usually compatibility aliases, stale cross-arch constants, or local scaffolding and should be justified or removed.

- `NR_PSELECT6_TIME64` (nr=413)
- `NR_SIGNALFD` (nr=282)
- `NR_TX_OBSERVE_BEGIN` (nr=333)
- `NR_TX_OBSERVE_TRACE_OFF` (nr=335)
- `NR_TX_OBSERVE_TRACE_ON` (nr=334)

### Dispatched syscalls (243) — name → handler

Sorted by syscall number. `*` marks `async` handlers; `[stub]` marks bodies the heuristic flagged.

| NR | Name | Handler | Lane |
|---:|---|---|---|
| 0 | `NR_IO_SETUP` | `sys_io_setup` | sync |
| 1 | `NR_IO_DESTROY` | `sys_io_destroy` | sync |
| 2 | `NR_IO_SUBMIT` | `sys_io_submit` | sync |
| 4 | `NR_IO_GETEVENTS` | `sys_io_getevents` | async |
| 17 | `NR_GETCWD` | `sys_getcwd` | sync |
| 19 | `NR_EVENTFD2` | `sys_eventfd2` | sync |
| 20 | `NR_EPOLL_CREATE1` | `sys_epoll_create1` | sync |
| 21 | `NR_EPOLL_CTL` | `sys_epoll_ctl` | sync |
| 22 | `NR_EPOLL_PWAIT` | `sys_epoll_wait` | async |
| 23 | `NR_DUP` | `sys_dup` | sync |
| 24 | `NR_DUP3` | `sys_dup3` | sync |
| 25 | `NR_FCNTL` | `sys_fcntl` | sync |
| 26 | `NR_INOTIFY_INIT1` | `sys_inotify_init1` | sync |
| 27 | `NR_INOTIFY_ADD_WATCH` | `sys_inotify_add_watch` | sync |
| 28 | `NR_INOTIFY_RM_WATCH` | `sys_inotify_rm_watch` | sync |
| 29 | `NR_IOCTL` | `sys_ioctl` | sync |
| 30 | `NR_IOPRIO_SET` | `sys_ioprio_set` | sync |
| 31 | `NR_IOPRIO_GET` | `sys_ioprio_get` | sync |
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
| 47 | `NR_FALLOCATE` | `sys_fallocate` | async |
| 48 | `NR_FACCESSAT` | `sys_faccessat` | sync |
| 49 | `NR_CHDIR` | `sys_chdir` | async |
| 50 | `NR_FCHDIR` | `sys_fchdir` | async |
| 52 | `NR_FCHMOD` | `sys_fchmod` | sync |
| 53 | `NR_FCHMODAT` | `sys_fchmodat` | sync |
| 54 | `NR_FCHOWNAT` | `sys_fchownat` | sync |
| 55 | `NR_FCHOWN` | `sys_fchown` | sync |
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
| 68 | `NR_PWRITE64` | `sys_pwrite64` | async |
| 69 | `NR_PREADV` | `sys_preadv` | async |
| 70 | `NR_PWRITEV` | `sys_pwritev` | async |
| 71 | `NR_SENDFILE64` | `sys_sendfile64` | async |
| 72 | `NR_PSELECT6` | `sys_pselect6` | async |
| 73 | `NR_PPOLL` | `sys_ppoll` | async |
| 74 | `NR_SIGNALFD4` | `sys_signalfd4` | sync |
| 75 | `NR_VMSPLICE` | `sys_vmsplice` | async |
| 76 | `NR_SPLICE` | `sys_splice` | async |
| 77 | `NR_TEE` | `sys_tee` | sync |
| 78 | `NR_READLINKAT` | `sys_readlinkat` | async |
| 79 | `NR_NEWFSTATAT` | `sys_newfstatat` | async |
| 80 | `NR_FSTAT` | `sys_fstat` | sync |
| 81 | `NR_SYNC` | `sys_sync` | sync |
| 82 | `NR_FSYNC` | `sys_fsync` | sync |
| 83 | `NR_FDATASYNC` | `sys_fdatasync` | sync |
| 84 | `NR_SYNC_FILE_RANGE` | `sys_sync_file_range` | sync |
| 85 | `NR_TIMERFD_CREATE` | `sys_timerfd_create` | sync |
| 86 | `NR_TIMERFD_SETTIME` | `sys_timerfd_settime` | sync |
| 87 | `NR_TIMERFD_GETTIME` | `sys_timerfd_gettime` | sync |
| 88 | `NR_UTIMENSAT` | `sys_utimensat` | sync |
| 90 | `NR_CAPGET` | `sys_capget` | sync |
| 91 | `NR_CAPSET` | `sys_capset` | sync |
| 92 | `NR_PERSONALITY` | `sys_personality` | sync |
| 93 | `NR_EXIT` | `(inline)` | sync |
| 94 | `NR_EXIT_GROUP` | `sys_exit_group` | sync |
| 96 | `NR_SET_TID_ADDRESS` | `(inline)` | sync |
| 97 | `NR_UNSHARE` | `sys_unshare` | sync |
| 98 | `NR_FUTEX` | `(inline)` | sync |
| 99 | `NR_SET_ROBUST_LIST` | `sys_set_robust_list` | sync |
| 100 | `NR_GET_ROBUST_LIST` | `sys_get_robust_list` | sync |
| 101 | `NR_NANOSLEEP` | `sys_nanosleep` | async |
| 102 | `NR_GETITIMER` | `sys_getitimer` | sync |
| 103 | `NR_SETITIMER` | `sys_setitimer` | sync |
| 107 | `NR_TIMER_CREATE` | `sys_timer_create` | sync |
| 108 | `NR_TIMER_GETTIME` | `sys_timer_gettime` | sync |
| 109 | `NR_TIMER_GETOVERRUN` | `sys_timer_getoverrun` | sync |
| 110 | `NR_TIMER_SETTIME` | `sys_timer_settime` | sync |
| 111 | `NR_TIMER_DELETE` | `sys_timer_delete` | sync |
| 112 | `NR_CLOCK_SETTIME` | `sys_clock_settime` | sync |
| 113 | `NR_CLOCK_GETTIME` | `(inline)` | sync |
| 114 | `NR_CLOCK_GETRES` | `sys_clock_getres` | sync |
| 115 | `NR_CLOCK_NANOSLEEP` | `sys_clock_nanosleep` | async |
| 116 | `NR_SYSLOG` | `sys_syslog` | sync |
| 118 | `NR_SCHED_SETPARAM` | `sys_sched_setparam` | sync |
| 119 | `NR_SCHED_SETSCHEDULER` | `sys_sched_setscheduler` | sync |
| 120 | `NR_SCHED_GETSCHEDULER` | `sys_sched_getscheduler` | sync |
| 121 | `NR_SCHED_GETPARAM` | `sys_sched_getparam` | sync |
| 122 | `NR_SCHED_SETAFFINITY` | `sys_sched_setaffinity` | sync |
| 123 | `NR_SCHED_GETAFFINITY` | `sys_sched_getaffinity` | sync |
| 124 | `NR_SCHED_YIELD` | `sys_sched_yield` | sync |
| 125 | `NR_SCHED_GET_PRIORITY_MAX` | `sys_sched_get_priority_max` | sync |
| 126 | `NR_SCHED_GET_PRIORITY_MIN` | `sys_sched_get_priority_min` | sync |
| 127 | `NR_SCHED_RR_GET_INTERVAL` | `sys_sched_rr_get_interval` | sync |
| 128 | `NR_RESTART_SYSCALL` | `(inline)` | sync [stub] |
| 129 | `NR_KILL` | `sys_kill` | sync |
| 130 | `NR_TKILL` | `sys_tkill` | sync |
| 131 | `NR_TGKILL` | `sys_tgkill` | sync |
| 132 | `NR_SIGALTSTACK` | `sys_sigaltstack` | sync |
| 133 | `NR_RT_SIGSUSPEND` | `sys_rt_sigsuspend` | async |
| 134 | `NR_RT_SIGACTION` | `sys_rt_sigaction` | sync |
| 135 | `NR_RT_SIGPROCMASK` | `(inline)` | sync |
| 136 | `NR_RT_SIGPENDING` | `sys_rt_sigpending` | sync |
| 137 | `NR_RT_SIGTIMEDWAIT` | `sys_rt_sigtimedwait` | async |
| 138 | `NR_RT_SIGQUEUEINFO` | `sys_rt_sigqueueinfo` | sync |
| 139 | `NR_RT_SIGRETURN` | `sys_rt_sigreturn` | sync |
| 140 | `NR_SETPRIORITY` | `sys_setpriority` | sync |
| 141 | `NR_GETPRIORITY` | `sys_getpriority` | sync |
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
| 158 | `NR_GETGROUPS` | `sys_getgroups` | sync |
| 159 | `NR_SETGROUPS` | `sys_setgroups` | sync |
| 160 | `NR_UNAME` | `sys_uname` | sync |
| 161 | `NR_SETHOSTNAME` | `sys_sethostname` | sync |
| 163 | `NR_GETRLIMIT` | `sys_getrlimit` | sync |
| 164 | `NR_SETRLIMIT` | `sys_setrlimit` | sync |
| 165 | `NR_GETRUSAGE` | `sys_getrusage` | sync |
| 166 | `NR_UMASK` | `sys_umask` | sync |
| 167 | `NR_PRCTL` | `sys_prctl` | sync |
| 168 | `NR_GETCPU` | `sys_getcpu` | sync |
| 169 | `NR_GETTIMEOFDAY` | `(inline)` | sync |
| 170 | `NR_SETTIMEOFDAY` | `sys_settimeofday` | sync |
| 172 | `NR_GETPID` | `sys_getpid` | sync |
| 173 | `NR_GETPPID` | `(inline)` | sync |
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
| 192 | `NR_SEMTIMEDOP` | `sys_semtimedop` | async |
| 193 | `NR_SEMOP` | `sys_semop` | async |
| 194 | `NR_SHMGET` | `sys_shmget` | sync |
| 195 | `NR_SHMCTL` | `sys_shmctl` | sync |
| 196 | `NR_SHMAT` | `sys_shmat` | async |
| 197 | `NR_SHMDT` | `sys_shmdt` | async |
| 198 | `NR_SOCKET` | `sys_socket` | sync |
| 199 | `NR_SOCKETPAIR` | `sys_socketpair` | sync |
| 200 | `NR_BIND` | `sys_bind` | sync |
| 201 | `NR_LISTEN` | `sys_listen` | sync |
| 202 | `NR_ACCEPT` | `sys_accept` | sync |
| 203 | `NR_CONNECT` | `sys_connect` | sync |
| 204 | `NR_GETSOCKNAME` | `sys_getsockname` | sync |
| 205 | `NR_GETPEERNAME` | `sys_getpeername` | sync |
| 206 | `NR_SENDTO` | `sys_sendto` | sync |
| 207 | `NR_RECVFROM` | `sys_recvfrom` | sync |
| 208 | `NR_SETSOCKOPT` | `sys_setsockopt` | sync |
| 209 | `NR_GETSOCKOPT` | `sys_getsockopt` | sync |
| 210 | `NR_SHUTDOWN` | `sys_shutdown` | sync |
| 211 | `NR_SENDMSG` | `sys_sendmsg` | sync |
| 212 | `NR_RECVMSG` | `sys_recvmsg` | sync |
| 213 | `NR_READAHEAD` | `sys_readahead` | sync |
| 214 | `NR_BRK` | `sys_brk` | async |
| 215 | `NR_MUNMAP` | `(inline)` | sync |
| 216 | `NR_MREMAP` | `sys_mremap` | async |
| 220 | `NR_CLONE` | `sys_clone` | sync |
| 221 | `NR_EXECVE` | `sys_execve` | async |
| 222 | `NR_MMAP` | `(inline)` | sync |
| 223 | `NR_FADVISE64_64` | `sys_fadvise64` | sync |
| 226 | `NR_MPROTECT` | `(inline)` | sync |
| 227 | `NR_MSYNC` | `sys_msync` | async |
| 228 | `NR_MLOCK` | `sys_mlock` | async |
| 229 | `NR_MUNLOCK` | `sys_munlock` | async |
| 230 | `NR_MLOCKALL` | `sys_mlockall` | async |
| 231 | `NR_MUNLOCKALL` | `sys_munlockall` | async |
| 232 | `NR_MINCORE` | `sys_mincore` | sync |
| 233 | `NR_MADVISE` | `sys_madvise` | sync |
| 234 | `NR_REMAP_FILE_PAGES` | `sys_remap_file_pages` | sync |
| 236 | `NR_GET_MEMPOLICY` | `sys_get_mempolicy` | sync |
| 241 | `NR_PERF_EVENT_OPEN` | `sys_perf_event_open` | sync |
| 242 | `NR_ACCEPT4` | `sys_accept4` | sync |
| 243 | `NR_RECVMMSG` | `sys_recvmmsg` | sync |
| 260 | `NR_WAIT4` | `sys_wait4` | async |
| 261 | `NR_PRLIMIT64` | `sys_prlimit64` | sync |
| 262 | `NR_FANOTIFY_INIT` | `sys_fanotify_init` | sync |
| 263 | `NR_FANOTIFY_MARK` | `sys_fanotify_mark` | sync [stub] |
| 264 | `NR_NAME_TO_HANDLE_AT` | `sys_name_to_handle_at` | sync |
| 265 | `NR_OPEN_BY_HANDLE_AT` | `sys_open_by_handle_at` | sync |
| 267 | `NR_SYNCFS` | `sys_syncfs` | sync |
| 268 | `NR_SETNS` | `sys_setns` | sync |
| 269 | `NR_SENDMMSG` | `sys_sendmmsg` | sync |
| 272 | `NR_KCMP` | `sys_kcmp` | sync |
| 274 | `NR_SCHED_SETATTR` | `sys_sched_setattr` | sync |
| 275 | `NR_SCHED_GETATTR` | `sys_sched_getattr` | sync |
| 276 | `NR_RENAMEAT2` | `sys_renameat2` | async |
| 278 | `NR_GETRANDOM` | `sys_getrandom` | sync |
| 279 | `NR_MEMFD_CREATE` | `sys_memfd_create` | sync |
| 280 | `NR_BPF` | `sys_bpf` | sync |
| 282 | `NR_SIGNALFD` | `sys_signalfd` | sync |
| 282 | `NR_USERFAULTFD` | `sys_userfaultfd` | sync |
| 283 | `NR_MEMBARRIER` | `sys_membarrier` | sync |
| 284 | `NR_MLOCK2` | `sys_mlock2` | async |
| 285 | `NR_COPY_FILE_RANGE` | `sys_copy_file_range` | async |
| 286 | `NR_PREADV2` | `sys_preadv2` | async |
| 287 | `NR_PWRITEV2` | `sys_pwritev2` | async |
| 291 | `NR_STATX` | `sys_statx` | async |
| 333 | `NR_TX_OBSERVE_BEGIN` | `sys_tx_observe_begin` | sync |
| 334 | `NR_TX_OBSERVE_TRACE_ON` | `sys_tx_observe_trace_on` | sync |
| 335 | `NR_TX_OBSERVE_TRACE_OFF` | `sys_tx_observe_trace_off` | sync |
| 413 | `NR_PSELECT6_TIME64` | `sys_pselect6` | async |
| 424 | `NR_PIDFD_SEND_SIGNAL` | `sys_pidfd_send_signal` | sync |
| 425 | `NR_IO_URING_SETUP` | `sys_io_uring_setup` | sync |
| 426 | `NR_IO_URING_ENTER` | `sys_io_uring_enter` | sync |
| 428 | `NR_OPEN_TREE` | `sys_open_tree` | async |
| 430 | `NR_FSOPEN` | `sys_fsopen` | async |
| 433 | `NR_FSPICK` | `sys_fspick` | async |
| 434 | `NR_PIDFD_OPEN` | `sys_pidfd_open` | sync |
| 436 | `NR_CLOSE_RANGE` | `sys_close_range` | sync |
| 438 | `NR_PIDFD_GETFD` | `sys_pidfd_getfd` | sync |
| 439 | `NR_FACCESSAT2` | `sys_faccessat2` | sync |
| 441 | `NR_EPOLL_PWAIT2` | `sys_epoll_pwait2` | async |
| 447 | `NR_MEMFD_SECRET` | `sys_memfd_secret` | sync |
| 452 | `NR_FCHMODAT2` | `sys_fchmodat` | sync |

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
