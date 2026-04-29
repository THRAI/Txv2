//! Physical page allocator substrate.
//!
//! # Map topology
//!
//! Boot gives the page substrate a normalized physical map. The allocator keeps
//! two flat, dense views over the covered raw-PPN interval:
//!
//! - `FrameMeta[ppn - base_ppn]` stores semantic liveness for the frame.
//! - `bitmap[ppn - base_ppn]` stores whether the v1 global backend may allocate it.
//!
//! Usable RAM frames can enter the bitmap. Reserved RAM, MMIO/device apertures,
//! firmware holes, kernel image pages, metadata pages, and pmap bootstrap pages
//! have metadata rows but their bitmap bits stay clear.
//!
//! In steady state, a globally free v1 frame has `FrameMeta.state == 0` and its
//! bitmap bit set. A live frame has a nonzero packed state and its bitmap bit
//! clear. `FrameReservation` is the deliberate middle state: the allocator has
//! cleared the bitmap bit, but `FrameMeta.state` remains zero until `commit()`
//! publishes an `OwnedFrame` by setting `refcount = 1`.
//!
//! ```text
//! bitmap free
//!   -> FrameReservation          // bitmap cleared, state still zero
//!   -> OwnedFrame                // refcount = 1
//!   -> role evidence / anchors   // map/cache/DMA/PT/permanent ownership
//!   -> state reaches zero
//!   -> bitmap free
//! ```
//!
//! Raw `Ppn` values are observable for PTE encoding and direct-map access, but
//! dropping a typed token is the only normal way to return a frame to the
//! allocator.
//!
//! # Usage shape
//!
//! - Kernel/page-cache callers reserve a frame with `reserve_frame(policy)`,
//!   commit it to `OwnedFrame`, acquire the role token they need, publish the
//!   external binding, then drop `OwnedFrame`.
//! - Pmap intermediate allocation consumes `OwnedFrame` with
//!   `into_page_table_frame()` and releases it only through pmap teardown.
//! - Permanent frames are claimed once and intentionally never return to the
//!   normal free pool.

mod bitmap_backend;
mod diagnostics;
mod frame_meta;
mod tokens;

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use tx_hal::{PhysAddr, Ppn, PtNode};

pub use bitmap_backend::BitmapPageAllocator;
pub use diagnostics::{AllocatorBackendKind, AllocatorDiagnostics};
pub use frame_meta::FrameMeta;
pub use tokens::{
    CachePin, DeviceFrame, DmaPin, FrameReservation, FrameRunReservation, MapPin, OwnedFrame,
    OwnedFrameRun, OwnedFrameRunIter, PermanentFrame, PtFrame,
};

/// Allocation failures surfaced by the page allocator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocError {
    /// No free frame or contiguous run was available.
    Exhausted,
    /// The request was structurally invalid, such as a zero run length.
    InvalidRequest,
    /// Public substrate allocation was called before a backend was installed.
    NotInitialized,
    /// A second steady-state backend install was attempted.
    AlreadyInstalled,
    /// `ZeroPolicy::Zeroed` was requested before a direct-map scrubber existed.
    ZeroScrubUnavailable,
    /// A caller tried to claim a reserved frame through the normal path.
    ReservedFrame,
    /// A packed `FrameMeta` counter would overflow.
    CounterOverflow,
    /// A packed `FrameMeta` counter would underflow.
    CounterUnderflow,
    /// A bitmap bit was already free at a return-to-pool linearization point.
    DoubleFree,
}

/// Content-safety policy for a freshly reserved frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZeroPolicy {
    /// Scrub the frame through the direct map before returning the reservation.
    Zeroed,
    /// Caller promises to overwrite the whole frame before user-visible use.
    UninitFullOverwrite,
}

/// Direct-map scrubber used to satisfy `ZeroPolicy::Zeroed`.
///
/// # Safety
///
/// The function must be installed only after the direct map covers the frame
/// range represented by this allocator. It must zero exactly the frame
/// addressed by the `Ppn` and must not rely on allocator-owned Rust aliases.
pub type FrameZeroer = unsafe fn(Ppn);

