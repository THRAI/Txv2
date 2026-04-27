# Block-Device Filesystem (bdev-fs)

<!-- txdoc:05-FILESYSTEM-BDEV-FS -->

**Status.** v1 (2026-04-24). Draft.

**Purpose.** Specify `bdev-fs`, the pseudo-filesystem that turns registered `BlockDeviceRegistration`s into page-backed RNodes so that `/dev/vda`, `/dev/sda`, `/dev/mmcblk0p1`, etc. have file semantics — page cache, mmap, uniform `read` / `write` — without the device subsystem providing any file-ops machinery of its own. bdev-fs is the route-C target from [`DEVICE.md §5.3`](../06_devices/DEVICE.md); it is to block devices what `TtyIdentity` is to character-device TTYs, except that bdev-fs leans almost entirely on existing PAGE_BACKED infrastructure and adds only the bytes ↔ blocks translation.

**Audience.** Filesystem implementers who need to mount-on-block-device (tx-ext4 in particular), anyone opening a block device from userspace, reviewers auditing the block-device path for cache coherence.

**Companion documents.**

- [`DEVICE.md`](../06_devices/DEVICE.md) §2.2 (tier 2), §4 (device classes), §5.3 (Route C), §7 (init phase 5), §10.2 (`io_complete_wire` attachment), §11 (per-target block-device inventory).
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §2 (`RNodeBacking`), §3 (`PageContainer`), §4 (PC lifecycle), §5 (`FsPageBacking` trait). bdev-fs is a vanilla consumer of this trait.
- [`TX_EXT4_PLAN.md`](TX_EXT4_PLAN_v1_2.md) §3.2 (`BlockDevice` trait). bdev-fs's `FsPageBacking` impl is what sits between tx-ext4 (or any block-device-consuming filesystem) and the driver's `BlockDeviceOps`.
- [`object_model.md`](../00_meta-framework/object_model_v2.md) §3 (entities), §8.1.1 (bifurcation). bdev-fs's MountPayload follows the standard Mount Identity/Payload split; no new entity classes are introduced.
- [`INVARIANTS_v4.md`](../00_meta-framework/INVARIANTS_v4.md) — ARCH-5 (publication rule), PRED-7 (race degradation). Partition-table mutation is rejected until phase 5 of tx-ext4; v1 partitions are compile-time or mount-time only.
- [`CONCEPTS_v4.md`](../00_meta-framework/CONCEPTS_v4.md) §8 (authoritative bindings and derived materializations). bdev-fs's `devt → Cap<PageContainer>` coherence index is an authoritative binding in the same sense as a persistent filesystem's `fs_object_id → RNode` index.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) — four-module layout (§6).
- [`VFS_CHECKS_V2.1.md`](VFS_CHECKS_V2.1.md) — `MountPayload` structure, `FsOps` consumer side.

### Zone-derived type policy

<!-- txdoc:BDEV-FS-ZONE-DERIVED-TYPE-POLICY-1 -->

bdev-fs introduces no new semantic entity classes. It uses role-shaped evidence
owned by Mount, Device, and PageBacked:

| bdev-fs declaration | Public handle | Reclamation role |
|---|---|---|
| bdev-fs instance | `MountPayload` evidence from MOUNT | filesystem-instance payload, not a new entity family |
| block-device registration | `&'static BlockDeviceRegistration` | tier-2 static fact, no zone |
| `devt -> PageContainer` coherence map | `Weak<PageContainer>` with upgrade on demand | stale-tolerant index hint |
| opened block-device content | `Cap<PageContainer>` | page-backed content entity |
| partition table rows | values on mount payload | binding values, no zone |

The filesystem never selects `Zone<T, Policy>`. It maps static device facts to
page-backed entities through Mount/PageBacked evidence.

---

## 1. Motivation

<!-- txdoc:BDEV-FS-MOTIVATION-1 -->

Linux exposes block devices via two layered mechanisms: a character-like device-node path (`/dev/sda`, implemented by `block_device_operations`) and the block-layer page cache (`bdev->bd_inode`, implemented as a magic inode that plumbs through the block layer for reads/writes/mmap). The two are connected but conceptually distinct: the device node carries the `file_operations` that gets injected at open, and the inode carries the page cache.

