---
date: 2026-04-30
topic: "Bus static wire storage"
status: complete
---

# Decision: Bus Static Wire Storage

## Context

The device design needs bus wires in static device/block registration tables.
The earlier wording asked for zero-argument `RawQueue::new_const()` /
`RawPort::new_const()`, but the current raw handles are cloneable and normally
share heap state through `Arc`, so a zero-argument const constructor cannot
create correct shared subscriber storage.

## Decision

Split static storage from raw handles:

- `StaticRawQueue` and `StaticRawPort` are const-constructible backing storage.
- `StaticRawQueue::raw()` and `StaticRawPort::raw()` create cloneable raw
  handles to that storage.
- `RawQueue::from_static(&StaticRawQueue)` and
  `RawPort::from_static(&StaticRawPort)` provide explicit constructor spelling.
- `DeclaredQueue<E>::from_static` and `DeclaredPort<E>::from_static` combine
  static backing storage with typed declaration validation.
- Dynamic `RawQueue::new()` / `RawPort::new()` still use `Arc`-backed storage.

`DEVICE.md` now describes static-backed wires using the storage/handle split
instead of the impossible zero-argument const raw constructor.

## Consequences

Device and block registrations can be built as `static` tables without runtime
wire allocation. The hot path still speaks `RawQueue` / `RawPort`, so existing
reactor waits and tests do not need a second API.

Static storage is process-lifetime storage. It is suitable for board-owned
device registries, not for zone-owned dynamic objects that need typed
zone/device owner manifests over embedded wires.

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

Connect concrete VFS/device owner implementations after the
owner-retire fence, and add trace subscriber/nop-patching runtime once
subsystem examples stabilize.

## Blockers

None for static raw queue/port storage or typed static wrappers. Production
VFS/device/block runtime is still blocked on trace subscriber/nop-patching
runtime, target-fd reverse-index teardown, global epoll table integration,
spill/fanout policy, concrete
VFS/device owner implementations, and the kernel timer/IRQ runtime loop.
