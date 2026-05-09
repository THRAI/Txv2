use tx_ext4_format::pager::{BlockImage, DirEntryLite};
use tx_substrate::epoch::Guard;
use tx_subsystems::execution::{Errno, StepOutcome};
use tx_subsystems::vfs::execution::FsOps;
use tx_subsystems::vfs::structure::{
    Credential, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta,
};

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
        let name = match InlineName::new(entry.name()) {
            Ok(name) => name,
            Err(err) => return StepOutcome::Err(err),
        };
        StepOutcome::Done(Some((
            DirEntry {
                name,
                fs_object_id: inode_fs_object_id(entry.inode),
                kind: ext4_file_type_to_kind(entry.file_type),
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

// === Wave 9b: parallel v3 trait impl =================================
//
// `impl FsOpsV3 for Ext4FsInstance<I>` mirrors the v4 body above
// one-for-one. ext4 is the wave-9b backend most likely to surface
// real `Advanced(t)` outcomes because the v4 surface dispatches to
// on-disk block I/O via `with_pager(...).read_*` — but the current
// read-only ext4 surface goes through `Ext4Pager::*` which returns
// `Result<T, Ext4FormatError>`, **not** `StepOutcome`, so the v4
// `FsOps` impls land on `StepOutcome::Done(t)` / `StepOutcome::Err(e)`
// only. There are no `Advanced(t)` / `Blocked` / `AdvancedThenBlocked`
// returns from the v4 bodies today; the v3 mapping has zero ambiguous
// Continue-vs-Done call sites in this wave.
//
// Per the wave-9a design doc
// (`docs/progress/decisions/2026-05-09-fsops-v3-design.md`) and the
// trait-surface contract: where the v4 fn does (in a future async/
// journal-aware revision) return `Advanced(t)`, the trait surface
// translates `Advanced(t)` → `done(t)` (one-shot v3 contract); any
// partial-progress accounting lives at the *caller* (wave 9c walker).
// `Blocked` / `AdvancedThenBlocked` map defensively to `EAGAIN` since
// the v3 trait returns `NoProgress` and the trait surface has no
// carrier-progress yield shape that fits a one-shot identity-side
// query.
//
// Fully-qualified `tx_substrate::step_v3::*` references at the impl
// sites avoid clashing with `tx_subsystems::execution::StepOutcome`
// already in scope, per the wave-4/6/7 trait-impl convention.

use tx_subsystems::vfs::FsOpsV3;

/// v3 sibling factory for `MountOutput::fs_ops_v3` cutover.
///
/// Wave 9c walker entry points populate `MountOutput::fs_ops_v3`
/// from this constructor; mirrors `Tmpfs::fs_ops_v3_arc`. The cfg gate
/// from wave 9b is lifted in wave 9c now that `MountOutput` carries
/// the field and `mount_ext4_read_only` populates it.
impl<I> Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    pub(crate) fn fs_ops_v3_arc(
        self: alloc::sync::Arc<Self>,
    ) -> alloc::sync::Arc<dyn FsOpsV3> {
        self
    }
}

impl<I> FsOpsV3 for Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
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
            StepOutcome::Done(meta) | StepOutcome::Advanced(meta) => {
                tx_substrate::step_v3::StepOutcome::done(meta)
            }
            StepOutcome::AdvancedThenBlocked(meta, _) => {
                tx_substrate::step_v3::StepOutcome::done(meta)
            }
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

    // `read_link`, `materialise_rnode`, `step_chmod`, `step_chown` all
    // inherit the trait-default `ENOSYS` mapping, matching the v4
    // behaviour where ext4 does not override those methods either.
}
