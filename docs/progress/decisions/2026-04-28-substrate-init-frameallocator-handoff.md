# Decision: Substrate Init Frameallocator Handoff

**Date:** 2026-04-28

## Decision

- `tx_substrate::init::<P>()` is now the BSP-only handoff from HAL `BootInfo`
  and `BootstrapPmapInfo` into the installed steady-state bitmap allocator.
- The allocator metadata layout is dense over `[ppn_base, ppn_base +
  frame_count)`, not zero-based from physical address 0. Raw `Ppn` remains the
  public identity; the bitmap backend subtracts `ppn_base` internally.
- HAL `BootstrapPmapInfo` now publishes `reserved_page_tables`, a static slice
  of bootstrap page-table physical ranges that substrate subtracts before
  populating free frames.
- The first executable slice now requests HAL direct-map extension before
  allocator installation if RAM/free metadata would fall outside the bootstrap
  window.

## Context

- RV64 QEMU now enters Rust in the high alias and drops the temporary identity
  bridge before substrate runs, so substrate must consume published HAL facts
  rather than reconstruct linker or boot-static addresses.
- QEMU RAM starts at `0x8000_0000`; dense metadata avoids wasting rows for the
  low physical hole while keeping raw PPN lookup straightforward.
- The page allocator token interface was already in place; this decision wires
  boot-populated `FrameMeta[]`, bitmap storage, free spans, and the direct-map
  zero scrubber into that interface.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-substrate`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo xtask lint unused`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `cargo xtask ci`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `git diff --check`

## Next Step

- Implement the next pmap slice: general 2 MiB/4 KiB MMIO reserve/commit,
  unmap hooks, and then switch page-table intermediates from the boot PT-node
  pool to typed frame allocation.

## Blockers

- Slab/global allocator initialization is still deferred, so allocation-using
  subsystems must not run immediately after this first `tx_substrate::init`
  slice.
