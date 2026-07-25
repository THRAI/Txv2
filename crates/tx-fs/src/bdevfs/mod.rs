//! bdev-fs — block-device pseudo-filesystem.
//!
//! Per `txdoc:05-FILESYSTEM-BDEV-FS`
//! (`docs/design/05_filesystem/BDEV_FS.md`): bdev-fs maps registered
//! `BlockDeviceRegistration`s into page-backed RNodes so that
//! `/dev/vda`, `/dev/sda`, etc. have file semantics — page cache,
//! mmap, uniform `read` / `write` — without the device subsystem
//! providing file-ops machinery of its own.
//!
//! bdev-fs is a tier-2 projection-shaped filesystem in the same
//! family as procfs and devpts: entries resolve through the static
//! block-device registry rather than an in-memory BTreeMap. The
//! directory structure is read-only; all mutating operations reject
//! with `EROFS`. The block-device *content* is read-write through
//! the `FsPageBacking` path.
//!
//! ## Architecture
//!
//! `BdevFsMountPayload` is the filesystem-instance payload (per
//! `txdoc:BDEV-FS-ZONE-DERIVED-TYPE-POLICY-1`). It holds:
//!
//! - `partition_table` — compile-time partition layout (v1 stub).
//! - `coherence` — devt → `Weak<PageContainer>` index so that every
//!   open of `/dev/vda` materialises the same PageContainer
//!   (`txdoc:BDEV-FS-COHERENCE-1`).  If the PC has been reclaimed
//!   (Weak upgrade fails), a fresh one is created and re-indexed.
//!
//! Both `FsOps` and `FsPageBacking` are implemented on
//! `BdevFsMountPayload`.  `BdevFs` is retained as a zero-state
//! convenience alias.
//!
//! Active-doc anchors:
//! - `txdoc:BDEV-FS-MOTIVATION-1` → §1
//! - `txdoc:BDEV-FS-ZONE-DERIVED-TYPE-POLICY-1` → type policy table
//! - `txdoc:BDEV-FS-FSOPS-1` → §3 (FsOps surface)
//! - `txdoc:BDEV-FS-FSPAGEBACKING-1` → §4 (FsPageBacking surface)
//! - `txdoc:BDEV-FS-COHERENCE-1` → §5.1 (devt→PC index)
//! - `txdoc:BDEV-FS-PARTITION-TABLE-1` → §5.3 (MBR/GPT, v1 stub)
//! - `txdoc:BDEV-FS-MODULE-LAYOUT-1` → §9

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub mod adapter;

use adapter::step_engine::{
    self as step_engine, page_allocator, Cap, NoProgress, PayloadCap, SpinMutex, StepOutcome, Weak,
    ZeroPolicy,
};

use tx_subsystems::device::{self, BlockDeviceHandle, DevT};
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};
use tx_subsystems::mount::{MountPayload, MountPayloadPin};
use tx_subsystems::page_backed::{reserve_frame_with_reclaim, Frame, FsPageBacking, PageContainer};
use tx_subsystems::vfs::{
    Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta, RNode, RNodeBacking,
    S_IFBLK, S_IFDIR,
};

// ============================================================================
// Constants
// ============================================================================

/// Stable `FsObjectId` for the bdev-fs root directory.
pub const BDEVFS_ROOT_ID: FsObjectId = FsObjectId::new(0x6264_6600);

/// Base id-space for block-device entries.  Each entry gets
/// `BDEVFS_ENTRY_BASE + snapshot_index`.
const BDEVFS_ENTRY_BASE: u64 = 0x6264_6601;

/// Mode for any block-device entry resolved by bdev-fs.
pub const BDEVFS_BLOCK_MODE: u16 = S_IFBLK | 0o660;

/// Mode for the bdev-fs root directory.
pub const BDEVFS_ROOT_MODE: u16 = S_IFDIR | 0o755;

// ============================================================================
// Partition-table types (v1 stub)
// ============================================================================

/// Compile-time partition layout.
///
/// Per `txdoc:BDEV-FS-PARTITION-TABLE-1`: v1 has no runtime partition
/// support.  `PartitionTable` is a marker type that future phases
/// will populate from MBR or GPT.
#[derive(Clone, Debug, Default)]
pub struct PartitionTable {
    entries: BTreeMap<u32, PartitionEntry>,
}

/// One partition row.
#[derive(Clone, Copy, Debug)]
pub struct PartitionEntry {
    /// Device identifier the partition belongs to.
    pub parent_devt: DevT,
    /// Partition index within the parent device (1-based).
    pub index: u32,
    /// First LBA of the partition.
    pub start_lba: u64,
    /// Length in LBA units.
    pub len_lba: u64,
}

impl PartitionTable {
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Look up a partition by its parent devt + index.
    pub fn get(&self, parent_devt: DevT, index: u32) -> Option<&PartitionEntry> {
        self.entries
            .get(&index)
            .filter(|e| e.parent_devt == parent_devt)
    }
}

// ============================================================================
// BdevFsMountPayload — the filesystem-instance payload
// ============================================================================

