//! Per-hart slot primitive.
//!
//! [`HartLocal<T>`] provides a `[OnceCell<T>; MAX_HARTS]`-style store with a
//! typed accessor that resolves the current hart via [`PercpuIf`].
//!
//! Designed to be placed in a `static` so that each logical subsystem that
//! needs per-hart state gets one named, typed slot rather than rolling its own
//! `[AtomicBool; N] + [UnsafeCell<MaybeUninit<T>>; N]`.
//!
//! # Usage
//!
//! ```rust,ignore
//! static MY_STATE: HartLocal<MyThing> = HartLocal::new();
//!
//! // During hart boot (called once per hart):
//! MY_STATE.init(cpu_id, MyThing::new());
//!
//! // In hot paths on any hart:
//! if let Some(s) = MY_STATE.get::<P>() { … }
//! ```
//!
//! # Capacity
//!
//! Limited to [`MAX_HARTS`] harts.  This constant matches the bit-width of
//! [`CpuMask`] (a `u64`), so the slot array is 64 entries.  A `CpuId` whose
//! `.0` is ≥ `MAX_HARTS` is out of range; `init` panics and `get` returns
//! `None`.
//!
//! # Sync safety
//!
//! Each slot uses an `AtomicBool` written-flag plus an `UnsafeCell<MaybeUninit<T>>`.
//! `init` stores a value then raises the flag with `Release` ordering; `get`
//! reads the flag with `Acquire` ordering before touching the cell.  No slot is
//! ever mutated after the flag is raised, so concurrent `get` calls are safe.
//!
//! `HartLocal<T>` is `Sync` when `T: Send + Sync` — same rule as
//! `std::sync::OnceLock<T>`.

use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{CpuId, PercpuIf};

/// Maximum number of harts supported by [`HartLocal<T>`].
///
/// Matches the bit-width of [`CpuMask`](crate::CpuMask), which is backed by a
/// `u64`.
pub const MAX_HARTS: usize = 64;

// ---------------------------------------------------------------------------
// Internal sync once-cell — one per slot
// ---------------------------------------------------------------------------

struct Slot<T> {
    ready: AtomicBool,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: The `ready` flag guards all access to `value`.  Once `ready` is
// `true` (with Release/Acquire ordering), `value` is never mutated again.
// Requiring `T: Send` is sufficient for cross-hart `init`; the `get` borrow
// only ever hands out a shared `&T`.  `T: Sync` ensures concurrent `get`s
// are sound.
unsafe impl<T: Send + Sync> Sync for Slot<T> {}

impl<T> Slot<T> {
    const fn new() -> Self {
        Self {
            ready: AtomicBool::new(false),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Write `value` and raise the ready flag.  Panics if already initialised.
    fn init(&self, value: T) {
        // Guard: reject double-init without yet claiming the slot.
        // A Relaxed load is sufficient — we just need to detect the common
        // case before doing the unsafe write.
        if self.ready.load(Ordering::Relaxed) {
            panic!("HartLocal slot already initialised");
        }

        // SAFETY: `init` is intended to be called exactly once per hart
        // during boot, before any concurrent `get` on this hart.  We write
        // the value before publishing `ready`, so no reader can observe the
        // uninitialised cell through `get`.
        unsafe {
            (*self.value.get()).write(value);
        }

        // Publish with Release ordering so that the `value` write
        // (above) is visible to any thread that subsequently loads
        // `ready` with Acquire ordering (see `get`).
        //
        // If `ready` was already `true` (unlikely race — two callers
        // simultaneously trying to init the same hart's slot), the store
        // will silently overwrite it.  The design contract is that `init`
        // is called once per hart during single-threaded boot; the Relaxed
        // load above catches the ordered sequential case.
        self.ready.store(true, Ordering::Release);
    }

    /// Return `Some(&T)` if the slot has been initialised.
    fn get(&self) -> Option<&T> {
        if self.ready.load(Ordering::Acquire) {
            // SAFETY: `ready` was `true` with Acquire ordering, so the
            // Release store in `init` happened-before this load.  The value
            // is fully initialised and will never be mutated again.
            Some(unsafe { (*self.value.get()).assume_init_ref() })
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// HartLocal<T>
// ---------------------------------------------------------------------------

/// Per-hart slot backed by `[Slot<T>; MAX_HARTS]`.
///
/// See [module documentation](self) for usage and safety notes.
pub struct HartLocal<T> {
    slots: [Slot<T>; MAX_HARTS],
}

// SAFETY: forwarded from `Slot<T>` — requires `T: Send + Sync`.
unsafe impl<T: Send + Sync> Sync for HartLocal<T> {}

// `HartLocal<T>` itself does not need to be `Send` for most uses (it lives in
// a `static`), but we implement it for completeness when `T: Send + Sync`.
unsafe impl<T: Send + Sync> Send for HartLocal<T> {}

impl<T> HartLocal<T> {
    // Slot::new() is const, and arrays of const-initialised items are const.
    // The `[expr; N]` syntax requires `Copy` for non-const exprs, but
    // `const { Slot::new() }` (a const block) is fine for any `T`.
    //
    // We use a const helper to avoid requiring `T: Copy`.
    const NEW_SLOT: Slot<T> = Slot::new();

    /// Create an empty `HartLocal<T>`.  Every slot starts uninitialised.
    ///
    /// Suitable for use in `static` initialisers.
    pub const fn new() -> Self {
        Self {
            slots: [Self::NEW_SLOT; MAX_HARTS],
        }
    }

    /// Initialise the slot for `hart`.
    ///
    /// # Panics
    ///
    /// Panics if `hart.0 >= MAX_HARTS` or if this slot has already been
    /// initialised on `hart`.  Intended to be called exactly once per hart
    /// during boot.
    pub fn init(&self, hart: CpuId, value: T) {
        assert!(
            hart.0 < MAX_HARTS,
            "HartLocal::init: CpuId({}) >= MAX_HARTS ({})",
            hart.0,
            MAX_HARTS
        );
        self.slots[hart.0].init(value);
    }

    /// Return a shared reference to the value for the current hart, or `None`
    /// if the slot has not yet been initialised.
    ///
    /// Uses `P::current_cpu_id()` to resolve the calling hart.  Returns `None`
    /// if the resolved id is out of range or the slot is uninitialised.
    pub fn get<P: PercpuIf>(&self) -> Option<&T> {
        let id = P::current_cpu_id();
        if id.0 >= MAX_HARTS {
            return None;
        }
        self.slots[id.0].get()
    }
}

impl<T> Default for HartLocal<T> {
    fn default() -> Self {
        Self::new()
    }
}
