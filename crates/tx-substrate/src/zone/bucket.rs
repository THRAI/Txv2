//! Small per-CPU cache of free zone slots.
//!
//! Buckets reduce trips to the central Keg. Access is protected by CPU pinning
//! at the `Zone` layer; the bucket itself is just a fixed-size stack.

use core::marker::PhantomData;
use core::ptr::NonNull;

use super::slot::Slot;

pub const DEFAULT_BUCKET_CAPACITY: usize = 32;

pub struct ZoneBucket<T: 'static, const N: usize = DEFAULT_BUCKET_CAPACITY> {
    /// Stack of free slots owned by this CPU bucket.
    slots: [Option<NonNull<Slot<T>>>; N],
    len: usize,
    _marker: PhantomData<T>,
}

impl<T: 'static, const N: usize> ZoneBucket<T, N> {
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; N],
            len: 0,
            _marker: PhantomData,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len == N
    }

    pub fn capacity(&self) -> usize {
        N
    }

    pub(crate) fn pop(&mut self) -> Option<NonNull<Slot<T>>> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        self.slots[self.len].take()
    }

    pub(crate) fn push(&mut self, slot: NonNull<Slot<T>>) -> Result<(), NonNull<Slot<T>>> {
        if self.len == N {
            return Err(slot);
        }
        self.slots[self.len] = Some(slot);
        self.len += 1;
        Ok(())
    }

    /// Move as many owned slots as possible into `target`.
    ///
    /// A slot that does not fit is restored before returning, so every slot
    /// remains owned by exactly one bucket throughout the transfer.
    pub(crate) fn move_slots_to<const M: usize>(&mut self, target: &mut ZoneBucket<T, M>) -> usize {
        let mut moved = 0usize;
        while let Some(slot) = self.pop() {
            match target.push(slot) {
                Ok(()) => moved += 1,
                Err(slot) => {
                    // `pop` just created one free entry in this bucket.
                    assert!(self.push(slot).is_ok());
                    break;
                }
            }
        }
        moved
    }

    pub(crate) fn clear(&mut self) {
        while self.pop().is_some() {}
    }
}

impl<T: 'static, const N: usize> Default for ZoneBucket<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::NonNull;

    use super::ZoneBucket;
    use crate::zone::slot::Slot;

    fn fake_slot<T>(address: usize) -> NonNull<Slot<T>> {
        NonNull::new(address as *mut Slot<T>).expect("non-zero fake slot")
    }

    #[test]
    fn move_slots_to_preserves_overflow_ownership() {
        let mut source = ZoneBucket::<u8, 3>::new();
        let mut target = ZoneBucket::<u8, 2>::new();
        source.push(fake_slot(0x1000)).expect("source slot 1");
        source.push(fake_slot(0x2000)).expect("source slot 2");
        source.push(fake_slot(0x3000)).expect("source slot 3");
        target.push(fake_slot(0x4000)).expect("target slot");

        assert_eq!(source.move_slots_to(&mut target), 1);
        assert_eq!(source.len(), 2);
        assert_eq!(target.len(), 2);

        let mut addresses = [
            source.pop().expect("source remainder").as_ptr() as usize,
            source.pop().expect("source remainder").as_ptr() as usize,
            target.pop().expect("target moved slot").as_ptr() as usize,
            target.pop().expect("target original slot").as_ptr() as usize,
        ];
        addresses.sort_unstable();
        assert_eq!(addresses, [0x1000, 0x2000, 0x3000, 0x4000]);
    }
}
