# tx-ext4: Project Plan

<!-- txdoc:05-FILESYSTEM-TX-EXT4-PLAN-V1-2 -->

**Status.** v1.2 (2026-04-20). Draft plan for the ext4 filesystem backend for txKernel.

**Supersedes (v1.2 → v1.1).** Adds two design commitments to §1: (a) the stateless-per-inode rule — tx-ext4 holds no decoded per-inode state; POSIX-abstract metadata lives on VFS's RNode, ext4-specific fields are addressed as bytes in the inode-table PC and re-parsed on use; (b) a new §1.4 cache and reclaim policy that locks in v1 behavior (CLOCK reclaim for file pages phase 2, pinned metadata PCs, dentry-cache Tier 2 reclaim, periodic writeback phase 4, no swap), closing `PAGE_BACKED_v1.md §12.1` for v1 scope.

**Supersedes (v1.1 → v1).** Adds §3 "Interfaces" explicitly specifying the traits tx-ext4 implements (`FsPageBacking`, `FsOps`), the trait it consumes (`BlockDevice`), the value types it handles (`InodeMeta`, `DirCursor`, `DirEntry`, `Credential`, `FsObjectId`), the mount handshake (`MountInitContext` + `MetadataPcFactory` + `MountOutput`), and an explicit import allowlist/denylist enforceable by grep-lint. Downstream sections renumbered (§4+). Content of final goals, phase breakdown, and test milestones unchanged.

