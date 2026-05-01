---
date: 2026-05-01
topic: "RV64 timer trap idle smoke"
status: complete
---

# RV64 Timer Trap Idle Smoke

## Decision

RV64 QEMU now handles supervisor timer interrupts in the executable direct-mode
trap vector. The handler cancels the expired SBI timer deadline and returns to
kernel code. `TimeIf` now includes `enable_timer_wakeups()` so CoreInit can
prepare a hart for deadline-driven idle without taking ownership of scheduler
policy in HAL.

CoreInit uses this path in a bounded boot smoke. It submits a timeout waiter to
`BOOT_REACTOR`, steps the shared reactor once to arm the platform deadline,
waits with WFI, then steps the reactor again after the timer trap returns. The
smoke emits:

```text
txkernel:qemu-riscv64-virt:reactor:timer-idle:ok
```

## Why

The previous CoreInit hart-loop adapter could program deadlines, but RV64 timer
interrupts still fell into the panic path. This left the production idle loop
blocked on a platform timer return path. The new slice keeps policy in
CoreInit/reactor and adds only the low-level HAL mechanics needed to wake a hart
from a programmed deadline.

## Boundary

- This is a direct-mode boot smoke, not the final saved-register trap shell.
- The timer handler only cancels the expired platform deadline and returns.
- Reactor timeout observation still happens in the next `hart_loop` step.
- Scheduler tick/preemption policy, external IRQ/device dispatch, final
  per-hart reactor sharding, userspace traps, and `return_to_userspace` remain
  later work.

## Verification

- `cargo fmt --check`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 15000`
- Serial log included:
  - `txkernel:qemu-riscv64-virt:reactor:runtime-loop:ok`
  - `txkernel:qemu-riscv64-virt:reactor:timer-idle:ok`
  - `txkernel:qemu-riscv64-virt:boot:ok`
