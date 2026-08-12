# VM chunked resident and protect batch audit

Date: 2026-08-01

## Question

Determine the current state of the chunked pmap resident backend and explain
why `VmPmap::protect_range` does not batch pmap invalidations even though the
HAL exposes `shootdown_mappings`.

## Current state

- `ChunkedPmapResidentStore` is implemented behind the resident-store facade.
  It uses 64-entry sorted chunks, removes fully covered chunks without shifting
  the global resident suffix, and retains only boundary-chunk shifts.
- The current checkout still selects `VecPmapResidentStore` by default.
  `tx_vm_pmap_chunked_resident` is only an opt-in cfg. Existing tests compare
  both backends for lookup, replacement, removal, boundary drains, whole-chunk
  drains, and 4096 mixed sparse operations.
- The durable June 5 state still names a lossless pthread/map-path A/B as the
  promotion gate. No matching chunked guest artifact was found. The June 4
  map-path artifact is a Vec baseline and shows why the experiment matters:
  `pmap.teardown_shifted_entries` reached 11,295 and
  `pmap.teardown_drain_ns` reached 27.810 ms.
- A July 21 commit on the separate `origin/final-smp` lineage promoted chunked
  to the production alias as part of a broad EBR/vmalloc change, but that
  commit is not an ancestor of this checkout. Its progress evidence verifies
  compilation and subsystem tests, not the previously requested resident
  guest A/B, so it is useful implementation precedent rather than a current
  promotion receipt.

## Why protect is not batched

`protect_range` was added on May 13 as a narrow fork CoW optimization: keep
parent resident pages hot by changing their PTEs to read-only instead of
tearing them down. It calls the plural HAL entry with a one-element slice after
every page.

The May 31 pmap optimization later changed `teardown_range` to collect all
invalidations and issue one shootdown, including flushing an already modified
prefix before returning an error. That change only made `protect_range`
enumerate resident pages; it did not move protect invalidations into the new
collector. This is a scoped historical omission, not a missing HAL capability.

On RV64, one real batch matters: the board coalesces adjacent invalidations,
does ASID-scoped local fences, computes the remote ASID-residency target mask
once, and sends one SBI RFENCE per coalesced range. Passing singleton slices
defeats coalescing and repeats the remote coordination. LA64 and the M1Dock mock
currently inherit the trait's per-invalidation fallback, so they preserve
correctness but do not receive the same batching benefit.

## Recommended landing order

1. Add a VM regression with multiple resident private pages that requires one
   protect shootdown batch containing all invalidations. Add a partial-failure
   regression that requires the successfully modified prefix to be flushed.
2. Change `protect_range` to update PTEs and resident shadow state per page,
   collect ordered invalidations, and issue once at the end or immediately
   before an error return. Keep `shootdowns` as a batch count.
3. Run the focused VM tests and RV64 pmap coalescing tests, then the normal VM
   and host unit gates.
4. Run an isomorphic, lossless guest A/B for Vec versus chunked with only
   `tx_vm_pmap_chunked_resident` changed. Compare teardown total/drain/shift,
   protect batch count, clone latency, and regressions on insert-heavy sparse
   workloads.
5. Promote chunked only if the guest receipt closes the insertion-versus-drain
   tradeoff. The source change is small, but the evidence gate is still open.

## Verification for this audit

- CodeGraph call-path and source exploration for `VmPmap`, `PmapIf`, RV64
  shootdown, and the resident-store facade.
- Git history/blame for the May 13 protect path, May 31 teardown batching, June
  5 chunked backend, and the separate July 21 promotion precedent.
- No runtime benchmark or source-code test was run; this pass changed progress
  documentation only.

## Blockers

- No lossless chunked guest A/B artifact exists in the current progress tree.
- The current checkout has unrelated dirty changes, including an existing
  `STATUS.md` edit; any implementation must keep its write set to VM pmap/tests
  plus this progress record.
