use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::adapter::step_engine::{Cap, Guard, PayloadCap, SpinMutex, Weak};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak as ArcWeak};
use alloc::vec::Vec;
use tx_ext4_format::mutation::{Ext4MutationPlan, FsyncStamp};
use tx_ext4_format::pager::{
    BlockImage, DirEntryLite, Ext4Pager, InodeMetaLite, InodeNo, BLOCK_SIZE,
};
use tx_ext4_format::Ext4FormatError;
use tx_subsystems::execution::Errno;
use tx_subsystems::fs_iface::{BackendPageRequest, BackendPlanner};
use tx_subsystems::mount::{MountPayload, MountPayloadPin};
use tx_subsystems::page_backed::{PageContainer, PageContainerKind};
use tx_subsystems::vfs::structure::DirCursor;
use tx_subsystems::vfs::structure::{FsObjectId, InodeMeta, Timespec};

use crate::journal::Ext4MutationPlanSource;
use crate::planner::Ext4MappingTable;

pub(crate) const EXT4_ROOT_INODE: u32 = 2;
pub(crate) const READDIR_WINDOW_ENTRIES: usize = 64;
const DIR_VERSION_SHARDS: usize = 256;

pub trait FilePageContainerBinder: Send + Sync {
    fn bind_file_page_container(&self, container: Cap<PageContainer>);
}

/// Mount-local adapter from a backend request to the pure format mutation
/// planner. Page data remains owned by L4 and is attached by the journal
/// runtime during admission.
pub(crate) struct Ext4PagerMutationPlanSource<I> {
    backend: Arc<SpinMutex<Option<ArcWeak<Ext4FsInstance<I>>>>>,
}

impl<I> Clone for Ext4PagerMutationPlanSource<I> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
        }
    }
}

impl<I> Ext4PagerMutationPlanSource<I> {
    pub(crate) fn new() -> Self {
        Self {
            backend: Arc::new(SpinMutex::new(None)),
        }
    }

    pub(crate) fn bind(&self, backend: &Arc<Ext4FsInstance<I>>) {
        *self.backend.lock() = Some(Arc::downgrade(backend));
    }
}

