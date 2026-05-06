---
date: 2026-05-06
topic: "fork / clone / wait4 scaffolding (scoping research for the post-ELF slice)"
status: complete
prior:
  - docs/progress/decisions/2026-05-06-elf-loader-and-execve.md
  - docs/progress/research/2026-05-06-musl-ltp-execve-coverage.md
  - docs/progress/plans/2026-05-06-elf-loader-and-execve.md
  - docs/progress/plans/2026-04-29-kernel-main-long-term-checklist.json
---

# fork / clone / wait4 scaffolding research

This note scopes the slice that lands the Linux `clone(2)` /
`wait4(2)` / `waitid(2)` syscall arms on top of the trio +
pre-ELF + ELF-loader scaffolding. The ELF-loader decision note's
follow-up list places this immediately after CSPRNG; it is the
biggest LTP-coverage unlock remaining (LTP `execve05` concurrent-
exec stress, fork+execve / wait+execve / setpgid+execve chains,
and the entire LTP `fork`/`clone`/`wait4`/`wait`/`waitpid`/
`waitid` directories, ~46 tests).

## Spec summary (PROCESS_v1, THREAD_RUNTIME_v1)

`PROCESS_v1` collapses Linux `fork(2)` / `vfork(2)` / `clone(2)`
/ `clone3(2)` into a single underlying mechanism with two paths:
**`step_clone_process`** (the `!CLONE_THREAD` path — new process,
new pid, new ProcessIdentity per
`txdoc:PROCESS-STEP-CLONE-PROCESS-NEW-PROCESS-PATH-CLONE-THREAD-1`)
and **`step_clone_thread`** (the `CLONE_THREAD` path — same
process, new thread, shared payload per
`txdoc:PROCESS-STEP-CLONE-THREAD-CLONE-THREAD-PATH-1`).
`PROCESS_v1` §7.1 anchors `txdoc:PROCESS-FORK-AND-CLONE-1` and
`txdoc:PROCESS-CLONE-FLAG-SUPPORT-V1-1` (the v1-supported flag
table). Plain `fork(2)` resolves to `clone(SIGCHLD, 0)` (no
CLONE_* bits, SIGCHLD as the parent's termination signal); the
parent-child binding is materialised on `ProcessPayload.parent`
(weak) and `ProcessPayload.children` (strong, retained until
reap), with the child inheriting the parent's pgrp and session.

The **wait family** (`txdoc:PROCESS-WAIT-FAMILY-1`,
`PROCESS_v1` §7.4) walks `parent.children` for a zombie matching
the selector, reaps it (withdraws the strong `Cap` from
`parent.children` and from the child's `pgrp.members`), and
returns `(pid, exit_status)`. SIGCHLD delivery to the parent is
the §7.3.3 phase-5 cascade — posted by `step_process_exit` after
the exiting thread group's payload is dropped and exit_status
written so the parent observes a complete zombie when it reacts.
The fd-table inheritance contract is "snapshot the parent's
table, share each `Cap<OpenFile>` slot" for plain fork (no
CLONE_FILES sharing); signal dispositions are likewise copied
(no CLONE_SIGHAND sharing). `THREAD_RUNTIME_v1` §"Thread exit
notification (CLONE_CHILD_CLEARTID / set_tid_address)" pins the
thread-side hooks (`set_tid_address(2)`, FUTEX_WAKE on
clear_child_tid at thread exit) used by `pthread_create` /
`pthread_join`.

## Existing scaffolding (trio + pre-ELF + ELF loader)

Most of the surface for plain fork already exists. The slice is
mostly syscall-driver wiring and a small handful of fixups.

- **`step_fork::<P: PmapIf>(parent: &Cap<ProcessIdentity>) ->
  Result<Cap<ProcessIdentity>, ForkError>`** at
  `crates/tx-subsystems/src/process/execution.rs:248`. Already
  does end-to-end fork: snapshots aspace/cred/cwd/fds/cloexec/brk
  under the parent's payload lock; calls
  `AddressSpace::fork_aspace::<P>(&parent_aspace)`; signs the
  child `AddressSpace` `Cap`; allocates a new pid via
  `allocate_pid()` (`structure.rs:791`, monotonic
  `AtomicU32::fetch_add`); signs `ProcessIdentity` with
  `Some(parent.downgrade())`; signs leader `ThreadIdentity`;
  signs payload with cloned fd table + cloexec word + brk pair;
  registers the child in `parent_pgrp.members` and
  `parent.children`. The leader thread's `saved_user_context`
  starts as `None` (default — see Gap below).
- **`AddressSpace::fork_aspace::<P>(parent: &AddressSpace)`** at
  `crates/tx-subsystems/src/vm/execution.rs:87`. Clones recipes
  under an `ExclusiveWriter` reservation on `UserRange::full_user_v1()`
  (`txdoc:VM-FORK-ASPACE`, VM_v1_2 §9.5); for each recipe,
  commits a fresh `recipes.commit_map` on the child aspace and,
  when the entry is private, tears down the parent's pmap range
  so subsequent writes by either side fault into per-side anon
  pages. **No COW yet** — the child shares recipes and re-faults
  on demand; private-anon writes in the parent post-fork will
  re-fault through the parent's recipes. This is correct for the
  slice's posix-`fork` semantics (each side gets independent
  copy-on-fault behaviour); not COW-optimal, but POSIX-correct.
