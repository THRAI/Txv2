# Bus Typed RawTrace Payloads

**Date:** 2026-05-01

## Context

The bus had typed queue/port declarations, declaration macros, typed wait
adapters, and declared subscription-graph helpers. `RawTrace` was still an
untyped zero-argument placeholder, even though the active bus design treats
tracepoints as the third publication primitive with structured payloads.

## Decision

Add the first typed tracepoint surface in `tx_substrate::bus`:

- `TracePayload` is the marker trait for copyable structured trace payloads.
- `TraceDeclaration<P>` records the tracepoint name and payload type.
- `RawTrace<P>` stores the declaration and exposes `emit(payload: P)`.
- `bus_tracepoint!` generates copyable payload structs and implements
  `TracePayload` for them.

The implementation is intentionally no-op for emission. It preserves payload
types and declaration names at call sites without introducing trace subscriber
registration, nop-patching, or ftrace/perf/BPF delivery in the bus hot path.

## Consequences

Subsystem-facing code can now declare tracepoint payload shapes without falling
back to untyped `emit()` placeholders or inventing private trace APIs. This
closes the typed `RawTrace` payload gap in the bus API, while keeping the
runtime trace machinery as a separate production slice.

Remaining production work:

- trace subscriber registration and nop-patching runtime;
- target-fd reverse-index teardown, global epoll table integration, and
  spill/fanout storage;
- concrete VFS/device/fs/block owners and owner manifests;
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

Move the bus/runtime spine to either concrete VFS/device owner manifests over
embedded declared wires, or target-fd reverse-index/global epoll ownership once
fd ownership is ready to bind.
