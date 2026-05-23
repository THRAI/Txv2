---
date: 2026-05-22
topic: "pthread implementation audit against active specs and pinned musl"
status: current
refs:
  - docs/design/04_process-signals/SIGNAL_v1.md
  - docs/design/04_process-signals/PROCESS_v1.md
  - docs/design/02_execution/THREAD_RUNTIME_v1.md
  - external/musl/src/thread/pthread_cancel.c
  - external/musl/src/thread/pthread_create.c
  - external/musl/arch/riscv64/bits/syscall.h.in
  - external/oscomp-autotest/kernel/judge/judge_libctest-musl.py
---

# pthread musl audit

## Scope

This audit covers the current pthread-facing implementation in
`/Users/3y/.codex/worktrees/86df/Tx` against active process/thread/signal specs
and the pinned `external/musl` submodule at
`5122f9f3c99fee366167c5de98b31546312921ab`.

The live symptom motivating the pass is the OSComp `libctest-musl` pthread
family blocking or timing out around `pthread_cancel_points`,
`pthread_cancel`, `pthread_cond`, and `pthread_tsd`. A focused host regression
for the earlier wake bug now passes:

```text
cargo test -p tx-kernel futex_wake_return_reenters_userspace_without_mailbox_event -- --nocapture
```

## Findings

### 1. Fixed: the `pthread_cancel` blocker was signal-action metadata plus sigreturn frame restore, not the old futex wake bug.

musl installs `SIGCANCEL` with:

- `SA_SIGINFO | SA_RESTART | SA_ONSTACK`
- a full `sa_mask`
- a three-argument handler that receives `siginfo_t *` and `ucontext_t *`

In `external/musl/src/thread/pthread_cancel.c`, `cancel_handler` reads
`uc->uc_mcontext.MC_PC` and, when the interrupted PC is inside musl's
cancellation-point window, rewrites it to `__cp_cancel`. On riscv64,
`external/musl/arch/riscv64/pthread_arch.h` defines `MC_PC` as
`__gregs[0]`.

The RV64 HAL frame code is now much closer to the musl ABI: it writes a
Linux-compatible `ucontext_t`, places the handler arguments in `a0/a1/a2`, and
`rt_sigreturn` can restore a modified PC from that ucontext. That part is
covered by the local `ucontext_gregs0_at_linux_abi_offset` and round-trip tests
in `boards/tx-hal-riscv64-qemu-virt/src/signal_frame.rs`.

The original mismatch was above HAL:

- `sys_rt_sigaction` decoded `sa_flags` and `sa_mask` but dropped them, storing
  only `SigDisposition::{Default, Ignore, Handler(addr)}`. On RV64 musl the
  internal `struct k_sigaction` layout is `handler`, `flags`, `mask[2]`,
  `unused`; there is no `SA_RESTORER` field in front of the mask.
- old-action copyout returned only the handler address; flags and mask were
  always zero.
- `AstOutcome::DeliverHandler` carried only `sig` and `handler`.
- `thread_future` built `SignalFrameWrite` with zero `siginfo` and zero
  `flags`, never applied handler `sa_mask`, `SA_NODEFER`, `SA_ONSTACK`, or
  `SA_RESETHAND`, and `rt_sigreturn` resumed the parked pre-handler context
  rather than the user-edited on-stack ucontext.

This violates `txdoc:SIGNAL-SIGACTIONTABLE-1`,
`txdoc:SIGNAL-AST-TRAP-RETURN-SIGNAL-CONTRACT-1`,
`txdoc:SIGNAL-SIGNAL-FRAME-CONSTRUCTION-1`, and
`txdoc:SIGNAL-MASK-COMPUTATION-HANDLER-ENTRY-1`, all of which require
`SigActionEntry`-shaped storage, handler flavor, flags, `sa_mask`, siginfo, and
ucontext/mask restoration semantics.

Fixed in this pass:

- `SigActionTable` now stores `SigActionEntry { disposition, flags, sa_mask,
  restorer }`, with compatibility accessors for disposition-only callers.
- `rt_sigaction` round-trips the pinned RV64 musl layout (`handler`, `flags`,
  `mask`, `unused`) and strips uncatchable bits through `SignalMask`.
- `AstOutcome::DeliverHandler` carries the full action entry.
- `thread_future` computes the handler-entry mask from old mask plus
  `sa_mask`, blocks the delivered signal unless `SA_NODEFER`, uses the
  alternate stack when `SA_ONSTACK` is configured, handles `SA_RESETHAND`,
  passes siginfo/flags to HAL, and clears the consumed siginfo slot.
- `rt_sigreturn` now reads the platform signal frame at the trampoline context
  SP and restores the user-edited ucontext and saved mask. This preserves
  musl's `MC_PC = __cp_cancel` rewrite.
- `tkill` for handler-installed thread-directed signals now posts to the
  requested TID. This matters because musl `pthread_kill` implements
  `pthread_cancel` with `SYS_tkill(t->tid, SIGCANCEL)`.

