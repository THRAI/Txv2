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
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::adapter::step_engine::{
    self as epoch, page_allocator, Errno as V3Errno, NoProgress, StepOutcome as V3,
};
use tx_ext4_format::journal::JBD2_BLOCK_SIZE;
use tx_ext4_format::ondisk::{BitmapMut, Extent, GroupDesc, Inode, Superblock};
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_subsystems::fs_iface::{
    BackendPageRequest, BackendPlan, FsObjectKey, IoDataLeaseId, IoDataSource, IoDataTarget,
    PageFrameRef,
};
use tx_subsystems::io_manager::block::{BioVec, DeviceKey};
use tx_subsystems::io_manager::page::{
    PageGeneration, PageIoFlags, PageIoOp, PageIoRange, PageIoRequestId,
};
use tx_subsystems::mount::{DevId, MountOptions, MountPayload, MountPayloadPin, SourceLabel};
use tx_subsystems::page_backed::{
    step_write, Frame, FsPageBacking, PageContainer, PageContainerKind, PageIndex,
};
use tx_subsystems::vfs::structure::{
    DirCursor, FsObjectId, InodeKind, InodeMeta, OpenFile, OpenFileFlags, RNode, RNodeBacking,
};
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

fn mark_inode_bitmap_used(image: &mut MemImage, count: usize) {
    let mut bitmap = BitmapMut::new(image.block_mut(3));
    for bit in 0..count {
        bitmap.set(bit).unwrap();
    }
}

fn mark_block_bitmap_used(image: &mut MemImage, count: usize) {
    let mut bitmap = BitmapMut::new(image.block_mut(2));
    for bit in 0..count {
        bitmap.set(bit).unwrap();
    }
}

fn build_image() -> MemImage {
    let mut image = MemImage::new(64);

    let sb = Superblock {
        inodes_count: 64,
        blocks_count: 64,
        free_blocks_count: 32,
        free_inodes_count: 52,
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
    BitmapMut::new(image.block_mut(2)).set(20).unwrap();

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

fn build_tier1_destroy_image() -> MemImage {
    let mut image = build_tier1_mount_image();
    mark_inode_bitmap_used(&mut image, 12);
    let mut victim = Inode::default();
    victim.mode = 0x8000 | 0o644;
    victim.uid = 1000;
    victim.gid = 1000;
    victim.size = BLOCK_SIZE as u64;
    victim.links_count = 0;
    victim.blocks_512 = 8;
    victim.flags = Inode::EXTENTS_FL;
    victim.dtime = 41;
    victim
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 20,
        }])
        .unwrap();
    write_inode_at(&mut image, 12, &victim);
    image
}

fn build_tier1_empty_dir_image() -> MemImage {
    let mut image = build_tier1_mount_image();

    let mut root_inode = Inode::default();
    root_inode.mode = 0x4000 | 0o755;
    root_inode.size = BLOCK_SIZE as u64;
    root_inode.blocks_512 = 8;
    root_inode.links_count = 3;
    root_inode.flags = Inode::EXTENTS_FL;
    root_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 16,
        }])
        .unwrap();
    write_inode_at(&mut image, 2, &root_inode);

    let mut empty_inode = Inode::default();
    empty_inode.mode = 0x4000 | 0o755;
    empty_inode.uid = 1000;
    empty_inode.gid = 1000;
    empty_inode.size = BLOCK_SIZE as u64;
    empty_inode.blocks_512 = 8;
    empty_inode.links_count = 2;
    empty_inode.flags = Inode::EXTENTS_FL;
    empty_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 21,
        }])
        .unwrap();
    write_inode_at(&mut image, 13, &empty_inode);

    encode_dir(
        image.block_mut(16),
        &[
            (2, 2, b".".as_slice()),
            (2, 2, b"..".as_slice()),
            (12, 1, b"hello".as_slice()),
            (13, 2, b"empty".as_slice()),
        ],
    );
    encode_dir(
        image.block_mut(21),
        &[(13, 2, b".".as_slice()), (2, 2, b"..".as_slice())],
    );
    BitmapMut::new(image.block_mut(2)).set(21).unwrap();
    image
}

