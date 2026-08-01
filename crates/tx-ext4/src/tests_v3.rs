//! Wave-9b: `FsOps` + `FsPageBacking` shape tests against
//! `Ext4FsInstance`.
//!
//! Pins the v3 outcome shape end-to-end through both v3 traits on
//! the on-disk ext4 backend. The mock image is a synchronous
//! `BlockImage` (mirrors the format-crate `MemImage` fixture in
//! `crates/tx-ext4-format/tests/pager_mock.rs`) so the unit tests
//! drive the same `Ext4FsInstance::open` path that `mount_ext4_read_only`
//! exercises in production.
//!
//! Cites: `docs/progress/decisions/2026-05-09-fsops-v3-design.md`
//! (wave 8/9 design); `txdoc:STEP-V2-OUTCOME-ALGEBRA-1` (closed
//! four-variant outcome); `tmpfs/tests.rs` (canonical wave-9a
//! reference).

#![cfg(test)]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::adapter::step_engine::{
    self as epoch, page_allocator, Errno as V3Errno, NoProgress, StepOutcome as V3,
};
use tx_ext4_format::ondisk::{Extent, GroupDesc, Inode, Superblock};
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_subsystems::fs_iface::{
    BackendPageRequest, BackendPlan, FsObjectKey, IoDataLeaseId, IoDataSource, IoDataTarget,
    PageFrameRef,
};
use tx_subsystems::io_manager::block::DeviceKey;
use tx_subsystems::io_manager::page::{
    PageGeneration, PageIoFlags, PageIoOp, PageIoRange, PageIoRequestId,
};
use tx_subsystems::mount::{DevId, MountOptions, MountPayload, SourceLabel};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::vfs::structure::{DirCursor, FsObjectId, InodeMeta};
use tx_subsystems::vfs::FsOps;

use crate::planner::{Ext4BlockGeometry, Ext4FsyncPlanSource, Ext4PlannerBinding};
use crate::read_backend::{Ext4FsInstance, Ext4PagerMutationPlanSource};
use crate::{
    journal::{
        Ext4MutationPlanSource, JournalFsyncSource, JournalMutationRuntime, JournalPagePool,
        JournalRecordLayout, MutationJournalLayout,
    },
    mount::{
        mount_ext4_read_only, mount_ext4_read_write,
        mount_ext4_read_write_with_mutation_journal_io_manager_planner,
    },
};

/// Shared serialisation lock. Mirrors `tx_fs::test_support::FS_TEST_LOCK`:
/// every test in this crate's lib binary observes the same per-CPU
/// epoch slot in `tx_substrate`, so tests cannot concurrently create
/// epoch guards (the substrate enforces no-nesting on the same logical
/// CPU; see `crates/tx-substrate/src/epoch/local.rs`). Every test grabs
/// this lock before touching epoch / zone state.
static EXT4_V3_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// === Mock BlockImage ===================================================
//
// Minimal sync `BlockImage` impl matching the format-crate test
// fixture. Unit tests live in-crate so they can reach the
// `pub(crate) Ext4FsInstance::open` constructor; the integration test
// in `tests/async_adapter.rs` covers the parallel `Ext4Async` API.

#[derive(Clone)]
struct MemImage {
    blocks: Vec<Page4K>,
}

impl MemImage {
    fn new(n: usize) -> Self {
        Self {
            blocks: alloc::vec![[0u8; BLOCK_SIZE]; n],
        }
    }

    fn block_mut(&mut self, idx: u64) -> &mut Page4K {
        &mut self.blocks[idx as usize]
    }
}

