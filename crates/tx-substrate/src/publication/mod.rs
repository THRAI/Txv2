use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use tx_hal::CpuId;

use crate::epoch::{self, EpochError, Guard, LocalRetireGuard, RcuHead, MAX_EPOCH_CPUS};

use node::PublishedNode;

mod node;

const PUBLICATION_RETRY_DRAIN_BUDGET: usize = 64;

#[cfg(test)]
static RETRY_DRAIN_TEST_EPOCH_ADVANCES: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishError {
    Allocation,
}

/// Guard-scoped immutable root publication.
///
/// Retired values may be destroyed on another CPU after the owner is dropped,
/// so borrowed or non-`Send` payloads are rejected at the type boundary.
///
/// ```compile_fail
/// use std::rc::Rc;
/// use tx_substrate::Published;
/// let _ = Published::try_new(Rc::new(1));
/// ```
///
/// ```compile_fail
/// use tx_substrate::Published;
/// let value = 1;
/// let _ = Published::try_new(&value);
/// ```
pub struct Published<T: Send + 'static> {
    root: AtomicPtr<PublishedNode<T>>,
    writer_claimed: AtomicBool,
    _owns_value: PhantomData<T>,
}

pub struct PublishReservation<'a, T: Send + 'static> {
    owner: &'a Published<T>,
    next: Option<NonNull<PublishedNode<T>>>,
    committed: bool,
    claim: Option<WriterClaimGuard<'a, T>>,
}

/// An allocated publication node that owns no writer authority.
#[must_use = "detached publication nodes must be committed or explicitly dropped"]
pub struct DetachedPublication<T: Send + 'static> {
    next: Option<NonNull<PublishedNode<T>>>,
}

#[must_use = "reserved detached publication storage must be initialized or released"]
pub struct DetachedPublicationSlot<T: Send + 'static> {
    node: Option<NonNull<MaybeUninit<PublishedNode<T>>>>,
}

pub struct DetachedPublicationParts<T: Send + 'static> {
    pub value: T,
    pub allocation: DetachedNodeAllocation<T>,
}

#[must_use = "detached publication wrapper allocations must be released"]
pub struct DetachedNodeAllocation<T: Send + 'static> {
    node: Option<NonNull<MaybeUninit<PublishedNode<T>>>>,
}

unsafe impl<T: Send + 'static> Send for DetachedPublicationSlot<T> {}
unsafe impl<T: Send + 'static> Send for DetachedNodeAllocation<T> {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishRetryReason {
    WriterBusy,
    RetireBackpressure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservedCommitInvariant {
    WriterBusy,
    WrongCpu,
    MissingRetireCredit,
}

#[must_use = "publication retries retain the detached node"]
pub struct PublishCommitRetry<T: Send + 'static> {
    reason: PublishRetryReason,
    publication: DetachedPublication<T>,
}

struct WriterClaimGuard<'a, T: Send + 'static> {
    owner: &'a Published<T>,
}

