# Bus Subscription Graph Slice

**Date:** 2026-04-30

## Context

The bus already had SMP-safe raw subscriber storage, typed queue/port
declarations, epoch-fenced wire retirement, and static raw backing storage.
Long-lived consumers such as epoll still needed an owner for subscription
tokens so registrations can survive across waits without embedding token
lifetime rules in every caller.

## Decision

Add `tx_substrate::bus::SubscriptionGraph<N>` as a bounded first slice of the
static subscription graph.

The graph:

- owns raw `RawQueueSubscription` and `RawPortSubscription` tokens;
- returns generation-checked `SubscriptionGraphKey` handles;
- rejects stale keys, empty interests, wrong-kind updates, and terminal raw
  wires;
- supports update, remove, kind/state inspection, and take-ready operations;
- started raw-carrier based so declared graph helpers and fd graph policy could
  be added above it.

The graph itself is not the final global epoll table. Callers that share one
graph across harts must still place it behind their owner lock. This slice only
provides the durable token ownership and stale-key discipline needed by that
future table. Later graph work added bounded ready/terminal scans and explicit
graph clear teardown.

## Consequences

Subsystem/runtime work can now build an epoll-style owner without keeping raw
subscription tokens in ad hoc side structures. The bus hot path remains in the
raw carrier subscriber lists; graph mutation is still a cold-path operation.

Remaining production work:

- declared graph helpers over `DeclaredQueue<E>` / `DeclaredPort<E>` landed in
  `docs/progress/decisions/2026-05-01-bus-declared-subscription-graph-helpers.md`;
- typed trace payloads landed in
  `docs/progress/decisions/2026-05-01-bus-typed-rawtrace-payloads.md`;
- ready/terminal scans and explicit graph clear teardown landed in
  `docs/progress/decisions/2026-05-01-bus-graph-ready-scan-teardown.md`;
- trace subscriber/nop-patching runtime;
- target-fd reverse-index teardown, global epoll table integration, and
  spill/fanout storage;
- concrete VFS/device owner implementations;
- production lost-wake linearization with IRQ/timer/runtime integration.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-substrate --test bus`
- `cargo test -p tx-substrate`
- `cargo test -p tx-reactor --test wait_bus`
- `cargo test -p tx-reactor`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask progress validate`
- `cargo xtask lint docs`
- `cargo xtask lint arch`
- `cargo xtask lint unused`
- `cargo xtask ci`
- `git diff --check`

## Next Step

Keep pushing the production SMP path through concrete VFS/device owner
implementations over embedded wires and the kernel timer/IRQ runtime loop. Add
the target-fd reverse-index/global epoll policy layer when VFS/file-descriptor
ownership lands.
