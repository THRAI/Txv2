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
    cursor_from_offset, cursor_offset, map_inode_meta, Ext4FsInstance, READDIR_WINDOW_ENTRIES,
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
        let parent_inode = match self.resolve_object(parent) {
            Ok((parent, _)) => parent,
            Err(err) => {
                tx_subsystems::vfs::resolution::diagnostic::record_diag(21);
                return StepOutcome::err(err);
            }
        };

        match self.lookup_cached(parent, parent_inode, name) {
            Ok(Some(inode)) => match self.object_id_for_inode(inode) {
                Ok(id) => StepOutcome::done(id),
                Err(err) => StepOutcome::err(err.into()),
            },
            Ok(None) => {
                tx_subsystems::vfs::resolution::diagnostic::record_diag(22); // ENOENT
                StepOutcome::err(Errno::ENOENT)
            }
            Err(err) => {
                tx_subsystems::vfs::resolution::diagnostic::record_diag(23); // format err
                StepOutcome::err(err)
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
            Err(err) => return StepOutcome::err(err),
        };

        match self.inode_meta_cached(inode) {
            Ok(meta) => StepOutcome::done(map_inode_meta(meta)),
            Err(err) => StepOutcome::err(err),
        }
    }

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS);
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err),
        };
        match self.with_pager(|pager| {
            pager
                .write_inode_meta_journaled(inode, inode_meta_lite(meta))
                .map(|_| ())
        }) {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(err),
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
            return StepOutcome::err(Errno::EROFS);
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e),
        };
        let now_sec = current_ext4_time_sec();
        match self.with_pager_namespace_mutation(&[parent], |pager| {
            pager.create_regular_file(parent_ino, name, mode, cred.uid, cred.gid, now_sec)
        }) {
            Ok(new_ino) => {
                self.invalidate_lookup_cache_for(parent);
                let meta = match self.with_pager(|pager| pager.inode_meta(new_ino)) {
                    Ok(m) => m,
                    Err(e) => return StepOutcome::err(e),
                };
                StepOutcome::done((
                    tx_subsystems::vfs::structure::FsObjectId::from_inode_generation(
                        new_ino.get(),
                        meta.generation,
                    ),
                    map_inode_meta(meta),
                ))
            }
            Err(e) => StepOutcome::err(e),
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
            return StepOutcome::err(Errno::EROFS);
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e),
        };
        let target_ino = match self.resolve_object(target) {
            Ok((v, _)) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.with_pager_namespace_mutation(&[parent], |pager| {
            pager.unlink_inode(parent_ino, name, target_ino)
        }) {
            Ok(remaining_links) => {
                self.invalidate_lookup_cache_for(parent);
                if remaining_links == 0 {
                    self.mark_inode_orphaned(target);
                }
                StepOutcome::done(())
            }
            Err(e) => StepOutcome::err(e),
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
            return StepOutcome::err(Errno::EROFS);
        }
        let old_parent_ino = match inode_no(old_parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e),
        };
        let new_parent_ino = match inode_no(new_parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e),
        };

        // Resolve old inode number and its ext4 file_type.
        let old_ino = match self.with_pager(|pager| pager.lookup(old_parent_ino, old_name)) {
            Ok(Some(ino)) => ino,
            Ok(None) => return StepOutcome::err(Errno::ENOENT),
            Err(e) => return StepOutcome::err(e),
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
            Err(e) => return StepOutcome::err(e),
        };

        // Remove destination entry if it already exists (best-effort;
        // the syscall layer already enforces RENAME_NOREPLACE before we
        // get here, so this path is for overwrite-replace semantics).
        let _ = self.with_pager(|pager| pager.remove_dir_entry(new_parent_ino, new_name));

        // Install the new directory entry.
        if let Err(e) = self.with_pager(|pager| {
            pager.append_dir_entry(new_parent_ino, new_name, old_ino, file_type)
        }) {
            return StepOutcome::err(e);
        }
        self.invalidate_lookup_cache_for(new_parent_ino);

        // Remove the old directory entry.
        match self.with_pager(|pager| pager.remove_dir_entry(old_parent_ino, old_name)) {
            Ok(_) => {
                self.invalidate_lookup_cache_for(old_parent_ino);
                StepOutcome::done(())
            }
            Err(e) => StepOutcome::err(e),
        }
    }

    fn link(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
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
            return StepOutcome::err(Errno::EROFS);
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e),
        };
        match self.with_pager(|pager| pager.lookup(parent_ino, name)) {
            Ok(Some(_)) => return StepOutcome::err(Errno::EEXIST),
            Ok(None) => {}
            Err(e) => return StepOutcome::err(e),
        }
        let now_sec = current_ext4_time_sec();
        match self.with_pager_namespace_mutation(&[parent], |pager| {
            pager.create_directory(parent_ino, name, mode, cred.uid, cred.gid, now_sec)
        }) {
            Ok(new_ino) => {
                self.invalidate_lookup_cache_for(parent);
                let meta = match self.with_pager(|pager| pager.inode_meta(new_ino)) {
                    Ok(m) => m,
                    Err(e) => return StepOutcome::err(e),
                };
                StepOutcome::done((
                    tx_subsystems::vfs::structure::FsObjectId::from_inode_generation(
                        new_ino.get(),
                        meta.generation,
                    ),
                    map_inode_meta(meta),
                ))
            }
            Err(e) => StepOutcome::err(e),
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
            return StepOutcome::err(Errno::EROFS);
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e),
        };
        let target_ino = match self.resolve_object(target) {
            Ok((v, _)) => v,
            Err(e) => return StepOutcome::err(e.into()),
        };
        match self.with_pager_namespace_mutation(&[parent], |pager| {
            pager.unlink_directory(parent_ino, name, target_ino)
        }) {
            Ok(()) => {
                self.invalidate_lookup_cache_for(parent);
                self.mark_inode_orphaned(target);
                StepOutcome::done(())
            }
            Err(e) => StepOutcome::err(e),
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
        StepOutcome::err(Errno::ENOSYS)
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err),
        };
        let offset = match cursor_offset(cursor) {
            Ok(offset) => offset,
            Err(err) => return StepOutcome::err(err),
        };
        let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
        let mut next_offsets = [0u64; READDIR_WINDOW_ENTRIES];
        let count =
            match self.read_dir_entries_cached(inode, offset, &mut entries, &mut next_offsets) {
                Ok(count) => count,
                Err(err) => return StepOutcome::err(err),
            };
        if count == 0 {
            return StepOutcome::done(None);
        }

        let entry = entries[0];
        let name = match InlineName::new(entry.name()) {
            Ok(name) => name,
            Err(err) => return StepOutcome::err(err),
        };
        let entry_id = match self.object_id_for_inode(entry.inode) {
            Ok(id) => id,
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
        StepOutcome::err(Errno::ENOSYS)
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        _meta: InodeMeta,
        mount: &Cap<MountPayload>,
        guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, NoProgress> {
        if let Err(err) = self.resolve_object(fs_object_id) {
            return StepOutcome::err(err.into());
        }
        let pin = match self.mount_pin.lock().clone() {
            Some(p) => p,
            None => return StepOutcome::err(Errno::ENOSYS),
        };

        // Always allocate a writable growth window for regular files.
        // `PageContainer::new()` initialises `size_bytes` to `page_count *
        // PAGE_SIZE` (the physical capacity), not to the inode's logical size,
        // so we must call `set_size_bytes` afterwards.  Without this correction
        // O_APPEND writes compute `offset = size_bytes = PAGE_SIZE`, which
        // immediately exceeds `capacity = PAGE_SIZE`, yielding EINVAL.
        let (pc, disk_meta) = match self.get_or_create_page_container(
            fs_object_id,
            pin,
            EXT4_FILE_INITIAL_PAGE_WINDOW,
            guard,
        ) {
            Ok(pc) => pc,
            Err(_) => return StepOutcome::err(Errno::ENOMEM),
        };
        pc.set_size_bytes(meta.size);
        self.bind_file_page_container(pc.clone());

        match RNode::new_cap_in_mount(
            fs_object_id,
            materialized_meta,
            RNodeBacking::PageBacked { pc },
            mount,
        ) {
            Ok(rnode) => StepOutcome::done(rnode),
            Err(_) => StepOutcome::err(Errno::ENOMEM),
        }
    }

    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<alloc::boxed::Box<[u8]>, NoProgress> {
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err),
        };
        match self.with_pager(|pager| pager.read_symlink(inode)) {
            Ok(bytes) => StepOutcome::done(bytes.into_boxed_slice()),
            Err(err) => StepOutcome::err(err),
        }
    }

    // `chmod_inode`, `chown_inode` commit through `serialize_inode_meta`.
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
        // `write_inode_meta_journaled` preserves the on-disk generation; this
        // value is intentionally not applied by the format layer.
        generation: 0,
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
