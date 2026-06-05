use core::fmt;
use core::marker::PhantomData;
use tx_hal::Ppn;

use super::{AllocError, PageAllocator};

/// Allocator claim for one frame.
///
/// Use `commit()` to publish it as an `OwnedFrame`. If this value is dropped
/// before commit, the frame is returned to the backend and never becomes live.
#[must_use]
pub struct FrameReservation<'a, A: PageAllocator> {
    allocator: &'a A,
    ppn: Ppn,
    committed: bool,
    _not_send: PhantomData<*const ()>,
}

impl<'a, A: PageAllocator> FrameReservation<'a, A> {
    pub(crate) fn new(allocator: &'a A, ppn: Ppn) -> Self {
        Self {
            allocator,
            ppn,
            committed: false,
            _not_send: PhantomData,
        }
    }

    /// PPN reserved by this rollback token.
    pub fn ppn(&self) -> Ppn {
        self.ppn
    }

    /// Publish the frame by setting `FrameMeta.refcount = 1`.
    pub fn commit(mut self) -> OwnedFrame<'a, A> {
        self.allocator.commit_reserved(self.ppn);
        self.committed = true;
        OwnedFrame::new(self.allocator, self.ppn)
    }
}

impl<A: PageAllocator> Drop for FrameReservation<'_, A> {
    fn drop(&mut self) {
        if !self.committed {
            self.allocator.rollback_reserved(self.ppn);
        }
    }
}

impl<A: PageAllocator> fmt::Debug for FrameReservation<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameReservation")
            .field("ppn", &self.ppn)
            .field("committed", &self.committed)
            .finish()
    }
}

/// Allocator claim for a contiguous run of frames.
///
/// The whole run is rollback-only until committed. Dropping an uncommitted run
/// restores all bitmap bits.
#[must_use]
pub struct FrameRunReservation<'a, A: PageAllocator> {
    allocator: &'a A,
    base: Ppn,
    count: usize,
    committed: bool,
    _not_send: PhantomData<*const ()>,
}

impl<'a, A: PageAllocator> FrameRunReservation<'a, A> {
    pub(crate) fn new(allocator: &'a A, base: Ppn, count: usize) -> Self {
        Self {
            allocator,
            base,
            count,
            committed: false,
            _not_send: PhantomData,
        }
    }

    /// Base PPN of the reserved run.
    pub fn base(&self) -> Ppn {
        self.base
    }

    /// Number of frames in the reserved run.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Publish every frame in the run by setting each refcount to one.
    pub fn commit(mut self) -> OwnedFrameRun<'a, A> {
        self.allocator.commit_reserved_run(self.base, self.count);
        self.committed = true;
        OwnedFrameRun::new(self.allocator, self.base, self.count)
    }
}

impl<A: PageAllocator> Drop for FrameRunReservation<'_, A> {
    fn drop(&mut self) {
        if !self.committed {
            self.allocator.rollback_reserved_run(self.base, self.count);
        }
    }
}

impl<A: PageAllocator> fmt::Debug for FrameRunReservation<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameRunReservation")
            .field("base", &self.base)
            .field("count", &self.count)
            .field("committed", &self.committed)
            .finish()
    }
}

/// Fresh owned physical frame.
///
/// `OwnedFrame` represents generic frame ownership while a caller prepares the
/// page for a role. Role handoff is: acquire the role token first, publish the
/// external binding second, then drop `OwnedFrame` last.
#[must_use]
pub struct OwnedFrame<'a, A: PageAllocator> {
    allocator: &'a A,
    ppn: Ppn,
    active: bool,
    _not_send: PhantomData<*const ()>,
}

impl<'a, A: PageAllocator> OwnedFrame<'a, A> {
    fn new(allocator: &'a A, ppn: Ppn) -> Self {
        Self {
            allocator,
            ppn,
            active: true,
            _not_send: PhantomData,
        }
    }

    /// PPN of the owned frame.
    pub fn ppn(&self) -> Ppn {
        self.ppn
    }

