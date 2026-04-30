# Reactor Third-Wave Dispatcher Scout

**Date:** 2026-04-30

**Scope:** prepare the next reactor dispatch shard after the first two worker
waves added task lifecycle, scheduler shell, wait/bus, timer idle,
interrupt classification, AST markers, completion, and sync rendezvous.

## Inputs

- Active worktree leases were rechecked with
  `cargo xtask progress list worktrees --json`. The old
  `2026-04-29-zone-ebr-integration` record was stale: its worktree path no
  longer exists and `git worktree list --porcelain` reports only the main
  checkout. The record is now marked `merged`.
- `REACTOR_v0` still names three gaps before full userspace-runtime work:
  poll-boundary AST/preemption consumption, HAL-driven reactor/timer loop
  integration, and userspace-run.
- `THREAD_RUNTIME_v1` keeps signal delivery split across wait-adapt site A and
  AST site B. The next reactor pass must stop at mechanism and must not route
  POSIX signals or touch `ThreadPayload`.
- `BUS_v1` still wants typed declarations, terminal wire cleanup, and stronger
  RawQueue/RawPort subscriber behavior. The substrate lease is now clear.
- The current `tx-reactor` crate still lacks the accepted `yield_now`
  cooperative yield helper from the serial API decision.

## Dispatch Decision

The next pass should dispatch four lanes. These are isolated enough for
parallel subagent development if workers obey the write scopes in
`docs/progress/plans/2026-04-30-reactor-third-wave-dispatch.json`.

1. **reactor-yield-now**
   - Owns `crates/tx-reactor/src/lib.rs`,
     `crates/tx-reactor/src/yield_now.rs`, and
     `crates/tx-reactor/tests/yield_now.rs`.
   - Implements a no-alloc cooperative `yield_now()` future: first poll wakes
     the current task and returns `Pending`; the next poll returns `Ready`.
   - Does not change scheduler policy, timer handling, task status enums, or
     userspace-run behavior.

2. **reactor-ast-poll-boundary**
   - Owns `crates/tx-reactor/src/task.rs`,
     `crates/tx-reactor/src/runtime.rs`, and
     `crates/tx-reactor/tests/ast_runtime.rs`.
   - Stores an `AstSlot` on each task, exposes reactor-local queue/consume
     entry points, and consumes pending AST batches at poll boundaries.
   - Does not deliver POSIX signals, build signal frames, inspect
     `ThreadPayload`, or modify wait interruption policy.

3. **kernel-coreinit-hal-clock**
   - Owns `crates/tx-kernel/src/init.rs` and
     `crates/tx-reactor/tests/timer_idle.rs` only if a host-side expectation
     adjustment is needed.
   - Wires the boot reactor smoke through the existing
     `run_until_idle_with_clock` closure surface using `P: TimeIf`, preserving
     current boot and reactor sentinels.
   - Does not implement the permanent WFI loop, scheduler/process init, device
     init, or userspace entry.

4. **substrate-bus-wire-hardening**
   - Owns `crates/tx-substrate/src/bus/mod.rs`,
     `crates/tx-substrate/tests/bus.rs`, and any focused adjustments needed in
     `crates/tx-reactor/tests/wait_bus.rs`.
   - Adds focused RawQueue/RawPort hardening: observable subscriber validity,
     terminal/gone notification behavior, and stronger tests for update,
     unsubscribe, and terminal fire semantics.
   - Does not make substrate depend on reactor, add semantic readiness
     predicates, implement epoll, or add SMP/epoch-backed subscriber storage.

## Deferred

- **userspace-run** remains a later pass. It still crosses HAL saved-register
  trap frames, return-to-userspace, `ThreadPayload.regs`, AST signal delivery,
  and interesting-trap resolution.
- **real SMP shootdown coordination** remains later. The reactor-local
  `SyncRendezvous` shape exists, but substrate must not gain a dependency on
  `tx-reactor`; a higher VM/kernel coordination layer needs to own that bridge.
