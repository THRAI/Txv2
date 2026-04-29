# 2026-04-29: process roots, ASID shootdown, and boot anchors

## Context

After kernel pmap reserve/commit, protect, and committed-node teardown existed,
the remaining page-substrate pmap gap was the handoff shape for VM-owned
`AddressSpace` roots. The substrate also still treated boot metadata and
bootstrap pmap pages as reserved-but-dead metadata rows rather than explicit
permanent ownership.

## Decision

Add concrete v1 `PmapRoot` and `Asid` types to `tx-hal`. RV64 QEMU now creates a
process root by allocating a page-table root, copying the kernel high half from
the bootstrap root, and assigning an ASID from a fixed bitmap. Root teardown
walks only the user half and releases committed user intermediates through the
pmap `PtNode` ownership path before freeing the root and ASID.

Substrate now has an ASID-scoped shootdown batch next to the kernel-global
batch; both retain `MapPin`s until after the HAL invalidation call. During boot,
the installed bitmap allocator also claims permanent anchors for the kernel
image, allocator metadata, and bootstrap page-table ranges.

## Verification

- `cargo fmt`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo test -p tx-substrate`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`

## Next Step

Add VM-facing range wrappers over the single mapping operations, extend
shootdown accounting beyond page-sized pins, and install the full trap
shell/user-return boundary.

## Blockers

Remote-hart shootdown still needs the SMP IPI/ack path. The current ASID batch
uses local invalidation and is suitable for the single-hart RV64 QEMU path.
