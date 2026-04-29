//! Page-table-node ownership sources.
//!
//! Core data structures/state maintained here:
//! - `PT_NODE_ALLOCATED`: a bitmap for the linker-carved early boot PT-node
//!   pool.
//! - `INSTALLED_PT_NODE_ALLOCATOR`: the one-shot substrate handoff to typed
//!   page-table frame allocation.
//! - `CommittedPtNodeRegistry`: a fixed table from committed branch-table
//!   physical addresses back to their owning `PtNode`.
//! - `RegistryGuard`: the RAII spin-lock guard for the committed-node registry.
//!
//! Main state modification functions:
//! - `alloc_pt_node()` and `free_pt_node()` choose typed frames first and fall
//!   back to the boot pool.
//! - `install_pt_node_allocator()` switches steady-state allocation to the
//!   substrate-provided source.
//! - `register_committed_intermediates()` records PT-node ownership at commit.
//! - `release_committed_pt_node_from_bag()` releases ownership after a later
//!   prune.
//!
//! Helper groups:
//! - boot-pool helpers scan/update the allocation bitmap and zero pages;
//! - installed-allocator helpers encode/decode the function pointer;
//! - registry helpers lock, insert, take, and clear committed node entries.
//!
//! Early boot uses a fixed board-owned PT-node pool because the frame allocator
//! is not live yet. After substrate installs a typed allocator, new PT nodes
//! prefer typed page-table frames and fall back to the boot pool only on
//! exhaustion. See
//! `docs/progress/decisions/2026-04-29-pmap-typed-intermediate-source.md` and
//! `docs/progress/decisions/2026-04-29-pmap-committed-pt-node-teardown.md`.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tx_hal::{
    AllocError, PhysAddr, PmapError, PmapReservationIntermediates, PtNode, PtNodeAllocator,
};

use crate::boot_static::{BootStaticBag, IdentityDropped};

use super::{pool_index, pt_node_zero_ptr, zero_page, PT_NODE_POOL_ENTRIES};

const COMMITTED_PT_NODE_REGISTRY_ENTRIES: usize = 256;

static PT_NODE_ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static INSTALLED_PT_NODE_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);
static COMMITTED_PT_NODE_REGISTRY_LOCK: AtomicBool = AtomicBool::new(false);
static COMMITTED_PT_NODES: CommittedPtNodeRegistry =
    CommittedPtNodeRegistry(UnsafeCell::new([None; COMMITTED_PT_NODE_REGISTRY_ENTRIES]));

/// Fixed-size registry from committed branch-table physical pages to `PtNode`.
///
/// Branch PTEs only store a physical address. This registry keeps the typed
/// ownership token needed to release the page-table page when an unmap later
/// prunes the branch.
struct CommittedPtNodeRegistry(UnsafeCell<[Option<PtNode>; COMMITTED_PT_NODE_REGISTRY_ENTRIES]>);

unsafe impl Sync for CommittedPtNodeRegistry {}

// Allocation starts with the installed typed allocator when available and falls
// back to the static boot pool. The boot pool is a bitmap over linker-carved
// page-table pages in `BootStaticBag`; typed frames carry their own release
// hook through `PtNode`.
pub(crate) fn alloc_pt_node() -> Result<PtNode, AllocError> {
    alloc_pt_node_from_bag(BootStaticBag::<IdentityDropped>::global_ref())
}

pub(super) fn alloc_pt_node_from_bag<State>(
    bag: &BootStaticBag<State>,
) -> Result<PtNode, AllocError> {
    if let Some(allocator) = installed_pt_node_allocator() {
        if let Ok(node) = allocator() {
            return Ok(node);
        }
    }

    alloc_boot_pool_pt_node_from_bag(bag)
}

fn alloc_boot_pool_pt_node_from_bag<State>(
    bag: &BootStaticBag<State>,
) -> Result<PtNode, AllocError> {
    loop {
        let allocated = PT_NODE_ALLOCATED.load(Ordering::Acquire);
        if allocated == pool_full_mask() {
            return Err(AllocError::Exhausted);
        }

        for index in 0..PT_NODE_POOL_ENTRIES {
            let bit = 1usize << index;
            if allocated & bit != 0 {
                continue;
            }
            if PT_NODE_ALLOCATED
                .compare_exchange(
                    allocated,
                    allocated | bit,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                let phys = bag.pt_node_phys(index);
                zero_pt_node(bag, index);
                return Ok(PtNode::boot_pool(phys));
            }
            break;
        }
    }
}

