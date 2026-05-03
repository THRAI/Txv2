# Bus Declared Subscription Graph Helpers

**Date:** 2026-05-01

## Context

The bus already had declared queue/port wrappers, typed declaration macros, and
a bounded `SubscriptionGraph<N>` that owns long-lived raw queue/port
subscription tokens. That raw graph was sufficient as the durable token owner,
but subsystem-facing graph users would still have to erase declared readiness or
event interests to `u64` before registering epoll-style subscriptions.

## Decision

Add typed helper APIs over the existing bounded graph:

- `DeclaredSubscriptionGraphKey<E>` wraps `SubscriptionGraphKey` while
  preserving the declared event/readiness type at graph call sites.
- `subscribe_declared_queue` and `subscribe_declared_port` accept
  `DeclaredQueue<E>` / `DeclaredPort<E>` and validate interest bits against the
  wire declaration before delegating to the raw graph.
- `update_declared_queue` and `update_declared_port` reject undeclared interest
  bits before updating the owned subscription token.
- `remove_declared`, `kind_declared`, `state_declared`, and
  `take_declared_ready` preserve the typed handle while reusing the existing
  generation and stale-key discipline.

The helper layer is intentionally thin. The raw graph remains the storage and
lifetime owner; the typed layer prevents subsystem and epoll-style call sites
from constructing raw masks that are not part of the declared wire contract.

## Consequences

The reactor/bus spine now has typed declarations across direct wait adapters
and long-lived graph registrations. A future epoll/fd table can build on the
bounded graph without immediately accepting raw interests from VFS or device
owners.

Remaining production work:

- target-fd reverse-index teardown, global epoll table integration, and
  spill/fanout storage;
- concrete VFS/device/fs/block owners and owner manifests;
- trace subscriber/nop-patching runtime;
- production IRQ/timer lost-wake linearization and final per-hart reactor loop;
- VM/user/trap integration.

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

Move the next production slice to either concrete VFS/device owner manifests
over embedded declared wires, or the target-fd reverse-index/global epoll owner
once fd ownership is ready to bind.
