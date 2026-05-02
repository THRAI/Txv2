# M1 Dock mock Sv39 bootstrap pmap

## Context

The RV64 M1 Dock mock was a QEMU/OpenSBI smoke board with identity execution,
static memory facts, a typed PT-node allocator handoff, and one UART MMIO
region reported as identity-precovered. That let substrate phase 3 run, but the
mock still reported `BootstrapPmapInfo.root = 0` and had no real pmap mutation.

## Decision

The M1 mock now builds and installs a board-owned Sv39 identity root during H1,
after BSS clear and before `rust_entry`.

The bootstrap root maps:

- QEMU RAM through a 1 GiB identity leaf at `0x8000_0000`.
- QEMU virt UART through a root branch, an L1 branch, and a 4 KiB L0 leaf at
  `0x1000_0000`.

`BootstrapPmapInfo` now publishes the root physical address and reserves the
root, low-MMIO L1, and UART L0 page-table pages. The mock also has a narrow real
kernel mapping path: `reserve_kernel_mapping()` and
`commit_kernel_mapping()` can materialize identity 4 KiB mappings in the low
MMIO identity range below QEMU RAM. Missing L1/L0 tables are allocated through
the installed `PtNodeAllocator`, carried in `PmapReservationIntermediates`, and
rolled back if the reservation is abandoned. Existing mappings return
`Ok(None)`, conflicting slots return `AlreadyMapped`, and non-identity or
out-of-range requests remain `Unsupported`.

This remains the QEMU virt-based M1 mock. It is not a K210 hardware boot path,
does not yet implement process roots, and does not claim general Sv39 mapping
outside the UART window.

## Verification

- Red first: `cargo test -p tx-hal-riscv64-m1dock-mock` failed because the root
  was zero and UART-neighbor reserve returned `Unsupported`.
- Red follow-up: `cargo test -p tx-hal-riscv64-m1dock-mock second_uart_window_allocates_l0_before_commit`
  failed because the next UART 2 MiB window returned `Unsupported` before
  PT-node-backed L0 allocation existed.
- `cargo test -p tx-hal-riscv64-m1dock-mock`
- `cargo xtask build --target rv64-m1dock-mock`
- `cargo xtask qemu --target rv64-m1dock-mock --profile smoke --expect-sentinel --timeout-ms 10000`
- `RUSTFLAGS=-Dunused cargo check -p tx-kernel-riscv64-m1dock-mock --target riscv64gc-unknown-none-elf`

## Next

Grow the M1 mock pmap in this order:

1. Add committed PT-node ownership for later pruning.
2. Add kernel mapping unmap/protect and local invalidation.
3. Add process-root creation with shared kernel mappings.
4. Only after that, discuss real K210 packaging or hardware boot separately.
