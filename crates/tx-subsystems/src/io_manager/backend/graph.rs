//! Dependency-aware admission for filesystem backend bio graphs.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::execution::Errno;
use crate::fs_iface::{BackendBioGraph, BackendBioNodeId, BioPlanList};
use crate::io_manager::block::{BlockRequestId, QueueError, SubmitOutcome};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendGraphAdvance {
    Pending { submitted: Vec<SubmitOutcome> },
    Complete(Result<(), Errno>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendGraphStage {
    Ready {
        nodes: Vec<BackendBioNodeId>,
        bios: BioPlanList,
    },
    Waiting,
    Complete(Result<(), Errno>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendGraphSchedulerError {
    UnknownBlockRequest(BlockRequestId),
    InvalidSubmissionReceipt,
    Queue(QueueError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeState {
    Pending,
    Staged,
    Submitted,
    Complete,
}

/// Owns graph-local dependency state. It never owns filesystem data frames or
/// driver tags; L4 retains those responsibilities.
#[derive(Debug)]
pub struct BackendGraphScheduler {
    graph: BackendBioGraph,
    states: BTreeMap<BackendBioNodeId, NodeState>,
    requests: BTreeMap<BlockRequestId, Vec<BackendBioNodeId>>,
    failure: Option<Errno>,
}

impl BackendGraphScheduler {
    pub fn new(graph: BackendBioGraph) -> Self {
        let states = graph
            .nodes()
            .iter()
            .map(|node| (node.id, NodeState::Pending))
            .collect();
        Self {
            graph,
            states,
            requests: BTreeMap::new(),
            failure: None,
        }
    }

    pub fn stage_ready(&mut self) -> BackendGraphStage {
        if self.failure.is_none() {
            let ready = self
                .graph
                .nodes()
                .iter()
                .filter(|node| self.states.get(&node.id) == Some(&NodeState::Pending))
                .filter(|node| self.dependencies_complete(node.id))
                .map(|node| (node.id, node.bio.clone()))
                .collect::<Vec<_>>();
            if !ready.is_empty() {
                let mut nodes = Vec::with_capacity(ready.len());
                let mut bios = Vec::with_capacity(ready.len());
                for (node, bio) in ready {
                    self.states.insert(node, NodeState::Staged);
                    nodes.push(node);
                    bios.push(bio);
                }
                return BackendGraphStage::Ready {
                    nodes,
                    bios: BioPlanList::from_vec(bios),
                };
            }
        }

        if self.is_terminal() {
            BackendGraphStage::Complete(match self.failure {
                Some(errno) => Err(errno),
                None => Ok(()),
            })
        } else {
            BackendGraphStage::Waiting
        }
    }

    pub fn apply_submission_receipt(
        &mut self,
        nodes: &[BackendBioNodeId],
        submitted: &[SubmitOutcome],
        failure: Option<Errno>,
    ) -> Result<BackendGraphAdvance, BackendGraphSchedulerError> {
        if submitted.len() > nodes.len()
            || (failure.is_none() && submitted.len() != nodes.len())
            || nodes
                .iter()
                .any(|node| self.states.get(node) != Some(&NodeState::Staged))
        {
            return Err(BackendGraphSchedulerError::InvalidSubmissionReceipt);
        }

        for (node, outcome) in nodes.iter().copied().zip(submitted.iter().copied()) {
            let request = match outcome {
                SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
            };
            self.requests.entry(request).or_default().push(node);
            self.states.insert(node, NodeState::Submitted);
        }
        for node in nodes.iter().skip(submitted.len()) {
            self.states.insert(*node, NodeState::Pending);
        }
        if let Some(errno) = failure {
            self.failure.get_or_insert(errno);
        }

        if self.is_terminal() {
            Ok(BackendGraphAdvance::Complete(match self.failure {
                Some(errno) => Err(errno),
                None => Ok(()),
            }))
        } else {
            Ok(BackendGraphAdvance::Pending {
                submitted: submitted.to_vec(),
            })
        }
    }

    pub fn complete(
        &mut self,
        request: BlockRequestId,
        result: Result<(), Errno>,
    ) -> Result<BackendGraphStage, BackendGraphSchedulerError> {
        let Some(nodes) = self.requests.remove(&request) else {
            return Err(BackendGraphSchedulerError::UnknownBlockRequest(request));
        };
        for node in nodes {
            self.states.insert(node, NodeState::Complete);
        }
        if let Err(errno) = result {
            self.failure.get_or_insert(errno);
        }
        Ok(self.stage_ready())
    }

    pub fn is_terminal(&self) -> bool {
        self.requests.is_empty()
            && (self.failure.is_some()
                || self
                    .states
                    .values()
                    .all(|state| *state == NodeState::Complete))
    }

    pub fn handles_request(&self, request: BlockRequestId) -> bool {
        self.requests.contains_key(&request)
    }

    fn dependencies_complete(&self, node: BackendBioNodeId) -> bool {
        self.graph
            .dependencies()
            .iter()
            .filter(|dependency| dependency.after == node)
            .all(|dependency| self.states.get(&dependency.before) == Some(&NodeState::Complete))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_iface::{BackendBioDependency, BackendBioNode, IoDataSource};
    use crate::io_manager::block::{
        BioPlan, BioVec, BlockFlags, BlockOp, BlockRequestId, DeviceKey, LbaRange,
    };

    fn node(id: u64, lba: u64) -> BackendBioNode {
        BackendBioNode::new(
            BackendBioNodeId::new(id),
            BioPlan::new(
                DeviceKey::new(7),
                BlockOp::Write,
                LbaRange::new(lba, 1),
                alloc::vec![BioVec::new(id, 0, 512)],
                BlockFlags::EMPTY,
            ),
            IoDataSource::None,
        )
    }

    fn submit_staged(
        scheduler: &mut BackendGraphScheduler,
        request_ids: &[u64],
    ) -> BackendGraphAdvance {
        let BackendGraphStage::Ready { nodes, bios } = scheduler.stage_ready() else {
            panic!("graph has no ready nodes");
        };
        assert_eq!(nodes.len(), request_ids.len());
        assert_eq!(bios.as_slice().len(), request_ids.len());
        let submitted = request_ids
            .iter()
            .map(|id| SubmitOutcome::Queued(BlockRequestId::new(*id)))
            .collect::<Vec<_>>();
        scheduler
            .apply_submission_receipt(&nodes, &submitted, None)
            .expect("apply submission receipt")
    }

    #[test]
    fn graph_admits_roots_then_releases_join_after_all_dependencies() {
        let graph = BackendBioGraph::new(
            alloc::vec![node(1, 10), node(2, 20), node(3, 30)],
            alloc::vec![
                BackendBioDependency::new(BackendBioNodeId::new(1), BackendBioNodeId::new(3)),
                BackendBioDependency::new(BackendBioNodeId::new(2), BackendBioNodeId::new(3)),
            ],
        )
        .expect("valid graph");
        let mut scheduler = BackendGraphScheduler::new(graph);

        let BackendGraphAdvance::Pending { submitted } = submit_staged(&mut scheduler, &[1, 2])
        else {
            panic!("roots should be queued");
        };
        assert_eq!(submitted.len(), 2);
        let first = match submitted[0] {
            SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
        };
        let second = match submitted[1] {
            SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
        };

        assert_eq!(
            scheduler.complete(first, Ok(())).expect("first completion"),
            BackendGraphStage::Waiting
        );
        let BackendGraphStage::Ready { nodes, bios } = scheduler
            .complete(second, Ok(()))
            .expect("second completion")
        else {
            panic!("join should become ready after both roots complete");
        };
        assert_eq!(nodes.len(), 1);
        assert_eq!(bios.as_slice().len(), 1);
        let submitted = [SubmitOutcome::Queued(BlockRequestId::new(3))];
        assert!(matches!(
            scheduler
                .apply_submission_receipt(&nodes, &submitted, None)
                .expect("apply join receipt"),
            BackendGraphAdvance::Pending { .. }
        ));
        let joined = match submitted[0] {
            SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
        };
        assert_eq!(
            scheduler.complete(joined, Ok(())).expect("join completion"),
            BackendGraphStage::Complete(Ok(()))
        );
    }

    #[test]
    fn graph_failure_stops_dependent_admission_and_returns_terminal_error() {
        let graph = BackendBioGraph::new(
            alloc::vec![node(1, 10), node(2, 20)],
            alloc::vec![BackendBioDependency::new(
                BackendBioNodeId::new(1),
                BackendBioNodeId::new(2),
            )],
        )
        .expect("valid graph");
        let mut scheduler = BackendGraphScheduler::new(graph);

        let BackendGraphAdvance::Pending { submitted } = submit_staged(&mut scheduler, &[1]) else {
            panic!("root should be queued");
        };
        let request = match submitted[0] {
            SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
        };
        assert_eq!(
            scheduler
                .complete(request, Err(Errno::EIO))
                .expect("failure completion"),
            BackendGraphStage::Complete(Err(Errno::EIO))
        );
        assert_eq!(
            scheduler.stage_ready(),
            BackendGraphStage::Complete(Err(Errno::EIO))
        );
    }
}
