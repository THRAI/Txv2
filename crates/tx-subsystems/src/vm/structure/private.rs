//! Per-mapping private-CoW page tracking.
//!
//! See `docs/progress/decisions/2026-05-12-pc-cow-implementation-plan.md`
//! and the design rules in `docs/design/03_memory-vm/VM_v1_2.md`
//! (`txdoc:VM-7-2-WRITE-FAULT-ON-MAP-PRIVATE`,
//! `txdoc:VM-7-3-PRIVATE-FRAMES-HAVE-NO-BACK-INDEX`) and
//! `docs/design/03_memory-vm/PAGE_BACKED_v1.md` (`txdoc:PAGE-BACKED-10-3`).
//!
//! `PrivatePageSet` is the authoritative store for a `VmEntry`'s
//! private (CoW-diverged) pages. It is attached to the mapping identity
//! (the `VmEntry`), keyed by `VmPageOff` (page offset within the entry's
//! range) — not by absolute `UserPage`. Absolute-VA keying is fragile
//! under munmap+mmap-same-VA, MAP_FIXED, mremap, and VmEntry
//! split/merge.
//!
//! A `PrivatePageSet` is zone-allocated so a `Cap<PrivatePageSet>` lives
//! on `VmEntry` cheaply (one atomic refcount bump per `VmEntry::clone`).
//! Recipe tree re-publishes (split/merge during mprotect, swap_commit,
//! etc.) preserve mapping identity by reusing the same Cap.
//!
//! Private contents are **authoritative**, not cache: after a write,
//! the bytes are unrecoverable from any other source. `cache_ref` on
//! the underlying `Frame` keeps each entry alive while the set retains
//! it; pmap `MapPin`s are acquired separately on PTE install.

use alloc::collections::BTreeMap;
use tx_hal::Ppn;
use step_engine::page_allocator::{self, BitmapPageAllocator, CachePin};
use crate::vm::adapter::step_engine::{Cap, SpinMutex, Zone, ZoneAllocated, ZoneError};
use crate::vm::adapter::step_engine::{self as step_engine};

/// Page offset within a `VmEntry`'s range. `VmPageOff(0)` is the first
/// page of the entry; offsets are relative to the entry's `range.start`
/// and stay stable across `mremap`-move.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VmPageOff(pub u64);

/// CoW state of a private frame.
///
/// Transitions:
/// - `Exclusive → SharedCow`: fork (`fork_share`).
/// - `SharedCow → Exclusive`: first writer copies + replaces via
///   `replace_if_match`.
/// - New entries always start `Exclusive` (a write fault that missed
///   the set allocated a private frame and won the publish CAS).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivateFrameState {
    /// This mapping owns the frame outright; writable PTE OK.
    Exclusive,
    /// Frame is read-aliased between fork-related mappings (parent and
    /// at least one child share it). First writer on any side must
    /// allocate a new frame and transition that side to `Exclusive`.
    SharedCow,
}

/// One private CoW page held by a `PrivatePageSet`.
///
/// The `CachePin` keeps the frame's `cache_ref >= 1` for the lifetime
/// of the entry. `state` discriminates between exclusive ownership and
/// fork-shared aliasing.
pub struct PrivateFrame {
    pub ppn: Ppn,
    pub state: PrivateFrameState,
    pub cache_pin: CachePin<'static, BitmapPageAllocator<'static>>,
}

impl PrivateFrame {
    pub fn new(
        ppn: Ppn,
        state: PrivateFrameState,
        cache_pin: CachePin<'static, BitmapPageAllocator<'static>>,
    ) -> Self {
        Self {
            ppn,
            state,
            cache_pin,
        }
    }
}

impl core::fmt::Debug for PrivateFrame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PrivateFrame")
            .field("ppn", &self.ppn)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// Snapshot of a `PrivateFrame` returned by lookup. Carries `ppn` and
/// `state` for the caller to act on without holding the set's lock,
/// plus the source identity used by `replace_if_match`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrivateFrameSnapshot {
    pub ppn: Ppn,
    pub state: PrivateFrameState,
}

