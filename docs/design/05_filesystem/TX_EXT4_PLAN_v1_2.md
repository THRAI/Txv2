# tx-ext4: Project Plan

<!-- txdoc:05-FILESYSTEM-TX-EXT4-PLAN-V1-2 -->

**Status.** v1.2, Linux-compatibility profile revision (2026-07-25). Active
plan for the ext4 filesystem backend for txKernel.

**2026-07-25 direction.** The production design remains a Tx-native kernel
filesystem instance. A userspace daemon or a separate in-kernel ext4 service is
not the primary implementation path: neither removes the on-disk and durability
work, and both add a request transport, page-transfer, failure and bootstrap
protocol. Linux ext4, e2fsprogs and xfstests are the compatibility authorities;
rsext4 is an algorithm source only. Delivery is split into a bounded Tier 1
profile followed by Tier 2 mainstream-Linux compatibility.

**Supersedes (v1.2 → v1.1).** Adds two design commitments to §1: (a) the stateless-per-inode rule — tx-ext4 holds no decoded per-inode state; POSIX-abstract metadata lives on VFS's RNode, ext4-specific fields are addressed as bytes in the inode-table PC and re-parsed on use; (b) a new §1.4 cache and reclaim policy that locks in v1 behavior (CLOCK reclaim for file pages phase 2, pinned metadata PCs, dentry-cache Tier 2 reclaim, periodic writeback phase 4, no swap), closing `PAGE_BACKED_v1.md §12.1` for v1 scope.

**Supersedes (v1.1 → v1).** Adds §3 "Interfaces" explicitly specifying the traits tx-ext4 implements (`FsPageBacking`, `FsOps`), the trait it consumes (`BlockDevice`), the value types it handles (`InodeMeta`, `DirCursor`, `DirEntry`, `Credential`, `FsObjectId`), the mount handshake (`MountInitContext` + `MetadataPcFactory` + `MountOutput`), and an explicit import allowlist/denylist enforceable by grep-lint. Downstream sections renumbered (§4+). Content of final goals, phase breakdown, and test milestones unchanged.

