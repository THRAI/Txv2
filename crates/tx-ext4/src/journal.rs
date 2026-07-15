//! Ext4-owned JBD2 ordered-mode transaction planning.
//!
//! This module converts already-owned write buffers into neutral L6 graphs. It
//! neither owns metadata/page caches nor executes I/O; L5 keeps transaction
//! state and L6 executes the resulting graph.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tx_ext4_format::journal::{Jbd2MetadataUpdate, Jbd2TransactionImage, JBD2_BLOCK_SIZE};
use tx_ext4_format::mutation::Ext4MutationPlan;
use tx_ext4_format::pager::Page4K;
use tx_ext4_format::Ext4FormatError;
use tx_substrate::zone::Cap;
use tx_subsystems::execution::{Guard, StepOutcome};
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

use crate::planner::Ext4FsyncPlanSource;
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
    NotCommitted,
}

/// Mount-owned single-commit lifecycle. The stored value retains every record
/// lease until a durable commit completion authorizes checkpoint submission.
pub struct JournalTransactionState<T> {
    active: Option<(bool, T)>,
}

impl<T> JournalTransactionState<T> {
    pub const fn new() -> Self {
        Self { active: None }
    }
    pub fn begin(&mut self, transaction: T) -> Result<(), JournalTransactionStateError> {
        if self.active.is_some() {
            return Err(JournalTransactionStateError::Busy);
        }
        self.active = Some((false, transaction));
        Ok(())
    }
    pub fn mark_commit_durable(&mut self) -> Result<(), JournalTransactionStateError> {
        let Some((committed, _)) = self.active.as_mut() else {
            return Err(JournalTransactionStateError::Missing);
        };
        *committed = true;
        Ok(())
    }
    pub fn active(&self) -> Option<&T> {
        self.active.as_ref().map(|(_, transaction)| transaction)
    }
    pub fn discard(&mut self) -> Option<T> {
        self.active.take().map(|(_, transaction)| transaction)
    }
    pub fn take_checkpoint_ready(&mut self) -> Result<Option<T>, JournalTransactionStateError> {
        let Some((committed, _)) = self.active.as_ref() else {
            return Ok(None);
        };
        if !committed {
            return Err(JournalTransactionStateError::NotCommitted);
        }
        Ok(self.active.take().map(|(_, transaction)| transaction))
    }
}

impl<T> Default for JournalTransactionState<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl PreparedJournalTransaction {
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

/// Ext4 mount-owned bridge from fsync requests to retained JBD2 transactions.
///
/// The source owns the prepared transaction and hence every journal-record
/// lease until the matching L4 graph completion makes the commit durable.
pub struct JournalFsyncSource {
    state: SpinMutex<JournalFsyncSourceState>,
}

struct JournalFsyncSourceState {
    transaction: JournalTransactionState<PreparedJournalTransaction>,
    submitted: Option<PageIoRequestId>,
}

impl JournalFsyncSource {
    pub const fn new() -> Self {
        Self {
            state: SpinMutex::new(JournalFsyncSourceState {
                transaction: JournalTransactionState::new(),
                submitted: None,
            }),
        }
    }

    pub fn begin(
        &self,
        transaction: PreparedJournalTransaction,
    ) -> Result<(), JournalTransactionStateError> {
        self.state.lock().transaction.begin(transaction)
    }

    /// Take the post-commit checkpoint graph after a matching durable commit.
    pub fn take_checkpoint_graph(&self) -> Result<Option<BackendBioGraph>, JournalTransactionStateError> {
        let mut state = self.state.lock();
        let Some(transaction) = state.transaction.take_checkpoint_ready()? else {
            return Ok(None);
        };
        state.submitted = None;
        transaction
            .plan()
            .checkpoint_graph_after_commit()
            .map_err(|_| JournalTransactionStateError::NotCommitted)
    }
}

impl Default for JournalFsyncSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Ext4FsyncPlanSource for JournalFsyncSource {
    fn plan_fsync(&self, request: &BackendPageRequest) -> BackendPlan {
        if request.op != PageIoOp::Fsync {
            return BackendPlan::Err(tx_subsystems::execution::Errno::EINVAL);
        }
        let mut state = self.state.lock();
        if state.submitted.is_some() {
            return BackendPlan::Err(tx_subsystems::execution::Errno::EBUSY);
        }
        let Some(transaction) = state.transaction.active() else {
            return BackendPlan::Err(tx_subsystems::execution::Errno::EAGAIN);
        };
        let graph = match transaction.plan().commit_graph() {
            Ok(graph) => graph,
            Err(_) => return BackendPlan::Err(tx_subsystems::execution::Errno::EIO),
        };
        state.submitted = Some(request.id);
        BackendPlan::SubmitGraph(graph)
    }

    fn complete_fsync(&self, completion: BackendPageCompletion) {
        if completion.op != PageIoOp::Fsync {
            return;
        }
        let mut state = self.state.lock();
        if state.submitted != Some(completion.id) {
            return;
        }
        state.submitted = None;
        match completion.result {
            PageIoResult::Done => {
                let _ = state.transaction.mark_commit_durable();
            }
            PageIoResult::Err(_) => {
                let _ = state.transaction.discard();
            }
        }
    }
}

impl Ext4FsyncPlanSource for Arc<JournalFsyncSource> {
    fn plan_fsync(&self, request: &BackendPageRequest) -> BackendPlan {
        self.as_ref().plan_fsync(request)
    }

    fn complete_fsync(&self, completion: BackendPageCompletion) {
        self.as_ref().complete_fsync(completion);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use tx_ext4_format::mutation::{
        Ext4MutationPlan, FsyncStamp, MetaRole, MetadataBlock, MutationOrigin, SealedDataWrite,
    };

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