impl<I> Ext4MutationPlanSource for Ext4PagerMutationPlanSource<I>
where
    I: BlockImage + Send + 'static,
{
    fn plan_writeback_mutation(
        &self,
        request: &BackendPageRequest,
    ) -> Result<Ext4MutationPlan, Errno> {
        if request.range.page_count() != 1 {
            return Err(Errno::EINVAL);
        }
        let generation = request.generation_hint.ok_or(Errno::EINVAL)?;
        let backend = self
            .backend
            .lock()
            .as_ref()
            .and_then(ArcWeak::upgrade)
            .ok_or(Errno::EIO)?;
        let object = FsObjectId::new(request.object.raw());
        let (inode, _) = backend.resolve_object(object)?;

        backend.with_pager(|pager| {
            pager.plan_write_page(
                inode,
                request.range.start_page(),
                &[0; BLOCK_SIZE],
                FsyncStamp::new(generation.raw()),
            )
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum OrphanState {
    Pending,
    Reclaiming,
}

pub(crate) struct Ext4FsInstance<I> {
    pager: Ext4PagerCell<I>,
    backend_planner: Option<Arc<dyn BackendPlanner>>,
    extent_mapping: Option<Arc<Ext4MappingTable>>,
    lookup_cache: SpinMutex<LookupCache>,
    dir_cache: SpinMutex<DirCache>,
    /// Lock-free namespace generations used to validate lookup/readdir cache
    /// fills against concurrent directory mutations.
    ///
    /// The pager serializes on-disk operations, but cache insertion happens
    /// after that pager guard is released. Without a generation, a reader can
    /// read an old directory, a writer can commit and invalidate, and then the
    /// reader can publish its old snapshot after the invalidation. Hashed
    /// shards avoid a global namespace lock; collisions only cause harmless
    /// extra cache misses.
    dir_versions: [AtomicU64; DIR_VERSION_SHARDS],
    /// Per-inode page-cache coherence index. Every hard-link alias of a
    /// regular inode must materialise the same PageContainer while it is
    /// alive; separate containers would permit stale reads and writeback
    /// through one name to overwrite data written through another.
    page_containers: SpinMutex<BTreeMap<FsObjectId, Weak<PageContainer>>>,
    /// Inodes whose namespace link count reached zero. Entries remain here
    /// while an RNode/OpenFile/mmap/PageContainer can still reach the payload;
    /// the last-payload callback retries `destroy_inode`.
    orphaned_inodes: SpinMutex<BTreeMap<FsObjectId, OrphanState>>,
    pub(crate) mount_pin: SpinMutex<Option<MountPayloadPin>>,
    file_page_container_binder: SpinMutex<Option<Arc<dyn FilePageContainerBinder>>>,
    /// Per-mount read-only flag. When `true`, every mutating
    /// `FsOps` method (`create_inode`, `mkdir`, `unlink`, …) and
    /// every page-cache writeback rejects with `EROFS`. The flag is
    /// set at mount time by `mount_ext4_read_only`; the public
    /// `mount_ext4_read_write` entry point clears it. Matches
    /// Linux's `MS_RDONLY` semantics.
    read_only: AtomicBool,
    /// Journal-backed mounts must not bypass their ordered mutation runtime
    /// through the legacy direct-pager writeback path.
    legacy_writeback_enabled: AtomicBool,
}

impl<I: BlockImage> Ext4FsInstance<I> {
    #[cfg(test)]
    pub(crate) fn open(image: I, read_only: bool) -> Result<Arc<Self>, Errno> {
        Self::open_with_backend_planner_and_mapping(image, read_only, None, None)
    }

    pub(crate) fn open_with_backend_planner_and_mapping(
        image: I,
        read_only: bool,
        backend_planner: Option<Arc<dyn BackendPlanner>>,
        extent_mapping: Option<Arc<Ext4MappingTable>>,
    ) -> Result<Arc<Self>, Errno> {
        Ok(Arc::new(Self {
            pager: Ext4PagerCell::new(Ext4Pager::open(image).map_err(map_format_error)?),
            backend_planner,
            extent_mapping,
            lookup_cache: SpinMutex::new(LookupCache::empty()),
            dir_cache: SpinMutex::new(DirCache::empty()),
            dir_versions: core::array::from_fn(|_| AtomicU64::new(0)),
            page_containers: SpinMutex::new(BTreeMap::new()),
            orphaned_inodes: SpinMutex::new(BTreeMap::new()),
            mount_pin: SpinMutex::new(None),
            file_page_container_binder: SpinMutex::new(None),
            read_only: AtomicBool::new(read_only),
            legacy_writeback_enabled: AtomicBool::new(true),
        }))
    }

    pub(crate) fn backend_planner(&self) -> Option<Arc<dyn BackendPlanner>> {
        self.backend_planner.clone()
    }

    pub(crate) fn bind_mount_payload(&self, payload: &Cap<MountPayload>) {
        let payload = PayloadCap::from_cap(payload.clone());
        *self.mount_pin.lock() = Some(MountPayloadPin::acquire(&payload));
    }

    pub(crate) fn set_file_page_container_binder(
        &self,
        binder: Option<Arc<dyn FilePageContainerBinder>>,
    ) {
        *self.file_page_container_binder.lock() = binder;
    }

    fn bind_file_page_container(&self, container: Cap<PageContainer>) {
        if let Some(binder) = self.file_page_container_binder.lock().clone() {
            binder.bind_file_page_container(container);
        }
    }

    /// Returns `true` when this mount was opened with `MS_RDONLY`
    /// (or via `mount_ext4_read_only`). Mutating `FsOps` methods
    /// consult this and short-circuit with `EROFS`.
    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::Acquire)
    }

    pub(crate) fn disable_legacy_writeback(&self) {
        self.legacy_writeback_enabled
            .store(false, Ordering::Release);
    }

    pub(crate) fn legacy_writeback_enabled(&self) -> bool {
        self.legacy_writeback_enabled.load(Ordering::Acquire)
    }

    pub(crate) fn with_pager<T>(
        &self,
        f: impl FnOnce(&mut Ext4Pager<I>) -> tx_ext4_format::Result<T>,
    ) -> Result<T, Errno> {
        self.with_pager_raw(f).map_err(map_format_error)
    }

    pub(crate) fn with_pager_raw<T>(
        &self,
        f: impl FnOnce(&mut Ext4Pager<I>) -> tx_ext4_format::Result<T>,
    ) -> tx_ext4_format::Result<T> {
        let mut pager = self.pager.lock();
        f(&mut pager)
    }

    pub(crate) fn with_pager_namespace_mutation<T>(
        &self,
        parents: &[FsObjectId],
        f: impl FnOnce(&mut Ext4Pager<I>) -> tx_ext4_format::Result<T>,
    ) -> Result<T, Errno> {
        self.with_pager(|pager| {
            let value = f(pager)?;
            for parent in parents {
                self.bump_dir_version(*parent);
            }
            Ok(value)
        })
    }

    fn dir_version(&self, object: FsObjectId) -> u64 {
        self.dir_versions[dir_version_shard(object)].load(Ordering::Acquire)
    }

    fn bump_dir_version(&self, object: FsObjectId) {
        self.dir_versions[dir_version_shard(object)].fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn lookup_cached(
        &self,
        parent: FsObjectId,
        parent_inode: InodeNo,
        name: &[u8],
    ) -> Result<Option<InodeNo>, Errno> {
        let version = self.dir_version(parent);
        let lookup_cached = {
            let mut cache = self.lookup_cache.lock();
            cache.get(parent, name, version)
        };
        if let Some(cached) = lookup_cached {
            return Ok(cached);
        }

        // Never nest dir_cache and lookup_cache. In Rust 2021 an `if let`
        // scrutinee temporary can live through the body, so spelling this as
        // `if let Some(..) = self.dir_cache.lock().lookup(..)` and then taking
        // lookup_cache in the body creates the reverse of the invalidation
        // order (lookup_cache -> dir_cache). Concurrent readdir + namespace
        // mutation can then deadlock both CPUs. Copy the result out under one
        // lock and release it before touching the other cache.
        let dir_cached = {
            let mut cache = self.dir_cache.lock();
            cache.lookup(parent, name, version)
        };
        if let Some(cached) = dir_cached {
            self.lookup_cache.lock().insert(
                parent,
                name,
                cached.unwrap_or(InodeNo::new(0)),
                version,
            );
            return Ok(cached);
        }
        let (found, version) = self.with_pager(|pager| {
            let found = pager.lookup(parent_inode, name)?;
            Ok((found, self.dir_version(parent)))
        })?;
        // Cache negatives too (inode 0 sentinel): every shell command's PATH
        // search stats mostly-nonexistent names against testcases/bin
        // (~2800 entries); an uncached miss is a full linear directory scan
        // (~48 ms under TCG, measured) repeated for every command.
        self.lookup_cache
            .lock()
            .insert(parent, name, found.unwrap_or(InodeNo::new(0)), version);
        Ok(found)
    }

    /// Resolve and validate a persistent object incarnation.
    ///
    /// An inode bitmap slot can be reused immediately after reclamation.
    /// Every backend entry point therefore validates both the low inode
    /// number and the on-disk generation before touching data or metadata.
    pub(crate) fn resolve_object(
        &self,
        fs_object_id: FsObjectId,
    ) -> Result<(InodeNo, InodeMetaLite), Errno> {
        let inode = inode_no(fs_object_id)?;
        let meta = self.inode_meta_with_mapping(inode)?;
        if meta.mode == 0 {
            return Err(Errno::ENOENT);
        }
        if meta.generation != fs_object_id.inode_generation() {
            return Err(Errno::ESTALE);
        }
        Ok((inode, meta))
    }

    pub(crate) fn object_id_for_inode(&self, inode: InodeNo) -> Result<FsObjectId, Errno> {
        let meta = self.inode_meta_with_mapping(inode)?;
        if meta.mode == 0 {
            return Err(Errno::ENOENT);
        }
        Ok(fs_object_id(inode, meta.generation))
    }

    fn inode_meta_with_mapping(&self, inode: InodeNo) -> Result<InodeMetaLite, Errno> {
        let (meta, extent_root) = match self.extent_mapping.as_ref() {
            Some(_) => self.with_pager(|pager| pager.inode_meta_and_extent_root(inode))?,
            None => (
                self.with_pager(|pager| pager.inode_meta(inode))?,
                Vec::new(),
            ),
        };
        if meta.mode & 0xF000 == 0x8000 {
            if let Some(mapping) = self.extent_mapping.as_ref() {
                mapping.insert_extent_root(inode.get() as u64, &extent_root)?;
            }
        }
        Ok(meta)
    }

    pub(crate) fn read_dir_entries_cached(
        &self,
        fs_object_id: FsObjectId,
        inode: InodeNo,
        start_offset: u64,
        out: &mut [DirEntryLite; READDIR_WINDOW_ENTRIES],
        next_offsets: &mut [u64; READDIR_WINDOW_ENTRIES],
    ) -> Result<usize, Errno> {
        let version = self.dir_version(fs_object_id);
        if let Some(count) =
            self.dir_cache
                .lock()
                .get(fs_object_id, start_offset, version, out, next_offsets)
        {
            return Ok(count);
        }

        let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
        let mut cached_next_offsets = [0u64; READDIR_WINDOW_ENTRIES];
        let (count, version) = self.with_pager(|pager| {
            let count = pager.read_dir_entries_from_offset(
                inode,
                start_offset,
                &mut entries,
                &mut cached_next_offsets,
            )?;
            Ok((count, self.dir_version(fs_object_id)))
        })?;
        self.dir_cache.lock().insert(
            fs_object_id,
            start_offset,
            version,
            &entries,
            &cached_next_offsets,
            count,
        );
        {
            let mut lookup_cache = self.lookup_cache.lock();
            for entry in entries.iter().take(count) {
                lookup_cache.insert(fs_object_id, entry.name(), entry.inode, version);
            }
        }
        out[..count].copy_from_slice(&entries[..count]);
        next_offsets[..count].copy_from_slice(&cached_next_offsets[..count]);
        Ok(count)
    }

    pub(crate) fn invalidate_lookup_cache_for(&self, parent: FsObjectId) {
        self.lookup_cache.lock().invalidate_parent(parent);
        self.dir_cache.lock().invalidate(parent);
    }

    /// Resolve or create the one coherent page cache for `inode`.
    ///
    /// The coherence lock is held while the on-disk inode is revalidated and
    /// the new container is published. `destroy_orphaned_inode` takes the
    /// same lock before freeing/reusing the inode number, closing the race
    /// between lookup metadata and materialisation on another CPU.
    pub(crate) fn get_or_create_page_container(
        &self,
        fs_object_id: FsObjectId,
        mount: MountPayloadPin,
        minimum_page_count: u64,
        guard: &Guard<'_>,
    ) -> Result<(Cap<PageContainer>, InodeMetaLite), Errno> {
        {
            let mut index = self.page_containers.lock();
            if let Some(weak) = index.get(&fs_object_id).copied() {
                if let Some(container) = weak.upgrade(guard) {
                    drop(index);
                    let (_, meta) = self.resolve_object(fs_object_id)?;
                    return Ok((container, meta));
                }
                index.remove(&fs_object_id);
            }
        }

        let (_, meta) = self.resolve_object(fs_object_id)?;
        if meta.nlinks == 0 {
            return Err(Errno::ENOENT);
        }
        let page_count = meta
            .size
            .div_ceil(tx_subsystems::vm::USER_PAGE_SIZE as u64)
            .max(minimum_page_count);
        let container = PageContainer::new_cap(
            PageContainerKind::File {
                mount,
                fs_object_id,
            },
            page_count,
        )
        .map_err(|_| Errno::ENOMEM)?;
        container.set_size_bytes_persisted(meta.size);

        // Publish under the coherence lock, but allocate the candidate before
        // taking it: zone allocation can drain EBR and run a PageContainer
        // finalizer, which may re-enter this instance.
        let orphaned = self.orphaned_inodes.lock();
        if orphaned.contains_key(&fs_object_id) {
            drop(orphaned);
            drop(container);
            return Err(Errno::ENOENT);
        }
        let mut index = self.page_containers.lock();
        if let Some(weak) = index.get(&fs_object_id).copied() {
            if let Some(existing) = weak.upgrade(guard) {
                drop(index);
                drop(orphaned);
                drop(container);
                let (_, latest) = self.resolve_object(fs_object_id)?;
                return Ok((existing, latest));
            }
            index.remove(&fs_object_id);
        }
        let (_, latest) = self.resolve_object(fs_object_id)?;
        if latest.nlinks == 0 {
            drop(index);
            drop(orphaned);
            drop(container);
            return Err(Errno::ENOENT);
        }
        container.set_size_bytes_persisted(latest.size);
        index.insert(fs_object_id, container.downgrade());
        drop(index);
        drop(orphaned);
        self.bind_file_page_container(container.clone());
        Ok((container, latest))
    }

    pub(crate) fn mark_inode_orphaned(&self, fs_object_id: FsObjectId) {
        self.orphaned_inodes
            .lock()
            .insert(fs_object_id, OrphanState::Pending);
    }

    /// Reclaim a zero-link inode only after its coherent PageContainer no
    /// longer has a strong reference. ext4 materialises every inode kind
    /// through this indexed file container, so directories and symlinks use
    /// the same final-payload gate as regular files.
    pub(crate) fn destroy_orphaned_inode(
        &self,
        fs_object_id: FsObjectId,
        guard: &Guard<'_>,
    ) -> Result<(), Errno> {
        let inode = inode_no(fs_object_id)?;
        {
            let mut orphaned = self.orphaned_inodes.lock();
            if orphaned.get(&fs_object_id) != Some(&OrphanState::Pending) {
                return Ok(());
            }
            orphaned.insert(fs_object_id, OrphanState::Reclaiming);
        }

        let current = self.with_pager(|pager| pager.inode_meta(inode));
        match current {
            Ok(meta) if meta.mode != 0 && meta.generation == fs_object_id.inode_generation() => {}
            Ok(_) | Err(Errno::ENOENT) => {
                self.orphaned_inodes.lock().remove(&fs_object_id);
                return Ok(());
            }
            Err(error) => {
                self.orphaned_inodes
                    .lock()
                    .insert(fs_object_id, OrphanState::Pending);
                return Err(error);
            }
        }

        let mut containers = self.page_containers.lock();
        if let Some(weak) = containers.get(&fs_object_id).copied() {
            if let Some(live) = weak.upgrade(guard) {
                drop(containers);
                self.orphaned_inodes
                    .lock()
                    .insert(fs_object_id, OrphanState::Pending);
                // Dropping this temporary reference outside every lock may
                // itself be the last release and re-enter the callback.
                drop(live);
                return Ok(());
            }
            containers.remove(&fs_object_id);
        }
        drop(containers);

        // Extent collection allocates and block I/O can trigger unrelated
        // completion/finalizer work. Never hold orphan/coherence locks here.
        let result = self.with_pager(|pager| pager.destroy_inode(inode));
        let mut orphaned = self.orphaned_inodes.lock();
        if result.is_ok() {
            orphaned.remove(&fs_object_id);
        } else {
            orphaned.insert(fs_object_id, OrphanState::Pending);
        }
        result
    }
}

fn dir_version_shard(object: FsObjectId) -> usize {
    let raw = object.as_u64();
    (raw ^ (raw >> 32)) as usize % DIR_VERSION_SHARDS
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
        object: FsObjectId,
        start_offset: u64,
        version: u64,
        out: &mut [DirEntryLite; READDIR_WINDOW_ENTRIES],
        next_offsets: &mut [u64; READDIR_WINDOW_ENTRIES],
    ) -> Option<usize> {
        let (index, window_index) =
            self.entries.iter().enumerate().find_map(|(index, entry)| {
                if !entry.valid || entry.object != object || entry.version != version {
                    return None;
                }
                if entry.start_offset == start_offset {
                    return Some((index, 0));
                }
                entry
                    .next_offsets
                    .iter()
                    .take(entry.count.saturating_sub(1))
                    .position(|offset| *offset == start_offset)
                    .map(|offset_index| (index, offset_index + 1))
            })?;
        self.clock = self.clock.wrapping_add(1);
        let entry = &mut self.entries[index];
        entry.last_used = self.clock;
        let count = entry.count - window_index;
        out[..count].copy_from_slice(&entry.entries[window_index..entry.count]);
        next_offsets[..count].copy_from_slice(&entry.next_offsets[window_index..entry.count]);
        Some(count)
    }

    fn lookup(&mut self, object: FsObjectId, name: &[u8], version: u64) -> Option<Option<InodeNo>> {
        let mut complete_first_window = None;
        let mut found_index = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if !entry.valid || entry.object != object || entry.version != version {
                continue;
            }
            if entry.start_offset == 0 && entry.count < READDIR_WINDOW_ENTRIES {
                complete_first_window = Some(index);
            }
            if entry
                .entries
                .iter()
                .take(entry.count)
                .any(|dir_entry| dir_entry.name() == name)
            {
                found_index = Some(index);
                break;
            }
        }
        let index = match found_index {
            Some(index) => index,
            None => {
                let index = complete_first_window?;
                self.clock = self.clock.wrapping_add(1);
                self.entries[index].last_used = self.clock;
                return Some(None);
            }
        };
        self.clock = self.clock.wrapping_add(1);
        let entry = &mut self.entries[index];
        entry.last_used = self.clock;
        for dir_entry in entry.entries.iter().take(entry.count) {
            if dir_entry.name() == name {
                return Some(Some(dir_entry.inode));
            }
        }
        None
    }

    fn insert(
        &mut self,
        object: FsObjectId,
        start_offset: u64,
        version: u64,
        entries: &[DirEntryLite; READDIR_WINDOW_ENTRIES],
        next_offsets: &[u64; READDIR_WINDOW_ENTRIES],
        count: usize,
    ) {
        self.clock = self.clock.wrapping_add(1);
        let victim = self
            .entries
            .iter()
            .position(|entry| {
                !entry.valid || (entry.object == object && entry.start_offset == start_offset)
            })
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
        entry.object = object;
        entry.start_offset = start_offset;
        entry.version = version;
        entry.count = count.min(READDIR_WINDOW_ENTRIES);
        entry.last_used = self.clock;
        entry.entries.clear();
        entry.entries.extend_from_slice(&entries[..entry.count]);
        entry.next_offsets.clear();
        entry
            .next_offsets
            .extend_from_slice(&next_offsets[..entry.count]);
    }

    fn invalidate(&mut self, object: FsObjectId) {
        for entry in &mut self.entries {
            if entry.valid && entry.object == object {
                entry.valid = false;
            }
        }
    }
}

struct DirCacheEntry {
    valid: bool,
    object: FsObjectId,
    start_offset: u64,
    version: u64,
    count: usize,
    entries: Vec<DirEntryLite>,
    next_offsets: Vec<u64>,
    last_used: u64,
}

impl DirCacheEntry {
    fn empty() -> Self {
        Self {
            valid: false,
            object: FsObjectId::new(0),
            start_offset: 0,
            version: 0,
            count: 0,
            entries: Vec::new(),
            next_offsets: Vec::new(),
            last_used: 0,
        }
    }
}

// 512 entries: PATH searches alone touch (commands × 7 PATH dirs) names per
// shell test, most of them misses against a ~2800-entry testcases/bin — the
// negative entries below only pay off if the working set fits.
const LOOKUP_CACHE_ENTRIES: usize = 512;
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

    /// `None` = not cached; `Some(None)` = cached-negative (ENOENT);
    /// `Some(Some(ino))` = cached hit. Negative entries reuse `inode == 0`
    /// (ext4 inode numbers start at 1) and are invalidated by the same
    /// `invalidate_parent` calls that cover create/unlink/rename.
    fn get(&mut self, parent: FsObjectId, name: &[u8], version: u64) -> Option<Option<InodeNo>> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.matches(parent, name, version))?;
        self.clock = self.clock.wrapping_add(1);
        self.entries[index].last_used = self.clock;
        let inode = self.entries[index].inode;
        if inode == InodeNo::new(0) {
            Some(None)
        } else {
            Some(Some(inode))
        }
    }

    fn insert(&mut self, parent: FsObjectId, name: &[u8], inode: InodeNo, version: u64) {
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
        self.entries[victim] = LookupCacheEntry::new(parent, name, inode, version, self.clock);
    }

    fn invalidate_parent(&mut self, parent: FsObjectId) {
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
    parent: FsObjectId,
    inode: InodeNo,
    version: u64,
    name_len: u8,
    name: [u8; LOOKUP_CACHE_NAME_BYTES],
    last_used: u64,
}

