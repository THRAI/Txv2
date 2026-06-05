use core::cell::UnsafeCell;
use core::convert::TryFrom;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::adapter::step_engine::{Cap, PayloadCap, SpinMutex};
use alloc::sync::Arc;
use alloc::vec::Vec;
use tx_ext4_format::pager::{BlockImage, DirEntryLite, Ext4Pager, InodeMetaLite, InodeNo};
use tx_ext4_format::Ext4FormatError;
use tx_subsystems::execution::Errno;
use tx_subsystems::mount::{MountPayload, MountPayloadPin};
use tx_subsystems::vfs::structure::DirCursor;
use tx_subsystems::vfs::structure::{FsObjectId, InodeMeta, Timespec};

pub(crate) const EXT4_ROOT_INODE: u32 = 2;
pub(crate) const READDIR_WINDOW_ENTRIES: usize = 64;

pub(crate) struct Ext4FsInstance<I> {
    pager: Ext4PagerCell<I>,
    lookup_cache: SpinMutex<LookupCache>,
    dir_cache: SpinMutex<DirCache>,
    inode_meta_cache: SpinMutex<InodeMetaCache>,
    pub(crate) mount_pin: SpinMutex<Option<MountPayloadPin>>,
    /// Per-mount read-only flag. When `true`, every mutating
    /// `FsOps` method (`create_inode`, `mkdir`, `unlink`, …) and
    /// every page-cache writeback rejects with `EROFS`. The flag is
    /// set at mount time by `mount_ext4_read_only`; the public
    /// `mount_ext4_read_write` entry point clears it. Matches
    /// Linux's `MS_RDONLY` semantics.
    read_only: AtomicBool,
}

impl<I: BlockImage> Ext4FsInstance<I> {
    pub(crate) fn open(image: I, read_only: bool) -> Result<Arc<Self>, Errno> {
        Ok(Arc::new(Self {
            pager: Ext4PagerCell::new(Ext4Pager::open(image).map_err(map_format_error)?),
            lookup_cache: SpinMutex::new(LookupCache::empty()),
            dir_cache: SpinMutex::new(DirCache::empty()),
            inode_meta_cache: SpinMutex::new(InodeMetaCache::empty()),
            mount_pin: SpinMutex::new(None),
            read_only: AtomicBool::new(read_only),
        }))
    }

    pub(crate) fn bind_mount_payload(&self, payload: &Cap<MountPayload>) {
        let payload = PayloadCap::from_cap(payload.clone());
        *self.mount_pin.lock() = Some(MountPayloadPin::acquire(&payload));
    }

    /// Returns `true` when this mount was opened with `MS_RDONLY`
    /// (or via `mount_ext4_read_only`). Mutating `FsOps` methods
    /// consult this and short-circuit with `EROFS`.
    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::Acquire)
    }

    pub(crate) fn with_pager<T>(
        &self,
        f: impl FnOnce(&mut Ext4Pager<I>) -> tx_ext4_format::Result<T>,
    ) -> Result<T, Errno> {
        let mut pager = self.pager.lock();
        f(&mut pager).map_err(map_format_error)
    }

    pub(crate) fn lookup_cached(
        &self,
        parent: InodeNo,
        name: &[u8],
    ) -> Result<Option<InodeNo>, Errno> {
        if let Some(inode) = self.lookup_cache.lock().get(parent, name) {
            return Ok(Some(inode));
        }
        if let Some(cached) = self.dir_cache.lock().lookup(parent, name) {
            if let Some(inode) = cached {
                self.lookup_cache.lock().insert(parent, name, inode);
            }
            return Ok(cached);
        }
        let found = self.with_pager(|pager| pager.lookup(parent, name))?;
        if let Some(inode) = found {
            self.lookup_cache.lock().insert(parent, name, inode);
        }
        Ok(found)
    }

    pub(crate) fn inode_meta_cached(&self, inode: InodeNo) -> Result<InodeMetaLite, Errno> {
        if let Some(meta) = self.inode_meta_cache.lock().get(inode) {
            return Ok(meta);
        }
        let meta = self.with_pager(|pager| pager.inode_meta(inode))?;
        if inode_meta_is_dir(meta) {
            self.inode_meta_cache.lock().insert(inode, meta);
        }
        Ok(meta)
    }

    pub(crate) fn read_dir_entries_cached(
        &self,
        inode: InodeNo,
        out: &mut [DirEntryLite; READDIR_WINDOW_ENTRIES],
    ) -> Result<usize, Errno> {
        if let Some(count) = self.dir_cache.lock().get(inode, out) {
            return Ok(count);
        }

        let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
        let count = self.with_pager(|pager| pager.read_dir_entries(inode, &mut entries))?;
        self.dir_cache.lock().insert(inode, &entries, count);
        {
            let mut lookup_cache = self.lookup_cache.lock();
            for entry in entries.iter().take(count) {
                lookup_cache.insert(inode, entry.name(), entry.inode);
            }
        }
        out[..count].copy_from_slice(&entries[..count]);
        Ok(count)
    }

    pub(crate) fn invalidate_lookup_cache_for(&self, parent: InodeNo) {
        self.lookup_cache.lock().invalidate_parent(parent);
        self.dir_cache.lock().invalidate(parent);
        self.inode_meta_cache.lock().invalidate(parent);
    }
}

