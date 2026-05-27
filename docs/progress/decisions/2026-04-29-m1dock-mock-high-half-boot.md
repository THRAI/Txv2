# M1 Dock mock high-half boot

## Context

The M1 Dock mock had enough early Sv39 state to reach substrate, but it still
ran as a low-linked identity/direct kernel. That made process-root work
irrational: HAL_v1 and PAGE_SUBSTRATE_v1 require every process root to share the
kernel high half, while the M1 mock had no high kernel half to copy.

## Decision

The QEMU/OpenSBI M1 mock now uses a high-VMA/low-LMA RISC-V boot shape. The
linker keeps only `.text.trampoline` at `0x8020_0000`; H1 assembly clears BSS
through load addresses, builds the bootstrap root, enables Sv39, rewrites
`sp`/`gp`, and jumps to high `rust_entry` at `0xffff_ffff_8020_0000`.

The bootstrap root maps:

- QEMU RAM at the temporary low identity slot for the early bridge.
- QEMU RAM at the high direct-map alias rooted at `0xffff_ffc0_0000_0000`.
- The kernel high alias with a coarse 1 GiB bootstrap leaf.
- The QEMU UART at its high direct-map MMIO alias so substrate's early MMIO
  mapping pass sees it as precovered.

`PlatformConfig` now advertises high direct-map, high kernel, and Sv39 user-top
constants. `BootstrapPmapInfo` reports high direct-map/kernel virtual facts
while still recording the temporary low identity RAM bridge.

This is still the QEMU/OpenSBI mock path only. It does not claim real K210
hardware boot, and it still avoids RV64 QEMU's `BootStaticBag` abstraction.

## Verification

- Red first:
  `cargo test -p tx-hal-riscv64-m1dock-mock high_half_platform_config_and_mmio_are_declared`
  failed because `DIRECT_MAP_BASE` was still `0`.
- `cargo fmt -p tx-hal-riscv64-m1dock-mock --check`
- `cargo test -p tx-hal-riscv64-m1dock-mock`
- `cargo xtask build --target rv64-m1dock-mock`
- `RUSTFLAGS=-Dunused cargo check -p tx-kernel-riscv64-m1dock-mock --target riscv64gc-unknown-none-elf`
- `cargo xtask qemu --target rv64-m1dock-mock --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo fmt --check`
- `cargo xtask lint arch`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `git diff --check`

Full `cargo xtask check` was also attempted. It is currently blocked outside
this HAL/pmap slice by unrelated `tx-ext4-format` clippy failures
(`new_without_default` and `len_without_is_empty` in
`crates/tx-ext4-format/src/ondisk.rs`).

## Next

The follow-up module split moved the M1 pmap implementation into `src/pmap.rs`.
The next pmap behavior step is process-root/ASID support.
