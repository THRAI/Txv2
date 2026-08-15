//! Ext4-owned JBD2 ordered-mode transaction planning.
//!
//! This module converts already-owned write buffers into neutral L6 graphs. It
//! neither owns metadata/page caches nor executes I/O; L5 keeps transaction
//! state and L6 executes the resulting graph.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use tx_ext4_format::journal::{
    Jbd2Features, Jbd2MetadataUpdate, Jbd2Revoke, Jbd2TransactionImage, JBD2_BLOCK_SIZE,
};
use tx_ext4_format::mutation::Ext4MutationPlan;
use tx_ext4_format::pager::{JournalGeometry, Page4K};
use tx_ext4_format::Ext4FormatError;
use tx_substrate::zone::Cap;
use tx_subsystems::execution::{Errno, Guard, StepOutcome};
use tx_subsystems::fs_iface::{
    BackendBioDependency, BackendBioGraph, BackendBioGraphError, BackendBioNode, BackendBioNodeId,
    BackendPageCompletion, BackendPageRequest, BackendPlan, IoDataLeaseId, IoDataSource,
    PageFrameRef, WaitSourceId,
};
use tx_subsystems::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};
use tx_subsystems::io_manager::page::PageIoOp;
use tx_subsystems::mount::MountTransactionFrontier;
use tx_subsystems::page_backed::{
    AnonSwapPolicy, MaterializeAccess, PageCacheError, PageContainer, PageContainerKind, PageIndex,
    PageLease,
};

pub use crate::mutation_lifecycle::{JournalFsyncSource, JournalSettlementObserver};
use crate::planner::Ext4WritePlanSource;
use crate::sync::SpinMutex;
use crate::{adapter::wait_routing, adapter::wait_routing::WaitSource};

pub(crate) const METADATA_MUTATION_READY: u64 = 0x1;

/// One mount-local owner for ext4 metadata planning and journal admission.
///
/// Ownership is atomic rather than a borrowed spin-lock guard because L4
/// writeback retains it across asynchronous block-I/O completion.
pub struct JournalMetadataMutationAdmission {
    busy: AtomicBool,
    ready: Arc<WaitSource>,
}

impl JournalMetadataMutationAdmission {
    pub fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            ready: wait_routing::new_wait_source(),
        }
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<JournalMetadataMutationPermit> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| JournalMetadataMutationPermit {
                admission: Some(Arc::clone(self)),
            })
    }

    pub(crate) fn wait_source_id(&self) -> u64 {
        self.ready.id().raw()
    }
}

impl Default for JournalMetadataMutationAdmission {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for JournalMetadataMutationAdmission {
    fn drop(&mut self) {
        wait_routing::unregister_source(&self.ready);
    }
}

/// Owned proof that one foreground mutation or asynchronous writeback owns
/// the mount-local metadata admission point.
pub struct JournalMetadataMutationPermit {
    admission: Option<Arc<JournalMetadataMutationAdmission>>,
}

impl JournalMetadataMutationPermit {
    const fn detached() -> Self {
        Self { admission: None }
    }
}

impl Drop for JournalMetadataMutationPermit {
    fn drop(&mut self) {
        let Some(admission) = self.admission.take() else {
            return;
        };
        admission.busy.store(false, Ordering::Release);
        wait_routing::notify_all(&admission.ready, METADATA_MUTATION_READY);
    }
}

/// One L5-owned write buffer retained until its L6 completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalBio {
    pub plan: BioPlan,
    pub source: IoDataSource,
}