- **`ProcessPayload.parent: SpinMutex<Option<Weak<ProcessIdentity>>>`**
  + **`children: SpinMutex<Vec<Cap<ProcessIdentity>>>`** at
  `crates/tx-subsystems/src/process/structure.rs:123`/`:138`. The
  upward-and-downward bindings PROCESS_v1 §2.1 demands; populated
  by `step_fork` and walked by `step_waitpid_nohang` and
  `sever_children`. `parent_cap()` (`structure.rs:160`) is the
  upward snapshot used by `getppid` and SIGCHLD posting; `parent_pid()`
  (`structure.rs:170`) renders it as a `Pid`.
- **`step_waitpid_nohang(parent, target: WaitTarget) ->
  Result<(Pid, ExitStatus), WaitError>`** at
  `crates/tx-subsystems/src/process/execution.rs:523`.
  Implements the WNOHANG-style poll: walks `parent.children`,
  resolves `WaitTarget::CallerPgrp` to a concrete `Pgid` via
  `parent.pgrp_cap()`, returns `(NoChildren | NoneReady |
  matching zombie reaped)`. **Selectors covered**: `Any`,
  `Pid(p)`, `Pgrp(g)`, `CallerPgrp` — full POSIX `pid_t` arg
  surface. **Flags NOT supported**: blocking variant, `WUNTRACED`,
  `WCONTINUED`, `WSTOPPED`, `__WALL`, `__WCLONE`. **Return shape**:
  `(Pid, ExitStatus)`; `ExitStatus::wait_status_word()` on
  `structure.rs:70` already produces the Linux status-word
  encoding (`128 + sig` for `Signaled`, raw int for `Exited`)
  the syscall driver returns to userspace.
- **`step_exit_group(process, status)`** at
  `crates/tx-subsystems/src/process/execution.rs:335` and
  **`step_process_exit`** at `:367`: both call
  `post_sigchld_to_parent` (`:487`) which calls
  `signal::step_kill_process(&parent, Signum::SIGCHLD)` — so
  SIGCHLD is **already posted to the parent at zombification**
  via `crates/tx-subsystems/src/signal.rs:547`'s
  `step_kill_process` → `post_signal` path into the parent's
  leader thread `thread_pending` queue. `siginfo` (CLD_EXITED /
  CLD_KILLED, si_pid, si_status) is **not** wired — the comment
  at `execution.rs:483` flags this as deferred until a SigInfo
  carrier lands.
