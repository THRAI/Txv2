//! Phase 3b tests: tmpfs as a standalone backend.
//!
//! Mount-table wiring is exercised separately in `tx-kernel`'s init
//! tests; these tests construct a `Tmpfs` instance directly and
//! exercise the `FsOps` + `FsPageBacking` surface against it.

use alloc::sync::Arc;

use super::adapter::step_engine::{self as step_engine, guard, page_allocator, Errno, StepOutcome};
use tx_subsystems::cred::{Capability, CapabilitySet};
use tx_subsystems::page_backed::{AnonSwapPolicy, FsPageBacking, PageContainer, PageContainerKind};
use tx_subsystems::vfs::{
    Credential, DirCursor, FsObjectId, FsOps, InlineName, InodeKind, InodeMeta, OpenFile,
    OpenFileFlags, RNodeBacking, S_IFDIR, S_IFMT, S_IFREG, S_ISGID, S_ISUID,
};

use super::{Tmpfs, TmpfsPayload, TMPFS_ROOT_OBJECT_ID};

fn init_substrate() {
    tx_test_support::init_host();
    // tmpfs's `PageContainer::new_cap` requires the
    // `PAGE_CONTAINER_ZONE` to be registered. `register_all` is
    // idempotent (`register_static_zone` is a no-op for zones it
    // already saw).
    tx_subsystems::zones::register_all().expect("tx-subsystems zones");
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for tmpfs tests: {error:?}"),
    }
}

#[test]
#[cfg(tx_lock_metrics_fs)]
fn tmpfs_state_lock_metrics_are_cfg_gated() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Tmpfs::new();

    assert!(
        tmpfs.state.lock_metrics_enabled(),
        "tmpfs state lock must emit observed lock rows when tx_lock_metrics_fs is enabled"
    );
}

/// Build a throw-away `Cap<MountPayload>` over `tmpfs` so tests that
/// call `FsOps::materialise_rnode` can satisfy the trait's
/// mount-stamping contract (`docs/Txv3/02_INVARIANTS_v5.md` BIF-… and
/// `walker::fs_ops_for` resolution).
fn test_mount_payload(
    tmpfs: &alloc::sync::Arc<Tmpfs>,
) -> step_engine::Cap<tx_subsystems::mount::MountPayload> {
    use tx_subsystems::mount::{DevId, MountOptions, MountPayload, SourceLabel};
    MountPayload::new_cap(
        tmpfs.clone() as alloc::sync::Arc<dyn tx_subsystems::vfs::FsOps>,
        tmpfs.clone() as alloc::sync::Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
        None,
        DevId::new(1),
        MountOptions::default(),
        "tmpfs",
        SourceLabel::Static("tmpfs-test"),
    )
    .expect("test mount payload")
}

#[test]
fn tmpfs_create_then_lookup_round_trip() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

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
fn tmpfs_inode_meta_snapshot_reads_pagecontainer_size_after_snapshot() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    )
    .expect("page container");
    pc.set_size_bytes(4096);

    let mut meta = tx_subsystems::vfs::InodeMeta::new(InodeKind::Regular, 0o100644);
    meta.size = 0;
    let snapshot = super::InodeMetaSnapshot {
        meta,
        nlink: 1,
        size_source: Some(pc.clone()),
    };

    pc.set_size_bytes(8192);
    let loaded = super::inode_meta_from_snapshot(snapshot);

    assert_eq!(loaded.nlinks, 1);
    assert_eq!(
        loaded.size, 8192,
        "tmpfs load_inode_meta should read PageContainer size after releasing the state lock"
    );
}

#[test]
fn tmpfs_mkdir_then_readdir_yields_dir_entry() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

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
fn tmpfs_mkdir_and_rmdir_update_parent_directory_nlinks() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let root_before = match tmpfs.load_inode_meta(TMPFS_ROOT_OBJECT_ID, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load root meta before mkdir failed: {other:?}"),
    };
    assert_eq!(root_before.nlinks, 2);

    let (dir_id, dir_meta) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"child", 0o755, &cred, &guard)
    {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir child failed: {other:?}"),
    };
    assert_eq!(dir_meta.nlinks, 2);

    let root_after_mkdir = match tmpfs.load_inode_meta(TMPFS_ROOT_OBJECT_ID, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load root meta after mkdir failed: {other:?}"),
    };
    assert_eq!(
        root_after_mkdir.nlinks, 3,
        "parent directory nlink should include child directory '..'"
    );

    assert_eq!(
        tmpfs.rmdir(TMPFS_ROOT_OBJECT_ID, b"child", dir_id, &guard),
        StepOutcome::Done(())
    );
    let root_after_rmdir = match tmpfs.load_inode_meta(TMPFS_ROOT_OBJECT_ID, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load root meta after rmdir failed: {other:?}"),
    };
    assert_eq!(root_after_rmdir.nlinks, 2);
}

#[test]
fn tmpfs_readdir_snapshot_builds_direntry_without_state_borrow() {
    let name = InlineName::new(b"dev").expect("inline name");
    let snapshot = super::ReaddirEntrySnapshot {
        child_id: FsObjectId::new(42),
        kind: InodeKind::Directory,
        name,
        next_cursor: DirCursor::from_u64(7),
    };

    let (entry, next) =
        super::dir_entry_from_readdir_snapshot(snapshot).expect("dir entry from snapshot");

    assert_eq!(entry.fs_object_id, FsObjectId::new(42));
    assert_eq!(entry.kind, InodeKind::Directory);
    assert_eq!(entry.name.as_bytes(), b"dev");
    assert_eq!(next, DirCursor::from_u64(7));
}

#[test]
fn tmpfs_readdir_stale_child_returns_enoent() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let ghost_id = FsObjectId::new(0xface);
    let ghost_name = InlineName::new(b"ghost").expect("ghost inline name");

    {
        let mut state = tmpfs.state.lock();
        let root_inode = state
            .inodes
            .get_mut(&TMPFS_ROOT_OBJECT_ID)
            .expect("root inode");
        let TmpfsPayload::Directory(children) = &mut root_inode.payload else {
            panic!("root must be directory");
        };
        children.insert(ghost_name, ghost_id);
    }

    assert_eq!(
        tmpfs.readdir(TMPFS_ROOT_OBJECT_ID, DirCursor::START, &guard),
        StepOutcome::Err(Errno::ENOENT)
    );
}

