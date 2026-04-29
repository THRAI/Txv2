//! Individual typed storage slot.
//!
//! A slot stores only metadata plus the object bytes. Ownership information is
//! recovered from the containing slab page, keeping each slot compact.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull};
use core::sync::atomic::Ordering;

use super::meta::{SlotMeta, SlotState};
use super::registry::SlotKey;
use super::slab::ZoneSlab;

#[repr(C)]
pub(crate) struct Slot<T: 'static> {
    /// Packed lifecycle word for this slot.
    pub(crate) meta: SlotMeta,
    /// Object storage. Initialized only in Reserved/Live/Dead/Retiring states.
    value: UnsafeCell<MaybeUninit<T>>,
}

unsafe impl<T: Send> Send for Slot<T> {}
unsafe impl<T: Send + Sync> Sync for Slot<T> {}

impl<T: 'static> Slot<T> {
    pub(crate) fn new_free() -> Self {
        Self {
            meta: SlotMeta::free(),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    pub(crate) fn meta(&self) -> &SlotMeta {
        &self.meta
    }

    pub(crate) fn slab(&self) -> NonNull<ZoneSlab<T>> {
        // Slots live in a page whose base address is the slab header.
        unsafe { ZoneSlab::from_slot(NonNull::from(self)) }
    }

    pub(crate) fn key(&self) -> SlotKey {
        let slab = self.slab();
        unsafe { slab.as_ref().key_for_slot(NonNull::from(self)) }
            .expect("zone slot must belong to its containing slab")
    }

    pub(crate) unsafe fn write_value(&self, value: T) {
        unsafe {
            (*self.value.get()).write(value);
        }
    }

    pub(crate) unsafe fn data_ptr(&self) -> *mut T {
        unsafe { (*self.value.get()).as_mut_ptr() }
    }

    pub(crate) unsafe fn data_ref(&self) -> &T {
        unsafe { &*self.data_ptr() }
    }
}

pub(crate) unsafe fn reclaim_slot<T: 'static>(ptr: *mut u8) {
    let slot = ptr as *mut Slot<T>;
    unsafe {
        // EBR has proven that no guard-scoped IdentRef can still dereference
        // this object, so it is now safe to run T's destructor.
        ptr::drop_in_place((*slot).data_ptr());

        loop {
            let cur = (*slot).meta().load(Ordering::Acquire);
            debug_assert_eq!(cur.state(), SlotState::Retiring);
            debug_assert_eq!(cur.retain(), 0);

            let new = cur
                .inc_generation()
                .with_retain(0)
                .with_state(SlotState::Free);
            if (*slot)
                .meta()
                .compare_exchange(cur, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                let slot = NonNull::new_unchecked(slot);
                let slab = ZoneSlab::from_slot(slot);
                // Return only the slot here. Whole-slab retirement is skipped
                // inside EBR callbacks to avoid nested EBR enqueue paths.
                slab.as_ref().zone().return_slot_from_reclaim(slot);
                break;
            }
        }
    }
}
