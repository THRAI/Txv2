use core::cell::UnsafeCell;
use core::convert::TryFrom;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::adapter::step_engine::{Cap, Guard, PayloadCap, SpinMutex, Weak as ZoneWeak};
use alloc::boxed::Box;
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
    Ext4MutationPlanSource, JournalMetadataMutationAdmission, JournalMetadataMutationPermit,
    JournalMutationRuntime, JournalMutationRuntimeError, JournalSettlementObserver,
    METADATA_MUTATION_READY,
};
use crate::planner::Ext4MappingTable;

pub(crate) const EXT4_ROOT_INODE: u32 = 2;
pub(crate) const READDIR_WINDOW_ENTRIES: usize = 64;
const DIR_VERSION_SHARDS: usize = 256;
const INODE_META_VERSION_SHARDS: usize = 1024;
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
    fn try_acquire_writeback_admission(
        &self,
    ) -> Result<JournalMetadataMutationPermit, tx_subsystems::fs_iface::WaitSourceId> {
        let backend = self
            .backend
            .lock()
            .as_ref()
            .and_then(ArcWeak::upgrade)
            .ok_or_else(|| tx_subsystems::fs_iface::WaitSourceId::new(0))?;
        backend
            .try_lock_metadata_mutation_admission()
            .ok_or_else(|| {
                tx_subsystems::fs_iface::WaitSourceId::new(
                    backend.metadata_mutation_admission.wait_source_id(),
                )
            })
    }

    fn settle_prior_writeback_mutation(
        &self,
        runtime: &JournalMutationRuntime,
    ) -> Result<(), Errno> {
        let backend = self
            .backend
            .lock()
            .as_ref()
            .and_then(ArcWeak::upgrade)
            .ok_or(Errno::EIO)?;
        backend.settle_metadata_mutation(runtime)
    }

    fn stage_writeback_after_images(&self, mutation: &Ext4MutationPlan) -> Result<(), Errno> {
        let backend = self
            .backend
            .lock()
            .as_ref()
            .and_then(ArcWeak::upgrade)
            .ok_or(Errno::EIO)?;
        backend.with_pager(|pager| {
            pager.stage_mutation_after_images(mutation);
            Ok(())
        })
    }

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
        let (inode, _) = backend.resolve_object(FsObjectId::new(request.object.raw()))?;
        if backend.is_read_only() {
            return Err(Errno::EROFS);
        }
        if backend.metadata_mutation_runtime().is_none() {
            return Err(Errno::EOPNOTSUPP);
        }

        // The runtime replaces these placeholders with the L4-owned sources.
        // Planning all pages together lets ext4 allocate holes once and fold
        // the PageContainer's byte-precise EOF into the same transaction.
        if request.range.page_count() > 1 {
            validate_multi_page_write_source(&request.source, request.range.page_count())?;
        }
        let page_count = usize::try_from(request.range.page_count()).map_err(|_| Errno::EINVAL)?;
        let pages = vec![[0; BLOCK_SIZE]; page_count];
        let exact_size = backend.file_page_container_size(FsObjectId::new(request.object.raw()));
        backend.with_pager(|pager| {
            pager.plan_write_pages_with_size(
                inode,
                request.range.start_page(),
                &pages,
                exact_size,
                FsyncStamp::new(generation.raw()),
            )
        })
    }
}

fn validate_multi_page_write_source(source: &IoDataSource, page_count: u64) -> Result<(), Errno> {
    let count = usize::try_from(page_count).map_err(|_| Errno::EINVAL)?;
    match source {
        IoDataSource::PageCacheSegments { segments, .. } => {
            if segments.len() != count
                || segments
                    .iter()
                    .any(|segment| segment.offset != 0 || segment.len != BLOCK_SIZE as u32)
            {
                return Err(Errno::EINVAL);
            }
        }
        IoDataSource::Direct { vecs, .. } => {
            if vecs.len() != count
                || vecs
                    .iter()
                    .any(|vec| vec.offset != 0 || vec.len != BLOCK_SIZE as u32)
            {
                return Err(Errno::EINVAL);
            }
        }
        IoDataSource::None | IoDataSource::PageCache { .. } => return Err(Errno::EINVAL),
    }
    Ok(())
}

