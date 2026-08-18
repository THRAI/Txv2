//! Packed slot metadata.
//!
//! State, retain count, and generation live in one atomic word so upgrade,
//! clone, drop, and reclaim paths can validate the full slot state with a
//! single compare-exchange.

use core::sync::atomic::{AtomicU64, Ordering};

use super::ZoneError;

const STATE_MASK: u64 = 0x7;
const HAS_NEXT_BIT: u64 = 1 << 3;
const GENERATION_EXHAUSTED_BIT: u64 = 1 << 4;
/// Internal one-shot ownership bit for the EBR reclaim callback.
///
/// This deliberately does not add another public lifecycle state: observers
/// must continue to treat the slot as Retiring until its destructor has run
/// and the generation is advanced. Only the callback which atomically sets
/// this bit may touch the payload or return the slot to the allocator.
const RECLAIM_CLAIMED_BIT: u64 = 1 << 5;
const RETAIN_SHIFT: u64 = 16;
const RETAIN_MASK: u64 = 0xffff_ffff;
const GENERATION_SHIFT: u64 = 48;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u64)]
pub enum SlotState {
    /// Slot contains no initialized `T` and is available to the allocator.
    Free = 0,
    /// Storage is reserved by a `ZoneReservation<T>` but not visible yet.
    Reserved = 1,
    /// Slot contains a published object and may be retained by `Cap<T>`.
    Live = 2,
    /// Slot has been queued to EBR and awaits the reclaim callback.
    Retiring = 3,
}

impl SlotState {
    fn from_bits(bits: u64) -> Option<Self> {
        match bits {
            0 => Some(Self::Free),
            1 => Some(Self::Reserved),
            2 => Some(Self::Live),
            3 => Some(Self::Retiring),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlotWord(u64);

impl SlotWord {
    /// Layout: state [2:0], flags [4:3], payload [47:16], generation [63:48].
    /// Payload is retain while Live and the raw next SlotKey while Retiring.
    pub const fn new(generation: u16, retain: u32, state: SlotState) -> Self {
        Self(
            ((generation as u64) << GENERATION_SHIFT)
                | ((retain as u64) << RETAIN_SHIFT)
                | state as u64,
        )
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub fn generation(self) -> u16 {
        (self.0 >> GENERATION_SHIFT) as u16
    }

    pub fn retain(self) -> u32 {
        ((self.0 >> RETAIN_SHIFT) & RETAIN_MASK) as u32
    }

    pub fn has_next(self) -> bool {
        self.0 & HAS_NEXT_BIT != 0
    }

    pub fn generation_exhausted(self) -> bool {
        self.0 & GENERATION_EXHAUSTED_BIT != 0
    }

    pub(crate) fn reclaim_claimed(self) -> bool {
        self.0 & RECLAIM_CLAIMED_BIT != 0
    }

    pub fn retiring_next_raw(self) -> Option<u32> {
        self.has_next().then_some(self.retain())
    }

    pub fn state(self) -> SlotState {
        SlotState::from_bits(self.0 & STATE_MASK).unwrap_or(SlotState::Retiring)
    }

    pub fn with_generation(self, generation: u16) -> Self {
        Self((self.0 & !(0xffff << GENERATION_SHIFT)) | ((generation as u64) << GENERATION_SHIFT))
    }

    pub fn with_retain(self, retain: u32) -> Self {
        Self((self.0 & !(RETAIN_MASK << RETAIN_SHIFT)) | ((retain as u64) << RETAIN_SHIFT))
    }

    pub fn with_state(self, state: SlotState) -> Self {
        Self((self.0 & !STATE_MASK) | state as u64)
    }

    pub fn with_retiring_next(self, next: Option<u32>) -> Self {
        let word = self.with_retain(next.unwrap_or(0));
        if next.is_some() {
            Self(word.0 | HAS_NEXT_BIT)
        } else {
            Self(word.0 & !HAS_NEXT_BIT)
        }
    }

    pub fn clear_retiring_link(self) -> Self {
        Self(self.with_retain(0).0 & !HAS_NEXT_BIT)
    }

    pub(crate) fn with_reclaim_claimed(self) -> Self {
        Self(self.0 | RECLAIM_CLAIMED_BIT)
    }

    fn clear_reclaim_claimed(self) -> Self {
        Self(self.0 & !RECLAIM_CLAIMED_BIT)
    }

    pub fn inc_retain(self) -> Result<Self, ZoneError> {
        let retain = self.retain();
        if retain == u32::MAX {
            return Err(ZoneError::RetainOverflow);
        }
        Ok(self.with_retain(retain + 1))
    }

    pub fn dec_retain(self) -> Result<Self, ZoneError> {
        let retain = self.retain();
        if retain == 0 {
            return Err(ZoneError::InvalidState);
        }
        Ok(self.with_retain(retain - 1))
    }

    pub fn next_free_generation(self) -> Self {
        let cleared = self
            .clear_retiring_link()
            .clear_reclaim_claimed()
            .with_state(SlotState::Free);
        if self.generation() == u16::MAX {
            Self(cleared.0 | GENERATION_EXHAUSTED_BIT)
        } else {
            cleared.with_generation(self.generation() + 1)
        }
    }
}

pub struct SlotMeta {
    /// Packed `SlotWord`.
    word: AtomicU64,
}

impl SlotMeta {
    pub const fn free() -> Self {
        Self {
            word: AtomicU64::new(SlotWord::new(0, 0, SlotState::Free).raw()),
        }
    }

    pub fn load(&self, ordering: Ordering) -> SlotWord {
        SlotWord(self.word.load(ordering))
    }

    /// Atomically acquire the one-shot right to reclaim a Retiring payload.
    ///
    /// Returning `None` is the release-safe outcome for a stale key or a
    /// duplicate callback: the caller must not inspect or destruct the value.
    pub(crate) fn try_claim_reclaim(&self, expected_generation: u16) -> Option<SlotWord> {
        loop {
            let current = self.load(Ordering::Acquire);
            if current.state() != SlotState::Retiring
                || current.generation() != expected_generation
                || current.reclaim_claimed()
            {
                return None;
            }
            let claimed = current.with_reclaim_claimed();
            match self.compare_exchange(current, claimed, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(claimed),
                Err(_) => continue,
            }
        }
    }

    pub fn compare_exchange(
        &self,
        current: SlotWord,
        new: SlotWord,
        success: Ordering,
        failure: Ordering,
    ) -> Result<SlotWord, SlotWord> {
        self.word
            .compare_exchange(current.raw(), new.raw(), success, failure)
            .map(SlotWord)
            .map_err(SlotWord)
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::{SlotMeta, SlotState};

    #[test]
    fn concurrent_reclaim_claim_has_exactly_one_winner() {
        let meta = SlotMeta::free();
        let free = meta.load(Ordering::Acquire);
        meta.compare_exchange(
            free,
            free.with_state(SlotState::Retiring),
            Ordering::Release,
            Ordering::Relaxed,
        )
        .expect("test metadata enters Retiring");

        let ready = std::sync::Barrier::new(3);
        let winners = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    ready.wait();
                    if meta.try_claim_reclaim(free.generation()).is_some() {
                        winners.fetch_add(1, Ordering::AcqRel);
                    }
                });
            }
            ready.wait();
        });

        assert_eq!(winners.load(Ordering::Acquire), 1);
    }
}
