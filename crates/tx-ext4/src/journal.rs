//! Ext4-owned JBD2 ordered-mode transaction planning.
//!
//! This module converts already-owned write buffers into neutral L6 graphs. It
//! neither owns metadata/page caches nor executes I/O; L5 keeps transaction
//! state and L6 executes the resulting graph.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tx_ext4_format::journal::JBD2_BLOCK_SIZE;
use tx_substrate::zone::Cap;
use tx_subsystems::execution::{Guard, StepOutcome};
use tx_subsystems::fs_iface::{
    BackendBioDependency, BackendBioGraph, BackendBioGraphError, BackendBioNode, BackendBioNodeId,
    IoDataLeaseId, IoDataSource, PageFrameRef,
};
use tx_subsystems::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};
use tx_subsystems::page_backed::{
    AnonSwapPolicy, MaterializeAccess, PageCacheError, PageContainer, PageContainerKind, PageIndex,
    PageLease,
};

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
