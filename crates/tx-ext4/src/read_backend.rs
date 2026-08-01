use core::cell::UnsafeCell;
use core::convert::TryFrom;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::adapter::step_engine::{Cap, Guard, PayloadCap, SpinMutex, Weak as ZoneWeak};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak as ArcWeak};
use alloc::vec;
use alloc::vec::Vec;
use tx_ext4_format::capability::CapabilityProfileHash;
use tx_ext4_format::mutation::{Ext4MutationPlan, FsyncStamp};
use tx_ext4_format::pager::{
    BlockImage, DirEntryLite, Ext4Pager, InodeMetaLite, InodeNo, BLOCK_SIZE,
};
use tx_ext4_format::Ext4FormatError;
use tx_subsystems::execution::Errno;
use tx_subsystems::fs_iface::{BackendPageRequest, BackendPlanner, IoDataSource};
use tx_subsystems::mount::{MountPayload, MountPayloadPin};
use tx_subsystems::page_backed::{PageContainer, PageContainerKind};
use tx_subsystems::vfs::structure::DirCursor;
use tx_subsystems::vfs::structure::{FsObjectId, InodeMeta, Timespec};

use crate::journal::{
    Ext4MutationPlanSource, JournalMutationRuntime, JournalMutationRuntimeError,
    JournalSettlementObserver,
};
use crate::planner::Ext4MappingTable;

pub(crate) const EXT4_ROOT_INODE: u32 = 2;
pub(crate) const READDIR_WINDOW_ENTRIES: usize = 64;

pub trait FilePageContainerBinder: Send + Sync {
    fn bind_file_page_container(&self, container: Cap<PageContainer>);
}

/// Mount-local adapter from a backend request to the pure format mutation
/// planner. It intentionally has no access to page-cache frames; L4 supplies
/// those separately to `JournalMutationRuntime` during admission.
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
        if request.range.is_empty() {
            return Err(Errno::EINVAL);
        }
        let generation = request.generation_hint.ok_or(Errno::EINVAL)?;
        let backend = self
            .backend
            .lock()
            .as_ref()
            .and_then(ArcWeak::upgrade)
            .ok_or(Errno::EIO)?;
        let inode = inode_no(FsObjectId::new(request.object.raw()))?;
        if backend.is_read_only() {
            return Err(Errno::EROFS);
        }
        if backend.metadata_mutation_runtime().is_none() {
            return Err(Errno::EOPNOTSUPP);
        }

        // The runtime replaces this placeholder with the L4-owned source.
        // The format plan therefore remains metadata-only from L5's view.
        if request.range.page_count() == 1 {
            return backend.with_pager(|pager| {
                pager.plan_write_page(
                    inode,
                    request.range.start_page(),
                    &[0; BLOCK_SIZE],
                    FsyncStamp::new(generation.raw()),
                )
            });
        }
        validate_multi_page_write_source(&request.source, request.range.page_count())?;
        let page_count = usize::try_from(request.range.page_count()).map_err(|_| Errno::EINVAL)?;
        let pages = vec![[0; BLOCK_SIZE]; page_count];
        backend.with_pager(|pager| {
            pager.plan_write_pages(
                inode,
                request.range.start_page(),
                &pages,
                FsyncStamp::new(generation.raw()),
            )
        })
    }
}

