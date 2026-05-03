---
date: 2026-04-30
topic: "Bus typed declaration surface"
status: complete
---

# Decision: Bus Typed Declaration Surface

## Context

The reactor/SMP work removed `Rc<RefCell<...>>` raw bus storage and added
terminal/subscriber reporting, but the public bus surface was still raw
`u64` masks/events. That was enough for prototype reactor waits and mock device
completion paths, but it left subsystem authors without the typed wire
declaration boundary described by `BUS_v1`.

## Decision

Add a first static-layer typed wrapper surface over the existing raw temporal
carriers:

- `WireEventSet` declares the bit set for one readiness/event type.
- `WireDeclaration<E>` records the wire name, carrier kind, and declared bits.
- `DeclaredQueue<E>` wraps `RawQueue`.
- `DeclaredPort<E>` wraps `RawPort`.

Typed fire, clear, subscribe, update, and terminate operations validate event
bits and subscription interests against the declaration before delegating to
the existing raw wire storage. The raw `RawQueue` and `RawPort` APIs remain
available for substrate-internal compatibility and current reactor wait
channels.

## Consequences

Subsystem-facing code can now use typed wire declarations instead of raw
integer masks for queue/port carriers. This narrows one production API gap
without forcing the reactor wait channel to migrate in the same slice.

This does not finish the production bus. Follow-up slices add the first
epoch-fenced terminal/drain retire handshake, static raw backing storage, typed
static wrappers, a bounded `SubscriptionGraph<N>` owner for long-lived raw
subscriptions, `WireOwnerRetireFence` for EBR-delayed owner-storage reclaim,
the generic `WireOwnerManifest` typed owner hook, and first readiness/lifecycle
bit-set declaration macros. A later trace slice adds typed `RawTrace` payloads.
Another later macro slice adds generated owner-manifest boilerplate. Trace
subscriber/nop-patching runtime, final global epoll graph policy, and concrete
VFS/device owner implementations remain later work.

## Verification

- `cargo test -p tx-substrate --test bus`
- `cargo test -p tx-substrate`
- `cargo test -p tx-reactor --test wait_bus`
- `cargo test -p tx-reactor`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo fmt --check`
- `cargo xtask progress validate`
- `cargo xtask lint docs`
- `cargo xtask lint arch`
- `cargo xtask lint unused`
- `cargo xtask ci`
- `git diff --check`

## Next Step

Connect concrete VFS/device owner implementations, then add trace subscriber/
nop-patching runtime once subsystem examples stabilize.

## Blockers

None for the typed queue/port wrapper slice. Production VFS/device/block
runtime readiness is still blocked on the remaining bus and kernel runtime
loop items listed above.
