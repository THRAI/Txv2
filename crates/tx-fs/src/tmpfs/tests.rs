//! Phase 3b tests: tmpfs as a standalone backend.
//!
//! Mount-table wiring is exercised separately in `tx-kernel`'s init
//! tests; these tests construct a `Tmpfs` instance directly and
//! exercise the `FsOps` + `FsPageBacking` surface against it.

use alloc::sync::Arc;

use tx_subsystems::execution::{Errno, StepOutcome};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::vfs::{
    Credential, DirCursor, FsObjectId, FsOps, InodeKind, RNodeBacking, S_IFMT,
};

use super::{Tmpfs, TMPFS_ROOT_OBJECT_ID};

fn init_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    // tmpfs's `PageContainer::new_cap` requires the
    // `PAGE_CONTAINER_ZONE` to be registered. `register_all` is
    // idempotent (`register_static_zone` is a no-op for zones it
    // already saw).
    tx_subsystems::zones::register_all().expect("tx-subsystems zones");
    match tx_substrate::page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for tmpfs tests: {error:?}"),
    }
}

#[test]
fn tmpfs_create_then_lookup_round_trip() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = tx_substrate::epoch::guard();
    let cred = Credential::default();

    let (file_id, file_meta) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"hello", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    assert_eq!(file_meta.kind(), InodeKind::Regular);
    // Mode preserved minus IFMT bits we asserted.
    assert_eq!(file_meta.mode & !S_IFMT, 0o644);

    // `lookup` resolves the same id we just allocated.
    match tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"hello", &guard) {
        StepOutcome::Done(id) => assert_eq!(id, file_id),
        other => panic!("lookup failed: {other:?}"),
    }

    // `load_inode_meta` re-reads the stored meta.
    match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => {
            assert_eq!(meta.kind(), InodeKind::Regular);
            assert_eq!(meta.size, 0);
        }
        other => panic!("load_inode_meta failed: {other:?}"),
    }

    // Re-creating the same name collides with POSIX EEXIST.
    assert_eq!(
        tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"hello", 0o100644, &cred, &guard),
        StepOutcome::Err(Errno::EEXIST)
    );
}

#[test]
fn tmpfs_mkdir_then_readdir_yields_dir_entry() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = tx_substrate::epoch::guard();
    let cred = Credential::default();

    let (dev_id, dev_meta) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"dev", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir failed: {other:?}"),
    };
    assert_eq!(dev_meta.kind(), InodeKind::Directory);

    // readdir from the root yields `dev` as a directory entry.
    let mut cursor = DirCursor::START;
    let mut found = false;
    loop {
        match tmpfs.readdir(TMPFS_ROOT_OBJECT_ID, cursor, &guard) {
            StepOutcome::Done(Some((entry, next))) => {
                if entry.name.as_bytes() == b"dev" {
                    assert_eq!(entry.fs_object_id, dev_id);
                    assert_eq!(entry.kind, InodeKind::Directory);
                    found = true;
                }
                cursor = next;
            }
            StepOutcome::Done(None) => break,
            other => panic!("readdir failed: {other:?}"),
        }
    }
    assert!(found, "readdir should yield mkdir'd /dev entry");

    // readdir of a regular file rejects.
    let (file_id, _) = match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &cred, &guard)
    {
        StepOutcome::Done(out) => out,
        other => panic!("create_inode failed: {other:?}"),
    };
    assert_eq!(
        tmpfs.readdir(file_id, DirCursor::START, &guard),
        StepOutcome::Err(Errno::ENOTDIR)
    );
}

#[test]
fn tmpfs_unlink_drops_inode() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = tx_substrate::epoch::guard();
    let cred = Credential::default();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"victim", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    assert_eq!(
        tmpfs.unlink(TMPFS_ROOT_OBJECT_ID, b"victim", file_id, &guard),
        StepOutcome::Done(())
    );

    // After unlink: lookup misses and the inode is gone from the
    // store (load_inode_meta returns ENOENT).
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"victim", &guard),
        StepOutcome::Err(Errno::ENOENT)
    );
    assert_eq!(
        tmpfs.load_inode_meta(file_id, &guard),
        StepOutcome::Err(Errno::ENOENT)
    );

    // unlink rejects directories (must use rmdir).
    let (dir_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"d", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir failed: {other:?}"),
    };
    assert_eq!(
        tmpfs.unlink(TMPFS_ROOT_OBJECT_ID, b"d", dir_id, &guard),
        StepOutcome::Err(Errno::EISDIR)
    );
}

