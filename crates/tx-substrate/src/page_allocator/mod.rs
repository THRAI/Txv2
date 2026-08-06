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
use tx_hal::{DmaConstraints, DmaDomain, DmaTranslation, PhysAddr, Ppn, PtNode};

pub use bitmap_backend::BitmapPageAllocator;
pub use diagnostics::{AllocatorBackendKind, AllocatorDiagnostics, FrameRoleDiagnostics};
pub use frame_meta::FrameMeta;
pub use tokens::{
    CachePin, DeviceFrame, DmaPin, FrameReservation, FrameRunReservation, GiftPin, MapPin,
    MapPinRun, OwnedFrame, OwnedFrameRun, OwnedFrameRunIter, PermanentFrame, PtFrame,
};

/// The page substrate owns fixed 4 KiB base frames.
///
/// Huge pages are pmap compositions of these frames; DMA run counts and
/// alignments below are therefore expressed in 4 KiB frame units.
pub const FRAME_SIZE: usize = 4096;

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
    /// Frame content copy was requested before a direct-map copy hook existed.
    FrameCopyUnavailable,
    /// Frame kernel address lookup was requested before a direct-map hook existed.
    FrameKernelAddrUnavailable,
    /// A caller tried to claim a reserved frame through the normal path.
    ReservedFrame,
    /// A packed `FrameMeta` counter would overflow.
    CounterOverflow,
    /// A packed `FrameMeta` counter would underflow.
    CounterUnderflow,
    /// A bitmap bit was already free at a return-to-pool linearization point.
    DoubleFree,
}

/// A physically contiguous DMA run request, in 4 KiB frame units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaRunRequest {
    pub count: usize,
    pub align: usize,
}

/// Failures from constrained DMA frame reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaAllocError {
    /// The page backend or its content-safety hook failed.
    Alloc(AllocError),
    /// The request or effective constraint set is invalid or widens the domain.
    InvalidRequest,
    /// No currently free run satisfies all effective constraints.
    UnsatisfiedConstraints,
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

/// Direct-map frame copy hook used for CoW and staged PageBacked byte movement.
///
/// # Safety
///
/// The function must be installed only after the direct map covers both frames.
/// It must copy exactly one page from `source` to `dest` and must tolerate no
/// allocator-owned Rust aliases.
pub type FrameCopier = unsafe fn(source: Ppn, dest: Ppn);

/// Direct-map kernel-address lookup hook used by PageBacked user-buffer copy.
///
/// Returns the kernel virtual address of the start of the frame addressed by
/// `ppn`. Callers who need a pointer at a non-zero offset within the frame add
/// the offset themselves.
///
/// # Safety
///
/// The function must be installed only after the direct map covers the frame
/// range represented by this allocator. The returned pointer is read/write for
/// `PAGE_SIZE` bytes for as long as the frame remains live; callers are
/// responsible for respecting cache and aliasing rules and must not retain the
/// pointer past the frame's lifetime.
pub type FrameKernelAddr = unsafe fn(ppn: Ppn) -> *mut u8;

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
    /// Acquire retained-frame transfer evidence for a live frame.
    fn acquire_gift_pin(&self, ppn: Ppn) -> Result<(), AllocError>;

    #[doc(hidden)]
    /// Release retained-frame transfer evidence and free if the whole state reaches zero.
    fn release_gift_pin(&self, ppn: Ppn);

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

impl BitmapPageAllocator<'_> {
    /// Reserve a DMA-capable run from this concrete backend.
    ///
    /// Normal production callers use the installed [`reserve_dma_run`] facade.
    /// Keeping this backend form public also lets host tests exercise sparse and
    /// high-address allocator fixtures without replacing the installed global
    /// allocator.
    pub fn reserve_dma_run(
        &self,
        domain: &DmaDomain,
        effective: DmaConstraints,
        request: DmaRunRequest,
        policy: ZeroPolicy,
    ) -> Result<FrameRunReservation<'_, Self>, DmaAllocError> {
        let byte_len = validate_dma_request(domain, effective, request)?;
        self.reserve_run_matching(request.count, request.align, policy, |base| {
            dma_candidate_matches(base, byte_len, domain, effective)
        })
        .map_err(|err| match err {
            AllocError::Exhausted => DmaAllocError::UnsatisfiedConstraints,
            AllocError::InvalidRequest => DmaAllocError::InvalidRequest,
            other => DmaAllocError::Alloc(other),
        })
    }
}

