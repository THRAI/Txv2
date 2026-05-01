---
date: 2026-04-30
topic: "Reactor reschedule dispatch bridge"
status: complete
---

# Decision: Reactor Reschedule Dispatch Bridge

## Context

`Phase1Scheduler` can now report `RunnablePlacement { target_hart,
wake_remote }`, but the scheduler must remain policy-only. The missing slice
was a mechanism boundary that records target-hart reschedule state and lets a
HAL-bound runtime send the actual reschedule IPI.

## Decision

`tx-reactor` now has a platform-independent dispatch bridge:

- `DispatchState` owns per-hart `PreemptionPoint` markers.
- `DispatchState::apply_runnable_placement` marks the target hart
  `need_resched`.
- `RescheduleSignal` is the narrow adapter trait for remote notification.
- `NoopRescheduleSignal` keeps host and single-hart paths platform-free.
- `WakeDispatchReport` records local reschedule and remote IPI counts for tests
  and future instrumentation.

`Reactor::drain_wakes_for_hart` now consumes task-local wake requests through
`Phase1Scheduler::task_runnable_from`, applies the resulting placement to
dispatch state, and invokes the provided `RescheduleSignal` only when the wake
targets a remote hart. `run_until_idle_on_hart` provides a logical per-hart
host path without starting the permanent AP loop yet.

The generic kernel runtime now has `SmpRescheduleSignal<P>`, a small adapter
that implements `tx_reactor::RescheduleSignal` by calling
`SmpIf::send_ipi(CpuId(target_hart), IpiKind::Reschedule)`.

## Changed Surface

- `crates/tx-reactor/src/dispatch.rs`
  - `DispatchState`
  - `RescheduleSignal`
  - `WakeDispatchAction`
  - `WakeDispatchReport`
- `crates/tx-reactor/src/runtime.rs`
  - `run_until_idle_on_hart`
  - `run_until_idle_on_hart_with_reschedule`
  - `drain_wakes_for_hart`
  - `dispatch_markers`
  - `consume_dispatch_markers`
- `crates/tx-kernel/src/init.rs`
  - `SmpRescheduleSignal<P>`
  - RV64 QEMU smoke path for a wait-channel wake dispatched as a remote
    reschedule IPI

## Verification

```text
cargo test -p tx-reactor --test dispatch
cargo test -p tx-reactor --test reactor_smoke
cargo test -p tx-reactor
cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 15000
```

QEMU serial includes:

```text
txkernel:qemu-riscv64-virt:reactor:dispatch:ipi:ok
```

## Next Step

Superseded by
`docs/progress/decisions/2026-04-30-ap-reactor-loop-wfi-smoke.md`: APs can now
wake from `IpiKind::Reschedule` and consume a bounded kernel work marker. The
marker follow-up was then superseded by
`docs/progress/decisions/2026-04-30-ap-reactor-shared-runqueue-smoke.md`: APs
now consume the target hart's reactor `need_resched` marker and drain a real
shared-reactor runqueue.

## Blockers

No blocker for the next SMP dispatcher loop. Remaining production gaps include
real AP-side `tx-reactor` task execution, full bus declaration hardening beyond
the first queue/port wrappers, concrete VFS/device owner implementations over
embedded wires, kernel-managed shootdown fallback, timer/IRQ delivery into the
runtime loop, and userspace trap/thread-runtime integration.
