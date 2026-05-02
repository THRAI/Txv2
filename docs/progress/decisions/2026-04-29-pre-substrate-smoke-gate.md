# Pre-substrate smoke gate

## Context

RV64 QEMU now reaches Rust through the platform-owned high-VMA boot path and is
ready to run substrate init before the smoke sentinel. LA64 QEMU and the RV64
M1 Dock mock needed the same `_start -> rust_entry -> tx_hal::entry` shape, but
do not yet have the board pmap, allocator, and BootInfo machinery required for
full substrate init.

## Decision

`PlatformConfig` now has `SUBSTRATE_BOOT_READY`, defaulting to false. Generic
`tx_kernel::kernel_main::<P>()` always calls `P::init_early(handoff)`, then runs
`tx_substrate::init::<P>()`, `P::init_later(handoff)`, and
`P::install_kernel_trap_vector()` only when the platform opts in.

RV64 QEMU is the only opted-in platform in the first wave. LA64 QEMU and RV64
M1 Dock mock use platform-owned `_start` implementations and board early
consoles to reach `txkernel:<board>:boot:ok` as pre-substrate smoke ports.
`BootHandoff`, `tx_hal::entry`, and the RV64 QEMU board-private
`BootStaticBag` shape stay unchanged.

Follow-up slices added board-owned memory facts, bootstrap-pmap facts, and a
typed PT-node allocator handoff for LA64 and M1. Those boards now opt into
substrate smoke, but still leave real mapping mutation, process roots, and
substrate MMIO mapping unsupported.

## Verification

- `cargo fmt --check`
- `cargo xtask check`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask build --target la64-qemu`
- `cargo xtask build --target rv64-m1dock-mock`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask qemu --target la64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask qemu --target rv64-m1dock-mock --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `git diff --check`

## Next

Grow LA64 QEMU and the M1 Dock mock from substrate-smoke ports into real
runtime-capable boards only after their pmap mutation, process-root, and MMIO
mapping surfaces carry real board truth.
