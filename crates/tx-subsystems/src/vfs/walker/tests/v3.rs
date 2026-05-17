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
use crate::vfs::adapter::step_engine::{guard, Errno, NoProgress, StepOutcome};
use crate::vfs::structure::{Credential, DirCursor, DirEntry, FsObjectId, InodeKind, InodeMeta};

use super::TestFs;

impl crate::vfs::FsOps for TestFs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        let inner = self.inner.lock();
        let Some(map) = inner.children.get(&parent) else {
            return StepOutcome::err(Errno::ENOTDIR);
        };
        match map.get(name) {
            Some(id) => StepOutcome::done(*id),
            None => StepOutcome::err(Errno::ENOENT),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress> {
        let inner = self.inner.lock();
        let Some((kind, _, mode_low, uid, gid)) = inner.inodes.get(&fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT);
        };
        // S_IFMT bits get OR-ed in by InodeMeta::new based on `kind`;
        // the per-inode mode_low covers the rwx triplets + setuid/
        // setgid bits the DAC slice tests exercise.
        let mut meta = InodeMeta::new(*kind, *mode_low);
        meta.uid = *uid;
        meta.gid = *gid;
        StepOutcome::done(meta)
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        StepOutcome::done(None)
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Box<[u8]>, NoProgress> {
        let inner = self.inner.lock();
        match inner.inodes.get(&fs_object_id) {
            Some((InodeKind::Symlink, Some(target), _, _, _)) => {
                StepOutcome::done(target.clone().into_boxed_slice())
            }
            Some(_) => StepOutcome::err(Errno::EINVAL),
            None => StepOutcome::err(Errno::ENOENT),
        }
    }
}

impl crate::page_backed::FsPageBacking for TestFs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Frame, NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn fsync_file(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }
}

// === tests pinning the v3 outcome shape end-to-end through TestFs =====

#[test]
fn testfs_v3_lookup_round_trips_after_add_dir() {
    use crate::vfs::adapter::step_engine::{Errno as V3Errno, StepOutcome as V3};
    use crate::vfs::FsOps;

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let dir_id = testfs.add_dir(FsObjectId::new(2), b"foo");

    let guard = guard();
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
    use crate::vfs::adapter::step_engine::{Errno as V3Errno, StepOutcome as V3};
    use crate::vfs::FsOps;

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let link_id = testfs.add_symlink(FsObjectId::new(2), b"l", b"target");

    let guard = guard();
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
    use StepOutcome as V3;

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let reg_id = testfs.add_regular(FsObjectId::new(2), b"file");

    let guard = guard();
    let meta = match <TestFs as FsOps>::load_inode_meta(&*testfs, reg_id, &guard) {
        V3::Done(meta) => meta,
        other => panic!("load_inode_meta v3: {other:?}"),
    };
    assert_eq!(meta.kind(), InodeKind::Regular);
}

#[test]
fn testfs_v3_fetch_page_default_returns_enosys() {
    use crate::page_backed::FsPageBacking;
    use crate::vfs::adapter::step_engine::{Errno as V3Errno, StepOutcome as V3};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    super::init_zones();

    let testfs = TestFs::new(FsObjectId::new(2));
    let guard = guard();
    assert_eq!(
        <TestFs as FsPageBacking>::fetch_page(&*testfs, FsObjectId::new(2), 0, &guard),
        V3::<Frame, NoProgress>::err(V3Errno::ENOSYS)
    );
    assert_eq!(
        <TestFs as FsPageBacking>::fsync_file(&*testfs, FsObjectId::new(2), &guard),
        V3::<(), NoProgress>::done(())
    );
}
