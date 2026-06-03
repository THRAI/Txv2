# Research: PageBacked/ext4 readahead policy

**Date:** 2026-05-30

## Question

Linux ext4 and the page cache hide sequential cold-file latency with readahead.
After the `pthread-minimal1` page-fault traces pointed at cold ext4-backed
executable page fetches, where should the equivalent txKernel policy live, and
what does the current implementation actually do?

## Findings

- Linux keeps the generic data readahead policy in the page-cache/MM layer.
  ext4 wires `read_folio` and `readahead` into its address-space operations and
  maps filesystem blocks for a readahead window; it is not the global owner of
  the file data cache policy.
- Tx currently has the same architectural split available, but only the
  single-page demand path is implemented. Exec image LOAD segments are installed
  as recipe-only `VmBacking::Page` entries and demand-fault later through
  `AddressSpace::fault_script_with_ufd_dispatch` ->
  `VmFaultOutcome::materialize_pagebacked_step` ->
  `PageContainer::materialize_page_for_fault_step` ->
  `PageContainer::materialize_file_page` ->
  `FsPageBacking::fetch_page`.
- `PageContainer` is the page-cache owner. `PAGE_BACKED_v1.md` makes the PC
  page index the single linearization point for materialized Frames, and
  persistent filesystem pages are represented as `PageContainerKind::File {
  fs, fs_object_id }` fetched through `FsPageBacking::fetch_page`.
- The live file-backed path fetches exactly one page. `materialize_file_page`
  checks the PC cache for the faulting `PageIndex`, computes one byte offset,
  calls `fetch_page`, and installs only that returned frame. There is no
  neighboring-page prefetch for file-backed faults.
- The one adjacent prefault optimization is private-anonymous write batching;
  it is gated by `VmBacking::PrivateAnon` and does not help executable or file
  data pages.
- VM hint surfaces are currently inert for readahead. `MADV_WILLNEED`,
  `MADV_NORMAL`, `MADV_RANDOM`, and `MADV_SEQUENTIAL` are observation-only
  no-ops, and `sys_readahead` validates the fd then returns success.
- Kernel-facing tx-ext4 has no data-page cache or data readahead. Its
  `Ext4FsInstance::fetch_page` validates alignment, converts `FsObjectId` to
  inode number, calls `Ext4Pager::read_page` for one file page, then copies the
  resulting 4 KiB buffer into a Frame. `Ext4Pager::read_page` reads inode
  metadata, resolves one logical page to one physical block, reads that block,
  and zero-fills holes or EOF tails.
- tx-ext4 does have small namespace/metadata caches: lookup cache, directory
  cache, and directory inode metadata cache. The on-disk format layer loads
  superblock/group descriptors at open, while inode table blocks and extent
  index nodes are still read on demand.
- The host async adapter has a local `BTreeMap` data-page cache, but that is a
  host/test adapter shape and should not be promoted as the production cache
  owner. The active ext4 plan says `PageContainer` is the cache and forbids
  rsext4-style multi-level runtime caching.

## Applicability To txKernel

- Generic file-data readahead should live in PageBacked/PageContainer policy.
  VM may trigger it from mmap/exec/read fault context and pass access hints, but
  VM should continue to own recipe validation and PTE publication only.
- Neighboring pages should become resident in the PC page index, not
  automatically mapped into the faulting address space. Publishing additional
  PTEs is a separate VM prefault policy.
- tx-ext4 should remain the filesystem byte-pager and on-disk format owner. It
  may later expose batch or extent-hint helpers so PageBacked can fill a window
  efficiently, but it should not own the generic sequential-read/data-cache
  policy.
- ext4-local metadata prefetch is a separate legitimate policy lane: inode
  table block windows, extent index child nodes, and htree leaves are
  filesystem-format details and can feed metadata PCs without creating RNodes
  or a second file-data cache.
- A decision record is premature. The current durable conclusion is a research
  finding: demand-only single-page fetch is the current bottleneck candidate,
  and PageBacked is the right policy home if we implement Linux-style data
  readahead.

## Draft Plan

1. Instrument before changing policy. Add subphase counters around
   `Ext4FsInstance::fetch_page` and `Ext4Pager::read_page` to split PC miss,
   inode metadata read, extent/block resolution, block read, and frame copy.
   Re-run the same `pthread-minimal1` 10-iteration page-fault trace and confirm
   the five worst `kind=2 -> done=2` windows.
2. Add a PageBacked-owned file readahead state/API. Keep it keyed by
   `PageContainer` and `PageIndex`; start with a bounded sequential window for
   `PageContainerKind::File` misses. The initial prototype can install adjacent
   pages into the PC only, leaving VM PTE publication unchanged.
3. Keep the first policy conservative: forward-only, small fixed window for
   executable/file-backed sequential misses, no reclaim coupling, and no
   per-inode decoded ext4 cache. Stop at EOF and skip pages already resident in
   the PC.
4. Only after PageBacked policy exists, consider an optional backend helper for
   batch reads or contiguous extent hints. tx-ext4 can use extents to reduce
   repeated block mapping, but PageBacked should still select the window.
5. Wire hints later: `sys_readahead`, `MADV_WILLNEED`, and
   `MADV_SEQUENTIAL` can feed the same PageBacked policy; `MADV_RANDOM` can
   suppress it. Do not special-case these in ext4.
6. Add tests that prove ownership boundaries: sequential file reads/faults
   populate adjacent PC entries through PageBacked; repeated ext4 lookup hits
   lookup/dir caches; metadata prefetch does not create RNodes or ext4-owned
   data-cache entries.
7. Verify with host tests first (`tx-subsystems` PageBacked/VM tests and
   `tx-ext4` pager tests), then with the saved libcbench probe pattern:
   rebuild rv64-qemu, run bounded `pthread-minimal1`, validate observe traces,
   run `fault-decode`, and compare execute-fault totals and worst windows.

## Sources

- `docs/design/03_memory-vm/PAGE_BACKED_v1.md`:
  `PageContainer`, page index linearization, `PageContainerKind::File`, and
  `FsPageBacking::fetch_page`.
- `docs/design/05_filesystem/TX_EXT4_PLAN_v1_2.md`: no rsext4-style
  multi-level cache, `PageContainer` as cache, tx-ext4 stateless per
  persistent object, and file read path through `FsPageBacking::fetch_page`.
- `docs/design/05_filesystem/MOUNT_v1.md`: VM/PageBacked use
  `Cap<MountPayload>` as file backing identity; mount is not the page-cache
  manager.
- `crates/tx-subsystems/src/vm/scripts.rs`,
  `crates/tx-subsystems/src/vm/execution.rs`,
  `crates/tx-subsystems/src/vm/structure/types.rs`,
  `crates/tx-subsystems/src/page_backed/mod.rs`,
  `crates/tx-subsystems/src/page_backed/fs_page_backing.rs`.
- `crates/tx-ext4/src/pager.rs`,
  `crates/tx-ext4/src/read_backend.rs`,
  `crates/tx-ext4/src/host_async.rs`,
  `crates/tx-ext4-format/src/pager.rs`.
- Linux sources checked on 2026-05-30:
  `fs/ext4/inode.c`, `fs/ext4/file.c`, `fs/ext4/readpage.c`,
  `mm/readahead.c`, and `mm/filemap.c`.
