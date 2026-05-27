# Decision: RV64 Parked-AP IPI Acknowledgement Smoke

**Date:** 2026-04-30

## Context

After parked AP boot, AP-local substrate init, and RFENCE-backed pmap
shootdown, the next SMP runtime risk was that `SmpIf::send_ipi` existed but no
parked AP path could actually receive and acknowledge a supervisor software
interrupt. Without that, reschedule IPIs and any later kernel-managed shootdown
fallback would have no executable receive path.

## Decision

Teach the RV64 QEMU kernel trap vector to handle supervisor software
interrupts as low-level IPIs. The trap vector now preserves caller-saved
registers, calls back into `SmpIf::ack_ipi(IpiKind::Reschedule)`, restores the
interrupted context, and returns with `sret`. Parked APs enable supervisor
software interrupts before entering their WFI loop.

The generic `SmpIf` surface now includes a small acknowledgement bitmap API
(`clear_ipi_ack_cpus`, `ipi_ack_cpus`, `wait_for_ipi_ack_cpus`) so the generic
CoreInit smoke can send `IpiKind::Reschedule` to online APs and require
acknowledgement before continuing. This is still mechanism-level; scheduler
policy and run-queue admission remain outside HAL.

## Changed Surface

- RV64 QEMU trap entry handles supervisor software interrupts without panicking.
- RV64 parked APs enable SSIE/SIE before WFI.
- RV64 `ack_ipi` records the current CPU in an acknowledgement bitmap and
  clears `sip.SSIP`.
- `tx_kernel::CoreInit` emits `txkernel:qemu-riscv64-virt:smp:ipi:ok` after
  all online remote APs acknowledge the smoke reschedule IPI.
- `HAL_v1.md` documents the acknowledgement bitmap as low-level mechanism, not
  scheduling policy.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel
  --timeout-ms 15000`

The QEMU serial log showed `Platform HART Count : 4`, `Boot HART ID : 2`,
`txkernel:qemu-riscv64-virt:smp:aps:online`,
`txkernel:qemu-riscv64-virt:smp:shootdown:ok`,
`txkernel:qemu-riscv64-virt:smp:ipi:ok`,
`txkernel:qemu-riscv64-virt:reactor:task:ok`, and
`txkernel:qemu-riscv64-virt:boot:ok`.

## Next Step

The next production-SMP slice should connect this low-level IPI receive path to
the reactor/scheduler idle loop, so a remote wake can move from bus event to
run-queue admission to target-hart wakeup.

## Blockers

No blocker for parked-AP IPI acknowledgement. Full scheduler IPI semantics,
timer interrupt dispatch, and a permanent interrupt-driven idle loop remain
pending.
