# Dispatch-lane classification — complete 89-syscall table

Per `docs/Txv3/04_SYSCALL_SHAPE_v1.md §6`. One syscall missing from the 90
dispatch arms is an unresolvable name collision (NR_FCHDIR is an ENOSYS stub).

## Lane 1: ImmediateSyscall — 19 syscalls

Pure ABI queries. No `StepOp`, no `drive`, no yield.
`call()` takes `&ImmediateCtx` (narrower than `SyscallCtx` — no VFS/VM/reactor
handles).

| NR | # | Function | Notes |
|----|----|----------|-------|
| NR_GETPID | 172 | getpid | `process.pid` |
| NR_GETPPID | 173 | getppid | `process.parent_pid()` |
| NR_GETPGRP | 81 | getpgrp | `process.pgrp_cap()` |
| NR_GETPGID | 155 | getpgid | self-only day-1 |
| NR_GETSID | 156 | getsid | self-only day-1 |
| NR_SET_TID_ADDRESS | 96 | set_tid_address | returns caller tid (promoted from Lane 2) |
| NR_SET_ROBUST_LIST | 99 | set_robust_list | returns 0 (promoted from Lane 2) |
| NR_GETUID | 174 | getuid | `cred().uid` |
| NR_GETEUID | 175 | geteuid | `cred().euid` |
| NR_GETGID | 176 | getgid | `cred().gid` |
| NR_GETEGID | 177 | getegid | `cred().egid` |
| NR_GETRESUID | 148 | getresuid | writes 3× u32 to user |
| NR_GETRESGID | 150 | getresgid | writes 3× u32 to user |
| NR_TIMES | 153 | times | writes tms to user |
| NR_GETTIMEOFDAY | 169 | gettimeofday | writes timeval |
| NR_UMASK | 166 | umask | swap_umask |
| NR_UNAME | 160 | uname | writes utsname |
| NR_PRLIMIT64 | 261 | prlimit64 | rlimit read |
| NR_RT_SIGRETURN | 139 | rt_sigreturn | context restore |

## Lane 2: OneShotStepOp — 43 syscalls

Semantic transitions with observe→commit→publish but never yield.
`drive_oneshot(&mut op, &mut script_ctx)`. Requires `StepOp<I, Progress = NoProgress>`
+ `OneShotStepOp<I>`.

| NR | # | Function | StepOp (if exists) |
|----|----|----------|-------------------|
| NR_SETUID | 146 | setuid | SetuidOp ✓ |
| NR_SETGID | 144 | setgid | SetgidOp ✓ |
| NR_SETREUID | 145 | setreuid | SetreuidOp ✓ |
| NR_SETREGID | 143 | setregid | SetregidOp ✓ |
| NR_SETRESUID | 147 | setresuid | SetresuidOp ✓ |
| NR_SETRESGID | 149 | setresgid | SetresgidOp ✓ |
| NR_SETSID | 157 | setsid | SetsidOp ✓ |
| NR_SETPGID | 154 | setpgid | SetpgidOp ✓ |
| NR_RT_SIGACTION | 134 | sigaction | — |
| NR_RT_SIGPROCMASK | 135 | sigprocmask | — |
| NR_KILL | 129 | kill | — |
| NR_TKILL | 130 | tkill | — |
| NR_TGKILL | 131 | tgkill | — |
| NR_EXIT | 93 | exit | ExitGroupOp ✓ |
| NR_EXIT_GROUP | 94 | exit_group | ExitGroupOp ✓ |
| NR_CLOSE | 57 | close | — |
| NR_DUP | 23 | dup | — |
| NR_DUP3 | 24 | dup3 | — |
| NR_FCNTL | 25 | fcntl | — |
| NR_PIPE2 | 59 | pipe2 | — |
| NR_CHDIR | 49 | chdir | ChdirOp ✓ |
| NR_MKDIRAT | 34 | mkdirat | — |
| NR_UNLINKAT | 35 | unlinkat | — |
| NR_SYMLINKAT | 36 | symlinkat | — |
| NR_LINKAT | 37 | linkat | — |
| NR_RENAMEAT2 | 276 | renameat2 | — |
| NR_TRUNCATE | 45 | truncate | — |
| NR_FTRUNCATE | 46 | ftruncate | — |
| NR_FCHMODAT | 53 | fchmodat | — |
| NR_FCHOWNAT | 54 | fchownat | — |
| NR_UTIMENSAT | 88 | utimensat | — |
| NR_FACCESSAT | 48 | faccessat | — |
| NR_FACCESSAT2 | 439 | faccessat2 | — |
| NR_NEWFSTATAT | 79 | newfstatat | — |
| NR_FSTAT | 80 | fstat | — |
| NR_STATX | 291 | statx | — |
| NR_IOCTL | 29 | ioctl (sync variants) | OpenFileIoctlOp ✓ |
| NR_LSEEK | 62 | lseek | OpenFileLseekOp ✓ |
| NR_READLINKAT | 78 | readlinkat | — |
| NR_MADVISE | 233 | madvise | — |
| NR_SIGNALFD4 | 74 | signalfd4 (create) | — |