/// bdev-fs mount payload implementing `FsOps` and `FsPageBacking`.
///
/// Holds the devt→PC coherence index so that every materialisation
/// of a given block device shares the same `PageContainer`.
///
/// Implements both trait surfaces so mount code can pass
/// `Arc<BdevFsMountPayload>` as `Arc<dyn FsOps>` and
/// `Arc<dyn FsPageBacking>`.
pub struct BdevFsMountPayload {
    /// Compile-time partition layout (v1: always empty).
    pub partition_table: SpinMutex<PartitionTable>,
    /// Coherence index: devt → Weak<PageContainer>.  Protected by
    /// the same lock that guards the partition table — the index
    /// and the partition table share a lock domain.
    coherence: SpinMutex<BTreeMap<DevT, Weak<PageContainer>>>,
}

impl BdevFsMountPayload {
    pub fn new() -> Self {
        Self {
            partition_table: SpinMutex::new(PartitionTable::new()),
            coherence: SpinMutex::new(BTreeMap::new()),
        }
    }

    /// Return an `Arc<dyn FsOps>` for use in `MountPayload::new_cap`.
    pub fn fs_ops_arc(self: &Arc<Self>) -> Arc<dyn FsOps> {
        Arc::clone(self) as Arc<dyn FsOps>
    }

    /// Return an `Arc<dyn FsPageBacking>` for use in `MountPayload::new_cap`.
    pub fn fs_page_backing_arc(self: &Arc<Self>) -> Arc<dyn FsPageBacking> {
        Arc::clone(self) as Arc<dyn FsPageBacking>
    }

    /// Find-or-create the `PageContainer` for a given device.
    ///
    /// Checks the coherence index first: if a `Weak<PageContainer>`
    /// for `devt` exists and is still alive, returns a clone of the
    /// upgraded `Cap`.  Otherwise creates a fresh PC, inserts a
    /// `Weak` into the index, and returns the new `Cap`.
    ///
    /// Per `txdoc:BDEV-FS-COHERENCE-1`.
    fn get_or_create_pc(
        &self,
        devt: DevT,
        reg: &'static device::BlockDeviceRegistration,
        fs_object_id: FsObjectId,
        mount: &Cap<MountPayload>,
        guard: &Guard<'_>,
    ) -> Result<Cap<PageContainer>, Errno> {
        {
            let mut index = self.coherence.lock();
            if let Some(weak) = index.get(&devt) {
                if let Some(cap) = weak.upgrade(guard) {
                    return Ok(cap);
                }
                // Weak failed — PC was reclaimed. Fall through to create a
                // new one outside the coherence lock.
                index.remove(&devt);
            }
        }

        let block_size = reg.ops.block_size() as u64;
        let total_bytes = reg.ops.total_blocks().saturating_mul(block_size);
        let new_pc = PageContainer::new_file_cap(
            MountPayloadPin::acquire(&PayloadCap::from_cap(mount.clone())),
            fs_object_id,
            total_bytes,
        )
        .map_err(|_| Errno::ENOMEM)?;

        {
            let mut index = self.coherence.lock();
            if let Some(weak) = index.get(&devt) {
                if let Some(cap) = weak.upgrade(guard) {
                    return Ok(cap);
                }
                index.remove(&devt);
            }

            index.insert(devt, new_pc.downgrade());
        }

        let _runtime = device::register_page_container_file_io_service(
            new_pc.clone(),
            BlockDeviceHandle::whole(reg),
        );
        Ok(new_pc)
    }
}

impl Default for BdevFsMountPayload {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// BdevFs — zero-state convenience alias
// ============================================================================

/// Zero-state bdev-fs backend (convenience alias).
///
/// Prefer `BdevFsMountPayload` for production use.
/// `BdevFs` exists so that simple standalone tests can construct
/// `Arc<dyn FsOps>` / `Arc<dyn FsPageBacking>` without a payload
/// allocation, but it has no coherence index: every materialisation
/// allocates a fresh `PageContainer`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BdevFs;

impl BdevFs {
    pub const fn new() -> Self {
        Self
    }

    pub fn fs_ops_arc() -> Arc<dyn FsOps> {
        Arc::new(Self)
    }

