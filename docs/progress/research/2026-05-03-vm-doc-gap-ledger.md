---
date: 2026-05-03
topic: "VM/PageBacked doc gap ledger"
status: complete
worktree: docs/progress/worktrees/2026-05-02-vm-pagebacked-impl.json
---

# VM/PageBacked Doc Gap Ledger

## Question

How far does the current `codex/vm-pagebacked-impl` implementation differ from
the active VM and PageBacked contracts, and what mitigation order should future
VM-owned slices follow?

## Summary

The current implementation is a strong structural staging slice, not the full
`VM_v1_2` / `PAGE_BACKED_v1` system. It covers roughly 40% of the VM structure
contract and roughly 20-25% of the full behavior contract.

Implemented or mostly staged: `AddressSpace`, snapshot-published range-indexed
recipes, `RangeLock`, map/unmap/protect/disjoint-remap primitives, fault recipe
checks, generalized PrivateAnon/PageBacked fault materialization, pmap
publication/replacement and teardown, anonymous `PageContainer` materialization,
VFS/Mount/PageBacked interface shells, and read-only VM projections.

Major gaps: final persistent/epoch recipe snapshots, async syscall/trap
integration, real `fault_script` retry semantics, full CoW byte-copy fidelity,
file/device PageBacked materialization, PageBacked read/write/truncate/fsync/
reclaim, fork/exec/brk/madvise/msync/mincore, and Process/ThreadRuntime
ownership.

## Implemented

- `AddressSpace` exists as a zone entity with recipes, VM pmap ownership,
  `RangeLock`, and stats, matching the structural direction of
  `txdoc:VM-2-ADDRESSSPACE-STRUCTURE`.
- VM recipes are authoritative range bindings over `VmEntry` values, and pmap
  PTEs are treated as derived materializations, matching
  `txdoc:VM-1-AUTHORITATIVE-BINDINGS-AND-MATERIALIZATIONS-IN-VM`.
- `RecipeIndex` now publishes owned `BTreeMap<...>` recipe snapshots. Readers
  clone a `RecipeSnapshot`, and writers replace the tree only after building a
  complete rewrite, so observers see a complete pre- or post-mutation recipe
  set during split rewrites.
- `RangeLock` exists as a VM-local coordination primitive with materializer and
  exclusive-writer modes, declared-range behavior, RAII guards, and overlap
  exclusion tests for the v1 range-mutation cases described by
  `txdoc:VM-3-RANGELOCK`.
- Mapping primitives cover mmap-style free placement, fixed admission,
  unmap/protect split rewrites, reserve-map, and disjoint-only remap while
  preserving current error behavior for overlap, invalid range, would-block,
  and pmap failures. These are synchronous core helpers for the script surfaces
  described by `txdoc:VM-5-SYSCALL-SCRIPTS`.
- Fault resolution observes recipes, checks access permissions, materializes
  anonymous PageBacked pages, revalidates publication, and publishes through
  the VM-owned pmap surface. This stages the consistency rule in
  `txdoc:VM-5-1-FAULT-HANDLER`, but it is not yet the async script.
- `VmFaultOutcome::materialize_pagebacked` now covers PrivateAnon and
  PageBacked recipes. PrivateAnon read faults materialize the permanent zero
  frame read-only; PrivateAnon writes allocate fresh private frames. MAP_PRIVATE
  PageBacked reads publish the shared source page read-only, and writes replace
  that mapping with a private frame without inserting it into the
  `PageContainer`.
- `vm::checks` owns value-based observation helpers for fault recipe admission,
  fault-publication revalidation, map admission, and disjoint-remap shape
  checks. These helpers deliberately stop short of final guard-scoped
  `IdentRef` witnesses.
- `vm::project` exposes deterministic read-only address-space rows, including
  stats and mapping projections, without leaking `Cap<PageContainer>` or
  backend identity.
