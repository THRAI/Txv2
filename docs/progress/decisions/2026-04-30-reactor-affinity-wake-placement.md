---
date: 2026-04-30
topic: "Reactor scheduler affinity and wake placement"
status: complete
---

# Decision: Reactor Scheduler Affinity And Wake Placement

## Context

The SMP bring-up slice now has a platform `SmpIf`, AP boot, AP-local substrate
initialization, RFENCE-backed remote pmap invalidation, and low-level IPI
send/ack smoke coverage. The reactor side still needed a narrow scheduler
decision surface that can say where a wake should land without making
`tx-reactor` depend on a concrete HAL or sending IPIs from policy code.

## Decision

`Phase1Scheduler` now honors initial affinity masks for task submission and
wake placement. A zero mask normalizes to the boot hart. Wakes prefer the
task's last hart when that hart is still allowed by the initial mask; otherwise
they fall back to the first allowed hart.

The scheduler exposes `RunnablePlacement { target_hart, wake_remote }` through
`task_runnable_from(task, hint, current_hart)`. This reports the selected
runqueue and whether the reactor dispatcher should notify another hart. It
intentionally does not send the IPI; dispatcher and kernel runtime code
translate `wake_remote` through `SmpIf::send_ipi`.

`Reactor::submit_task_with_meta` now lets the reactor submit a task with
explicit `InitialSchedMeta`, including affinity. The existing
`Reactor::submit_task` path remains kernel-cooperative by default.

## Changed Surface

- `crates/tx-reactor/src/scheduler.rs`
  - `InitialSchedMeta::with_affinity`
  - `RunnablePlacement`
  - affinity-aware initial queue selection
  - placement-returning wake helper
- `crates/tx-reactor/src/runtime.rs`
  - `Reactor::submit_task_with_meta`
- `docs/design/02_execution/SCHEDULER_v0.md`
  - Phase 1 now documents initial affinity placement and remote wake reporting.

## Verification

```text
cargo test -p tx-reactor --test scheduler
cargo test -p tx-reactor --test reactor_smoke
cargo test -p tx-reactor
cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo fmt --check
cargo xtask progress validate
cargo xtask lint docs
cargo xtask lint arch
cargo xtask lint unused
cargo xtask ci
git diff --check
```

Result: all checks pass. Docs lint still reports the repo's existing 31
retired-vocabulary warnings and treats them as warnings.

## Follow-up Status

The next-step dispatcher bridge was implemented in
`docs/progress/decisions/2026-04-30-reactor-reschedule-dispatch-bridge.md`,
and AP wake-loop smoke coverage was implemented in
`docs/progress/decisions/2026-04-30-ap-reactor-loop-wfi-smoke.md`. Real
AP-side shared-reactor runqueue draining was then implemented in
`docs/progress/decisions/2026-04-30-ap-reactor-shared-runqueue-smoke.md`.

## Blockers

No blocker for the dispatcher shard. Remaining production gaps are bus
trace subscriber/nop-patching runtime, concrete VFS/device owner
implementations, timer/IRQ delivery into the runtime loop, userspace
trap/thread runtime integration, and replacing the temporary shared reactor
lock with production per-hart/runtime ownership.