fn inode_meta_is_dir(meta: InodeMetaLite) -> bool {
    meta.mode & 0xF000 == 0x4000
}

const DIR_CACHE_ENTRIES: usize = 32;

struct DirCache {
    clock: u64,
    entries: Vec<DirCacheEntry>,
}

impl DirCache {
    fn empty() -> Self {
        Self {
            clock: 0,
            entries: Vec::new(),
        }
    }

    fn get(
        &mut self,
        inode: InodeNo,
        out: &mut [DirEntryLite; READDIR_WINDOW_ENTRIES],
    ) -> Option<usize> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.valid && entry.inode == inode)?;
        self.clock = self.clock.wrapping_add(1);
        let entry = &mut self.entries[index];
        entry.last_used = self.clock;
        out[..entry.count].copy_from_slice(&entry.entries[..entry.count]);
        Some(entry.count)
    }

    fn lookup(&mut self, inode: InodeNo, name: &[u8]) -> Option<Option<InodeNo>> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.valid && entry.inode == inode)?;
        self.clock = self.clock.wrapping_add(1);
        let entry = &mut self.entries[index];
        entry.last_used = self.clock;
        for dir_entry in entry.entries.iter().take(entry.count) {
            if dir_entry.name() == name {
                return Some(Some(dir_entry.inode));
            }
        }
        if entry.count < READDIR_WINDOW_ENTRIES {
            Some(None)
        } else {
            None
        }
    }

    fn insert(
        &mut self,
        inode: InodeNo,
        entries: &[DirEntryLite; READDIR_WINDOW_ENTRIES],
        count: usize,
    ) {
        self.clock = self.clock.wrapping_add(1);
        let victim = self
            .entries
            .iter()
            .position(|entry| !entry.valid || entry.inode == inode)
            .unwrap_or_else(|| {
                if self.entries.len() < DIR_CACHE_ENTRIES {
                    self.entries.push(DirCacheEntry::empty());
                    self.entries.len() - 1
                } else {
                    self.entries
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, entry)| entry.last_used)
                        .map(|(index, _)| index)
                        .unwrap_or(0)
                }
            });
        let entry = &mut self.entries[victim];
        entry.valid = true;
        entry.inode = inode;
        entry.count = count.min(READDIR_WINDOW_ENTRIES);
        entry.last_used = self.clock;
        entry.entries.clear();
        entry.entries.extend_from_slice(&entries[..entry.count]);
    }

    fn invalidate(&mut self, inode: InodeNo) {
        for entry in &mut self.entries {
            if entry.valid && entry.inode == inode {
                entry.valid = false;
            }
        }
    }
}

struct DirCacheEntry {
    valid: bool,
    inode: InodeNo,
    count: usize,
    entries: Vec<DirEntryLite>,
    last_used: u64,
}