PAGE_BACKED has already unified all page-indexed content behind one abstraction: a `PageContainer` wrapped by an RNode's `PageBacked` backing, dispatched via the narrow `FsPageBacking` trait on the owning filesystem. To slot block devices into that abstraction, we need a **filesystem instance** — something with a `MountPayload`, an `FsOps` impl, and an `FsPageBacking` impl — whose "files" are the registered block devices. That is bdev-fs.

This is not a full filesystem in the Linux sense. It has:

- **One directory** (the root), populated by enumerating the tier-2 block-device registrations and the partition table(s) parsed off them.
- **One kind of file** (a block device, or a partition slice of a block device).
- **No writes to the namespace** — `mknod`, `mkdir`, `unlink` all return EROFS. The "files" exist because devices were registered; there is no userspace path to create new ones.

Its complexity is entirely in the page cache path: translating a file offset into a block-device LBA, dispatching async I/O through the driver's `BlockDeviceOps`, and keeping the `devt → PC` map coherent so that `mount /dev/vda1` and `open("/dev/vda1", O_RDONLY)` see the same page cache.

---

## 2. Shape

<!-- txdoc:BDEV-FS-SHAPE-1 -->

### 2.1 Objects

<!-- txdoc:BDEV-FS-OBJECTS-1 -->

bdev-fs contributes one `MountPayload` to the Mount subsystem, and zero new entity types beyond that. All state lives on the MountPayload or in PCs the MountPayload holds Caps to.

```rust
// frame/bdev_fs/structure/mount.rs
pub struct BdevFsMountPayload {
    meta: SlotMeta,

    /// Coherence index: devt → Cap<PageContainer>. Multiple opens of the
    /// same block device (or the same partition) share this PC so that
    /// their page caches coincide.
    pub pc_by_devt: RwLock<BTreeMap<DevT, Weak<PageContainer>>>,

    /// Partition table, populated at mount time by parsing each block
    /// device's first MBR/GPT sector(s). devt → PartitionTable.
    pub partitions: RwLock<BTreeMap<DevT /* parent */, PartitionTable>>,

    /// Back-reference to the block-device registry (a static slice of
    /// &'static BlockDeviceRegistration) for enumeration.
    pub registrations: &'static [&'static BlockDeviceRegistration],
}

pub struct PartitionTable {
    pub entries: SmallVec<PartitionEntry, 8>,
}

pub struct PartitionEntry {
    pub child_devt: DevT,         // the partition's devt (e.g. /dev/vda1)
    pub name_suffix: u8,          // index for naming: "<parent>p<suffix>" or "<parent><suffix>"
    pub start_lba: u64,
    pub len_lba: u64,
}
```

### 2.2 fs_object_id conventions

<!-- txdoc:BDEV-FS-FS-OBJECT-ID-CONVENTIONS-1 -->

In bdev-fs, `FsObjectId` is the block device's `DevT` packed into the `FsObjectId`'s u64:

```
FsObjectId = (class_tag: u16 << 48) | (devt: u48)
```

The `class_tag` distinguishes whole-device entries from partition entries at the FsOps level (they resolve differently for partition-table parsing), though both kinds dispatch identically at FsPageBacking level.

---

## 3. FsOps implementation

<!-- txdoc:BDEV-FS-FSOPS-IMPLEMENTATION-1 -->

bdev-fs's `FsOps` is mostly restrictive. It serves two purposes: enumerate the block-device registry (for `readdir`), and translate names to fs_object_ids (for `lookup`). Everything mutational returns `EROFS`.

