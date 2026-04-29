# 2026-04-29: RV64 high-kernel alias final permissions

## Context

The bootstrap high-kernel alias originally used coarse 2 MiB RWX leaves. That
was enough to prove the low-to-high transition, but it left `PAGE_SUBSTRATE_v1`
with a standing gap: text, rodata, writable kernel storage, direct map, and MMIO
needed final kernel permissions before the path could be considered VM-ready.

## Decision

RV64 QEMU now maps the high-kernel alias through board-owned 4 KiB L0 tables:

- text pages are RX;
- rodata pages are R;
- data, bss, and boot-stack pages are RW;
- remaining high-alias pages outside the linked kernel image are unmapped.

The direct map and MMIO mappings remain RW and NX. The new kernel-alias L0 table
range is published through `BootstrapPmapInfo.reserved_page_tables`, so substrate
subtracts it from allocator-free RAM just like the root, kernel-alias L1, and
PT-node pool.

## Verification

- `cargo fmt`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`

## Next Step

Continue with process-root `PmapRoot`/ASID materialization, range pmap
operations, ASID/global shootdown batching, and the full trap shell.

## Blockers

No blocker for the kernel-only bootstrap path.