impl DirCacheEntry {
    fn empty() -> Self {
        Self {
            valid: false,
            inode: InodeNo::new(0),
            count: 0,
            entries: Vec::new(),
            last_used: 0,
        }
    }
}

const INODE_META_CACHE_ENTRIES: usize = 64;

struct InodeMetaCache {
    clock: u64,
    entries: [InodeMetaCacheEntry; INODE_META_CACHE_ENTRIES],
}

impl InodeMetaCache {
    const fn empty() -> Self {
        Self {
            clock: 0,
            entries: [InodeMetaCacheEntry::empty(); INODE_META_CACHE_ENTRIES],
        }
    }

    fn get(&mut self, inode: InodeNo) -> Option<InodeMetaLite> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.valid && entry.inode == inode)?;
        self.clock = self.clock.wrapping_add(1);
        self.entries[index].last_used = self.clock;
        Some(self.entries[index].meta)
    }

    fn insert(&mut self, inode: InodeNo, meta: InodeMetaLite) {
        self.clock = self.clock.wrapping_add(1);
        let victim = self
            .entries
            .iter()
            .position(|entry| !entry.valid || entry.inode == inode)
            .unwrap_or_else(|| {
                self.entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(index, _)| index)
                    .unwrap_or(0)
            });
        self.entries[victim] = InodeMetaCacheEntry {
            valid: true,
            inode,
            meta,
            last_used: self.clock,
        };
    }

    fn invalidate(&mut self, inode: InodeNo) {
        for entry in &mut self.entries {
            if entry.valid && entry.inode == inode {
                entry.valid = false;
            }
        }
    }
}

#[derive(Clone, Copy)]
struct InodeMetaCacheEntry {
    valid: bool,
    inode: InodeNo,
    meta: InodeMetaLite,
    last_used: u64,
}

impl InodeMetaCacheEntry {
    const fn empty() -> Self {
        Self {
            valid: false,
            inode: InodeNo::new(0),
            meta: InodeMetaLite {
                mode: 0,
                uid: 0,
                gid: 0,
                size: 0,
                nlinks: 0,
                blocks_512: 0,
                flags: 0,
                atime: 0,
                ctime: 0,
                mtime: 0,
            },
            last_used: 0,
        }
    }
}

const LOOKUP_CACHE_ENTRIES: usize = 64;
const LOOKUP_CACHE_NAME_BYTES: usize = 96;

struct LookupCache {
    clock: u64,
    entries: [LookupCacheEntry; LOOKUP_CACHE_ENTRIES],
}

impl LookupCache {
    const fn empty() -> Self {
        Self {
            clock: 0,
            entries: [LookupCacheEntry::empty(); LOOKUP_CACHE_ENTRIES],
        }
    }

    fn get(&mut self, parent: InodeNo, name: &[u8]) -> Option<InodeNo> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.matches(parent, name))?;
        self.clock = self.clock.wrapping_add(1);
        self.entries[index].last_used = self.clock;
        Some(self.entries[index].inode)
    }

    fn insert(&mut self, parent: InodeNo, name: &[u8], inode: InodeNo) {
        if name.len() > LOOKUP_CACHE_NAME_BYTES {
            return;
        }
        self.clock = self.clock.wrapping_add(1);
        let victim = self
            .entries
            .iter()
            .position(|entry| !entry.valid)
            .unwrap_or_else(|| {
                self.entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(index, _)| index)
                    .unwrap_or(0)
            });
        self.entries[victim] = LookupCacheEntry::new(parent, name, inode, self.clock);
    }

    fn invalidate_parent(&mut self, parent: InodeNo) {
        for entry in &mut self.entries {
            if entry.valid && entry.parent == parent {
                entry.valid = false;
            }
        }
    }
}

#[derive(Clone, Copy)]
struct LookupCacheEntry {
    valid: bool,
    parent: InodeNo,
    inode: InodeNo,
    name_len: u8,
    name: [u8; LOOKUP_CACHE_NAME_BYTES],
    last_used: u64,
}