fn validate_dma_request(
    domain: &DmaDomain,
    effective: DmaConstraints,
    request: DmaRunRequest,
) -> Result<u64, DmaAllocError> {
    if request.count == 0 || request.align == 0 {
        return Err(DmaAllocError::InvalidRequest);
    }
    let byte_len = request
        .count
        .checked_mul(FRAME_SIZE)
        .and_then(|len| u64::try_from(len).ok())
        .ok_or(DmaAllocError::InvalidRequest)?;
    request
        .align
        .checked_mul(FRAME_SIZE)
        .ok_or(DmaAllocError::InvalidRequest)?;

    let narrowed = domain
        .constraints
        .strict_intersection(effective)
        .map_err(|_| DmaAllocError::InvalidRequest)?;
    if narrowed != effective {
        return Err(DmaAllocError::InvalidRequest);
    }
    Ok(byte_len)
}

fn dma_candidate_matches(
    base: Ppn,
    byte_len: u64,
    domain: &DmaDomain,
    constraints: DmaConstraints,
) -> bool {
    let Some(phys_start) = u64::try_from(base.0)
        .ok()
        .and_then(|ppn| ppn.checked_mul(FRAME_SIZE as u64))
    else {
        return false;
    };
    if checked_range_end(phys_start, byte_len).is_none()
        || !is_aligned(phys_start, constraints.min_alignment)
    {
        return false;
    }

    match domain.translation {
        DmaTranslation::Direct { offset } => {
            let Some(device_start) = checked_add_signed(phys_start, offset) else {
                return false;
            };
            let Some(device_end) = checked_range_end(device_start, byte_len) else {
                return false;
            };
            if !fits_address_bits(device_end, constraints.dma_address_bits)
                || !is_aligned(device_start, constraints.min_alignment)
            {
                return false;
            }
            segments_fit(phys_start, Some(device_start), byte_len, constraints)
        }
        DmaTranslation::Managed { .. } => {
            // The IOVA is not known until DmaIf::map_dma.  Only the physical
            // alignment, boundary, segment length, and segment-count facts can
            // be proven at frame-reservation time.
            segments_fit(phys_start, None, byte_len, constraints)
        }
    }
}

fn checked_add_signed(value: u64, offset: i64) -> Option<u64> {
    if offset >= 0 {
        value.checked_add(offset as u64)
    } else {
        value.checked_sub(offset.unsigned_abs())
    }
}

fn checked_range_end(start: u64, len: u64) -> Option<u64> {
    start.checked_add(len.checked_sub(1)?)
}

fn fits_address_bits(end: u64, bits: u8) -> bool {
    bits == u64::BITS as u8 || end < (1u64 << bits)
}

fn is_aligned(address: u64, alignment: usize) -> bool {
    u64::try_from(alignment)
        .ok()
        .is_some_and(|alignment| address % alignment == 0)
}

fn boundary_room(address: u64, boundary: Option<u64>) -> u64 {
    match boundary {
        Some(boundary) => boundary - address % boundary,
        None => u64::MAX,
    }
}

fn segments_fit(
    mut phys_start: u64,
    mut device_start: Option<u64>,
    mut remaining: u64,
    constraints: DmaConstraints,
) -> bool {
    let Some(max_segment_len) = u64::try_from(constraints.max_segment_len).ok() else {
        return false;
    };
    let mut segments = 0u16;

    while remaining != 0 {
        if segments == constraints.max_segments {
            return false;
        }
        let mut segment_len = remaining
            .min(max_segment_len)
            .min(boundary_room(phys_start, constraints.segment_boundary));
        if let Some(device) = device_start {
            segment_len = segment_len.min(boundary_room(device, constraints.segment_boundary));
        }
        if segment_len == 0 {
            return false;
        }

        segments += 1;
        remaining -= segment_len;
        if remaining == 0 {
            return true;
        }
        let Some(next_phys) = phys_start.checked_add(segment_len) else {
            return false;
        };
        phys_start = next_phys;
        if let Some(device) = device_start {
            let Some(next_device) = device.checked_add(segment_len) else {
                return false;
            };
            device_start = Some(next_device);
        }
    }
    true
}

static INSTALLED_BITMAP_ALLOCATOR: AtomicPtr<BitmapPageAllocator<'static>> =
    AtomicPtr::new(ptr::null_mut());