#[test]
fn tmpfs_unlink_unhooks_name_but_keeps_inode_until_destroy_inode() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"victim", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    assert_eq!(
        tmpfs.unlink(TMPFS_ROOT_OBJECT_ID, b"victim", file_id, &guard),
        StepOutcome::Done(())
    );

    // After unlink: lookup misses, but an already-open RNode still
    // addresses the inode by object id until VFS destroys it on the
    // final live reference drop.
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"victim", &guard),
        StepOutcome::Err(Errno::ENOENT)
    );
    match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => assert_eq!(meta.kind(), InodeKind::Regular),
        other => panic!("unlinked open inode must remain addressable: {other:?}"),
    }

    assert_eq!(tmpfs.destroy_inode(file_id, &guard), StepOutcome::Done(()));
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
fn tmpfs_unlink_missing_target_inode_returns_enoent_without_mutating_namespace() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let ghost_id = FsObjectId::new(0xfeed);
    let ghost_name = InlineName::new(b"ghost").expect("ghost inline name");

    {
        let mut state = tmpfs.state.lock();
        let root_inode = state
            .inodes
            .get_mut(&TMPFS_ROOT_OBJECT_ID)
            .expect("root inode");
        let TmpfsPayload::Directory(children) = &mut root_inode.payload else {
            panic!("root must be directory");
        };
        children.insert(ghost_name, ghost_id);
    }

    assert_eq!(
        tmpfs.unlink(TMPFS_ROOT_OBJECT_ID, b"ghost", ghost_id, &guard),
        StepOutcome::Err(Errno::ENOENT)
    );

    let state = tmpfs.state.lock();
    let root_inode = state
        .inodes
        .get(&TMPFS_ROOT_OBJECT_ID)
        .expect("root inode after failed unlink");
    let TmpfsPayload::Directory(children) = &root_inode.payload else {
        panic!("root must be directory");
    };
    assert_eq!(children.get(&ghost_name).copied(), Some(ghost_id));
}

#[test]
fn tmpfs_destroy_inode_preserves_linked_inode() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"live", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    assert_eq!(tmpfs.destroy_inode(file_id, &guard), StepOutcome::Done(()));
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"live", &guard),
        StepOutcome::Done(file_id)
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("linked inode must survive destroy_inode: {other:?}"),
    };
    assert_eq!(meta.nlinks, 1);
}

#[test]
fn tmpfs_link_increments_nlink_and_unlink_decrements_one_name() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"primary", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    assert_eq!(
        tmpfs.link(TMPFS_ROOT_OBJECT_ID, b"alias", file_id, &guard),
        StepOutcome::Done(())
    );

    let linked_meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta after link failed: {other:?}"),
    };
    assert_eq!(linked_meta.nlinks, 2);

    assert_eq!(
        tmpfs.unlink(TMPFS_ROOT_OBJECT_ID, b"primary", file_id, &guard),
        StepOutcome::Done(())
    );

    let after_unlink = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta after unlink failed: {other:?}"),
    };
    assert_eq!(after_unlink.nlinks, 1);
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"alias", &guard),
        StepOutcome::Done(file_id)
    );
}

#[test]
fn tmpfs_link_missing_parent_returns_enoent() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"primary", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    assert_eq!(
        tmpfs.link(FsObjectId::new(0xdead_beef), b"alias", file_id, &guard),
        StepOutcome::Err(step_engine::Errno::ENOENT)
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta after failed link failed: {other:?}"),
    };
    assert_eq!(meta.nlinks, 1);
}

#[test]
fn tmpfs_rename_over_hard_linked_target_decrements_one_name() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (source_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"source", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create source failed: {other:?}"),
        };
    let (target_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"target", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create target failed: {other:?}"),
        };

    assert_eq!(
        tmpfs.link(TMPFS_ROOT_OBJECT_ID, b"target_alias", target_id, &guard),
        StepOutcome::Done(())
    );

    assert_eq!(
        tmpfs.rename(
            TMPFS_ROOT_OBJECT_ID,
            b"source",
            TMPFS_ROOT_OBJECT_ID,
            b"target",
            &guard
        ),
        StepOutcome::Done(())
    );

    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"target", &guard),
        StepOutcome::Done(source_id)
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"target_alias", &guard),
        StepOutcome::Done(target_id)
    );

    let target_meta = match tmpfs.load_inode_meta(target_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("hard-linked displaced target must remain addressable: {other:?}"),
    };
    assert_eq!(target_meta.nlinks, 1);
}

#[test]
fn tmpfs_rename_over_regular_target_keeps_displaced_inode_until_destroy_inode() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (source_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"source", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create source failed: {other:?}"),
        };
    let (target_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"target", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create target failed: {other:?}"),
        };

    assert_eq!(
        tmpfs.rename(
            TMPFS_ROOT_OBJECT_ID,
            b"source",
            TMPFS_ROOT_OBJECT_ID,
            b"target",
            &guard
        ),
        StepOutcome::Done(())
    );

    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"target", &guard),
        StepOutcome::Done(source_id)
    );
    let displaced_meta = match tmpfs.load_inode_meta(target_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("displaced inode must remain until destroy_inode: {other:?}"),
    };
    assert_eq!(displaced_meta.nlinks, 0);

    assert_eq!(
        tmpfs.destroy_inode(target_id, &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        tmpfs.load_inode_meta(target_id, &guard),
        StepOutcome::Err(Errno::ENOENT)
    );
}

#[test]
fn tmpfs_rename_between_hard_links_to_same_inode_is_noop() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"primary", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };
    assert_eq!(
        tmpfs.link(TMPFS_ROOT_OBJECT_ID, b"alias", file_id, &guard),
        StepOutcome::Done(())
    );

    assert_eq!(
        tmpfs.rename(
            TMPFS_ROOT_OBJECT_ID,
            b"primary",
            TMPFS_ROOT_OBJECT_ID,
            b"alias",
            &guard
        ),
        StepOutcome::Done(())
    );

    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"primary", &guard),
        StepOutcome::Done(file_id)
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"alias", &guard),
        StepOutcome::Done(file_id)
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta after same-inode rename failed: {other:?}"),
    };
    assert_eq!(meta.nlinks, 2);
}