pub(crate) fn free_pt_node(node: PtNode) {
    free_pt_node_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), node);
}

pub(super) fn free_pt_node_from_bag<State>(bag: &BootStaticBag<State>, node: PtNode) {
    if unsafe { node.release_typed_frame() } {
        return;
    }

    if let Some(index) = pool_index(bag, node.phys) {
        PT_NODE_ALLOCATED.fetch_and(!(1usize << index), Ordering::AcqRel);
    }
}

// The installed allocator is a one-shot handoff from substrate after the frame
// allocator is live. It deliberately stays a function pointer rather than a dyn
// object, matching the static HAL surface.
pub(crate) fn install_pt_node_allocator(allocator: PtNodeAllocator) -> Result<(), PmapError> {
    let value = allocator as usize;
    INSTALLED_PT_NODE_ALLOCATOR
        .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| PmapError::AlreadyMapped)
}

fn installed_pt_node_allocator() -> Option<PtNodeAllocator> {
    let value = INSTALLED_PT_NODE_ALLOCATOR.load(Ordering::Acquire);
    if value == 0 {
        return None;
    }

    Some(unsafe { core::mem::transmute::<usize, PtNodeAllocator>(value) })
}

/// RAII guard for the committed PT-node registry spin lock.
struct RegistryGuard;

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        COMMITTED_PT_NODE_REGISTRY_LOCK.store(false, Ordering::Release);
    }
}

fn lock_committed_pt_node_registry() -> RegistryGuard {
    while COMMITTED_PT_NODE_REGISTRY_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    RegistryGuard
}

// Committed-node registration and lookup are the bridge from page-table shape
// back to ownership. Register on commit, take on prune, then route release to
// either typed-frame teardown or boot-pool bitmap free.
pub(super) fn register_committed_pt_node(node: PtNode) {
    let _guard = lock_committed_pt_node_registry();
    let nodes = unsafe { &mut *COMMITTED_PT_NODES.0.get() };
    for slot in nodes.iter_mut() {
        if slot.is_some_and(|registered| registered.phys == node.phys) {
            return;
        }
    }
    for slot in nodes.iter_mut() {
        if slot.is_none() {
            *slot = Some(node);
            return;
        }
    }
    panic!("committed PT-node registry exhausted");
}

pub(super) fn register_committed_intermediates(intermediates: PmapReservationIntermediates) {
    if let Some(l1) = intermediates.l1 {
        register_committed_pt_node(l1);
    }
    if let Some(l0) = intermediates.l0 {
        register_committed_pt_node(l0);
    }
}

fn take_committed_pt_node(phys: PhysAddr) -> Option<PtNode> {
    let _guard = lock_committed_pt_node_registry();
    let nodes = unsafe { &mut *COMMITTED_PT_NODES.0.get() };
    for slot in nodes.iter_mut() {
        if slot.is_some_and(|registered| registered.phys == phys) {
            return slot.take();
        }
    }
    None
}

pub(super) fn release_committed_pt_node_from_bag<State>(
    bag: &BootStaticBag<State>,
    phys: PhysAddr,
) {
    if let Some(node) = take_committed_pt_node(phys) {
        free_pt_node_from_bag(bag, node);
    }
}

fn zero_pt_node<State>(bag: &BootStaticBag<State>, index: usize) {
    unsafe {
        zero_page(pt_node_zero_ptr(bag, index));
    }
}

const fn pool_full_mask() -> usize {
    (1usize << PT_NODE_POOL_ENTRIES) - 1
}

#[cfg(test)]
pub(super) fn install_pt_node_allocator_for_test(allocator: Option<PtNodeAllocator>) {
    INSTALLED_PT_NODE_ALLOCATOR.store(
        allocator.map_or(0, |allocator| allocator as usize),
        Ordering::Release,
    );
}

#[cfg(test)]
pub(super) fn reset_committed_pt_nodes_for_test() {
    let _guard = lock_committed_pt_node_registry();
    unsafe {
        (*COMMITTED_PT_NODES.0.get()).fill(None);
    }
}

#[cfg(test)]
pub(super) fn reset_pt_node_pool_allocations_for_test() {
    PT_NODE_ALLOCATED.store(0, Ordering::Release);
    reset_committed_pt_nodes_for_test();
}

#[cfg(test)]
pub(super) fn pt_node_allocated_for_test() -> usize {
    PT_NODE_ALLOCATED.load(Ordering::Acquire)
}
