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

## Current readiness

| Boundary | Current state | E3 action |
|---|---|---|
| Lease storage | `Box<[PageLease]>`, but only `single()` and `pages[0]` projection | add checked multi-segment construction/projection |
| Read target | one retained `CachedFrame` | retain a bounded target bundle before multi-page fetch admission |
| L4 admission | service submission and owner insertion are separate | migrate atomic request-plus-owner publication first |
| Planner payload | one `IoDataSource` and one `IoDataTarget`; an unused multi-source projection exists | add an opaque segment-preserving representation without aliasing `Direct` user DMA |
| Terminal route | one completion generation/frame and `page_count == 1` checks | aggregate one request's per-page facts before consuming its owner |
| L6 | already value-only and SG-capable | no ownership redesign |

The first implementation slice is the atomic L4 publication helper and its
writeback/fetch call sites, followed by the checked multi-segment lease types.
The plan stays in progress until real two-page admission, partial failure,
stale/redirty settlement, and exactly-once release tests pass.

## E3 slice landed

The first implementation slice is now present in the candidate worktree:

- L4 publishes a `PageService` request and its `OwnedFileIoRequest` under one
  manager lock; writeback and planner-backed fetch use this path.
- `PageDataLease` retains `(PageIndex, PageGeneration, PageLease)` segments,
  validates a caller-supplied bound and contiguous order, and emits a neutral
  multi-source projection while the single-page source remains compatible.
- Focused tests cover atomic owner visibility, two-segment generation/frame
  order, projection shape, and empty/hole rejection.

This is intentionally not a complete multi-page runtime: fetch targets still
arrive one page at a time, ext4 lowering still consumes one `PageCache` source
or target, and terminal completion still rejects `page_count != 1`. Batch
admission, graph/range fanout, stale/redirty sibling settlement, and
exactly-once bundle release remain the next E3 gate.

Verification for this slice:

- `cargo -q xtask unit`: passed (`tx-shims` 663, `tx-kernel` 119,
  `tx-ext4` 73 with 2 ignored, `tx-scripts` 168).
- `cargo test -p tx-subsystems --lib page_data_lease -- --test-threads=1`:
  3 passed.
- `cargo test -p tx-subsystems --lib file_io_owner_is_admitted_before_request_becomes_service_visible -- --test-threads=1`:
  1 passed.
- targeted `rustfmt --edition 2021 --check` and `git diff --check`: passed.

## Verification baseline

- `cargo xtask progress validate` is currently blocked before these edits by
  `docs/progress/plans/2026-08-04-smp-pelt-scheduler.json`, which references the
  absent `docs/progress/research/2026-08-04-smp-scheduler-readiness-audit.md`.
  This unrelated baseline record is not changed by E3.
- The candidate worktree started clean on branch `codex/ext4-smp-migration`.
- `.codegraph/` is absent in the candidate worktree, so source discovery uses
  `rg` and selected line reads.

## Next gate

Land and test atomic L4 publication, then implement the multi-segment owner and
terminal fanout without widening into writable mappings or JBD2 transaction
aggregation.