#[test]
fn tmpfs_rename_file_over_directory_returns_eisdir_without_mutating_namespace() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"file", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create file failed: {other:?}"),
        };
    let (dir_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"dir", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir failed: {other:?}"),
    };

    assert_eq!(
        tmpfs.rename(
            TMPFS_ROOT_OBJECT_ID,
            b"file",
            TMPFS_ROOT_OBJECT_ID,
            b"dir",
            &guard
        ),
        StepOutcome::Err(Errno::EISDIR)
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"file", &guard),
        StepOutcome::Done(file_id)
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"dir", &guard),
        StepOutcome::Done(dir_id)
    );
}

#[test]
fn tmpfs_rename_directory_over_file_returns_enotdir_without_mutating_namespace() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (dir_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"dir", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir failed: {other:?}"),
    };
    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"file", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create file failed: {other:?}"),
        };

    assert_eq!(
        tmpfs.rename(
            TMPFS_ROOT_OBJECT_ID,
            b"dir",
            TMPFS_ROOT_OBJECT_ID,
            b"file",
            &guard
        ),
        StepOutcome::Err(Errno::ENOTDIR)
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"dir", &guard),
        StepOutcome::Done(dir_id)
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"file", &guard),
        StepOutcome::Done(file_id)
    );
}

#[test]
fn tmpfs_rename_directory_over_nonempty_directory_returns_enotempty_without_mutating_namespace() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (source_dir_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"source_dir", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir source_dir failed: {other:?}"),
        };
    let (target_dir_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"target_dir", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir target_dir failed: {other:?}"),
        };
    let (child_id, _) = match tmpfs.create_inode(target_dir_id, b"child", 0o100644, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("create child in target_dir failed: {other:?}"),
    };

    assert_eq!(
        tmpfs.rename(
            TMPFS_ROOT_OBJECT_ID,
            b"source_dir",
            TMPFS_ROOT_OBJECT_ID,
            b"target_dir",
            &guard
        ),
        StepOutcome::Err(Errno::ENOTEMPTY)
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"source_dir", &guard),
        StepOutcome::Done(source_dir_id)
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"target_dir", &guard),
        StepOutcome::Done(target_dir_id)
    );
    assert_eq!(
        tmpfs.lookup(target_dir_id, b"child", &guard),
        StepOutcome::Done(child_id)
    );
}

#[test]
fn tmpfs_rename_directory_over_empty_directory_updates_parent_nlinks() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (source_dir_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"source_dir", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir source_dir failed: {other:?}"),
        };
    let (target_dir_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"target_dir", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir target_dir failed: {other:?}"),
        };

    let root_before = match tmpfs.load_inode_meta(TMPFS_ROOT_OBJECT_ID, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load root meta before rename failed: {other:?}"),
    };
    assert_eq!(root_before.nlinks, 4);

    assert_eq!(
        tmpfs.rename(
            TMPFS_ROOT_OBJECT_ID,
            b"source_dir",
            TMPFS_ROOT_OBJECT_ID,
            b"target_dir",
            &guard
        ),
        StepOutcome::Done(())
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"target_dir", &guard),
        StepOutcome::Done(source_dir_id)
    );
    let displaced_meta = match tmpfs.load_inode_meta(target_dir_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("displaced directory must remain until destroy_inode: {other:?}"),
    };
    assert_eq!(displaced_meta.nlinks, 0);
    let root_after = match tmpfs.load_inode_meta(TMPFS_ROOT_OBJECT_ID, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load root meta after rename failed: {other:?}"),
    };
    assert_eq!(
        root_after.nlinks, 3,
        "replacing an empty child directory removes one '..' contribution"
    );
}

#[test]
fn tmpfs_rename_directory_over_empty_directory_keeps_displaced_inode_until_destroy_inode() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (source_dir_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"source_dir", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir source_dir failed: {other:?}"),
        };
    let (target_dir_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"target_dir", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir target_dir failed: {other:?}"),
        };

    assert_eq!(
        tmpfs.rename(
            TMPFS_ROOT_OBJECT_ID,
            b"source_dir",
            TMPFS_ROOT_OBJECT_ID,
            b"target_dir",
            &guard
        ),
        StepOutcome::Done(())
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"target_dir", &guard),
        StepOutcome::Done(source_dir_id)
    );
    let displaced_meta = match tmpfs.load_inode_meta(target_dir_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("displaced directory must remain until destroy_inode: {other:?}"),
    };
    assert_eq!(displaced_meta.nlinks, 0);

    assert_eq!(
        tmpfs.destroy_inode(target_dir_id, &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        tmpfs.load_inode_meta(target_dir_id, &guard),
        StepOutcome::Err(Errno::ENOENT)
    );
}

#[test]
fn tmpfs_rename_file_across_directories_moves_name() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (old_dir_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"old_dir", 0o755, &cred, &guard)
    {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir old_dir failed: {other:?}"),
    };
    let (new_dir_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"new_dir", 0o755, &cred, &guard)
    {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir new_dir failed: {other:?}"),
    };
    let (file_id, _) = match tmpfs.create_inode(old_dir_id, b"file", 0o100644, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("create file in old_dir failed: {other:?}"),
    };

    assert_eq!(
        tmpfs.rename(old_dir_id, b"file", new_dir_id, b"moved", &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        tmpfs.lookup(old_dir_id, b"file", &guard),
        StepOutcome::Err(Errno::ENOENT)
    );
    assert_eq!(
        tmpfs.lookup(new_dir_id, b"moved", &guard),
        StepOutcome::Done(file_id)
    );

    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load moved file meta failed: {other:?}"),
    };
    assert_eq!(meta.nlinks, 1);
}

