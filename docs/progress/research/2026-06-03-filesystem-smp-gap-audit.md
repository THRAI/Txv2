# 2026-06-03: Filesystem SMP gap audit

## Scope

This note expands the filesystem slice of
`docs/progress/research/2026-06-03-smp-scheduler-gap-report.md`. It is a
code-grounded readiness audit for starting the SMP migration from filesystem
and PageBacked paths.

The audit focuses on implementation performance and implementation drift:

- VFS path resolution, dcache, and open-file dispatch.
- PageBacked `PageContainer` locking, file-page materialization, and user I/O.
- tmpfs, because rootfs and `/dev/shm` are tmpfs today.
- ext4/FAT and block-device bridge paths, because OSComp executes from the
  sdcard ext4 mount under `/musl`.
- bdev-fs block-device page-backed semantics.

The original 2026-06-03 audit was read-only. The status fields below were
updated after the follow-up repair stream through 2026-06-04.

## Executive summary

The filesystem stack is still not ready to be treated as a fully SMP-neutral
substrate for movable userspace tasks, but several P0/P1 gaps from this audit
are now closed or partially closed.

Closed repair items:

1. PageBacked file-backed misses now have per-page in-flight dedup.
2. PageBacked cold anon/file allocation and map-pin work moved out of the
   `PageContainer` metadata lock where the current APIs permit it.
3. bdev-fs block-device RNodes now route through file-backed PageContainers
   instead of anonymous zero-page materialization.
4. VFS positive dcache now covers regular files.
5. ext4/FAT bridges no longer collapse retryable block-device `Yield` /
   `Continue` outcomes into format I/O errors.

Still-open repair items:

1. VFS warm walks still carry strong `Cap` handles instead of guard-scoped
   `IdentRef`.
2. VFS async/resume is improved but not production-complete: `run_walker` now
   preserves `Defer`/`Error`, resume tokens retain the deferred component, and
   `resume_walker` preserves mount-namespace context across yield.
   `resume_walker_after_io` now consumes successful typed completions for
   lookup, inode-meta, readlink, and RNode materialisation.
3. tmpfs still has a mount-wide state lock. It now has lock metrics and
   several bounded lock-service shrinks, but no per-directory/per-inode split.
4. ext4/FAT pager paths are still synchronous and coarse-locked. Bridge retry
   mapping is fixed, but the format `BlockImage` traits still cannot preserve
   wait-source identity through true async block I/O.

## P0/P1 gap ledger

Legend:

- **Done**: the specific drift or performance bug from the audit has a code fix
  plus focused regression coverage.
- **Partial**: a narrow repair landed, but the larger design target remains
  incomplete.
- **Open**: no code fix has landed for that audit item.

