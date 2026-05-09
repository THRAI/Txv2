//! Wave 9a: `FsOpsV3` + `FsPageBackingV3` impls + tests on `TestFs`.
//!
//! Sibling to the v4 impls in the parent `tests` module. Per
//! `docs/progress/decisions/2026-05-09-fsops-v3-design.md`, every method
//! delegates to the v4 body and translates outcomes one-for-one. `TestFs`
//! is purely synchronous (no `Advanced` / `Blocked` variants in its v4
//! bodies), so the v3 mapping is trivial. Tests at the bottom pin the v3
//! outcome shape end-to-end.
//!
//! Lives in its own file so the parent `tests.rs` stays under the
//! `cargo xtask lint arch` 1500-line authored-file cap (the v3 impl
//! block alone is ~350 lines; with the v4 fixture it pushed `tests.rs`
//! over the cap).

use alloc::boxed::Box;

use crate::execution::{Guard, StepOutcome};
use crate::page_backed::{Frame, FsPageBacking};
use crate::vfs::structure::{Credential, DirCursor, DirEntry, FsObjectId, InodeMeta};
use crate::vfs::FsOps;

use super::TestFs;

impl crate::vfs::FsOpsV3 for TestFs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<FsObjectId, tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::lookup(self, parent, name, guard) {
            StepOutcome::Done(id) | StepOutcome::Advanced(id) => {
                tx_substrate::step_v3::StepOutcome::done(id)
            }
            StepOutcome::AdvancedThenBlocked(id, _) => tx_substrate::step_v3::StepOutcome::done(id),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<InodeMeta, tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::load_inode_meta(self, fs_object_id, guard) {
            StepOutcome::Done(m) | StepOutcome::Advanced(m) => {
                tx_substrate::step_v3::StepOutcome::done(m)
            }
            StepOutcome::AdvancedThenBlocked(m, _) => tx_substrate::step_v3::StepOutcome::done(m),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::serialize_inode_meta(self, fs_object_id, meta, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn create_inode(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        match <Self as FsOps>::create_inode(self, parent, name, mode, cred, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::unlink(self, parent, name, target, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn rename(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::rename(self, old_parent, old_name, new_parent, new_name, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn link(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::link(self, parent, name, target, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        match <Self as FsOps>::mkdir(self, parent, name, mode, cred, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::rmdir(self, parent, name, target, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn symlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        match <Self as FsOps>::symlink(self, parent, name, link_target, cred, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        Option<(DirEntry, DirCursor)>,
        tx_substrate::step_v3::NoProgress,
    > {
        match <Self as FsOps>::readdir(self, fs_object_id, cursor, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn destroy_inode(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::destroy_inode(self, fs_object_id, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<Box<[u8]>, tx_substrate::step_v3::NoProgress> {
        match <Self as FsOps>::read_link(self, fs_object_id, guard) {
            StepOutcome::Done(out) | StepOutcome::Advanced(out) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::AdvancedThenBlocked(out, _) => {
                tx_substrate::step_v3::StepOutcome::done(out)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }
}

impl crate::page_backed::FsPageBackingV3 for TestFs {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<Frame, tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::fetch_page(self, fs_object_id, offset, guard) {
            StepOutcome::Done(frame) | StepOutcome::Advanced(frame) => {
                tx_substrate::step_v3::StepOutcome::done(frame)
            }
            StepOutcome::AdvancedThenBlocked(frame, _) => {
                tx_substrate::step_v3::StepOutcome::done(frame)
            }
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        frame: &Frame,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::flush_page(self, fs_object_id, offset, frame, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::truncate(self, fs_object_id, new_size, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }

    fn fsync(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        match <Self as FsPageBacking>::fsync(self, fs_object_id, guard) {
            StepOutcome::Done(()) | StepOutcome::Advanced(()) => {
                tx_substrate::step_v3::StepOutcome::done(())
            }
            StepOutcome::AdvancedThenBlocked((), _) => tx_substrate::step_v3::StepOutcome::done(()),
            StepOutcome::Blocked(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EAGAIN)
            }
            StepOutcome::Err(e) => tx_substrate::step_v3::StepOutcome::err(e.into()),
        }
    }
}

// === tests pinning the v3 outcome shape end-to-end through TestFs =====

#[test]
fn testfs_v3_lookup_round_trips_after_add_dir() {
    use crate::vfs::FsOpsV3;
    use tx_substrate::step_v3::{Errno as V3Errno, NoProgress, StepOutcome as V3};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let dir_id = testfs.add_dir(FsObjectId::new(2), b"foo");

    let guard = tx_substrate::epoch::guard();
    assert_eq!(
        <TestFs as FsOpsV3>::lookup(&*testfs, FsObjectId::new(2), b"foo", &guard),
        V3::<_, NoProgress>::done(dir_id)
    );
    assert_eq!(
        <TestFs as FsOpsV3>::lookup(&*testfs, FsObjectId::new(2), b"missing", &guard),
        V3::<FsObjectId, NoProgress>::err(V3Errno::ENOENT)
    );
}

#[test]
fn testfs_v3_read_link_returns_target_bytes() {
    use crate::vfs::FsOpsV3;
    use tx_substrate::step_v3::{Errno as V3Errno, NoProgress, StepOutcome as V3};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let link_id = testfs.add_symlink(FsObjectId::new(2), b"l", b"target");

    let guard = tx_substrate::epoch::guard();
    let outcome = <TestFs as FsOpsV3>::read_link(&*testfs, link_id, &guard);
    match outcome {
        V3::Done(bytes) => assert_eq!(&*bytes, b"target".as_slice()),
        other => panic!("read_link v3: {other:?}"),
    }

    let dir_id = testfs.add_dir(FsObjectId::new(2), b"d");
    assert_eq!(
        <TestFs as FsOpsV3>::read_link(&*testfs, dir_id, &guard),
        V3::<Box<[u8]>, NoProgress>::err(V3Errno::EINVAL)
    );
}

#[test]
fn testfs_v3_load_inode_meta_returns_kind_and_mode() {
    use crate::vfs::FsOpsV3;
    use crate::vfs::structure::InodeKind;
    use tx_substrate::step_v3::StepOutcome as V3;

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let reg_id = testfs.add_regular(FsObjectId::new(2), b"file");

    let guard = tx_substrate::epoch::guard();
    let meta = match <TestFs as FsOpsV3>::load_inode_meta(&*testfs, reg_id, &guard) {
        V3::Done(meta) => meta,
        other => panic!("load_inode_meta v3: {other:?}"),
    };
    assert_eq!(meta.kind(), InodeKind::Regular);
}

#[test]
fn testfs_v3_fetch_page_default_returns_enosys() {
    use crate::page_backed::FsPageBackingV3;
    use tx_substrate::step_v3::{Errno as V3Errno, NoProgress, StepOutcome as V3};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let guard = tx_substrate::epoch::guard();
    assert_eq!(
        <TestFs as FsPageBackingV3>::fetch_page(&*testfs, FsObjectId::new(2), 0, &guard),
        V3::<Frame, NoProgress>::err(V3Errno::ENOSYS)
    );
    assert_eq!(
        <TestFs as FsPageBackingV3>::fsync(&*testfs, FsObjectId::new(2), &guard),
        V3::<(), NoProgress>::done(())
    );
}