impl BlockImage for MemImage {
    fn total_blocks(&self) -> u64 {
        self.blocks.len() as u64
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> tx_ext4_format::Result<()> {
        let src = self
            .blocks
            .get(block as usize)
            .ok_or(tx_ext4_format::Ext4FormatError::OutOfBounds)?;
        out.copy_from_slice(src);
        Ok(())
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> tx_ext4_format::Result<()> {
        let dst = self
            .blocks
            .get_mut(block as usize)
            .ok_or(tx_ext4_format::Ext4FormatError::OutOfBounds)?;
        dst.copy_from_slice(data);
        Ok(())
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        Ok(())
    }
}

struct CountingImage {
    image: MemImage,
    writes: Arc<AtomicUsize>,
}

impl BlockImage for CountingImage {
    fn total_blocks(&self) -> u64 {
        self.image.total_blocks()
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> tx_ext4_format::Result<()> {
        self.image.read_block(block, out)
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> tx_ext4_format::Result<()> {
        self.writes.fetch_add(1, Ordering::AcqRel);
        self.image.write_block(block, data)
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        self.image.barrier()
    }
}

fn init_substrate() {
    tx_test_support::init_host();
    tx_subsystems::zones::register_all().expect("tx-subsystems zones");
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for tx-ext4 v3 tests: {error:?}"),
    }
}

fn write_inode_at(image: &mut MemImage, ino: u32, inode: &Inode) {
    let index = (ino - 1) as usize;
    let offset = index * 256;
    let block = 4 + offset / BLOCK_SIZE;
    let in_block = offset % BLOCK_SIZE;
    inode
        .encode(&mut image.block_mut(block as u64)[in_block..in_block + 256])
        .unwrap();
}

fn encode_dir(block: &mut Page4K, entries: &[(u32, u8, &[u8])]) {
    block.fill(0);
    let mut offset = 0usize;
    for (idx, (inode, file_type, name)) in entries.iter().enumerate() {
        let rec_len = if idx == entries.len() - 1 {
            (BLOCK_SIZE - offset) as u16
        } else {
            (8 + name.len()).next_multiple_of(4) as u16
        };
        tx_ext4_format::ondisk::encode_dir_entry(
            *inode,
            rec_len,
            *file_type,
            name,
            &mut block[offset..],
        )
        .unwrap();
        offset += rec_len as usize;
    }
}

fn build_image() -> MemImage {
    let mut image = MemImage::new(64);

    let sb = Superblock {
        inodes_count: 64,
        blocks_count: 64,
        free_blocks_count: 32,
        log_block_size: 2,
        blocks_per_group: 64,
        inodes_per_group: 64,
        inode_size: 256,
        feature_incompat: Superblock::FEATURE_INCOMPAT_EXTENTS,
        feature_ro_compat: Superblock::FEATURE_RO_COMPAT_HUGE_FILE,
        journal_inode: 8,
        ..Superblock::default()
    };
    sb.encode(&mut image.block_mut(0)[1024..2048]).unwrap();

    GroupDesc {
        block_bitmap: 2,
        inode_bitmap: 3,
        inode_table: 4,
        free_blocks_count: 32,
        free_inodes_count: 52,
        used_dirs_count: 1,
        ..GroupDesc::default()
    }
    .encode(&mut image.block_mut(1)[..64])
    .unwrap();

    // root inode (ino 2): directory containing "hello" (ino 12).
    let mut root_inode = Inode::default();
    root_inode.mode = 0x4000 | 0o755;
    root_inode.size = BLOCK_SIZE as u64;
    root_inode.blocks_512 = 8;
    root_inode.links_count = 2;
    root_inode.flags = Inode::EXTENTS_FL;
    root_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 16,
        }])
        .unwrap();
    write_inode_at(&mut image, 2, &root_inode);

    // file inode (ino 12): regular file with 1 page mapped to block 20.
    let mut file_inode = Inode::default();
    file_inode.mode = 0x8000 | 0o644;
    file_inode.uid = 1000;
    file_inode.gid = 1000;
    file_inode.size = BLOCK_SIZE as u64;
    file_inode.links_count = 1;
    file_inode.blocks_512 = 8;
    file_inode.flags = Inode::EXTENTS_FL;
    file_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 20,
        }])
        .unwrap();
    write_inode_at(&mut image, 12, &file_inode);

    encode_dir(
        image.block_mut(16),
        &[
            (2, 2, b".".as_slice()),
            (2, 2, b"..".as_slice()),
            (12, 1, b"hello".as_slice()),
        ],
    );
    *image.block_mut(20) = [0x20; BLOCK_SIZE];

    image
}