/// Source identity for `replace_if_match` linearization. Two writers
/// racing on the same `(off, src_ppn, SharedCow)` will both attempt
/// the CAS; exactly one wins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrivateFrameIdentity {
    pub ppn: Ppn,
    pub state: PrivateFrameState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivatePageError {
    /// Another writer published a different frame at the same offset
    /// after we copied. Caller should drop its candidate frame and
    /// re-read the set.
    Conflict { current: PrivateFrameSnapshot },
    /// `replace_if_match` saw no entry at `off`.
    Missing,
    /// Zone allocation failed.
    Zone(ZoneError),
}

impl From<ZoneError> for PrivatePageError {
    fn from(error: ZoneError) -> Self {
        Self::Zone(error)
    }
}

static PRIVATE_PAGE_SET_ZONE: Zone<PrivatePageSet> = Zone::const_new();

unsafe impl ZoneAllocated for PrivatePageSet {
    fn zone() -> &'static Zone<Self> {
        &PRIVATE_PAGE_SET_ZONE
    }
}

/// Per-mapping private CoW store. Owned by exactly one `VmEntry`
/// identity at a time via `Cap<PrivatePageSet>`; cloned VmEntries
/// (recipe re-publishes) share the same Cap so the authoritative
/// state is observable through every clone.
pub struct PrivatePageSet {
    pages: SpinMutex<BTreeMap<VmPageOff, PrivateFrame>>,
}

// `PrivatePageSet` carries `CachePin`s (via `PrivateFrame`) that are
// `!Send` by virtue of pinning to a specific page-allocator. The lock
// + zone discipline make whole-set access kernel-safe; mirrors the
// `AddressSpace` Send/Sync override.
unsafe impl Send for PrivatePageSet {}
unsafe impl Sync for PrivatePageSet {}

impl PrivatePageSet {
    pub fn new() -> Self {
        Self {
            pages: SpinMutex::new(BTreeMap::new()),
        }
    }

    pub fn new_cap() -> Result<Cap<Self>, ZoneError> {
        let reservation = step_engine::reserve_for::<Self>()?;
        Ok(step_engine::sign_for(reservation, Self::new()))
    }

    /// Read-only snapshot at `off`.
    pub fn lookup(&self, off: VmPageOff) -> Option<PrivateFrameSnapshot> {
        let pages = self.pages.lock();
        pages.get(&off).map(|pf| PrivateFrameSnapshot {
            ppn: pf.ppn,
            state: pf.state,
        })
    }

    /// CAS-publish a fresh entry. Used by the write-fault miss path.
    /// Returns the published snapshot on success; `Conflict` if another
    /// writer beat us.
    pub fn install_if_absent(
        &self,
        off: VmPageOff,
        frame: PrivateFrame,
    ) -> Result<PrivateFrameSnapshot, PrivatePageError> {
        let mut pages = self.pages.lock();
        if let Some(existing) = pages.get(&off) {
            return Err(PrivatePageError::Conflict {
                current: PrivateFrameSnapshot {
                    ppn: existing.ppn,
                    state: existing.state,
                },
            });
        }
        let snap = PrivateFrameSnapshot {
            ppn: frame.ppn,
            state: frame.state,
        };
        pages.insert(off, frame);
        Ok(snap)
    }

    /// CAS-publish a replacement entry. Used by the `SharedCow →
    /// Exclusive` write-fault transition.
    ///
    /// Linearizes against concurrent writers: succeeds iff the current
    /// entry's `(ppn, state)` matches `expected`. On mismatch returns
    /// `Conflict { current }`; on absence returns `Missing`.
    pub fn replace_if_match(
        &self,
        off: VmPageOff,
        expected: PrivateFrameIdentity,
        new: PrivateFrame,
    ) -> Result<PrivateFrameSnapshot, PrivatePageError> {
        let mut pages = self.pages.lock();
        let Some(existing) = pages.get(&off) else {
            return Err(PrivatePageError::Missing);
        };
        if existing.ppn != expected.ppn || existing.state != expected.state {
            return Err(PrivatePageError::Conflict {
                current: PrivateFrameSnapshot {
                    ppn: existing.ppn,
                    state: existing.state,
                },
            });
        }
        let snap = PrivateFrameSnapshot {
            ppn: new.ppn,
            state: new.state,
        };
        pages.insert(off, new);
        Ok(snap)
    }