```rust
// frame/bdev_fs/fs_ops.rs
impl FsOps for BdevFsMountPayload {
    fn lookup(&self, parent: FsObjectId, name: &[u8], guard: &Guard)
        -> StepOutcome<FsObjectId>
    {
        if parent != ROOT { return Err(ENOENT); }

        // Match "<base>" — whole block device.
        for reg in self.registrations {
            if name == reg.name.as_bytes() {
                return Done(FsObjectId::whole(reg.devt));
            }
        }

        // Match "<base><p>N" or "<base>N" — partition.
        if let Some((base, suffix)) = parse_partition_name(name) {
            for reg in self.registrations {
                if base == reg.name.as_bytes() {
                    let parts = self.partitions.read(guard);
                    if let Some(tbl) = parts.get(&reg.devt) {
                        if let Some(e) = tbl.entries.iter().find(|e| e.name_suffix == suffix) {
                            return Done(FsObjectId::partition(e.child_devt));
                        }
                    }
                }
            }
        }

        Err(ENOENT)
    }

    fn load_inode_meta(&self, id: FsObjectId, _guard: &Guard)
        -> StepOutcome<InodeMeta>
    {
        let (devt, size) = self.size_for(id)?;
        Done(InodeMeta {
            mode: S_IFBLK | 0o660,
            uid: 0, gid: 0,          // root:disk, conceptually
            size,                     // bytes: block_count * block_size
            nlinks: 1,
            ..InodeMeta::default()
        })
    }

    fn readdir(&self, parent: FsObjectId, cursor: DirCursor, _guard: &Guard)
        -> StepOutcome<Option<(DirEntry, DirCursor)>>
    {
        if parent != ROOT { return Err(ENOTDIR); }
        // Emit: for each registration, the whole-device entry, then each
        // of its partitions. Cursor encodes (reg_idx, part_idx).
        // ...
    }

    // All namespace mutations return EROFS. Userspace cannot create,
    // remove, or rename entries in bdev-fs.
    fn create_inode(&self, _p: FsObjectId, _n: &[u8], _m: u16, _c: &Credential, _g: &Guard)
        -> StepOutcome<(FsObjectId, InodeMeta)> { Err(EROFS) }
    fn unlink(&self, _p: FsObjectId, _n: &[u8], _t: FsObjectId, _g: &Guard)
        -> StepOutcome<()> { Err(EROFS) }
    fn rename(&self, _op: FsObjectId, _on: &[u8], _np: FsObjectId, _nn: &[u8], _g: &Guard)
        -> StepOutcome<()> { Err(EROFS) }
    fn link(&self, _p: FsObjectId, _n: &[u8], _t: FsObjectId, _g: &Guard)
        -> StepOutcome<()> { Err(EROFS) }
    fn mkdir(&self, _p: FsObjectId, _n: &[u8], _m: u16, _c: &Credential, _g: &Guard)
        -> StepOutcome<(FsObjectId, InodeMeta)> { Err(EROFS) }
    fn rmdir(&self, _p: FsObjectId, _n: &[u8], _t: FsObjectId, _g: &Guard)
        -> StepOutcome<()> { Err(EROFS) }
    fn symlink(&self, _p: FsObjectId, _n: &[u8], _l: &[u8], _c: &Credential, _g: &Guard)
        -> StepOutcome<(FsObjectId, InodeMeta)> { Err(EROFS) }
    fn destroy_inode(&self, _id: FsObjectId, _g: &Guard) -> StepOutcome<()> {
        // Unreachable: RNodes backed by bdev-fs never reach payload-death
        // via unlink; registrations are &'static and do not disappear.
        Done(())
    }
    // serialize_inode_meta is a no-op (no mutable metadata to persist).
    fn serialize_inode_meta(&self, _id: FsObjectId, _m: &InodeMeta, _g: &Guard)
        -> StepOutcome<()> { Done(()) }
}
```

Size computation (`size_for`) for a whole device uses `ops.total_blocks() * ops.block_size()`. For a partition it uses `entry.len_lba * ops.block_size()` of the parent.

---

## 4. FsPageBacking implementation

<!-- txdoc:BDEV-FS-FSPAGEBACKING-IMPLEMENTATION-1 -->

This is the substance. `FsPageBacking` is the PAGE_BACKED trait that translates `(fs_object_id, offset)` page requests into backing-storage operations.

