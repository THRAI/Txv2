use crate::adapter::step_engine::{self as step_engine, Cap, NoProgress, StepOutcome};
use step_engine::Guard;
use tx_ext4_format::mutation::{FsyncStamp, SetAttr};
use tx_ext4_format::ondisk::Inode;
use tx_ext4_format::pager::{BlockImage, DirEntryLite};
use tx_subsystems::execution::Errno;
use tx_subsystems::mount::{MountPayload, MountTransactionFrontier};
use tx_subsystems::page_backed::FileFsyncFrontier;
use tx_subsystems::vfs::structure::{
    Credential, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta, RNode,
    RNodeBacking, Timespec,
};

use crate::read_backend::{
    cursor_from_offset, cursor_offset, inode_no, map_inode_meta, Ext4FsInstance,
    READDIR_WINDOW_ENTRIES,
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

impl<I: BlockImage> Ext4FsInstance<I> {
    fn plan_serialized_meta_update(
        &self,
        inode: tx_ext4_format::pager::InodeNo,
        meta: &InodeMeta,
    ) -> Result<Option<SetAttr>, Errno> {
        let current = self.inode_meta_cached(inode)?;
        if meta.size != current.size
            || meta.nlinks != current.nlinks
            || meta.blocks != current.blocks_512
            || meta.flags != current.flags
        {
            return Err(Errno::EOPNOTSUPP);
        }

        let mode_changed = meta.mode != current.mode;
        let owner_changed = meta.uid != current.uid || meta.gid != current.gid;
        let times_changed = timespec_changed(meta.atime, current.atime)
            || timespec_changed(meta.mtime, current.mtime)
            || timespec_changed(meta.ctime, current.ctime);
        match (
            mode_changed.then_some(SetAttr::Mode(meta.mode)),
            owner_changed.then_some(SetAttr::Owner {
                uid: (meta.uid != current.uid).then_some(meta.uid),
                gid: (meta.gid != current.gid).then_some(meta.gid),
            }),
            if times_changed {
                Some(SetAttr::Times {
                    atime_ns: timespec_changed(meta.atime, current.atime)
                        .then(|| timespec_to_ns(meta.atime))
                        .transpose()?,
                    mtime_ns: timespec_changed(meta.mtime, current.mtime)
                        .then(|| timespec_to_ns(meta.mtime))
                        .transpose()?,
                    ctime_ns: timespec_to_ns(meta.ctime)?,
                })
            } else {
                None
            },
        ) {
            (None, None, None) => Ok(None),
            (Some(update), None, None)
            | (None, Some(update), None)
            | (None, None, Some(update)) => Ok(Some(update)),
            _ => Err(Errno::EOPNOTSUPP),
        }
    }
}

fn timespec_changed(desired: Timespec, current_sec: u32) -> bool {
    desired.sec != current_sec as i64 || desired.nsec != 0
}

fn timespec_to_ns(ts: Timespec) -> Result<u64, Errno> {
    if ts.sec < 0 || !(0..1_000_000_000).contains(&ts.nsec) {
        return Err(Errno::EINVAL);
    }
    (ts.sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|sec| sec.checked_add(ts.nsec as u64))
        .ok_or(Errno::EINVAL)
}

fn fsync_stamp_from_meta(meta: &InodeMeta) -> Result<FsyncStamp, Errno> {
    timespec_to_ns(meta.ctime).map(FsyncStamp::new)
}

// === FsOps impl =====================================================
//
// The current read-only ext4 surface routes through `Ext4Pager::*`
// which returns `Result<T, Ext4FormatError>` (not `StepOutcome`), so
// every body here lands on `done` or `err` only — there is no
// `Advanced` / `Blocked` / `AdvancedThenBlocked` path through this
// read-only backend today.
//
// Fully-qualified `adapter::step_engine::*` references at the impl
// sites avoid clashing with `tx_subsystems::execution::Errno`
// already in scope.

use tx_subsystems::vfs::FsOps;

/// Static writable capacity for ext4 regular-file PageContainers.
///
/// Initial sparse-cache window for ext4 regular-file PageContainers.
///
/// This is not a file-size limit: file-backed PageContainers grow through the
/// ordinary buffered-I/O path.  Keep main's window so direct-I/O admission and
/// whole-file service ranges do not truncate normal compiler/linker outputs.
const EXT4_FILE_INITIAL_PAGE_WINDOW: u64 = 65536;

/// Factory for `MountOutput::fs_ops`.
///
/// Mirrors `Tmpfs::fs_ops_arc`.
impl<I> Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    pub(crate) fn fs_ops_arc(self: alloc::sync::Arc<Self>) -> alloc::sync::Arc<dyn FsOps> {
        self
    }
}

