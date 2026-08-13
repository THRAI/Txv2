# VM pmap materialization: PT sidecar and zero-copy range batches

Date: 2026-08-01
Status: recorded design decision; implementation not started

## Decision

The VM pmap optimization will align resident metadata with the hardware page-table
hierarchy. We will not promote the earlier proposal for an independent radix
resident index.

The authoritative/materialized split remains:

- `RecipeIndex` and `VmEntry` own range bindings, backing, protection intent, and
  binding generations.
- `PmapMaterializationRoot` owns the page-table materialization lifecycle.
- Each root-owned PT node carries a sidecar with child/slot occupancy, counts,
  generation, prune state, and leaf-local mutation state.
- A `MappingCell` sidecar record owns page-level `BindingStamp`, PPN/protection,
  `MapPin`, and transition state. `VmEntry` does not contain a dense page array.

The implementation must use four separately measurable batch layers:

1. detached resident/cell mutation chain;
2. fixed-block PTE mutation gather;
3. zero-copy invalidation run gather;
4. ASID/target-hart shootdown transport with acknowledgement and whole-ASID
   threshold selection.

Fixed-block chains may commit a successful prefix, shoot it down, release pins, and
continue. They must not construct a sorted `Vec`, copy a resident suffix, or release
an old `MapPin` before shootdown acknowledgement.

## Lock and ordering rules

The old `pmap.state` critical section is not an acceptable optimization boundary.
Resident/sidecar locks may cover claim, occupancy changes, and state publication only.
They must not cover HAL PTE mutation, shootdown, pin release, or an unbounded range
enumeration. A bounded PTE leaf walk under a leaf-local lock is allowed when it is
driven by occupancy bits.

Submission ordering is direction-aware:

- permission tightening, unmap, and source move: claim cells, weaken/clear PTEs,
  wait for shootdown, then commit recipe withdrawal/protection change and release
  pins;
- permission relaxation and destination publish: commit the new recipe, publish or
  patch PTEs, perform any required shootdown, then finalize cells.

This ordering is a design correction to the current `VM_v1_2.md` prose, which still
describes recipe-first teardown for every operation. It must be incorporated into
the canonical VM materialization contract before Rust implementation begins.

## Expected effects

The design targets zero resident suffix movement, no page-table empty-table scan per
4 KiB unmap, one range-level invalidation submission where transport permits it, and
shorter shadow locks. Existing measurements provide only estimates: resident drain
is expected to fall by roughly 60--90%, pmap-heavy tails by roughly 25--50%, and the
overall pthread guest path by roughly 5--15%. These are promotion hypotheses, not
acceptance results.

## Required gates

- RV64 root destroy/ASID reuse and remote shootdown acknowledgement correctness;
- LA64 shootdown action/activation semantics and M1Dock parity;
- sidecar occupancy/prune and partial-failure-prefix tests;
- protect and teardown proving one collected batch rather than page-sized singleton
  submissions;
- Vec/chunk/sidecar guest A/B receipts before changing the default backend;
- canonical updates to `VM_v1_2.md`, `PAGE_SUBSTRATE_v1.md`, `HAL_v1.md`, and any
  required invariant rows.

## References

- [VM pmap zero-copy batch design](../research/2026-08-01-vm-pmap-zero-copy-batch-design.md)
- [VM pmap full performance audit](../research/2026-08-01-vm-pmap-full-performance-audit.md)
- [VM chunked resident/protect batch note](../research/2026-08-01-vm-chunked-resident-protect-batch.md)
- `docs/design/03_memory-vm/VM_v1_2.md:1.1,5.3,5.4,9.8`
- `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md:7.3,7.4,9`
- `docs/design/01_substrate/HAL_v1.md:10.3-10.4`
