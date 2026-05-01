---
date: 2026-04-30
topic: "AP reactor loop WFI smoke"
status: complete
---

# Decision: AP Reactor Loop WFI Smoke

## Context

The reactor dispatch bridge could mark a remote hart `need_resched` and send
`IpiKind::Reschedule`, but the receiving AP only acknowledged the SSIP from the
trap path and returned to a permanent park loop. The next slice needed to prove
that a kernel-owned AP loop can wake from the IPI and run kernel-side work
without moving scheduler or reactor policy into HAL.

## Decision

`SmpIf` now exposes three low-level primitives in addition to send/ack:

- `enable_ipi_wakeups()`
- `wait_for_interrupt_once()`
- `pending_ipi(kind)`

These are hardware mechanics, not scheduler hooks. RV64 QEMU uses them to arm
SSIP wakeups, wait with `wfi`, and observe pending SSIP state. The generic
kernel AP entry now marks the CPU online and then enters
`CoreInit::secondary_reactor_loop` instead of calling `park_this_cpu()`.

The AP loop owns the wake discipline:

- Check a kernel-owned AP reactor work bit.
- If no work is present, wait once for an interrupt.
- Poll pending `IpiKind::Reschedule` state and acknowledge it.
- Loop back to consume the published work bit.

The dispatcher smoke path now publishes a bounded AP work marker before draining
the wait-channel wake through `SmpRescheduleSignal<P>`. The remote AP wakes,
acknowledges the reschedule IPI, consumes the marker, and records completion.
RV64 QEMU prints:

```text
txkernel:qemu-riscv64-virt:reactor:ap-loop:ok
```

## Boundary

This is not real AP-side `tx-reactor` task execution yet. The AP consumes a
kernel-owned work marker that is coupled to the reschedule IPI smoke; it does
not borrow or share the BSP's `Reactor`, drain a real AP runqueue, deliver timer
events, run userspace, or process production device IRQ completions.

The HAL remains policy-free: it exposes wake/pending/wait mechanics, while the
kernel loop decides when to check work, when to sleep, and what a reschedule IPI
means.

## Changed Surface

- `crates/tx-hal/src/lib.rs`
  - `SmpIf::enable_ipi_wakeups`
  - `SmpIf::wait_for_interrupt_once`
  - `SmpIf::pending_ipi`
- `boards/tx-hal-riscv64-qemu-virt/src/lib.rs`
  - RV64 SSIP wake/pending support and `wfi` wait primitive
- `crates/tx-kernel/src/init.rs`
  - `CoreInit::secondary_reactor_loop`
  - AP reactor work/done smoke bitmaps
  - RV64 QEMU dispatcher smoke waits for AP-loop completion

## Verification

```text
cargo fmt --check
cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo test -p tx-hal-riscv64-qemu-virt
cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 15000
```

QEMU serial includes:

```text
txkernel:qemu-riscv64-virt:smp:ipi:ok
txkernel:qemu-riscv64-virt:reactor:dispatch:ipi:ok
txkernel:qemu-riscv64-virt:reactor:ap-loop:ok
```

## Follow-up Status

Superseded by
`docs/progress/decisions/2026-04-30-ap-reactor-shared-runqueue-smoke.md`: APs
now consume the reactor `need_resched` marker and drain a real shared-reactor
runqueue after the reschedule IPI. The remaining work is no longer "replace the
marker"; it is to split the temporary shared reactor lock into production-grade
per-hart/runtime ownership and to harden bus subscription lifetimes.

## Blockers

Real production runtime still needs full bus declaration hardening beyond the
first queue/port wrappers, concrete VFS/device owner implementations over
embedded wires, kernel-managed shootdown fallback, timer/IRQ delivery into the runtime
loop, userspace trap/thread-runtime integration, and a reactor state model that
is safe to share or shard across harts.
