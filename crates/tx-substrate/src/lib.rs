#![no_std]
#![feature(associated_type_defaults)]

extern crate alloc;
#[cfg(test)]
extern crate std;

use tx_hal::{CpuId, TxPlatform};

#[doc(hidden)]
pub mod boot_memory;

pub mod bitmap {
    pub struct BitmapReservation;
}

pub mod bus;

#[cfg(tx_ds_metrics)]
pub mod ds_metrics;
pub mod epoch;
pub mod index;
pub mod mutation;

pub mod pmap {
    pub use tx_hal::pmap::*;
}

pub mod page_allocator;
pub mod slab;
pub mod slot;
pub mod step;
pub mod sync;
pub mod verbs;
pub mod wake;
pub mod zone;

pub use slot::AtomicSlot;
pub use sync::{LockMetricsOff, LockMetricsOn, SpinMutex, SpinMutexGuard, SpinWait};

#[doc(hidden)]
pub mod testing {
    use core::sync::atomic::{AtomicUsize, Ordering};

    const UNINITIALIZED: usize = 0;
    const INITIALIZING: usize = 1;
    const INITIALIZED: usize = 2;
    static STATE: AtomicUsize = AtomicUsize::new(UNINITIALIZED);

    pub fn init_host_for_test_once() {
        match STATE.compare_exchange(
            UNINITIALIZED,
            INITIALIZING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(INITIALIZED) => return,
            Err(_) => {
                while STATE.load(Ordering::Acquire) != INITIALIZED {
                    core::hint::spin_loop();
                }
                return;
            }
        }

        crate::page_allocator::testing::install_test_allocator_once()
            .expect("test page allocator install");
        unsafe {
            crate::epoch::testing::reset_for_test();
            crate::zone::testing::reset_for_test();
        }
        crate::epoch::testing::init_for_test();
        crate::zone::testing::init_for_test(
            4096,
            crate::page_allocator::testing::direct_map_base_for_test(),
        )
        .expect("test zone runtime init");
        STATE.store(INITIALIZED, Ordering::Release);
    }
}

pub mod page {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct PageSize(pub usize);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct PhysFrame {
        pub number: usize,
    }
}

pub mod reservation {
    pub struct ReservationToken {
        _private: (),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApInitError {
    Epoch(epoch::EpochError),
    Zone(zone::ZoneError),
}

impl From<epoch::EpochError> for ApInitError {
    fn from(value: epoch::EpochError) -> Self {
        Self::Epoch(value)
    }
}

impl From<zone::ZoneError> for ApInitError {
    fn from(value: zone::ZoneError) -> Self {
        Self::Zone(value)
    }
}

pub mod shootdown {
    use core::fmt;
    use core::marker::PhantomData;
    use core::mem::MaybeUninit;
    use tx_hal::{
        Asid, PhysAddr, PmapIf, PmapInvalidation, PmapReserveKind, PmapUnmapResult, Ppn, VirtAddr,
    };

    use crate::page_allocator::{MapPin, MapPinRun, PageAllocator};

    const PAGE_SIZE_4K: usize = 4096;

    pub struct ShootdownToken;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ShootdownError {
        Full,
        UnsupportedMapping,
        MismatchedFrame,
    }

    pub struct ShootdownPushError<'a, A: PageAllocator> {
        reason: ShootdownError,
        map_pin: MapPin<'a, A>,
    }

    impl<'a, A: PageAllocator> ShootdownPushError<'a, A> {
        pub fn reason(&self) -> ShootdownError {
            self.reason
        }

        pub fn into_map_pin(self) -> MapPin<'a, A> {
            self.map_pin
        }
    }

