//! Dependency-aware admission for filesystem backend bio graphs.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::execution::Errno;
use crate::fs_iface::{BackendBioGraph, BackendBioNodeId};
use crate::io_manager::block::{BlockQueue, BlockRequestId, QueueError, SubmitOutcome};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendGraphAdvance {
    Pending { submitted: Vec<SubmitOutcome> },
    Complete(Result<(), Errno>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendGraphSchedulerError {
    UnknownBlockRequest(BlockRequestId),
    Queue(QueueError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NodeState {
    Pending,
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

    pub fn start(
        &mut self,
        queue: &mut BlockQueue,
    ) -> Result<BackendGraphAdvance, BackendGraphSchedulerError> {
        self.admit_ready(queue)
    }

    pub fn complete(
        &mut self,
        request: BlockRequestId,
        result: Result<(), Errno>,
        queue: &mut BlockQueue,
    ) -> Result<BackendGraphAdvance, BackendGraphSchedulerError> {
        let Some(nodes) = self.requests.remove(&request) else {
            return Err(BackendGraphSchedulerError::UnknownBlockRequest(request));
        };
        for node in nodes {
            self.states.insert(node, NodeState::Complete);
        }
        if let Err(errno) = result {
            self.failure.get_or_insert(errno);
        }
        self.admit_ready(queue)
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

    fn admit_ready(
        &mut self,
        queue: &mut BlockQueue,
    ) -> Result<BackendGraphAdvance, BackendGraphSchedulerError> {
        if self.failure.is_none() {
            let ready = self
                .graph
                .nodes()
                .iter()
                .filter(|node| self.states.get(&node.id) == Some(&NodeState::Pending))
                .filter(|node| self.dependencies_complete(node.id))
                .cloned()
                .collect::<Vec<_>>();
            let mut submitted = Vec::new();
            for node in ready {
                let outcome = queue
                    .submit(node.bio.clone())
                    .map_err(BackendGraphSchedulerError::Queue)?;
                let request = match outcome {
                    SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
                };
                self.requests.entry(request).or_default().push(node.id);
                self.states.insert(node.id, NodeState::Submitted);
                submitted.push(outcome);
            }
            if !submitted.is_empty() {
                return Ok(BackendGraphAdvance::Pending { submitted });
            }
        }

        if self.is_terminal() {
            return Ok(BackendGraphAdvance::Complete(match self.failure {
                Some(errno) => Err(errno),
                None => Ok(()),
            }));
        }
        Ok(BackendGraphAdvance::Pending {
            submitted: Vec::new(),
        })
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
    use crate::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};

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
        let mut queue = BlockQueue::new(8);

        let BackendGraphAdvance::Pending { submitted } =
            scheduler.start(&mut queue).expect("start")
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
            scheduler
                .complete(first, Ok(()), &mut queue)
                .expect("first completion"),
            BackendGraphAdvance::Pending {
                submitted: Vec::new()
            }
        );
        let BackendGraphAdvance::Pending { submitted } = scheduler
            .complete(second, Ok(()), &mut queue)
            .expect("second completion")
        else {
            panic!("join should become ready after both roots complete");
        };
        assert_eq!(submitted.len(), 1);
        let joined = match submitted[0] {
            SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
        };
        assert_eq!(
            scheduler
                .complete(joined, Ok(()), &mut queue)
                .expect("join completion"),
            BackendGraphAdvance::Complete(Ok(()))
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
        let mut queue = BlockQueue::new(8);

        let BackendGraphAdvance::Pending { submitted } =
            scheduler.start(&mut queue).expect("start")
        else {
            panic!("root should be queued");
        };
        let request = match submitted[0] {
            SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
        };
        assert_eq!(
            scheduler
                .complete(request, Err(Errno::EIO), &mut queue)
                .expect("failure completion"),
            BackendGraphAdvance::Complete(Err(Errno::EIO))
        );
        assert_eq!(queue.len(), 1, "dependent bio must not be submitted");
    }
}
