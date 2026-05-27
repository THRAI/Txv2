use crate::adapter::step_engine::{self as step_engine, Cap, NoProgress, StepOutcome};
use step_engine::Guard;
use tx_ext4_format::pager::{BlockImage, DirEntryLite};
use tx_subsystems::execution::Errno;
use tx_subsystems::mount::MountPayload;
use tx_subsystems::page_backed::{PageContainer, PageContainerKind};
use tx_subsystems::vfs::structure::{
    Credential, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta, RNode,
    RNodeBacking,
};

use crate::read_backend::{
    cursor_from_index, cursor_index, fs_object_id as inode_fs_object_id, inode_no, map_inode_meta,
    Ext4FsInstance, READDIR_WINDOW_ENTRIES,
};
use tx_ext4_format::pager::InodeMetaLite;

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
                return StepOutcome::err(err);
            }
        };

        match self.lookup_cached(parent, name) {
            Ok(Some(inode)) => StepOutcome::done(inode_fs_object_id(inode)),
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
        match self.with_pager(|pager| pager.write_inode_meta(inode, inode_meta_lite(meta))) {
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
        match self.with_pager(|pager| {
            pager.create_regular_file(parent_ino, name, mode, cred.uid, cred.gid, 0)
        }) {
            Ok(new_ino) => {
                self.invalidate_lookup_cache_for(parent_ino);
                let meta = match self.with_pager(|pager| pager.inode_meta(new_ino)) {
                    Ok(m) => m,
                    Err(e) => return StepOutcome::err(e),
                };
                StepOutcome::done((inode_fs_object_id(new_ino), map_inode_meta(meta)))
            }
            Err(e) => StepOutcome::err(e),
        }
    }

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS);
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e),
        };
        match self.with_pager(|pager| pager.remove_dir_entry(parent_ino, name)) {
            Ok(_removed_ino) => {
                self.invalidate_lookup_cache_for(parent_ino);
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
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
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
        match self.with_pager(|pager| {
            pager.create_directory(parent_ino, name, mode, cred.uid, cred.gid, 0)
        }) {
            Ok(new_ino) => {
                self.invalidate_lookup_cache_for(parent_ino);
                let meta = match self.with_pager(|pager| pager.inode_meta(new_ino)) {
                    Ok(m) => m,
                    Err(e) => return StepOutcome::err(e),
                };
                StepOutcome::done((inode_fs_object_id(new_ino), map_inode_meta(meta)))
            }
            Err(e) => StepOutcome::err(e),
        }
    }

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS);
        }
        let parent_ino = match inode_no(parent) {
            Ok(v) => v,
            Err(e) => return StepOutcome::err(e),
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
        let index = match cursor_index(cursor) {
            Ok(index) => index,
            Err(err) => return StepOutcome::err(err),
        };
        if index >= READDIR_WINDOW_ENTRIES {
            return StepOutcome::err(Errno::ENOSYS);
        }

        let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
        let count = match self.read_dir_entries_cached(inode, &mut entries) {
            Ok(count) => count,
            Err(err) => return StepOutcome::err(err),
        };
        if index >= count {
            return StepOutcome::done(None);
        }

        let entry = entries[index];
        let name = match InlineName::new(entry.name()) {
            Ok(name) => name,
            Err(err) => return StepOutcome::err(err),
        };
        StepOutcome::done(Some((
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
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
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
            None => return StepOutcome::err(Errno::ENOSYS),
        };

        const PAGE_SIZE: u64 = 4096;
        // Always allocate at least one page so empty files have write capacity.
        // `PageContainer::new()` initialises `size_bytes` to `page_count *
        // PAGE_SIZE` (the physical capacity), not to the inode's logical size,
        // so we must call `set_size_bytes` afterwards.  Without this correction
        // O_APPEND writes compute `offset = size_bytes = PAGE_SIZE`, which
        // immediately exceeds `capacity = PAGE_SIZE`, yielding EINVAL.
        let page_count = meta.size.div_ceil(PAGE_SIZE).max(1);
        let pc = match PageContainer::new_cap(
            PageContainerKind::File {
                mount: pin,
                fs_object_id,
            },
            page_count,
        ) {
            Ok(pc) => pc,
            Err(_) => return StepOutcome::err(Errno::ENOMEM),
        };
        pc.set_size_bytes(meta.size);

        match RNode::new_cap_in_mount(fs_object_id, meta, RNodeBacking::PageBacked { pc }, mount) {
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

    fn get_xattr(
        &self,
        fs_object_id: FsObjectId,
        name: &[u8],
        value: &mut [u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<usize, NoProgress> {
        if let Err(errno) = tx_subsystems::vfs::xattr::validate_xattr_name(name) {
            return StepOutcome::err(errno.into());
        }
        if let Err(errno) = tx_subsystems::vfs::xattr::validate_xattr_value_len(value.len()) {
            return StepOutcome::err(errno.into());
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.xattrs(inode) {
            Ok(attrs) => {
                let Some(attr) = attrs.iter().find(|attr| attr.name == name) else {
                    return StepOutcome::err(Errno::ENODATA.into());
                };
                if value.is_empty() {
                    return StepOutcome::done(attr.value.len());
                }
                if value.len() < attr.value.len() {
                    return StepOutcome::err(Errno::ERANGE.into());
                }
                value[..attr.value.len()].copy_from_slice(&attr.value);
                StepOutcome::done(attr.value.len())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn list_xattr(
        &self,
        fs_object_id: FsObjectId,
        list: &mut [u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<usize, NoProgress> {
        if let Err(errno) = tx_subsystems::vfs::xattr::validate_xattr_list_len(list.len()) {
            return StepOutcome::err(errno.into());
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        match self.xattrs(inode) {
            Ok(attrs) => {
                let required = attrs
                    .iter()
                    .try_fold(0usize, |acc, attr| acc.checked_add(attr.name.len() + 1));
                let Some(required) = required else {
                    return StepOutcome::err(Errno::E2BIG.into());
                };
                if required > tx_subsystems::vfs::xattr::XATTR_LIST_MAX {
                    return StepOutcome::err(Errno::E2BIG.into());
                }
                if list.is_empty() {
                    return StepOutcome::done(required);
                }
                if list.len() < required {
                    return StepOutcome::err(Errno::ERANGE.into());
                }
                let mut cursor = 0usize;
                for attr in attrs {
                    let end = cursor + attr.name.len();
                    list[cursor..end].copy_from_slice(&attr.name);
                    list[end] = 0;
                    cursor = end + 1;
                }
                StepOutcome::done(required)
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn set_xattr(
        &self,
        fs_object_id: FsObjectId,
        name: &[u8],
        value: &[u8],
        flags: u32,
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        if let Err(errno) = tx_subsystems::vfs::xattr::validate_xattr_name(name) {
            return StepOutcome::err(errno.into());
        }
        if let Err(errno) = tx_subsystems::vfs::xattr::validate_xattr_value_len(value.len()) {
            return StepOutcome::err(errno.into());
        }
        if let Err(errno) = tx_subsystems::vfs::xattr::validate_xattr_set_flags(flags) {
            return StepOutcome::err(errno.into());
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let meta = match self.inode_meta_cached(inode) {
            Ok(meta) => map_inode_meta(meta),
            Err(err) => return StepOutcome::err(err.into()),
        };
        if let Err(errno) = tx_subsystems::vfs::xattr::check_xattr_write_perm(&meta, cred) {
            return StepOutcome::err(errno.into());
        }

        let create = flags & tx_subsystems::vfs::xattr::XATTR_CREATE != 0;
        let replace = flags & tx_subsystems::vfs::xattr::XATTR_REPLACE != 0;
        match self.set_xattr_on_disk(inode, name, value, create, replace) {
            Ok(true) => {
                self.invalidate_inode_meta(inode);
                StepOutcome::done(())
            }
            Ok(false) if create => StepOutcome::err(Errno::EEXIST.into()),
            Ok(false) => StepOutcome::err(Errno::ENODATA.into()),
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn remove_xattr(
        &self,
        fs_object_id: FsObjectId,
        name: &[u8],
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        if let Err(errno) = tx_subsystems::vfs::xattr::validate_xattr_name(name) {
            return StepOutcome::err(errno.into());
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let meta = match self.inode_meta_cached(inode) {
            Ok(meta) => map_inode_meta(meta),
            Err(err) => return StepOutcome::err(err.into()),
        };
        if let Err(errno) = tx_subsystems::vfs::xattr::check_xattr_write_perm(&meta, cred) {
            return StepOutcome::err(errno.into());
        }

        match self.remove_xattr_on_disk(inode, name) {
            Ok(true) => {
                self.invalidate_inode_meta(inode);
                StepOutcome::done(())
            }
            Ok(false) => StepOutcome::err(Errno::ENODATA.into()),
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
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let meta = match self.inode_meta_cached(inode) {
            Ok(meta) => meta,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let inode_meta = map_inode_meta(meta);
        if let Err(errno) = tx_subsystems::vfs::predicates::check_chmod_perm(&inode_meta, cred) {
            return StepOutcome::err(errno.into());
        }
        let mut updated = meta;
        updated.mode = (meta.mode & 0xF000) | (new_mode & 0o7777);
        match self.with_pager(|pager| pager.write_inode_meta_journaled(inode, updated)) {
            Ok(_) => {
                self.invalidate_inode_meta(inode);
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }

    fn step_chown(
        &self,
        fs_object_id: FsObjectId,
        new_uid: Option<u32>,
        new_gid: Option<u32>,
        cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }
        let inode = match inode_no(fs_object_id) {
            Ok(inode) => inode,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let meta = match self.inode_meta_cached(inode) {
            Ok(meta) => meta,
            Err(err) => return StepOutcome::err(err.into()),
        };
        let inode_meta = map_inode_meta(meta);
        if let Err(errno) =
            tx_subsystems::vfs::predicates::check_chown_perm(&inode_meta, new_uid, new_gid, cred)
        {
            return StepOutcome::err(errno.into());
        }
        let mut updated = meta;
        if let Some(uid) = new_uid {
            updated.uid = uid;
        }
        if let Some(gid) = new_gid {
            updated.gid = gid;
        }
        let privileged = cred.uid == 0
            || cred
                .effective_caps
                .contains(tx_subsystems::cred::Capability::FOWNER);
        if !privileged {
            updated.mode &= !(0o4000 | 0o2000);
        }
        match self.with_pager(|pager| pager.write_inode_meta_journaled(inode, updated)) {
            Ok(_) => {
                self.invalidate_inode_meta(inode);
                StepOutcome::done(())
            }
            Err(err) => StepOutcome::err(err.into()),
        }
    }
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