#[test]
fn tmpfs_rename_directory_across_directories_moves_subtree() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (old_parent_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"old_parent", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir old_parent failed: {other:?}"),
        };
    let (new_parent_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"new_parent", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir new_parent failed: {other:?}"),
        };
    let (dir_id, _) = match tmpfs.mkdir(old_parent_id, b"dir", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir dir failed: {other:?}"),
    };
    let (child_id, _) = match tmpfs.create_inode(dir_id, b"child", 0o100644, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("create child in dir failed: {other:?}"),
    };

    assert_eq!(
        tmpfs.rename(old_parent_id, b"dir", new_parent_id, b"moved_dir", &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        tmpfs.lookup(old_parent_id, b"dir", &guard),
        StepOutcome::Err(Errno::ENOENT)
    );
    assert_eq!(
        tmpfs.lookup(new_parent_id, b"moved_dir", &guard),
        StepOutcome::Done(dir_id)
    );
    assert_eq!(
        tmpfs.lookup(dir_id, b"child", &guard),
        StepOutcome::Done(child_id)
    );
}

#[test]
fn tmpfs_rename_directory_across_directories_updates_parent_nlinks() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (old_parent_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"old_parent", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir old_parent failed: {other:?}"),
        };
    let (new_parent_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"new_parent", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir new_parent failed: {other:?}"),
        };
    let (dir_id, _) = match tmpfs.mkdir(old_parent_id, b"dir", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir dir failed: {other:?}"),
    };

    let old_before = match tmpfs.load_inode_meta(old_parent_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load old parent meta before rename failed: {other:?}"),
    };
    let new_before = match tmpfs.load_inode_meta(new_parent_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load new parent meta before rename failed: {other:?}"),
    };
    assert_eq!(old_before.nlinks, 3);
    assert_eq!(new_before.nlinks, 2);

    assert_eq!(
        tmpfs.rename(old_parent_id, b"dir", new_parent_id, b"moved_dir", &guard),
        StepOutcome::Done(())
    );

    let old_after = match tmpfs.load_inode_meta(old_parent_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load old parent meta after rename failed: {other:?}"),
    };
    let new_after = match tmpfs.load_inode_meta(new_parent_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load new parent meta after rename failed: {other:?}"),
    };
    assert_eq!(old_after.nlinks, 2);
    assert_eq!(new_after.nlinks, 3);
    assert_eq!(
        tmpfs.lookup(new_parent_id, b"moved_dir", &guard),
        StepOutcome::Done(dir_id)
    );
}

#[test]
fn tmpfs_rename_directory_into_own_descendant_returns_einval_without_mutating_namespace() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (dir_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"dir", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir dir failed: {other:?}"),
    };
    let (child_dir_id, _) = match tmpfs.mkdir(dir_id, b"child_dir", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir child_dir failed: {other:?}"),
    };

    assert_eq!(
        tmpfs.rename(
            TMPFS_ROOT_OBJECT_ID,
            b"dir",
            child_dir_id,
            b"moved_dir",
            &guard
        ),
        StepOutcome::Err(Errno::EINVAL)
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"dir", &guard),
        StepOutcome::Done(dir_id)
    );
    assert_eq!(
        tmpfs.lookup(dir_id, b"child_dir", &guard),
        StepOutcome::Done(child_dir_id)
    );
    assert_eq!(
        tmpfs.lookup(child_dir_id, b"moved_dir", &guard),
        StepOutcome::Err(Errno::ENOENT)
    );
}

#[test]
fn tmpfs_fetch_page_materialises_anon_then_flush_noop() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"page-test", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    // First fetch materialises a fresh anon frame.
    let frame_first = match <Tmpfs as FsPageBacking>::fetch_page(&*tmpfs, file_id, 0, &guard) {
        StepOutcome::Done(frame) => frame,
        other => panic!("fetch_page failed: {other:?}"),
    };

    // Second fetch returns the same PPN — anon container cached the
    // page on the first access.
    let frame_second = match <Tmpfs as FsPageBacking>::fetch_page(&*tmpfs, file_id, 0, &guard) {
        StepOutcome::Done(frame) => frame,
        other => panic!("fetch_page (second) failed: {other:?}"),
    };
    assert_eq!(frame_first.ppn(), frame_second.ppn());

    // flush_page is a no-op on tmpfs.
    assert_eq!(
        <Tmpfs as FsPageBacking>::flush_page(&*tmpfs, file_id, 0, &frame_first, &guard),
        StepOutcome::Done(())
    );
    // fsync is a no-op too.
    assert_eq!(
        <Tmpfs as FsPageBacking>::fsync_file(&*tmpfs, file_id, &guard),
        StepOutcome::Done(())
    );

    // Directories cannot be fetched as pages.
    assert_eq!(
        <Tmpfs as FsPageBacking>::fetch_page(&*tmpfs, TMPFS_ROOT_OBJECT_ID, 0, &guard),
        StepOutcome::Err(Errno::EISDIR)
    );

    // Misaligned offsets reject.
    assert_eq!(
        <Tmpfs as FsPageBacking>::fetch_page(&*tmpfs, file_id, 17, &guard),
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
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"trunc", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    // Grow visible size to one full page via truncate.
    let page = tx_subsystems::vm::USER_PAGE_SIZE as u64;
    assert_eq!(
        <Tmpfs as FsPageBacking>::truncate(&*tmpfs, file_id, page, &guard),
        StepOutcome::Done(())
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta failed: {other:?}"),
    };
    assert_eq!(meta.size, page);

    // Shrink back to 0 — visible size + payload size both reset.
    assert_eq!(
        <Tmpfs as FsPageBacking>::truncate(&*tmpfs, file_id, 0, &guard),
        StepOutcome::Done(())
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta failed: {other:?}"),
    };
    assert_eq!(meta.size, 0);

    // Truncate of a directory rejects.
    assert_eq!(
        <Tmpfs as FsPageBacking>::truncate(&*tmpfs, TMPFS_ROOT_OBJECT_ID, 0, &guard),
        StepOutcome::Err(Errno::EISDIR)
    );

    // Truncate of an absent inode rejects.
    assert_eq!(
        <Tmpfs as FsPageBacking>::truncate(&*tmpfs, FsObjectId::new(0xdead_beef), 0, &guard),
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
    let guard = guard();
    let cred = Credential::root();

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
fn tmpfs_failed_create_does_not_consume_file_backing_or_object_id() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (first_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"dup", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("initial create_inode failed: {other:?}"),
        };

    assert_eq!(
        tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"dup", 0o100644, &cred, &guard),
        StepOutcome::Err(Errno::EEXIST)
    );

    let (next_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"next", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("next create_inode failed: {other:?}"),
        };
    assert_eq!(
        next_id.as_u64(),
        first_id.as_u64() + 1,
        "failed create_inode must reject before allocating file backing or an object id"
    );
}