    /// Acquire pmap/PTE role evidence for this live frame.
    pub fn try_map_pin(&self) -> Result<MapPin<'a, A>, AllocError> {
        self.allocator.acquire_map_pin(self.ppn)?;
        Ok(MapPin::new(self.allocator, self.ppn))
    }

    /// Acquire page-cache role evidence for this live frame.
    pub fn try_cache_pin(&self) -> Result<CachePin<'a, A>, AllocError> {
        self.allocator.acquire_cache_pin(self.ppn)?;
        Ok(CachePin::new(self.allocator, self.ppn))
    }

    /// Acquire DMA/long-term pin evidence for this live frame.
    pub fn try_dma_pin(&self) -> Result<DmaPin<'a, A>, AllocError> {
        self.allocator.acquire_dma_pin(self.ppn)?;
        Ok(DmaPin::new(self.allocator, self.ppn))
    }

    /// Acquire user-page-transfer evidence for this live frame.
    pub fn try_gift_pin(&self) -> Result<GiftPin<'a, A>, AllocError> {
        self.allocator.acquire_gift_pin(self.ppn)?;
        Ok(GiftPin::new(self.allocator, self.ppn))
    }

    /// Transfer ownership to pmap page-table storage.
    ///
    /// The returned `PtFrame` must be released through pmap teardown, not by
    /// ordinary owned-frame drop.
    pub fn into_page_table_frame(mut self) -> PtFrame<'a, A> {
        self.allocator.adopt_page_table_frame(self.ppn);
        self.active = false;
        PtFrame::new(self.allocator, self.ppn)
    }

    /// Transfer ownership to a never-free permanent anchor.
    ///
    /// This is used for the zero frame and similar kernel-lifetime storage.
    /// Dropping the returned token does not release the frame.
    pub fn into_permanent_frame(mut self) -> PermanentFrame<'a, A> {
        self.allocator.adopt_permanent_frame(self.ppn);
        self.active = false;
        PermanentFrame::new(self.allocator, self.ppn)
    }
}

impl<A: PageAllocator> Drop for OwnedFrame<'_, A> {
    fn drop(&mut self) {
        if self.active {
            self.allocator.release_owned(self.ppn);
        }
    }
}

impl<A: PageAllocator> fmt::Debug for OwnedFrame<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedFrame")
            .field("ppn", &self.ppn)
            .field("active", &self.active)
            .finish()
    }
}

/// Owned contiguous run of physical frames.
///
/// Dropping the run releases every owned refcount. Use `split()` when the caller
/// wants ordinary single-frame ownership tokens after a run allocation.
#[must_use]
pub struct OwnedFrameRun<'a, A: PageAllocator> {
    allocator: &'a A,
    base: Ppn,
    count: usize,
    active: bool,
    _not_send: PhantomData<*const ()>,
}

impl<'a, A: PageAllocator> OwnedFrameRun<'a, A> {
    fn new(allocator: &'a A, base: Ppn, count: usize) -> Self {
        Self {
            allocator,
            base,
            count,
            active: true,
            _not_send: PhantomData,
        }
    }

    /// Base PPN of the owned run.
    pub fn base(&self) -> Ppn {
        self.base
    }

    /// Number of frames in the owned run.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Acquire pmap/PTE role evidence for every frame in this live run.
    pub fn try_map_pin_run(&self) -> Result<MapPinRun<'a, A>, AllocError> {
        let mut acquired = 0usize;
        while acquired < self.count {
            let ppn = Ppn(self.base.0 + acquired);
            if let Err(err) = self.allocator.acquire_map_pin(ppn) {
                while acquired > 0 {
                    acquired -= 1;
                    self.allocator.release_map_pin(Ppn(self.base.0 + acquired));
                }
                return Err(err);
            }
            acquired += 1;
        }
        Ok(MapPinRun::new(self.allocator, self.base, self.count))
    }

    /// Split the run into individual `OwnedFrame` tokens without heap use.
    pub fn split(mut self) -> OwnedFrameRunIter<'a, A> {
        self.active = false;
        OwnedFrameRunIter {
            allocator: self.allocator,
            base: self.base,
            count: self.count,
            next: 0,
        }
    }
}

