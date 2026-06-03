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

No implementation changes were made in this pass.

## Executive summary

The filesystem stack is not ready to be treated as an SMP-neutral substrate for
movable userspace tasks. The most urgent issues are not ordinary queue
contention; they are long or duplicated service in cold paths and several
places where the code shape still differs from the active design contracts.

The first repair slice should be PageBacked-local:

1. Add per-page in-flight wait/dedup state for file-backed page misses.
2. Move anonymous page allocation and map-pin acquisition out of the
   `PageContainer` metadata lock where feasible.
3. Add PageBacked and VFS lock-service / phase observe rows before larger
   walker or backend refactors.

The VFS `IdentRef` warm-walk migration and ext4 async pager conversion are
larger design-alignment projects. They should follow the PageBacked lock/dedup
slice unless an observe run directly points at path-walk refcount traffic or
ext4 synchronous I/O as the dominant tail.

## P0/P1 gap table

| Area | Gap | Evidence | Risk | Priority |
| --- | --- | --- | --- | --- |
| PageBacked | File-backed cold misses have no per-page in-flight slot | `materialize_file_page` checks cached state, calls backend `fetch_page`, then installs with `install_if_absent`; a racing hart can do the same work and lose at install (`crates/tx-subsystems/src/page_backed/mod.rs:603`, `:626`, `:650`) | Duplicate I/O/allocation and tail variance under SMP cold faults | P0 |
| PageBacked | Anon allocation and map-pin acquisition happen under the PC lock | `materialize_anon` holds `state` while calling `allocate_cached_frame`, `install_if_absent`, `mark_dirty`, and `acquire_map_pin` (`crates/tx-subsystems/src/page_backed/mod.rs:402`, `:412`, `:416`, `:430`) | Lock service grows with allocator and frame metadata cost; tmpfs writes and shm can amplify this | P0 |
| PageBacked | `materialized_from_state` acquires map pins while the caller still holds locked state | Called from cached/file/device paths while borrowing `PageContainerState`; it calls `page_allocator::acquire_map_pin` (`crates/tx-subsystems/src/page_backed/mod.rs:675`, `:713`, `:733`, `:972`) | PC metadata lock can include page-substrate service | P0 |
| VFS | Warm path still carries strong `Cap<DEntry>`/`Cap<RNode>` | `WalkingState` and `PathResolution` store `Cap`; `DEntry` child cache stores strong child caps (`crates/tx-subsystems/src/vfs/resolution/state.rs:70`, `:104`; `crates/tx-subsystems/src/vfs/structure.rs:749`) | Refcount and clone/drop traffic on every component; mismatches `VFS_CHECKS_V2.1` warm-walk contract | P1 |
| VFS | Walker async/resume is staged, not production-shaped | `walk_to_completion` maps `NeedIO` to `EAGAIN`; `run_walker` turns any error into an empty `WalkingState`; `resume_walker` has no IO result and drops namespace context (`crates/tx-subsystems/src/vfs/resolution/driver.rs:163`, `:172`, `:198`) | True yielding backends cannot be plugged through the current walker without semantic loss | P1 |
| VFS dcache | Cache is directory-only in practice | Cache hit filters cached child to `Directory`; insertion only occurs when `child_meta.kind() == Directory` (`crates/tx-subsystems/src/vfs/resolution/step.rs:155`, `:269`) | Regular file and symlink lookups repeatedly hit backend; if intentional, policy needs to be explicit | P1 |
| tmpfs | One mount-wide `SpinMutex<TmpfsState>` serializes namespace operations | `Tmpfs` owns `state: SpinMutex<TmpfsState>` over the inode table; `lookup`, `create_inode`, `unlink`, `rename`, `mkdir`, `readdir`, chmod/chown all lock it (`crates/tx-fs/src/tmpfs/mod.rs:101`, `:126`, `:212`, `:293`, `:370`, `:422`, `:650`) | Rootfs, `/tmp`, and `/dev/shm` workloads serialize on one lock | P1 |
| ext4/FAT | Kernel pager paths are synchronous and coarse-locked | ext4 and FAT wrap the full pager in spin cells (`crates/tx-ext4/src/read_backend.rs:426`; `crates/tx-fat/src/read_backend.rs:152`) | Cold-cache path can spin while another hart traverses on-disk metadata or does block I/O | P0 for filesystem-heavy SMP experiments |
| ext4/FAT bridge | Block-device `Yield` / `Continue` is collapsed to format I/O error | ext4 bridge maps any non-`Done` block read/write to `Truncated`; FAT bridge maps non-`Done` to `IO` (`crates/tx-fs/src/tx_ext4_bridge.rs:64`, `:70`; `crates/tx-fs/src/fat_bridge.rs:57`, `:64`) | Design says I/O should yield; current shape cannot model async block devices correctly | P0 for async backend enablement |
| bdev-fs | Block-device RNodes are PageBacked over an `Anon` PC | `get_or_create_pc` creates `PageContainerKind::Anon`; `materialise_rnode` exposes it as `RNodeBacking::PageBacked` (`crates/tx-fs/src/bdevfs/mod.rs:167`, `:188`, `:324`, `:351`) | Needs confirmation: ordinary PageBacked reads may hit zero-filled anon PC instead of device fetch | P0/P1 correctness audit |

