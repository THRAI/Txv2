# Decision: RV64 Direct Map Extension

**Date:** 2026-04-28

## Decision

- HAL now exposes a minimal executable pmap mutation surface for boot:
  `reserve_kernel_direct_map_1g`, `commit_kernel_direct_map_1g`, and
  `extend_direct_map`.
- RV64 QEMU implements that surface by reserving empty Sv39 root slots and
  committing 1 GiB global direct-map leaves. Existing matching leaves are
  idempotent and return no reservation.
- `tx_substrate::init::<P>()` computes the page-covered RAM end from
  `BootInfo` and requests direct-map extension before validating coverage and
  carving `FrameMeta[]` / bitmap storage.

## Context

- The first allocator handoff could install frames only when the bootstrap
  direct map already covered all reported RAM. This was enough for 256 MiB QEMU
  smoke, but not for the substrate-ready contract.
- The direct-map extension path needs no intermediate page-table nodes on Sv39:
  each 1 GiB leaf lives directly in the root page table.
- General page/MMIO reserve-commit, unmap, and shootdown are still separate
  pmap slices; this decision lands the boot-critical 1 GiB direct-map case only.

## Verification

- `cargo test -p tx-substrate`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo fmt --check`
- `cargo xtask lint unused`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `cargo xtask ci`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `git diff --check`

## Next Step

- Extend the same pmap vocabulary to 2 MiB/4 KiB MMIO mappings and then switch
  page-table intermediates from the boot PT-node pool to typed frame
  allocation.

## Blockers

- Slab/global allocator initialization is still deferred.