    pub fn fs_page_backing_arc() -> Arc<dyn FsPageBacking> {
        Arc::new(Self)
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn entry_index(fs_object_id: FsObjectId) -> Option<usize> {
    let raw = fs_object_id.as_u64();
    raw.checked_sub(BDEVFS_ENTRY_BASE).map(|i| i as usize)
}

fn device_by_index(index: usize) -> Option<&'static device::BlockDeviceRegistration> {
    device::block_device_snapshot().into_iter().nth(index)
}

/// Resolve a bdev-fs `FsObjectId` to its underlying
/// `&'static BlockDeviceRegistration`.
///
/// Returns `None` when the id is the bdev-fs root, refers to a slot
/// that has since been removed from the registry, or was minted by a
/// non-bdev-fs backend. This is the helper called out as
/// `bdev_fs::block_device_handle_for` in
/// `docs/design/05_filesystem/BDEV_FS.md` §8.1 — the bridge a
/// filesystem (tx-ext4) uses at mount-time to turn a userspace
/// `/dev/block/<name>` path into a block-device handle.
pub fn block_device_for_object_id(
    fs_object_id: FsObjectId,
) -> Option<&'static device::BlockDeviceRegistration> {
    let index = entry_index(fs_object_id)?;
    device_by_index(index)
}

/// Plan a page-sized bdev-fs read/write as a neutral L6 `BioPlan`.
///
/// This is a compatibility-side planning helper for the staged I/O manager
/// migration. It does not submit to `BlockDeviceHandle`; the existing
/// `FsPageBacking` direct path remains the live executor.
pub fn plan_page_bio(
    fs_object_id: FsObjectId,
    offset: u64,
    op: BlockOp,
    buffer_key: u64,
) -> Result<BioPlan, Errno> {
    if !matches!(op, BlockOp::Read | BlockOp::Write) {
        return Err(Errno::EINVAL);
    }

    let reg = block_device_for_object_id(fs_object_id).ok_or(Errno::ENOENT)?;
    let block_size = u64::from(reg.ops.block_size());
    let page_size = tx_subsystems::vm::USER_PAGE_SIZE as u64;
    if block_size == 0 || page_size == 0 || !page_size.is_multiple_of(block_size) {
        return Err(Errno::EINVAL);
    }
    if !offset.is_multiple_of(page_size) {
        return Err(Errno::EINVAL);
    }

    let start_lba = offset / block_size;
    let handle = BlockDeviceHandle::whole(reg);
    let device_blocks = handle.len_lba();
    if start_lba >= device_blocks {
        return Err(Errno::EINVAL);
    }

    let blocks_per_page = page_size / block_size;
    let block_count = core::cmp::min(blocks_per_page, device_blocks - start_lba);
    Ok(BioPlan::new(
        DeviceKey::new(reg.devt.raw()),
        op,
        LbaRange::new(start_lba, block_count),
        alloc::vec![BioVec::new(buffer_key, 0, page_size as u32)],
        BlockFlags::EMPTY,
    ))
}

/// Plan a bdev-fs barrier as a neutral L6 `BioPlan`.
///
/// The helper intentionally only plans the operation; legacy fsync still calls
/// `BlockDeviceHandle::barrier` until L6 submission becomes the live path.
pub fn plan_barrier_bio(fs_object_id: FsObjectId) -> Result<BioPlan, Errno> {
    let reg = block_device_for_object_id(fs_object_id).ok_or(Errno::ENOENT)?;
    Ok(BioPlan::new(
        DeviceKey::new(reg.devt.raw()),
        BlockOp::Barrier,
        LbaRange::new(0, 0),
        Vec::new(),
        BlockFlags::BARRIER,
    ))
}

fn block_device_meta(reg: &device::BlockDeviceRegistration) -> InodeMeta {
    let block_size = reg.ops.block_size() as u64;
    let total_bytes = reg.ops.total_blocks().saturating_mul(block_size);
    InodeMeta {
        mode: BDEVFS_BLOCK_MODE,
        size: total_bytes,
        ..InodeMeta::new(InodeKind::BlockDevice, BDEVFS_BLOCK_MODE)
    }
}

fn devt_for_index(index: usize) -> Option<DevT> {
    device_by_index(index).map(|r| r.devt)
}

// ============================================================================
// FsOps for BdevFsMountPayload
// ============================================================================

impl FsOps for BdevFsMountPayload {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        if parent != BDEVFS_ROOT_ID {
            return StepOutcome::err(step_engine::Errno::ENOTDIR);
        }
        let snapshot = device::block_device_snapshot();
        for (idx, reg) in snapshot.into_iter().enumerate() {
            if reg.name.as_bytes() == name {
                let id = BDEVFS_ENTRY_BASE
                    .checked_add(idx as u64)
                    .map(FsObjectId::new)
                    .unwrap_or(BDEVFS_ROOT_ID);
                return StepOutcome::done(id);
            }
        }
        StepOutcome::err(step_engine::Errno::ENOENT)
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress> {
        if fs_object_id == BDEVFS_ROOT_ID {
            return StepOutcome::done(InodeMeta::new(InodeKind::Directory, BDEVFS_ROOT_MODE));
        }
        let Some(idx) = entry_index(fs_object_id) else {
            return StepOutcome::err(step_engine::Errno::ENOENT);
        };
        let Some(reg) = device_by_index(idx) else {
            return StepOutcome::err(step_engine::Errno::ENOENT);
        };
        StepOutcome::done(block_device_meta(reg))
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        mount: &Cap<MountPayload>,
        guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, NoProgress> {
        if meta.kind() != InodeKind::BlockDevice {
            return StepOutcome::err(Errno::ENOSYS.into());
        }
        let Some(idx) = entry_index(fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let Some(reg) = device_by_index(idx) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let Some(devt) = devt_for_index(idx) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };

        // Per txdoc:BDEV-FS-COHERENCE-1: reuse the existing
        // PageContainer when one is still live for this devt.
        let container = match self.get_or_create_pc(devt, reg, fs_object_id, mount, guard) {
            Ok(pc) => pc,
            Err(e) => return StepOutcome::err(e.into()),
        };

        match RNode::new_cap_in_mount(
            fs_object_id,
            meta,
            RNodeBacking::PageBacked { pc: container },
            mount,
        ) {
            Ok(rnode) => StepOutcome::done(rnode),
            Err(_) => StepOutcome::err(Errno::ENOMEM.into()),
        }
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        if fs_object_id != BDEVFS_ROOT_ID {
            return StepOutcome::err(step_engine::Errno::ENOTDIR);
        }
        let index = cursor.as_u64() as usize;
        let snapshot = device::block_device_snapshot();
        let Some(reg) = snapshot.into_iter().nth(index) else {
            return StepOutcome::done(None);
        };
        let child_id = BDEVFS_ENTRY_BASE
            .checked_add(index as u64)
            .map(FsObjectId::new)
            .unwrap_or(BDEVFS_ROOT_ID);
        let entry = match DirEntry::new(child_id, InodeKind::BlockDevice, reg.name.as_bytes()) {
            Ok(e) => e,
            Err(err) => return StepOutcome::err(err.into()),
        };
        StepOutcome::done(Some((entry, DirCursor::from_u64(cursor.as_u64() + 1))))
    }

    // --- mutating ops reject with EROFS ---

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    fn read_link(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<alloc::boxed::Box<[u8]>, NoProgress> {
        StepOutcome::err(Errno::EINVAL.into())
    }

    fn chmod_inode(
        &self,
        _fs_object_id: FsObjectId,
        _new_mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn chown_inode(
        &self,
        _fs_object_id: FsObjectId,
        _new_uid: Option<u32>,
        _new_gid: Option<u32>,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
}

// ============================================================================
// FsPageBacking for BdevFsMountPayload
// ============================================================================

impl FsPageBacking for BdevFsMountPayload {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<Frame, NoProgress> {
        let Some(idx) = entry_index(fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let Some(reg) = device_by_index(idx) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };

        let handle = BlockDeviceHandle::whole(reg);
        let block_size = reg.ops.block_size() as u64;
        let page_size = tx_subsystems::vm::USER_PAGE_SIZE as u64;

        if !offset.is_multiple_of(page_size) {
            return StepOutcome::err(Errno::EINVAL.into());
        }

        let start_lba = offset / block_size;
        let device_blocks = handle.len_lba();
        let blocks_per_page = page_size / block_size;

        if start_lba >= device_blocks {
            return allocate_zeroed_page();
        }

        if blocks_per_page == 1 {
            let owned = match allocate_owned_page() {
                Some(f) => f,
                None => return StepOutcome::err(Errno::ENOMEM.into()),
            };
            let ppn = owned.ppn();
            let mut frames = [Frame::new(ppn)];

            match handle.read_blocks(start_lba, &mut frames, guard) {
                StepOutcome::Done(()) => {}
                StepOutcome::Err(e) => return StepOutcome::err(e),
                StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                    return StepOutcome::err(Errno::EIO.into());
                }
            }
            let _permanent = owned.into_permanent_frame();
            StepOutcome::done(frames[0])
        } else {
            let target_owned = match allocate_owned_page() {
                Some(f) => f,
                None => return StepOutcome::err(Errno::ENOMEM.into()),
            };
            let target_ppn = target_owned.ppn();
            let target_addr = match page_allocator::frame_kernel_addr(target_ppn) {
                Ok(addr) => addr,
                Err(_) => return StepOutcome::err(Errno::EFAULT.into()),
            };

            for i in 0..blocks_per_page {
                let lba = start_lba + i;
                if lba >= device_blocks {
                    break;
                }

                let temp_owned = match allocate_owned_page() {
                    Some(f) => f,
                    None => return StepOutcome::err(Errno::ENOMEM.into()),
                };
                let temp_ppn = temp_owned.ppn();
                let mut frames = [Frame::new(temp_ppn)];

                match handle.read_blocks(lba, &mut frames, guard) {
                    StepOutcome::Done(()) => {
                        let src_addr = match page_allocator::frame_kernel_addr(frames[0].ppn()) {
                            Ok(addr) => addr,
                            Err(_) => return StepOutcome::err(Errno::EFAULT.into()),
                        };
                        let byte_offset = i.saturating_mul(block_size) as usize;
                        let bytes_to_copy =
                            core::cmp::min(block_size as usize, page_size as usize - byte_offset);
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                src_addr,
                                target_addr.add(byte_offset),
                                bytes_to_copy,
                            );
                        }
                    }
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                        return StepOutcome::err(Errno::EIO.into());
                    }
                }
                drop(temp_owned);
            }

            let target_frame = Frame::new(target_ppn);
            let _permanent = target_owned.into_permanent_frame();
            StepOutcome::done(target_frame)
        }
    }

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let Some(idx) = entry_index(fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let Some(reg) = device_by_index(idx) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };

        let handle = BlockDeviceHandle::whole(reg);
        let block_size = reg.ops.block_size() as u64;
        let page_size = tx_subsystems::vm::USER_PAGE_SIZE as u64;

        if !offset.is_multiple_of(page_size) {
            return StepOutcome::err(Errno::EINVAL.into());
        }

        let start_lba = offset / block_size;
        let device_blocks = handle.len_lba();
        let blocks_per_page = page_size / block_size;

        if start_lba >= device_blocks {
            return StepOutcome::err(Errno::EINVAL.into());
        }

        if blocks_per_page == 1 {
            let frames = [*frame];
            match handle.write_blocks(start_lba, &frames, guard) {
                StepOutcome::Done(()) => StepOutcome::done(()),
                StepOutcome::Err(e) => StepOutcome::err(e),
                StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                    StepOutcome::err(Errno::EIO.into())
                }
            }
        } else {
            let src_addr = match page_allocator::frame_kernel_addr(frame.ppn()) {
                Ok(addr) => addr,
                Err(_) => return StepOutcome::err(Errno::EFAULT.into()),
            };

            for i in 0..blocks_per_page {
                let lba = start_lba + i;
                if lba >= device_blocks {
                    break;
                }

                let temp_owned = match allocate_owned_page() {
                    Some(f) => f,
                    None => return StepOutcome::err(Errno::ENOMEM.into()),
                };
                let temp_ppn = temp_owned.ppn();
                let temp_addr = match page_allocator::frame_kernel_addr(temp_ppn) {
                    Ok(addr) => addr,
                    Err(_) => return StepOutcome::err(Errno::EFAULT.into()),
                };
                let byte_offset = i.saturating_mul(block_size) as usize;
                let bytes_to_copy =
                    core::cmp::min(block_size as usize, page_size as usize - byte_offset);

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src_addr.add(byte_offset),
                        temp_addr,
                        bytes_to_copy,
                    );
                }