impl LookupCacheEntry {
    const fn empty() -> Self {
        Self {
            valid: false,
            parent: FsObjectId::new(0),
            inode: InodeNo::new(0),
            version: 0,
            name_len: 0,
            name: [0; LOOKUP_CACHE_NAME_BYTES],
            last_used: 0,
        }
    }

    fn new(parent: FsObjectId, name: &[u8], inode: InodeNo, version: u64, last_used: u64) -> Self {
        let mut entry = Self::empty();
        entry.valid = true;
        entry.parent = parent;
        entry.inode = inode;
        entry.version = version;
        entry.name_len = name.len() as u8;
        entry.name[..name.len()].copy_from_slice(name);
        entry.last_used = last_used;
        entry
    }

    fn matches(&self, parent: FsObjectId, name: &[u8], version: u64) -> bool {
        self.valid
            && self.parent == parent
            && self.version == version
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
    let raw = fs_object_id.inode_number();
    if raw == 0 {
        return Err(Errno::ENOENT);
    }
    Ok(InodeNo::new(raw))
}

pub(crate) fn fs_object_id(inode: InodeNo, generation: u32) -> FsObjectId {
    FsObjectId::from_inode_generation(inode.get(), generation)
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

pub(crate) fn cursor_offset(cursor: DirCursor) -> Result<u64, Errno> {
    if cursor.0[8..].iter().any(|byte| *byte != 0) {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&cursor.0[..8]);
    Ok(u64::from_le_bytes(raw))
}

pub(crate) fn cursor_from_offset(offset: u64) -> DirCursor {
    let mut raw = [0u8; 16];
    raw[..8].copy_from_slice(&offset.to_le_bytes());
    DirCursor(raw)
}

pub(crate) fn map_format_error(err: Ext4FormatError) -> Errno {
    match err {
        Ext4FormatError::BadMagic | Ext4FormatError::Corrupt | Ext4FormatError::Truncated => {
            Errno::EINVAL
        }
        Ext4FormatError::OutOfBounds => Errno::ENOENT,
        Ext4FormatError::Unsupported => Errno::ENOSYS,
        Ext4FormatError::ExtentTreeFull { .. } => Errno::EFBIG,
        Ext4FormatError::WouldBlock => Errno::EAGAIN,
        Ext4FormatError::NotEmpty => Errno::ENOTEMPTY,
        Ext4FormatError::IsDirectory => Errno::EISDIR,
        Ext4FormatError::NotDirectory => Errno::ENOTDIR,
        Ext4FormatError::InvalidInput => Errno::EINVAL,
    }
}

fn timespec(sec: u32) -> Timespec {
    Timespec {
        sec: sec as i64,
        nsec: 0,
    }
}
