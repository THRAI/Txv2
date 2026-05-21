# musl kernel-to-user ABI audit (non-SysV)

## Scope + method

Audited the non-SysV kernel-to-user interfaces that musl can reach through
libc wrappers or raw Linux syscall consumers. Reference source was the pinned
`external/musl` submodule at `5122f9f3c99fee366167c5de98b31546312921ab`,
especially:

- `arch/generic/bits/termios.h`
- `arch/generic/bits/statfs.h`
- `arch/{riscv64,loongarch64}/bits/signal.h`
- `include/sys/{epoll,resource,signalfd,stat,utsname}.h`
- `include/mqueue.h`
- `src/{termios,linux,mq,signal}/`

This pass excluded the SysV IPC structs already covered by the companion
2026-05-21 SysV IPC audit.

## Fixed in this pass

- **TTY `TCGETS` / `TCSETS` termios prefix.** Tx's kernel `Termios` image was
  missing the `c_line` byte between `c_lflag` and `c_cc`, so musl's
  `tcgetattr.c` direct `ioctl(fd, TCGETS, tio)` path observed every control
  character shifted by one byte. `Termios` is now `repr(C)`, has the Linux
  generic kernel prefix `c_line`, and is pinned at 36 bytes. `Winsize` is also
  `repr(C)` and pinned at 8 bytes.
- **`statfs` / `fstatfs` LP64 layout.** Tx was writing `f_namelen` and
  `f_frsize` at raw offsets 88 and 96; musl generic LP64 `struct statfs` puts
  them at offsets 64 and 72 after `fsid_t`. The syscall arm now writes a
  `repr(C)` `StatfsLayout` of 120 bytes.
- **`timerfd_settime` / `timerfd_gettime`.** The syscall arm previously read
  the user's `itimerspec` but discarded it, so every settime effectively
  disarmed the timer. It now copies the real 32-byte LP64 `itimerspec`,
  rejects invalid `tv_nsec` and unknown flags, accepts
  `TFD_TIMER_CANCEL_ON_SET`, returns the old remaining duration, and dispatches
  `timerfd_gettime(fd, curr_value)` with argument 1 as the writeback pointer.
- **`epoll_create1` / `epoll_ctl` / `epoll_pwait`.** Tx had x86_64-like
  epoll syscall numbers, including `epoll_create1 = 291`, which collides with
  `statx` on RV64/LA64 generic Linux. The dispatcher now uses musl's generic
  numbers (`20`, `21`, `22`), wires the syscalls, registers the epoll zone,
  preserves caller-provided `epoll_event.data`, writes the natural LP64
  16-byte event shape (`events` at offset 0, data at offset 8), and reports
  eventfd readiness for nonblocking `epoll_pwait(..., timeout = 0)`.
- **`uname.machine`.** `uname(2)` now reports `riscv64` or `loongarch64`
  from the selected `AuxvIf` platform facts instead of hardcoding `riscv64`.
- **`wait4(..., rusage)`.** musl's `wait3` / `wait4` wrappers may pass a
  `struct rusage *`. Tx now zero-fills Linux's raw 144-byte LP64 rusage
  prefix on successful reap instead of rejecting non-null `rusage` with
  `EINVAL`; musl keeps the public reserved tail in libc-owned memory.
- **`sigaltstack`.** The syscall now copies the LP64 `stack_t` shape
  (`ss_sp`, `ss_flags`, `ss_size`), validates `SS_ONSTACK`, `SS_DISABLE`, and
  `MINSIGSTKSZ`, stores the per-thread alternate-stack registration, and writes
  the previous state through `old_ss`.