```rust
// frame/bdev_fs/page_backing.rs
impl FsPageBacking for BdevFsMountPayload {
    fn fetch_page(
        &self,
        id: FsObjectId,
        offset: u64,
        guard: &Guard,
    ) -> StepOutcome<Frame> {
        let (reg, start_lba, len_lba) = self.resolve(id, guard)?;
        let bs = (reg.ops.block_size)(reg) as u64;
        let blocks_per_page = PAGE_SIZE as u64 / bs;                  // typically 8
        let page_lba = start_lba + (offset / PAGE_SIZE as u64) * blocks_per_page;

        // Bounds check against partition length (whole-device: len_lba is total).
        if (offset / PAGE_SIZE as u64) * blocks_per_page + blocks_per_page > len_lba {
            return Err(EIO);
        }

        let frame = frame::alloc_zeroed()?;
        match (reg.ops.step_read_blocks)(reg, page_lba, blocks_per_page as u32,
                                         core::slice::from_mut(&mut frame.as_mut())) {
            StepOutcome::Done(()) => Done(frame),
            StepOutcome::Blocked(ch, mask) => {
                // Driver yielded; reactor will resume us on the
                // registration's io_complete_wire.
                // On resume, step is re-entered; we re-validate the PC
                // coherence state per ARCH-5 and continue.
                frame::free(frame);
                Blocked(ch, mask)
            }
            StepOutcome::AdvancedThenBlocked(_, ch, mask) => Blocked(ch, mask),
            StepOutcome::Err(e) => { frame::free(frame); Err(e) }
            StepOutcome::Advanced(_) => unreachable!("block read is terminal"),
        }
    }

    fn flush_page(
        &self,
        id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &Guard,
    ) -> StepOutcome<()> {
        let (reg, start_lba, len_lba) = self.resolve(id, guard)?;
        let bs = (reg.ops.block_size)(reg) as u64;
        let blocks_per_page = PAGE_SIZE as u64 / bs;
        let page_lba = start_lba + (offset / PAGE_SIZE as u64) * blocks_per_page;
        if (offset / PAGE_SIZE as u64) * blocks_per_page + blocks_per_page > len_lba {
            return Err(EIO);
        }
        (reg.ops.step_write_blocks)(reg, page_lba, blocks_per_page as u32,
                                    core::slice::from_ref(frame))
    }

    fn truncate(&self, _id: FsObjectId, _new_size: u64, _g: &Guard)
        -> StepOutcome<()>
    {
        // Block devices have fixed size. Truncate never extends / shrinks.
        Err(EINVAL)
    }

    fn fsync(&self, id: FsObjectId, guard: &Guard) -> StepOutcome<()> {
        let (reg, _, _) = self.resolve(id, guard)?;
        (reg.ops.step_barrier)(reg)
    }

    fn supports_reflink(&self, _other: &PageContainer) -> bool { false }
}
```

`resolve(id, guard)` returns the triple `(registration_ref, start_lba, len_lba)`: for whole devices, `(reg, 0, reg.total_blocks())`; for partitions, `(parent_reg, entry.start_lba, entry.len_lba)`. Both cases end up calling the same parent registration's `ops.step_read_blocks` — a partition is not its own driver, just a contiguous slice of its parent.

### 4.1 Why partitions ride the parent's driver

<!-- txdoc:BDEV-FS-WHY-PARTITIONS-RIDE-PARENT-S-DRIVER-1 -->

Linux models partitions as objects with their own `gendisk` and remapping in `blk_mq`. We instead translate LBAs at the filesystem level. The partition's `FsObjectId` carries the child devt (for path resolution and page-cache coherence), but its `FsPageBacking` ops go through the parent's `BlockDeviceOps`. Simpler; no new driver layer.

One consequence: writes through `/dev/vda1`'s page cache are subject to the *same* driver dispatch as writes through `/dev/vda` at the equivalent offset. If both are open, the page caches are independent (different `fs_object_id`), so **concurrent writes through parent and child aliases of the same region are not coherent with each other**. This matches Linux's behavior and reason-to-avoid: userspace must not alias.

---

## 5. PC coherence and the shared-page-cache rule

<!-- txdoc:BDEV-FS-PC-COHERENCE-SHARED-PAGE-CACHE-RULE-1 -->

