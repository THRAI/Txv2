//! VM public value vocabulary.

use crate::page_backed::{
    MaterializeAccess, MaterializedPage, MaterializedPagePin, PageCacheError, PageContainer,
    PageIndex,
};
use step_engine::page_allocator::{self, ZeroPolicy};
use tx_substrate::zone::Cap;

use super::private::{
    PrivateFrame, PrivateFrameIdentity, PrivateFrameSnapshot, PrivateFrameState, PrivatePageError,
    PrivatePageSet, VmPageOff,
};
use crate::vm::VmPmapError;
use crate::vm::adapter::step_engine::{self as step_engine};

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

/// V1 conservative user-space top used by VM operations that must
/// serialize against all other VM operations on an `AddressSpace` (fork,
/// exec). Sized at 256 GiB which fits inside Sv39's 256 GiB user-space
/// half and inside Sv48's 128 TiB user-space half. Revisit when the
/// per-platform user VA cap is finalized.
pub const FULL_USER_V1_TOP: usize = 1 << 38;

impl UserRange {
    /// Full user-space range covering `[0, FULL_USER_V1_TOP)` for
    /// operations that must serialize against every concurrent VM
    /// operation on an AddressSpace (fork, exec).
    pub fn full_user_v1() -> Self {
        Self::new_aligned(UserVirtAddr(0), FULL_USER_V1_TOP).expect("full user v1 range")
    }

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

