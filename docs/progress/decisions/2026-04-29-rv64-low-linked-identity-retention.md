# RV64 Low-Linked Identity Retention

Date: 2026-04-29

## Decision

Superseded by
`docs/progress/decisions/2026-04-29-rv64-high-vma-low-lma-linker.md`.
This note records why identity retention was necessary while RV64 QEMU was
still low-linked.

RV64 QEMU kept the low identity mapping live during the earlier low-linked Rust
boot path, even after validating that PC/SP/GP had crossed to the high kernel
alias. The explicit identity-teardown pmap operation remained implemented and
unit-tested, but it was not used by live substrate boot until the kernel was
linked at its high VMA with a low load address.

RV64 QEMU `BootInfo` also publishes the firmware/kernel-loader gap
`[0x8000_0000, 0x8020_0000)` as reserved RAM. Substrate must not carve
`FrameMeta[]`, bitmaps, or page allocations from OpenSBI-owned physical pages.

## Evidence

`cargo xtask ci-slow` failed in the RV64 smoke lane after substrate boot started.
QEMU trace showed:

- an instruction page fault at low linked text inside
  `core::sync::atomic::atomic_load::<usize>` after identity teardown;
- then, after identity was retained, a store access fault through the direct map
  to physical `0x8000_0000`, proving metadata placement had selected
  firmware-owned RAM.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo xtask ci-slow`

## Next

Implement a high-VMA/low-LMA linker plan or relocation pass before making live
identity teardown part of H2/H3 boot. Keep the explicit pmap teardown tests as
the guard for that future slice.