## VFS walker and dcache

The active VFS design requires the warm walker to carry `IdentRef` under a
guard and upgrade to `Cap` only when crossing a yield. The implementation does
the reverse for the hot shape: `WalkingState.current`, `mount_root`,
`PathResolution.dentry`, and `PathResolution.rnode` are all strong caps. The
terminal witness builders convert those caps to `IdentRef`, but only after the
walk has already paid cap traffic.

The current child cache is also strong-reference and directory-scoped:

- `DEntry` stores `parent: Option<Cap<DEntry>>`, `rnode: Cap<RNode>`, and
  `children: SpinMutex<BTreeMap<InlineName, Cap<DEntry>>>`.
- `cached_child()` clones a `Cap<DEntry>`.
- `cache_child()` inserts a strong cap.
- `kernel_step` accepts only cached directory children and inserts only
  directory children.

That directory-only cache may be an intentional day-1 policy, but it should be
documented. If not intentional, it produces extra backend lookup and
materialization traffic for regular files and symlinks, including common
`execve`, `open`, and shebang paths.

The walker also has CPU-shape issues that are secondary to the cap mismatch:
component extraction uses `Vec::drain(..next_slash)` and `remove(0)` on the
remaining path buffer, which can repeatedly move bytes on long paths. That is
not the first SMP blocker, but it is visible work on a path that should
eventually be a zero-allocation component cursor.

Recommendation:

1. Instrument path walk first: component count, cache hit/miss, dcache lock
   service, backend lookup/meta/materialize duration, and cap clone/upgrade
   type tags.
2. Decide whether directory-only dcache is a deliberate v1 policy.
3. Defer full `IdentRef` warm-walk migration until PageBacked cold-path P0s are
   addressed or observe data points at VFS cap traffic as the primary tail.

## PageBacked and PageContainer

PageBacked is the highest-value first repair area because it is shared by tmpfs,
shm/memfd-style files, ext4 file-backed exec/faults, and block-device file
views.

Current positive shape:

- File-backed fetch is outside the PC metadata lock. `materialize_file_page`
  checks the cache, calls `FsPageBacking::fetch_page`, then installs under the
  lock.
- User-buffer direct StepOps exist and syscall paths can call
  `step_read_to_user` / `step_write_from_user` for PageBacked files.
- Kernel-buffer helpers exist for VFS-internal and splice-like paths.

Current gaps:

- There is no per-page in-flight slot. Two harts missing the same file-backed
  page can both issue backend fetches and allocate frames; `install_if_absent`
  only chooses a winner after the duplicated work.
- `materialize_anon` allocates a frame and acquires a map pin while holding the
  PC lock.
- `materialized_from_state` acquires map pins while callers still hold the PC
  state lock.
- PageCacheIndex is a single `BTreeMap` per `PageContainer`. This is acceptable
  for v1 if critical sections stay tiny, but it is not acceptable if the lock
  encloses allocation, I/O setup, or map-pin acquisition.

Recommended first implementation slice:

1. Extend `PageContainerState` with a per-page entry state, for example
   `Resident(PageCacheEntry)` plus `InFlight(wait_source/owner/generation)`.
   Keep this internal to PageBacked; do not change filesystem backends first.