| Area | Gap | Status | Current evidence | Remaining risk | Priority |
| --- | --- | --- | --- | --- | --- |
| PageBacked | File-backed cold misses have no per-page in-flight slot | Done | `PageContainerState` now tracks in-flight file fetches; `cargo test -p tx-subsystems file_page_ -- --nocapture` and `cargo test -p tx-subsystems page_backed -- --nocapture` passed. See `docs/progress/STATUS.md` entry "PageBacked file-page misses now join overlapping fetches." | None for duplicate same-page file fetches; future work can tune wait-source costs. | P0 closed |
| PageBacked | Anon allocation and map-pin acquisition happen under the PC lock | Done | Cold anon/file frame allocation and `MapPin` acquisition moved outside the PC state critical section; regressions include `anon_page_materialization_keeps_allocator_work_outside_state_lock` and `file_page_materialization_keeps_map_pin_outside_state_lock`. | Residual `PageCacheIndex` is still one `BTreeMap` per PC, but critical sections no longer include allocator/map-pin service for this gap. | P0 closed |
| PageBacked | `materialized_from_state` acquires map pins while the caller still holds locked state | Done | Hot cached materialization snapshots state under lock, unlocks for `MapPin`, and revalidates before returning. `cargo test -p tx-subsystems materialization_keeps -- --nocapture` passed. | Revalidation adds retry paths; no remaining lock-held map-pin work for this audit item. | P0 closed |
| VFS | Warm path still carries strong `Cap<DEntry>`/`Cap<RNode>` | Open | `WalkingState`, `PathResolution`, and dcache entries still carry strong `Cap` handles. | Refcount and clone/drop traffic remain on hot path; still mismatches `VFS_CHECKS_V2.1` warm-walk `IdentRef` target. | P1 open |
| VFS | Walker async/resume is staged, not production-shaped | Partial | `run_walker` now drives `kernel_step` directly, preserves lookup `Yield` as `WalkState::Defer { request, resume, .. }`, preserves errors as `WalkState::Error`, keeps the deferred component in the resume token, carries mount-namespace context through `resume_walker`, and `resume_walker_after_io` consumes successful typed completions for `DirLookup`, `LoadInodeMeta`, `ReadLink`, and `MaterialiseRnode` without issuing a second backend operation for the completed stage. `cargo test -p tx-subsystems run_walker -- --nocapture`, `cargo test -p tx-subsystems run_walker_resume_preserves_mount_namespace -- --nocapture`, and `cargo test -p tx-subsystems resume_walker_after_ -- --nocapture` passed. | The walker still carries strong `Cap` state rather than guard-scoped `IdentRef`, and true async backends still need scheduler/reactor integration around the typed request/result channel. | P1 partial |
| VFS dcache | Cache is directory-only in practice | Done | `kernel_step` now caches all positive child dentries and the cache hit path no longer filters to directories. `cargo test -p tx-subsystems step_walk_caches_regular_file_positive_lookup -- --nocapture` passed. | Negative cache and broad invalidation policy remain future work. | P1 closed for positive regular-file dcache |
| tmpfs | One mount-wide `SpinMutex<TmpfsState>` serializes namespace operations | Partial | `tx_lock_metrics_fs` and `debug.lock.fs.tmpfs.state` now expose tmpfs lock-service measurements. tmpfs symlink payloads now store `Arc<[u8]>`, so `read_link` clones a shared target handle under the state lock and copies the returned `Box<[u8]>` after unlock. `readdir` now snapshots child id/kind/name/cursor under the state lock and builds `DirEntry` after unlock. `load_inode_meta` now snapshots inode meta/nlink and the optional regular-file `PageContainer` cap under the state lock, then reads `PageContainer::size_bytes()` after unlock. `link` now validates the new name before taking the state lock and increments the target regular file's `nlink` / cached metadata when publishing the alias. Same-directory `rename` now treats an overwritten hard-linked destination as one namespace unlink, decrementing `nlink` and preserving the displaced inode while another alias exists; it returns success without mutating namespace state when old and new names already point at the same inode; it rejects file-directory cross-type replacement before mutating the directory map; and it rejects directory-over-non-empty-directory replacement as `ENOTEMPTY` before mutating the directory map. `cargo test -p tx-fs tmpfs_state_lock_metrics_are_cfg_gated -- --nocapture`, `cargo test -p tx-fs tmpfs_symlink_payload_uses_shared_target_bytes -- --nocapture`, `cargo test -p tx-fs tmpfs_readdir_snapshot_builds_direntry_without_state_borrow -- --nocapture`, `cargo test -p tx-fs tmpfs_inode_meta_snapshot_reads_pagecontainer_size_after_snapshot -- --nocapture`, `cargo test -p tx-fs tmpfs_link_increments_nlink_and_unlink_decrements_one_name -- --nocapture`, `cargo test -p tx-fs tmpfs_rename_over_hard_linked_target_decrements_one_name -- --nocapture`, `cargo test -p tx-fs tmpfs_rename_between_hard_links_to_same_inode_is_noop -- --nocapture`, `cargo test -p tx-fs tmpfs_rename_file_over_directory_returns_eisdir_without_mutating_namespace -- --nocapture`, `cargo test -p tx-fs tmpfs_rename_directory_over_file_returns_enotdir_without_mutating_namespace -- --nocapture`, and `cargo test -p tx-fs tmpfs_rename_directory_over_nonempty_directory_returns_enotempty_without_mutating_namespace -- --nocapture` passed. | The mount-wide lock still exists; per-directory/per-inode split is not implemented and should wait for trace evidence. Regular-file container cap snapshots still clone a strong cap under the state lock. Cross-directory rename is still out of scope. | P1 partial |
| ext4/FAT | Kernel pager paths are synchronous and coarse-locked | Open | ext4 and FAT still wrap pager state in coarse sync cells. | Cold-cache paths can still serialize or spin under filesystem-heavy SMP. | P0 open for filesystem-heavy SMP experiments |
| ext4/FAT bridge | Block-device `Yield` / `Continue` is collapsed to format I/O error | Done | ext4/FAT format layers now expose `WouldBlock`; bridges map retryable `Continue`/`Yield` to `WouldBlock`/`EAGAIN`. `cargo test -p tx-fs ext4_bridge_maps_retrying -- --nocapture` and `cargo test -p tx-fs fat_bridge_maps_retrying -- --nocapture` passed. | The original wait-source identity is still lost because the synchronous `BlockImage` format trait cannot carry it. Full async pager work remains separate. | P0 closed for bridge error mapping; async identity still open under pager item |
| bdev-fs | Block-device RNodes are PageBacked over an `Anon` PC | Done | bdev-fs now creates file-backed PageContainers for block-device nodes and routes reads through `BdevFsMountPayload::fetch_page`. `cargo test -p tx-fs materialised_block_device_rnode_reads_through_bdevfs_page_backing -- --nocapture` passed. | Coherence-index lock service can still be measured later, but the correctness drift is closed. | P0/P1 closed |

