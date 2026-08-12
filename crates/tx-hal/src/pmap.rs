//! Generic pmap range helpers.
//!
//! Core data structures/state maintained here:
//! - `PmapRangeReservation<P, N>`: a stack-backed, no-allocation transaction
//!   holding up to `N` reserved `PmapReservation`s.
//! - caller-provided output slices for unmap/protect evidence; this module does
//!   not allocate result buffers.
//!
//! Main data-flow functions:
//! - `reserve_page_range()` reserves a contiguous 4 KiB range and returns the
//!   transaction object.
//! - `PmapRangeReservation::commit()` publishes all reserved pages.
//! - `Drop` for `PmapRangeReservation` rolls back any uncommitted prefix.
//! - `unmap_page_range()` and `protect_page_range()` collect per-page evidence
//!   for later shootdown/accounting.
//!
//! Helper logic is limited to checked page stepping, output initialization, and
//! error normalization. Boards still own concrete page-table walks and PTE
//! updates; VM still owns range locks and rematerialization policy. See
//! `docs/progress/decisions/2026-04-29-hal-pmap-surface-refactor.md`.

use core::marker::PhantomData;
use core::mem::MaybeUninit;

use super::{
    PhysAddr, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, VirtAddr,
};

const PAGE_SIZE_4K: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PmapRangeError {
    EmptyRange,
    BufferTooSmall,
    AddressOverflow,
    Pmap(PmapError),
    MissingReservation,
}

