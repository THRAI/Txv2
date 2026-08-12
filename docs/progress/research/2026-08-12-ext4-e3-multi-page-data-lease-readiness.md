# Ext4 E3 multi-page data-lease readiness

Date: 2026-08-12

## Decision

Start the ext4-to-SMP migration with the existing `PageDataLease` and
`OwnedFileIoRequest` ownership chain. The RCU-on-VM worktree is only a source
for its atomic L4 request-plus-owner publication helper. It does not contain a
real multi-page read or writeback path, so its PageBacked runtime, reactor,
namespace, fsync, and background-service code must not be imported.

E3 remains one vertical ownership change:

1. PageBacked acquires a bounded, contiguous set of page generations and DMA
   retentions.
2. One `OwnedFileIoRequest` transfers the complete bundle to L4 atomically
   with request publication.
3. Filesystem planning sees only immutable, opaque segment descriptors.
4. L6 keeps returning value-only BIO/node completion facts.
5. One terminal L4 return consumes the owner once, then PageBacked settles
   every participating PageSlot against its captured generation.

## Canonical Gate

| Proposed item | Authority | Gate |
|---|---|---|
| Extend the private `PageDataLease` in place to retain multiple page-cache frames and pins | `MEMORY_IO_ARCHITECTURE_v1.md` section 5.1; `PAGE_BACKED_v1.md` dirty-page reclaim row; current `page_backed/lifecycle.rs` | implement in E3 |
| Preserve a one-page compatibility constructor while adding checked bounded construction and neutral segment projection | `MEMORY_IO_ARCHITECTURE_v1.md` sections 4.3 and 5.1; current `PageDataLeaseProjection` | implement in E3 |
| Publish the queued request and `OwnedFileIoRequest` under one L4 manager lock | `MEMORY_IO_ARCHITECTURE_v1.md` section 4.3; `EXT4_LIFECYCLE_v1.md` section 3; verified helper in the RCU-on-VM worktree | migrate in E3 |
| Retain per-page generation facts in the owner bundle and consume the owner exactly once at terminal settlement | `MEMORY_IO_ARCHITECTURE_v1.md` sections 3.2, 4.3, and 5.1; `EXT4_LIFECYCLE_v1.md` sections 2-3 | implement in E3 |
| Keep BIO graphs, tags, queue depth, and L6 completion resource-neutral | `MEMORY_IO_ARCHITECTURE_v1.md` section 4.3; current block runtime and graph scheduler | preserve |
| Let ext4 `MutationHandle` retain child request IDs rather than page-data leases | `EXT4_LIFECYCLE_v1.md` section 4 | preserve; E5 convergence |
| Freeze writable PTEs across writeback | No active design defines the owned VM lease and restore protocol | needs design update; defer to E4 |
| Apply RCU to PageSlot contents, DMA pins, RangeLock, PTEs, shootdown, or JBD2 phase | Explicitly rejected by the current migration audit and memory/I/O ownership contract | do not implement |

## E3 writeback slice complete

The candidate now implements the bounded multi-page writeback custody path:

- L4 publishes a `PageService` request and its `OwnedFileIoRequest` under one
  manager lock. Writeback and planner-backed single-page fetch both use the
  atomic request-plus-owner path.
- `PageDataLease` retains checked, contiguous
  `(PageIndex, PageGeneration, PageLease)` segments. One lease supports up to
  64 dirty pages while preserving the single-page compatibility entry point.
- `queue_dirty_file_writeback()` groups adjacent frontier pages. One batch
  creates one `PageIoRange`, one request, and one retained owner; any failed
  pre-admission step rolls back every PageSlot transition and cache pin.
- Terminal handling consumes the owner once and settles each PageSlot using
  the generation stored in the lease. Submit failure, device error, redirty,
  one stale sibling, and duplicate terminal paths are covered.
- `PageCacheSegments` carries opaque frame segments through the filesystem
  interface. The ext4 mapped planner emits one SG BIO only when every segment
  maps to consecutive physical blocks; holes, metadata-first mapping,
  noncontiguous blocks, and malformed lengths/counts fail closed.
- A custom ext4/JBD2 write planner remains authoritative. Its journal bridge
  splits a valid multi-page source into ordered per-page sources with the same
  lease identity before mutation staging.
- Explicit fsync scans the frozen frontier, groups adjacent dirty pages into
  bounded requests, and falls back page-by-page only when a batch cannot be
  admitted.
- Partial L6 admission retains every non-empty accepted prefix and emits one
  sticky admission error only after the prefix reaches terminal completion.
  A zero-accepted ordinary BIO, metadata-first batch, or graph root fails
  synchronously, leaving no DMA route or continuation that could outlive its
  PageBacked owner.

Full regression exposed and fixed two earlier branch inconsistencies:

- synchronous demand-read planning now attributes terminal planner errors to
  the exact request and returns its errno instead of falling through to the
  compatibility pager as `EAGAIN`;
- `FileFsyncOp` sends only file-backed containers through the L4 frontier;
  anon/device containers use the existing `FsPageBacking` fallback instead of
  returning `EINVAL`.

Multi-page fetch/readahead and writable-PTE freeze are explicitly out of this
migration scope. JBD2 transaction aggregation, production cutover, and live
crash/xfstests evidence are later gates and are not claimed by E3.

Verification for the completed writeback slice:

- `cargo check -p tx-subsystems -p tx-ext4`: passed.
- `cargo test -p tx-subsystems --lib page_backed -- --test-threads=1`:
  191 passed.
- `cargo test -p tx-ext4 --lib -- --test-threads=1`: 80 passed, 2 ignored.
- `cargo test -p tx-ext4 --lib split_writeback_page_cache_segments -- --test-threads=1`:
  2 passed.
- `cargo -q xtask unit`: passed (`tx-shims` 663, `tx-kernel` 119,
  `tx-ext4` 80 with 2 ignored, `tx-scripts` 168).

## Verification baseline

- `cargo xtask progress validate` is currently blocked before these edits by
  `docs/progress/plans/2026-08-04-smp-pelt-scheduler.json`, which references the
  absent `docs/progress/research/2026-08-04-smp-scheduler-readiness-audit.md`.
  This unrelated baseline record is not changed by E3.
- The candidate worktree started clean on branch `codex/ext4-smp-migration`.
- `.codegraph/` is absent in the candidate worktree, so source discovery uses
  `rg` and selected line reads.

## Next gate

Refresh the current ext4 acceptance oracles, then converge the completed
writeback lease through the current `MutationHandle` and JBD2 ownership path.
Do not add multi-page fetch/readahead, writable-PTE freeze, PELT, or network
work to that phase.
