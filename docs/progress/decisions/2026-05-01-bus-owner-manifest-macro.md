# Bus Owner Manifest Macro

**Date:** 2026-05-01

## Context

The bus already had `WireOwnerManifest`, `WireOwnerRetireFence`, and
`retire_wire_owner<T>()`, but concrete VFS/device/block owners would still
need to hand-write the same unsafe manifest shape: retire every embedded wire
under one guard, aggregate the fence, then provide a typed reclaim callback.

## Decision

Add `tx_substrate::bus::bus_wire_owner_manifest!` for simple embedded-wire
owners.

The macro:

- generates the unsafe `WireOwnerManifest` impl for one owner type;
- takes a field-retire list such as `queue => retire(BROKEN);`;
- retires every listed wire under the supplied epoch guard;
- builds and extends one `WireOwnerRetireFence`;
- wires the typed reclaim callback through the existing
  `retire_wire_owner<T>()` path.

The macro intentionally stays narrow. It covers the common owner shape where
each embedded bus wire is a direct field and each retire operation is a single
method call. Owners with more complex teardown can still hand-write
`WireOwnerManifest`.

## Consequences

Concrete subsystem owners can now use the same typed owner-retire path without
copying unsafe fence/reclaim boilerplate. This closes the bus-side owner
manifest boilerplate gap while preserving the existing safety boundary: the
owner type is still responsible for listing every embedded wire exactly once
and for reclaiming its storage exactly once after the EBR delay window.

Remaining production work:

- concrete VFS/device/fs/block owner types and manifests;
- trace subscriber/nop-patching runtime;
- target-fd reverse-index teardown, global epoll table integration, and
  spill/fanout storage;
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

The next production slice should move from generic bus API hardening to the
first concrete owner surface: a small VFS/device/fs/block owner with declared
embedded wires and a manifest generated through this macro, or the final
epoll/fd teardown owner once fd ownership is ready to bind.