## VFS walker and dcache

The active VFS design requires the warm walker to carry `IdentRef` under a
guard and upgrade to `Cap` only when crossing a yield. The implementation does
the reverse for the hot shape: `WalkingState.current`, `mount_root`,
`PathResolution.dentry`, and `PathResolution.rnode` are all strong caps. The
terminal witness builders convert those caps to `IdentRef`, but only after the
walk has already paid cap traffic.

The child cache is still strong-reference, but the positive-cache scope changed
after this audit:

- `DEntry` stores `parent: Option<Cap<DEntry>>`, `rnode: Cap<RNode>`, and
  `children: SpinMutex<BTreeMap<InlineName, Cap<DEntry>>>`.
- `cached_child()` clones a `Cap<DEntry>`.
- `cache_child()` inserts a strong cap.
- As of 2026-06-03, `kernel_step` accepts and inserts positive child dentries
  for regular files as well as directories.

The old directory-only positive-cache drift is closed for regular files. The
remaining dcache questions are negative caching, broad invalidation/reclaim,
and whether symlink positives should be cached with a specific policy.

The walker also has CPU-shape issues that are secondary to the cap mismatch:
component extraction now goes through a shared owned-`Vec` splitter that
collapses separator runs without repeated front `remove(0)` or
`drain(..next_slash)` movement in the normal and typed-resume paths. That
bounded repair reduces visible CPU churn, but it is not the final design: the
walk should eventually become a zero-allocation component cursor tied to the
same guard-scoped `IdentRef` warm-walk migration.

Current remaining recommendation:

1. Instrument path walk first: component count, cache hit/miss, dcache lock
   service, backend lookup/meta/materialize duration, and cap clone/upgrade
   type tags.
2. Defer full `IdentRef` warm-walk migration until PageBacked cold-path P0s are
   addressed or observe data points at VFS cap traffic as the primary tail.
3. Typed I/O-result application now covers all current walker I/O request
   variants. The remaining async work is wiring true async backends and
   migrating warm-walk state to guard-scoped `IdentRef`.

## PageBacked and PageContainer

PageBacked is the highest-value first repair area because it is shared by tmpfs,
shm/memfd-style files, ext4 file-backed exec/faults, and block-device file
views.

Current positive shape:

- File-backed fetch is outside the PC metadata lock. `materialize_file_page`
  checks the cache, calls `FsPageBacking::fetch_page`, then installs under the
  lock.
- File-backed fetches now have same-page in-flight dedup, so racing harts join
  the owner instead of duplicating backend fetch and allocation work.
- Cold anon/file allocation and materialization `MapPin` acquisition happen
  outside the PC metadata lock, with revalidation before returning a mapped
  page.
- User-buffer direct StepOps exist and syscall paths can call
  `step_read_to_user` / `step_write_from_user` for PageBacked files.
- Kernel-buffer helpers exist for VFS-internal and splice-like paths.

Current residual gaps:

- PageCacheIndex is a single `BTreeMap` per `PageContainer`. This is acceptable
  for v1 if critical sections stay tiny, but it is not acceptable if the lock
  encloses allocation, I/O setup, or map-pin acquisition.
- In-flight wait-source cost and wake fanout have not yet been tuned from SMP
  traces.

Completed first implementation slice:

1. `PageContainerState` gained internal per-page in-flight file-fetch state.
2. On file miss, the code installs or joins `InFlight` under the PC lock, then drops the
   lock before backend `fetch_page`.
3. On fetch completion, the owner publishes resident state and wakes joiners;
   yield/error/truncate clear stale in-flight state.
4. For anon pages, code reserves/allocates outside the PC lock after a miss
   reservation, then install under lock. If another hart wins, drop/release the
   speculative frame and return the resident page.
5. Hot paths snapshot `ppn`/marks under lock, acquire `MapPin` after lock
   release, and revalidate before returning.

## tmpfs

tmpfs is more important than its size suggests:

