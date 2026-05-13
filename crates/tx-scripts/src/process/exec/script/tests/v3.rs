//! `FsOps` + `FsPageBacking` impls on `ExecTestFs`.
//!
//! `ExecTestFs` is purely synchronous (no `Continue` / `Yield`
//! variants in its bodies, with the lone exception of `fetch_page`'s
//! pass-through of `PageContainer::materialize_page`).
//!
//! Lives in its own file so the parent `tests.rs` stays under the
//! `cargo xtask lint arch` 1500-line authored-file cap.

use alloc::boxed::Box;

use crate::adapter::step_engine::{self as step_engine, guard, Cap, Errno as V3Errno, NoProgress, StepOutcome as V3Outcome};
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::page_backed::{Frame, MaterializeAccess, PageIndex};
use tx_subsystems::vfs::structure::{
    Credential, DirCursor, DirEntry, FsObjectId, InodeKind, InodeMeta, RNode, RNodeBacking,
    S_IFDIR, S_IFREG,
};
use tx_subsystems::vm::USER_PAGE_SIZE;

use super::{ExecTestFs, ExecTestInode};

impl tx_subsystems::vfs::FsOps for ExecTestFs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Outcome<FsObjectId, NoProgress> {
        let inner = self.inner.lock();
        let Some(map) = inner.children.get(&parent) else {
            return V3Outcome::err(Errno::ENOTDIR.into());
        };
        match map.get(name) {
            Some(id) => V3Outcome::done(*id),
            None => V3Outcome::err(Errno::ENOENT.into()),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<InodeMeta, NoProgress> {
        let inner = self.inner.lock();
        let Some(inode) = inner.inodes.get(&fs_object_id) else {
            return V3Outcome::err(Errno::ENOENT.into());
        };
        let meta = match inode {
            ExecTestInode::Directory => InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            ExecTestInode::Regular {
                size,
                mode_bits,
                uid,
                gid,
                ..
            } => {
                let mut meta = InodeMeta::new(InodeKind::Regular, S_IFREG | *mode_bits);
                meta.size = *size;
                meta.uid = *uid;
                meta.gid = *gid;
                meta
            }
        };
        V3Outcome::done(meta)
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
        V3Outcome::err(Errno::ENOSYS.into())
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::err(Errno::ENOSYS.into())
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::err(Errno::ENOSYS.into())
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::err(Errno::ENOSYS.into())
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
        V3Outcome::err(Errno::ENOSYS.into())
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::err(Errno::ENOSYS.into())
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
        V3Outcome::err(Errno::ENOSYS.into())
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> V3Outcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        V3Outcome::done(None)
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::done(())
    }

    fn read_link(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<Box<[u8]>, NoProgress> {
        V3Outcome::err(Errno::EINVAL.into())
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        _guard: &Guard<'_>,
    ) -> V3Outcome<Cap<RNode>, NoProgress> {
        let inner = self.inner.lock();
        let Some(inode) = inner.inodes.get(&fs_object_id) else {
            return V3Outcome::err(Errno::ENOENT.into());
        };
        match inode {
            ExecTestInode::Regular { container, .. } => {
                match RNode::new_cap(
                    fs_object_id,
                    meta,
                    RNodeBacking::PageBacked {
                        pc: container.clone(),
                    },
                ) {
                    Ok(rnode) => V3Outcome::done(rnode),
                    Err(_) => V3Outcome::err(Errno::ENOMEM.into()),
                }
            }
            ExecTestInode::Directory => V3Outcome::err(Errno::EISDIR.into()),
        }
    }
}

impl tx_subsystems::page_backed::FsPageBacking for ExecTestFs {
    fn fetch_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        guard: &Guard<'_>,
    ) -> V3Outcome<Frame, NoProgress> {
        let inner = self.inner.lock();
        let container = match inner.inodes.get(&fs_object_id) {
            Some(ExecTestInode::Regular { container, .. }) => container.clone(),
            Some(ExecTestInode::Directory) => return V3Outcome::err(Errno::EISDIR.into()),
            None => return V3Outcome::err(Errno::ENOENT.into()),
        };
        drop(inner);

        let page_size = USER_PAGE_SIZE as u64;
        if !offset.is_multiple_of(page_size) {
            return V3Outcome::err(Errno::EINVAL.into());
        }
        let page_index = PageIndex::new(offset / page_size);
        match container.materialize_page(page_index, MaterializeAccess::Read, guard) {
            V3Outcome::Done(materialised) => V3Outcome::done(Frame::new(materialised.ppn)),
            V3Outcome::Continue { .. } => V3Outcome::err(V3Errno::EAGAIN),
            V3Outcome::Yield { .. } => V3Outcome::err(V3Errno::EAGAIN),
            V3Outcome::Err(errno) => V3Outcome::err(errno),
        }
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::done(())
    }

    fn fsync(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> V3Outcome<(), NoProgress> {
        V3Outcome::done(())
    }
}

// === tests pinning the v3 outcome shape end-to-end through ExecTestFs ===

#[test]
fn exec_testfs_v3_lookup_round_trips_after_add_regular() {
    use step_engine::{Errno as V3Errno, NoProgress, StepOutcome as V3};
    use tx_subsystems::vfs::FsOps;

    let _setup = super::setup();
    let (_root_dentry, fs) = super::build_fs_root();
    let bytes = super::minimal_elf_bytes();
    let file_id = fs.add_regular_with_bytes(FsObjectId::new(2), b"init", &bytes);

    let guard = guard();
    assert_eq!(
        <ExecTestFs as FsOps>::lookup(&*fs, FsObjectId::new(2), b"init", &guard),
        V3::<_, NoProgress>::done(file_id)
    );
    assert_eq!(
        <ExecTestFs as FsOps>::lookup(&*fs, FsObjectId::new(2), b"missing", &guard),
        V3::<FsObjectId, NoProgress>::err(V3Errno::ENOENT)
    );
}

#[test]
fn exec_testfs_v3_load_inode_meta_returns_regular() {
    use step_engine::StepOutcome as V3;
    use tx_subsystems::vfs::structure::InodeKind;
    use tx_subsystems::vfs::FsOps;

    let _setup = super::setup();
    let (_root_dentry, fs) = super::build_fs_root();
    let bytes = super::minimal_elf_bytes();
    let file_id = fs.add_regular_with_bytes(FsObjectId::new(2), b"init", &bytes);

    let guard = guard();
    let meta = match <ExecTestFs as FsOps>::load_inode_meta(&*fs, file_id, &guard) {
        V3::Done(meta) => meta,
        other => panic!("load_inode_meta v3: {other:?}"),
    };
    assert_eq!(meta.kind(), InodeKind::Regular);
}

#[test]
fn exec_testfs_v3_create_inode_returns_enosys() {
    use step_engine::{Errno as V3Errno, NoProgress, StepOutcome as V3};
    use tx_subsystems::vfs::FsOps;

    let _setup = super::setup();
    let (_root_dentry, fs) = super::build_fs_root();

    let guard = guard();
    let cred = Credential::root();
    assert_eq!(
        <ExecTestFs as FsOps>::create_inode(&*fs, FsObjectId::new(2), b"new", 0o644, &cred, &guard),
        V3::<(FsObjectId, InodeMeta), NoProgress>::err(V3Errno::ENOSYS)
    );
}

#[test]
fn exec_testfs_v3_fetch_page_zero_offset_returns_frame() {
    use step_engine::StepOutcome as V3;
    use tx_subsystems::page_backed::FsPageBacking;

    let _setup = super::setup();
    let (_root_dentry, fs) = super::build_fs_root();
    let bytes = super::minimal_elf_bytes();
    let file_id = fs.add_regular_with_bytes(FsObjectId::new(2), b"init", &bytes);

    let guard = guard();
    match <ExecTestFs as FsPageBacking>::fetch_page(&*fs, file_id, 0, &guard) {
        V3::Done(_frame) => {}
        other => panic!("fetch_page v3: {other:?}"),
    }
}

#[test]
fn exec_testfs_v3_fsync_returns_done() {
    use step_engine::{NoProgress, StepOutcome as V3};
    use tx_subsystems::page_backed::FsPageBacking;

    let _setup = super::setup();
    let (_root_dentry, fs) = super::build_fs_root();

    let guard = guard();
    assert_eq!(
        <ExecTestFs as FsPageBacking>::fsync(&*fs, FsObjectId::new(2), &guard),
        V3::<(), NoProgress>::done(())
    );
}
