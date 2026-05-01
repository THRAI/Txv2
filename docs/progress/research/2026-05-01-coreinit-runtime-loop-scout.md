---
date: 2026-05-01
topic: "CoreInit runtime-loop integration scout"
status: complete
plan: docs/progress/plans/2026-05-01-reactor-runtime-dispatch.json
---

# CoreInit Runtime-Loop Scout

## Current state

- `crates/tx-kernel/src/init.rs` still has a finite BSP boot path:
  `CoreInit<P>::boot()` initializes the platform/substrate, installs the
  kernel trap vector, initializes `BOOT_REACTOR`, boots APs, runs smokes,
  prints the boot sentinel, and powers off.
- `BOOT_REACTOR` is the current shared reactor instance. `SmpRescheduleSignal<P>`
  is already the kernel-owned bridge from `tx_reactor::RescheduleSignal` to
  `<P as tx_hal::SmpIf>::send_ipi(..., IpiKind::Reschedule)`.
- APs enter `secondary_reactor_loop()`: enable IPI wakeups, run
  `run_rescheduled_on_hart_with_reschedule()` when the target hart has a
  reactor `need_resched` marker, otherwise execute one HAL `WFI` wait and ack a
  pending reschedule IPI. This is a smoke loop, not the final runtime loop.
- `crates/tx-reactor/src/runtime.rs` already has useful pieces:
  `run_until_idle_with_clock()` for timer-queue advancement plus deadline
  programming, `run_rescheduled_on_hart_with_reschedule()` for marker-gated
  AP work, and `drain_wakes_for_hart()` for remote wake placement.
- `crates/tx-reactor/src/dispatch.rs` keeps the right ownership split:
  reactor dispatch records per-hart preemption markers and asks an outer
  `RescheduleSignal` to send remote IPIs. It does not call HAL directly.
- `tx-hal` exposes the needed low-level axes: `TimeIf` for monotonic time and
  per-hart deadlines, `SmpIf` for CPU identity, WFI, and IPIs, `IrqIf` for
  external IRQ state, and `TrapIf` for trap-vector install/classification.
- RV64 QEMU implements `TimeIf` and `SmpIf`, and classifies timer, external,
  and IPI traps. Its executable direct-mode trap vector now saves a full
  integer-register frame and routes through the first `KernelTrapSink` spine.
  Timer and IPI traps resume through the sink; external IRQ is still a resume
  stub; sync/syscall/user-facing traps still terminate until their owners
  exist. Full mutable trap-frame writeback and user return are still not
  implemented.

## Proposed integration seam

The next seam should be a `CoreInit<P>` runtime-loop adapter over the reactor's
per-hart loop shell. `tx-reactor` should stay platform-independent and return
mechanism reports such as ran work, consumed reschedule markers, timer wakes,
next deadline, and idle. `CoreInit<P>` should bind those reports to the static
HAL surface: `TimeIf` for clock/deadline programming, `SmpIf` for WFI/IPI
ack/send, and `TrapIf` only for timer/IPI classification and return-to-kernel
dispatch.

Ownership boundary:

- HAL/board owns raw trap entry, hardware timer programming, WFI, IPI
  send/ack, and IRQ-controller mechanics.
- `tx-kernel::CoreInit` owns boot/runtime sequencing and the HAL-to-reactor
  adapter for the selected `P: TxPlatform`.
- `tx-reactor` owns task state, wait/wake, timers, scheduler placement, and
  per-hart dispatch markers.
- ThreadRuntime, VM, syscall, signal, and real return-to-userspace remain
  outside this slice.

## Minimal next slice

1. Wait for the userspace-run and per-hart loop shells to land, then freeze the
   coordinator-approved `tx_reactor::hart_loop` API before editing
   `CoreInit`.
2. Add a private CoreInit adapter around `BOOT_REACTOR` that supplies:
   current `HartId` from `SmpIf::current_cpu_id()`, clock reads from
   `TimeIf::read_ns()`, deadline programming through
   `TimeIf::{set_deadline_ns,cancel_deadline}`, and remote reschedule through
   the existing `SmpRescheduleSignal<P>`.
3. Replace `run_secondary_reactor_once()` with the per-hart loop step API.
   The AP outer loop should only interpret the step result: continue when work
   ran, otherwise idle with `SmpIf::wait_for_interrupt_once()` and acknowledge
   pending reschedule IPIs.
4. Add a BSP bounded runtime-loop smoke using the same adapter before changing
   the boot tail to an infinite production loop. This preserves the existing
   `boot:ok` poweroff smoke while proving the production seam.
5. Add only the trap dispatch needed for timer/IPI runtime progress. IPI traps
   should acknowledge the reschedule source and return to the loop; timer traps
   should return to the loop so the next step can advance reactor timers and
   program the next deadline. If the per-hart shell lacks a way to record a
   timer-triggered current-hart reschedule marker, that is the only reactor API
   request this slice should raise.
6. Keep external IRQ dispatch as a later device-runtime slice. Unknown sync
   traps and userspace traps can keep the current panic/unsupported behavior
   until the full trap sink exists.
7. After root runtime tasks exist, a later slice can replace the BSP poweroff
   tail with the permanent CoreInit runtime loop.

## Tests/smokes

- `cargo test -p tx-reactor --test hart_loop` once the per-hart shell tests
  exist.
- `cargo test -p tx-reactor --test timer_idle`
- `cargo test -p tx-reactor --test dispatch`
- `cargo test -p tx-hal-riscv64-qemu-virt` for timer/IPI/external trap
  classification and HAL time/SMP regressions.
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 15000`
  preserving current AP online, IPI, reactor dispatch, AP loop/runqueue,
  reactor task, and boot sentinels; add at most one new bounded
  runtime-loop/timer sentinel for this slice.
- `cargo xtask progress validate`

## Blockers/non-goals

- Blocker: the per-hart loop shell API is the prerequisite. `CoreInit` should
  not invent a parallel step contract.
- Blocker: the current executable RV64 trap vector now has a saved-register
  sink spine. Mutable trap-frame writeback, scheduler tick policy, external IRQ
  dispatch, and userspace return remain pending.
- Blocker: there are no root runtime tasks yet, so switching the BSP tail
  directly to an infinite loop would break the current smoke contract.
- Non-goal: VM address spaces, page-fault handling, syscall dispatch,
  `ThreadRuntime`, signal policy, saved-register `ThreadPayload.regs`, and
  real `return_to_userspace`.
- Non-goal: external IRQ/device/block completion routing, final per-hart
  reactor sharding, priority-aware remote preemption, and load balancing.
