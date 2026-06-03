//! Phase 3b tests: tmpfs as a standalone backend.
//!
//! Mount-table wiring is exercised separately in `tx-kernel`'s init
//! tests; these tests construct a `Tmpfs` instance directly and
//! exercise the `FsOps` + `FsPageBacking` surface against it.

use alloc::sync::Arc;

use super::adapter::step_engine::{self as step_engine, Errno, StepOutcome, guard, page_allocator};
use tx_subsystems::cred::{Capability, CapabilitySet};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::vfs::{
    Credential, DirCursor, FsObjectId, FsOps, InodeKind, OpenFile, OpenFileFlags, RNodeBacking,
    S_IFMT, S_ISGID, S_ISUID,
};

use super::{TMPFS_ROOT_OBJECT_ID, Tmpfs};

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

    // Privileged callers preserve the setuid bit on chown — set
    // it again, then chown via admin and verify it survives.
    assert_eq!(
        <Tmpfs as FsOps>::step_chmod(&*tmpfs, file_id, S_ISUID | 0o755, &admin, &guard),
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
    assert_eq!(
        meta.mode & S_ISUID,
        S_ISUID,
        "privileged chown should preserve S_ISUID"
    );
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
    use step_engine::{Cap, StepOutcome as V3, reserve_for, sign_for};
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