- **POSIX message queues.** musl's `mqd_t` is an `int`, so `mq_open` now
  installs an `OpenFileBacking::PosixMq` in the normal fd table and
  `mq_close` works through `close(2)`. The syscall layer wires generic
  `mq_open`, `mq_unlink`, `mq_timedsend`, `mq_timedreceive`,
  `mq_getsetattr`, and `mq_notify`; decodes and writes the 64-byte LP64
  `mq_attr`; preserves `O_CLOEXEC` and `O_NONBLOCK`; maps priorities through
  the SysV backing queue; and accepts `SIGEV_SIGNAL`, `SIGEV_NONE`,
  `SIGEV_THREAD`, and `SIGEV_THREAD_ID` registration shapes.
  Follow-up tightening covers raw `read(2)`/`write(2)` rejection for mq fds,
  `mq_maxmsg` count enforcement, `MQ_PRIO_MAX` validation, POSIX
  highest-priority/FIFO receive order, synchronous epoll `EPOLLIN`/`EPOLLOUT`
  readiness, and blocking null-timeout send/receive waits over the existing
  SysV queue wait sources. `SIGEV_SIGNAL` and `SIGEV_THREAD_ID` `mq_notify`
  registrations are queue-wide and deliver one-shot signals on the
  empty-to-nonempty send edge when no blocked receiver was woken to consume the
  message; unsupported `SIGEV_THREAD` is rejected rather than recorded as a
  no-op.

## Currently musl-compatible enough

- **Initial exec stack / auxv.** The current stack builder emits the musl
  critical auxv entries: `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`,
  `AT_BASE`, `AT_ENTRY`, uid/gid/security facts, `AT_RANDOM`, `AT_HWCAP`,
  `AT_HWCAP2`, `AT_PLATFORM`, `AT_CLKTCK`, `AT_SYSINFO_EHDR`, `AT_EXECFN`,
  `AT_FLAGS`, and `AT_NULL`. `AT_PLATFORM`, `AT_SYSINFO_EHDR`, and `AT_EXECFN`
  may carry zero when their backing string/vDSO is absent; musl tolerates that.
- **Core LP64 structs.** `stat`, `statx`, `linux_dirent64`, `timespec`,
  `timeval`, `tms`, `iovec`, `pollfd`, `rlimit`, `utsname`, `signalfd_siginfo`
  size, `eventfd`'s u64 read/write payload, and userfaultfd's current UAPI
  handshakes match the expected byte shapes for RV64/LA64 generic Linux ABIs.
- **Thread basics used by musl.** `clone` accepts the pthread-style flag set
  currently needed by the in-tree pthread work (`CLONE_VM`, `CLONE_THREAD`,
  `CLONE_SIGHAND`, `CLONE_SETTLS`, `CLONE_CHILD_CLEARTID`,
  `CLONE_PARENT_SETTID`, `CLONE_FILES`, `CLONE_FS`, `CLONE_SYSVSEM`) and
  `futex` covers the wait/wake shape musl uses for basic synchronization.
- **POSIX mq basic wrappers.** musl `mq_open`, `mq_close`, `mq_send`,
  `mq_receive`, `mq_setattr`, `mq_getattr`, `mq_unlink`, and simple
  `mq_notify` paths have fd/lp64 shape coverage, priority ordering, count
  limits, zero-timeout epoll readiness, and blocking null-timeout send/receive
  waits. `SIGEV_SIGNAL`/`SIGEV_THREAD_ID` notification delivery is wired
  through the signal mailbox path, with blocked-receiver suppression pinned.
- **Kernel/user layout redlight.** `cargo xtask lint kernel-user-layouts`
  dumps the Rust `KernelToUserLayout` descriptors and candidate registry,
  compiles C probes against pinned musl RV64/LA64 headers, and compares
  checked/prefix/manual size, alignment, and field offsets. The registry now
  accounts for the current musl-facing kernel/user struct surface with
  checked, prefix, manual, deferred, or excluded status and a reason for every
  non-enforced candidate.

## Remaining gaps

- **epoll is still partial.** The musl generic `epoll_create1` / `epoll_ctl` /
  `epoll_pwait` path is live for eventfd-style nonblocking polls, including
  preserved `epoll_event.data`, `EEXIST` / `ENOENT` registration errors, and
  `DEL` with a null event pointer. Full Linux blocking wait semantics and
  readiness scans for every fd kind are still deferred; timerfd/signalfd
  source IDs are recorded, but the synchronous syscall shim only reports the
  ready subset it can inspect immediately.
