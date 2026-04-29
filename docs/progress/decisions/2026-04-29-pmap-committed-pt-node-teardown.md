# 2026-04-29: committed page-table node teardown

## Context

RV64 QEMU pmap rollback already released uncommitted intermediate tables, but a
committed branch PTE kept only the child table's physical address. After commit,
the pmap had no way to recover the `PtNode` token that carries release authority
for either static `PT_NODE_POOL` entries or typed page-table frames.

## Decision

Keep a board-private committed PT-node ownership registry for the current
kernel-only pmap subset. `commit_kernel_mapping()` registers newly allocated
intermediate `PtNode`s. `unmap_kernel_mapping()` prunes empty child tables and
looks the table physical address up in the registry before calling the existing
pmap release path.

This preserves the invariant from `PAGE_SUBSTRATE_v1`: page-table frame release
is a pmap-only operation, and branch-PTE teardown must recover typed release
authority before a page-table frame can re-enter the allocator.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo test -p tx-substrate`

## Next Step

Replace the fixed board-private registry with root-owned ownership state when
`PmapRoot`/ASID process roots are implemented. Continue toward final kernel PTE
permissions, range operations, ASID/global shootdown batching, and the full trap
shell.

## Blockers

No immediate blocker for the kernel-only pmap subset. The registry is a
bootstrap-era implementation detail, not the final process-root ownership model.
