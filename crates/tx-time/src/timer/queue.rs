use alloc::vec::Vec;

use crate::TimerKey;

/// Private queue contract used by [`super::TimerEngine`].
pub(super) trait TimerQueue {
    fn insert(&mut self, key: TimerKey, deadline_ns: u64);
    fn remove(&mut self, key: TimerKey) -> bool;
    fn rearm(&mut self, key: TimerKey, deadline_ns: u64) -> bool;
    fn drain_due(&mut self, now_ns: u64, out: &mut Vec<TimerKey>);
    fn next_deadline_ns(&self) -> Option<u64>;
}
