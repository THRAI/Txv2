//! Mount-local ownership for one admitted ext4 mutation.
//!
//! Callbacks name only request IDs and results. This handle retains prepared
//! journal records and the ring extent until one terminal path consumes them.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use tx_subsystems::execution::Errno;
use tx_subsystems::fs_iface::{
    BackendBioGraph, BackendPageCompletion, BackendPageRequest, BackendPlan, PageCompletion,
    PageCompletionList,
};
use tx_subsystems::io_manager::page::{
    PageGeneration, PageIoCompletionKind, PageIoOp, PageIoRequestId, PageIoResult,
};
use tx_subsystems::mount::MountTransactionFrontier;

use crate::journal::{
    JournalMetadataMutationPermit, JournalRing, JournalRingReservation,
    JournalTransactionStateError, PreparedJournalTransaction,
};
use crate::planner::Ext4FsyncPlanSource;
use crate::sync::SpinMutex;
use tx_ext4_format::mutation::DeferredFreeClaim;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MutationPhase {
    Admitted,
    Prepared,
    CommitPending,
    CommittedNeedsSettlement,
    CheckpointPending,
    TailReclaimPending,
    AbortRequested,
    AbortDraining,
    Settled,
    RolledBack,
    CommitUnknown,
    RecoveryOnly,
}

struct JournalExtentToken {
    ring: Arc<JournalRing>,
    reservation: JournalRingReservation,
}

impl JournalExtentToken {
    fn reclaim(&self, checkpoint_succeeded: bool) -> Result<(), JournalTransactionStateError> {
        self.ring
            .complete(&self.reservation, checkpoint_succeeded)
            .map_err(|_| JournalTransactionStateError::NotCommitted)
    }
}

/// All cross-yield state of one admitted mutation. It stores no `Guard`,
/// borrowed metadata claim, or L4 data lease; L4 owns data bundles and this
/// owner keeps only their terminal request IDs.
pub(crate) struct MutationHandle {
    phase: MutationPhase,
    transaction: PreparedJournalTransaction,
    metadata_admission: Option<JournalMetadataMutationPermit>,
    journal_extent: Option<JournalExtentToken>,
    data_request: Option<PageIoRequestId>,
    commit_request: Option<PageIoRequestId>,
    deferred_frees: Vec<DeferredFreeClaim>,
    last_error: Option<Errno>,
}

pub(crate) enum MutationTerminal {
    Retained,
    Release,
}

/// Mount-owned cache and mapping invalidation after a durable checkpoint.
///
/// The source stores only a weak observer so the planner's strong source
/// reference cannot keep a mounted backend alive after unmount.
pub trait JournalSettlementObserver: Send + Sync {
    fn settle_after_checkpoint(&self);
}

/// Ext4 mount-owned bridge from L4/L6 callbacks to one `MutationHandle`.
pub struct JournalFsyncSource {
    state: SpinMutex<JournalFsyncSourceState>,
}

struct JournalFsyncSourceState {
    mutation: Option<MutationHandle>,
    checkpoint_submitted: bool,
    recovery_only: bool,
    mount_error: Option<Errno>,
    settlement_observer: Option<Weak<dyn JournalSettlementObserver>>,
}

impl JournalFsyncSource {
    pub const fn new() -> Self {
        Self {
            state: SpinMutex::new(JournalFsyncSourceState {
                mutation: None,
                checkpoint_submitted: false,
                recovery_only: false,
                mount_error: None,
                settlement_observer: None,
            }),
        }
    }

    pub fn begin(
        &self,
        transaction: PreparedJournalTransaction,
    ) -> Result<(), JournalTransactionStateError> {
        self.begin_with_deferred_frees(transaction, Vec::new())
    }

    pub fn begin_with_deferred_frees(
        &self,
        transaction: PreparedJournalTransaction,
        deferred_frees: Vec<DeferredFreeClaim>,
    ) -> Result<(), JournalTransactionStateError> {
        self.begin_handle(MutationHandle::admit(transaction, None, deferred_frees))
    }

    pub fn begin_with_ring(
        &self,
        transaction: PreparedJournalTransaction,
        ring: Arc<JournalRing>,
        reservation: JournalRingReservation,
        deferred_frees: Vec<DeferredFreeClaim>,
    ) -> Result<(), JournalTransactionStateError> {
        self.begin_handle(MutationHandle::admit(
            transaction,
            Some((ring, reservation)),
            deferred_frees,
        ))
    }

    fn begin_handle(&self, mutation: MutationHandle) -> Result<(), JournalTransactionStateError> {
        let mut state = self.state.lock();
        if state.mutation.is_some() {
            return Err(JournalTransactionStateError::Busy);
        }
        state.mutation = Some(mutation);
        Ok(())
    }