#[test]
fn tmpfs_fetch_page_materialises_anon_then_flush_noop() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = tx_substrate::epoch::guard();
    let cred = Credential::default();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"page-test", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    // First fetch materialises a fresh anon frame.
    let frame_first = match FsPageBacking::fetch_page(&*tmpfs, file_id, 0, &guard) {
        StepOutcome::Done(frame) => frame,
        other => panic!("fetch_page failed: {other:?}"),
    };

    // Second fetch returns the same PPN — anon container cached the
    // page on the first access.
    let frame_second = match FsPageBacking::fetch_page(&*tmpfs, file_id, 0, &guard) {
        StepOutcome::Done(frame) => frame,
        other => panic!("fetch_page (second) failed: {other:?}"),
    };
    assert_eq!(frame_first.ppn(), frame_second.ppn());

    // flush_page is a no-op on tmpfs.
    assert_eq!(
        FsPageBacking::flush_page(&*tmpfs, file_id, 0, &frame_first, &guard),
        StepOutcome::Done(())
    );
    // fsync is a no-op too.
    assert_eq!(
        FsPageBacking::fsync(&*tmpfs, file_id, &guard),
        StepOutcome::Done(())
    );

    // Directories cannot be fetched as pages.
    assert_eq!(
        FsPageBacking::fetch_page(&*tmpfs, TMPFS_ROOT_OBJECT_ID, 0, &guard),
        StepOutcome::Err(Errno::EISDIR)
    );

    // Misaligned offsets reject.
    assert_eq!(
        FsPageBacking::fetch_page(&*tmpfs, file_id, 17, &guard),
        StepOutcome::Err(Errno::EINVAL)
    );
}

#[test]
fn tmpfs_truncate_zeroes_size_and_reflects_in_meta() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = tx_substrate::epoch::guard();
    let cred = Credential::default();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"trunc", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    // Grow visible size to one full page via truncate.
    let page = tx_subsystems::vm::USER_PAGE_SIZE as u64;
    assert_eq!(
        FsPageBacking::truncate(&*tmpfs, file_id, page, &guard),
        StepOutcome::Done(())
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta failed: {other:?}"),
    };
    assert_eq!(meta.size, page);

    // Shrink back to 0 — visible size + payload size both reset.
    assert_eq!(
        FsPageBacking::truncate(&*tmpfs, file_id, 0, &guard),
        StepOutcome::Done(())
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta failed: {other:?}"),
    };
    assert_eq!(meta.size, 0);

    // Truncate of a directory rejects.
    assert_eq!(
        FsPageBacking::truncate(&*tmpfs, TMPFS_ROOT_OBJECT_ID, 0, &guard),
        StepOutcome::Err(Errno::EISDIR)
    );

    // Truncate of an absent inode rejects.
    assert_eq!(
        FsPageBacking::truncate(&*tmpfs, FsObjectId::new(0xdead_beef), 0, &guard),
        StepOutcome::Err(Errno::ENOENT)
    );
}

#[test]
fn tmpfs_create_existing_returns_eexist() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = tx_substrate::epoch::guard();
    let cred = Credential::default();

    // First create succeeds.
    let _ = match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"dup", 0o100644, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("create_inode failed: {other:?}"),
    };

    // Second create at the same name surfaces POSIX EEXIST. mkdir
    // and symlink share the same collision path; assert all three
    // for completeness.
    assert_eq!(
        tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"dup", 0o100644, &cred, &guard),
        StepOutcome::Err(Errno::EEXIST)
    );
    assert_eq!(
        tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"dup", 0o755, &cred, &guard),
        StepOutcome::Err(Errno::EEXIST)
    );
    assert_eq!(
        tmpfs.symlink(TMPFS_ROOT_OBJECT_ID, b"dup", b"target", &cred, &guard),
        StepOutcome::Err(Errno::EEXIST)
    );
}

