# Reactor Second-Wave Worker Audit

**Date:** 2026-04-30

**Scope:** second-wave reactor worker merge from
`docs/progress/plans/2026-04-30-reactor-parallel-shards.json`.

## Worker Results

- Wait-interrupt classification added `InterruptSource`,
  `InterruptSummary`, `AtomicInterruptSummary`, `NoInterrupts`, and
  `Channel::wait_event_with_interrupts`. `wait_event` keeps the existing
  no-interrupt path through `NoInterrupts`.
- AST/preemption added task-local `AstSlot` / `AstMarker` marker batching and
  atomic `PreemptionPoint` marker consumption for poll-boundary reschedule
  decisions.
- Completion middleware added counted `Completion` and closed-set
  `CountdownCompletion` over the existing wait-event adapter.
- Sync coordination added `SyncRendezvous`, `SyncTargetToken`, and
  acknowledgment result typing for host-testable shootdown-style rendezvous.

## Coordinator Audit Decisions

- Kept wait-interrupt classification reactor-local. The interrupt source is a
  predicate seam only; no POSIX signal queues, dispositions, thread entities,
  or AST delivery policy were added.
- Accepted the AST/preemption shard as mechanism-only. It is not wired into
  `Task`/`Reactor` polling yet, which keeps the current runtime behavior
  stable while giving the thread-runtime lane a future attachment point.
- Tightened `CountdownCompletion` to require `NonZeroU32` at construction,
  matching `COMPLETION_v1`'s API sketch. Empty target sets remain admitted only
  for `SyncRendezvous`, where an empty rendezvous is complete immediately.
- Kept sync coordination reactor-local and did not add `tx-substrate` wiring
  because the active zone/EBR integration lease still owns
  `crates/tx-substrate/src/lib.rs`.

## Residual Gaps

- AST markers are not yet consumed by `Task`/`Reactor` or tied to
  return-to-userspace delivery.
- Userspace-run remains deferred because it crosses saved-register trap
  handling, ThreadPayload, HAL return, and CoreInit reactor-loop wiring.
- `RawQueue` / `RawPort` were first-slice host-testable primitives during this
  audit. Follow-up bus slices have since added typed declarations, SMP-safe
  subscriber storage, epoch-protected destruction, static backing, and a
  bounded `SubscriptionGraph<N>` owner. Follow-up work also added
  `WireOwnerRetireFence` for EBR-delayed owner-storage reclaim. Remaining bus
  work is concrete VFS/device owner implementations plus target-fd reverse-index
  teardown, global epoll table integration, and spill/fanout policy.
- Sync coordination is not a real SMP shootdown protocol yet; it is the
  reactor-local rendezvous shape that the future substrate/HAL path can call.
- CoreInit reactor-loop wiring still waits for the `init.rs` lease to clear.

## Verification

Fresh verification after coordinator audit:

- `cargo fmt --check`
- `cargo test -p tx-reactor --test wait_interrupt` (6 tests)
- `cargo test -p tx-reactor --test completion` (6 tests)
- `cargo test -p tx-reactor --test sync_coord` (4 tests)
- `cargo test -p tx-reactor` (65 tests)
- `cargo test -p tx-substrate`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask lint arch`
- `cargo xtask lint unused`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `cargo xtask ci` (11 passed, 0 skipped, 0 failed)
- `git diff --check`
