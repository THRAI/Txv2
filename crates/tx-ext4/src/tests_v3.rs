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

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::eprintln;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::adapter::step_engine::{
    self as epoch, page_allocator, Errno as V3Errno, NoProgress, StepOp, StepOutcome as V3,
};
use tx_ext4_format::journal::JBD2_BLOCK_SIZE;
use tx_ext4_format::ondisk::{
    BitmapMut, BitmapView, Extent, ExtentHeader, ExtentIdx, ExtentNode, GroupDesc, Inode,
    Superblock,
};
use tx_ext4_format::pager::{BlockImage, Ext4Pager, InodeNo, Page4K, BLOCK_SIZE};
use tx_substrate::step::PageProgress;
use tx_subsystems::fs_iface::{
    BackendPageRequest, BackendPlan, FsObjectKey, IoDataLeaseId, IoDataSource, IoDataTarget,
    PageFrameRef,
};
use tx_subsystems::io_manager::block::{BioVec, DeviceKey};
use tx_subsystems::io_manager::page::{
    PageGeneration, PageIoFlags, PageIoOp, PageIoRange, PageIoRequestId,
};
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, MountPayloadPin,
    SourceLabel,
};
use tx_subsystems::page_backed::{
    step_write, Frame, FsPageBacking, PageContainer, PageContainerKind, PageIndex,
};
use tx_subsystems::vfs::structure::{
    DEntry, DirCursor, FsObjectId, InlineName, InodeKind, InodeMeta, OpenFile, OpenFileFlags,
    RNode, RNodeBacking,
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

#[derive(Clone)]
struct SharedCountingImage {
    image: Arc<std::sync::Mutex<MemImage>>,
    writes: Arc<AtomicUsize>,
}

impl SharedCountingImage {
    fn new(image: MemImage, writes: Arc<AtomicUsize>) -> (Self, Arc<std::sync::Mutex<MemImage>>) {
        let image = Arc::new(std::sync::Mutex::new(image));
        (
            Self {
                image: Arc::clone(&image),
                writes,
            },
            image,
        )
    }
}

impl BlockImage for SharedCountingImage {
    fn total_blocks(&self) -> u64 {
        self.image.lock().expect("shared image lock").total_blocks()
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> tx_ext4_format::Result<()> {
        self.image
            .lock()
            .expect("shared image lock")
            .read_block(block, out)
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> tx_ext4_format::Result<()> {
        self.writes.fetch_add(1, Ordering::AcqRel);
        self.image
            .lock()
            .expect("shared image lock")
            .write_block(block, data)
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        self.image.lock().expect("shared image lock").barrier()
    }
}

#[derive(Clone)]
struct SharedFileImage {
    file: Arc<std::sync::Mutex<File>>,
    total_blocks: u64,
    writes: Arc<AtomicUsize>,
}

impl SharedFileImage {
    fn open(path: &Path, writes: Arc<AtomicUsize>) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open Linux-generated ext4 image");
        let total_blocks = file
            .metadata()
            .expect("stat Linux-generated ext4 image")
            .len()
            / BLOCK_SIZE as u64;
        Self {
            file: Arc::new(std::sync::Mutex::new(file)),
            total_blocks,
            writes,
        }
    }
}

impl BlockImage for SharedFileImage {
    fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> tx_ext4_format::Result<()> {
        let offset = block
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or(tx_ext4_format::Ext4FormatError::OutOfBounds)?;
        let mut file = self.file.lock().expect("shared Linux image lock");
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_exact(out))
            .map_err(|_| tx_ext4_format::Ext4FormatError::Corrupt)
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> tx_ext4_format::Result<()> {
        let offset = block
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or(tx_ext4_format::Ext4FormatError::OutOfBounds)?;
        let mut file = self.file.lock().expect("shared Linux image lock");
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.write_all(data))
            .map_err(|_| tx_ext4_format::Ext4FormatError::Corrupt)?;
        self.writes.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        self.file
            .lock()
            .expect("shared Linux image lock")
            .sync_data()
            .map_err(|_| tx_ext4_format::Ext4FormatError::Corrupt)
    }
}

struct DockerFixture {
    root: PathBuf,
}

impl DockerFixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let root = std::env::current_dir()
            .expect("current directory")
            .join("target")
            .join(format!(
                "tx-ext4-runtime-docker-{}-{unique}",
                std::process::id()
            ));
        fs::create_dir_all(&root).expect("create Docker fixture directory");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for DockerFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn docker_image_available(image_name: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image_name])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn docker_run(image_name: &str, fixture: &DockerFixture, readonly: bool, script: String) {
    let suffix = if readonly { ",readonly" } else { "" };
    let mount = format!(
        "type=bind,source={},target=/fixture{suffix}",
        fixture.root.display()
    );
    let output = Command::new("docker")
        .args([
            "run".to_owned(),
            "--rm".to_owned(),
            "--mount".to_owned(),
            mount,
            image_name.to_owned(),
            "sh".to_owned(),
            "-lc".to_owned(),
            script,
        ])
        .output()
        .expect("run ext4 Docker fixture");
    assert!(
        output.status.success(),
        "Docker ext4 command failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn docker_build_tier1_depth_two_unwritten_fixture(
    image_name: &str,
    fixture: &DockerFixture,
    image: &Path,
) {
    let script = format!(
        "set -eu; image=/fixture/{}; commands=/fixture/depth-two.debugfs; dd if=/dev/zero of=\"$image\" bs=1M count=64 status=none; mke2fs -q -t ext4 -F -b 4096 -g 1024 -O extent,^64bit \"$image\"; printf 'write /etc/hostname /file\\n' > \"$commands\"; for i in $(seq 2 2 2800); do printf 'fallocate /file %s %s\\n' \"$i\" \"$i\" >> \"$commands\"; done; printf 'sif /file size 11472896\\n' >> \"$commands\"; debugfs -w -f \"$commands\" \"$image\" >/dev/null; e2fsck -fy \"$image\" >/dev/null; e2fsck -fn \"$image\" >/dev/null",
        image.file_name()
            .expect("fixture image file name")
            .to_str()
            .expect("fixture image file name utf8"),
    );
    docker_run(image_name, fixture, false, script);
}

fn docker_build_depth_three_fragmented_fixture(
    image_name: &str,
    fixture: &DockerFixture,
    image: &Path,
) {
    const NODE_MAX: u64 = 340;
    const ROOT_MAX: u64 = 4;
    let unwritten_extent_count = ROOT_MAX * NODE_MAX * NODE_MAX + 1;
    let script = format!(
        "set -eux; image=/fixture/{}; commands=/fixture/depth-three.debugfs; truncate -s 3G \"$image\"; mke2fs -q -t ext4 -F -b 4096 -O extent,^64bit \"$image\"; printf 'write /etc/hostname /file\\n' > \"$commands\"; count={unwritten_extent_count}; i=2; n=0; while [ \"$n\" -lt \"$count\" ]; do start=$i; if [ $((n % 1000)) -eq 999 ]; then end=$((i + 2)); i=$((i + 4)); else end=$i; i=$((i + 2)); fi; printf 'fallocate /file %s %s\\n' \"$start\" \"$end\" >> \"$commands\"; n=$((n + 1)); done; printf 'sif /file size %s\\n' \"$((i * 4096))\" >> \"$commands\"; debugfs -w -f \"$commands\" \"$image\" >/dev/null; debugfs -w -R 'fallocate /file 1 1' \"$image\" >/dev/null; debugfs -w -R 'fallocate /file 3 3' \"$image\" >/dev/null; e2fsck -fn \"$image\" >/dev/null",
        image.file_name()
            .expect("fixture image file name")
            .to_str()
            .expect("fixture image file name utf8"),
    );
    docker_run(image_name, fixture, false, script);
}

fn find_near_full_unwritten_extent_with_full_parent(
    image: &SharedFileImage,
    node_bytes: &[u8],
) -> Option<u64> {
    let header = ExtentHeader::parse(node_bytes).ok()?;
    match ExtentNode::parse(node_bytes).ok()? {
        ExtentNode::Leaf(extents) if header.entries + 2 > header.max => extents
            .iter()
            .find(|extent| !extent.is_initialized() && extent.initialized_len() >= 3)
            .map(|extent| u64::from(extent.logical_block) + 1),
        ExtentNode::Leaf(_) => None,
        ExtentNode::Index(indexes) => {
            let parent_is_full = header.entries == header.max;
            for index in indexes {
                let mut child = [0u8; BLOCK_SIZE];
                image.read_block(index.child, &mut child).ok()?;
                if parent_is_full {
                    if let Some(logical_block) = find_near_full_unwritten_extent(image, &child) {
                        return Some(logical_block);
                    }
                }
                if let Some(logical_block) =
                    find_near_full_unwritten_extent_with_full_parent(image, &child)
                {
                    return Some(logical_block);
                }
            }
            None
        }
    }
}

fn find_near_full_unwritten_extent(image: &SharedFileImage, node_bytes: &[u8]) -> Option<u64> {
    let header = ExtentHeader::parse(node_bytes).ok()?;
    match ExtentNode::parse(node_bytes).ok()? {
        ExtentNode::Leaf(extents) if header.entries + 2 > header.max => extents
            .iter()
            .find(|extent| !extent.is_initialized() && extent.initialized_len() >= 3)
            .map(|extent| u64::from(extent.logical_block) + 1),
        ExtentNode::Leaf(_) => None,
        ExtentNode::Index(indexes) => {
            for index in indexes {
                let mut child = [0u8; BLOCK_SIZE];
                image.read_block(index.child, &mut child).ok()?;
                if let Some(logical_block) = find_near_full_unwritten_extent(image, &child) {
                    return Some(logical_block);
                }
            }
            None
        }
    }
}

fn docker_e2fsck(image_name: &str, fixture: &DockerFixture, image: &Path) {
    let image_path = format!(
        "/fixture/{}",
        image
            .file_name()
            .expect("fixture image file name")
            .to_str()
            .expect("fixture image file name utf8")
    );
    docker_run(
        image_name,
        fixture,
        true,
        format!("set -eu; e2fsck -fn {image_path}"),
    );
}

fn init_substrate() {
    tx_test_support::init_host();
    tx_subsystems::zones::register_all().expect("tx-subsystems zones");
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for tx-ext4 v3 tests: {error:?}"),
    }
}