- Boot rootfs is tmpfs.
- `/dev/shm` is tmpfs.
- POSIX shm/named-sem and libc scratch workloads touch tmpfs early.
- Writable root/tmp paths use tmpfs even when executable code comes from ext4
  under `/musl`.

The current implementation has one `SpinMutex<TmpfsState>` over
`BTreeMap<FsObjectId, TmpfsInode>`, and directory payloads are nested
`BTreeMap<InlineName, FsObjectId>`. Most FsOps methods are simple and bounded,
but the mount-wide lock serializes unrelated directories and inodes. A few
methods clone or allocate while close to the lock boundary:

- `create_inode` allocates a `PageContainer` before taking the tmpfs lock,
  which is good.
- `materialise_rnode` clones the file's `Cap<PageContainer>` under the tmpfs
  lock, drops the lock, then signs the RNode, which is mostly good but still
  pays cap traffic under the namespace lock.
- `fetch_page` snapshots the container cap under lock, drops the lock, then
  calls PageBacked, which is the right direction.
- `read_link` now snapshots an `Arc<[u8]>` symlink target under the tmpfs
  lock, drops the lock, then copies the target bytes into the caller-owned
  `Box<[u8]>`, avoiding a target-length copy while serving
  `debug.lock.fs.tmpfs.state`.
- `readdir` now snapshots the child id, child kind, inline name, and next
  cursor under the tmpfs lock, drops the lock, then constructs the returned
  `DirEntry`, avoiding name validation/copy work while serving
  `debug.lock.fs.tmpfs.state`.
- `load_inode_meta` now snapshots inode metadata, link count, and the optional
  regular-file `PageContainer` cap under the tmpfs lock, drops the lock, then
  reads `PageContainer::size_bytes()` so visible file size does not require a
  PageContainer query while serving `debug.lock.fs.tmpfs.state`.
- `link` now validates the new directory name before taking the tmpfs lock and
  increments the target regular file's `nlink` / cached metadata when
  publishing the alias, closing the drift where `load_inode_meta` still
  reported one link after a successful hard link.
- Same-directory `rename` now treats an overwritten destination as a single
  namespace unlink for the displaced inode: if another hard-link alias exists,
  tmpfs decrements `nlink` and preserves the inode table entry instead of
  leaving the surviving alias pointing at `ENOENT`.
- Same-directory `rename` also returns success without mutation when old and
  new names already point at the same inode, preserving both names and the
  visible link count for hard-link aliases.
- Same-directory `rename` now rejects regular-file-over-directory as `EISDIR`
  and directory-over-regular-file as `ENOTDIR` during read-only validation, so
  failed cross-type replacement leaves both directory entries unchanged.
- Same-directory `rename` now rejects directory-over-non-empty-directory
  replacement as `ENOTEMPTY` during read-only validation, so a failed
  replacement preserves the source directory, target directory, and target
  children.

Current status and recommendation:

1. Do not start by replacing tmpfs with a full per-inode lock/index redesign.
2. tmpfs lock-service metrics are now available through `tx_lock_metrics_fs`
   as `debug.lock.fs.tmpfs.state`.
3. Symlink readlink, directory entry construction, regular-file visible size
   reads, and hard-link name validation no longer deep-copy, validate, or query
   returned data under the state lock; keep looking for similar bounded
   lock-held clone/copy work before taking on a full state split.
4. After PageBacked PC fixes, split tmpfs state into at least an inode table
   index plus per-directory/per-inode payload locks if traces show namespace
   lock service, especially under `/dev/shm` and tmpfile-heavy workloads.

## ext4 and FAT

The active ext4 plan says kernel-facing I/O should be coroutine-compatible:
every I/O call should yield and resume. Current production kernel paths are not
there yet.

ext4:

- `Ext4FsInstance` has a single `Ext4PagerCell` protecting the whole pager.
- `FsPageBacking::fetch_page` reads through `with_pager`, then materializes a
  frame, and returns only `Done` or `Err`.
- The block bridge adapts the kernel `BlockDevice` to the format crate's
  synchronous `BlockImage`; retryable block-device outcomes now map to
  format-level `WouldBlock`, but original wait-source identity is not
  preserved through the format trait.

FAT:

- The same single-pager-cell pattern exists.
- `fetch_page` walks the FAT chain and allocates per-cluster buffers inside the
  pager closure.
- Its block bridge now maps retryable block-device outcomes to `WouldBlock`,
  but the pager path remains synchronous and coarse-locked.

Current status and recommendation:

1. Treat current ext4/FAT as bring-up and functional-compatibility backends for
   SMP work, not as the acceptance target for filesystem-heavy SMP performance.
