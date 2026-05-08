# fork / clone / wait4

Status: proposed (planning only). Companion to
`docs/progress/plans/2026-04-29-kernel-main-long-term-checklist.json`
(`fork-clone-wait4` follow-up step from the ELF-loader decision
note's priority list moves toward complete after this slice).
The fork/clone/wait4 slice that unlocks LTP `execve05`,
fork+execve chains, wait+execve chains, and shell-prerequisite
blocking-wait. Bare-SIGCHLD `clone()` only;
`CLONE_VFORK` / `CLONE_THREAD` / `CLONE_VM` (other than the bare
aspace clone implicit in `step_fork`) are deferred to a future
slice. Builds on:

- The Process scaffolding shipped with the trio + pre-ELF + ELF
  loader. `step_fork::<P>` at
  `crates/tx-subsystems/src/process/execution.rs:248` already
  produces a new `Cap<ProcessIdentity>` + leader thread + payload
  + parent.children/pgrp wiring;
  `AddressSpace::fork_aspace::<P>` at
  `crates/tx-subsystems/src/vm/execution.rs:87` clones the
  parent's recipes under an `ExclusiveWriter` reservation.
  `step_waitpid_nohang` at
  `crates/tx-subsystems/src/process/execution.rs:523` already
  walks `parent.children`, supports the four POSIX selectors
  (`Any`, `Pid`, `Pgrp`, `CallerPgrp`), and reaps zombies.
- `post_sigchld_to_parent` at
  `crates/tx-subsystems/src/process/execution.rs:487` is already
  invoked from both `step_exit_group` and `step_process_exit`,
  so SIGCHLD is posted to the parent at zombification today.
- `SyscallResult` at
  `crates/tx-shims/src/linux_syscall/mod.rs:209` already carries
  `Return / Error / NoReturn / ExecCommitted` (the latter from
  Wave 4 of the ELF loader).
- The TTY input wait carrier (Channel +
  `tx_subsystems::wait_carrier::register_wait_channel` at
  `crates/tx-subsystems/src/wait_carrier.rs:28`) is the in-tree
  pattern this slice copies for the new `exit_port` wait carrier.

Inputs:
[`docs/progress/research/2026-05-06-fork-clone-wait4-scaffolding.md`](../research/2026-05-06-fork-clone-wait4-scaffolding.md)
fully (the slice's gap list, LTP coverage matrix, and musl
fork+clone usage table are all cited inline below);
[`docs/progress/decisions/2026-05-06-elf-loader-and-execve.md`](../decisions/2026-05-06-elf-loader-and-execve.md)
for the existing seams the slice builds on (most importantly the
`SyscallResult::ExecCommitted` variant, the `dispatch::<P>`
generic shape, the bootstrap-exec smoke pattern, and the host
test harness used in Wave 5).

## Goal

Demonstrate that a static-musl-shape RV64 fixture binary forks
itself via a bare `clone(SIGCHLD, 0)`, the parent calls `wait4`
and **blocks** until the child zombifies, the child writes
`b"child\n"` and exits via `exit_group(0)`, the parent's
blocking wait4 resolves, the parent reads back the child's pid +
zero status word, then writes `b"parent\n"` and exits via
`exit_group(0)`. End state: both processes reach zombie cleanly,
the console captured both lines in the right order, the
production reactor loop drained both threads. Smoke target —
**pick (a) host test** for the slice (a single host test in
`crates/tx-kernel/src/init/tests.rs` extending the ELF loader's
existing `boot_smoke_bootstrap_exec_seeds_init_user_context_from_fixture`
shape). Real RV64 QEMU boot is the deferred (b) variant; it
needs the same `--image` xtask flag the ELF loader's Wave 5
flagged as a deferred follow-up.

## Doc anchors

- `txdoc:PROCESS-FORK-AND-CLONE-1`,
  `txdoc:PROCESS-CLONE-FLAG-SUPPORT-V1-1`,
  `txdoc:PROCESS-STEP-CLONE-PROCESS-NEW-PROCESS-PATH-CLONE-THREAD-1`,
  `txdoc:PROCESS-STEP-CLONE-THREAD-CLONE-THREAD-PATH-1`
  (`docs/design/04_process-signals/PROCESS_v1.md` §7.1) — the
  fork-and-clone step catalog + the v1-supported flag table. The
  slice ships the `!CLONE_THREAD` (new-process) path only;
  `step_clone_thread` is deferred. Bare-SIGCHLD `clone()` is
  the smallest valid reachable subset of the v1 flag table.
- `txdoc:PROCESS-WAIT-FAMILY-1`
  (`docs/design/04_process-signals/PROCESS_v1.md` §7.4) — the
  wait family. The spec-shape `script_waitpid` returns
  `Result<(Pid, ExitStatus), Errno>`; the slice's blocking arm
  consumes `caller_proc.children_state_channel()` (named
  `exit_port` in the code) per the spec's §"Block until a
  child's state changes" arm.
- `txdoc:PROCESS-STEP-EXIT-GROUP-1`,
  `txdoc:PROCESS-STEP-PROCESS-EXIT-1`
  (`docs/design/04_process-signals/PROCESS_v1.md` §7.3.2/§7.3.3)
  — the §7.3.3 phase-5 cascade fires `exit_port` alongside
  SIGCHLD. The trio + ELF loader work already wires SIGCHLD;
  this slice adds the `exit_port` fire.
- `txdoc:PROCESS-SET-TID-ADDRESS-1`
  (`docs/design/04_process-signals/PROCESS_v1.md` §7.8) —
  `set_tid_address(2)` semantics. The slice ships a stub-success
  arm (return `1`) per the research note's gap (9); full
  semantics defer until a `pthread_create`-shaped slice.
- `txdoc:VM-FORK-ASPACE` (per the research note's reference;
  resolves to `docs/design/03_memory-vm/VM_v1_2.md` line 769's
  `async fn fork_aspace`) — already implemented at
  `crates/tx-subsystems/src/vm/execution.rs:87`. Cited for
  context only; no edits.
- `txdoc:THREAD-2-6-THREAD-EXIT-NOTIFICATION-CLONE-CHILD-CLEARTID`
  (`docs/design/02_execution/THREAD_RUNTIME_v1.md` §2.6) — the
  per-thread `clear_child_tid` slot + FUTEX_WAKE on thread exit.
  Out of slice; flagged in the deferred section.
- `txdoc:SIGNAL-DELIVERY-V1` (existing
  `crate::signal::step_kill_process` at
  `crates/tx-subsystems/src/signal.rs:547`) — for SIGCHLD
  delivery. Already wired by `post_sigchld_to_parent`; cited but
  not re-edited.

## Part 1 — Cross-doc supporting edits

The slice's prerequisites that aren't syscall arms. Three
sub-items: child-context seeding, the blocking wait4 carrier,
and the musl-startup stubs that the fixture binary will hit.

### A. Child saved_user_context seeding

The Linux contract: the child returns from `clone()` with the
parent's GPRs except `a0 = 0`; the parent sees the child pid in
`a0`. RV64-specific: `a0` is `regs[10]`, and the `ecall`
instruction is exactly 4 bytes — the parent traps with
`sepc = ecall_pc`, so the child must resume at `ecall_pc + 4`.
None of this seeding exists today: `step_fork`
(`crates/tx-subsystems/src/process/execution.rs:248`) produces
a child whose leader-thread `saved_user_context` is `None` (the
default produced by `ThreadPayload::fresh` at
`crates/tx-subsystems/src/thread_runtime/structure.rs:154`).

**Decision: a sibling helper, not an extension to `step_fork`.**
Open Q #5 from the research note recommended this and the plan
adopts it. Rationale: `step_fork` should stay context-free so
non-syscall callers (a future kernel-side daemon-spawner, the
trio's bootstrap-helper test harness) can call it without
synthesising a fake parent `UserTrapContext`. The syscall-driver
helper composes naturally.

#### Surface

- New free function in
  `crates/tx-subsystems/src/process/execution.rs` (sibling to
  `step_fork`):
  ```text
  pub fn seed_child_leader_context(
      child: &Cap<ProcessIdentity>,
      parent_ctx: &tx_hal::UserTrapContext,
  )
  ```
- Body:
  1. Extract `parent_ctx.regs` (32 GPRs), `parent_ctx.pc`, and
     `parent_ctx.status` (per the layout at
     `crates/tx-hal/src/trap.rs:133`).
  2. Compose a `UserTrapContext { regs: parent_regs, pc:
     parent_ctx.pc + 4, status: parent_ctx.status }` — copy
     all 32 GPRs verbatim, then overwrite `regs[10] = 0`.
  3. Resolve the child's leader thread via
     `child.threads.lock()[0].clone()` (under the payload guard
     — the leader is at index 0 by `step_fork` construction at
     `process/execution.rs:294`).
  4. Call
     `leader.payload_cap().expect("fork leader has payload").store_saved_user_context(Some(child_ctx))`.
- Infallible. The leader is guaranteed live-with-payload by
  `step_fork`'s post-conditions. No `?` operator; no awaits.
- Sibling helper, not a method on `ProcessIdentity` — keeps the
  RV64-ABI knowledge (the `+4` skip and the `regs[10]` index)
  local to this one site so a future ARM64 / x86_64 port has a
  single grep target.

#### Tests (live in
`crates/tx-subsystems/src/process/tests/seed_child_leader_context.rs`
or as a sub-mod in the existing process tests):

- `seed_child_leader_context_zeros_a0_and_advances_pc_past_ecall`
  — synthesise a parent `UserTrapContext` with
  `regs[10] = 0xdead`, `pc = 0x1000`, `status = 0x123`; run
  `step_fork` then `seed_child_leader_context`; read back the
  leader's snapshot and assert `regs[10] == 0`, `pc == 0x1004`,
  `status == 0x123`, `regs[i] == parent.regs[i]` for `i != 10`.
- `seed_child_leader_context_preserves_parent_sp_and_other_gprs`
  — assert `regs[2]` (sp) is identical between parent and child
  (Linux's bare-clone-with-stack=NULL convention).

### B. Blocking wait4 carrier (`exit_port`)

Per `txdoc:PROCESS-WAIT-FAMILY-1` line 943
(`children_state_channel`) — the wait family wants a
parent-side wake-up that fires when any child's state changes
(zombification, in v1). The TTY input wait
(`crates/tx-subsystems/src/tty/structure/identity.rs:289`'s
`wait_channel: Channel` + `wait_carrier_id` registered via
`wait_carrier::register_wait_channel` at
`identity.rs:305`) is the in-tree shape this slice copies.

**Decision: per-process, not per-process-group.** Open Q below
flags the alternative; per-process is the simplest shape that
covers all four `WaitTarget` selectors uniformly — `Any`,
`Pid(p)`, `CallerPgrp`, and `Pgrp(g)` all walk the same caller's
`children` list and fire on the same event (any child of this
parent zombifies). A pgrp-keyed channel would require routing
the fire side based on the dying process's pgrp at the time of
death, plus walking pgrp.members from the wait side; the
benefit is zero if a process has only one wait4 caller (the
common case), and the lookup cost is paid every fire.

#### Surface

- New field on `ProcessPayload` (in
  `crates/tx-subsystems/src/process/structure.rs` near
  `pub(crate) fds:` at line 499):
  ```text
  /// Reactor wait carrier that fires when **any** child of this
  /// process zombifies. Created at payload-sign time, registered
  /// with `wait_carrier::register_wait_channel` so async script
  /// wrappers can `wait_on_token` it. Released at payload drop
  /// (the slice's last cross-cutting risk #1).
  ///
  /// Pattern mirrors `TtyIdentity.wait_channel` /
  /// `wait_carrier_id` — the only other in-tree carrier today.
  pub(crate) exit_port: tx_reactor::wait::Channel,
  pub(crate) exit_port_carrier_id: u64,
  ```
- `sign_process_payload` (the existing helper at
  `crates/tx-subsystems/src/process/structure.rs` invoked from
  `bootstrap_init_process` and `step_fork`) constructs the
  `Channel` + registers the carrier id once:
  ```text
  let exit_port = tx_reactor::wait::Channel::new();
  let exit_port_carrier_id =
      crate::wait_carrier::register_wait_channel(exit_port.clone());
  ```
- New accessor pair on `ProcessIdentity` (the public surface
  syscall arms reach through):
  ```text
  pub fn exit_port_carrier_id(&self) -> Option<u64>;
  pub fn fire_exit_port(&self);
  ```
  `exit_port_carrier_id` reads through `payload.lock()`; returns
  `None` for zombies. `fire_exit_port` looks up the channel via
  the same path and calls `channel.fire(Mask::all())` (or the
  one-bit mask the slice picks — see Open Question #1 below).
- Drop / release: `ProcessPayload::Drop` (or a manual release
  during `step_exit_group` / `step_process_exit` when payload
  transitions to `None`) calls
  `wait_carrier::release_wait_channel(self.exit_port_carrier_id)`.
  See cross-cutting risk #1.

#### Fire site

- `post_sigchld_to_parent` at
  `crates/tx-subsystems/src/process/execution.rs:487` is the
  exact site: it already runs after the dying process is
  zombified (per the comment at `:483-486`). Add one line after
  the existing `step_kill_process` call:
  ```text
  parent.fire_exit_port();
  ```
  No-op for parents that are themselves zombies (`fire_exit_port`
  returns early when `payload.lock()` is `None`). No-op for the
  bootstrap-init case (init has no parent, `parent_cap()`
  returns `None`, the function returns before reaching this
  line).
- Comment at `process/execution.rs:362` ("the remaining §7.3.3
  phase-5 cascade — `exit_port` wake — when `exit_port`
  machinery arrives") gets a one-line update pointing at this
  slice's fire site.

#### Tests

- `exit_port_fires_when_child_zombifies` — fork a child; spawn
  a separate task that awaits the parent's `exit_port` (via
  `wait_carrier::wait_on_token` with a token built from
  `parent.exit_port_carrier_id().unwrap()`); call
  `step_exit_group(child, Exited(0))`; assert the awaiter
  resolves.
- `exit_port_does_not_fire_for_unrelated_process_exit` —
  process A and process B unrelated; B exits; A's `exit_port`
  has no pending fire.
- `exit_port_carrier_id_is_none_for_zombie_parent` — exit the
  process; assert `exit_port_carrier_id()` returns `None`.

### C. set_tid_address / set_robust_list shims

musl's startup (`__libc_start_main` → `__init_tls` → `__init_tp`)
calls both syscalls before `main`. Per the research note's gap
(9): without these stubs every musl-static binary's startup
hits the `_ => SyscallResult::Error(ENOSYS_VALUE)` default at
`crates/tx-shims/src/linux_syscall/mod.rs:256`. musl ignores the
return value but the `-ENOSYS` adds noise to fault-decode
traces.

**Decision: fake-success stubs.** Open Q #6 from the research
note recommended this and the plan adopts it.
- `NR_SET_TID_ADDRESS`: returns `1` (the leader's tid; matches
  Linux's "the calling thread's tid" semantic; the slice's
  single-threaded processes always have tid `1`). No state
  stored — the kernel discards the user pointer the syscall
  passes in. Future `pthread_create` slice wires the real tid +
  `clear_child_tid` slot per `THREAD_RUNTIME_v1` §2.6.
- `NR_SET_ROBUST_LIST`: returns `0` unconditionally. No state
  stored — robust-list machinery is its own slice (futexes +
  per-thread robust list head walking at thread exit).

Both stubs land in Part 5.

## Part 2 — NR_CLONE syscall arm

The slice's central new syscall. Implements the bare-`SIGCHLD`
shape that musl's `_Fork.c:35` relies on
(`__syscall(SYS_clone, SIGCHLD, 0)`). Out-of-slice flag
combinations (vfork, posix_spawn, pthread_create) explicitly
reject with `-EINVAL`.

### Surface

- `crates/tx-shims/src/linux_syscall/numbers.rs` —
  `pub const NR_CLONE: u64 = 220;` (Linux RV64 generic ABI).
- `crates/tx-shims/src/linux_syscall/mod.rs` — dispatch arm at
  the existing `match req.nr` block (line 245):
  ```text
  nr if nr == NR_CLONE => sys_clone::<P>(req.args, ctx).await,
  ```
  Note: `NR_CLONE` is `u64` so it goes through the `nr if ...`
  guard, matching the existing `NR_EXECVE` arm pattern at
  line 255.
- New function in the same file:
  ```text
  async fn sys_clone<'a, P: PmapIf>(
      args: [u64; 6],
      ctx: &SyscallCtx<'a>,
  ) -> SyscallResult
  ```

### Argument layout

Linux RV64 generic ABI for `clone(flags, stack, parent_tidptr,
tls, child_tidptr)` — note the order matches arm64, **not**
x86_64. musl's `_Fork.c:35` invokes this as
`__syscall(SYS_clone, SIGCHLD, 0)` — arity 2; remaining argument
slots are zero-filled by the calling convention. The arm reads:

- `args[0]` — flags (`u64`); low 8 bits = termination signal,
  upper bits = `CLONE_*` flags.
- `args[1]` — stack pointer for the child. `0` (NULL) means
  "use the parent's stack" per Linux convention; bare clone
  always passes 0.
- `args[2]` — parent_tidptr (ignored without `CLONE_PARENT_SETTID`).
- `args[3]` — tls (ignored without `CLONE_SETTLS`).
- `args[4]` — child_tidptr (ignored without
  `CLONE_CHILD_SETTID` / `CLONE_CHILD_CLEARTID`).

### Behaviour

1. Validate flags. `SIGCHLD = 17`. The arm accepts **only**
   `args[0] == SIGCHLD` for the slice; any other value
   (including `SIGCHLD | CLONE_VM`, `SIGCHLD | CLONE_VFORK`,
   the pthread_create flag set, or a different termination
   signal) returns `SyscallResult::Error(EINVAL_VALUE)`. This
   is the explicit out-of-slice gate; the deferred-section
   below names the flags that need their own slice.
2. Validate stack. `args[1] == 0` is required (Linux's "use
   parent's stack" semantic; bare clone always passes this).
   Non-zero stack returns `-EINVAL` for the slice (the
   posix_spawn/pthread_create paths use a non-NULL stack;
   they're deferred).
3. Snapshot parent context. Read
   `ctx.thread.payload_cap().expect("live").saved_user_context()`
   — the trap-shell stored this at trap-entry per Plan B
   discipline (`ThreadPayload.saved_user_context` at
   `thread_runtime/structure.rs:133`). Returns
   `Option<UserTrapContext>` — `None` means the thread isn't
   in a syscall (impossible here; we *are* the syscall arm).
   Treat `None` as a fatal kernel invariant; the slice may
   choose to return `-EINVAL` defensively or panic with a
   `:clone:no-context` sentinel — see Open Question #2 below.
4. Run `step_fork::<P>(&ctx.process)`. Map errors:
   - `ForkError::ParentZombie` → `-ESRCH` (the parent is a
     zombie — impossible here; the parent is the calling
     process and is by definition alive).
   - `ForkError::Vm(VmMapError::WouldBlock)` → loop with a
     small bounded retry count (max 8) and `-EAGAIN` on
     exhaustion. `fork_aspace` takes the parent's
     `ExclusiveWriter` reservation; the only way it sees
     `WouldBlock` is if a concurrent VM op holds the lock,
     which v1 single-thread-per-process processes don't have.
   - `ForkError::Zone(_)` → `-ENOMEM`.
5. Seed child context.
   `seed_child_leader_context(&child, &parent_ctx)` per Part 1A.
   Infallible.
6. Return.
   `SyscallResult::Return(child.pid.0 as i64)`.
   The child's first run-to-userspace happens through the
   production reactor loop's next iteration — the leader
   thread is registered at `step_fork`-time via
   `sign_thread`-construction, but its task is **not yet
   submitted to the reactor**. See cross-cutting risk #2.

The child returns `0` to userspace via the
`saved_user_context.regs[10] = 0` seeding. The parent returns
the child pid via `SyscallResult::Return` (which the existing
thread future writes into the trap-frame's a0 via
`pending_syscall_return` per Plan B).

### Tests
(`crates/tx-shims/src/linux_syscall/tests.rs` mod):

- `sys_clone_with_sigchld_only_returns_child_pid_to_parent` —
  call `dispatch::<P>` with
  `req = { nr: NR_CLONE, args: [SIGCHLD, 0, 0, 0, 0, 0] }`;
  assert `Return(pid > 1)`; read back the child's leader
  context and assert `regs[10] == 0` and
  `pc == parent_ctx.pc + 4`.
- `sys_clone_rejects_clone_vm` — flags = `SIGCHLD | CLONE_VM
  (0x100)`; expect `Error(EINVAL_VALUE)`.
- `sys_clone_rejects_clone_vfork` — flags = `SIGCHLD |
  CLONE_VFORK (0x4000)`; expect `Error(EINVAL_VALUE)`.
- `sys_clone_rejects_clone_thread_set` — flags = the full
  pthread_create OR-set (`CLONE_VM | CLONE_FS | CLONE_FILES |
  CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM | CLONE_SETTLS |
  CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID | 0`); expect
  `Error(EINVAL_VALUE)`.
- `sys_clone_rejects_nonzero_stack` — flags = `SIGCHLD`,
  stack = `0x4000_0000`; expect `Error(EINVAL_VALUE)`.
- `sys_clone_child_pid_visible_to_parent_via_children_list` —
  call `sys_clone`; assert `ctx.process.children().len() == 1`
  and the child's pid matches the returned value.
- `sys_clone_with_sigchld_seeds_child_a0_to_zero` — verify the
  child's leader-thread saved context has `regs[10] == 0` and
  the parent context is unchanged.

## Part 3 — NR_WAIT4 syscall arm (with blocking)

The slice's other central syscall. Implements the **blocking**
variant — Open Q #4 in the research note recommended shipping
blocking in the same slice, and the brief locks that decision in
(LTP `wait401` is the smoke target). The blocking discipline
follows the canonical async-wait pattern from
`vm::execution::fault_script` and the existing `sys_read` arm at
`crates/tx-shims/src/linux_syscall/mod.rs:400` — a polling loop
that takes a fresh `tx_substrate::epoch::guard()` each
iteration and `wait_on_token`s on the `exit_port` carrier
between attempts.

### Surface

- `crates/tx-shims/src/linux_syscall/numbers.rs` —
  `pub const NR_WAIT4: u64 = 260;`
- Constants at the same site: `pub const WNOHANG: i32 = 1;`
  (the only options bit the slice supports — `WUNTRACED`,
  `WCONTINUED`, etc. land with stop/cont signal infrastructure
  in a future slice).
- Dispatch arm at line 245 alongside the others:
  ```text
  nr if nr == NR_WAIT4 => sys_wait4(req.args, ctx).await,
  ```
- New function:
  ```text
  async fn sys_wait4<'a>(
      args: [u64; 6],
      ctx: &SyscallCtx<'a>,
  ) -> SyscallResult
  ```

### Argument layout

Linux RV64 ABI: `wait4(pid, status, options, rusage)`.

- `args[0]` — pid (signed `i32` cast from `u64`).
- `args[1]` — status pointer (`*mut i32`); `NULL` allowed
  (caller doesn't want the status word).
- `args[2]` — options (`u32`).
- `args[3]` — rusage pointer. The slice rejects non-`NULL` with
  `-EINVAL` (the research note's gap (2) flagged "v1 may
  zero-fill"; the plan opts for stricter — LTP doesn't check
  rusage, and adding zero-fill is busy-work that doesn't
  unblock anything). Documented under Out of scope.

### pid → WaitTarget translation

Per the existing `WaitTarget` doc at
`crates/tx-subsystems/src/process/execution.rs:123-145` (the
mapping is already pinned in the comment):

- `pid > 0` → `WaitTarget::Pid(Pid(pid as u32))`
- `pid == 0` → `WaitTarget::CallerPgrp`
- `pid == -1` → `WaitTarget::Any`
- `pid < -1` → `WaitTarget::Pgrp(Pgid((-pid) as u32))`
- `pid == i32::MIN` → `-EINVAL` (overflow-on-negate edge,
  matches Linux per LTP `wait403`).

### Behaviour

```text
let wnohang = (options & WNOHANG) != 0;
loop {
    let result = step_waitpid_nohang(&ctx.process, target);
    match result {
        Ok((child_pid, status)) => {
            // Encode status word into *args[1] if non-NULL.
            if status_ptr != 0 {
                let word = encode_wait_status_word(status);
                // SAFETY: kernel-buffer bootstrap exemption — TODO(phase-userva).
                unsafe {
                    core::ptr::write_volatile(status_ptr as *mut i32, word);
                }
            }
            return SyscallResult::Return(child_pid.0 as i64);
        }
        Err(WaitError::NoChildren) => {
            return SyscallResult::Error(ECHILD_VALUE);  // -ECHILD
        }
        Err(WaitError::NoneReady) => {
            if wnohang {
                return SyscallResult::Return(0);
            }
            // Park on exit_port. Get a token via the parent's
            // carrier id; await the channel; loop and re-poll.
            let Some(carrier_id) = ctx.process.exit_port_carrier_id()
                else {
                    // Parent is itself a zombie — caller raced.
                    return SyscallResult::Error(ESRCH_VALUE);
                };
            let token = WaitToken::new(carrier_id, EXIT_PORT_INTEREST);
            if let Some(future) = wait_carrier::wait_on_token(token) {
                let _ = future.await;
            }
            // Loop to re-poll; the wake races a third party reaping
            // the same child, so step_waitpid_nohang might still
            // return NoneReady — that's fine, we re-park.
        }
    }
}
```

`ECHILD_VALUE = 10` (Linux generic ABI errno for "No child
processes"). New constant alongside `EBADF_VALUE` etc. at the
top of `mod.rs`.

`EXIT_PORT_INTEREST` is a one-bit `Mask` constant — see Open
Question #1.

### Wait-status word encoding

Match Linux `<sys/wait.h>`. The existing
`ExitStatus::wait_status_word` accessor at
`crates/tx-subsystems/src/process/structure.rs:70` returns the
day-1 shell-convention encoding (`raw int` for `Exited`,
`128 + signum` for `Signaled`); the slice replaces this with the
full POSIX/Linux encoding:

```text
fn encode_wait_status_word(status: ExitStatus) -> i32 {
    match status {
        ExitStatus::Exited(code) => (code & 0xff) << 8,
        ExitStatus::Signaled(sig) => sig.raw() as i32 & 0x7f,
        // Future: Stopped → ((sig << 8) | 0x7f); Continued → 0xffff.
        // Future: Signaled w/ core dump → also OR 0x80.
    }
}
```

Macros applied by userspace: `WIFEXITED(s) == ((s) & 0x7f) ==
0`; `WEXITSTATUS(s) == ((s) >> 8) & 0xff`; `WIFSIGNALED(s) ==
(((s) & 0x7f) > 0 && ((s) & 0x7f) < 0x7f)`; `WTERMSIG(s) ==
(s) & 0x7f`. The existing `wait_status_word` accessor stays
as-is for the trio's smoke compatibility (debug/print uses);
the slice's syscall arm uses the new encoder.

Documented under Open Question #3 — should `wait_status_word`
be replaced with `encode_wait_status_word` in tree, or both
kept for different audiences?

### Tests

- `sys_wait4_returns_echild_when_no_children_match`.
- `sys_wait4_wnohang_no_children_returns_echild_not_zero` —
  edge case: WNOHANG with `ECHILD` is the same `ECHILD` (not
  `0`). LTP `wait402` checks this.
- `sys_wait4_wnohang_no_zombie_returns_zero` — fork a child
  but don't let it exit; WNOHANG; expect `Return(0)`.
- `sys_wait4_blocking_resolves_when_child_exits` — fork a
  child; spawn a task that calls `step_exit_group(child,
  Exited(42))` after a small await delay; call
  `sys_wait4(-1, &mut status, 0, NULL)` (no WNOHANG); assert
  `Return(child_pid)` and `status == 0x2a00` (42 << 8).
- `sys_wait4_status_encodes_signaled_term` — exit child via
  `step_exit_group_with_signal(SIGKILL = 9)`; wait4; assert
  `status == 9` (sig in low 7 bits).
- `sys_wait4_pid_negone_matches_any_child`.
- `sys_wait4_pid_zero_matches_caller_pgrp`.
- `sys_wait4_pid_lt_negone_matches_target_pgrp`.
- `sys_wait4_rejects_intmin` — `pid = i32::MIN`; expect
  `Error(EINVAL_VALUE)` (per LTP wait403's edge-case
  conversion concern).
- `sys_wait4_rejects_nonzero_rusage` — rusage_ptr = `0x1000`;
  expect `Error(EINVAL_VALUE)`.
- `sys_wait4_blocking_handles_child_zombified_between_poll_and_wait`
  — race smoke: between the first `step_waitpid_nohang` poll
  (returns `NoneReady`) and the `wait_on_token` await, the
  child zombifies. The await might miss the fire; the loop's
  re-poll catches the zombie. Assert convergence within 2
  iterations.

## Part 4 — Process-tree introspection syscalls

Five small arms. Each backs a helper that already exists in
`tx-subsystems::process` (per the research note's gap 7, 8,
the trio's setpgid/setsid coverage).

### Shape

- `crates/tx-shims/src/linux_syscall/numbers.rs` adds:
  ```text
  pub const NR_GETPPID:  u64 = 173;
  pub const NR_SETPGID:  u64 = 154;
  pub const NR_GETPGID:  u64 = 155;
  pub const NR_GETPGRP:  u64 = 81;   // legacy / glibc-only;
                                     // include for completeness
  pub const NR_GETSID:   u64 = 156;
  pub const NR_SETSID:   u64 = 157;
  ```
  The research note's gap (7) was unsure about `NR_GETPGRP =
  81`. Linux's RV64 generic ABI does **not** ship `NR_GETPGRP`
  (glibc emulates it as `getpgid(0)`); the slice still defines
  the constant for grep-stability and adds an `_ => -ENOSYS`
  for it deliberately at the dispatch site.
  Verify before commit: if the constant collides with another
  syscall on RV64 generic, drop it.
- `linux_syscall/mod.rs` dispatch arms (each one-liner):
  ```text
  nr if nr == NR_GETPPID  => sys_getppid(ctx),
  nr if nr == NR_SETPGID  => sys_setpgid(req.args, ctx),
  nr if nr == NR_GETPGID  => sys_getpgid(req.args, ctx),
  nr if nr == NR_GETSID   => sys_getsid(req.args, ctx),
  nr if nr == NR_SETSID   => sys_setsid(ctx),
  ```

### Per-arm body

- `sys_getppid(ctx) -> SyscallResult`:
  Read `ctx.process.parent_pid().0 as i64`. Returns `0`
  (Pid::RESERVED) for init or for processes whose parent has
  been reclaimed — matches the comment at
  `process/structure.rs:170`. Real Linux returns `1` (init)
  for orphaned processes; the slice's `parent_pid` accessor
  returns `Pid::RESERVED` (0) instead. Note: this differs from
  Linux init-as-reaper semantics; the existing `sever_children`
  at `process/execution.rs:391` reparents to init when init is
  registered, so under normal flows orphans will see
  `parent_pid() == 1`. The `0` return is the pre-init bootstrap
  edge — flagged for the slice's tests but not a behaviour
  change.
- `sys_setpgid(args, ctx)`:
  - `args[0]` = pid; `args[0] == 0` means "self".
  - `args[1]` = pgid; `args[1] == 0` means "use pid".
  - Validate pid: only "self" supported in v1 (per the trio's
    `step_setpgid` comment at `process/execution.rs:632` —
    new_pgid only equals target.pid). Cross-process setpgid
    needs a pid → Cap lookup the slice doesn't add. Resolves
    to `step_setpgid(&ctx.process, Pgid(target.pid.0))`.
    Errors:
    - `SetpgidError::Unimplemented` → `-EPERM` (matches
      Linux's `EPERM` for cross-pgrp setpgid).
    - `SetpgidError::Zone(_)` → `-ENOMEM`.
- `sys_getpgid(args, ctx)`:
  - `args[0]` = pid; `0` means self.
  - For the slice, only "self" supported (cross-pid lookup
    deferred). Reads `ctx.process.pgrp_cap().pgid.0 as i64`.
- `sys_getsid(args, ctx)`:
  - `args[0]` = pid; `0` means self.
  - Same restriction. Reads
    `ctx.process.pgrp_cap().session_cap().sid.0 as i64`.
- `sys_setsid(ctx)`:
  - No args (apart from the implicit caller). Runs
    `step_setsid(&ctx.process)`. On `SetsidError::Zone(_)`
    return `-ENOMEM`. Note: real Linux returns `-EPERM` if the
    caller is already a process-group leader; the trio's
    `step_setsid` at `process/execution.rs:661` doesn't enforce
    this. The slice ships the trio's behaviour and flags as a
    follow-up.

### Tests

- `sys_getppid_returns_parent_pid_after_fork`.
- `sys_getppid_returns_zero_for_orphan_pre_init` — bootstrap
  test only; not user-reachable in normal flows.
- `sys_setpgid_self_creates_new_pgrp`.
- `sys_setpgid_cross_process_returns_eperm` — pass non-self
  pid; expect `-EPERM`.
- `sys_setpgid_self_to_other_existing_pgid_returns_eperm` —
  trio `step_setpgid::Unimplemented` path.
- `sys_getpgid_self_returns_caller_pgrp`.
- `sys_getsid_self_returns_caller_session`.
- `sys_setsid_makes_caller_session_leader`.

## Part 5 — musl-startup stubs

Two arms; each one line of body. Per Part 1C's decision.

### Shape

- `crates/tx-shims/src/linux_syscall/numbers.rs` adds:
  ```text
  pub const NR_SET_TID_ADDRESS: u64 = 96;
  pub const NR_SET_ROBUST_LIST: u64 = 99;
  ```
- `crates/tx-shims/src/linux_syscall/mod.rs` dispatch arms:
  ```text
  nr if nr == NR_SET_TID_ADDRESS =>
      SyscallResult::Return(1),  // leader tid; TODO(phase-tls)
  nr if nr == NR_SET_ROBUST_LIST =>
      SyscallResult::Return(0),  // TODO(phase-robust-list)
  ```
- Each TODO marker carries a one-line comment naming the
  follow-up slice.

### Tests

- `sys_set_tid_address_returns_one_unconditionally` — pass any
  pointer; expect `Return(1)`.
- `sys_set_robust_list_returns_zero_unconditionally`.

These tests are tiny but pin the no-op contract — once the
real slice arrives the test names land in the deletion diff,
making the cutover obvious.

## Part 6 — RV64 fixture binary v2

Extend the hand-encoded `init_fixture.rs` from the ELF loader's
Wave 5 to actually fork+wait+exit. The existing fixture
(`crates/tx-kernel/src/init/init_fixture.rs`, 218 bytes) is too
small. Two options:

(a) **Extend `init_fixture.rs`.** The current fixture's text is
~24 instructions (`li`/`auipc`/`addi` + ecall pair).
Extend to ~70 instructions for the fork+wait+exit shape. Tests
that pin the existing 218-byte length break — that's fine; the
slice is intentionally rewriting the smoke target.

(b) **Add a sibling `init_fork_fixture.rs`.** Keep the trio's
hello-world fixture as-is for regression coverage; add a second
fixture under a feature flag or as a plain second const blob.
The bootstrap path picks whichever the test wants.

**Decision: (a).** Open Question #4 below restates this; the
case for (b) is fixture diversity, but the slice's smoke
intentionally supersedes the ELF loader's hello-world smoke
(which leaves no after-image). The existing
`boot_smoke_bootstrap_exec_seeds_init_user_context_from_fixture`
test stays intact (it asserts the exec-front-end seeded the
context, not what the program does); the slice's new test
drives the full reactor loop and asserts the fork+wait
round-trip. Both can run.

### Code (RV64 asm sketch)

The fixture is hand-encoded byte-by-byte. The structure:

```text
parent:
    li   a7, NR_CLONE          ; a7 = 220
    li   a0, SIGCHLD           ; a0 = 17  (low-byte termination signal)
    li   a1, 0                 ; a1 = 0   (stack=NULL)
    li   a2, 0                 ; a2 = 0   (parent_tidptr=NULL)
    li   a3, 0                 ; a3 = 0   (tls=NULL)
    li   a4, 0                 ; a4 = 0   (child_tidptr=NULL)
    ecall
    ; child sees a0 = 0 here; parent sees a0 = child_pid.
    beq  a0, x0, child_path
parent_path:
    ; parent: wait4(-1, NULL, 0, NULL) (blocking)
    li   a7, NR_WAIT4
    li   a0, -1
    li   a1, 0
    li   a2, 0
    li   a3, 0
    ecall
    ; write "parent\n"
    li   a7, NR_WRITE
    li   a0, 1
    auipc a1, ...               ; pointer to "parent\n"
    addi  a1, a1, ...
    li   a2, 7
    ecall
    ; exit_group(0)
    li   a7, NR_EXIT_GROUP
    li   a0, 0
    ecall
child_path:
    ; child: write "child\n"
    li   a7, NR_WRITE
    li   a0, 1
    auipc a1, ...               ; pointer to "child\n"
    addi  a1, a1, ...
    li   a2, 6
    ecall
    ; exit_group(0)
    li   a7, NR_EXIT_GROUP
    li   a0, 0
    ecall
.data
"child\n"
"parent\n"
```

Estimated byte count: ~250-300 bytes. The existing fixture's
`auipc`/`addi` pattern works for the data references; the
parent/child branch is the only new control-flow edge.

### Tests

- 7 fixture-pin tests in the same shape as the existing
  `init_fixture.rs::tests` — every byte against accidental
  edits. The fixture's exact byte count and entry point lock
  in via constants `INIT_FIXTURE_FORK_LEN` /
  `INIT_FIXTURE_FORK_ENTRY_OFFSET`.
- `init_fixture_v2_parses_into_image_plan` — the fixture
  passes `parse_image_plan` (the ELF loader's parser at
  `crates/tx-scripts/src/process/exec/loader.rs::parse_image_plan`).
- `init_fixture_v2_branch_target_is_within_text_segment` —
  decode the `beq` offset; assert it points to `child_path`
  inside the same LOAD segment (defends against sign-extension
  bugs in the hand-encoded immediate).

## Part 7 — End-to-end smoke

The slice's payoff. Replace (or extend) the ELF loader's
`boot_smoke_bootstrap_exec_seeds_init_user_context_from_fixture`
host smoke. The new test uses the panic-as-yield reactor drive
pattern from pre-ELF Phase 7's
`boot_smoke_production_userspace_loop_writes_console_then_exits`
combined with the ELF loader Wave 5's exec-front-end pattern.

### Shape

`crates/tx-kernel/src/init/tests.rs::boot_smoke_fork_wait_round_trip_through_reactor_loop`:

1. Drive `drive_boot_wiring` (substrate + reactor setup).
2. Drive `run_bootstrap_exec_for_init` (ELF loader's Wave 5
   helper) with the new fork-fixture (Part 6).
3. Spawn the production reactor loop. The loop drives both
   threads to completion: parent's `clone` syscall lands,
   `step_fork` runs, the child's leader thread is registered
   via `Reactor::submit_task` (see cross-cutting risk #2).
4. Drain. Assert (in order):
   - `init.children().len() == 1` after the parent's `clone`.
   - Both processes reach `is_zombie() == true` with
     `ExitStatus::Exited(0)`.
   - Console captured `b"child\n"` followed by `b"parent\n"`
     (the parent's wait4 blocks until child exit, so child's
     write must happen first; smoke for the blocking-wait
     contract).
   - Parent's wait4 returned the child's pid (smoke via a
     side-channel: read back the parent's
     `pending_syscall_return` snapshot at the moment of
     parent's exit_group; or thread the wait4 return through
     a kernel-side observable).
   - Child's leader thread had `regs[10] == 0` at the moment
     of its first userspace entry (smoke for the seed
     contract; capture from the trap-handoff IRQ side at
     entry, store in a test-only side-channel slot).

### Race-resilient assertion ordering

The console is FIFO-ordered through the TTY; the parent and
child both write to fd 1 (the same `Cap<OpenFile>`). Without
the blocking-wait contract, child and parent writes could
interleave on a multi-task reactor. The test's "child before
parent" assertion is **only valid because the parent's wait4
blocks past the child's write+exit_group**. The test thereby
smokes the blocking-wait contract end-to-end — if blocking
regresses to busy-poll, the parent might race its write before
the child's, and the assertion catches it.

## Cross-cutting risks

1. **`exit_port` wait-channel lifetime.** When the parent
   process zombifies (or its payload is otherwise dropped),
   the `exit_port: Channel` plus its registered carrier id
   need to be released — otherwise `wait_carrier`'s `REGISTRY`
   `BTreeMap` leaks one entry per dead process.
   **Mitigation**: register a `Drop` on `ProcessPayload` that
   calls `wait_carrier::release_wait_channel(self.exit_port_carrier_id)`.
   Verify the EBR-deferred drop semantics — the payload's
   `Drop` must fire after the last reader on the carrier id
   has dropped. Today's pattern (TtyIdentity at
   `tty/structure/identity.rs:289-317`) registers but never
   releases — that's a pre-existing leak the trio absorbed
   (TTY identities are typically permanent). For
   ProcessPayloads (transient) the leak is a real bug; the
   slice fixes it.
2. **Reactor task submission for the child.** `step_fork`
   produces the child's leader thread but does **not**
   submit the thread future to the Reactor. The trio's path
   (init.rs's `run_userspace_reactor_loop` at
   `init.rs:911`) only calls `Reactor::submit_task` for init's
   leader. The slice has to wire the child's leader into the
   reactor at clone-arm-time. Cleanest seam: a new helper
   `submit_thread_future_for_child::<P>(child_thread)` in
   `tx-kernel::init` that mirrors the BSP-side init submission.
   Called inline from `sys_clone` after `seed_child_leader_context`.
   Tracking issue: `tx-kernel` depends on `tx-shims` (the
   syscall arm crate); the helper has to live somewhere both
   can reach. Easiest is a `submit_for_clone` exported from
   `tx-kernel`'s thread_future module that `sys_clone` calls
   through a boxed function pointer the kernel sets at
   bootstrap time. Or — alternatively — extend `step_fork` /
   add a new `step_fork_and_submit::<P>` step that takes a
   submission-callback and lives in tx-shims. **Preferred:
   sibling helper exported from tx-kernel; tx-shims calls
   through a once-init function-pointer slot**, matching how
   tx-shims already reaches platform-specific code via
   `PmapIf`.
3. **Race: child can zombify between `step_waitpid_nohang`'s
   first attempt and the await on `exit_port`.** The
   spec-shape pattern is the standard double-check: poll →
   register interest → re-poll. The Channel's wait_on_token
   future is created **after** the poll; if the child fires
   `exit_port` between poll-returns-NoneReady and
   wait_on_token-creates-future, the future is created with no
   pending fire and waits forever. Mitigation: the
   `Channel`'s `wait` API must implement
   register-then-check-readiness semantics (the in-tree
   `tx_reactor::wait::Channel` at
   `crates/tx-reactor/src/wait.rs:176` should — verify before
   commit). Backup: if the Channel doesn't already do this,
   wrap the await in a bounded retry loop with a tiny
   yield-backoff. Test
   `sys_wait4_blocking_handles_child_zombified_between_poll_and_wait`
   (Part 3) covers the race.
4. **`saved_user_context.pc + 4` assumes `ecall` is 4 bytes.**
   On RV64 it always is (the C extension's compressed `c.ecall`
   does not exist; `ecall` is RV32I/RV64I base-ISA only). For
   portability to ARM64 (where `svc` is also 4 bytes) and
   x86_64 (where `syscall` is 2 bytes!), the +4 needs to be a
   per-platform constant. **Mitigation**: extract a constant
   `pub const SYSCALL_INSTR_BYTES: usize = 4;` in
   `crates/tx-hal/src/trap.rs` (RV64 path) or define it on the
   `PmapIf`-shaped platform trait. The slice ships RV64-only;
   the constant just gets a doc-comment naming the per-arch
   value. Flag for the future port.
5. **musl's `__syscall_cp` cancellation-aware wrapper.**
   musl's pthread cancellation hooks wrap `wait4`/`pause` in a
   `__syscall_cp` checkpoint — but **plain `_Fork`** at
   `_Fork.c:35` uses raw `__syscall`, not `__syscall_cp`. So
   `clone(SIGCHLD, 0)` is NOT cancellation-aware. `wait4` IS,
   but txKernel has no thread-cancellation infrastructure
   anyway — the slice can ignore the wrapper entirely. Once
   pthread_cancel lands the slice's syscall arms re-validate.
6. **Child stack: bare clone with `stack=NULL`.** Linux
   convention: NULL stack means "child uses the parent's
   stack." That works because parent and child have separate
   address spaces post-`fork_aspace`, so writes to the shared
   sp don't collide. **Verify musl actually passes 0** —
   `_Fork.c:35` reads `__syscall(SYS_clone, SIGCHLD, 0)` —
   yes, the second argument is `0`. The slice's
   `sys_clone` validates `args[1] == 0` and rejects non-zero
   (which is the posix_spawn / pthread_create path, deferred).
7. **Pid recycling guarantees for LTP `fork13`.**
   `allocate_pid` at `process/structure.rs:791` is monotonic
   `AtomicU32::fetch_add(1, Relaxed)`, so reuse is impossible
   until u32 wraps. fork13 passes by construction. Flagged
   for the future namespace-aware Pid slice.
8. **`exec_port` interest-mask granularity.** Open Question
   #1 below. Today's TTY uses `Mask::from_bits(token.interest())`
   with a small bit-set; the slice picks one bit (`0x1`) for
   "child zombified." Future signals (`exit_port` could carry
   stop/cont fires once they exist) get a separate bit each.
9. **Status-word encoding migration.** The existing
   `wait_status_word` on `ExitStatus` (at
   `process/structure.rs:70`) produces day-1's `128 + sig`
   shell shape; the slice's `encode_wait_status_word` produces
   the proper Linux `<sys/wait.h>` shape. Two encoders
   coexisting risks a caller picking the wrong one. **Mitigation**:
   rename `wait_status_word` to `legacy_wait_status_word`
   with a deprecation TODO, and keep both in tree until the
   trio's smokes that consume it migrate. (The smokes at
   `crates/tx-kernel/src/init/tests.rs` only consume it
   through assertion text comparisons — they're easy to
   migrate.) Open Question #3 covers the cleanup decision.

## Out of scope (deliberately deferred)

- **CLONE_VFORK + posix_spawn.** Per the research note's gap
  (5). Adds `Shared<AddressSpace>` (parent and child point at
  the same `Cap<AddressSpace>`) plus a parent-thread suspend
  primitive that wakes on child exec-or-exit. txKernel has no
  per-thread suspend that wakes on a subscriber-event today.
  **Defer.**
- **CLONE_THREAD + pthread_create + TLS via SET_TLS_DESC** —
  Per the research note's gap (6). Needs `step_clone_thread`
  (does not exist), `Shared<T>` for AddressSpace / FdTable /
  SigActionTable (none of these exist), `set_tid_address`
  storage + tid_address slot on ThreadPayload (does not exist),
  FUTEX_WAKE on `clear_child_tid` at thread exit (does not
  exist). The whole pthread slice is its own work item the
  size of the trio. **Defer.**
- **CLONE_FILES / CLONE_FS / CLONE_SIGHAND shared-on-clone** —
  Per `txdoc:PROCESS-CLONE-FLAG-SUPPORT-V1-1`. Today
  `step_fork` always copies (Linux `fork(2)` semantics). The
  shared variants need the same `Shared<T>` infrastructure as
  CLONE_THREAD. **Defer.**
- **`NR_WAITID` (`siginfo_t` carrier).** Per Open Q #2 from
  the research note (and the brief's locked decision). Adds
  ~11 LTP tests (`waitid01..11`); most are flag-combination
  tests that need WUNTRACED/WCONTINUED anyway. **Defer.**
- **Per-thread `tid_address` futex wakeup** — full
  `set_tid_address` semantics. Requires futex infrastructure
  + per-thread `clear_child_tid` slot + thread-exit hook.
  **Defer with the pthread slice.**
- **Robust-list machinery** — full `set_robust_list`. Needs
  futex + per-thread robust-list head + walk-on-thread-exit.
  **Defer with the futex slice.**
- **Process-group session semantics beyond what the trio
  shipped** — job control, terminal ownership transitions on
  parent exit (the trio's `session_leader_hangup_cascade` at
  `process/execution.rs:440` covers SIGHUP-on-leader-death,
  but `tcsetpgrp`/`tcgetpgrp` aren't wired). **Defer.**
- **Waitpid stop/continue (`WUNTRACED`, `WCONTINUED`)** —
  needs `Stopped`/`Continued` `ExitStatus` variants + the
  signal-driven stop/cont pathway. **Defer with stop/cont
  signal slice.**
- **Cross-process `setpgid(target, pgid)` and
  `getpgid(target_pid)`** — needs a pid → `Cap<ProcessIdentity>`
  lookup the slice doesn't add. The trio's
  `step_setpgid` only supports self. **Defer.**
- **rusage zero-fill** — the slice rejects non-NULL rusage
  with `-EINVAL`. Future LTP tests that pass an rusage pointer
  will need zero-fill. **Defer with the rusage slice.**
- **Real RV64 QEMU smoke through `cargo xtask qemu`** —
  same deferral as the ELF loader's Wave 5. Picks up after
  this slice's host smoke proves fork+wait works. Needs the
  same `--image` xtask flag.

## Phasing

Each step is a self-contained PR. Land in this order; later
parts depend on earlier ones (Parts 2/3 need Part 1; Part 6
needs Parts 2+3+4+5; Part 7 ties together).

1. **S — Part 1A `seed_child_leader_context` helper.**
   Touches `crates/tx-subsystems/src/process/execution.rs`
   only. Pure function + 2 tests. Estimated ~80 LOC.
2. **M — Part 1B `exit_port` wait carrier.** Touches
   `crates/tx-subsystems/src/process/structure.rs` (new
   field, accessor pair, drop hook),
   `crates/tx-subsystems/src/process/execution.rs` (fire
   site in `post_sigchld_to_parent`). 3 tests. Cross-cutting
   risk #1 fix is part of this PR. Estimated ~250 LOC.
3. **M — Part 2 `NR_CLONE` syscall arm.** Touches
   `crates/tx-shims/src/linux_syscall/numbers.rs`,
   `crates/tx-shims/src/linux_syscall/mod.rs` (new arm,
   bounded user-buffer helper if needed),
   `crates/tx-kernel/src/thread_future.rs` (the reactor
   submission helper for cross-cutting risk #2). 7 tests.
   Estimated ~400 LOC.
4. **M — Part 3 `NR_WAIT4` syscall arm + status encoder.**
   Touches `crates/tx-shims/src/linux_syscall/numbers.rs`,
   `crates/tx-shims/src/linux_syscall/mod.rs` (new arm +
   status-word encoder + ECHILD constant). 9 tests including
   the race smoke. Estimated ~350 LOC.
5. **S — Part 4 process-tree introspection syscalls.** 5
   small arms; all back existing helpers. 8 tests. Estimated
   ~200 LOC.
6. **S — Part 5 musl-startup stubs.** 2 stub arms + 2 tests.
   Estimated ~50 LOC.
7. **M — Part 6 RV64 fixture binary v2.** Touches
   `crates/tx-kernel/src/init/init_fixture.rs` (extend
   in-place per the (a) decision); the fixture-pin tests
   migrate alongside. Estimated ~400 LOC including the new
   fixture bytes + assert harness.
8. **M — Part 7 end-to-end smoke.** Touches
   `crates/tx-kernel/src/init/tests.rs` (new test using the
   panic-as-yield reactor drive pattern). Estimated ~300 LOC
   including the side-channel observability hooks.

Total: 8 PRs, ~2000 LOC. Comparable to the ELF loader slice
in shape but with a tighter blast radius (no doc-anchor
edits, no new VM surface, no new exec primitives).

## Open questions

1. **`exit_port` interest-mask shape.** The Channel API takes
   a `Mask::from_bits(u64)`. Future fires (`exit_port` could
   carry stop/cont/continued events once those signals exist)
   need their own bit. The slice picks `0x1` for "child
   zombified." Should the slice carve out a documented
   bit-table now (anticipating stop = `0x2`, continued =
   `0x4`, etc.) or leave that to the slice that adds those
   events? **Recommend**: ship `EXIT_PORT_CHILD_ZOMBIFIED
   = 0x1` only, with a doc-comment saying future bits land
   alongside their wakers. No premature taxonomy.
2. **`saved_user_context == None` at clone-time — DECIDED
   2026-05-06: panic with `:clone:no-context` sentinel.**
   Matches the precedent set by the ELF loader's
   `:bootstrap-exec:fail`: kernel-invariant violations panic
   with a stable string sentinel CI can grep. `-EINVAL` was
   considered but rejected because a `None`
   `saved_user_context` at clone-time means the trap shell
   never stored it at trap-entry, which is a kernel bug, not
   a userspace error.
3. **`ExitStatus::wait_status_word` cleanup — DECIDED
   2026-05-06: replace in tree with the POSIX encoding, and
   migrate the 2 trio smokes that reference it.** The trio's
   `128 + sig` is the *shell* convention (bash-style program
   exit code), not the kernel↔userspace `wait4` ABI. Now that
   we have real userspace, the kernel needs the POSIX encoding;
   the shell-style encoding belongs in userspace shells, not
   in the kernel. Migration is small (2 smoke assertions).
4. **Single end-to-end smoke binary or two — DECIDED
   2026-05-06: extend `init_fixture.rs` in place.** The
   existing fixture's hello-world output isn't asserted by
   any surviving smoke (the production-loop smoke scripts
   its own syscalls; the bootstrap-exec smoke only asserts
   the seed). Changing init's runtime behaviour to fork +
   child-writes + parent-waits is safe; single source of
   truth keeps fixture maintenance light.
5. **`exit_port` granularity — per-process or
   per-process-group?** The plan picks per-process. A
   per-pgrp variant routes fires by pgrp at zombification
   time and lets pgrp-shaped wait4 (`pid < -1`) wait on a
   group channel directly instead of walking children. The
   benefit is zero for v1 (all wait4 callers walk children
   anyway via `step_waitpid_nohang`). **Recommend
   per-process** as the plan; user input wanted only if the
   pgrp variant has a coverage benefit not yet identified.
