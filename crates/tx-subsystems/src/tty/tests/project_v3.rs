//! `FsOps` + `FsPageBacking` impls on `DevptsInstance`.
//!
//! Devpts is a PTY-side projection: no `Advanced` / `Blocked`
//! outcomes are produced — every body lands on `Done` or `Err`.

use alloc::format;

use crate::execution::StepOutcome;
use crate::page_backed::{Frame, FsPageBacking};
use crate::test_support::EPOCH_TEST_LOCK as TTY_ZONE_TEST_LOCK;
use crate::tty::project::{
    open_ptmx, DevptsInstance, DEVPTS_PTMX_OBJECT_ID, DEVPTS_ROOT_OBJECT_ID,
};
use crate::vfs::{Credential, DirCursor, FsObjectId, FsOps, InodeKind, InodeMeta};

use tx_hal::Ppn;
use tx_substrate::step_v3::{Errno as V3Errno, NoProgress, StepOutcome as V3};

/// Register every kernel zone (matching `legacy_phase_a::init_zones`)
/// because `open_ptmx` allocates PTY identity / payload / open-file
/// caps that span the tty + page-backed + vfs zone tables. The
/// shared `support::init_zones` helper only registers the tty zones,
/// so calling it here would fail later allocations with `EIO`.
fn init_zones() {
    tx_substrate::testing::init_host_for_test_once();
    crate::zones::register_all().expect("kernel zones");
    crate::tty::structure::registry::reset_for_tests();
}

// === FsOps lookup / load_inode_meta / readdir ========================

#[test]
fn devpts_v3_lookup_round_trips_to_ptmx_and_allocated_slaves() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let devpts = DevptsInstance;

    // Allocate a single PTY so a numeric devpts entry exists.
    let pty = match open_ptmx(&guard) {
        StepOutcome::Done(pty) => pty,
        other => panic!("open_ptmx failed: {other:?}"),
    };

    // ptmx is the static entry.
    assert_eq!(
        <DevptsInstance as FsOps>::lookup(&devpts, DEVPTS_ROOT_OBJECT_ID, b"ptmx", &guard),
        V3::<_, NoProgress>::done(DEVPTS_PTMX_OBJECT_ID)
    );

    // The numeric slave entry resolves to a backend-shaped FsObjectId.
    let slave_name = format!("{}", pty.index);
    let v3_slave = <DevptsInstance as FsOps>::lookup(
        &devpts,
        DEVPTS_ROOT_OBJECT_ID,
        slave_name.as_bytes(),
        &guard,
    );
    match v3_slave {
        V3::Done(_) => {}
        other => panic!("v3 lookup of allocated slave failed: {other:?}"),
    }

    // Missing names → ENOENT, bridged through v3.
    assert_eq!(
        <DevptsInstance as FsOps>::lookup(&devpts, DEVPTS_ROOT_OBJECT_ID, b"99", &guard),
        V3::<FsObjectId, NoProgress>::err(V3Errno::ENOENT)
    );
    // Wrong parent → ENOENT.
    assert_eq!(
        <DevptsInstance as FsOps>::lookup(&devpts, FsObjectId::new(9), b"ptmx", &guard),
        V3::<FsObjectId, NoProgress>::err(V3Errno::ENOENT)
    );
}

#[test]
fn devpts_v3_load_inode_meta_for_root_and_ptmx() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let devpts = DevptsInstance;

    assert_eq!(
        <DevptsInstance as FsOps>::load_inode_meta(&devpts, DEVPTS_ROOT_OBJECT_ID, &guard),
        V3::<_, NoProgress>::done(InodeMeta::new(InodeKind::Directory, 0o040755))
    );
    assert_eq!(
        <DevptsInstance as FsOps>::load_inode_meta(&devpts, DEVPTS_PTMX_OBJECT_ID, &guard),
        V3::<_, NoProgress>::done(InodeMeta::new(InodeKind::CharDevice, 0o020666))
    );
    // Unknown ids → ENOENT.
    assert_eq!(
        <DevptsInstance as FsOps>::load_inode_meta(&devpts, FsObjectId::new(9), &guard),
        V3::<InodeMeta, NoProgress>::err(V3Errno::ENOENT)
    );
}

#[test]
fn devpts_v3_readdir_lists_ptmx_and_returns_none_at_end() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let devpts = DevptsInstance;

    let first = <DevptsInstance as FsOps>::readdir(
        &devpts,
        DEVPTS_ROOT_OBJECT_ID,
        DirCursor::START,
        &guard,
    );
    let (entry, next) = match first {
        V3::Done(Some((entry, next))) => (entry, next),
        other => panic!("readdir v3 first: {other:?}"),
    };
    assert_eq!(entry.name.as_bytes(), b"ptmx");

    // readdir on a non-directory id returns ENOTDIR.
    assert_eq!(
        <DevptsInstance as FsOps>::readdir(
            &devpts,
            DEVPTS_PTMX_OBJECT_ID,
            DirCursor::START,
            &guard,
        ),
        V3::<Option<_>, NoProgress>::err(V3Errno::ENOTDIR)
    );

    // Walk past the registered entries — eventually returns Done(None).
    let mut cursor = next;
    let mut steps = 0;
    loop {
        steps += 1;
        if steps > 1024 {
            panic!("readdir v3 did not terminate");
        }
        match <DevptsInstance as FsOps>::readdir(&devpts, DEVPTS_ROOT_OBJECT_ID, cursor, &guard) {
            V3::Done(Some((_, next))) => cursor = next,
            V3::Done(None) => break,
            other => panic!("readdir v3 walk: {other:?}"),
        }
    }
}