impl<T: Send + 'static> Published<T> {
    pub fn try_new(initial: T) -> Result<Self, PublishError> {
        let initial = node::try_allocate(initial, defer_node::<T>)?;
        Ok(Self {
            root: AtomicPtr::new(initial.as_ptr()),
            writer_claimed: AtomicBool::new(false),
            _owns_value: PhantomData,
        })
    }

    pub fn read<'g>(&'g self, _guard: &'g Guard<'_>) -> &'g T {
        let root = self.root.load(Ordering::Acquire);
        debug_assert!(!root.is_null());
        unsafe { &(*root).value }
    }

    pub fn prepare_replace(&self, next: T) -> Result<PublishReservation<'_, T>, PublishError> {
        let next = node::try_allocate(next, defer_node::<T>)?;
        let claim = self.acquire_writer();
        Ok(PublishReservation {
            owner: self,
            next: Some(next),
            committed: false,
            claim: Some(claim),
        })
    }

    pub fn prepare_detached(&self, next: T) -> Result<DetachedPublication<T>, PublishError> {
        let next = node::try_allocate(next, defer_node::<T>)?;
        Ok(DetachedPublication { next: Some(next) })
    }

    pub fn prepare_detached_reserved(
        &self,
        next: T,
        mut slot: DetachedPublicationSlot<T>,
    ) -> DetachedPublication<T> {
        let raw = slot
            .node
            .take()
            .expect("reserved detached publication slot");
        unsafe {
            raw.as_ptr()
                .write(MaybeUninit::new(PublishedNode::new(next, defer_node::<T>)));
        }
        DetachedPublication {
            next: Some(raw.cast()),
        }
    }

    /// Attempt one bounded publication commit.
    ///
    /// The final retire preflight, writer claim, root swap, and enqueue are one
    /// callback-free local-retire interval. Contention and retire pressure are
    /// reported before the root changes, with the detached node returned for a
    /// lock-external retry.
    pub fn try_commit(
        &self,
        mut publication: DetachedPublication<T>,
    ) -> Result<(), PublishCommitRetry<T>> {
        let next = publication.next.expect("detached publication node");
        let outcome = epoch::with_local_retire_guard(|local_guard| {
            let prepared_epoch = local_guard.sample_epoch_after_barrier();
            if local_guard
                .preflight_head_bags_after_barrier(prepared_epoch)
                .is_err()
            {
                return Err(PublishRetryReason::RetireBackpressure);
            }
            let Some(_writer) = self.try_acquire_writer() else {
                return Err(PublishRetryReason::WriterBusy);
            };
            let old = self.root.swap(next.as_ptr(), Ordering::AcqRel);
            // A continuously active LocalRetireGuard permits at most the one
            // epoch advance already covered by the two preflighted bags.
            let retired_at = local_guard.sample_epoch_after_barrier();
            unsafe {
                local_guard.enqueue_prepared_head_after_barrier(
                    NonNull::new_unchecked(old.cast::<RcuHead>()),
                    retired_at,
                );
            }
            publication.next = None;
            Ok(())
        })
        .expect("Published::try_commit requires an initialized online epoch CPU");

        outcome.map_err(|reason| PublishCommitRetry {
            reason,
            publication,
        })
    }

    pub fn commit_reserved(
        &self,
        mut publication: DetachedPublication<T>,
        retire: epoch::LocalRetireReservation,
    ) -> Result<(), ReservedCommitInvariant> {
        let next = publication.next.expect("detached publication node");
        epoch::with_local_retire_guard(|local_guard| {
            retire
                .validate_for(local_guard)
                .map_err(map_reserved_retire_invariant)?;
            let Some(_writer) = self.try_acquire_writer() else {
                return Err(ReservedCommitInvariant::WriterBusy);
            };
            let old = self.root.swap(next.as_ptr(), Ordering::AcqRel);
            publication.next = None;
            let retired_at = local_guard.sample_epoch_after_barrier();
            unsafe {
                retire
                    .enqueue_after_barrier(
                        local_guard,
                        NonNull::new_unchecked(old.cast::<RcuHead>()),
                        retired_at,
                    )
                    .expect("validated local-retire credit changed during reserved commit");
            }
            Ok(())
        })
        .map_err(|_| ReservedCommitInvariant::MissingRetireCredit)?
    }

    fn acquire_writer(&self) -> WriterClaimGuard<'_, T> {
        while self
            .writer_claimed
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        WriterClaimGuard { owner: self }
    }

    fn try_acquire_writer(&self) -> Option<WriterClaimGuard<'_, T>> {
        self.writer_claimed
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| WriterClaimGuard { owner: self })
    }
}

fn map_reserved_retire_invariant(
    invariant: epoch::ReservedRetireInvariant,
) -> ReservedCommitInvariant {
    match invariant {
        epoch::ReservedRetireInvariant::WrongCpu => ReservedCommitInvariant::WrongCpu,
        epoch::ReservedRetireInvariant::SlotNotReserved
        | epoch::ReservedRetireInvariant::SlotAlreadyFilled => {
            ReservedCommitInvariant::MissingRetireCredit
        }
    }
}

impl<T: Send + 'static> DetachedPublicationSlot<T> {
    pub fn try_new() -> Result<Self, PublishError> {
        Ok(Self {
            node: Some(node::try_allocate_uninit()?),
        })
    }
}

impl<T: Send + 'static> DetachedNodeAllocation<T> {
    pub fn into_slot(mut self) -> DetachedPublicationSlot<T> {
        DetachedPublicationSlot {
            node: self.node.take(),
        }
    }
}

impl<T: Send + 'static> Drop for DetachedPublicationSlot<T> {
    fn drop(&mut self) {
        if let Some(node) = self.node.take() {
            unsafe {
                node::deallocate_uninit(node);
            }
        }
    }
}