- `PageContainer`, `PageContainerKind`, `PageCacheIndex`, `CachePin`, and
  anonymous materialization exist in the direction of
  `txdoc:PAGE-BACKED-3-PAGECONTAINER`.
- VFS/Mount/PageBacked interface shells define `RNodeBacking`,
  `PageContainerKind::File`, `FsOps`, `FsPageBacking`, device handles, mount
  payloads, and VFS witness names so later filesystem-backed behavior can use
  the canonical names from `txdoc:PAGE-BACKED-2-RNODEBACKING` and
  `txdoc:PAGE-BACKED-6-FSPAGEBACKING`.

## Staged

- The recipe index is snapshot-published, but it is still a cloned
  `BTreeMap<...>` staging structure under a small publication mutex. It is not
  yet the final persistent/epoch range index with guard-scoped lifetime
  evidence.
- `RangeLock` is a bounded v1 reservation set. It proves the declared-range
  discipline and writer/materializer exclusion, but it is not an optimized
  concurrent interval index.
- `vm::checks` returns staged witness values and cloned entries. Final
  guard/lifetime-shaped evidence should wait until Process/ThreadRuntime
  observes `AddressSpace` through the real cap/guard path.
- `VmPmap` owns HAL pmap publication, unmap teardown, ASID shootdown, and a
  VM shadow map for `MapPin` ownership. It does not yet provide the full pmap
  walk/protect/demotion surface needed by fork, CoW, mprotect retagging, or
  mincore.
- `PageCacheIndex::install_if_match` exists only under tests because no
  production truncate, writeback, eviction, or CoW path consumes replacement or
  withdrawal yet.
- `materialize_pagebacked_anon` remains as a compatibility wrapper. The new
  `materialize_pagebacked` path covers PrivateAnon and PageBacked recipes.
  PageContainer now has a uniform `materialize_page` dispatcher for Anon, File,
  and Device variants, but VM fault publication still consumes the older
  synchronous compatibility shape until `fault_script` can carry wait-aware
  `StepOutcome` values.
- MAP_PRIVATE CoW currently proves pmap replacement and private-frame
  ownership, but source frame byte copying is still deferred until Tx exposes a
  VM-safe frame-copy primitive over the direct map.
- The VFS/Mount/PageBacked types are intentionally thin interface shells. They
  preserve names and boundaries, but they do not implement ext4/devfs/bdev-fs
  behavior or real file/device mmap backing.

## Blocked Or Not Implemented

- Final persistent/epoch recipe snapshots are missing. The current
  owned `BTreeMap<...>` snapshot publication closes the partial-rewrite
  visibility gap for readers, but it does not yet provide the final epoch range
  index or guard-scoped witness shape.
- Async `fault_script` retry/yield behavior is missing. Current fault handling
  is a synchronous helper; it does not drop reservations across I/O waits or
  return wait-aware `StepOutcome` values as required by
  `txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE` and
  `txdoc:VM-5-1-FAULT-HANDLER`.
- Full CoW byte-copy fidelity is partially staged. The substrate now exposes a
  direct-map `FrameCopier` hook and `copy_frame_contents(source, dest)`, and
  MAP_PRIVATE PageBacked write faults copy the shared source page into the
  private frame before publishing the writable replacement. PrivateAnon
  zero-frame writes are still satisfied by zeroed fresh frames. Remaining byte
  fidelity work is user-buffer `step_read`/`step_write`, byte-accurate
  partial-page truncate/growth, and concrete backend byte transfer.
- PageBacked range operations are partially staged. `step_read` and
  `step_write` now materialize ranges, advance `OpenFile` offsets, propagate
  wait outcomes, mark written pages dirty, and reject Device writes, but they
  are intentionally copyless until Tx exposes direct-map/user-buffer copying.
  `step_truncate` and `step_fsync` are now staged over fixed page-count
  capacity: truncate asks File backing first and withdraws cached pages at or
  beyond the new boundary, while fsync flushes dirty File pages and then calls
  backing metadata sync. Final dynamic `PC.size`, truncate-up growth,
  byte-accurate partial-page truncate handling, production writeback,
  reclaim integration, and real backend behavior remain future PageBacked-owned
  work under `txdoc:PAGE-BACKED-5-RANGE-OPERATIONS`.