                let frames = [Frame::new(temp_ppn)];
                match handle.write_blocks(lba, &frames, guard) {
                    StepOutcome::Done(()) => {}
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                        return StepOutcome::err(Errno::EIO.into());
                    }
                }
                drop(temp_owned);
            }
            StepOutcome::done(())
        }
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        // v1: block-device size is immutable (BDEV_FS.md §10.4).
        StepOutcome::err(Errno::EINVAL.into())
    }

    fn fsync_file(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let Some(idx) = entry_index(fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let Some(reg) = device_by_index(idx) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let handle = BlockDeviceHandle::whole(reg);
        match handle.barrier(guard) {
            StepOutcome::Done(()) => StepOutcome::done(()),
            StepOutcome::Err(e) => StepOutcome::err(e),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                StepOutcome::err(Errno::EIO.into())
            }
        }
    }

    fn supports_reflink(&self, _other: &PageContainer) -> bool {
        false
    }
}

// ============================================================================
// FsOps for BdevFs (zero-state convenience impl)
// ============================================================================

impl FsOps for BdevFs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        if parent != BDEVFS_ROOT_ID {
            return StepOutcome::err(step_engine::Errno::ENOTDIR);
        }
        let snapshot = device::block_device_snapshot();
        for (idx, reg) in snapshot.into_iter().enumerate() {
            if reg.name.as_bytes() == name {
                let id = BDEVFS_ENTRY_BASE
                    .checked_add(idx as u64)
                    .map(FsObjectId::new)
                    .unwrap_or(BDEVFS_ROOT_ID);
                return StepOutcome::done(id);
            }
        }
        StepOutcome::err(step_engine::Errno::ENOENT)
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress> {
        if fs_object_id == BDEVFS_ROOT_ID {
            return StepOutcome::done(InodeMeta::new(InodeKind::Directory, BDEVFS_ROOT_MODE));
        }
        let Some(idx) = entry_index(fs_object_id) else {
            return StepOutcome::err(step_engine::Errno::ENOENT);
        };
        let Some(reg) = device_by_index(idx) else {
            return StepOutcome::err(step_engine::Errno::ENOENT);
        };
        StepOutcome::done(block_device_meta(reg))
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        mount: &Cap<MountPayload>,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, NoProgress> {
        // No coherence index — every call allocates a fresh PC.
        if meta.kind() != InodeKind::BlockDevice {
            return StepOutcome::err(Errno::ENOSYS.into());
        }
        let Some(idx) = entry_index(fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let Some(reg) = device_by_index(idx) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };

        let total_bytes = reg
            .ops
            .total_blocks()
            .saturating_mul(reg.ops.block_size() as u64);
        let container = match PageContainer::new_file_cap(
            MountPayloadPin::acquire(&PayloadCap::from_cap(mount.clone())),
            fs_object_id,
            total_bytes,
        ) {
            Ok(c) => c,
            Err(_) => return StepOutcome::err(Errno::ENOMEM.into()),
        };

        match RNode::new_cap_in_mount(
            fs_object_id,
            meta,
            RNodeBacking::PageBacked { pc: container },
            mount,
        ) {
            Ok(rnode) => StepOutcome::done(rnode),
            Err(_) => StepOutcome::err(Errno::ENOMEM.into()),
        }
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        if fs_object_id != BDEVFS_ROOT_ID {
            return StepOutcome::err(step_engine::Errno::ENOTDIR);
        }
        let index = cursor.as_u64() as usize;
        let snapshot = device::block_device_snapshot();
        let Some(reg) = snapshot.into_iter().nth(index) else {
            return StepOutcome::done(None);
        };
        let child_id = BDEVFS_ENTRY_BASE
            .checked_add(index as u64)
            .map(FsObjectId::new)
            .unwrap_or(BDEVFS_ROOT_ID);
        let entry = match DirEntry::new(child_id, InodeKind::BlockDevice, reg.name.as_bytes()) {
            Ok(e) => e,
            Err(err) => return StepOutcome::err(err.into()),
        };
        StepOutcome::done(Some((entry, DirCursor::from_u64(cursor.as_u64() + 1))))
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    fn read_link(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<alloc::boxed::Box<[u8]>, NoProgress> {
        StepOutcome::err(Errno::EINVAL.into())
    }

    fn chmod_inode(
        &self,
        _fs_object_id: FsObjectId,
        _new_mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn chown_inode(
        &self,
        _fs_object_id: FsObjectId,
        _new_uid: Option<u32>,
        _new_gid: Option<u32>,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
}

impl FsPageBacking for BdevFs {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> StepOutcome<Frame, NoProgress> {
        let Some(idx) = entry_index(fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let Some(reg) = device_by_index(idx) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };

        let handle = BlockDeviceHandle::whole(reg);
        let block_size = reg.ops.block_size() as u64;
        let page_size = tx_subsystems::vm::USER_PAGE_SIZE as u64;

        if !offset.is_multiple_of(page_size) {
            return StepOutcome::err(Errno::EINVAL.into());
        }

        let start_lba = offset / block_size;
        let device_blocks = handle.len_lba();
        let blocks_per_page = page_size / block_size;

        if start_lba >= device_blocks {
            return allocate_zeroed_page();
        }

        if blocks_per_page == 1 {
            let owned = match allocate_owned_page() {
                Some(f) => f,
                None => return StepOutcome::err(Errno::ENOMEM.into()),
            };
            let ppn = owned.ppn();
            let mut frames = [Frame::new(ppn)];

            match handle.read_blocks(start_lba, &mut frames, guard) {
                StepOutcome::Done(()) => {}
                StepOutcome::Err(e) => return StepOutcome::err(e),
                StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                    return StepOutcome::err(Errno::EIO.into());
                }
            }
            let _permanent = owned.into_permanent_frame();
            StepOutcome::done(frames[0])
        } else {
            let target_owned = match allocate_owned_page() {
                Some(f) => f,
                None => return StepOutcome::err(Errno::ENOMEM.into()),
            };
            let target_ppn = target_owned.ppn();
            let target_addr = match page_allocator::frame_kernel_addr(target_ppn) {
                Ok(addr) => addr,
                Err(_) => return StepOutcome::err(Errno::EFAULT.into()),
            };

            for i in 0..blocks_per_page {
                let lba = start_lba + i;
                if lba >= device_blocks {
                    break;
                }

                let temp_owned = match allocate_owned_page() {
                    Some(f) => f,
                    None => return StepOutcome::err(Errno::ENOMEM.into()),
                };
                let temp_ppn = temp_owned.ppn();
                let mut frames = [Frame::new(temp_ppn)];

                match handle.read_blocks(lba, &mut frames, guard) {
                    StepOutcome::Done(()) => {
                        let src_addr = match page_allocator::frame_kernel_addr(frames[0].ppn()) {
                            Ok(addr) => addr,
                            Err(_) => return StepOutcome::err(Errno::EFAULT.into()),
                        };
                        let byte_offset = i.saturating_mul(block_size) as usize;
                        let bytes_to_copy =
                            core::cmp::min(block_size as usize, page_size as usize - byte_offset);
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                src_addr,
                                target_addr.add(byte_offset),
                                bytes_to_copy,
                            );
                        }
                    }
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                        return StepOutcome::err(Errno::EIO.into());
                    }
                }
                drop(temp_owned);
            }

            let target_frame = Frame::new(target_ppn);
            let _permanent = target_owned.into_permanent_frame();
            StepOutcome::done(target_frame)
        }
    }

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let Some(idx) = entry_index(fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let Some(reg) = device_by_index(idx) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };

        let handle = BlockDeviceHandle::whole(reg);
        let block_size = reg.ops.block_size() as u64;
        let page_size = tx_subsystems::vm::USER_PAGE_SIZE as u64;

        if !offset.is_multiple_of(page_size) {
            return StepOutcome::err(Errno::EINVAL.into());
        }

        let start_lba = offset / block_size;
        let device_blocks = handle.len_lba();
        let blocks_per_page = page_size / block_size;

        if start_lba >= device_blocks {
            return StepOutcome::err(Errno::EINVAL.into());
        }

        if blocks_per_page == 1 {
            let frames = [*frame];
            match handle.write_blocks(start_lba, &frames, guard) {
                StepOutcome::Done(()) => StepOutcome::done(()),
                StepOutcome::Err(e) => StepOutcome::err(e),
                StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                    StepOutcome::err(Errno::EIO.into())
                }
            }
        } else {
            let src_addr = match page_allocator::frame_kernel_addr(frame.ppn()) {
                Ok(addr) => addr,
                Err(_) => return StepOutcome::err(Errno::EFAULT.into()),
            };

            for i in 0..blocks_per_page {
                let lba = start_lba + i;
                if lba >= device_blocks {
                    break;
                }

                let temp_owned = match allocate_owned_page() {
                    Some(f) => f,
                    None => return StepOutcome::err(Errno::ENOMEM.into()),
                };
                let temp_ppn = temp_owned.ppn();
                let temp_addr = match page_allocator::frame_kernel_addr(temp_ppn) {
                    Ok(addr) => addr,
                    Err(_) => return StepOutcome::err(Errno::EFAULT.into()),
                };
                let byte_offset = i.saturating_mul(block_size) as usize;
                let bytes_to_copy =
                    core::cmp::min(block_size as usize, page_size as usize - byte_offset);

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src_addr.add(byte_offset),
                        temp_addr,
                        bytes_to_copy,
                    );
                }

                let frames = [Frame::new(temp_ppn)];
                match handle.write_blocks(lba, &frames, guard) {
                    StepOutcome::Done(()) => {}
                    StepOutcome::Err(e) => return StepOutcome::err(e),
                    StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                        return StepOutcome::err(Errno::EIO.into());
                    }
                }
                drop(temp_owned);
            }
            StepOutcome::done(())
        }
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EINVAL.into())
    }

    fn fsync_file(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let Some(idx) = entry_index(fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let Some(reg) = device_by_index(idx) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let handle = BlockDeviceHandle::whole(reg);
        match handle.barrier(guard) {
            StepOutcome::Done(()) => StepOutcome::done(()),
            StepOutcome::Err(e) => StepOutcome::err(e),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                StepOutcome::err(Errno::EIO.into())
            }
        }
    }

    fn supports_reflink(&self, _other: &PageContainer) -> bool {
        false
    }
}