fn assert_metadata_settled(runtime: &JournalMutationRuntime, writes: &AtomicUsize) {
    assert!(
        writes.load(Ordering::Acquire) > 0,
        "metadata mutation must reach durable journal/checkpoint writes before success",
    );
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::default()
    );
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

fn build_tier1_mount_image_with_orphan_file_compat() -> MemImage {
    let mut image = build_tier1_mount_image();
    let mut superblock = Superblock::parse(&image.block_mut(0)[1024..2048]).expect("superblock");
    superblock.feature_compat |= Superblock::FEATURE_COMPAT_ORPHAN_FILE;
    superblock
        .encode(&mut image.block_mut(0)[1024..2048])
        .expect("encode Tier 1 orphan_file-compatible superblock");
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

fn build_tier1_depth_two_extent_image(links_count: u16) -> MemImage {
    let mut image = build_tier1_mount_image();
    for block in [20, 21, 30, 32, 33, 34, 35] {
        BitmapMut::new(image.block_mut(2)).set(block).unwrap();
    }

    ExtentNode::encode_leaf(
        &[Extent {
            logical_block: 0,
            len: 2,
            physical_start: 20,
        }],
        image.block_mut(34),
    )
    .unwrap();
    ExtentNode::encode_leaf(
        &[Extent {
            logical_block: 2,
            len: 1,
            physical_start: 30,
        }],
        image.block_mut(35),
    )
    .unwrap();
    ExtentNode::encode_index(
        1,
        &[ExtentIdx {
            logical_block: 0,
            child: 34,
        }],
        image.block_mut(32),
    )
    .unwrap();
    ExtentNode::encode_index(
        1,
        &[ExtentIdx {
            logical_block: 2,
            child: 35,
        }],
        image.block_mut(33),
    )
    .unwrap();

    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o600;
    inode.size = 3 * BLOCK_SIZE as u64;
    inode.blocks_512 = 56;
    inode.links_count = links_count;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[
                ExtentIdx {
                    logical_block: 0,
                    child: 32,
                },
                ExtentIdx {
                    logical_block: 2,
                    child: 33,
                },
            ],
            2,
        )
        .unwrap();
    write_inode_at(&mut image, 12, &inode);
    image
}

