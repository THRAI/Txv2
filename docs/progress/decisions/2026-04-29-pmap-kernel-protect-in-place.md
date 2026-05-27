# Kernel pmap protect-in-place

## Context

PAGE_SUBSTRATE_v1 distinguishes safe permission updates from unsafe
rematerialization cases. The current pmap surface had reserve/commit/unmap for
kernel mappings, but no protect operation.

## Decision

`PmapIf` now exposes `protect_kernel_mapping(virt, kind, permissions)`.
`PmapPermissions` provides small portable permission evidence for read, write,
execute, user, and global bits plus kernel RO/RW/RX presets.

RV64 QEMU implements safe same-granularity kernel leaf updates in place. If the
leaf exists and the requested permissions are valid for Sv39, pmap rewrites the
PTE while preserving the physical address and returns a `PmapInvalidation`.
Absent mappings return `None`; unsafe cases such as split-required superpages
return `InvalidRequest` so VM can leave the authoritative binding intact and
fault/rematerialize later.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt protect_kernel_mapping`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo test -p tx-substrate`

## Next

Continue PAGE_SUBSTRATE_v1 with the concrete RV64 trap vector, then return to
pmap root/range work and committed intermediate ownership tracking.