    impl<A: PageAllocator> fmt::Debug for ShootdownPushError<'_, A> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("ShootdownPushError")
                .field("reason", &self.reason)
                .field("map_pin", &self.map_pin)
                .finish()
        }
    }

    pub struct ShootdownRunPushError<'a, A: PageAllocator> {
        reason: ShootdownError,
        map_pin_run: MapPinRun<'a, A>,
    }

    impl<'a, A: PageAllocator> ShootdownRunPushError<'a, A> {
        pub fn reason(&self) -> ShootdownError {
            self.reason
        }

        pub fn into_map_pin_run(self) -> MapPinRun<'a, A> {
            self.map_pin_run
        }
    }

    impl<A: PageAllocator> fmt::Debug for ShootdownRunPushError<'_, A> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("ShootdownRunPushError")
                .field("reason", &self.reason)
                .field("map_pin_run", &self.map_pin_run)
                .finish()
        }
    }

    enum PendingMapReleaseToken<'a, A: PageAllocator> {
        Page(MapPin<'a, A>),
        Run(MapPinRun<'a, A>),
    }

    impl<A: PageAllocator> PendingMapReleaseToken<'_, A> {
        fn release(self) {
            match self {
                Self::Page(map_pin) => drop(map_pin),
                Self::Run(map_pin_run) => drop(map_pin_run),
            }
        }
    }

    struct PendingMapRelease<'a, A: PageAllocator> {
        result: PmapUnmapResult,
        token: PendingMapReleaseToken<'a, A>,
    }

    pub struct KernelShootdownBatch<'a, A: PageAllocator, const N: usize> {
        entries: [MaybeUninit<PendingMapRelease<'a, A>>; N],
        len: usize,
        _not_send: PhantomData<*const ()>,
    }

    impl<'a, A: PageAllocator, const N: usize> KernelShootdownBatch<'a, A, N> {
        pub fn new() -> Self {
            Self {
                entries: [const { MaybeUninit::uninit() }; N],
                len: 0,
                _not_send: PhantomData,
            }
        }

        pub fn len(&self) -> usize {
            self.len
        }

        pub fn is_empty(&self) -> bool {
            self.len == 0
        }

        pub fn push_page_unmap_result(
            &mut self,
            result: PmapUnmapResult,
            map_pin: MapPin<'a, A>,
        ) -> Result<(), ShootdownPushError<'a, A>> {
            if result.kind() != PmapReserveKind::Page4K
                || !result.phys().0.is_multiple_of(PAGE_SIZE_4K)
            {
                return Err(ShootdownPushError {
                    reason: ShootdownError::UnsupportedMapping,
                    map_pin,
                });
            }

            if result_ppn(result.phys()) != map_pin.ppn() {
                return Err(ShootdownPushError {
                    reason: ShootdownError::MismatchedFrame,
                    map_pin,
                });
            }

            if self.len == N {
                return Err(ShootdownPushError {
                    reason: ShootdownError::Full,
                    map_pin,
                });
            }

            self.entries[self.len].write(PendingMapRelease {
                result,
                token: PendingMapReleaseToken::Page(map_pin),
            });
            self.len += 1;
            Ok(())
        }

        pub fn push_unmap_result(
            &mut self,
            result: PmapUnmapResult,
            map_pin_run: MapPinRun<'a, A>,
        ) -> Result<(), ShootdownRunPushError<'a, A>> {
            if result.page_count() != map_pin_run.count() || result.base_ppn() != map_pin_run.base()
            {
                return Err(ShootdownRunPushError {
                    reason: ShootdownError::MismatchedFrame,
                    map_pin_run,
                });
            }

            if self.len == N {
                return Err(ShootdownRunPushError {
                    reason: ShootdownError::Full,
                    map_pin_run,
                });
            }

            self.entries[self.len].write(PendingMapRelease {
                result,
                token: PendingMapReleaseToken::Run(map_pin_run),
            });
            self.len += 1;
            Ok(())
        }

        pub fn issue_and_release<P: PmapIf>(mut self) {
            let len = self.len;

            let mut invalidations = [PmapInvalidation::new(VirtAddr(0), 0); N];
            for (index, slot) in invalidations.iter_mut().enumerate().take(len) {
                let entry = unsafe { self.entries[index].assume_init_ref() };
                *slot = entry.result.invalidation();
            }

            P::shootdown_kernel_mappings(&invalidations[..len]);

            self.len = 0;

            for index in 0..len {
                let entry = unsafe { self.entries[index].assume_init_read() };
                entry.token.release();
            }
        }
    }

    impl<A: PageAllocator, const N: usize> Default for KernelShootdownBatch<'_, A, N> {
        fn default() -> Self {
            Self::new()
        }
    }

    impl<A: PageAllocator, const N: usize> Drop for KernelShootdownBatch<'_, A, N> {
        fn drop(&mut self) {
            debug_assert!(
                self.len == 0,
                "KernelShootdownBatch must be issued before pending map pins release"
            );
        }
    }

    pub struct AddressSpaceShootdownBatch<'a, A: PageAllocator, const N: usize> {
        asid: Asid,
        entries: [MaybeUninit<PendingMapRelease<'a, A>>; N],
        len: usize,
        _not_send: PhantomData<*const ()>,
    }

    impl<'a, A: PageAllocator, const N: usize> AddressSpaceShootdownBatch<'a, A, N> {
        pub fn new(asid: Asid) -> Self {
            Self {
                asid,
                entries: [const { MaybeUninit::uninit() }; N],
                len: 0,
                _not_send: PhantomData,
            }
        }

        pub fn asid(&self) -> Asid {
            self.asid
        }

        pub fn len(&self) -> usize {
            self.len
        }

        pub fn is_empty(&self) -> bool {
            self.len == 0
        }

        pub fn push_page_unmap_result(
            &mut self,
            result: PmapUnmapResult,
            map_pin: MapPin<'a, A>,
        ) -> Result<(), ShootdownPushError<'a, A>> {
            if result.kind() != PmapReserveKind::Page4K
                || !result.phys().0.is_multiple_of(PAGE_SIZE_4K)
            {
                return Err(ShootdownPushError {
                    reason: ShootdownError::UnsupportedMapping,
                    map_pin,
                });
            }

            if result_ppn(result.phys()) != map_pin.ppn() {
                return Err(ShootdownPushError {
                    reason: ShootdownError::MismatchedFrame,
                    map_pin,
                });
            }

            if self.len == N {
                return Err(ShootdownPushError {
                    reason: ShootdownError::Full,
                    map_pin,
                });
            }

            self.entries[self.len].write(PendingMapRelease {
                result,
                token: PendingMapReleaseToken::Page(map_pin),
            });
            self.len += 1;
            Ok(())
        }

        pub fn push_unmap_result(
            &mut self,
            result: PmapUnmapResult,
            map_pin_run: MapPinRun<'a, A>,
        ) -> Result<(), ShootdownRunPushError<'a, A>> {
            if result.page_count() != map_pin_run.count() || result.base_ppn() != map_pin_run.base()
            {
                return Err(ShootdownRunPushError {
                    reason: ShootdownError::MismatchedFrame,
                    map_pin_run,
                });
            }

            if self.len == N {
                return Err(ShootdownRunPushError {
                    reason: ShootdownError::Full,
                    map_pin_run,
                });
            }

            self.entries[self.len].write(PendingMapRelease {
                result,
                token: PendingMapReleaseToken::Run(map_pin_run),
            });
            self.len += 1;
            Ok(())
        }

        pub fn issue_and_release_with(mut self, shootdown_mappings: fn(Asid, &[PmapInvalidation])) {
            let len = self.len;

            let mut invalidations = [PmapInvalidation::new(VirtAddr(0), 0); N];
            for (index, slot) in invalidations.iter_mut().enumerate().take(len) {
                let entry = unsafe { self.entries[index].assume_init_ref() };
                *slot = entry.result.invalidation();
            }

            shootdown_mappings(self.asid, &invalidations[..len]);

            self.len = 0;

            for index in 0..len {
                let entry = unsafe { self.entries[index].assume_init_read() };
                entry.token.release();
            }
        }

        pub fn issue_and_release<P: PmapIf>(self) {
            self.issue_and_release_with(P::shootdown_mappings);
        }
    }

    impl<A: PageAllocator, const N: usize> Drop for AddressSpaceShootdownBatch<'_, A, N> {
        fn drop(&mut self) {
            debug_assert!(
                self.len == 0,
                "AddressSpaceShootdownBatch must be issued before pending map pins release"
            );
        }
    }

    fn result_ppn(phys: PhysAddr) -> Ppn {
        Ppn(phys.0 / PAGE_SIZE_4K)
    }
}

