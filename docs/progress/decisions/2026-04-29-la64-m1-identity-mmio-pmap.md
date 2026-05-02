# LA64 and M1 identity MMIO pmap bridge

## Context

LA64 QEMU and the RV64 M1 Dock mock had substrate smoke running with static
memory facts and a typed PT-node allocator handoff, but `PlatformInfo` still
advertised no MMIO. That avoided a false mapping claim, but it also meant
substrate phase 3 was not exercising board MMIO facts on these ports.

## Decision

Both boards now publish exactly one UART MMIO region:

- LA64 QEMU: `uart0` at `0x1fe0_01e0`, page-covered by `0x1fe0_0000`.
- RV64 M1 Dock mock: QEMU virt `uart0` at `0x1000_0000`.

`PmapIf::reserve_kernel_mapping()` returns `Ok(None)` only for the exact
page-aligned identity mapping that substrate phase 3 requests for those UART
pages. `Ok(None)` means the board reports the mapping is already covered by the
current early execution model, so substrate does not commit a new PTE. Any
non-identity request, different page, or different granularity still returns
`PmapError::Unsupported`.

This is a deliberate bridge, not full pmap parity. LA64 still needs real
DMW/page-table ownership and M1 still needs a real Sv39 mutation root before
either board can claim general kernel mapping mutation, process roots, or driver
MMIO beyond this identity-covered boot page.

Follow-up: the M1 mock now has a real Sv39 identity bootstrap root and a narrow
UART-window reserve/commit path; see
`docs/progress/decisions/2026-04-29-m1dock-mock-sv39-bootstrap-pmap.md`.

## Verification

- `cargo test -p tx-hal-loongarch64-qemu-virt`
- `cargo test -p tx-hal-riscv64-m1dock-mock`
- `cargo fmt --check`
- `cargo xtask qemu --target la64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask qemu --target rv64-m1dock-mock --profile smoke --expect-sentinel --timeout-ms 10000`

## Next

Replace the identity-precovered bridge with board-owned pmap mutation:

- LA64: activate/document the DMW/MMU ownership model, then map non-identity
  MMIO and implement invalidation.
- M1 mock: establish an Sv39 root for the QEMU virt mock, then add kernel
  mapping mutation and later process-root creation.
