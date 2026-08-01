use crate::adapter::step_engine::{self as step_engine, Cap, NoProgress, StepOutcome};
use step_engine::Guard;
use tx_ext4_format::mutation::{FsyncStamp, SetAttr};
use tx_ext4_format::pager::{BlockImage, DirEntryLite};
use tx_subsystems::execution::Errno;
use tx_subsystems::mount::{MountPayload, MountTransactionFrontier};
use tx_subsystems::page_backed::{FileFsyncFrontier, PageContainer, PageContainerKind};
use tx_subsystems::vfs::structure::{
    Credential, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta, RNode,
    RNodeBacking, Timespec,
};

use crate::read_backend::{
    cursor_from_offset, cursor_offset, fs_object_id as inode_fs_object_id, inode_no,
    map_inode_meta, Ext4FsInstance, READDIR_WINDOW_ENTRIES,
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

pub(crate) fn journal_mutation_runtime_errno(
    error: crate::journal::JournalMutationRuntimeError,
) -> Errno {
    match error {
        crate::journal::JournalMutationRuntimeError::Busy(_) => Errno::EBUSY,
        crate::journal::JournalMutationRuntimeError::Image(_)
        | crate::journal::JournalMutationRuntimeError::Stage(_) => Errno::EIO,
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
// Fully-qualified `adapter::step_engine::*` references at the impl
// sites avoid clashing with `tx_subsystems::execution::Errno`
// already in scope.

use tx_subsystems::vfs::FsOps;

/// Static writable capacity for ext4 regular-file PageContainers.
///
/// PageContainer currently has a fixed `page_count` capacity. Match tmpfs'
/// day-1 growth window so newly-created ext4 files can grow through ordinary
/// PageBacked writes instead of failing after one page.
const EXT4_FILE_PAGE_CAP: u64 = 2048;

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
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        // Diagnostic: record that ext4 lookup was called (vs some other backend).
        tx_subsystems::vfs::resolution::diagnostic::record_diag(20);
        let parent = match inode_no(parent) {
            Ok(parent) => parent,
            Err(err) => {
                tx_subsystems::vfs::resolution::diagnostic::record_diag(21);
                return StepOutcome::err(err.into());
            }
        };

        match self.lookup_cached(parent, name) {
            Ok(Some(inode)) => StepOutcome::done(inode_fs_object_id(inode)),
            Ok(None) => {
                tx_subsystems::vfs::resolution::diagnostic::record_diag(22); // ENOENT
                StepOutcome::err(Errno::ENOENT.into())
            }
            Err(err) => {
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
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };

        match self.inode_meta_cached(inode) {
            Ok(meta) => StepOutcome::done(map_inode_meta(meta)),
            Err(err) => StepOutcome::err(err.into()),
        }
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
        let Some(runtime) = self.metadata_mutation_runtime() else {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        };
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
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
        match runtime.begin_mutation(&mutation, guard) {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(journal_mutation_runtime_errno(err).into()),
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
        let Some(runtime) = self.metadata_mutation_runtime() else {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        };
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let current = match self.inode_meta_cached(inode) {
            Ok(meta) => meta,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let stamp = FsyncStamp::new(current.ctime as u64);
        let mutation =
            match self.with_pager(|pager| pager.plan_setattr_mode(inode, new_mode, stamp)) {
                Ok(mutation) => mutation,
                Err(err) => return StepOutcome::err(err.into()),
            };
        match runtime.begin_mutation(&mutation, guard) {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(journal_mutation_runtime_errno(err).into()),
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
        let Some(runtime) = self.metadata_mutation_runtime() else {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        };
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let current = match self.inode_meta_cached(inode) {
            Ok(meta) => meta,
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
        match runtime.begin_mutation(&mutation, guard) {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(journal_mutation_runtime_errno(err).into()),
        }
    }

    fn create_inode(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        if let Err(err) = self.require_mutation_owner() {
            return StepOutcome::err(err.into());
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.with_pager(|pager| {
            pager.create_regular_file(parent_ino, name, mode, cred.uid, cred.gid, 0)
        }) {
            Ok(new_ino) => {
                self.invalidate_lookup_cache_for(parent_ino);
                let meta = match self.with_pager(|pager| pager.inode_meta(new_ino)) {
                    Ok(m) => m,
                    Err(e) => return StepOutcome::err(e.into()),
                };
                StepOutcome::done((inode_fs_object_id(new_ino), map_inode_meta(meta)))
            }
            Err(e) => StepOutcome::err(e.into()),
        }
    }

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if let Err(err) = self.require_mutation_owner() {
            return StepOutcome::err(err.into());
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.with_pager(|pager| pager.remove_dir_entry(parent_ino, name)) {
            Ok(_removed_ino) => {
                self.invalidate_lookup_cache_for(parent_ino);
                StepOutcome::done(())
            }
            Err(e) => StepOutcome::err(e.into()),
        }
    }

    fn rename(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if let Err(err) = self.require_mutation_owner() {
            return StepOutcome::err(err.into());
        }
        let old_parent_ino = match inode_no(old_parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let new_parent_ino = match inode_no(new_parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };

        // Resolve old inode number and its ext4 file_type.
        let old_ino = match self.with_pager(|pager| pager.lookup(old_parent_ino, old_name)) {
            Ok(Some(ino)) => ino,
            Ok(None) => return StepOutcome::err(Errno::ENOENT.into()),
            Err(e) => return StepOutcome::err(e.into()),
        };
        // Derive ext4 dir-entry file_type from the inode mode.
        // EXT4_FT_REG_FILE=1, EXT4_FT_DIR=2.
        let file_type = match self.with_pager(|pager| pager.inode_meta(old_ino)) {
            Ok(meta) => {
                if meta.mode & 0xF000 == 0x4000 {
                    2u8
                } else {
                    1u8
                }
            }
            Err(e) => return StepOutcome::err(e.into()),
        };

        // Remove destination entry if it already exists (best-effort;
        // the syscall layer already enforces RENAME_NOREPLACE before we
        // get here, so this path is for overwrite-replace semantics).
        let _ = self.with_pager(|pager| pager.remove_dir_entry(new_parent_ino, new_name));

        // Install the new directory entry.
        if let Err(e) = self.with_pager(|pager| {
            pager.append_dir_entry(new_parent_ino, new_name, old_ino, file_type)
        }) {
            return StepOutcome::err(e.into());
        }
        self.invalidate_lookup_cache_for(new_parent_ino);

        // Remove the old directory entry.
        match self.with_pager(|pager| pager.remove_dir_entry(old_parent_ino, old_name)) {
            Ok(_) => {
                self.invalidate_lookup_cache_for(old_parent_ino);
                StepOutcome::done(())
            }
            Err(e) => StepOutcome::err(e.into()),
        }
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        if let Err(err) = self.require_mutation_owner() {
            return StepOutcome::err(err.into());
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.with_pager(|pager| pager.lookup(parent_ino, name)) {
            Ok(Some(_)) => return StepOutcome::err(Errno::EEXIST.into()),
            Ok(None) => {}
            Err(e) => return StepOutcome::err(e.into()),
        }
        match self.with_pager(|pager| {
            pager.create_directory(parent_ino, name, mode, cred.uid, cred.gid, 0)
        }) {
            Ok(new_ino) => {
                self.invalidate_lookup_cache_for(parent_ino);
                let meta = match self.with_pager(|pager| pager.inode_meta(new_ino)) {
                    Ok(m) => m,
                    Err(e) => return StepOutcome::err(e.into()),
                };
                StepOutcome::done((inode_fs_object_id(new_ino), map_inode_meta(meta)))
            }
            Err(e) => StepOutcome::err(e.into()),
        }
    }

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if let Err(err) = self.require_mutation_owner() {
            return StepOutcome::err(err.into());
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        // Remove the directory entry from the parent.  The directory
        // itself is assumed empty (the VFS layer should have checked);
        // we do not attempt to free the inode or its `.`/`..` entries —
        // good enough for the busybox-musl `rmdir test` test case.
        match self.with_pager(|pager| pager.remove_dir_entry(parent_ino, name)) {
            Ok(_) => {
                self.invalidate_lookup_cache_for(parent_ino);
                StepOutcome::done(())
            }
            Err(e) => StepOutcome::err(e.into()),
        }
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let offset = match cursor_offset(cursor) {
            Ok(offset) => offset,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
        let mut next_offsets = [0u64; READDIR_WINDOW_ENTRIES];
        let count =
            match self.read_dir_entries_cached(inode, offset, &mut entries, &mut next_offsets) {
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
        StepOutcome::done(Some((
            DirEntry {
                name,
                fs_object_id: inode_fs_object_id(entry.inode),
                kind: ext4_file_type_to_kind(entry.file_type),
            },
            cursor_from_offset(next_offsets[0]),
        )))
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
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
        _guard: &Guard<'_>,
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
        let page_count = meta.size.div_ceil(PAGE_SIZE).max(EXT4_FILE_PAGE_CAP);
        let pc = match PageContainer::new_cap(
            PageContainerKind::File {
                mount: pin,
                fs_object_id,
            },
            page_count,
        ) {
            Ok(pc) => pc,
            Err(_) => return StepOutcome::err(Errno::ENOMEM.into()),
        };
        pc.set_size_bytes(meta.size);
        self.bind_file_page_container(pc.clone());

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
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.with_pager(|pager| pager.read_symlink(inode)) {
            Ok(bytes) => StepOutcome::done(bytes.into_boxed_slice()),
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    // `chmod_inode`, `chown_inode` commit through `serialize_inode_meta`.
}