pub fn init<P: TxPlatform>() {
    // L5 Phase span: SubstrateBsp begin (OBS-8).
    //
    // Emitted before substrate bring-up so the span captures the full
    // BSP init interval.  `tx_observe::current()` may return None here
    // if observation has not been initialised yet (normal early-boot
    // order); the emit is a no-op in that case.
    let phase_span = emit_phase_span_begin(
        tx_observe_types::BootPhaseKind::SubstrateBsp,
        <P as tx_hal::PercpuIf>::current_cpu_id().0 as u8,
    );

    let _ = P::platform_info();
    sync::install_platform_spin_progress::<P>();
    boot_memory::init_from_hal::<P>();
    epoch::init_on_bsp::<P>().expect("tx_substrate::init epoch initialization failed");
    zone::init_on_bsp::<P>().expect("tx_substrate::init zone initialization failed");
    slab::init::<P>().expect("tx_substrate::init slab heap initialization failed");
    slab::allocation_smoke().expect("tx_substrate::init slab allocation smoke failed");

    // L6 Mutation emit gates: turn on by default at BSP init so the
    // observation pipeline carries `MutationZoneSign` and
    // `MutationIndexCommit` instants once the L0/L2/L4 backbone is
    // in place. Per OBS-V1 §3 / `08_OBSERVATION_v1.md` §6 HOOKS-1,
    // these are the only L6 events emitted today; the daemon decodes
    // them as `Instant` records scoped inside the surrounding L4/L2
    // span hierarchy.
    //
    // Tests opt out by writing `false` to either gate before exercising
    // a code path; see `tests/obs8_zone_sign_emit.rs`.
    use core::sync::atomic::Ordering;
    zone::MUTATION_EMIT_ENABLED.store(true, Ordering::Release);
    index::INDEX_MUTATION_EMIT_ENABLED.store(true, Ordering::Release);

    // L5 Phase span end.
    emit_phase_span_end(phase_span);
}