fn build_tier1_depth_two_destroy_image() -> MemImage {
    let mut image = build_tier1_depth_two_extent_image(0);
    mark_inode_bitmap_used(&mut image, 12);
    image
}

fn build_tier1_depth_three_unwritten_image() -> MemImage {
    let mut image = build_tier1_mount_image();
    for block in [20, 21, 22, 30] {
        BitmapMut::new(image.block_mut(2)).set(block).unwrap();
    }
    ExtentNode::encode_leaf(
        &[Extent {
            logical_block: 0,
            len: Extent::UNINITIALIZED_MASK | 3,
            physical_start: 30,
        }],
        image.block_mut(20),
    )
    .unwrap();
    ExtentNode::encode_index(
        1,
        &[ExtentIdx {
            logical_block: 0,
            child: 20,
        }],
        image.block_mut(21),
    )
    .unwrap();
    ExtentNode::encode_index(
        2,
        &[ExtentIdx {
            logical_block: 0,
            child: 21,
        }],
        image.block_mut(22),
    )
    .unwrap();
    let mut inode = Inode::default();
    inode.mode = Inode::S_IFREG | 0o600;
    inode.size = 4 * BLOCK_SIZE as u64;
    inode.blocks_512 = 32;
    inode.links_count = 1;
    inode.flags = Inode::EXTENTS_FL;
    inode
        .set_extent_index_root(
            &[ExtentIdx {
                logical_block: 0,
                child: 22,
            }],
            3,
        )
        .unwrap();
    write_inode_at(&mut image, 12, &inode);
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
        JournalPagePool::new(32).expect("journal pool"),
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
                blocks: vec![
                    9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
                ],
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
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 3, false);
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

fn mounted_shared_unlink_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<SharedCountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<MemImage>>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 3, false);
    let (image, image_handle) =
        SharedCountingImage::new(build_tier1_mount_image(), Arc::clone(&writes));
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        image,
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 mutation ext4 image");
    (mounted, runtime, writes, image_handle)
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

fn mounted_shared_counting_depth_two_truncate_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<SharedCountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<MemImage>>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_metadata(sequence, 5, true);
    let (image, image_handle) =
        SharedCountingImage::new(build_tier1_depth_two_extent_image(1), Arc::clone(&writes));
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        image,
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 depth-two truncate image");
    (mounted, runtime, writes, image_handle)
}

fn mounted_counting_depth_two_destroy_fs(
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
            image: build_tier1_depth_two_destroy_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 depth-two destroy image");
    (mounted, runtime, writes)
}

