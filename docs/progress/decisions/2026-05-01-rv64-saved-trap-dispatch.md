---
date: 2026-05-01
topic: "RV64 saved trap dispatch spine"
status: complete
---

# RV64 Saved Trap Dispatch Spine

## Decision

RV64 QEMU now has a first saved-register trap dispatch spine. The direct-mode
trap vector saves all integer registers plus `scause`, `sepc`, `stval`, and
`sstatus` into `Rv64TrapFrame`, calls the board binary's named
`tx_kernel_riscv64_qemu_trap_dispatch` symbol, applies the returned
`TrapAction`, restores the frame, and returns with `sret`.

The board binary links the platform crate to `tx-kernel` without adding a
`tx-hal-riscv64-qemu-virt -> tx-kernel` dependency. `tx-hal` exposes the first
`KernelTrapSink<P>` contract with `TrapAction`, `FaultInfo`,
`TrapFrameView`, and `TrapFrameMut`; `tx-kernel::trap::KernelTrapDispatcher`
implements the current kernel sink.

## Current Behavior

- Timer interrupts call the kernel sink and cancel the expired platform
  deadline.
- Reschedule IPIs call the kernel sink and acknowledge `IpiKind::Reschedule`.
- External IRQ traps currently resume as a stub.
- Page faults, syscalls, illegal instructions, breakpoints, alignment faults,
  and unknown traps terminate through the panic path until their semantic
  owners exist.

## Boundary

- `TrapFrameMut` is currently a typed mutable-boundary marker over a view; it
  does not yet support syscall return writes, PC rewrites, signal frame setup,
  or userspace return.
- The userspace trampoline, `return_to_userspace`, ThreadRuntime,
  VM page-fault handling, syscall dispatch, signal policy, and external
  device IRQ dispatch remain later slices.
- The platform crate still owns raw trap entry mechanics. `tx-kernel` owns the
  sink policy. There is no runtime HAL manager or callback table.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 15000`
- Serial log preserved:
  - `txkernel:qemu-riscv64-virt:reactor:ap-runqueue:ok`
  - `txkernel:qemu-riscv64-virt:reactor:timer-idle:ok`
  - `txkernel:qemu-riscv64-virt:boot:ok`