impl<A: PageAllocator> Drop for OwnedFrameRun<'_, A> {
    fn drop(&mut self) {
        if self.active {
            for offset in 0..self.count {
                self.allocator.release_owned(Ppn(self.base.0 + offset));
            }
        }
    }
}

impl<A: PageAllocator> fmt::Debug for OwnedFrameRun<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedFrameRun")
            .field("base", &self.base)
            .field("count", &self.count)
            .field("active", &self.active)
            .finish()
    }
}

/// Iterator returned by `OwnedFrameRun::split()`.
///
/// Frames yielded from the iterator become ordinary `OwnedFrame` tokens.
/// Unyielded frames are released if the iterator is dropped early.
#[must_use]
pub struct OwnedFrameRunIter<'a, A: PageAllocator> {
    allocator: &'a A,
    base: Ppn,
    count: usize,
    next: usize,
}

impl<'a, A: PageAllocator> Iterator for OwnedFrameRunIter<'a, A> {
    type Item = OwnedFrame<'a, A>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.count {
            return None;
        }
        let ppn = Ppn(self.base.0 + self.next);
        self.next += 1;
        Some(OwnedFrame::new(self.allocator, ppn))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.count - self.next;
        (remaining, Some(remaining))
    }
}

impl<A: PageAllocator> ExactSizeIterator for OwnedFrameRunIter<'_, A> {}

impl<A: PageAllocator> Drop for OwnedFrameRunIter<'_, A> {
    fn drop(&mut self) {
        while self.next < self.count {
            let ppn = Ppn(self.base.0 + self.next);
            self.next += 1;
            self.allocator.release_owned(ppn);
        }
    }
}

impl<A: PageAllocator> fmt::Debug for OwnedFrameRunIter<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedFrameRunIter")
            .field("base", &self.base)
            .field("count", &self.count)
            .field("next", &self.next)
            .finish()
    }
}

macro_rules! role_pin {
    ($(#[$meta:meta])* $name:ident, $release:ident) => {
        $(#[$meta])*
        #[must_use]
        pub struct $name<'a, A: PageAllocator> {
            allocator: &'a A,
            ppn: Ppn,
            active: bool,
            _not_send: PhantomData<*const ()>,
        }

        impl<'a, A: PageAllocator> $name<'a, A> {
            pub(crate) fn new(allocator: &'a A, ppn: Ppn) -> Self {
                Self {
                    allocator,
                    ppn,
                    active: true,
                    _not_send: PhantomData,
                }
            }

            /// PPN protected by this role token.
            pub fn ppn(&self) -> Ppn {
                self.ppn
            }
        }

        impl<A: PageAllocator> Drop for $name<'_, A> {
            fn drop(&mut self) {
                if self.active {
                    self.allocator.$release(self.ppn);
                }
            }
        }

        impl<A: PageAllocator> fmt::Debug for $name<'_, A> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("ppn", &self.ppn)
                    .field("active", &self.active)
                    .finish()
            }
        }
    };
}

role_pin!(
    /// Evidence that pmap/PTE state may still reference a frame.
    ///
    /// Dropping the token decrements `map_count`; the frame is freed only when
    /// all other owner and role counters are also zero.
    MapPin,
    release_map_pin
);

/// Evidence that pmap/PTE state may still reference a contiguous frame run.
///
/// This is the run-shaped counterpart to `MapPin`, used for superpage PTEs
/// that retain many 4 KiB frame rows through one mapping.
#[must_use]
pub struct MapPinRun<'a, A: PageAllocator> {
    allocator: &'a A,
    base: Ppn,
    count: usize,
    active: bool,
    _not_send: PhantomData<*const ()>,
}

impl<'a, A: PageAllocator> MapPinRun<'a, A> {
    fn new(allocator: &'a A, base: Ppn, count: usize) -> Self {
        Self {
            allocator,
            base,
            count,
            active: true,
            _not_send: PhantomData,
        }
    }

    /// Base PPN protected by this role token.
    pub fn base(&self) -> Ppn {
        self.base
    }

    /// Number of 4 KiB frames protected by this role token.
    pub fn count(&self) -> usize {
        self.count
    }
}

