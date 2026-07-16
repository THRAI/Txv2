//! Ext4-owned JBD2 ordered-mode transaction planning.
//!
//! This module converts already-owned write buffers into neutral L6 graphs. It
//! neither owns metadata/page caches nor executes I/O; L5 keeps transaction
//! state and L6 executes the resulting graph.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tx_ext4_format::journal::{Jbd2MetadataUpdate, Jbd2TransactionImage, JBD2_BLOCK_SIZE};
use tx_ext4_format::mutation::Ext4MutationPlan;
use tx_ext4_format::pager::{JournalGeometry, Page4K};
use tx_ext4_format::Ext4FormatError;
use tx_substrate::zone::Cap;
use tx_subsystems::execution::{Errno, Guard, StepOutcome};
use tx_subsystems::fs_iface::{
    BackendBioDependency, BackendBioGraph, BackendBioGraphError, BackendBioNode, BackendBioNodeId,
    BackendPageCompletion, BackendPageRequest, BackendPlan, IoDataLeaseId, IoDataSource,
    PageFrameRef,
};
use tx_subsystems::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};
use tx_subsystems::io_manager::page::{PageIoOp, PageIoRequestId, PageIoResult};
use tx_subsystems::page_backed::{
    AnonSwapPolicy, MaterializeAccess, PageCacheError, PageContainer, PageContainerKind, PageIndex,
    PageLease,
};

use crate::planner::{Ext4FsyncPlanSource, Ext4WritePlanSource};
use crate::sync::SpinMutex;

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
/// none of those leases and makes the commit write FUA-backed. A checkpoint is
/// intentionally emitted as a separate graph after durable commit completion.
#[derive(Debug)]
pub struct JournalTransactionPlan {
    sequence: u32,
    device: DeviceKey,
    data_writes: Vec<JournalBio>,
    descriptor: JournalBio,
    metadata_writes: Vec<JournalBio>,
    commit: JournalBio,
    checkpoint_writes: Vec<JournalBio>,
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
            commit,
            checkpoint_writes,
        };
        plan.validate()?;
        Ok(plan)
    }

    pub const fn sequence(&self) -> u32 {
        self.sequence
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

    /// Submit journal descriptor, metadata after-images, and FUA commit only
    /// after [`Self::data_graph`] has completed successfully.
    pub fn commit_graph_after_data(&self) -> Result<BackendBioGraph, JournalTransactionPlanError> {
        let mut builder = GraphBuilder::new();
        let descriptor = builder.push(self.descriptor.clone())?;
        let mut journal_ids = Vec::new();
        journal_ids.push(descriptor);
        for write in &self.metadata_writes {
            journal_ids.push(builder.push(write.clone())?);
        }
        let journal_fence = builder.push(fence(self.device))?;
        for journal in journal_ids {
            builder.depends_on(journal, journal_fence);
        }
        let mut commit = self.commit.clone();
        commit.plan.flags = commit.plan.flags.union(BlockFlags::FUA);
        let commit = builder.push(commit)?;
        builder.depends_on(journal_fence, commit);
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
        builder.depends_on(data_fence, descriptor);
        let mut journal_ids = Vec::new();
        journal_ids.push(descriptor);
        for write in &self.metadata_writes {
            let node = builder.push(write.clone())?;
            builder.depends_on(data_fence, node);
            journal_ids.push(node);
        }

        let journal_fence = builder.push(fence(self.device))?;
        for journal in journal_ids {
            builder.depends_on(journal, journal_fence);
        }

        let mut commit = self.commit.clone();
        commit.plan.flags = commit.plan.flags.union(BlockFlags::FUA);
        let commit = builder.push(commit)?;
        builder.depends_on(journal_fence, commit);
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
        if self.checkpoint_writes.is_empty() {
            return Ok(None);
        }
        let mut builder = GraphBuilder::new();
        for write in &self.checkpoint_writes {
            builder.push(write.clone())?;
        }
        builder.finish().map(Some)
    }

    fn validate(&self) -> Result<(), JournalTransactionPlanError> {
        for write in self
            .data_writes
            .iter()
            .chain(core::iter::once(&self.descriptor))
            .chain(self.metadata_writes.iter())
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
    next_page: AtomicU64,
}

impl JournalPagePool {
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
            next_page: AtomicU64::new(0),
        })
    }

    pub fn stage(
        &self,
        bytes: &[u8; JBD2_BLOCK_SIZE],
        guard: &Guard<'_>,
    ) -> Result<JournalRecordLease, JournalPagePoolError> {
        let page = PageIndex::new(self.next_page.fetch_add(1, Ordering::AcqRel));
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
            StepOutcome::Done(lease) => Ok(JournalRecordLease { page, lease }),
            StepOutcome::Err(errno) => Err(JournalPagePoolError::Page(PageCacheError::Backend(
                errno.into(),
            ))),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                Err(JournalPagePoolError::WouldBlock)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalRecordLayout {
    pub descriptor: LbaRange,
    pub metadata: Vec<LbaRange>,
    pub commit: LbaRange,
}

impl JournalRecordLayout {
    pub fn new(descriptor: LbaRange, metadata: Vec<LbaRange>, commit: LbaRange) -> Self {
        Self {
            descriptor,
            metadata,
            commit,
        }
    }
}

/// Mount-owned placement and identity needed to encode one mutation plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationJournalLayout {
    pub device: DeviceKey,
    pub sectors_per_block: u64,
    pub journal_uuid: [u8; 16],
    pub sequence: u32,
    pub records: JournalRecordLayout,
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
            sequence,
            records,
        }
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
    blocks: Vec<u64>,
    first: usize,
    state: SpinMutex<JournalRingState>,
}

