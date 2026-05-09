use tx_ext4_format::pager::{BlockImage, DirEntryLite};
use tx_substrate::epoch::Guard;
use tx_subsystems::execution::Errno;
use tx_subsystems::vfs::structure::{
    Credential, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta,
};

use crate::read_backend::{
    cursor_from_index, cursor_index, fs_object_id as inode_fs_object_id, inode_no, map_inode_meta,
    Ext4FsInstance, READDIR_WINDOW_ENTRIES,
};

// ext4 dir-entry file_type codes (POSIX-shaped). Maps the on-disk byte
// code into the canonical `InodeKind` enum surfaced by `tx-subsystems`.
fn ext4_file_type_to_kind(file_type: u8) -> InodeKind {
    match file_type {
        2 => InodeKind::Directory,
        3 => InodeKind::CharDevice,
        4 => InodeKind::BlockDevice,
        5 => InodeKind::Fifo,
        6 => InodeKind::Socket,
        7 => InodeKind::Symlink,
        _ => InodeKind::Regular,
    }
}

// === FsOps impl =====================================================
//
// The current read-only ext4 surface routes through `Ext4Pager::*`
// which returns `Result<T, Ext4FormatError>` (not `StepOutcome`), so
// every body here lands on `done` or `err` only — there is no
// `Advanced` / `Blocked` / `AdvancedThenBlocked` path through this
// read-only backend today.
//
// Fully-qualified `tx_substrate::step_v3::*` references at the impl
// sites avoid clashing with `tx_subsystems::execution::Errno`
// already in scope.

use tx_subsystems::vfs::FsOps;

/// Factory for `MountOutput::fs_ops`.
///
/// Mirrors `Tmpfs::fs_ops_arc`.
impl<I> Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    pub(crate) fn fs_ops_arc(
        self: alloc::sync::Arc<Self>,
    ) -> alloc::sync::Arc<dyn FsOps> {
        self
    }
}

impl<I> FsOps for Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<FsObjectId, tx_substrate::step_v3::NoProgress> {
        let parent = match inode_no(parent) {
            Ok(parent) => parent,
            Err(err) => return tx_substrate::step_v3::StepOutcome::err(err.into()),
        };

        match self.with_pager(|pager| pager.lookup(parent, name)) {
            Ok(Some(inode)) => {
                tx_substrate::step_v3::StepOutcome::done(inode_fs_object_id(inode))
            }
            Ok(None) => tx_substrate::step_v3::StepOutcome::err(Errno::ENOENT.into()),
            Err(err) => tx_substrate::step_v3::StepOutcome::err(err.into()),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<InodeMeta, tx_substrate::step_v3::NoProgress> {
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return tx_substrate::step_v3::StepOutcome::err(err.into()),
        };

        match self.with_pager(|pager| pager.inode_meta(inode)) {
            Ok(meta) => tx_substrate::step_v3::StepOutcome::done(map_inode_meta(meta)),
            Err(err) => tx_substrate::step_v3::StepOutcome::err(err.into()),
        }
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
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
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
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
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
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
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<
        Option<(DirEntry, DirCursor)>,
        tx_substrate::step_v3::NoProgress,
    > {
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return tx_substrate::step_v3::StepOutcome::err(err.into()),
        };
        let index = match cursor_index(cursor) {
            Ok(index) => index,
            Err(err) => return tx_substrate::step_v3::StepOutcome::err(err.into()),
        };
        if index >= READDIR_WINDOW_ENTRIES {
            return tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into());
        }

        let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
        let count = match self.with_pager(|pager| pager.read_dir_entries(inode, &mut entries)) {
            Ok(count) => count,
            Err(err) => return tx_substrate::step_v3::StepOutcome::err(err.into()),
        };
        if index >= count {
            return tx_substrate::step_v3::StepOutcome::done(None);
        }

        let entry = entries[index];
        let name = match InlineName::new(entry.name()) {
            Ok(name) => name,
            Err(err) => return tx_substrate::step_v3::StepOutcome::err(err.into()),
        };
        tx_substrate::step_v3::StepOutcome::done(Some((
            DirEntry {
                name,
                fs_object_id: inode_fs_object_id(entry.inode),
                kind: ext4_file_type_to_kind(entry.file_type),
            },
            cursor_from_index(index + 1),
        )))
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
        tx_substrate::step_v3::StepOutcome::err(Errno::ENOSYS.into())
    }

    // `read_link`, `materialise_rnode`, `step_chmod`, `step_chown` all
    // inherit the trait-default `ENOSYS` mapping; ext4 does not
    // override those methods.
}
