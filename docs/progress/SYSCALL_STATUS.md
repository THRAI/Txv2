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

**Last refresh:** 2026-05-18 (recount after PR #30 + PR #31 —
all 32 basic-musl OSComp tests passing on rv64-qemu; `rt_sigreturn`
wired, dispatched arms grew 96 → 109).

## Headline counts

- `pub const NR_*` defined in `numbers.rs`: **119**
- Dispatched in `mod.rs` (per match arms): **109**
- Currently stubbed (returns `-ENOSYS`): see "Already-partial" below.
- Outright unwired (no `NR_*` definition, no arm): **~116** syscalls — see
  categorized list below.

The numbers above were recounted on the last refresh. When you change the
table, recount with:

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
| `preadv` / `pwritev` / `fallocate` / `readahead` | +20 | S–M (3–4w) | `writev` loop + VFS hooks exist |
| `getrlimit` / `setrlimit` + `sched_getaffinity` | +15 | S (~2w) | per-proc resource field |
| POSIX `mq_*` | +15 | M (3–4w) | single kernel queue object, no namespace work |
| per-process timers (`timer_create` family, `setitimer`, `getrusage`) | +20 | M (2–3w) | deadline tracking shared with `timerfd` |
| `sendfile` / `splice` / `copy_file_range` | +15 | M (3–4w) | needs page-cache coherence path |
| `utimensat` + `/proc` skeleton for busybox `df`/`ps`/`free` | +10 (busybox-musl) | M (~3w) | busybox-musl pass-rate blocker per STATUS 2026-05-18 |
| SysV IPC (`msg` / `sem` / `shm`) | +40 | L (6–10w) | new namespace-aware subsystem |
| Network stack (full socket API) | +60 | XL (12–16w) | no subsystem exists — TCP/UDP state, sockaddr unions, sk_buff |
| `inotify` / `fanotify` | +10 | M (~3w) | new event queue subsystem |
| `chroot` / `pivot_root` / `swap*` | +8 | S–M (1–2w) | `chroot` is one field; bdev-fs lands swap |
| `seccomp` / capabilities / `keyctl` | +15 | L (6–8w) | filter bytecode + keyring |
| `ptrace` | +20 | XL (12w+) | parallel exec context — out of scope v1 |
| `bpf` / `perf_event_open` | +10 | XL (12w+) | out of scope v1 |

**Reading the table.** Best LTP-impact-per-effort today:
`preadv`/`pwritev`/`fallocate`, then `getrlimit`/`setrlimit`/`sched_getaffinity`,
then `utimensat` + minimal `/proc` (closes the busybox-musl gap).
SysV IPC and the network stack are larger but unlock the biggest LTP
coverage jumps. `ptrace` and `bpf` are explicitly **out of scope for v1**
— flag them and move on unless the user specifically chartered them.

**Recently landed (removed from this table):** `fork` + CLOEXEC bitmap
(turned out to be already wired and tested when audited 2026-05-18 —
unblocked 32/32 basic-musl after orthogonal signal-frame / mount / dirfd
fixes in PR #30 + #31).

## OSComp + LTP coverage (the gold standard)

The skill treats OSComp and LTP as the canonical correctness bar. Track
both here so the project knows what real-world tests pass on top of the
internal sanity checks.

### OSComp

Harness: `external/oscomp-autotest/` (git submodule). Runner:
`cargo xtask oscomp qemu --target rv64-qemu`.

**basic-musl: 32/32 PASSING (as of 2026-05-18, PR #30 + PR #31).** Full
suite reaches `END test_*` markers within a single QEMU run:

`brk`, `chdir`, `clone`, `close`, `dup`, `dup2`, `execve`, `exit`, `fork`,
`fstat`, `getcwd`, `getdents`, `getpid`, `getppid`, `gettimeofday`,
`mkdir_`, `mmap`, `mount`, `munmap`, `open`, `openat`, `pipe`, `read`,
`sleep`, `times`, `umount`, `uname`, `unlink`, `wait`, `waitpid`,
`write`, `yield`.

(`test_echo` is the fixture invoked by `test_execve`, not counted as a
separate test.)

**busybox-musl: partial.** STATUS.md 2026-05-18 notes:
- Passes most invocations; `userspace:exited:0` reached.
- Known failures: `df`, `dmesg`, `ps`, `free`, `touch` — root cause is
  the missing `/proc` skeleton + unwired `utimensat` (tracked in the
  high-stakes table above).

**libc-bench, lmbench, cyclictest, iperf, netperf, libctest, ltp, iozone, lua:**
not yet run end-to-end against this kernel. Track per-suite passes as
they land.

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
