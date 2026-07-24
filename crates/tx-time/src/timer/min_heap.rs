use alloc::vec::Vec;

use crate::TimerKey;

use super::queue::TimerQueue;

#[derive(Clone, Copy)]
struct HeapEntry {
    key: TimerKey,
    deadline_ns: u64,
    generation: u64,
}

struct Slot {
    key: TimerKey,
    deadline_ns: u64,
    generation: u64,
    heap_index: usize,
    live: bool,
}

#[derive(Clone, Copy)]
struct KeySlot {
    key: TimerKey,
    slot_index: usize,
}

/// Indexed binary min-heap ordered by `(deadline_ns, key.raw())`.
pub(super) struct MinHeap {
    heap: Vec<HeapEntry>,
    slots: Vec<Slot>,
    key_slots: Vec<Option<KeySlot>>,
}

impl MinHeap {
    pub(super) const fn new() -> Self {
        Self {
            heap: Vec::new(),
            slots: Vec::new(),
            key_slots: Vec::new(),
        }
    }

    pub(super) fn has_insert_capacity(&self) -> bool {
        self.heap.len() < self.heap.capacity()
            && self.slots.len() < self.slots.capacity()
            && self.key_slots_can_insert()
    }

    /// Reserve one insertion slot while the engine state is not spin-locked.
    pub(super) fn reserve_for_insert(&mut self) {
        self.heap.reserve(1);
        self.slots.reserve(1);
        if self.key_slots_can_insert() {
            return;
        }

        let required = self
            .slots
            .len()
            .checked_add(1)
            .and_then(|count| count.checked_mul(2))
            .expect("timer key index capacity exhausted")
            .max(8);
        let mut key_slots = Vec::with_capacity(required);
        key_slots.resize(required, None);
        for (slot_index, slot) in self.slots.iter().enumerate() {
            Self::insert_key_slot_into(&mut key_slots, slot.key, slot_index);
        }
        self.key_slots = key_slots;
    }

    pub(super) fn due_count(&self, now_ns: u64) -> usize {
        self.heap
            .iter()
            .filter(|entry| entry.deadline_ns <= now_ns)
            .count()
    }

    fn key_slots_can_insert(&self) -> bool {
        !self.key_slots.is_empty()
            && self
                .slots
                .len()
                .checked_add(1)
                .and_then(|count| count.checked_mul(2))
                .is_some_and(|required| required <= self.key_slots.len())
    }

    fn hash(key: TimerKey) -> usize {
        let mut raw = key.raw();
        raw ^= raw >> 33;
        raw = raw.wrapping_mul(0xff51_afd7_ed55_8ccd);
        raw ^= raw >> 33;
        raw = raw.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        (raw ^ (raw >> 33)) as usize
    }

    fn key_slot_index(key_slots: &[Option<KeySlot>], key: TimerKey) -> Option<usize> {
        if key_slots.is_empty() {
            return None;
        }
        let mut index = Self::hash(key) % key_slots.len();
        loop {
            match key_slots[index] {
                Some(entry) if entry.key == key => return Some(index),
                Some(_) => index = (index + 1) % key_slots.len(),
                None => return None,
            }
        }
    }

    fn insert_key_slot_into(key_slots: &mut [Option<KeySlot>], key: TimerKey, slot_index: usize) {
        let mut index = Self::hash(key) % key_slots.len();
        loop {
            if key_slots[index].is_none() {
                key_slots[index] = Some(KeySlot { key, slot_index });
                return;
            }
            index = (index + 1) % key_slots.len();
        }
    }

    fn slot_index(&self, key: TimerKey) -> Option<usize> {
        let key_slot = Self::key_slot_index(&self.key_slots, key)?;
        Some(self.key_slots[key_slot]?.slot_index)
    }

    fn slot(&self, key: TimerKey) -> Option<&Slot> {
        self.slot_index(key).and_then(|index| self.slots.get(index))
    }

    fn slot_mut(&mut self, key: TimerKey) -> Option<&mut Slot> {
        let index = self.slot_index(key)?;
        self.slots.get_mut(index)
    }

    fn less(left: HeapEntry, right: HeapEntry) -> bool {
        (left.deadline_ns, left.key.raw()) < (right.deadline_ns, right.key.raw())
    }

    fn swap_entries(&mut self, left: usize, right: usize) {
        self.heap.swap(left, right);
        let left_key = self.heap[left].key;
        let right_key = self.heap[right].key;
        self.slot_mut(left_key)
            .expect("heap entry must have a slot")
            .heap_index = left;
        self.slot_mut(right_key)
            .expect("heap entry must have a slot")
            .heap_index = right;
    }

    fn sift_up(&mut self, mut index: usize) {
        while index != 0 {
            let parent = (index - 1) / 2;
            if !Self::less(self.heap[index], self.heap[parent]) {
                break;
            }
            self.swap_entries(index, parent);
            index = parent;
        }
    }

    fn sift_down(&mut self, mut index: usize) {
        loop {
            let left = index * 2 + 1;
            if left >= self.heap.len() {
                return;
            }
            let right = left + 1;
            let child = if right < self.heap.len() && Self::less(self.heap[right], self.heap[left])
            {
                right
            } else {
                left
            };
            if !Self::less(self.heap[child], self.heap[index]) {
                return;
            }
            self.swap_entries(index, child);
            index = child;
        }
    }