fn validate_multi_page_write_source(source: &IoDataSource, page_count: u64) -> Result<(), Errno> {
    let IoDataSource::Direct { vecs, .. } = source else {
        return Err(Errno::EINVAL);
    };
    if vecs.len() != usize::try_from(page_count).map_err(|_| Errno::EINVAL)? {
        return Err(Errno::EINVAL);
    }
    if vecs
        .iter()
        .any(|vec| vec.offset != 0 || vec.len != BLOCK_SIZE as u32)
    {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

pub(crate) struct Ext4FsInstance<I> {
    pager: Ext4PagerCell<I>,
    backend_planner: Option<Arc<dyn BackendPlanner>>,
    extent_mapping: Option<Arc<Ext4MappingTable>>,
    lookup_cache: SpinMutex<LookupCache>,
    dir_cache: SpinMutex<DirCache>,
    inode_meta_cache: SpinMutex<InodeMetaCache>,
    pub(crate) mount_pin: SpinMutex<Option<MountPayloadPin>>,
    file_page_container_binder: SpinMutex<Option<Arc<dyn FilePageContainerBinder>>>,
    file_page_containers: SpinMutex<BTreeMap<FsObjectId, ZoneWeak<PageContainer>>>,
    metadata_mutation_runtime: SpinMutex<Option<Arc<JournalMutationRuntime>>>,
    buffered_write_reservations: SpinMutex<BTreeMap<(u32, u64), Ext4MutationPlan>>,
    /// Per-mount read-only flag. When `true`, every mutating
    /// `FsOps` method (`create_inode`, `mkdir`, `unlink`, …) and
    /// every page-cache writeback rejects with `EROFS`. The flag is
    /// set at mount time by `mount_ext4_read_only`; the public
    /// `mount_ext4_read_write` entry point clears it. Matches
    /// Linux's `MS_RDONLY` semantics.
    read_only: AtomicBool,
    /// Mutation-journal mounts must submit writeback through their bound L5
    /// planner. The compatibility pager would otherwise update home blocks
    /// before the ordered transaction is committed.
    legacy_writeback_enabled: AtomicBool,
    capability_profile_hash: SpinMutex<Option<CapabilityProfileHash>>,
}

impl<I: BlockImage> Ext4FsInstance<I> {
    pub(crate) fn open(image: I, read_only: bool) -> Result<Arc<Self>, Errno> {
        Self::open_with_backend_planner(image, read_only, None)
    }

    pub(crate) fn open_with_backend_planner(
        image: I,
        read_only: bool,
        backend_planner: Option<Arc<dyn BackendPlanner>>,
    ) -> Result<Arc<Self>, Errno> {
        Self::open_with_backend_planner_and_mapping(image, read_only, backend_planner, None)
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
            inode_meta_cache: SpinMutex::new(InodeMetaCache::empty()),
            mount_pin: SpinMutex::new(None),
            file_page_container_binder: SpinMutex::new(None),
            file_page_containers: SpinMutex::new(BTreeMap::new()),
            metadata_mutation_runtime: SpinMutex::new(None),
            buffered_write_reservations: SpinMutex::new(BTreeMap::new()),
            read_only: AtomicBool::new(read_only),
            legacy_writeback_enabled: AtomicBool::new(true),
            capability_profile_hash: SpinMutex::new(None),
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

    pub(crate) fn bind_file_page_container(&self, container: Cap<PageContainer>) {
        if let Some(binder) = self.file_page_container_binder.lock().clone() {
            binder.bind_file_page_container(container);
        }
    }

    pub(crate) fn file_page_container_for_materialized_inode(
        &self,
        fs_object_id: FsObjectId,
        page_count: u64,
        size_bytes: u64,
        mount: MountPayloadPin,
        guard: &Guard<'_>,
    ) -> Result<Cap<PageContainer>, Errno> {
        {
            let mut index = self.file_page_containers.lock();
            if let Some(weak) = index.get(&fs_object_id) {
                if let Some(container) = weak.upgrade(guard) {
                    if container.size_bytes() < size_bytes {
                        container.set_size_bytes(size_bytes);
                    }
                    return Ok(container);
                }
                index.remove(&fs_object_id);
            }
        }

        let container = PageContainer::new_cap(
            PageContainerKind::File {
                mount,
                fs_object_id,
            },
            page_count,
        )
        .map_err(|_| Errno::ENOMEM)?;
        container.set_size_bytes(size_bytes);

        {
            let mut index = self.file_page_containers.lock();
            if let Some(weak) = index.get(&fs_object_id) {
                if let Some(existing) = weak.upgrade(guard) {
                    if existing.size_bytes() < size_bytes {
                        existing.set_size_bytes(size_bytes);
                    }
                    return Ok(existing);
                }
                index.remove(&fs_object_id);
            }
            index.insert(fs_object_id, container.downgrade());
        }

        self.bind_file_page_container(container.clone());
        Ok(container)
    }

    pub(crate) fn bind_metadata_mutation_runtime(&self, runtime: Arc<JournalMutationRuntime>) {
        *self.metadata_mutation_runtime.lock() = Some(runtime);
    }

    pub(crate) fn metadata_mutation_runtime(&self) -> Option<Arc<JournalMutationRuntime>> {
        self.metadata_mutation_runtime.lock().clone()
    }

    pub(crate) fn begin_metadata_mutation(
        &self,
        runtime: &JournalMutationRuntime,
        mutation: &Ext4MutationPlan,
        guard: &Guard<'_>,
    ) -> Result<(), JournalMutationRuntimeError>
    where
        I: Send + 'static,
    {
        match runtime.begin_mutation(mutation, guard) {
            Ok(()) => {}
            Err(JournalMutationRuntimeError::Busy(_)) => {
                self.settle_metadata_mutation(runtime)
                    .map_err(JournalMutationRuntimeError::Settlement)?;
                runtime.begin_mutation(mutation, guard)?;
            }
            Err(error) => return Err(error),
        }
        let _ = self.with_pager(|pager| {
            pager.stage_mutation_after_images(mutation);
            Ok(())
        });
        if mutation.data.is_empty() {
            self.settle_metadata_mutation(runtime)
                .map_err(JournalMutationRuntimeError::Settlement)?;
        }
        Ok(())
    }

    pub(crate) fn reserve_buffered_write(
        &self,
        inode: InodeNo,
        file_page_index: u64,
        mutation: Ext4MutationPlan,
    ) -> Result<(), Errno> {
        let key = (inode.get(), file_page_index);
        let mut reservations = self.buffered_write_reservations.lock();
        if reservations.contains_key(&key) {
            return Ok(());
        }
        if !reservations.is_empty() {
            return Err(Errno::EBUSY);
        }
        reservations.insert(key, mutation);
        Ok(())
    }

    pub(crate) fn buffered_write_reservation(
        &self,
        inode: InodeNo,
        file_page_index: u64,
    ) -> Option<Ext4MutationPlan> {
        self.buffered_write_reservations
            .lock()
            .get(&(inode.get(), file_page_index))
            .cloned()
    }

    pub(crate) fn clear_buffered_write_reservation(&self, inode: InodeNo, file_page_index: u64) {
        self.buffered_write_reservations
            .lock()
            .remove(&(inode.get(), file_page_index));
    }

    #[cfg(test)]
    pub(crate) fn buffered_write_reservation_count_for_test(&self) -> usize {
        self.buffered_write_reservations.lock().len()
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

    pub(crate) fn set_capability_profile_hash(&self, profile_hash: CapabilityProfileHash) {
        *self.capability_profile_hash.lock() = Some(profile_hash);
    }

    pub(crate) fn capability_profile_hash(&self) -> Option<CapabilityProfileHash> {
        *self.capability_profile_hash.lock()
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
        if let Some(cached) = self.lookup_cache.lock().get(parent, name) {
            return Ok(cached);
        }
        if let Some(cached) = self.dir_cache.lock().lookup(parent, name) {
            self.lookup_cache
                .lock()
                .insert(parent, name, cached.unwrap_or(InodeNo::new(0)));
            return Ok(cached);
        }
        let found = self.with_pager(|pager| pager.lookup(parent, name))?;
        // Cache negatives too (inode 0 sentinel): every shell command's PATH
        // search stats mostly-nonexistent names against testcases/bin
        // (~2800 entries); an uncached miss is a full linear directory scan
        // (~48 ms under TCG, measured) repeated for every command.
        self.lookup_cache
            .lock()
            .insert(parent, name, found.unwrap_or(InodeNo::new(0)));
        Ok(found)
    }

    pub(crate) fn inode_meta_cached(&self, inode: InodeNo) -> Result<InodeMetaLite, Errno> {
        if let Some(meta) = self.inode_meta_cache.lock().get(inode) {
            return Ok(meta);
        }
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
        if inode_meta_is_dir(meta) {
            self.inode_meta_cache.lock().insert(inode, meta);
        }
        Ok(meta)
    }

    pub(crate) fn read_dir_entries_cached(
        &self,
        inode: InodeNo,
        start_offset: u64,
        out: &mut [DirEntryLite; READDIR_WINDOW_ENTRIES],
        next_offsets: &mut [u64; READDIR_WINDOW_ENTRIES],
    ) -> Result<usize, Errno> {
        if let Some(count) = self
            .dir_cache
            .lock()
            .get(inode, start_offset, out, next_offsets)
        {
            return Ok(count);
        }

        let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
        let mut cached_next_offsets = [0u64; READDIR_WINDOW_ENTRIES];
        let count = self.with_pager(|pager| {
            pager.read_dir_entries_from_offset(
                inode,
                start_offset,
                &mut entries,
                &mut cached_next_offsets,
            )
        })?;
        self.dir_cache
            .lock()
            .insert(inode, start_offset, &entries, &cached_next_offsets, count);
        {
            let mut lookup_cache = self.lookup_cache.lock();
            for entry in entries.iter().take(count) {
                lookup_cache.insert(inode, entry.name(), entry.inode);
            }
        }
        out[..count].copy_from_slice(&entries[..count]);
        next_offsets[..count].copy_from_slice(&cached_next_offsets[..count]);
        Ok(count)
    }

    pub(crate) fn invalidate_lookup_cache_for(&self, parent: InodeNo) {
        self.lookup_cache.lock().invalidate_parent(parent);
        self.dir_cache.lock().invalidate(parent);
        self.inode_meta_cache.lock().invalidate(parent);
    }

    /// Rebuild every mount-local derived view after durable journal settlement.
    ///
    /// The pager and BlockImage have already made the home writes durable. The
    /// remaining mapping and namespace caches are only accelerators and must
    /// not survive a checkpoint that can change inode, directory, or extent
    /// metadata.
    pub(crate) fn settle_metadata_caches(&self) {
        let _ = self.with_pager(|pager| {
            pager.settle_image_cache();
            Ok(())
        });
        if let Some(mapping) = self.extent_mapping.as_ref() {
            mapping.clear();
        }
        *self.lookup_cache.lock() = LookupCache::empty();
        *self.dir_cache.lock() = DirCache::empty();
        *self.inode_meta_cache.lock() = InodeMetaCache::empty();
    }

    #[cfg(test)]
    pub(crate) fn cache_entry_counts(&self) -> (usize, usize, usize) {
        (
            self.lookup_cache
                .lock()
                .entries
                .iter()
                .filter(|entry| entry.valid)
                .count(),
            self.dir_cache
                .lock()
                .entries
                .iter()
                .filter(|entry| entry.valid)
                .count(),
            self.inode_meta_cache
                .lock()
                .entries
                .iter()
                .filter(|entry| entry.valid)
                .count(),
        )
    }
}

impl<I: BlockImage + Send + 'static> JournalSettlementObserver for Ext4FsInstance<I> {
    fn settle_after_checkpoint(&self) {
        self.settle_metadata_caches();
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
        start_offset: u64,
        out: &mut [DirEntryLite; READDIR_WINDOW_ENTRIES],
        next_offsets: &mut [u64; READDIR_WINDOW_ENTRIES],
    ) -> Option<usize> {
        let (index, window_index) =
            self.entries.iter().enumerate().find_map(|(index, entry)| {
                if !entry.valid || entry.inode != inode {
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

    fn lookup(&mut self, inode: InodeNo, name: &[u8]) -> Option<Option<InodeNo>> {
        let mut complete_first_window = None;
        let mut found_index = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if !entry.valid || entry.inode != inode {
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
        inode: InodeNo,
        start_offset: u64,
        entries: &[DirEntryLite; READDIR_WINDOW_ENTRIES],
        next_offsets: &[u64; READDIR_WINDOW_ENTRIES],
        count: usize,
    ) {
        self.clock = self.clock.wrapping_add(1);
        let victim = self
            .entries
            .iter()
            .position(|entry| {
                !entry.valid || (entry.inode == inode && entry.start_offset == start_offset)
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
        entry.inode = inode;
        entry.start_offset = start_offset;
        entry.count = count.min(READDIR_WINDOW_ENTRIES);
        entry.last_used = self.clock;
        entry.entries.clear();
        entry.entries.extend_from_slice(&entries[..entry.count]);
        entry.next_offsets.clear();
        entry
            .next_offsets
            .extend_from_slice(&next_offsets[..entry.count]);
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
    start_offset: u64,
    count: usize,
    entries: Vec<DirEntryLite>,
    next_offsets: Vec<u64>,
    last_used: u64,
}

impl DirCacheEntry {
    fn empty() -> Self {
        Self {
            valid: false,
            inode: InodeNo::new(0),
            start_offset: 0,
            count: 0,
            entries: Vec::new(),
            next_offsets: Vec::new(),
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
    fn get(&mut self, parent: InodeNo, name: &[u8]) -> Option<Option<InodeNo>> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.matches(parent, name))?;
        self.clock = self.clock.wrapping_add(1);
        self.entries[index].last_used = self.clock;
        let inode = self.entries[index].inode;
        if inode == InodeNo::new(0) {
            Some(None)
        } else {
            Some(Some(inode))
        }
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
        Ext4FormatError::WouldBlock => Errno::EAGAIN,
    }
}

fn timespec(sec: u32) -> Timespec {
    Timespec {
        sec: sec as i64,
        nsec: 0,
    }
}
