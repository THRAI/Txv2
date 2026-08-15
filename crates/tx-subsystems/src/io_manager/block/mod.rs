//! L6 block-submission values and queue helpers.

use alloc::collections::{BTreeSet, VecDeque};
use alloc::vec::Vec;

use crate::execution::Errno;

use super::runtime::{IoServiceKind, QueueDepth, ServiceBudget, ServiceKick};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DeviceKey(u64);

impl DeviceKey {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BlockRequestId(u64);

impl BlockRequestId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LbaRange {
    start_lba: u64,
    block_count: u64,
}

impl LbaRange {
    pub const fn new(start_lba: u64, block_count: u64) -> Self {
        Self {
            start_lba,
            block_count,
        }
    }

    pub const fn start_lba(self) -> u64 {
        self.start_lba
    }

    pub const fn block_count(self) -> u64 {
        self.block_count
    }

    pub fn end_lba(self) -> Option<u64> {
        self.start_lba.checked_add(self.block_count)
    }

    pub const fn is_empty(self) -> bool {
        self.block_count == 0
    }

    fn extend_by(&mut self, other: Self) -> bool {
        if self.end_lba() == Some(other.start_lba) {
            self.block_count = self.block_count.saturating_add(other.block_count);
            true
        } else {
            false
        }
    }