fn build_tier1_mount_image() -> MemImage {
    let mut image = build_image();
    let mut superblock = Superblock::parse(&image.block_mut(0)[1024..2048]).expect("superblock");
    superblock.feature_compat = 0x0004;
    superblock.feature_ro_compat |= Superblock::FEATURE_RO_COMPAT_METADATA_CSUM;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .expect("encode Tier 1 superblock");
    image
}

fn open_fs() -> Arc<Ext4FsInstance<MemImage>> {
    Ext4FsInstance::open(build_image(), false).expect("open ext4 mem image")
}

fn open_fs_read_only() -> Arc<Ext4FsInstance<MemImage>> {
    Ext4FsInstance::open(build_image(), true).expect("open ext4 mem image (RO)")
}

fn open_fs_with_io_manager_binding() -> (Arc<Ext4FsInstance<MemImage>>, Ext4PlannerBinding) {
    let binding = Ext4PlannerBinding::new(Ext4BlockGeometry::new(DeviceKey::new(7), 8));
    let fs = Ext4FsInstance::open_with_backend_planner_and_mapping(
        build_image(),
        false,
        Some(binding.planner()),
        Some(binding.mapping()),
    )
    .expect("open ext4 mem image with planner binding");
    (fs, binding)
}

fn mutation_runtime_for_test(sequence: u32) -> Arc<JournalMutationRuntime> {
    let fsync = Arc::new(JournalFsyncSource::new());
    Arc::new(JournalMutationRuntime::new(
        fsync,
        JournalPagePool::new(10).expect("journal pool"),
        MutationJournalLayout::new(
            DeviceKey::new(7),
            8,
            [1; 16],
            sequence,
            JournalRecordLayout::new(
                tx_subsystems::io_manager::block::LbaRange::new(80, 8),
                alloc::vec![tx_subsystems::io_manager::block::LbaRange::new(88, 8)],
                tx_subsystems::io_manager::block::LbaRange::new(120, 8),
            ),
        ),
    ))
}

fn mounted_counting_mutation_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test(sequence);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_mount_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 mutation ext4 image");
    (mounted, runtime, writes)
}

#[test]
fn ext4_mutation_mount_rejects_a_non_tier1_fixture() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fsync = Arc::new(JournalFsyncSource::new());
    let runtime = Arc::new(JournalMutationRuntime::new(
        Arc::clone(&fsync),
        JournalPagePool::new(10).expect("journal pool"),
        MutationJournalLayout::new(
            DeviceKey::new(7),
            8,
            [1; 16],
            7,
            JournalRecordLayout::new(
                tx_subsystems::io_manager::block::LbaRange::new(80, 8),
                alloc::vec![
                    tx_subsystems::io_manager::block::LbaRange::new(88, 8),
                    tx_subsystems::io_manager::block::LbaRange::new(96, 8),
                    tx_subsystems::io_manager::block::LbaRange::new(104, 8),
                    tx_subsystems::io_manager::block::LbaRange::new(112, 8),
                ],
                tx_subsystems::io_manager::block::LbaRange::new(120, 8),
            ),
        ),
    ));
    let result = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        build_image(),
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        runtime,
    );
    assert!(matches!(result, Err(V3Errno::EOPNOTSUPP)));
}

#[test]
fn ext4_metadata_serialize_admits_a_journal_mutation_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let (mounted, runtime, writes) = mounted_counting_mutation_fs(17);
    let guard = epoch::guard();
    let fs_ops = mounted.fs_ops();
    let mut meta = match fs_ops.load_inode_meta(FsObjectId::new(12), &guard) {
        V3::Done(meta) => meta,
        other => panic!("load meta for setattr admission: {other:?}"),
    };
    meta.mode = 0o100600;

    assert_eq!(
        fs_ops.serialize_inode_meta(FsObjectId::new(12), &meta, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(17)
    );
    assert!(matches!(
        runtime.source().plan_fsync(&BackendPageRequest::new(
            FsObjectKey::new(12),
            PageIoRequestId::new(91),
            PageIoRange::new(0, 1),
            PageIoOp::Fsync,
            PageIoFlags::BARRIER,
            None,
        )),
        BackendPlan::SubmitGraph(_)
    ));
}