impl JournalBio {
    pub const fn new(plan: BioPlan, source: IoDataSource) -> Self {
        Self { plan, source }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalTransactionPlanError {
    CrossDevice,
    EmptyMetadata,
    EmptyWrite,
    NonWrite,
    TooManyNodes,
    Graph(BackendBioGraphError),
}

impl From<BackendBioGraphError> for JournalTransactionPlanError {
    fn from(value: BackendBioGraphError) -> Self {
        Self::Graph(value)
    }
}

/// A closed ordered-mode transaction before it is submitted to L6.
///
/// The caller owns every supplied `JournalBio` lease. `commit_graph` consumes
/// none of those leases and emits explicit device flushes around the commit.
/// A checkpoint is intentionally emitted as a separate graph after durable
/// commit completion.
#[derive(Debug)]
pub struct JournalTransactionPlan {
    sequence: u32,
    device: DeviceKey,
    data_writes: Vec<JournalBio>,
    descriptor: JournalBio,
    metadata_writes: Vec<JournalBio>,
    revokes: Vec<JournalBio>,
    commit: JournalBio,
    checkpoint_writes: Vec<JournalBio>,
    activation: Option<JournalBio>,
    clean: Option<JournalBio>,
}

impl JournalTransactionPlan {
    pub fn new(
        sequence: u32,
        data_writes: Vec<JournalBio>,
        descriptor: JournalBio,
        metadata_writes: Vec<JournalBio>,
        commit: JournalBio,
        checkpoint_writes: Vec<JournalBio>,
    ) -> Result<Self, JournalTransactionPlanError> {
        Self::with_revokes(
            sequence,
            data_writes,
            descriptor,
            metadata_writes,
            Vec::new(),
            commit,
            checkpoint_writes,
        )
    }

    pub fn with_revoke(
        sequence: u32,
        data_writes: Vec<JournalBio>,
        descriptor: JournalBio,
        metadata_writes: Vec<JournalBio>,
        revoke: Option<JournalBio>,
        commit: JournalBio,
        checkpoint_writes: Vec<JournalBio>,
    ) -> Result<Self, JournalTransactionPlanError> {
        Self::with_revokes(
            sequence,
            data_writes,
            descriptor,
            metadata_writes,
            revoke.into_iter().collect(),
            commit,
            checkpoint_writes,
        )
    }

    pub fn with_revokes(
        sequence: u32,
        data_writes: Vec<JournalBio>,
        descriptor: JournalBio,
        metadata_writes: Vec<JournalBio>,
        revokes: Vec<JournalBio>,
        commit: JournalBio,
        checkpoint_writes: Vec<JournalBio>,
    ) -> Result<Self, JournalTransactionPlanError> {
        if metadata_writes.is_empty() {
            return Err(JournalTransactionPlanError::EmptyMetadata);
        }
        let device = descriptor.plan.device;
        let plan = Self {
            sequence,
            device,
            data_writes,
            descriptor,
            metadata_writes,
            revokes,
            commit,
            checkpoint_writes,
            activation: None,
            clean: None,
        };
        plan.validate()?;
        Ok(plan)
    }

    pub fn with_superblock_state(mut self, activation: JournalBio, clean: JournalBio) -> Self {
        self.activation = Some(activation);
        self.clean = Some(clean);
        self
    }

    pub const fn sequence(&self) -> u32 {
        self.sequence
    }

    pub fn contains_data_writes(&self) -> bool {
        !self.data_writes.is_empty()
    }

    pub const fn device(&self) -> DeviceKey {
        self.device
    }

    /// Submit ordered file data and a durability fence before journal commit.
    pub fn data_graph(&self) -> Result<BackendBioGraph, JournalTransactionPlanError> {
        let mut builder = GraphBuilder::new();
        let mut data_ids = Vec::new();
        for write in &self.data_writes {
            data_ids.push(builder.push(write.clone())?);
        }
        let fence_id = builder.push(fence(self.device))?;
        for data in data_ids {
            builder.depends_on(data, fence_id);
        }
        builder.finish()
    }

    /// Submit journal descriptor, metadata after-images, and an explicit-flush
    /// commit sequence only after [`Self::data_graph`] has completed
    /// successfully. The mount has not yet admitted a device that proves FUA,
    /// so the graph deliberately uses the flush fallback.
    pub fn commit_graph_after_data(&self) -> Result<BackendBioGraph, JournalTransactionPlanError> {
        let mut builder = GraphBuilder::new();
        let activation_fence = self
            .activation
            .as_ref()
            .map(|activation| {
                let activation = builder.push(activation.clone())?;
                let fence = builder.push(fence(self.device))?;
                builder.depends_on(activation, fence);
                Ok::<_, JournalTransactionPlanError>(fence)
            })
            .transpose()?;
        let descriptor = builder.push(self.descriptor.clone())?;
        if let Some(activation_fence) = activation_fence {
            builder.depends_on(activation_fence, descriptor);
        }
        let mut journal_ids = Vec::new();
        journal_ids.push(descriptor);
        for write in &self.metadata_writes {
            journal_ids.push(builder.push(write.clone())?);
        }
        for revoke in &self.revokes {
            journal_ids.push(builder.push(revoke.clone())?);
        }
        let journal_fence = builder.push(fence(self.device))?;
        for journal in journal_ids {
            builder.depends_on(journal, journal_fence);
        }
        let commit = builder.push(self.commit.clone())?;
        builder.depends_on(journal_fence, commit);
        let commit_flush = builder.push(fence(self.device))?;
        builder.depends_on(commit, commit_flush);
        builder.finish()
    }

    /// Build the fsync-critical graph.
    ///
    /// L6 completion of this graph means all data writes preceded a durable
    /// journal commit record. It does not mean checkpointing has finished.
    pub fn commit_graph(&self) -> Result<BackendBioGraph, JournalTransactionPlanError> {
        let mut builder = GraphBuilder::new();
        let mut data_ids = Vec::new();
        for write in &self.data_writes {
            data_ids.push(builder.push(write.clone())?);
        }

        let data_fence = builder.push(fence(self.device))?;
        for data in data_ids {
            builder.depends_on(data, data_fence);
        }

        let descriptor = builder.push(self.descriptor.clone())?;
        if let Some(activation) = &self.activation {
            let activation = builder.push(activation.clone())?;
            builder.depends_on(data_fence, activation);
            let activation_fence = builder.push(fence(self.device))?;
            builder.depends_on(activation, activation_fence);
            builder.depends_on(activation_fence, descriptor);
        } else {
            builder.depends_on(data_fence, descriptor);
        }
        let mut journal_ids = Vec::new();
        journal_ids.push(descriptor);
        for write in &self.metadata_writes {
            let node = builder.push(write.clone())?;
            builder.depends_on(data_fence, node);
            journal_ids.push(node);
        }
        for revoke in &self.revokes {
            let node = builder.push(revoke.clone())?;
            builder.depends_on(data_fence, node);
            journal_ids.push(node);
        }

        let journal_fence = builder.push(fence(self.device))?;
        for journal in journal_ids {
            builder.depends_on(journal, journal_fence);
        }

        let commit = builder.push(self.commit.clone())?;
        builder.depends_on(journal_fence, commit);
        let commit_flush = builder.push(fence(self.device))?;
        builder.depends_on(commit, commit_flush);
        builder.finish()
    }

    /// Build home-location writes after a durable commit has completed.
    ///
    /// The caller must not submit this graph until `commit_graph` has completed
    /// successfully. Keeping the graph separate makes checkpoint latency
    /// non-critical to fsync completion.
    pub fn checkpoint_graph_after_commit(
        &self,
    ) -> Result<Option<BackendBioGraph>, JournalTransactionPlanError> {
        if self.checkpoint_writes.is_empty() && self.clean.is_none() {
            return Ok(None);
        }
        let mut builder = GraphBuilder::new();
        let mut home_ids = Vec::new();
        for write in &self.checkpoint_writes {
            home_ids.push(builder.push(write.clone())?);
        }
        let checkpoint_fence = builder.push(fence(self.device))?;
        for home in home_ids {
            builder.depends_on(home, checkpoint_fence);
        }
        if let Some(clean) = &self.clean {
            let clean = builder.push(clean.clone())?;
            builder.depends_on(checkpoint_fence, clean);
            let clean_flush = builder.push(fence(self.device))?;
            builder.depends_on(clean, clean_flush);
        }
        builder.finish().map(Some)
    }

    fn validate(&self) -> Result<(), JournalTransactionPlanError> {
        for write in self
            .data_writes
            .iter()
            .chain(core::iter::once(&self.descriptor))
            .chain(self.metadata_writes.iter())
            .chain(self.revokes.iter())
            .chain(core::iter::once(&self.commit))
            .chain(self.checkpoint_writes.iter())
        {
            if write.plan.device != self.device {
                return Err(JournalTransactionPlanError::CrossDevice);
            }
            if write.plan.op != BlockOp::Write {
                return Err(JournalTransactionPlanError::NonWrite);
            }
            if write.plan.lba.is_empty() || write.plan.vecs.is_empty() {
                return Err(JournalTransactionPlanError::EmptyWrite);
            }
        }
        Ok(())
    }
}

struct GraphBuilder {
    next_id: u64,
    nodes: Vec<BackendBioNode>,
    dependencies: Vec<BackendBioDependency>,
}

impl GraphBuilder {
    const fn new() -> Self {
        Self {
            next_id: 1,
            nodes: Vec::new(),
            dependencies: Vec::new(),
        }
    }

    fn push(&mut self, write: JournalBio) -> Result<BackendBioNodeId, JournalTransactionPlanError> {
        let id = BackendBioNodeId::new(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(JournalTransactionPlanError::TooManyNodes)?;
        self.nodes
            .push(BackendBioNode::new(id, write.plan, write.source));
        Ok(id)
    }

    fn depends_on(&mut self, before: BackendBioNodeId, after: BackendBioNodeId) {
        self.dependencies
            .push(BackendBioDependency::new(before, after));
    }

    fn finish(self) -> Result<BackendBioGraph, JournalTransactionPlanError> {
        Ok(BackendBioGraph::new(self.nodes, self.dependencies)?)
    }
}

fn fence(device: DeviceKey) -> JournalBio {
    JournalBio::new(
        BioPlan::new(
            device,
            BlockOp::Barrier,
            LbaRange::new(0, 0),
            Vec::new(),
            BlockFlags::BARRIER.union(BlockFlags::FLUSH),
        ),
        IoDataSource::None,
    )
}

/// Errors while staging encoded JBD2 records in a private metadata page pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalPagePoolError {
    Capacity,
    UnsupportedPageSize,
    Page(PageCacheError),
    FrameAddress,
    WouldBlock,
    Zone,
}

/// One page-backed JBD2 record retained until its L6 request has completed.
#[derive(Debug)]
pub struct JournalRecordLease {
    page: PageIndex,
    lease: PageLease,
    free: Arc<SpinMutex<Vec<PageIndex>>>,
}

impl Drop for JournalRecordLease {
    fn drop(&mut self) {
        self.free.lock().push(self.page);
    }
}

impl JournalRecordLease {
    pub const fn ppn(&self) -> tx_hal::Ppn {
        self.lease.ppn()
    }

    pub const fn page(&self) -> PageIndex {
        self.page
    }

    /// Build an L5-owned journal record write without relinquishing this lease.
    pub fn as_journal_bio(&self, device: DeviceKey, lba: LbaRange) -> JournalBio {
        let ppn = self.lease.ppn();
        JournalBio::new(
            BioPlan::new(
                device,
                BlockOp::Write,
                lba,
                alloc::vec![BioVec::new(ppn.0 as u64, 0, JBD2_BLOCK_SIZE as u32)],
                BlockFlags::EMPTY,
            ),
            IoDataSource::page_cache(
                IoDataLeaseId::new(self.page.as_u64().saturating_add(1)),
                PageFrameRef::new(ppn),
                0,
                JBD2_BLOCK_SIZE as u32,
            ),
        )
    }
}

/// Private persistent metadata pages used for journal descriptor/data/commit records.
///
/// A pool page is allocated once and remains owned by the PageContainer. Each
/// staged record additionally carries a `PageLease`; callers retain that lease
/// through graph completion before allowing the record to be recycled.
pub struct JournalPagePool {
    pages: Cap<PageContainer>,
    initialization: AtomicU8,
    next_page: AtomicU64,
    free: Arc<SpinMutex<Vec<PageIndex>>>,
}

impl JournalPagePool {
    const UNINITIALIZED: u8 = 0;
    const INITIALIZING: u8 = 1;
    const READY: u8 = 2;

    /// Pages needed to stage one owned mutation through checkpoint completion.
    /// Metadata has one journal copy and one home-checkpoint copy. Revoke
    /// records have only their journal copy; data is staged only when it is
    /// not retained by an L4-owned source.
    pub fn required_pages(
        owned_data_pages: usize,
        metadata_pages: usize,
        revoke_pages: usize,
        has_superblock_state: bool,
    ) -> Result<u64, JournalPagePoolError> {
        let superblock_state_pages = usize::from(has_superblock_state) * 2;
        let pages = owned_data_pages
            .checked_add(
                metadata_pages
                    .checked_mul(2)
                    .ok_or(JournalPagePoolError::Capacity)?,
            )
            .and_then(|pages| pages.checked_add(revoke_pages))
            .and_then(|pages| pages.checked_add(2))
            .and_then(|pages| pages.checked_add(superblock_state_pages))
            .ok_or(JournalPagePoolError::Capacity)?;
        u64::try_from(pages).map_err(|_| JournalPagePoolError::Capacity)
    }

    pub fn new(page_capacity: u64) -> Result<Self, JournalPagePoolError> {
        if tx_subsystems::vm::USER_PAGE_SIZE != JBD2_BLOCK_SIZE {
            return Err(JournalPagePoolError::UnsupportedPageSize);
        }
        let pages = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Persistent,
            },
            page_capacity,
        )
        .map_err(|_| JournalPagePoolError::Zone)?;
        Ok(Self {
            pages,
            initialization: AtomicU8::new(Self::UNINITIALIZED),
            next_page: AtomicU64::new(0),
            free: Arc::new(SpinMutex::new(Vec::new())),
        })
    }

