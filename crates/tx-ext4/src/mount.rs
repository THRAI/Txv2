use core::marker::Send;

use crate::adapter::step_engine::Cap;
use alloc::sync::Arc;
use tx_ext4_format::pager::BlockImage;
use tx_ext4_format::pager::Ext4Pager;
use tx_ext4_format::{
    capability::{CapabilityProfileHash, Tier1Capabilities, Tier1MountFacts},
    ondisk::Superblock,
};
use tx_subsystems::execution::Errno;
use tx_subsystems::fs_iface::BackendPlanner;
use tx_subsystems::io_manager::block::DeviceKey;
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::vfs::structure::{FsObjectId, InodeMeta};
use tx_subsystems::vfs::FsOps;

use crate::journal::{
    JournalFsyncSource, JournalMutationRuntime, JournalMutationWriteSource, JournalPagePool,
};
use crate::planner::{Ext4BlockGeometry, Ext4PlannerBinding};
pub use crate::read_backend::FilePageContainerBinder;
use crate::read_backend::{
    map_inode_meta, Ext4FsInstance, Ext4PagerMutationPlanSource, EXT4_ROOT_INODE,
};
use tx_ext4_format::recover_if_required;

pub struct MountedExt4<I> {
    backend: Arc<Ext4FsInstance<I>>,
    pub root_fs_object_id: FsObjectId,
    pub root_inode_meta: InodeMeta,
}

impl<I> MountedExt4<I>
where
    I: BlockImage + Send + 'static,
{
    pub fn fs_ops(&self) -> Arc<dyn FsOps> {
        self.backend.clone().fs_ops_arc()
    }

    pub fn fs_page_backing(&self) -> Arc<dyn FsPageBacking> {
        self.backend.clone().fs_page_backing_arc()
    }

    pub fn backend_planner(&self) -> Option<Arc<dyn BackendPlanner>> {
        self.backend.backend_planner()
    }

    pub fn bind_mount_payload(&self, payload: &Cap<tx_subsystems::mount::MountPayload>) {
        self.backend.bind_mount_payload(payload);
    }

    pub fn set_file_page_container_binder(&self, binder: Option<Arc<dyn FilePageContainerBinder>>) {
        self.backend.set_file_page_container_binder(binder);
    }

    pub fn capability_profile_hash(&self) -> Option<CapabilityProfileHash> {
        self.backend.capability_profile_hash()
    }
}

/// Wire type so the tx-fs bridge crate can name the mounted
/// type without depending on the full `tx_ext4` lib.
pub type Ext4MountWire = MountedExt4<tx_ext4_format::pager::Page4K>;

/// Mount an ext4 image read-only.
///
/// Every mutating `FsOps` call against the returned `MountedExt4`
/// returns `EROFS`, mirroring Linux `MS_RDONLY`. Use this for media
/// that must not be modified (initramfs overlays, sealed boot
/// images, recovery partitions).
pub fn mount_ext4_read_only<I>(image: I) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_ext4_with_backend_planner(image, true, None)
}

/// Mount an ext4 image read-only with an optional I/O-manager planner.
///
/// `None` preserves the compatibility pager path. A concrete device bridge
/// supplies the planner only after it has a stable geometry and mapping source.
pub fn mount_ext4_read_only_with_backend_planner<I>(
    image: I,
    backend_planner: Option<Arc<dyn BackendPlanner>>,
) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_ext4_with_backend_planner(image, true, backend_planner)
}

/// Mount ext4 with the paired L5 planner and metadata cache used by the I/O
/// manager. Namespace metadata lookup seeds inode extent roots before L4 page
/// misses ask the planner to resolve file pages.
pub fn mount_ext4_read_only_with_io_manager_planner<I>(
    image: I,
    geometry: Ext4BlockGeometry,
) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_ext4_with_planner_binding(image, true, Ext4PlannerBinding::new(geometry))
}

/// Mount an ext4 image read-write.
///
/// `FsOps::create_inode` / `mkdir` / `unlink` / `rename` / `link`
/// flow through the existing pager surface
/// (`tx_ext4_format::pager::create_regular_file` etc.) and persist
/// via `BlockImage::write_block`. Crash-consistency guarantees match
/// the format crate's current journaling story — sufficient for a
/// graceful unmount but not against a hard power loss.
pub fn mount_ext4_read_write<I>(image: I) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_ext4_with_backend_planner(image, false, None)
}

/// Read-write counterpart of [`mount_ext4_read_only_with_backend_planner`].
pub fn mount_ext4_read_write_with_backend_planner<I>(
    image: I,
    backend_planner: Option<Arc<dyn BackendPlanner>>,
) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_ext4_with_backend_planner(image, false, backend_planner)
}

/// Read-write counterpart of [`mount_ext4_read_only_with_io_manager_planner`].
/// Buffered writeback remains deferred to Phase 6D; this only establishes the
/// read-planner binding and root-seeding contract.
pub fn mount_ext4_read_write_with_io_manager_planner<I>(
    image: I,
    geometry: Ext4BlockGeometry,
) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_ext4_with_planner_binding(image, false, Ext4PlannerBinding::new(geometry))
}

