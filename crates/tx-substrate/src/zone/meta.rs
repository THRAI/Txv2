//! Packed slot metadata.
//!
//! State, retain count, and generation live in one atomic word so upgrade,
//! clone, drop, and reclaim paths can validate the full slot state with a
//! single compare-exchange.

use core::sync::atomic::{AtomicU64, Ordering};

use super::ZoneError;

const STATE_MASK: u64 = 0x7;
const RETAIN_SHIFT: u64 = 16;
const RETAIN_MASK: u64 = 0xffff_ffff;
const GENERATION_SHIFT: u64 = 48;
/// Retain count value used as a no-upgrade barrier after the last Cap drops.
pub const RETAIN_SENTINEL_DEAD: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u64)]
pub enum SlotState {
    /// Slot contains no initialized `T` and is available to the allocator.
    Free = 0,
    /// Storage is reserved by a `ZoneReservation<T>` but not visible yet.
    Reserved = 1,
    /// Slot contains a published object and may be retained by `Cap<T>`.
    Live = 2,
    /// Semantic death has happened; upgrades are blocked, but EBR has not run.
    Dead = 3,
    /// Slot has been queued to EBR and awaits the reclaim callback.
    Retiring = 4,
}

impl SlotState {
    fn from_bits(bits: u64) -> Option<Self> {
        match bits {
            0 => Some(Self::Free),
            1 => Some(Self::Reserved),
            2 => Some(Self::Live),
            3 => Some(Self::Dead),
            4 => Some(Self::Retiring),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlotWord(u64);

impl SlotWord {
    /// Layout: bits [2:0] state, [47:16] retain, [63:48] generation.
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

    pub fn inc_retain(self) -> Result<Self, ZoneError> {
        let retain = self.retain();
        if retain == RETAIN_SENTINEL_DEAD {
            return Err(ZoneError::RetainOverflow);
        }
        Ok(self.with_retain(retain + 1))
    }

    pub fn dec_retain(self) -> Result<Self, ZoneError> {
        let retain = self.retain();
        if retain == 0 || retain == RETAIN_SENTINEL_DEAD {
            return Err(ZoneError::InvalidState);
        }
        Ok(self.with_retain(retain - 1))
    }

    pub fn inc_generation(self) -> Self {
        self.with_generation(self.generation().wrapping_add(1))
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