    pub(crate) fn attach_metadata_admission(
        &self,
        permit: JournalMetadataMutationPermit,
    ) -> Result<(), JournalTransactionStateError> {
        let mut state = self.state.lock();
        let Some(mutation) = state.mutation.as_mut() else {
            return Err(JournalTransactionStateError::Busy);
        };
        mutation.attach_metadata_admission(permit)
    }

    pub fn bind_settlement_observer(&self, observer: Arc<dyn JournalSettlementObserver>) {
        self.state.lock().settlement_observer = Some(Arc::downgrade(&observer));
    }

    pub fn plan_data(&self, request: &BackendPageRequest) -> BackendPlan {
        if request.op != PageIoOp::Writeback {
            return BackendPlan::Err(Errno::EINVAL);
        }
        let mut state = self.state.lock();
        if state.recovery_only {
            return BackendPlan::Err(Errno::EIO);
        }
        let Some(mutation) = state.mutation.as_mut() else {
            return BackendPlan::Err(Errno::EAGAIN);
        };
        match mutation.prepare_data(request.id) {
            Ok(graph) => BackendPlan::SubmitGraph(graph),
            Err(JournalTransactionStateError::Busy) => BackendPlan::Err(Errno::EBUSY),
            Err(_) => BackendPlan::Err(Errno::EIO),
        }
    }

    pub fn complete_data(&self, completion: BackendPageCompletion) {
        if completion.op != PageIoOp::Writeback {
            return;
        }
        let mut state = self.state.lock();
        let Some(mutation) = state.mutation.as_mut() else {
            return;
        };
        if matches!(
            mutation.complete_data(completion.id, completion.result),
            MutationTerminal::Release
        ) {
            state.mutation = None;
        }
    }

    /// Abort a transaction whose data graph could not be handed to L6.
    /// Once a request id has been registered, the ordinary completion path is
    /// the sole owner and this helper deliberately leaves the handle intact.
    pub(crate) fn abort_unsubmitted_data(&self, error: Errno) {
        let mut state = self.state.lock();
        let Some(mutation) = state.mutation.as_mut() else {
            return;
        };
        if matches!(
            mutation.abort_unsubmitted_data(error),
            MutationTerminal::Release
        ) {
            state.mutation = None;
        }
    }

    /// Builds one post-commit graph without releasing retained record leases.
    pub fn take_checkpoint_graph(
        &self,
    ) -> Result<Option<BackendBioGraph>, JournalTransactionStateError> {
        let mut state = self.state.lock();
        if state.checkpoint_submitted {
            return Err(JournalTransactionStateError::Busy);
        }
        let Some(mutation) = state.mutation.as_mut() else {
            return Ok(None);
        };
        let graph = mutation.prepare_checkpoint()?;
        state.checkpoint_submitted = graph.is_some();
        Ok(graph)
    }

    pub fn complete_checkpoint_result(
        &self,
        result: Result<(), Errno>,
    ) -> Result<(), JournalTransactionStateError> {
        let mut state = self.state.lock();
        if !state.checkpoint_submitted {
            return Err(JournalTransactionStateError::NotCommitted);
        }
        state.checkpoint_submitted = false;
        if let Err(error) = result {
            state.mount_error = Some(error);
            let Some(mutation) = state.mutation.as_mut() else {
                return Err(JournalTransactionStateError::NotCommitted);
            };
            let _ = mutation.complete_checkpoint(Err(error))?;
            return Ok(());
        }
        let Some(mutation) = state.mutation.as_mut() else {
            return Err(JournalTransactionStateError::NotCommitted);
        };
        mutation.complete_checkpoint(Ok(()))?;
        let observer = state.settlement_observer.as_ref().and_then(Weak::upgrade);
        drop(state);
        if let Some(observer) = observer {
            observer.settle_after_checkpoint();
        }
        let mut state = self.state.lock();
        let Some(mutation) = state.mutation.as_mut() else {
            return Err(JournalTransactionStateError::NotCommitted);
        };
        if matches!(mutation.complete_tail_reclaim()?, MutationTerminal::Release) {
            state.mutation = None;
        }
        Ok(())
    }

    /// Records an ambiguous commit completion. Recovery remains the only
    /// authority that may later release the retained journal extent.
    pub fn commit_unknown(&self, request: PageIoRequestId, error: Errno) {
        let mut state = self.state.lock();
        let Some(mutation) = state.mutation.as_mut() else {
            return;
        };
        if mutation.commit_unknown(request, error) {
            state.recovery_only = true;
            state.mount_error = Some(error);
        }
    }