### 2. `pthread_create` / join fundamentals are now mostly present, with caveats.

musl `pthread_create` issues:

```text
clone(CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD |
      CLONE_SYSVSEM | CLONE_SETTLS | CLONE_PARENT_SETTID |
      CLONE_CHILD_CLEARTID | CLONE_DETACHED,
      stack, ptid, tls, ctid)
```

Current Tx has a CLONE_THREAD path in `sys_clone`, seeds the child stack and TLS
register, writes `CLONE_PARENT_SETTID`, stores `CLONE_CHILD_CLEARTID`, submits
the new thread to the reactor, and `step_thread_exit` performs the
clear-and-futex-wake protocol. This aligns with
`txdoc:PROCESS-STEP-CLONE-THREAD-CLONE-THREAD-PATH-1`,
`txdoc:PROCESS-SET-TID-ADDRESS-1`, and
`txdoc:THREAD-2-6-THREAD-EXIT-NOTIFICATION-CLONE-CHILD-CLEARTID`.

Caveats:

- `CLONE_CHILD_SETTID` is not implemented. musl `pthread_create` does not use
  it, so it is not the current blocker, but the generic musl clone wrapper can
  expose it later.
- The allowed mask for pthread clones includes `CLONE_NEWCGROUP` and
  `CLONE_NEWUTS`; musl pthread does not set those. This is not the immediate
  failure, but it is broader than the active clone-thread spec.
- `set_robust_list` stores state, but robust-list exit processing is partial
  (see below).

### 3. Futex coverage is enough for basic pthread waits, not for the full musl pthread suite.

Tx supports `FUTEX_WAIT` and `FUTEX_WAKE` with a wait-source-backed park/wake
loop. The prior wake-return bug is covered by the passing host test above.

The current implementation deliberately returns `ENOSYS` for `FUTEX_REQUEUE`,
`FUTEX_CMP_REQUEUE`, `FUTEX_WAKE_OP`, PI operations, and bitset variants.
musl's `pthread_cond_timedwait.c` uses `FUTEX_REQUEUE` as the efficient
fallback path in `unlock_requeue`, and condition-variable tests in
`judge_libctest-musl.py` include `pthread_cond`, `pthread_cond_smasher`, and
`pthread_condattr_setclock`.

Impact: `pthread_cancel` should be fixed first because it is the earliest
signal-specific failure, but the broader pthread condition-variable family will
still need at least compatible `FUTEX_REQUEUE` behavior or a tested fallback
story.

### 4. Robust-list compliance is incomplete.

musl probes robust mutex support via `SYS_get_robust_list` in
`pthread_mutexattr_setrobust.c`; Tx has `NR_SET_ROBUST_LIST = 99` but no
`NR_GET_ROBUST_LIST = 100` dispatch. The riscv64 musl syscall header pins both
numbers.

On exit, Tx's `walk_robust_list` handles only `list_op_pending`; it does not
walk the full robust-list chain. musl's robust mutex code maintains the chain in
`pthread_mutex_trylock.c` / `pthread_mutex_unlock.c`, and the OSComp baseline
includes `pthread_robust_detach`.

Impact: not the first `pthread_cancel` blocker, but full pthread robust
coverage is not musl-compliant yet.

### 5. Fixed: RV64 syscall-number compliance had a real membarrier mismatch and a timerfd collision.

`external/musl/arch/riscv64/bits/syscall.h.in` pins:

- `__NR_membarrier = 283`
- `__NR_get_robust_list = 100`
- `__NR_set_robust_list = 99`
- `__NR_futex = 98`

Tx had `NR_MEMBARRIER = 324`, with comments claiming RV64 has no dedicated
slot. That conflicted with the pinned local musl header. musl calls
`__membarrier_init()` during first pthread creation when the symbol is linked,
and dynamic TLS can also use membarrier. Correcting membarrier to 283 also
exposed that Tx had incorrectly assigned `NR_TIMERFD_CREATE = 283`; the pinned
RV64 header defines `__NR_timerfd_create = 85`.

Fixed in this pass: `NR_MEMBARRIER = 283`, `NR_TIMERFD_CREATE = 85`, and host
tests pin both values against the local musl header.

### 6. Fixed: signal wake hints now re-read the thread summary, and production binds the mailbox.

After the signal-action fixes, the cancellation path still had a wait-adapt
hazard: `drive()` treated every `MailboxEvent::SignalDelivered` as an immediate
interrupt. That is too eager for musl cancellation because musl masks
`SIGCANCEL` around internal non-cancellation-point regions such as wrappers that
temporarily disable cancellation; a signal post while masked is only a wake hint
and must force a re-poll of the wait predicate, not return `EINTR`.

The opposite production hazard was also present: `post_signal` posted through
`ThreadPayload::mailbox_handle()`, but the live `PerHartSlotted` task wrapper
was not binding the current reactor task mailbox into the payload. That meant a
pending signal could update `InterruptSummary` but fail to wake a syscall
parked in the task mailbox path.

