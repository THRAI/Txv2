//! FsOps implementation for tx-fat.
//!
//! The current read-only FAT surface routes through `FatPager::*`, which
//! returns `Result<T, FatFormatError>` (not `StepOutcome`), so every body
//! here lands on `done` or `err` only — there is no `Advanced` / `Blocked`
//! / `AdvancedThenBlocked` path.

use crate::adapter::step_engine::{Cap, Guard, NoProgress, StepOutcome};
use crate::read_backend::{
    cluster_from_fs_id, cursor_from_cluster_index, is_fat_root, map_cached_meta, unix_to_fat_date,
    unix_to_fat_time, FatFsInstance, FAT_ROOT_CLUSTER_SENTINEL,
};
use alloc::boxed::Box;
use alloc::vec::Vec;
use tx_fat_format::ondisk::ATTR_DIRECTORY;
use tx_fat_format::pager::{BlockImage, DirEntryLite};
use tx_subsystems::execution::Errno;
use tx_subsystems::mount::MountPayload;
use tx_subsystems::page_backed::{PageContainer, PageContainerKind};
use tx_subsystems::vfs::structure::{
    Credential, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta, RNode,
    RNodeBacking,
};

use tx_subsystems::vfs::FsOps;

// ====================================================================
// Factory
// ====================================================================

impl<I> FatFsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    pub(crate) fn fs_ops_arc(self: alloc::sync::Arc<Self>) -> alloc::sync::Arc<dyn FsOps> {
        self
    }
}

// ====================================================================
// FsOps impl
// ====================================================================