pub fn init_on_ap(cpu: CpuId) -> Result<(), ApInitError> {
    // L5 Phase span: SubstrateAp begin (OBS-8).
    let phase_span =
        emit_phase_span_begin(tx_observe_types::BootPhaseKind::SubstrateAp, cpu.0 as u8);

    epoch::init_on_ap(cpu)?;
    zone::init_on_ap(cpu)?;

    // L5 Phase span end.
    emit_phase_span_end(phase_span);
    Ok(())
}

// ---------------------------------------------------------------------------
// L5 Phase span helpers (OBS-8)
// ---------------------------------------------------------------------------

/// Emit a `SpanBegin(phase.<kind>)` record and return the span id.
///
/// Returns `SpanId::NONE` when no emitter is available (before observation
/// init or on boards without a ring transport).
///
/// Convergence point: substrate `init` (BSP) and `init_on_ap` (AP).
/// Not inside a `StepOp::step` body — substrate init functions are called
/// before any step machinery runs (OBS-A-1).
///
/// Spec ref: `docs/Txv3/08_OBSERVATION_v1.md` §16 OBS-8, §6 L5.
#[inline]
fn emit_phase_span_begin(
    phase_kind: tx_observe_types::BootPhaseKind,
    hart_id: u8,
) -> tx_observe::SpanId {
    if let Some(em) = tx_observe::current() {
        use tx_observe::encode::{encode_phase_transition, phase_transition_tag};
        use tx_observe::{EventNameId, TxTraceLevel};
        use tx_observe_types::PayloadPhaseTransition;

        let p = PayloadPhaseTransition {
            phase_kind: phase_kind as u8,
            hart_id,
            _pad: [0u8; 14],
        };
        let (payload_bytes, _) = encode_phase_transition(&p);
        em.span_begin(
            TxTraceLevel::Phase,
            EventNameId::from_raw(phase_kind as u32),
            tx_observe::SpanId::NONE,
            phase_transition_tag(),
            &payload_bytes,
        )
    } else {
        tx_observe::SpanId::NONE
    }
}

/// Emit a `SpanEnd` record closing `span`.  A no-op when `span` is
/// `SpanId::NONE` (no emitter was available at span-begin time).
#[inline]
fn emit_phase_span_end(span: tx_observe::SpanId) {
    if span == tx_observe::SpanId::NONE {
        return;
    }
    if let Some(em) = tx_observe::current() {
        use tx_observe_types::TxPayloadTag;
        em.span_end(span, TxPayloadTag::None, &[]);
    }
}