Fixed in this continuation:

- `drive()` now threads a narrow subject interrupt view into wait-source,
  agent, and timer resolution.
- `SignalDelivered` remains a consumed wake hint. On receipt, the driver
  re-reads the subject thread summary: `deliverable_signal` maps to
  `Interrupted`, `termination` maps to `Killed`, and masked/non-deliverable
  hints map to `Retry`.
- `PerHartSlotted::poll` binds the current reactor task mailbox into the
  running `ThreadPayload` so later `post_signal` calls have a wake destination.
- Host regressions pin both sides: a real-subject `tx-scripts` drive test for a
  masked signal hint, and a `tx-kernel` thread-future test that observes the
  mailbox binding during the task poll.

## Remaining repair order

1. Rerun the dedicated OSComp `libctest-musl` pthread cases after rebuilding
   the guest image, especially `pthread_cancel` and `pthread_cancel_points`.
2. Add or explicitly defer `NR_GET_ROBUST_LIST = 100`; then complete
   robust-list chain walking for `pthread_robust_detach`.
3. Add compatible `FUTEX_REQUEUE` coverage for musl pthread cond paths.

## Tailored pthread follow-up

The final blocker for the static `pthread_cancel` / `pthread_cancel_points`
pair was not signal-frame handling anymore. The live serial showed musl's
`shm_open` path mapping into `/dev/shm/testshm` and failing with `-EROFS`,
which then kept the cancellation point in the error path. The relevant guest
flags were the musl-shaped `O_RDWR|O_CREAT|O_NOFOLLOW|O_CLOEXEC|O_NONBLOCK`
plus the observed large-file bit.

Fix applied in `crates/tx-kernel/src/init.rs`:

- add a retained `DEV_SHM_MOUNT` slot for boot state,
- mount a writable tmpfs at `/dev/shm` after devfs comes up,
- keep the devfs `/dev/shm` directory as the mountpoint stub only,
- mirror the same topology in the boot-wiring host test.

Verification that closed this blocker:

```text
cargo test -p tx-kernel init::tests::boot_wiring_mounts_writable_tmpfs_at_dev_shm_for_musl_shm_open -- --nocapture
cargo test -p tx-kernel init::tests -- --nocapture
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
qemu-system-riscv64 ... -append 'tx.oscomp.groups=libctest-musl'
python3 tools/oscomp-judge.py target/oscomp/os_serial_out_rv_pthread_cancel_devshm_20260522.txt target/oscomp/pthread-cancel-data
```

The guest serial now prints `Pass!` for both `entry-static.exe pthread_cancel`
and `entry-static.exe pthread_cancel_points`, and the judge script reports the
two tailored pthread entries passing.

## Verification run during audit

```text
cargo test -p tx-kernel futex_wake_return_reenters_userspace_without_mailbox_event -- --nocapture
```

Result: passed. This confirms the earlier successful-`FUTEX_WAKE` return path
is not the remaining `pthread_cancel` blocker in this checkout.

## Verification run after fix

```text
cargo fmt --check
cargo test -p tx-subsystems --lib signal::tests -- --nocapture
cargo test -p tx-kernel thread_future::tests -- --nocapture
cargo test -p tx-shims --lib linux_syscall::tests::dispatch_rt_sigaction -- --nocapture
cargo test -p tx-shims --lib linux_syscall::tests::timerfd_dispatch -- --nocapture
cargo check -p tx-shims -p tx-subsystems -p tx-kernel
cargo -q xtask unit
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo xtask progress validate
git diff --check
```

Result: passed. The only repeated warning was the existing
`tx-kernel/src/irq.rs:117 clear_uart_rx_pending` dead-code warning.

Guest `libctest-musl` was not rerun in this pass.

## Verification run after wait-adapt continuation

```text
cargo fmt --check
cargo test -p tx-scripts --test drive -- --nocapture
cargo test -p tx-kernel thread_future::tests -- --nocapture
cargo test -p tx-subsystems --test v3_signal_mailbox -- --nocapture
cargo test -p tx-subsystems --test v3_signal_eligibility -- --nocapture
cargo check -p tx-scripts -p tx-kernel -p tx-substrate -p tx-subsystems
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
timeout 90s cargo xtask oscomp qemu --target rv64-qemu --data target/oscomp/pthread-cancel-data
cargo xtask fault-decode --target rv64-qemu \
  --serial target/oscomp/os_serial_out_rv_pthread_cancel_waitadapt_20260522.txt \
  --all --brief
```

Result: passed. The only repeated warning was the existing
`tx-kernel/src/irq.rs:117 clear_uart_rx_pending` dead-code warning.

The bounded guest attempt booted and saved
`target/oscomp/os_serial_out_rv_pthread_cancel_waitadapt_20260522.txt`, but it
selected `oscomp:groups:default` rather than the libctest pthread payload and
the judge scored `0/0`. `fault-decode` found no trap lines in that serial. This
run is evidence that the rebuilt kernel boots the SD card, not pthread
validation.
