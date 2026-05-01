//! VM foundation values and the first bounded AddressSpace recipe core.
//!
//! This module intentionally stops before zone-owned `AddressSpace` evidence,
//! persistent epoch snapshots, page-backed content, or pmap materialization.
//! The types here are host-testable arithmetic, coordination, and recipe-index
//! building blocks for those later layers.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::page_backed::{
    MaterializeAccess, MaterializedPage, PageCacheError, PageContainer, PageIndex,
};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

pub const USER_PAGE_SIZE: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UserVirtAddr(pub usize);

impl UserVirtAddr {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn as_usize(self) -> usize {
        self.0
    }

    pub const fn is_page_aligned(self) -> bool {
        self.0.is_multiple_of(USER_PAGE_SIZE)
    }

    pub const fn containing_page(self) -> UserPage {
        UserPage(self.0 / USER_PAGE_SIZE)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UserPage(pub usize);

impl UserPage {
    pub const fn start_addr(self) -> UserVirtAddr {
        UserVirtAddr(self.0 * USER_PAGE_SIZE)
    }

    pub fn checked_start_addr(self) -> Result<UserVirtAddr, UserRangeError> {
        self.0
            .checked_mul(USER_PAGE_SIZE)
            .map(UserVirtAddr)
            .ok_or(UserRangeError::Overflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserRangeError {
    ZeroLength,
    Unaligned,
    Overflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserRange {
    start: UserVirtAddr,
    end: UserVirtAddr,
}

impl UserRange {
    pub fn new_aligned(start: UserVirtAddr, len: usize) -> Result<Self, UserRangeError> {
        if len == 0 {
            return Err(UserRangeError::ZeroLength);
        }
        if !start.is_page_aligned() || !len.is_multiple_of(USER_PAGE_SIZE) {
            return Err(UserRangeError::Unaligned);
        }
        let end = start.0.checked_add(len).ok_or(UserRangeError::Overflow)?;
        let range = Self {
            start,
            end: UserVirtAddr(end),
        };
        range.validate_aligned()?;
        Ok(range)
    }

    pub fn from_pages(start: UserPage, page_count: usize) -> Result<Self, UserRangeError> {
        let len = page_count
            .checked_mul(USER_PAGE_SIZE)
            .ok_or(UserRangeError::Overflow)?;
        Self::new_aligned(start.checked_start_addr()?, len)
    }

    pub fn containing_page(addr: UserVirtAddr) -> Result<Self, UserRangeError> {
        let start = addr.containing_page().checked_start_addr()?;
        let end = start
            .0
            .checked_add(USER_PAGE_SIZE)
            .ok_or(UserRangeError::Overflow)?;
        Ok(Self {
            start,
            end: UserVirtAddr(end),
        })
    }

    pub const fn start(self) -> UserVirtAddr {
        self.start
    }

    pub const fn end(self) -> UserVirtAddr {
        self.end
    }

    pub const fn len(self) -> usize {
        self.end.0 - self.start.0
    }

    pub const fn is_empty(self) -> bool {
        self.start.0 == self.end.0
    }

    pub const fn page_count(self) -> usize {
        self.len() / USER_PAGE_SIZE
    }

    pub const fn contains_addr(self, addr: UserVirtAddr) -> bool {
        self.start.0 <= addr.0 && addr.0 < self.end.0
    }

    pub const fn contains_range(self, other: Self) -> bool {
        self.start.0 <= other.start.0 && other.end.0 <= self.end.0
    }

    pub const fn overlaps(self, other: Self) -> bool {
        self.start.0 < other.end.0 && other.start.0 < self.end.0
    }

    pub const fn iter_pages(self) -> UserPageIter {
        UserPageIter {
            next: self.start.0 / USER_PAGE_SIZE,
            end: self.end.0 / USER_PAGE_SIZE,
        }
    }

    fn validate_aligned(self) -> Result<(), UserRangeError> {
        if self.start.0 >= self.end.0 {
            return Err(UserRangeError::ZeroLength);
        }
        if !self.start.is_page_aligned() || !self.end.is_page_aligned() {
            return Err(UserRangeError::Unaligned);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserPageIter {
    next: usize,
    end: usize,
}

impl Iterator for UserPageIter {
    type Item = UserPage;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.end {
            None
        } else {
            let page = UserPage(self.next);
            self.next += 1;
            Some(page)
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.end - self.next;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for UserPageIter {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Prot {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl Prot {
    pub const NONE: Self = Self::new(false, false, false);
    pub const READ: Self = Self::new(true, false, false);
    pub const READ_WRITE: Self = Self::new(true, true, false);
    pub const READ_EXECUTE: Self = Self::new(true, false, true);

    pub const fn new(read: bool, write: bool, execute: bool) -> Self {
        Self {
            read,
            write,
            execute,
        }
    }

    pub const fn permits(self, access: AccessMode) -> bool {
        match access {
            AccessMode::Read => self.read,
            AccessMode::Write => self.write,
            AccessMode::Execute => self.execute,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessMode {
    Read,
    Write,
    Execute,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VmEntryFlags {
    pub shared: bool,
    pub grows_down: bool,
    pub locked: bool,
}

impl VmEntryFlags {
    pub const PRIVATE: Self = Self::new(false, false, false);
    pub const SHARED: Self = Self::new(true, false, false);

    pub const fn new(shared: bool, grows_down: bool, locked: bool) -> Self {
        Self {
            shared,
            grows_down,
            locked,
        }
    }
}

/// Draft/test backing value. This is not a real PageContainer or VM authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmBackingDraft {
    None,
    PrivateAnon,
    PageBacked { container_id: u64, offset: u64 },
    MockPage { id: u64, offset: u64 },
}

impl VmBackingDraft {
    fn at_range_offset(self, delta: usize) -> Result<Self, VmEntryError> {
        match self {
            Self::PageBacked {
                container_id,
                offset,
            } => Ok(Self::PageBacked {
                container_id,
                offset: offset
                    .checked_add(
                        u64::try_from(delta).map_err(|_| VmEntryError::BackingOffsetOverflow)?,
                    )
                    .ok_or(VmEntryError::BackingOffsetOverflow)?,
            }),
            Self::MockPage { id, offset } => Ok(Self::MockPage {
                id,
                offset: offset
                    .checked_add(
                        u64::try_from(delta).map_err(|_| VmEntryError::BackingOffsetOverflow)?,
                    )
                    .ok_or(VmEntryError::BackingOffsetOverflow)?,
            }),
            other => Ok(other),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmEntry {
    pub range: UserRange,
    pub prot: Prot,
    pub flags: VmEntryFlags,
    pub backing: VmBackingDraft,
}

impl VmEntry {
    pub const fn new(
        range: UserRange,
        prot: Prot,
        flags: VmEntryFlags,
        backing: VmBackingDraft,
    ) -> Self {
        Self {
            range,
            prot,
            flags,
            backing,
        }
    }

    pub fn split_for_unmap(self, hole: UserRange) -> Result<VmEntryRewrite, VmEntryError> {
        self.split_rewrite(hole, None)
    }

    pub fn split_for_protect(
        self,
        target: UserRange,
        prot: Prot,
    ) -> Result<VmEntryRewrite, VmEntryError> {
        self.split_rewrite(target, Some(prot))
    }

    fn split_rewrite(
        self,
        target: UserRange,
        target_prot: Option<Prot>,
    ) -> Result<VmEntryRewrite, VmEntryError> {
        if !self.range.contains_range(target) {
            return Err(VmEntryError::RangeNotContained);
        }

        let before = if self.range.start < target.start {
            Some(self.sub_entry(self.range.start, target.start, self.prot)?)
        } else {
            None
        };
        let target_entry = match target_prot {
            Some(prot) => Some(self.sub_entry(target.start, target.end, prot)?),
            None => None,
        };
        let after = if target.end < self.range.end {
            Some(self.sub_entry(target.end, self.range.end, self.prot)?)
        } else {
            None
        };

        Ok(VmEntryRewrite {
            before,
            target: target_entry,
            after,
        })
    }

    fn sub_entry(
        self,
        start: UserVirtAddr,
        end: UserVirtAddr,
        prot: Prot,
    ) -> Result<Self, VmEntryError> {
        let len = end
            .0
            .checked_sub(start.0)
            .ok_or(VmEntryError::RangeNotContained)?;
        let range = UserRange::new_aligned(start, len).map_err(VmEntryError::Range)?;
        let delta = start.0 - self.range.start.0;
        Ok(Self {
            range,
            prot,
            flags: self.flags,
            backing: self.backing.at_range_offset(delta)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmEntryRewrite {
    pub before: Option<VmEntry>,
    pub target: Option<VmEntry>,
    pub after: Option<VmEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmEntryError {
    Range(UserRangeError),
    RangeNotContained,
    BackingOffsetOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapPlacement {
    RequireFree,
    FixedReplace,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AddressSpaceStats {
    pub recipe_count: usize,
    pub vm_size: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PmapSeam {
    pub materialization_deferred: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmMapCommit {
    pub changed_pages: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmMapError {
    AlreadyMapped,
    InvalidRange,
    MissingMapping,
    NoFreeRange,
    WouldBlock,
    BackingOffsetOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmMapTarget {
    Anywhere {
        window: UserRange,
        page_count: usize,
    },
    Fixed {
        range: UserRange,
        placement: MapPlacement,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmMapRequest {
    pub target: VmMapTarget,
    pub prot: Prot,
    pub flags: VmEntryFlags,
    pub backing: VmBackingDraft,
}

impl VmMapRequest {
    pub const fn anywhere(
        window: UserRange,
        page_count: usize,
        prot: Prot,
        flags: VmEntryFlags,
        backing: VmBackingDraft,
    ) -> Self {
        Self {
            target: VmMapTarget::Anywhere { window, page_count },
            prot,
            flags,
            backing,
        }
    }

    pub const fn fixed(
        range: UserRange,
        placement: MapPlacement,
        prot: Prot,
        flags: VmEntryFlags,
        backing: VmBackingDraft,
    ) -> Self {
        Self {
            target: VmMapTarget::Fixed { range, placement },
            prot,
            flags,
            backing,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmMapOutcome {
    pub range: UserRange,
    pub commit: VmMapCommit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmRemapRequest {
    pub old_range: UserRange,
    pub new_range: UserRange,
}

impl VmRemapRequest {
    pub const fn new(old_range: UserRange, new_range: UserRange) -> Self {
        Self {
            old_range,
            new_range,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmRemapOutcome {
    pub old_range: UserRange,
    pub new_range: UserRange,
    pub commit: VmMapCommit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmFault {
    pub addr: UserVirtAddr,
    pub access: AccessMode,
}

impl VmFault {
    pub const fn new(addr: UserVirtAddr, access: AccessMode) -> Self {
        Self { addr, access }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmFaultOutcome {
    pub page_range: UserRange,
    pub entry: VmEntry,
    pub access: AccessMode,
    pub pmap_materialization_deferred: bool,
}

impl VmFaultOutcome {
    pub fn materialize_pagebacked_anon(
        self,
        pc: &mut PageContainer,
    ) -> Result<VmFaultMaterialization, VmFaultError> {
        let page_index = self.backing_page_index()?;
        let access = match self.access {
            AccessMode::Write => MaterializeAccess::Write,
            AccessMode::Read | AccessMode::Execute => MaterializeAccess::Read,
        };
        let page = pc
            .materialize_anon(page_index, access)
            .map_err(VmFaultError::PageCache)?;
        Ok(VmFaultMaterialization {
            page_index,
            page,
            pmap_materialization_deferred: self.pmap_materialization_deferred,
        })
    }

    fn backing_page_index(self) -> Result<PageIndex, VmFaultError> {
        let VmBackingDraft::PageBacked { offset, .. } = self.entry.backing else {
            return Err(VmFaultError::BackingMismatch);
        };
        let delta = self
            .page_range
            .start()
            .as_usize()
            .checked_sub(self.entry.range.start().as_usize())
            .ok_or(VmFaultError::BackingOffsetOverflow)?;
        let byte_offset = offset
            .checked_add(u64::try_from(delta).map_err(|_| VmFaultError::BackingOffsetOverflow)?)
            .ok_or(VmFaultError::BackingOffsetOverflow)?;
        if !byte_offset.is_multiple_of(USER_PAGE_SIZE as u64) {
            return Err(VmFaultError::BackingOffsetOverflow);
        }
        Ok(PageIndex::new(byte_offset / USER_PAGE_SIZE as u64))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmFaultMaterialization {
    pub page_index: PageIndex,
    pub page: MaterializedPage,
    pub pmap_materialization_deferred: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmFaultError {
    Range(UserRangeError),
    NoRecipe,
    ProtectionViolation,
    WouldBlock,
    BackingMismatch,
    BackingOffsetOverflow,
    PageCache(PageCacheError),
}

pub struct AddressSpace {
    recipes: RecipeIndex,
    pmap: PmapSeam,
    range_lock: RangeLock,
    stats: AddressSpaceStatsCell,
}

impl AddressSpace {
    pub fn new() -> Self {
        Self {
            recipes: RecipeIndex::new(),
            pmap: PmapSeam {
                materialization_deferred: true,
            },
            range_lock: RangeLock::new(),
            stats: AddressSpaceStatsCell::new(),
        }
    }

    pub const fn pmap(&self) -> &PmapSeam {
        &self.pmap
    }

    pub const fn range_lock(&self) -> &RangeLock {
        &self.range_lock
    }

    pub fn stats(&self) -> AddressSpaceStats {
        self.stats.load()
    }

    pub fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry> {
        self.recipes.lookup(addr)
    }

    pub fn find_free_range(&self, window: UserRange, page_count: usize) -> Option<UserRange> {
        self.recipes.find_free_range(window, page_count)
    }

    pub fn recipes_overlapping(&self, range: UserRange) -> Vec<VmEntry> {
        self.recipes.overlapping(range)
    }

    pub fn resolve_fault(&self, fault: VmFault) -> Result<VmFaultOutcome, VmFaultError> {
        let page_range = UserRange::containing_page(fault.addr).map_err(VmFaultError::Range)?;
        let _guard = match self.range_lock.acquire(page_range, LockMode::Materializer) {
            AcquireResult::Acquired(guard) => guard,
            AcquireResult::WouldBlock(_) => return Err(VmFaultError::WouldBlock),
        };

        let entry = self
            .recipes
            .lookup(fault.addr)
            .ok_or(VmFaultError::NoRecipe)?;
        if !entry.prot.permits(fault.access) {
            return Err(VmFaultError::ProtectionViolation);
        }

        Ok(VmFaultOutcome {
            page_range,
            entry,
            access: fault.access,
            pmap_materialization_deferred: self.pmap.materialization_deferred,
        })
    }

    pub fn map_script(&self, request: VmMapRequest) -> Result<VmMapOutcome, VmMapError> {
        let (range, placement) = match request.target {
            VmMapTarget::Anywhere { window, page_count } => {
                let range = self
                    .find_free_range(window, page_count)
                    .ok_or(VmMapError::NoFreeRange)?;
                (range, MapPlacement::RequireFree)
            }
            VmMapTarget::Fixed { range, placement } => (range, placement),
        };
        let entry = VmEntry::new(range, request.prot, request.flags, request.backing);

        match self.reserve_map(entry, placement) {
            MapReserveResult::Reserved(reservation) => {
                let commit = reservation.commit()?;
                Ok(VmMapOutcome { range, commit })
            }
            MapReserveResult::WouldBlock(_) => Err(VmMapError::WouldBlock),
            MapReserveResult::Err(error) => Err(error),
        }
    }

    pub fn remap_script(&self, request: VmRemapRequest) -> Result<VmRemapOutcome, VmMapError> {
        if request.old_range.overlaps(request.new_range)
            || request.old_range.len() != request.new_range.len()
        {
            return Err(VmMapError::InvalidRange);
        }

        let _guard_pair = match self.range_lock.acquire_pair(
            (request.old_range, LockMode::ExclusiveWriter),
            (request.new_range, LockMode::ExclusiveWriter),
        ) {
            AcquirePairResult::Acquired(pair) => pair,
            AcquirePairResult::WouldBlock(_) => return Err(VmMapError::WouldBlock),
        };
        let commit = self
            .recipes
            .remap_disjoint(request.old_range, request.new_range)?;
        self.stats.store(self.recipes.stats());
        Ok(VmRemapOutcome {
            old_range: request.old_range,
            new_range: request.new_range,
            commit,
        })
    }

    pub fn reserve_map(&self, entry: VmEntry, placement: MapPlacement) -> MapReserveResult<'_> {
        let guard = match self
            .range_lock
            .acquire(entry.range, LockMode::ExclusiveWriter)
        {
            AcquireResult::Acquired(guard) => guard,
            AcquireResult::WouldBlock(blocked) => return MapReserveResult::WouldBlock(blocked),
        };

        if let Err(error) = self.recipes.validate_map(entry, placement) {
            return MapReserveResult::Err(error);
        }

        MapReserveResult::Reserved(MapReservation {
            aspace: self,
            entry,
            placement,
            _guard: guard,
        })
    }

    pub fn unmap(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        let _guard = self.acquire_writer(range)?;
        let commit = self.recipes.unmap(range)?;
        self.stats.store(self.recipes.stats());
        Ok(commit)
    }

    pub fn protect(&self, range: UserRange, prot: Prot) -> Result<VmMapCommit, VmMapError> {
        let _guard = self.acquire_writer(range)?;
        let commit = self.recipes.protect(range, prot)?;
        self.stats.store(self.recipes.stats());
        Ok(commit)
    }

    fn acquire_writer(&self, range: UserRange) -> Result<RangeGuard<'_>, VmMapError> {
        match self.range_lock.acquire(range, LockMode::ExclusiveWriter) {
            AcquireResult::Acquired(guard) => Ok(guard),
            AcquireResult::WouldBlock(_) => Err(VmMapError::WouldBlock),
        }
    }

    fn commit_reserved_map(
        &self,
        entry: VmEntry,
        placement: MapPlacement,
    ) -> Result<VmMapCommit, VmMapError> {
        let commit = self.recipes.commit_map(entry, placement)?;
        self.stats.store(self.recipes.stats());
        Ok(commit)
    }
}

impl Default for AddressSpace {
    fn default() -> Self {
        Self::new()
    }
}

pub enum MapReserveResult<'a> {
    Reserved(MapReservation<'a>),
    WouldBlock(WouldBlock<'a>),
    Err(VmMapError),
}

impl core::fmt::Debug for MapReserveResult<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Reserved(_) => f.write_str("Reserved(..)"),
            Self::WouldBlock(_) => f.write_str("WouldBlock(..)"),
            Self::Err(error) => f.debug_tuple("Err").field(error).finish(),
        }
    }
}

pub struct MapReservation<'a> {
    aspace: &'a AddressSpace,
    entry: VmEntry,
    placement: MapPlacement,
    _guard: RangeGuard<'a>,
}

impl MapReservation<'_> {
    pub fn entry(&self) -> VmEntry {
        self.entry
    }

    pub fn placement(&self) -> MapPlacement {
        self.placement
    }

    pub fn commit(self) -> Result<VmMapCommit, VmMapError> {
        self.aspace.commit_reserved_map(self.entry, self.placement)
    }
}

/// Authoritative recipe range index.
///
/// This wrapper is backed by `BTreeMap` keyed by mapping start address, so it
/// is no longer the artificial fixed-capacity array from the first slice. It
/// still deliberately stops short of VM_v1_2's persistent/epoch snapshot
/// contract until the substrate exposes a fitting persistent range index.
struct RecipeIndex {
    entries: SpinMutex<BTreeMap<UserVirtAddr, VmEntry>>,
}

impl RecipeIndex {
    fn new() -> Self {
        Self {
            entries: SpinMutex::new(BTreeMap::new()),
        }
    }

    fn lookup(&self, addr: UserVirtAddr) -> Option<VmEntry> {
        lookup_in(&self.entries.lock(), addr)
    }

    fn stats(&self) -> AddressSpaceStats {
        let entries = self.entries.lock();
        stats_for(&entries)
    }

    fn find_free_range(&self, window: UserRange, page_count: usize) -> Option<UserRange> {
        let entries = self.entries.lock();
        find_gap_in(&entries, window, page_count)
    }

    fn overlapping(&self, range: UserRange) -> Vec<VmEntry> {
        let entries = self.entries.lock();
        overlapping_in(&entries, range)
    }

    fn validate_map(&self, entry: VmEntry, placement: MapPlacement) -> Result<(), VmMapError> {
        let entries = self.entries.lock();
        match placement {
            MapPlacement::RequireFree => validate_insert_free(&entries, entry),
            MapPlacement::FixedReplace => rewrite_fixed(&entries, entry).map(|_| ()),
        }
    }

    fn commit_map(
        &self,
        entry: VmEntry,
        placement: MapPlacement,
    ) -> Result<VmMapCommit, VmMapError> {
        let mut entries = self.entries.lock();
        match placement {
            MapPlacement::RequireFree => {
                validate_insert_free(&entries, entry)?;
                push_entry(&mut entries, entry);
                Ok(VmMapCommit {
                    changed_pages: entry.range.page_count(),
                })
            }
            MapPlacement::FixedReplace => {
                let (rewritten, changed_pages) = rewrite_fixed(&entries, entry)?;
                *entries = rewritten;
                Ok(VmMapCommit { changed_pages })
            }
        }
    }

    fn unmap(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        let mut entries = self.entries.lock();
        let (rewritten, changed_pages) = rewrite_unmap(&entries, range)?;
        *entries = rewritten;
        Ok(VmMapCommit { changed_pages })
    }

    fn protect(&self, range: UserRange, prot: Prot) -> Result<VmMapCommit, VmMapError> {
        let mut entries = self.entries.lock();
        let (rewritten, changed_pages) = rewrite_protect(&entries, range, prot)?;
        *entries = rewritten;
        Ok(VmMapCommit { changed_pages })
    }

    fn remap_disjoint(
        &self,
        old_range: UserRange,
        new_range: UserRange,
    ) -> Result<VmMapCommit, VmMapError> {
        let mut entries = self.entries.lock();
        let (rewritten, changed_pages) = rewrite_remap_disjoint(&entries, old_range, new_range)?;
        *entries = rewritten;
        Ok(VmMapCommit { changed_pages })
    }
}

struct AddressSpaceStatsCell {
    recipe_count: AtomicUsize,
    vm_size: AtomicUsize,
}

impl AddressSpaceStatsCell {
    const fn new() -> Self {
        Self {
            recipe_count: AtomicUsize::new(0),
            vm_size: AtomicUsize::new(0),
        }
    }

    fn load(&self) -> AddressSpaceStats {
        AddressSpaceStats {
            recipe_count: self.recipe_count.load(Ordering::Acquire),
            vm_size: self.vm_size.load(Ordering::Acquire),
        }
    }

    fn store(&self, stats: AddressSpaceStats) {
        self.recipe_count
            .store(stats.recipe_count, Ordering::Release);
        self.vm_size.store(stats.vm_size, Ordering::Release);
    }
}

fn validate_insert_free(
    entries: &BTreeMap<UserVirtAddr, VmEntry>,
    entry: VmEntry,
) -> Result<(), VmMapError> {
    if entries
        .values()
        .any(|existing| existing.range.overlaps(entry.range))
    {
        return Err(VmMapError::AlreadyMapped);
    }

    Ok(())
}

fn rewrite_fixed(
    entries: &BTreeMap<UserVirtAddr, VmEntry>,
    replacement: VmEntry,
) -> Result<(BTreeMap<UserVirtAddr, VmEntry>, usize), VmMapError> {
    let mut rewritten = BTreeMap::new();
    let mut replaced_pages = 0;

    for existing in entries.values().copied() {
        let Some(overlap) = range_intersection(existing.range, replacement.range) else {
            push_entry(&mut rewritten, existing);
            continue;
        };

        replaced_pages += overlap.page_count();
        let rewrite = existing.split_for_unmap(overlap).map_err(vm_entry_error)?;
        if let Some(before) = rewrite.before {
            push_entry(&mut rewritten, before);
        }
        if let Some(after) = rewrite.after {
            push_entry(&mut rewritten, after);
        }
    }

    push_entry(&mut rewritten, replacement);
    Ok((rewritten, replaced_pages + replacement.range.page_count()))
}

fn rewrite_unmap(
    entries: &BTreeMap<UserVirtAddr, VmEntry>,
    range: UserRange,
) -> Result<(BTreeMap<UserVirtAddr, VmEntry>, usize), VmMapError> {
    let mut rewritten = BTreeMap::new();
    let mut changed_pages = 0;

    for existing in entries.values().copied() {
        let Some(overlap) = range_intersection(existing.range, range) else {
            push_entry(&mut rewritten, existing);
            continue;
        };

        changed_pages += overlap.page_count();
        let rewrite = existing.split_for_unmap(overlap).map_err(vm_entry_error)?;
        if let Some(before) = rewrite.before {
            push_entry(&mut rewritten, before);
        }
        if let Some(after) = rewrite.after {
            push_entry(&mut rewritten, after);
        }
    }

    Ok((rewritten, changed_pages))
}

fn rewrite_protect(
    entries: &BTreeMap<UserVirtAddr, VmEntry>,
    range: UserRange,
    prot: Prot,
) -> Result<(BTreeMap<UserVirtAddr, VmEntry>, usize), VmMapError> {
    if !range_is_fully_mapped(entries, range) {
        return Err(VmMapError::MissingMapping);
    }

    let mut rewritten = BTreeMap::new();
    let mut changed_pages = 0;

    for existing in entries.values().copied() {
        let Some(overlap) = range_intersection(existing.range, range) else {
            push_entry(&mut rewritten, existing);
            continue;
        };

        changed_pages += overlap.page_count();
        let rewrite = existing
            .split_for_protect(overlap, prot)
            .map_err(vm_entry_error)?;
        if let Some(before) = rewrite.before {
            push_entry(&mut rewritten, before);
        }
        if let Some(target) = rewrite.target {
            push_entry(&mut rewritten, target);
        }
        if let Some(after) = rewrite.after {
            push_entry(&mut rewritten, after);
        }
    }

    Ok((rewritten, changed_pages))
}

fn rewrite_remap_disjoint(
    entries: &BTreeMap<UserVirtAddr, VmEntry>,
    old_range: UserRange,
    new_range: UserRange,
) -> Result<(BTreeMap<UserVirtAddr, VmEntry>, usize), VmMapError> {
    if old_range.overlaps(new_range) || old_range.len() != new_range.len() {
        return Err(VmMapError::InvalidRange);
    }
    if !range_is_fully_mapped(entries, old_range) {
        return Err(VmMapError::MissingMapping);
    }
    validate_insert_free(
        entries,
        VmEntry::new(
            new_range,
            Prot::NONE,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::None,
        ),
    )?;

    let mut rewritten = BTreeMap::new();
    let mut moved = Vec::new();

    for existing in entries.values().copied() {
        let Some(overlap) = range_intersection(existing.range, old_range) else {
            push_entry(&mut rewritten, existing);
            continue;
        };

        let rewrite = existing.split_for_unmap(overlap).map_err(vm_entry_error)?;
        if let Some(before) = rewrite.before {
            push_entry(&mut rewritten, before);
        }
        if let Some(after) = rewrite.after {
            push_entry(&mut rewritten, after);
        }

        let moving = existing
            .split_for_protect(overlap, existing.prot)
            .map_err(vm_entry_error)?
            .target
            .ok_or(VmMapError::MissingMapping)?;
        let delta = overlap.start().as_usize() - old_range.start().as_usize();
        let target_start = UserVirtAddr(
            new_range
                .start()
                .as_usize()
                .checked_add(delta)
                .ok_or(VmMapError::InvalidRange)?,
        );
        let target_range = UserRange::new_aligned(target_start, overlap.len())
            .map_err(|_| VmMapError::InvalidRange)?;
        moved.push(VmEntry::new(
            target_range,
            moving.prot,
            moving.flags,
            moving.backing,
        ));
    }

    for entry in moved {
        push_entry(&mut rewritten, entry);
    }

    Ok((rewritten, old_range.page_count() + new_range.page_count()))
}

fn range_is_fully_mapped(entries: &BTreeMap<UserVirtAddr, VmEntry>, range: UserRange) -> bool {
    let mut cursor = range.start().as_usize();
    let end = range.end().as_usize();

    while cursor < end {
        let Some(entry) = lookup_in(entries, UserVirtAddr(cursor)) else {
            return false;
        };
        cursor = entry.range.end().as_usize().min(end);
    }

    true
}

fn push_entry(entries: &mut BTreeMap<UserVirtAddr, VmEntry>, entry: VmEntry) {
    entries.insert(entry.range.start(), entry);
}

fn lookup_in(entries: &BTreeMap<UserVirtAddr, VmEntry>, addr: UserVirtAddr) -> Option<VmEntry> {
    entries
        .range(..=addr)
        .next_back()
        .map(|(_, entry)| *entry)
        .filter(|entry| entry.range.contains_addr(addr))
}

fn stats_for(entries: &BTreeMap<UserVirtAddr, VmEntry>) -> AddressSpaceStats {
    let mut stats = AddressSpaceStats::default();
    for entry in entries.values() {
        stats.recipe_count += 1;
        stats.vm_size += entry.range.len();
    }
    stats
}

fn find_gap_in(
    entries: &BTreeMap<UserVirtAddr, VmEntry>,
    window: UserRange,
    page_count: usize,
) -> Option<UserRange> {
    let len = page_count.checked_mul(USER_PAGE_SIZE)?;
    if len == 0 || len > window.len() {
        return None;
    }

    let mut cursor = window.start().as_usize();
    let window_end = window.end().as_usize();

    for entry in entries.values().copied() {
        if entry.range.end().as_usize() <= cursor {
            continue;
        }
        if entry.range.start().as_usize() >= window_end {
            break;
        }
        if !entry.range.overlaps(window) {
            continue;
        }

        let entry_start = entry
            .range
            .start()
            .as_usize()
            .max(window.start().as_usize());
        if cursor.checked_add(len)? <= entry_start {
            return UserRange::new_aligned(UserVirtAddr(cursor), len).ok();
        }
        cursor = cursor.max(entry.range.end().as_usize());
        if cursor >= window_end {
            return None;
        }
    }

    if cursor.checked_add(len)? <= window_end {
        UserRange::new_aligned(UserVirtAddr(cursor), len).ok()
    } else {
        None
    }
}

fn overlapping_in(entries: &BTreeMap<UserVirtAddr, VmEntry>, range: UserRange) -> Vec<VmEntry> {
    entries
        .values()
        .copied()
        .filter(|entry| entry.range.overlaps(range))
        .collect()
}

fn range_intersection(a: UserRange, b: UserRange) -> Option<UserRange> {
    let start = a.start().as_usize().max(b.start().as_usize());
    let end = a.end().as_usize().min(b.end().as_usize());
    if start >= end {
        return None;
    }
    UserRange::new_aligned(UserVirtAddr(start), end - start).ok()
}

fn vm_entry_error(error: VmEntryError) -> VmMapError {
    match error {
        VmEntryError::BackingOffsetOverflow => VmMapError::BackingOffsetOverflow,
        VmEntryError::Range(_) | VmEntryError::RangeNotContained => VmMapError::AlreadyMapped,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockMode {
    ExclusiveWriter,
    Materializer,
}

pub enum AcquireResult<'a> {
    Acquired(RangeGuard<'a>),
    WouldBlock(WouldBlock<'a>),
}

pub enum AcquirePairResult<'a> {
    Acquired(RangeGuardPair<'a>),
    WouldBlock(WouldBlock<'a>),
}

pub struct WouldBlock<'a> {
    pending_writer: Option<PendingWriter<'a>>,
}

impl<'a> WouldBlock<'a> {
    pub fn pending_writer(self) -> Option<PendingWriter<'a>> {
        self.pending_writer
    }

    pub fn has_pending_writer(&self) -> bool {
        self.pending_writer.is_some()
    }
}

pub struct RangeGuard<'a> {
    lock: &'a RangeLock,
    id: u64,
    range: UserRange,
}

impl Drop for RangeGuard<'_> {
    fn drop(&mut self) {
        self.lock.release_active(self.id, self.range);
    }
}

pub struct RangeGuardPair<'a> {
    first: RangeGuard<'a>,
    second: RangeGuard<'a>,
}

impl<'a> RangeGuardPair<'a> {
    pub const fn first(&self) -> &RangeGuard<'a> {
        &self.first
    }

    pub const fn second(&self) -> &RangeGuard<'a> {
        &self.second
    }
}

pub struct PendingWriter<'a> {
    lock: &'a RangeLock,
    id: u64,
    range: UserRange,
}

impl<'a> PendingWriter<'a> {
    pub fn try_acquire(self) -> AcquireResult<'a> {
        self.lock.acquire_pending_writer(self)
    }
}

impl Drop for PendingWriter<'_> {
    fn drop(&mut self) {
        self.lock.release_pending_writer(self.id, self.range);
    }
}

/// Simple bounded interval-reservation set for VM_v1 coordination.
///
/// VM_v1_2 leaves the internal RangeLock data structure implementation-defined.
/// This type preserves the declared overlap semantics and writer preference,
/// but it is not the final optimized segment tree/concurrent interval index.
pub struct RangeLock {
    state: SpinMutex<RangeLockState>,
}

impl RangeLock {
    pub const fn new() -> Self {
        Self {
            state: SpinMutex::new(RangeLockState::new()),
        }
    }

    pub fn acquire(&self, range: UserRange, mode: LockMode) -> AcquireResult<'_> {
        let mut state = self.state.lock();
        match mode {
            LockMode::Materializer => {
                if state.materializer_blocked(range) {
                    AcquireResult::WouldBlock(WouldBlock {
                        pending_writer: None,
                    })
                } else {
                    state.acquire_active(self, range, mode)
                }
            }
            LockMode::ExclusiveWriter => {
                if state.writer_blocked(range, None) {
                    state.acquire_pending_writer(self, range)
                } else {
                    state.acquire_active(self, range, mode)
                }
            }
        }
    }

    pub fn acquire_pair(
        &self,
        a: (UserRange, LockMode),
        b: (UserRange, LockMode),
    ) -> AcquirePairResult<'_> {
        let mut state = self.state.lock();
        if state.pair_blocked(a, b) {
            return AcquirePairResult::WouldBlock(WouldBlock {
                pending_writer: None,
            });
        }

        let first = state.reserve_active_slot(a.0, a.1);
        let second = state.reserve_active_slot(b.0, b.1);
        match (first, second) {
            (Some(first_id), Some(second_id)) => AcquirePairResult::Acquired(RangeGuardPair {
                first: RangeGuard {
                    lock: self,
                    id: first_id,
                    range: a.0,
                },
                second: RangeGuard {
                    lock: self,
                    id: second_id,
                    range: b.0,
                },
            }),
            (Some(first_id), None) => {
                state.release_active(first_id, a.0);
                AcquirePairResult::WouldBlock(WouldBlock {
                    pending_writer: None,
                })
            }
            _ => AcquirePairResult::WouldBlock(WouldBlock {
                pending_writer: None,
            }),
        }
    }

    fn acquire_pending_writer<'a>(&'a self, mut pending: PendingWriter<'a>) -> AcquireResult<'a> {
        let mut state = self.state.lock();
        let range = pending.range;
        if state.writer_blocked(range, Some(pending.id)) {
            return AcquireResult::WouldBlock(WouldBlock {
                pending_writer: Some(pending),
            });
        }
        state.release_pending_writer(pending.id, range);
        pending.id = 0;
        core::mem::forget(pending);
        state.acquire_active(self, range, LockMode::ExclusiveWriter)
    }

    fn release_active(&self, id: u64, range: UserRange) {
        self.state.lock().release_active(id, range);
    }

    fn release_pending_writer(&self, id: u64, range: UserRange) {
        if id != 0 {
            self.state.lock().release_pending_writer(id, range);
        }
    }
}

impl Default for RangeLock {
    fn default() -> Self {
        Self::new()
    }
}

const MAX_ACTIVE_RANGES: usize = 16;
const MAX_PENDING_WRITERS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Reservation {
    id: u64,
    range: UserRange,
    mode: LockMode,
}

struct RangeLockState {
    active: ReservationIntervalTree<MAX_ACTIVE_RANGES>,
    pending_writers: ReservationIntervalTree<MAX_PENDING_WRITERS>,
    next_id: u64,
}

impl RangeLockState {
    const fn new() -> Self {
        Self {
            active: ReservationIntervalTree::new(),
            pending_writers: ReservationIntervalTree::new(),
            next_id: 1,
        }
    }

    fn materializer_blocked(&self, range: UserRange) -> bool {
        self.active.any_overlap_where(range, |reservation| {
            reservation.mode == LockMode::ExclusiveWriter
        }) || self.pending_writers.any_overlap_where(range, |_| true)
    }

    fn writer_blocked(&self, range: UserRange, own_pending_id: Option<u64>) -> bool {
        if self.active.any_overlap_where(range, |_| true) {
            return true;
        }

        match self.pending_writers.min_overlap_id_where(range, |_| true) {
            Some(first_pending_id) => match own_pending_id {
                Some(own_id) => first_pending_id < own_id,
                None => true,
            },
            None => false,
        }
    }

    fn pair_blocked(&self, a: (UserRange, LockMode), b: (UserRange, LockMode)) -> bool {
        self.single_blocked_by_pair(a, b) || self.single_blocked_by_pair(b, a)
    }

    fn single_blocked_by_pair(
        &self,
        current: (UserRange, LockMode),
        other: (UserRange, LockMode),
    ) -> bool {
        match current.1 {
            LockMode::Materializer => {
                self.materializer_blocked(current.0)
                    || (other.1 == LockMode::ExclusiveWriter && current.0.overlaps(other.0))
            }
            LockMode::ExclusiveWriter => {
                self.writer_blocked(current.0, None) || current.0.overlaps(other.0)
            }
        }
    }

    fn acquire_active<'a>(
        &mut self,
        lock: &'a RangeLock,
        range: UserRange,
        mode: LockMode,
    ) -> AcquireResult<'a> {
        match self.reserve_active_slot(range, mode) {
            Some(id) => AcquireResult::Acquired(RangeGuard { lock, id, range }),
            None => AcquireResult::WouldBlock(WouldBlock {
                pending_writer: None,
            }),
        }
    }

    fn reserve_active_slot(&mut self, range: UserRange, mode: LockMode) -> Option<u64> {
        let id = self.take_id();
        if self.active.insert(Reservation { id, range, mode }) {
            Some(id)
        } else {
            None
        }
    }

    fn acquire_pending_writer<'a>(
        &mut self,
        lock: &'a RangeLock,
        range: UserRange,
    ) -> AcquireResult<'a> {
        let id = self.take_id();
        if !self.pending_writers.insert(Reservation {
            id,
            range,
            mode: LockMode::ExclusiveWriter,
        }) {
            return AcquireResult::WouldBlock(WouldBlock {
                pending_writer: None,
            });
        }
        AcquireResult::WouldBlock(WouldBlock {
            pending_writer: Some(PendingWriter { lock, id, range }),
        })
    }

    fn release_active(&mut self, id: u64, range: UserRange) {
        self.active.remove(id, range);
    }

    fn release_pending_writer(&mut self, id: u64, range: UserRange) {
        self.pending_writers.remove(id, range);
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReservationKeyOrdering {
    Less,
    Equal,
    Greater,
}

#[derive(Clone, Copy)]
struct ReservationNode {
    reservation: Option<Reservation>,
    left: Option<usize>,
    right: Option<usize>,
    height: i8,
    max_end: usize,
}

impl ReservationNode {
    const fn empty() -> Self {
        Self {
            reservation: None,
            left: None,
            right: None,
            height: 0,
            max_end: 0,
        }
    }

    fn new(reservation: Reservation) -> Self {
        Self {
            reservation: Some(reservation),
            left: None,
            right: None,
            height: 1,
            max_end: reservation.range.end().as_usize(),
        }
    }
}

struct ReservationIntervalTree<const N: usize> {
    root: Option<usize>,
    nodes: [ReservationNode; N],
}

impl<const N: usize> ReservationIntervalTree<N> {
    const fn new() -> Self {
        Self {
            root: None,
            nodes: [const { ReservationNode::empty() }; N],
        }
    }

    fn insert(&mut self, reservation: Reservation) -> bool {
        let Some(slot) = self
            .nodes
            .iter()
            .position(|node| node.reservation.is_none())
        else {
            return false;
        };

        self.nodes[slot] = ReservationNode::new(reservation);
        self.root = Some(match self.root {
            Some(root) => self.insert_node(root, slot),
            None => slot,
        });
        true
    }

    fn remove(&mut self, id: u64, range: UserRange) -> Option<Reservation> {
        let mut removed = None;
        self.root = self.remove_node(self.root, id, range, &mut removed);
        removed
    }

    fn any_overlap_where(&self, range: UserRange, predicate: impl Fn(Reservation) -> bool) -> bool {
        self.any_overlap_from(self.root, range, &predicate)
    }

    fn min_overlap_id_where(
        &self,
        range: UserRange,
        predicate: impl Fn(Reservation) -> bool,
    ) -> Option<u64> {
        self.min_overlap_id_from(self.root, range, &predicate)
    }

    fn insert_node(&mut self, root: usize, inserted: usize) -> usize {
        let inserted_reservation = self.nodes[inserted]
            .reservation
            .expect("inserted node is populated");
        let root_reservation = self.nodes[root]
            .reservation
            .expect("root node is populated");

        match compare_reservation_key(
            inserted_reservation.id,
            inserted_reservation.range,
            root_reservation.id,
            root_reservation.range,
        ) {
            ReservationKeyOrdering::Less => {
                self.nodes[root].left = Some(match self.nodes[root].left {
                    Some(left) => self.insert_node(left, inserted),
                    None => inserted,
                });
            }
            ReservationKeyOrdering::Equal | ReservationKeyOrdering::Greater => {
                self.nodes[root].right = Some(match self.nodes[root].right {
                    Some(right) => self.insert_node(right, inserted),
                    None => inserted,
                });
            }
        }

        self.rebalance(root)
    }

    fn remove_node(
        &mut self,
        root: Option<usize>,
        id: u64,
        range: UserRange,
        removed: &mut Option<Reservation>,
    ) -> Option<usize> {
        let root = root?;
        let root_reservation = self.nodes[root]
            .reservation
            .expect("root node is populated");

        match compare_reservation_key(id, range, root_reservation.id, root_reservation.range) {
            ReservationKeyOrdering::Less => {
                self.nodes[root].left = self.remove_node(self.nodes[root].left, id, range, removed);
            }
            ReservationKeyOrdering::Greater => {
                self.nodes[root].right =
                    self.remove_node(self.nodes[root].right, id, range, removed);
            }
            ReservationKeyOrdering::Equal => {
                *removed = Some(root_reservation);
                return self.remove_root(root);
            }
        }

        Some(self.rebalance(root))
    }

    fn remove_root(&mut self, root: usize) -> Option<usize> {
        match (self.nodes[root].left, self.nodes[root].right) {
            (None, None) => {
                self.nodes[root] = ReservationNode::empty();
                None
            }
            (Some(child), None) | (None, Some(child)) => {
                self.nodes[root] = ReservationNode::empty();
                Some(child)
            }
            (Some(_), Some(right)) => {
                let successor = self.min_node(right);
                let successor_reservation = self.nodes[successor]
                    .reservation
                    .expect("successor node is populated");
                let mut discarded = None;
                self.nodes[root].reservation = Some(successor_reservation);
                self.nodes[root].right = self.remove_node(
                    self.nodes[root].right,
                    successor_reservation.id,
                    successor_reservation.range,
                    &mut discarded,
                );
                Some(self.rebalance(root))
            }
        }
    }

    fn min_node(&self, root: usize) -> usize {
        let mut cursor = root;
        while let Some(left) = self.nodes[cursor].left {
            cursor = left;
        }
        cursor
    }

    fn any_overlap_from(
        &self,
        root: Option<usize>,
        range: UserRange,
        predicate: &impl Fn(Reservation) -> bool,
    ) -> bool {
        let Some(root) = root else {
            return false;
        };
        let node = &self.nodes[root];
        let reservation = node.reservation.expect("tree node is populated");

        if self.subtree_may_overlap(node.left, range) {
            if self.any_overlap_from(node.left, range, predicate) {
                return true;
            }
        }

        if reservation.range.overlaps(range) && predicate(reservation) {
            return true;
        }

        if reservation.range.start().as_usize() < range.end().as_usize() {
            return self.any_overlap_from(node.right, range, predicate);
        }

        false
    }

    fn min_overlap_id_from(
        &self,
        root: Option<usize>,
        range: UserRange,
        predicate: &impl Fn(Reservation) -> bool,
    ) -> Option<u64> {
        let root = root?;
        let node = &self.nodes[root];
        let reservation = node.reservation.expect("tree node is populated");
        let mut best = None;

        if self.subtree_may_overlap(node.left, range) {
            best = min_option_id(best, self.min_overlap_id_from(node.left, range, predicate));
        }

        if reservation.range.overlaps(range) && predicate(reservation) {
            best = min_option_id(best, Some(reservation.id));
        }

        if reservation.range.start().as_usize() < range.end().as_usize() {
            best = min_option_id(best, self.min_overlap_id_from(node.right, range, predicate));
        }

        best
    }

    fn subtree_may_overlap(&self, root: Option<usize>, range: UserRange) -> bool {
        root.is_some_and(|root| self.nodes[root].max_end > range.start().as_usize())
    }

    fn rebalance(&mut self, root: usize) -> usize {
        self.recompute(root);
        let balance = self.balance_factor(root);

        if balance > 1 {
            let left = self.nodes[root]
                .left
                .expect("left-heavy node has left child");
            if self.balance_factor(left) < 0 {
                self.nodes[root].left = Some(self.rotate_left(left));
            }
            return self.rotate_right(root);
        }

        if balance < -1 {
            let right = self.nodes[root]
                .right
                .expect("right-heavy node has right child");
            if self.balance_factor(right) > 0 {
                self.nodes[root].right = Some(self.rotate_right(right));
            }
            return self.rotate_left(root);
        }

        root
    }

    fn rotate_left(&mut self, root: usize) -> usize {
        let new_root = self.nodes[root].right.expect("rotate_left requires right");
        let transferred = self.nodes[new_root].left;
        self.nodes[new_root].left = Some(root);
        self.nodes[root].right = transferred;
        self.recompute(root);
        self.recompute(new_root);
        new_root
    }

    fn rotate_right(&mut self, root: usize) -> usize {
        let new_root = self.nodes[root].left.expect("rotate_right requires left");
        let transferred = self.nodes[new_root].right;
        self.nodes[new_root].right = Some(root);
        self.nodes[root].left = transferred;
        self.recompute(root);
        self.recompute(new_root);
        new_root
    }

    fn recompute(&mut self, index: usize) {
        let reservation = self.nodes[index]
            .reservation
            .expect("tree node is populated");
        let left = self.nodes[index].left;
        let right = self.nodes[index].right;
        self.nodes[index].height = 1 + self.node_height(left).max(self.node_height(right));
        self.nodes[index].max_end = reservation
            .range
            .end()
            .as_usize()
            .max(self.node_max_end(left))
            .max(self.node_max_end(right));
    }

    fn balance_factor(&self, index: usize) -> i8 {
        self.node_height(self.nodes[index].left) - self.node_height(self.nodes[index].right)
    }

    fn node_height(&self, index: Option<usize>) -> i8 {
        index.map_or(0, |index| self.nodes[index].height)
    }

    fn node_max_end(&self, index: Option<usize>) -> usize {
        index.map_or(0, |index| self.nodes[index].max_end)
    }
}

fn compare_reservation_key(
    a_id: u64,
    a_range: UserRange,
    b_id: u64,
    b_range: UserRange,
) -> ReservationKeyOrdering {
    match a_range.start().as_usize().cmp(&b_range.start().as_usize()) {
        core::cmp::Ordering::Less => ReservationKeyOrdering::Less,
        core::cmp::Ordering::Greater => ReservationKeyOrdering::Greater,
        core::cmp::Ordering::Equal => match a_id.cmp(&b_id) {
            core::cmp::Ordering::Less => ReservationKeyOrdering::Less,
            core::cmp::Ordering::Equal => ReservationKeyOrdering::Equal,
            core::cmp::Ordering::Greater => ReservationKeyOrdering::Greater,
        },
    }
}

fn min_option_id(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

struct SpinMutex<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinMutex<T> {}

impl<T> SpinMutex<T> {
    const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    fn lock(&self) -> SpinMutexGuard<'_, T> {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinMutexGuard { mutex: self }
    }
}

struct SpinMutexGuard<'a, T> {
    mutex: &'a SpinMutex<T>,
}

impl<T> core::ops::Deref for SpinMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> core::ops::DerefMut for SpinMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for SpinMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: usize, pages: usize) -> UserRange {
        UserRange::new_aligned(UserVirtAddr(start), pages * USER_PAGE_SIZE).expect("valid range")
    }

    fn acquired(result: AcquireResult<'_>) -> RangeGuard<'_> {
        match result {
            AcquireResult::Acquired(guard) => guard,
            AcquireResult::WouldBlock(_) => panic!("expected acquired"),
        }
    }

    fn pair_acquired(result: AcquirePairResult<'_>) -> RangeGuardPair<'_> {
        match result {
            AcquirePairResult::Acquired(guards) => guards,
            AcquirePairResult::WouldBlock(_) => panic!("expected pair acquired"),
        }
    }

    fn map_reserved(result: MapReserveResult<'_>) -> MapReservation<'_> {
        match result {
            MapReserveResult::Reserved(reservation) => reservation,
            MapReserveResult::WouldBlock(_) => panic!("expected map reservation"),
            MapReserveResult::Err(error) => panic!("unexpected map reserve error: {error:?}"),
        }
    }

    fn map_error(result: MapReserveResult<'_>) -> VmMapError {
        match result {
            MapReserveResult::Reserved(_) => panic!("expected map reserve error"),
            MapReserveResult::WouldBlock(_) => panic!("expected map reserve error, got block"),
            MapReserveResult::Err(error) => error,
        }
    }

    fn would_block(result: AcquireResult<'_>) -> WouldBlock<'_> {
        match result {
            AcquireResult::Acquired(_) => panic!("expected would-block"),
            AcquireResult::WouldBlock(blocked) => blocked,
        }
    }

    #[test]
    fn vm_user_range_rejects_zero_unaligned_and_overflow() {
        assert_eq!(
            UserRange::new_aligned(UserVirtAddr(0), 0),
            Err(UserRangeError::ZeroLength)
        );
        assert_eq!(
            UserRange::new_aligned(UserVirtAddr(1), USER_PAGE_SIZE),
            Err(UserRangeError::Unaligned)
        );
        assert_eq!(
            UserRange::new_aligned(UserVirtAddr(0), USER_PAGE_SIZE - 1),
            Err(UserRangeError::Unaligned)
        );
        assert_eq!(
            UserRange::new_aligned(UserVirtAddr(usize::MAX - 4095), USER_PAGE_SIZE),
            Err(UserRangeError::Overflow)
        );
    }

    #[test]
    fn vm_user_range_iterates_pages_and_counts_them() {
        let pages = range(0x4000, 3);
        assert_eq!(pages.page_count(), 3);
        assert_eq!(pages.iter_pages().len(), 3);

        let mut iter = pages.iter_pages();
        assert_eq!(iter.next(), Some(UserPage(4)));
        assert_eq!(iter.next(), Some(UserPage(5)));
        assert_eq!(iter.next(), Some(UserPage(6)));
        assert_eq!(iter.next(), None);

        assert_eq!(UserVirtAddr(0x4123).containing_page(), UserPage(4));
        assert_eq!(
            UserRange::containing_page(UserVirtAddr(0x4123)),
            Ok(range(0x4000, 1))
        );
        assert_eq!(
            UserRange::containing_page(UserVirtAddr(usize::MAX)),
            Err(UserRangeError::Overflow)
        );
    }

    #[test]
    fn vm_range_lock_conflict_matrix_matches_modes() {
        let lock = RangeLock::new();
        let first = range(0x1000, 2);
        let overlap = range(0x2000, 1);
        let disjoint = range(0x8000, 1);

        let writer = acquired(lock.acquire(first, LockMode::ExclusiveWriter));
        would_block(lock.acquire(overlap, LockMode::ExclusiveWriter));
        would_block(lock.acquire(overlap, LockMode::Materializer));
        let disjoint_writer = acquired(lock.acquire(disjoint, LockMode::ExclusiveWriter));
        drop(disjoint_writer);
        drop(writer);

        let materializer_a = acquired(lock.acquire(first, LockMode::Materializer));
        let materializer_b = acquired(lock.acquire(overlap, LockMode::Materializer));
        would_block(lock.acquire(overlap, LockMode::ExclusiveWriter));
        drop(materializer_b);
        drop(materializer_a);
    }

    #[test]
    fn vm_range_lock_pending_writer_blocks_new_materializers() {
        let lock = RangeLock::new();
        let first = range(0x1000, 1);

        let materializer = acquired(lock.acquire(first, LockMode::Materializer));
        let pending = would_block(lock.acquire(first, LockMode::ExclusiveWriter))
            .pending_writer()
            .expect("blocked writer should declare pending range");

        would_block(lock.acquire(first, LockMode::Materializer));
        drop(materializer);

        let writer = acquired(pending.try_acquire());
        would_block(lock.acquire(first, LockMode::Materializer));
        drop(writer);

        let materializer_after_drop = acquired(lock.acquire(first, LockMode::Materializer));
        drop(materializer_after_drop);
    }

    #[test]
    fn vm_range_lock_overlapping_pending_writers_are_fifo() {
        let lock = RangeLock::new();
        let first = range(0x1000, 1);

        let materializer = acquired(lock.acquire(first, LockMode::Materializer));
        let pending_a = would_block(lock.acquire(first, LockMode::ExclusiveWriter))
            .pending_writer()
            .expect("first writer queues");
        let pending_b = would_block(lock.acquire(first, LockMode::ExclusiveWriter))
            .pending_writer()
            .expect("second writer queues");
        drop(materializer);

        let blocked_b = would_block(pending_b.try_acquire());
        let writer_a = acquired(pending_a.try_acquire());
        drop(writer_a);

        let writer_b = acquired(
            blocked_b
                .pending_writer()
                .expect("second writer remains queued")
                .try_acquire(),
        );
        drop(writer_b);
    }

    #[test]
    fn vm_range_guard_drop_releases_reservation() {
        let lock = RangeLock::new();
        let first = range(0x1000, 1);

        {
            let writer = acquired(lock.acquire(first, LockMode::ExclusiveWriter));
            would_block(lock.acquire(first, LockMode::Materializer));
            drop(writer);
        }

        let materializer = acquired(lock.acquire(first, LockMode::Materializer));
        drop(materializer);
    }

    #[test]
    fn vm_range_lock_acquires_two_ranges_atomically() {
        let lock = RangeLock::new();
        let a = range(0x1000, 1);
        let b = range(0x4000, 1);

        let pair = pair_acquired(lock.acquire_pair(
            (a, LockMode::ExclusiveWriter),
            (b, LockMode::ExclusiveWriter),
        ));
        would_block(lock.acquire(a, LockMode::Materializer));
        would_block(lock.acquire(b, LockMode::Materializer));
        drop(pair);

        let after = acquired(lock.acquire(a, LockMode::Materializer));
        drop(after);
    }

    #[test]
    fn vm_range_lock_tree_removal_clears_only_removed_overlap() {
        let lock = RangeLock::new();
        let low = range(0x1000, 1);
        let middle = range(0x5000, 1);
        let high = range(0x9000, 1);

        let low_writer = acquired(lock.acquire(low, LockMode::ExclusiveWriter));
        let middle_writer = acquired(lock.acquire(middle, LockMode::ExclusiveWriter));
        let high_writer = acquired(lock.acquire(high, LockMode::ExclusiveWriter));

        would_block(lock.acquire(middle, LockMode::Materializer));
        drop(middle_writer);

        let middle_materializer = acquired(lock.acquire(middle, LockMode::Materializer));
        would_block(lock.acquire(low, LockMode::Materializer));
        would_block(lock.acquire(high, LockMode::Materializer));

        drop(middle_materializer);
        drop(low_writer);
        drop(high_writer);
    }

    #[test]
    fn vm_range_lock_tree_keeps_disjoint_reservations_independent() {
        let lock = RangeLock::new();
        let a = range(0x1000, 1);
        let b = range(0x8000, 1);
        let c = range(0x10000, 1);

        let writer_a = acquired(lock.acquire(a, LockMode::ExclusiveWriter));
        let writer_b = acquired(lock.acquire(b, LockMode::ExclusiveWriter));
        let materializer_c = acquired(lock.acquire(c, LockMode::Materializer));

        would_block(lock.acquire(a, LockMode::Materializer));
        would_block(lock.acquire(b, LockMode::Materializer));
        let second_materializer_c = acquired(lock.acquire(c, LockMode::Materializer));

        drop(second_materializer_c);
        drop(materializer_c);
        drop(writer_b);
        drop(writer_a);
    }

    #[test]
    fn vm_range_lock_tree_pending_fifo_is_range_scoped() {
        let lock = RangeLock::new();
        let first = range(0x2000, 1);
        let disjoint = range(0xa000, 1);

        let materializer = acquired(lock.acquire(first, LockMode::Materializer));
        let pending_a = would_block(lock.acquire(first, LockMode::ExclusiveWriter))
            .pending_writer()
            .expect("first overlapping writer queues");
        let disjoint_writer = acquired(lock.acquire(disjoint, LockMode::ExclusiveWriter));
        let pending_b = would_block(lock.acquire(first, LockMode::ExclusiveWriter))
            .pending_writer()
            .expect("second overlapping writer queues");

        drop(materializer);

        let blocked_b = would_block(pending_b.try_acquire());
        let writer_a = acquired(pending_a.try_acquire());
        drop(writer_a);

        let writer_b = acquired(
            blocked_b
                .pending_writer()
                .expect("second overlapping writer remains queued")
                .try_acquire(),
        );

        drop(writer_b);
        drop(disjoint_writer);
    }

    #[test]
    fn vm_range_lock_tree_active_writer_blocks_materializer_overlap_only() {
        let lock = RangeLock::new();
        let writer_range = range(0x7000, 2);
        let overlap = range(0x8000, 1);
        let disjoint = range(0xb000, 1);

        let writer = acquired(lock.acquire(writer_range, LockMode::ExclusiveWriter));

        would_block(lock.acquire(overlap, LockMode::Materializer));
        let disjoint_materializer = acquired(lock.acquire(disjoint, LockMode::Materializer));

        drop(disjoint_materializer);
        drop(writer);
    }

    #[test]
    fn vm_entry_split_for_unmap_preserves_survivors_and_offsets() {
        let entry = VmEntry::new(
            range(0x1000, 4),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::MockPage { id: 7, offset: 10 },
        );

        let rewrite = entry
            .split_for_unmap(range(0x2000, 2))
            .expect("hole is inside entry");

        assert_eq!(
            rewrite.before.expect("left survivor"),
            VmEntry::new(
                range(0x1000, 1),
                Prot::READ_WRITE,
                VmEntryFlags::PRIVATE,
                VmBackingDraft::MockPage { id: 7, offset: 10 },
            )
        );
        assert_eq!(
            rewrite.after.expect("right survivor"),
            VmEntry::new(
                range(0x4000, 1),
                Prot::READ_WRITE,
                VmEntryFlags::PRIVATE,
                VmBackingDraft::MockPage {
                    id: 7,
                    offset: 10 + (3 * USER_PAGE_SIZE) as u64,
                },
            )
        );
        assert_eq!(rewrite.target, None);
    }

    #[test]
    fn vm_entry_split_for_protect_rewrites_middle_only() {
        let entry = VmEntry::new(
            range(0x1000, 3),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBackingDraft::PrivateAnon,
        );

        let rewrite = entry
            .split_for_protect(range(0x2000, 1), Prot::READ)
            .expect("target is inside entry");

        assert_eq!(rewrite.before.expect("before").prot, Prot::READ_WRITE);
        assert_eq!(rewrite.target.expect("target").prot, Prot::READ);
        assert_eq!(rewrite.after.expect("after").prot, Prot::READ_WRITE);
        assert_eq!(
            entry.split_for_protect(range(0x4000, 1), Prot::READ),
            Err(VmEntryError::RangeNotContained)
        );
    }

    #[test]
    fn vm_address_space_map_reservation_publishes_recipe_on_commit() {
        let aspace = AddressSpace::new();
        let entry = VmEntry::new(
            range(0x4000, 2),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );

        let reservation = map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree));
        assert_eq!(aspace.lookup(UserVirtAddr(0x4000)), None);
        assert_eq!(aspace.stats(), AddressSpaceStats::default());

        let commit = reservation.commit().expect("map commit");

        assert_eq!(commit.changed_pages, 2);
        assert_eq!(aspace.lookup(UserVirtAddr(0x4000)), Some(entry));
        assert_eq!(aspace.lookup(UserVirtAddr(0x5fff)), Some(entry));
        assert_eq!(aspace.lookup(UserVirtAddr(0x6000)), None);
        assert_eq!(
            aspace.stats(),
            AddressSpaceStats {
                recipe_count: 1,
                vm_size: 2 * USER_PAGE_SIZE,
            }
        );
    }

    #[test]
    fn vm_address_space_dropped_map_reservation_rolls_back() {
        let aspace = AddressSpace::new();
        let entry = VmEntry::new(
            range(0x8000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::None,
        );

        let reservation = map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree));
        drop(reservation);

        assert_eq!(aspace.lookup(UserVirtAddr(0x8000)), None);
        assert_eq!(aspace.stats(), AddressSpaceStats::default());
    }

    #[test]
    fn vm_address_space_rejects_nonfixed_overlap() {
        let aspace = AddressSpace::new();
        let first = VmEntry::new(
            range(0x1000, 2),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(first, MapPlacement::RequireFree))
            .commit()
            .expect("initial map");

        let overlap = VmEntry::new(
            range(0x2000, 1),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );

        assert_eq!(
            map_error(aspace.reserve_map(overlap, MapPlacement::RequireFree)),
            VmMapError::AlreadyMapped
        );
        assert_eq!(aspace.lookup(UserVirtAddr(0x1000)), Some(first));
    }

    #[test]
    fn vm_address_space_fixed_map_replaces_overlap_and_preserves_survivors() {
        let aspace = AddressSpace::new();
        let original = VmEntry::new(
            range(0x1000, 4),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBackingDraft::MockPage { id: 9, offset: 0 },
        );
        map_reserved(aspace.reserve_map(original, MapPlacement::RequireFree))
            .commit()
            .expect("initial map");

        let replacement = VmEntry::new(
            range(0x2000, 2),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );
        let commit = map_reserved(aspace.reserve_map(replacement, MapPlacement::FixedReplace))
            .commit()
            .expect("fixed replace");

        assert_eq!(commit.changed_pages, 4);
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x1000)),
            Some(VmEntry::new(
                range(0x1000, 1),
                Prot::READ_WRITE,
                VmEntryFlags::SHARED,
                VmBackingDraft::MockPage { id: 9, offset: 0 },
            ))
        );
        assert_eq!(aspace.lookup(UserVirtAddr(0x2000)), Some(replacement));
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x4000)),
            Some(VmEntry::new(
                range(0x4000, 1),
                Prot::READ_WRITE,
                VmEntryFlags::SHARED,
                VmBackingDraft::MockPage {
                    id: 9,
                    offset: (3 * USER_PAGE_SIZE) as u64,
                },
            ))
        );
        assert_eq!(
            aspace.stats(),
            AddressSpaceStats {
                recipe_count: 3,
                vm_size: 4 * USER_PAGE_SIZE,
            }
        );
    }

    #[test]
    fn vm_address_space_unmap_splits_recipe_and_updates_stats() {
        let aspace = AddressSpace::new();
        let original = VmEntry::new(
            range(0x1000, 4),
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(original, MapPlacement::RequireFree))
            .commit()
            .expect("initial map");

        let commit = aspace.unmap(range(0x2000, 2)).expect("unmap commit");

        assert_eq!(commit.changed_pages, 2);
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x1000)).expect("left").range,
            range(0x1000, 1)
        );
        assert_eq!(aspace.lookup(UserVirtAddr(0x2000)), None);
        assert_eq!(aspace.lookup(UserVirtAddr(0x3000)), None);
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x4000)).expect("right").range,
            range(0x4000, 1)
        );
        assert_eq!(
            aspace.stats(),
            AddressSpaceStats {
                recipe_count: 2,
                vm_size: 2 * USER_PAGE_SIZE,
            }
        );
    }

    #[test]
    fn vm_address_space_protect_rewrites_only_declared_range() {
        let aspace = AddressSpace::new();
        let original = VmEntry::new(
            range(0x1000, 3),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBackingDraft::MockPage { id: 11, offset: 0 },
        );
        map_reserved(aspace.reserve_map(original, MapPlacement::RequireFree))
            .commit()
            .expect("initial map");

        let commit = aspace
            .protect(range(0x2000, 1), Prot::READ)
            .expect("protect commit");

        assert_eq!(commit.changed_pages, 1);
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x1000)).expect("left").prot,
            Prot::READ_WRITE
        );
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x2000)).expect("target").prot,
            Prot::READ
        );
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x3000)).expect("right").prot,
            Prot::READ_WRITE
        );
        assert_eq!(
            aspace.protect(range(0x8000, 1), Prot::READ),
            Err(VmMapError::MissingMapping)
        );
    }

    #[test]
    fn vm_address_space_finds_first_gap_inside_search_window() {
        let aspace = AddressSpace::new();
        let first = VmEntry::new(
            range(0x1000, 2),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );
        let second = VmEntry::new(
            range(0x5000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(first, MapPlacement::RequireFree))
            .commit()
            .expect("first map");
        map_reserved(aspace.reserve_map(second, MapPlacement::RequireFree))
            .commit()
            .expect("second map");

        assert_eq!(
            aspace.find_free_range(range(0x1000, 6), 2),
            Some(range(0x3000, 2))
        );
        assert_eq!(aspace.find_free_range(range(0x1000, 6), 3), None);
    }

    #[test]
    fn vm_address_space_lists_recipes_overlapping_declared_range_in_order() {
        let aspace = AddressSpace::new();
        let left = VmEntry::new(
            range(0x1000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );
        let right = VmEntry::new(
            range(0x3000, 2),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBackingDraft::MockPage { id: 5, offset: 0 },
        );
        map_reserved(aspace.reserve_map(left, MapPlacement::RequireFree))
            .commit()
            .expect("left map");
        map_reserved(aspace.reserve_map(right, MapPlacement::RequireFree))
            .commit()
            .expect("right map");

        assert_eq!(
            aspace.recipes_overlapping(range(0x1000, 3)),
            alloc::vec![left, right]
        );
    }

    #[test]
    fn vm_fault_resolution_requires_authoritative_recipe_and_permissions() {
        let aspace = AddressSpace::new();
        let entry = VmEntry::new(
            range(0x4000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("map");

        let outcome = aspace
            .resolve_fault(VmFault::new(UserVirtAddr(0x4008), AccessMode::Read))
            .expect("read fault resolves");

        assert_eq!(outcome.page_range, range(0x4000, 1));
        assert_eq!(outcome.entry, entry);
        assert!(outcome.pmap_materialization_deferred);
        assert_eq!(
            aspace.resolve_fault(VmFault::new(UserVirtAddr(0x4008), AccessMode::Write)),
            Err(VmFaultError::ProtectionViolation)
        );
        assert_eq!(
            aspace.resolve_fault(VmFault::new(UserVirtAddr(0x8000), AccessMode::Read)),
            Err(VmFaultError::NoRecipe)
        );
    }

    #[test]
    fn vm_fault_resolution_waits_behind_overlapping_writer() {
        let aspace = AddressSpace::new();
        let entry = VmEntry::new(
            range(0x1000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("map");
        let _writer = acquired(
            aspace
                .range_lock()
                .acquire(range(0x1000, 1), LockMode::ExclusiveWriter),
        );

        assert_eq!(
            aspace.resolve_fault(VmFault::new(UserVirtAddr(0x1000), AccessMode::Read)),
            Err(VmFaultError::WouldBlock)
        );
    }

    #[test]
    fn vm_fault_materializes_pagebacked_anon_page_from_recipe_offset() {
        let aspace = AddressSpace::new();
        let entry = VmEntry::new(
            range(0x2000, 2),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBackingDraft::PageBacked {
                container_id: 7,
                offset: USER_PAGE_SIZE as u64,
            },
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("map");
        let mut pc = crate::page_backed::PageContainer::new(
            crate::page_backed::PageContainerKind::Anon {
                swap_policy: crate::page_backed::AnonSwapPolicy::Reclaimable,
            },
            4,
        );

        let outcome = aspace
            .resolve_fault(VmFault::new(UserVirtAddr(0x3000), AccessMode::Write))
            .expect("fault resolves");
        let materialized = outcome
            .materialize_pagebacked_anon(&mut pc)
            .expect("pagebacked materialization");

        assert_eq!(
            materialized.page_index,
            crate::page_backed::PageIndex::new(2)
        );
        assert!(materialized.page.newly_installed);
        assert!(materialized.page.dirty);
        assert_eq!(
            pc.lookup(crate::page_backed::PageIndex::new(2)),
            Some(materialized.page.frame)
        );
    }

    #[test]
    fn vm_fault_materialization_rejects_non_pagebacked_recipe() {
        let aspace = AddressSpace::new();
        let entry = VmEntry::new(
            range(0x4000, 1),
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );
        map_reserved(aspace.reserve_map(entry, MapPlacement::RequireFree))
            .commit()
            .expect("map");
        let mut pc = crate::page_backed::PageContainer::new(
            crate::page_backed::PageContainerKind::Anon {
                swap_policy: crate::page_backed::AnonSwapPolicy::Reclaimable,
            },
            1,
        );

        let outcome = aspace
            .resolve_fault(VmFault::new(UserVirtAddr(0x4000), AccessMode::Read))
            .expect("fault resolves");

        assert_eq!(
            outcome.materialize_pagebacked_anon(&mut pc),
            Err(VmFaultError::BackingMismatch)
        );
    }

    #[test]
    fn vm_map_script_places_nonfixed_mapping_in_first_recipe_gap() {
        let aspace = AddressSpace::new();
        map_reserved(aspace.reserve_map(
            VmEntry::new(
                range(0x1000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBackingDraft::PrivateAnon,
            ),
            MapPlacement::RequireFree,
        ))
        .commit()
        .expect("seed left");
        map_reserved(aspace.reserve_map(
            VmEntry::new(
                range(0x4000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBackingDraft::PrivateAnon,
            ),
            MapPlacement::RequireFree,
        ))
        .commit()
        .expect("seed right");

        let outcome = aspace
            .map_script(VmMapRequest::anywhere(
                range(0x1000, 5),
                2,
                Prot::READ_WRITE,
                VmEntryFlags::PRIVATE,
                VmBackingDraft::PrivateAnon,
            ))
            .expect("map into gap");

        assert_eq!(outcome.range, range(0x2000, 2));
        assert_eq!(outcome.commit.changed_pages, 2);
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x2000)).expect("mapped").prot,
            Prot::READ_WRITE
        );
        assert_eq!(
            aspace.map_script(VmMapRequest::anywhere(
                range(0x1000, 5),
                2,
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBackingDraft::PrivateAnon,
            )),
            Err(VmMapError::NoFreeRange)
        );
    }

    #[test]
    fn vm_map_script_fixed_replace_uses_declared_range() {
        let aspace = AddressSpace::new();
        let original = VmEntry::new(
            range(0x1000, 3),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBackingDraft::MockPage { id: 33, offset: 0 },
        );
        map_reserved(aspace.reserve_map(original, MapPlacement::RequireFree))
            .commit()
            .expect("seed");

        let replacement = VmMapRequest::fixed(
            range(0x2000, 1),
            MapPlacement::FixedReplace,
            Prot::READ,
            VmEntryFlags::PRIVATE,
            VmBackingDraft::PrivateAnon,
        );
        let outcome = aspace.map_script(replacement).expect("fixed replace");

        assert_eq!(outcome.range, range(0x2000, 1));
        assert_eq!(outcome.commit.changed_pages, 2);
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x1000)).expect("left").range,
            range(0x1000, 1)
        );
        assert_eq!(
            aspace
                .lookup(UserVirtAddr(0x2000))
                .expect("replacement")
                .prot,
            Prot::READ
        );
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x3000)).expect("right").range,
            range(0x3000, 1)
        );
    }

    #[test]
    fn vm_remap_script_moves_disjoint_range_and_preserves_source_survivors() {
        let aspace = AddressSpace::new();
        let original = VmEntry::new(
            range(0x1000, 4),
            Prot::READ_WRITE,
            VmEntryFlags::SHARED,
            VmBackingDraft::MockPage { id: 44, offset: 0 },
        );
        map_reserved(aspace.reserve_map(original, MapPlacement::RequireFree))
            .commit()
            .expect("seed");

        let outcome = aspace
            .remap_script(VmRemapRequest::new(range(0x2000, 2), range(0x8000, 2)))
            .expect("remap");

        assert_eq!(outcome.old_range, range(0x2000, 2));
        assert_eq!(outcome.new_range, range(0x8000, 2));
        assert_eq!(outcome.commit.changed_pages, 4);
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x1000)).expect("left").range,
            range(0x1000, 1)
        );
        assert_eq!(aspace.lookup(UserVirtAddr(0x2000)), None);
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x4000)).expect("right").range,
            range(0x4000, 1)
        );
        assert_eq!(
            aspace.lookup(UserVirtAddr(0x8000)),
            Some(VmEntry::new(
                range(0x8000, 2),
                Prot::READ_WRITE,
                VmEntryFlags::SHARED,
                VmBackingDraft::MockPage {
                    id: 44,
                    offset: USER_PAGE_SIZE as u64,
                },
            ))
        );
    }

    #[test]
    fn vm_remap_script_rejects_overlapping_or_occupied_destination() {
        let aspace = AddressSpace::new();
        map_reserved(aspace.reserve_map(
            VmEntry::new(
                range(0x1000, 4),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBackingDraft::PrivateAnon,
            ),
            MapPlacement::RequireFree,
        ))
        .commit()
        .expect("seed old");
        map_reserved(aspace.reserve_map(
            VmEntry::new(
                range(0x8000, 1),
                Prot::READ,
                VmEntryFlags::PRIVATE,
                VmBackingDraft::PrivateAnon,
            ),
            MapPlacement::RequireFree,
        ))
        .commit()
        .expect("seed dest");

        assert_eq!(
            aspace.remap_script(VmRemapRequest::new(range(0x1000, 2), range(0x2000, 2))),
            Err(VmMapError::InvalidRange)
        );
        assert_eq!(
            aspace.remap_script(VmRemapRequest::new(range(0x1000, 1), range(0x8000, 1))),
            Err(VmMapError::AlreadyMapped)
        );
    }
}
