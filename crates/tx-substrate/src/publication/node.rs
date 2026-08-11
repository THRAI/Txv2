use alloc::alloc::{alloc, dealloc};
use core::alloc::Layout;
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::epoch::RcuHead;

use super::PublishError;

static FAIL_NEXT_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
pub(super) struct PublishedNode<T: Send + 'static> {
    pub(super) head: RcuHead,
    pub(super) value: T,
}

impl<T: Send + 'static> PublishedNode<T> {
    pub(super) fn new(
        value: T,
        reclaim: unsafe fn(*mut RcuHead, &mut crate::epoch::LocalRetireGuard),
    ) -> Self {
        Self {
            head: RcuHead::new(reclaim),
            value,
        }
    }
}

const _: () = assert!(core::mem::size_of::<RcuHead>() == 16);

pub(super) fn try_allocate<T: Send + 'static>(
    value: T,
    reclaim: unsafe fn(*mut RcuHead, &mut crate::epoch::LocalRetireGuard),
) -> Result<NonNull<PublishedNode<T>>, PublishError> {
    if consume_injected_failure() {
        return Err(PublishError::Allocation);
    }

    let layout = Layout::new::<PublishedNode<T>>();
    let raw = unsafe { alloc(layout) }.cast::<PublishedNode<T>>();
    let node = NonNull::new(raw).ok_or(PublishError::Allocation)?;
    unsafe {
        ptr::write(node.as_ptr(), PublishedNode::new(value, reclaim));
    }
    Ok(node)
}

pub(super) fn try_allocate_uninit<T: Send + 'static>(
) -> Result<NonNull<MaybeUninit<PublishedNode<T>>>, PublishError> {
    if consume_injected_failure() {
        return Err(PublishError::Allocation);
    }

    let raw =
        unsafe { alloc(Layout::new::<PublishedNode<T>>()) }.cast::<MaybeUninit<PublishedNode<T>>>();
    NonNull::new(raw).ok_or(PublishError::Allocation)
}

pub(super) unsafe fn deallocate_uninit<T: Send + 'static>(
    node: NonNull<MaybeUninit<PublishedNode<T>>>,
) {
    unsafe {
        dealloc(
            node.as_ptr().cast::<u8>(),
            Layout::new::<PublishedNode<T>>(),
        );
    }
}

pub(super) unsafe fn destroy<T: Send + 'static>(node: NonNull<PublishedNode<T>>) {
    struct AllocationGuard<T: Send + 'static>(NonNull<PublishedNode<T>>);

    impl<T: Send + 'static> Drop for AllocationGuard<T> {
        fn drop(&mut self) {
            unsafe {
                dealloc(
                    self.0.as_ptr().cast::<u8>(),
                    Layout::new::<PublishedNode<T>>(),
                );
            }
        }
    }

    let _allocation = AllocationGuard(node);
    unsafe {
        ptr::drop_in_place(ptr::addr_of_mut!((*node.as_ptr()).value));
    }
}

pub(super) fn fail_next_allocations(count: usize) {
    FAIL_NEXT_ALLOCATIONS.store(count, Ordering::Release);
}

fn consume_injected_failure() -> bool {
    FAIL_NEXT_ALLOCATIONS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
}