impl JournalRing {
    pub fn new(
        device: DeviceKey,
        sectors_per_block: u64,
        geometry: JournalGeometry,
    ) -> Result<Self, JournalRingError> {
        if sectors_per_block == 0 {
            return Err(JournalRingError::ZeroSectorsPerBlock);
        }
        let max_len = geometry.superblock.max_len as usize;
        let first = geometry.superblock.first as usize;
        if max_len != geometry.blocks.len() || max_len < 2 || first == 0 || first >= max_len {
            return Err(JournalRingError::InvalidGeometry);
        }
        Ok(Self {
            device,
            sectors_per_block,
            journal_uuid: geometry.superblock.uuid,
            blocks: geometry.blocks,
            first,
            state: SpinMutex::new(JournalRingState {
                cursor: first,
                sequence: geometry.superblock.sequence,
                next_reservation_id: 1,
                active: None,
            }),
        })
    }

    pub fn reserve(
        &self,
        metadata_blocks: usize,
    ) -> Result<JournalRingReservation, JournalRingError> {
        if metadata_blocks == 0 {
            return Err(JournalRingError::EmptyMetadata);
        }
        let record_blocks = metadata_blocks
            .checked_add(2)
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
        for index in cursor + 1..end - 1 {
            metadata.push(self.lba_for(index)?);
        }
        let reservation = JournalRingReservation {
            layout: MutationJournalLayout::new(
                self.device,
                self.sectors_per_block,
                self.journal_uuid,
                state.sequence,
                JournalRecordLayout::new(descriptor, metadata, self.lba_for(end - 1)?),
            ),
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
            state.cursor = (active.end == self.blocks.len()).then_some(self.first).unwrap_or(active.end);
            state.sequence = state.sequence.wrapping_add(1).max(1);
        }
        Ok(())
    }