/// Non-dynamic page allocator backend interface.
///
/// Production callers normally use the installed substrate functions at the
/// bottom of this module. Tests and future backends can use `A: PageAllocator`
/// generics without introducing `dyn PageAllocator` into the substrate path.
pub trait PageAllocator: Sized {
    /// Reserve one physical frame.
    ///
    /// The returned `FrameReservation` is rollback-only until committed; its
    /// bitmap bit has been cleared, but `FrameMeta.state` is still zero.
    fn reserve_frame(&self, policy: ZeroPolicy) -> Result<FrameReservation<'_, Self>, AllocError>;

    /// Reserve a contiguous PPN run at an `align`-multiple base.
    ///
    /// Runs bypass any future per-CPU magazines and use the global backend.
    fn reserve_run(
        &self,
        count: usize,
        align: usize,
        policy: ZeroPolicy,
    ) -> Result<FrameRunReservation<'_, Self>, AllocError>;

    /// Current allocator-free frame count.
    fn free_count(&self) -> usize;

    /// Total number of metadata rows/addressable frames covered by the backend.
    fn total_count(&self) -> usize;

    /// Return backend-local diagnostics for logging and memory accounting.
    fn backend_diagnostics(&self) -> AllocatorDiagnostics;

    #[doc(hidden)]
    /// Promote a reservation to `OwnedFrame` by setting `refcount = 1`.
    fn commit_reserved(&self, ppn: Ppn);

    #[doc(hidden)]
    /// Promote a reserved run to owned frames by setting each refcount to one.
    fn commit_reserved_run(&self, base: Ppn, count: usize);

    #[doc(hidden)]
    /// Drop a reservation before publish by returning the bitmap bit.
    fn rollback_reserved(&self, ppn: Ppn);

    #[doc(hidden)]
    /// Drop an uncommitted run reservation before publish.
    fn rollback_reserved_run(&self, base: Ppn, count: usize);

    #[doc(hidden)]
    /// Acquire pmap/PTE role evidence for a live frame.
    fn acquire_map_pin(&self, ppn: Ppn) -> Result<(), AllocError>;

    #[doc(hidden)]
    /// Release pmap/PTE role evidence and free if the whole state reaches zero.
    fn release_map_pin(&self, ppn: Ppn);

    #[doc(hidden)]
    /// Acquire page-cache role evidence for a live frame.
    fn acquire_cache_pin(&self, ppn: Ppn) -> Result<(), AllocError>;

    #[doc(hidden)]
    /// Release page-cache role evidence and free if the whole state reaches zero.
    fn release_cache_pin(&self, ppn: Ppn);

    #[doc(hidden)]
    /// Acquire DMA/long-term pin evidence for a live frame.
    fn acquire_dma_pin(&self, ppn: Ppn) -> Result<(), AllocError>;

    #[doc(hidden)]
    /// Release DMA/long-term pin evidence and free if the whole state reaches zero.
    fn release_dma_pin(&self, ppn: Ppn);

    #[doc(hidden)]
    /// Release generic owned-frame refcount.
    fn release_owned(&self, ppn: Ppn);

    #[doc(hidden)]
    /// Convert an `OwnedFrame` into pmap-owned page-table storage.
    fn adopt_page_table_frame(&self, ppn: Ppn);

    #[doc(hidden)]
    /// Convert an `OwnedFrame` into a never-free permanent anchor.
    fn adopt_permanent_frame(&self, ppn: Ppn);

    #[doc(hidden)]
    /// Release pmap-owned page-table storage through explicit teardown.
    fn release_page_table_frame(&self, ppn: Ppn);
}

static INSTALLED_BITMAP_ALLOCATOR: AtomicPtr<BitmapPageAllocator<'static>> =
    AtomicPtr::new(ptr::null_mut());
const NO_ZERO_FRAME: usize = usize::MAX;
static ZERO_FRAME_PPN: AtomicUsize = AtomicUsize::new(NO_ZERO_FRAME);

/// Install the steady-state bitmap allocator.
///
/// Boot calls this once after `FrameMeta[]` and the bitmap are carved and
/// populated. Later public substrate allocation functions delegate here.
pub fn install_bitmap_allocator(
    allocator: &'static BitmapPageAllocator<'static>,
) -> Result<(), AllocError> {
    INSTALLED_BITMAP_ALLOCATOR
        .compare_exchange(
            ptr::null_mut(),
            allocator as *const BitmapPageAllocator<'static> as *mut BitmapPageAllocator<'static>,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| AllocError::AlreadyInstalled)
}