**Purpose.** Define the compatibility contract, ownership boundaries,
deliverables, phase structure and executable acceptance gates for an async,
coroutine-compatible ext4 filesystem behind VFS, PageBacked and the I/O
manager. The implementation follows Linux-visible semantics and the ext4
on-disk contract without copying Linux VFS, page-cache or locking internals.
rsext4 ([Starry-OS/rsext4](https://github.com/Starry-OS/rsext4)) is included as
a git submodule for algorithm study; no rsext4 code is used at runtime.

**Audience.** Implementers working on tx-ext4, reviewers auditing the backend boundary, agents extending the filesystem in future phases.

**Companion documents.**

- [`EXT4_LIFECYCLE_v1.md`](EXT4_LIFECYCLE_v1.md) — canonical Tier 1 ownership and terminal-state contract for file-I/O requests, admitted mutations, fsync/sync/unmount settlement, production-path convergence, and crash/e2fsprogs acceptance. Where older phase prose implies caller-managed cleanup, commit-time journal release, multiple active Tier 1 transactions, accumulated uncheckpointed transactions, or concurrent checkpoint publication, this companion's serialized Tier 1 lifecycle wins. Phase-4 T4.5/T4.6 below use the revised serialized form.
- [`MEMORY_IO_ARCHITECTURE_v1.md`](../03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md) — canonical dual-plane file-I/O and global memory-pressure architecture; it supersedes this plan's older global `FrameMeta` CLOCK and dirty-authority prose.
- [`VFS_CHECKS_V2.1.md`](VFS_CHECKS_V2.1.md) and [`MOUNT_v1.md`](MOUNT_v1.md) — VFS ownership boundary, `FsOps` and `FsPageBacking` consumer side.
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) — `FsPageBacking` trait, `PageContainer` model.
- [`VM_v1_2.md`](../03_memory-vm/VM_v1_2.md) — fault handler and `FsPageBacking::fetch_page` integration.
- [`IO_MANAGER_v1.md`](IO_MANAGER_v1.md) — target successor path for file-data I/O: ext4 remains the concrete mapping/journal backend and produces neutral page/block plans while the I/O manager owns batching, submission, completion, and block scheduling.
- [`01_CONCEPTS_v5.md §3.5`](../../Txv3/01_CONCEPTS_v5.md) — factoring/topology axes used throughout this plan.
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) — `StepOutcome` contract; all async methods return step outcomes.
- [Linux ext4 documentation](https://www.kernel.org/doc/html/latest/filesystems/ext4/index.html) — on-disk structures, feature flags, allocation, directories and JBD2 behavior.
- [Linux `fs/ext4`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/ext4) — observable behavior and error-path oracle where prose is incomplete.
- [e2fsprogs](https://git.kernel.org/pub/scm/fs/ext2/e2fsprogs.git) — image construction, inspection and consistency authority (`mke2fs`, `debugfs`, `dumpe2fs`, `e2fsck`).
- [xfstests](https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git) — Linux filesystem behavior and crash-regression oracle, filtered by declared feature support.

### Zone-derived type policy

<!-- txdoc:TX-EXT4-PLAN-ZONE-DERIVED-TYPE-POLICY-1 -->

tx-ext4 is a filesystem backend hosted by Mount and PageBacked. It owns
backend state, not new user-visible identity entities:

| tx-ext4 declaration | Public handle | Reclamation role |
|---|---|---|
| mounted ext4 instance | `MountPayload` evidence supplied by MOUNT | filesystem-instance payload |
| file content cache | `Cap<PageContainer>` held by PageBacked/VFS structures | page-backed content entity |
| metadata PCs | `Cap<PageContainer>` held on the mount payload | private page-backed metadata cache |
| ext4 inode/extent/dir records | values parsed from metadata PCs | binding/materialization values, no zone |
| block device | `&'static BlockDeviceRegistration` or bdev-fs-provided backend handle | static/device-backed fact |

tx-ext4 must not introduce `Cap<RNode>` caches or raw `Zone<T, Policy>`
choices. Its persistent object identity is expressed as `FsObjectId` values
resolved through VFS/PageBacked.

---

## 1. Scope and non-goals

<!-- txdoc:TX-EXT4-PLAN-SCOPE-NON-GOALS-1 -->

### 1.1 In scope

<!-- txdoc:TX-EXT4-PLAN-IN-SCOPE-1 -->

- A tx-ext4 crate implementing `FsOps` and `FsPageBacking`.
- An async block device trait that tx-ext4 consumes for all disk I/O.
- A declared Tier 1 ext4 profile for production bring-up and a Tier 2 profile
  for mainstream Linux ext4 behavior, as specified in §1.5-§1.8.
- Linux-visible regular-file, directory, symlink, hard-link, extent, allocation,
  metadata, fsync and recovery semantics within the active tier.
- JBD2 ordered-mode journaling with mount-time replay.
- Filesystem-owned metadata kept in `MountPayload`-held `Cap<PageContainer>` handles (option A from the tx-ext4 plan discussion): **metadata PCs are not exposed as RNodes**; they are filesystem-private buffered block ranges.

### 1.2 Out of scope (for v1)

<!-- txdoc:TX-EXT4-PLAN-OUT-SCOPE-V1-1 -->

- Reflink (`copy_file_range` with sharing) — ext4 does not support it.
- Quotas.
- Encryption, verity, fscrypt.
- ext2/ext3 back-compat modes — assume ext4 with extents, 4 KiB blocks, 64-bit features where used.
- Block sizes other than 4 KiB.
- Online resize.
- Writeback mode or journal=data — only ordered mode.
- Tier 3 features listed in §1.9. POSIX ACLs and common xattrs move to Tier 2;
  they are no longer permanently excluded by this plan.

### 1.3 Non-negotiable design constraints

<!-- txdoc:TX-EXT4-PLAN-NON-NEGOTIABLE-DESIGN-CONSTRAINTS-1 -->

- **No runtime dependence on rsext4.** rsext4 is a reference and the on-disk format is ported from it; the runtime library is ours.
- **No ext4 daemon or private ext4 service boundary in the production path.**
  The mounted filesystem instance is the L5 mapping/journal planner consumed by
  PageBacked and the I/O manager. Long-lived L4/L6 service futures remain
  generic I/O-manager mechanism, not a second filesystem server.
- **Linux is the behavioral oracle, not the internal template.** Error codes,
  persistence rules, feature admission and filesystem-visible results follow
  Linux. Tx retains its own VFS, PageBacked, StepOp, publication and scheduling
  structure.
- **No rsext4-style multi-level cache.** `PageContainer` is the cache.
- **No synchronous blocking.** Every I/O call yields a `StepOutcome::Yield { shape: YieldShape::OnWaitSource { .. } }` and resumes when the block device completes. The block device trait is async.
- **No `&mut self` threading.** Concurrent operations on the same `Ext4FsInstance` must be admissible. State mutation goes through PC-level publication discipline (ARCH-5) and substrate primitives.
- **No `Cap<RNode>` held inside tx-ext4.** All operations key on `fs_object_id`.
- **tx-ext4 is stateless per persistent object.** All per-inode state lives in one of two places: (a) POSIX-abstract decoded metadata (`InodeMeta`) on VFS's RNode, serialized to/from on-disk records by tx-ext4; (b) ext4-specific fields (extent tree root, htree info, flags beyond POSIX) addressed as *bytes* in the inode-table PC, re-parsed on each use. **tx-ext4 does not maintain a per-inode decoded cache of ext4-specific fields.** The only persistent state tx-ext4 holds is mount-level: block device handle, superblock mirror, journal state, metadata PC handles. This is stronger than "no Cap<RNode>" — it closes off a second decoded-cache coherence domain. If profiling later shows per-inode re-parse is a measurable cost, a bounded decoded-extent-root cache keyed by `(fs_object_id, modification_counter)` may be added as a phase-5 optimization; the invalidation key ensures coherence across rematerialization.

### 1.4 Cache and reclaim policy

<!-- txdoc:TX-EXT4-PLAN-CACHE-RECLAIM-POLICY-1 -->

This section classifies ext4/VFS caches and their owner-side reclaim
eligibility. Global policy, watermarks, provider arbitration, allocation retry,
and file-page replacement algorithms are owned by
[`MEMORY_IO_ARCHITECTURE_v1.md`](../03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md).

**Caches in scope.**

| Cache | What it holds | Lifecycle |
|---|---|---|
| File-page PCs | Clean and dirty file-content frames per `PageContainerKind::File` | Bounded by memory pressure |
| Metadata PCs (tx-ext4) | Inode table, GDT, bitmaps, journal blocks | Pinned for mount lifetime in v1 |
| Anon PCs | Anonymous memory, tmpfs, shm | Pinned until explicit teardown (no swap) |
| Dentry cache | First-class DEntry edge objects | Bounded by memory pressure |
| RNode zone | Live-node slots | Dies when no edges and no payload-capable holders |
| Coherence index | `fs_object_id → Weak<RNode>` per mount | Weak references; entries die with their RNodes |
| Slab (kernel heap) | Kernel allocations backing Box/Vec/etc | Simple free-on-empty; no per-CPU cache in v1 |
| Journal transactions | Admitted metadata after-images, revoke records, commit/checkpoint state | Bounded by journal size; checkpoint plus safe tail advancement releases the journal extent |

**Provider policy by phase.**

- **Phase 1.** File-page and ext4 metadata state may remain pinned while the
  read-only bring-up profile is bounded. Allocation exhaustion fails cleanly.
- **Phase 2.** PageBacked registers clean file pages through its global
  `ReclaimProvider`; ext4 does not sweep `FrameMeta` or own the CLOCK hand. VFS
  registers dentry/RNode caches through owner adapters.
- **Phase 4.** Dirty ordinary file pages remain PageSlot-owned and enter the
  global writeback control loop. ext4 supplies layout, immutable metadata
  after-images, journal admission and ordering. Dirty metadata is never evicted
  as an ordinary clean file page.
- **Later measured stage.** ext4 may register bounded metadata caches only once
  `FrozenMetadataLease`, transaction-safe claim rules, and refault/cost
  accounting are executable.

**Explicit non-policies (v1).**

- **No swap.** Anon PCs are pinned until explicit teardown. This has been a standing commitment (`PAGE_SUBSTRATE_v1.md` §8).
- **No metadata PC eviction in the first control-plane stage.** Revisit only
  through an ext4-owned provider after transaction-safe claim semantics land.
- **No per-CPU slab caches.** Slab returns frames to the frame allocator when a slab is fully free; no high-water mark.
- **No reverse-mapping machinery in v1.** Owners validate typed mapping/pin
  facts at claim time; policy does not walk raw frames back into semantic
  objects.
- **No ext4-private global reclaim priority.** Provider arbitration belongs to
  the memory-pressure coordinator.
- **No dentry-cache periodic pruning.** Reclaim is on-demand at high-water only.

**Cross-reference and closure.**

The global contract now lives in `MEMORY_IO_ARCHITECTURE_v1.md`. This plan
retains only ext4 cache classification and transaction-safe owner behavior.

**What this buys us.**

- Memory pressure produces graceful degradation instead of ENOMEM under any reasonable load.
- Replacement algorithms can evolve without importing ext4 semantics into the
  allocator or policy layer.
- Background and direct work remain bounded, with actual allocator-free and
  refault feedback.
- Matches the no-swap discipline: memory pressure affects only reclaimable pages (clean file, evictable dentry), never anon.

### 1.5 Compatibility authority

<!-- txdoc:TX-EXT4-PLAN-COMPATIBILITY-AUTHORITY-1 -->

There is no single complete ext4 specification. A claim is accepted only when
it is consistent with the following authority stack, in this order:

1. **On-disk authority:** current Linux ext4 format documentation plus the
   structure encoders/validators in e2fsprogs.
2. **Behavioral authority:** a current Linux ext4 mount for syscall results,
   error cases, namespace semantics and mount-state transitions.
3. **Consistency authority:** `e2fsck -fn` on an offline copy after every
   mutation or recovery campaign. Tx self-checks do not override e2fsck.
4. **Regression authority:** applicable generic/ext4 xfstests and focused Linux
   differential fixtures.
5. **Algorithm references:** rsext4 and other implementations may explain an
   algorithm, but never define Tx ownership, cache, locking or durability.

Every capability-ledger row names its active tier, feature bits, Linux or
e2fsprogs witness, Tx owner, error mapping and crash gate. A passing unit test
without an image-level witness proves only the local algorithm.

### 1.6 Feature admission and mount policy

<!-- txdoc:TX-EXT4-PLAN-FEATURE-ADMISSION-MOUNT-POLICY-1 -->

Mount begins by producing an immutable `Ext4FeatureSet` and selecting
`ReadOnly`, `Tier1ReadWrite`, or `Tier2ReadWrite`. Admission follows Linux's
feature classes:

- Unknown `incompat` bits reject both RO and RW mount. The format layer reports
  unsupported; the initial Linux-compatible mount boundary returns `EINVAL`.
- Unsupported `ro_compat` bits permit RO mount only when all structures needed
  for safe reading are understood; an initial RW mount returns `EINVAL`, while
  an attempted RO-to-RW remount returns `EROFS`, before any home-block write.
- Unknown `compat` bits may be ignored only when Linux defines them as safe to
  ignore. Their fields and bytes are preserved by round-trip encoders.
- A bad required checksum, impossible geometry, out-of-bounds reference or
  malformed tree returns `EUCLEAN` (or `EIO` when the failure is device I/O),
  never a panic and never partial mount publication.
- A runtime metadata or journal error aborts the current transaction and moves
  the mount to an error state. Tier 1 implements deterministic remount-read-only
  behavior; continuing RW after an integrity error is forbidden.

The exact accepted masks live in one generated/tested table in
`tx-ext4-format`; mount code must not duplicate ad hoc bit checks. Fixture
generation records `mke2fs`, kernel and e2fsprogs versions plus `dumpe2fs -h`
output so a distro default change cannot silently broaden the profile.

### 1.7 Tier 1 - controlled production profile

<!-- txdoc:TX-EXT4-PLAN-TIER1-CONTROLLED-PROFILE-1 -->

Tier 1 is the first production and boot acceptance gate. Its image recipe is
controlled and versioned; accepting an arbitrary distro-default ext4 image is
not a Tier 1 claim.

| Area | Tier 1 contract |
|---|---|
| Geometry | 4 KiB blocks; 128/256-byte inodes; extents required; 64-bit block numbers accepted; one external block device; no online resize |
| Common features | journal, filetype dirents, extents, `64bit`, `flex_bg`, sparse-super/large-file/huge-file forms, `extra_isize`, `dir_nlink`, `metadata_csum` and `csum_seed` when emitted by the pinned recipe |
| Mapping | inline extent root and the tested depth-1 indexed form; holes and sparse growth; larger/deeper shapes reject before mutation rather than corrupting |
| Directories | linear directories plus htree lookup/readdir; insertion is supported while the current leaf has capacity; split/rebalance belongs to Tier 2 |
| Objects | regular files, directories, fast/block symlinks and hard links; unlinked-open lifetime and classic-orphan recovery are journaled for supported shapes |
| Mutation | create, mkdir, link, symlink, unlink, rmdir, same/cross-directory rename, chmod/chown/utimens, buffered write, truncate and fsync/fdatasync through immutable mutation admission |
| Durability | JBD2 ordered mode, replay, revoke and classic-orphan recovery for supported free/truncate shapes, regular/directory fsync, fdatasync, syncfs, sync, checkpoint, clean unmount, serialized journal wrap/backpressure and recovery-only mount state on uncertain commit or settlement failure |
| Integration | VFS/Mount/PageBacked ownership, L5 ext4 plans, L4/L6 I/O-manager execution, mmap/read/write and Alpine/OSComp guest workflows |

Tier 1 may return `EOPNOTSUPP` before mutation for a shape outside the table,
but it must not advertise RW admission for a feature whose ordinary operation
can reach an unhandled shape. The controlled fixture bounds file fragmentation,
directory fanout and extent depth accordingly.

### 1.8 Tier 2 - mainstream Linux ext4 compatibility

<!-- txdoc:TX-EXT4-PLAN-TIER2-MAINSTREAM-LINUX-1 -->

Tier 2 removes the controlled-shape limits while preserving the same ownership
and transaction protocol. It adds:

- arbitrary legal extent-tree depth, multi-child split/merge, unwritten extent
  conversion, fragmented files and files larger than 4 GiB;
- complete htree collision, split and rebalance behavior for large directories;
- `orphan_file` admission/recovery plus high-concurrency unlinked-open and
  truncate recovery beyond the Tier 1 classic-orphan shapes;
- common xattrs, `security.*`/`user.*` storage needed by Linux userland and
  POSIX ACLs, after the VFS/credential authority seam is documented;
- common `fallocate` operations (preallocate, punch-hole, zero-range),
  `statfs`, `msync`, `O_DIRECT` coherency, `FIEMAP` and the common ext4
  ioctl subset selected by actual Linux tests;
- full mount error policy, remount-RO/RW transitions, forced-unmount semantics,
  and sustained SMP mutation under journal pressure;
- the applicable xfstests generic/ext4 groups, with every exclusion tied to a
  named Tier 3 feature rather than a generic skip.

Tier 2 is reached incrementally by vertical slices. Tier 1 remains a permanent
fast gate and must stay green; Tier 2 work cannot weaken its feature rejection
or durability guarantees.

### 1.9 Tier 3 and explicit non-goals

<!-- txdoc:TX-EXT4-PLAN-TIER3-NON-GOALS-1 -->

The combined Tier 1+2 plan still excludes non-4-KiB block sizes, ext2/ext3
indirect-block mode, `bigalloc`, inline data, encryption/fscrypt, verity,
casefold, quotas/project quotas, DAX, MMP, online resize, reflink and
`data=journal`/`data=writeback`. Encountering an incompatibility that requires
one of these features extends the profile only through a design update and a
new fixture/xfstests gate; it is not fixed by silently accepting the bit.

---

## 2. Repository layout

<!-- txdoc:TX-EXT4-PLAN-REPOSITORY-LAYOUT-1 -->

```
tx-kernel/
├── crates/
│   ├── tx-ext4/              # this crate
│   │   ├── src/
│   │   │   ├── format/       # phase 0: sans-IO
│   │   │   ├── ondisk/       # phase 1+: PC-backed metadata access
│   │   │   ├── pager/        # impl FsPageBacking
│   │   │   ├── namespace/    # impl FsOps
│   │   │   ├── journal/      # phase 4: JBD2 step machine
│   │   │   ├── mount.rs
│   │   │   └── lib.rs
│   │   ├── tests/            # host-runnable integration tests
│   │   └── Cargo.toml
│   └── tx-ext4-format/       # extracted phase-0 crate (no_std, no kernel deps)
├── external/
│   └── rsext4/               # git submodule, reference only
└── …
```

### 2.1 Crate split

<!-- txdoc:TX-EXT4-PLAN-CRATE-SPLIT-1 -->

- **`tx-ext4-format`** (standalone, `no_std`, no dependencies on kernel crates): pure on-disk format definitions and parsing/encoding functions. Runnable in host `cargo test` without any txKernel infrastructure. This is the phase-0 deliverable.
- **`tx-ext4`** (kernel crate): consumes `tx-ext4-format` and the kernel's VFS/PC/block-device interfaces. Implements the traits. Phases 1+.

The split exists so phase 0 can be developed, tested, and stabilized without waiting for the kernel crates it eventually integrates with.

### 2.2 Submodule discipline

<!-- txdoc:TX-EXT4-PLAN-SUBMODULE-DISCIPLINE-1 -->

```
git submodule add https://github.com/Starry-OS/rsext4 external/rsext4
```

- `external/rsext4` is never compiled as part of the build.
- Porting notes in `tx-ext4-format` source cite specific rsext4 files as "reference: rsext4/src/ext4_backend/inode.rs" where appropriate.
- Upstream updates to rsext4 are not automatically pulled; the pin updates when we deliberately consult it.

---

## 3. Interfaces

<!-- txdoc:TX-EXT4-PLAN-INTERFACES-1 -->

This section specifies the exact types tx-ext4 implements and consumes. These are contract-level: tx-ext4 sees types from VFS, VM, and foundation; it never sees RNode, DEntry, OpenFile, or any other live-node entity. Imports in tx-ext4 are constrained by this section.

### 3.1 Imports permitted from kernel crates

<!-- txdoc:TX-EXT4-PLAN-IMPORTS-PERMITTED-KERNEL-CRATES-1 -->

tx-ext4 may import from these crates only:

| Dependency surface | Types permitted |
|---|---|
| `tx-fnd::types` | `Errno`, `PageSize`, numeric newtypes |
| `tx-fnd::step` | `StepOutcome<T>`, `Guard`, `Channel`, `Mask`, `Blocked`, `Done`, `Advanced` |
| `tx-fnd::sync` | `AtomicU64`, `AtomicU32` (for superblock mirror counters) |
| `tx-fnd::block` | `BlockDevice` trait, `PhysicalBlockNumber`, `BlockReadReq`, `BlockWriteReq` |
| PageBacked compatibility surface | `PageDataLease`/staged `PageLease`, opaque object/range keys, and current `FsPageBacking` bridge while migration is active; this may remain module-local before extraction |
| `tx-pager-api` | range/layout values, `FileLayoutPlanner`, `FileIoPlan<K>`, and opaque payload keys only; this may remain module-local before crate extraction |
| `tx-vfs::fs_ops` | `FsOps` trait (implemented by tx-ext4), `InodeMeta`, `DirEntry`, `DirCursor`, `Credential`, `FsObjectId` |
| `tx-vfs::mount` | `MountId`, `MountInitContext` (for mount bringup handshake) |

**Forbidden imports** (enforced by grep-lint in CI, per T1.7):

- `tx-vfs::rnode` — no `RNode`, `Cap<RNode>`, `Weak<RNode>`, `IdentRef<RNode>`.
- `tx-vfs::dentry` — no `DEntry` types.
- `tx-vfs::open_file` — no `OpenFile` types.
- `tx-vfs::walker` — no walker state types.
- `tx-proc::*` — no process/thread entities.

The asymmetry: the kernel-facing `tx-ext4` adapter may consume PageBacked
capabilities, but the pure pager never imports PageBacked, PPN, `BioVec`,
reactor, or VFS live-node types. VFS constructs all RNode state itself.

### 3.2 `FsPageBacking` — trait implemented by tx-ext4

<!-- txdoc:TX-EXT4-PLAN-FSPAGEBACKING-TRAIT-IMPLEMENTED-TX-EXT4-1 -->

Defined in `tx-vm::page_backed`; this is the current compatibility bridge. It
remains valid during compatible migration, but the target splits pure
`FileLayoutPlanner` planning from the Tx adapter that retains PageDataLease and
lowers `FileIoPlan<K>` into the existing `BackendBioGraph`.

```rust
pub trait FsPageBacking: Send + Sync + 'static {
    /// Fetch the page at `offset` for the persistent object `fs_object_id`.
    /// Returns `Done(frame)` if content is immediately available; `Blocked` if
    /// backing-device I/O is in flight (the step yields on the returned channel+mask).
    ///
    /// Implementation allocates the Frame, reads from the block device into it,
    /// and returns the populated Frame. The caller (VM `step_read` or fault handler)
    /// installs it into the PC page index via `install_if_match`.
    fn fetch_page<'g>(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &'g Guard,
    ) -> StepOutcome<Frame>;

    /// Write a dirty page back to backing storage.
    /// For Data PCs: ordered-mode writes the block directly (pre-commit-record).
    /// For metadata PCs: enqueues the dirty block into the current journal transaction
    /// and returns `Done(())` immediately; actual disk write happens at txn commit.
    fn flush_page<'g>(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    /// Truncate the persistent object to `new_size`. Shrinks the extent tree,
    /// frees blocks via bitmap updates, updates the on-disk inode size.
    /// Returns `EROFS`, `EDQUOT`, or format-level errors before mutating any
    /// page-level state.
    fn truncate<'g>(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    /// Flush all dirty pages for `fs_object_id` to backing storage. For journaled
    /// filesystems, also forces the current transaction (if it contains this inode's
    /// metadata) through its commit step machine to durability.
    fn fsync<'g>(
        &self,
        fs_object_id: FsObjectId,
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    /// Capability query. ext4: always false.
    fn supports_reflink(&self, _other: &PageContainer) -> bool { false }
}
```

All four mutating methods are `StepOutcome`-returning: `Blocked` yields the coroutine on the reactor wire registered by the block device for this I/O; resumption re-enters the step function, re-observes any PC state it cached (per ARCH-5 re-read discipline), and continues.

### 3.3 `FsOps` — trait implemented by tx-ext4

<!-- txdoc:TX-EXT4-PLAN-FSOPS-TRAIT-IMPLEMENTED-TX-EXT4-1 -->

Defined in `tx-vfs::fs_ops`. This is tx-ext4's namespace-oracle role. Full signature:

```rust
pub trait FsOps: Send + Sync + 'static {
    /// Namespace lookup. Returns the persistent object id for `name` in `parent`.
    /// Returns `ENOENT` if no entry. Does not construct or return RNodes.
    fn lookup<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        guard: &'g Guard,
    ) -> StepOutcome<FsObjectId>;

    /// Read on-disk inode record, decode POSIX metadata, return it.
    /// Called by the MountPayload find-or-create protocol (VFS_CHECKS_V2.1.md §6.2) when
    /// the coherence index has no live RNode for this fs_object_id.
    /// The returned InodeMeta.mode (S_IFMT bits) classifies the file kind;
    /// no separate discriminant is returned.
    fn load_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        guard: &'g Guard,
    ) -> StepOutcome<InodeMeta>;

    /// Serialize VFS-side decoded metadata back into the on-disk inode record.
    /// Called after VFS mutates InodeMeta fields (chmod, chown, utimes, nlinks
    /// adjustment). For journaled mode: marks the inode-table block dirty in the
    /// current transaction.
    fn serialize_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    /// Allocate a new on-disk inode under `parent` with `name`, initial `mode`,
    /// and credential-derived uid/gid. Returns the new id and the constructed
    /// InodeMeta that VFS will install on the fresh RNode.
    fn create_inode<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &'g Guard,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    /// Remove the directory entry `name` in `parent` that targets `target`.
    /// Decrements `target`'s on-disk nlink. Does NOT destroy the inode;
    /// destruction is triggered by VFS via `destroy_inode` when payload
    /// liveness falls false.
    fn unlink<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    /// Atomic rename. If `new_parent`/`new_name` points at an existing target,
    /// that target's nlink is decremented; destroy semantics are VFS's concern.
    fn rename<'g>(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    fn link<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    fn mkdir<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &'g Guard,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn rmdir<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    fn symlink<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &'g Guard,
    ) -> StepOutcome<(FsObjectId, InodeMeta)>;

    fn readdir<'g>(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        guard: &'g Guard,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>>;

    /// Free the on-disk inode and all blocks it references. Called from VFS's
    /// RNode payload-drop path when payload_live falls false (nlinks == 0 and
    /// no OpenFile pins remain). Must succeed or leave the on-disk state
    /// recoverable by e2fsck.
    fn destroy_inode<'g>(
        &self,
        fs_object_id: FsObjectId,
        guard: &'g Guard,
    ) -> StepOutcome<()>;
}
```

**Unified type across both traits.** `FsObjectId` is a `#[repr(transparent)] pub struct FsObjectId(pub u64)` defined in `tx-vfs::fs_ops`. ext4 uses the low 32 bits (inode number); the type is 64-bit to accommodate filesystems with larger identifier spaces.

### 3.4 VFS-side types consumed by tx-ext4

<!-- txdoc:TX-EXT4-PLAN-VFS-SIDE-TYPES-CONSUMED-TX-EXT4-1 -->

Types tx-ext4 must understand and construct, all defined by VFS:

```rust
/// Decoded POSIX inode metadata. VFS-side authoritative representation of an
/// inode's attributes. tx-ext4 serializes this to/from on-disk inode records.
/// This is the only "inode-like" type tx-ext4 handles; it does not see RNode.
pub struct InodeMeta {
    pub mode: u16,          // S_IFMT | perm bits
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub nlinks: u32,
    pub blocks: u64,        // 512-byte blocks allocated; st_blocks report
    pub flags: u32,         // immutable, append-only, etc. (POSIX-abstract)
}

/// Opaque directory iteration cursor. Implementations define internal layout.
/// For ext4: encodes htree traversal state or linear block offset.
pub struct DirCursor(pub [u8; 16]);

/// One entry returned by readdir.
pub struct DirEntry {
    pub name: ArrayString<256>,    // NAME_MAX
    pub fs_object_id: FsObjectId,
    pub d_type: u8,                // DT_* classification for optimization
}

/// Credential for authorization checks at mutation time. Snapshot at syscall entry.
pub struct Credential {
    pub uid: u32,
    pub gid: u32,
    pub egid: u32,
    pub groups: ArrayVec<u32, 32>,
    pub capabilities: CapSet,
}
```

None of these types reference RNode, DEntry, OpenFile, or any live-node entity.

### 3.5 `BlockDevice` — trait consumed by tx-ext4

<!-- txdoc:TX-EXT4-PLAN-BLOCKDEVICE-TRAIT-CONSUMED-TX-EXT4-1 -->

The async block device abstraction. Defined in `tx-fnd::block`; tx-ext4 consumes it, a virtio-blk or NVMe driver implements it.

```rust
pub trait BlockDevice: Send + Sync + 'static {
    /// Read `count` blocks starting at `block_id` into a caller-provided Frame slice.
    /// Returns `Blocked(channel, mask)` with the channel+mask on which to resume;
    /// on resume, the step checks completion status via `poll_completion`.
    fn read_blocks<'g>(
        &self,
        block_id: PhysicalBlockNumber,
        count: u32,
        target: &mut [Frame],
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    /// Write `count` blocks from Frames to the device, starting at `block_id`.
    /// Ordering: two calls on the same BlockDevice are NOT ordered with each other
    /// unless a barrier is issued between them via `barrier`.
    fn write_blocks<'g>(
        &self,
        block_id: PhysicalBlockNumber,
        count: u32,
        source: &[Frame],
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    /// Durability barrier. All writes submitted before the barrier are durable
    /// before any write submitted after it can reach the medium. Required for
    /// journal ordering (data-before-commit-record-before-checkpoint).
    fn barrier<'g>(&self, guard: &'g Guard) -> StepOutcome<()>;

    fn total_blocks(&self) -> u64;
    fn block_size(&self) -> u32;
}
```

**Contrast with rsext4's `BlockDevice`.** rsext4's trait has synchronous `&mut self` methods (`read`, `write` with `BlockDevResult` return). This trait replaces all of them: no `&mut self` (the device is concurrent-capable), no synchronous return (async via `StepOutcome`), adds `barrier` (required for journal; rsext4's ordered-mode depends on the kernel to supply this).

### 3.6 `MountInitContext` — tx-ext4 ↔ VFS mount handshake

<!-- txdoc:TX-EXT4-PLAN-MOUNTINITCONTEXT-TX-EXT4-VFS-MOUNT-HANDSHAKE-1 -->

When VFS mounts an ext4 image, it passes tx-ext4 a `MountInitContext` and receives back a constructed `Box<dyn FsOps + FsPageBacking>` (or similar fat pointer). Specifically:

```rust
pub struct MountInitContext {
    pub block_device: Arc<dyn BlockDevice>,
    pub mount_id: MountId,
    /// Handle VFS gives tx-ext4 for allocating metadata PCs. tx-ext4 calls this
    /// to construct the inode-table PC, GDT PC, bitmap PCs at mount time.
    /// These PCs are held by the resulting MountPayload.
    pub metadata_pc_factory: MetadataPcFactory,
    /// Options parsed from mount(2): read-only, noatime, etc.
    pub options: MountOptions,
}

pub trait MetadataPcFactory: Send + Sync {
    /// Construct a PC whose contents are backed by a contiguous block range
    /// on the block device, populated lazily on fetch. Return the handle.
    /// These PCs do not have RNodes; they are held as fields inside
    /// MountPayload and only accessed by the owning filesystem.
    fn create_metadata_pc(
        &self,
        start_block: PhysicalBlockNumber,
        block_count: u64,
    ) -> Result<Cap<PageContainer>, Errno>;
}

pub struct MountOutput {
    pub fs_ops: Arc<dyn FsOps>,
    pub fs_page_backing: Arc<dyn FsPageBacking>,
    pub root_fs_object_id: FsObjectId,       // for VFS to construct the root RNode
    pub root_inode_meta: InodeMeta,
}
```

The factory pattern keeps PC construction authority with VM/VFS (who know the zone and publication discipline) while letting tx-ext4 drive the mount's resource layout.

### 3.7 Summary of the boundary

<!-- txdoc:TX-EXT4-PLAN-SUMMARY-BOUNDARY-1 -->

tx-ext4 implements two traits (`FsPageBacking`, `FsOps`), consumes one trait (`BlockDevice`), handles a handful of value types (`InodeMeta`, `DirCursor`, `DirEntry`, `Credential`, `FsObjectId`), and invokes one factory at mount time (`MetadataPcFactory`). That is the complete surface. Any other interaction with the rest of the kernel is a leak and should be caught at review.

---

## 4. Final goals and compatibility gates

<!-- txdoc:TX-EXT4-PLAN-FINAL-GOALS-V1-ACCEPTANCE-1 -->

### 4.1 Tier 1 functional goals

<!-- txdoc:TX-EXT4-PLAN-FUNCTIONAL-GOALS-1 -->

1. **Mount admission.** Mount the pinned Tier 1 4-KiB profile with or
   without an unreplayed journal. Select RO/RW from the feature table and reject
   unsupported/corrupt images before publishing the mount.
2. **Read path.** `read(2)` on any regular file in the filesystem returns correct data. Works through VFS walker → RNode PageBacked(File) PC → `FsPageBacking::fetch_page` → extent walk → block device read → frame installed. Yields on disk I/O.
3. **Directory traversal.** `readdir(2)`, `getdents(2)` and lookup work over
   linear and Tier 1 htree directories. A mutation that would require a Tier 2
   htree split fails before admission.
4. **Write path.** `write(2)`, `truncate(2)`, `ftruncate(2)` correctly mutate file data. Sizes grow through the extent allocator. `flush_page` produces correct on-disk content.

   File timestamps use the kernel's single `CLOCK_REALTIME` timebase. The
   platform monotonic source is installed once at boot and combined with the
   wall-clock offset; filesystem code must not invent an independent epoch or
   pass zero as "now". Creation initializes atime/mtime/ctime from that clock.
   Page-cache writeback and explicit truncate commit size, mtime, and ctime in
   the same inode-table update, so build tools cannot observe new content with
   an epoch-zero or stale modification time.
5. **Namespace mutations.** `creat`, `open(O_CREAT)`, `unlink`, `rmdir`, `mkdir`, `rename`, `link`, `symlink` all work and leave the filesystem consistent. Unlinked-but-open holds correctly via `destroy_inode` triggered by payload-liveness loss.
6. **fsync.** `fsync(2)` flushes data pages and commits any pending journal transaction containing the file's metadata.
7. **Journal correctness.** Crash (simulated via deterministic hard-kill cut
   points) during every supported mutation, replay, and run `e2fsck -fn`: the
   filesystem is clean, fsync-success data survives, and no uncommitted state is
   published after recovery.
8. **Orphan correctness.** Unlinking an open file and truncating a file across a
   crash use the classic orphan mechanism for Tier 1 shapes; replay/recovery
   finishes reclamation without an e2fsck repair.
9. **Boot-level scenarios.** Boot tx-kernel with an ext4 root, run busybox, run a C compiler on a source file to an ext4 output.

### 4.2 Tier 1 integration goals

<!-- txdoc:TX-EXT4-PLAN-INTEGRATION-GOALS-1 -->

1. **VFS walker consumes tx-ext4.** The walker's `NeedIO` resume path correctly delegates to `FsOps::lookup` and `FsOps::load_inode_meta` via the MountPayload coherence-index find-or-create protocol.
2. **Page fault handler consumes tx-ext4.** User-space page faults on file-backed mappings drive through `FsPageBacking::fetch_page` with correct `StepOutcome::Yield { shape: YieldShape::OnWaitSource { .. } }` yield behavior.
3. **No tx-ext4 reference to `Cap<RNode>`.** Verified by `grep` — the tx-ext4 crate does not import `RNode` or hold it in any type.
4. **No rsext4 code in runtime build.** Verified by `cargo tree` — rsext4 is not a compile-time or runtime dependency.
5. **One production durability path.** Production RW mounts use discovered
   journal geometry and the planner/runtime path. Direct pager home writes are
   test or compatibility oracles and cannot be selected by production code.
6. **No filesystem service transport.** ext4 produces L5 plans; generic L4/L6
   service futures execute them. No private request/reply server sits between
   VFS/PageBacked and the mounted instance.

### 4.3 Performance goals (soft targets, not blockers)

<!-- txdoc:TX-EXT4-PLAN-PERFORMANCE-GOALS-SOFT-TARGETS-NOT-BLOCKERS-1 -->

- Sequential read throughput within 2× of the raw block device throughput.
- No heap allocation on the syscall hot path (prefault discipline, per VM spec).
- Shootdown batching per step commit (inherited from page substrate).

Performance is explicitly secondary to correctness for v1.

### 4.4 Tier 2 closure goals

<!-- txdoc:TX-EXT4-PLAN-TIER2-CLOSURE-GOALS-1 -->

Tier 2 is complete only when all §1.8 slices have Linux differential and crash
evidence, the declared xfstests set has no unexplained failures, and image
exchange works in both directions:

1. Linux creates and mutates a supported Tier 2 image; Tx mounts and continues
   using it without `e2fsck` repair.
2. Tx performs each supported mutation and cleanly unmounts; Linux mounts it RW
   and applicable xfstests continue to pass.
3. Power-cut campaigns cover extent/htree splits, orphan recovery, xattr/ACL,
   fallocate, direct-I/O overlap and journal wrap.
4. Every skipped xfstest maps to a named Tier 3 feature or a separately tracked
   non-filesystem kernel gap. Timeouts and harness failures are not skips.
5. SMP stress shows no duplicate allocation, stale mapping publication,
   deadlock or post-abort RW mutation.

---

## 5. Phase breakdown and test milestones

<!-- txdoc:TX-EXT4-PLAN-PHASE-BREAKDOWN-TEST-MILESTONES-1 -->

Each phase has a self-contained deliverable and explicit test milestones. Phase
0-1 establish shared read/format foundations; phases 2-4 close Tier 1; phase 5
is the ordered Tier 2 expansion. The current implementation may already contain
parts of a later phase, but a phase closes only when its full gate passes.

| Phase | Compatibility role | Promotion gate |
|---|---|---|
| 0 | Shared oracle and sans-I/O format | deterministic fixtures and byte-preserving codecs |
| 1 | Shared RO integration | feature admission plus planner-driven read witnesses |
| 2 | Tier 1 mutations | bounded-shape namespace/data operations through mutation admission |
| 3 | Tier 1 async execution | all production file I/O through L4/L5/L6 |
| 4 | Tier 1 durability and production cutover | replay/fsync/checkpoint/crash evidence and one RW path |
| 5 | Tier 2 mainstream Linux closure | advanced shapes/features plus declared xfstests set |

### Phase 0 — Sans-IO format module

<!-- txdoc:TX-EXT4-PLAN-PHASE-0-SANS-IO-FORMAT-MODULE-1 -->

**Crate.** `tx-ext4-format` (standalone, `no_std`).

**Scope.**

- Superblock (`SUPERBLOCK_MAGIC`, block size, feature flags, inode size, blocks-per-group, inodes-per-group, inode table block, journal inode, UUID, label).
- Block group descriptor (32-bit and 64-bit variants).
- Inode record (parse/encode; both 128-byte and 256-byte layouts).
- Extent tree node (header + entries or indices); pure walk function: `walk(node_bytes, logical_block) → Physical | Descend(block)`.
- Linear directory block parsing: iterator over `(name, inode, file_type)` entries with length validation.
- Htree root/node parsing: given a hash, traverse to the leaf directory block.
- Directory name hash functions (half-md4, legacy, tea — whichever the superblock declares).
- Bitmap parsing: iterator over free/used bits.
- JBD2 on-disk record formats (journal superblock, descriptor block, commit block, revoke block, data record) — encoding/decoding, not journal-state machine.
- Checksum routines (seed-based CRC32c for superblock, GDT, inode, htree, directory block).

**Explicit non-scope for phase 0:**

- Any I/O.
- Any caching.
- Any mutable state beyond what's needed to encode a struct.
- Extent tree splits and merges (Tier 1 bounded insertion is phase 2; arbitrary
  depth is phase 5).
- Htree node splits (phase 5).

**Test milestones.**

- **T0.1 Read known-good image.** Take an ext4 image created by `mkfs.ext4` in a controlled configuration, parse its superblock, enumerate every inode in the inode table, resolve every directory entry. Compare against output from `dumpe2fs` and `debugfs`. No discrepancies.
- **T0.2 Extent tree walk against debugfs.** For a file with 5 extents, walk the tree for every logical block, compare physical-block results against `debugfs -c 'bmap <inode> <block>'`.
- **T0.3 Htree traversal.** For a directory with 500+ entries forcing htree, look up every name by hash traversal and compare against a linear enumeration of the same directory.
- **T0.4 Checksum validation.** Re-compute checksums for every on-disk structure in the test image and verify they match the stored values. Any mismatch is a bug in format code.
- **T0.5 Round-trip encode.** Parse a struct, encode it back, compare byte-for-byte against the original. Must be identity for all supported structure types.
- **T0.6 JBD2 record decoding.** Given a filesystem deliberately left dirty (mount, write, kill without unmount), parse the journal. Enumerate transactions. Verify commit records, descriptor records, data records all decode.

**Phase-0 exit criterion.** All T0.* pass. Test image coverage includes: clean filesystem, filesystem with linear directories, filesystem with htree directories, filesystem with multi-extent files, filesystem with sparse files, filesystem with a dirty journal.

**Estimated effort.** 1–2 weeks for a focused implementer with rsext4 open as reference.

**Dependencies.** None. Runs on host with stock Rust.

---

### Phase 1 — Read-only mount, no journal

<!-- txdoc:TX-EXT4-PLAN-PHASE-1-READ-ONLY-MOUNT-NO-JOURNAL-1 -->

**Crate.** `tx-ext4` (kernel crate); depends on `tx-ext4-format`.

**Scope.**

- `Ext4FsInstance` struct. Holds: block device handle, parsed superblock, `Cap<PageContainer>` handles for metadata regions (GDT, inode table, block bitmap, inode bitmap — one or more PCs depending on filesystem size), a `dirty_state` flag (refuse to mount if dirty until phase 4 lands replay).
- `MountPayload` wiring: when a mount is created, construct the `Ext4FsInstance`, build the metadata PCs, build the coherence index for RNodes (an `fs_object_id → Weak<RNode>` map per CONCEPTS §15.7).
- Metadata PC population: filesystem-private mount-lifetime metadata PCs use
  the existing PageBacked/I/O-manager seam. They are not RNodes, not generic
  file-data cache entries and not an rsext4-style decoded metadata cache.
- `impl FsOps` — read-side:
  - `lookup(parent_fs_object_id, name) → fs_object_id`: htree or linear walk against the directory's data PC.
  - `load_inode_meta(fs_object_id) → InodeMeta`: read the inode-table PC at the right offset, parse with format module, return decoded metadata.
  - `readdir(fs_object_id, cursor) → DirEntry`: iterate entries.
- `impl FsPageBacking` — read-side:
  - `fetch_page(fs_object_id, offset) → Frame`: extent walk in inode-table PC bytes to find physical block; read from block device; install in PC.
  - `flush_page`, `truncate`, `fsync`: return `Errno::EROFS` for phase 1.
- **Async block device trait.** `BlockDevice::read_block(blockno) → Future<Result<Frame, Errno>>`. Writes not yet required. Integrates with reactor's I/O completion wire.

**Test milestones.**

- **T1.1 Mount and enumerate.** Mount a clean read-only ext4 image; `ls -lR /` on it matches a reference listing.
- **T1.2 File read.** `cat` a 100 KB file; `cmp` byte-for-byte against reference.
- **T1.3 Large file read.** `cat` a 100 MB file spanning many extents, including a hole (sparse file); verify content and hole-produces-zeros.
- **T1.4 Deep directory.** Read file from 20-level-deep path.
- **T1.5 Many entries.** `ls` a directory with 10,000 entries forcing htree.
- **T1.6 Concurrent readers.** Three processes simultaneously reading different parts of the same large file. No corruption, no deadlocks, yields correctly.
- **T1.7 No Cap<RNode> in tx-ext4.** Static check: `grep -r "Cap<RNode>\|&RNode\|Weak<RNode>" crates/tx-ext4/src/` returns no matches.
- **T1.8 I/O yields.** Trace a page fault on a file-backed mapping; verify the coroutine parked on block-device completion and resumed on completion wire fire.

**Phase-1 exit criterion.** All T1.* pass. Booting tx-kernel to a shell with `/bin/ls` on an ext4 rootfs works if writes are never attempted.

**Estimated effort.** 3–4 weeks assuming VM/PC/walker infrastructure is mature.

**Dependencies.**

- VM subsystem: `PageContainer`, `FsPageBacking` trait, `step_read`.
- VFS: walker, `FsOps` trait definition, MountPayload coherence index.
- Reactor: async I/O completion wires.
- An async block device implementation (virtio-blk on qemu-riscv64, or a loopback-file async shim for host tests).

---

### Mutation admission contract (all read-write phases)

<!-- txdoc:TX-EXT4-MUTATION-ADMISSION-CONTRACT-1 -->

Every persistent ext4 mutation, including `chmod`, `chown`, `utimens`, size
changes, extent allocation, directory updates, bitmap updates and inode
link-count changes, enters the same immutable admission protocol. A backend
must not publish a decoded metadata change and must not call a direct home
write as a substitute for this protocol.

1. **Read set.** The operation reads the superblock/group descriptor state,
   inode-table and directory/extent/bitmap blocks it will inspect. Each input
   records `(block_no, observed_generation, observed_checksum)` (or the
   equivalent immutable block identity supplied by the metadata PC). Missing
   blocks and feature bits are also preconditions. The read set is frozen before
   reservation; a later re-read is a conflict, not an implicit refresh.
2. **After-image.** The format planner computes a complete immutable
   `AfterImage { block_no, bytes }` for every changed block and an explicit
   merge row for blocks touched by more than one logical field. The source
   `BlockImage` and all caller-owned `InodeMeta` values remain unchanged. An
   after-image may be admitted only if it preserves unknown ext4 fields and
   passes the format/checksum validators.
3. **Reservation.** Admission reserves journal descriptor, data and commit
   space, plus any allocator claims, before publication. Reservation failure
   returns `ENOSPC`/`EBUSY` without changing a PC, RNode, inode bitmap or
   journal cursor. Reservations are mount-local and released on every error
   path.
4. **Prepare and commit.** `JournalMutationRuntime` validates the read set,
   stages the after-images, writes ordered data before the commit record, and
   makes the transaction durable only after the commit record is confirmed.
   Conflicts retry from a fresh read set; they never merge mutable references
   captured across an await.
5. **Rollback.** Any failed prepare, I/O completion, checksum check or journal
   abort releases reservations and discards staged after-images. The original
   metadata PCs and VFS state are still authoritative. Recovery replays only
   transactions with a valid commit record and matching checksums.
6. **Publication.** The VFS `RNode`/`PageBacked` metadata projection is updated
   only after terminal commit completion makes the admitted transaction
   durable. Admission alone is not publication. The
   publication carries the same mutation origin and object identity used by
   the read set, so a rematerialized inode cannot observe an uncommitted mode,
   size or link-count value. `fsync` waits for this transaction and its ordered
   data dependencies; it does not flush an unadmitted dirty metadata page.

The contract is implemented by Tx-owned planner/runtime types. rsext4's
synchronous `&mut BlockDevice`, cache hierarchy, path/open-file APIs and
in-place mutation helpers are reference material only and are not migration
targets. `write_inode_meta_journaled` is a compatibility oracle for existing
tests; it is not an extension point for new production mutations and is
retired when the `SetAttr` vertical slice lands.

**Admission exit criterion.** Host tests prove read-set conflicts, complete
after-image coverage, reservation release, rollback without source mutation,
and publication-after-commit. A static check rejects new production calls to
`BlockImage::write_block` outside the journal runtime.

---

### Phase 2 — Writes using mutation admission

<!-- txdoc:TX-EXT4-PLAN-PHASE-2-WRITES-WITHOUT-JOURNAL-1 -->

**Scope.**

- `FsPageBacking::flush_page` for Data PCs: extent walk, immutable data after-image and journal admission.
- `FsPageBacking::truncate`: shrink extent tree, free blocks (updates bitmap PCs), update inode size through one mutation plan.
- `FsPageBacking::fsync`: flush dirty data pages of the target PC and force-commit the admitted transaction containing their metadata.
- `FsOps::create_inode`: bitmap scan, allocate inode, write inode record to inode-table PC (marking that PC dirty).
- `FsOps::serialize_inode_meta`: write back updated `InodeMeta` to the inode-table PC.
- `FsOps::destroy_inode`: free inode; free all extent-mapped blocks; clear the inode record.
- `FsOps::unlink`: remove directory entry from parent directory's data PC; decrement `InodeMeta.nlinks` (via `serialize_inode_meta`); if `nlinks` reaches zero and no OpenFile pins exist, trigger `destroy_inode` via the VFS-side script (not from within tx-ext4).
- `FsOps::mkdir`, `rmdir`, `rename`, `link`, `symlink`: equivalent; all go through directory-block mutation + inode bitmap/allocation + `serialize_inode_meta`.
- Tier 1 inline/depth-1 extent insertion, merge and supported-tail truncate.
  A request requiring a second child or deeper root returns `EOPNOTSUPP` before
  mutation and is a Tier 2 fixture.
- Linear-directory mutation and htree insertion while the selected leaf has
  capacity. Htree split/rebalance is Tier 2.
- Classic orphan-chain admission and mount recovery for unlinked-open and
  truncate-in-progress operations within Tier 1 shape bounds.

**Crash behavior.** Every supported phase-2 mutation is journal-admitted. A
crash may lose an operation without a durable commit, but replay must leave a
consistent filesystem; `e2fsck -n` is required after each crash-cut fixture.

**Test milestones.**

- **T2.1 Create, write, read back.** `touch`, `echo hello > f`, `cat f` — roundtrip correct.
- **T2.2 Profile-bound write.** Write, read and `cmp` a fragmented file that
  reaches the Tier 1 depth-1 extent form without requiring a second child.
- **T2.3 Truncate down and up.** Create 10 MB file; `truncate -s 1000 f`; `truncate -s 10M f`; verify old content beyond 1000 is zero on re-read (sparse-file semantics).
- **T2.4 Delete and reclaim.** Fill filesystem to 90%; delete half the files; `df` shows reclaimed space; new files succeed.
- **T2.5 Concurrent writes.** Two processes writing to different files, different directories. No interference.
- **T2.6 Rename across directories.** `mv a/foo b/bar` in linear and
  non-splitting htree fixtures. Verify one atomic namespace result in both.
- **T2.7 Unlinked but open.** Process A opens /tmp/f, process B unlinks /tmp/f. Process A continues reading and writing; contents correct. Close A's fd; verify inode actually reclaimed (ext4 free-inode count increases).
- **T2.8 Hard links.** `ln a b`; mutate through either; `stat` shows same inode; `unlink a` leaves b valid; `unlink b` actually frees.
- **T2.9 Offline `e2fsck` after clean unmount.** After clean unmount, `e2fsck -n` reports no errors.

**Phase-2 exit criterion.** All T2.* pass with immutable mutation plans and
post-operation `e2fsck -n`. Booting tx-kernel, compiling a small C program to
ext4, rebooting (clean unmount), reading the binary back all work.

**Estimated effort.** 4–6 weeks. Arbitrary extent/htree split/merge is kept out
of this bounded Tier 1 phase and scheduled in phase 5.

**Dependencies.** Phase 1 complete.

---

### Phase 3 — Async block device integration

<!-- txdoc:TX-EXT4-PLAN-PHASE-3-ASYNC-BLOCK-DEVICE-INTEGRATION-1 -->

**Scope.** If phases 1–2 used a synchronous-stub block device for testing, phase 3 is when we swap in the reactor-integrated async block device on real hardware (qemu-virtio-blk for bring-up, real PCIe NVMe later).

This phase may be absorbed into phases 1–2 depending on reactor maturity. Listed separately because the risk concentrates at this integration point.

**Test milestones.**

- **T3.1 qemu-virtio-blk under load.** 10 concurrent readers, 5 concurrent writers; filesystem operations complete correctly; no lost requests; no deadlocks.
- **T3.2 Latency distribution.** Measure syscall-to-completion latencies for `read` and `write`; verify no pathological tail (>100× median).
- **T3.3 Reactor wake storms.** Many coroutines blocked on I/O, many completions arriving in batches. All resumed; no lost wakes.

**Phase-3 exit criterion.** tx-ext4 on real virtio-blk sustains hours of mixed workload without corruption, lost I/O, or deadlock.

**Dependencies.** Reactor with I/O-completion wire support.

---

### Phase 4 — JBD2 journal, ordered mode

<!-- txdoc:TX-EXT4-PLAN-PHASE-4-JBD2-JOURNAL-ORDERED-MODE-1 -->

**Scope.**

- `JournalState` inside `MountPayload`: one Tier 1 transaction owner, its
  admitted/committing/committed-needs-settlement phase, journal extent token,
  and journal PC handles. Multiple simultaneously active or accumulated
  uncheckpointed transactions are Tier 2 scheduling optimizations.
- Mount-time replay: scan journal, identify committed transactions (those with valid commit records and matching checksums), replay their metadata writes directly. Use phase 0 JBD2 record parsing and phase 1 PC primitives. Replay is bounded work using only phases 0–2.
- Transaction API inside tx-ext4: operations that mutate metadata start by attaching to the current transaction. Mutations buffer in the transaction, not directly in the metadata PCs.
- Commit step machine: one step function per commit phase.
  - `step_data_wait`: wait for all data-page writes of ordered-mode files participating in this transaction to complete. Yields on the owned wait source while I/O is outstanding.
  - `step_metadata_write`: write the transaction's metadata blocks to the journal. Yields on journal I/O.
  - `step_commit_record`: write the commit record with checksum. Yields on journal I/O.
  - `step_checkpoint`: before admitting the next Tier 1 mutation, write the
    metadata from the journal back to home locations, flush, advance the safe
    journal tail, refresh caches, and settle the transaction token.
- Ordering rules:
  - Data writes (for files in this transaction) must reach disk before the commit record.
  - The commit record must reach disk before metadata is checkpointed to its home location.
  - Tier 1 serializes later mutation admission behind checkpoint, safe tail
    advancement, cache settlement, and token release. Background checkpoint
    and accumulated committed transactions are Tier 2 optimizations.
- Flush and force-commit paths for `fsync`.

**Test milestones.**

- **T4.1 Mount-time replay.** Deliberately kill qemu after a write but before journal checkpoint; remount; verify file visible with post-write content.
- **T4.2 Crash and e2fsck -n.** Random kill during sustained write workload (1000 iterations); remount; `e2fsck -n` reports clean; no silent corruption detected by secondary integrity checks (file-data CRCs).
- **T4.3 fsync durability.** Process A writes then fsyncs; crash; remount; A's data is present.
- **T4.4 No fsync means can lose.** Process writes but does not fsync; crash; may or may not see the data but filesystem is consistent.
- **T4.5 Concurrent callers, serialized owner.** Two processes mutate different
  inodes. The mount owner serializes their handles without lost wakeups,
  starvation, leaked tokens, or unnecessary blocking after the earlier handle
  settles. Tier 1 does not require simultaneously active transactions.
- **T4.6 Serialized journal wrap.** Drive sequential transactions across the
  ring boundary. Checkpoint and safe tail advancement reclaim each extent;
  admission blocks while the current owner has not settled and resumes with
  forward progress after reclamation.
- **T4.7 Interop with `e2fsprogs`.** After clean unmount, mount the same image on Linux, run `fsck.ext4 -f`; Linux reports clean; read a file through Linux; content correct.

**Phase-4 exit criterion.** All T4.* pass. Power-cut torture runs for 1000
iterations without corruption, production boot and dynamic mount use the sole
discovered-journal planner/runtime RW path, and the Tier 1 matrix in §1.7 has no
unproved row.

**Estimated effort.** 4–6 weeks.

**Dependencies.** Phase 2 complete. Reactor stable enough for the commit step machine's multi-phase wait sequencing.

---

### Phase 5 — Tier 2 mainstream Linux closure

<!-- txdoc:TX-EXT4-PLAN-PHASE-5-HARD-CASES-HARDENING-1 -->

**Ordered slices.** Each slice keeps Tier 1 green and adds its own Linux image,
error, fsck and crash witnesses.

1. **Mapping and allocation:** arbitrary legal extent depth, multi-child
   split/merge, unwritten extents, fragmented/large files and cross-group ENOSPC
   behavior.
2. **Large directories:** htree collision chains, leaf/index split, deletion
   and rebalance with checksum-correct directory tails.
3. **Orphan recovery:** implement `orphan_file` admission/recovery and stress
   high-concurrency unlinked-open/truncate recovery beyond Tier 1 bounds.
4. **Metadata compatibility:** common xattrs, `security.*`/`user.*`, POSIX ACLs,
   special inodes and the VFS credential checks that authorize them.
5. **Space and coherency:** common fallocate modes, `statfs`, `syncfs`, `msync`,
   direct-I/O overlap/invalidation and the selected `FIEMAP`/ioctl surface.
6. **Mount/error lifecycle:** remount/clean-unmount behavior, journal abort and
   remount-RO policy under injected metadata/device errors.
7. **Stress and performance:** fsx/fsstress, the declared xfstests set, long SMP
   mutation, journal-pressure fairness, fuzzing and §4.3 performance targets.

**Test milestones.** Tier 2 maintains a checked-in manifest containing the
exact generic/ext4 xfstests selected, excluded tests with a Tier 3 or
non-filesystem blocker, tool/kernel versions and the last result. In addition,
Linux-to-Tx and Tx-to-Linux image exchange, large-directory/extent crash cuts,
ACL/xattr round trips and direct/buffered overlap tests are mandatory.

**Phase-5 exit criterion.** Every §1.8 bullet has a passing vertical slice; the
xfstests manifest has no unexplained fail/timeout/not-run result; all produced
images pass `e2fsck -fn`; Tier 1 remains green.

**Dependencies.** Phase 4 complete.

---

## 6. Testing infrastructure

<!-- txdoc:TX-EXT4-PLAN-TESTING-INFRASTRUCTURE-1 -->

### 6.1 Host-runnable tests (phase 0)

<!-- txdoc:TX-EXT4-PLAN-HOST-RUNNABLE-TESTS-PHASE-0-1 -->

`tx-ext4-format` runs standard `cargo test`. Fixture generation is an explicit
tooling command under `tools/ext4/`, never an implicit `build.rs` side effect.
Generated images are copied before mutation; the manifest records generator
commands, feature masks, tool versions and hashes. CI may use committed fixtures
without e2fsprogs, but fixture promotion requires a host with current
`mke2fs`, `debugfs`, `dumpe2fs` and `e2fsck`.

### 6.2 Kernel integration tests (phases 1+)

<!-- txdoc:TX-EXT4-PLAN-KERNEL-INTEGRATION-TESTS-PHASES-1-1 -->

Run under qemu-system-riscv64 with virtio-blk. A test harness:

- Provides test images with known content.
- Boots the kernel with a test runner instead of init.
- Test runner exercises syscalls, collects results, exits qemu with a status code.
- CI runs this as a smoke test on every commit.

### 6.3 Crash testing (phase 4+)

<!-- txdoc:TX-EXT4-PLAN-CRASH-TESTING-PHASE-4-1 -->

A dedicated crash-test harness:

- Boots kernel with a stressor workload.
- At a random time in a random phase, kills qemu with SIGKILL.
- Remounts the image read-only in Linux; runs `e2fsck -n -f`.
- Boots tx-kernel on the same image; verifies expected state.
- Repeat 1000× nightly.

### 6.4 Interop testing (phase 4+)

<!-- txdoc:TX-EXT4-PLAN-INTEROP-TESTING-PHASE-4-1 -->

After any tx-ext4 unmount, the resulting image is mounted by Linux's ext4 driver and checked. Any divergence (Linux finds errors we don't, or vice versa) is a bug.

### 6.5 Profile and xfstests manifests

<!-- txdoc:TX-EXT4-PLAN-PROFILE-XFSTESTS-MANIFESTS-1 -->

Tier 1 and Tier 2 each maintain a machine-readable manifest. It records the
accepted feature masks and shape bounds, fixture hashes, Linux/e2fsprogs/
xfstests revisions, selected tests, results, exclusions and evidence paths.
Promotion rules are mechanical:

- Tier 1: all declared cases pass; no timeout, crash or not-run result.
- Tier 2: every selected case passes; every exclusion names a Tier 3 feature or
  a separately tracked kernel blocker.
- A tool or kernel version change invalidates the recorded promotion result
  until the affected fixture/differential set is rerun.
- `e2fsck -fn` is run offline on a copy. A harness that cannot obtain exclusive
  image access is a failed run, not a pass or skip.

---

## 7. Risk register

<!-- txdoc:TX-EXT4-PLAN-RISK-REGISTER-1 -->

| Risk | Phase | Mitigation |
|---|---|---|
| Direct pager and planner/runtime paths remain simultaneously reachable | 2-4 | Cut over callsite by callsite; static gate forbids production direct home writes before Tier 1 promotion. |
| JBD2 semantics subtly wrong; silent corruption survives `e2fsck` | 4 | Interop-test with Linux on every crash-test iteration. |
| Extent-tree/htree split logic bugs cause data loss | 5 | Keep Tier 1 shape bounds explicit; fuzz Tier 2 mutations + post-op `e2fsck`. |
| Metadata-PC lifetime or publication drifts into a second cache | 1-5 | Keep metadata private to the mount; no decoded per-inode cache or VFS live-node retain. |
| rsext4 format code has bugs we propagate | 0 | Validate all phase-0 output against `debugfs` ground truth (T0.*). |
| Linux behavior changes or prose omits an edge case | all | Pin oracle versions and use differential/xfstests evidence rather than prose inference. |
| Lock ordering across parent-parent-target in rename deadlocks under load | 2 | Enforce stable object-key ordering; write deadlock and concurrent-rename tests. |
| Performance soft targets unreachable with current PC/reactor design | 5 | Document the gap; propose design changes as a separate ADR rather than fixing inside tx-ext4. |

---

## 8. Resolved implementation decisions

<!-- txdoc:TX-EXT4-PLAN-OPEN-DECISIONS-DEFERRED-IMPLEMENTATION-START-1 -->

1. **Filesystem placement.** `tx-ext4` is a mounted kernel filesystem instance,
   not a daemon or independent service. Generic I/O-manager futures provide
   async execution.
2. **Metadata storage.** Metadata pages are mount-private PageBacked resources,
   not RNodes and not a decoded per-inode cache. A future storage optimization
   cannot change this ownership rule.
3. **Block I/O.** L5 emits neutral plans; L4/L6 own request lifecycle,
   scheduling, completion, barriers and direct-I/O coherency. Production ext4
   does not own a private block scheduler.
4. **Host execution.** `tx-ext4-format` stays executor-free. Host adapters may
   drive explicit test operations, but production `tx-ext4` remains free of
   Tokio and rsext4 runtime APIs.
5. **Error taxonomy.** The internal format layer distinguishes unsupported,
   corrupt and device-I/O failures. The syscall/mount boundary matches Linux:
   initial unsupported feature sets return `EINVAL`, an invalid RO-to-RW
   remount returns `EROFS`, operation-specific unsupported shapes return
   `EOPNOTSUPP`, integrity corruption returns `EUCLEAN` where Linux exposes
   `EFSCORRUPTED`, device failure returns `EIO`, and allocation exhaustion
   returns `ENOSPC`. A generation/read-set conflict retries or returns the
   operation's documented transient error before publication.
6. **Compatibility expansion.** New feature bits enter Tier 1/2 only with a
   capability-ledger row, deterministic fixture, Linux/e2fsprogs oracle and
   crash policy. Code presence or rsext4 support is insufficient.

---

## 9. Success criterion, compressed

<!-- txdoc:TX-EXT4-PLAN-SUCCESS-CRITERION-COMPRESSED-1 -->

**Tier 1:** tx-kernel boots from or mounts the pinned profile on
qemu-virtio-blk; Alpine/OSComp and a C compiler can create, mutate, fsync,
rename and execute files on it. After 1000 deterministic crash cuts, every
image is clean under `e2fsck -fn`, fsync-success data survives, and Linux can
mount the result RW. Production has one planner/runtime durability path.

**Tier 2:** the §1.8 surface works on mainstream supported Linux ext4 images,
both image-exchange directions pass, and the versioned xfstests manifest has no
unexplained failure, timeout or not-run entry. Features outside §1.8 are
rejected according to §1.6 rather than weakening the claim.
