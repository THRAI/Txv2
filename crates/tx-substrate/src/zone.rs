//! Bounded zone slots and role-shaped identity handles.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use crate::epoch::Guard;

const EMPTY: u8 = 0;
const RESERVED: u8 = 1;
const COMMITTED: u8 = 2;

/// Zone reservation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZoneError {
    /// No free slot exists in this bounded zone.
    Full,
}

struct Slot<T> {
    state: AtomicU8,
    generation: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Slot<T> {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            generation: AtomicUsize::new(0),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    fn bump_generation(&self) -> usize {
        let mut current = self.generation.load(Ordering::Acquire);
        loop {
            let mut next = current.wrapping_add(1);
            if next == 0 {
                next = 1;
            }

            match self.generation.compare_exchange(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return next,
                Err(observed) => current = observed,
            }
        }
    }
}

/// A bounded, no-heap zone for identity-bearing objects.
pub struct Zone<T, const N: usize> {
    slots: [Slot<T>; N],
}

unsafe impl<T: Send, const N: usize> Sync for Zone<T, N> {}

impl<T, const N: usize> Zone<T, N> {
    /// Construct an empty zone.
    pub const fn new() -> Self {
        Self {
            slots: [const { Slot::new() }; N],
        }
    }

    /// Reserve one empty slot. Dropping the reservation rolls it back.
    pub fn reserve(&self) -> Result<ZoneReservation<'_, T, N>, ZoneError> {
        reserve(self)
    }

    /// Sign a reservation by installing a value and publishing a `Cap`.
    pub fn sign(&self, reservation: ZoneReservation<'_, T, N>, value: T) -> Cap<T> {
        sign(reservation, value)
    }
}

impl<T, const N: usize> Default for Zone<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for Zone<T, N> {
    fn drop(&mut self) {
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire) == COMMITTED {
                unsafe {
                    (*slot.value.get()).assume_init_drop();
                }
            }
        }
    }
}

/// Reserve one empty slot from a zone.
pub fn reserve<T, const N: usize>(
    zone: &Zone<T, N>,
) -> Result<ZoneReservation<'_, T, N>, ZoneError> {
    for (slot_index, slot) in zone.slots.iter().enumerate() {
        if slot
            .state
            .compare_exchange(EMPTY, RESERVED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let generation = slot.bump_generation();
            return Ok(ZoneReservation {
                zone,
                slot_index,
                generation,
                committed: false,
            });
        }
    }

    Err(ZoneError::Full)
}

/// Publish a reserved slot and return identity-retaining evidence.
pub fn sign<T, const N: usize>(mut reservation: ZoneReservation<'_, T, N>, value: T) -> Cap<T> {
    let slot = &reservation.zone.slots[reservation.slot_index];
    unsafe {
        (*slot.value.get()).write(value);
    }
    slot.state.store(COMMITTED, Ordering::Release);
    reservation.committed = true;

    Cap {
        slot: slot as *const Slot<T>,
        slot_index: reservation.slot_index,
        generation: reservation.generation,
        _marker: PhantomData,
    }
}

/// Linear slot reservation. Drop rolls back when unsigned.
pub struct ZoneReservation<'z, T, const N: usize> {
    zone: &'z Zone<T, N>,
    slot_index: usize,
    generation: usize,
    committed: bool,
}

impl<T, const N: usize> ZoneReservation<'_, T, N> {
    /// Reserved slot index.
    pub fn slot_index(&self) -> usize {
        self.slot_index
    }

    /// Generation that will be published if this reservation is signed.
    pub fn generation(&self) -> usize {
        self.generation
    }
}

impl<T, const N: usize> Drop for ZoneReservation<'_, T, N> {
    fn drop(&mut self) {
        if !self.committed {
            self.zone.slots[self.slot_index]
                .state
                .store(EMPTY, Ordering::Release);
        }
    }
}

/// Identity-retaining handle for a committed zone slot.
pub struct Cap<T> {
    slot: *const Slot<T>,
    slot_index: usize,
    generation: usize,
    _marker: PhantomData<T>,
}

impl<T> Clone for Cap<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Cap<T> {}

impl<T> Cap<T> {
    /// Slot index inside the source zone.
    pub fn slot_index(&self) -> usize {
        self.slot_index
    }

    /// Slot generation captured when the cap was signed.
    pub fn generation(&self) -> usize {
        self.generation
    }

    /// Downgrade to a non-retaining generation-checked weak handle.
    pub fn downgrade(&self) -> Weak<T> {
        Weak {
            slot: self.slot,
            slot_index: self.slot_index,
            generation: self.generation,
            _marker: PhantomData,
        }
    }

    /// Observe the identity under a guard.
    pub fn ident_ref<'g>(&self, guard: &'g Guard<'_>) -> Option<IdentRef<'g, T>> {
        self.downgrade().upgrade(guard)
    }
}

/// Non-retaining, generation-checked identity hint.
pub struct Weak<T> {
    slot: *const Slot<T>,
    slot_index: usize,
    generation: usize,
    _marker: PhantomData<T>,
}

impl<T> Clone for Weak<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Weak<T> {}

impl<T> Weak<T> {
    /// Slot index inside the source zone.
    pub fn slot_index(&self) -> usize {
        self.slot_index
    }

    /// Captured generation.
    pub fn generation(&self) -> usize {
        self.generation
    }

    /// Upgrade to a guard-scoped observation if the slot is still committed
    /// at the same generation.
    pub fn upgrade<'g>(&self, _guard: &'g Guard<'_>) -> Option<IdentRef<'g, T>> {
        let slot = unsafe { self.slot.as_ref()? };
        if slot.state.load(Ordering::Acquire) != COMMITTED {
            return None;
        }
        if slot.generation.load(Ordering::Acquire) != self.generation {
            return None;
        }

        let value = unsafe { &*(*slot.value.get()).as_ptr() };
        Some(IdentRef {
            value,
            slot_index: self.slot_index,
            generation: self.generation,
        })
    }
}

/// Guard-scoped identity observation.
pub struct IdentRef<'g, T> {
    value: &'g T,
    slot_index: usize,
    generation: usize,
}

impl<T> IdentRef<'_, T> {
    /// Slot index inside the source zone.
    pub fn slot_index(&self) -> usize {
        self.slot_index
    }

    /// Observed generation.
    pub fn generation(&self) -> usize {
        self.generation
    }
}

impl<T> Deref for IdentRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value
    }
}
