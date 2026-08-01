use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};

use super::AllocError;

const REFCOUNT_SHIFT: u32 = 0;
const REFCOUNT_BITS: u32 = 10;
const REFCOUNT_MASK: u32 = (1 << REFCOUNT_BITS) - 1;
const REFCOUNT_ONE: u32 = 1 << REFCOUNT_SHIFT;

const MAP_COUNT_SHIFT: u32 = 10;
const MAP_COUNT_BITS: u32 = 10;
const MAP_COUNT_MASK: u32 = (1 << MAP_COUNT_BITS) - 1;
const MAP_COUNT_ONE: u32 = 1 << MAP_COUNT_SHIFT;

const CACHE_REF_SHIFT: u32 = 20;
const CACHE_REF_BITS: u32 = 8;
const CACHE_REF_MASK: u32 = (1 << CACHE_REF_BITS) - 1;
const CACHE_REF_ONE: u32 = 1 << CACHE_REF_SHIFT;

const PIN_COUNT_SHIFT: u32 = 28;
const PIN_COUNT_BITS: u32 = 4;
const PIN_COUNT_MASK: u32 = (1 << PIN_COUNT_BITS) - 1;
const PIN_COUNT_ONE: u32 = 1 << PIN_COUNT_SHIFT;

const FLAG_DIRECT_MAPPED: u16 = 1 << 2;
const FLAG_RESERVED: u16 = 1 << 3;

/// Per-physical-frame metadata.
///
/// `state` is a packed liveness word:
///
/// - bits `0..10`: generic owner/retainer refcount
/// - bits `10..20`: pmap/PTE map count
/// - bits `20..28`: page-cache inclusion count
/// - bits `28..32`: DMA/long-term pin count
///
/// `flags` holds non-counter properties such as `reserved` and
/// `direct_mapped`. Reserved frames cannot return through the normal allocator
/// free path.
#[repr(C, align(8))]
pub struct FrameMeta {
    state: AtomicU32,
    flags: AtomicU16,
}