#[test]
fn tmpfs_failed_mkdir_does_not_consume_object_id() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (first_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"dupdir", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("initial mkdir failed: {other:?}"),
    };

    assert_eq!(
        tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"dupdir", 0o755, &cred, &guard),
        StepOutcome::Err(Errno::EEXIST)
    );

    let (next_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"after_dir", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode after failed mkdir failed: {other:?}"),
        };
    assert_eq!(
        next_id.as_u64(),
        first_id.as_u64() + 1,
        "failed mkdir must not consume an object id"
    );
}

#[test]
fn tmpfs_failed_symlink_does_not_consume_target_copy_or_object_id() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (first_id, _) =
        match tmpfs.symlink(TMPFS_ROOT_OBJECT_ID, b"duplink", b"target", &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("initial symlink failed: {other:?}"),
        };

    assert_eq!(
        tmpfs.symlink(
            TMPFS_ROOT_OBJECT_ID,
            b"duplink",
            b"unused-target",
            &cred,
            &guard
        ),
        StepOutcome::Err(Errno::EEXIST)
    );

    let (next_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"after_link", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode after failed symlink failed: {other:?}"),
        };
    assert_eq!(
        next_id.as_u64(),
        first_id.as_u64() + 1,
        "failed symlink must reject before copying the target bytes or consuming an object id"
    );
}

#[test]
fn tmpfs_rmdir_nonempty_returns_enotempty() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

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

#[test]
fn tmpfs_rmdir_wrong_target_returns_enoent_before_inspecting_unrelated_target() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (empty_dir_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"empty_dir", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir empty_dir failed: {other:?}"),
        };
    let (nonempty_dir_id, _) =
        match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"nonempty_dir", 0o755, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("mkdir nonempty_dir failed: {other:?}"),
        };
    let _child = match tmpfs.create_inode(nonempty_dir_id, b"child", 0o100644, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("create child failed: {other:?}"),
    };

    assert_eq!(
        tmpfs.rmdir(TMPFS_ROOT_OBJECT_ID, b"empty_dir", nonempty_dir_id, &guard),
        StepOutcome::Err(Errno::ENOENT),
        "rmdir must verify parent/name maps to target before inspecting target contents"
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"empty_dir", &guard),
        StepOutcome::Done(empty_dir_id)
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"nonempty_dir", &guard),
        StepOutcome::Done(nonempty_dir_id)
    );
}

#[test]
fn tmpfs_rmdir_keeps_directory_inode_until_destroy_inode() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (dir_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"victim_dir", 0o755, &cred, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("mkdir failed: {other:?}"),
    };

    assert_eq!(
        tmpfs.rmdir(TMPFS_ROOT_OBJECT_ID, b"victim_dir", dir_id, &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"victim_dir", &guard),
        StepOutcome::Err(Errno::ENOENT)
    );
    let unlinked_meta = match tmpfs.load_inode_meta(dir_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("rmdir target must remain until destroy_inode: {other:?}"),
    };
    assert_eq!(unlinked_meta.nlinks, 0);

    assert_eq!(tmpfs.destroy_inode(dir_id, &guard), StepOutcome::Done(()));
    assert_eq!(
        tmpfs.load_inode_meta(dir_id, &guard),
        StepOutcome::Err(Errno::ENOENT)
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
    let guard = guard();
    let cred = Credential::root();

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
    let rnode = match <Tmpfs as FsOps>::materialise_rnode(
        &*tmpfs,
        file_id,
        file_meta,
        &test_mount_payload(&tmpfs),
        &guard,
    ) {
        StepOutcome::Done(rnode) => rnode,
        other => panic!("materialise_rnode for regular file: {other:?}"),
    };
    match rnode.backing() {
        RNodeBacking::PageBacked { pc } => {
            // Sanity: the container's `page_count` matches tmpfs's
            // static cap (`TMPFS_FILE_PAGE_CAP = 2048`). `size_bytes`
            // initialises to `page_count * USER_PAGE_SIZE` (the
            // PageContainer's capacity); inode-visible size lives on
            // `InodeMeta::size`, not the container, so we don't pin
            // the byte-size here. The shared-Cap contract is the
            // important part: cloning the inode's container into the
            // RNode means writes via `FsPageBacking` and reads via
            // `OpenFile::step_read` / `read_exact_at` see the same
            // underlying pages.
            assert_eq!(pc.page_count(), 2048, "tmpfs file page-cap shape");
        }
        other => panic!("expected PageBacked backing for regular file, got {other:?}"),
    }
    // RNode meta round-trips the input meta.
    assert_eq!(rnode.fs_object_id(), file_id);
    assert_eq!(rnode.meta().kind(), InodeKind::Regular);
}

#[test]
fn tmpfs_materialise_rnode_uses_backend_inode_meta_not_stale_input() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"stale_meta", 0o100640, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    let mut stale_meta = InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o777);
    stale_meta.nlinks = 99;

    let rnode = match <Tmpfs as FsOps>::materialise_rnode(
        &*tmpfs,
        file_id,
        stale_meta,
        &test_mount_payload(&tmpfs),
        &guard,
    ) {
        StepOutcome::Done(rnode) => rnode,
        other => panic!("materialise_rnode with stale meta failed: {other:?}"),
    };

    assert_eq!(rnode.meta().kind(), InodeKind::Regular);
    assert_eq!(rnode.meta().mode & S_IFMT, S_IFREG);
    assert_eq!(rnode.meta().nlinks, 1);
}

