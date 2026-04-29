# RV64 High-VMA Low-LMA Linker

**Date:** 2026-04-29

## Decision

RV64 QEMU now links the Rust kernel at high VMA while keeping low physical load
addresses. OpenSBI still enters `_start` at `0x8020_0000`, but `_start` lives
alone in `.text.trampoline` and uses only suffixed `_load` symbols while
translation is off. Canonical linker symbols such as `__kernel_start`,
`__text_start`, `__bss_start`, `__tx_boot_stack_top`, `__global_pointer$`, and
`rust_entry` are high VMA facts.

The trampoline clears BSS, constructs the identity, direct-map, and high-kernel
bootstrap page tables from low physical symbols, enables Sv39, rewrites
`sp`/`gp`, and jumps to high `rust_entry`. High Rust then constructs the
`BootStaticBag`, publishes `BootstrapPmapInfo`, parses the firmware DTB, proves
high `pc`/`sp`/`gp`, and clears the temporary low identity bridge before
substrate init.

This supersedes the temporary low-linked identity-retention decision. The
explicit pmap identity-teardown tests remain useful guards, but live RV64 QEMU
boot now exercises the teardown path.

## Verification

- `cargo test -p xtask rv64_qemu -- --nocapture`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo test -p tx-substrate`
- `cargo xtask build --target rv64-qemu`
- `rust-readobj --file-headers target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt`
- `rust-readobj --program-headers target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt`
- `rust-nm -n target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask lint unused`

## Next

Keep `.text.trampoline` assembly-only and keep low symbols suffixed `_load`.
Future boards can adopt the same high-VMA/low-LMA phase contract without
sharing RV64 QEMU's concrete linker storage layout.
