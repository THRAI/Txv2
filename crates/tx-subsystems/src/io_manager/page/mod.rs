//! L4 page-submission values.

use alloc::collections::VecDeque;

use crate::execution::Errno;

pub mod admission;
pub mod completion;
pub mod service;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PageContainerKey(u64);

impl PageContainerKey {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PageIoRequestId(u64);

impl PageIoRequestId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PageGeneration(u64);

impl PageGeneration {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageIoRange {
    start_page: u64,
    page_count: u64,
}

impl PageIoRange {
    pub const fn new(start_page: u64, page_count: u64) -> Self {
        Self {
            start_page,
            page_count,
        }
    }

    pub const fn start_page(self) -> u64 {
        self.start_page
    }

    pub const fn page_count(self) -> u64 {
        self.page_count
    }

    pub fn end_page(self) -> Option<u64> {
        self.start_page.checked_add(self.page_count)
    }

    pub const fn is_empty(self) -> bool {
        self.page_count == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageIoOp {
    Read,
    Writeback,
    Fsync,
    Checkpoint,
    Readahead,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PageIoPriority {
    Completion = 0,
    Demand = 1,
    Fsync = 2,
    ForegroundWrite = 3,
    Readahead = 4,
    BackgroundWriteback = 5,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageIoFlags(u32);

impl PageIoFlags {
    pub const EMPTY: Self = Self(0);
    pub const DEMAND: Self = Self(1 << 0);
    pub const READAHEAD: Self = Self(1 << 1);
    pub const WRITEBACK: Self = Self(1 << 2);
    pub const BARRIER: Self = Self(1 << 3);

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageIoRequest {
    pub id: PageIoRequestId,
    pub pc: PageContainerKey,
    pub range: PageIoRange,
    pub op: PageIoOp,
    pub priority: PageIoPriority,
    pub flags: PageIoFlags,
    pub generation_hint: Option<PageGeneration>,
}

impl PageIoRequest {
    pub const fn new(
        id: PageIoRequestId,
        pc: PageContainerKey,
        range: PageIoRange,
        op: PageIoOp,
        priority: PageIoPriority,
        flags: PageIoFlags,
        generation_hint: Option<PageGeneration>,
    ) -> Self {
        Self {
            id,
            pc,
            range,
            op,
            priority,
            flags,
            generation_hint,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageIoResult {
    Done,
    Err(Errno),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageIoCompletionKind {
    ReadInstalled,
    WritebackFinished,
    Invalidated,
    Noop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageIoCompletion {
    pub id: PageIoRequestId,
    pub range: PageIoRange,
    pub result: PageIoResult,
    pub generation: PageGeneration,
    pub kind: PageIoCompletionKind,
}

impl PageIoCompletion {
    pub const fn new(
        id: PageIoRequestId,
        range: PageIoRange,
        result: PageIoResult,
        generation: PageGeneration,
        kind: PageIoCompletionKind,
    ) -> Self {
        Self {
            id,
            range,
            result,
            generation,
            kind,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageQueueError {
    Full,
    EmptyRange,
}

#[derive(Debug)]
pub struct PageRequestQueue {
    next_id: u64,
    max_pending: usize,
    pending: VecDeque<PageIoRequest>,
}

impl PageRequestQueue {
    pub fn new(max_pending: usize) -> Self {
        Self {
            next_id: 1,
            max_pending,
            pending: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn has_capacity(&self) -> bool {
        self.pending.len() < self.max_pending
    }

    pub fn submit(
        &mut self,
        pc: PageContainerKey,
        range: PageIoRange,
        op: PageIoOp,
        priority: PageIoPriority,
        flags: PageIoFlags,
        generation_hint: Option<PageGeneration>,
    ) -> Result<PageIoRequestId, PageQueueError> {
        if range.is_empty() {
            return Err(PageQueueError::EmptyRange);
        }
        if self.pending.len() >= self.max_pending {
            return Err(PageQueueError::Full);
        }
        let id = PageIoRequestId::new(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.pending.push_back(PageIoRequest::new(
            id,
            pc,
            range,
            op,
            priority,
            flags,
            generation_hint,
        ));
        Ok(id)
    }

    pub fn pop_next(&mut self) -> Option<PageIoRequest> {
        let (index, _) = self
            .pending
            .iter()
            .enumerate()
            .min_by_key(|(_, request)| request.priority)?;
        self.pending.remove(index)
    }

    pub fn remove(&mut self, id: PageIoRequestId) -> Option<PageIoRequest> {
        let index = self.pending.iter().position(|request| request.id == id)?;
        self.pending.remove(index)
    }

    pub fn find(
        &self,
        pc: PageContainerKey,
        range: PageIoRange,
        op: PageIoOp,
    ) -> Option<&PageIoRequest> {
        self.pending
            .iter()
            .find(|request| request.pc == pc && request.range == range && request.op == op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_manager_page_queue_prioritizes_demand_over_readahead() {
        let mut queue = PageRequestQueue::new(4);
        let pc = PageContainerKey::new(7);

        let readahead = queue
            .submit(
                pc,
                PageIoRange::new(10, 2),
                PageIoOp::Readahead,
                PageIoPriority::Readahead,
                PageIoFlags::READAHEAD,
                None,
            )
            .expect("submit readahead");
        let demand = queue
            .submit(
                pc,
                PageIoRange::new(9, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(3)),
            )
            .expect("submit demand");

        assert_eq!(queue.pop_next().expect("demand").id, demand);
        assert_eq!(queue.pop_next().expect("readahead").id, readahead);
        assert!(queue.pop_next().is_none());
    }

    #[test]
    fn io_manager_page_queue_rejects_empty_and_full_admission() {
        let pc = PageContainerKey::new(1);
        let mut queue = PageRequestQueue::new(1);

        assert_eq!(
            queue.submit(
                pc,
                PageIoRange::new(0, 0),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                None,
            ),
            Err(PageQueueError::EmptyRange)
        );
        assert!(queue
            .submit(
                pc,
                PageIoRange::new(0, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                None,
            )
            .is_ok());
        assert_eq!(
            queue.submit(
                pc,
                PageIoRange::new(1, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                None,
            ),
            Err(PageQueueError::Full)
        );
    }

    #[test]
    fn io_manager_page_queue_removes_request_by_id_without_reordering_others() {
        let pc = PageContainerKey::new(1);
        let mut queue = PageRequestQueue::new(3);
        let first = queue
            .submit(
                pc,
                PageIoRange::new(0, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(1)),
            )
            .expect("first");
        let second = queue
            .submit(
                pc,
                PageIoRange::new(1, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(2)),
            )
            .expect("second");

        assert_eq!(queue.remove(first).expect("removed first").id, first);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop_next().expect("remaining second").id, second);
        assert!(queue.remove(first).is_none());
    }
}
