//! Intrusive retired-node lists used by EBR.
//!
//! Nodes are stored in a domain-owned fixed array. Lists link nodes by array
//! index instead of raw pointers so the first kernel slice can avoid dynamic
//! allocation inside retirement paths.

use core::ptr;

/// Fixed retired-node pool for the first EBR slice.
pub const RETIRED_NODE_POOL_CAPACITY: usize = 1024;

pub(crate) struct RetiredNode {
    /// Raw object storage that must not be reused until the epoch window closes.
    pub(crate) ptr: *mut u8,
    /// Object-specific destructor/recycle callback.
    pub(crate) reclaim_fn: unsafe fn(*mut u8),
    /// Global epoch observed when the object was retired.
    pub(crate) retired_at_epoch: u64,
    /// Next node index in a per-CPU retired/free list.
    pub(crate) next: Option<usize>,
}

impl RetiredNode {
    pub(crate) const fn empty() -> Self {
        Self {
            ptr: ptr::null_mut(),
            reclaim_fn: noop_reclaim,
            retired_at_epoch: 0,
            next: None,
        }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::empty();
    }
}

unsafe fn noop_reclaim(_ptr: *mut u8) {}

#[repr(align(64))]
pub(crate) struct PerCpuRetiredPool {
    pub(crate) nodes: [RetiredNode; RETIRED_NODE_POOL_CAPACITY],
    pub(crate) free_head: Option<usize>,
}

impl PerCpuRetiredPool {
    pub(crate) const fn new() -> Self {
        Self {
            nodes: [const { RetiredNode::empty() }; RETIRED_NODE_POOL_CAPACITY],
            free_head: None,
        }
    }

    pub(crate) fn reset(&mut self) {
        for index in 0..RETIRED_NODE_POOL_CAPACITY {
            self.nodes[index].reset();
            self.nodes[index].next = if index + 1 < RETIRED_NODE_POOL_CAPACITY {
                Some(index + 1)
            } else {
                None
            };
        }
        self.free_head = Some(0);
    }

    pub(crate) fn alloc_node(&mut self) -> Option<usize> {
        let index = self.free_head?;
        self.free_head = self.nodes[index].next;
        self.nodes[index].reset();
        Some(index)
    }

    pub(crate) fn free_node(&mut self, index: usize) {
        debug_assert!(index < RETIRED_NODE_POOL_CAPACITY);
        self.nodes[index].reset();
        self.nodes[index].next = self.free_head;
        self.free_head = Some(index);
    }
}

pub(crate) struct RetiredList {
    /// Head index in the domain node array.
    pub(crate) head: Option<usize>,
    /// Number of nodes currently linked from `head`.
    pub(crate) count: usize,
}

impl RetiredList {
    pub(crate) const fn new() -> Self {
        Self {
            head: None,
            count: 0,
        }
    }

    pub(crate) fn push(&mut self, nodes: &mut [RetiredNode], index: usize) {
        nodes[index].next = self.head;
        self.head = Some(index);
        self.count += 1;
    }

    pub(crate) fn pop(&mut self, nodes: &mut [RetiredNode]) -> Option<usize> {
        let index = self.head?;
        self.head = nodes[index].next;
        nodes[index].next = None;
        self.count -= 1;
        Some(index)
    }
}

pub(crate) fn append_index(
    nodes: &mut [RetiredNode],
    head: &mut Option<usize>,
    tail: &mut Option<usize>,
    index: usize,
) {
    nodes[index].next = None;
    match *tail {
        Some(tail_index) => {
            nodes[tail_index].next = Some(index);
            *tail = Some(index);
        }
        None => {
            *head = Some(index);
            *tail = Some(index);
        }
    }
}