fn build_tier1_rename_overwrite_image() -> MemImage {
    let mut image = build_tier1_mount_image();

    let mut other_inode = Inode::default();
    other_inode.mode = 0x8000 | 0o644;
    other_inode.uid = 1000;
    other_inode.gid = 1000;
    other_inode.size = BLOCK_SIZE as u64;
    other_inode.links_count = 1;
    other_inode.blocks_512 = 8;
    other_inode.flags = Inode::EXTENTS_FL;
    other_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 21,
        }])
        .unwrap();
    write_inode_at(&mut image, 14, &other_inode);

    encode_dir(
        image.block_mut(16),
        &[
            (2, 2, b".".as_slice()),
            (2, 2, b"..".as_slice()),
            (12, 1, b"hello".as_slice()),
            (14, 1, b"other".as_slice()),
        ],
    );
    *image.block_mut(21) = [0x21; BLOCK_SIZE];
    BitmapMut::new(image.block_mut(2)).set(21).unwrap();
    image
}

fn build_tier1_cross_dir_rename_image() -> MemImage {
    let mut image = build_tier1_mount_image();

    let mut root_inode = Inode::default();
    root_inode.mode = 0x4000 | 0o755;
    root_inode.size = BLOCK_SIZE as u64;
    root_inode.blocks_512 = 8;
    root_inode.links_count = 3;
    root_inode.flags = Inode::EXTENTS_FL;
    root_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 16,
        }])
        .unwrap();
    write_inode_at(&mut image, 2, &root_inode);

    let mut dir_inode = Inode::default();
    dir_inode.mode = 0x4000 | 0o755;
    dir_inode.uid = 1000;
    dir_inode.gid = 1000;
    dir_inode.size = BLOCK_SIZE as u64;
    dir_inode.blocks_512 = 8;
    dir_inode.links_count = 2;
    dir_inode.flags = Inode::EXTENTS_FL;
    dir_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 1,
            physical_start: 21,
        }])
        .unwrap();
    write_inode_at(&mut image, 13, &dir_inode);

    encode_dir(
        image.block_mut(16),
        &[
            (2, 2, b".".as_slice()),
            (2, 2, b"..".as_slice()),
            (12, 1, b"hello".as_slice()),
            (13, 2, b"target".as_slice()),
        ],
    );
    encode_dir(
        image.block_mut(21),
        &[(13, 2, b".".as_slice()), (2, 2, b"..".as_slice())],
    );
    BitmapMut::new(image.block_mut(2)).set(21).unwrap();
    image
}

fn build_tier1_create_image() -> MemImage {
    let mut image = build_tier1_mount_image();
    mark_inode_bitmap_used(&mut image, 13);
    image
}

fn build_tier1_mkdir_image() -> MemImage {
    let mut image = build_tier1_create_image();
    mark_block_bitmap_used(&mut image, 48);
    image
}

fn build_two_page_mapped_image() -> MemImage {
    let mut image = build_image();
    let mut file_inode = Inode::default();
    file_inode.mode = 0x8000 | 0o644;
    file_inode.uid = 1000;
    file_inode.gid = 1000;
    file_inode.size = 2 * BLOCK_SIZE as u64;
    file_inode.links_count = 1;
    file_inode.blocks_512 = 16;
    file_inode.flags = Inode::EXTENTS_FL;
    file_inode
        .set_extent_root(&[Extent {
            logical_block: 0,
            len: 2,
            physical_start: 20,
        }])
        .unwrap();
    write_inode_at(&mut image, 12, &file_inode);
    *image.block_mut(21) = [0x21; BLOCK_SIZE];
    BitmapMut::new(image.block_mut(2)).set(21).unwrap();
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
    mutation_runtime_for_test_with_metadata(sequence, 1, false)
}

