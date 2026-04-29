# RV64 High-Half Entry Handoff

**Date:** 2026-04-28

## Decision

RV64 QEMU now enters the board binary's `rust_entry` through the high kernel
alias instead of calling the low linked symbol after `satp` is installed.

`BootStaticBag` captures the linked boot stack top, `__global_pointer$`, and
`rust_entry` address. `HighBootTransition` checks that all three addresses are
inside the kernel alias window and converts them to high virtual addresses.
The `_start` path writes this transition triple to the low boot stack, installs
the bootstrap page table, rewrites `sp` and `gp`, then jumps to the high
`rust_entry` PC.

The 1 GiB low identity leaf remained mapped for this slice as a teardown bridge,
not the Rust execution address. The follow-up identity-teardown sentinel now
removes it after BootInfo has consumed the firmware DTB pointer.

## Related Cleanup

The RV64 kernel image link exposed an accidental pre-heap `alloc` dependency in
`tx-substrate`: `OwnedFrameRun::split()` returned a `Vec`. It now returns a
no-alloc iterator that releases unyielded frames on drop, preserving the token
state machine without requiring a global allocator before the slab exists.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo test -p tx-substrate`
- `cargo test -p xtask`
- `cargo check -p tx-hal-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo fmt --check`
- `cargo xtask lint arch`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `git diff --check`

## Next Step

Split the high kernel alias into final text RX, rodata R, and data/bss/stack RW
permissions, then extend the direct map beyond the first GiB when platform RAM
requires it.
