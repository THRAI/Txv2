# Bus Typed Owner Manifest Hook

**Date:** 2026-04-30

## Context

`WireOwnerRetireFence` connected wire-level retire records to EBR-delayed
owner-storage reclaim, but direct callers still supplied an erased reclaim
callback. The next step was to put the completeness and reclaim contract on the
containing owner type so semantic call sites do not hand-roll the erased fence
call.

## Decision

Add `tx_substrate::bus::WireOwnerManifest` and
`tx_substrate::bus::retire_wire_owner<T>()`.

The manifest is an unsafe trait implemented by the owner type. It provides:

- the complete embedded-wire retire sequence for that owner;
- the typed reclaim callback for the owner storage.

`retire_wire_owner<T>()` takes a typed owner pointer and epoch guard, calls the
manifest to retire every embedded wire, then queues the owner storage through
EBR using the owner type's reclaim callback.

## Consequences

The generic bus API now has a typed owner hook over the erased fence. This
keeps the unsafe completeness contract at the owner type and removes erased
reclaim callbacks from normal retire call sites.

This is still not the concrete VFS/device runtime. Actual subsystem owner
types still need to implement manifests, and later trace/owner declaration
macros should generate most of the repetitive wire-retire code.

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

Add concrete manifests on the first real dynamic device or VFS owner. A later
macro slice now generates simple owner-manifest boilerplate for direct
embedded-wire field lists.