/// Read-write ext4 mount with an L5 JBD2 fsync source.
///
/// The caller retains `journal_fsync` to stage prepared transactions; the
/// mounted backend planner holds a second strong reference to submit their
/// commit graphs and receive terminal completion notifications.
pub fn mount_ext4_read_write_with_journal_io_manager_planner<I>(
    image: I,
    geometry: Ext4BlockGeometry,
    journal_fsync: Arc<JournalFsyncSource>,
) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_ext4_with_planner_binding(
        image,
        false,
        Ext4PlannerBinding::with_fsync_plan_source(geometry, journal_fsync),
    )
}

/// Read-write ext4 mount whose buffered writeback is admitted into the
/// mount-local JBD2 runtime before L4 submits ordered data I/O.
pub fn mount_ext4_read_write_with_mutation_journal_io_manager_planner<I>(
    image: I,
    geometry: Ext4BlockGeometry,
    runtime: Arc<JournalMutationRuntime>,
) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    let mutation_provider = Ext4PagerMutationPlanSource::new();
    let source = runtime.source();
    let metadata_runtime = Arc::clone(&runtime);
    let binding = Ext4PlannerBinding::with_plan_sources(
        geometry,
        source.clone(),
        JournalMutationWriteSource::new(mutation_provider.clone(), runtime),
    );
    let mounted = open_ext4_with_planner_binding(image, false, binding)?;
    mounted.backend.disable_legacy_writeback();
    mutation_provider.bind(&mounted.backend);
    mounted
        .backend
        .bind_metadata_mutation_runtime(metadata_runtime);
    source.bind_settlement_observer(mounted.backend.clone());
    Ok(mounted)
}

/// Build a mutation-journal mount from the image's own JBD2 geometry.
pub fn mount_ext4_read_write_with_discovered_journal<I>(
    image: I,
    geometry: Ext4BlockGeometry,
    device: DeviceKey,
    pool: JournalPagePool,
) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    validate_tier1_rw_profile(&image)?;
    let mut pager = Ext4Pager::open(image).map_err(|_| Errno::EIO)?;
    let superblock = pager.superblock();
    let journal_geometry = pager.journal_geometry().map_err(|_| Errno::EIO)?;
    let mut image = pager.into_inner();
    let _recovery =
        recover_if_required(&mut image, &superblock, &journal_geometry).map_err(|_| Errno::EIO)?;
    let mut pager = Ext4Pager::open(image).map_err(|_| Errno::EIO)?;
    pager.mark_recovery_required().map_err(|_| Errno::EIO)?;
    let journal_geometry = pager.journal_geometry().map_err(|_| Errno::EIO)?;
    let next_sequence = journal_geometry.superblock.sequence;
    let image = pager.into_inner();
    let source = Arc::new(JournalFsyncSource::new());
    let runtime = Arc::new(
        JournalMutationRuntime::from_geometry_with_sequence(
            source,
            pool,
            device,
            geometry.sectors_per_block,
            journal_geometry,
            next_sequence,
        )
        .map_err(|_| Errno::EIO)?,
    );
    mount_ext4_read_write_with_mutation_journal_io_manager_planner(image, geometry, runtime)
}

fn open_ext4_with_backend_planner<I>(
    image: I,
    read_only: bool,
    backend_planner: Option<Arc<dyn BackendPlanner>>,
) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_ext4_with_backend_planner_and_mapping(image, read_only, backend_planner, None)
}

fn open_ext4_with_planner_binding<I>(
    image: I,
    read_only: bool,
    binding: Ext4PlannerBinding,
) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    open_ext4_with_backend_planner_and_mapping(
        image,
        read_only,
        Some(binding.planner()),
        Some(binding.mapping()),
    )
}

fn open_ext4_with_backend_planner_and_mapping<I>(
    image: I,
    read_only: bool,
    backend_planner: Option<Arc<dyn BackendPlanner>>,
    extent_mapping: Option<Arc<crate::planner::Ext4MappingTable>>,
) -> Result<MountedExt4<I>, Errno>
where
    I: BlockImage + Send + 'static,
{
    let profile_hash = if read_only {
        None
    } else {
        Some(validate_tier1_rw_profile(&image)?)
    };
    let backend = Ext4FsInstance::open_with_backend_planner_and_mapping(
        image,
        read_only,
        backend_planner,
        extent_mapping,
    )?;
    if let Some(profile_hash) = profile_hash {
        backend.set_capability_profile_hash(profile_hash);
    }
    let root_fs_object_id = FsObjectId::new(EXT4_ROOT_INODE as u64);
    let root_inode_meta = backend
        .with_pager(|pager| pager.inode_meta(tx_ext4_format::pager::InodeNo::new(EXT4_ROOT_INODE)))
        .map(map_inode_meta)?;

    Ok(MountedExt4 {
        backend,
        root_fs_object_id,
        root_inode_meta,
    })
}

fn validate_tier1_rw_profile<I: BlockImage>(image: &I) -> Result<CapabilityProfileHash, Errno> {
    let mut block = [0; tx_ext4_format::pager::BLOCK_SIZE];
    image.read_block(0, &mut block).map_err(|_| Errno::EIO)?;
    let superblock = Superblock::parse(&block[1024..2048]).map_err(|_| Errno::EIO)?;
    Tier1Capabilities::generated()
        .admit_mount(Tier1MountFacts::from_superblock(&superblock))
        .map_err(|_| Errno::EOPNOTSUPP)
}