2. Instrument before refactoring: split backend fetch time into lookup/meta,
   block mapping, block read, frame allocation, and frame copy.
3. Keep generic file-data readahead in PageBacked. ext4 can later expose batch
   read or extent hints, but PageBacked should choose the file-data window and
   install adjacent pages into the PC.
4. Plan the async backend conversion as a separate slice: bridge retry mapping
   is fixed, but the current `BlockImage` format trait shape still cannot
   preserve `Yield` wait-source identity.

## bdev-fs

bdev-fs had a focused correctness drift in the original audit. That drift is
now closed, but bdev-fs can still be measured for coherence-index lock service
before it becomes a hot SMP filesystem performance path.

The design says bdev-fs maps registered block devices to PageBacked RNodes and
its `FsPageBacking` translates page cache misses to block-device I/O. The code
implements `FsPageBacking::fetch_page` and `flush_page`, and now
`materialise_rnode` creates a `PageContainerKind::File` PC so ordinary
PageBacked read/materialization routes through `BdevFsMountPayload::fetch_page`
instead of zero-filled anonymous materialization.

Remaining recommendation:

1. Preserve bdev-fs's coherence index, but avoid holding the coherence lock
   across PageContainer allocation or weak upgrade if lock-service data shows
   it matters.
2. Add bdev-fs-specific lock-service metrics only if OSComp traces show
   block-device file views on the hot path.

## OSComp boot path impact

The current boot path is mixed:

- `mount_rootfs_from_boot_media()` mounts tmpfs as rootfs.
- `/dev/shm` is mounted as tmpfs.
- bdev-fs is mounted at `/dev/block`.
- sdcard ext4 is mounted at `/musl`.
- OSComp sdcard boot executes `/musl/musl/busybox` and runs commands from
  `/musl/musl`, while rootfs shims and writable scratch paths still hit tmpfs.

Therefore, the first filesystem SMP fixes should not be ext4-only. A realistic
OSComp run can combine ext4 executable/file faults, tmpfs scratch/shm, VFS path
walks, and PageBacked user-buffer reads/writes. PageBacked lock and in-flight
behavior is the common denominator.

## Proposed repair order

1. **PageBacked in-flight and lock shrink**: done for the audit's P0 lock and
   dedup gaps.
2. **bdev-fs correctness proof**: done; block-device RNodes now use
   file-backed PageContainers and route through bdev-fs page backing.
3. **VFS dcache policy clarification**: partially done; positive regular-file
   dcache is fixed, while negative cache, symlink cache, and invalidation
   policy remain future work.
4. **tmpfs lock metrics and split**: partially done; mount-wide lock metrics
   exist, symlink readlink no longer copies target bytes under the state lock,
   readdir builds `DirEntry` after unlock, and `load_inode_meta` reads
   PageContainer size after unlock. `link` also now maintains nlink correctly
   and validates the new name before taking the state lock. Same-directory
   `rename` now preserves displaced hard-linked inodes by decrementing nlink,
   treats rename-between-aliases of the same inode as a no-op, and rejects
   file-directory cross-type replacement plus non-empty target-directory
   replacement before mutation. Cross-directory rename and state splitting
   remain trace-gated.
5. **VFS async/resume repair**: partially done; `run_walker` preserves Defer
   and Error state, resume tokens retain the deferred component, and
   namespace-carrying resume is covered. Typed I/O-result resume now covers
   successful lookup, inode-meta, readlink, and RNode-materialise completions.
6. **VFS component-parser CPU-shape repair**: done for the bounded issue;
   normal and typed-resume component extraction now use a shared splitter
   without repeated front byte movement. A true zero-allocation cursor remains
   part of the broader warm-walk migration rather than this repair slice.
7. **VFS warm-walk migration**:
   convert the walker to guard-scoped `IdentRef` and a real `ResumeToken`
   protocol when path-walk traffic is proven hot or async backends need it.
8. **ext4/FAT async pager**:
   finish true async block-device I/O and remove single-pager lock
   serialization as a dedicated backend project. Bridge retry/error mapping is
   already fixed.

## Verification

The original audit was read-only. The 2026-06-04 updates converted it into a
status ledger and incorporate the verification evidence recorded in
`docs/progress/STATUS.md` for the completed repair slices. The VFS
async/resume row now also reflects the namespace-preserving resume regression
added in the current repair pass, the VFS section records the bounded
component-parser CPU-shape repair, and the tmpfs row records the symlink
readlink, readdir, `load_inode_meta`, hard-link, and same-directory rename
lock-held-work/semantic shrinks, including cross-type and non-empty-directory
replacement rejection.