impl LookupCacheEntry {
    const fn empty() -> Self {
        Self {
            valid: false,
            parent: InodeNo::new(0),
            inode: InodeNo::new(0),
            name_len: 0,
            name: [0; LOOKUP_CACHE_NAME_BYTES],
            last_used: 0,
        }
    }

    fn new(parent: InodeNo, name: &[u8], inode: InodeNo, last_used: u64) -> Self {
        let mut entry = Self::empty();
        entry.valid = true;
        entry.parent = parent;
        entry.inode = inode;
        entry.name_len = name.len() as u8;
        entry.name[..name.len()].copy_from_slice(name);
        entry.last_used = last_used;
        entry
    }

    fn matches(&self, parent: InodeNo, name: &[u8]) -> bool {
        self.valid
            && self.parent == parent
            && self.name_len as usize == name.len()
            && &self.name[..name.len()] == name
    }
}

struct Ext4PagerCell<I> {
    locked: AtomicBool,
    pager: UnsafeCell<Ext4Pager<I>>,
}

unsafe impl<I: Send> Sync for Ext4PagerCell<I> {}

impl<I> Ext4PagerCell<I> {
    fn new(pager: Ext4Pager<I>) -> Self {
        Self {
            locked: AtomicBool::new(false),
            pager: UnsafeCell::new(pager),
        }
    }

    fn lock(&self) -> Ext4PagerGuard<'_, I> {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        Ext4PagerGuard { cell: self }
    }
}

struct Ext4PagerGuard<'a, I> {
    cell: &'a Ext4PagerCell<I>,
}

impl<I> Deref for Ext4PagerGuard<'_, I> {
    type Target = Ext4Pager<I>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.cell.pager.get() }
    }
}

impl<I> DerefMut for Ext4PagerGuard<'_, I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.cell.pager.get() }
    }
}

impl<I> Drop for Ext4PagerGuard<'_, I> {
    fn drop(&mut self) {
        self.cell.locked.store(false, Ordering::Release);
    }
}

pub(crate) fn inode_no(fs_object_id: FsObjectId) -> Result<InodeNo, Errno> {
    let raw = u32::try_from(fs_object_id.as_u64()).map_err(|_| Errno::ENOENT)?;
    if raw == 0 {
        return Err(Errno::ENOENT);
    }
    Ok(InodeNo::new(raw))
}

pub(crate) fn fs_object_id(inode: InodeNo) -> FsObjectId {
    FsObjectId::new(inode.get() as u64)
}

pub(crate) fn map_inode_meta(meta: InodeMetaLite) -> InodeMeta {
    InodeMeta {
        mode: meta.mode,
        uid: meta.uid,
        gid: meta.gid,
        size: meta.size,
        atime: timespec(meta.atime),
        mtime: timespec(meta.mtime),
        ctime: timespec(meta.ctime),
        nlinks: meta.nlinks,
        blocks: meta.blocks_512,
        flags: meta.flags,
    }
}

pub(crate) fn cursor_index(cursor: DirCursor) -> Result<usize, Errno> {
    if cursor.0[8..].iter().any(|byte| *byte != 0) {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&cursor.0[..8]);
    usize::try_from(u64::from_le_bytes(raw)).map_err(|_| Errno::EINVAL)
}

pub(crate) fn cursor_from_index(index: usize) -> DirCursor {
    let mut raw = [0u8; 16];
    raw[..8].copy_from_slice(&(index as u64).to_le_bytes());
    DirCursor(raw)
}

pub(crate) fn map_format_error(err: Ext4FormatError) -> Errno {
    match err {
        Ext4FormatError::BadMagic | Ext4FormatError::Corrupt | Ext4FormatError::Truncated => {
            Errno::EINVAL
        }
        Ext4FormatError::OutOfBounds => Errno::ENOENT,
        Ext4FormatError::Unsupported => Errno::ENOSYS,
        Ext4FormatError::WouldBlock => Errno::EAGAIN,
    }
}

fn timespec(sec: u32) -> Timespec {
    Timespec {
        sec: sec as i64,
        nsec: 0,
    }
}