#[test]
fn tmpfs_rmdir_nonempty_returns_enotempty() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = tx_substrate::epoch::guard();
    let cred = Credential::default();

    let (dir_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"d", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir failed: {other:?}"),
    };

    // Populate the directory with a child so the rmdir attempt
    // observes a non-empty target.
    let _ = match tmpfs.create_inode(dir_id, b"child", 0o100644, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("create_inode in subdir failed: {other:?}"),
    };

    // rmdir must surface POSIX ENOTEMPTY.
    assert_eq!(
        tmpfs.rmdir(TMPFS_ROOT_OBJECT_ID, b"d", dir_id, &guard),
        StepOutcome::Err(Errno::ENOTEMPTY)
    );

    // After unlinking the child, rmdir succeeds.
    let child_id = match tmpfs.lookup(dir_id, b"child", &guard) {
        StepOutcome::Done(id) => id,
        other => panic!("lookup of child failed: {other:?}"),
    };
    assert_eq!(
        tmpfs.unlink(dir_id, b"child", child_id, &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        tmpfs.rmdir(TMPFS_ROOT_OBJECT_ID, b"d", dir_id, &guard),
        StepOutcome::Done(())
    );
}

// ---------------------------------------------------------------------------
// `materialise_rnode` override tests (Phase 7 of the ELF-loader plan).
//
// The override produces an `RNodeBacking::PageBacked { pc }` for
// regular files so the VFS walker can resolve `/init` to a
// page-backed RNode the exec script accepts. The default `ENOSYS`
// would otherwise break the walker → exec_script bridge for tmpfs's
// regular files.
// ---------------------------------------------------------------------------

#[test]
fn tmpfs_materialise_rnode_for_regular_file_returns_page_backed() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = tx_substrate::epoch::guard();
    let cred = Credential::default();

    let (file_id, file_meta) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"init", 0o100755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    // The override must produce a Cap<RNode> whose backing is the
    // file's `Cap<PageContainer>`. We assert through the variant
    // discriminant; the `Cap<PageContainer>::key` (or any other
    // identity check) would suffice but the variant alone is the
    // contract Phase 7 needs.
    let rnode = match FsOps::materialise_rnode(&*tmpfs, file_id, file_meta, &guard) {
        StepOutcome::Done(rnode) => rnode,
        other => panic!("materialise_rnode for regular file: {other:?}"),
    };
    match rnode.backing() {
        RNodeBacking::PageBacked { pc } => {
            // Sanity: the container's `page_count` matches tmpfs's
            // static cap (`TMPFS_FILE_PAGE_CAP = 1024`). `size_bytes`
            // initialises to `page_count * USER_PAGE_SIZE` (the
            // PageContainer's capacity); inode-visible size lives on
            // `InodeMeta::size`, not the container, so we don't pin
            // the byte-size here. The shared-Cap contract is the
            // important part: cloning the inode's container into the
            // RNode means writes via `FsPageBacking` and reads via
            // `OpenFile::step_read` / `read_exact_at` see the same
            // underlying pages.
            assert_eq!(pc.page_count(), 1024, "tmpfs file page-cap shape");
        }
        other => panic!("expected PageBacked backing for regular file, got {other:?}"),
    }
    // RNode meta round-trips the input meta.
    assert_eq!(rnode.fs_object_id(), file_id);
    assert_eq!(rnode.meta().kind(), InodeKind::Regular);
}

#[test]
fn tmpfs_materialise_rnode_for_directory_returns_eisdir() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = tx_substrate::epoch::guard();

    // The walker handles Directory inline; reaching the override
    // with a directory inode is a backend bug. The override returns
    // `EISDIR` rather than panicking so a misroute surfaces as a
    // recoverable error.
    let meta = match tmpfs.load_inode_meta(TMPFS_ROOT_OBJECT_ID, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta(root): {other:?}"),
    };
    assert_eq!(
        FsOps::materialise_rnode(&*tmpfs, TMPFS_ROOT_OBJECT_ID, meta, &guard),
        StepOutcome::Err(tx_subsystems::execution::Errno::EISDIR)
    );
}

#[test]
fn tmpfs_materialise_rnode_for_symlink_returns_einval() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = tx_substrate::epoch::guard();
    let cred = Credential::default();

    let (link_id, link_meta) =
        match tmpfs.symlink(TMPFS_ROOT_OBJECT_ID, b"alias", b"target", &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("symlink failed: {other:?}"),
        };

    // Walker resolves Symlink inline via `read_link`; the override
    // surface returns `EINVAL` on the unexpected re-entry path,
    // mirroring the Linux `inode_operations.lookup` shape for non-
    // page-backed kinds tmpfs intentionally rejects here.
    assert_eq!(
        FsOps::materialise_rnode(&*tmpfs, link_id, link_meta, &guard),
        StepOutcome::Err(tx_subsystems::execution::Errno::EINVAL)
    );
}