    fn repair(&mut self, index: usize) {
        if index != 0 && Self::less(self.heap[index], self.heap[(index - 1) / 2]) {
            self.sift_up(index);
        } else {
            self.sift_down(index);
        }
    }

    fn remove_at(&mut self, index: usize) -> HeapEntry {
        let removed = self.heap.swap_remove(index);
        if index < self.heap.len() {
            let moved_key = self.heap[index].key;
            self.slot_mut(moved_key)
                .expect("moved heap entry must have a slot")
                .heap_index = index;
            self.repair(index);
        }
        removed
    }
}

impl TimerQueue for MinHeap {
    fn insert(&mut self, key: TimerKey, deadline_ns: u64) {
        assert!(
            self.has_insert_capacity(),
            "timer heap capacity was not prepared"
        );
        let index = self.slots.len();
        let heap_index = self.heap.len();
        self.slots.push(Slot {
            key,
            deadline_ns,
            generation: 0,
            heap_index,
            live: true,
        });
        Self::insert_key_slot_into(&mut self.key_slots, key, index);
        self.heap.push(HeapEntry {
            key,
            deadline_ns,
            generation: 0,
        });
        self.sift_up(heap_index);
    }

    fn remove(&mut self, key: TimerKey) -> bool {
        let Some(slot) = self.slot(key) else {
            return false;
        };
        if !slot.live {
            return false;
        }
        let heap_index = slot.heap_index;
        self.remove_at(heap_index);
        self.slot_mut(key)
            .expect("removed heap entry must have a slot")
            .live = false;
        true
    }

    fn rearm(&mut self, key: TimerKey, deadline_ns: u64) -> bool {
        let Some(slot) = self.slot_mut(key) else {
            return false;
        };
        if !slot.live {
            return false;
        }
        slot.generation = slot.generation.wrapping_add(1);
        slot.deadline_ns = deadline_ns;
        let heap_index = slot.heap_index;
        let generation = slot.generation;
        self.heap[heap_index] = HeapEntry {
            key,
            deadline_ns,
            generation,
        };
        self.repair(heap_index);
        true
    }

    fn drain_due(&mut self, now_ns: u64, out: &mut Vec<TimerKey>) {
        while let Some(entry) = self.heap.first().copied() {
            if entry.deadline_ns > now_ns {
                break;
            }
            let removed = self.remove_at(0);
            let Some(slot) = self.slot_mut(removed.key) else {
                continue;
            };
            if slot.live && slot.generation == removed.generation {
                slot.live = false;
                assert!(
                    out.len() < out.capacity(),
                    "timer due output capacity was not prepared"
                );
                out.push(removed.key);
            }
        }
    }

    fn next_deadline_ns(&self) -> Option<u64> {
        self.heap.first().map(|entry| entry.deadline_ns)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::TimerKey;

    use super::{MinHeap, TimerQueue};

    fn insert(queue: &mut MinHeap, key: TimerKey, deadline_ns: u64) {
        queue.reserve_for_insert();
        queue.insert(key, deadline_ns);
    }

    #[test]
    fn heap_orders_by_deadline_then_key() {
        let mut queue = MinHeap::new();
        insert(&mut queue, TimerKey::new(1), 20);
        insert(&mut queue, TimerKey::new(2), 10);
        insert(&mut queue, TimerKey::new(3), 10);
        let mut out = Vec::with_capacity(queue.due_count(10));

        queue.drain_due(10, &mut out);

        assert_eq!(out, vec![TimerKey::new(2), TimerKey::new(3)]);
        assert_eq!(queue.next_deadline_ns(), Some(20));
    }

    #[test]
    fn rearm_moves_entry_and_increments_generation() {
        let mut queue = MinHeap::new();
        let key = TimerKey::new(1);
        insert(&mut queue, key, 10);
        assert!(queue.rearm(key, 30));
        let mut out = Vec::new();

        queue.drain_due(10, &mut out);

        assert!(out.is_empty());
        assert_eq!(queue.next_deadline_ns(), Some(30));
    }

    #[test]
    fn interior_removal_repairs_moved_slot_indexes() {
        let mut queue = MinHeap::new();
        let keys = [
            TimerKey::new(1),
            TimerKey::new(2),
            TimerKey::new(3),
            TimerKey::new(4),
            TimerKey::new(5),
        ];
        for (offset, key) in keys.into_iter().enumerate() {
            insert(&mut queue, key, ((offset + 1) as u64) * 10);
        }
        let mut out = Vec::new();

        assert!(queue.remove(keys[1]));
        assert!(queue.rearm(keys[4], 15));
        out.reserve(queue.due_count(15));
        queue.drain_due(15, &mut out);
        assert_eq!(out, vec![keys[0], keys[4]]);

        assert!(queue.rearm(keys[3], 25));
        out.reserve(queue.due_count(30));
        queue.drain_due(30, &mut out);
        assert_eq!(out, vec![keys[0], keys[4], keys[3], keys[2]]);
        assert_eq!(queue.next_deadline_ns(), None);
    }
}
