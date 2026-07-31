//! Bounded key/value index with linear reservations.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::sync::atomic::{AtomicBool, Ordering};

use alloc::vec::Vec;

use crate::epoch::Guard;

// ---------------------------------------------------------------------------
// L6 mutation emit gate (OBS-8)
// ---------------------------------------------------------------------------

/// Runtime gate for L6 `MutationIndexCommit` observation events.
///
/// Defaults to **off** so the change is observable on demand without
/// perturbing existing benchmarks.  Flip to `true` at boot to enable.
pub static INDEX_MUTATION_EMIT_ENABLED: AtomicBool = AtomicBool::new(false);

const EMPTY: u8 = 0;
const RESERVED_EMPTY: u8 = 1;
const COMMITTED: u8 = 2;
const RESERVED_COMMITTED: u8 = 3;

/// Index operation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexError {
    /// No free entry exists in this bounded index.
    Full,
    /// The key already has a committed or reserved entry.
    Duplicate,
    /// No committed entry exists for the key.
    Missing,
    /// The key is currently reserved by another operation.
    Busy,
}

struct Entry<K, V> {
    state: UnsafeCell<u8>,
    key: UnsafeCell<MaybeUninit<K>>,
    value: UnsafeCell<MaybeUninit<V>>,
}

impl<K, V> Entry<K, V> {
    const fn new() -> Self {
        Self {
            state: UnsafeCell::new(EMPTY),
            key: UnsafeCell::new(MaybeUninit::uninit()),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

struct SpinLock {
    held: AtomicBool,
}

impl SpinLock {
    const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> SpinGuard<'_> {
        while self
            .held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self }
    }
}

struct SpinGuard<'a> {
    lock: &'a SpinLock,
}

impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}

/// Fixed-capacity index whose writes are mediated by reservations.
pub struct Index<K, V, const N: usize> {
    lock: SpinLock,
    entries: [Entry<K, V>; N],
}

unsafe impl<K: Send, V: Send, const N: usize> Sync for Index<K, V, N> {}

impl<K, V, const N: usize> Index<K, V, N> {
    /// Construct an empty index.
    pub const fn new() -> Self {
        Self {
            lock: SpinLock::new(),
            entries: [const { Entry::new() }; N],
        }
    }
}

impl<K: Eq, V, const N: usize> Index<K, V, N> {
    /// Reserve a key that is not currently committed or reserved.
    pub fn reserve(&self, key: K) -> Result<IndexReservation<'_, K, V, N>, IndexError> {
        let _guard = self.lock.lock();

        for entry in &self.entries {
            let state = unsafe { *entry.state.get() };
            if state != EMPTY && unsafe { (*entry.key.get()).assume_init_ref() == &key } {
                return Err(IndexError::Duplicate);
            }
        }

        let Some((slot_index, entry)) = self
            .entries
            .iter()
            .enumerate()
            .find(|(_, entry)| unsafe { *entry.state.get() == EMPTY })
        else {
            return Err(IndexError::Full);
        };

        unsafe {
            (*entry.key.get()).write(key);
            *entry.state.get() = RESERVED_EMPTY;
        }

        Ok(IndexReservation {
            index: self,
            slot_index,
            committed: false,
        })
    }

    /// Observe a committed value while holding the index read critical
    /// section.
    ///
    /// `Index` stores keys and values inline and `withdraw()` may immediately
    /// move them out and reuse the slot.  Consequently an epoch guard alone
    /// cannot protect a borrowed slot: the slot storage itself is not retired
    /// through EBR.  Keep the index lock in `IndexRef` so the key/value remain
    /// initialized until the caller has cloned or copied what it needs.
    pub fn lookup<'i>(&'i self, key: &K, _guard: &Guard<'_>) -> Option<IndexRef<'i, K, V>> {
        let lock = self.lock.lock();

        for entry in &self.entries {
            let state = unsafe { *entry.state.get() };
            if state == COMMITTED && unsafe { (*entry.key.get()).assume_init_ref() == key } {
                let key = unsafe { &*(*entry.key.get()).as_ptr() };
                let value = unsafe { &*(*entry.value.get()).as_ptr() };
                return Some(IndexRef {
                    key,
                    value,
                    _lock: lock,
                });
            }
        }

        None
    }

    /// Copy a stable projection of a committed value while holding the index
    /// lock, then release the lock before returning it.
    ///
    /// This is the preferred lookup form for values that expose a compact
    /// identity token (for example a zone `Cap` raw key).  The caller can
    /// upgrade that token under its epoch guard after this function returns,
    /// avoiding both a borrowed inline slot and lock-order coupling between
    /// the index and the value's own lifetime machinery.
    pub fn lookup_project<R>(&self, key: &K, project: impl FnOnce(&V) -> R) -> Option<R> {
        let _lock = self.lock.lock();

        for entry in &self.entries {
            let state = unsafe { *entry.state.get() };
            if state == COMMITTED && unsafe { (*entry.key.get()).assume_init_ref() == key } {
                let value = unsafe { (*entry.value.get()).assume_init_ref() };
                return Some(project(value));
            }
        }

        None
    }

    pub(crate) fn reserve_committed(
        &self,
        key: &K,
    ) -> Result<CommittedReservation<'_, K, V, N>, IndexError> {
        let _guard = self.lock.lock();

        for (slot_index, entry) in self.entries.iter().enumerate() {
            let state = unsafe { *entry.state.get() };
            if state != EMPTY && unsafe { (*entry.key.get()).assume_init_ref() == key } {
                return match state {
                    COMMITTED => {
                        unsafe {
                            *entry.state.get() = RESERVED_COMMITTED;
                        }
                        Ok(CommittedReservation {
                            index: self,
                            slot_index,
                            committed: false,
                        })
                    }
                    RESERVED_EMPTY | RESERVED_COMMITTED => Err(IndexError::Busy),
                    _ => Err(IndexError::Missing),
                };
            }
        }

        Err(IndexError::Missing)
    }
}