#[test]
fn tmpfs_tmpfile_shape_read_after_write_survives_unlink() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, file_meta) = match tmpfs.create_inode(
        TMPFS_ROOT_OBJECT_ID,
        b"tmpfile_probe",
        0o100600,
        &cred,
        &guard,
    ) {
        StepOutcome::Done(out) => out,
        other => panic!("create_inode failed: {other:?}"),
    };
    let rnode = match <Tmpfs as FsOps>::materialise_rnode(
        &*tmpfs,
        file_id,
        file_meta,
        &test_mount_payload(&tmpfs),
        &guard,
    ) {
        StepOutcome::Done(rnode) => rnode,
        other => panic!("materialise_rnode failed: {other:?}"),
    };
    let file = OpenFile::new_cap(
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
    .expect("open tmpfile-shaped file");

    let payload: alloc::vec::Vec<u8> = (0..8192).map(|i| (i % 251) as u8).collect();
    assert_eq!(
        file.step_write(&payload, &guard),
        StepOutcome::Done(payload.len())
    );

    assert_eq!(
        tmpfs.unlink(TMPFS_ROOT_OBJECT_ID, b"tmpfile_probe", file_id, &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"tmpfile_probe", &guard),
        StepOutcome::Err(Errno::ENOENT)
    );

    assert_eq!(file.step_lseek(0, 0, &guard), StepOutcome::Done(0));
    let mut readback = alloc::vec![0u8; payload.len()];
    assert_eq!(
        file.step_read(&mut readback, &guard),
        StepOutcome::Done(payload.len())
    );
    assert_eq!(readback, payload);
}

#[test]
fn tmpfs_materialise_rnode_for_directory_returns_eisdir() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();

    // The walker handles Directory inline; reaching the override
    // with a directory inode is a backend bug. The override returns
    // `EISDIR` rather than panicking so a misroute surfaces as a
    // recoverable error.
    let meta = match tmpfs.load_inode_meta(TMPFS_ROOT_OBJECT_ID, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta(root): {other:?}"),
    };
    assert_eq!(
        <Tmpfs as FsOps>::materialise_rnode(
            &*tmpfs,
            TMPFS_ROOT_OBJECT_ID,
            meta,
            &test_mount_payload(&tmpfs),
            &guard
        ),
        StepOutcome::Err(Errno::EISDIR)
    );
}

#[test]
fn tmpfs_materialise_rnode_for_symlink_returns_einval() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

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
        <Tmpfs as FsOps>::materialise_rnode(
            &*tmpfs,
            link_id,
            link_meta,
            &test_mount_payload(&tmpfs),
            &guard
        ),
        StepOutcome::Err(Errno::EINVAL)
    );
}

#[test]
fn tmpfs_read_link_returns_target_bytes() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (link_id, _) =
        match tmpfs.symlink(TMPFS_ROOT_OBJECT_ID, b"link", b"target/path", &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("symlink failed: {other:?}"),
        };
    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"regular", 0o100644, &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode failed: {other:?}"),
        };

    let target = match <Tmpfs as FsOps>::read_link(&*tmpfs, link_id, &guard) {
        StepOutcome::Done(target) => target,
        other => panic!("read_link failed: {other:?}"),
    };
    assert_eq!(target.as_ref(), b"target/path");
    assert_eq!(
        <Tmpfs as FsOps>::read_link(&*tmpfs, file_id, &guard),
        StepOutcome::Err(Errno::EINVAL)
    );
}

#[test]
fn tmpfs_symlink_payload_uses_shared_target_bytes() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (link_id, _) =
        match tmpfs.symlink(TMPFS_ROOT_OBJECT_ID, b"link", b"target/path", &cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("symlink failed: {other:?}"),
        };

    let state = tmpfs.state.lock();
    let inode = state.inodes.get(&link_id).expect("link inode");
    let TmpfsPayload::Symlink(target) = &inode.payload else {
        panic!("link inode should carry symlink payload");
    };
    let target_clone = target.clone();
    assert_eq!(&*target_clone, b"target/path");
    assert_eq!(
        Arc::strong_count(target),
        2,
        "symlink payload clones should share target bytes instead of copying under the tmpfs lock"
    );
}

// ---------------------------------------------------------------------------
// `step_chmod` / `step_chown` tests (Wave 3 Part 2 of the DAC + setuid
// slice). Validate the POSIX permission rules tmpfs enforces:
//
// - chmod: caller must be the inode owner OR carry `CAP_FOWNER`;
//   otherwise EPERM.
// - chown: only `CAP_FOWNER` grants arbitrary changes; non-privileged
//   callers may chown only to their own uid/gid; setuid/setgid bits
//   are silently cleared on non-privileged chown (Linux's
//   anti-escalation rule).
// ---------------------------------------------------------------------------

fn cred_with_caps(uid: u32, gid: u32, caps: CapabilitySet) -> Credential {
    Credential {
        uid,
        gid,
        effective_caps: caps,
    }
}

#[test]
fn tmpfs_chmod_owner_succeeds() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let owner = cred_with_caps(1000, 0, CapabilitySet::EMPTY);

    // Create a file owned by uid 1000 with mode 0o644.
    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode: {other:?}"),
        };
    assert_eq!(
        <Tmpfs as FsOps>::step_chmod(&*tmpfs, file_id, 0o600, &owner, &guard),
        StepOutcome::Done(())
    );

    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.mode & !S_IFMT, 0o600);
}

#[test]
fn tmpfs_chmod_non_owner_returns_eperm() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let owner = cred_with_caps(1000, 0, CapabilitySet::EMPTY);
    let stranger = cred_with_caps(2000, 0, CapabilitySet::EMPTY);

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode: {other:?}"),
        };
    assert_eq!(
        <Tmpfs as FsOps>::step_chmod(&*tmpfs, file_id, 0o600, &stranger, &guard),
        StepOutcome::Err(Errno::EPERM)
    );
}

#[test]
fn tmpfs_chmod_with_fowner_cap_succeeds() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let owner = cred_with_caps(1000, 0, CapabilitySet::EMPTY);
    let mut admin_caps = CapabilitySet::EMPTY;
    admin_caps.add(Capability::FOWNER);
    let admin = cred_with_caps(2000, 0, admin_caps);

    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode: {other:?}"),
        };
    assert_eq!(
        <Tmpfs as FsOps>::step_chmod(&*tmpfs, file_id, 0o755, &admin, &guard),
        StepOutcome::Done(())
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.mode & !S_IFMT, 0o755);
}

