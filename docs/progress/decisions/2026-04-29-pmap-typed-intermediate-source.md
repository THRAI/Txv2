# Decision: Pmap Typed Intermediate Source

**Date:** 2026-04-29

## Decision

- `tx_substrate::init::<P>()` installs
  `page_allocator::reserve_page_table_node` into
  `PmapIf::install_pt_node_allocator()` after the bitmap allocator is live.
- The installed source reserves a zeroed frame, commits it to `OwnedFrame`,
  converts it with `into_page_table_frame()`, and returns a `PtNode` carrying a
  typed-frame release hook.
- RV64 QEMU pmap allocation now prefers the installed typed source and falls
  back to `PT_NODE_POOL` only when typed allocation is exhausted.
- Rollback releases typed intermediates through the `PtNode` release authority;
  boot-pool nodes still return to the board-private static pool.

## Context

- Boot MMIO mapping still runs before frame allocation, so it must keep using
  `PT_NODE_POOL`.
- Once `FrameMeta[]` and the bitmap allocator are installed, new page-table
  intermediates should carry typed ownership instead of being anonymous static
  pool pages.
- The release hook keeps the dependency direction intact: HAL does not import
  `tx-substrate`; the substrate-owned typed node hands pmap a narrow release
  authority with the token.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt post_boot_reservation_uses_typed_pt_allocator_before_boot_pool`
- `cargo test -p tx-hal-riscv64-qemu-virt typed_pt_allocator_exhaustion_falls_back_to_boot_pool`
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

- Add substrate post-shootdown frame accounting and then free empty
  intermediate tables through typed page-table-frame teardown.

## Blockers

- Slab/global allocator initialization and the minimal trap vector are still
  deferred.