2. On file miss, install or join `InFlight` under the PC lock, then drop the
   lock before backend `fetch_page`.
3. On fetch completion, convert frame to cache entry and publish resident under
   the PC lock, then notify waiters outside or with bounded source-lock service.
4. For anon pages, reserve/allocate outside the PC lock after a miss
   reservation, then install under lock. If another hart wins, drop/release the
   speculative frame and return the resident page.
5. Snapshot `ppn`/marks under lock and acquire `MapPin` after lock release
   where the page-substrate API permits it; otherwise add a short comment and
   lock-service metrics proving the residual cost.

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

Recommendation:

1. Do not start by replacing tmpfs with a full per-inode lock/index redesign.
2. Add tmpfs lock-service metrics around FsOps operations and classify by op.
3. After PageBacked PC fixes, split tmpfs state into at least an inode table
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
  synchronous `BlockImage`; any non-`Done` block-device outcome becomes a
  format error.

FAT:

- The same single-pager-cell pattern exists.
- `fetch_page` walks the FAT chain and allocates per-cluster buffers inside the
  pager closure.
- Its block bridge also collapses non-`Done` block-device outcomes to I/O
  errors.

Recommendation:

1. Treat current ext4/FAT as bring-up and functional-compatibility backends for
   SMP work, not as the acceptance target for filesystem-heavy SMP performance.
2. Instrument before refactoring: split backend fetch time into lookup/meta,
   block mapping, block read, frame allocation, and frame copy.
3. Keep generic file-data readahead in PageBacked. ext4 can later expose batch
   read or extent hints, but PageBacked should choose the file-data window and
   install adjacent pages into the PC.
4. Plan the async backend conversion as a separate slice: the current
   `BlockImage` format trait shape cannot preserve `Yield`.

## bdev-fs

bdev-fs deserves a focused correctness audit before it becomes part of an SMP
filesystem performance path.

The design says bdev-fs maps registered block devices to PageBacked RNodes and
its `FsPageBacking` translates page cache misses to block-device I/O. The code
does implement `FsPageBacking::fetch_page` and `flush_page`, but
`materialise_rnode` currently creates a `PageContainerKind::Anon` PC and
publishes it as `RNodeBacking::PageBacked`.

That is suspicious because PageBacked dispatches backend `fetch_page` only for
`PageContainerKind::File`; an `Anon` PC materializes zeroed anonymous pages
locally. If no separate path swaps in bdev-fs `FsPageBacking`, then ordinary
read/write/mmap on `/dev/block/<device>` may not hit the device at all.

Recommendation:

1. Add a focused host test that opens a bdev-fs block device RNode and proves
   `OpenFile::step_read` or PageBacked materialization calls
   `BdevFsMountPayload::fetch_page`.
2. If the test confirms the drift, change bdev-fs PCs to the File kind with a
   mount/payload pin or add a device-specific PageContainer kind that routes to
   block-device fetch/flush explicitly.
3. Preserve bdev-fs's coherence index, but avoid holding the coherence lock
   across PageContainer allocation or weak upgrade if lock-service data shows
   it matters.

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

1. **PageBacked in-flight and lock shrink**:
   per-page in-flight state, allocation outside PC lock, map-pin outside PC
   lock if possible, and PageBacked lock-service rows.
2. **bdev-fs correctness proof**:
   verify whether block-device RNodes actually route through bdev-fs
   `FsPageBacking`; fix PC kind/routing if not.
3. **VFS dcache policy clarification**:
   decide directory-only dcache; add metrics and, if desired, cache regular
   file/symlink positives with an explicit invalidation/reclaim story.
4. **tmpfs lock metrics and split**:
   measure mount-wide tmpfs lock service, then split only if it appears in
   OSComp traces.
5. **VFS warm-walk migration**:
   convert the walker to guard-scoped `IdentRef` and a real `ResumeToken`
   protocol when path-walk traffic is proven hot or async backends need it.
6. **ext4/FAT async pager**:
   preserve `Yield` through block-device I/O and remove single-pager lock
   serialization as a dedicated backend project.

## Verification

This was a read-only code audit plus progress-note update. No Rust code was
changed in this pass.