#[test]
fn tmpfs_chown_unprivileged_to_self_succeeds() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let owner = cred_with_caps(1000, 200, CapabilitySet::EMPTY);
    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode: {other:?}"),
        };

    // Chown to current uid/gid (no-op-shaped success).
    assert_eq!(
        <Tmpfs as FsOps>::step_chown(&*tmpfs, file_id, Some(1000), Some(200), &owner, &guard),
        StepOutcome::Done(())
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.uid, 1000);
    assert_eq!(meta.gid, 200);
}

#[test]
fn tmpfs_chown_unprivileged_to_other_returns_eperm() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let owner = cred_with_caps(1000, 200, CapabilitySet::EMPTY);
    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode: {other:?}"),
        };

    // Try to chown to a foreign uid; non-privileged callers can only
    // chown to their own uid.
    assert_eq!(
        <Tmpfs as FsOps>::step_chown(&*tmpfs, file_id, Some(2000), None, &owner, &guard),
        StepOutcome::Err(Errno::EPERM)
    );
    // Same for gid.
    assert_eq!(
        <Tmpfs as FsOps>::step_chown(&*tmpfs, file_id, None, Some(999), &owner, &guard),
        StepOutcome::Err(Errno::EPERM)
    );
}

#[test]
fn tmpfs_chown_clears_setuid_bit_for_non_privileged() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let mut admin_caps = CapabilitySet::EMPTY;
    admin_caps.add(Capability::FOWNER);
    let admin = cred_with_caps(0, 0, admin_caps);

    // Create a setuid+setgid file owned by uid 1000.
    let owner = cred_with_caps(1000, 200, CapabilitySet::EMPTY);
    let mode = (S_ISUID | S_ISGID | 0o755) | tx_subsystems::vfs::S_IFREG;
    let (file_id, _) = match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", mode, &owner, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("create_inode: {other:?}"),
    };
    // Sanity: setuid/setgid should be set on the freshly-created
    // inode (the mode passed in is preserved by `create_inode`).
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.mode & S_ISUID, S_ISUID);
    assert_eq!(meta.mode & S_ISGID, S_ISGID);

    // Non-privileged owner self-chown clears the setuid + setgid
    // bits. Use the owner cred (no CAP_FOWNER).
    assert_eq!(
        <Tmpfs as FsOps>::step_chown(&*tmpfs, file_id, Some(1000), Some(200), &owner, &guard),
        StepOutcome::Done(())
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.mode & S_ISUID, 0);
    assert_eq!(meta.mode & S_ISGID, 0);

    // Linux's chown(2) clearing rule also applies to privileged
    // callers for executable files: setuid is cleared, and setgid is
    // cleared when the group-execute bit is present.
    assert_eq!(
        <Tmpfs as FsOps>::step_chmod(&*tmpfs, file_id, S_ISUID | S_ISGID | 0o770, &admin, &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        <Tmpfs as FsOps>::step_chown(&*tmpfs, file_id, Some(1000), None, &admin, &guard),
        StepOutcome::Done(())
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.mode & (S_ISUID | S_ISGID), 0);
    assert_eq!(meta.mode & !S_IFMT, 0o770);
}

#[test]
fn tmpfs_chown_preserves_setgid_without_group_execute() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let admin = cred_with_caps(0, 0, CapabilitySet::FULL);
    let mode = (S_ISUID | S_ISGID | 0o700) | tx_subsystems::vfs::S_IFREG;
    let (file_id, _) = match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", mode, &admin, &guard) {
        StepOutcome::Done(out) => out,
        other => panic!("create_inode: {other:?}"),
    };

    assert_eq!(
        <Tmpfs as FsOps>::step_chown(&*tmpfs, file_id, Some(0), Some(0), &admin, &guard),
        StepOutcome::Done(())
    );
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.mode & S_ISUID, 0);
    assert_eq!(meta.mode & S_ISGID, S_ISGID);
    assert_eq!(meta.mode & !S_IFMT, S_ISGID | 0o700);
}

// === FsOps + FsPageBacking tests ====================================
//
// Tests below pin the outcome shape end-to-end through both `FsOps`
// and `FsPageBacking` on `Tmpfs`.

#[test]
fn tmpfs_v3_lookup_round_trips_after_create() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    use step_engine::{Errno as V3Errno, NoProgress, StepOutcome as V3};
    use tx_subsystems::vfs::FsOps;

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    // create_inode.
    let (file_id, file_meta) = match <Tmpfs as FsOps>::create_inode(
        &*tmpfs,
        TMPFS_ROOT_OBJECT_ID,
        b"hello-v3",
        0o100644,
        &cred,
        &guard,
    ) {
        V3::Done((id, meta)) => (id, meta),
        other => panic!("create_inode v3: {other:?}"),
    };
    assert_eq!(file_meta.kind(), InodeKind::Regular);

    // lookup v3 round-trips the same id.
    assert_eq!(
        <Tmpfs as FsOps>::lookup(&*tmpfs, TMPFS_ROOT_OBJECT_ID, b"hello-v3", &guard),
        V3::<_, NoProgress>::done(file_id)
    );

    // missing-name → ENOENT round-trips through the v3 errno bridge.
    assert_eq!(
        <Tmpfs as FsOps>::lookup(&*tmpfs, TMPFS_ROOT_OBJECT_ID, b"missing-v3", &guard),
        V3::<FsObjectId, NoProgress>::err(V3Errno::ENOENT)
    );
}

#[test]
fn tmpfs_v3_mkdir_yields_directory_inode() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    use step_engine::StepOutcome as V3;
    use tx_subsystems::vfs::FsOps;

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (dir_id, dir_meta) = match <Tmpfs as FsOps>::mkdir(
        &*tmpfs,
        TMPFS_ROOT_OBJECT_ID,
        b"v3dir",
        0o755,
        &cred,
        &guard,
    ) {
        V3::Done(out) => out,
        other => panic!("mkdir v3: {other:?}"),
    };
    assert_eq!(dir_meta.kind(), InodeKind::Directory);

    // load_inode_meta over v3 returns the same kind.
    let loaded = match <Tmpfs as FsOps>::load_inode_meta(&*tmpfs, dir_id, &guard) {
        V3::Done(m) => m,
        other => panic!("load_inode_meta v3: {other:?}"),
    };
    assert_eq!(loaded.kind(), InodeKind::Directory);
}