- VM syscall-script surfaces are missing for `fault_script`, `mmap_script`,
  `munmap_script`, `mprotect_script`, `mremap_script`, `brk_script`,
  `madvise`, `msync`, and `mincore`. Existing synchronous methods should remain
  compatibility/core helpers until the scripts can call them.
- `fork_aspace` and `exec_aspace` are missing. They depend on snapshot recipes,
  pmap walk/protect support, CoW demotion, and Process ownership semantics from
  `txdoc:VM-5-6-FORK` and `txdoc:VM-5-7-EXEC`.
- Trap page-fault dispatch is missing. ThreadRuntime still needs the authority
  path that supplies process-owned `Cap<AddressSpace>` evidence and translates
  VM failures into synchronous fault delivery.
- Concrete VFS backend implementations are missing. `FsPageBacking` should be
  proven with mocks before wiring ext4/devfs/bdev-fs behavior.

## Deferred By Design Docs

The following items are not implementation blockers for the next VM-owned
slices because the active docs already defer them or mark them as v1 debt:

- Reverse mapping / rmap, under `txdoc:VM-9-1-NO-RMAP`.
- hugetlb and transparent huge pages, under
  `txdoc:VM-9-2-NO-HUGETLB-NO-TRANSPARENT-HUGE-PAGES` and
  `txdoc:PAGE-BACKED-12-6-LARGE-PAGES`.
- userfaultfd, under `txdoc:VM-9-3-NO-USERFAULTFD`.
- optimized in-place mprotect retagging, under
  `txdoc:VM-9-8-IN-PLACE-PTE-PERMISSION-PATCHING-NOT-ATTEMPTED`.
- advanced RangeLock fairness tuning, under
  `txdoc:VM-9-9-FAIRNESS-MAY-NEED-TUNING-UNDER-PATHOLOGICAL-WORKLOADS`.
- full reclaim policy and writeback scheduling, under
  `txdoc:PAGE-BACKED-8-RECLAIM-UNDER-MEMORY-PRESSURE`,
  `txdoc:PAGE-BACKED-12-1-RECLAIM-POLICY`, and
  `txdoc:PAGE-BACKED-12-2-WRITEBACK-SCHEDULING`.

## Mitigation Order

1. Record this gap ledger and keep the worktree/status records aligned.
2. Close the partial-rewrite recipe visibility gap with a snapshot-published
   recipe index while preserving current map/unmap/protect/remap behavior and
   public helper names.
3. Generalize fault materialization: add `materialize_pagebacked`, implement
   `PrivateAnon` zero-frame reads and private writes, then add MAP_PRIVATE
   page-backed read-only shared installs and CoW replacement writes with
   full-frame source copy.
4. Complete PageBacked v1 core before filesystem backends: keep
   `PageContainer::materialize_page` as the Anon/File/Device dispatch boundary,
   keep copyless `step_read`/`step_write` as staged range-progress scripts, and
   keep `step_truncate`/`step_fsync` as fixed-capacity mock-backed lifecycle
   scripts until dynamic `PC.size` and user-buffer copy helpers exist.
5. Add VM-owned syscall-script surfaces over the synchronous compatibility
   helpers, returning wait-aware outcomes where the docs require retry/yield.
6. Plan runtime integration last: fork after snapshot recipes and CoW demotion,
   exec after Process ownership semantics, and trap page-fault dispatch after
   ThreadRuntime can supply the real authority path.

## Verification For This Ledger

- `cargo fmt --check`
- `cargo xtask progress validate`
- `cargo xtask lint docs`
- `git diff --check`