### 5.1 The problem

<!-- txdoc:BDEV-FS-THE-PROBLEM-1 -->

Two scenarios both need the same page cache for `/dev/vda`:

1. `cat /dev/vda` (read through the file descriptor).
2. `mount /dev/vda /mnt/target` (tx-ext4 reads metadata PCs pointing at offsets of the same device).

If each code path constructs its own PC, their page caches are disjoint, and writes through one won't be visible through the other without explicit fsync + invalidate. Linux's `bdev` layer has exactly this contract, and it's a common source of subtle bugs.

### 5.2 The rule

<!-- txdoc:BDEV-FS-THE-RULE-1 -->

**bdev-fs maintains one PC per devt.** The first code path to reach for `/dev/vda`'s PC — whether an `open()` via devfs lookup or a tx-ext4 mount — creates it; subsequent reachers fetch the same Cap from the coherence index.

### 5.3 The coherence index

<!-- txdoc:BDEV-FS-THE-COHERENCE-INDEX-1 -->

`BdevFsMountPayload::pc_by_devt: RwLock<BTreeMap<DevT, Weak<PageContainer>>>`.

- **Keys** are DevTs — both whole-device and partition.
- **Values** are `Weak<PageContainer>`. Weak because bdev-fs doesn't *own* the PCs; its holders (RNodes, tx-ext4 mount payloads) do. When all holders drop, the PC reclaims and the Weak in this map becomes inert.

Find-or-create uses the conditional-commit primitive family from `CONCEPTS §15.7`:

```rust
// frame/bdev_fs/execution/get_or_create_pc.rs
pub fn get_or_create_pc(
    payload: &BdevFsMountPayload,
    id: FsObjectId,
    guard: &Guard,
) -> StepOutcome<Cap<PageContainer>> {
    let devt = id.devt();

    // Fast path: existing live PC.
    {
        let map = payload.pc_by_devt.read(guard);
        if let Some(weak) = map.get(&devt) {
            if let Some(cap) = weak.upgrade() {
                return Done(cap);
            }
        }
    }

    // Slow path: create PC, install via install_if_absent.
    let (reg, _, len_lba) = payload.resolve(id, guard)?;
    let total_bytes = len_lba * (reg.ops.block_size)(reg) as u64;
    let new_pc = page_container::new(
        PageContainerKind::File {
            fs: FsHandle::BdevFs(payload.self_handle()),
            fs_object_id: id,
        },
        total_bytes,
    )?;

    // Conditional commit: install only if no live PC exists for this devt.
    match payload.pc_by_devt.install_if_absent(devt, Arc::downgrade(&new_pc), guard) {
        InstallResult::Committed => Done(new_pc),
        InstallResult::Lost { existing_weak } => {
            // Another caller won the race.
            drop(new_pc);
            existing_weak.upgrade().ok_or(ERACE)   // rare; retry at script level
        }
    }
}
```

This matches the `install_if_absent` shape from CONCEPTS §15.7.4 — install only if no binding already exists. Concurrent callers racing to create a PC for the same devt converge on exactly one.

### 5.4 Ownership ladder

<!-- txdoc:BDEV-FS-OWNERSHIP-LADDER-1 -->

```
BlockDeviceRegistration (&'static)          — tier-2, lives forever
        ▲
        │ points to
BdevFsMountPayload                          — zone-allocated
    pc_by_devt: { DevT → Weak<PC> }         — coherence index, no retention
        ▲                ▲
        │                │
        │                └─── referenced by tx-ext4's MountPayload
        │                     for metadata PCs (strong Cap, keeps PC alive)
        │
        └─── referenced by devfs-synthesized RNodes' PageBacked backing
             (strong Cap per RNode, keeps PC alive)
```

The bdev-fs MountPayload itself does not hold strong Caps on device PCs — holding strong Caps would tie PC lifetime to mount lifetime, which is wrong for `umount`-hostile paths. Weak refs let device PCs reclaim when all of their *consumers* (RNodes and filesystem mounts) drop them, independent of whether bdev-fs is still mounted.

---

## 6. Partition-table parsing

