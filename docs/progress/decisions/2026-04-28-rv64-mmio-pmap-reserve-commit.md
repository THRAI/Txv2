# Decision: RV64 MMIO Pmap Reserve Commit

**Date:** 2026-04-28

## Decision

- HAL now exposes boot-time generic kernel mapping hooks:
  `reserve_kernel_mapping(virt, phys, kind)` and
  `commit_kernel_mapping(reservation)`.
- `PmapReserveKind` now distinguishes 1 GiB superpages, 2 MiB superpages, and
  4 KiB pages.
- RV64 QEMU implements 2 MiB and 4 KiB kernel mappings by allocating
  intermediate page-table nodes from `PT_NODE_POOL` before the frame allocator
  exists.
- `tx_substrate::init::<P>()` walks `PlatformInfo.mmio_regions`, page-covers
  each region, prefers aligned 2 MiB MMIO leaves, falls back to 4 KiB leaves,
  and immediately commits each reservation.

## Context

- The previous pmap slice could extend the high direct map with 1 GiB leaves
  only. Platform MMIO needs finer granularity for CLINT, PLIC, UART, and
  virtio-mmio windows.
- This remains a boot-only mutation path. Substrate reserves and commits in the
  same loop, so rollback for abandoned reservations is deliberately left to the
  later full pmap transaction surface.
- Device/MMIO physical frames remain outside allocator ownership. The mapping
  path publishes PTEs for access; it does not create freeable frame authority.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt mmio_mapping`
- `cargo test -p tx-substrate --test boot_memory mmio_mapping_choice`
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

- Add pmap rollback/unmap/shootdown vocabulary, then switch post-boot
  intermediate page-table allocation from `PT_NODE_POOL` to typed frame tokens.

## Blockers

- Slab/global allocator initialization is still deferred.