fn mutation_runtime_for_test_with_metadata(
    sequence: u32,
    metadata_slots: u64,
    with_revoke: bool,
) -> Arc<JournalMutationRuntime> {
    let fsync = Arc::new(JournalFsyncSource::new());
    let metadata = (0..metadata_slots)
        .map(|slot| tx_subsystems::io_manager::block::LbaRange::new(88 + slot * 8, 8))
        .collect();
    let mut records = JournalRecordLayout::new(
        tx_subsystems::io_manager::block::LbaRange::new(80, 8),
        metadata,
        tx_subsystems::io_manager::block::LbaRange::new(120, 8),
    );
    if with_revoke {
        records = records.with_revoke(tx_subsystems::io_manager::block::LbaRange::new(128, 8));
    }
    Arc::new(JournalMutationRuntime::new(
        fsync,
        JournalPagePool::new(16).expect("journal pool"),
        MutationJournalLayout::new(DeviceKey::new(7), 8, [1; 16], sequence, records),
    ))
}

fn mutation_runtime_for_test_with_ring(
    sequence: u32,
    metadata_slots: u64,
    with_revoke: bool,
) -> Arc<JournalMutationRuntime> {
    let _ = metadata_slots;
    let _ = with_revoke;
    Arc::new(
        JournalMutationRuntime::from_geometry_with_sequence(
            Arc::new(JournalFsyncSource::new()),
            JournalPagePool::new(16).expect("journal pool"),
            DeviceKey::new(7),
            8,
            tx_ext4_format::pager::JournalGeometry {
                superblock: tx_ext4_format::journal::Jbd2Superblock {
                    block_type: 4,
                    block_size: JBD2_BLOCK_SIZE as u32,
                    max_len: 16,
                    first: 1,
                    sequence,
                    start: 0,
                    uuid: [1; 16],
                },
                blocks: vec![9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24],
                superblock_page: None,
            },
            sequence,
        )
        .expect("runtime with ring"),
    )
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

fn mounted_counting_truncate_free_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 4, true);
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

fn mounted_counting_write_growth_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 4, false);
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

fn mounted_counting_unlink_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 2, false);
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

fn mounted_counting_destroy_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 5, true);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_destroy_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 destroy mutation ext4 image");
    (mounted, runtime, writes)
}

fn mounted_counting_create_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 5, false);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_create_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 create mutation ext4 image");
    (mounted, runtime, writes)
}

fn mounted_counting_mkdir_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 7, false);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_mkdir_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 mkdir mutation ext4 image");
    (mounted, runtime, writes)
}

fn mounted_counting_mkdir_fs_with_ring(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_ring(sequence, 7, false);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_mkdir_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 mkdir mutation ext4 image with ring");
    (mounted, runtime, writes)
}

fn mounted_counting_symlink_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 5, false);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_create_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 symlink mutation ext4 image");
    (mounted, runtime, writes)
}

fn mounted_counting_link_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 2, false);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_mount_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 hard-link mutation ext4 image");
    (mounted, runtime, writes)
}

fn mounted_counting_rename_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 1, false);
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

fn mounted_counting_rename_overwrite_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 2, false);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_rename_overwrite_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 rename-overwrite mutation ext4 image");
    (mounted, runtime, writes)
}

fn mounted_counting_cross_dir_rename_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 2, false);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_cross_dir_rename_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 cross-dir rename mutation ext4 image");
    (mounted, runtime, writes)
}

fn mounted_counting_rmdir_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 2, false);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_empty_dir_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 empty-dir mutation ext4 image");
    (mounted, runtime, writes)
}

