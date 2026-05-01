# Reactor First-Wave Worker Audit

**Date:** 2026-04-30

**Scope:** first-wave reactor worker merge from
`docs/progress/plans/2026-04-30-reactor-parallel-shards.json`.

## Worker Results

- Task lifecycle added `TaskTable`, generation-checked `TaskKey`, stale-handle
  rejection, `cancel_task`, `drain_completed`, `drain_cancelled`, and task
  wake draining under `crates/tx-reactor/src/task.rs`.
- Scheduler added direct Phase 1 policy queries/tests and tightened
  `StopReason::Yielded` behavior: fair tasks reset budget into the preempted
  queue; kernel-only tasks stay cooperative.
- Wait/bus added minimal `RawQueue` / `RawPort` mechanics and moved
  `Channel` wake delivery to a bus-backed port. `wait_event` now uses the
  check/register/recheck/park shape before sleeping and rechecks after every
  wake.
- Timer/idle added `run_until_idle_with_clock` and `RunIdleReport`, preserving
  host-driven `advance_time_to` while giving CoreInit a future HAL `TimeIf`
  adapter shape.

## Coordinator Audit Decisions

- Integrated `TaskTable` into `Reactor` instead of leaving it as an isolated
  test-only helper. `Reactor::submit` still returns legacy `TaskId` for smoke
  compatibility; `Reactor::submit_task` returns generation-safe `TaskKey`.
- Added `Reactor::cancel_task`, `drain_completed`, and `drain_cancelled`; these
  delegate to `TaskTable` and keep scheduler metadata in sync through
  `task_dropped`.
- Replaced the worker's path-included substrate bus module with a real
  `tx-substrate` dependency and `tx_substrate::bus` import. `crates/tx-substrate/src/lib.rs`
  now wires `pub mod bus;` so tests exercise the public substrate module.
- Added coordinator integration tests proving reactor-level cancel/drain
  rejects stale keys and reuses slots only under a new generation.

## Residual Gaps

- `RawQueue` / `RawPort` are still minimal host-testable primitives in this
  first-wave snapshot: no typed declaration macros, no SMP/epoch-protected
  subscriber storage, no terminal wire destruction handshake, and no epoll
  long-lived subscription graph. Later SMP/bus follow-ups add SMP-safe raw
  storage, typed queue/port wrappers, and an epoch-fenced terminal/drain retire
  handshake, bounded subscription graph, and owner-storage EBR fence, but not
  the final epoll graph or concrete VFS/device owner implementations.
- Timer/idle has a HAL-shaped callback adapter but is not wired into
  `CoreInit`; `crates/tx-kernel/src/init.rs` remains deferred because another
  progress worktree still records that path in its active lease.
- Interruptible/killable wait classification, AST delivery, userspace-run, and
  synchronous cross-hart coordination remain later reactor waves.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-reactor` (43 tests)
- `cargo test -p tx-substrate`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask lint arch`
- `cargo xtask lint unused`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `cargo xtask ci`
- `git diff --check`

All listed commands passed. `cargo xtask lint docs` continues to report the
the then-current docs lint warning policy.