    fn lba_for(&self, logical: usize) -> Result<LbaRange, JournalRingError> {
        let physical = *self.blocks.get(logical).ok_or(JournalRingError::InvalidGeometry)?;
        let start = physical
            .checked_mul(self.sectors_per_block)
            .ok_or(JournalRingError::LbaOverflow)?;
        Ok(LbaRange::new(start, self.sectors_per_block))
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
        if layout.records.metadata.len() != mutation.metadata.len() {
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
            let home = u32::try_from(metadata.home)
                .map_err(|_| MutationJournalImageError::MetadataHomeOutOfRange)?;
            updates.push(Jbd2MetadataUpdate::new(home, metadata.after));
            checkpoint_writes.push(MutationBlockWrite {
                lba: layout.block_lba(metadata.home)?,
                bytes: metadata.after,
            });
        }

        Ok(Self {
            image: Jbd2TransactionImage::encode_legacy(
                layout.sequence,
                layout.journal_uuid,
                updates,
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
    pub fn stage_mutation_with_data_sources(
        pool: &JournalPagePool,
        mutation: MutationJournalImage,
        data_sources: Vec<IoDataSource>,
        guard: &Guard<'_>,
    ) -> Result<Self, PreparedJournalTransactionError> {
        if mutation.data_writes.len() != data_sources.len()
            || mutation.layout.records.metadata.len() != mutation.image.metadata_blocks.len()
        {
            return Err(PreparedJournalTransactionError::Layout);
        }
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
        let sequence = tx_ext4_format::journal::Jbd2Commit::parse(&mutation.image.commit)
            .map_err(|_| PreparedJournalTransactionError::Layout)?
            .header
            .sequence;
        let plan = JournalTransactionPlan::new(
            sequence,
            data_writes,
            descriptor_bio,
            metadata_writes,
            commit_bio,
            checkpoint_writes,
        )
        .map_err(PreparedJournalTransactionError::Plan)?;
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
        if mutation.layout.records.metadata.len() != mutation.image.metadata_blocks.len() {
            return Err(PreparedJournalTransactionError::Layout);
        }

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

        let sequence = tx_ext4_format::journal::Jbd2Commit::parse(&mutation.image.commit)
            .map_err(|_| PreparedJournalTransactionError::Layout)?
            .header
            .sequence;
        let plan = JournalTransactionPlan::new(
            sequence,
            data_writes,
            descriptor_bio,
            metadata_writes,
            commit_bio,
            checkpoint_writes,
        )
        .map_err(PreparedJournalTransactionError::Plan)?;
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
        if layout.metadata.len() != image.metadata_blocks.len() {
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
        let commit = pool
            .stage(&image.commit, guard)
            .map_err(PreparedJournalTransactionError::Pool)?;
        let descriptor_bio = records[0].as_journal_bio(device, layout.descriptor);
        let commit_bio = commit.as_journal_bio(device, layout.commit);
        records.push(commit);
        let plan = JournalTransactionPlan::new(
            tx_ext4_format::journal::Jbd2Commit::parse(&image.commit)
                .map_err(|_| PreparedJournalTransactionError::Layout)?
                .header
                .sequence,
            data_writes,
            descriptor_bio,
            metadata_writes,
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
            ))
        }
    };
    Ok(JournalBio::new(
        BioPlan::new(device, BlockOp::Write, lba, vecs, BlockFlags::EMPTY),
        source,
    ))
}

/// Ext4 mount-owned bridge from fsync requests to retained JBD2 transactions.
///
/// The source owns the prepared transaction and hence every journal-record
/// lease until the matching L4 graph completion makes the commit durable.
pub struct JournalFsyncSource {
    state: SpinMutex<JournalFsyncSourceState>,
}

struct JournalFsyncSourceState {
    transaction: JournalTransactionState<PreparedJournalTransaction>,
    data_submitted: Option<PageIoRequestId>,
    commit_submitted: Option<PageIoRequestId>,
    checkpoint_submitted: bool,
    ring_reservation: Option<(Arc<JournalRing>, JournalRingReservation)>,
}

impl JournalFsyncSource {
    pub const fn new() -> Self {
        Self {
            state: SpinMutex::new(JournalFsyncSourceState {
                transaction: JournalTransactionState::new(),
                data_submitted: None,
                commit_submitted: None,
                checkpoint_submitted: false,
                ring_reservation: None,
            }),
        }
    }

    pub fn begin(
        &self,
        transaction: PreparedJournalTransaction,
    ) -> Result<(), JournalTransactionStateError> {
        self.state.lock().transaction.begin(transaction)
    }

    pub fn begin_with_ring(
        &self,
        transaction: PreparedJournalTransaction,
        ring: Arc<JournalRing>,
        reservation: JournalRingReservation,
    ) -> Result<(), JournalTransactionStateError> {
        let mut state = self.state.lock();
        state.transaction.begin(transaction)?;
        state.ring_reservation = Some((ring, reservation));
        Ok(())
    }

    pub fn plan_data(&self, request: &BackendPageRequest) -> BackendPlan {
        if request.op != PageIoOp::Writeback {
            return BackendPlan::Err(tx_subsystems::execution::Errno::EINVAL);
        }
        let mut state = self.state.lock();
        if state.data_submitted.is_some() || state.commit_submitted.is_some() {
            return BackendPlan::Err(tx_subsystems::execution::Errno::EBUSY);
        }
        let Some(transaction) = state.transaction.active() else {
            return BackendPlan::Err(tx_subsystems::execution::Errno::EAGAIN);
        };
        let graph = match transaction.plan().data_graph() {
            Ok(graph) => graph,
            Err(_) => return BackendPlan::Err(tx_subsystems::execution::Errno::EIO),
        };
        state.data_submitted = Some(request.id);
        BackendPlan::SubmitGraph(graph)
    }

    pub fn complete_data(&self, completion: BackendPageCompletion) {
        if completion.op != PageIoOp::Writeback {
            return;
        }
        let mut state = self.state.lock();
        if state.data_submitted != Some(completion.id) {
            return;
        }
        state.data_submitted = None;
        match completion.result {
            PageIoResult::Done => {
                let _ = state.transaction.mark_data_durable();
            }
            PageIoResult::Err(_) => {
                let _ = state.transaction.discard();
            }
        }
    }

    /// Build the post-commit checkpoint graph while retaining transaction leases.
    pub fn take_checkpoint_graph(
        &self,
    ) -> Result<Option<BackendBioGraph>, JournalTransactionStateError> {
        let mut state = self.state.lock();
        if state.checkpoint_submitted {
            return Err(JournalTransactionStateError::Busy);
        }
        let Some(transaction) = state.transaction.checkpoint_ready()? else {
            return Ok(None);
        };
        let graph = transaction
            .plan()
            .checkpoint_graph_after_commit()
            .map_err(|_| JournalTransactionStateError::NotCommitted)?;
        state.checkpoint_submitted = graph.is_some();
        Ok(graph)
    }

    /// Record one checkpoint graph terminal result.
    ///
    /// An I/O error leaves the committed transaction and its record leases
    /// intact for a later checkpoint retry. Only a successful home-write graph
    /// releases journal space.
    pub fn complete_checkpoint_result(
        &self,
        result: Result<(), Errno>,
    ) -> Result<(), JournalTransactionStateError> {
        let mut state = self.state.lock();
        if !state.checkpoint_submitted {
            return Err(JournalTransactionStateError::NotCommitted);
        }
        state.checkpoint_submitted = false;
        if result.is_err() {
            return Ok(());
        }
        let _ = state.transaction.complete_checkpoint()?;
        let ring_reservation = state.ring_reservation.take();
        state.data_submitted = None;
        state.commit_submitted = None;
        drop(state);
        if let Some((ring, reservation)) = ring_reservation {
            ring.complete(&reservation, true)
                .map_err(|_| JournalTransactionStateError::NotCommitted)?;
        }
        Ok(())
    }

    /// Release retained transaction leases only after checkpoint I/O completes.
    pub fn complete_checkpoint(&self) -> Result<(), JournalTransactionStateError> {
        self.complete_checkpoint_result(Ok(()))
    }
}

impl Default for JournalFsyncSource {
    fn default() -> Self {
        Self::new()
    }
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
}

/// L5 metadata owner for a single immutable ext4 writeback mutation.
///
/// The provider may inspect its own inode/extent/bitmap state to build a
/// plan, but it never receives an L4 frame or page-cache lease.
pub trait Ext4MutationPlanSource: Send + Sync + 'static {
    fn plan_writeback_mutation(
        &self,
        request: &BackendPageRequest,
    ) -> Result<Ext4MutationPlan, Errno>;
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
        guard: &Guard<'_>,
    ) -> Result<(), Errno> {
        if request.op != PageIoOp::Writeback || matches!(request.source, IoDataSource::None) {
            return Err(Errno::EINVAL);
        }
        let mutation = self.planner.plan_writeback_mutation(request)?;
        self.runtime
            .begin_mutation_with_data_sources(&mutation, vec![request.source.clone()], guard)
            .map_err(journal_mutation_runtime_errno)
    }

    fn plan_writeback(
        &self,
        _geometry: crate::planner::Ext4BlockGeometry,
        request: &BackendPageRequest,
        _mapping: crate::planner::Ext4ReadMapping,
    ) -> BackendPlan {
        self.runtime.plan_data(request)
    }

    fn complete_writeback(&self, completion: BackendPageCompletion) {
        self.runtime.complete_data(completion);
    }
}

fn journal_mutation_runtime_errno(error: JournalMutationRuntimeError) -> Errno {
    match error {
        JournalMutationRuntimeError::Busy(_) => Errno::EBUSY,
        JournalMutationRuntimeError::Image(_) | JournalMutationRuntimeError::Stage(_) => Errno::EIO,
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

    pub fn with_ring(source: Arc<JournalFsyncSource>, pool: JournalPagePool, ring: Arc<JournalRing>) -> Self {
        Self { source, pool, layout: None, ring: Some(ring) }
    }

    pub fn source(&self) -> Arc<JournalFsyncSource> {
        Arc::clone(&self.source)
    }

    pub fn begin_mutation(
        &self,
        mutation: &Ext4MutationPlan,
        guard: &Guard<'_>,
    ) -> Result<(), JournalMutationRuntimeError> {
        let (layout, reservation) = self.layout_for(mutation)?;
        let image = match MutationJournalImage::from_plan(mutation, layout) {
            Ok(image) => image,
            Err(error) => return Err(self.release_reservation(reservation, JournalMutationRuntimeError::Image(error))),
        };
        let transaction = match PreparedJournalTransaction::stage_mutation(&self.pool, image, guard) {
            Ok(transaction) => transaction,
            Err(error) => return Err(self.release_reservation(reservation, JournalMutationRuntimeError::Stage(error))),
        };
        self.begin_transaction(transaction, reservation)
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
            Err(error) => return Err(self.release_reservation(reservation, JournalMutationRuntimeError::Image(error))),
        };
        let transaction = match PreparedJournalTransaction::stage_mutation_with_data_sources(
            &self.pool,
            image,
            data_sources,
            guard,
        ) {
            Ok(transaction) => transaction,
            Err(error) => return Err(self.release_reservation(reservation, JournalMutationRuntimeError::Stage(error))),
        };
        self.begin_transaction(transaction, reservation)
    }

    pub fn plan_data(&self, request: &BackendPageRequest) -> BackendPlan {
        self.source.plan_data(request)
    }

    pub fn complete_data(&self, completion: BackendPageCompletion) {
        self.source.complete_data(completion);
    }

    fn layout_for(&self, mutation: &Ext4MutationPlan) -> Result<(MutationJournalLayout, Option<(Arc<JournalRing>, JournalRingReservation)>), JournalMutationRuntimeError> {
        if let Some(layout) = &self.layout { return Ok((layout.clone(), None)); }
        let ring = self.ring.as_ref().expect("runtime has layout or ring");
        let reservation = ring.reserve(mutation.metadata.len()).map_err(|_| JournalMutationRuntimeError::Busy(JournalTransactionStateError::Busy))?;
        Ok((reservation.layout.clone(), Some((Arc::clone(ring), reservation))))
    }

    fn begin_transaction(&self, transaction: PreparedJournalTransaction, reservation: Option<(Arc<JournalRing>, JournalRingReservation)>) -> Result<(), JournalMutationRuntimeError> {
        match reservation {
            Some((ring, reservation)) => match self.source.begin_with_ring(transaction, Arc::clone(&ring), reservation.clone()) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = ring.complete(&reservation, false);
                    Err(error)
                }
            },
            None => self.source.begin(transaction),
        }.map_err(JournalMutationRuntimeError::Busy)
    }

    fn release_reservation(&self, reservation: Option<(Arc<JournalRing>, JournalRingReservation)>, error: JournalMutationRuntimeError) -> JournalMutationRuntimeError {
        if let Some((ring, reservation)) = reservation { let _ = ring.complete(&reservation, false); }
        error
    }
}