impl FrameMeta {
    /// Construct an all-zero metadata row.
    ///
    /// All-zero metadata means "no semantic holder." The row becomes globally
    /// allocatable only when the backend also sets the corresponding bitmap bit.
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
            flags: AtomicU16::new(0),
        }
    }

    /// True when this physical frame is excluded from the normal free pool.
    pub fn is_reserved(&self) -> bool {
        self.flags.load(Ordering::Acquire) & FLAG_RESERVED != 0
    }

    /// Mark the frame as reserved against normal allocator return.
    pub fn mark_reserved(&self) {
        self.flags.fetch_or(FLAG_RESERVED, Ordering::AcqRel);
    }

    /// Clear the reserved flag before an explicit owner-specific teardown.
    pub fn clear_reserved(&self) {
        self.flags.fetch_and(!FLAG_RESERVED, Ordering::AcqRel);
    }

    /// Mark the frame as covered by the kernel direct map.
    pub fn mark_direct_mapped(&self) {
        self.flags.fetch_or(FLAG_DIRECT_MAPPED, Ordering::AcqRel);
    }

    /// True when the frame is expected to be accessible through the direct map.
    pub fn is_direct_mapped(&self) -> bool {
        self.flags.load(Ordering::Acquire) & FLAG_DIRECT_MAPPED != 0
    }

    /// Clear the direct-map bookkeeping flag.
    pub fn clear_direct_mapped(&self) {
        self.flags.fetch_and(!FLAG_DIRECT_MAPPED, Ordering::AcqRel);
    }

    pub(crate) fn state(&self) -> u32 {
        self.state.load(Ordering::Acquire)
    }

    pub(crate) fn refcount(&self) -> u32 {
        (self.state() >> REFCOUNT_SHIFT) & REFCOUNT_MASK
    }

    pub(crate) fn map_count(&self) -> u32 {
        (self.state() >> MAP_COUNT_SHIFT) & MAP_COUNT_MASK
    }

    pub(crate) fn cache_ref(&self) -> u32 {
        (self.state() >> CACHE_REF_SHIFT) & CACHE_REF_MASK
    }

    pub(crate) fn pin_count(&self) -> u32 {
        (self.state() >> PIN_COUNT_SHIFT) & PIN_COUNT_MASK
    }

    pub(crate) fn claim_owned(&self) -> Result<(), AllocError> {
        if self.is_reserved() {
            return Err(AllocError::ReservedFrame);
        }

        self.state
            .compare_exchange(0, REFCOUNT_ONE, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| AllocError::InvalidRequest)
    }

    pub(crate) fn claim_permanent(&self) -> Result<(), AllocError> {
        self.state
            .compare_exchange(0, REFCOUNT_ONE, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| AllocError::InvalidRequest)
    }

    pub(crate) fn force_free_state(&self) {
        self.state.store(0, Ordering::Release);
    }

    fn increment_counter(
        &self,
        shift: u32,
        mask: u32,
        one: u32,
        require_live: bool,
    ) -> Result<(), AllocError> {
        loop {
            let old = self.state.load(Ordering::Acquire);
            // Role counters may only be acquired from a live frame. This is
            // the frame-level sentinel check used by map/cache/DMA upgrades.
            if require_live && old == 0 {
                return Err(AllocError::InvalidRequest);
            }

            if ((old >> shift) & mask) == mask {
                return Err(AllocError::CounterOverflow);
            }

            let new = old + one;
            match self
                .state
                .compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(_) => continue,
            }
        }
    }

    fn decrement_counter(&self, shift: u32, mask: u32, one: u32) -> Result<u32, AllocError> {
        loop {
            let old = self.state.load(Ordering::Acquire);
            if ((old >> shift) & mask) == 0 {
                return Err(AllocError::CounterUnderflow);
            }

            let new = old - one;
            match self
                .state
                .compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(new),
                Err(_) => continue,
            }
        }
    }

    pub(crate) fn decrement_refcount(&self) -> Result<u32, AllocError> {
        self.decrement_counter(REFCOUNT_SHIFT, REFCOUNT_MASK, REFCOUNT_ONE)
    }

    pub(crate) fn increment_refcount(&self) -> Result<(), AllocError> {
        self.increment_counter(REFCOUNT_SHIFT, REFCOUNT_MASK, REFCOUNT_ONE, true)
    }

    pub(crate) fn increment_map_count(&self) -> Result<(), AllocError> {
        self.increment_counter(MAP_COUNT_SHIFT, MAP_COUNT_MASK, MAP_COUNT_ONE, true)
    }

    pub(crate) fn decrement_map_count(&self) -> Result<u32, AllocError> {
        self.decrement_counter(MAP_COUNT_SHIFT, MAP_COUNT_MASK, MAP_COUNT_ONE)
    }

    pub(crate) fn increment_cache_ref(&self) -> Result<(), AllocError> {
        self.increment_counter(CACHE_REF_SHIFT, CACHE_REF_MASK, CACHE_REF_ONE, true)
    }

    pub(crate) fn decrement_cache_ref(&self) -> Result<u32, AllocError> {
        self.decrement_counter(CACHE_REF_SHIFT, CACHE_REF_MASK, CACHE_REF_ONE)
    }

    pub(crate) fn increment_pin_count(&self) -> Result<(), AllocError> {
        self.increment_counter(PIN_COUNT_SHIFT, PIN_COUNT_MASK, PIN_COUNT_ONE, true)
    }

    pub(crate) fn decrement_pin_count(&self) -> Result<u32, AllocError> {
        self.decrement_counter(PIN_COUNT_SHIFT, PIN_COUNT_MASK, PIN_COUNT_ONE)
    }

    #[doc(hidden)]
    pub fn state_for_test(&self) -> u32 {
        self.state()
    }

    #[doc(hidden)]
    pub fn refcount_for_test(&self) -> u32 {
        self.refcount()
    }

    #[doc(hidden)]
    pub fn map_count_for_test(&self) -> u32 {
        self.map_count()
    }

    #[doc(hidden)]
    pub fn cache_ref_for_test(&self) -> u32 {
        self.cache_ref()
    }

    #[doc(hidden)]
    pub fn pin_count_for_test(&self) -> u32 {
        self.pin_count()
    }
}

impl Default for FrameMeta {
    fn default() -> Self {
        Self::new()
    }
}

const _: () = assert!(core::mem::size_of::<FrameMeta>() == 8);
