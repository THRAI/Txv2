# Bus Graph Ready Scan And Teardown

**Date:** 2026-05-01

## Context

`SubscriptionGraph<N>` already owned long-lived queue/port subscription tokens
for epoll-style users, but consumers still needed to know every key in advance
to test readiness, and epoll-fd close relied on dropping the whole graph. That
was enough for focused prototypes, but not a good boundary for the future
global epoll/fd runtime.

## Decision

Add a bounded graph scan and explicit graph teardown:

- `SubscriptionGraphReady` records the generation-checked key, wire kind, and
  subscription state for one ready or terminal graph entry.
- `SubscriptionGraph::collect_ready(&mut [SubscriptionGraphReady])` walks graph
  entries in slot order, fills caller-provided storage, consumes ordinary
  readiness, and reports terminal subscriptions without removing them.
- `SubscriptionGraph::clear()` drops every owned subscription token, returns
  the removed count, and leaves old keys stale.

The scan is bounded by caller storage so the future epoll loop can choose stack
or slab-backed batch sizes without requiring allocation in the graph API.
Terminal entries remain visible until policy code removes them; that keeps the
terminal/hangup decision in fd/epoll policy rather than hiding it inside the
substrate bus.

## Consequences

This gives the fd/epoll layer a substrate-owned primitive for "what is ready
now?" and an explicit epoll-fd-close cleanup point. It does not define target-fd
reverse indexes, duplicate-registration policy, spill storage, fanout batching,
or how terminal events map to poll masks. Those remain fd/VFS/epoll subsystem
work.

Remaining production work:

- target-fd reverse-index teardown and final global epoll table integration;
- spill storage and fanout policy for large subscriber sets;
- concrete VFS/device/fs/block owner types and manifests;
- trace subscriber/nop-patching runtime;
- production IRQ/timer lost-wake linearization and final per-hart reactor loop;
- VM/user/trap integration.

## Verification

- `cargo test -p tx-substrate --test bus subscription_graph_collects_ready_and_terminal_entries_for_epoll_scan`
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

Build the fd/epoll owner side around this graph API: target-fd reverse indexes,
close ordering, duplicate registration policy, and the mapping from terminal
wire state into poll/epoll event masks.