#[test]
fn ext4_chmod_and_chown_public_paths_admit_metadata_mutations() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();

    let (chmod_mount, chmod_runtime, chmod_writes) = mounted_counting_mutation_fs(18);
    let chmod_ops = chmod_mount.fs_ops();
    assert_eq!(
        chmod_ops.chmod_inode(FsObjectId::new(12), 0o600, &cred, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(chmod_writes.load(Ordering::Acquire), 0);
    assert_eq!(
        chmod_runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(18)
    );

    let (chown_mount, chown_runtime, chown_writes) = mounted_counting_mutation_fs(19);
    let chown_ops = chown_mount.fs_ops();
    assert_eq!(
        chown_ops.chown_inode(FsObjectId::new(12), Some(1000), Some(1000), &cred, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(chown_writes.load(Ordering::Acquire), 0);
    assert_eq!(
        chown_runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(19)
    );
}

#[test]
fn ext4_mapped_write_mutation_requires_a_mutation_owner() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let provider = Ext4PagerMutationPlanSource::new();
    provider.bind(&fs);
    let request = BackendPageRequest::new(
        FsObjectKey::new(12),
        PageIoRequestId::new(71),
        PageIoRange::new(0, 1),
        PageIoOp::Writeback,
        PageIoFlags::WRITEBACK,
        Some(PageGeneration::new(8)),
    );

    assert_eq!(
        provider.plan_writeback_mutation(&request),
        Err(V3Errno::EOPNOTSUPP)
    );
}

// === Tests =============================================================

#[test]
fn ext4_v3_lookup_round_trips_through_v3_outcome() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let guard = epoch::guard();

    // Existing name → Done(file_id).
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::lookup(&*fs, FsObjectId::new(2), b"hello", &guard),
        V3::<_, NoProgress>::done(FsObjectId::new(12))
    );

    // Missing name → Err(ENOENT) bridged through the v3 errno.
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::lookup(&*fs, FsObjectId::new(2), b"missing", &guard),
        V3::<FsObjectId, NoProgress>::err(V3Errno::ENOENT)
    );
}

#[test]
fn ext4_v3_load_inode_meta_returns_done_for_real_inode() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let guard = epoch::guard();

    let meta = match <Ext4FsInstance<MemImage> as FsOps>::load_inode_meta(
        &*fs,
        FsObjectId::new(12),
        &guard,
    ) {
        V3::Done(m) => m,
        other => panic!("load_inode_meta v3: {other:?}"),
    };
    assert_eq!(meta.size, BLOCK_SIZE as u64);
    assert_eq!(meta.uid, 1000);
}

#[test]
fn ext4_inode_metadata_seeds_l5_extent_root_for_page_planning() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let (fs, binding) = open_fs_with_io_manager_binding();
    let guard = epoch::guard();

    assert!(matches!(
        <Ext4FsInstance<MemImage> as FsOps>::load_inode_meta(&*fs, FsObjectId::new(12), &guard,),
        V3::Done(_)
    ));

    let request = BackendPageRequest::new_with_source_and_target(
        FsObjectKey::new(12),
        PageIoRequestId::new(41),
        PageIoRange::new(0, 1),
        PageIoOp::Read,
        PageIoFlags::DEMAND,
        Some(PageGeneration::new(1)),
        IoDataSource::None,
        IoDataTarget::page_cache(
            IoDataLeaseId::new(1),
            PageFrameRef::new(tx_hal::Ppn(0x42)),
            0,
            BLOCK_SIZE as u32,
        ),
    );

    let BackendPlan::SubmitBios(bios) = binding.planner().plan_page_io(request) else {
        panic!("seeded inline extent root must plan the file-data bio");
    };
    assert_eq!(bios.as_slice()[0].lba.start_lba(), 20 * 8);
}

#[test]
fn checkpoint_settlement_clears_mount_mapping_and_namespace_caches() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let (fs, binding) = open_fs_with_io_manager_binding();
    let guard = epoch::guard();

    assert!(matches!(
        <Ext4FsInstance<MemImage> as FsOps>::lookup(&*fs, FsObjectId::new(2), b"hello", &guard),
        V3::Done(_)
    ));
    assert!(matches!(
        <Ext4FsInstance<MemImage> as FsOps>::load_inode_meta(&*fs, FsObjectId::new(12), &guard),
        V3::Done(_)
    ));
    binding
        .mapping()
        .insert(12, 0, crate::planner::Ext4ReadMapping::Hole);

    assert!(binding.mapping().len() > 0);
    assert_ne!(fs.cache_entry_counts(), (0, 0, 0));

    fs.settle_metadata_caches();

    assert_eq!(binding.mapping().len(), 0);
    assert_eq!(fs.cache_entry_counts(), (0, 0, 0));
}

