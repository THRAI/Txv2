---
date: 2026-05-01
topic: "CoreInit hart-loop adapter"
status: complete
---

# CoreInit Hart-Loop Adapter

## Decision

`tx_kernel::CoreInit` now owns the first HAL binding for the reactor per-hart
loop shell. The adapter steps `BOOT_REACTOR` through
`tx_reactor::hart_loop::step_hart_loop_at`, supplies the current hart and
monotonic time from the selected `P: TxPlatform`, maps
`HartLoopDeadlineAction` to `TimeIf::{set_deadline_ns,cancel_deadline}`, and
keeps remote reschedule IPI delivery behind the existing
`SmpRescheduleSignal<P>`.

APs use this bounded step before entering WFI. The BSP smoke task also runs on
`BOOT_REACTOR` through the same adapter, then emits
`txkernel:<board>:reactor:runtime-loop:ok` before the existing boot sentinel.

## Why

The previous AP loop called a marker-gated reactor runner directly and the BSP
smoke used a separate local reactor plus callback clock. That proved pieces in
isolation, but it did not exercise the production seam where CoreInit binds
the platform HAL to the reactor's per-hart step report.

## Boundary

- This is a bounded boot/runtime smoke, not the permanent BSP root runtime
  loop.
- Timer deadline programming is wired, but full timer-trap return and device
  IRQ dispatch are still later slices.
- `tx-reactor` remains platform-independent; WFI, timer hardware, IPI
  acknowledgement, and trap-vector mechanics stay in HAL/CoreInit.
- VM, ThreadRuntime, signal policy, userspace trap classification, and real
  `return_to_userspace` remain outside this slice.

## Verification

- `cargo fmt --check`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 15000`
- Serial log included:
  - `txkernel:qemu-riscv64-virt:reactor:ap-runqueue:ok`
  - `txkernel:qemu-riscv64-virt:reactor:task:ok`
  - `txkernel:qemu-riscv64-virt:reactor:runtime-loop:ok`
  - `txkernel:qemu-riscv64-virt:boot:ok`