    fn ensure_initialized(&self) -> Result<(), JournalPagePoolError> {
        if self.initialization.load(Ordering::Acquire) == Self::READY {
            return Ok(());
        }
        if self
            .initialization
            .compare_exchange(
                Self::UNINITIALIZED,
                Self::INITIALIZING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(JournalPagePoolError::WouldBlock);
        }
        match self.pages.materialize_private_anon_pages_batch() {
            Ok(()) => {
                self.initialization.store(Self::READY, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                self.initialization
                    .store(Self::UNINITIALIZED, Ordering::Release);
                Err(JournalPagePoolError::Page(error))
            }
        }
    }

    fn ensure_capacity(&self, required: u64) -> Result<(), JournalPagePoolError> {
        let free = self.free.lock().len() as u64;
        let fresh = self
            .pages
            .page_count()
            .saturating_sub(self.next_page.load(Ordering::Acquire));
        if free.saturating_add(fresh) < required {
            return Err(JournalPagePoolError::Capacity);
        }
        Ok(())
    }

    pub fn stage(
        &self,
        bytes: &[u8; JBD2_BLOCK_SIZE],
        guard: &Guard<'_>,
    ) -> Result<JournalRecordLease, JournalPagePoolError> {
        self.ensure_initialized()?;
        let page = self
            .free
            .lock()
            .pop()
            .unwrap_or_else(|| PageIndex::new(self.next_page.fetch_add(1, Ordering::AcqRel)));
        if page.as_u64() >= self.pages.page_count() {
            return Err(JournalPagePoolError::Capacity);
        }

        let materialized = self
            .pages
            .materialize_anon(page, MaterializeAccess::Write)
            .map_err(JournalPagePoolError::Page)?;
        let address = tx_substrate::page_allocator::frame_kernel_addr(materialized.ppn)
            .map_err(|_| JournalPagePoolError::FrameAddress)?;
        // `materialized.map_pin` keeps this page live and mapped until the copy
        // finishes. The PC's cache pin owns the frame afterwards.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), address, JBD2_BLOCK_SIZE);
        }
        drop(materialized);

        match self.pages.export_page_lease(page, guard) {
            StepOutcome::Done(lease) => Ok(JournalRecordLease {
                page,
                lease,
                free: Arc::clone(&self.free),
            }),
            StepOutcome::Err(errno) => {
                self.free.lock().push(page);
                Err(JournalPagePoolError::Page(PageCacheError::Backend(
                    errno.into(),
                )))
            }
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                self.free.lock().push(page);
                Err(JournalPagePoolError::WouldBlock)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalRecordLayout {
    pub descriptor: LbaRange,
    pub metadata: Vec<LbaRange>,
    pub revokes: Vec<LbaRange>,
    pub commit: LbaRange,
}

impl JournalRecordLayout {
    pub fn new(descriptor: LbaRange, metadata: Vec<LbaRange>, commit: LbaRange) -> Self {
        Self {
            descriptor,
            metadata,
            revokes: Vec::new(),
            commit,
        }
    }

    pub fn with_revoke(mut self, revoke: LbaRange) -> Self {
        self.revokes.push(revoke);
        self
    }

    pub fn with_revokes(mut self, revokes: Vec<LbaRange>) -> Self {
        self.revokes = revokes;
        self
    }
}

/// Mount-owned placement and identity needed to encode one mutation plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationJournalLayout {
    pub device: DeviceKey,
    pub sectors_per_block: u64,
    pub journal_uuid: [u8; 16],
    pub features: Jbd2Features,
    pub sequence: u32,
    pub records: JournalRecordLayout,
    pub superblock_state: Option<JournalSuperblockState>,
}

/// JBD2 state pages for one ring reservation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalSuperblockState {
    pub lba: LbaRange,
    pub activate: Page4K,
    pub clean: Page4K,
}

impl MutationJournalLayout {
    pub const fn new(
        device: DeviceKey,
        sectors_per_block: u64,
        journal_uuid: [u8; 16],
        sequence: u32,
        records: JournalRecordLayout,
    ) -> Self {
        Self {
            device,
            sectors_per_block,
            journal_uuid,
            features: Jbd2Features::REVOKE,
            sequence,
            records,
            superblock_state: None,
        }
    }

    pub fn with_superblock_state(mut self, state: JournalSuperblockState) -> Self {
        self.superblock_state = Some(state);
        self
    }

    /// Select the validated descriptor/revoke layout discovered from the
    /// journal superblock. The default constructor remains the legacy 32-bit
    /// revoke-capable test profile; production rings always overwrite it with
    /// discovery.
    pub const fn with_features(mut self, features: Jbd2Features) -> Self {
        self.features = features;
        self
    }

    fn block_lba(&self, physical_block: u64) -> Result<LbaRange, MutationJournalImageError> {
        if self.sectors_per_block == 0 {
            return Err(MutationJournalImageError::ZeroSectorsPerBlock);
        }
        let start_lba = physical_block
            .checked_mul(self.sectors_per_block)
            .ok_or(MutationJournalImageError::LbaOverflow)?;
        Ok(LbaRange::new(start_lba, self.sectors_per_block))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalRingError {
    InvalidGeometry,
    ZeroSectorsPerBlock,
    EmptyMetadata,
    TooLarge,
    LbaOverflow,
    Busy,
    Missing,
    ReservationMismatch,
    Superblock,
}

/// One journal record range retained until its checkpoint terminal result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalRingReservation {
    pub layout: MutationJournalLayout,
    id: u64,
    end: usize,
}

struct JournalRingState {
    cursor: usize,
    sequence: u32,
    next_reservation_id: u64,
    active: Option<JournalRingReservation>,
}

/// Mount-owned allocator for the logical JBD2 journal ring.
///
/// The ring grants at most one record at a time. That is intentional for the
/// current single-open-transaction runtime: a record is not reusable until its
/// home-block checkpoint succeeds, so allocation cannot outrun reclamation.
pub struct JournalRing {
    device: DeviceKey,
    sectors_per_block: u64,
    journal_uuid: [u8; 16],
    features: Jbd2Features,
    blocks: Vec<u64>,
    first: usize,
    superblock: tx_ext4_format::journal::Jbd2Superblock,
    superblock_page: Option<Page4K>,
    superblock_lba: Option<LbaRange>,
    state: SpinMutex<JournalRingState>,
}

impl JournalRing {
    pub fn new(
        device: DeviceKey,
        sectors_per_block: u64,
        geometry: JournalGeometry,
    ) -> Result<Self, JournalRingError> {
        let sequence = geometry.superblock.sequence;
        Self::with_sequence(device, sectors_per_block, geometry, sequence)
    }

    /// Construct a ring after mount-time replay has established the next
    /// transaction sequence. The record cursor is clean because replayed
    /// home blocks have been checkpointed before this runtime is exposed.
    pub fn with_sequence(
        device: DeviceKey,
        sectors_per_block: u64,
        geometry: JournalGeometry,
        sequence: u32,
    ) -> Result<Self, JournalRingError> {
        if sectors_per_block == 0 {
            return Err(JournalRingError::ZeroSectorsPerBlock);
        }
        let max_len = geometry.superblock.max_len as usize;
        let first = geometry.superblock.first as usize;
        if max_len != geometry.blocks.len() || max_len < 2 || first == 0 || first >= max_len {
            return Err(JournalRingError::InvalidGeometry);
        }
        let superblock_lba = geometry
            .superblock_page
            .as_ref()
            .map(|_| Self::lba_for_parts(&geometry.blocks, sectors_per_block, 0))
            .transpose()?;
        Ok(Self {
            device,
            sectors_per_block,
            journal_uuid: geometry.superblock.uuid,
            features: geometry.features,
            superblock: geometry.superblock,
            superblock_page: geometry.superblock_page,
            superblock_lba,
            blocks: geometry.blocks,
            first,
            state: SpinMutex::new(JournalRingState {
                cursor: first,
                sequence: sequence.max(1),
                next_reservation_id: 1,
                active: None,
            }),
        })
    }

    pub fn reserve(
        &self,
        metadata_blocks: usize,
    ) -> Result<JournalRingReservation, JournalRingError> {
        self.reserve_with_revoke_pages(metadata_blocks, 0)
    }

    pub fn reserve_for(
        &self,
        metadata_blocks: usize,
        has_revoke: bool,
    ) -> Result<JournalRingReservation, JournalRingError> {
        self.reserve_with_revoke_pages(metadata_blocks, usize::from(has_revoke))
    }

    pub fn reserve_with_revoke_pages(
        &self,
        metadata_blocks: usize,
        revoke_pages: usize,
    ) -> Result<JournalRingReservation, JournalRingError> {
        if metadata_blocks == 0 {
            return Err(JournalRingError::EmptyMetadata);
        }
        let record_blocks = metadata_blocks
            .checked_add(revoke_pages)
            .and_then(|count| count.checked_add(2))
            .ok_or(JournalRingError::TooLarge)?;
        if record_blocks > self.blocks.len() - self.first {
            return Err(JournalRingError::TooLarge);
        }
        let mut state = self.state.lock();
        if state.active.is_some() {
            return Err(JournalRingError::Busy);
        }
        let cursor = if state.cursor + record_blocks > self.blocks.len() {
            self.first
        } else {
            state.cursor
        };
        let end = cursor + record_blocks;
        let descriptor = self.lba_for(cursor)?;
        let mut metadata = Vec::new();
        for index in cursor + 1..cursor + 1 + metadata_blocks {
            metadata.push(self.lba_for(index)?);
        }
        let mut revokes = Vec::new();
        for index in cursor + 1 + metadata_blocks..end - 1 {
            revokes.push(self.lba_for(index)?);
        }
        let mut layout = MutationJournalLayout::new(
            self.device,
            self.sectors_per_block,
            self.journal_uuid,
            state.sequence,
            JournalRecordLayout::new(descriptor, metadata, self.lba_for(end - 1)?)
                .with_revokes(revokes),
        )
        .with_features(self.features);
        if let (Some(page), Some(lba)) = (self.superblock_page, self.superblock_lba) {
            let mut activate = page;
            self.superblock
                .write_state(&mut activate, state.sequence, cursor as u32)
                .map_err(|_| JournalRingError::Superblock)?;
            let mut clean = page;
            self.superblock
                .write_state(&mut clean, state.sequence.wrapping_add(1).max(1), 0)
                .map_err(|_| JournalRingError::Superblock)?;
            layout = layout.with_superblock_state(JournalSuperblockState {
                lba,
                activate,
                clean,
            });
        }
        let reservation = JournalRingReservation {
            layout,
            id: state.next_reservation_id,
            end,
        };
        state.next_reservation_id = state.next_reservation_id.wrapping_add(1).max(1);
        state.active = Some(reservation.clone());
        Ok(reservation)
    }