fn page_container_for_mounted_file<I>(
    mounted: &crate::mount::MountedExt4<I>,
    fs_object_id: FsObjectId,
    size_bytes: u64,
    page_count: u64,
) -> PageContainer
where
    I: BlockImage + Send + 'static,
{
    let mount = MountPayload::new_cap(
        mounted.fs_ops(),
        mounted.fs_page_backing(),
        None,
        DevId::new(8),
        MountOptions::default(),
        "ext4",
        SourceLabel::Static("ext4"),
    )
    .expect("ext4 mount payload");
    let pc = PageContainer::new(
        PageContainerKind::File {
            mount: MountPayloadPin::acquire(&epoch::PayloadCap::from_cap(mount)),
            fs_object_id,
        },
        page_count,
    );
    pc.set_size_bytes(size_bytes);
    pc
}

fn open_file_for_page_container(pc: &PageContainer) -> OpenFile {
    let pc = PageContainer::new_cap(pc.kind().clone(), pc.page_count())
        .expect("page container cap for open file");
    let rnode = RNode::new_cap(
        FsObjectId::new(700),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc },
    )
    .expect("rnode cap");
    OpenFile::new(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
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
fn ext4_truncate_public_path_admits_metadata_mutation_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_mutation_fs(20);
    let backing = mounted.fs_page_backing();

    assert_eq!(
        backing.truncate(FsObjectId::new(12), BLOCK_SIZE as u64 + 13, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(20)
    );
}

#[test]
fn ext4_truncate_cross_block_shrink_admits_free_revoke_mutation() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_truncate_free_fs(21);
    let backing = mounted.fs_page_backing();

    assert_eq!(
        backing.truncate(FsObjectId::new(12), 0, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(21)
    );
}

#[test]
fn ext4_flush_public_path_admits_mapped_writeback_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_mutation_fs(22);
    let backing = mounted.fs_page_backing();
    let frame = tx_subsystems::page_backed::Frame::new(
        page_allocator::zero_frame_ppn().expect("zero frame"),
    );

    assert_eq!(
        backing.flush_page(FsObjectId::new(12), 0, &frame, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(22)
    );
}

#[test]
fn ext4_flush_public_path_admits_bounded_hole_growth_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_write_growth_fs(23);
    let backing = mounted.fs_page_backing();
    let frame = tx_subsystems::page_backed::Frame::new(
        page_allocator::zero_frame_ppn().expect("zero frame"),
    );

    assert_eq!(
        backing.flush_page(FsObjectId::new(12), 4 * BLOCK_SIZE as u64, &frame, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(23)
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

#[test]
fn ext4_prepare_write_range_requires_a_mutation_owner_before_dirty_publication() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let guard = epoch::guard();

    assert_eq!(
        <Ext4FsInstance<MemImage> as FsPageBacking>::prepare_write_range(
            &*fs,
            FsObjectId::new(12),
            4 * BLOCK_SIZE as u64,
            BLOCK_SIZE,
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EOPNOTSUPP)
    );
}

#[test]
fn ext4_buffered_extending_write_reserves_before_dirty_and_flushes_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_write_growth_fs(24);
    let pc = page_container_for_mounted_file(&mounted, FsObjectId::new(12), BLOCK_SIZE as u64, 8);
    let of = open_file_for_page_container(&pc);
    of.set_offset(4 * BLOCK_SIZE as u64);

    assert_eq!(step_write(&pc, &of, 32, &guard), V3::done(32));
    assert_eq!(of.offset(), 4 * BLOCK_SIZE as u64 + 32);
    assert_eq!(mounted.buffered_write_reservation_count_for_test(), 1);
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert!(pc.page_marks(PageIndex::new(4)).expect("page 4").dirty);

    let ppn = pc.lookup(PageIndex::new(4)).expect("dirty page 4 resident");
    let frame = Frame::new(ppn);
    assert_eq!(
        mounted.fs_page_backing().flush_page(
            FsObjectId::new(12),
            4 * BLOCK_SIZE as u64,
            &frame,
            &guard
        ),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(mounted.buffered_write_reservation_count_for_test(), 0);
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(24)
    );
}

#[test]
fn ext4_unlink_public_path_admits_namespace_mutation_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_unlink_fs(25);

    assert_eq!(
        mounted
            .fs_ops()
            .unlink(FsObjectId::new(2), b"hello", FsObjectId::new(12), &guard,),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(25)
    );
}

