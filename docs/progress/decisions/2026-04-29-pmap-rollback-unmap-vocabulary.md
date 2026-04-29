# Decision: Pmap Rollback Unmap Vocabulary

**Date:** 2026-04-29

## Decision

- `PmapReservation` now carries the `PT_NODE_POOL` intermediates allocated
  while reserving a kernel mapping.
- HAL exposes `rollback_kernel_mapping()`, `unmap_kernel_mapping()`, and
  `shootdown_kernel_mapping()` alongside the existing kernel mapping
  reserve/commit calls.
- RV64 QEMU rollback clears newly-created branch PTEs and frees the owned
  boot PT nodes. Committed 2 MiB / 4 KiB kernel leaves can be unmapped into a
  `PmapUnmapResult`, whose `PmapInvalidation` is then passed to the shootdown
  hook.
- The direct map remains lifetime-persistent: the current unmap path rejects
  1 GiB leaf teardown.

## Context

- The MMIO mapping slice allocated L1/L0 intermediates during reservation and
  immediately committed in substrate. That was enough for boot, but it left no
  interface for abandoned reservations or teardown.
- This slice keeps the interface kernel-mapping scoped. Process-root pmap
  materialization, ASID-aware invalidations, and substrate post-shootdown
  frame-accounting are still later work.
- RV64 QEMU host tests mutate board-static page-table storage, so pmap tests
  that touch those statics now take a test-only mutex. The failure that exposed
  this was a parallel test race, not a pmap encoding failure.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt pmap::tests::`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo test -p tx-substrate`
- `cargo fmt --check`
- `cargo xtask lint unused`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `cargo xtask ci`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `git diff --check`

## Next Step

- Switch post-boot page-table intermediate allocation from `PT_NODE_POOL` to
  typed frame tokens, then attach substrate post-shootdown frame accounting.

## Blockers

- Slab/global allocator initialization and the minimal trap vector are still
  deferred.
