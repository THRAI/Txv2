# LA64 and M1 memory/pmap prep facts

## Context

LA64 QEMU and the RV64 M1 Dock mock already reach the generic
`tx_kernel::kernel_main` smoke path, but still must not run substrate init.
The next useful slice is to publish board truth that future substrate readiness
can build on without copying RV64 QEMU's board-private `BootStaticBag`.

## Decision

Both boards now publish static `BootInfo` and descriptive
`BootstrapPmapInfo` facts. In the first prep step they left
`SUBSTRATE_BOOT_READY` at the default false value; a same-day follow-up added
the typed PT-node allocator handoff and enabled substrate smoke.

LA64 QEMU records QEMU `virt` RAM as `0x0..0x1000_0000`, reserves the
low/kernel-loaded range through the aligned linker-derived kernel end, and
derives `kernel_image` from `__kernel_start..__kernel_end`. Its bootstrap pmap
facts describe the current identity/direct execution model with no hardware
page-table root, no PT-node pool, and no reserved page-table ranges.

The RV64 M1 Dock mock records QEMU/OpenSBI RAM as
`0x8000_0000..0x9000_0000`, adds the reserved firmware loader gap
`0x8000_0000..0x8020_0000`, and derives `kernel_image` from linker symbols.
Its bootstrap pmap facts likewise describe the current identity/direct model.
This remains a QEMU/OpenSBI mock path, not real K210 hardware boot.

The first prep pass intentionally left `PlatformInfo.mmio_regions` empty for
substrate mapping. Their early consoles worked through the current
firmware/direct execution convention, but claiming general substrate-mapped
MMIO before a clear board pmap story would have been misleading.

Follow-up: see
`docs/progress/decisions/2026-04-29-la64-m1-substrate-smoke.md` for the
allocator-handoff slice that flipped both gates on for substrate smoke only.
Another same-day follow-up publishes the UART MMIO pages as identity-precovered
phase-3 substrate mappings; see
`docs/progress/decisions/2026-04-29-la64-m1-identity-mmio-pmap.md`.

## Verification

- `cargo test -p tx-hal-loongarch64-qemu-virt`
- `cargo test -p tx-hal-riscv64-m1dock-mock`
- `cargo xtask build --target la64-qemu`
- `cargo xtask build --target rv64-m1dock-mock`
- `cargo xtask qemu --target la64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask qemu --target rv64-m1dock-mock --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask check`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`

## Next

Implement real board pmap mutation, MMIO mapping, and process-root support
before treating either board as more than substrate-smoke ready.
