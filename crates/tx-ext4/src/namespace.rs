use tx_ext4_format::pager::{BlockImage, DirEntryLite};
use tx_substrate::epoch::Guard;
use tx_subsystems::step::{Errno, StepOutcome};
use tx_subsystems::vfs::fs_ops::{Credential, DirCursor, DirEntry, FsOps};
use tx_subsystems::vfs::structure::{FsObjectId, InodeMeta, NameOwned};

use crate::read_backend::{
    cursor_from_index, cursor_index, fs_object_id as inode_fs_object_id, inode_no, map_inode_meta,
    Ext4FsInstance, READDIR_WINDOW_ENTRIES,
};

impl<I> FsOps for Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    fn lookup<'g>(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<FsObjectId> {
        let parent = match inode_no(parent) {
            Ok(parent) => parent,
            Err(err) => return StepOutcome::Err(err),
        };

        match self.with_pager(|pager| pager.lookup(parent, name)) {
            Ok(Some(inode)) => StepOutcome::Done(inode_fs_object_id(inode)),
            Ok(None) => StepOutcome::Err(Errno::ENOENT),
            Err(err) => StepOutcome::Err(err),
        }
    }

    fn load_inode_meta<'g>(
        &self,
        fs_object_id: FsObjectId,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<InodeMeta> {
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::Err(err),
        };

        match self.with_pager(|pager| pager.inode_meta(inode)) {
            Ok(meta) => StepOutcome::Done(map_inode_meta(meta)),
            Err(err) => StepOutcome::Err(err),
        }
    }

    fn serialize_inode_meta<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn create_inode<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn unlink<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn rename<'g>(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn link<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn mkdir<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn rmdir<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn symlink<'g>(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn readdir<'g>(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::Err(err),
        };
        let index = match cursor_index(cursor) {
            Ok(index) => index,
            Err(err) => return StepOutcome::Err(err),
        };
        if index >= READDIR_WINDOW_ENTRIES {
            return StepOutcome::Err(Errno::ENOSYS);
        }

        let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
        let count = match self.with_pager(|pager| pager.read_dir_entries(inode, &mut entries)) {
            Ok(count) => count,
            Err(err) => return StepOutcome::Err(err),
        };
        if index >= count {
            return StepOutcome::Done(None);
        }

        let entry = entries[index];
        let name = match NameOwned::from_component(entry.name()) {
            Ok(name) => name,
            Err(err) => return StepOutcome::Err(err),
        };
        StepOutcome::Done(Some((
            DirEntry {
                name,
                fs_object_id: inode_fs_object_id(entry.inode),
                d_type: entry.file_type,
            },
            cursor_from_index(index + 1),
        )))
    }

    fn destroy_inode<'g>(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &'g Guard<'g>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }
}