impl<I> FsOps for Ext4FsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    fn lookup_cache_version(&self, parent: FsObjectId) -> Option<u64> {
        Some(self.dir_version(parent))
    }

    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        // Keep the path-resolution probe in debug builds without bouncing one
        // global cache line between harts on every successful release lookup.
        #[cfg(debug_assertions)]
        tx_subsystems::vfs::resolution::diagnostic::record_diag(20);
        match self.lookup_object_cached(parent, name) {
            Ok(Some(object)) => StepOutcome::done(object),
            Ok(None) => {
                #[cfg(debug_assertions)]
                tx_subsystems::vfs::resolution::diagnostic::record_diag(22); // ENOENT
                StepOutcome::err(Errno::ENOENT.into())
            }
            Err(err) => {
                #[cfg(debug_assertions)]
                tx_subsystems::vfs::resolution::diagnostic::record_diag(23); // format err
                StepOutcome::err(err.into())
            }
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress> {
        let meta = match self.resolve_object(fs_object_id) {
            Ok((_, meta)) => meta,
            Err(err) => return StepOutcome::err(err.into()),
        };
        StepOutcome::done(map_inode_meta(meta))
    }

    fn cached_inode_meta(&self, fs_object_id: FsObjectId) -> Option<InodeMeta> {
        let inode = inode_no(fs_object_id).ok()?;
        let meta = self.inode_meta_cached_only(inode)?;
        if meta.mode == 0 || meta.generation != fs_object_id.inode_generation() {
            return None;
        }
        Some(map_inode_meta(meta))
    }

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let inode = match self.resolve_object(fs_object_id) {
            Ok((inode, _)) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let update = match self.plan_serialized_meta_update(inode, meta) {
            Ok(Some(update)) => update,
            Ok(None) => return StepOutcome::done(()),
            Err(err) => return StepOutcome::err(err.into()),
        };
        let stamp = match fsync_stamp_from_meta(meta) {
            Ok(stamp) => stamp,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let mutation = match self.with_pager(|pager| pager.plan_setattr(inode, update, stamp)) {
            Ok(mutation) => mutation,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_inode_meta_for(fs_object_id);
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn chmod_inode(
        &self,
        fs_object_id: FsObjectId,
        new_mode: u16,
        _cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let (inode, current) = match self.resolve_object(fs_object_id) {
            Ok(resolved) => resolved,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let stamp = FsyncStamp::new(current.ctime as u64);
        let mutation =
            match self.with_pager(|pager| pager.plan_setattr_mode(inode, new_mode, stamp)) {
                Ok(mutation) => mutation,
                Err(err) => return StepOutcome::err(err.into()),
            };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_inode_meta_for(fs_object_id);
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn chown_inode(
        &self,
        fs_object_id: FsObjectId,
        new_uid: Option<u32>,
        new_gid: Option<u32>,
        _cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let (inode, current) = match self.resolve_object(fs_object_id) {
            Ok(resolved) => resolved,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let stamp = FsyncStamp::new(current.ctime as u64);
        let mutation = match self.with_pager(|pager| {
            pager.plan_setattr(
                inode,
                SetAttr::Owner {
                    uid: new_uid,
                    gid: new_gid,
                },
                stamp,
            )
        }) {
            Ok(mutation) => mutation,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_inode_meta_for(fs_object_id);
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn create_inode(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let parent_ino = match self.resolve_object(parent) {
            Ok((v, _)) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let (new_ino, mutation) = match self.with_pager(|pager| {
            pager.plan_create_regular_file(
                parent_ino,
                name,
                mode,
                cred.uid,
                cred.gid,
                FsyncStamp::new(0),
            )
        }) {
            Ok(result) => result,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_lookup_cache_for(parent);
                self.invalidate_inode_meta_no(new_ino);
                let object = match self.object_id_for_inode(new_ino) {
                    Ok(object) => object,
                    Err(err) => return StepOutcome::err(err.into()),
                };
                self.invalidate_file_page_container(object);
                let meta = match self.resolve_object(object) {
                    Ok((_, meta)) => meta,
                    Err(err) => return StepOutcome::err(err.into()),
                };
                StepOutcome::done((object, map_inode_meta(meta)))
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let parent_ino = match self.resolve_object(parent) {
            Ok((v, _)) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let (target_ino, current) = match self.resolve_object(target) {
            Ok(resolved) => resolved,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let mutation = match self.with_pager(|pager| {
            pager.plan_unlink_dir_entry(
                parent_ino,
                name,
                target_ino,
                FsyncStamp::new(current.ctime as u64),
            )
        }) {
            Ok(mutation) => mutation,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_lookup_cache_for(parent);
                self.invalidate_inode_meta_for(target);
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn rename(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        if old_parent == new_parent && old_name == new_name {
            return StepOutcome::done(());
        }
        let old_parent_ino = match self.resolve_object(old_parent) {
            Ok((v, _)) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let new_parent_ino = match self.resolve_object(new_parent) {
            Ok((v, _)) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };

        let old_ino = match self.lookup_cached(old_parent, old_parent_ino, old_name) {
            Ok(Some(ino)) => ino,
            Ok(None) => return StepOutcome::err(Errno::ENOENT.into()),
            Err(e) => return StepOutcome::err(e.into()),
        };
        let overwritten_ino = match self.lookup_cached(new_parent, new_parent_ino, new_name) {
            Ok(Some(ino)) => Some(ino),
            Ok(None) => None,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let old_object = match self.object_id_for_inode(old_ino) {
            Ok(object) => object,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let current = match self.resolve_object(old_object) {
            Ok((_, meta)) => meta,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let current_kind = current.mode & 0xF000;
        let same_parent_directory_rename = current_kind == Inode::S_IFDIR
            && old_parent_ino == new_parent_ino
            && overwritten_ino.is_none();
        if current_kind != Inode::S_IFREG && !same_parent_directory_rename {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        }
        let overwritten_meta = match overwritten_ino {
            Some(ino) => {
                let object = match self.object_id_for_inode(ino) {
                    Ok(object) => object,
                    Err(e) => return StepOutcome::err(e.into()),
                };
                match self.resolve_object(object) {
                    Ok((_, meta)) => Some(meta),
                    Err(e) => return StepOutcome::err(e.into()),
                }
            }
            None => None,
        };
        if overwritten_meta
            .as_ref()
            .is_some_and(|meta| meta.mode & 0xF000 != 0x8000)
        {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        }
        let mutation = match self.with_pager(|pager| {
            match (old_parent_ino == new_parent_ino, overwritten_ino) {
                (true, Some(ino)) => pager.plan_rename_overwrite_dir_entry(
                    old_parent_ino,
                    old_name,
                    new_name,
                    old_ino,
                    ino,
                    FsyncStamp::new(current.ctime as u64),
                ),
                (true, None) => pager.plan_rename_dir_entry(
                    old_parent_ino,
                    old_name,
                    new_name,
                    old_ino,
                    FsyncStamp::new(current.ctime as u64),
                ),
                (false, Some(_)) => Err(tx_ext4_format::Ext4FormatError::Unsupported),
                (false, None) => pager.plan_cross_dir_rename_dir_entry(
                    old_parent_ino,
                    old_name,
                    new_parent_ino,
                    new_name,
                    old_ino,
                    FsyncStamp::new(current.ctime as u64),
                ),
            }
        }) {
            Ok(mutation) => mutation,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_lookup_cache_for(old_parent);
                self.invalidate_lookup_cache_for(new_parent);
                self.invalidate_inode_meta_no(old_ino);
                if let Some(overwritten_ino) = overwritten_ino {
                    self.invalidate_inode_meta_no(overwritten_ino);
                }
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn link(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let parent_ino = match self.resolve_object(parent) {
            Ok((v, _)) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let (target_ino, current) = match self.resolve_object(target) {
            Ok(resolved) => resolved,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.lookup_cached(parent, parent_ino, name) {
            Ok(Some(_)) => return StepOutcome::err(Errno::EEXIST.into()),
            Ok(None) => {}
            Err(e) => return StepOutcome::err(e.into()),
        }
        if current.mode & 0xF000 != 0x8000 {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        }
        let mutation = match self.with_pager(|pager| {
            pager.plan_link_dir_entry(
                parent_ino,
                name,
                target_ino,
                FsyncStamp::new(current.ctime as u64),
            )
        }) {
            Ok(mutation) => mutation,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_lookup_cache_for(parent);
                self.invalidate_inode_meta_for(target);
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let parent_ino = match self.resolve_object(parent) {
            Ok((v, _)) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.lookup_cached(parent, parent_ino, name) {
            Ok(Some(_)) => return StepOutcome::err(Errno::EEXIST.into()),
            Ok(None) => {}
            Err(e) => return StepOutcome::err(e.into()),
        }
        let (new_ino, _data_block, mutation) = match self.with_pager(|pager| {
            pager.plan_create_directory(
                parent_ino,
                name,
                mode,
                cred.uid,
                cred.gid,
                FsyncStamp::new(0),
            )
        }) {
            Ok(result) => result,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_lookup_cache_for(parent);
                self.invalidate_inode_meta_no(new_ino);
                let object = match self.object_id_for_inode(new_ino) {
                    Ok(object) => object,
                    Err(err) => return StepOutcome::err(err.into()),
                };
                let meta = match self.resolve_object(object) {
                    Ok((_, meta)) => meta,
                    Err(err) => return StepOutcome::err(err.into()),
                };
                StepOutcome::done((object, map_inode_meta(meta)))
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let parent_ino = match self.resolve_object(parent) {
            Ok((v, _)) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let (target_ino, current) = match self.resolve_object(target) {
            Ok(resolved) => resolved,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let mutation = match self.with_pager(|pager| {
            pager.plan_rmdir_dir_entry(
                parent_ino,
                name,
                target_ino,
                FsyncStamp::new(current.ctime as u64),
            )
        }) {
            Ok(mutation) => mutation,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_lookup_cache_for(parent);
                // The numeric directory inode may become reusable as soon as
                // the removed directory loses its final live reference. Drop
                // its cached generation/kind together with its old children
                // before a later path walk can validate a stale object ID.
                self.invalidate_lookup_cache_for(target);
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn symlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let parent_ino = match self.resolve_object(parent) {
            Ok((v, _)) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.lookup_cached(parent, parent_ino, name) {
            Ok(Some(_)) => return StepOutcome::err(Errno::EEXIST.into()),
            Ok(None) => {}
            Err(e) => return StepOutcome::err(e.into()),
        }
        let (new_ino, mutation) = match self.with_pager(|pager| {
            pager.plan_create_fast_symlink(
                parent_ino,
                name,
                link_target,
                cred.uid,
                cred.gid,
                FsyncStamp::new(0),
            )
        }) {
            Ok(result) => result,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_lookup_cache_for(parent);
                self.invalidate_inode_meta_no(new_ino);
                let object = match self.object_id_for_inode(new_ino) {
                    Ok(object) => object,
                    Err(err) => return StepOutcome::err(err.into()),
                };
                let meta = match self.resolve_object(object) {
                    Ok((_, meta)) => meta,
                    Err(err) => return StepOutcome::err(err.into()),
                };
                StepOutcome::done((object, map_inode_meta(meta)))
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        let inode = match self.resolve_object(fs_object_id) {
            Ok((inode, _)) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let offset = match cursor_offset(cursor) {
            Ok(offset) => offset,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
        let mut next_offsets = [0u64; READDIR_WINDOW_ENTRIES];
        let count = match self.read_dir_entries_cached(
            fs_object_id,
            inode,
            offset,
            &mut entries,
            &mut next_offsets,
        ) {
            Ok(count) => count,
            Err(err) => return StepOutcome::err(err.into()),
        };
        if count == 0 {
            return StepOutcome::done(None);
        }

        let entry = entries[0];
        let name = match InlineName::new(entry.name()) {
            Ok(name) => name,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let entry_id = match self.object_id_for_inode(entry.inode) {
            Ok(object) => object,
            Err(err) => return StepOutcome::err(err.into()),
        };
        StepOutcome::done(Some((
            DirEntry {
                name,
                fs_object_id: entry_id,
                kind: ext4_file_type_to_kind(entry.file_type),
            },
            cursor_from_offset(next_offsets[0]),
        )))
    }

    fn destroy_inode(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let Some(_admission) = self.lock_metadata_mutation_for_frontend() else {
            return self.wait_for_metadata_mutation_admission();
        };
        let (inode, current) = match self.resolve_object(fs_object_id) {
            Ok(resolved) => resolved,
            Err(err) => return StepOutcome::err(err.into()),
        };
        // VFS object-lifetime retirement probes `destroy_inode` whenever the
        // final in-memory pin disappears, including ordinary linked files and
        // directories.  A live link proves that backend storage is not
        // reclaimable, so do not serialize that common read-only retirement
        // through the mutation planner merely to have it reject the inode.
        if current.nlinks != 0 {
            return StepOutcome::done(());
        }
        let mutation = match self.with_pager(|pager| {
            pager.plan_destroy_inode(inode, FsyncStamp::new(current.ctime.into()))
        }) {
            Ok(mutation) => mutation,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.commit_metadata_mutation(&mutation, guard) {
            Ok(()) => {
                self.invalidate_file_page_container(fs_object_id);
                self.settle_metadata_caches();
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn settle_file(
        &self,
        _fs_object_id: FsObjectId,
        _generation_frontier: &FileFsyncFrontier,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        self.settle_metadata_caches();
        StepOutcome::done(())
    }

    fn settle_mount(
        &self,
        _transaction_frontier: MountTransactionFrontier,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        self.settle_metadata_caches();
        StepOutcome::done(())
    }

    fn shutdown(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        match self.shutdown_mount() {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        mount: &Cap<MountPayload>,
        guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, NoProgress> {
        let pin = match self.mount_pin.lock().clone() {
            Some(p) => p,
            None => return StepOutcome::err(Errno::ENOSYS.into()),
        };

        const PAGE_SIZE: u64 = 4096;
        // Always allocate a writable growth window for regular files.
        // `PageContainer::new()` initialises `size_bytes` to `page_count *
        // PAGE_SIZE` (the physical capacity), not to the inode's logical size,
        // so we must call `set_size_bytes` afterwards.  Without this correction
        // O_APPEND writes compute `offset = size_bytes = PAGE_SIZE`, which
        // immediately exceeds `capacity = PAGE_SIZE`, yielding EINVAL.
        let page_count = meta
            .size
            .div_ceil(PAGE_SIZE)
            .max(EXT4_FILE_INITIAL_PAGE_WINDOW);
        let pc = match self.file_page_container_for_materialized_inode(
            fs_object_id,
            page_count,
            meta.size,
            pin,
            guard,
        ) {
            Ok(pc) => pc,
            Err(err) => return StepOutcome::err(err.into()),
        };

        match RNode::new_cap_in_mount(fs_object_id, meta, RNodeBacking::PageBacked { pc }, mount) {
            Ok(rnode) => StepOutcome::done(rnode),
            Err(_) => StepOutcome::err(Errno::ENOMEM.into()),
        }
    }

    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<alloc::boxed::Box<[u8]>, NoProgress> {
        let inode = match self.resolve_object(fs_object_id) {
            Ok((inode, _)) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.with_pager(|pager| pager.read_symlink(inode)) {
            Ok(bytes) => StepOutcome::done(bytes.into_boxed_slice()),
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    // `chmod_inode`, `chown_inode` commit through `serialize_inode_meta`.
}
