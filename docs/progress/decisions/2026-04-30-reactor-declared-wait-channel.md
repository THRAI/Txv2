# Reactor Declared Wait Channel

**Date:** 2026-04-30

## Context

The bus now has typed queue/port declarations and first declaration macros, but
the reactor wait adapter still exposed only `Channel` plus raw `Mask` over a
raw `RawPort`. That was enough for prototype wake/recheck behavior, but it
erased the subsystem's declared event type at the wait boundary.

## Decision

Add `tx_reactor::wait::DeclaredChannel<E>` over `DeclaredPort<E>`.

The typed channel:

- wraps an existing declared bus port or creates one from `WireDeclaration<E>`;
- exposes typed `wait`, `wait_event`, and interrupt-aware wait-event futures;
- validates undeclared interests before polling through fallible `try_*`
  constructors;
- preserves the raw empty-interest behavior for direct waits;
- can be attached to the reactor timer queue through
  `Reactor::declared_channel` and `Reactor::declared_channel_from_port`.

The existing raw `Channel` / `Mask` path remains for completion and
sync-coordinate internals.

## Consequences

Subsystem-facing port waits can now keep their declared event type through the
reactor wait adapter instead of erasing it to a raw mask. This closes the first
typed declared-wire consumption gap for port-shaped waits.

This is not the full production wait spine. A follow-up adds
`DeclaredReadinessChannel<E>` for typed queue/readiness waits. Typed graph
helpers also landed in a later bus slice. Concrete subsystem migrations,
target-fd reverse-index teardown, global epoll table integration, concrete
owner manifests, trace subscriber/nop-patching runtime, the final per-hart
reactor loop, and VM/user/trap integration remain later slices. A later bus
slice added typed tracepoint payloads.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-reactor --test wait_bus`
- `cargo test -p tx-reactor`
- `cargo test -p tx-substrate --test bus`
- `cargo test -p tx-substrate`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask progress validate`
- `cargo xtask lint docs`
- `cargo xtask lint arch`
- `cargo xtask lint unused`
- `cargo xtask ci`
- `git diff --check`

## Next Step

Migrate the first concrete VFS/device/block wait site to the declared wait
channels, or add target-fd reverse-index/global epoll policy when
file-descriptor ownership lands.
