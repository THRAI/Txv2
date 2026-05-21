//! Mount entry points for tx-fat.
//!
//! Mirrors `tx-ext4::mount`.

use crate::adapter::step_engine::Cap;
use crate::read_backend::{fs_object_id, FatFsInstance, FAT_ROOT_CLUSTER_SENTINEL};
use alloc::sync::Arc;
use tx_fat_format::pager::BlockImage;
use tx_subsystems::execution::Errno;
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::vfs::structure::{FsObjectId, InodeMeta};
use tx_subsystems::vfs::FsOps;

pub struct MountedFat<I> {
    backend: Arc<FatFsInstance<I>>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}

impl<I> MountedFat<I>
where
    I: BlockImage + Send + 'static,
{
    pub fn fs_ops(&self) -> Arc<dyn FsOps> {
        self.backend.clone().fs_ops_arc()
    }

    pub fn fs_page_backing(&self) -> Arc<dyn FsPageBacking> {
        self.backend.clone().fs_page_backing_arc()
    }

    pub fn bind_mount_payload(&self, payload: &Cap<tx_subsystems::mount::MountPayload>) {
        self.backend.bind_mount_payload(payload);
    }
}

/// Mount a FAT volume read-only.
///
/// Every mutating `FsOps` call returns `EROFS`.
pub fn mount_fat_read_only<I>(image: I) -> Result<MountedFat<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_fat(image, true)
}

/// Mount a FAT volume read-write (deferred — all mutating ops return ENOSYS for now).
pub fn mount_fat_read_write<I>(image: I) -> Result<MountedFat<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_fat(image, false)
}

fn open_fat<I>(image: I, read_only: bool) -> Result<MountedFat<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    let backend = FatFsInstance::open(image, read_only)?;

    // Determine root cluster and meta
    let bpb = &backend.with_pager(|p| Ok(p.bpb.clone()))?;
    use tx_fat_format::ondisk::FatType;
    let root_cluster = match bpb.fat_type {
        FatType::FAT32 => bpb.root_cluster,
        _ => FAT_ROOT_CLUSTER_SENTINEL,
    };

    let root_entries = backend.with_pager(|p| {
        if bpb.fat_type == FatType::FAT32 {
            p.read_dir_entries(root_cluster)
        } else {
            p.read_root_dir_entries()
        }
    })?;

    // The root directory itself is represented by the first entry (the volume label)
    // or a synthetic entry. We synthesize InodeMeta for the root.
    let root_fs_object_id = fs_object_id(root_cluster, 0);
    let root_inode_meta = InodeMeta {
        mode: 0o555 | 0o040000, // S_IFDIR | 0555
        uid: 0,
        gid: 0,
        size: (root_entries.len() * 32) as u64, // approximate
        atime: tx_subsystems::vfs::structure::Timespec { sec: 0, nsec: 0 },
        mtime: tx_subsystems::vfs::structure::Timespec { sec: 0, nsec: 0 },
        ctime: tx_subsystems::vfs::structure::Timespec { sec: 0, nsec: 0 },
        nlinks: 1,
        blocks: 0,
        flags: 0,
    };

    Ok(MountedFat {
        backend,
        root_fs_object_id,
        root_inode_meta,
    })
}
