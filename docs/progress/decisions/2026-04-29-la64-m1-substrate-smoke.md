# LA64 and M1 substrate smoke handoff

## Context

LA64 QEMU and the RV64 M1 Dock mock had board-owned `_start` paths plus static
`BootInfo` and `BootstrapPmapInfo` facts. The remaining blocker for running
`tx_substrate::init::<P>()` in smoke mode was the typed PT-node allocator
handoff installed by page-substrate after frame allocation.

## Decision

LA64 QEMU and the QEMU/OpenSBI M1 Dock mock now set
`SUBSTRATE_BOOT_READY=true`. This is a substrate-smoke claim only.

Both boards implement a board-local, one-shot `PmapIf::install_pt_node_allocator`
slot, delegate `PmapIf::alloc_pt_node()` to the installed allocator, and release
typed nodes through `PmapIf::free_pt_node()`. They do not add fake kernel
mapping mutation, fake process roots, or fake MMIO mapping. In this slice
`PlatformInfo` kept `mmio_regions` empty, and early console output remained an
early boot convention rather than a substrate-mapped device claim. A follow-up
slice published only the identity-precovered UART MMIO pages; see
`docs/progress/decisions/2026-04-29-la64-m1-identity-mmio-pmap.md`.

RV64 QEMU remains the only board with a real high-VMA pmap mutation path,
process-root handoff, kernel MMIO mapping, and BootStaticBag authority. The LA64
and M1 smoke boards intentionally keep their memory/pmap facts local instead of
extracting a shared bag abstraction.

## Verification

- `cargo test -p tx-hal-loongarch64-qemu-virt`
- `cargo test -p tx-hal-riscv64-m1dock-mock`
- `cargo fmt --check`
- `cargo xtask check`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask build --target la64-qemu`
- `cargo xtask build --target rv64-m1dock-mock`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask qemu --target la64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask qemu --target rv64-m1dock-mock --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `git diff --check`

## Next

Add real board pmap mutation, process-root support, and MMIO mapping before
claiming LA64 or M1 can run beyond substrate smoke.
