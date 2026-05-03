---
date: 2026-04-30
topic: "Bus epoch-fenced retire handshake"
status: complete
---

# Decision: Bus Epoch-Fenced Retire Handshake

## Context

The bus had terminal/subscriber reporting and typed queue/port declarations,
but destruction was still expressed only as raw `terminate(...)` calls. The
active bus design requires wire destruction to run under epoch protection so
subscribers tolerate owner teardown and later storage reuse.

## Decision

Add an explicit epoch-fenced retire handshake for queue/port wires:

- `RawQueue::retire(bits, &Guard)` and `RawPort::retire(event, &Guard)`.
- `retire_silently(&Guard)` for carriers with no synthetic terminal event.
- `DeclaredQueue<E>` and `DeclaredPort<E>` typed wrappers that validate the
  terminal event before delegating to the raw carrier.
- `WireRetirement` records carrier kind, terminal bits, wake count, whether the
  call performed the terminal transition, and the guard epoch/CPU used for the
  handshake.

The existing `terminate(...)` APIs remain as compatibility shims for tests and
callers that are not yet in an epoch-aware owner destruction path.

## Consequences

Subsystem owner teardown can now express the bus destruction boundary as a
guarded operation rather than an unqualified raw terminal call. The API makes
the intended order concrete: enter epoch, retire wire, drain terminal wakes,
then continue owner teardown/reclamation.

This is not the final physical reclamation path. The current raw wire backing
storage is still `Arc` managed, and subscriptions still keep that storage alive
by reference count. A later zone/device-owner slice must embed wires directly
in owner storage and retire physical owner memory through EBR after this
handshake.

## Verification

- `cargo test -p tx-substrate --test bus`
- `cargo test -p tx-substrate`
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

Connect concrete VFS/device owner implementations so owners combine complete
embedded wire retire records with the `WireOwnerRetireFence` EBR-delayed
storage reuse hook.

## Blockers

None for the epoch-fenced terminal/drain handshake. Production VFS/device/block
runtime readiness is still blocked on target-fd reverse-index teardown, global
epoll table integration, spill/fanout policy, concrete VFS/device owner
implementations, and the kernel
timer/IRQ runtime loop. Static raw wire backing, the bounded
`SubscriptionGraph<N>` token owner, and the `WireOwnerRetireFence` EBR bridge
landed in follow-up slices.
