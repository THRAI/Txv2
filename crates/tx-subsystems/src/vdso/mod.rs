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

use core::{
    ptr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use alloc::vec::Vec;
use tx_services::time::{timekeeper, TimekeeperIf};
use tx_substrate::page_allocator;
use tx_time::vdso::{VdsoClockMode, VvarData, VvarSnapshot};

// ---------------------------------------------------------------------------
// VVAR page
// ---------------------------------------------------------------------------

/// Serializes writes to the one mapped [`VvarData`] frame.
///
/// The writer state deliberately lives outside the frame: `VvarData` is the
/// complete user-visible ABI, while this type is kernel-only lifecycle state.
pub struct VvarPublisher {
    writer_locked: AtomicBool,
    applied_realtime_generation: AtomicU64,
}

impl VvarPublisher {
    pub const fn new() -> Self {
        Self {
            writer_locked: AtomicBool::new(false),
            applied_realtime_generation: AtomicU64::new(0),
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
        timekeeper().set_clock_params(mult, shift, !0u64);
    }

    pub fn update(&self, realtime: (u64, u64), monotonic: (u64, u64)) {
        let data = vvar_data();
        let shift = unsafe { ptr::read_volatile(ptr::addr_of!(data.shift)) };
        let snapshot = VvarSnapshot {
            realtime_generation: self.applied_realtime_generation.load(Ordering::Acquire),
            cycle_last: read_cycle_counter(),
            mask: unsafe { ptr::read_volatile(ptr::addr_of!(data.mask)) },
            mult: unsafe { ptr::read_volatile(ptr::addr_of!(data.mult)) },
            shift,
            realtime_sec: realtime.0,
            realtime_nsec_shifted: realtime.1 << shift,
            monotonic_sec: monotonic.0,
            monotonic_nsec_shifted: monotonic.1 << shift,
        };
        self.update_from_snapshot(snapshot);
    }

    pub fn update_from_snapshot(&self, snapshot: VvarSnapshot) {
        let data = vvar_data() as *const VvarData as *mut VvarData;
        unsafe { self.publish_to_raw(data, snapshot) };
    }

    #[cfg(test)]
    fn publish_to(&self, data: &mut VvarData, snapshot: VvarSnapshot) {
        unsafe { self.publish_to_raw(data, snapshot) };
    }

    unsafe fn publish_to_raw(&self, data: *mut VvarData, snapshot: VvarSnapshot) {
        self.lock_writer();
        if snapshot.realtime_generation < self.applied_realtime_generation.load(Ordering::Acquire) {
            self.unlock_writer();
            return;
        }

        // `cycle_last == 0` is the timekeeper's non-Ready counter marker. A
        // missing fast counter is never exposed as a usable vDSO clock mode.
        let clock_mode = if snapshot.cycle_last == 0 {
            VdsoClockMode::Syscall
        } else {
            VdsoClockMode::RiscvTime
        };
        let payload = VvarData::from_snapshot(snapshot, clock_mode);
        let seq = ptr::addr_of_mut!((*data).seq);
        let odd = ptr::read_volatile(seq).wrapping_add(1) | 1;

        ptr::write_volatile(seq, odd);
        core::sync::atomic::fence(Ordering::Release);

        ptr::write_volatile(ptr::addr_of_mut!((*data).abi_version), payload.abi_version);
        ptr::write_volatile(ptr::addr_of_mut!((*data).clock_mode), payload.clock_mode);
        ptr::write_volatile(
            ptr::addr_of_mut!((*data).realtime_sec),
            payload.realtime_sec,
        );
        ptr::write_volatile(
            ptr::addr_of_mut!((*data).realtime_nsec_shifted),
            payload.realtime_nsec_shifted,
        );
        ptr::write_volatile(
            ptr::addr_of_mut!((*data).monotonic_sec),
            payload.monotonic_sec,
        );
        ptr::write_volatile(
            ptr::addr_of_mut!((*data).monotonic_nsec_shifted),
            payload.monotonic_nsec_shifted,
        );
        ptr::write_volatile(ptr::addr_of_mut!((*data).cycle_last), payload.cycle_last);
        ptr::write_volatile(ptr::addr_of_mut!((*data).mult), payload.mult);
        ptr::write_volatile(ptr::addr_of_mut!((*data).shift), payload.shift);
        ptr::write_volatile(ptr::addr_of_mut!((*data).mask), payload.mask);
        ptr::write_volatile(
            ptr::addr_of_mut!((*data).realtime_generation),
            payload.realtime_generation,
        );
        self.applied_realtime_generation
            .store(snapshot.realtime_generation, Ordering::Release);
        core::sync::atomic::fence(Ordering::Release);
        ptr::write_volatile(seq, odd.wrapping_add(1));
        self.unlock_writer();
    }

    pub fn applied_realtime_generation(&self) -> u64 {
        self.applied_realtime_generation.load(Ordering::Acquire)
    }

    fn lock_writer(&self) {
        while self
            .writer_locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    fn unlock_writer(&self) {
        self.writer_locked.store(false, Ordering::Release);
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
static mut VVAR_PTR: *const VvarData = core::ptr::null();
static VVAR_PUBLISHER: VvarPublisher = VvarPublisher::new();

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
    let vvar_ptr: *mut VvarData = page_allocator::frame_kernel_addr(vvar_ppn)
        .map_err(|_| VdsoInitError::DirectMap)? as *mut VvarData;
    unsafe {
        core::ptr::write(
            vvar_ptr,
            VvarData::from_snapshot(
                VvarSnapshot {
                    realtime_generation: 0,
                    cycle_last: 0,
                    mask: !0,
                    mult: 0,
                    shift: 0,
                    realtime_sec: 0,
                    realtime_nsec_shifted: 0,
                    monotonic_sec: 0,
                    monotonic_nsec_shifted: 0,
                },
                VdsoClockMode::Syscall,
            ),
        );
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

/// Compatibility publisher surface for existing kernel/time hook call sites.
///
/// This is not the mapped ABI frame. New VM code should use [`vvar_data`] and
/// [`vvar_ppn`] when it needs the VVAR mapping contract.
pub fn vvar_page() -> &'static VvarPublisher {
    &VVAR_PUBLISHER
}

/// The sole VVAR ABI frame owned by the kernel vDSO lifecycle.
pub fn vvar_data() -> &'static VvarData {
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
    use tx_time::vdso::{VdsoClockMode, VvarData, VVAR_ABI_VERSION};

    #[test]
    fn publisher_writes_the_shared_vvar_abi_with_an_even_sequence() {
        let publisher = VvarPublisher::new();
        let mut data = VvarData::from_snapshot(
            VvarSnapshot {
                realtime_generation: 3,
                cycle_last: 42,
                mask: !0,
                mult: 2,
                shift: 3,
                realtime_sec: 10,
                realtime_nsec_shifted: 123 << 3,
                monotonic_sec: 5,
                monotonic_nsec_shifted: 456 << 3,
            },
            VdsoClockMode::Syscall,
        );

        publisher.publish_to(
            &mut data,
            VvarSnapshot {
                realtime_generation: 4,
                cycle_last: 99,
                mask: !0,
                mult: 7,
                shift: 2,
                realtime_sec: 20,
                realtime_nsec_shifted: 321 << 2,
                monotonic_sec: 8,
                monotonic_nsec_shifted: 654 << 2,
            },
        );

        assert_eq!(data.seq, 2);
        assert_eq!(data.abi_version, VVAR_ABI_VERSION);
        assert_eq!(data.clock_mode, VdsoClockMode::RiscvTime as u32);
        assert_eq!(data.cycle_last, 99);
        assert_eq!(data.realtime_generation, 4);
        assert_eq!(data.realtime_nsec_shifted, 321 << 2);
        assert_eq!(data.monotonic_nsec_shifted, 654 << 2);
    }

    #[test]
    fn publisher_uses_syscall_mode_when_counter_is_not_eligible() {
        let publisher = VvarPublisher::new();
        let mut data = VvarData::from_snapshot(
            VvarSnapshot {
                realtime_generation: 0,
                cycle_last: 1,
                mask: !0,
                mult: 1,
                shift: 0,
                realtime_sec: 0,
                realtime_nsec_shifted: 0,
                monotonic_sec: 0,
                monotonic_nsec_shifted: 0,
            },
            VdsoClockMode::RiscvTime,
        );

        publisher.publish_to(
            &mut data,
            VvarSnapshot {
                realtime_generation: 1,
                cycle_last: 0,
                mask: !0,
                mult: 1,
                shift: 0,
                realtime_sec: 1,
                realtime_nsec_shifted: 0,
                monotonic_sec: 1,
                monotonic_nsec_shifted: 0,
            },
        );

        assert_eq!(data.clock_mode, VdsoClockMode::Syscall as u32);
        assert_eq!(data.seq % 2, 0);
    }

    #[test]
    fn vvar_update_snapshot_publishes_shifted_bases_without_recomputing_cycle_last() {
        let publisher = VvarPublisher::new();
        let mut data = VvarData::from_snapshot(
            VvarSnapshot {
                realtime_generation: 0,
                cycle_last: 0,
                mask: !0,
                mult: 0,
                shift: 0,
                realtime_sec: 0,
                realtime_nsec_shifted: 0,
                monotonic_sec: 0,
                monotonic_nsec_shifted: 0,
            },
            VdsoClockMode::Syscall,
        );
        let snapshot = VvarSnapshot {
            realtime_generation: 0,
            cycle_last: 42,
            mask: !0,
            mult: 2,
            shift: 3,
            realtime_sec: 10,
            realtime_nsec_shifted: 123 << 3,
            monotonic_sec: 5,
            monotonic_nsec_shifted: 456 << 3,
        };

        publisher.publish_to(&mut data, snapshot);

        assert_eq!(data.cycle_last, 42);
        assert_eq!(data.mult, 2);
        assert_eq!(data.shift, 3);
        assert_eq!(data.realtime_nsec_shifted, 123 << 3);
        assert_eq!(data.monotonic_nsec_shifted, 456 << 3);
        assert_eq!(data.seq % 2, 0);
    }

    #[test]
    fn delayed_older_snapshot_cannot_overwrite_newer_realtime_generation() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;

        let publisher = Arc::new(VvarPublisher::new());
        let data = Arc::new(std::sync::Mutex::new(VvarData::from_snapshot(
            VvarSnapshot {
                realtime_generation: 0,
                cycle_last: 0,
                mask: !0,
                mult: 0,
                shift: 0,
                realtime_sec: 0,
                realtime_nsec_shifted: 0,
                monotonic_sec: 0,
                monotonic_nsec_shifted: 0,
            },
            VdsoClockMode::Syscall,
        )));
        let newer_applied = Arc::new(AtomicBool::new(false));
        let older = VvarSnapshot {
            realtime_generation: 7,
            cycle_last: 7,
            mask: !0,
            mult: 1,
            shift: 0,
            realtime_sec: 70,
            realtime_nsec_shifted: 7,
            monotonic_sec: 7,
            monotonic_nsec_shifted: 7,
        };
        let newer = VvarSnapshot {
            realtime_generation: 8,
            cycle_last: 8,
            mask: !0,
            mult: 1,
            shift: 0,
            realtime_sec: 80,
            realtime_nsec_shifted: 8,
            monotonic_sec: 8,
            monotonic_nsec_shifted: 8,
        };

        std::thread::scope(|scope| {
            let publisher_for_old = Arc::clone(&publisher);
            let data_for_old = Arc::clone(&data);
            let newer_for_old = Arc::clone(&newer_applied);
            scope.spawn(move || {
                while !newer_for_old.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
                publisher_for_old.publish_to(&mut data_for_old.lock().unwrap(), older);
            });

            publisher.publish_to(&mut data.lock().unwrap(), newer);
            newer_applied.store(true, Ordering::Release);
        });

        let data = data.lock().unwrap();
        assert_eq!(publisher.applied_realtime_generation(), 8);
        assert_eq!(data.realtime_sec, 80);
        assert_eq!(data.realtime_nsec_shifted, 8);
    }

    fn snapshot_for_generation(generation: u64) -> VvarSnapshot {
        VvarSnapshot {
            realtime_generation: generation,
            cycle_last: generation ^ 0x55aa_55aa_55aa_55aa,
            mask: !0,
            mult: 11,
            shift: 0,
            realtime_sec: generation,
            realtime_nsec_shifted: generation.wrapping_mul(3),
            monotonic_sec: generation.wrapping_mul(5),
            monotonic_nsec_shifted: generation.wrapping_mul(7),
        }
    }

    unsafe fn read_stable_generation(data: *const VvarData) -> Option<u64> {
        let first = unsafe { ptr::read_volatile(ptr::addr_of!((*data).seq)) };
        if first & 1 != 0 {
            return None;
        }
        core::sync::atomic::fence(Ordering::Acquire);
        let generation = unsafe { ptr::read_volatile(ptr::addr_of!((*data).realtime_generation)) };
        let cycle_last = unsafe { ptr::read_volatile(ptr::addr_of!((*data).cycle_last)) };
        let realtime_sec = unsafe { ptr::read_volatile(ptr::addr_of!((*data).realtime_sec)) };
        let realtime_nsec =
            unsafe { ptr::read_volatile(ptr::addr_of!((*data).realtime_nsec_shifted)) };
        let monotonic_sec = unsafe { ptr::read_volatile(ptr::addr_of!((*data).monotonic_sec)) };
        let monotonic_nsec =
            unsafe { ptr::read_volatile(ptr::addr_of!((*data).monotonic_nsec_shifted)) };
        core::sync::atomic::fence(Ordering::Acquire);
        let second = unsafe { ptr::read_volatile(ptr::addr_of!((*data).seq)) };
        if first != second || second & 1 != 0 {
            return None;
        }
        assert_eq!(cycle_last, generation ^ 0x55aa_55aa_55aa_55aa);
        assert_eq!(realtime_sec, generation);
        assert_eq!(realtime_nsec, generation.wrapping_mul(3));
        assert_eq!(monotonic_sec, generation.wrapping_mul(5));
        assert_eq!(monotonic_nsec, generation.wrapping_mul(7));
        Some(generation)
    }

    #[test]
    fn concurrent_vvar_readers_accept_only_complete_snapshots_and_finish_even() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;

        const READERS: usize = 4;
        const UPDATES: u64 = 20_000;
        const POST_COMPLETE_READS: usize = 512;

        let publisher = Arc::new(VvarPublisher::new());
        let data = Arc::new(VvarData::from_snapshot(
            snapshot_for_generation(0),
            VdsoClockMode::RiscvTime,
        ));
        let start = Arc::new(AtomicBool::new(false));
        let complete = Arc::new(AtomicBool::new(false));

        std::thread::scope(|scope| {
            let writer_publisher = Arc::clone(&publisher);
            let writer_data = Arc::clone(&data);
            let writer_start = Arc::clone(&start);
            let writer_complete = Arc::clone(&complete);
            scope.spawn(move || {
                while !writer_start.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
                for generation in 1..=UPDATES {
                    unsafe {
                        writer_publisher.publish_to_raw(
                            Arc::as_ptr(&writer_data) as *mut VvarData,
                            snapshot_for_generation(generation),
                        );
                    }
                }
                writer_complete.store(true, Ordering::Release);
            });

            for _ in 0..READERS {
                let reader_data = Arc::clone(&data);
                let reader_start = Arc::clone(&start);
                let reader_complete = Arc::clone(&complete);
                scope.spawn(move || {
                    while !reader_start.load(Ordering::Acquire) {
                        core::hint::spin_loop();
                    }
                    let mut accepted_after_complete = 0;
                    while !reader_complete.load(Ordering::Acquire)
                        || accepted_after_complete < POST_COMPLETE_READS
                    {
                        if unsafe { read_stable_generation(Arc::as_ptr(&reader_data)) }.is_some()
                            && reader_complete.load(Ordering::Acquire)
                        {
                            accepted_after_complete += 1;
                        }
                    }
                });
            }

            start.store(true, Ordering::Release);
        });

        assert_eq!(publisher.applied_realtime_generation(), UPDATES);
        let final_seq = unsafe { ptr::read_volatile(ptr::addr_of!(data.seq)) };
        assert_eq!(
            final_seq & 1,
            0,
            "writer must not leave VVAR permanently odd"
        );
        assert_eq!(
            unsafe { read_stable_generation(Arc::as_ptr(&data)) },
            Some(UPDATES)
        );
    }
}

#[derive(Debug)]
pub enum VdsoInitError {
    ImageNotAvailable,
    Alloc,
    DirectMap,
}