impl<I> FsOps for FatFsInstance<I>
where
    I: BlockImage + Send + 'static,
{
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        let parent_cluster = cluster_from_fs_id(parent);

        let entries = if is_fat_root(parent) {
            match self.with_pager(|p| p.read_root_dir_entries()) {
                Ok(e) => e,
                Err(err) => return StepOutcome::err(err.into()),
            }
        } else {
            match self.with_pager(|p| p.read_dir_entries(parent_cluster)) {
                Ok(e) => e,
                Err(err) => return StepOutcome::err(err.into()),
            }
        };

        for (entry_idx, entry) in entries.iter().enumerate() {
            let entry_name = entry.display_name();
            if !name_eq(entry_name, name) {
                continue;
            }

            // "." — return the current directory's own FsObjectId.
            if entry_name == b"." {
                return StepOutcome::done(parent);
            }

            // ".." — return the parent directory's FsObjectId.
            // FAT stores the parent cluster in the ".." dirent
            // (0 for the root's parent).
            if entry_name == b".." {
                let parent_cluster = entry.first_cluster;
                if parent_cluster == 0 {
                    // Root's parent is root itself.
                    return StepOutcome::done(parent);
                }
                // Encode root sentinel for FAT12/16 root, or the
                // actual cluster for FAT32 subdirectories.
                let parent_fs_id = if is_fat_root(parent) {
                    // Navigating up from a FAT12/16 root: the
                    // parent cluster is 0, already handled above.
                    parent
                } else {
                    // The parent might be the root (FAT12/16) or
                    // another subdirectory.  Check by comparing
                    // parent_cluster against the root sentinel
                    // heuristic: if the current parent's parent is
                    // root, parent_cluster might be a real
                    // cluster.  Since we don't have the BPB here,
                    // encode as a regular cluster.  The VFS walker
                    // will re-enter lookup if it needs to go
                    // further up.
                    crate::read_backend::fs_object_id(parent_cluster, 0)
                };
                return StepOutcome::done(parent_fs_id);
            }

            // Regular entry
            let child_cluster = entry.first_cluster;
            let fs_id = crate::read_backend::fs_object_id(child_cluster, entry_idx as u32);

            // Cache the dirent metadata including parent info for
            // serialize_inode_meta / truncate.
            self.dirent_cache
                .lock()
                .insert(fs_id, entry, parent_cluster, entry_idx as u32);

            return StepOutcome::done(fs_id);
        }

        StepOutcome::err(Errno::ENOENT.into())
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress> {
        // Root directory — return synthesized meta.
        if is_fat_root(fs_object_id) {
            return StepOutcome::done(InodeMeta {
                mode: 0o555 | 0o040000,
                uid: 0,
                gid: 0,
                size: 0,
                atime: Default::default(),
                mtime: Default::default(),
                ctime: Default::default(),
                nlinks: 1,
                blocks: 0,
                flags: 0,
            });
        }

        // Check the dirent cache populated by lookup / readdir.
        if let Some(cached) = self.dirent_cache.lock().get(fs_object_id) {
            return StepOutcome::done(map_cached_meta(&cached));
        }

        // Cache miss — best-effort heuristic for non-cached entries.
        // A directory's first cluster likely starts with a "." entry.
        let cluster = cluster_from_fs_id(fs_object_id);
        let is_dir = match self.with_pager(|p| {
            let mut buf = [0u8; 512];
            p.read_cluster(cluster, &mut buf)?;
            Ok(buf[11] & ATTR_DIRECTORY != 0)
        }) {
            Ok(v) => v,
            Err(err) => return StepOutcome::err(err.into()),
        };

        StepOutcome::done(InodeMeta {
            mode: if is_dir {
                0o555 | 0o040000
            } else {
                0o444 | 0o100000
            },
            uid: 0,
            gid: 0,
            size: 0,
            atime: Default::default(),
            mtime: Default::default(),
            ctime: Default::default(),
            nlinks: 1,
            blocks: 0,
            flags: 0,
        })
    }

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        // Root directory metadata is synthetic — nothing to persist.
        if is_fat_root(fs_object_id) {
            return StepOutcome::done(());
        }

        // Look up the cached dirent to find the parent directory.
        let cached = match self.dirent_cache.lock().get(fs_object_id) {
            Some(c) => c,
            None => return StepOutcome::err(Errno::ENOENT.into()),
        };

        let fat_date = unix_to_fat_date(meta.mtime.sec);
        let fat_time = unix_to_fat_time(meta.mtime.sec);
        let new_size = meta.size as u32;

        let parent_cluster = cached.parent_cluster;
        let target_cluster = cached.first_cluster;

        let result = self.with_pager(|p| {
            if parent_cluster == FAT_ROOT_CLUSTER_SENTINEL {
                p.update_dirent_in_root(target_cluster, new_size, fat_date, fat_time)
            } else {
                p.update_dirent_in_subdir(
                    parent_cluster,
                    target_cluster,
                    new_size,
                    fat_date,
                    fat_time,
                )
            }
        });

        match result {
            Ok(()) => {
                // Update the cache with the new values.
                let mut cache = self.dirent_cache.lock();
                if let Some(mut c) = cache.get(fs_object_id) {
                    c.size = new_size;
                    c.write_date = fat_date;
                    c.write_time = fat_time;
                    cache.insert(
                        fs_object_id,
                        &DirEntryLite {
                            short_name: [0; 11], // dummy — only meta fields matter
                            lfn_utf8: Vec::new(),
                            attr: c.attr,
                            first_cluster: c.first_cluster,
                            size: new_size,
                            write_date: fat_date,
                            write_time: fat_time,
                        },
                        c.parent_cluster,
                        c.entry_index,
                    );
                }
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
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }

        let parent_cluster = cluster_from_fs_id(parent);
        let short_name = build_short_name(name);
        let is_dir = mode & 0o170000 == 0o040000;
        let attr: u8 = if is_dir {
            ATTR_DIRECTORY
        } else {
            0x20 // archive
        };

        // Allocate a cluster.
        let cluster = match self.with_pager(|p| p.alloc_cluster()) {
            Ok(c) => c,
            Err(err) => return StepOutcome::err(err.into()),
        };

        // For directories, create "." and ".." entries in the new cluster.
        if is_dir {
            let result = self.with_pager(|p| {
                let cluster_size = p.bpb.bytes_per_cluster() as usize;
                let mut buf = alloc::vec![0u8; cluster_size];

                // "." entry.
                let dot_name = build_dot_name();
                let dot_cluster_lo = (cluster & 0xFFFF) as u16;
                let dot_cluster_hi = ((cluster >> 16) & 0xFFFF) as u16;
                buf[0..11].copy_from_slice(&dot_name);
                buf[11] = ATTR_DIRECTORY;
                buf[26..28].copy_from_slice(&dot_cluster_lo.to_le_bytes());
                buf[20..22].copy_from_slice(&dot_cluster_hi.to_le_bytes());

                // ".." entry.
                let dotdot_name = build_dotdot_name();
                let parent_cluster_val = if is_fat_root(parent) {
                    0
                } else {
                    parent_cluster
                };
                let parent_lo = (parent_cluster_val & 0xFFFF) as u16;
                let parent_hi = ((parent_cluster_val >> 16) & 0xFFFF) as u16;
                buf[32..43].copy_from_slice(&dotdot_name);
                buf[32 + 11] = ATTR_DIRECTORY;
                buf[32 + 26..32 + 28].copy_from_slice(&parent_lo.to_le_bytes());
                buf[32 + 20..32 + 22].copy_from_slice(&parent_hi.to_le_bytes());

                p.write_cluster(cluster, &buf)
            });
            if let Err(err) = result {
                return StepOutcome::err(err.into());
            }
        }

        // Append the dirent to the parent directory.
        // Use current time (0 = 1980-01-01 for now; hook into RTC later).
        let now_date = unix_to_fat_date(0);
        let now_time = unix_to_fat_time(0);
        let entry_index = if is_fat_root(parent) {
            match self.with_pager(|p| {
                p.append_dirent_in_root(&short_name, attr, cluster, 0, now_date, now_time)
            }) {
                Ok(idx) => idx,
                Err(err) => return StepOutcome::err(err.into()),
            }
        } else {
            match self.with_pager(|p| {
                p.append_dirent_in_subdir(
                    parent_cluster,
                    &short_name,
                    attr,
                    cluster,
                    0,
                    now_date,
                    now_time,
                )
            }) {
                Ok(idx) => idx,
                Err(err) => return StepOutcome::err(err.into()),
            }
        };

        let fs_id = crate::read_backend::fs_object_id(cluster, entry_index);
        let meta = InodeMeta {
            mode: if is_dir {
                0o555 | 0o040000
            } else {
                mode & 0o777 | 0o100000
            },
            uid: 0,
            gid: 0,
            size: 0,
            atime: Default::default(),
            mtime: Default::default(),
            ctime: Default::default(),
            nlinks: 1,
            blocks: 0,
            flags: 0,
        };

        // Cache the new entry.
        let dummy = DirEntryLite {
            short_name,
            lfn_utf8: Vec::new(),
            attr,
            first_cluster: cluster,
            size: 0,
            write_date: 0,
            write_time: 0,
        };
        self.dirent_cache
            .lock()
            .insert(fs_id, &dummy, parent_cluster, entry_index);

        StepOutcome::done((fs_id, meta))
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }

        // Look up cached dirent for cluster chain and parent info.
        let cached = match self.dirent_cache.lock().get(target) {
            Some(c) => c,
            None => return StepOutcome::err(Errno::ENOENT.into()),
        };

        // Directories must go through rmdir.
        if cached.attr & ATTR_DIRECTORY != 0 {
            return StepOutcome::err(Errno::EISDIR.into());
        }

        // Free the cluster chain.
        if let Err(err) = self.with_pager(|p| p.free_cluster_chain(cached.first_cluster)) {
            return StepOutcome::err(err.into());
        }

        // Delete the dirent.
        let result = if cached.parent_cluster == FAT_ROOT_CLUSTER_SENTINEL {
            self.with_pager(|p| p.delete_dirent_in_root(cached.entry_index))
        } else {
            self.with_pager(|p| {
                p.delete_dirent_in_subdir(cached.parent_cluster, cached.entry_index)
            })
        };

        match result {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(err.into()),
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

        // Look up the old entry.
        let old_entry = match self.lookup(old_parent, old_name, _guard) {
            StepOutcome::Done(fs_id) => fs_id,
            StepOutcome::Err(e) => return StepOutcome::Err(e),
            _ => return StepOutcome::err(Errno::EIO.into()),
        };

        let cached = match self.dirent_cache.lock().get(old_entry) {
            Some(c) => c,
            None => return StepOutcome::err(Errno::ENOENT.into()),
        };

        let old_parent_cluster = cached.parent_cluster;
        let old_entry_index = cached.entry_index;
        let target_cluster = cached.first_cluster;

        // Same directory: just update the name in-place.
        if old_parent == new_parent {
            let new_short = build_short_name(new_name);
            let result = if is_fat_root(old_parent) {
                self.with_pager(|p| p.rename_dirent_in_root(old_entry_index, &new_short))
            } else {
                let parent_cluster = cluster_from_fs_id(old_parent);
                self.with_pager(|p| {
                    p.rename_dirent_in_subdir(parent_cluster, old_entry_index, &new_short)
                })
            };

            match result {
                Ok(()) => {
                    // Update the cached short name.
                    let mut cache = self.dirent_cache.lock();
                    // Re-insert with updated short_name (the rest of
                    // the metadata is unchanged).
                    let dummy = DirEntryLite {
                        short_name: new_short,
                        lfn_utf8: Vec::new(),
                        attr: cached.attr,
                        first_cluster: cached.first_cluster,
                        size: cached.size,
                        write_date: cached.write_date,
                        write_time: cached.write_time,
                    };
                    cache.insert(old_entry, &dummy, old_parent_cluster, old_entry_index);
                    StepOutcome::done(())
                }
                Err(err) => StepOutcome::err(err.into()),
            }
        } else {
            // Cross-directory rename: create entry in new parent,
            // then delete entry from old parent.
            let new_short = build_short_name(new_name);
            let new_parent_cluster = cluster_from_fs_id(new_parent);

            // Create the new dirent first (if this fails, we haven't
            // touched the old one yet and can return an error safely).
            let result: core::result::Result<(), Errno> = (|| {
                let now_date = unix_to_fat_date(0);
                let now_time = unix_to_fat_time(0);
                let _new_entry_index = if is_fat_root(new_parent) {
                    self.with_pager(|p| {
                        p.append_dirent_in_root(
                            &new_short,
                            cached.attr,
                            target_cluster,
                            cached.size,
                            now_date,
                            now_time,
                        )
                    })?
                } else {
                    self.with_pager(|p| {
                        p.append_dirent_in_subdir(
                            new_parent_cluster,
                            &new_short,
                            cached.attr,
                            target_cluster,
                            cached.size,
                            now_date,
                            now_time,
                        )
                    })?
                };

                // Delete the old dirent.
                if old_parent_cluster == FAT_ROOT_CLUSTER_SENTINEL {
                    self.with_pager(|p| p.delete_dirent_in_root(old_entry_index))?;
                } else {
                    self.with_pager(|p| {
                        p.delete_dirent_in_subdir(old_parent_cluster, old_entry_index)
                    })?;
                }

                Ok(())
            })();

            match result {
                Ok(()) => StepOutcome::done(()),
                Err(err) => StepOutcome::err(err.into()),
            }
        }
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        // FAT does not support hard links.
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        // mkdir is create_inode with S_IFDIR set.
        let dir_mode = mode | 0o040000; // ensure S_IFDIR
        self.create_inode(parent, name, dir_mode, cred, guard)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if self.is_read_only() {
            return StepOutcome::err(Errno::EROFS.into());
        }

        let cached = match self.dirent_cache.lock().get(target) {
            Some(c) => c,
            None => return StepOutcome::err(Errno::ENOENT.into()),
        };

        // Must be a directory.
        if cached.attr & ATTR_DIRECTORY == 0 {
            return StepOutcome::err(Errno::ENOTDIR.into());
        }

        // Check that the directory is empty (only "." and "..").
        let dir_cluster = cached.first_cluster;
        let is_empty = match self.with_pager(|p| {
            let entries = p.read_dir_entries(dir_cluster)?;
            Ok(entries
                .iter()
                .filter(|e| {
                    let name = e.display_name();
                    !is_dot_or_dotdot(name) && e.attr & 0x08 == 0 // skip volume labels
                })
                .count()
                == 0)
        }) {
            Ok(v) => v,
            Err(err) => return StepOutcome::err(err.into()),
        };

        if !is_empty {
            return StepOutcome::err(Errno::ENOTEMPTY.into());
        }

        // Free the cluster chain.
        if let Err(err) = self.with_pager(|p| p.free_cluster_chain(dir_cluster)) {
            return StepOutcome::err(err.into());
        }

        // Delete the dirent.
        let result = if cached.parent_cluster == FAT_ROOT_CLUSTER_SENTINEL {
            self.with_pager(|p| p.delete_dirent_in_root(cached.entry_index))
        } else {
            self.with_pager(|p| {
                p.delete_dirent_in_subdir(cached.parent_cluster, cached.entry_index)
            })
        };

        match result {
            Ok(()) => StepOutcome::done(()),
            Err(err) => StepOutcome::err(err.into()),
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
        StepOutcome::err(Errno::ENOSYS.into()) // FAT does not support symlinks
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        use tx_fat_format::ondisk::ATTR_VOLUME_ID;

        let cluster = cluster_from_fs_id(fs_object_id);

        let entries = if is_fat_root(fs_object_id) {
            match self.with_pager(|p| p.read_root_dir_entries()) {
                Ok(e) => e,
                Err(err) => return StepOutcome::err(err.into()),
            }
        } else {
            match self.with_pager(|p| p.read_dir_entries(cluster)) {
                Ok(e) => e,
                Err(err) => return StepOutcome::err(err.into()),
            }
        };

        // Filter out volume labels and dot/dotdot entries
        let visible: Vec<&DirEntryLite> = entries
            .iter()
            .filter(|e| e.attr & ATTR_VOLUME_ID == 0 && !is_dot_or_dotdot(e.display_name()))
            .collect();

        let index = {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&cursor.0[4..12]);
            u64::from_le_bytes(raw) as usize
        };

        if index >= visible.len() {
            return StepOutcome::done(None);
        }

        let entry = visible[index];
        let name = match InlineName::new(entry.display_name()) {
            Ok(n) => n,
            Err(err) => return StepOutcome::err(err.into()),
        };

        let kind = if entry.attr & ATTR_DIRECTORY != 0 {
            InodeKind::Directory
        } else {
            InodeKind::Regular
        };

        let child_cluster = entry.first_cluster;
        // Use the visible index as the entry offset so load_inode_meta
        // can find the cached dirent.
        let fs_id = crate::read_backend::fs_object_id(child_cluster, index as u32);

        // Cache the dirent including parent info for serialize / truncate.
        self.dirent_cache
            .lock()
            .insert(fs_id, entry, cluster, index as u32);

        StepOutcome::done(Some((
            DirEntry {
                name,
                fs_object_id: fs_id,
                kind,
            },
            cursor_from_cluster_index(cluster, index + 1),
        )))
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
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
        let page_count = meta.size.div_ceil(PAGE_SIZE).max(1);
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
        pc.set_size_bytes_persisted(meta.size);

        match RNode::new_cap_in_mount(fs_object_id, meta, RNodeBacking::PageBacked { pc }, mount) {
            Ok(rnode) => StepOutcome::done(rnode),
            Err(_) => StepOutcome::err(Errno::ENOMEM.into()),
        }
    }

    fn read_link(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Box<[u8]>, NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into()) // FAT does not support symlinks
    }
}

