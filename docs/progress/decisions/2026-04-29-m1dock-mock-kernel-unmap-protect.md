# M1 Dock mock kernel unmap/protect lifecycle

## Context

The M1 Dock mock could reserve and commit low-MMIO identity 4 KiB kernel
mappings, including page-table intermediates allocated through the installed
`PtNodeAllocator`. After commit, however, the branch PTEs no longer carried
typed ownership, so the board could not safely prune an empty table or return
its backing typed frame.

## Decision

`tx-hal-riscv64-m1dock-mock` now keeps a small board-private registry for
committed page-table nodes that were born from `PmapReservationIntermediates`.
Commit registers fresh L1/L0 nodes before publishing the leaf. Kernel unmap
stays intentionally narrow: it accepts only page-aligned, high direct-map,
4 KiB low-MMIO leaves, clears the leaf, prunes empty committed L0/L1 tables,
and returns the typed frame through the original `PtNode` releaser.

`protect_kernel_mapping()` rewrites an existing same-granularity kernel leaf in
place using the shared `PmapPermissions` vocabulary and rejects unsafe kernel
permission shapes such as user mappings, no-access leaves, or write-without-read
encodings. `shootdown_kernel_mapping()` is the local Sv39 fence hook for this
mock board.

This keeps the M1 mock aligned with the HAL_v1 pmap lifecycle without copying
RV64 QEMU's `BootStaticBag`, direct-map extension, or process-root machinery.

## Verification

- Red first:
  `cargo test -p tx-hal-riscv64-m1dock-mock unmap_prunes_committed_l0_and_releases_pt_node`
  failed with `Unsupported`.
- Red first:
  `cargo test -p tx-hal-riscv64-m1dock-mock protect_kernel_mapping_updates_leaf_and_reports_invalidation`
  failed with `Unsupported`.
- `cargo fmt -p tx-hal-riscv64-m1dock-mock --check`
- `cargo test -p tx-hal-riscv64-m1dock-mock`
- `cargo xtask build --target rv64-m1dock-mock`
- `RUSTFLAGS=-Dunused cargo check -p tx-kernel-riscv64-m1dock-mock --target riscv64gc-unknown-none-elf`
- `cargo xtask qemu --target rv64-m1dock-mock --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `git diff --check`

Full `cargo xtask check` was also attempted. It is currently blocked outside
this HAL/pmap slice by unrelated `tx-ext4-format` clippy failures
(`new_without_default` and `len_without_is_empty` in
`crates/tx-ext4-format/src/ondisk.rs`).

## Next

The follow-up high-half slice moved the supported kernel-MMIO virtual surface
from low identity addresses to high direct-map aliases. The next M1 pmap step is
a pmap module split, then process-root/ASID support and ASID-scoped shootdown
for the QEMU/OpenSBI mock. Real K210 boot remains a separate packaging/hardware
milestone.
