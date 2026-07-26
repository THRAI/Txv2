use crate::adapter::step_engine::{self as step_engine, Cap, NoProgress, StepOutcome};
use step_engine::Guard;
use tx_ext4_format::pager::{BlockImage, DirEntryLite, InodeMetaLite};
use tx_subsystems::execution::Errno;
use tx_subsystems::mount::MountPayload;
use tx_subsystems::vfs::structure::{
    Credential, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta, RNode,
    RNodeBacking,
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

fn current_ext4_time_sec() -> u32 {
    tx_subsystems::wall_clock::current_realtime_sec().min(u32::MAX as u64) as u32
}

/// Initial sparse-cache window for ext4 regular-file PageContainers.
///
/// This is not a file-size limit: file-backed PageContainers are sparse and
/// may grow beyond this window. Keeping a moderate initial value preserves
/// the existing cache geometry without rejecting large linker outputs.
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
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.with_pager(|pager| {
            pager
                .write_inode_meta_journaled(inode, inode_meta_lite(meta))
                .map(|_| ())
        }) {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn step_chmod(
        &self,
        fs_object_id: FsObjectId,
        new_mode: u16,
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        // ext4 previously inherited the trait's ENOSYS default; with
        // the onsite alpine image mounted as an ext4 root, git init's
        // core.filemode probe (chmod on .git/config.lock) hit it and
        // aborted. Semantics mirror the tmpfs implementation: keep
        // IFMT, replace the low 12 permission bits, persist through
        // the journaled inode-meta write.
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let meta = match self.inode_meta_cached(inode) {
            Ok(meta) => map_inode_meta(meta),
            Err(err) => return StepOutcome::err(err.into()),
        };
        if let Err(e) = tx_subsystems::vfs::predicates::check_chmod_perm(&meta, cred) {
            return StepOutcome::err(e.into());
        }
        let mut updated = meta;
        updated.mode = (updated.mode & tx_subsystems::vfs::structure::S_IFMT) | (new_mode & 0o7777);
        let write = self.with_pager(|pager| {
            pager
                .write_inode_meta_journaled(inode, inode_meta_lite(&updated))
                .map(|_| ())
        });
        match write {
            Ok(()) => {
                self.invalidate_inode_meta(inode);
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
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let now_sec = current_ext4_time_sec();
        match self.with_pager(|pager| {
            pager.create_regular_file(parent_ino, name, mode, cred.uid, cred.gid, now_sec)
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
        target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let target_ino = match inode_no(target) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.with_pager(|pager| pager.unlink_inode(parent_ino, name, target_ino)) {
            Ok(remaining_links) => {
                self.invalidate_lookup_cache_for(parent_ino);
                self.invalidate_inode_meta(target_ino);
                if remaining_links == 0 {
                    self.mark_inode_orphaned(target_ino);
                }
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
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let old_parent_ino = match inode_no(old_parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let new_parent_ino = match inode_no(new_parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };

        match self.with_pager(|pager| {
            pager.rename_inode(old_parent_ino, old_name, new_parent_ino, new_name)
        }) {
            Ok(outcome) => {
                self.invalidate_lookup_cache_for(old_parent_ino);
                self.invalidate_lookup_cache_for(new_parent_ino);
                if let Some((displaced, remaining_links)) = outcome.displaced {
                    self.invalidate_inode_meta(displaced);
                    if remaining_links == 0 {
                        self.mark_inode_orphaned(displaced);
                    }
                }
                StepOutcome::done(())
            }
            Err(e) => StepOutcome::err(e.into()),
        }
    }

    fn link(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let target_ino = match inode_no(target) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.lookup_cached(parent_ino, name) {
            Ok(Some(_)) => return StepOutcome::err(Errno::EEXIST.into()),
            Ok(None) => {}
            Err(e) => return StepOutcome::err(e.into()),
        }
        let meta = match self.inode_meta_cached(target_ino) {
            Ok(meta) => meta,
            Err(e) => return StepOutcome::err(e.into()),
        };
        if meta.mode & 0xF000 == 0x4000 {
            return StepOutcome::err(Errno::EPERM.into());
        }
        match self.with_pager(|pager| pager.link_inode(parent_ino, name, target_ino)) {
            Ok(()) => {
                self.invalidate_lookup_cache_for(parent_ino);
                self.invalidate_inode_meta(target_ino);
                StepOutcome::done(())
            }
            Err(e) => StepOutcome::err(e.into()),
        }
    }

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
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
        let now_sec = current_ext4_time_sec();
        match self.with_pager(|pager| {
            pager.create_directory(parent_ino, name, mode, cred.uid, cred.gid, now_sec)
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
        target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        let target_ino = match inode_no(target) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.with_pager(|pager| pager.unlink_directory(parent_ino, name, target_ino)) {
            Ok(()) => {
                self.invalidate_lookup_cache_for(parent_ino);
                self.invalidate_inode_meta(target_ino);
                self.mark_inode_orphaned(target_ino);
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
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.destroy_orphaned_inode(inode, guard) {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        _meta: InodeMeta,
        mount: &Cap<MountPayload>,
        guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, NoProgress> {
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let pin = match self.mount_pin.lock().clone() {
            Some(p) => p,
            None => return StepOutcome::err(Errno::ENOSYS.into()),
        };

        // Always allocate a writable growth window for regular files.
        // `PageContainer::new()` initialises `size_bytes` to `page_count *
        // PAGE_SIZE` (the physical capacity), not to the inode's logical size,
        // so we must call `set_size_bytes` afterwards.  Without this correction
        // O_APPEND writes compute `offset = size_bytes = PAGE_SIZE`, which
        // immediately exceeds `capacity = PAGE_SIZE`, yielding EINVAL.
        let (pc, disk_meta) = match self.get_or_create_page_container(
            inode,
            fs_object_id,
            pin,
            EXT4_FILE_INITIAL_PAGE_WINDOW,
            guard,
        ) {
            Ok(result) => result,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let mut materialized_meta = map_inode_meta(disk_meta);
        materialized_meta.size = pc.size_bytes();

        match RNode::new_cap_in_mount(
            fs_object_id,
            materialized_meta,
            RNodeBacking::PageBacked { pc },
            mount,
        ) {
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

    // `step_chown` would commit the same way if a workload needs it.
}

fn inode_meta_lite(meta: &InodeMeta) -> InodeMetaLite {
    fn sec_to_u32(sec: i64) -> u32 {
        if sec <= 0 {
            0
        } else {
            sec.min(u32::MAX as i64) as u32
        }
    }

    InodeMetaLite {
        mode: meta.mode,
        uid: meta.uid,
        gid: meta.gid,
        size: meta.size,
        nlinks: meta.nlinks,
        blocks_512: meta.blocks,
        flags: meta.flags,
        atime: sec_to_u32(meta.atime.sec),
        ctime: sec_to_u32(meta.ctime.sec),
        mtime: sec_to_u32(meta.mtime.sec),
    }
}
