# Bus Owner-Storage EBR Fence

**Date:** 2026-04-30

## Context

The bus had wire-level retire calls that terminate a queue/port, drain
subscribers, and record the guard epoch/CPU. That was enough to prove a wire no
longer has live subscriptions, but not enough for a containing zone/device
object to hand its storage to EBR with a durable record that every embedded
wire was retired first.

## Decision

Add `tx_substrate::bus::WireOwnerRetireFence`.

An owner builds the fence from one or more `WireRetirement` records. The fence:

- accepts only retirements that performed the terminal transition;
- requires all included wires to have been retired under the same guard epoch
  and CPU;
- aggregates wire and wake counts for diagnostics;
- queues the containing owner storage through the epoch domain with an
  owner-provided reclaim callback.

The final call is unsafe and erased on purpose. The bus can prove that the
listed wire retirements happened, but only the owner can know that the list is
complete and that the reclaim callback matches the containing storage.

## Consequences

Dynamic owners now have an executable bridge from embedded wire destruction to
EBR-delayed physical storage reuse. The bus still does not own semantic entity
lifetime: zone/device code remains responsible for semantic death, no-upgrade
barriers, and complete embedded-wire manifests.

Remaining production work:

- concrete VFS/device owner implementations using typed embedded-wire
  manifests;
- trace subscriber/nop-patching runtime;
- target-fd reverse-index teardown, global epoll table integration, and
  spill/fanout policy;
- timer/IRQ delivery into the kernel runtime loop.

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

Add concrete device/VFS owner implementations using the typed manifest hook,
using `bus_wire_owner_manifest!` when the owner has a direct embedded-wire
field list.