impl<T: Send + 'static> Drop for DetachedNodeAllocation<T> {
    fn drop(&mut self) {
        if let Some(node) = self.node.take() {
            unsafe {
                node::deallocate_uninit(node);
            }
        }
    }
}

impl<T: Send + 'static> Drop for WriterClaimGuard<'_, T> {
    fn drop(&mut self) {
        self.owner.writer_claimed.store(false, Ordering::Release);
    }
}

impl<T: Send + 'static> PublishCommitRetry<T> {
    pub fn reason(&self) -> PublishRetryReason {
        self.reason
    }

    pub fn into_publication(mut self) -> DetachedPublication<T> {
        DetachedPublication {
            next: self.publication.next.take(),
        }
    }
}

impl<T: Send + 'static> DetachedPublication<T> {
    pub fn into_deferred_parts(mut self) -> DetachedPublicationParts<T> {
        let node = self.next.take().expect("detached publication node");
        let value = unsafe { core::ptr::read(core::ptr::addr_of!((*node.as_ptr()).value)) };
        DetachedPublicationParts {
            value,
            allocation: DetachedNodeAllocation {
                node: Some(node.cast()),
            },
        }
    }
}

impl<T: Send + 'static> Drop for DetachedPublication<T> {
    fn drop(&mut self) {
        if let Some(next) = self.next.take() {
            unsafe {
                node::destroy(next);
            }
        }
    }
}

impl<T: Send + 'static> Drop for Published<T> {
    fn drop(&mut self) {
        debug_assert!(!self.writer_claimed.load(Ordering::Relaxed));
        let root = self.root.load(Ordering::Relaxed);
        let root = NonNull::new(root).expect("Published root must remain initialized");
        unsafe {
            node::destroy(root);
        }
    }
}

impl<T: Send + 'static> PublishReservation<'_, T> {
    #[track_caller]
    pub fn commit(mut self) {
        let caller = core::panic::Location::caller();
        let next = self.next.expect("prepared publication node");
        let mut retry_drain = None;
        loop {
            let preflight = epoch::with_local_retire_guard(|local_guard| {
                let prepared_epoch = local_guard.sample_epoch_after_barrier();
                local_guard.preflight_head_bags_after_barrier(prepared_epoch)?;
                let old = self.owner.root.swap(next.as_ptr(), Ordering::AcqRel);
                self.committed = true;
                let retired_at = local_guard.sample_epoch_after_barrier();
                unsafe {
                    local_guard.enqueue_prepared_head_after_barrier(
                        NonNull::new_unchecked(old.cast::<RcuHead>()),
                        retired_at,
                    );
                }
                Ok(())
            })
            .expect("Published::commit requires an initialized online epoch CPU");

            match preflight {
                Ok(()) => break,
                Err(EpochError::RetireBagOccupied) => {
                    drop(self.claim.take());
                    retry_drain = Some(epoch::drain_with_budget(PUBLICATION_RETRY_DRAIN_BUDGET));
                    #[cfg(test)]
                    for _ in 0..RETRY_DRAIN_TEST_EPOCH_ADVANCES.swap(0, Ordering::AcqRel) {
                        let _ = epoch::drain_with_budget(0);
                    }
                    self.claim = Some(self.owner.acquire_writer());
                }
                Err(err) => panic!(
                    "Published::commit retire bag preflight: {err:?}; caller={}:{}; retry_drain={retry_drain:?}; epoch={:?}; bags={:?}",
                    caller.file(),
                    caller.line(),
                    epoch::summary(),
                    epoch::local_head_bag_summary(),
                ),
            }
        }
        self.next = None;
        drop(self.claim.take());
    }
}

impl<T: Send + 'static> Drop for PublishReservation<'_, T> {
    fn drop(&mut self) {
        if !self.committed {
            drop(self.claim.take());
            if let Some(next) = self.next.take() {
                unsafe {
                    node::destroy(next);
                }
            }
        }
    }
}

struct DeferredDropList {
    head: UnsafeCell<*mut RcuHead>,
    tail: UnsafeCell<*mut RcuHead>,
    count: AtomicUsize,
}

unsafe impl Sync for DeferredDropList {}

impl DeferredDropList {
    const fn new() -> Self {
        Self {
            head: UnsafeCell::new(core::ptr::null_mut()),
            tail: UnsafeCell::new(core::ptr::null_mut()),
            count: AtomicUsize::new(0),
        }
    }
}

static DEFERRED_DROPS: [DeferredDropList; MAX_EPOCH_CPUS] =
    [const { DeferredDropList::new() }; MAX_EPOCH_CPUS];