#[test]
fn ext4_file_and_mount_settlement_hooks_clear_mount_caches() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let (fs, binding) = open_fs_with_io_manager_binding();
    let guard = epoch::guard();

    assert!(matches!(
        <Ext4FsInstance<MemImage> as FsOps>::lookup(&*fs, FsObjectId::new(2), b"hello", &guard),
        V3::Done(_)
    ));
    assert!(matches!(
        <Ext4FsInstance<MemImage> as FsOps>::load_inode_meta(&*fs, FsObjectId::new(12), &guard),
        V3::Done(_)
    ));
    binding
        .mapping()
        .insert(12, 0, crate::planner::Ext4ReadMapping::Hole);
    assert!(binding.mapping().len() > 0);
    assert_ne!(fs.cache_entry_counts(), (0, 0, 0));

    let generation_frontier = tx_subsystems::page_backed::FileFsyncFrontier::empty();
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::settle_file(
            &*fs,
            FsObjectId::new(12),
            &generation_frontier,
            &guard
        ),
        V3::done(())
    );
    assert_eq!(binding.mapping().len(), 0);
    assert_eq!(fs.cache_entry_counts(), (0, 0, 0));

    assert!(matches!(
        <Ext4FsInstance<MemImage> as FsOps>::lookup(&*fs, FsObjectId::new(2), b"hello", &guard),
        V3::Done(_)
    ));
    binding
        .mapping()
        .insert(12, 0, crate::planner::Ext4ReadMapping::Hole);
    assert!(binding.mapping().len() > 0);

    let transaction_frontier = tx_subsystems::mount::MountTransactionFrontier::new(0);
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::settle_mount(&*fs, transaction_frontier, &guard),
        V3::done(())
    );
    assert_eq!(binding.mapping().len(), 0);
    assert_eq!(fs.cache_entry_counts(), (0, 0, 0));
}

#[test]
fn ext4_shutdown_settles_mount_caches_and_releases_mount_pin() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let payload = MountPayload::new_cap(
        fs.clone() as Arc<dyn FsOps>,
        fs.clone() as Arc<dyn FsPageBacking>,
        None,
        DevId::new(90),
        MountOptions::default(),
        "ext4",
        SourceLabel::Static("test"),
    )
    .expect("mount payload");
    fs.bind_mount_payload(&payload);
    assert_eq!(payload.payload_pin_count(), 1);

    let guard = epoch::guard();
    assert!(matches!(
        <Ext4FsInstance<MemImage> as FsOps>::lookup(&*fs, FsObjectId::new(2), b"hello", &guard),
        V3::Done(_)
    ));
    assert!(matches!(
        <Ext4FsInstance<MemImage> as FsOps>::load_inode_meta(&*fs, FsObjectId::new(12), &guard),
        V3::Done(_)
    ));
    assert_ne!(fs.cache_entry_counts(), (0, 0, 0));

    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::shutdown(&*fs, &guard),
        V3::done(())
    );

    assert_eq!(payload.payload_pin_count(), 0);
    assert_eq!(fs.cache_entry_counts(), (0, 0, 0));
}

#[test]
fn ext4_v3_mutation_methods_require_a_mutation_owner() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();

    let result = <Ext4FsInstance<MemImage> as FsOps>::create_inode(
        &*fs,
        FsObjectId::new(2),
        b"new",
        0o100644,
        &cred,
        &guard,
    );
    assert_eq!(
        result,
        V3::<(FsObjectId, InodeMeta), NoProgress>::err(V3Errno::EOPNOTSUPP)
    );

    let result = <Ext4FsInstance<MemImage> as FsOps>::mkdir(
        &*fs,
        FsObjectId::new(2),
        b"newdir",
        0o755,
        &cred,
        &guard,
    );
    assert_eq!(
        result,
        V3::<(FsObjectId, InodeMeta), NoProgress>::err(V3Errno::EOPNOTSUPP)
    );

    // destroy_inode is still unimplemented.
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::destroy_inode(&*fs, FsObjectId::new(12), &guard),
        V3::<(), NoProgress>::err(V3Errno::ENOSYS)
    );
}

