# RV64 High-Half Alias Bootstrap Slice

**Date:** 2026-04-28

## Decision

RV64 QEMU's first low-to-high transition slice keeps the existing low identity
1 GiB QEMU RAM leaf and adds high aliases in the bootstrap root:

- direct map at `0xffff_ffc0_0000_0000 + phys`, initially covering the first
  QEMU RAM GiB with a 1 GiB leaf;
- coarse high kernel alias starting at `0xffff_ffff_8020_0000`, backed by a
  dedicated bootstrap L1 table and 2 MiB leaves;
- richer `BootstrapPmapInfo` fields for direct-map, kernel-image, and temporary
  identity ranges.

The low identity leaf is a temporary bridge for the smoke path. It stays until
the high-half continuation and identity-teardown sentinel are proven, then it
should be removed from steady state.

## Context

The kernel is still physically linked at `0x8020_0000`. This decision first
implemented the safe alias-building milestone so page-substrate work could
reason about stable high addresses without breaking the current sentinel boot
path. A later high-entry slice now uses those aliases while keeping identity
mapped.

txKernel remains stackless at the task/thread level. Any stack transition in
this path is a rewrite from the low identity address of the current per-hart
stack to its high/direct-map alias, not a per-thread kernel-stack policy.

## Consequences

- Process page-table design can keep treating upper Sv39 root slots as shared
  kernel mappings.
- `USER_TOP` reserves the top 4 MiB for future user trampoline/signal/VDSO
  pages; phase 1 signal delivery still uses stack-resident trampoline bytes.
- Follow-up status: the high-entry and identity-teardown slices have now landed.
  Remaining pmap work is final kernel permission splitting, direct-map extension
  beyond the first GiB, reserve/commit/unmap, shootdown integration, and
  trap-vector bring-up.

## Verification

- Added and passed targeted RV64 QEMU pmap tests for the high direct-map root
  slot, temporary identity bridge, high kernel root branch, and published
  direct-map base.