pub(crate) struct DeferredDropStats {
    pub(crate) dropped: usize,
    pub(crate) remaining: usize,
}

pub(crate) fn drain_deferred_drops(budget: usize) -> DeferredDropStats {
    let drained = epoch::with_local_retire_guard(|local_guard| {
        let cpu = local_guard.cpu_id();
        let mut dropped = 0usize;
        while dropped < budget {
            let current = unsafe { detach_one_deferred(cpu) };
            if current.is_null() {
                break;
            }
            let reclaim = unsafe { (*current).reclaim };
            local_guard.with_local_execution_open(|local_guard| unsafe {
                reclaim(current, local_guard);
            });
            dropped += 1;
        }
        DeferredDropStats {
            dropped,
            remaining: deferred_drop_count(cpu),
        }
    });
    let Ok(stats) = drained else {
        return DeferredDropStats {
            dropped: 0,
            remaining: 0,
        };
    };
    stats
}

pub(crate) unsafe fn transfer_deferred_drops(source: CpuId, destination: CpuId) {
    if source == destination {
        return;
    }
    let source = &DEFERRED_DROPS[source.0];
    let destination = &DEFERRED_DROPS[destination.0];
    let source_head = unsafe { *source.head.get() };
    if source_head.is_null() {
        return;
    }
    let source_tail = unsafe { *source.tail.get() };
    let destination_head = unsafe { *destination.head.get() };
    unsafe {
        (*source_tail).next = destination_head;
        *destination.head.get() = source_head;
        if destination_head.is_null() {
            *destination.tail.get() = source_tail;
        }
        *source.head.get() = core::ptr::null_mut();
        *source.tail.get() = core::ptr::null_mut();
    }
    let transferred = source.count.swap(0, Ordering::Relaxed);
    destination.count.fetch_add(transferred, Ordering::Relaxed);
}

pub(crate) fn fail_next_allocations_for_test(count: usize) {
    node::fail_next_allocations(count);
}

pub(crate) fn deferred_drop_count(cpu: CpuId) -> usize {
    DEFERRED_DROPS[cpu.0].count.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_recovers_when_epoch_advances_again_after_retry_drain() {
        crate::testing::init_host_for_test_once();
        for _ in 0..4 {
            let _ = epoch::drain_with_budget(usize::MAX);
        }

        let cell = Published::try_new(0usize).expect("initial publication");
        for value in 1..=3 {
            cell.prepare_replace(value)
                .expect("prepared replacement")
                .commit();
            let _ = epoch::drain_with_budget(0);
        }

        RETRY_DRAIN_TEST_EPOCH_ADVANCES.store(2, Ordering::Release);
        cell.prepare_replace(4)
            .expect("prepared replacement after a second epoch advance")
            .commit();

        let guard = epoch::guard();
        assert_eq!(*cell.read(&guard), 4);
    }
}

unsafe fn defer_node<T: Send + 'static>(head: *mut RcuHead, local_guard: &mut LocalRetireGuard) {
    unsafe {
        (*head).reclaim = drop_node::<T>;
        push_deferred(local_guard.cpu_id(), head);
    }
}

unsafe fn drop_node<T: Send + 'static>(head: *mut RcuHead, _local_guard: &mut LocalRetireGuard) {
    let node = NonNull::new(head.cast::<PublishedNode<T>>())
        .expect("deferred Published node must be non-null");
    unsafe {
        node::destroy(node);
    }
}

unsafe fn push_deferred(cpu: CpuId, head: *mut RcuHead) {
    let list = &DEFERRED_DROPS[cpu.0];
    let previous = unsafe { *list.head.get() };
    unsafe {
        (*head).next = previous;
        *list.head.get() = head;
        if previous.is_null() {
            *list.tail.get() = head;
        }
    }
    list.count.fetch_add(1, Ordering::Relaxed);
}

unsafe fn detach_one_deferred(cpu: CpuId) -> *mut RcuHead {
    let list = &DEFERRED_DROPS[cpu.0];
    let current = unsafe { *list.head.get() };
    if current.is_null() {
        return current;
    }
    unsafe {
        *list.head.get() = (*current).next;
    }
    if unsafe { *list.head.get() }.is_null() {
        unsafe {
            *list.tail.get() = core::ptr::null_mut();
        }
    }
    list.count.fetch_sub(1, Ordering::Relaxed);
    current
}
