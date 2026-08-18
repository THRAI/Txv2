//! Private three-bag retirement storage.

use crate::zone::RetiredSlot;

use super::domain::EpochError;
use super::RcuHead;

pub(crate) const EPOCH_BAG_COUNT: usize = 3;

pub(crate) struct EpochBag {
    pub(crate) epoch: u64,
    pub(crate) zone_head: Option<RetiredSlot>,
    pub(crate) zone_count: usize,
    pub(crate) rcu_head: *mut RcuHead,
    pub(crate) rcu_count: usize,
}

impl EpochBag {
    const fn new() -> Self {
        Self {
            epoch: 0,
            zone_head: None,
            zone_count: 0,
            rcu_head: core::ptr::null_mut(),
            rcu_count: 0,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.zone_head.is_none() && self.rcu_head.is_null()
    }

    pub(crate) fn reset_if_empty(&mut self, epoch: u64) -> bool {
        if self.epoch == epoch {
            return true;
        }
        if !self.is_empty() {
            return false;
        }
        self.epoch = epoch;
        true
    }
}

pub(crate) struct LocalRetireState {
    pub(crate) bags: [EpochBag; EPOCH_BAG_COUNT],
}

impl LocalRetireState {
    pub(crate) const fn new() -> Self {
        Self {
            bags: [const { EpochBag::new() }; EPOCH_BAG_COUNT],
        }
    }

    pub(crate) fn reset(&mut self) {
        self.bags = [const { EpochBag::new() }; EPOCH_BAG_COUNT];
    }

    pub(crate) fn bag_for_epoch_mut(&mut self, epoch: u64) -> Option<&mut EpochBag> {
        let bag = &mut self.bags[epoch as usize % EPOCH_BAG_COUNT];
        bag.reset_if_empty(epoch).then_some(bag)
    }

    pub(crate) unsafe fn try_enqueue_rcu(
        &mut self,
        epoch: u64,
        mut head: core::ptr::NonNull<RcuHead>,
    ) -> Result<(), EpochError> {
        if unsafe { head.as_ref().is_queued() } {
            return Err(EpochError::RetireBagOccupied);
        }
        let bag = self
            .bag_for_epoch_mut(epoch)
            .ok_or(EpochError::RetireBagOccupied)?;
        let previous_head = bag.rcu_head;
        unsafe {
            // A singleton points to itself, so every queued node has a non-null
            // link and duplicate enqueue remains detectable.
            head.as_mut().next = if previous_head.is_null() {
                head.as_ptr()
            } else {
                previous_head
            };
        }
        bag.rcu_head = head.as_ptr();
        bag.rcu_count += 1;
        Ok(())
    }

    pub(crate) fn retired_count(&self) -> usize {
        self.bags
            .iter()
            .map(|bag| bag.zone_count + bag.rcu_count)
            .sum()
    }

    pub(crate) fn can_merge_from(&self, source: &Self) -> bool {
        self.bags
            .iter()
            .zip(source.bags.iter())
            .all(|(target, source)| {
                source.is_empty() || target.is_empty() || target.epoch == source.epoch
            })
    }

    /// Merge an offlined CPU's bags into the coordinator's same epoch slots.
    /// Both states must be exclusively owned by the caller.
    pub(crate) unsafe fn merge_from(&mut self, source: &mut Self) -> Result<(), EpochError> {
        if !self.can_merge_from(source) {
            return Err(EpochError::RetireBagOccupied);
        }

        for bag_index in 0..EPOCH_BAG_COUNT {
            if source.bags[bag_index].is_empty() {
                continue;
            }
            let source_epoch = source.bags[bag_index].epoch;
            assert!(self.bags[bag_index].reset_if_empty(source_epoch));

            if let Some(source_head) = source.bags[bag_index].zone_head {
                if let Some(target_head) = self.bags[bag_index].zone_head {
                    let mut tail = source_head;
                    while let Some(next) = crate::zone::retiring_next(tail) {
                        tail = next;
                    }
                    crate::zone::set_retiring_next(tail, Some(target_head));
                }
                self.bags[bag_index].zone_head = Some(source_head);
                self.bags[bag_index].zone_count += source.bags[bag_index].zone_count;
                source.bags[bag_index].zone_head = None;
                source.bags[bag_index].zone_count = 0;
            }

            if !source.bags[bag_index].rcu_head.is_null() {
                let source_head = source.bags[bag_index].rcu_head;
                let target_head = self.bags[bag_index].rcu_head;
                if !target_head.is_null() {
                    let mut tail = source_head;
                    loop {
                        let next = unsafe { (*tail).next };
                        if next == tail {
                            break;
                        }
                        tail = next;
                    }
                    unsafe {
                        (*tail).next = target_head;
                    }
                }
                self.bags[bag_index].rcu_head = source_head;
                self.bags[bag_index].rcu_count += source.bags[bag_index].rcu_count;
                source.bags[bag_index].rcu_head = core::ptr::null_mut();
                source.bags[bag_index].rcu_count = 0;
            }

            source.bags[bag_index].epoch = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::NonNull;

    use super::*;

    unsafe fn noop_rcu_reclaim(_head: *mut RcuHead, _guard: &mut super::super::LocalRetireGuard) {}

    #[test]
    fn offline_merge_is_failure_atomic_on_epoch_collision() {
        let mut target = LocalRetireState::new();
        let mut source = LocalRetireState::new();
        target.reset();
        source.reset();
        let mut target_node = RcuHead::new(noop_rcu_reclaim);
        let mut source_node = RcuHead::new(noop_rcu_reclaim);
        unsafe {
            target
                .try_enqueue_rcu(4, NonNull::from(&mut target_node))
                .expect("target RCU enqueue");
            source
                .try_enqueue_rcu(1, NonNull::from(&mut source_node))
                .expect("source RCU enqueue");
        }

        let result = unsafe { target.merge_from(&mut source) };

        assert_eq!(result, Err(EpochError::RetireBagOccupied));
        assert_eq!(target.retired_count(), 1);
        assert_eq!(source.retired_count(), 1);
        assert!(target_node.is_queued());
        assert!(source_node.is_queued());
    }
}