// ====================================================================
// Helpers
// ====================================================================

fn name_eq(entry_name: &[u8], query: &[u8]) -> bool {
    if entry_name.len() != query.len() {
        return false;
    }
    // Case-insensitive comparison (FAT names are case-insensitive)
    entry_name
        .iter()
        .zip(query.iter())
        .all(|(&a, &b)| a.eq_ignore_ascii_case(&b))
}

fn is_dot_or_dotdot(name: &[u8]) -> bool {
    name == b"." || name == b".."
}

/// Build an 8.3 short name from a user-provided filename.
///
/// Splits the name at the last '.', converts to uppercase, removes
/// invalid FAT characters, and pads/truncates to 8.3.
fn build_short_name(name: &[u8]) -> [u8; 11] {
    let mut short = [b' '; 11];

    // Find the last '.' for the extension.
    let dot_pos = name.iter().rposition(|&b| b == b'.');
    let (base, ext) = match dot_pos {
        Some(pos) => (&name[..pos], &name[pos + 1..]),
        None => (name, &[][..]),
    };

    // Copy base name (up to 8 chars).
    let mut bi = 0;
    for &b in base {
        if bi >= 8 {
            break;
        }
        let ch = to_valid_fat_char(b);
        if ch != 0 {
            short[bi] = ch;
            bi += 1;
        }
    }

    // Copy extension (up to 3 chars).
    let mut ei = 0;
    for &b in ext {
        if ei >= 3 {
            break;
        }
        let ch = to_valid_fat_char(b);
        if ch != 0 {
            short[8 + ei] = ch;
            ei += 1;
        }
    }

    short
}

fn build_dot_name() -> [u8; 11] {
    let mut n = [b' '; 11];
    n[0] = b'.';
    n
}

fn build_dotdot_name() -> [u8; 11] {
    let mut n = [b' '; 11];
    n[0] = b'.';
    n[1] = b'.';
    n
}

/// Convert a byte to a valid FAT short-name character (uppercase,
/// alphanumeric + limited symbols, reject others).
fn to_valid_fat_char(b: u8) -> u8 {
    match b {
        b'A'..=b'Z' => b,
        b'a'..=b'z' => b - 32, // uppercase
        b'0'..=b'9' => b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'(' | b')' | b'-' | b'@' | b'^' | b'_'
        | b'`' | b'{' | b'}' | b'~' => b,
        b' ' => 0, // skip spaces
        _ => 0,    // reject
    }
}
