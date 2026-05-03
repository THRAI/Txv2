# M1 Dock mock process roots and ASIDs

## Context

The RV64 M1 Dock mock already reached high-VMA Rust entry and substrate smoke on
QEMU/OpenSBI, and its board-private pmap could reserve, commit, protect, and
unmap high direct-map low-MMIO kernel pages. The next memory-prep gap before
VM-facing work was the `PmapIf` process-root surface: root allocation, ASID
assignment, user mapping materialization, and teardown ownership for committed
user page-table intermediates.

## Decision

The M1 mock now implements the same HAL vocabulary shape as RV64 QEMU without
extracting a shared bag abstraction. `src/pmap.rs` owns a fixed ASID bitmap with
ASID 0 reserved, allocates `PmapRoot` page-table nodes through the installed
`PtNodeAllocator`, copies only the kernel high half from the bootstrap root,
and recursively tears down the user half before returning the ASID and root
node.

User mappings support `Superpage1G`, `Superpage2M`, and `Page4K`
reserve/rollback/commit paths, validate the user allocation ceiling, register
committed L1/L0 intermediates, and prune empty committed tables on unmap.
`protect_mapping()` rewrites same-granularity leaves in place and
`shootdown_mapping()` issues the local Sv39 fence on target. This remains a
QEMU/OpenSBI mock milestone; it does not claim real K210 hardware boot.

The private M1 pmap tests moved to `src/pmap_tests.rs` so the production pmap
file can grow this behavior while staying under the 1,500-line authored source
limit enforced by `cargo xtask lint arch`.

## Verification

- `cargo test -p tx-hal-riscv64-m1dock-mock process_root_ -- --nocapture`
- `cargo fmt -p tx-hal-riscv64-m1dock-mock --check`
- `cargo test -p tx-hal-riscv64-m1dock-mock`
- `cargo clippy -p tx-hal-riscv64-m1dock-mock --all-targets -- -D warnings`
- `cargo xtask build --target rv64-m1dock-mock`
- `RUSTFLAGS=-Dunused cargo check -p tx-kernel-riscv64-m1dock-mock --target riscv64gc-unknown-none-elf`
- `cargo xtask qemu --target rv64-m1dock-mock --profile smoke --expect-sentinel --timeout-ms 10000`
- `cargo xtask lint arch`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `git diff --check`

`cargo xtask check` was also attempted after this slice. It is currently
blocked outside the M1 pmap work by `crates/tx-ext4/src/host_async.rs` tripping
Clippy's `manual_is_multiple_of` lint.

## Next

Keep M1 on the QEMU/OpenSBI substrate-smoke path until the VM AddressSpace,
trap/user-return shell, and real K210 boot/linker path exist. LA64 still needs
its own process-root/ASID-equivalent path rather than copying the M1 Sv39 code.
