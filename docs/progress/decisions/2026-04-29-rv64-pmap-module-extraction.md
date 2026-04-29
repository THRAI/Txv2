# 2026-04-29: RV64 pmap module extraction

## Context

The RV64 QEMU pmap implementation had grown into one large file that mixed
bootstrap address-space construction, kernel mapping mutations, process-root
orchestration, PT-node allocation/teardown, and tests. The HAL pmap range
surface had also lived inline inside `tx-hal/src/lib.rs`.

## Decision

Split pmap code by responsibility while preserving the existing public HAL
surface:

- `tx_hal::pmap` range helpers now live in `crates/tx-hal/src/pmap.rs`.
- `pmap/address_space.rs` owns process root creation/destruction, ASID
  allocation, user mapping reserve/commit/protect/unmap, and recursive
  committed PT teardown.
- `pmap/pt_node.rs` owns the bootstrap PT-node pool, installed typed PT-node
  source, and committed PT-node registry.
- `pmap/kernel_space.rs` owns kernel/direct-map/MMIO mapping orchestration and
  leaf protect/unmap helpers used by both kernel and process roots.
- `pmap/mod.rs` remains the board facade for bootstrap/high-half pipeline and
  shared Sv39 table helpers; unit tests live in `pmap/tests.rs`.

## Verification

- `cargo fmt`
- `cargo test -p tx-hal -p tx-substrate -p tx-hal-riscv64-qemu-virt`
- `cargo fmt --check`
- `cargo xtask lint unused`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `cargo xtask ci`
- `git diff --check`

## Next Step

The remaining cleanup opportunity is to split the RV64 bootstrap/high-half
pipeline out of `pmap/mod.rs` once the next behavioral boot slice needs that
boundary.

## Blockers

No blocker for the extraction. Full user/VM readiness still depends on
superpage/multi-frame accounting, remote-hart shootdown, and the full trap
shell.