<!-- txdoc:BDEV-FS-PARTITION-TABLE-PARSING-1 -->

### 6.1 When parsing happens

<!-- txdoc:BDEV-FS-WHEN-PARSING-HAPPENS-1 -->

Once per block device, at `device::init()` time (phase 3, per DEVICE.md §7.1). The parser runs synchronously in the tier-2 init path, after the driver's `step_init` returns (so the device is ready for I/O) and before `bdev_fs::init()` (phase 5) mounts bdev-fs.

```rust
// frame/bdev_fs/execution/parse_partitions.rs
pub fn parse_partitions(
    reg: &'static BlockDeviceRegistration,
) -> StepOutcome<PartitionTable> {
    // Read LBA 0 and LBA 1 into scratch frames.
    let mut sectors = [Frame::new_temp()?, Frame::new_temp()?];
    (reg.ops.step_read_blocks)(reg, 0, 2, &mut sectors)?;

    // First try GPT (LBA 1 signature); fall back to MBR (LBA 0 bytes 0x1FE..0x200).
    if sectors[1].as_slice()[0..8] == *b"EFI PART" {
        gpt::parse(reg, &sectors)
    } else if sectors[0].as_slice()[510..512] == [0x55, 0xAA] {
        mbr::parse(reg, &sectors)
    } else {
        // No partition table. Single-whole-device case.
        Done(PartitionTable::empty())
    }
}
```

GPT and MBR parsers are small (< 200 LOC each). They produce `PartitionTable` entries with `(child_devt, start_lba, len_lba, name_suffix)` tuples; the child_devt is allocated by bdev-fs from a range reserved for each parent's partition space (e.g., parent vda = `{major: VIRTIO_BLK, minor: 0}`; children = `{major: VIRTIO_BLK, minor: 1..}`).

### 6.2 No partition-table mutation in v1

<!-- txdoc:BDEV-FS-NO-PARTITION-TABLE-MUTATION-V1-1 -->

`fdisk`, `parted`, etc. are out of scope for v1. Mutating the partition table requires:

- Writing new LBA 0 / LBA 1 content (easy via `flush_page`).
- Revalidating that no active mount spans the old-but-not-new boundaries (hard — cross-subsystem invariant).
- Updating `BdevFsMountPayload::partitions` coherently with live mount state (needs publish discipline).

None of this is needed to boot busybox + gcc + nginx. Deferred.

**Consequence**: to change partition layout, a userspace tool writes the disk image before boot (or from a different kernel), and this kernel re-parses at next boot.

### 6.3 Loop-devices and image files

<!-- txdoc:BDEV-FS-LOOP-DEVICES-IMAGE-FILES-1 -->

Not in v1. If we want `losetup`-style "mount this file as a block device," that's a separate pseudo-driver registering a synthetic `BlockDeviceRegistration` whose `ops` redirect to file-based I/O. Future work.

---

## 7. Mount lifecycle

<!-- txdoc:BDEV-FS-MOUNT-LIFECYCLE-1 -->

### 7.1 Mount

<!-- txdoc:BDEV-FS-MOUNT-1 -->

Exactly one bdev-fs instance exists per system, mounted at `/dev/block` (or similar — exact path per init policy, not architectural):

```rust
// frame/bdev_fs/execution/step_mount.rs
pub fn init() -> StepOutcome<Cap<MountIdentity>> {
    // Build the MountPayload referencing the global block-device registry.
    let registrations = frame::device::block_registrations();   // &'static [&BDR]

    // Pre-parse partition tables for every registration (synchronous).
    let mut partitions = BTreeMap::new();
    for &reg in registrations {
        let tbl = bdev_fs::parse_partitions(reg)?;
        partitions.insert(reg.devt, tbl);
    }

    let payload = BdevFsMountPayload {
        pc_by_devt: RwLock::new(BTreeMap::new()),
        partitions: RwLock::new(partitions),
        registrations,
        ...
    };

    // Hand off to Mount subsystem to install the MountPayload and
    // return a MountIdentity Cap. Mount at /dev/block.
    mount::step_mount_commit(
        MountSpec {
            fstype: "bdev",
            payload: MountPayload::BdevFs(Box::new(payload)),
            mountpoint: "/dev/block",
            ..
        },
    )
}
```