// === FsOps mutations are read-only / EROFS ==========================

#[test]
fn devpts_v3_mutations_are_erofs() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let devpts = DevptsInstance;
    let cred = Credential::default();

    assert_eq!(
        <DevptsInstance as FsOps>::serialize_inode_meta(
            &devpts,
            DEVPTS_ROOT_OBJECT_ID,
            &InodeMeta::new(InodeKind::Directory, 0o040755),
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EROFS)
    );
    assert_eq!(
        <DevptsInstance as FsOps>::create_inode(
            &devpts,
            DEVPTS_ROOT_OBJECT_ID,
            b"x",
            0o020600,
            &cred,
            &guard,
        ),
        V3::<(FsObjectId, InodeMeta), NoProgress>::err(V3Errno::EROFS)
    );
    assert_eq!(
        <DevptsInstance as FsOps>::mkdir(
            &devpts,
            DEVPTS_ROOT_OBJECT_ID,
            b"d",
            0o040755,
            &cred,
            &guard,
        ),
        V3::<(FsObjectId, InodeMeta), NoProgress>::err(V3Errno::EROFS)
    );
    assert_eq!(
        <DevptsInstance as FsOps>::rmdir(
            &devpts,
            DEVPTS_ROOT_OBJECT_ID,
            b"d",
            FsObjectId::new(99),
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EROFS)
    );
    assert_eq!(
        <DevptsInstance as FsOps>::unlink(
            &devpts,
            DEVPTS_ROOT_OBJECT_ID,
            b"0",
            FsObjectId::new(99),
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EROFS)
    );
    assert_eq!(
        <DevptsInstance as FsOps>::rename(
            &devpts,
            DEVPTS_ROOT_OBJECT_ID,
            b"a",
            DEVPTS_ROOT_OBJECT_ID,
            b"b",
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EROFS)
    );
    assert_eq!(
        <DevptsInstance as FsOps>::link(
            &devpts,
            DEVPTS_ROOT_OBJECT_ID,
            b"x",
            FsObjectId::new(99),
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::EROFS)
    );
    assert_eq!(
        <DevptsInstance as FsOps>::symlink(
            &devpts,
            DEVPTS_ROOT_OBJECT_ID,
            b"l",
            b"target",
            &cred,
            &guard,
        ),
        V3::<(FsObjectId, InodeMeta), NoProgress>::err(V3Errno::EROFS)
    );
}

#[test]
fn devpts_v3_destroy_inode_done_for_known_objects() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let devpts = DevptsInstance;

    // Root + ptmx are known and return Done(()).
    assert_eq!(
        <DevptsInstance as FsOps>::destroy_inode(&devpts, DEVPTS_ROOT_OBJECT_ID, &guard),
        V3::<(), NoProgress>::done(())
    );
    assert_eq!(
        <DevptsInstance as FsOps>::destroy_inode(&devpts, DEVPTS_PTMX_OBJECT_ID, &guard),
        V3::<(), NoProgress>::done(())
    );
    // Unknown id → ENOENT.
    assert_eq!(
        <DevptsInstance as FsOps>::destroy_inode(&devpts, FsObjectId::new(9), &guard),
        V3::<(), NoProgress>::err(V3Errno::ENOENT)
    );
}

// === FsPageBacking — devpts has no page cache =======================

#[test]
fn devpts_v3_fs_page_backing_returns_enosys() {
    let _serial = TTY_ZONE_TEST_LOCK.lock().expect("tty zone test lock");
    init_zones();
    let guard = tx_substrate::epoch::guard();
    let devpts = DevptsInstance;

    assert_eq!(
        <DevptsInstance as FsPageBacking>::fetch_page(&devpts, DEVPTS_PTMX_OBJECT_ID, 0, &guard,),
        V3::<Frame, NoProgress>::err(V3Errno::ENOSYS)
    );
    let frame = Frame::new(Ppn(0));
    assert_eq!(
        <DevptsInstance as FsPageBacking>::flush_page(
            &devpts,
            DEVPTS_PTMX_OBJECT_ID,
            0,
            &frame,
            &guard,
        ),
        V3::<(), NoProgress>::err(V3Errno::ENOSYS)
    );
    assert_eq!(
        <DevptsInstance as FsPageBacking>::truncate(&devpts, DEVPTS_PTMX_OBJECT_ID, 0, &guard,),
        V3::<(), NoProgress>::err(V3Errno::ENOSYS)
    );
    assert_eq!(
        <DevptsInstance as FsPageBacking>::fsync(&devpts, DEVPTS_PTMX_OBJECT_ID, &guard),
        V3::<(), NoProgress>::err(V3Errno::ENOSYS)
    );
}