pub(crate) struct Ext4FsInstance<I> {
    pager: Ext4PagerCell<I>,
    backend_planner: Option<Arc<dyn BackendPlanner>>,
    extent_mapping: Option<Arc<Ext4MappingTable>>,
    lookup_cache: LookupCache,
    dir_cache: SpinMutex<DirCache>,
    /// Lock-free namespace generations used to validate lookup/readdir cache
    /// fills against concurrent directory mutations.
    ///
    /// Pager operations serialize the disk snapshot, but cache insertion runs
    /// after that pager guard is released. A generation prevents an old read
    /// from being published after a creator has committed and invalidated the
    /// directory caches.
    dir_versions: [AtomicU64; DIR_VERSION_SHARDS],
    /// Versioned publication prevents a reader that started before a metadata
    /// mutation from inserting its old pager snapshot after invalidation.
    /// Hash collisions only cause conservative misses.
    inode_meta_versions: [AtomicU64; INODE_META_VERSION_SHARDS],
    /// Whole-cache generation used by journal settlement and shutdown.
    inode_meta_epoch: AtomicU64,
    inode_meta_cache: InodeMetaCache,
    pub(crate) mount_pin: SpinMutex<Option<MountPayloadPin>>,
    file_page_container_binder: SpinMutex<Option<Arc<dyn FilePageContainerBinder>>>,
    file_page_containers: SpinMutex<BTreeMap<FsObjectId, ZoneWeak<PageContainer>>>,
    metadata_mutation_runtime: SpinMutex<Option<Arc<JournalMutationRuntime>>>,
    /// Serializes the mount-local metadata plan/admit/settle transaction.
    ///
    /// Planning consults the pager's staged after-images, so the plan and its
    /// admission must form one critical section. Merely relying on the JBD2
    /// runtime's inner lock allows two creators to build from the same inode
    /// bitmap snapshot and then exposes its internal Busy state to userspace.
    metadata_mutation_admission: Arc<JournalMetadataMutationAdmission>,
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
            lookup_cache: LookupCache::empty(),
            dir_cache: SpinMutex::new(DirCache::empty()),
            dir_versions: core::array::from_fn(|_| AtomicU64::new(0)),
            inode_meta_versions: core::array::from_fn(|_| AtomicU64::new(0)),
            inode_meta_epoch: AtomicU64::new(0),
            inode_meta_cache: InodeMetaCache::empty(),
            mount_pin: SpinMutex::new(None),
            file_page_container_binder: SpinMutex::new(None),
            file_page_containers: SpinMutex::new(BTreeMap::new()),
            metadata_mutation_runtime: SpinMutex::new(None),
            metadata_mutation_admission: Arc::new(JournalMetadataMutationAdmission::new()),
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

    /// Forget the mount-local page-cache identity for one inode number.
    ///
    /// An unlinked file may keep its `PageContainer` alive while close-time
    /// writeback drains.  Once ext4 reclaims and later reallocates that inode
    /// number, the old container must remain usable only through those
    /// existing references; a newly materialised inode needs a fresh logical
    /// size and resident-page namespace.
    pub(crate) fn invalidate_file_page_container(&self, fs_object_id: FsObjectId) {
        self.file_page_containers.lock().remove(&fs_object_id);
    }

