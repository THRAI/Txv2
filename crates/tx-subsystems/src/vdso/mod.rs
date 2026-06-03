//! vDSO image data — shared between tx-kernel (init) and tx-scripts (exec).
//!
//! The kernel calls [`init_vdso`] once during early boot to allocate
//! dedicated frames for the vDSO image and the VVAR page.  The
//! exec-script path reads the frames through [`kernel_vdso`] /
//! [`vdso_available`] and [`vvar_ppn`].
//!
//! ## VVAR design
//!
//! The VVAR page is a dedicated, page-aligned kernel frame mapped
//! read-only into every user address space.  It carries a seqlock-
//! guarded snapshot of clock conversion state and realtime/monotonic
//! basetimes. User-mode vDSO code reads the hardware counter and
//! computes current time from that snapshot without trapping.

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::vec::Vec;
use tx_substrate::page_allocator;

use crate::wall_clock::VvarSnapshot;

// ---------------------------------------------------------------------------
// VVAR page
// ---------------------------------------------------------------------------

/// Seqlock-guarded time page mapped read-only into every user address space.
#[repr(C, align(4096))]
pub struct VvarPage {
    pub seq: AtomicU64,
    pub realtime_sec: AtomicU64,
    pub realtime_nsec: AtomicU64,
    pub monotonic_sec: AtomicU64,
    pub monotonic_nsec: AtomicU64,
    pub cycle_last: AtomicU64,
    pub mult: AtomicU64,
    pub shift: AtomicU64,
    pub mask: AtomicU64,
    pub _reserved: [u8; 4024],
}

unsafe impl Sync for VvarPage {}

impl Default for VvarPage {
    fn default() -> Self {
        Self::new()
    }
}

impl VvarPage {
    pub const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            realtime_sec: AtomicU64::new(0),
            realtime_nsec: AtomicU64::new(0),
            monotonic_sec: AtomicU64::new(0),
            monotonic_nsec: AtomicU64::new(0),
            cycle_last: AtomicU64::new(0),
            mult: AtomicU64::new(0),
            shift: AtomicU64::new(0),
            mask: AtomicU64::new(!0u64),
            _reserved: [0u8; 4024],
        }
    }

    pub fn init_clock_params(&self, timebase_hz: u64) {
        // Test platforms (and any boot path that has not yet probed the
        // timebase) can call this with `timebase_hz = 0`. Guard against
        // div-by-zero and leave the params at their default zeros — the
        // vDSO clock path treats `mult == 0` as "uninitialised" and
        // falls back to the syscall lane.
        if timebase_hz == 0 {
            return;
        }
        const NSEC_PER_SEC: u64 = 1_000_000_000;
        let max_mult: u64 = (1u64 << 32) - 1;
        let mut shift: u64 = 0;
        while ((NSEC_PER_SEC << shift) / timebase_hz) > max_mult && shift < 60 {
            shift += 1;
        }
        let mult = ((NSEC_PER_SEC << shift) as u128 / timebase_hz as u128) as u64;
        self.mult.store(mult, Ordering::Relaxed);
        self.shift.store(shift, Ordering::Relaxed);
        self.mask.store(!0u64, Ordering::Relaxed);
        crate::wall_clock::set_clock_params(mult, shift, !0u64);
    }

    pub fn update(&self, realtime: (u64, u64), monotonic: (u64, u64)) {
        let shift = self.shift.load(Ordering::Acquire);
        let snapshot = VvarSnapshot {
            cycle_last: read_cycle_counter(),
            mask: self.mask.load(Ordering::Acquire),
            mult: self.mult.load(Ordering::Acquire),
            shift,
            realtime_sec: realtime.0,
            realtime_nsec_shifted: realtime.1 << shift,
            monotonic_sec: monotonic.0,
            monotonic_nsec_shifted: monotonic.1 << shift,
        };
        self.update_from_snapshot(snapshot);
    }

    pub fn update_from_snapshot(&self, snapshot: VvarSnapshot) {
        self.seq.fetch_add(1, Ordering::Relaxed);
        core::sync::atomic::fence(Ordering::Release);
        self.realtime_sec
            .store(snapshot.realtime_sec, Ordering::Relaxed);
        self.realtime_nsec
            .store(snapshot.realtime_nsec_shifted, Ordering::Relaxed);
        self.monotonic_sec
            .store(snapshot.monotonic_sec, Ordering::Relaxed);
        self.monotonic_nsec
            .store(snapshot.monotonic_nsec_shifted, Ordering::Relaxed);
        self.cycle_last
            .store(snapshot.cycle_last, Ordering::Relaxed);
        self.mult.store(snapshot.mult, Ordering::Relaxed);
        self.shift.store(snapshot.shift, Ordering::Relaxed);
        self.mask.store(snapshot.mask, Ordering::Relaxed);
        core::sync::atomic::fence(Ordering::Release);
        self.seq.fetch_add(1, Ordering::Release);
    }
}

