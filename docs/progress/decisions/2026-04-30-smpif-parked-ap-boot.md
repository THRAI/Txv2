# Decision: SmpIf First, Parked AP Boot Next

**Date:** 2026-04-30

## Context

Reactor and subsystem work needed a real low-level SMP boundary before moving
toward multi-hart runtime behavior. The existing HAL surface had only a
placeholder `SmpIf`; RV64 QEMU did not discover possible CPUs, boot APs, or
prove that reactor/bus wait storage could cross hart boundaries.

## Decision

Set up the HAL `SmpIf` API first, then wire RV64 QEMU to boot secondary harts
into a parked AP path after BSP substrate init and full trap-vector
installation. This pass proves low-level AP startup and online publication, but
does not claim scheduler multi-hart execution, IPI reschedule policy,
kernel-managed TLB shootdown, or a permanent interrupt-driven WFI loop.

## Changed Surface

- `tx-hal` now exposes `CpuMask`, `SecondaryEntry`, `IpiKind`, online/possible
  CPU masks, AP online publication, AP boot, parking, and IPI send/ack hooks.
- `PlatformInfo` now carries `possible_cpu_count`.
- RV64 QEMU parses CPU nodes from the DTB, runs QEMU smoke with `-smp 4`,
  starts APs through SBI HSM, installs per-hart `tp`, marks APs online, and
  parks them.
- RV64 QEMU reserves separate temporary boot-stack slots for BSP/AP harts so a
  nonzero boot hart cannot collide with AP hart 0.
- `tx-kernel::CoreInit` boots APs after substrate init and kernel trap-vector
  installation, before the existing zone/reactor smoke.
- Reactor/bus storage moved from `Rc<RefCell<...>>` to `Arc` plus spin-locked
  state for raw bus wires, timer waits, and sync rendezvous coordination.

## Verification

- `cargo test -p tx-substrate --test bus`
- `cargo test -p tx-reactor --test wait_bus`
- `cargo test -p tx-reactor --test timer_idle`
- `cargo test -p tx-reactor --test sync_coord`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- ELF inspection confirmed `tx_rv64_qemu_secondary_start` in
  `.text.trampoline`.
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel
  --timeout-ms 15000`

The final QEMU serial log showed `Platform HART Count : 4`,
`Boot HART ID : 3`,
`txkernel:qemu-riscv64-virt:smp:aps:online`,
`txkernel:qemu-riscv64-virt:reactor:task:ok`, and
`txkernel:qemu-riscv64-virt:boot:ok`.

## Next Step

Do not dispatch VFS/device/block runtime as production-ready yet. The next
dispatcher shard should finish bus declaration hardening beyond the first
typed queue/port wrapper surface and add concrete VFS/device owner
implementations over embedded wires, then bridge low-level IPI mechanics into
scheduler reschedule and TLB shootdown protocols. The permanent
interrupt-driven reactor/WFI loop remains a separate kernel-runtime slice.

## Blockers

None for parked AP boot. Remaining blockers for full production SMP runtime are
trace subscriber/nop-patching runtime, concrete VFS/device owner
implementations, scheduler/IPI integration, shootdown coordination, and the
permanent interrupt loop.