    pub fn mount_error(&self) -> Option<Errno> {
        self.state.lock().mount_error
    }

    pub fn active_transaction_frontier(&self) -> MountTransactionFrontier {
        self.state
            .lock()
            .mutation
            .as_ref()
            .map(MutationHandle::transaction_frontier)
            .unwrap_or_default()
    }

    #[doc(hidden)]
    pub fn try_reuse_for_test(&self, physical_block: u64) -> Result<(), Errno> {
        if self
            .state
            .lock()
            .mutation
            .as_ref()
            .is_some_and(|mutation| mutation.retains_deferred_free(physical_block))
        {
            Err(Errno::EBUSY)
        } else {
            Ok(())
        }
    }

    pub fn complete_checkpoint(&self) -> Result<(), JournalTransactionStateError> {
        self.complete_checkpoint_result(Ok(()))
    }
}

impl Default for JournalFsyncSource {
    fn default() -> Self {
        Self::new()
    }
}

impl MutationHandle {
    pub(crate) fn admit(
        transaction: PreparedJournalTransaction,
        ring: Option<(Arc<JournalRing>, JournalRingReservation)>,
        deferred_frees: Vec<DeferredFreeClaim>,
    ) -> Self {
        let phase = if transaction.contains_data_writes() {
            MutationPhase::Admitted
        } else {
            MutationPhase::Prepared
        };
        Self {
            phase,
            transaction,
            metadata_admission: None,
            journal_extent: ring
                .map(|(ring, reservation)| JournalExtentToken { ring, reservation }),
            data_request: None,
            commit_request: None,
            deferred_frees,
            last_error: None,
        }
    }

    /// Transfer the mount-local metadata owner into the journal lifecycle.
    ///
    /// Ordered-data completion is not terminal: commit, checkpoint, cache
    /// settlement, and tail reclaim still own the same transaction. Keeping
    /// the permit here makes later metadata operations wait on admission
    /// instead of entering the retained transaction and observing `EBUSY`.
    pub(crate) fn attach_metadata_admission(
        &mut self,
        permit: JournalMetadataMutationPermit,
    ) -> Result<(), JournalTransactionStateError> {
        if self.metadata_admission.is_some() {
            return Err(JournalTransactionStateError::Busy);
        }
        self.metadata_admission = Some(permit);
        Ok(())
    }

    fn transaction_frontier(&self) -> MountTransactionFrontier {
        MountTransactionFrontier::new(self.transaction.sequence() as u64)
    }

    pub(crate) fn prepare_data(
        &mut self,
        request: PageIoRequestId,
    ) -> Result<BackendBioGraph, JournalTransactionStateError> {
        if self.phase == MutationPhase::Admitted {
            self.phase = MutationPhase::Prepared;
        }
        if self.phase != MutationPhase::Prepared || self.data_request.is_some() {
            return Err(JournalTransactionStateError::Busy);
        }
        let graph = self
            .transaction
            .plan()
            .data_graph()
            .map_err(|_| JournalTransactionStateError::DataNotDurable)?;
        self.data_request = Some(request);
        Ok(graph)
    }

    pub(crate) fn complete_data(
        &mut self,
        request: PageIoRequestId,
        result: PageIoResult,
    ) -> MutationTerminal {
        if self.data_request != Some(request) {
            return MutationTerminal::Retained;
        }
        self.data_request = None;
        match result {
            PageIoResult::Done => MutationTerminal::Retained,
            PageIoResult::Err(error) => {
                self.last_error = Some(error);
                self.rollback_precommit();
                MutationTerminal::Release
            }
        }
    }

    fn abort_unsubmitted_data(&mut self, error: Errno) -> MutationTerminal {
        if self.data_request.is_some()
            || !matches!(
                self.phase,
                MutationPhase::Admitted | MutationPhase::Prepared
            )
        {
            return MutationTerminal::Retained;
        }
        self.last_error = Some(error);
        self.rollback_precommit();
        MutationTerminal::Release
    }

    pub(crate) fn prepare_commit(
        &mut self,
        request: PageIoRequestId,
    ) -> Result<BackendBioGraph, JournalTransactionStateError> {
        if self.phase != MutationPhase::Prepared
            || self.data_request.is_some()
            || self.commit_request.is_some()
        {
            return Err(JournalTransactionStateError::DataNotDurable);
        }
        let graph = self
            .transaction
            .plan()
            .commit_graph_after_data()
            .map_err(|_| JournalTransactionStateError::DataNotDurable)?;
        self.phase = MutationPhase::CommitPending;
        self.commit_request = Some(request);
        Ok(graph)
    }

