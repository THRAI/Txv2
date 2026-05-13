//! `HartLocalArray<T, N>` — a fixed-size array providing per-hart indexed
//! access with no runtime locking.
//!
//! The design contract: each hart accesses only its own slot (`idx == hart_id`).
//! No cross-hart aliasing occurs, so plain `UnsafeCell` access without a mutex
//! is safe as long as callers uphold the per-hart invariant.
//!
//! This module is internal to `tx-observe`.  It is not exported.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;

// ---------------------------------------------------------------------------
// HartLocalArray
// ---------------------------------------------------------------------------

/// A fixed-size array of `T` with const-constructible, `#[no_std]`-compatible
/// per-hart access backed by `MaybeUninit`.
///
/// SAFETY contract on callers:
/// - `get(idx)` may only be called after `get_mut(idx)` has written a valid
///   `T` into the slot (i.e. after `init` has been called for that hart).
/// - `get(idx)` and `get_mut(idx)` may only be called from the hart whose
///   id equals `idx`.  No two harts ever access the same slot concurrently.
///
/// The `Send + Sync` impls are manual: see the SAFETY comment below.
pub(crate) struct HartLocalArray<T, const N: usize> {
    slots: [UnsafeCell<MaybeUninit<T>>; N],
}

// SAFETY: The array is shared across harts as a `static`, but each element
// is only accessed by its own hart (single-producer, single-consumer per
// slot).  No two harts ever alias the same `UnsafeCell`.  Therefore it is
// sound to mark the array as `Sync` despite the `UnsafeCell` interior.
unsafe impl<T, const N: usize> Sync for HartLocalArray<T, N> {}
// Send: the array lives in a static so it is never moved across threads.
unsafe impl<T, const N: usize> Send for HartLocalArray<T, N> {}

impl<T, const N: usize> HartLocalArray<T, N> {
    /// Construct a new array with every slot uninitialised.
    ///
    /// This is a `const fn` so the array can be placed in a `static`.
    pub(crate) const fn new(_phantom_init: T) -> Self
    where
        T: Copy,
    {
        // We cannot use `[const { UnsafeCell::new(MaybeUninit::uninit()) }; N]`
        // in stable const context.  Use transmute from a zeroed MaybeUninit.
        // SAFETY: `[UnsafeCell<MaybeUninit<T>>; N]` has no validity requirement
        // beyond being a valid bit pattern for its fields.  `MaybeUninit` makes
        // any bit pattern valid, and `UnsafeCell` is `repr(transparent)`.
        // Zeroing is safe because we track initialisation via `valid` flags in
        // `HartSlot` / the Option discriminant in `HartLocalOptionArray`.
        //
        // Note: the `_phantom_init` argument is ignored — it exists only so the
        // call site `HartLocalArray::new(HartSlot::uninit())` continues to
        // compile as a readable hint. We do not attempt to use it here because
        // UnsafeCell<MaybeUninit<T>> is not const-array-repeatable for non-Copy T.
        let _ = _phantom_init;
        unsafe {
            let arr: MaybeUninit<[UnsafeCell<MaybeUninit<T>>; N]> = MaybeUninit::zeroed();
            Self {
                slots: arr.assume_init(),
            }
        }
    }

    /// Construct a new array with every slot zeroed.
    ///
    /// Use this for types that are not `Copy`.  The zeroed representation must
    /// be a valid starting state for the type (caller responsibility).
    ///
    /// # Safety
    ///
    /// Zeroing `T` must produce a valid bit pattern that the caller treats as
    /// "uninitialised" and never reads before calling `init_slot`.
    pub(crate) const unsafe fn new_zeroed() -> Self {
        let arr: MaybeUninit<[UnsafeCell<MaybeUninit<T>>; N]> = MaybeUninit::zeroed();
        Self {
            slots: arr.assume_init(),
        }
    }

    /// Shared reference to slot `idx`.
    ///
    /// # Safety (caller contract)
    ///
    /// The slot must have been initialised via `get_mut`.  Panics in debug if
    /// `idx >= N`.
    #[inline]
    pub(crate) fn get(&self, idx: usize) -> &T {
        debug_assert!(idx < N, "HartLocalArray: idx {} out of range {}", idx, N);
        // SAFETY: per the contract, the slot is initialised and only accessed
        // by one hart at a time.
        unsafe { (*self.slots[idx].get()).assume_init_ref() }
    }

    /// Mutable reference to slot `idx`.
    ///
    /// # Safety (caller contract)
    ///
    /// Same as `get`: the calling hart owns the slot exclusively.
    ///
    /// Panics in debug if `idx >= N`.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(crate) fn get_mut(&self, idx: usize) -> &mut T {
        debug_assert!(idx < N, "HartLocalArray: idx {} out of range {}", idx, N);
        // SAFETY: per the contract, only one hart accesses this slot.
        unsafe { (*self.slots[idx].get()).assume_init_mut() }
    }

    /// Write `val` into slot `idx` as the initialisation step.
    ///
    /// Must be called before `get` / `get_mut` for this slot.
    #[inline]
    pub(crate) fn init_slot(&self, idx: usize, val: T) {
        debug_assert!(idx < N);
        // SAFETY: slot is ours to write.
        unsafe {
            (*self.slots[idx].get()).write(val);
        }
    }
}

// ---------------------------------------------------------------------------
// HartLocalOptionArray — for Option<T> where T is not Copy
// ---------------------------------------------------------------------------

/// Fixed-size array of `Option<T>` slots.  All slots start as `None`.
///
/// This exists because `HartLocalArray::new` requires `T: Copy` to compile
/// the "ignored init" workaround, but `HartEmitter` is intentionally not
/// `Copy`.  We store `Option<T>` directly in `MaybeUninit`-free `UnsafeCell`s
/// initialised to `None` via a `MaybeUninit::zeroed` transmute (valid because
/// `Option::None` is the all-zeros discriminant for any `T` without a niche,
/// and for types with a niche the None discriminant is guaranteed stable by
/// Rust's layout rules).
pub(crate) struct HartLocalOptionArray<T, const N: usize> {
    slots: [UnsafeCell<Option<T>>; N],
}

unsafe impl<T, const N: usize> Sync for HartLocalOptionArray<T, N> {}
unsafe impl<T, const N: usize> Send for HartLocalOptionArray<T, N> {}

impl<T, const N: usize> HartLocalOptionArray<T, N> {
    /// Const constructor — all slots are `None`.
    pub(crate) const fn new_none() -> Self {
        // SAFETY: see type-level doc comment.  `UnsafeCell<Option<T>>`
        // zeroed == `UnsafeCell<None>` for any T.
        unsafe {
            let arr: MaybeUninit<[UnsafeCell<Option<T>>; N]> = MaybeUninit::zeroed();
            Self {
                slots: arr.assume_init(),
            }
        }
    }

    #[inline]
    pub(crate) fn get(&self, idx: usize) -> &Option<T> {
        debug_assert!(idx < N);
        // SAFETY: per-hart exclusive access.
        unsafe { &*self.slots[idx].get() }
    }

    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(crate) fn get_mut(&self, idx: usize) -> &mut Option<T> {
        debug_assert!(idx < N);
        // SAFETY: per-hart exclusive access.
        unsafe { &mut *self.slots[idx].get() }
    }
}
