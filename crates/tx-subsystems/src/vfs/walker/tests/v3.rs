//! `FsOps` + `FsPageBacking` impls + tests on `TestFs`.
//!
//! `TestFs` is purely synchronous: every method either succeeds with
//! `Done` or returns an `Err`. Tests at the bottom pin the outcome
//! shape end-to-end.
//!
//! Lives in its own file so the parent `tests.rs` stays under the
//! `cargo xtask lint arch` 1500-line authored-file cap.

use alloc::boxed::Box;

use crate::execution::Guard;
use crate::page_backed::Frame;
use crate::vfs::structure::{Credential, DirCursor, DirEntry, FsObjectId, InodeKind, InodeMeta};

use super::TestFs;

impl crate::vfs::FsOps for TestFs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<FsObjectId, tx_substrate::step_v3::NoProgress> {
        let inner = self.inner.lock();
        let Some(map) = inner.children.get(&parent) else {
            return tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOTDIR);
        };
        match map.get(name) {
            Some(id) => tx_substrate::step_v3::StepOutcome::done(*id),
            None => tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOENT),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<InodeMeta, tx_substrate::step_v3::NoProgress> {
        let inner = self.inner.lock();
        let Some((kind, _, mode_low, uid, gid)) = inner.inodes.get(&fs_object_id) else {
            return tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOENT);
        };
        // S_IFMT bits get OR-ed in by InodeMeta::new based on `kind`;
        // the per-inode mode_low covers the rwx triplets + setuid/
        // setgid bits the DAC slice tests exercise.
        let mut meta = InodeMeta::new(*kind, *mode_low);
        meta.uid = *uid;
        meta.gid = *gid;
        tx_substrate::step_v3::StepOutcome::done(meta)
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        (FsObjectId, InodeMeta),
        tx_substrate::step_v3::NoProgress,
    > {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        Option<(DirEntry, DirCursor)>,
        tx_substrate::step_v3::NoProgress,
    > {
        tx_substrate::step_v3::StepOutcome::done(None)
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::done(())
    }

    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<Box<[u8]>, tx_substrate::step_v3::NoProgress> {
        let inner = self.inner.lock();
        match inner.inodes.get(&fs_object_id) {
            Some((InodeKind::Symlink, Some(target), _, _, _)) => {
                tx_substrate::step_v3::StepOutcome::done(target.clone().into_boxed_slice())
            }
            Some(_) => {
                tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::EINVAL)
            }
            None => tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOENT),
        }
    }
}

impl crate::page_backed::FsPageBacking for TestFs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<Frame, tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(tx_substrate::step_v3::Errno::ENOSYS)
    }

    fn fsync(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::done(())
    }
}

// === tests pinning the v3 outcome shape end-to-end through TestFs =====

#[test]
fn testfs_v3_lookup_round_trips_after_add_dir() {
    use crate::vfs::FsOps;
    use tx_substrate::step_v3::{Errno as V3Errno, NoProgress, StepOutcome as V3};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let dir_id = testfs.add_dir(FsObjectId::new(2), b"foo");

    let guard = tx_substrate::epoch::guard();
    assert_eq!(
        <TestFs as FsOps>::lookup(&*testfs, FsObjectId::new(2), b"foo", &guard),
        V3::<_, NoProgress>::done(dir_id)
    );
    assert_eq!(
        <TestFs as FsOps>::lookup(&*testfs, FsObjectId::new(2), b"missing", &guard),
        V3::<FsObjectId, NoProgress>::err(V3Errno::ENOENT)
    );
}

#[test]
fn testfs_v3_read_link_returns_target_bytes() {
    use crate::vfs::FsOps;
    use tx_substrate::step_v3::{Errno as V3Errno, NoProgress, StepOutcome as V3};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let link_id = testfs.add_symlink(FsObjectId::new(2), b"l", b"target");

    let guard = tx_substrate::epoch::guard();
    let outcome = <TestFs as FsOps>::read_link(&*testfs, link_id, &guard);
    match outcome {
        V3::Done(bytes) => assert_eq!(&*bytes, b"target".as_slice()),
        other => panic!("read_link v3: {other:?}"),
    }

    let dir_id = testfs.add_dir(FsObjectId::new(2), b"d");
    assert_eq!(
        <TestFs as FsOps>::read_link(&*testfs, dir_id, &guard),
        V3::<Box<[u8]>, NoProgress>::err(V3Errno::EINVAL)
    );
}

#[test]
fn testfs_v3_load_inode_meta_returns_kind_and_mode() {
    use crate::vfs::FsOps;
    use tx_substrate::step_v3::StepOutcome as V3;

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let reg_id = testfs.add_regular(FsObjectId::new(2), b"file");

    let guard = tx_substrate::epoch::guard();
    let meta = match <TestFs as FsOps>::load_inode_meta(&*testfs, reg_id, &guard) {
        V3::Done(meta) => meta,
        other => panic!("load_inode_meta v3: {other:?}"),
    };
    assert_eq!(meta.kind(), InodeKind::Regular);
}

#[test]
fn testfs_v3_fetch_page_default_returns_enosys() {
    use crate::page_backed::FsPageBacking;
    use tx_substrate::step_v3::{Errno as V3Errno, NoProgress, StepOutcome as V3};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let guard = tx_substrate::epoch::guard();
    assert_eq!(
        <TestFs as FsPageBacking>::fetch_page(&*testfs, FsObjectId::new(2), 0, &guard),
        V3::<Frame, NoProgress>::err(V3Errno::ENOSYS)
    );
    assert_eq!(
        <TestFs as FsPageBacking>::fsync(&*testfs, FsObjectId::new(2), &guard),
        V3::<(), NoProgress>::done(())
    );
}