const NO_FRAME_COPIER: usize = 0;
static FRAME_COPIER: AtomicUsize = AtomicUsize::new(NO_FRAME_COPIER);
const NO_FRAME_KERNEL_ADDR: usize = 0;
static FRAME_KERNEL_ADDR: AtomicUsize = AtomicUsize::new(NO_FRAME_KERNEL_ADDR);
const NO_ZERO_FRAME: usize = usize::MAX;
static ZERO_FRAME_PPN: AtomicUsize = AtomicUsize::new(NO_ZERO_FRAME);

macro_rules! measure_page_allocator {
    ($method_name:expr, $body:block) => {{
        #[cfg(all(tx_ds_metrics, tx_ds_metrics_page_allocator))]
        {
            crate::ds_metrics::measure($method_name, || $body)
        }
        #[cfg(not(all(tx_ds_metrics, tx_ds_metrics_page_allocator)))]
        {
            $body
        }
    }};
}

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

/// Install the frame-copy hook used by page-backed CoW and future byte-copy paths.
pub fn install_frame_copier(copier: FrameCopier) -> Result<(), AllocError> {
    FRAME_COPIER
        .compare_exchange(
            NO_FRAME_COPIER,
            copier as usize,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| AllocError::AlreadyInstalled)
}

/// Install the direct-map kernel-address lookup used by user-buffer copy paths.
pub fn install_frame_kernel_addr(lookup: FrameKernelAddr) -> Result<(), AllocError> {
    FRAME_KERNEL_ADDR
        .compare_exchange(
            NO_FRAME_KERNEL_ADDR,
            lookup as usize,
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
    measure_page_allocator!(b"debug.ds.substrate.page_allocator.reserve_frame", {
        installed_bitmap_allocator()?.reserve_frame(policy)
    })
}

/// Acquire pmap/PTE role evidence for an already-live frame.
///
/// Page cache code uses this after observing a cached PPN and before handing a
/// frame to VM/pmap publication. The returned token owns the `map_count`
/// contribution and releases it on drop.
pub fn acquire_map_pin(
    ppn: Ppn,
) -> Result<MapPin<'static, BitmapPageAllocator<'static>>, AllocError> {
    let allocator = installed_bitmap_allocator()?;
    allocator.acquire_map_pin(ppn)?;
    Ok(MapPin::new(allocator, ppn))
}

/// Acquire page-cache role evidence for an already-live frame.
///
/// Filesystem page fetchers can return a populated frame by PPN; PageBacked
/// uses this helper when it installs that frame into a `PageContainer` index.
pub fn acquire_cache_pin(
    ppn: Ppn,
) -> Result<CachePin<'static, BitmapPageAllocator<'static>>, AllocError> {
    let allocator = installed_bitmap_allocator()?;
    allocator.acquire_cache_pin(ppn)?;
    Ok(CachePin::new(allocator, ppn))
}

/// Acquire DMA/long-term-pin role evidence for an already-live frame.
///
/// Device drivers use this when a frame is handed to hardware and must not be
/// reclaimed until the DMA mapping is torn down. The returned token owns the
/// `pin_count` contribution and releases it on drop.
pub fn acquire_dma_pin(
    ppn: Ppn,
) -> Result<DmaPin<'static, BitmapPageAllocator<'static>>, AllocError> {
    let allocator = installed_bitmap_allocator()?;
    allocator.acquire_dma_pin(ppn)?;
    Ok(DmaPin::new(allocator, ppn))
}

/// Acquire user-page-transfer evidence for an already-live frame.
///
/// VM uses this after materializing an eligible user page and before publishing
/// a `UserPageGift` pipe descriptor. The returned token owns a retained
/// `refcount` contribution and releases it on drop.
pub fn acquire_gift_pin(
    ppn: Ppn,
) -> Result<GiftPin<'static, BitmapPageAllocator<'static>>, AllocError> {
    let allocator = installed_bitmap_allocator()?;
    allocator.acquire_gift_pin(ppn)?;
    Ok(GiftPin::new(allocator, ppn))
}

/// Copy the full contents of one frame to another through the installed direct-map hook.
pub fn copy_frame_contents(source: Ppn, dest: Ppn) -> Result<(), AllocError> {
    let copier = FRAME_COPIER.load(Ordering::Acquire);
    if copier == NO_FRAME_COPIER {
        return Err(AllocError::FrameCopyUnavailable);
    }
    let copier: FrameCopier = unsafe { core::mem::transmute(copier) };
    unsafe { copier(source, dest) };
    Ok(())
}

/// Look up the direct-map kernel virtual address of `ppn`.
///
/// Returns a pointer to the start of the frame. Callers add their own offset.
/// The pointer is valid for the lifetime of the frame; the caller is
/// responsible for not retaining it past frame teardown.
pub fn frame_kernel_addr(ppn: Ppn) -> Result<*mut u8, AllocError> {
    let lookup = FRAME_KERNEL_ADDR.load(Ordering::Acquire);
    if lookup == NO_FRAME_KERNEL_ADDR {
        return Err(AllocError::FrameKernelAddrUnavailable);
    }
    let lookup: FrameKernelAddr = unsafe { core::mem::transmute(lookup) };
    Ok(unsafe { lookup(ppn) })
}

/// Reserve a contiguous frame run from the installed backend.
pub fn reserve_run(
    count: usize,
    align: usize,
    policy: ZeroPolicy,
) -> Result<FrameRunReservation<'static, BitmapPageAllocator<'static>>, AllocError> {
    measure_page_allocator!(b"debug.ds.substrate.page_allocator.reserve_run", {
        installed_bitmap_allocator()?.reserve_run(count, align, policy)
    })
}

/// Reserve a contiguous frame run satisfying one device-selected DMA domain.
///
/// `effective` must be equal to or stricter than the domain's published
/// constraints. Direct translations are checked before reservation; managed
/// domains defer device-address validation to `DmaIf::map_dma`.
pub fn reserve_dma_run(
    domain: &'static DmaDomain,
    effective: DmaConstraints,
    request: DmaRunRequest,
    policy: ZeroPolicy,
) -> Result<FrameRunReservation<'static, BitmapPageAllocator<'static>>, DmaAllocError> {
    measure_page_allocator!(b"debug.ds.substrate.page_allocator.reserve_dma_run", {
        let allocator = installed_bitmap_allocator().map_err(DmaAllocError::Alloc)?;
        allocator.reserve_dma_run(domain, effective, request, policy)
    })
}

/// Free-frame count from the installed backend.
pub fn free_count() -> Result<usize, AllocError> {
    Ok(installed_bitmap_allocator()?.free_count())
}

/// Release one owned frame through the installed bitmap allocator.
///
/// This is for substrate components that intentionally hold a raw PPN after
/// committing an `OwnedFrame` token into their own lifetime protocol.
pub fn release_owned_frame(ppn: Ppn) -> Result<(), AllocError> {
    measure_page_allocator!(b"debug.ds.substrate.page_allocator.release_owned_frame", {
        installed_bitmap_allocator()?.release_owned(ppn);
        Ok(())
    })
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
    measure_page_allocator!(
        b"debug.ds.substrate.page_allocator.reserve_page_table_node",
        {
            let frame = reserve_frame(ZeroPolicy::Zeroed)
                .map_err(|_| tx_hal::AllocError::Exhausted)?
                .commit();
            let pt_frame = frame.into_page_table_frame();
            let phys = PhysAddr(pt_frame.ppn().0 * FRAME_SIZE);
            core::mem::forget(pt_frame);
            Ok(PtNode::typed_frame(phys, release_page_table_node))
        }
    )
}

/// Claim and remember the kernel zero frame.
///
/// Boot calls this after the allocator and direct-map zeroer are installed.
/// The frame is returned as a permanent anchor and intentionally never re-enters
/// the normal allocator pool.
pub fn claim_zero_frame() -> Result<Ppn, AllocError> {
    measure_page_allocator!(b"debug.ds.substrate.page_allocator.claim_zero_frame", {
        if ZERO_FRAME_PPN.load(Ordering::Acquire) != NO_ZERO_FRAME {
            return Err(AllocError::AlreadyInstalled);
        }
        let frame = reserve_frame(ZeroPolicy::Zeroed)?.commit();
        let ppn = frame.ppn();
        match ZERO_FRAME_PPN.compare_exchange(
            NO_ZERO_FRAME,
            ppn.0,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                let _anchor = frame.into_permanent_frame();
                Ok(ppn)
            }
            Err(_) => {
                drop(frame);
                Err(AllocError::AlreadyInstalled)
            }
        }
    })
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
    measure_page_allocator!(
        b"debug.ds.substrate.page_allocator.claim_permanent_frame",
        { installed_bitmap_allocator()?.claim_permanent_frame(ppn) }
    )
}

