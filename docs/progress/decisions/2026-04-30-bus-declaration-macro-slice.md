# Bus Declaration Macro Slice

**Date:** 2026-04-30

## Context

The bus already had `WireEventSet`, `WireDeclaration<E>`, `DeclaredQueue<E>`,
and `DeclaredPort<E>`, but subsystem examples still needed to hand-write small
bit-set wrapper types for every readiness or lifecycle declaration. That kept
the typed declaration surface usable but too noisy for device, VFS, and block
subsystem development.

## Decision

Add the first declaration macro slice in `tx_substrate::bus`:

- `bus_event_set!` generates a typed `WireEventSet` bit newtype, declared-bit
  union, `from_bits`, `bits`, `is_empty`, `contains`, and bitwise operators.
- `bus_readiness!` is the queue/readiness-oriented alias.
- `bus_lifecycle!` is the port/lifecycle-oriented alias.

The macros are exported at the crate root by `#[macro_export]` and re-exported
through `tx_substrate::bus` so subsystem code can write
`tx_substrate::bus::bus_readiness! { ... }` near the wire declaration.

## Consequences

Subsystem-facing queue/port declarations no longer need manual `WireEventSet`
boilerplate. The generated types integrate with `DeclaredQueue<E>`,
`DeclaredPort<E>`, static wire backing storage, and existing undeclared-bit
validation.

This is intentionally not the full production declaration system. It makes the
current bus/reactor spine easier to use for prototype subsystem development,
not production-complete for VFS/device/block runtime. A follow-up reactor slice
adds typed declared-port wait consumption through `DeclaredChannel<E>`, but
typed queue/readiness wait adapters, tracepoint macros, final global epoll/fd
teardown policy, concrete VFS/device/fs/block owners, the final per-hart
AP/reactor loop, and VM/user/trap integration remain later slices. Later
reactor and bus slices added typed queue/readiness waits, declared graph
helpers, typed tracepoint payload structs, and generated owner-manifest
boilerplate.

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

Use these macros in the first real device/VFS/block wire declarations, then
add trace subscriber/nop-patching runtime once those subsystem examples
stabilize.