impl<K, V, const N: usize> Index<K, V, N> {
    /// Snapshot committed values through a caller-supplied projection.
    pub fn snapshot_values_filter_map<R>(
        &self,
        _guard: &Guard<'_>,
        mut f: impl FnMut(&V) -> Option<R>,
    ) -> Vec<R> {
        let _lock = self.lock.lock();
        let mut values = Vec::new();

        for entry in &self.entries {
            let state = unsafe { *entry.state.get() };
            if state == COMMITTED {
                let value = unsafe { (*entry.value.get()).assume_init_ref() };
                if let Some(mapped) = f(value) {
                    values.push(mapped);
                }
            }
        }

        values
    }
}

impl<K, V, const N: usize> Default for Index<K, V, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V, const N: usize> Drop for Index<K, V, N> {
    fn drop(&mut self) {
        for entry in &mut self.entries {
            let state = unsafe { *entry.state.get() };
            match state {
                RESERVED_EMPTY => unsafe {
                    (*entry.key.get()).assume_init_drop();
                },
                COMMITTED | RESERVED_COMMITTED => unsafe {
                    (*entry.key.get()).assume_init_drop();
                    (*entry.value.get()).assume_init_drop();
                },
                _ => {}
            }
        }
    }
}

/// Reservation for installing an absent key. Drop rolls back the key.
pub struct IndexReservation<'i, K, V, const N: usize> {
    index: &'i Index<K, V, N>,
    slot_index: usize,
    committed: bool,
}

impl<K, V, const N: usize> IndexReservation<'_, K, V, N> {
    /// Reserved key.
    pub fn key(&self) -> &K {
        let entry = &self.index.entries[self.slot_index];
        unsafe { (*entry.key.get()).assume_init_ref() }
    }

    /// Commit the reserved key with a value.
    pub fn commit(mut self, value: V) {
        {
            let _guard = self.index.lock.lock();
            let entry = &self.index.entries[self.slot_index];
            unsafe {
                (*entry.value.get()).write(value);
                *entry.state.get() = COMMITTED;
            }
        }
        self.committed = true;

        // L6 MutationIndexCommit emit (OBS-8).
        //
        // Emitted after the state transition so the entry is already
        // observable to readers.  Gated by `INDEX_MUTATION_EMIT_ENABLED`
        // (default off).  This is a substrate convergence point, not
        // inside a `StepOp::step` body (OBS-A-1).
        if INDEX_MUTATION_EMIT_ENABLED.load(Ordering::Relaxed) {
            if let Some(em) = tx_observe::current() {
                // `index_id` is the lower 32 bits of the index pointer —
                // a stable per-instance discriminant within a single boot.
                let index_id = self.index as *const _ as usize as u32;
                em.mutation_index_commit(
                    index_id,
                    self.slot_index as u32,
                    0, // generic V; no Cap available here
                );
            }
        }
    }
}

impl<K, V, const N: usize> Drop for IndexReservation<'_, K, V, N> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        let _guard = self.index.lock.lock();
        let entry = &self.index.entries[self.slot_index];
        unsafe {
            if *entry.state.get() == RESERVED_EMPTY {
                (*entry.key.get()).assume_init_drop();
                *entry.state.get() = EMPTY;
            }
        }
    }
}

/// Reservation for mutating an existing committed entry.
pub(crate) struct CommittedReservation<'i, K, V, const N: usize> {
    index: &'i Index<K, V, N>,
    slot_index: usize,
    committed: bool,
}

impl<K, V, const N: usize> CommittedReservation<'_, K, V, N> {
    pub(crate) fn withdraw(mut self) -> V {
        let _guard = self.index.lock.lock();
        let entry = &self.index.entries[self.slot_index];
        let value = unsafe {
            let value = (*entry.value.get()).assume_init_read();
            (*entry.key.get()).assume_init_drop();
            *entry.state.get() = EMPTY;
            value
        };
        self.committed = true;
        value
    }

    pub(crate) fn swap(mut self, replacement: V) -> V {
        let _guard = self.index.lock.lock();
        let entry = &self.index.entries[self.slot_index];
        let old = unsafe {
            let old = (*entry.value.get()).assume_init_read();
            (*entry.value.get()).write(replacement);
            *entry.state.get() = COMMITTED;
            old
        };
        self.committed = true;
        old
    }
}

impl<K, V, const N: usize> Drop for CommittedReservation<'_, K, V, N> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        let _guard = self.index.lock.lock();
        let entry = &self.index.entries[self.slot_index];
        unsafe {
            if *entry.state.get() == RESERVED_COMMITTED {
                *entry.state.get() = COMMITTED;
            }
        }
    }
}

/// Guard-scoped committed index observation.
pub struct IndexRef<'g, K, V> {
    key: &'g K,
    value: &'g V,
    _lock: SpinGuard<'g>,
}

impl<K, V> IndexRef<'_, K, V> {
    /// Observed key.
    pub fn key(&self) -> &K {
        self.key
    }

    /// Observed value.
    pub fn value(&self) -> &V {
        self.value
    }
}

impl<K, V> Deref for IndexRef<'_, K, V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        self.value
    }
}
