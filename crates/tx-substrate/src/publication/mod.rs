use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use tx_hal::CpuId;

use crate::epoch::{self, EpochError, Guard, LocalRetireGuard, RcuHead, MAX_EPOCH_CPUS};

use node::PublishedNode;

mod node;

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
        self.acquire_writer();
        Ok(PublishReservation {
            owner: self,
            next: Some(next),
            committed: false,
        })
    }

    fn acquire_writer(&self) {
        while self
            .writer_claimed
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
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
    pub fn commit(mut self) {
        let next = self.next.expect("prepared publication node");
        let mut retried_after_drain = false;
        loop {
            let preflight = epoch::with_local_retire_guard(|local_guard| {
                let prepared_epoch = local_guard.sample_epoch_after_barrier();
                local_guard.prepare_head_bags_after_barrier(prepared_epoch)?;
                let old = self.owner.root.swap(next.as_ptr(), Ordering::AcqRel);
                self.committed = true;
                let retired_at = local_guard.sample_epoch_after_barrier();
                debug_assert!(
                    retired_at == prepared_epoch || retired_at == prepared_epoch.saturating_add(1)
                );
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
                Err(EpochError::RetireBagOccupied) if !retried_after_drain => {
                    retried_after_drain = true;
                    let _ = epoch::drain_with_budget(usize::MAX);
                }
                Err(err) => panic!("Published::commit retire bag preflight: {err:?}"),
            }
        }
        self.next = None;
    }
}

impl<T: Send + 'static> Drop for PublishReservation<'_, T> {
    fn drop(&mut self) {
        if !self.committed {
            if let Some(next) = self.next.take() {
                unsafe {
                    node::destroy(next);
                }
            }
        }
        self.owner.writer_claimed.store(false, Ordering::Release);
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
