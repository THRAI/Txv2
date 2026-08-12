//! Individual typed storage slot.
//!
//! A slot stores only metadata plus the object bytes. Ownership information is
//! recovered from the containing slab page, keeping each slot compact.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU64, Ordering};

use super::cap::cap_kind_byte;
use super::meta::{SlotMeta, SlotState};
use super::registry::SlotKey;
use super::slab::ZoneSlab;

static RECLAIM_SLOT_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
pub(crate) struct Slot<T: 'static> {
    /// Packed lifecycle word for this slot.
    pub(crate) meta: SlotMeta,
    /// Object storage. Initialized only in Reserved/Live/Retiring states.
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

pub(crate) unsafe fn reclaim_slot<T: 'static>(slot: NonNull<Slot<T>>) {
    let trace_seq = reclaim_slot_trace_sample();
    if let Some(seq) = trace_seq {
        emit_reclaim_slot_trace(b"debug.zone.reclaim_slot.begin", seq);
        emit_reclaim_slot_trace(b"debug.zone.reclaim_slot.kind", cap_kind_byte::<T>() as i64);
        emit_reclaim_slot_trace(
            b"debug.zone.reclaim_slot.size",
            core::mem::size_of::<T>() as i64,
        );
    }
    unsafe {
        // EBR has proven that no guard-scoped IdentRef can still dereference
        // this object, so it is now safe to run T's destructor.
        ptr::drop_in_place(slot.as_ref().data_ptr());

        loop {
            let cur = slot.as_ref().meta().load(Ordering::Acquire);
            debug_assert_eq!(cur.state(), SlotState::Retiring);

            let new = cur.next_free_generation();
            if slot
                .as_ref()
                .meta()
                .compare_exchange(cur, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                if !new.generation_exhausted() {
                    let slab = ZoneSlab::from_slot(slot);
                    // Whole-slab retirement is skipped inside EBR callbacks.
                    slab.as_ref().zone().return_slot_from_reclaim(slot);
                }
                break;
            }
        }
    }
    if let Some(seq) = trace_seq {
        emit_reclaim_slot_trace(b"debug.zone.reclaim_slot.end", seq);
    }
}

fn reclaim_slot_trace_sample() -> Option<i64> {
    let seq = RECLAIM_SLOT_TRACE_SAMPLE.fetch_add(1, Ordering::Relaxed);
    (seq < 128 || seq.is_power_of_two()).then_some(seq as i64)
}

fn emit_reclaim_slot_trace(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
    }
}