// ============================================================================
// Internal helpers
// ============================================================================

fn allocate_zeroed_page() -> StepOutcome<Frame, NoProgress> {
    let owned = match allocate_owned_page() {
        Some(f) => f,
        None => return StepOutcome::err(Errno::ENOMEM.into()),
    };
    let ppn = owned.ppn();
    let _permanent = owned.into_permanent_frame();
    StepOutcome::done(Frame::new(ppn))
}

fn allocate_owned_page() -> Option<
    tx_substrate::page_allocator::OwnedFrame<
        'static,
        tx_substrate::page_allocator::BitmapPageAllocator<'static>,
    >,
> {
    reserve_frame_with_reclaim(ZeroPolicy::Zeroed)
        .ok()
        .map(|r| r.commit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_subsystems::device::{BlockDevice, BlockDeviceOps, PhysicalBlockNumber};
    use tx_subsystems::io_manager::block::{BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};
    use tx_subsystems::mount::{DevId, MountOptions, SourceLabel};
    use tx_subsystems::page_backed::{read_exact_at, PageContainerKind};

    struct PatternBlockDevice;

    impl BlockDeviceOps for PatternBlockDevice {
        fn read_blocks(
            &self,
            block_id: PhysicalBlockNumber,
            target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            for (slot, frame) in target.iter().enumerate() {
                let mut bytes = [0u8; 16];
                bytes[..8].copy_from_slice(b"bdevfs!\0");
                bytes[8..16].copy_from_slice(&(block_id.as_u64() + slot as u64).to_le_bytes());
                tx_substrate::page_allocator::testing::write_frame_bytes_for_test(
                    frame.ppn(),
                    0,
                    &bytes,
                );
            }
            StepOutcome::done(())
        }

        fn write_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _source: &[Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }

        fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }
    }

    impl BlockDevice for PatternBlockDevice {
        fn total_blocks(&self) -> u64 {
            8
        }

        fn block_size(&self) -> u32 {
            tx_subsystems::vm::USER_PAGE_SIZE as u32
        }
    }

    static PATTERN_DEVICE: PatternBlockDevice = PatternBlockDevice;
    static PATTERN_REG: device::BlockDeviceRegistration = device::BlockDeviceRegistration {
        devt: DevT::new(8, 64),
        name: "vdr",
        ops: &PATTERN_DEVICE,
    };
    static PATTERN_REGS: &[&device::BlockDeviceRegistration] = &[&PATTERN_REG];

    fn init_bdevfs_test() {
        tx_test_support::init_host();
        tx_subsystems::zones::register_all().expect("tx-subsystems zones");
        match page_allocator::claim_zero_frame() {
            Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for bdevfs tests: {error:?}"),
        }
        device::reset_block_registry_for_test();
        assert_eq!(
            device::register_block_devices(PATTERN_REGS),
            StepOutcome::Done(())
        );
    }

    fn bdevfs_mount_payload(bdevfs: &Arc<BdevFsMountPayload>) -> Cap<MountPayload> {
        MountPayload::new_cap(
            bdevfs.fs_ops_arc(),
            bdevfs.fs_page_backing_arc(),
            None,
            DevId::new(64),
            MountOptions::default(),
            "bdev",
            SourceLabel::Static("bdevfs-test"),
        )
        .expect("bdevfs test mount payload")
    }

    #[test]
    fn materialised_block_device_rnode_reads_through_bdevfs_page_backing() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bdevfs_test();
        let guard = step_engine::guard();
        let bdevfs = Arc::new(BdevFsMountPayload::new());
        let mount = bdevfs_mount_payload(&bdevfs);

        let fs_object_id = match bdevfs.lookup(BDEVFS_ROOT_ID, b"vdr", &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup vdr failed: {other:?}"),
        };
        let meta = match bdevfs.load_inode_meta(fs_object_id, &guard) {
            StepOutcome::Done(meta) => meta,
            other => panic!("load_inode_meta vdr failed: {other:?}"),
        };
        let rnode = match bdevfs.materialise_rnode(fs_object_id, meta, &mount, &guard) {
            StepOutcome::Done(rnode) => rnode,
            other => panic!("materialise_rnode vdr failed: {other:?}"),
        };
        let RNodeBacking::PageBacked { pc } = rnode.backing() else {
            panic!("bdevfs block devices must materialise as page-backed rnodes");
        };
        assert!(
            matches!(pc.kind(), PageContainerKind::File { .. }),
            "block-device PageContainers must route misses through bdevfs FsPageBacking"
        );

        let mut bytes = [0u8; 16];
        match read_exact_at(pc, 0, &mut bytes, &guard) {
            StepOutcome::Done(()) => {}
            other => panic!("read_exact_at from bdevfs PC failed: {other:?}"),
        }

        assert_eq!(&bytes[..8], b"bdevfs!\0");
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 0);
    }

    #[test]
    fn materialised_block_device_registers_one_file_io_service_runtime() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bdevfs_test();
        device::reset_page_container_file_io_service_registry_for_test();
        let guard = step_engine::guard();
        let bdevfs = Arc::new(BdevFsMountPayload::new());
        let mount = bdevfs_mount_payload(&bdevfs);

        let fs_object_id = match bdevfs.lookup(BDEVFS_ROOT_ID, b"vdr", &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup vdr failed: {other:?}"),
        };
        let meta = match bdevfs.load_inode_meta(fs_object_id, &guard) {
            StepOutcome::Done(meta) => meta,
            other => panic!("load_inode_meta vdr failed: {other:?}"),
        };

        let first = match bdevfs.materialise_rnode(fs_object_id, meta, &mount, &guard) {
            StepOutcome::Done(rnode) => rnode,
            other => panic!("first materialise_rnode vdr failed: {other:?}"),
        };
        assert_eq!(device::page_container_file_io_service_runtime_count(), 1);

        let second = match bdevfs.materialise_rnode(fs_object_id, meta, &mount, &guard) {
            StepOutcome::Done(rnode) => rnode,
            other => panic!("second materialise_rnode vdr failed: {other:?}"),
        };
        assert_eq!(device::page_container_file_io_service_runtime_count(), 1);

        let RNodeBacking::PageBacked { pc: first_pc } = first.backing() else {
            panic!("first bdevfs rnode must be page-backed");
        };
        let RNodeBacking::PageBacked { pc: second_pc } = second.backing() else {
            panic!("second bdevfs rnode must be page-backed");
        };
        assert_eq!(first_pc.page_count(), second_pc.page_count());

        let snapshot = device::page_container_file_io_service_runtimes_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].handle().registration().devt, PATTERN_REG.devt);
        assert_eq!(snapshot[0].handle().start_lba(), 0);
        assert_eq!(
            snapshot[0].handle().len_lba(),
            PATTERN_REG.ops.total_blocks()
        );
        assert_eq!(snapshot[0].container().page_count(), first_pc.page_count());
    }

    #[test]
    fn bdevfs_plans_page_offsets_as_bioplans_without_touching_driver() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bdevfs_test();
        let guard = step_engine::guard();
        let bdevfs = BdevFsMountPayload::new();
        let fs_object_id = match bdevfs.lookup(BDEVFS_ROOT_ID, b"vdr", &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup vdr failed: {other:?}"),
        };

        let read = plan_page_bio(
            fs_object_id,
            tx_subsystems::vm::USER_PAGE_SIZE as u64,
            BlockOp::Read,
            0xabc,
        )
        .expect("read page should plan to a bio");
        assert_eq!(read.device, DeviceKey::new(PATTERN_REG.devt.raw()));
        assert_eq!(read.op, BlockOp::Read);
        assert_eq!(read.lba, LbaRange::new(1, 1));
        assert_eq!(
            read.vecs,
            alloc::vec![BioVec::new(
                0xabc,
                0,
                tx_subsystems::vm::USER_PAGE_SIZE as u32
            )]
        );
        assert_eq!(read.flags, BlockFlags::EMPTY);

        let barrier = plan_barrier_bio(fs_object_id).expect("barrier should plan");
        assert_eq!(barrier.device, DeviceKey::new(PATTERN_REG.devt.raw()));
        assert_eq!(barrier.op, BlockOp::Barrier);
        assert_eq!(barrier.lba, LbaRange::new(0, 0));
        assert!(barrier.vecs.is_empty());
        assert!(barrier.flags.contains(BlockFlags::BARRIER));
    }
}