#[test]
fn ext4_destroy_public_path_admits_zero_link_regular_inode_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_destroy_fs(26);

    assert_eq!(
        mounted.fs_ops().destroy_inode(FsObjectId::new(12), &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(26)
    );
}

#[test]
fn ext4_create_public_path_admits_regular_file_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();
    let (mounted, runtime, writes) = mounted_counting_create_fs(31);

    assert_eq!(
        mounted
            .fs_ops()
            .create_inode(FsObjectId::new(2), b"created", 0o100640, &cred, &guard,),
        V3::<_, NoProgress>::done((
            FsObjectId::new(14),
            InodeMeta {
                mode: 0o100640,
                uid: 0,
                gid: 0,
                size: 0,
                atime: Default::default(),
                mtime: Default::default(),
                ctime: Default::default(),
                nlinks: 1,
                blocks: 0,
                flags: Inode::EXTENTS_FL,
            },
        ))
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(31)
    );
}

#[test]
fn ext4_mkdir_public_path_admits_directory_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();
    let (mounted, runtime, writes) = mounted_counting_mkdir_fs(32);

    assert_eq!(
        mounted
            .fs_ops()
            .mkdir(FsObjectId::new(2), b"newdir", 0o755, &cred, &guard,),
        V3::<_, NoProgress>::done((
            FsObjectId::new(14),
            InodeMeta {
                mode: 0o40755,
                uid: 0,
                gid: 0,
                size: BLOCK_SIZE as u64,
                atime: Default::default(),
                mtime: Default::default(),
                ctime: Default::default(),
                nlinks: 2,
                blocks: 8,
                flags: Inode::EXTENTS_FL,
            },
        ))
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(32)
    );
}

#[test]
fn ext4_symlink_public_path_admits_fast_symlink_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();
    let (mounted, runtime, writes) = mounted_counting_symlink_fs(33);

    assert_eq!(
        mounted
            .fs_ops()
            .symlink(FsObjectId::new(2), b"alink", b"nested/child", &cred, &guard,),
        V3::<_, NoProgress>::done((
            FsObjectId::new(14),
            InodeMeta {
                mode: 0o120777,
                uid: 0,
                gid: 0,
                size: b"nested/child".len() as u64,
                atime: Default::default(),
                mtime: Default::default(),
                ctime: Default::default(),
                nlinks: 1,
                blocks: 0,
                flags: 0,
            },
        ))
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(33)
    );
}

#[test]
fn ext4_link_public_path_admits_namespace_mutation_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_link_fs(28);

    assert_eq!(
        mounted
            .fs_ops()
            .link(FsObjectId::new(2), b"alias", FsObjectId::new(12), &guard,),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(28)
    );
}

#[test]
fn ext4_rename_public_path_admits_same_dir_mutation_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_rename_fs(26);

    assert_eq!(
        mounted.fs_ops().rename(
            FsObjectId::new(2),
            b"hello",
            FsObjectId::new(2),
            b"moved",
            &guard,
        ),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(26)
    );
}

#[test]
fn ext4_rename_public_path_admits_same_dir_overwrite_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_rename_overwrite_fs(29);

    assert_eq!(
        mounted.fs_ops().rename(
            FsObjectId::new(2),
            b"hello",
            FsObjectId::new(2),
            b"other",
            &guard,
        ),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(29)
    );
}

#[test]
fn ext4_rename_public_path_admits_cross_dir_regular_file_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_cross_dir_rename_fs(30);

    assert_eq!(
        mounted.fs_ops().rename(
            FsObjectId::new(2),
            b"hello",
            FsObjectId::new(13),
            b"moved",
            &guard,
        ),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(30)
    );
}