/// A bounded, in-place collection of ordered invalidation runs.
///
/// The storage is fixed at the call site's capacity. Adjacent invalidations
/// are merged into the last run without allocating or moving a second
/// coalesced buffer. `next_cursor` advances only when an invalidation is
/// accepted, so a full gather identifies the exact retry prefix.
pub struct InvalidationRunGather<const N: usize> {
    entries: [MaybeUninit<PmapInvalidation>; N],
    len: usize,
    next_cursor: VirtAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidationRunGatherError {
    kind: InvalidationRunGatherErrorKind,
    next_cursor: VirtAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidationRunGatherErrorKind {
    Capacity,
    InvalidCursor,
    AddressOverflow,
}

impl InvalidationRunGatherError {
    pub const fn kind(self) -> InvalidationRunGatherErrorKind {
        self.kind
    }

    pub const fn next_cursor(self) -> VirtAddr {
        self.next_cursor
    }
}

impl<const N: usize> InvalidationRunGather<N> {
    pub const fn new(next_cursor: VirtAddr) -> Self {
        Self {
            entries: [const { MaybeUninit::uninit() }; N],
            len: 0,
            next_cursor,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn next_cursor(&self) -> VirtAddr {
        self.next_cursor
    }

    /// Append one invalidation and the cursor immediately after its source
    /// page. A rejected append leaves both the stored prefix and cursor intact.
    pub fn push(
        &mut self,
        invalidation: PmapInvalidation,
        next_cursor: VirtAddr,
    ) -> Result<(), InvalidationRunGatherError> {
        let Some(invalidation_end) = invalidation.virt().0.checked_add(invalidation.size()) else {
            return Err(InvalidationRunGatherError {
                kind: InvalidationRunGatherErrorKind::AddressOverflow,
                next_cursor: self.next_cursor,
            });
        };
        if next_cursor.0 <= self.next_cursor.0 || next_cursor.0 < invalidation_end {
            return Err(InvalidationRunGatherError {
                kind: InvalidationRunGatherErrorKind::InvalidCursor,
                next_cursor: self.next_cursor,
            });
        }
        if let Some(last) = self.last_mut() {
            let Some(last_end) = last.virt().0.checked_add(last.size()) else {
                return Err(InvalidationRunGatherError {
                    kind: InvalidationRunGatherErrorKind::AddressOverflow,
                    next_cursor: self.next_cursor,
                });
            };
            if last_end == invalidation.virt().0 {
                let Some(size) = last.size().checked_add(invalidation.size()) else {
                    return Err(InvalidationRunGatherError {
                        kind: InvalidationRunGatherErrorKind::AddressOverflow,
                        next_cursor: self.next_cursor,
                    });
                };
                *last = PmapInvalidation::new(last.virt(), size);
                self.next_cursor = next_cursor;
                return Ok(());
            }
        }
        if self.len == N {
            return Err(InvalidationRunGatherError {
                kind: InvalidationRunGatherErrorKind::Capacity,
                next_cursor: self.next_cursor,
            });
        }
        self.entries[self.len].write(invalidation);
        self.len += 1;
        self.next_cursor = next_cursor;
        Ok(())
    }

    /// Borrow the coalesced prefix directly from the fixed block.
    pub fn as_slice(&self) -> &[PmapInvalidation] {
        // SAFETY: entries [0, len) are initialized by `push`; the storage is
        // contiguous and `MaybeUninit<T>` has the same layout as `T`.
        unsafe { core::slice::from_raw_parts(self.entries.as_ptr().cast(), self.len) }
    }

    pub fn clear(&mut self, next_cursor: VirtAddr) {
        self.len = 0;
        self.next_cursor = next_cursor;
    }

    fn last_mut(&mut self) -> Option<&mut PmapInvalidation> {
        if self.len == 0 {
            None
        } else {
            // SAFETY: every slot below len was initialized by `push`.
            Some(unsafe { self.entries[self.len - 1].assume_init_mut() })
        }
    }
}

impl From<PmapError> for PmapRangeError {
    fn from(value: PmapError) -> Self {
        Self::Pmap(value)
    }
}

/// Stack-backed reservation transaction for a page range.
///
/// Entries are stored in `MaybeUninit` so callers can reserve up to `N` pages
/// without heap allocation. Dropping an uncommitted value rolls back the
/// reserved prefix through the platform pmap.
pub struct PmapRangeReservation<'a, P: PmapIf, const N: usize> {
    root: &'a PmapRoot,
    entries: [MaybeUninit<PmapReservation>; N],
    len: usize,
    _platform: PhantomData<P>,
    _not_send: PhantomData<*const ()>,
}

impl<'a, P: PmapIf, const N: usize> PmapRangeReservation<'a, P, N> {
    fn new(root: &'a PmapRoot) -> Self {
        Self {
            root,
            entries: [const { MaybeUninit::uninit() }; N],
            len: 0,
            _platform: PhantomData,
            _not_send: PhantomData,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, reservation: PmapReservation) {
        self.entries[self.len].write(reservation);
        self.len += 1;
    }

    pub fn commit(mut self, permissions: PmapPermissions) -> usize {
        let len = self.len;
        self.len = 0;
        for index in 0..len {
            let reservation = unsafe { self.entries[index].assume_init_read() };
            P::commit_mapping(self.root, reservation, permissions);
        }
        len
    }
}

impl<P: PmapIf, const N: usize> Drop for PmapRangeReservation<'_, P, N> {
    fn drop(&mut self) {
        for index in 0..self.len {
            let reservation = unsafe { self.entries[index].assume_init_read() };
            P::rollback_mapping(self.root, reservation);
        }
        self.len = 0;
    }
}

// Range operations are intentionally page-sized v1 helpers. They collect
// unmap/protect results into caller-provided slices so substrate or VM can pair
// invalidations with map-count release after shootdown.
pub fn reserve_page_range<'a, P: PmapIf, const N: usize>(
    root: &'a PmapRoot,
    virt: VirtAddr,
    phys: PhysAddr,
    pages: usize,
) -> Result<PmapRangeReservation<'a, P, N>, PmapRangeError> {
    if pages == 0 {
        return Err(PmapRangeError::EmptyRange);
    }
    if pages > N {
        return Err(PmapRangeError::BufferTooSmall);
    }

    let mut range = PmapRangeReservation::<P, N>::new(root);
    for index in 0..pages {
        let offset = index
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(PmapRangeError::AddressOverflow)?;
        let page_virt = VirtAddr(
            virt.0
                .checked_add(offset)
                .ok_or(PmapRangeError::AddressOverflow)?,
        );
        let page_phys = PhysAddr(
            phys.0
                .checked_add(offset)
                .ok_or(PmapRangeError::AddressOverflow)?,
        );
        let reservation = P::reserve_mapping(root, page_virt, page_phys, PmapReserveKind::Page4K)?
            .ok_or(PmapRangeError::MissingReservation)?;
        range.push(reservation);
    }
    Ok(range)
}

pub fn unmap_page_range<P: PmapIf>(
    root: &PmapRoot,
    virt: VirtAddr,
    pages: usize,
    out: &mut [Option<PmapUnmapResult>],
) -> Result<usize, PmapRangeError> {
    if pages == 0 {
        return Err(PmapRangeError::EmptyRange);
    }
    if pages > out.len() {
        return Err(PmapRangeError::BufferTooSmall);
    }

    let mut len = 0;
    for slot in out.iter_mut().take(pages) {
        *slot = None;
    }
    for index in 0..pages {
        let offset = index
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(PmapRangeError::AddressOverflow)?;
        let page_virt = VirtAddr(
            virt.0
                .checked_add(offset)
                .ok_or(PmapRangeError::AddressOverflow)?,
        );
        if let Some(result) = P::unmap_mapping(root, page_virt, PmapReserveKind::Page4K)? {
            out[len] = Some(result);
            len += 1;
        }
    }
    Ok(len)
}

pub fn protect_page_range<P: PmapIf>(
    root: &PmapRoot,
    virt: VirtAddr,
    pages: usize,
    permissions: PmapPermissions,
    out: &mut [Option<PmapInvalidation>],
) -> Result<usize, PmapRangeError> {
    if pages == 0 {
        return Err(PmapRangeError::EmptyRange);
    }
    if pages > out.len() {
        return Err(PmapRangeError::BufferTooSmall);
    }

    let mut len = 0;
    for slot in out.iter_mut().take(pages) {
        *slot = None;
    }
    for index in 0..pages {
        let offset = index
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(PmapRangeError::AddressOverflow)?;
        let page_virt = VirtAddr(
            virt.0
                .checked_add(offset)
                .ok_or(PmapRangeError::AddressOverflow)?,
        );
        if let Some(invalidation) =
            P::protect_mapping(root, page_virt, PmapReserveKind::Page4K, permissions)?
        {
            out[len] = Some(invalidation);
            len += 1;
        }
    }
    Ok(len)
}