- **POSIX message queues still lack timeout expiry and full `SIGEV_THREAD`
  notification delivery.**
  Basic musl wrappers no longer hit `ENOSYS`, fd shape/readiness is covered,
  receive order follows POSIX priority rules, and null-timeout blocking
  send/receive waits park on queue wait sources. Queue-wide
  `SIGEV_SIGNAL`/`SIGEV_THREAD_ID` `mq_notify` delivers one-shot signals on the
  empty-to-nonempty edge when no receiver was woken. Absolute timeout expiry is
  still deferred, permission/name semantics are minimal, `SIGEV_THREAD`
  callback delivery is unsupported until the socket/netlink surface exists, and
  queue namespace lifetime is still day-1.
- **Raw Linux AIO and io_uring intentionally diverge.** The current `io_setup`
  family is fd-shaped rather than Linux's `aio_context_t *` ABI, and
  `io_uring_setup` returns an fd without populating `io_uring_params` or
  user-mmappable rings. musl's POSIX `aio_*` implementation is thread-based
  and does not require raw Linux AIO, but raw-syscall or liburing-style users
  are incompatible.
- **Signal delivery remains semantically incomplete.** `sigaltstack` now
  registers/query-copies the stack record, but `SA_ONSTACK` signal frame
  placement is still deferred. `rt_sigtimedwait` and `signalfd` provide
  128-byte Linux-shaped records, but only a subset of siginfo fields are
  populated today.
- **Userfaultfd is UAPI-shaped but not a complete Linux userfaultfd.** The
  ioctl/read records are byte-compatible for the implemented subset, but the
  full fault path and all modes/ioctls are not complete.
- **Accounting and timing semantics are skeletal.** `wait4` zeroes the raw
  rusage prefix, not accounted values. CPU-time clocks alias to monotonic
  time. `nanosleep` does not yet report a remaining interval on interruption
  because full signal-interrupt paths are still in progress.
- **LA64 `sigaltstack` minimum differs from musl's header.** musl declares
  `MINSIGSTKSZ` as 4096 for LoongArch64 and 2048 for RV64. Tx currently uses
  the Linux generic 2048 constant in the syscall layer. musl pre-validates
  `sigaltstack()` with its own `sysconf(_SC_MINSIGSTKSZ)` before issuing the
  syscall, so normal musl callers remain safe, but raw LA64 callers may see a
  looser kernel check.

## Verification

- `cargo test -p tx-shims --lib linux_syscall::tests::timerfd_dispatch -- --nocapture`
- `cargo test -p tx-shims --lib linux_syscall::tests::sigaltstack_dispatch -- --nocapture`
- `cargo test -p tx-shims --lib linux_syscall::tests::fork_clone_wait4_wave3::dispatch_wait4_rusage_nonzero_writes_zeroed_rusage -- --nocapture`
- `cargo test -p tx-shims --lib linux_syscall::tests::fcntl_misc::dispatch_uname -- --nocapture`
- `cargo test -p tx-shims --lib linux_syscall::tests::ioctl_dispatch::dispatch_ioctl_tcgets_writes_linux_kernel_termios_layout -- --nocapture`
- `cargo test -p tx-shims --lib linux_syscall::tests::stat_family::dispatch_statfs -- --nocapture`
- `cargo test -p tx-subsystems --lib timerfd::tests -- --nocapture`
- `cargo test -p tx-shims linux_syscall::tests::epoll_dispatch -- --nocapture`
- `cargo test -p tx-shims linux_syscall::tests::mq_dispatch -- --nocapture`
- `cargo test -p tx-shims --lib linux_syscall::tests::kernel_user_layouts -- --nocapture`
- `python3 tools/check-kernel-user-layouts.py`
- `cargo xtask lint kernel-user-layouts`
- `cargo check -p tx-shims -p tx-subsystems`
- `cargo -q xtask unit`
- `cargo xtask progress validate`
- `cargo xtask lint docs`
- `git diff --check`

Broader workspace validation is recorded in `docs/progress/STATUS.md`.