- **`SigActionTable`** at `crates/tx-subsystems/src/signal.rs:192`.
  Held by-value on `ProcessPayload.sig_actions`
  (`structure.rs:462`). **Not behind `Arc` / `Shared<T>`**: today
  fork copies dispositions by re-signing the payload (the
  default-constructed `SigActionTable::default()` is what the
  `sign_process_payload` path produces). **`CLONE_SIGHAND`
  sharing infrastructure does not exist** — `Shared<SigActionTable>`
  per PROCESS_v1 §3 §"Frame inline slots" is unimplemented.
  `step_reset_for_exec` (the exec slice's exec-time reset) lives
  on `SigActionTable` at `signal.rs:226`; fork-time copy is
  implicit in payload re-construction.
- **`ProcessPayload.fds: SpinMutex<[Option<Cap<OpenFile>>; FD_TABLE_SIZE]>`**
  + **`fd_cloexec: AtomicU32`** at `structure.rs:499`/`:516`.
  `snapshot_fds()` (`structure.rs:614`) and `fd_cloexec_word()`
  (`structure.rs:647`) are the fork-side primitives; both
  already invoked by `step_fork`. Per-slot `Cap<OpenFile>` is
  cloned (parent and child share the same `OpenFile` per slot —
  POSIX shared file-offset semantics); the array itself is
  copied. **`CLONE_FILES` sharing infrastructure does not exist**
  — there is no `Shared<FdTable>` / `Arc<FdTable>` indirection;
  the fd table is by-value on the payload.
- **`ThreadPayload.saved_user_context: SpinMutex<Option<UserTrapContext>>`**
  at `crates/tx-subsystems/src/thread_runtime/structure.rs:133`,
  with `store_saved_user_context` at `:192`. The slot exec
  populates at PoNR (entry, sp, zero gprs); fork's leader thread
  is signed via `sign_thread` (`process/execution.rs:294`) which
  produces a default `ThreadPayload` with `saved_user_context = None`.
  The **child thread's snapshot is not seeded by `step_fork` today**
  — the syscall driver must do the parent-snapshot copy + `regs[10] = 0`
  (RV64 a0) write before the child is scheduled (see Gaps
  below).
- **Linux syscall numbers** at `crates/tx-shims/src/linux_syscall/numbers.rs`:
  `NR_WRITE = 64`, `NR_READ = 63`, `NR_EXIT = 93`, `NR_EXIT_GROUP = 94`,
  `NR_GETPID = 172`, `NR_BRK = 214`, `NR_RT_SIGACTION = 134`,
  `NR_RT_SIGPROCMASK = 135`, `NR_FCNTL = 25`, `NR_EXECVE = 221`.
  **Missing**: `NR_CLONE = 220`, `NR_WAIT4 = 260`, `NR_WAITID = 95`,
  `NR_GETPPID = 173`, `NR_GETPGID = 155`, `NR_SETPGID = 154`,
  `NR_GETPGRP = (deprecated, glibc-only)`, `NR_SETSID = 157`,
  `NR_SET_TID_ADDRESS = 96`, `NR_SET_ROBUST_LIST = 99` (RV64
  generic ABI numbers).
- **`step_setpgid(target, new_pgid)`** at
  `process/execution.rs:636` already exists (Day-1 limited to
  `new_pgid == target.pid`); pgrp/session bindings survive fork
  by construction (the child inherits `parent_pgrp` at
  `execution.rs:279`).

## Gaps the slice has to build

1. **`NR_CLONE = 220` syscall arm.** Linux `clone(flags, stack,
   parent_tidptr, tls, child_tidptr)` — note the **RV64 generic
   ABI argument order** is `(flags, stack, parent_tidptr, tls,
   child_tidptr)` (matches arm64; differs from x86_64). The arm
   branches on the low-byte termination signal vs CLONE_THREAD.
   For the MVP `clone(SIGCHLD, 0, …)` case (musl fork; see
   `_Fork.c:35`), forwards to `step_fork::<P>(parent)` after
   asserting no unsupported flags. Returns the child pid in the
   parent and `0` in the child (see (4)). **Linux RV64 has no
   `NR_FORK` and no `NR_VFORK`** — both routes are clone-only.
2. **`NR_WAIT4 = 260` syscall arm.** Linux `wait4(pid, wstatus,
   options, rusage)`. Translate the signed `pid` to a
   `WaitTarget` (`pid > 0` → `Pid`, `pid == 0` → `CallerPgrp`,
   `pid == -1` → `Any`, `pid < -1` → `Pgrp(-pid)` — already
   pinned in `WaitTarget`'s comment at `process/execution.rs:124`).
   Translate `options` (`WNOHANG = 1`, `WUNTRACED = 2`,
   `WCONTINUED = 8`); reject anything but `WNOHANG` until a
   blocking variant lands. Write the status word to `wstatus`
   (8-byte user pointer; `read_user_cstr`-style validation
   needed). `rusage` may be `NULL`; if non-NULL, write a
   zero-filled `struct rusage` for v1 (LTP doesn't check).
   Map `WaitError::NoChildren` → `-ECHILD`, `WaitError::NoneReady`
   → `0` (POSIX WNOHANG return).
3. **`NR_WAITID = 95` syscall arm.** Linux `waitid(idtype, id,
   infop, options, rusage)`. Translate `(idtype, id)` to
   `WaitTarget`; populate `siginfo_t` at `infop`. `siginfo_t` is
   ~128 bytes; the SI_PID / SI_UID / SI_STATUS / SI_CODE
   (CLD_EXITED / CLD_KILLED / CLD_DUMPED) fields are written.
   `step_waitpid_nohang` returns `(Pid, ExitStatus)`; **no
   siginfo carrier exists yet** (commented at
   `process/execution.rs:483`). Either (a) extend the step's
   return type to include `cred + signum` snapshots so the
   syscall driver can compose `siginfo_t`, or (b) defer
   `NR_WAITID` to a later slice (LTP `waitid*` is 11 tests).
4. **Child thread's `saved_user_context` seed.** The Linux
   contract: child returns from clone with the parent's GPRs
   except `a0 = 0`; parent's clone returns the child pid in
   `a0`. The syscall arm therefore must:
   (a) snapshot the parent's `UserTrapContext` from
   `parent_thread.saved_user_context()`,
   (b) seed the child leader thread's `saved_user_context` to
   that snapshot with `regs[10] = 0` (RV64 a0) and `pc =
   parent_pc + 4` (return past the `ecall`),
   (c) return the child pid as the parent's syscall result
   (`SyscallResult::Return(child_pid as i64)`).
   None of (a)/(b)/(c) exist today — `step_fork` produces the
   identity but not the context seed. Cleanest seam: a new
   `step_fork_with_context` or a post-`step_fork` helper
   `seed_child_leader_context(child_proc, parent_ctx)` invoked
   from the syscall arm.
5. **CLONE_VFORK / CLONE_VM (posix_spawn path).**
   `posix_spawn.c:253` calls `__clone(child, stack, CLONE_VM |
   CLONE_VFORK | SIGCHLD, &args)`. **CLONE_VM**: parent and
   child share the same `Cap<AddressSpace>` (no `fork_aspace`
   call; the child holds a `clone()` of the parent's
   `Cap<AddressSpace>`). **CLONE_VFORK**: the parent thread is
   suspended (made un-runnable by the reactor) until the child
   either calls execve or exits, at which point the child wakes
   the parent. txKernel has no "suspend until subscriber-event"
   primitive on a per-thread basis; the closest is
   `wait_carrier`'s wake/wait (used by `NR_WRITE` to wait on
   console drain). Adding vfork **probably defers** unless the
   slice wants to land posix_spawn support too — a static-musl
   shell (`busybox` etc.) uses `posix_spawn` only when
   explicitly compiled for it; plain `fork+exec` flow works
   without vfork.
6. **CLONE_THREAD / CLONE_VM / CLONE_SIGHAND / CLONE_FILES /
   CLONE_FS / CLONE_SETTLS / CLONE_PARENT_SETTID /
   CLONE_CHILD_CLEARTID / CLONE_SYSVSEM / CLONE_DETACHED**
   (pthread_create per `pthread_create.c:325-328`).
   `step_clone_thread` (PROCESS_v1 §7.1.3) does not exist;
   `Shared<T>` infrastructure for AddressSpace / FdTable /
   SigActionTable does not exist; `set_tid_address`, the
   tid_address slot on ThreadPayload, and the FUTEX_WAKE on
   clear_child_tid at thread exit do not exist. This is a
   **separate slice** the size of the trio — defer.
7. **`NR_GETPPID = 173`.** Trivial: returns
   `parent.parent_pid().0 as i64` for live parent, or `1`
   (init's pid) when reparented. `parent_pid()` already exists
   at `structure.rs:170`.
8. **`NR_SETPGID = 154` / `NR_GETPGID = 155` / `NR_SETSID = 157`.**
   `step_setpgid` exists (`execution.rs:636`); `step_setsid`
   exists in tx-subsystems (per the trio decision). Syscall
   arms missing.
9. **`NR_SET_TID_ADDRESS = 96` / `NR_SET_ROBUST_LIST = 99`.**
   musl calls these unconditionally at startup
   (`__libc_start_main` → `__init_tls` → `set_tid_address`,
   `__init_tp` calls `set_robust_list`). **Stub**: return `1`
   (the leader's tid) for `set_tid_address`, return `0` for
   `set_robust_list`. musl ignores both return values in the
   static path. Without these stubs every musl-static binary's
   startup will hit `ENOSYS_VALUE` and (correctly) ignore it —
   so even today's hello-world fixture would survive without
   them; but adding stubs is one-line each and avoids spurious
   `ENOSYS` traces in fault-decode output.
10. **Pid recycling guard for LTP `fork13`.** `allocate_pid()`
    at `structure.rs:791` is a monotonic `fetch_add`, so pid
    reuse is impossible until u32 wraps; LTP fork13 should pass
    by construction.
11. **`exit_group` zombie children retention.** When init reaps
    via wait4, the reaped child's `Cap<ProcessIdentity>`
    becomes drop-eligible after epoch drain (per
    `step_waitpid_nohang`'s comment at `execution.rs:566-580`).
    Verify the EBR drop schedules the `AddressSpace` Cap as
    well — this is just the existing zone-machinery behaviour
    but worth a smoke test in the slice.

## musl fork+clone usage

Three call sites in static-musl that the slice cares about:

| Call site                        | Flags                                                                                                                                       | musl source                                                                            |
|----------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------|
| `_Fork()` (the syscall behind `fork()`) | `SIGCHLD` only                                                                                                                              | `src/process/_Fork.c:35` — `ret = __syscall(SYS_clone, SIGCHLD, 0)`                    |
| `posix_spawn()`                  | `CLONE_VM \| CLONE_VFORK \| SIGCHLD`                                                                                                        | `src/process/posix_spawn.c:253` — `pid = __clone(child, stack+sizeof stack, CLONE_VM\|CLONE_VFORK\|SIGCHLD, &args)` |
| `pthread_create()`               | `CLONE_VM \| CLONE_FS \| CLONE_FILES \| CLONE_SIGHAND \| CLONE_THREAD \| CLONE_SYSVSEM \| CLONE_SETTLS \| CLONE_PARENT_SETTID \| CLONE_CHILD_CLEARTID \| CLONE_DETACHED` | `src/thread/pthread_create.c:325-328`                                                  |

The MVP need-to-support set is therefore **`SIGCHLD` only** —
plain `fork()`. `posix_spawn` and `pthread_create` are out of
the slice unless explicitly elected. Static-musl `_start_c` →
`__libc_start_main` does not call `clone` directly during
startup; the only clone path a hello-world execve target hits
is the one its own program-text invokes, and the LTP execve
helpers (e.g., `execve01_child`) do not fork.

The **smallest clone-flag set the slice needs to support** is
the single bare-`SIGCHLD` invocation — i.e., the `clone(SIGCHLD,
0, NULL, 0, NULL)` shape — which forwards directly to the
existing `step_fork` plus the new "seed child context with a0=0"
hook.

## LTP coverage matrix

Test counts confirmed against the GitHub directory listings:
fork (10), clone (11), wait (2), wait4 (3), waitpid (11), waitid (11) — **48 tests** plus the unblocked `execve05`.

| Test               | What it verifies                                                                                            | Kernel surface to pass |
|--------------------|-------------------------------------------------------------------------------------------------------------|------------------------|
| `fork01`           | fork returns without error and returns child pid                                                            | `clone(SIGCHLD, 0)` + child-pid return |
| `fork03`           | child can use a large text space and many operations                                                        | demand-fault correctness post-fork |
| `fork04`           | environ is shared from parent into child (copy-on-write)                                                    | aspace fork preserves anon-private mappings (already works via fork_aspace) |
| `fork05`           | LDT propagation from parent to child (x86 only — likely SKIPS on RV64)                                      | n/a |
| `fork07`           | all children inherit parent's open fd                                                                       | fd-table snapshot in `step_fork` already does this |
| `fork08`           | parent's fds unaffected by child's close                                                                    | fd-table is per-payload (no `Shared<FdTable>`); already works |
| `fork09`           | child can close-and-reopen all parent's open files                                                          | fd-table per-payload + shared `Cap<OpenFile>` semantics |
| `fork10`           | fd inheritance with shared offset across parent/child                                                       | shared `Cap<OpenFile>` per slot (already works); needs `OpenFile`'s offset to be shared (it is — single `Cap`) |
| `fork13`           | pids not reused immediately                                                                                  | monotonic `allocate_pid` ensures no near-term reuse |
| `fork14`           | fork fails with `ENOMEM` when VMA length > 16 TB                                                             | aspace allocation failure path; v1 may pass-by-not-supporting >16TB |
| `clone01`          | child pid correct, flags = `SIGCHLD`                                                                         | bare-SIGCHLD clone path |
| `clone02`          | flag matrix: `CLONE_VM\|CLONE_FS\|CLONE_FILES\|CLONE_SIGHAND\|SIGCHLD` vs `SIGCHLD` only                      | requires `Shared<T>` infrastructure for VM/FS/FILES/SIGHAND — defer |
| `clone03`          | `getpid()` from child equals clone()'s return value                                                          | child-side pid via `NR_GETPID`; bare-SIGCHLD clone |
| `clone04`          | `clone(NULL stack)` returns `EINVAL`                                                                         | argument validation |
| `clone05`          | unknown — likely flag-validation                                                                              | argument validation |
| `clone06`          | unknown — likely `CLONE_PARENT_SETTID` write-back                                                             | requires CLONE_PARENT_SETTID — defer |
| `clone07–11`       | unknown — likely additional flag combinations                                                                | likely defer |
| `wait01`           | wait returns `ECHILD` when no children                                                                       | `step_waitpid_nohang` returns `WaitError::NoChildren` already |
| `wait02`           | wait retrieves terminated child's pid + exit status                                                          | bare wait4 + status-word encoding |
| `wait401`          | wait4 waits for child to exit, returns correct pid + status                                                  | requires **blocking** wait4 (not WNOHANG) — gap (5) |
| `wait402`          | wait4 with invalid pid returns `ECHILD`                                                                      | already covered by `WaitError::NoChildren` |
| `wait403`          | wait4(`INT_MIN`) returns `ESRCH`, not UB                                                                     | edge-case validation in pid → WaitTarget conversion |
| `waitpid01`        | child-killed-by-signal: parent receives correct status                                                       | `ExitStatus::Signaled(sig)` + 128+sig encoding (already works) |
| `waitpid03–13`     | various combinations of `WUNTRACED`/`WNOHANG`/process-group selectors                                        | mostly `WUNTRACED`/`WCONTINUED` (defer); pgrp selectors already work in `WaitTarget::Pgrp` |
| `waitid01`         | waitid + `WEXITED` returns correct value                                                                     | `NR_WAITID` arm + `siginfo_t` write |
| `waitid02–11`      | flag combinations: `WSTOPPED`, `WCONTINUED`, `__WALL`, `WNOWAIT`, etc.                                       | mostly defer (no stop/cont wiring; `WNOWAIT` reap-without-withdraw missing) |
| `execve05`         | concurrent exec: N children all exec simultaneously                                                          | unblocked once `clone(SIGCHLD, 0)` lands; exec is already concurrency-safe per the detached-aspace + EXEC-PONR design |

**Summary**: the bare-SIGCHLD clone + WNOHANG wait4 alone cover
~14 LTP tests outright (fork01, fork03, fork04, fork07–10, fork13,
clone01, clone03, clone04, wait01, wait02, wait402, waitpid01,
plus execve05) and partially cover ~5 more. The remaining
~25 tests are gated on (a) blocking wait4, (b) WUNTRACED /
WSTOPPED / WCONTINUED + stop/cont signal infrastructure,
(c) CLONE_PARENT_SETTID / CLONE_CHILD_CLEARTID, (d) WNOWAIT,
(e) waitid siginfo carrier, or (f) `Shared<T>` for CLONE_VM /
CLONE_FILES / CLONE_FS / CLONE_SIGHAND.

## Slice sizing for txKernel

### MVP — bare fork + WNOHANG wait4

Roughly:

- 4 new syscall numbers (`NR_CLONE`, `NR_WAIT4`, `NR_GETPPID`,
  `NR_SETPGID`/`NR_GETPGID`/`NR_SETSID`) + 2 stubs
  (`NR_SET_TID_ADDRESS`, `NR_SET_ROBUST_LIST`).
- 1 new syscall arm `sys_clone` that translates the flag set,
  rejects unsupported bits, calls `step_fork::<P>(parent)`,
  seeds the child leader thread's `saved_user_context` from the
  parent's snapshot with `regs[10] = 0` and
  `pc = parent_pc + 4`, returns the child pid.
- 1 new helper `seed_child_leader_context(child_proc,
  parent_ctx)` (or fold into `sys_clone`).
- 1 new syscall arm `sys_wait4` that translates the signed pid
  argument to `WaitTarget`, dispatches to `step_waitpid_nohang`,
  writes the status word + zero rusage to user pointers,
  returns the child pid (or `0` on `NoneReady` with `WNOHANG`).
- 1-3 trivial syscall arms (`getppid`, `setpgid`, `getpgid`,
  `setsid` — all already have step backings).
- 2 stub arms (`set_tid_address`, `set_robust_list`).
- Smoke test exercising `fork() → child writes "child\n" →
  exit_group(0); parent wait4 → asserts (child_pid, 0)`.

**Slice size: M (medium).** Comparable to a single trio wave —
much smaller than the ELF loader slice because the Process /
VM heavy lifting (`step_fork`, `fork_aspace`, parent/children
binding, SIGCHLD posting, `step_waitpid_nohang`) is already
done. Estimate 4 phase commits.

### Beyond MVP — blocking wait4 + posix_spawn

- **Blocking wait4** requires a parent-thread suspension
  primitive that wakes on a specific child's zombification.
  Today's `wait_carrier` is fd-shaped (one carrier per
  console-write-stall point); the wait family wants a per-
  parent or per-child notification slot. PROCESS_v1 §2 names
  `exit_port` (per-process, fires at last-thread exit) and
  `thread_exit_port` (per-thread, fires at thread zombification)
  as the wait-side wakeups; neither exists in code today.
  **Adds: `exit_port` slot on ProcessPayload + reactor-side
  wait integration in `step_waitpid_blocking`.**
- **CLONE_VFORK + CLONE_VM** (`posix_spawn` support) needs
  `Shared<AddressSpace>` (parent and child point at the same
  `Cap<AddressSpace>`) plus a parent-suspend-until-child-
  exec-or-exit primitive. The exec side already wakes correctly
  (the child's pmap install + `ProcessPayload.aspace` swap are
  the visible event). Adds two flag bits to the clone arm and
  the suspend/wake primitive.

### Beyond that — pthread_create

`CLONE_THREAD` + `CLONE_VM` + `CLONE_SIGHAND` + `CLONE_FILES` +
`CLONE_FS` + `CLONE_SETTLS` + `CLONE_PARENT_SETTID` +
`CLONE_CHILD_CLEARTID` + `set_tid_address(2)` + tid_address
slot + FUTEX_WAKE on clear_child_tid at thread exit. Requires
`Shared<T>` infrastructure for AddressSpace / FdTable /
SigActionTable per PROCESS_v1 §3 (entirely missing today). This
is its own slice — comparable to the trio in size. **Out of
scope for this research.**

## Implementation readiness verdict

**Ready to plan.** The kernel-side step catalogue (`step_fork`,
`fork_aspace`, parent/children binding, SIGCHLD posting on
zombification, `step_waitpid_nohang`, `step_setpgid`, exit-side
cascades) is already in place from the trio + pre-ELF work.
The slice is concentrated in tx-shims (syscall arms + arg
translation) plus one helper to seed the child leader's
`saved_user_context`. The big architectural question —
**`Shared<T>` infrastructure for full CLONE_FILES / CLONE_VM /
CLONE_SIGHAND** — is **out of scope** for the MVP because musl
fork is bare `SIGCHLD` and the ELF-loader smoke is `fork+execve`,
not `pthread_create`.

The one design call that needs the planning session's input is
the **siginfo carrier**: `NR_WAITID` requires a populated
`siginfo_t` (CLD_EXITED, si_pid, si_status, si_uid). Either
extend `step_waitpid_nohang`'s return shape to carry the snapshot
needed for siginfo composition, or defer `NR_WAITID` and only
ship `NR_WAIT4`. LTP `waitid01` is the only siginfo-coverage
test in the MVP target band.

## Open questions for the planning session

1. **vfork / posix_spawn — in slice or deferred?** Plain
   `fork+execve` is sufficient for execve05, fork+execve chains,
   wait+execve, and a static-musl shell that doesn't elect
   posix_spawn. CLONE_VFORK + CLONE_VM costs the parent-suspend
   primitive (which doesn't exist) plus shared-aspace plumbing.
   Recommend **deferred**.
2. **pthread_create — in slice or deferred?** Out-of-scope per
   the brief; needs `Shared<T>` + `step_clone_thread` +
   `set_tid_address` + tid_address slot + FUTEX wakeups. **Defer.**
3. **`NR_WAITID` — in slice or deferred?** Requires either a
   siginfo carrier or an extended return type from
   `step_waitpid_nohang`. LTP coverage cost of deferring: 11
   waitid tests stay red (most are flag-combination tests that
   need other surfaces too). **Recommend defer; ship NR_WAIT4
   only.**
4. **Blocking wait4 — in slice or deferred?** WNOHANG-only
   wait4 covers wait402 / waitpid01 / execve05 / fork+exec
   chains where the parent immediately polls. wait401 +
   waitpid03..13 want blocking. Adds `exit_port` infra. The
   ELF-loader smoke (fork → child execve → parent waitpid)
   could degrade-test by busy-polling, but real workloads
   (LTP runner) want blocking. **Recommend ship blocking in
   the same slice** — it's the same kernel-side step with a
   wait-channel hook, much smaller than `Shared<T>`.
5. **Child leader thread `saved_user_context` seam.** Should
   `step_fork` take an optional parent-context argument and seed
   the child internally, or stay context-free with a syscall-
   driver helper `seed_child_leader_context`? The latter keeps
   `step_fork` callable from non-syscall paths (which the
   bootstrap may someday want). **Recommend syscall-driver helper.**
6. **`NR_SET_TID_ADDRESS` / `NR_SET_ROBUST_LIST` stubs** —
   ship as ENOSYS-stub (return `-ENOSYS`) or as fake-success
   stub (return `1` / `0`)? musl ignores both return values in
   the static path; fake-success keeps fault-decode traces
   cleaner. **Recommend fake-success stubs.**