unsafe fn release_page_table_node(phys: PhysAddr) {
    if let Ok(allocator) = installed_bitmap_allocator() {
        allocator.release_page_table_frame(Ppn(phys.0 / FRAME_SIZE));
    }
}

/// Backend diagnostics from the installed allocator.
pub fn backend_diagnostics() -> Result<AllocatorDiagnostics, AllocError> {
    Ok(installed_bitmap_allocator()?.backend_diagnostics())
}

/// Best-effort frame-role accounting from the installed allocator.
///
/// This performs a full metadata scan and is intended for panic/diagnostic
/// paths, not the allocation fast path.
pub fn frame_role_diagnostics() -> Result<FrameRoleDiagnostics, AllocError> {
    Ok(installed_bitmap_allocator()?.frame_role_diagnostics())
}

#[doc(hidden)]
pub mod testing {
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use tx_hal::Ppn;

    use super::{
        install_bitmap_allocator, install_frame_copier, install_frame_kernel_addr, AllocError,
        BitmapPageAllocator, FrameMeta,
    };

    const TEST_FRAME_COUNT: usize = 1024;
    const TEST_BITMAP_WORDS: usize = TEST_FRAME_COUNT / 64;

    static INITIALIZED: AtomicBool = AtomicBool::new(false);
    static TEST_METAS: [FrameMeta; TEST_FRAME_COUNT] =
        [const { FrameMeta::new() }; TEST_FRAME_COUNT];
    static TEST_BITMAP: [AtomicU64; TEST_BITMAP_WORDS] =
        [const { AtomicU64::new(0) }; TEST_BITMAP_WORDS];
    static TEST_ALLOCATOR: BitmapPageAllocator<'static> = BitmapPageAllocator::new_with_zeroer(
        &TEST_METAS,
        &TEST_BITMAP,
        TEST_FRAME_COUNT,
        zero_for_test,
    );

    unsafe fn zero_for_test(ppn: Ppn) {
        unsafe {
            core::ptr::write_bytes(test_frame_ptr(ppn, 0), 0, 4096);
        }
    }

    unsafe fn copy_for_test(source: Ppn, dest: Ppn) {
        unsafe {
            core::ptr::copy_nonoverlapping(
                test_frame_ptr(source, 0),
                test_frame_ptr(dest, 0),
                4096,
            );
        }
    }

    unsafe fn kernel_addr_for_test(ppn: Ppn) -> *mut u8 {
        unsafe { test_frame_ptr(ppn, 0) }
    }

    pub fn install_test_allocator_once() -> Result<(), AllocError> {
        if INITIALIZED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            for ppn in 0..TEST_FRAME_COUNT {
                TEST_ALLOCATOR.mark_free_for_test(Ppn(ppn));
            }
            install_bitmap_allocator(&TEST_ALLOCATOR)?;
            install_frame_copier(copy_for_test)?;
            install_frame_kernel_addr(kernel_addr_for_test)?;
            return Ok(());
        }

        Ok(())
    }

    pub fn direct_map_base_for_test() -> usize {
        TEST_DIRECT_MAP.0.get() as *mut u8 as usize
    }

    pub fn write_frame_bytes_for_test(ppn: Ppn, offset: usize, bytes: &[u8]) {
        assert!(offset <= 4096);
        assert!(bytes.len() <= 4096 - offset);
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                test_frame_ptr(ppn, offset),
                bytes.len(),
            );
        }
    }

    pub fn read_frame_bytes_for_test(ppn: Ppn, offset: usize, dest: &mut [u8]) {
        assert!(offset <= 4096);
        assert!(dest.len() <= 4096 - offset);
        unsafe {
            core::ptr::copy_nonoverlapping(
                test_frame_ptr(ppn, offset),
                dest.as_mut_ptr(),
                dest.len(),
            );
        }
    }

    unsafe fn test_frame_ptr(ppn: Ppn, offset: usize) -> *mut u8 {
        assert!(ppn.0 < TEST_FRAME_COUNT);
        assert!(offset <= 4096);
        unsafe {
            (*TEST_DIRECT_MAP.0.get())
                .as_mut_ptr()
                .add(ppn.0 * 4096 + offset)
        }
    }

    #[repr(align(4096))]
    struct DirectMap(UnsafeCell<[u8; TEST_FRAME_COUNT * 4096]>);

    unsafe impl Sync for DirectMap {}

    static TEST_DIRECT_MAP: DirectMap = DirectMap(UnsafeCell::new([0; TEST_FRAME_COUNT * 4096]));
}