    /// Finish an active reservation after its checkpoint terminal result.
    ///
    /// A failed checkpoint leaves the cursor and sequence unchanged, allowing
    /// the exact same record to be retried without overwriting it.
    pub fn complete(
        &self,
        reservation: &JournalRingReservation,
        checkpoint_succeeded: bool,
    ) -> Result<(), JournalRingError> {
        let mut state = self.state.lock();
        let Some(active) = state.active.take() else {
            return Err(JournalRingError::Missing);
        };
        if &active != reservation {
            state.active = Some(active);
            return Err(JournalRingError::ReservationMismatch);
        }
        if checkpoint_succeeded {
            state.cursor = (active.end == self.blocks.len())
                .then_some(self.first)
                .unwrap_or(active.end);
            state.sequence = state.sequence.wrapping_add(1).max(1);
        }
        Ok(())
    }

    fn lba_for(&self, logical: usize) -> Result<LbaRange, JournalRingError> {
        Self::lba_for_parts(&self.blocks, self.sectors_per_block, logical)
    }

    fn revoke_page_count(&self, block_count: usize) -> usize {
        Jbd2Revoke::page_count_with_features(block_count, self.features)
    }

    fn lba_for_parts(
        blocks: &[u64],
        sectors_per_block: u64,
        logical: usize,
    ) -> Result<LbaRange, JournalRingError> {
        let physical = *blocks
            .get(logical)
            .ok_or(JournalRingError::InvalidGeometry)?;
        let start = physical
            .checked_mul(sectors_per_block)
            .ok_or(JournalRingError::LbaOverflow)?;
        Ok(LbaRange::new(start, sectors_per_block))
    }
}

/// One owned 4 KiB block write that will be staged into a mount-private pool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationBlockWrite {
    pub lba: LbaRange,
    pub bytes: Page4K,
}

/// Pure JBD2 and block-I/O projection of one immutable ext4 mutation plan.
///
/// This does not allocate frames or submit I/O. The later staging step retains
/// the owned bytes in page leases through durable commit completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationJournalImage {
    pub layout: MutationJournalLayout,
    pub image: Jbd2TransactionImage,
    pub data_writes: Vec<MutationBlockWrite>,
    pub checkpoint_writes: Vec<MutationBlockWrite>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationJournalImageError {
    EmptyMetadata,
    RecordLayout,
    MetadataHomeOutOfRange,
    RevokeHomeOutOfRange,
    ZeroSectorsPerBlock,
    LbaOverflow,
    Format(Ext4FormatError),
}

impl From<Ext4FormatError> for MutationJournalImageError {
    fn from(value: Ext4FormatError) -> Self {
        Self::Format(value)
    }
}

impl MutationJournalImage {
    pub fn from_plan(
        mutation: &Ext4MutationPlan,
        layout: MutationJournalLayout,
    ) -> Result<Self, MutationJournalImageError> {
        if mutation.metadata.is_empty() {
            return Err(MutationJournalImageError::EmptyMetadata);
        }
        if layout.records.metadata.len() != mutation.metadata.len()
            || layout.records.revokes.len()
                != Jbd2Revoke::page_count_with_features(mutation.revokes.len(), layout.features)
        {
            return Err(MutationJournalImageError::RecordLayout);
        }

        let mut data_writes = Vec::new();
        for write in &mutation.data {
            data_writes.push(MutationBlockWrite {
                lba: layout.block_lba(write.physical_block)?,
                bytes: write.bytes,
            });
        }

        let mut updates = Vec::new();
        let mut checkpoint_writes = Vec::new();
        for metadata in &mutation.metadata {
            if !layout.features.block_64bit && metadata.home > u32::MAX as u64 {
                return Err(MutationJournalImageError::MetadataHomeOutOfRange);
            }
            updates.push(Jbd2MetadataUpdate::new64(metadata.home, metadata.after));
            checkpoint_writes.push(MutationBlockWrite {
                lba: layout.block_lba(metadata.home)?,
                bytes: metadata.after,
            });
        }

        if !layout.features.block_64bit
            && mutation
                .revokes
                .iter()
                .any(|revoke| revoke.physical_block > u32::MAX as u64)
        {
            return Err(MutationJournalImageError::RevokeHomeOutOfRange);
        }
        let revoked_blocks = mutation
            .revokes
            .iter()
            .map(|revoke| revoke.physical_block)
            .collect();
        Ok(Self {
            image: Jbd2TransactionImage::encode_with_features_and_revokes(
                layout.sequence,
                layout.journal_uuid,
                updates,
                revoked_blocks,
                layout.features,
            )?,
            layout,
            data_writes,
            checkpoint_writes,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedJournalTransactionError {
    Layout,
    Pool(JournalPagePoolError),
    Plan(JournalTransactionPlanError),
}

pub struct PreparedJournalTransaction {
    plan: JournalTransactionPlan,
    records: Vec<JournalRecordLease>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalTransactionStateError {
    Busy,
    Missing,
    DataNotDurable,
    CommitNotSubmitted,
    NotCommitted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JournalTransactionPhase {
    Draft,
    DataDurable,
    CommitSubmitted,
    CommitDurable,
}

/// Mount-owned single-commit lifecycle. The stored value retains every record
/// lease until a durable commit completion authorizes checkpoint submission.
pub struct JournalTransactionState<T> {
    active: Option<(JournalTransactionPhase, T)>,
}

impl<T> JournalTransactionState<T> {
    pub const fn new() -> Self {
        Self { active: None }
    }
    pub fn begin(&mut self, transaction: T) -> Result<(), JournalTransactionStateError> {
        if self.active.is_some() {
            return Err(JournalTransactionStateError::Busy);
        }
        self.active = Some((JournalTransactionPhase::Draft, transaction));
        Ok(())
    }
    pub fn mark_data_durable(&mut self) -> Result<(), JournalTransactionStateError> {
        let Some((phase, _)) = self.active.as_mut() else {
            return Err(JournalTransactionStateError::Missing);
        };
        if *phase != JournalTransactionPhase::Draft {
            return Err(JournalTransactionStateError::DataNotDurable);
        }
        *phase = JournalTransactionPhase::DataDurable;
        Ok(())
    }
    pub fn mark_commit_submitted(&mut self) -> Result<(), JournalTransactionStateError> {
        let Some((phase, _)) = self.active.as_mut() else {
            return Err(JournalTransactionStateError::Missing);
        };
        if *phase != JournalTransactionPhase::DataDurable {
            return Err(JournalTransactionStateError::DataNotDurable);
        }
        *phase = JournalTransactionPhase::CommitSubmitted;
        Ok(())
    }
    pub fn mark_commit_durable(&mut self) -> Result<(), JournalTransactionStateError> {
        let Some((phase, _)) = self.active.as_mut() else {
            return Err(JournalTransactionStateError::Missing);
        };
        if *phase != JournalTransactionPhase::CommitSubmitted {
            return Err(JournalTransactionStateError::CommitNotSubmitted);
        }
        *phase = JournalTransactionPhase::CommitDurable;
        Ok(())
    }
    /// Compatibility transition for the existing combined data+commit graph.
    pub fn mark_combined_commit_durable(&mut self) -> Result<(), JournalTransactionStateError> {
        let Some((phase, _)) = self.active.as_mut() else {
            return Err(JournalTransactionStateError::Missing);
        };
        if *phase != JournalTransactionPhase::Draft {
            return Err(JournalTransactionStateError::CommitNotSubmitted);
        }
        *phase = JournalTransactionPhase::CommitDurable;
        Ok(())
    }
    pub fn active(&self) -> Option<&T> {
        self.active.as_ref().map(|(_, transaction)| transaction)
    }
    pub fn discard(&mut self) -> Option<T> {
        self.active.take().map(|(_, transaction)| transaction)
    }
    pub fn checkpoint_ready(&self) -> Result<Option<&T>, JournalTransactionStateError> {
        let Some((phase, transaction)) = self.active.as_ref() else {
            return Ok(None);
        };
        if *phase != JournalTransactionPhase::CommitDurable {
            return Err(JournalTransactionStateError::NotCommitted);
        }
        Ok(Some(transaction))
    }
    pub fn complete_checkpoint(&mut self) -> Result<Option<T>, JournalTransactionStateError> {
        self.checkpoint_ready()?;
        Ok(self.active.take().map(|(_, transaction)| transaction))
    }
    pub fn take_checkpoint_ready(&mut self) -> Result<Option<T>, JournalTransactionStateError> {
        self.complete_checkpoint()
    }
}

impl<T> Default for JournalTransactionState<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl PreparedJournalTransaction {
    pub fn sequence(&self) -> u32 {
        self.plan.sequence()
    }

    pub fn contains_data_writes(&self) -> bool {
        self.plan.contains_data_writes()
    }

    pub fn stage_mutation_with_data_sources(
        pool: &JournalPagePool,
        mutation: MutationJournalImage,
        data_sources: Vec<IoDataSource>,
        guard: &Guard<'_>,
    ) -> Result<Self, PreparedJournalTransactionError> {
        if mutation.data_writes.len() != data_sources.len()
            || mutation.layout.records.metadata.len() != mutation.image.metadata_blocks.len()
            || mutation.layout.records.revokes.len() != mutation.image.revokes.len()
        {
            return Err(PreparedJournalTransactionError::Layout);
        }
        let required_pages = JournalPagePool::required_pages(
            0,
            mutation.image.metadata_blocks.len(),
            mutation.image.revokes.len(),
            mutation.layout.superblock_state.is_some(),
        )
        .map_err(PreparedJournalTransactionError::Pool)?;
        pool.ensure_capacity(required_pages)
            .map_err(PreparedJournalTransactionError::Pool)?;
        let device = mutation.layout.device;
        let mut records = Vec::new();
        let mut data_writes = Vec::new();
        for (write, source) in mutation.data_writes.into_iter().zip(data_sources) {
            data_writes.push(journal_bio_from_l4_source(device, write.lba, source)?);
        }

        let descriptor = pool
            .stage(&mutation.image.descriptor, guard)
            .map_err(PreparedJournalTransactionError::Pool)?;
        let descriptor_bio = descriptor.as_journal_bio(device, mutation.layout.records.descriptor);
        records.push(descriptor);
        let mut metadata_writes = Vec::new();
        for (bytes, lba) in mutation
            .image
            .metadata_blocks
            .iter()
            .zip(mutation.layout.records.metadata.iter().copied())
        {
            let record = pool
                .stage(bytes, guard)
                .map_err(PreparedJournalTransactionError::Pool)?;
            metadata_writes.push(record.as_journal_bio(device, lba));
            records.push(record);
        }
        let mut revokes = Vec::new();
        for (bytes, lba) in mutation
            .image
            .revokes
            .iter()
            .zip(mutation.layout.records.revokes.iter().copied())
        {
            let record = pool
                .stage(bytes, guard)
                .map_err(PreparedJournalTransactionError::Pool)?;
            revokes.push(record.as_journal_bio(device, lba));
            records.push(record);
        }
        let commit = pool
            .stage(&mutation.image.commit, guard)
            .map_err(PreparedJournalTransactionError::Pool)?;
        let commit_bio = commit.as_journal_bio(device, mutation.layout.records.commit);
        records.push(commit);
        let mut checkpoint_writes = Vec::new();
        for write in mutation.checkpoint_writes {
            let record = pool
                .stage(&write.bytes, guard)
                .map_err(PreparedJournalTransactionError::Pool)?;
            checkpoint_writes.push(record.as_journal_bio(device, write.lba));
            records.push(record);
        }
        let superblock_state = stage_superblock_state(
            pool,
            device,
            mutation.layout.superblock_state.as_ref(),
            guard,
            &mut records,
        )?;
        let sequence = tx_ext4_format::journal::Jbd2Commit::parse(&mutation.image.commit)
            .map_err(|_| PreparedJournalTransactionError::Layout)?
            .header
            .sequence;
        let mut plan = JournalTransactionPlan::with_revokes(
            sequence,
            data_writes,
            descriptor_bio,
            metadata_writes,
            revokes,
            commit_bio,
            checkpoint_writes,
        )
        .map_err(PreparedJournalTransactionError::Plan)?;
        if let Some((activation, clean)) = superblock_state {
            plan = plan.with_superblock_state(activation, clean);
        }
        Ok(Self { plan, records })
    }

    /// Stage a complete immutable ext4 mutation into owned pool pages.
    ///
    /// Data pages feed the ordered-data phase, journal record pages feed the
    /// durable commit phase, and separate metadata pages feed checkpointing.
    /// Every page lease remains in `records` until the transaction is released.
    pub fn stage_mutation(
        pool: &JournalPagePool,
        mutation: MutationJournalImage,
        guard: &Guard<'_>,
    ) -> Result<Self, PreparedJournalTransactionError> {
        if mutation.layout.records.metadata.len() != mutation.image.metadata_blocks.len()
            || mutation.layout.records.revokes.len() != mutation.image.revokes.len()
        {
            return Err(PreparedJournalTransactionError::Layout);
        }
        let required_pages = JournalPagePool::required_pages(
            mutation.data_writes.len(),
            mutation.image.metadata_blocks.len(),
            mutation.image.revokes.len(),
            mutation.layout.superblock_state.is_some(),
        )
        .map_err(PreparedJournalTransactionError::Pool)?;
        pool.ensure_capacity(required_pages)
            .map_err(PreparedJournalTransactionError::Pool)?;

        let device = mutation.layout.device;
        let mut records = Vec::new();
        let mut data_writes = Vec::new();
        for write in mutation.data_writes {
            let record = pool
                .stage(&write.bytes, guard)
                .map_err(PreparedJournalTransactionError::Pool)?;
            data_writes.push(record.as_journal_bio(device, write.lba));
            records.push(record);
        }

        let descriptor = pool
            .stage(&mutation.image.descriptor, guard)
            .map_err(PreparedJournalTransactionError::Pool)?;
        let descriptor_bio = descriptor.as_journal_bio(device, mutation.layout.records.descriptor);
        records.push(descriptor);

        let mut metadata_writes = Vec::new();
        for (bytes, lba) in mutation
            .image
            .metadata_blocks
            .iter()
            .zip(mutation.layout.records.metadata.iter().copied())
        {
            let record = pool
                .stage(bytes, guard)
                .map_err(PreparedJournalTransactionError::Pool)?;
            metadata_writes.push(record.as_journal_bio(device, lba));
            records.push(record);
        }
        let mut revokes = Vec::new();
        for (bytes, lba) in mutation
            .image
            .revokes
            .iter()
            .zip(mutation.layout.records.revokes.iter().copied())
        {
            let record = pool
                .stage(bytes, guard)
                .map_err(PreparedJournalTransactionError::Pool)?;
            revokes.push(record.as_journal_bio(device, lba));
            records.push(record);
        }

        let commit = pool
            .stage(&mutation.image.commit, guard)
            .map_err(PreparedJournalTransactionError::Pool)?;
        let commit_bio = commit.as_journal_bio(device, mutation.layout.records.commit);
        records.push(commit);

        let mut checkpoint_writes = Vec::new();
        for write in mutation.checkpoint_writes {
            let record = pool
                .stage(&write.bytes, guard)
                .map_err(PreparedJournalTransactionError::Pool)?;
            checkpoint_writes.push(record.as_journal_bio(device, write.lba));
            records.push(record);
        }
        let superblock_state = stage_superblock_state(
            pool,
            device,
            mutation.layout.superblock_state.as_ref(),
            guard,
            &mut records,
        )?;

        let sequence = tx_ext4_format::journal::Jbd2Commit::parse(&mutation.image.commit)
            .map_err(|_| PreparedJournalTransactionError::Layout)?
            .header
            .sequence;
        let mut plan = JournalTransactionPlan::with_revokes(
            sequence,
            data_writes,
            descriptor_bio,
            metadata_writes,
            revokes,
            commit_bio,
            checkpoint_writes,
        )
        .map_err(PreparedJournalTransactionError::Plan)?;
        if let Some((activation, clean)) = superblock_state {
            plan = plan.with_superblock_state(activation, clean);
        }
        Ok(Self { plan, records })
    }

    pub fn stage(
        pool: &JournalPagePool,
        image: tx_ext4_format::journal::Jbd2TransactionImage,
        layout: JournalRecordLayout,
        device: DeviceKey,
        data_writes: Vec<JournalBio>,
        checkpoint_writes: Vec<JournalBio>,
        guard: &Guard<'_>,
    ) -> Result<Self, PreparedJournalTransactionError> {
        if layout.metadata.len() != image.metadata_blocks.len()
            || layout.revokes.len() != image.revokes.len()
        {
            return Err(PreparedJournalTransactionError::Layout);
        }
        let descriptor = pool
            .stage(&image.descriptor, guard)
            .map_err(PreparedJournalTransactionError::Pool)?;
        let mut records = alloc::vec![descriptor];
        let mut metadata_writes = Vec::new();
        for (bytes, lba) in image
            .metadata_blocks
            .iter()
            .zip(layout.metadata.iter().copied())
        {
            let record = pool
                .stage(bytes, guard)
                .map_err(PreparedJournalTransactionError::Pool)?;
            metadata_writes.push(record.as_journal_bio(device, lba));
            records.push(record);
        }
        let mut revokes = Vec::new();
        for (bytes, lba) in image.revokes.iter().zip(layout.revokes.iter().copied()) {
            let record = pool
                .stage(bytes, guard)
                .map_err(PreparedJournalTransactionError::Pool)?;
            revokes.push(record.as_journal_bio(device, lba));
            records.push(record);
        }
        let commit = pool
            .stage(&image.commit, guard)
            .map_err(PreparedJournalTransactionError::Pool)?;
        let descriptor_bio = records[0].as_journal_bio(device, layout.descriptor);
        let commit_bio = commit.as_journal_bio(device, layout.commit);
        records.push(commit);
        let plan = JournalTransactionPlan::with_revokes(
            tx_ext4_format::journal::Jbd2Commit::parse(&image.commit)
                .map_err(|_| PreparedJournalTransactionError::Layout)?
                .header
                .sequence,
            data_writes,
            descriptor_bio,
            metadata_writes,
            revokes,
            commit_bio,
            checkpoint_writes,
        )
        .map_err(PreparedJournalTransactionError::Plan)?;
        Ok(Self { plan, records })
    }

    pub fn plan(&self) -> &JournalTransactionPlan {
        &self.plan
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }
}

fn stage_superblock_state(
    pool: &JournalPagePool,
    device: DeviceKey,
    state: Option<&JournalSuperblockState>,
    guard: &Guard<'_>,
    records: &mut Vec<JournalRecordLease>,
) -> Result<Option<(JournalBio, JournalBio)>, PreparedJournalTransactionError> {
    let Some(state) = state else {
        return Ok(None);
    };
    let activate = pool
        .stage(&state.activate, guard)
        .map_err(PreparedJournalTransactionError::Pool)?;
    let activate_bio = activate.as_journal_bio(device, state.lba);
    records.push(activate);
    let clean = pool
        .stage(&state.clean, guard)
        .map_err(PreparedJournalTransactionError::Pool)?;
    let clean_bio = clean.as_journal_bio(device, state.lba);
    records.push(clean);
    Ok(Some((activate_bio, clean_bio)))
}

fn journal_bio_from_l4_source(
    device: DeviceKey,
    lba: LbaRange,
    source: IoDataSource,
) -> Result<JournalBio, PreparedJournalTransactionError> {
    let vecs = match &source {
        IoDataSource::PageCache {
            frame, offset, len, ..
        } if *len == JBD2_BLOCK_SIZE as u32 => {
            vec![BioVec::new(frame.ppn().0 as u64, *offset, *len)]
        }
        IoDataSource::Direct { vecs, .. } if !vecs.is_empty() => vecs.clone(),
        _ => {
            return Err(PreparedJournalTransactionError::Plan(
                JournalTransactionPlanError::EmptyWrite,
            ));
        }
    };
    Ok(JournalBio::new(
        BioPlan::new(device, BlockOp::Write, lba, vecs, BlockFlags::EMPTY),
        source,
    ))
}

/// Mount-owned admission path from immutable ext4 mutations into JBD2 state.
///
/// PageBacked and VFS never observe its journal records or pool leases; they
/// only hand ext4 a completed mutation plan at the writeback boundary.
pub struct JournalMutationRuntime {
    source: Arc<JournalFsyncSource>,
    pool: JournalPagePool,
    layout: Option<MutationJournalLayout>,
    ring: Option<Arc<JournalRing>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalMutationRuntimeError {
    Image(MutationJournalImageError),
    Stage(PreparedJournalTransactionError),
    Busy(JournalTransactionStateError),
    Settlement(Errno),
}

/// L5 metadata owner for a single immutable ext4 writeback mutation.
///
/// The provider may inspect its own inode/extent/bitmap state to build a
/// plan, but it never receives an L4 frame or page-cache lease.
pub trait Ext4MutationPlanSource: Send + Sync + 'static {
    /// Acquire the mount-wide metadata owner. Test and compatibility sources
    /// without a live mount use a detached permit.
    fn try_acquire_writeback_admission(
        &self,
    ) -> Result<JournalMetadataMutationPermit, WaitSourceId> {
        Ok(JournalMetadataMutationPermit::detached())
    }

    /// Finish a preceding transaction after its ordered-data graph completed
    /// and before admitting another writeback mutation.
    fn settle_prior_writeback_mutation(
        &self,
        _runtime: &JournalMutationRuntime,
    ) -> Result<(), Errno> {
        Err(Errno::EBUSY)
    }

    fn plan_writeback_mutation(
        &self,
        request: &BackendPageRequest,
    ) -> Result<Ext4MutationPlan, Errno>;

    /// Publish the same immutable metadata after-images used by foreground
    /// namespace mutations once journal admission succeeds.
    fn stage_writeback_after_images(&self, _mutation: &Ext4MutationPlan) -> Result<(), Errno> {
        Ok(())
    }
}

/// Bridges a pure ext4 mutation planner to the mount-local JBD2 runtime.
///
/// L4 calls `prepare_writeback` while it still retains the data lease. The
/// data graph and terminal completion paths then operate only on request ids
/// and runtime state.
pub struct JournalMutationWriteSource<P> {
    planner: P,
    runtime: Arc<JournalMutationRuntime>,
}

impl<P> JournalMutationWriteSource<P> {
    pub const fn new(planner: P, runtime: Arc<JournalMutationRuntime>) -> Self {
        Self { planner, runtime }
    }
}

impl<P: Ext4MutationPlanSource> Ext4WritePlanSource for JournalMutationWriteSource<P> {
    fn prepare_writeback(
        &self,
        request: &BackendPageRequest,
        _guard: &Guard<'_>,
    ) -> Result<(), Errno> {
        if request.op != PageIoOp::Writeback || matches!(request.source, IoDataSource::None) {
            return Err(Errno::EINVAL);
        }
        split_writeback_data_sources(&request.source, request.range.page_count()).map(|_| ())
    }

    fn plan_writeback(
        &self,
        _geometry: crate::planner::Ext4BlockGeometry,
        request: &BackendPageRequest,
        _mapping: crate::planner::Ext4ReadMapping,
    ) -> BackendPlan {
        let permit = match self.planner.try_acquire_writeback_admission() {
            Ok(permit) => permit,
            Err(wait) => return BackendPlan::Yield(wait),
        };
        let mutation = match self.planner.plan_writeback_mutation(request) {
            Ok(mutation) => mutation,
            Err(errno) => return BackendPlan::Err(errno),
        };
        let data_sources =
            match split_writeback_data_sources(&request.source, request.range.page_count()) {
                Ok(sources) => sources,
                Err(errno) => return BackendPlan::Err(errno),
            };
        let guard = tx_substrate::epoch::borrow_current_guard()
            .unwrap_or_else(crate::adapter::step_engine::guard);
        match self
            .runtime
            .begin_mutation_with_data_sources(&mutation, data_sources.clone(), &guard)
        {
            Ok(()) => {}
            Err(JournalMutationRuntimeError::Busy(_)) => {
                if let Err(errno) = self.planner.settle_prior_writeback_mutation(&self.runtime) {
                    return BackendPlan::Err(errno);
                }
                if let Err(error) =
                    self.runtime
                        .begin_mutation_with_data_sources(&mutation, data_sources, &guard)
                {
                    return BackendPlan::Err(journal_mutation_runtime_errno(error));
                }
            }
            Err(error) => return BackendPlan::Err(journal_mutation_runtime_errno(error)),
        }
        if self.runtime.attach_metadata_admission(permit).is_err() {
            self.runtime.abort_unsubmitted_data(Errno::EBUSY);
            return BackendPlan::Err(Errno::EBUSY);
        }
        if let Err(errno) = self.planner.stage_writeback_after_images(&mutation) {
            self.runtime.abort_unsubmitted_data(errno);
            return BackendPlan::Err(errno);
        }
        let plan = self.runtime.plan_data(request);
        if matches!(plan, BackendPlan::Err(_)) {
            self.runtime.abort_unsubmitted_data(Errno::EIO);
        }
        plan
    }

    fn complete_writeback(&self, completion: BackendPageCompletion) {
        self.runtime.complete_data(completion);
    }
}

fn journal_mutation_runtime_errno(error: JournalMutationRuntimeError) -> Errno {
    match error {
        JournalMutationRuntimeError::Busy(_) => Errno::EBUSY,
        JournalMutationRuntimeError::Image(_) | JournalMutationRuntimeError::Stage(_) => Errno::EIO,
        JournalMutationRuntimeError::Settlement(errno) => errno,
    }
}

fn split_writeback_data_sources(
    source: &IoDataSource,
    page_count: u64,
) -> Result<Vec<IoDataSource>, Errno> {
    if page_count == 0 {
        return Err(Errno::EINVAL);
    }
    match source {
        IoDataSource::PageCache {
            lease,
            frame,
            offset,
            len,
        } if page_count == 1 && *len == JBD2_BLOCK_SIZE as u32 => {
            Ok(vec![IoDataSource::page_cache(
                *lease, *frame, *offset, *len,
            )])
        }
        IoDataSource::PageCacheSegments { lease, segments } => {
            if segments.len() != usize::try_from(page_count).map_err(|_| Errno::EINVAL)? {
                return Err(Errno::EINVAL);
            }
            let mut out = Vec::new();
            for segment in segments {
                if segment.offset != 0 || segment.len != JBD2_BLOCK_SIZE as u32 {
                    return Err(Errno::EINVAL);
                }
                out.push(IoDataSource::page_cache(
                    *lease,
                    segment.frame,
                    segment.offset,
                    segment.len,
                ));
            }
            Ok(out)
        }
        IoDataSource::Direct { lease, vecs } => {
            if vecs.len() != usize::try_from(page_count).map_err(|_| Errno::EINVAL)? {
                return Err(Errno::EINVAL);
            }
            let mut out = Vec::new();
            for vec in vecs {
                if vec.offset != 0 || vec.len != JBD2_BLOCK_SIZE as u32 {
                    return Err(Errno::EINVAL);
                }
                out.push(IoDataSource::direct(*lease, alloc::vec![*vec]));
            }
            Ok(out)
        }
        _ => Err(Errno::EINVAL),
    }
}

impl JournalMutationRuntime {
    pub fn new(
        source: Arc<JournalFsyncSource>,
        pool: JournalPagePool,
        layout: MutationJournalLayout,
    ) -> Self {
        Self {
            source,
            pool,
            layout: Some(layout),
            ring: None,
        }
    }

    pub fn with_ring(
        source: Arc<JournalFsyncSource>,
        pool: JournalPagePool,
        ring: Arc<JournalRing>,
    ) -> Self {
        Self {
            source,
            pool,
            layout: None,
            ring: Some(ring),
        }
    }

    pub fn from_geometry(
        source: Arc<JournalFsyncSource>,
        pool: JournalPagePool,
        device: DeviceKey,
        sectors_per_block: u64,
        geometry: JournalGeometry,
    ) -> Result<Self, JournalRingError> {
        let sequence = geometry.superblock.sequence;
        Self::from_geometry_with_sequence(
            source,
            pool,
            device,
            sectors_per_block,
            geometry,
            sequence,
        )
    }

    pub fn from_geometry_with_sequence(
        source: Arc<JournalFsyncSource>,
        pool: JournalPagePool,
        device: DeviceKey,
        sectors_per_block: u64,
        geometry: JournalGeometry,
        sequence: u32,
    ) -> Result<Self, JournalRingError> {
        Ok(Self::with_ring(
            source,
            pool,
            Arc::new(JournalRing::with_sequence(
                device,
                sectors_per_block,
                geometry,
                sequence,
            )?),
        ))
    }

    pub fn source(&self) -> Arc<JournalFsyncSource> {
        Arc::clone(&self.source)
    }

    pub fn snapshot_transaction_frontier(&self) -> MountTransactionFrontier {
        self.source.active_transaction_frontier()
    }

    pub fn begin_mutation(
        &self,
        mutation: &Ext4MutationPlan,
        guard: &Guard<'_>,
    ) -> Result<(), JournalMutationRuntimeError> {
        let (layout, reservation) = self.layout_for(mutation)?;
        let image = match MutationJournalImage::from_plan(mutation, layout) {
            Ok(image) => image,
            Err(error) => {
                return Err(self
                    .release_reservation(reservation, JournalMutationRuntimeError::Image(error)));
            }
        };
        let transaction = match PreparedJournalTransaction::stage_mutation(&self.pool, image, guard)
        {
            Ok(transaction) => transaction,
            Err(error) => {
                return Err(self
                    .release_reservation(reservation, JournalMutationRuntimeError::Stage(error)));
            }
        };
        self.begin_transaction(transaction, reservation, mutation.deferred_frees.clone())
    }

    pub fn begin_mutation_with_data_sources(
        &self,
        mutation: &Ext4MutationPlan,
        data_sources: Vec<IoDataSource>,
        guard: &Guard<'_>,
    ) -> Result<(), JournalMutationRuntimeError> {
        let (layout, reservation) = self.layout_for(mutation)?;
        let image = match MutationJournalImage::from_plan(mutation, layout) {
            Ok(image) => image,
            Err(error) => {
                return Err(self
                    .release_reservation(reservation, JournalMutationRuntimeError::Image(error)));
            }
        };
        let transaction = match PreparedJournalTransaction::stage_mutation_with_data_sources(
            &self.pool,
            image,
            data_sources,
            guard,
        ) {
            Ok(transaction) => transaction,
            Err(error) => {
                return Err(self
                    .release_reservation(reservation, JournalMutationRuntimeError::Stage(error)));
            }
        };
        self.begin_transaction(transaction, reservation, mutation.deferred_frees.clone())
    }

    pub fn plan_data(&self, request: &BackendPageRequest) -> BackendPlan {
        self.source.plan_data(request)
    }

    pub fn complete_data(&self, completion: BackendPageCompletion) {
        self.source.complete_data(completion);
    }

    fn attach_metadata_admission(
        &self,
        permit: JournalMetadataMutationPermit,
    ) -> Result<(), JournalTransactionStateError> {
        self.source.attach_metadata_admission(permit)
    }

    pub(crate) fn abort_unsubmitted_data(&self, error: Errno) {
        self.source.abort_unsubmitted_data(error);
    }

    fn layout_for(
        &self,
        mutation: &Ext4MutationPlan,
    ) -> Result<
        (
            MutationJournalLayout,
            Option<(Arc<JournalRing>, JournalRingReservation)>,
        ),
        JournalMutationRuntimeError,
    > {
        if let Some(layout) = &self.layout {
            return Ok((layout.clone(), None));
        }
        let ring = self.ring.as_ref().expect("runtime has layout or ring");
        let reservation = ring
            .reserve_with_revoke_pages(
                mutation.metadata.len(),
                ring.revoke_page_count(mutation.revokes.len()),
            )
            .map_err(|_| JournalMutationRuntimeError::Busy(JournalTransactionStateError::Busy))?;
        Ok((
            reservation.layout.clone(),
            Some((Arc::clone(ring), reservation)),
        ))
    }

    fn begin_transaction(
        &self,
        transaction: PreparedJournalTransaction,
        reservation: Option<(Arc<JournalRing>, JournalRingReservation)>,
        deferred_frees: Vec<tx_ext4_format::mutation::DeferredFreeClaim>,
    ) -> Result<(), JournalMutationRuntimeError> {
        match reservation {
            Some((ring, reservation)) => match self.source.begin_with_ring(
                transaction,
                Arc::clone(&ring),
                reservation.clone(),
                deferred_frees,
            ) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = ring.complete(&reservation, false);
                    Err(error)
                }
            },
            None => self
                .source
                .begin_with_deferred_frees(transaction, deferred_frees),
        }
        .map_err(JournalMutationRuntimeError::Busy)
    }

    fn release_reservation(
        &self,
        reservation: Option<(Arc<JournalRing>, JournalRingReservation)>,
        error: JournalMutationRuntimeError,
    ) -> JournalMutationRuntimeError {
        if let Some((ring, reservation)) = reservation {
            let _ = ring.complete(&reservation, false);
        }
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use tx_ext4_format::journal::Jbd2Superblock;
    use tx_ext4_format::mutation::{
        Ext4MutationPlan, FsyncStamp, MetaRole, MetadataBlock, MutationOrigin, SealedDataWrite,
    };
    use tx_ext4_format::pager::JournalGeometry;
    use tx_hal::Ppn;
    use tx_subsystems::fs_iface::PageCacheSegment;

    #[test]
    fn split_writeback_page_cache_segments_preserves_order_frames_and_lease() {
        let lease = IoDataLeaseId::new(17);
        let first = PageCacheSegment::new(PageFrameRef::new(Ppn(0x41)), 0, 4096);
        let second = PageCacheSegment::new(PageFrameRef::new(Ppn(0x42)), 0, 4096);
        let source = IoDataSource::page_cache_segments(lease, [first, second].into());

        assert_eq!(
            split_writeback_data_sources(&source, 2),
            Ok(vec![
                IoDataSource::page_cache(lease, first.frame, first.offset, first.len),
                IoDataSource::page_cache(lease, second.frame, second.offset, second.len),
            ])
        );
    }

    #[test]
    fn split_writeback_page_cache_segments_rejects_invalid_shapes() {
        let lease = IoDataLeaseId::new(18);
        let valid = PageCacheSegment::new(PageFrameRef::new(Ppn(0x51)), 0, 4096);
        let cases = [
            (2, IoDataSource::page_cache_segments(lease, [valid].into())),
            (
                1,
                IoDataSource::page_cache_segments(
                    lease,
                    [PageCacheSegment::new(valid.frame, 1, 4096)].into(),
                ),
            ),
            (
                1,
                IoDataSource::page_cache_segments(
                    lease,
                    [PageCacheSegment::new(valid.frame, 0, 4095)].into(),
                ),
            ),
        ];

        for (page_count, source) in cases {
            assert_eq!(
                split_writeback_data_sources(&source, page_count),
                Err(Errno::EINVAL)
            );
        }
    }

    #[test]
    fn journal_pool_sizing_counts_owned_data_metadata_checkpoint_and_state_pages() {
        assert_eq!(JournalPagePool::required_pages(1, 2, 0, true).unwrap(), 9);
        assert_eq!(JournalPagePool::required_pages(0, 1, 0, false).unwrap(), 4);
        assert_eq!(JournalPagePool::required_pages(0, 1, 2, false).unwrap(), 6);
    }

    #[test]
    fn journal_ring_reservation_waits_for_checkpoint_and_wraps_without_crossing_tail() {
        let geometry = JournalGeometry {
            superblock: Jbd2Superblock {
                block_type: 4,
                block_size: JBD2_BLOCK_SIZE as u32,
                max_len: 8,
                first: 1,
                sequence: 11,
                start: 0,
                uuid: [0x3c; 16],
            },
            features: Jbd2Features::REVOKE,
            blocks: vec![40, 41, 42, 50, 51, 52, 53, 54],
            superblock_page: None,
        };
        let ring = JournalRing::new(DeviceKey::new(9), 8, geometry).expect("valid journal ring");

        let first = ring.reserve(2).expect("reserve first record");
        assert_eq!(first.layout.sequence, 11);
        assert_eq!(first.layout.records.descriptor, LbaRange::new(41 * 8, 8));
        assert_eq!(
            first.layout.records.metadata,
            vec![LbaRange::new(42 * 8, 8), LbaRange::new(50 * 8, 8)]
        );
        assert_eq!(first.layout.records.commit, LbaRange::new(51 * 8, 8));
        assert_eq!(ring.reserve(1), Err(JournalRingError::Busy));
        ring.complete(&first, true)
            .expect("checkpoint releases record");

        let wrapped = ring.reserve(3).expect("reserve wrapped record");
        assert_eq!(wrapped.layout.sequence, 12);
        assert_eq!(wrapped.layout.records.descriptor, LbaRange::new(41 * 8, 8));
        assert_eq!(
            wrapped.layout.records.metadata,
            vec![
                LbaRange::new(42 * 8, 8),
                LbaRange::new(50 * 8, 8),
                LbaRange::new(51 * 8, 8)
            ]
        );
        assert_eq!(wrapped.layout.records.commit, LbaRange::new(52 * 8, 8));
        ring.complete(&wrapped, false)
            .expect("failed checkpoint retains record");

        let retry = ring.reserve(3).expect("retry retains cursor and sequence");
        assert_ne!(
            retry, wrapped,
            "retry must receive a fresh reservation identity"
        );
        assert_eq!(
            ring.complete(&wrapped, true),
            Err(JournalRingError::ReservationMismatch)
        );
        ring.complete(&retry, true)
            .expect("current retry completes after stale completion rejection");
    }

    #[test]
    fn journal_ring_places_every_revoke_page_before_commit() {
        let geometry = JournalGeometry {
            superblock: Jbd2Superblock {
                block_type: 4,
                block_size: JBD2_BLOCK_SIZE as u32,
                max_len: 8,
                first: 1,
                sequence: 11,
                start: 0,
                uuid: [0x3c; 16],
            },
            features: Jbd2Features::REVOKE,
            blocks: vec![40, 41, 42, 50, 51, 52, 53, 54],
            superblock_page: None,
        };
        let ring = JournalRing::new(DeviceKey::new(9), 8, geometry).expect("valid journal ring");

        let reservation = ring
            .reserve_with_revoke_pages(1, 2)
            .expect("reserve metadata and two revoke records");

        assert_eq!(
            reservation.layout.records.descriptor,
            LbaRange::new(41 * 8, 8)
        );
        assert_eq!(
            reservation.layout.records.metadata,
            vec![LbaRange::new(42 * 8, 8)]
        );
        assert_eq!(
            reservation.layout.records.revokes,
            vec![LbaRange::new(50 * 8, 8), LbaRange::new(51 * 8, 8)]
        );
        assert_eq!(reservation.layout.records.commit, LbaRange::new(52 * 8, 8));
    }

    #[test]
    fn journal_ring_reservation_prepares_activation_and_clean_superblock_pages() {
        let superblock = Jbd2Superblock {
            block_type: 4,
            block_size: JBD2_BLOCK_SIZE as u32,
            max_len: 8,
            first: 1,
            sequence: 11,
            start: 0,
            uuid: [0x3c; 16],
        };
        let mut superblock_page = [0; JBD2_BLOCK_SIZE];
        superblock_page[..4].copy_from_slice(&tx_ext4_format::journal::JBD2_MAGIC.to_be_bytes());
        superblock_page[4..8].copy_from_slice(&superblock.block_type.to_be_bytes());
        superblock_page[8..12].copy_from_slice(&0u32.to_be_bytes());
        superblock_page[12..16].copy_from_slice(&superblock.block_size.to_be_bytes());
        superblock_page[16..20].copy_from_slice(&superblock.max_len.to_be_bytes());
        superblock_page[20..24].copy_from_slice(&superblock.first.to_be_bytes());
        superblock_page[24..28].copy_from_slice(&superblock.sequence.to_be_bytes());
        superblock_page[28..32].copy_from_slice(&superblock.start.to_be_bytes());
        superblock_page[48..64].copy_from_slice(&superblock.uuid);
        let ring = JournalRing::new(
            DeviceKey::new(9),
            8,
            JournalGeometry {
                superblock,
                features: Jbd2Features::REVOKE,
                blocks: vec![40, 41, 42, 50, 51, 52, 53, 54],
                superblock_page: Some(superblock_page),
            },
        )
        .unwrap();

        let reservation = ring.reserve(1).unwrap();
        let state = reservation.layout.superblock_state.as_ref().unwrap();

        assert_eq!(state.lba, LbaRange::new(40 * 8, 8));
        assert_eq!(
            Jbd2Superblock::parse(&state.activate).unwrap(),
            Jbd2Superblock {
                sequence: 11,
                start: 1,
                ..superblock
            }
        );
        assert_eq!(
            Jbd2Superblock::parse(&state.clean).unwrap(),
            Jbd2Superblock {
                sequence: 12,
                start: 0,
                ..superblock
            }
        );
    }

    #[test]
    fn mutation_journal_image_separates_ordered_data_and_metadata_checkpoint_writes() {
        let mut mutation = Ext4MutationPlan::new(MutationOrigin::FlushPage, 12, FsyncStamp::new(7));
        mutation.data.push(SealedDataWrite {
            logical_page: 3,
            physical_block: 7,
            bytes: [0xD3; JBD2_BLOCK_SIZE],
        });
        mutation
            .push_metadata(MetadataBlock {
                home: 5,
                role: MetaRole::BlockBitmap,
                before_version: 1,
                after: [0xB5; JBD2_BLOCK_SIZE],
                depends_on: Vec::new(),
            })
            .unwrap();
        mutation
            .push_metadata(MetadataBlock {
                home: 6,
                role: MetaRole::InodeTable,
                before_version: 2,
                after: [0xC6; JBD2_BLOCK_SIZE],
                depends_on: Vec::new(),
            })
            .unwrap();
        let layout = MutationJournalLayout::new(
            DeviceKey::new(9),
            8,
            [0xAB; 16],
            44,
            JournalRecordLayout::new(
                LbaRange::new(100, 8),
                vec![LbaRange::new(108, 8), LbaRange::new(116, 8)],
                LbaRange::new(124, 8),
            ),
        );

        let image = MutationJournalImage::from_plan(&mutation, layout).unwrap();

        assert_eq!(image.data_writes.len(), 1);
        assert_eq!(image.data_writes[0].lba, LbaRange::new(56, 8));
        assert_eq!(image.data_writes[0].bytes, [0xD3; JBD2_BLOCK_SIZE]);
        assert_eq!(image.checkpoint_writes.len(), 2);
        assert_eq!(image.checkpoint_writes[0].lba, LbaRange::new(40, 8));
        assert_eq!(image.checkpoint_writes[1].lba, LbaRange::new(48, 8));
        let descriptor =
            tx_ext4_format::journal::Jbd2Descriptor::parse_legacy(&image.image.descriptor).unwrap();
        assert_eq!(descriptor.header.sequence, 44);
        assert_eq!(
            descriptor
                .tags
                .iter()
                .map(|tag| tag.target_block)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert_eq!(
            tx_ext4_format::journal::Jbd2Commit::parse(&image.image.commit)
                .unwrap()
                .header
                .sequence,
            44
        );
    }

    #[test]
    fn mutation_journal_image_roundtrips_64bit_descriptor_and_revoke_blocks() {
        let high_metadata = u64::from(u32::MAX) + 17;
        let high_revoke = u64::from(u32::MAX) + 33;
        let mut mutation = Ext4MutationPlan::new(MutationOrigin::Unlink, 12, FsyncStamp::new(9));
        mutation
            .push_metadata(MetadataBlock {
                home: high_metadata,
                role: MetaRole::InodeTable,
                before_version: 1,
                after: [0x6D; JBD2_BLOCK_SIZE],
                depends_on: Vec::new(),
            })
            .unwrap();
        mutation.defer_free(high_revoke);
        let features = Jbd2Features::REVOKE_64BIT;
        let layout = MutationJournalLayout::new(
            DeviceKey::new(9),
            8,
            [0xAB; 16],
            45,
            JournalRecordLayout::new(
                LbaRange::new(100, 8),
                vec![LbaRange::new(108, 8)],
                LbaRange::new(124, 8),
            )
            .with_revoke(LbaRange::new(116, 8)),
        )
        .with_features(features);

        let image = MutationJournalImage::from_plan(&mutation, layout).unwrap();
        let descriptor = tx_ext4_format::journal::Jbd2Descriptor::parse_with_features(
            &image.image.descriptor,
            features,
        )
        .unwrap();
        let revoke = tx_ext4_format::journal::Jbd2Revoke::parse_with_features(
            &image.image.revokes[0],
            features,
        )
        .unwrap();

        assert_eq!(descriptor.tags[0].target_block, high_metadata);
        assert_eq!(revoke.blocks, vec![high_revoke]);
        assert_eq!(
            image.checkpoint_writes[0].lba,
            LbaRange::new(high_metadata * 8, 8)
        );
    }
}