*= StepOp wrapper exists; migration is add `OneShotStepOp` marker + update call site*

## Lane 3: Full async drive — 29 syscalls

May yield via VFS/VM/timer/wait-source. `drive(op, &mut script_ctx, mode).await`.

| NR | # | Function | Yield source |
|----|----|----------|-------------|
| NR_READ | 63 | read | pipe/socket/tty/disk — OnWaitSource |
| NR_WRITE | 64 | write | same |
| NR_READV | 65 | readv | iovec cursor — OnWaitSource |
| NR_WRITEV | 66 | writev | same |
| NR_OPENAT | 56 | openat | path walk — OnWaitSource |
| NR_PPOLL | 73 | ppoll | poll — OnWaitSource |
| NR_GETDENTS64 | 61 | getdents64 | dir read — OnWaitSource |
| NR_FUTEX | 98 | futex (wait+wake) | park — OnWaitSource |
| NR_NANOSLEEP | 101 | nanosleep | timer — OnTimer |
| NR_CLOCK_NANOSLEEP | 115 | clock_nanosleep | timer — OnTimer |
| NR_CLOCK_GETTIME | 113 | clock_gettime | some clocks sleep |
| NR_BRK | 214 | brk | VM materialization |
| NR_MMAP | 222 | mmap | VM materialization |
| NR_MUNMAP | 215 | munmap | VM shootdown |
| NR_MPROTECT | 226 | mprotect | VM materialization |
| NR_MREMAP | 216 | mremap | VM materialization |
| NR_MSYNC | 227 | msync | writeback wait |
| NR_CLONE | 220 | clone/fork | process creation |
| NR_EXECVE | 221 | execve | ELF load + image switch |
| NR_WAIT4 | 260 | wait4 | block on child exit |
| NR_IO_SETUP | 206 | io_setup | AIO ctx creation |
| NR_IO_SUBMIT | 209 | io_submit | AIO submission |
| NR_IO_GETEVENTS | 208 | io_getevents | block on completion |
| NR_IO_DESTROY | 207 | io_destroy | AIO ctx teardown |
| NR_IO_URING_SETUP | 425 | io_uring_setup | uring ctx creation |
| NR_IO_URING_ENTER | 426 | io_uring_enter | (ENOSYS pending) |
| NR_USERFAULTFD | 282 | userfaultfd | OnAgent delegate |
| NR_GETRANDOM | 278 | getrandom | entropy block |
| NR_FCHDIR | 50 | fchdir | (ENOSYS stub) |

## Judgment Rules

To classify a syscall:

1. **Does the call chain ever yield?** If no and it only reads kernel state →
   Lane 1 (ImmediateSyscall).
2. **Does the call involve a semantic mutation (observe→commit→publish) that
   never blocks?** → Lane 2 (OneShotStepOp).
3. **Does the call access VFS path resolution, VM materialization, timer
   sleep, or wait-source registration?** → Lane 3 (Full async drive).

Edge cases:
- `umask` is Lane 1 (immediate): it writes a per-process field and returns the
  old value — no StepOp needed.
- `futex(FUTEX_WAKE)` is Lane 2: it's a one-shot mutation. Only
  `FUTEX_WAIT` needs full async (it parks). The current dispatch doesn't split
  these — the `NR_FUTEX` arm dispatches to a composite handler.
- `gettimeofday` is Lane 1: wall clock read via timer HAL — no yield.
- `clock_gettime` is Lane 3: some clock sources may sleep.