#[test]
fn ext4_materialise_new_regular_file_requires_a_mutation_owner() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();

    let result = <Ext4FsInstance<MemImage> as FsOps>::create_inode(
        &*fs,
        FsObjectId::new(2),
        b"iozone",
        0o100644,
        &cred,
        &guard,
    );
    assert_eq!(
        result,
        V3::<(FsObjectId, InodeMeta), NoProgress>::err(V3Errno::EOPNOTSUPP)
    );
}

#[test]
fn ext4_v3_mutation_methods_rejected_on_read_only_mount_with_erofs() {
    // Linux `MS_RDONLY` semantics: every mutating FsOps method must
    // short-circuit with `-EROFS` when the mount was opened
    // read-only. Read-only is signalled by passing `true` to
    // `Ext4FsInstance::open`; `mount_ext4_read_only` does this at
    // its sole call site.
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs_read_only();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();

    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::create_inode(
            &*fs,
            FsObjectId::new(2),
            b"new",
            0o100644,
            &cred,
            &guard,
        ),
        V3::<_, NoProgress>::err(V3Errno::EROFS),
        "create_inode on RO mount must return EROFS",
    );

    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::mkdir(
            &*fs,
            FsObjectId::new(2),
            b"newdir",
            0o755,
            &cred,
            &guard,
        ),
        V3::<_, NoProgress>::err(V3Errno::EROFS),
        "mkdir on RO mount must return EROFS",
    );

    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::unlink(
            &*fs,
            FsObjectId::new(2),
            b"hello",
            FsObjectId::new(12),
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EROFS),
        "unlink on RO mount must return EROFS",
    );

    // Read-side lookup still works — RO doesn't break observation.
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::lookup(&*fs, FsObjectId::new(2), b"hello", &guard),
        V3::<_, NoProgress>::done(FsObjectId::new(12)),
        "lookup on RO mount still succeeds",
    );
}

#[test]
fn production_namespace_without_mutation_owner_writes_nothing() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let writes = Arc::new(AtomicUsize::new(0));
    let fs = Ext4FsInstance::open(
        CountingImage {
            image: build_image(),
            writes: Arc::clone(&writes),
        },
        false,
    )
    .expect("open writable ext4 image without a mutation runtime");
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();
    let frame = tx_subsystems::page_backed::Frame::new(
        page_allocator::zero_frame_ppn().expect("zero frame"),
    );

    assert_eq!(
        <Ext4FsInstance<CountingImage> as FsOps>::serialize_inode_meta(
            &*fs,
            FsObjectId::new(12),
            &InodeMeta::new(tx_subsystems::vfs::structure::InodeKind::Regular, 0o644),
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EOPNOTSUPP),
    );
    assert_eq!(
        <Ext4FsInstance<CountingImage> as FsOps>::create_inode(
            &*fs,
            FsObjectId::new(2),
            b"new",
            0o100644,
            &cred,
            &guard,
        ),
        V3::<_, NoProgress>::err(V3Errno::EOPNOTSUPP),
    );
    assert_eq!(
        <Ext4FsInstance<CountingImage> as FsOps>::mkdir(
            &*fs,
            FsObjectId::new(2),
            b"newdir",
            0o755,
            &cred,
            &guard,
        ),
        V3::<_, NoProgress>::err(V3Errno::EOPNOTSUPP),
    );
    assert_eq!(
        <Ext4FsInstance<CountingImage> as FsOps>::unlink(
            &*fs,
            FsObjectId::new(2),
            b"hello",
            FsObjectId::new(12),
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EOPNOTSUPP),
    );
    assert_eq!(
        <Ext4FsInstance<CountingImage> as FsOps>::rename(
            &*fs,
            FsObjectId::new(2),
            b"hello",
            FsObjectId::new(2),
            b"renamed",
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EOPNOTSUPP),
    );
    assert_eq!(
        <Ext4FsInstance<CountingImage> as FsOps>::rmdir(
            &*fs,
            FsObjectId::new(2),
            b"empty",
            FsObjectId::new(13),
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EOPNOTSUPP),
    );
    assert_eq!(
        <Ext4FsInstance<CountingImage> as FsPageBacking>::flush_page(
            &*fs,
            FsObjectId::new(12),
            0,
            &frame,
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EOPNOTSUPP),
    );
    assert_eq!(
        <Ext4FsInstance<CountingImage> as FsPageBacking>::truncate(
            &*fs,
            FsObjectId::new(12),
            0,
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EOPNOTSUPP),
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
}