    /// Drop entries whose offsets fall in `[range_start_off, range_end_off)`.
    /// Each removed `PrivateFrame` releases its `CachePin` on drop.
    ///
    /// Used by munmap / mremap-shrink / MAP_FIXED-replace.
    pub fn drain_range(&self, start: VmPageOff, end: VmPageOff) {
        let mut pages = self.pages.lock();
        let removed: alloc::vec::Vec<VmPageOff> =
            pages.range(start..end).map(|(off, _)| *off).collect();
        for off in removed {
            pages.remove(&off);
        }
    }

    /// Number of resident entries (for tests / observability).
    pub fn len(&self) -> usize {
        self.pages.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.lock().is_empty()
    }

    /// Fork: transition every entry to `SharedCow` in `self` and return
    /// a sibling set carrying matching `SharedCow` entries (each frame's
    /// `cache_ref` bumps once for the child).
    ///
    /// Allocation failure mid-walk rolls back: the child's set drops
    /// (releasing every cache_pin it acquired) and `self`'s state
    /// transitions are committed only for entries the child also got.
    /// On the rollback path entries we already transitioned to
    /// `SharedCow` in `self` *stay* `SharedCow`; that's safe because
    /// `SharedCow` is strictly more conservative than `Exclusive` (it
    /// triggers an extra CoW on next write, never less safe).
    pub fn fork_share(&self) -> Result<Cap<Self>, PrivatePageError> {
        let child_set = Self::new_cap()?;
        let mut parent_pages = self.pages.lock();
        for (off, parent_frame) in parent_pages.iter_mut() {
            let cache_pin = page_allocator::acquire_cache_pin(parent_frame.ppn).map_err(|_| {
                // Allocation failure: drop everything we collected on
                // the child by returning the partly-built child set
                // (caller drops). Parent stays in its potentially
                // partially-transitioned state which, as noted, is
                // safe.
                PrivatePageError::Zone(ZoneError::AllocationFailed)
            })?;
            parent_frame.state = PrivateFrameState::SharedCow;
            let child_frame = PrivateFrame {
                ppn: parent_frame.ppn,
                state: PrivateFrameState::SharedCow,
                cache_pin,
            };
            child_set.pages.lock().insert(*off, child_frame);
        }
        Ok(child_set)
    }

    /// VmEntry split helper: build a fresh set containing only entries
    /// whose offsets fall in `[start, end)` (relative to the parent
    /// entry), rebased by `-rebase_delta` (so offsets become relative
    /// to the new sub-entry's range start).
    ///
    /// Each surviving entry takes a fresh `CachePin` on its frame; the
    /// original set keeps its entries unchanged. Caller is responsible
    /// for dropping the parts of the parent set that no longer belong
    /// to the surviving sub-entry (via `drain_range`).
    pub fn split(
        &self,
        start: VmPageOff,
        end: VmPageOff,
        rebase_delta: u64,
    ) -> Result<Cap<Self>, PrivatePageError> {
        let new_set = Self::new_cap()?;
        let parent_pages = self.pages.lock();
        for (off, parent_frame) in parent_pages.range(start..end) {
            let cache_pin = page_allocator::acquire_cache_pin(parent_frame.ppn)
                .map_err(|_| PrivatePageError::Zone(ZoneError::AllocationFailed))?;
            let new_off = VmPageOff(off.0 - rebase_delta);
            let frame = PrivateFrame {
                ppn: parent_frame.ppn,
                state: parent_frame.state,
                cache_pin,
            };
            new_set.pages.lock().insert(new_off, frame);
        }
        Ok(new_set)
    }
}

impl Default for PrivatePageSet {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for PrivatePageSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PrivatePageSet")
            .field("len", &self.len())
            .finish()
    }
}
