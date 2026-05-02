---
date: 2026-04-30
topic: "Bus module split"
status: complete
---

# Decision: Bus Module Split

## Context

`crates/tx-substrate/src/bus/mod.rs` had grown past 1,400 lines after typed
declarations, epoch-fenced retire records, and static wire storage landed. The
next bus production slices need room without tripping the repo's 1,500-line
authored source lint.

## Decision

Split the bus implementation by responsibility:

- `mod.rs`: public facade.
- `common.rs`: shared errors, declaration types, `WireRetirement`, raw storage
  selection, subscriber records, and spin locking.
- `queue.rs`: `RawQueue`, `StaticRawQueue`, `DeclaredQueue<E>`, and queue
  subscription behavior.
- `port.rs`: `RawPort`, `StaticRawPort`, `DeclaredPort<E>`, and port
  subscription behavior.
- Later bus slices added `graph.rs`, `owner.rs`, `macros.rs`, and `trace.rs`
  for the long-lived subscription owner, owner-storage retire hooks,
  declaration macros, and typed `RawTrace<P>` payload declarations.

The public `tx_substrate::bus::*` imports remain available through facade
re-exports. No semantic API change is intended in this split.

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

Continue bus production work from the split modules: concrete VFS/device owner
implementations plus target-fd reverse-index teardown, global epoll table
integration, and spill/fanout policy are
the next meaningful blockers. A bounded `SubscriptionGraph<N>` token-owner
slice, `WireOwnerRetireFence` owner-storage EBR bridge, and
`WireOwnerManifest` typed owner hook landed after this split.

## Blockers

None for the split itself. Broader verification should still run before any
commit that includes the split.
