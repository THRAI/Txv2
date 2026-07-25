# Memory and File-I/O Dual-Plane Architecture

Date: 2026-07-25

## Context

The one-CPU, 4-GiB, no-swap clean build of the large Rust repository takes
hours under Tx, while the comparable Linux baseline is approximately 6000
seconds. The current checkout already has PageBacked file pages, typed frame
tokens, generation-checked `PageSlot`, neutral page/block plans, an existing
`BackendBioGraph`, adjacent-LBA merge, direct-I/O DMA pins, and ext4 mutation
and journal staging. It lacks one consistent cross-layer contract for global
memory pressure, file-page ownership, zero-copy payload planning, and ext4
metadata freezing.

Older component prose also disagreed about dirty authority. Some sections
assigned dirty and I/O-lock state to `FrameMeta`; the newer I/O design and live
`PageSlot` implementation use a generation-checked owner state machine. The
ext4 plan additionally prescribed a filesystem-private global `FrameMeta` CLOCK
sweep even though reclaim policy must coordinate PageBacked, VFS, ext4 metadata
and Zone/slab owners.

## Decision

1. Adopt two orthogonal planes:
   - the file-I/O data plane carries PageBacked payload leases through pure
     filesystem layout planning and the existing BIO DAG to typed completion;
   - the global memory-pressure control plane coordinates allocator pressure,
     pure policy, owner reclaim providers, bounded reclaim/writeback, and
     allocation retry.
2. PageBacked is the only long-lived ordinary file-data cache owner. VM is a
   consumer; ext4 owns layout, allocation, metadata and JBD2 semantics; the I/O
   manager owns only request execution; the physical allocator retains bitmap,
   PPN, `FrameMeta` and typed-token authority.
3. Freeze four behavioral interfaces: `FileLayoutPlanner`, `ReclaimProvider`,
   `MemoryPolicy`, and `AllocationGateway`. Freeze two core immutable values:
   `PageDataLease` and the existing `BackendBioGraph`. Add one ext4-internal
   transaction capability: `FrozenMetadataLease`. Do not create parallel lease,
   planner, graph, or completion stacks.
4. `PageSlot` is the sole semantic authority for ordinary file-page fetch,
   dirty, writeback, redirty, error, and completion generation. Replacement
   metadata contains only referenced/no-reclaim/age/queue hints. `FrameMeta`
   contains physical-lifecycle and role evidence only.
5. The aligned normal ordinary-file payload path has zero PageBacked-to-device
   copy and zero bounce bytes. Metadata freeze and JBD2 encoding copies are
   separate accounting domains because immutable transaction after-images and
   escaped/checksummed journal records may require distinct storage.
6. Use compatible extraction: preserve the current I/O runtime and graph,
   establish module/API dependency seams first, and move to target crates only
   when the seam is stable.
7. The initial control policy is clean-only, bounded second-chance reclaim with
   persistent provider cursors and actual allocator-free feedback. Dirty
   writeback, metadata providers and refault-aware policy land only after their
   ownership and generation protocols are executable.
8. Align `IO_MANAGER_v1` as the file-I/O execution plane. The coordinator sends
   bounded intent to PageBacked owner admission, never raw pages or direct
   `PageSlot` mutations. The Tx filesystem adapter has resource custody only
   during planning/lowering; successful graph admission atomically transfers
   custody to L4. L6 completes ready BIO nodes but cannot release leases or
   clean pages. L4 transfers one owned terminal settlement back to PageBacked,
   which alone validates generations and commits `PageSlot` transitions.

The canonical contract is
[`MEMORY_IO_ARCHITECTURE_v1.md`](../../design/03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md).

## Rejected alternatives

- **One VM BufferManager owning allocator and caches:** rejected because it
  collapses semantic ownership into a new global authority and lock domain.
- **Six parallel interface families:** rejected because Tx already has useful
  lease, graph, and completion values; parallel types would create ambiguous
  lifetimes and two production paths.
- **Filesystem-private global CLOCK and synchronous allocator reclaim:**
  rejected because heterogeneous owners need separate claim rules, and
  allocator locks must not enter filesystem/I/O work.
- **Immediate generic storage-framework rewrite:** rejected because it expands
  the correctness and performance validation surface before the present seams
  are measured and stabilized.
- **Absolute zero-copy metadata journaling:** rejected because stable immutable
  after-images and JBD2 record encoding may require controlled copies. Ordinary
  file payload remains zero extra copy on the aligned normal path.

## Consequences

- The new umbrella document supersedes component prose that assigns ordinary
  dirty/writeback authority to `FrameMeta`, prescribes raw-frame CLOCK selection,
  or recursively performs synchronous writeback from allocation failure.
- `PAGE_SUBSTRATE_v1`, `PAGE_BACKED_v1`, `IO_MANAGER_v1`, and
  `TX_EXT4_PLAN_v1_2` retain local authority but now point to the umbrella
  contract for cross-layer behavior.
- `IO_MANAGER_v1` now distinguishes L4 graph/resource custody from L6 BIO-node
  execution and preserves the existing `BackendBioGraph` as the sole execution
  DAG. Current `FsPageBacking`, `PageIoPlan`, `BackendPlan`, and `BioPlan` names
  are compatibility vocabulary, not a second target interface family.
- The first implementation gate is dirty-authority unification and canonical
  PageContainer identity, followed by multi-page lease/planner extraction and
  clean-only pressure coordination.
- Performance targets are product milestones rather than algorithm contracts:
  first <= 9000 seconds, then <= 7200 seconds on the fixed one-CPU/4-GiB clean
  build witness, with attribution and copy/write-amplification counters.

## Verification and next step

This decision changes documentation only. `cargo xtask lint docs` passed, with
the 7 existing active-doc warnings that intentionally discuss retired
vocabulary. `cargo xtask progress validate` passed for 37 records, and `git
diff --check` passed. The next step is an implementation-readiness audit and a
staged plan beginning with `PageSlot` dirty authority and static ratchets. The
one-CPU, 4-GiB clean-build witness remains blocked on a current complete Tx
trace and baseline artifact.