    pub(crate) fn complete_commit(
        &mut self,
        request: PageIoRequestId,
        result: PageIoResult,
    ) -> MutationTerminal {
        if self.phase != MutationPhase::CommitPending || self.commit_request != Some(request) {
            return MutationTerminal::Retained;
        }
        self.commit_request = None;
        match result {
            PageIoResult::Done => {
                self.phase = MutationPhase::CommittedNeedsSettlement;
                MutationTerminal::Retained
            }
            PageIoResult::Err(error) => {
                self.last_error = Some(error);
                self.rollback_precommit();
                MutationTerminal::Release
            }
        }
    }

    pub(crate) fn prepare_checkpoint(
        &mut self,
    ) -> Result<Option<BackendBioGraph>, JournalTransactionStateError> {
        if self.phase != MutationPhase::CommittedNeedsSettlement {
            return Err(JournalTransactionStateError::NotCommitted);
        }
        let graph = self
            .transaction
            .plan()
            .checkpoint_graph_after_commit()
            .map_err(|_| JournalTransactionStateError::NotCommitted)?;
        if graph.is_some() {
            self.phase = MutationPhase::CheckpointPending;
        }
        Ok(graph)
    }

    pub(crate) fn complete_checkpoint(
        &mut self,
        result: Result<(), Errno>,
    ) -> Result<MutationTerminal, JournalTransactionStateError> {
        if self.phase != MutationPhase::CheckpointPending {
            return Err(JournalTransactionStateError::NotCommitted);
        }
        if let Err(error) = result {
            self.last_error = Some(error);
            self.phase = MutationPhase::CommittedNeedsSettlement;
            return Ok(MutationTerminal::Retained);
        }
        self.phase = MutationPhase::TailReclaimPending;
        Ok(MutationTerminal::Retained)
    }

    fn complete_tail_reclaim(&mut self) -> Result<MutationTerminal, JournalTransactionStateError> {
        if self.phase != MutationPhase::TailReclaimPending {
            return Err(JournalTransactionStateError::NotCommitted);
        }
        if let Some(extent) = self.journal_extent.as_ref() {
            extent.reclaim(true)?;
            self.journal_extent = None;
        }
        self.phase = MutationPhase::Settled;
        Ok(MutationTerminal::Release)
    }

    fn retains_deferred_free(&self, physical_block: u64) -> bool {
        self.deferred_frees
            .iter()
            .any(|claim| claim.physical_block == physical_block)
    }

    fn rollback_precommit(&mut self) {
        self.phase = MutationPhase::AbortRequested;
        self.phase = MutationPhase::AbortDraining;
        if let Some(extent) = self.journal_extent.take() {
            let _ = extent.reclaim(false);
        }
        self.phase = MutationPhase::RolledBack;
    }

    fn commit_unknown(&mut self, request: PageIoRequestId, error: Errno) -> bool {
        if self.phase != MutationPhase::CommitPending || self.commit_request != Some(request) {
            return false;
        }
        self.last_error = Some(error);
        self.phase = MutationPhase::CommitUnknown;
        self.phase = MutationPhase::RecoveryOnly;
        true
    }
}

impl Ext4FsyncPlanSource for JournalFsyncSource {
    fn plan_fsync(&self, request: &BackendPageRequest) -> BackendPlan {
        if request.op != PageIoOp::Fsync {
            return BackendPlan::Err(Errno::EINVAL);
        }
        let mut state = self.state.lock();
        if state.recovery_only {
            return BackendPlan::Err(Errno::EIO);
        }
        let Some(mutation) = state.mutation.as_mut() else {
            return BackendPlan::Complete(PageCompletionList::from_vec(alloc::vec![
                PageCompletion::new(
                    request.id,
                    request.range,
                    PageIoResult::Done,
                    request.generation_hint.unwrap_or(PageGeneration::new(0)),
                    PageIoCompletionKind::Noop,
                ),
            ]));
        };
        match mutation.prepare_commit(request.id) {
            Ok(graph) => BackendPlan::SubmitGraph(graph),
            Err(JournalTransactionStateError::DataNotDurable) => BackendPlan::Err(Errno::EAGAIN),
            Err(JournalTransactionStateError::Busy) => BackendPlan::Err(Errno::EBUSY),
            Err(_) => BackendPlan::Err(Errno::EIO),
        }
    }

    fn complete_fsync(&self, completion: BackendPageCompletion) {
        if completion.op != PageIoOp::Fsync {
            return;
        }
        let mut state = self.state.lock();
        let Some(mutation) = state.mutation.as_mut() else {
            return;
        };
        if matches!(
            mutation.complete_commit(completion.id, completion.result),
            MutationTerminal::Release
        ) {
            state.mutation = None;
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
