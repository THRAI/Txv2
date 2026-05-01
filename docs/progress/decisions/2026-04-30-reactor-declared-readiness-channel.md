# Reactor Declared Readiness Channel

**Date:** 2026-04-30

## Context

`DeclaredChannel<E>` closed typed declared-port waits, but readiness-shaped
waits still needed an adapter over `DeclaredQueue<E>`. Device, VFS, and block
subsystems commonly wait on level-triggered readiness, so leaving only the port
adapter would still push those call sites back through raw masks.

## Decision

Add `tx_reactor::wait::DeclaredReadinessChannel<E>` over `DeclaredQueue<E>`.

The typed readiness channel:

- wraps an existing declared bus queue or creates one from `WireDeclaration<E>`;
- exposes typed direct waits and typed `wait_event` futures with interrupt and
  timeout support;
- validates undeclared readiness interests before polling through fallible
  `try_*` constructors;
- preserves raw direct-wait behavior for empty interests;
- exposes typed `fire`, `try_fire`, `clear`, `try_clear`, and `peek_bits` over
  the backing queue;
- can be attached to the reactor timer queue through
  `Reactor::declared_readiness_channel` and
  `Reactor::declared_readiness_channel_from_queue`.

The raw `Channel` / `Mask` path remains for completion and sync-coordinate
internals.

## Consequences

Subsystem-facing readiness waits can now keep their declared readiness type
through the reactor wait adapter instead of erasing it to a raw mask. Together
with `DeclaredChannel<E>`, the reactor now has typed declared wait adapters for
both bus queue and port carriers.

This is not the full production VFS/device/block runtime. Concrete subsystem
owners still need to migrate to declared waits, and target-fd reverse-index
teardown, global epoll table integration, concrete owner manifests, trace
subscriber/nop-patching runtime, the final per-hart reactor loop, and VM/user/
trap integration remain later slices.
Declared bus graph helpers and typed tracepoint payloads landed as later bus
slices.

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

Migrate the first concrete VFS/device/block wait site to declared wait
channels, or add target-fd reverse-index/global epoll policy when
file-descriptor ownership lands.