    fn prepend_by(&mut self, other: Self) -> bool {
        if other.end_lba() == Some(self.start_lba) {
            self.start_lba = other.start_lba;
            self.block_count = self.block_count.saturating_add(other.block_count);
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockOp {
    Read,
    Write,
    Flush,
    Barrier,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockFlags(u32);

impl BlockFlags {
    pub const EMPTY: Self = Self(0);
    pub const FUA: Self = Self(1 << 0);
    pub const FLUSH: Self = Self(1 << 1);
    pub const BARRIER: Self = Self(1 << 2);

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BioVec {
    pub buffer_key: u64,
    pub offset: u32,
    pub len: u32,
}

impl BioVec {
    pub const fn new(buffer_key: u64, offset: u32, len: u32) -> Self {
        Self {
            buffer_key,
            offset,
            len,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BioPlan {
    pub device: DeviceKey,
    pub op: BlockOp,
    pub lba: LbaRange,
    pub vecs: Vec<BioVec>,
    pub flags: BlockFlags,
}

impl BioPlan {
    pub fn new(
        device: DeviceKey,
        op: BlockOp,
        lba: LbaRange,
        vecs: Vec<BioVec>,
        flags: BlockFlags,
    ) -> Self {
        Self {
            device,
            op,
            lba,
            vecs,
            flags,
        }
    }

    pub const fn requires_fence(&self) -> bool {
        matches!(self.op, BlockOp::Flush | BlockOp::Barrier)
            || self.flags.contains(BlockFlags::BARRIER)
            || self.flags.contains(BlockFlags::FLUSH)
    }

    fn can_merge_with(&self, other: &Self) -> bool {
        !self.requires_fence()
            && !other.requires_fence()
            && matches!(
                (self.op, other.op),
                (BlockOp::Read, BlockOp::Read) | (BlockOp::Write, BlockOp::Write)
            )
            && self.device == other.device
            && self.flags == other.flags
            && self.lba.end_lba() == Some(other.lba.start_lba)
    }

    fn merge_adjacent(&mut self, mut other: Self) -> bool {
        if !self.can_merge_with(&other) || !self.lba.extend_by(other.lba) {
            return false;
        }
        self.vecs.append(&mut other.vecs);
        true
    }

    fn prepend_adjacent(&mut self, mut other: Self) -> bool {
        if other.requires_fence()
            || self.requires_fence()
            || !matches!(
                (self.op, other.op),
                (BlockOp::Read, BlockOp::Read) | (BlockOp::Write, BlockOp::Write)
            )
            || self.device != other.device
            || self.flags != other.flags
            || !self.lba.prepend_by(other.lba)
        {
            return false;
        }
        other.vecs.append(&mut self.vecs);
        self.vecs = other.vecs;
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bio {
    pub id: BlockRequestId,
    pub plan: BioPlan,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BlockTag(u64);

impl BlockTag {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockDispatch {
    pub tag: BlockTag,
    pub bio: Bio,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockCompletion {
    pub tag: BlockTag,
    pub id: BlockRequestId,
    pub plan: BioPlan,
    pub result: Result<(), Errno>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockDeviceCompletion {
    pub tag: BlockTag,
    pub result: Result<(), Errno>,
}

impl BlockDeviceCompletion {
    pub const fn new(tag: BlockTag, result: Result<(), Errno>) -> Self {
        Self { tag, result }
    }
}

pub trait BlockDispatchExecutor {
    fn submit(&mut self, dispatch: &BlockDispatch);
}

pub trait BlockCompletionSource {
    fn poll_completion(&mut self) -> Option<BlockDeviceCompletion>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockCompletionError {
    UnknownTag(BlockTag),
    DepthUnderflow(BlockTag),
}

#[derive(Debug)]
pub struct BlockTagTable {
    next_tag: u64,
    in_flight: Vec<BlockDispatch>,
}

impl BlockTagTable {
    pub fn new() -> Self {
        Self {
            next_tag: 1,
            in_flight: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.in_flight.len()
    }

    pub fn is_empty(&self) -> bool {
        self.in_flight.is_empty()
    }

    pub fn has_fence_in_flight(&self) -> bool {
        self.in_flight
            .iter()
            .any(|dispatch| dispatch.bio.plan.requires_fence())
    }

    pub fn lookup(&self, tag: BlockTag) -> Option<&BlockDispatch> {
        self.in_flight.iter().find(|dispatch| dispatch.tag == tag)
    }

    fn dispatch(&mut self, bio: Bio) -> BlockDispatch {
        let tag = BlockTag::new(self.next_tag);
        self.next_tag = self.next_tag.wrapping_add(1).max(1);
        let dispatch = BlockDispatch { tag, bio };
        self.in_flight.push(dispatch.clone());
        dispatch
    }

    pub fn complete(
        &mut self,
        depth: &mut QueueDepth,
        tag: BlockTag,
        result: Result<(), Errno>,
    ) -> Result<BlockCompletion, BlockCompletionError> {
        let Some(index) = self
            .in_flight
            .iter()
            .position(|dispatch| dispatch.tag == tag)
        else {
            return Err(BlockCompletionError::UnknownTag(tag));
        };
        if !depth.complete_one() {
            return Err(BlockCompletionError::DepthUnderflow(tag));
        }
        let dispatch = self.in_flight.swap_remove(index);
        Ok(BlockCompletion {
            tag,
            id: dispatch.bio.id,
            plan: dispatch.bio.plan,
            result,
        })
    }

    pub fn complete_device(
        &mut self,
        depth: &mut QueueDepth,
        completion: BlockDeviceCompletion,
    ) -> Result<BlockCompletion, BlockCompletionError> {
        self.complete(depth, completion.tag, completion.result)
    }

    pub fn complete_polled<S>(
        &mut self,
        depth: &mut QueueDepth,
        source: &mut S,
    ) -> Result<Option<BlockCompletion>, BlockCompletionError>
    where
        S: BlockCompletionSource + ?Sized,
    {
        let Some(completion) = source.poll_completion() else {
            return Ok(None);
        };
        self.complete_device(depth, completion).map(Some)
    }
}

impl Default for BlockTagTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockServiceNext {
    Runnable,
    Sleeping,
    WaitingForCompletion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockServiceStep {
    pub dispatches: Vec<BlockDispatch>,
    pub next: BlockServiceNext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockServiceDriven {
    pub step: BlockServiceStep,
    pub kicks: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockServiceDriver {
    budget: ServiceBudget,
}

impl BlockServiceDriver {
    pub const fn new(budget: ServiceBudget) -> Self {
        Self { budget }
    }

    pub fn drive_once<F>(
        &mut self,
        queue: &mut BlockQueue,
        depth: &mut QueueDepth,
        tags: &mut BlockTagTable,
        mut kick: F,
    ) -> BlockServiceDriven
    where
        F: FnMut(ServiceKick) -> bool,
    {
        let step = drive_block_turn(queue, depth, tags, self.budget);
        let kicks = if step.next == BlockServiceNext::Runnable {
            usize::from(kick(ServiceKick::new(IoServiceKind::Block)))
        } else {
            0
        };
        BlockServiceDriven { step, kicks }
    }

    pub fn drive_once_with_executor<E, F>(
        &mut self,
        queue: &mut BlockQueue,
        depth: &mut QueueDepth,
        tags: &mut BlockTagTable,
        executor: &mut E,
        kick: F,
    ) -> BlockServiceDriven
    where
        E: BlockDispatchExecutor + ?Sized,
        F: FnMut(ServiceKick) -> bool,
    {
        let driven = self.drive_once(queue, depth, tags, kick);
        for dispatch in &driven.step.dispatches {
            executor.submit(dispatch);
        }
        driven
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueError {
    EmptyRange,
    Full,
    DispatchDepthFull,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitOutcome {
    Queued(BlockRequestId),
    Merged(BlockRequestId),
}

#[derive(Debug)]
pub struct BlockQueue {
    next_id: u64,
    max_pending: usize,
    pending: VecDeque<Bio>,
    dispatch_blocked: BTreeSet<BlockRequestId>,
}

impl BlockQueue {
    pub fn new(max_pending: usize) -> Self {
        Self {
            next_id: 1,
            max_pending,
            pending: VecDeque::new(),
            dispatch_blocked: BTreeSet::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(crate) fn dispatch_blocked_len(&self) -> usize {
        self.dispatch_blocked.len()
    }

    pub(crate) fn front_dispatch_blocked(&self) -> bool {
        self.pending
            .front()
            .is_some_and(|bio| self.dispatch_blocked.contains(&bio.id))
    }

    pub fn submit(&mut self, plan: BioPlan) -> Result<SubmitOutcome, QueueError> {
        if plan.lba.is_empty() && !matches!(plan.op, BlockOp::Flush | BlockOp::Barrier) {
            return Err(QueueError::EmptyRange);
        }

        if let Some(back) = self.pending.back_mut() {
            let id = back.id;
            if back.plan.merge_adjacent(plan.clone()) {
                return Ok(SubmitOutcome::Merged(id));
            }
        }
        if let Some(front) = self.pending.front_mut() {
            let id = front.id;
            if front.plan.prepend_adjacent(plan.clone()) {
                return Ok(SubmitOutcome::Merged(id));
            }
        }

        if self.pending.len() >= self.max_pending {
            return Err(QueueError::Full);
        }

        let id = BlockRequestId::new(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.pending.push_back(Bio { id, plan });
        Ok(SubmitOutcome::Queued(id))
    }

    /// Hold an admitted BIO in L6 until its owner has committed every
    /// completion route needed before device dispatch.
    pub fn block_dispatch(&mut self, id: BlockRequestId) {
        self.dispatch_blocked.insert(id);
    }

    /// Make a previously held BIO eligible for ordinary queue dispatch.
    pub fn unblock_dispatch(&mut self, id: BlockRequestId) {
        self.dispatch_blocked.remove(&id);
    }

    pub fn can_dispatch_next(&self, depth: &QueueDepth) -> bool {
        let Some(front) = self.pending.front() else {
            return false;
        };
        if self.dispatch_blocked.contains(&front.id) {
            return false;
        }
        if front.plan.requires_fence() {
            depth.in_flight() == 0 && depth.has_capacity()
        } else {
            depth.has_capacity()
        }
    }

    pub fn pop_dispatchable(&mut self, depth: &mut QueueDepth) -> Result<Option<Bio>, QueueError> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        if !self.can_dispatch_next(depth) {
            return Err(QueueError::DispatchDepthFull);
        }
        depth
            .try_start()
            .map_err(|_| QueueError::DispatchDepthFull)?;
        Ok(self.pending.pop_front())
    }

    pub fn can_dispatch_next_tagged(&self, depth: &QueueDepth, tags: &BlockTagTable) -> bool {
        if tags.has_fence_in_flight() {
            return false;
        }
        let Some(front) = self.pending.front() else {
            return false;
        };
        if self.dispatch_blocked.contains(&front.id) {
            return false;
        }
        if front.plan.requires_fence() {
            depth.in_flight() == 0 && tags.is_empty() && depth.has_capacity()
        } else {
            depth.has_capacity()
        }
    }

    pub fn pop_dispatchable_tagged(
        &mut self,
        depth: &mut QueueDepth,
        tags: &mut BlockTagTable,
    ) -> Result<Option<BlockDispatch>, QueueError> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        if !self.can_dispatch_next_tagged(depth, tags) {
            return Err(QueueError::DispatchDepthFull);
        }
        depth
            .try_start()
            .map_err(|_| QueueError::DispatchDepthFull)?;
        let bio = self
            .pending
            .pop_front()
            .expect("pending front existed before dispatch");
        Ok(Some(tags.dispatch(bio)))
    }
}

fn drive_block_turn(
    queue: &mut BlockQueue,
    depth: &mut QueueDepth,
    tags: &mut BlockTagTable,
    mut budget: ServiceBudget,
) -> BlockServiceStep {
    let mut dispatches = Vec::new();
    while budget.take_one() {
        match queue.pop_dispatchable_tagged(depth, tags) {
            Ok(Some(dispatch)) => dispatches.push(dispatch),
            Ok(None) => break,
            Err(QueueError::DispatchDepthFull) => break,
            Err(QueueError::EmptyRange | QueueError::Full) => break,
        }
    }

    let next = if queue.is_empty() {
        BlockServiceNext::Sleeping
    } else if queue.can_dispatch_next_tagged(depth, tags) {
        BlockServiceNext::Runnable
    } else {
        BlockServiceNext::WaitingForCompletion
    };

    BlockServiceStep { dispatches, next }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_plan(start: u64, count: u64, buffer_key: u64) -> BioPlan {
        BioPlan::new(
            DeviceKey::new(3),
            BlockOp::Read,
            LbaRange::new(start, count),
            alloc::vec![BioVec::new(buffer_key, 0, (count * 512) as u32)],
            BlockFlags::EMPTY,
        )
    }

    #[test]
    fn io_manager_block_queue_merges_adjacent_lba_reads() {
        let mut queue = BlockQueue::new(4);
        let first = queue.submit(read_plan(8, 2, 1)).expect("first");
        let second = queue.submit(read_plan(10, 3, 2)).expect("second");

        assert_eq!(first, SubmitOutcome::Queued(BlockRequestId::new(1)));
        assert_eq!(second, SubmitOutcome::Merged(BlockRequestId::new(1)));
        assert_eq!(queue.len(), 1);
        let mut depth = QueueDepth::new(1);
        let bio = queue
            .pop_dispatchable(&mut depth)
            .expect("dispatch ok")
            .expect("bio");
        assert_eq!(bio.plan.lba, LbaRange::new(8, 5));
        assert_eq!(bio.plan.vecs.len(), 2);
    }

    #[test]
    fn io_manager_block_queue_merges_front_adjacent_lba_reads() {
        let mut queue = BlockQueue::new(4);
        let first = queue.submit(read_plan(10, 3, 1)).expect("first");
        let second = queue.submit(read_plan(8, 2, 2)).expect("second");

        assert_eq!(first, SubmitOutcome::Queued(BlockRequestId::new(1)));
        assert_eq!(second, SubmitOutcome::Merged(BlockRequestId::new(1)));
        assert_eq!(queue.len(), 1);
        let mut depth = QueueDepth::new(1);
        let bio = queue
            .pop_dispatchable(&mut depth)
            .expect("dispatch ok")
            .expect("bio");
        assert_eq!(bio.plan.lba, LbaRange::new(8, 5));
        assert_eq!(bio.plan.vecs[0].buffer_key, 2);
        assert_eq!(bio.plan.vecs[1].buffer_key, 1);
    }

    #[test]
    fn io_manager_block_queue_enforces_pending_depth() {
        let mut queue = BlockQueue::new(1);

        assert!(queue.submit(read_plan(0, 1, 1)).is_ok());
        assert_eq!(queue.submit(read_plan(4, 1, 2)), Err(QueueError::Full));
    }

    #[test]
    fn io_manager_block_queue_barrier_waits_for_in_flight_to_drain() {
        let barrier = BioPlan::new(
            DeviceKey::new(3),
            BlockOp::Barrier,
            LbaRange::new(0, 0),
            Vec::new(),
            BlockFlags::BARRIER,
        );
        let mut queue = BlockQueue::new(4);
        queue.submit(barrier).expect("barrier");

        let mut depth = QueueDepth::new(2);
        depth.try_start().expect("simulate existing dispatch");
        assert!(!queue.can_dispatch_next(&depth));
        assert_eq!(
            queue.pop_dispatchable(&mut depth),
            Err(QueueError::DispatchDepthFull)
        );

        assert!(depth.complete_one());
        let bio = queue
            .pop_dispatchable(&mut depth)
            .expect("dispatch ok")
            .expect("barrier bio");
        assert_eq!(bio.plan.op, BlockOp::Barrier);
    }

    #[test]
    fn io_manager_block_queue_allocates_tags_and_completes_by_tag() {
        let mut queue = BlockQueue::new(4);
        let mut depth = QueueDepth::new(2);
        let mut tags = BlockTagTable::new();
        queue.submit(read_plan(2, 1, 7)).expect("read");

        let dispatch = queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("dispatch ok")
            .expect("dispatch");
        assert_eq!(dispatch.bio.id, BlockRequestId::new(1));
        assert_eq!(dispatch.tag, BlockTag::new(1));
        assert_eq!(depth.in_flight(), 1);
        assert_eq!(
            tags.lookup(dispatch.tag).map(|in_flight| in_flight.bio.id),
            Some(BlockRequestId::new(1))
        );

        let completion = tags
            .complete(&mut depth, dispatch.tag, Ok(()))
            .expect("completion lookup");
        assert_eq!(completion.id, BlockRequestId::new(1));
        assert_eq!(completion.plan.lba, LbaRange::new(2, 1));
        assert_eq!(completion.result, Ok(()));
        assert_eq!(depth.in_flight(), 0);
        assert!(tags.lookup(dispatch.tag).is_none());
        assert_eq!(
            tags.complete(&mut depth, dispatch.tag, Ok(())),
            Err(BlockCompletionError::UnknownTag(dispatch.tag))
        );
    }

    #[test]
    fn io_manager_block_queue_barrier_in_flight_blocks_later_dispatch() {
        let barrier = BioPlan::new(
            DeviceKey::new(3),
            BlockOp::Barrier,
            LbaRange::new(0, 0),
            Vec::new(),
            BlockFlags::BARRIER,
        );
        let mut queue = BlockQueue::new(4);
        let mut depth = QueueDepth::new(2);
        let mut tags = BlockTagTable::new();
        queue.submit(barrier).expect("barrier");
        queue
            .submit(read_plan(4, 1, 9))
            .expect("read after barrier");

        let barrier_dispatch = queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("barrier dispatch ok")
            .expect("barrier dispatch");
        assert_eq!(barrier_dispatch.bio.plan.op, BlockOp::Barrier);
        assert!(tags.has_fence_in_flight());
        assert!(!queue.can_dispatch_next_tagged(&depth, &tags));
        assert_eq!(
            queue.pop_dispatchable_tagged(&mut depth, &mut tags),
            Err(QueueError::DispatchDepthFull)
        );

        let completion = tags
            .complete(&mut depth, barrier_dispatch.tag, Ok(()))
            .expect("barrier completion");
        assert_eq!(completion.id, barrier_dispatch.bio.id);
        assert!(!tags.has_fence_in_flight());

        let read_dispatch = queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("read dispatch ok")
            .expect("read dispatch");
        assert_eq!(read_dispatch.bio.plan.op, BlockOp::Read);
    }

    #[test]
    fn block_service_dispatches_tagged_bio_and_rekicks_for_dispatchable_backlog() {
        let mut queue = BlockQueue::new(4);
        queue.submit(read_plan(2, 1, 7)).expect("first read");
        queue.submit(read_plan(8, 1, 8)).expect("second read");
        let mut depth = QueueDepth::new(2);
        let mut tags = BlockTagTable::new();
        let mut driver = BlockServiceDriver::new(crate::io_manager::runtime::ServiceBudget::new(1));
        let mut kicks = Vec::new();

        let driven = driver.drive_once(&mut queue, &mut depth, &mut tags, |kick| {
            kicks.push(kick);
            true
        });

        assert_eq!(driven.step.next, BlockServiceNext::Runnable);
        assert_eq!(driven.step.dispatches.len(), 1);
        assert_eq!(driven.step.dispatches[0].tag, BlockTag::new(1));
        assert_eq!(driven.step.dispatches[0].bio.id, BlockRequestId::new(1));
        assert_eq!(depth.in_flight(), 1);
        assert_eq!(tags.len(), 1);
        assert_eq!(queue.len(), 1);
        assert_eq!(driven.kicks, 1);
        assert_eq!(
            kicks,
            alloc::vec![crate::io_manager::runtime::ServiceKick::new(
                crate::io_manager::runtime::IoServiceKind::Block
            )]
        );
    }

    #[test]
    fn block_service_waits_for_completion_when_depth_blocks_front() {
        let mut queue = BlockQueue::new(4);
        queue.submit(read_plan(2, 1, 7)).expect("read");
        let mut depth = QueueDepth::new(1);
        depth.try_start().expect("simulate in-flight");
        let mut tags = BlockTagTable::new();
        let mut driver = BlockServiceDriver::new(crate::io_manager::runtime::ServiceBudget::new(1));

        let driven = driver.drive_once(&mut queue, &mut depth, &mut tags, |_| {
            panic!("depth-blocked service must not kick itself")
        });

        assert_eq!(
            driven.step,
            BlockServiceStep {
                dispatches: Vec::new(),
                next: BlockServiceNext::WaitingForCompletion,
            }
        );
        assert_eq!(driven.kicks, 0);
        assert_eq!(queue.len(), 1);
        assert_eq!(tags.len(), 0);
        assert_eq!(depth.in_flight(), 1);
    }

    #[test]
    fn block_service_submits_dispatches_to_executor_port() {
        #[derive(Default)]
        struct RecordingExecutor {
            submitted: Vec<BlockDispatch>,
        }

        impl BlockDispatchExecutor for RecordingExecutor {
            fn submit(&mut self, dispatch: &BlockDispatch) {
                self.submitted.push(dispatch.clone());
            }
        }

        let mut queue = BlockQueue::new(4);
        queue.submit(read_plan(32, 1, 9)).expect("read");
        let mut depth = QueueDepth::new(1);
        let mut tags = BlockTagTable::new();
        let mut executor = RecordingExecutor::default();
        let mut driver = BlockServiceDriver::new(crate::io_manager::runtime::ServiceBudget::new(1));

        let driven = driver.drive_once_with_executor(
            &mut queue,
            &mut depth,
            &mut tags,
            &mut executor,
            |_| false,
        );

        assert_eq!(driven.step.dispatches.len(), 1);
        assert_eq!(executor.submitted, driven.step.dispatches);
        assert_eq!(executor.submitted[0].tag, BlockTag::new(1));
        assert_eq!(executor.submitted[0].bio.id, BlockRequestId::new(1));
    }

    #[test]
    fn block_tag_table_completes_polled_device_completion() {
        struct SingleCompletionSource(Option<BlockDeviceCompletion>);

        impl BlockCompletionSource for SingleCompletionSource {
            fn poll_completion(&mut self) -> Option<BlockDeviceCompletion> {
                self.0.take()
            }
        }

        let mut queue = BlockQueue::new(4);
        queue.submit(read_plan(40, 1, 10)).expect("read");
        let mut depth = QueueDepth::new(1);
        let mut tags = BlockTagTable::new();
        let dispatch = queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("dispatch check")
            .expect("dispatch");
        let mut source =
            SingleCompletionSource(Some(BlockDeviceCompletion::new(dispatch.tag, Ok(()))));

        let completion = tags
            .complete_polled(&mut depth, &mut source)
            .expect("valid completion")
            .expect("completion");

        assert_eq!(completion.tag, dispatch.tag);
        assert_eq!(completion.id, dispatch.bio.id);
        assert_eq!(completion.result, Ok(()));
        assert_eq!(depth.in_flight(), 0);
        assert!(tags.is_empty());
        assert_eq!(tags.complete_polled(&mut depth, &mut source), Ok(None));
    }
}