/// Return the installed bitmap allocator, or `NotInitialized`.
pub fn installed_bitmap_allocator() -> Result<&'static BitmapPageAllocator<'static>, AllocError> {
    let ptr = INSTALLED_BITMAP_ALLOCATOR.load(Ordering::Acquire);
    if ptr.is_null() {
        return Err(AllocError::NotInitialized);
    }

    Ok(unsafe { &*ptr })
}

/// Reserve one frame from the installed backend.
pub fn reserve_frame(
    policy: ZeroPolicy,
) -> Result<FrameReservation<'static, BitmapPageAllocator<'static>>, AllocError> {
    installed_bitmap_allocator()?.reserve_frame(policy)
}

/// Reserve a contiguous frame run from the installed backend.
pub fn reserve_run(
    count: usize,
    align: usize,
    policy: ZeroPolicy,
) -> Result<FrameRunReservation<'static, BitmapPageAllocator<'static>>, AllocError> {
    installed_bitmap_allocator()?.reserve_run(count, align, policy)
}

/// Free-frame count from the installed backend.
pub fn free_count() -> Result<usize, AllocError> {
    Ok(installed_bitmap_allocator()?.free_count())
}

/// Total frame count from the installed backend.
pub fn total_count() -> Result<usize, AllocError> {
    Ok(installed_bitmap_allocator()?.total_count())
}

/// Allocate a pmap intermediate page-table node from the installed allocator.
///
/// This is the post-boot PT-node source installed into `PmapIf` once the frame
/// allocator is live. The returned `PtNode` carries a release hook so pmap
/// rollback can return abandoned intermediate pages without the HAL crate
/// depending on substrate internals.
pub fn reserve_page_table_node() -> Result<PtNode, tx_hal::AllocError> {
    let frame = reserve_frame(ZeroPolicy::Zeroed)
        .map_err(|_| tx_hal::AllocError::Exhausted)?
        .commit();
    let pt_frame = frame.into_page_table_frame();
    let phys = PhysAddr(pt_frame.ppn().0 * 4096);
    core::mem::forget(pt_frame);
    Ok(PtNode::typed_frame(phys, release_page_table_node))
}

/// Claim and remember the kernel zero frame.
///
/// Boot calls this after the allocator and direct-map zeroer are installed.
/// The frame is returned as a permanent anchor and intentionally never re-enters
/// the normal allocator pool.
pub fn claim_zero_frame() -> Result<Ppn, AllocError> {
    let frame = reserve_frame(ZeroPolicy::Zeroed)?.commit();
    let ppn = frame.ppn();
    let _anchor = frame.into_permanent_frame();
    ZERO_FRAME_PPN
        .compare_exchange(NO_ZERO_FRAME, ppn.0, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| AllocError::AlreadyInstalled)?;
    Ok(ppn)
}

/// Return the permanent zero-frame PPN after boot has claimed it.
pub fn zero_frame_ppn() -> Result<Ppn, AllocError> {
    let ppn = ZERO_FRAME_PPN.load(Ordering::Acquire);
    if ppn == NO_ZERO_FRAME {
        return Err(AllocError::NotInitialized);
    }
    Ok(Ppn(ppn))
}

/// Claim a covered frame as a never-free permanent anchor.
///
/// Boot uses this for allocator metadata and bootstrap page-table storage that
/// was already removed from the free bitmap during memory planning.
pub fn claim_permanent_frame(
    ppn: Ppn,
) -> Result<PermanentFrame<'static, BitmapPageAllocator<'static>>, AllocError> {
    installed_bitmap_allocator()?.claim_permanent_frame(ppn)
}

unsafe fn release_page_table_node(phys: PhysAddr) {
    if let Ok(allocator) = installed_bitmap_allocator() {
        allocator.release_page_table_frame(Ppn(phys.0 / 4096));
    }
}

/// Backend diagnostics from the installed allocator.
pub fn backend_diagnostics() -> Result<AllocatorDiagnostics, AllocError> {
    Ok(installed_bitmap_allocator()?.backend_diagnostics())
}