impl Ext4FsyncPlanSource for JournalFsyncSource {
    fn plan_fsync(&self, request: &BackendPageRequest) -> BackendPlan {
        if request.op != PageIoOp::Fsync {
            return BackendPlan::Err(tx_subsystems::execution::Errno::EINVAL);
        }
        let mut state = self.state.lock();
        if state.data_submitted.is_some() || state.commit_submitted.is_some() {
            return BackendPlan::Err(tx_subsystems::execution::Errno::EBUSY);
        }
        let Some(transaction) = state.transaction.active() else {
            return BackendPlan::Err(tx_subsystems::execution::Errno::EAGAIN);
        };
        let graph = match transaction.plan().commit_graph_after_data() {
            Ok(graph) => graph,
            Err(_) => return BackendPlan::Err(tx_subsystems::execution::Errno::EIO),
        };
        if state.transaction.mark_commit_submitted().is_err() {
            return BackendPlan::Err(tx_subsystems::execution::Errno::EBUSY);
        }
        state.commit_submitted = Some(request.id);
        BackendPlan::SubmitGraph(graph)
    }

    fn complete_fsync(&self, completion: BackendPageCompletion) {
        if completion.op != PageIoOp::Fsync {
            return;
        }
        let mut state = self.state.lock();
        if state.commit_submitted != Some(completion.id) {
            return;
        }
        state.commit_submitted = None;
        match completion.result {
            PageIoResult::Done => {
                let _ = state.transaction.mark_commit_durable();
            }
            PageIoResult::Err(_) => {
                let _ = state.transaction.discard();
            }
        }
    }

