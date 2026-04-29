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

    pub(crate) fn clear(&mut self) {
        while self.pop().is_some() {}
    }
}

impl<T: 'static, const N: usize> Default for ZoneBucket<T, N> {
    fn default() -> Self {
        Self::new()
    }
}