#[test]
fn ext4_rmdir_public_path_admits_namespace_mutation_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_rmdir_fs(27);

    assert_eq!(
        mounted
            .fs_ops()
            .rmdir(FsObjectId::new(2), b"empty", FsObjectId::new(13), &guard,),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(27)
    );
}

#[test]
fn ext4_multi_page_direct_writeback_planner_accepts_mapped_batch() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = Ext4FsInstance::open(build_two_page_mapped_image(), false)
        .expect("open two-page ext4 mem image");
    fs.bind_metadata_mutation_runtime(mutation_runtime_for_test(22));
    let provider = Ext4PagerMutationPlanSource::new();
    provider.bind(&fs);
    let source = IoDataSource::direct(
        IoDataLeaseId::new(90),
        alloc::vec![
            BioVec::new(0x100, 0, BLOCK_SIZE as u32),
            BioVec::new(0x101, 0, BLOCK_SIZE as u32),
        ],
    );
    let request = BackendPageRequest::new_with_source(
        FsObjectKey::new(12),
        PageIoRequestId::new(72),
        PageIoRange::new(0, 2),
        PageIoOp::Writeback,
        PageIoFlags::WRITEBACK,
        Some(PageGeneration::new(9)),
        source,
    );

    let plan = provider
        .plan_writeback_mutation(&request)
        .expect("mapped multi-page Direct writeback plan");
    assert_eq!(plan.data.len(), 2);
    assert_eq!(plan.data[0].logical_page, 0);
    assert_eq!(plan.data[0].physical_block, 20);
    assert_eq!(plan.data[1].logical_page, 1);
    assert_eq!(plan.data[1].physical_block, 21);
    assert_eq!(plan.metadata.len(), 1);
    assert_eq!(plan.metadata[0].home, 4);

    let short_direct = BackendPageRequest::new_with_source(
        request.object,
        PageIoRequestId::new(73),
        request.range,
        request.op,
        request.flags,
        request.generation_hint,
        IoDataSource::direct(
            IoDataLeaseId::new(91),
            alloc::vec![BioVec::new(0x200, 0, BLOCK_SIZE as u32)],
        ),
    );
    assert_eq!(
        provider.plan_writeback_mutation(&short_direct),
        Err(V3Errno::EINVAL)
    );

    let partial_direct = BackendPageRequest::new_with_source(
        request.object,
        PageIoRequestId::new(74),
        request.range,
        request.op,
        request.flags,
        request.generation_hint,
        IoDataSource::direct(
            IoDataLeaseId::new(92),
            alloc::vec![
                BioVec::new(0x300, 0, BLOCK_SIZE as u32),
                BioVec::new(0x301, 16, (BLOCK_SIZE - 16) as u32),
            ],
        ),
    );
    assert_eq!(
        provider.plan_writeback_mutation(&partial_direct),
        Err(V3Errno::EINVAL)
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

    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::destroy_inode(&*fs, FsObjectId::new(12), &guard),
        V3::<(), NoProgress>::err(V3Errno::EOPNOTSUPP)
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
fn ext4_mkdir_public_path_admits_directory_with_ring_runtime_without_home_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();
    let (mounted, runtime, writes) = mounted_counting_mkdir_fs_with_ring(32);

    assert_eq!(
        mounted
            .fs_ops()
            .mkdir(FsObjectId::new(2), b"newdir", 0o755, &cred, &guard,),
        V3::<_, NoProgress>::done((
            FsObjectId::new(14),
            InodeMeta {
                mode: 0o40755,
                uid: 0,
                gid: 0,
                size: BLOCK_SIZE as u64,
                atime: Default::default(),
                mtime: Default::default(),
                ctime: Default::default(),
                nlinks: 2,
                blocks: 8,
                flags: Inode::EXTENTS_FL,
            },
        ))
    );
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(32)
    );
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