impl<A: PageAllocator> Drop for MapPinRun<'_, A> {
    fn drop(&mut self) {
        if self.active {
            for offset in 0..self.count {
                self.allocator.release_map_pin(Ppn(self.base.0 + offset));
            }
        }
    }
}

impl<A: PageAllocator> fmt::Debug for MapPinRun<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapPinRun")
            .field("base", &self.base)
            .field("count", &self.count)
            .field("active", &self.active)
            .finish()
    }
}

role_pin!(
    /// Evidence that page-cache indexing may still reference a frame.
    ///
    /// Dropping the token decrements `cache_ref`; the frame is freed only when
    /// all other owner and role counters are also zero.
    CachePin,
    release_cache_pin
);
role_pin!(
    /// Evidence that DMA or another long-term pin may still reference a frame.
    ///
    /// Dropping the token decrements `pin_count`; the frame is freed only when
    /// all other owner and role counters are also zero.
    DmaPin,
    release_dma_pin
);
role_pin!(
    /// Evidence that a VM user-page gift transfer may still reference a frame.
    ///
    /// Dropping the token decrements retained `refcount`; the frame is freed
    /// only when all other owner and role counters are also zero.
    GiftPin,
    release_gift_pin
);

/// Pmap-owned page-table frame.
///
/// This token is created from `OwnedFrame::into_page_table_frame()`. It marks
/// the frame `reserved | direct_mapped` so normal allocator drops cannot reclaim
/// it. Use `release_for_pmap_teardown()` when the pmap tree removes the page.
#[must_use]
pub struct PtFrame<'a, A: PageAllocator> {
    allocator: &'a A,
    ppn: Ppn,
    active: bool,
    _not_send: PhantomData<*const ()>,
}

impl<'a, A: PageAllocator> PtFrame<'a, A> {
    fn new(allocator: &'a A, ppn: Ppn) -> Self {
        Self {
            allocator,
            ppn,
            active: true,
            _not_send: PhantomData,
        }
    }

    /// PPN of the page-table frame.
    pub fn ppn(&self) -> Ppn {
        self.ppn
    }

    /// Release a page-table frame after pmap teardown has made it unreachable.
    pub fn release_for_pmap_teardown(mut self) {
        self.allocator.release_page_table_frame(self.ppn);
        self.active = false;
    }
}

impl<A: PageAllocator> Drop for PtFrame<'_, A> {
    fn drop(&mut self) {
        debug_assert!(
            !self.active,
            "PtFrame must be released through pmap teardown"
        );
    }
}

impl<A: PageAllocator> fmt::Debug for PtFrame<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtFrame")
            .field("ppn", &self.ppn)
            .field("active", &self.active)
            .finish()
    }
}

/// Never-free frame anchor.
///
/// Permanent frames keep `refcount = 1` and `reserved = true` for the lifetime
/// of the kernel. Dropping this Rust value does not return the frame to the
/// allocator.
#[must_use]
pub struct PermanentFrame<'a, A: PageAllocator> {
    _allocator: &'a A,
    ppn: Ppn,
    _not_send: PhantomData<*const ()>,
}

impl<'a, A: PageAllocator> PermanentFrame<'a, A> {
    pub(crate) fn new(allocator: &'a A, ppn: Ppn) -> Self {
        Self {
            _allocator: allocator,
            ppn,
            _not_send: PhantomData,
        }
    }

    /// PPN of the permanent frame.
    pub fn ppn(&self) -> Ppn {
        self.ppn
    }
}

impl<A: PageAllocator> fmt::Debug for PermanentFrame<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermanentFrame")
            .field("ppn", &self.ppn)
            .finish()
    }
}

/// Typed evidence for a device/MMIO physical frame.
///
/// Device frames are not allocator-owned and never enter normal free logic.
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceFrame {
    ppn: Ppn,
}

impl DeviceFrame {
    /// Construct typed evidence for a non-RAM/device PPN.
    pub const fn new(ppn: Ppn) -> Self {
        Self { ppn }
    }

    /// Return the underlying PPN for pmap/device mapping code.
    pub const fn ppn(self) -> Ppn {
        self.ppn
    }
}