**Purpose.** Define the deliverables, phase structure, and test milestones for implementing an async, coroutine-compatible ext4 filesystem as the first persistent-FS backend behind VFS and PageContainer. rsext4 ([Starry-OS/rsext4](https://github.com/Starry-OS/rsext4)) is included as a git submodule and referenced during porting of on-disk format code; no rsext4 code is used at runtime.

**Audience.** Implementers working on tx-ext4, reviewers auditing the backend boundary, agents extending the filesystem in future phases.

**Companion documents.**

- [`VFS_CHECKS_V2.1.md`](VFS_CHECKS_V2.1.md) and [`MOUNT_v1.md`](MOUNT_v1.md) — VFS ownership boundary, `FsOps` and `FsPageBacking` consumer side.
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) — `FsPageBacking` trait, `PageContainer` model.
- [`VM_v1_2.md`](../03_memory-vm/VM_v1_2.md) — fault handler and `FsPageBacking::fetch_page` integration.
- [`CONCEPTS_v4.md §3.5`](../00_meta-framework/CONCEPTS_v4.md) — factoring/topology axes used throughout this plan.
- [`STEP_MODEL_v1.md`](../02_execution/STEP_MODEL_v1.md) — `StepOutcome` contract; all async methods return step outcomes.

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
- ext4 on-disk format support sufficient for Linux-2.6-era userland: regular files, directories (linear and htree), symlinks, hard links, extent-based block mapping, inode/block bitmap allocation.
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
- POSIX ACLs (stick to mode bits).

### 1.3 Non-negotiable design constraints

<!-- txdoc:TX-EXT4-PLAN-NON-NEGOTIABLE-DESIGN-CONSTRAINTS-1 -->

- **No runtime dependence on rsext4.** rsext4 is a reference and the on-disk format is ported from it; the runtime library is ours.
- **No rsext4-style multi-level cache.** `PageContainer` is the cache.
- **No synchronous blocking.** Every I/O call yields a `StepOutcome::Blocked` and resumes when the block device completes. The block device trait is async.
- **No `&mut self` threading.** Concurrent operations on the same `Ext4FsInstance` must be admissible. State mutation goes through PC-level publication discipline (ARCH-5) and substrate primitives.
- **No `Cap<RNode>` held inside tx-ext4.** All operations key on `fs_object_id`.
- **tx-ext4 is stateless per persistent object.** All per-inode state lives in one of two places: (a) POSIX-abstract decoded metadata (`InodeMeta`) on VFS's RNode, serialized to/from on-disk records by tx-ext4; (b) ext4-specific fields (extent tree root, htree info, flags beyond POSIX) addressed as *bytes* in the inode-table PC, re-parsed on each use. **tx-ext4 does not maintain a per-inode decoded cache of ext4-specific fields.** The only persistent state tx-ext4 holds is mount-level: block device handle, superblock mirror, journal state, metadata PC handles. This is stronger than "no Cap<RNode>" — it closes off a second decoded-cache coherence domain. If profiling later shows per-inode re-parse is a measurable cost, a bounded decoded-extent-root cache keyed by `(fs_object_id, modification_counter)` may be added as a phase-5 optimization; the invalidation key ensures coherence across rematerialization.

### 1.4 Cache and reclaim policy

<!-- txdoc:TX-EXT4-PLAN-CACHE-RECLAIM-POLICY-1 -->

This section locks in the v1 reclaim policy for VFS- and filesystem-adjacent caches, closing the open question left in `PAGE_BACKED_v1.md §12.1` for v1 scope. Cache tiers are introduced phase-by-phase.

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
| Journal transactions | Pre-commit buffered metadata mutations | Bounded by journal size; commit releases |

**v1 policy by phase.**

- **Phase 1 (read-only mount).** No reclaim. ENOMEM on frame allocation fails cleanly up the stack. Clean file pages accumulate without eviction; acceptable because read-only workloads are bounded and bring-up doesn't need reclaim correctness.

- **Phase 2 (writes, no journal).** Pressure-driven CLOCK reclaim for file pages only:
  - Each `FrameMeta` gains a `used` bit (we have space in the existing packed layout).
  - `step_read`/`step_write` / fault handler set `used` when touching a frame.
  - On `alloc_frame` failure, the caller triggers a reclaim pass: CLOCK sweep over `FrameMeta` array; a candidate frame is `cache_ref == 1 ∧ map_count == 0 ∧ !flags.dirty ∧ !used`. Candidates are dropped from their PC page index and freed. If `used`, the bit is cleared and the sweep moves on.
  - One pass is bounded; if it frees nothing, the caller returns ENOMEM.
  - Anon PCs never produce candidates (cache_ref represents pinning, not caching).
  - Metadata PCs never produce candidates (marked with a per-PC `no_reclaim` flag).
  - Dirty pages never produce candidates (must be written first; writeback comes phase 4).

  The CLOCK hand is a per-mount or per-NUMA-node cursor (v1: one global cursor; refine later).

- **Phase 2 (dentry cache reclaim).** Dentries follow the same Tier-2 shape: each DEntry has a `used` bit set on walker traversal, cleared on reclaim sweep. Candidates are `cache_ref == 1 ∧ !used ∧ !negative_dentry_in_active_use`. Reclaim pass triggers when the dentry zone exceeds a high-water mark (configurable; default = 75% of zone capacity). Note that dentries pinned by active walkers (via witness IdentRefs under an epoch guard) are not candidates by construction — `cache_ref > 1` excludes them.

- **Phase 4 (journal).** Periodic writeback for dirty file pages, implemented as a reactor-spawned task. Parameters:
  - Timer-driven sweep every 5 seconds OR when dirty-byte count exceeds 10% of total memory, whichever first.
  - Sweep is bounded (process at most N pages per pass; N configurable, default 1024) to avoid long latency tails.
  - Dirty metadata pages are not flushed by this daemon — they go through journal commit.
  - `fsync` short-circuits the timer: synchronous walk of the target PC's dirty frames + force-commit any in-flight transaction containing them.

**Explicit non-policies (v1).**

- **No swap.** Anon PCs are pinned until explicit teardown. This has been a standing commitment (`PAGE_SUBSTRATE_v1.md` §8).
- **No metadata PC eviction.** Metadata is small (tens of MB for typical ext4 sizes); pinning it keeps tx-ext4 hot paths fast. Revisit if profiling shows pressure.
- **No per-CPU slab caches.** Slab returns frames to the frame allocator when a slab is fully free; no high-water mark.
- **No reverse-mapping machinery.** Reclaim is purely forward (start from FrameMeta, check PC membership); no walking from frame to mappers. This is what `map_count == 0` in the candidate predicate buys us — it tells us no mapper holds the frame without needing to find who.
- **No per-mount reclaim priority.** All file-page PCs compete on equal terms for memory.
- **No dentry-cache periodic pruning.** Reclaim is on-demand at high-water only.

**Soft targets (not blockers for v1).**

- Reclaim sweep latency under 1ms for the CLOCK hand on a 64GB-frame system (bounded scan, cache-friendly access).
- Writeback daemon pause under 10ms per sweep window.
- No allocation failure under steady-state workloads within 80% of memory.

**Cross-reference and closure.**

This supersedes the "deferred to a reclaim-specific doc" language in `PAGE_BACKED_v1.md §12.1` *for v1 scope*. A future `RECLAIM.md` may refine this with per-CPU accounting, reverse-mapping, and NUMA-aware cursors. Scope creep is explicitly rejected for v1: we ship with CLOCK + timer-driven writeback + pinned metadata, and nothing more.

**What this buys us.**

- Memory pressure produces graceful degradation instead of ENOMEM under any reasonable load.
- No LRU list overhead (CLOCK needs one bit per frame, no linked-list maintenance).
- Reclaim is fully synchronous under pressure (no background daemon required for correctness; writeback daemon in phase 4 is a latency optimization, not a correctness primitive).
- Matches the no-swap discipline: memory pressure affects only reclaimable pages (clean file, evictable dentry), never anon.

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

| Crate | Types permitted |
|---|---|
| `tx-fnd::types` | `Errno`, `PageSize`, numeric newtypes |
| `tx-fnd::step` | `StepOutcome<T>`, `Guard`, `Channel`, `Mask`, `Blocked`, `Done`, `Advanced` |
| `tx-fnd::sync` | `AtomicU64`, `AtomicU32` (for superblock mirror counters) |
| `tx-fnd::block` | `BlockDevice` trait, `PhysicalBlockNumber`, `BlockReadReq`, `BlockWriteReq` |
| `tx-vm::frame` | `Frame`, `Cap<Frame>`, `FrameMeta` access for dirty/io-locked bits |
| `tx-vm::page_container` | `Cap<PageContainer>`, `PageContainer` (as opaque), `step_read`, `step_write` against PCs |
| `tx-vm::page_backed` | `FsPageBacking` trait (implemented by tx-ext4) |
| `tx-vfs::fs_ops` | `FsOps` trait (implemented by tx-ext4), `InodeMeta`, `DirEntry`, `DirCursor`, `Credential`, `FsObjectId` |
| `tx-vfs::mount` | `MountId`, `MountInitContext` (for mount bringup handshake) |

**Forbidden imports** (enforced by grep-lint in CI, per T1.7):

- `tx-vfs::rnode` — no `RNode`, `Cap<RNode>`, `Weak<RNode>`, `IdentRef<RNode>`.
- `tx-vfs::dentry` — no `DEntry` types.
- `tx-vfs::open_file` — no `OpenFile` types.
- `tx-vfs::walker` — no walker state types.
- `tx-proc::*` — no process/thread entities.

The asymmetry: tx-ext4 depends on VM (for PCs/Frames) but not on VFS live-node types. VFS depends on tx-ext4's trait implementations but constructs all RNode state itself.

### 3.2 `FsPageBacking` — trait implemented by tx-ext4

<!-- txdoc:TX-EXT4-PLAN-FSPAGEBACKING-TRAIT-IMPLEMENTED-TX-EXT4-1 -->

Defined in `tx-vm::page_backed`; this is tx-ext4's byte-pager role. The trait was sketched in [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §6; the definitive async signatures tx-ext4 implements:

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

## 4. Final goals (v1 acceptance)

<!-- txdoc:TX-EXT4-PLAN-FINAL-GOALS-V1-ACCEPTANCE-1 -->

### 4.1 Functional goals

<!-- txdoc:TX-EXT4-PLAN-FUNCTIONAL-GOALS-1 -->

1. **Mount.** Mount a pre-populated 4 KiB-block ext4 image with or without an unreplayed journal. Replay on mount if dirty. Reject filesystems with unsupported features (with clear errnos).
2. **Read path.** `read(2)` on any regular file in the filesystem returns correct data. Works through VFS walker → RNode PageBacked(File) PC → `FsPageBacking::fetch_page` → extent walk → block device read → frame installed. Yields on disk I/O.
3. **Directory traversal.** `readdir(2)`, `getdents(2)` work over both linear and htree directories. `lookup` walks through htree.
4. **Write path.** `write(2)`, `truncate(2)`, `ftruncate(2)` correctly mutate file data. Sizes grow through the extent allocator. `flush_page` produces correct on-disk content.
5. **Namespace mutations.** `creat`, `open(O_CREAT)`, `unlink`, `rmdir`, `mkdir`, `rename`, `link`, `symlink` all work and leave the filesystem consistent. Unlinked-but-open holds correctly via `destroy_inode` triggered by payload-liveness loss.
6. **fsync.** `fsync(2)` flushes data pages and commits any pending journal transaction containing the file's metadata.
7. **Journal correctness.** Crash (simulated via hard-killing the emulator) at arbitrary points during active mutations, remount, run `e2fsck -n`: filesystem reported as clean, no corruption.
8. **Boot-level scenarios.** Boot tx-kernel with an ext4 root, run busybox, run a C compiler on a source file to an ext4 output.

### 4.2 Integration goals

<!-- txdoc:TX-EXT4-PLAN-INTEGRATION-GOALS-1 -->

1. **VFS walker consumes tx-ext4.** The walker's `NeedIO` resume path correctly delegates to `FsOps::lookup` and `FsOps::load_inode_meta` via the MountPayload coherence-index find-or-create protocol.
2. **Page fault handler consumes tx-ext4.** User-space page faults on file-backed mappings drive through `FsPageBacking::fetch_page` with correct `StepOutcome::Blocked` yield behavior.
3. **No tx-ext4 reference to `Cap<RNode>`.** Verified by `grep` — the tx-ext4 crate does not import `RNode` or hold it in any type.
4. **No rsext4 code in runtime build.** Verified by `cargo tree` — rsext4 is not a compile-time or runtime dependency.

### 4.3 Performance goals (soft targets, not blockers)

<!-- txdoc:TX-EXT4-PLAN-PERFORMANCE-GOALS-SOFT-TARGETS-NOT-BLOCKERS-1 -->

- Sequential read throughput within 2× of the raw block device throughput.
- No heap allocation on the syscall hot path (prefault discipline, per VM spec).
- Shootdown batching per step commit (inherited from page substrate).

Performance is explicitly secondary to correctness for v1.

---

## 5. Phase breakdown and test milestones

<!-- txdoc:TX-EXT4-PLAN-PHASE-BREAKDOWN-TEST-MILESTONES-1 -->

Each phase has a self-contained deliverable and explicit test milestones. Phases are sequenced but phase 0 runs in parallel with phase 1 dependencies maturing elsewhere.

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
- Extent tree splits and merges (these require allocation context; phase 2).
- Htree node splits (phase 2).

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
- Metadata PC population: the metadata PCs are synthetic (option A from the design discussion). Their `PageContainerKind` is a new variant `Metadata { region: MetaRegion }` OR — simpler — `Anon` with backing populated on-demand by an internal helper that does block-device reads. **Decision deferred to implementation start**; the observable surface is identical either way (PCs are `Cap<PageContainer>`).
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

### Phase 2 — Writes without journal

<!-- txdoc:TX-EXT4-PLAN-PHASE-2-WRITES-WITHOUT-JOURNAL-1 -->

**Scope.**

- `FsPageBacking::flush_page` for Data PCs: extent walk, write to physical block.
- `FsPageBacking::truncate`: shrink extent tree, free blocks (updates bitmap PCs), update inode size.
- `FsPageBacking::fsync`: flush dirty data pages of the target PC; metadata flush is a no-op in this phase (no journal yet).
- `FsOps::create_inode`: bitmap scan, allocate inode, write inode record to inode-table PC (marking that PC dirty).
- `FsOps::serialize_inode_meta`: write back updated `InodeMeta` to the inode-table PC.
- `FsOps::destroy_inode`: free inode; free all extent-mapped blocks; clear the inode record.
- `FsOps::unlink`: remove directory entry from parent directory's data PC; decrement `InodeMeta.nlinks` (via `serialize_inode_meta`); if `nlinks` reaches zero and no OpenFile pins exist, trigger `destroy_inode` via the VFS-side script (not from within tx-ext4).
- `FsOps::mkdir`, `rmdir`, `rename`, `link`, `symlink`: equivalent; all go through directory-block mutation + inode bitmap/allocation + `serialize_inode_meta`.
- Extent tree splits and merges (for writes that grow files past a leaf's capacity; for truncates that split an extent).
- Htree splits (for directories that outgrow an htree leaf).

**Crash behavior.** Crashes corrupt the filesystem. Acceptable for phase 2; remount requires `e2fsck` until phase 4 journal lands.

**Test milestones.**

- **T2.1 Create, write, read back.** `touch`, `echo hello > f`, `cat f` — roundtrip correct.
- **T2.2 Large write.** Write a 500 MB file, read it back, `cmp`. Verify extent tree has multiple leaves.
- **T2.3 Truncate down and up.** Create 10 MB file; `truncate -s 1000 f`; `truncate -s 10M f`; verify old content beyond 1000 is zero on re-read (sparse-file semantics).
- **T2.4 Delete and reclaim.** Fill filesystem to 90%; delete half the files; `df` shows reclaimed space; new files succeed.
- **T2.5 Concurrent writes.** Two processes writing to different files, different directories. No interference.
- **T2.6 Rename across directories.** `mv a/foo b/bar` with both parents holding many entries (force htree). Verify namespace mutation correct in both.
- **T2.7 Unlinked but open.** Process A opens /tmp/f, process B unlinks /tmp/f. Process A continues reading and writing; contents correct. Close A's fd; verify inode actually reclaimed (ext4 free-inode count increases).
- **T2.8 Hard links.** `ln a b`; mutate through either; `stat` shows same inode; `unlink a` leaves b valid; `unlink b` actually frees.
- **T2.9 Offline `e2fsck` after clean unmount.** After clean unmount, `e2fsck -n` reports no errors.

**Phase-2 exit criterion.** All T2.* pass. Booting tx-kernel, compiling a small C program to ext4, rebooting (clean unmount), reading the binary back all work.

**Estimated effort.** 4–6 weeks. The extent-tree and htree split/merge paths are the hardest part.

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

- `JournalState` inside `MountPayload`: current transaction, outstanding committed-but-uncheckpointed transactions, journal PC handles.
- Mount-time replay: scan journal, identify committed transactions (those with valid commit records and matching checksums), replay their metadata writes directly. Use phase 0 JBD2 record parsing and phase 1 PC primitives. Replay is bounded work using only phases 0–2.
- Transaction API inside tx-ext4: operations that mutate metadata start by attaching to the current transaction. Mutations buffer in the transaction, not directly in the metadata PCs.
- Commit step machine: one step function per commit phase.
  - `step_data_wait`: wait for all data-page writes of ordered-mode files participating in this transaction to complete. Returns `Blocked` on outstanding I/O.
  - `step_metadata_write`: write the transaction's metadata blocks to the journal. Returns `Blocked` on journal I/O.
  - `step_commit_record`: write the commit record with checksum. Returns `Blocked` on journal I/O.
  - `step_checkpoint`: later, write the metadata from the journal back to the metadata PCs' home locations.
- Ordering rules:
  - Data writes (for files in this transaction) must reach disk before the commit record.
  - The commit record must reach disk before metadata is checkpointed to its home location.
  - Checkpoint can happen arbitrarily later; transactions accumulate in the journal until their home-location writes are confirmed durable.
- Flush and force-commit paths for `fsync`.

**Test milestones.**

- **T4.1 Mount-time replay.** Deliberately kill qemu after a write but before journal checkpoint; remount; verify file visible with post-write content.
- **T4.2 Crash and e2fsck -n.** Random kill during sustained write workload (1000 iterations); remount; `e2fsck -n` reports clean; no silent corruption detected by secondary integrity checks (file-data CRCs).
- **T4.3 fsync durability.** Process A writes then fsyncs; crash; remount; A's data is present.
- **T4.4 No fsync means can lose.** Process writes but does not fsync; crash; may or may not see the data but filesystem is consistent.
- **T4.5 Concurrent transactions.** Two processes mutating different inodes. Both transactions progress; neither blocks the other unnecessarily; commits serialize correctly.
- **T4.6 Journal wrap.** Fill the journal; transactions block on checkpoint progress; verify forward progress continues.
- **T4.7 Interop with `e2fsprogs`.** After clean unmount, mount the same image on Linux, run `fsck.ext4 -f`; Linux reports clean; read a file through Linux; content correct.

**Phase-4 exit criterion.** All T4.* pass. Power-cut torture runs for 1000 iterations without corruption.

**Estimated effort.** 4–6 weeks.

**Dependencies.** Phase 2 complete. Reactor stable enough for the commit step machine's multi-phase wait sequencing.

---

### Phase 5 — Hard cases and hardening

<!-- txdoc:TX-EXT4-PLAN-PHASE-5-HARD-CASES-HARDENING-1 -->

**Scope.**

- Edge cases in extent-tree split/merge (multi-level, very fragmented).
- Htree rebalancing on directory shrinkage.
- Inode-table block allocation when filesystem is full.
- Checksum validation at mount and rejection of corrupted structures.
- Large file support (>4 GiB; requires 64-bit extent features).
- xattr support if we determine userland needs it.
- Stress testing and fuzzing.
- Performance tuning to hit soft targets from §4.3.

No fixed test milestones; this is an open-ended hardening phase. Exit when the filesystem passes the same stress tests Linux's ext4 passes (fsstress, fsx, xfstests — subset applicable to our feature coverage).

**Dependencies.** Phase 4 complete.

---

## 6. Testing infrastructure

<!-- txdoc:TX-EXT4-PLAN-TESTING-INFRASTRUCTURE-1 -->

### 6.1 Host-runnable tests (phase 0)

<!-- txdoc:TX-EXT4-PLAN-HOST-RUNNABLE-TESTS-PHASE-0-1 -->

`tx-ext4-format` runs standard `cargo test`. Test image generation uses a `build.rs` that invokes `mkfs.ext4` and `debugfs` at test time (skipped in CI environments without those tools; a pre-generated set of images is committed for reproducibility).

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

---

## 7. Risk register

<!-- txdoc:TX-EXT4-PLAN-RISK-REGISTER-1 -->

| Risk | Phase | Mitigation |
|---|---|---|
| Reactor I/O completion wire not ready when phase 1 starts | 1 | Build sync-stub block device; swap at phase 3. |
| JBD2 semantics subtly wrong; silent corruption survives `e2fsck` | 4 | Interop-test with Linux on every crash-test iteration. |
| Extent-tree/htree split logic bugs cause data loss | 2 | Fuzz with random mutation sequences + post-op `e2fsck`. |
| Metadata PC (option A) lifecycle interactions surprise us | 1 | Keep the metadata-PC surface narrow; if problems arise, revisit design before phase 2. |
| rsext4 format code has bugs we propagate | 0 | Validate all phase-0 output against `debugfs` ground truth (T0.*). |
| Lock ordering across parent-parent-target in rename deadlocks under load | 2 | Enforce address-ordered locking; write a deadlock-detection test. |
| Performance soft targets unreachable with current PC/reactor design | 5 | Document the gap; propose design changes as a separate ADR rather than fixing inside tx-ext4. |

---

## 8. Open decisions deferred to implementation start

<!-- txdoc:TX-EXT4-PLAN-OPEN-DECISIONS-DEFERRED-IMPLEMENTATION-START-1 -->

1. **Metadata PC variant.** Option A commits to "metadata PCs, not RNodes." Still to decide: is this a new `PageContainerKind::Metadata` variant, or reuse `Anon` with an internal populate-on-fetch helper? Decision at phase 1 kickoff, whichever is simpler; doesn't affect the observable interface.
2. **Block device trait shape.** Does the block device expose single-block or range reads? What about ordering barriers for journal writes? Decision before phase 1 starts; likely influenced by what virtio-blk provides.
3. **Async runtime shape for host tests.** Tokio? A minimal custom executor? Decision at phase 0 (for any tests that simulate async I/O on host).
4. **Error taxonomy.** Map ext4 on-disk errors (bad checksums, out-of-bounds fields) to which errnos at the trait boundary? Decision before phase 2.

---

## 9. Success criterion, compressed

<!-- txdoc:TX-EXT4-PLAN-SUCCESS-CRITERION-COMPRESSED-1 -->

tx-kernel boots on qemu-virtio-blk with an ext4 rootfs. A C compiler built on Linux can be run in tx-kernel to compile a C source to an ELF on the same ext4. The result of 1000 random-kill crash tests is: filesystem clean per `e2fsck -n`, file contents match pre-crash fsync expectations, no silent corruption detected through the Linux interop check. This is Linux-2.6-era parity for the filesystem layer.