#[cfg(target_arch = "riscv64")]
fn read_cycle_counter() -> u64 {
    let now: u64;
    unsafe {
        core::arch::asm!("rdtime {t}", t = out(reg) now, options(nomem, nostack));
    }
    now
}

#[cfg(not(target_arch = "riscv64"))]
fn read_cycle_counter() -> u64 {
    0
}

// ---------------------------------------------------------------------------
// Global singletons
// ---------------------------------------------------------------------------

pub struct KernelVdso {
    pub num_pages: usize,
    pub frames: &'static [tx_hal::Ppn],
}

static mut KERNEL_VDSO: Option<KernelVdso> = None;
static mut VVAR_PPN: Option<tx_hal::Ppn> = None;
static mut VVAR_PTR: *const VvarPage = core::ptr::null();

pub fn init_vdso() -> Result<(), VdsoInitError> {
    let image = tx_vdso::VDSO_IMAGE;
    let num_pages = tx_vdso::VDSO_NUM_PAGES;

    if !tx_vdso::VDSO_AVAILABLE || image.is_empty() {
        return Err(VdsoInitError::ImageNotAvailable);
    }

    let mut frames: Vec<tx_hal::Ppn> = Vec::with_capacity(num_pages);
    for page_idx in 0..num_pages {
        let start = page_idx * 4096;
        let end = core::cmp::min(start + 4096, image.len());
        let slice = &image[start..end];

        let owned = page_allocator::reserve_frame(page_allocator::ZeroPolicy::UninitFullOverwrite)
            .map_err(|_| VdsoInitError::Alloc)?
            .commit();
        let ppn = owned.ppn();
        let dst = page_allocator::frame_kernel_addr(ppn).map_err(|_| VdsoInitError::DirectMap)?;
        unsafe {
            core::ptr::copy_nonoverlapping(slice.as_ptr(), dst, slice.len());
            if slice.len() < 4096 {
                core::ptr::write_bytes(dst.add(slice.len()), 0, 4096 - slice.len());
            }
        }
        let _permanent = owned.into_permanent_frame();
        frames.push(ppn);
    }

    let vvar_owned = page_allocator::reserve_frame(page_allocator::ZeroPolicy::Zeroed)
        .map_err(|_| VdsoInitError::Alloc)?
        .commit();
    let vvar_ppn = vvar_owned.ppn();
    let vvar_ptr: *mut VvarPage = page_allocator::frame_kernel_addr(vvar_ppn)
        .map_err(|_| VdsoInitError::DirectMap)? as *mut VvarPage;
    unsafe {
        core::ptr::write(vvar_ptr, VvarPage::new());
    }
    let _vvar_permanent = vvar_owned.into_permanent_frame();

    unsafe {
        KERNEL_VDSO = Some(KernelVdso {
            num_pages,
            frames: frames.leak(),
        });
        VVAR_PPN = Some(vvar_ppn);
        VVAR_PTR = vvar_ptr;
    }
    Ok(())
}

pub fn kernel_vdso() -> &'static KernelVdso {
    #[allow(static_mut_refs)]
    unsafe { KERNEL_VDSO.as_ref() }.expect("kernel_vdso() called before init_vdso()")
}

pub fn vvar_ppn() -> tx_hal::Ppn {
    unsafe { VVAR_PPN }.expect("vvar_ppn() called before init_vdso()")
}

pub fn vvar_page() -> &'static VvarPage {
    unsafe { &*VVAR_PTR }
}

pub fn vdso_available() -> bool {
    #[allow(static_mut_refs)]
    {
        tx_vdso::VDSO_AVAILABLE
            && !tx_vdso::VDSO_IMAGE.is_empty()
            && unsafe { KERNEL_VDSO.is_some() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vvar_update_snapshot_publishes_shifted_bases_without_recomputing_cycle_last() {
        let page = VvarPage::new();
        let snapshot = VvarSnapshot {
            cycle_last: 42,
            mask: !0,
            mult: 2,
            shift: 3,
            realtime_sec: 10,
            realtime_nsec_shifted: 123 << 3,
            monotonic_sec: 5,
            monotonic_nsec_shifted: 456 << 3,
        };

        page.update_from_snapshot(snapshot);

        assert_eq!(page.cycle_last.load(Ordering::Acquire), 42);
        assert_eq!(page.mult.load(Ordering::Acquire), 2);
        assert_eq!(page.shift.load(Ordering::Acquire), 3);
        assert_eq!(page.realtime_nsec.load(Ordering::Acquire), 123 << 3);
        assert_eq!(page.monotonic_nsec.load(Ordering::Acquire), 456 << 3);
        assert_eq!(page.seq.load(Ordering::Acquire) % 2, 0);
    }
}

#[derive(Debug)]
pub enum VdsoInitError {
    ImageNotAvailable,
    Alloc,
    DirectMap,
}