#[test]
fn tmpfs_v3_fetch_page_done_for_anon_file() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    use step_engine::{Errno as V3Errno, NoProgress, StepOutcome as V3};
    use tx_subsystems::page_backed::FsPageBacking;
    use tx_subsystems::vfs::FsOps;

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) = match <Tmpfs as FsOps>::create_inode(
        &*tmpfs,
        TMPFS_ROOT_OBJECT_ID,
        b"page-v3",
        0o100644,
        &cred,
        &guard,
    ) {
        V3::Done(out) => out,
        other => panic!("create_inode v3: {other:?}"),
    };

    let frame_first = match <Tmpfs as FsPageBacking>::fetch_page(&*tmpfs, file_id, 0, &guard) {
        V3::Done(frame) => frame,
        other => panic!("fetch_page v3: {other:?}"),
    };
    let frame_second = match <Tmpfs as FsPageBacking>::fetch_page(&*tmpfs, file_id, 0, &guard) {
        V3::Done(frame) => frame,
        other => panic!("fetch_page (second) v3: {other:?}"),
    };
    assert_eq!(frame_first.ppn(), frame_second.ppn());

    // Misaligned offsets reject through the v3 errno bridge.
    assert_eq!(
        <Tmpfs as FsPageBacking>::fetch_page(&*tmpfs, file_id, 17, &guard),
        V3::<tx_subsystems::page_backed::Frame, NoProgress>::err(V3Errno::EINVAL)
    );
}

#[test]
fn tmpfs_v3_truncate_then_load_meta_reflects_size() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    use step_engine::{NoProgress, StepOutcome as V3};
    use tx_subsystems::page_backed::FsPageBacking;
    use tx_subsystems::vfs::FsOps;

    let tmpfs = Arc::new(Tmpfs::new());
    let guard = guard();
    let cred = Credential::root();

    let (file_id, _) = match <Tmpfs as FsOps>::create_inode(
        &*tmpfs,
        TMPFS_ROOT_OBJECT_ID,
        b"trunc-v3",
        0o100644,
        &cred,
        &guard,
    ) {
        V3::Done(out) => out,
        other => panic!("create_inode v3: {other:?}"),
    };

    let page = tx_subsystems::vm::USER_PAGE_SIZE as u64;
    assert_eq!(
        <Tmpfs as FsPageBacking>::truncate(&*tmpfs, file_id, page, &guard),
        V3::<(), NoProgress>::done(())
    );

    let meta = match <Tmpfs as FsOps>::load_inode_meta(&*tmpfs, file_id, &guard) {
        V3::Done(m) => m,
        other => panic!("load_inode_meta v3: {other:?}"),
    };
    assert_eq!(meta.size, page);

    // fsync is a no-op on tmpfs in v3 too.
    assert_eq!(
        <Tmpfs as FsPageBacking>::fsync_file(&*tmpfs, file_id, &guard),
        V3::<(), NoProgress>::done(())
    );
}

// === Walker end-to-end against Tmpfs =================================
//
// This test pins that `FsOps for Tmpfs` exercises end-to-end through
// `step_walk` and through the fs_ops field on `MountOutput`.

#[test]
fn step_walk_against_tmpfs_resolves_real_path() {
    use step_engine::{reserve_for, sign_for, Cap, StepOutcome as V3};
    use tx_subsystems::mount::{
        DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
    };
    use tx_subsystems::vfs::structure::{
        DEntry, InlineName, InodeMeta, RNode, RNodeBacking, S_IFDIR,
    };

    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let (tmpfs, mount_output) = Tmpfs::new_root();

    let payload: Cap<MountPayload> = MountPayload::new_cap(
        mount_output.fs_ops.clone(),
        mount_output.fs_page_backing.clone(),
        None,
        DevId::new(1),
        MountOptions::default(),
        "tmpfs",
        SourceLabel::Static("rootfs-tmpfs-v3"),
    )
    .expect("payload reservation");

    let root_rnode: Cap<RNode> = {
        let raw = RNode::new(
            mount_output.root_fs_object_id,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        )
        .with_containing_mount(&payload);
        let res = reserve_for::<RNode>().expect("rnode reservation");
        sign_for(res, raw)
    };

    let _mount = MountIdentity::new_cap(
        MountId::new(1),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("mount identity reservation");

    let root_dentry: Cap<DEntry> =
        DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");

    // Use real Tmpfs mkdir to add an entry the walker has to find by
    // resolving through `FsOps for Tmpfs`.
    let cred = Credential::root();
    let guard = guard();
    let _new_dir = match tmpfs.mkdir(mount_output.root_fs_object_id, b"dir", 0o755, &cred, &guard) {
        V3::Done(out) => out,
        other => panic!("tmpfs mkdir failed: {other:?}"),
    };

    // Block-device-backed mounts can yield from `step_walk` (via
    // `FsPageBacking::fetch_page` returning `Yield` while a sector
    // load is in flight), so the canonical drive shape is async via
    // the `PathWalkOp` StepOp wrap. tmpfs itself never yields, so the
    // inline `step()` loop here resolves on the first poll — but the
    // shape mirrors what a block-backed FS would drive through the
    // reactor's `tx_scripts::drive::drive::<PathWalkOp, _>` loop.
    drop(guard);
    use tx_substrate::step::{
        NoProgress, ProcessIdentity, ScriptCtx, StepOp, StepOutcome as V3Outcome,
    };
    use tx_subsystems::vfs::PathWalkOp;
    let mut op = PathWalkOp {
        rooted_at: root_dentry.clone(),
        path: b"/dir".to_vec(),
        cred,
    };
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();
    let outcome: V3<Cap<DEntry>, NoProgress> = loop {
        match StepOp::<ProcessIdentity>::step(&mut op, &mut ctx) {
            V3Outcome::Done(d) => break V3::Done(d),
            V3Outcome::Err(e) => break V3::Err(e),
            V3Outcome::Continue { .. } => continue,
            V3Outcome::Yield { .. } => {
                panic!("tmpfs walk should not yield; block-backed fs would park here");
            }
        }
    };
    match outcome {
        V3::Done(d) => {
            assert_eq!(d.name().as_bytes(), b"dir");
        }
        other => panic!("expected v3 Done(dir) against tmpfs, got {other:?}"),
    }
}
