//! Pure VM foundation values.
//!
//! This module intentionally stops before real `AddressSpace`, zone-owned
//! recipes, page-backed content, or pmap materialization. The types here are
//! host-testable arithmetic and coordination building blocks for those later
//! layers.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

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
        self.0 % USER_PAGE_SIZE == 0
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
        if !start.is_page_aligned() || len % USER_PAGE_SIZE != 0 {
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
    MockPage { id: u64, offset: u64 },
}

impl VmBackingDraft {
    fn at_range_offset(self, delta: usize) -> Result<Self, VmEntryError> {
        match self {
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
}

impl Drop for RangeGuard<'_> {
    fn drop(&mut self) {
        self.lock.release_active(self.id);
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
}

impl<'a> PendingWriter<'a> {
    pub fn try_acquire(self) -> AcquireResult<'a> {
        self.lock.acquire_pending_writer(self)
    }
}

impl Drop for PendingWriter<'_> {
    fn drop(&mut self) {
        self.lock.release_pending_writer(self.id);
    }
}

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
                },
                second: RangeGuard {
                    lock: self,
                    id: second_id,
                },
            }),
            (Some(first_id), None) => {
                state.release_active(first_id);
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
        let Some(range) = state.pending_range(pending.id) else {
            pending.id = 0;
            return AcquireResult::WouldBlock(WouldBlock {
                pending_writer: None,
            });
        };
        if state.writer_blocked(range, Some(pending.id)) {
            return AcquireResult::WouldBlock(WouldBlock {
                pending_writer: Some(pending),
            });
        }
        state.release_pending_writer(pending.id);
        pending.id = 0;
        core::mem::forget(pending);
        state.acquire_active(self, range, LockMode::ExclusiveWriter)
    }

    fn release_active(&self, id: u64) {
        self.state.lock().release_active(id);
    }

    fn release_pending_writer(&self, id: u64) {
        if id != 0 {
            self.state.lock().release_pending_writer(id);
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
    active: [Option<Reservation>; MAX_ACTIVE_RANGES],
    pending_writers: [Option<Reservation>; MAX_PENDING_WRITERS],
    next_id: u64,
}

impl RangeLockState {
    const fn new() -> Self {
        Self {
            active: [None; MAX_ACTIVE_RANGES],
            pending_writers: [None; MAX_PENDING_WRITERS],
            next_id: 1,
        }
    }

    fn materializer_blocked(&self, range: UserRange) -> bool {
        self.active.iter().flatten().any(|reservation| {
            reservation.mode == LockMode::ExclusiveWriter && reservation.range.overlaps(range)
        }) || self
            .pending_writers
            .iter()
            .flatten()
            .any(|reservation| reservation.range.overlaps(range))
    }

    fn writer_blocked(&self, range: UserRange, own_pending_id: Option<u64>) -> bool {
        self.active
            .iter()
            .flatten()
            .any(|reservation| reservation.range.overlaps(range))
            || self.pending_writers.iter().flatten().any(|reservation| {
                if !reservation.range.overlaps(range) {
                    return false;
                }
                match own_pending_id {
                    Some(own_id) => reservation.id < own_id,
                    None => true,
                }
            })
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
            Some(id) => AcquireResult::Acquired(RangeGuard { lock, id }),
            None => AcquireResult::WouldBlock(WouldBlock {
                pending_writer: None,
            }),
        }
    }

    fn reserve_active_slot(&mut self, range: UserRange, mode: LockMode) -> Option<u64> {
        let index = self.active.iter().position(Option::is_none)?;
        let id = self.take_id();
        self.active[index] = Some(Reservation { id, range, mode });
        Some(id)
    }

    fn acquire_pending_writer<'a>(
        &mut self,
        lock: &'a RangeLock,
        range: UserRange,
    ) -> AcquireResult<'a> {
        let Some(index) = self.pending_writers.iter().position(Option::is_none) else {
            return AcquireResult::WouldBlock(WouldBlock {
                pending_writer: None,
            });
        };
        let id = self.take_id();
        self.pending_writers[index] = Some(Reservation {
            id,
            range,
            mode: LockMode::ExclusiveWriter,
        });
        AcquireResult::WouldBlock(WouldBlock {
            pending_writer: Some(PendingWriter { lock, id }),
        })
    }

    fn pending_range(&self, id: u64) -> Option<UserRange> {
        self.pending_writers
            .iter()
            .flatten()
            .find(|reservation| reservation.id == id)
            .map(|reservation| reservation.range)
    }

    fn release_active(&mut self, id: u64) {
        if let Some(slot) = self
            .active
            .iter_mut()
            .find(|slot| slot.map(|reservation| reservation.id) == Some(id))
        {
            *slot = None;
        }
    }

    fn release_pending_writer(&mut self, id: u64) {
        if let Some(slot) = self
            .pending_writers
            .iter_mut()
            .find(|slot| slot.map(|reservation| reservation.id) == Some(id))
        {
            *slot = None;
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
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
}
