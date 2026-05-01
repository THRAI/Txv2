# Reactor Second-Wave Scout

**Date:** 2026-04-30

**Scope:** decide the next parallel reactor dispatch after the first-wave task,
scheduler, wait/bus, and timer/idle worker merge.

## Inputs

- Active reactor contract: `REACTOR_v0` names task scheduling, wait/wake, AST,
  synchronous coordination, and userspace-run as reactor responsibilities.
- Thread-runtime contract: wait-adapt owns site-A interruption classification
  through an `InterruptSource` predicate; AST owns site-B return-to-userspace
  delivery.
- Completion contract: counted completions are reactor/wait middleware with a
  private channel and consuming `wait(protocol)` semantics.
- First-wave audit: task lifecycle, Phase 1 scheduler shell, lost-wake-safe
  `wait_event`, and host timer/idle driving are merged into the working tree.
- Active lease check: `2026-04-29-zone-ebr-integration` still owns
  `crates/tx-kernel/src/init.rs`, `crates/tx-substrate/src/lib.rs`,
  `crates/tx-reactor/tests/reactor_smoke.rs`, and `docs/progress/STATUS.md`.

## Dispatch Decision

The next pass should dispatch four isolated reactor lanes:

1. **wait-interrupt-classification**
   - Owns `crates/tx-reactor/src/interrupt.rs`,
     `crates/tx-reactor/src/wait.rs`, and
     `crates/tx-reactor/tests/wait_interrupt.rs`.
   - Adds the reactor-local `InterruptSource`/summary contract and makes
     `wait_event` return `Interrupted` or `Killed` under the documented
     protocol rules.
   - Does not implement POSIX signal routing, pending queues, or thread
     entities.

2. **ast-preempt**
   - Owns `crates/tx-reactor/src/ast.rs`,
     `crates/tx-reactor/src/preempt.rs`, and
     `crates/tx-reactor/tests/ast_preempt.rs`.
   - Builds a task-local AST/preemption marker surface that can be consumed
     between polls.
   - Does not touch `wait.rs` or signal delivery policy.

3. **completion-middleware**
   - Owns `crates/tx-reactor/src/completion.rs` and
     `crates/tx-reactor/tests/completion.rs`.
   - Implements counted `Completion` and closed-set `CountdownCompletion`
     over the existing `Channel::wait_event` contract.
   - Requires a tiny coordinator pre-step to add the module facade in
     `crates/tx-reactor/src/lib.rs` so the worker can avoid shared facade
     edits.

4. **sync-coord-interface**
   - Owns `crates/tx-reactor/src/sync_coord.rs` and
     `crates/tx-reactor/tests/sync_coord.rs`.
   - Shapes a reactor-local synchronous rendezvous primitive suitable for
     shootdown-style acknowledgments.
   - Stays interface-only for this pass; no `tx-substrate` module wiring while
     the zone/EBR integration lease owns `crates/tx-substrate/src/lib.rs`.

## Deferred

- **userspace-run** is not a good next shard. It crosses `ThreadPayload`,
  saved-register trap handling, HAL trap return, AST delivery, and CoreInit
  wiring. That is not isolated enough yet.
- **CoreInit reactor loop wiring** waits for the active init lease to clear.
- **bus hardening** waits for the substrate lease to clear. `RawQueue` /
  `RawPort` have since gained typed declarations, SMP-safe subscriber storage,
  epoch destruction, static backing, and a bounded `SubscriptionGraph<N>` owner
  for long-lived raw subscriptions. Follow-up work also added
  `WireOwnerRetireFence` for EBR-delayed owner-storage reclaim. Remaining bus
  hardening is concrete VFS/device owner implementations plus target-fd
  reverse-index teardown, global epoll table integration, and spill/fanout
  policy, but that is substrate/runtime work rather than the next
  reactor-mechanism pass.
- **full POSIX signal delivery** waits on thread-runtime/process entities. The
  next wait-interrupt lane should stop at the predicate and classification
  seam.

## Verification

Scout verification:

- `cargo xtask progress list worktrees --json`
- targeted reads of active reactor/thread-runtime/completion/bus docs
- targeted reads of current `tx-reactor` module split

Implementation verification for the eventual second-wave merge should include:

- `cargo fmt --check`
- `cargo test -p tx-reactor --test wait_interrupt`
- `cargo test -p tx-reactor --test ast_preempt`
- `cargo test -p tx-reactor --test completion`
- `cargo test -p tx-reactor --test sync_coord`
- `cargo test -p tx-reactor`
- `cargo xtask progress validate`
- `cargo xtask ci`
- `git diff --check`

`docs/progress/STATUS.md` was intentionally not updated in this scout pass
because the active zone/EBR worktree record still leases that file.