    pub(crate) fn file_page_container_size(&self, fs_object_id: FsObjectId) -> Option<u64> {
        let guard = tx_substrate::epoch::borrow_current_guard()
            .unwrap_or_else(crate::adapter::step_engine::guard);
        let container = {
            let index = self.file_page_containers.lock();
            index
                .get(&fs_object_id)
                .and_then(|container| container.upgrade(&guard))
        }?;
        Some(container.size_bytes())
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
                        container.set_size_bytes_persisted(size_bytes);
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
        container.set_size_bytes_persisted(size_bytes);

        {
            let mut index = self.file_page_containers.lock();
            if let Some(weak) = index.get(&fs_object_id) {
                if let Some(existing) = weak.upgrade(guard) {
                    if existing.size_bytes() < size_bytes {
                        existing.set_size_bytes_persisted(size_bytes);
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

    pub(crate) fn try_lock_metadata_mutation_admission(
        &self,
    ) -> Option<JournalMetadataMutationPermit> {
        self.metadata_mutation_admission.try_acquire()
    }

    pub(crate) fn lock_metadata_mutation_for_frontend(
        &self,
    ) -> Option<JournalMetadataMutationPermit> {
        // A compatibility mount performs the complete mutation synchronously
        // in the caller. Returning `Yield` here is incorrect for close: the
        // VFS has already detached the descriptor and can only defer the
        // writeback, so a concurrent creator may observe a zero-length file.
        // Wait locally just like the pager's existing spin lock. Journal
        // mounts remain nonblocking because their permit can be retained by
        // L4 across an actual asynchronous I/O completion.
        if self.metadata_mutation_runtime().is_none() && self.legacy_writeback_enabled() {
            loop {
                if let Some(permit) = self.metadata_mutation_admission.try_acquire() {
                    return Some(permit);
                }
                core::hint::spin_loop();
            }
        }
        self.try_lock_metadata_mutation_admission()
    }

    pub(crate) fn wait_for_metadata_mutation_admission<T>(
        &self,
    ) -> crate::adapter::step_engine::StepOutcome<T, crate::adapter::step_engine::NoProgress> {
        crate::adapter::step_engine::StepOutcome::yield_on_wait_source(
            crate::adapter::step_engine::NoProgress,
            self.metadata_mutation_admission.wait_source_id(),
            METADATA_MUTATION_READY,
        )
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

    /// Execute one planner result using the mount's selected commit model.
    ///
    /// Journal-backed mounts retain the asynchronous transaction runtime.
    /// Compatibility read-write mounts apply the same rich mutation plan
    /// directly, preserving the fast root-filesystem path used before the
    /// async integration without falling back to reduced namespace support.
    pub(crate) fn commit_metadata_mutation(
        &self,
        mutation: &Ext4MutationPlan,
        guard: &Guard<'_>,
    ) -> Result<(), Errno>
    where
        I: Send + 'static,
    {
        if let Some(runtime) = self.metadata_mutation_runtime() {
            return self
                .begin_metadata_mutation(&runtime, mutation, guard)
                .map_err(|error| match error {
                    JournalMutationRuntimeError::Busy(_) => Errno::EBUSY,
                    JournalMutationRuntimeError::Image(_)
                    | JournalMutationRuntimeError::Stage(_) => Errno::EIO,
                    JournalMutationRuntimeError::Settlement(errno) => errno,
                });
        }
        if !self.legacy_writeback_enabled() {
            return Err(Errno::EOPNOTSUPP);
        }
        self.with_pager(|pager| pager.apply_mutation_direct(mutation))
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

    pub(crate) fn dir_version(&self, object: FsObjectId) -> u64 {
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
        'stable_snapshot: loop {
            let version = self.dir_version(parent);
            if let Some(cached) = self.lookup_cache.get(parent, name, version) {
                if self.dir_version(parent) == version {
                    return Ok(cached);
                }
                continue;
            }

            // Scan in byte-offset windows and populate every observed name,
            // not just the requested one. Cargo probes thousands of siblings
            // in `target/debug/deps`; making each first-time name restart a
            // private linear scan throws away nearly all locality. The window
            // cache also makes a concurrent readdir share the same snapshot.
            let mut offset = 0u64;
            loop {
                let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
                let mut next_offsets = [0u64; READDIR_WINDOW_ENTRIES];
                let count = self.read_dir_entries_cached(
                    parent,
                    parent_inode,
                    offset,
                    &mut entries,
                    &mut next_offsets,
                )?;

                // A namespace mutation can commit between two windows. Do not
                // combine entries from different directory generations.
                if self.dir_version(parent) != version {
                    continue 'stable_snapshot;
                }
                if let Some(entry) = entries
                    .iter()
                    .take(count)
                    .find(|entry| entry.name() == name)
                {
                    return Ok(Some(entry.inode));
                }
                if count == 0 {
                    self.lookup_cache
                        .insert(parent, name, InodeNo::new(0), version);
                    return Ok(None);
                }
                offset = next_offsets[count - 1];
            }
        }
    }

    /// Resolve a directory name to its persistent inode incarnation.
    ///
    /// The directory scan initially knows only the numeric inode. Cache the
    /// generation-bearing object ID in the same versioned name entry after the
    /// first inode read, so repeated stat/open probes do not serialize on the
    /// pager merely to reconstruct an identity they have already validated.
    pub(crate) fn lookup_object_cached(
        &self,
        parent: FsObjectId,
        name: &[u8],
    ) -> Result<Option<FsObjectId>, Errno> {
        loop {
            let version = self.dir_version(parent);
            if let Some(cached) = self.lookup_cache.get_object(parent, name, version) {
                if self.dir_version(parent) == version {
                    return Ok(cached);
                }
                continue;
            }
            // A versioned object-ID hit above has already validated both the
            // directory incarnation and the child incarnation. Only a miss
            // needs to re-read/validate the parent before scanning on disk;
            // doing this before every hit serialized all Cargo path probes on
            // the mount-wide directory-metadata cache.
            let (parent_inode, _) = self.resolve_directory_object_cached(parent)?;
            if self.dir_version(parent) != version {
                continue;
            }
            let Some(inode) = self.lookup_cached(parent, parent_inode, name)? else {
                return Ok(None);
            };
            if self.dir_version(parent) != version {
                continue;
            }
            let object = self.object_id_for_inode(inode)?;
            if self.dir_version(parent) != version {
                continue;
            }
            self.lookup_cache
                .set_object(parent, name, inode, object, version);
            return Ok(Some(object));
        }
    }

    pub(crate) fn inode_meta_cached(&self, inode: InodeNo) -> Result<InodeMetaLite, Errno> {
        loop {
            let version = self.inode_meta_version(inode);
            if let Some(meta) = self.inode_meta_cache.get(inode, version) {
                return Ok(meta);
            }
            let meta = self.inode_meta_with_mapping(inode)?;
            if self.inode_meta_version(inode) != version {
                continue;
            }
            self.inode_meta_cache.insert(inode, meta, version);
            return Ok(meta);
        }
    }

    /// Read the versioned inode cache without filling it.  A concurrent
    /// invalidation either makes the initial lookup miss or changes the
    /// version checked after the lookup; both cases conservatively fall back
    /// to the ordinary metadata path.
    pub(crate) fn inode_meta_cached_only(&self, inode: InodeNo) -> Option<InodeMetaLite> {
        loop {
            let version = self.inode_meta_version(inode);
            let meta = self.inode_meta_cache.get(inode, version)?;
            if self.inode_meta_version(inode) == version {
                return Some(meta);
            }
        }
    }

    fn inode_meta_version(&self, inode: InodeNo) -> InodeMetaCacheVersion {
        InodeMetaCacheVersion {
            epoch: self.inode_meta_epoch.load(Ordering::Acquire),
            inode: self.inode_meta_versions[inode_meta_version_shard(inode)]
                .load(Ordering::Acquire),
        }
    }

    pub(crate) fn invalidate_inode_meta_for(&self, object: FsObjectId) {
        self.invalidate_inode_meta_no(InodeNo::new(object.inode_number()));
    }

    pub(crate) fn invalidate_inode_meta_no(&self, inode: InodeNo) {
        self.inode_meta_versions[inode_meta_version_shard(inode)].fetch_add(1, Ordering::AcqRel);
        self.inode_meta_cache.invalidate(inode);
    }

    /// Resolve and validate one persistent inode incarnation.
    ///
    /// ext4 can reuse a numeric inode immediately after the last reference to
    /// an unlinked object disappears.  Every externally supplied object ID
    /// therefore carries both the inode number and its on-disk generation;
    /// accepting only the low inode bits would let an old dentry or page cache
    /// address a newly allocated file.
    pub(crate) fn resolve_object(
        &self,
        fs_object_id: FsObjectId,
    ) -> Result<(InodeNo, InodeMetaLite), Errno> {
        let inode = inode_no(fs_object_id)?;
        let meta = self.inode_meta_cached(inode)?;
        if meta.mode == 0 {
            return Err(Errno::ENOENT);
        }
        if meta.generation != fs_object_id.inode_generation() {
            return Err(Errno::ESTALE);
        }
        Ok((inode, meta))
    }

    /// Directory-only identity validation with a mount-local metadata cache.
    /// Path walking repeatedly validates the same handful of parent
    /// directories; their generation and kind are immutable for the lifetime
    /// of the object, and namespace mutation invalidates the corresponding
    /// cache entry before a later lookup can observe a reused inode.
    pub(crate) fn resolve_directory_object_cached(
        &self,
        fs_object_id: FsObjectId,
    ) -> Result<(InodeNo, InodeMetaLite), Errno> {
        let inode = inode_no(fs_object_id)?;
        let meta = self.inode_meta_cached(inode)?;
        if !inode_meta_is_dir(meta) {
            return Err(if meta.mode == 0 {
                Errno::ENOENT
            } else {
                Errno::ENOTDIR
            });
        }
        if meta.generation != fs_object_id.inode_generation() {
            return Err(Errno::ESTALE);
        }
        Ok((inode, meta))
    }

    pub(crate) fn object_id_for_inode(&self, inode: InodeNo) -> Result<FsObjectId, Errno> {
        let meta = self.inode_meta_cached(inode)?;
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
        object: FsObjectId,
        inode: InodeNo,
        start_offset: u64,
        out: &mut [DirEntryLite; READDIR_WINDOW_ENTRIES],
        next_offsets: &mut [u64; READDIR_WINDOW_ENTRIES],
    ) -> Result<usize, Errno> {
        let version = self.dir_version(object);
        if let Some(count) =
            self.dir_cache
                .lock()
                .get(object, start_offset, version, out, next_offsets)
        {
            return Ok(count);
        }

        let mut entries = [DirEntryLite::empty(); READDIR_WINDOW_ENTRIES];
        let mut cached_next_offsets = [0u64; READDIR_WINDOW_ENTRIES];
        let (count, snapshot_version) = self.with_pager(|pager| {
            let count = pager.read_dir_entries_from_offset(
                inode,
                start_offset,
                &mut entries,
                &mut cached_next_offsets,
            )?;
            Ok((count, self.dir_version(object)))
        })?;
        self.dir_cache.lock().insert(
            object,
            start_offset,
            snapshot_version,
            &entries,
            &cached_next_offsets,
            count,
        );
        for entry in entries.iter().take(count) {
            self.lookup_cache
                .insert(object, entry.name(), entry.inode, snapshot_version);
        }
        out[..count].copy_from_slice(&entries[..count]);
        next_offsets[..count].copy_from_slice(&cached_next_offsets[..count]);
        Ok(count)
    }

    pub(crate) fn invalidate_lookup_cache_for(&self, parent: FsObjectId) {
        self.bump_dir_version(parent);
        self.lookup_cache.invalidate_parent(parent);
        self.dir_cache.lock().invalidate(parent);
        self.invalidate_inode_meta_for(parent);
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
        self.lookup_cache.clear();
        *self.dir_cache.lock() = DirCache::empty();
        self.inode_meta_epoch.fetch_add(1, Ordering::AcqRel);
        self.inode_meta_cache.clear();
    }

    #[cfg(test)]
    pub(crate) fn cache_entry_counts(&self) -> (usize, usize, usize) {
        (
            self.lookup_cache.entry_count(),
            self.dir_cache
                .lock()
                .entries
                .iter()
                .filter(|entry| entry.valid)
                .count(),
            self.inode_meta_cache.entry_count(),
        )
    }
}

impl<I: BlockImage + Send + 'static> JournalSettlementObserver for Ext4FsInstance<I> {
    fn settle_after_checkpoint(&self) {
        self.settle_metadata_caches();
    }
}

fn dir_version_shard(object: FsObjectId) -> usize {
    let raw = object.as_u64();
    (raw ^ (raw >> 32)) as usize % DIR_VERSION_SHARDS
}

fn inode_meta_version_shard(inode: InodeNo) -> usize {
    let raw = inode.get() as usize;
    raw.wrapping_mul(0x9e37_79b9) % INODE_META_VERSION_SHARDS
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

// Cargo's cached dependency check stats several thousand files. Keep metadata
// separately from retained DEntries so closing a file may release its
// PageContainer without forcing the next stat through the mount-wide pager
// lock. A sharded four-way cache bounds both lock contention and lookup work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InodeMetaCacheVersion {
    epoch: u64,
    inode: u64,
}

const INODE_META_CACHE_ENTRIES: usize = 4096;
const INODE_META_CACHE_WAYS: usize = 4;
const INODE_META_CACHE_SHARDS: usize = 64;
const INODE_META_CACHE_ENTRIES_PER_SHARD: usize =
    INODE_META_CACHE_ENTRIES / INODE_META_CACHE_SHARDS;
const INODE_META_CACHE_SETS_PER_SHARD: usize =
    INODE_META_CACHE_ENTRIES_PER_SHARD / INODE_META_CACHE_WAYS;

struct InodeMetaCache {
    shards: Box<[SpinMutex<InodeMetaCacheShard>]>,
}

impl InodeMetaCache {
    fn empty() -> Self {
        let mut shards = Vec::with_capacity(INODE_META_CACHE_SHARDS);
        for _ in 0..INODE_META_CACHE_SHARDS {
            shards.push(SpinMutex::new(InodeMetaCacheShard::empty()));
        }
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    fn get(&self, inode: InodeNo, version: InodeMetaCacheVersion) -> Option<InodeMetaLite> {
        let (shard, range) = inode_meta_cache_location(inode);
        self.shards[shard].lock().get(inode, version, range)
    }

    fn insert(&self, inode: InodeNo, meta: InodeMetaLite, version: InodeMetaCacheVersion) {
        let (shard, range) = inode_meta_cache_location(inode);
        self.shards[shard]
            .lock()
            .insert(inode, meta, version, range);
    }

    fn invalidate(&self, inode: InodeNo) {
        let (shard, range) = inode_meta_cache_location(inode);
        self.shards[shard].lock().invalidate(inode, range);
    }

    fn clear(&self) {
        for shard in self.shards.iter() {
            *shard.lock() = InodeMetaCacheShard::empty();
        }
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                shard
                    .lock()
                    .entries
                    .iter()
                    .filter(|entry| entry.valid)
                    .count()
            })
            .sum()
    }
}

struct InodeMetaCacheShard {
    clock: u64,
    entries: [InodeMetaCacheEntry; INODE_META_CACHE_ENTRIES_PER_SHARD],
}

impl InodeMetaCacheShard {
    const fn empty() -> Self {
        Self {
            clock: 0,
            entries: [InodeMetaCacheEntry::empty(); INODE_META_CACHE_ENTRIES_PER_SHARD],
        }
    }

    fn get(
        &mut self,
        inode: InodeNo,
        version: InodeMetaCacheVersion,
        mut range: core::ops::Range<usize>,
    ) -> Option<InodeMetaLite> {
        let index = range.find(|index| {
            let entry = &self.entries[*index];
            entry.valid && entry.inode == inode && entry.version == version
        })?;
        self.clock = self.clock.wrapping_add(1);
        self.entries[index].last_used = self.clock;
        Some(self.entries[index].meta)
    }

    fn insert(
        &mut self,
        inode: InodeNo,
        meta: InodeMetaLite,
        version: InodeMetaCacheVersion,
        range: core::ops::Range<usize>,
    ) {
        self.clock = self.clock.wrapping_add(1);
        let victim = range
            .clone()
            .find(|index| {
                let entry = &self.entries[*index];
                !entry.valid || (entry.inode == inode && entry.version == version)
            })
            .unwrap_or_else(|| {
                range
                    .min_by_key(|index| self.entries[*index].last_used)
                    .unwrap_or(0)
            });
        self.entries[victim] = InodeMetaCacheEntry {
            valid: true,
            inode,
            meta,
            version,
            last_used: self.clock,
        };
    }

    fn invalidate(&mut self, inode: InodeNo, range: core::ops::Range<usize>) {
        for index in range {
            let entry = &mut self.entries[index];
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
    version: InodeMetaCacheVersion,
    last_used: u64,
}

impl InodeMetaCacheEntry {
    const fn empty() -> Self {
        Self {
            valid: false,
            inode: InodeNo::new(0),
            meta: InodeMetaLite {
                generation: 0,
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
            version: InodeMetaCacheVersion { epoch: 0, inode: 0 },
            last_used: 0,
        }
    }
}

fn inode_meta_cache_location(inode: InodeNo) -> (usize, core::ops::Range<usize>) {
    let hash = (inode.get() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let global_set = hash as usize % (INODE_META_CACHE_ENTRIES / INODE_META_CACHE_WAYS);
    let shard = global_set % INODE_META_CACHE_SHARDS;
    let local_set = global_set / INODE_META_CACHE_SHARDS;
    debug_assert!(local_set < INODE_META_CACHE_SETS_PER_SHARD);
    let first = local_set * INODE_META_CACHE_WAYS;
    (shard, first..first + INODE_META_CACHE_WAYS)
}

// Cargo's `target/debug/deps` and the OSComp `testcases/bin` directory both
// exceed 512 names. A cache smaller than either working set turns unique stat
// probes back into repeated full-directory scans. Four thousand entries cost
// well below 1 MiB per ext4 mount and retain the complete measured workloads.
const LOOKUP_CACHE_ENTRIES: usize = 4096;
const LOOKUP_CACHE_NAME_BYTES: usize = 96;
const LOOKUP_CACHE_WAYS: usize = 4;
const LOOKUP_CACHE_SHARDS: usize = 64;
const LOOKUP_CACHE_ENTRIES_PER_SHARD: usize = LOOKUP_CACHE_ENTRIES / LOOKUP_CACHE_SHARDS;
const LOOKUP_CACHE_SETS_PER_SHARD: usize = LOOKUP_CACHE_ENTRIES_PER_SHARD / LOOKUP_CACHE_WAYS;

struct LookupCache {
    shards: Box<[SpinMutex<LookupCacheShard>]>,
}

impl LookupCache {
    fn empty() -> Self {
        let mut shards = Vec::with_capacity(LOOKUP_CACHE_SHARDS);
        for _ in 0..LOOKUP_CACHE_SHARDS {
            shards.push(SpinMutex::new(LookupCacheShard::empty()));
        }
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    fn get(&self, parent: FsObjectId, name: &[u8], version: u64) -> Option<Option<InodeNo>> {
        let (shard, range) = lookup_cache_location(parent, name);
        self.shards[shard].lock().get(parent, name, version, range)
    }

    fn get_object(
        &self,
        parent: FsObjectId,
        name: &[u8],
        version: u64,
    ) -> Option<Option<FsObjectId>> {
        let (shard, range) = lookup_cache_location(parent, name);
        self.shards[shard]
            .lock()
            .get_object(parent, name, version, range)
    }

    fn insert(&self, parent: FsObjectId, name: &[u8], inode: InodeNo, version: u64) {
        if name.len() > LOOKUP_CACHE_NAME_BYTES {
            return;
        }
        let (shard, range) = lookup_cache_location(parent, name);
        self.shards[shard]
            .lock()
            .insert(parent, name, inode, version, range);
    }

    fn set_object(
        &self,
        parent: FsObjectId,
        name: &[u8],
        inode: InodeNo,
        object: FsObjectId,
        version: u64,
    ) {
        let (shard, range) = lookup_cache_location(parent, name);
        self.shards[shard]
            .lock()
            .set_object(parent, name, inode, object, version, range);
    }

    fn invalidate_parent(&self, parent: FsObjectId) {
        for shard in self.shards.iter() {
            shard.lock().invalidate_parent(parent);
        }
    }

    fn clear(&self) {
        for shard in self.shards.iter() {
            *shard.lock() = LookupCacheShard::empty();
        }
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                shard
                    .lock()
                    .entries
                    .iter()
                    .filter(|entry| entry.valid)
                    .count()
            })
            .sum()
    }

    #[cfg(test)]
    fn occupied_for(&self, parent: FsObjectId, name: &[u8]) -> usize {
        let (shard, range) = lookup_cache_location(parent, name);
        let shard = self.shards[shard].lock();
        range.filter(|index| shard.entries[*index].valid).count()
    }
}

struct LookupCacheShard {
    clock: u64,
    entries: [LookupCacheEntry; LOOKUP_CACHE_ENTRIES_PER_SHARD],
}

impl LookupCacheShard {
    const fn empty() -> Self {
        Self {
            clock: 0,
            entries: [LookupCacheEntry::empty(); LOOKUP_CACHE_ENTRIES_PER_SHARD],
        }
    }

    /// `None` = not cached; `Some(None)` = cached-negative (ENOENT);
    /// `Some(Some(ino))` = cached hit. Negative entries reuse `inode == 0`
    /// (ext4 inode numbers start at 1) and are invalidated by the same
    /// `invalidate_parent` calls that cover create/unlink/rename.
    fn get(
        &mut self,
        parent: FsObjectId,
        name: &[u8],
        version: u64,
        range: core::ops::Range<usize>,
    ) -> Option<Option<InodeNo>> {
        let index = range
            .clone()
            .find(|index| self.entries[*index].matches(parent, name, version))?;
        self.clock = self.clock.wrapping_add(1);
        self.entries[index].last_used = self.clock;
        let inode = self.entries[index].inode;
        if inode == InodeNo::new(0) {
            Some(None)
        } else {
            Some(Some(inode))
        }
    }

    fn insert(
        &mut self,
        parent: FsObjectId,
        name: &[u8],
        inode: InodeNo,
        version: u64,
        range: core::ops::Range<usize>,
    ) {
        self.clock = self.clock.wrapping_add(1);
        let victim = range
            .clone()
            .find(|index| {
                let entry = &self.entries[*index];
                !entry.valid || entry.same_name(parent, name)
            })
            .unwrap_or_else(|| {
                range
                    .min_by_key(|index| self.entries[*index].last_used)
                    .unwrap_or(0)
            });
        self.entries[victim] = LookupCacheEntry::new(parent, name, inode, version, self.clock);
    }

    /// Return a cached incarnation when the name entry has already paid for
    /// the child-inode read. A negative name entry needs no object ID and is
    /// therefore immediately authoritative.
    fn get_object(
        &mut self,
        parent: FsObjectId,
        name: &[u8],
        version: u64,
        range: core::ops::Range<usize>,
    ) -> Option<Option<FsObjectId>> {
        let index = range
            .clone()
            .find(|index| self.entries[*index].matches(parent, name, version))?;
        self.clock = self.clock.wrapping_add(1);
        let entry = &mut self.entries[index];
        entry.last_used = self.clock;
        if entry.inode == InodeNo::new(0) {
            Some(None)
        } else if entry.object_valid {
            Some(Some(entry.object))
        } else {
            None
        }
    }

    fn set_object(
        &mut self,
        parent: FsObjectId,
        name: &[u8],
        inode: InodeNo,
        object: FsObjectId,
        version: u64,
        range: core::ops::Range<usize>,
    ) {
        let Some(index) = range.clone().find(|index| {
            let entry = &self.entries[*index];
            entry.matches(parent, name, version) && entry.inode == inode
        }) else {
            return;
        };
        self.clock = self.clock.wrapping_add(1);
        let entry = &mut self.entries[index];
        entry.object = object;
        entry.object_valid = true;
        entry.last_used = self.clock;
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
    object: FsObjectId,
    object_valid: bool,
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
            object: FsObjectId::new(0),
            object_valid: false,
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

    fn same_name(&self, parent: FsObjectId, name: &[u8]) -> bool {
        self.valid
            && self.parent == parent
            && self.name_len as usize == name.len()
            && &self.name[..name.len()] == name
    }
}

fn lookup_cache_location(parent: FsObjectId, name: &[u8]) -> (usize, core::ops::Range<usize>) {
    // FNV-1a is sufficient here: the key is not attacker-visible hash-table
    // state, and a four-way set bounds every lookup to four fixed-size entry
    // probes instead of scanning all 512 cached names under one spin lock.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in parent.as_u64().to_le_bytes().iter().chain(name) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let global_set = hash as usize % (LOOKUP_CACHE_ENTRIES / LOOKUP_CACHE_WAYS);
    let shard = global_set % LOOKUP_CACHE_SHARDS;
    let local_set = global_set / LOOKUP_CACHE_SHARDS;
    debug_assert!(local_set < LOOKUP_CACHE_SETS_PER_SHARD);
    let first = local_set * LOOKUP_CACHE_WAYS;
    (shard, first..first + LOOKUP_CACHE_WAYS)
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
        Ext4FormatError::WouldBlock => Errno::EAGAIN,
    }
}

fn timespec(sec: u32) -> Timespec {
    Timespec {
        sec: sec as i64,
        nsec: 0,
    }
}

#[cfg(test)]
mod lookup_cache_tests {
    use super::*;

    fn inode_meta(generation: u32, size: u64) -> InodeMetaLite {
        InodeMetaLite {
            generation,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            size,
            nlinks: 1,
            blocks_512: size.div_ceil(512),
            flags: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
        }
    }

    #[test]
    fn sharded_inode_meta_cache_hits_and_invalidates_one_inode() {
        let cache = InodeMetaCache::empty();
        let first = InodeNo::new(41);
        let second = InodeNo::new(42);
        let version = InodeMetaCacheVersion { epoch: 2, inode: 5 };
        cache.insert(first, inode_meta(3, 4096), version);
        cache.insert(second, inode_meta(7, 8192), version);

        assert_eq!(cache.get(first, version), Some(inode_meta(3, 4096)));
        assert_eq!(cache.get(second, version), Some(inode_meta(7, 8192)));
        cache.invalidate(first);
        assert_eq!(cache.get(first, version), None);
        assert_eq!(cache.get(second, version), Some(inode_meta(7, 8192)));
    }

    #[test]
    fn sharded_inode_meta_cache_replaces_same_inode_without_duplicate_slot() {
        let cache = InodeMetaCache::empty();
        let inode = InodeNo::new(73);
        let version = InodeMetaCacheVersion { epoch: 0, inode: 4 };
        cache.insert(inode, inode_meta(1, 16), version);
        cache.insert(inode, inode_meta(1, 32), version);

        assert_eq!(cache.get(inode, version), Some(inode_meta(1, 32)));
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn inode_meta_cache_rejects_a_pre_invalidation_publication() {
        let cache = InodeMetaCache::empty();
        let inode = InodeNo::new(91);
        let old = InodeMetaCacheVersion { epoch: 3, inode: 8 };
        let new = InodeMetaCacheVersion { epoch: 3, inode: 9 };

        cache.insert(inode, inode_meta(1, 1024), old);
        assert_eq!(cache.get(inode, new), None);
        cache.insert(inode, inode_meta(1, 2048), new);
        assert_eq!(cache.get(inode, new), Some(inode_meta(1, 2048)));
    }

    #[test]
    fn set_associative_lookup_cache_replaces_a_stale_name_version_in_place() {
        let parent = FsObjectId::from_inode_generation(7, 3);
        let cache = LookupCache::empty();
        cache.insert(parent, b"libcore.rlib", InodeNo::new(41), 1);
        assert_eq!(
            cache.get(parent, b"libcore.rlib", 1),
            Some(Some(InodeNo::new(41)))
        );

        cache.insert(parent, b"libcore.rlib", InodeNo::new(52), 2);
        assert_eq!(cache.get(parent, b"libcore.rlib", 1), None);
        assert_eq!(
            cache.get(parent, b"libcore.rlib", 2),
            Some(Some(InodeNo::new(52)))
        );
        let occupied = cache.occupied_for(parent, b"libcore.rlib");
        assert_eq!(
            occupied, 1,
            "a new directory version replaces its stale slot"
        );
    }

    #[test]
    fn set_associative_lookup_cache_preserves_negative_entries_and_parent_invalidation() {
        let parent = FsObjectId::from_inode_generation(9, 1);
        let cache = LookupCache::empty();
        cache.insert(parent, b"missing-tool", InodeNo::new(0), 4);
        assert_eq!(cache.get(parent, b"missing-tool", 4), Some(None));
        cache.invalidate_parent(parent);
        assert_eq!(cache.get(parent, b"missing-tool", 4), None);
    }

    #[test]
    fn lookup_cache_promotes_an_inode_hit_to_a_persistent_object_id() {
        let parent = FsObjectId::from_inode_generation(11, 2);
        let child = FsObjectId::from_inode_generation(73, 9);
        let cache = LookupCache::empty();
        cache.insert(parent, b"liballoc.rlib", InodeNo::new(73), 6);

        assert_eq!(cache.get_object(parent, b"liballoc.rlib", 6), None);
        cache.set_object(parent, b"liballoc.rlib", InodeNo::new(73), child, 6);
        assert_eq!(
            cache.get_object(parent, b"liballoc.rlib", 6),
            Some(Some(child))
        );
        assert_eq!(cache.get_object(parent, b"liballoc.rlib", 7), None);
    }
}
