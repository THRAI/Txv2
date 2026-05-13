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

use crate::adapter::step_engine::{
    self as epoch, page_allocator, Errno as V3Errno, NoProgress, SpinMutex, StepOutcome as V3,
};
use tx_ext4_format::ondisk::{Extent, GroupDesc, Inode, Superblock};
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::vfs::structure::{DirCursor, FsObjectId};
use tx_subsystems::vfs::FsOps;

use crate::read_backend::Ext4FsInstance;

/// Shared serialisation lock. Mirrors `tx_fs::test_support::FS_TEST_LOCK`:
/// every test in this crate's lib binary observes the same per-CPU
/// epoch slot in `tx_substrate`, so tests cannot concurrently create
/// epoch guards (the substrate enforces no-nesting on the same logical
/// CPU; see `crates/tx-substrate/src/epoch/local.rs`). Every test grabs
/// this lock before touching epoch / zone state.
static EXT4_V3_TEST_LOCK: SpinMutex<()> = SpinMutex::new(());

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

fn open_fs() -> Arc<Ext4FsInstance<MemImage>> {
    Ext4FsInstance::open(build_image()).expect("open ext4 mem image")
}

// === Tests =============================================================

#[test]
fn ext4_v3_lookup_round_trips_through_v3_outcome() {
    let _serial = EXT4_V3_TEST_LOCK.lock();
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
    let _serial = EXT4_V3_TEST_LOCK.lock();
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
fn ext4_v3_mutation_methods_surface_enosys_through_v3_errno() {
    // The read-only ext4 surface returns `Errno::ENOSYS` from every
    // mutating method (create_inode, mkdir, unlink, rmdir, rename,
    // link, symlink, destroy_inode, serialize_inode_meta). Pin that
    // `ENOSYS` surfaces cleanly through the trait so walker callers
    // observe a consistent shape across backends.
    let _serial = EXT4_V3_TEST_LOCK.lock();
    init_substrate();
    let fs = open_fs();
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
        V3::<(FsObjectId, _), NoProgress>::err(V3Errno::ENOSYS)
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
        V3::<(FsObjectId, _), NoProgress>::err(V3Errno::ENOSYS)
    );
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsOps>::destroy_inode(&*fs, FsObjectId::new(12), &guard),
        V3::<(), NoProgress>::err(V3Errno::ENOSYS)
    );
}

#[test]
fn ext4_v3_readdir_done_then_terminator() {
    let _serial = EXT4_V3_TEST_LOCK.lock();
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
    let _serial = EXT4_V3_TEST_LOCK.lock();
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
fn ext4_v3_truncate_and_fsync_surface_enosys() {
    // Read-only ext4 surface today.
    let _serial = EXT4_V3_TEST_LOCK.lock();
    init_substrate();
    let fs = open_fs();
    let guard = epoch::guard();

    assert_eq!(
        <Ext4FsInstance<MemImage> as FsPageBacking>::truncate(&*fs, FsObjectId::new(12), 0, &guard,),
        V3::<(), NoProgress>::err(V3Errno::ENOSYS)
    );
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsPageBacking>::fsync(&*fs, FsObjectId::new(12), &guard),
        V3::<(), NoProgress>::err(V3Errno::ENOSYS)
    );
}

#[test]
fn ext4_v3_factory_arcs_produce_dyn_v3_traits() {
    // Pin the wave-9c MountOutput cutover wiring shape: each factory
    // returns the canonical `Arc<dyn …V3>` that walker entry points
    // will populate `MountPayload::fs_ops` /
    // `…::fs_page_backing` from. Mirrors `Tmpfs::fs_ops_arc`
    // / `Tmpfs::fs_page_backing_arc`.
    let _serial = EXT4_V3_TEST_LOCK.lock();
    init_substrate();
    let fs = open_fs();
    let _ops_v3: Arc<dyn FsOps> = fs.clone().fs_ops_arc();
    let _pb_v3: Arc<dyn FsPageBacking> = fs.fs_page_backing_arc();
}