### 7.2 Unmount

<!-- txdoc:BDEV-FS-UNMOUNT-1 -->

Rejected. Unmounting bdev-fs would render `/dev/vda*` unopenable. The Mount subsystem can refuse umount for in-use mounts; bdev-fs is always in use (root filesystem mounts refer into it).

### 7.3 Interaction with devfs

<!-- txdoc:BDEV-FS-INTERACTION-WITH-DEVFS-1 -->

devfs (from DEVICE.md §6) discovers a block device under `/dev/vda` and wants to construct a `PageBacked { pc: <Cap> }` RNode. It calls bdev-fs's `get_or_create_pc(id, guard)` to obtain the Cap, then constructs the RNode. The RNode holds the Cap for as long as it's alive; releases it on RNode reclaim.

devfs does not need a direct handle on the bdev-fs MountPayload — a small helper `bdev_fs::pc_for_device(devt) -> StepOutcome<Cap<PageContainer>>` in `frame/bdev_fs/` provides access. The helper looks up the global bdev-fs mount and calls `get_or_create_pc`.

---

## 8. Integration with tx-ext4

<!-- txdoc:BDEV-FS-INTEGRATION-TX-EXT4-1 -->

### 8.1 What tx-ext4 gets

<!-- txdoc:BDEV-FS-WHAT-TX-EXT4-GETS-1 -->

tx-ext4's `MountInitContext` (per TX_EXT4_PLAN §3.4) consumes a block-device handle. That handle is:

```rust
pub struct BlockDeviceHandle {
    pub reg: &'static BlockDeviceRegistration,
    pub start_lba: u64,       // 0 for whole device; partition's start otherwise
    pub len_lba: u64,
}
```

`BlockDeviceHandle::step_read(lba_offset, n, &mut frames)` and `step_write(...)` simply add `start_lba + lba_offset` and call through to `reg.ops.step_read_blocks` / `step_write_blocks`. The bdev-fs layer provides these handles to tx-ext4 at mount time:

```rust
// When userspace runs `mount /dev/vda1 /mnt -t ext4`:
//   1. VFS walks the mount syscall, resolves /dev/vda1 to an RNode
//      that bdev-fs owns (id = FsObjectId::partition(vda1_devt)).
//   2. VFS calls ext4's mount entrypoint with the RNode.
//   3. ext4's mount entrypoint asks bdev-fs:
//         let handle = bdev_fs::block_device_handle_for(id)?;
//      which returns BlockDeviceHandle pointing at the parent reg
//      with partition start_lba / len_lba baked in.
//   4. ext4 constructs its Ext4FsInstance around the handle,
//      builds metadata PCs, etc.
```

### 8.2 Shared page cache for raw and cooked views

<!-- txdoc:BDEV-FS-SHARED-PAGE-CACHE-RAW-COOKED-VIEWS-1 -->

If a process opens `/dev/vda1` for reading while tx-ext4 is mounted on it, both go through the same bdev-fs PC for `vda1`. This means:

- Reads see a byte-consistent view of the device (modulo tx-ext4's in-flight journal).
- *Writes* through the raw fd bypass tx-ext4's consistency (metadata cache, journal) — the same footgun as Linux. The PageBacked write through `/dev/vda1` goes through `flush_page`, which goes through `BlockDeviceOps::step_write_blocks`, which bypasses ext4 entirely. Data corruption will follow if both are mutating.

We accept the footgun; Linux does too. The alternative — refusing raw-device write while a filesystem is mounted on it — is a cross-subsystem invariant we're not ready to enforce in v1.

### 8.3 tx-ext4 metadata PCs

<!-- txdoc:BDEV-FS-TX-EXT4-METADATA-PCS-1 -->

Per TX_EXT4_PLAN §1.1, tx-ext4 keeps its on-disk metadata (superblock mirror, block group descriptors, inode tables, bitmaps) in `Cap<PageContainer>` handles held inside its own `MountPayload`. Those PCs are **not** bdev-fs PCs — they have a different `PageContainerKind` (either `Metadata`, or `Anon` with custom backing, per TX_EXT4_PLAN §1.1 option A). They fetch their content through the same `BlockDeviceHandle` but with offsets into metadata regions; the fetch goes through `reg.ops.step_read_blocks` directly, not through bdev-fs.

In other words: **bdev-fs owns the raw-device view; tx-ext4 owns the filesystem view; both bottom out at the same driver.** The coherence boundary is the driver's block-level I/O: writes committed to the driver are visible to subsequent reads from the driver, regardless of which upstream consumer issued them. bdev-fs and tx-ext4's metadata caches are *not* coherent with each other; they must not alias (normal Linux-level filesystem invariant).

---

## 9. Module layout

<!-- txdoc:BDEV-FS-MODULE-LAYOUT-1 -->

```
frame/bdev_fs/
    structure/
        mount.rs              BdevFsMountPayload
        partition.rs          PartitionTable, PartitionEntry
        handle.rs             BlockDeviceHandle (exported to filesystems)
    checks/                   (empty — no witness production;
                               registration refs and PC Caps are the evidence.)
    execution/
        step_mount.rs         bdev_fs::init() — phase-5 init
        parse_partitions.rs   MBR / GPT parsing
        get_or_create_pc.rs   Coherence-index find-or-create
        pc_for_device.rs      devfs → bdev-fs bridge
        block_device_handle_for.rs   filesystem → bdev-fs bridge
    fs_ops.rs                 impl FsOps for BdevFsMountPayload (§3)
    page_backing.rs           impl FsPageBacking (§4)
    mbr.rs                    MBR parser
    gpt.rs                    GPT parser
```

---

## 10. Open questions

<!-- txdoc:BDEV-FS-OPEN-QUESTIONS-1 -->

**10.1 Whole-device partition-table access.** Opening `/dev/vda` for writing and updating the partition table works mechanically (writes go through `flush_page` → `step_write_blocks`), but leaves `BdevFsMountPayload::partitions` stale. A refresh hook (ioctl `BLKRRPART` equivalent) could re-parse, but for v1 there is no such interface and no support for changing partition tables at runtime.

**10.2 DIRECT_IO.** Userspace `open("/dev/vda", O_DIRECT)` currently goes through the page cache like any other open. `O_DIRECT` is honored only as a hint (cache invalidated after write completes). True O_DIRECT (user-buffer-to-driver without page-cache touch) is deferred.

**10.3 Barriers, flush, FUA.** `step_barrier` is called from `FsPageBacking::fsync`. Whether it implies FUA (force unit access for subsequent writes) or only barrier semantics is a driver-side decision. Documented per-driver; no uniform policy.

**10.4 Device size changes.** If a driver's `total_blocks()` changes at runtime (rare — online resize of a virtual disk in qemu is the only example), bdev-fs's `InodeMeta.size` is stale. v1 ignores this; the PC's `size` field is set at first materialization and not updated.

**10.5 Permissions.** `InodeMeta.mode` is hard-coded to `S_IFBLK | 0o660`, uid/gid 0. No user-configurable device permissions. Adequate for a kernel that runs only root-owned processes in the v1 scope; revisit when multi-user is relevant.

---

## References

<!-- txdoc:BDEV-FS-REFERENCES-1 -->

- [`DEVICE.md`](../06_devices/DEVICE.md) §2.2, §4, §5.3, §7, §10.2, §11.
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §2, §3, §4, §5.
- [`TX_EXT4_PLAN.md`](TX_EXT4_PLAN_v1_2.md) §3.2, §3.4, §1.1.
- [`object_model.md`](../00_meta-framework/object_model_v2.md) §3, §8.1.1.
- [`CONCEPTS_v4.md`](../00_meta-framework/CONCEPTS_v4.md) §8, §15.7.
- [`INVARIANTS_v4.md`](../00_meta-framework/INVARIANTS_v4.md) — ARCH-5, PRED-7.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md).
- [`VFS_CHECKS_V2.1.md`](VFS_CHECKS_V2.1.md) — MountPayload and FsOps consumer side.