- **long-running kernel idle/WFI loop** remains later. The CoreInit lane should
  only move the current smoke task onto the HAL-clock surface.

## Worker Prompt Skeletons

Each worker must be told that it is not alone in the codebase and must not
revert edits outside its write scope.

### reactor-yield-now

Implement the accepted cooperative yield helper for `tx-reactor`.

Write scope:
- `crates/tx-reactor/src/lib.rs`
- `crates/tx-reactor/src/yield_now.rs`
- `crates/tx-reactor/tests/yield_now.rs`

Requirements:
- Add a public `yield_now() -> impl Future<Output = ()>` helper, preferably
  backed by a small named future type.
- First poll must call `cx.waker().wake_by_ref()` and return `Pending`.
- A later poll must return `Ready(())`.
- Add tests proving a task can yield once and then complete, and that two
  yielding tasks both make progress without changing scheduler policy.
- Run `cargo test -p tx-reactor --test yield_now`.

### reactor-ast-poll-boundary

Wire the existing AST marker mechanism into reactor task state.

Write scope:
- `crates/tx-reactor/src/task.rs`
- `crates/tx-reactor/src/runtime.rs`
- `crates/tx-reactor/tests/ast_runtime.rs`

Requirements:
- Store an `AstSlot` per live task.
- Add reactor-local queue and consume APIs using `TaskKey` so stale handles are
  rejected consistently with existing lifecycle methods.
- Consume pending AST markers at poll boundaries before polling the task
  future, while preserving marker coalescing and task locality.
- Keep this mechanism-only: no signal routing, no handler-frame construction,
  no `ThreadPayload`.
- Run `cargo test -p tx-reactor --test ast_runtime`.

### kernel-coreinit-hal-clock

Move CoreInit's current reactor smoke task onto the HAL-shaped clock driver.

Write scope:
- `crates/tx-kernel/src/init.rs`
- `crates/tx-reactor/tests/timer_idle.rs` only if a focused expectation change
  is needed

Requirements:
- Use `Reactor::run_until_idle_with_clock` in `run_reactor_smoke_task`.
- Source `now` from `P::read_ns()`.
- Program or cancel the current hart deadline through `P::set_deadline_ns` and
  `P::cancel_deadline`.
- Preserve the existing `txkernel:<board>:reactor:task:ok` and
  `txkernel:<board>:boot:ok` sentinel order.
- Do not introduce a permanent WFI loop or device/process/userspace init.
- Run `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`.

### substrate-bus-wire-hardening

Harden the first-slice bus primitives without changing dependency direction.

Write scope:
- `crates/tx-substrate/src/bus/mod.rs`
- `crates/tx-substrate/tests/bus.rs`
- `crates/tx-reactor/tests/wait_bus.rs` only for focused compatibility
  adjustments

Requirements:
- Add explicit subscriber validity/terminal state tests for both RawQueue and
  RawPort.
- Add terminal/gone behavior appropriate to first-slice untyped masks/events:
  terminal fires wake subscribers and later operations report unsubscribed or
  terminal state predictably.
- Preserve no semantic readiness predicates in the bus.
- Do not add a `tx-reactor` dependency to `tx-substrate`.
- Run `cargo test -p tx-substrate` and
  `cargo test -p tx-reactor --test wait_bus`.

## Verification

Scout/pre-dispatch verification:

- `git worktree list --porcelain`
- `cargo xtask progress list worktrees --json`
- `cargo xtask progress validate`
- targeted reads of `REACTOR_v0`, `SCHEDULER_v0`, `THREAD_RUNTIME_v1`,
  `BUS_v1`, current `tx-reactor`, `tx-kernel::init`, and bus code.

Expected post-merge verification for this third wave:

- `cargo fmt --check`
- `cargo test -p tx-reactor --test yield_now`
- `cargo test -p tx-reactor --test ast_runtime`
- `cargo test -p tx-substrate`
- `cargo test -p tx-reactor`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask lint arch`
- `cargo xtask lint unused`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `cargo xtask ci`
- `git diff --check`