fn mounted_shared_counting_depth_three_unwritten_fs(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<SharedCountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<MemImage>>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test(sequence);
    let (image, image_handle) = SharedCountingImage::new(
        build_tier1_depth_three_unwritten_image(),
        Arc::clone(&writes),
    );
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        image,
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 depth-three unwritten image");
    (mounted, runtime, writes, image_handle)
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

fn mounted_counting_create_fs_with_ring(
    sequence: u32,
) -> (
    crate::mount::MountedExt4<CountingImage>,
    Arc<JournalMutationRuntime>,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let runtime = mutation_runtime_for_test_with_ring(sequence, 5, false);
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        CountingImage {
            image: build_tier1_create_image(),
            writes: Arc::clone(&writes),
        },
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Tier 1 create mutation ext4 image with ring");
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

fn ext4_root_dentry_for_mounted<I>(
    mounted: &crate::mount::MountedExt4<I>,
    dev: u32,
    mount_id: u64,
) -> (
    epoch::Cap<DEntry>,
    epoch::Cap<MountIdentity>,
    epoch::Cap<MountPayload>,
)
where
    I: BlockImage + Send + 'static,
{
    let payload = MountPayload::new_cap(
        mounted.fs_ops(),
        mounted.fs_page_backing(),
        None,
        DevId::new(dev),
        MountOptions::default(),
        "ext4",
        SourceLabel::Static("ext4"),
    )
    .expect("ext4 mount payload for VFS path test");
    mounted.bind_mount_payload(&payload);
    let root_rnode = RNode::new_cap_in_mount(
        mounted.root_fs_object_id,
        mounted.root_inode_meta,
        RNodeBacking::Directory,
        &payload,
    )
    .expect("ext4 root rnode");
    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("ext4 root dentry");
    let root_mount = MountIdentity::new_cap_with_root_dentry(
        MountId::new(mount_id),
        None,
        root_dentry.clone(),
        None,
        payload.clone(),
        MountFlags::empty(),
    )
    .expect("ext4 root mount identity");
    (root_dentry, root_mount, payload)
}

#[test]
fn ext4_materialise_regular_file_reuses_live_page_container_for_same_inode() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fs = open_fs();
    let mount = MountPayload::new_cap(
        fs.clone().fs_ops_arc(),
        fs.clone().fs_page_backing_arc(),
        None,
        DevId::new(8),
        MountOptions::default(),
        "ext4",
        SourceLabel::Static("ext4"),
    )
    .expect("ext4 mount payload");
    fs.bind_mount_payload(&mount);
    let guard = epoch::guard();
    let fs_ops = fs.clone().fs_ops_arc();
    let file_id = FsObjectId::new(12);
    let meta = match fs_ops.load_inode_meta(file_id, &guard) {
        V3::Done(meta) => meta,
        other => panic!("load inode meta for materialise reuse test: {other:?}"),
    };

    let first = match fs_ops.materialise_rnode(file_id, meta, &mount, &guard) {
        V3::Done(rnode) => rnode,
        other => panic!("first materialise: {other:?}"),
    };
    let first_pc = match first.backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        other => panic!("first materialise returned non-page-backed rnode: {other:?}"),
    };
    first_pc.set_size_bytes(meta.size + 7);

    let second = match fs_ops.materialise_rnode(file_id, meta, &mount, &guard) {
        V3::Done(rnode) => rnode,
        other => panic!("second materialise: {other:?}"),
    };
    let second_pc = match second.backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        other => panic!("second materialise returned non-page-backed rnode: {other:?}"),
    };

    assert_eq!(second_pc, first_pc);
    assert_eq!(second_pc.size_bytes(), meta.size + 7);
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
    assert_metadata_settled(&runtime, &writes);
    assert!(matches!(
        runtime.source().plan_fsync(&BackendPageRequest::new(
            FsObjectKey::new(12),
            PageIoRequestId::new(91),
            PageIoRange::new(0, 1),
            PageIoOp::Fsync,
            PageIoFlags::BARRIER,
            None,
        )),
        BackendPlan::Err(V3Errno::EAGAIN)
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
    assert_metadata_settled(&chmod_runtime, &chmod_writes);

    let (chown_mount, chown_runtime, chown_writes) = mounted_counting_mutation_fs(19);
    let chown_ops = chown_mount.fs_ops();
    assert_eq!(
        chown_ops.chown_inode(FsObjectId::new(12), Some(1000), Some(1000), &cred, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&chown_runtime, &chown_writes);
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
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
}

#[test]
fn ext4_depth_two_truncate_public_path_checkpoints_descendant_after_images() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let mut planner = Ext4Pager::open(build_tier1_depth_two_extent_image(1))
        .expect("open Tier 1 depth-two truncate image");
    let plan = planner
        .plan_truncate_size(
            InodeNo::new(12),
            BLOCK_SIZE as u64,
            tx_ext4_format::mutation::FsyncStamp::new(46),
        )
        .expect("plan depth-two truncate");
    assert_eq!(plan.metadata.len(), 5);
    assert_eq!(plan.revokes.len(), 4);
    let direct_runtime = mutation_runtime_for_test_with_metadata(46, 5, true);
    assert!(
        direct_runtime.begin_mutation(&plan, &guard).is_ok(),
        "depth-two truncate runtime admission must stage every metadata and revoke record"
    );
    let (mounted, runtime, writes, image) = mounted_shared_counting_depth_two_truncate_fs(46);

    assert_eq!(
        mounted
            .fs_page_backing()
            .truncate(FsObjectId::new(12), BLOCK_SIZE as u64, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);

    let image = image.lock().expect("shared image lock");
    let bitmap = BitmapView::new(&image.blocks[2]);
    assert!(bitmap.is_set(20));
    assert!(!bitmap.is_set(21));
    assert!(!bitmap.is_set(30));
    assert!(bitmap.is_set(32));
    assert!(!bitmap.is_set(33));
    assert!(bitmap.is_set(34));
    assert!(!bitmap.is_set(35));

    match ExtentNode::parse(&image.blocks[34]).unwrap() {
        ExtentNode::Leaf(extents) => assert_eq!(
            extents,
            vec![Extent {
                logical_block: 0,
                len: 1,
                physical_start: 20,
            }]
        ),
        ExtentNode::Index(_) => panic!("depth-two leaf must stay a leaf"),
    }
    match ExtentNode::parse(&image.blocks[32]).unwrap() {
        ExtentNode::Index(indexes) => assert_eq!(
            indexes,
            vec![ExtentIdx {
                logical_block: 0,
                child: 34,
            }]
        ),
        ExtentNode::Leaf(_) => panic!("depth-two parent must stay indexed"),
    }
    let inode = Inode::parse(&image.blocks[4][11 * 256..12 * 256]).unwrap();
    assert_eq!(inode.size, BLOCK_SIZE as u64);
    assert_eq!(inode.blocks_512, 24);
}

#[test]
fn ext4_depth_three_flush_public_path_checkpoints_leaf_after_image() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes, image) = mounted_shared_counting_depth_three_unwritten_fs(49);
    let frame = tx_subsystems::page_backed::Frame::new(
        page_allocator::zero_frame_ppn().expect("zero frame"),
    );

    assert_eq!(
        mounted.fs_page_backing().flush_page(
            FsObjectId::new(12),
            BLOCK_SIZE as u64,
            &frame,
            &guard
        ),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(
        mounted.fs_ops().chmod_inode(
            FsObjectId::new(12),
            0o640,
            &tx_subsystems::vfs::Credential::root(),
            &guard,
        ),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);

    let image = image.lock().expect("shared image lock");
    match ExtentNode::parse(&image.blocks[20]).unwrap() {
        ExtentNode::Leaf(extents) => assert_eq!(
            extents,
            vec![
                Extent {
                    logical_block: 0,
                    len: Extent::UNINITIALIZED_MASK | 1,
                    physical_start: 30,
                },
                Extent {
                    logical_block: 1,
                    len: 1,
                    physical_start: 31,
                },
                Extent {
                    logical_block: 2,
                    len: Extent::UNINITIALIZED_MASK | 1,
                    physical_start: 32,
                },
            ]
        ),
        ExtentNode::Index(_) => panic!("depth-three flush must retain a leaf node"),
    }
}

/// Linux creates the fragmented unwritten extent tree. Tx then performs the
/// conversion through the mounted `FsPageBacking` and `FsOps` interfaces, and
/// Linux e2fsprogs validates the persisted result.
#[test]
#[ignore = "requires the tx-ext4-xfstests-tier1 Docker image"]
fn docker_linux_depth_two_unwritten_flush_survives_tx_runtime_settlement() {
    let image_name = std::env::var("TX_EXT4_E2FSPROGS_DOCKER_IMAGE")
        .unwrap_or_else(|_| "tx-ext4-xfstests-tier1:local".to_owned());
    if !docker_image_available(&image_name) {
        eprintln!("skipping Docker ext4 runtime verification; image unavailable: {image_name}");
        return;
    }

    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fixture = DockerFixture::new();
    let image_path = fixture.path("linux-depth-two-unwritten.ext4");
    docker_build_tier1_depth_two_unwritten_fixture(&image_name, &fixture, &image_path);

    let writes = Arc::new(AtomicUsize::new(0));
    let image = SharedFileImage::open(&image_path, Arc::clone(&writes));
    let mut pager = Ext4Pager::open(image.clone()).expect("open Linux ext4 fixture");
    let inode = pager
        .lookup(InodeNo::new(2), b"file")
        .expect("lookup Linux /file")
        .expect("Linux /file exists");
    let (_, root) = pager
        .inode_meta_and_extent_root(inode)
        .expect("read Linux /file extent root");
    assert_eq!(
        ExtentHeader::parse(&root)
            .expect("parse Linux /file extent root")
            .depth,
        2,
        "fixture must exercise a Linux-generated depth-two extent root"
    );
    let journal_geometry = pager
        .journal_geometry()
        .expect("read Linux JBD2 geometry for the mounted runtime");
    drop(pager);

    let runtime = Arc::new(
        JournalMutationRuntime::from_geometry_with_sequence(
            Arc::new(JournalFsyncSource::new()),
            JournalPagePool::new(32).expect("journal pool"),
            DeviceKey::new(7),
            8,
            journal_geometry,
            61,
        )
        .expect("build runtime from Linux JBD2 geometry"),
    );
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        image,
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Linux Tier 1 ext4 fixture through the mutation runtime");
    let guard = epoch::guard();
    let frame = Frame::new(page_allocator::zero_frame_ppn().expect("zero frame"));

    assert_eq!(
        mounted.fs_page_backing().flush_page(
            FsObjectId::new(inode.get() as u64),
            2 * BLOCK_SIZE as u64,
            &frame,
            &guard,
        ),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(
        mounted.fs_ops().chmod_inode(
            FsObjectId::new(inode.get() as u64),
            0o640,
            &tx_subsystems::vfs::Credential::root(),
            &guard,
        ),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);
    drop(guard);
    drop(mounted);

    docker_e2fsck(&image_name, &fixture, &image_path);
}

/// Linux creates a depth-three fragmented extent tree whose target leaf split
/// carries through a full parent. Tx performs that conversion through the
/// public writeback and settlement interfaces before Linux verifies the image.
#[test]
#[ignore = "builds a 3 GiB Linux depth-three extent fixture"]
fn docker_linux_depth_three_parent_carry_flush_survives_tx_runtime_settlement() {
    let image_name = std::env::var("TX_EXT4_E2FSPROGS_DOCKER_IMAGE")
        .unwrap_or_else(|_| "tx-ext4-xfstests-tier1:local".to_owned());
    if !docker_image_available(&image_name) {
        eprintln!("skipping Docker ext4 runtime verification; image unavailable: {image_name}");
        return;
    }

    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let fixture = DockerFixture::new();
    let image_path = fixture.path("linux-depth-three-parent-carry.ext4");
    eprintln!("depth-three runtime witness: build fixture");
    docker_build_depth_three_fragmented_fixture(&image_name, &fixture, &image_path);

    let writes = Arc::new(AtomicUsize::new(0));
    let image = SharedFileImage::open(&image_path, Arc::clone(&writes));
    let mut pager = Ext4Pager::open(image.clone()).expect("open Linux ext4 fixture");
    let inode = pager
        .lookup(InodeNo::new(2), b"file")
        .expect("lookup Linux /file")
        .expect("Linux /file exists");
    let (_, root) = pager
        .inode_meta_and_extent_root(inode)
        .expect("read Linux /file extent root");
    assert_eq!(
        ExtentHeader::parse(&root)
            .expect("parse Linux /file extent root")
            .depth,
        3,
        "fixture must exercise a Linux-generated depth-three extent root"
    );
    let target_logical_block = find_near_full_unwritten_extent_with_full_parent(&image, &root)
        .expect("fixture must retain a near-full unwritten leaf beneath a full parent");
    let journal_geometry = pager
        .journal_geometry()
        .expect("read Linux JBD2 geometry for the mounted runtime");
    drop(pager);

    eprintln!("depth-three runtime witness: mount target={target_logical_block}");
    let runtime = Arc::new(
        JournalMutationRuntime::from_geometry_with_sequence(
            Arc::new(JournalFsyncSource::new()),
            JournalPagePool::new(32).expect("journal pool"),
            DeviceKey::new(7),
            8,
            journal_geometry,
            62,
        )
        .expect("build runtime from Linux JBD2 geometry"),
    );
    let mounted = mount_ext4_read_write_with_mutation_journal_io_manager_planner(
        image,
        Ext4BlockGeometry::new(DeviceKey::new(7), 8),
        Arc::clone(&runtime),
    )
    .expect("mount Linux depth-three fixture through the mutation runtime");
    let guard = epoch::guard();
    let frame = Frame::new(page_allocator::zero_frame_ppn().expect("zero frame"));

    eprintln!("depth-three runtime witness: flush");
    assert_eq!(
        mounted.fs_page_backing().flush_page(
            FsObjectId::new(inode.get() as u64),
            target_logical_block * BLOCK_SIZE as u64,
            &frame,
            &guard,
        ),
        V3::<(), NoProgress>::done(())
    );
    eprintln!("depth-three runtime witness: settle");
    assert_eq!(
        mounted.fs_ops().chmod_inode(
            FsObjectId::new(inode.get() as u64),
            0o640,
            &tx_subsystems::vfs::Credential::root(),
            &guard,
        ),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);
    drop(guard);
    drop(mounted);

    eprintln!("depth-three runtime witness: e2fsck");
    docker_e2fsck(&image_name, &fixture, &image_path);
    eprintln!("depth-three runtime witness: complete");
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
fn ext4_close_visibility_flush_clears_buffered_write_reservation_for_next_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_write_growth_fs(25);
    let pc = page_container_for_mounted_file(&mounted, FsObjectId::new(12), BLOCK_SIZE as u64, 8);
    let of = open_file_for_page_container(&pc);

    of.set_offset(4 * BLOCK_SIZE as u64);
    assert_eq!(step_write(&pc, &of, 32, &guard), V3::done(32));
    assert_eq!(mounted.buffered_write_reservation_count_for_test(), 1);

    of.set_offset(5 * BLOCK_SIZE as u64);
    assert_eq!(
        step_write(&pc, &of, 32, &guard),
        V3::err(V3Errno::EBUSY),
        "a second extending buffered write reproduces the stale reservation blocker"
    );

    assert_eq!(
        pc.flush_dirty_pages_for_close_visibility(&guard),
        V3::<(), PageProgress>::done(())
    );
    assert_eq!(mounted.buffered_write_reservation_count_for_test(), 0);
    assert_eq!(writes.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(25)
    );

    assert_eq!(step_write(&pc, &of, 32, &guard), V3::done(32));
    assert_eq!(mounted.buffered_write_reservation_count_for_test(), 1);
}

#[test]
fn ext4_truncate_before_flush_clears_buffered_write_reservation() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_truncate_free_fs(26);
    let pc = page_container_for_mounted_file(&mounted, FsObjectId::new(12), BLOCK_SIZE as u64, 8);
    let of = open_file_for_page_container(&pc);

    of.set_offset(4 * BLOCK_SIZE as u64);
    assert_eq!(step_write(&pc, &of, 32, &guard), V3::done(32));
    assert_eq!(mounted.buffered_write_reservation_count_for_test(), 1);

    assert_eq!(
        mounted
            .fs_page_backing()
            .truncate(FsObjectId::new(12), 0, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(mounted.buffered_write_reservation_count_for_test(), 0);
    assert_metadata_settled(&runtime, &writes);
}

#[test]
fn ext4_metadata_mutation_settles_prior_ordered_data_transaction_before_admission() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_mutation_fs(29);
    let backing = mounted.fs_page_backing();
    let cred = tx_subsystems::vfs::Credential::root();
    let frame = tx_subsystems::page_backed::Frame::new(
        page_allocator::zero_frame_ppn().expect("zero frame"),
    );

    assert_eq!(
        backing.flush_page(FsObjectId::new(12), 0, &frame, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(29)
    );

    assert_eq!(
        mounted
            .fs_ops()
            .chmod_inode(FsObjectId::new(12), 0o755, &cred, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
}

#[test]
fn ext4_unlink_settlement_publishes_directory_inode_and_orphan_head() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes, image) = mounted_shared_unlink_fs(27);
    let fs_ops = mounted.fs_ops();

    assert_eq!(
        fs_ops.unlink(FsObjectId::new(2), b"hello", FsObjectId::new(12), &guard,),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);
    assert_eq!(
        fs_ops.lookup(FsObjectId::new(2), b"hello", &guard),
        V3::<FsObjectId, NoProgress>::err(V3Errno::ENOENT)
    );
    let meta = match fs_ops.load_inode_meta(FsObjectId::new(12), &guard) {
        V3::Done(meta) => meta,
        other => panic!("load unlinked inode meta after settlement: {other:?}"),
    };
    assert_eq!(meta.nlinks, 0);

    let image = image.lock().expect("shared image lock");
    let superblock = Superblock::parse(&image.blocks[0][1024..2048]).expect("superblock");
    assert_eq!(superblock.last_orphan, 12);
    let inode_offset = 11 * 256;
    let inode = Inode::parse(&image.blocks[4][inode_offset..inode_offset + 256]).expect("inode 12");
    assert_eq!(inode.links_count, 0);
    assert_eq!(inode.dtime, 0);
}

#[test]
fn ext4_destroy_clears_singleton_orphan_before_inode_reuse() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();
    let (mounted, runtime, writes) = mounted_counting_create_fs_with_ring(41);
    let fs_ops = mounted.fs_ops();

    let (first_id, _) =
        match fs_ops.create_inode(FsObjectId::new(2), b"first", 0o100640, &cred, &guard) {
            V3::Done(created) => created,
            other => panic!("create first file for orphan reuse test: {other:?}"),
        };
    assert_eq!(
        fs_ops.unlink(FsObjectId::new(2), b"first", first_id, &guard,),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(
        fs_ops.destroy_inode(first_id, &guard),
        V3::<(), NoProgress>::done(())
    );
    let (second_id, _) =
        match fs_ops.create_inode(FsObjectId::new(2), b"second", 0o100640, &cred, &guard) {
            V3::Done(created) => created,
            other => panic!("create second file for orphan reuse test: {other:?}"),
        };
    assert_eq!(second_id, first_id);
    assert_eq!(
        fs_ops.unlink(FsObjectId::new(2), b"second", second_id, &guard,),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
}

#[test]
fn runtime_retains_depth_two_destroy_deferred_frees_until_checkpoint() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let mut pager = Ext4Pager::open(build_tier1_depth_two_destroy_image())
        .expect("open Tier 1 depth-two destroy image");
    let mutation = pager
        .plan_destroy_inode(
            InodeNo::new(12),
            tx_ext4_format::mutation::FsyncStamp::new(47),
        )
        .expect("plan depth-two destroy");
    assert_eq!(
        mutation
            .revokes
            .iter()
            .map(|claim| claim.physical_block)
            .collect::<Vec<_>>(),
        vec![20, 21, 30, 32, 33, 34, 35]
    );
    let runtime = mutation_runtime_for_test_with_metadata(47, 5, true);

    runtime
        .begin_mutation(&mutation, &guard)
        .expect("admit depth-two destroy mutation");
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(47)
    );
    for claim in &mutation.deferred_frees {
        assert_eq!(
            runtime.source().try_reuse_for_test(claim.physical_block),
            Err(V3Errno::EBUSY)
        );
    }
}

#[test]
fn ext4_depth_two_destroy_public_path_checkpoints_inode_and_allocation_metadata() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let (mounted, runtime, writes) = mounted_counting_depth_two_destroy_fs(48);

    assert_eq!(
        mounted.fs_ops().destroy_inode(FsObjectId::new(12), &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
}

#[test]
fn ext4_chmod_public_path_admits_regular_file_after_create_settlement() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();
    let (mounted, runtime, writes) = mounted_counting_create_fs_with_ring(32);
    let fs_ops = mounted.fs_ops();
    let (file_id, _) =
        match fs_ops.create_inode(FsObjectId::new(2), b"chmod-new", 0o644, &cred, &guard) {
            V3::Done(created) => created,
            other => panic!("create file for chmod: {other:?}"),
        };
    assert_metadata_settled(&runtime, &writes);
    let meta_after_create = match fs_ops.load_inode_meta(file_id, &guard) {
        V3::Done(meta) => meta,
        other => panic!("load meta after create settlement: {other:?}"),
    };
    assert_eq!(meta_after_create.mode & 0o777, 0o644);

    assert_eq!(
        fs_ops.chmod_inode(file_id, 0o755, &cred, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);
}

#[test]
fn ext4_chmod_public_path_admits_new_file_after_buffered_write_settlement() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();
    let (mounted, runtime, writes) = mounted_counting_mkdir_fs_with_ring(33);
    let fs_ops = mounted.fs_ops();
    let (dir_id, _) = match fs_ops.mkdir(FsObjectId::new(2), b"tier1-data", 0o755, &cred, &guard) {
        V3::Done(created) => created,
        other => panic!("mkdir tier1-data for chmod-after-write: {other:?}"),
    };
    assert_metadata_settled(&runtime, &writes);
    let (file_id, _) = match fs_ops.create_inode(dir_id, b"file", 0o644, &cred, &guard) {
        V3::Done(created) => created,
        other => panic!("create file for chmod-after-write: {other:?}"),
    };
    assert_metadata_settled(&runtime, &writes);

    let pc = page_container_for_mounted_file(&mounted, file_id, 0, 8);
    let of = open_file_for_page_container(&pc);
    assert_eq!(step_write(&pc, &of, 6, &guard), V3::done(6));
    let ppn = pc.lookup(PageIndex::new(0)).expect("dirty page 0 resident");
    let frame = Frame::new(ppn);
    assert_eq!(
        mounted
            .fs_page_backing()
            .flush_page(file_id, 0, &frame, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        tx_subsystems::mount::MountTransactionFrontier::new(35)
    );

    assert_eq!(
        fs_ops.chmod_inode(file_id, 0o755, &cred, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);
}

#[test]
fn ext4_chmod_public_path_admits_new_file_with_dirty_buffered_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();
    let (mounted, runtime, writes) = mounted_counting_mkdir_fs_with_ring(36);
    let fs_ops = mounted.fs_ops();
    let (dir_id, _) = match fs_ops.mkdir(FsObjectId::new(2), b"dirty-data", 0o755, &cred, &guard) {
        V3::Done(created) => created,
        other => panic!("mkdir dirty-data for chmod-dirty: {other:?}"),
    };
    assert_metadata_settled(&runtime, &writes);
    let (file_id, _) = match fs_ops.create_inode(dir_id, b"file", 0o644, &cred, &guard) {
        V3::Done(created) => created,
        other => panic!("create file for chmod-dirty: {other:?}"),
    };
    assert_metadata_settled(&runtime, &writes);

    let pc = page_container_for_mounted_file(&mounted, file_id, 0, 8);
    let of = open_file_for_page_container(&pc);
    assert_eq!(step_write(&pc, &of, 6, &guard), V3::done(6));
    assert_eq!(mounted.buffered_write_reservation_count_for_test(), 1);

    assert_eq!(
        fs_ops.chmod_inode(file_id, 0o755, &cred, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_metadata_settled(&runtime, &writes);
}

#[test]
fn ext4_chmod_vfs_path_admits_new_file_with_dirty_buffered_write() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();
    let guard = epoch::guard();
    let cred = tx_subsystems::vfs::Credential::root();
    let (mounted, runtime, writes) = mounted_counting_mkdir_fs_with_ring(39);
    let fs_ops = mounted.fs_ops();
    let (root_dentry, _root_mount, mount_payload) = ext4_root_dentry_for_mounted(&mounted, 39, 39);
    let (dir_id, _) = match fs_ops.mkdir(FsObjectId::new(2), b"dirty-data", 0o755, &cred, &guard) {
        V3::Done(created) => created,
        other => panic!("mkdir dirty-data for vfs chmod-dirty: {other:?}"),
    };
    assert_metadata_settled(&runtime, &writes);
    let (file_id, _) = match fs_ops.create_inode(dir_id, b"file", 0o644, &cred, &guard) {
        V3::Done(created) => created,
        other => panic!("create file for vfs chmod-dirty: {other:?}"),
    };
    assert_metadata_settled(&runtime, &writes);

    let pc = page_container_for_mounted_file(&mounted, file_id, 0, 8);
    let of = open_file_for_page_container(&pc);
    assert_eq!(step_write(&pc, &of, 6, &guard), V3::done(6));
    assert_eq!(mounted.buffered_write_reservation_count_for_test(), 1);
    let file_meta = match fs_ops.load_inode_meta(file_id, &guard) {
        V3::Done(meta) => meta,
        other => panic!("load meta for target dentry: {other:?}"),
    };
    let target_rnode = match fs_ops.materialise_rnode(file_id, file_meta, &mount_payload, &guard) {
        V3::Done(rnode) => rnode,
        other => panic!("materialise target dentry: {other:?}"),
    };
    let target_dentry = DEntry::new_cap(
        InlineName::new(b"file").expect("inline file name"),
        target_rnode,
    )
    .expect("target dentry");
    drop(guard);

    let mut chmod = tx_subsystems::vfs::composite::ChmodOp {
        rooted_at: &root_dentry,
        path: b"/dirty-data/file",
        mode: 0o755,
        cred: &cred,
        target: Some(target_dentry),
    };
    let mut script_ctx = epoch::ScriptCtx::<tx_subsystems::process::ProcessIdentity>::new();
    assert_eq!(chmod.step(&mut script_ctx), V3::<(), NoProgress>::done(()));
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
    assert_eq!(
        mounted
            .fs_ops()
            .lookup(FsObjectId::new(2), b"newdir", &guard),
        V3::<_, NoProgress>::done(FsObjectId::new(14))
    );
    assert_eq!(
        mounted
            .fs_ops()
            .load_inode_meta(FsObjectId::new(14), &guard),
        V3::<_, NoProgress>::done(InodeMeta {
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
        })
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
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
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
    assert_metadata_settled(&runtime, &writes);
    assert_eq!(
        mounted
            .fs_ops()
            .lookup(FsObjectId::new(2), b"newdir", &guard),
        V3::<_, NoProgress>::done(FsObjectId::new(14))
    );
    assert_eq!(
        mounted
            .fs_ops()
            .load_inode_meta(FsObjectId::new(14), &guard),
        V3::<_, NoProgress>::done(InodeMeta {
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
        })
    );
    assert_eq!(
        mounted
            .fs_ops()
            .create_inode(FsObjectId::new(14), b"file", 0o100640, &cred, &guard,),
        V3::<_, NoProgress>::done((
            FsObjectId::new(15),
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
    assert_metadata_settled(&runtime, &writes);
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
fn rw_mount_accepts_orphan_file_compat_without_claiming_orphan_file_operations() {
    let _serial = EXT4_V3_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let mounted = mount_ext4_read_write(build_tier1_mount_image_with_orphan_file_compat())
        .expect("OSComp-shaped orphan_file compat mount");
    assert_eq!(
        mounted.capability_profile_hash().map(|hash| hash.0),
        Some(tx_ext4_format::capability::Tier1Capabilities::generated().profile_hash()),
    );
    assert_eq!(
        tx_ext4_format::capability::Tier1Capabilities::generated()
            .admit(tx_ext4_format::capability::Tier1Request::OrphanFile),
        Err(tx_ext4_format::capability::Tier1Reject::Unsupported)
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