    pub const fn without_write(self) -> Self {
        Self {
            read: self.read,
            write: false,
            execute: self.execute,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VmBacking {
    None,
    PrivateAnon,
    Page { pc: Cap<PageContainer>, offset: u64 },
}

impl VmBacking {
    fn at_range_offset(&self, delta: usize) -> Result<Self, VmEntryError> {
        match self {
            Self::Page { pc, offset } => Ok(Self::Page {
                pc: pc.clone(),
                offset: offset
                    .checked_add(
                        u64::try_from(delta).map_err(|_| VmEntryError::BackingOffsetOverflow)?,
                    )
                    .ok_or(VmEntryError::BackingOffsetOverflow)?,
            }),
            other => Ok(other.clone()),
        }
    }
}

/// Per-VMA userfaultfd registration tag (PR-10 phase 3).
///
/// When `UFFDIO_REGISTER` succeeds against a VMA the shim records the
/// owning ufd's id and the mode bits on this tag. The phase 3 contract
/// is **read-only by the VM fault path** — the existence of this field
/// on a `VmEntry` does not yet alter `fault_script` behaviour
/// (that lands in phase 4). Phase 3 ships only the tag so a phase-4
/// branch can be added with no further structural changes.
///
/// Spec:
/// - `docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`
///   §3.6 (fault-path interception sketch), §6 row P-10.3.
/// - `docs/Txv3/05_DELEGATE_v1.md` §8.1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UfdRegistration {
    /// The owning ufd's stable id (also the `endpoint_marker` later
    /// phases pass to `DelegateRegistry::install_request`). Matches
    /// `UserfaultFd::ufd_id()` of the cap installed in the fd table.
    pub ufd_id: u64,
    /// Mode bits from `struct uffdio_register { mode }`. Phase 3 only
    /// accepts `UFFDIO_REGISTER_MODE_MISSING` (the most common Linux
    /// mode); WP / MINOR are reserved for follow-up phases.
    pub mode: u64,
}

#[derive(Clone, Debug)]
pub struct VmEntry {
    pub range: UserRange,
    pub prot: Prot,
    pub flags: VmEntryFlags,
    pub backing: VmBacking,
    /// Optional userfaultfd registration tag (PR-10 phase 3).
    ///
    /// `None` for every VMA built before phase 3 wiring or by callers
    /// that have not driven `UFFDIO_REGISTER`. Phase 3 sets this to
    /// `Some` when the shim's `step_uffdio_register` arm covers the
    /// VMA's range; phase 4's fault-path branch will read this tag
    /// before invoking `materialize_pagebacked` and substitute the
    /// `OnAgent` yield when present.
    pub ufd_registration: Option<UfdRegistration>,
    /// Per-mapping private-CoW page set. `None` for shared mappings
    /// and for private mappings that have not yet allocated one.
    /// Populated by the mmap path for private mappings and by the
    /// fork path for the child. See
    /// `docs/progress/decisions/2026-05-12-pc-cow-implementation-plan.md`.
    /// Excluded from `PartialEq`: range/prot/flags/backing identify
    /// the VMA semantically, and the recipe tree's split/republish
    /// already detects identity changes via `range`. Skipping
    /// `private` here keeps `VmEntry::new(...) == lookup(...)`
    /// behaviour stable across the auto-allocated Cap.
    pub private: Option<Cap<PrivatePageSet>>,
}

impl PartialEq for VmEntry {
    fn eq(&self, other: &Self) -> bool {
        self.range == other.range
            && self.prot == other.prot
            && self.flags == other.flags
            && self.backing == other.backing
            && self.ufd_registration == other.ufd_registration
    }
}

impl Eq for VmEntry {}

impl VmEntry {
    /// Construct a VmEntry without a userfaultfd registration tag and
    /// no private CoW set. Existing callers stay source-compatible.
    pub fn new(range: UserRange, prot: Prot, flags: VmEntryFlags, backing: VmBacking) -> Self {
        Self {
            range,
            prot,
            flags,
            backing,
            ufd_registration: None,
            private: None,
        }
    }

    /// Builder helper: return a clone of `self` with `ufd_registration`
    /// replaced by `tag`. Used by `step_uffdio_register` to tag each
    /// VMA in the registered range without bypassing the EBR-published
    /// recipe-tree contract.
    pub fn with_ufd_registration(mut self, tag: Option<UfdRegistration>) -> Self {
        self.ufd_registration = tag;
        self
    }

    /// Builder helper: return a clone of `self` with `private` replaced
    /// by `set`. Used by the mmap path on MAP_PRIVATE construction and
    /// by `fork_aspace` when populating the child.
    pub fn with_private(mut self, set: Option<Cap<PrivatePageSet>>) -> Self {
        self.private = set;
        self
    }

    pub fn split_for_unmap(&self, hole: UserRange) -> Result<VmEntryRewrite, VmEntryError> {
        self.split_rewrite(hole, None)
    }

    pub fn split_for_protect(
        &self,
        target: UserRange,
        prot: Prot,
    ) -> Result<VmEntryRewrite, VmEntryError> {
        self.split_rewrite(target, Some(prot))
    }

    fn split_rewrite(
        &self,
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
        &self,
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
        // Slice the private CoW set for this sub-range. Each sub-entry
        // owns a fresh Cap<PrivatePageSet> whose entries are rebased
        // to the new range start. The parent set is left intact;
        // recipe rewriters drop the parent VmEntry after publishing
        // the sub-entries, which releases the parent Cap.
        let private = match &self.private {
            Some(parent_set) => {
                let parent_start_page = self.range.start.0 / USER_PAGE_SIZE;
                let sub_start_page = start.0 / USER_PAGE_SIZE;
                let sub_end_page = end.0 / USER_PAGE_SIZE;
                let rebase_delta = (sub_start_page - parent_start_page) as u64;
                let sliced = parent_set
                    .split(
                        VmPageOff(rebase_delta),
                        VmPageOff((sub_end_page - parent_start_page) as u64),
                        rebase_delta,
                    )
                    .map_err(VmEntryError::Private)?;
                Some(sliced)
            }
            None => None,
        };
        Ok(Self {
            range,
            prot,
            flags: self.flags,
            backing: self.backing.at_range_offset(delta)?,
            // Userfaultfd tag is per-VMA and inherited verbatim across
            // split-for-unmap / split-for-protect rewrites. Phase 3
            // does not yet split the registered range on partial
            // overlap — the shim rejects partial overlaps with
            // `-EINVAL` so every sub-entry built here carries the
            // same tag.
            ufd_registration: self.ufd_registration,
            private,
        })
    }

    /// Page-offset of `page_addr` relative to this entry's range start.
    /// Caller ensures `page_addr` is in `self.range`. Returns the offset
    /// in pages, suitable for keying `PrivatePageSet`.
    pub fn page_offset_of(&self, page_addr: UserVirtAddr) -> Option<VmPageOff> {
        if !self.range.contains_addr(page_addr) {
            return None;
        }
        let delta = page_addr.0.checked_sub(self.range.start.0)?;
        Some(VmPageOff((delta / USER_PAGE_SIZE) as u64))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
    Private(PrivatePageError),
}

impl From<PrivatePageError> for VmEntryError {
    fn from(error: PrivatePageError) -> Self {
        Self::Private(error)
    }
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
    Pmap(VmPmapError),
    Private(PrivatePageError),
}

impl From<PrivatePageError> for VmMapError {
    fn from(error: PrivatePageError) -> Self {
        Self::Private(error)
    }
}

impl From<VmPmapError> for VmMapError {
    fn from(value: VmPmapError) -> Self {
        Self::Pmap(value)
    }
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmMapRequest {
    pub target: VmMapTarget,
    pub prot: Prot,
    pub flags: VmEntryFlags,
    pub backing: VmBacking,
}

impl VmMapRequest {
    pub fn anywhere(
        window: UserRange,
        page_count: usize,
        prot: Prot,
        flags: VmEntryFlags,
        backing: VmBacking,
    ) -> Self {
        Self {
            target: VmMapTarget::Anywhere { window, page_count },
            prot,
            flags,
            backing,
        }
    }

    pub fn fixed(
        range: UserRange,
        placement: MapPlacement,
        prot: Prot,
        flags: VmEntryFlags,
        backing: VmBacking,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmFaultOutcome {
    pub page_range: UserRange,
    pub entry: VmEntry,
    pub access: AccessMode,
    pub pmap_materialization_deferred: bool,
}

impl VmFaultOutcome {
    pub fn materialize_pagebacked_anon(&self) -> Result<VmFaultMaterialization, VmFaultError> {
        if !matches!(self.entry.backing, VmBacking::Page { .. }) {
            return Err(VmFaultError::BackingMismatch);
        }
        self.materialize_pagebacked()
    }

    pub fn materialize_pagebacked(&self) -> Result<VmFaultMaterialization, VmFaultError> {
        // SHARED mappings: passthrough materialize from backing — no
        // private-page consult.
        if self.entry.flags.shared {
            return match &self.entry.backing {
                VmBacking::Page { pc, .. } => self.materialize_page_shared(pc),
                VmBacking::PrivateAnon | VmBacking::None => Err(VmFaultError::BackingMismatch),
            };
        }
        // PRIVATE mappings: consult `entry.private` first, then fall
        // back to the backing on miss. Write-fault on miss CoWs into a
        // fresh private frame and publishes via `install_if_absent` on
        // the per-VmEntry `PrivatePageSet`; read-fault on miss
        // installs an RO PTE pointing at the backing frame so concurrent
        // readers can share it.
        self.materialize_private()
    }

    fn materialize_page_shared(
        &self,
        pc: &Cap<PageContainer>,
    ) -> Result<VmFaultMaterialization, VmFaultError> {
        let page_index = self.backing_page_index()?;
        let access_byte = page_index
            .as_u64()
            .checked_mul(USER_PAGE_SIZE as u64)
            .ok_or(VmFaultError::BackingOffsetOverflow)?;
        if access_byte >= pc.size_bytes() {
            return Err(VmFaultError::PageBeyondSize);
        }
        let access = match self.access {
            AccessMode::Write => MaterializeAccess::Write,
            _ => MaterializeAccess::Read,
        };
        let page = pc
            .materialize_anon(page_index, access)
            .map_err(VmFaultError::PageCache)?;
        Ok(VmFaultMaterialization {
            backing: VmFaultMaterializationBacking::PageBacked,
            page_index,
            page,
            publish_prot: self.entry.prot,
            replace_existing: false,
            pmap_materialization_deferred: self.pmap_materialization_deferred,
        })
    }

    fn materialize_private(&self) -> Result<VmFaultMaterialization, VmFaultError> {
        let backing_kind = match &self.entry.backing {
            VmBacking::Page { .. } => VmFaultMaterializationBacking::PageBacked,
            VmBacking::PrivateAnon => VmFaultMaterializationBacking::PrivateAnon,
            VmBacking::None => return Err(VmFaultError::BackingMismatch),
        };
        let page_index = match backing_kind {
            VmFaultMaterializationBacking::PageBacked => self.backing_page_index()?,
            VmFaultMaterializationBacking::PrivateAnon => self.private_anon_page_index()?,
        };
        let page_off = self.private_page_off()?;
        let write_fault = self.access == AccessMode::Write;

        // 1. Hit in `entry.private` → install PTE pointing at the
        //    stored private frame (RO for reads regardless of state,
        //    RW for write+Exclusive, alloc/copy/replace_if_match for
        //    write+SharedCow).
        if let Some(set) = &self.entry.private {
            if let Some(snap) = set.lookup(page_off) {
                return self.publish_existing_private(
                    snap,
                    write_fault,
                    set,
                    page_off,
                    page_index,
                    backing_kind,
                );
            }
        }

        // 2. Miss. For reads we install a RO PTE pointing at the
        //    backing frame (PC frame or zero frame) — concurrent
        //    readers across many mappings can share it. For writes
        //    we materialize the backing for byte-copy source, then
        //    allocate a fresh private frame, copy, and install into
        //    `entry.private` if present.
        if !write_fault {
            return self.materialize_private_read_miss(backing_kind, page_index);
        }
        self.materialize_private_write_miss(backing_kind, page_index, page_off)
    }

    fn publish_existing_private(
        &self,
        snap: PrivateFrameSnapshot,
        write_fault: bool,
        set: &Cap<PrivatePageSet>,
        page_off: VmPageOff,
        page_index: PageIndex,
        backing_kind: VmFaultMaterializationBacking,
    ) -> Result<VmFaultMaterialization, VmFaultError> {
        if !write_fault {
            // Read fault on hit: install RO PTE pointing at the
            // private frame. Acquiring a MapPin is sufficient because
            // the PrivatePageSet still holds a CachePin on this frame.
            let map_pin = page_allocator::acquire_map_pin(snap.ppn).map_err(page_alloc_error)?;
            return Ok(VmFaultMaterialization {
                backing: backing_kind,
                page_index,
                page: MaterializedPage {
                    ppn: snap.ppn,
                    map_pin: MaterializedPagePin::Allocated(map_pin),
                    newly_installed: false,
                    dirty: false,
                },
                publish_prot: self.entry.prot.without_write(),
                replace_existing: false,
                pmap_materialization_deferred: self.pmap_materialization_deferred,
            });
        }
        match snap.state {
            PrivateFrameState::Exclusive => {
                // Write fault on exclusive private page: install RW
                // PTE pointing at the existing private frame.
                let map_pin =
                    page_allocator::acquire_map_pin(snap.ppn).map_err(page_alloc_error)?;
                Ok(VmFaultMaterialization {
                    backing: backing_kind,
                    page_index,
                    page: MaterializedPage {
                        ppn: snap.ppn,
                        map_pin: MaterializedPagePin::Allocated(map_pin),
                        newly_installed: false,
                        dirty: true,
                    },
                    publish_prot: self.entry.prot,
                    replace_existing: true,
                    pmap_materialization_deferred: self.pmap_materialization_deferred,
                })
            }
            PrivateFrameState::SharedCow => {
                // Write fault on shared-CoW: allocate a fresh private
                // frame, copy from the shared frame, replace_if_match
                // in `entry.private`. Loser of the CAS retries via the
                // caller (the fault_script's outer loop re-observes
                // recipe and re-runs materialize); we surface that as
                // `WouldBlock` so the loop re-acquires the Materializer
                // reservation and tries again.
                let new = allocate_private_materialized_page_from_source(snap.ppn, true)?;
                let new_ppn = new.ppn;
                let cache_pin =
                    page_allocator::acquire_cache_pin(new_ppn).map_err(page_alloc_error)?;
                let frame = PrivateFrame::new(new_ppn, PrivateFrameState::Exclusive, cache_pin);
                let expected = PrivateFrameIdentity {
                    ppn: snap.ppn,
                    state: PrivateFrameState::SharedCow,
                };
                match set.replace_if_match(page_off, expected, frame) {
                    Ok(_) => Ok(VmFaultMaterialization {
                        backing: backing_kind,
                        page_index,
                        page: new,
                        publish_prot: self.entry.prot,
                        replace_existing: true,
                        pmap_materialization_deferred: self.pmap_materialization_deferred,
                    }),
                    Err(_) => {
                        // The CAS lost (concurrent writer raced). Drop
                        // our new page (releases pin) and ask the fault
                        // script to retry from the top.
                        drop(new);
                        Err(VmFaultError::WouldBlock)
                    }
                }
            }
        }
    }

    fn materialize_private_read_miss(
        &self,
        backing_kind: VmFaultMaterializationBacking,
        page_index: PageIndex,
    ) -> Result<VmFaultMaterialization, VmFaultError> {
        let page = match (&self.entry.backing, backing_kind) {
            (VmBacking::Page { pc, .. }, VmFaultMaterializationBacking::PageBacked) => {
                let access_byte = page_index
                    .as_u64()
                    .checked_mul(USER_PAGE_SIZE as u64)
                    .ok_or(VmFaultError::BackingOffsetOverflow)?;
                if access_byte >= pc.size_bytes() {
                    return Err(VmFaultError::PageBeyondSize);
                }
                pc.materialize_anon(page_index, MaterializeAccess::Read)
                    .map_err(VmFaultError::PageCache)?
            }
            (VmBacking::PrivateAnon, VmFaultMaterializationBacking::PrivateAnon) => {
                materialize_zero_frame()?
            }
            _ => return Err(VmFaultError::BackingMismatch),
        };
        Ok(VmFaultMaterialization {
            backing: backing_kind,
            page_index,
            page,
            publish_prot: self.entry.prot.without_write(),
            replace_existing: false,
            pmap_materialization_deferred: self.pmap_materialization_deferred,
        })
    }

    fn materialize_private_write_miss(
        &self,
        backing_kind: VmFaultMaterializationBacking,
        page_index: PageIndex,
        page_off: VmPageOff,
    ) -> Result<VmFaultMaterialization, VmFaultError> {
        // Allocate a fresh private frame, copying from backing source
        // when the backing has authoritative content.
        let new_page = match (&self.entry.backing, backing_kind) {
            (VmBacking::Page { pc, .. }, VmFaultMaterializationBacking::PageBacked) => {
                let access_byte = page_index
                    .as_u64()
                    .checked_mul(USER_PAGE_SIZE as u64)
                    .ok_or(VmFaultError::BackingOffsetOverflow)?;
                if access_byte >= pc.size_bytes() {
                    return Err(VmFaultError::PageBeyondSize);
                }
                let source = pc
                    .materialize_anon(page_index, MaterializeAccess::Read)
                    .map_err(VmFaultError::PageCache)?;
                let new = allocate_private_materialized_page_from_source(source.ppn, true)?;
                drop(source);
                new
            }
            (VmBacking::PrivateAnon, VmFaultMaterializationBacking::PrivateAnon) => {
                allocate_private_materialized_page(true)?
            }
            _ => return Err(VmFaultError::BackingMismatch),
        };
        // Install into `entry.private` if a set is attached. On CAS
        // failure (someone raced and published), drop our new page and
        // ask the script to retry.
        if let Some(set) = &self.entry.private {
            let cache_pin =
                page_allocator::acquire_cache_pin(new_page.ppn).map_err(page_alloc_error)?;
            let frame = PrivateFrame::new(new_page.ppn, PrivateFrameState::Exclusive, cache_pin);
            match set.install_if_absent(page_off, frame) {
                Ok(_) => {}
                Err(_) => {
                    drop(new_page);
                    return Err(VmFaultError::WouldBlock);
                }
            }
        }
        Ok(VmFaultMaterialization {
            backing: backing_kind,
            page_index,
            page: new_page,
            publish_prot: self.entry.prot,
            replace_existing: true,
            pmap_materialization_deferred: self.pmap_materialization_deferred,
        })
    }

    pub(in crate::vm) fn private_page_off(&self) -> Result<VmPageOff, VmFaultError> {
        let delta = self.recipe_page_delta()?;
        if !delta.is_multiple_of(USER_PAGE_SIZE) {
            return Err(VmFaultError::BackingOffsetOverflow);
        }
        Ok(VmPageOff((delta / USER_PAGE_SIZE) as u64))
    }

    /// PR-10 phase 6 — materialize a fresh private-anon page suitable
    /// for `UFFDIO_COPY` byte-move.
    ///
    /// `UFFDIO_COPY` semantics (per `man userfaultfd(2)`): the agent
    /// supplies a kernel-side `src` buffer whose bytes the kernel
    /// memcpy's into a freshly-installed private page covering
    /// `[dst_uaddr, dst_uaddr+len)`. Unlike a plain read fault, the
    /// destination must be a private (writable) page even on a read
    /// fault — the agent's bytes need to land somewhere we can write,
    /// and Linux's userfaultfd installs the page with full `entry.prot`
    /// regardless of the faulting access mode.
    ///
    /// This entrypoint always allocates a fresh private page (via
    /// [`allocate_private_materialized_page`]) and returns the
    /// materialization with `publish_prot = entry.prot` and
    /// `replace_existing = true` so the caller's
    /// `publish_page_with_replacement` swaps in the agent-filled page
    /// even if a zero-frame happened to be installed by a concurrent
    /// read-fault path.
    ///
    /// The byte-copy itself happens at the caller site
    /// (`fault_script_with_ufd_dispatch`) after this returns and before
    /// `publish_page_with_replacement` — see that function for the
    /// `copy_nonoverlapping` step.
    ///
    /// Restricted to `VmBacking::PrivateAnon`. Page-backed and
    /// Shared/None backings return `BackingMismatch` — phase 6 wires
    /// PrivateAnon only; other backings are a follow-up.
    pub fn materialize_pagebacked_for_ufd_copy(
        &self,
    ) -> Result<VmFaultMaterialization, VmFaultError> {
        if !matches!(self.entry.backing, VmBacking::PrivateAnon) {
            return Err(VmFaultError::BackingMismatch);
        }
        let page_index = self.private_anon_page_index()?;
        // Use `UninitFullOverwrite` because the agent's memcpy will
        // overwrite every byte of the page anyway — zeroing first
        // would be wasted work. The `dirty=true` flag matches the
        // post-COPY state of the page (the agent has written to it).
        let page = allocate_private_materialized_page_unzeroed(true)?;
        Ok(VmFaultMaterialization {
            backing: VmFaultMaterializationBacking::PrivateAnon,
            page_index,
            page,
            publish_prot: self.entry.prot,
            replace_existing: true,
            pmap_materialization_deferred: self.pmap_materialization_deferred,
        })
    }

    pub(in crate::vm) fn backing_page_index(&self) -> Result<PageIndex, VmFaultError> {
        let VmBacking::Page { offset, .. } = &self.entry.backing else {
            return Err(VmFaultError::BackingMismatch);
        };
        let delta = self.recipe_page_delta()?;
        let byte_offset = (*offset)
            .checked_add(u64::try_from(delta).map_err(|_| VmFaultError::BackingOffsetOverflow)?)
            .ok_or(VmFaultError::BackingOffsetOverflow)?;
        if !byte_offset.is_multiple_of(USER_PAGE_SIZE as u64) {
            return Err(VmFaultError::BackingOffsetOverflow);
        }
        Ok(PageIndex::new(byte_offset / USER_PAGE_SIZE as u64))
    }

    pub(in crate::vm) fn private_anon_page_index(&self) -> Result<PageIndex, VmFaultError> {
        if !matches!(self.entry.backing, VmBacking::PrivateAnon) {
            return Err(VmFaultError::BackingMismatch);
        }
        let delta = self.recipe_page_delta()?;
        if !delta.is_multiple_of(USER_PAGE_SIZE) {
            return Err(VmFaultError::BackingOffsetOverflow);
        }
        Ok(PageIndex::new((delta / USER_PAGE_SIZE) as u64))
    }

    fn recipe_page_delta(&self) -> Result<usize, VmFaultError> {
        let delta = self
            .page_range
            .start()
            .as_usize()
            .checked_sub(self.entry.range.start().as_usize())
            .ok_or(VmFaultError::BackingOffsetOverflow)?;
        Ok(delta)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmFaultMaterializationBacking {
    PageBacked,
    PrivateAnon,
}

#[derive(Debug)]
pub struct VmFaultMaterialization {
    pub backing: VmFaultMaterializationBacking,
    pub page_index: PageIndex,
    pub page: MaterializedPage,
    pub publish_prot: Prot,
    pub replace_existing: bool,
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
    PageBeyondSize,
    PageCache(PageCacheError),
    StaleRecipe,
    Pmap(VmPmapError),
}

impl From<VmPmapError> for VmFaultError {
    fn from(value: VmPmapError) -> Self {
        Self::Pmap(value)
    }
}

fn materialize_zero_frame() -> Result<MaterializedPage, VmFaultError> {
    let ppn = page_allocator::zero_frame_ppn().map_err(page_alloc_error)?;
    let map_pin = page_allocator::acquire_map_pin(ppn).map_err(page_alloc_error)?;
    Ok(MaterializedPage {
        ppn,
        map_pin: MaterializedPagePin::Allocated(map_pin),
        newly_installed: false,
        dirty: false,
    })
}

fn allocate_private_materialized_page(dirty: bool) -> Result<MaterializedPage, VmFaultError> {
    let frame = page_allocator::reserve_frame(ZeroPolicy::Zeroed)
        .map_err(page_alloc_error)?
        .commit();
    let ppn = frame.ppn();
    let map_pin = frame.try_map_pin().map_err(page_alloc_error)?;
    drop(frame);
    Ok(MaterializedPage {
        ppn,
        map_pin: MaterializedPagePin::Allocated(map_pin),
        newly_installed: true,
        dirty,
    })
}

/// PR-10 phase 6 — reserve a private materialized page without
/// zeroing it. The caller (the `UFFDIO_COPY` byte-move path in
/// `fault_script_with_ufd_dispatch`) overwrites every byte of the
/// page with the agent's `src` buffer immediately after this
/// returns, so the zero step is unnecessary.
fn allocate_private_materialized_page_unzeroed(
    dirty: bool,
) -> Result<MaterializedPage, VmFaultError> {
    let frame = page_allocator::reserve_frame(ZeroPolicy::UninitFullOverwrite)
        .map_err(page_alloc_error)?
        .commit();
    let ppn = frame.ppn();
    let map_pin = frame.try_map_pin().map_err(page_alloc_error)?;
    drop(frame);
    Ok(MaterializedPage {
        ppn,
        map_pin: MaterializedPagePin::Allocated(map_pin),
        newly_installed: true,
        dirty,
    })
}

fn allocate_private_materialized_page_from_source(
    source: tx_hal::Ppn,
    dirty: bool,
) -> Result<MaterializedPage, VmFaultError> {
    let reservation =
        page_allocator::reserve_frame(ZeroPolicy::UninitFullOverwrite).map_err(page_alloc_error)?;
    page_allocator::copy_frame_contents(source, reservation.ppn()).map_err(page_alloc_error)?;
    let frame = reservation.commit();
    let ppn = frame.ppn();
    let map_pin = frame.try_map_pin().map_err(page_alloc_error)?;
    drop(frame);
    Ok(MaterializedPage {
        ppn,
        map_pin: MaterializedPagePin::Allocated(map_pin),
        newly_installed: true,
        dirty,
    })
}

const fn page_alloc_error(error: step_engine::page_allocator::AllocError) -> VmFaultError {
    VmFaultError::PageCache(PageCacheError::Alloc(error))
}
