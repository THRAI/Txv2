# M1 Dock mock PT-node kernel mapping growth

## Context

The M1 mock had a real Sv39 identity bootstrap root, but the first mutation
slice only proved mappings within the preinstalled UART L0 table. The next
interface-aligned step was to prove that missing page-table levels can be
allocated through the HAL pmap PT-node allocator instead of adding more static
tables by hand.

## Decision

`tx-hal-riscv64-m1dock-mock` now walks the root/L1/L0 tree for low-MMIO
identity 4 KiB kernel mappings. If a needed L1 or L0 table is missing,
`reserve_kernel_mapping()` allocates it through `PmapIf::alloc_pt_node()`,
zeros it, installs the branch PTE, and carries the fresh `PtNode` in
`PmapReservationIntermediates`.

`commit_kernel_mapping()` publishes the final leaf in the reserved L0 table.
`rollback_kernel_mapping()` removes uncommitted branch PTEs and releases the
fresh nodes. The supported range is still intentionally narrow: identity
kernel mappings below QEMU RAM. Non-identity mappings, RAM superpage splits,
process roots, and general MMIO policy remain out of scope for this slice.

## Verification

- Red first:
  `cargo test -p tx-hal-riscv64-m1dock-mock second_uart_window_allocates_l0_before_commit`
  failed with `Unsupported`.
- `cargo fmt -p tx-hal-riscv64-m1dock-mock --check`
- `cargo test -p tx-hal-riscv64-m1dock-mock`
- `cargo xtask build --target rv64-m1dock-mock`
- `cargo xtask qemu --target rv64-m1dock-mock --profile smoke --expect-sentinel --timeout-ms 10000`
- `RUSTFLAGS=-Dunused cargo check -p tx-kernel-riscv64-m1dock-mock --target riscv64gc-unknown-none-elf`

Global `cargo fmt --check` is currently blocked by unrelated untracked
`crates/tx-ext4-format/` files introduced outside this HAL/pmap slice.

## Next

Add committed PT-node ownership, kernel unmap/protect, and local invalidation
before attempting process-root support.