    fn take_background_graph(&self) -> Result<Option<BackendBioGraph>, Errno> {
        self.take_checkpoint_graph().map_err(|error| match error {
            JournalTransactionStateError::Busy => Errno::EBUSY,
            _ => Errno::EIO,
        })
    }

    fn complete_background_graph(&self, result: Result<(), Errno>) {
        let _ = self.complete_checkpoint_result(result);
    }
}

impl Ext4FsyncPlanSource for Arc<JournalFsyncSource> {
    fn plan_fsync(&self, request: &BackendPageRequest) -> BackendPlan {
        self.as_ref().plan_fsync(request)
    }

    fn complete_fsync(&self, completion: BackendPageCompletion) {
        self.as_ref().complete_fsync(completion);
    }

    fn take_background_graph(&self) -> Result<Option<BackendBioGraph>, Errno> {
        self.as_ref().take_background_graph()
    }

    fn complete_background_graph(&self, result: Result<(), Errno>) {
        self.as_ref().complete_background_graph(result);
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
            blocks: vec![40, 41, 42, 50, 51, 52, 53, 54],
        };
        let ring = JournalRing::new(DeviceKey::new(9), 8, geometry).expect("valid journal ring");

        let first = ring.reserve(2).expect("reserve first record");
        assert_eq!(first.layout.sequence, 11);
        assert_eq!(first.layout.records.descriptor, LbaRange::new(41 * 8, 8));
        assert_eq!(first.layout.records.metadata, vec![LbaRange::new(42 * 8, 8), LbaRange::new(50 * 8, 8)]);
        assert_eq!(first.layout.records.commit, LbaRange::new(51 * 8, 8));
        assert_eq!(ring.reserve(1), Err(JournalRingError::Busy));
        ring.complete(&first, true).expect("checkpoint releases record");

        let wrapped = ring.reserve(3).expect("reserve wrapped record");
        assert_eq!(wrapped.layout.sequence, 12);
        assert_eq!(wrapped.layout.records.descriptor, LbaRange::new(41 * 8, 8));
        assert_eq!(wrapped.layout.records.metadata, vec![LbaRange::new(42 * 8, 8), LbaRange::new(50 * 8, 8), LbaRange::new(51 * 8, 8)]);
        assert_eq!(wrapped.layout.records.commit, LbaRange::new(52 * 8, 8));
        ring.complete(&wrapped, false).expect("failed checkpoint retains record");

        let retry = ring.reserve(3).expect("retry retains cursor and sequence");
        assert_ne!(retry, wrapped, "retry must receive a fresh reservation identity");
        assert_eq!(
            ring.complete(&wrapped, true),
            Err(JournalRingError::ReservationMismatch)
        );
        ring.complete(&retry, true)
            .expect("current retry completes after stale completion rejection");
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
}