#[test]
fn rw_mount_requires_the_pinned_tier1_profile_but_ro_oracles_remain_open() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();

    assert!(matches!(
        mount_ext4_read_write(build_image()),
        Err(V3Errno::EOPNOTSUPP)
    ));
    assert!(mount_ext4_read_only(build_image()).is_ok());
}

#[test]
fn rw_mount_stores_the_accepted_tier1_profile_hash() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let mounted = mount_ext4_read_write(build_tier1_mount_image()).expect("Tier 1 RW mount");
    assert_eq!(
        mounted.capability_profile_hash().map(|hash| hash.0),
        Some(tx_ext4_format::capability::Tier1Capabilities::generated().profile_hash()),
    );
}

#[test]
fn ext4_v3_readdir_done_then_terminator() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let guard = epoch::guard();

    // First entry: index 0 → Done(Some(...)).
    let cursor0 = DirCursor([0u8; 16]);
    let (entry, next) = match <Ext4FsInstance<MemImage> as FsOps>::readdir(
        &*fs,
        FsObjectId::new(2),
        cursor0,
        &guard,
    ) {
        V3::Done(Some(out)) => out,
        other => panic!("readdir[0] v3: {other:?}"),
    };
    assert_eq!(entry.fs_object_id, FsObjectId::new(2)); // "."
    assert_ne!(next.0, [0u8; 16]);
}

#[test]
fn ext4_v3_fetch_page_returns_done_frame_for_aligned_offset() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let guard = epoch::guard();

    let frame = match <Ext4FsInstance<MemImage> as FsPageBacking>::fetch_page(
        &*fs,
        FsObjectId::new(12),
        0,
        &guard,
    ) {
        V3::Done(f) => f,
        other => panic!("fetch_page v3: {other:?}"),
    };
    // ppn is host-allocated; just confirm we got a real frame back
    // (no Blocked/Advanced surfaced through the v3 surface).
    let _ = frame.ppn();

    // Misaligned offset → EINVAL through the v3 errno bridge.
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsPageBacking>::fetch_page(
            &*fs,
            FsObjectId::new(12),
            17,
            &guard,
        ),
        V3::<tx_subsystems::page_backed::Frame, NoProgress>::err(V3Errno::EINVAL)
    );
}

#[test]
fn ext4_v3_truncate_requires_a_mutation_owner_while_clean_fsync_succeeds() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let guard = epoch::guard();

    assert_eq!(
        <Ext4FsInstance<MemImage> as FsPageBacking>::truncate(&*fs, FsObjectId::new(12), 0, &guard,),
        V3::<(), NoProgress>::err(V3Errno::EOPNOTSUPP)
    );
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsPageBacking>::fsync_file(&*fs, FsObjectId::new(12), &guard),
        V3::<(), NoProgress>::done(())
    );
}

#[test]
fn ext4_v3_factory_arcs_produce_dyn_v3_traits() {
    // Pin the wave-9c MountOutput cutover wiring shape: each factory
    // returns the canonical `Arc<dyn …V3>` that walker entry points
    // will populate `MountPayload::fs_ops` /
    // `…::fs_page_backing` from. Mirrors `Tmpfs::fs_ops_arc`
    // / `Tmpfs::fs_page_backing_arc`.
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let _ops_v3: Arc<dyn FsOps> = fs.clone().fs_ops_arc();
    let _pb_v3: Arc<dyn FsPageBacking> = fs.fs_page_backing_arc();
}
