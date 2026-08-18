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
mod publication;
pub mod slab;
pub mod slot;
pub mod step;
pub mod sync;
pub mod verbs;
pub mod wake;
pub mod zone;

pub use publication::{
    DetachedNodeAllocation, DetachedPublication, DetachedPublicationParts, DetachedPublicationSlot,
    PublishCommitRetry, PublishError, PublishReservation, PublishRetryReason, Published,
    ReservedCommitInvariant,
};
pub use slot::AtomicSlot;
pub use sync::{
    LockMetricsOff, LockMetricsOn, RwSpinLock, RwSpinReadGuard, RwSpinWriteGuard, SpinMutex,
    SpinMutexGuard, SpinWait,
};
pub use zone::{BindingToken, PublishedBinding};

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

    pub fn fail_next_publication_allocations(count: usize) {
        crate::publication::fail_next_allocations_for_test(count);
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

    use tx_hal::pmap::{
        InvalidationRunGather, InvalidationRunGatherError, InvalidationRunGatherErrorKind,
    };

    use crate::page_allocator::{MapPin, MapPinRun, PageAllocator};

    const PAGE_SIZE_4K: usize = 4096;

    pub struct ShootdownToken;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ShootdownError {
        Full,
        InvalidCursor,
        AddressOverflow,
        UnsupportedMapping,
        MismatchedFrame,
    }

    pub struct ShootdownPushError<'a, A: PageAllocator> {
        reason: ShootdownError,
        map_pin: MapPin<'a, A>,
        next_cursor: VirtAddr,
    }

    impl<'a, A: PageAllocator> ShootdownPushError<'a, A> {
        pub fn reason(&self) -> ShootdownError {
            self.reason
        }

        pub fn into_map_pin(self) -> MapPin<'a, A> {
            self.map_pin
        }

        pub fn next_cursor(&self) -> VirtAddr {
            self.next_cursor
        }

        /// Complete a rejected-but-already-mutated mapping with a caller-
        /// supplied safe invalidation before releasing its map pin.
        pub fn issue_and_release_with(
            self,
            asid: Asid,
            invalidation: PmapInvalidation,
            shootdown_mappings: fn(Asid, &[PmapInvalidation]),
        ) {
            shootdown_mappings(asid, &[invalidation]);
            drop(self.map_pin);
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
        next_cursor: VirtAddr,
    }

    impl<'a, A: PageAllocator> ShootdownRunPushError<'a, A> {
        pub fn reason(&self) -> ShootdownError {
            self.reason
        }

        pub fn into_map_pin_run(self) -> MapPinRun<'a, A> {
            self.map_pin_run
        }

        pub fn next_cursor(&self) -> VirtAddr {
            self.next_cursor
        }

        pub fn issue_and_release_with(
            self,
            asid: Asid,
            invalidation: PmapInvalidation,
            shootdown_mappings: fn(Asid, &[PmapInvalidation]),
        ) {
            shootdown_mappings(asid, &[invalidation]);
            drop(self.map_pin_run);
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

    pub enum AcknowledgedMapReleaseToken<'a, A: PageAllocator> {
        Page(MapPin<'a, A>),
        Run(MapPinRun<'a, A>),
    }

    impl<A: PageAllocator> AcknowledgedMapReleaseToken<'_, A> {
        fn release(self) {
            match self {
                Self::Page(map_pin) => drop(map_pin),
                Self::Run(map_pin_run) => drop(map_pin_run),
            }
        }
    }

    struct PendingMapRelease<'a, A: PageAllocator> {
        result: PmapUnmapResult,
        token: AcknowledgedMapReleaseToken<'a, A>,
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
                    next_cursor: VirtAddr(0),
                });
            }

            if result_ppn(result.phys()) != map_pin.ppn() {
                return Err(ShootdownPushError {
                    reason: ShootdownError::MismatchedFrame,
                    map_pin,
                    next_cursor: VirtAddr(0),
                });
            }

            if self.len == N {
                return Err(ShootdownPushError {
                    reason: ShootdownError::Full,
                    map_pin,
                    next_cursor: VirtAddr(0),
                });
            }

            self.entries[self.len].write(PendingMapRelease {
                result,
                token: AcknowledgedMapReleaseToken::Page(map_pin),
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
                    next_cursor: VirtAddr(0),
                });
            }

            if self.len == N {
                return Err(ShootdownRunPushError {
                    reason: ShootdownError::Full,
                    map_pin_run,
                    next_cursor: VirtAddr(0),
                });
            }

            self.entries[self.len].write(PendingMapRelease {
                result,
                token: AcknowledgedMapReleaseToken::Run(map_pin_run),
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

    pub struct PendingMapReleaseGather<'a, A: PageAllocator, const N: usize> {
        asid: Asid,
        invalidations: InvalidationRunGather<N>,
        entries: [MaybeUninit<AcknowledgedMapReleaseToken<'a, A>>; N],
        len: usize,
        _not_send: PhantomData<*const ()>,
    }

    #[must_use = "acknowledged map releases must be consumed explicitly"]
    pub struct AcknowledgedMapReleaseBatch<'a, A: PageAllocator, const N: usize> {
        invalidations: InvalidationRunGather<N>,
        entries: [MaybeUninit<AcknowledgedMapReleaseToken<'a, A>>; N],
        len: usize,
        _not_send: PhantomData<*const ()>,
    }

    impl<'a, A: PageAllocator, const N: usize> AcknowledgedMapReleaseBatch<'a, A, N> {
        pub fn len(&self) -> usize {
            self.len
        }

        pub fn is_empty(&self) -> bool {
            self.len == 0 && self.invalidations.is_empty()
        }

        pub fn next_cursor(&self) -> VirtAddr {
            self.invalidations.next_cursor()
        }

        pub fn drain_into(mut self, sink: &mut impl FnMut(AcknowledgedMapReleaseToken<'a, A>)) {
            let len = self.len;
            self.len = 0;
            let next_cursor = self.invalidations.next_cursor();
            self.invalidations.clear(next_cursor);
            for index in 0..len {
                let token = unsafe { self.entries[index].assume_init_read() };
                sink(token);
            }
        }

        pub fn release(self) {
            self.drain_into(&mut |token| token.release());
        }
    }

    impl<A: PageAllocator, const N: usize> Drop for AcknowledgedMapReleaseBatch<'_, A, N> {
        fn drop(&mut self) {
            debug_assert!(
                self.len == 0 && self.invalidations.is_empty(),
                "acknowledged map releases must be consumed explicitly"
            );
        }
    }

    impl<'a, A: PageAllocator, const N: usize> PendingMapReleaseGather<'a, A, N> {
        pub fn new(asid: Asid) -> Self {
            Self::new_at(asid, VirtAddr(0))
        }

        pub fn new_at(asid: Asid, next_cursor: VirtAddr) -> Self {
            Self {
                asid,
                invalidations: InvalidationRunGather::new(next_cursor),
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
            self.invalidations.is_empty()
        }

        pub fn next_cursor(&self) -> VirtAddr {
            self.invalidations.next_cursor()
        }

        pub fn invalidation_len(&self) -> usize {
            self.invalidations.len()
        }

        pub fn push_invalidation(
            &mut self,
            invalidation: PmapInvalidation,
            next_cursor: VirtAddr,
        ) -> Result<(), InvalidationRunGatherError> {
            self.invalidations.push(invalidation, next_cursor)
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
                    next_cursor: self.next_cursor(),
                });
            }

            let Some(next_cursor) = result
                .virt()
                .0
                .checked_add(result.invalidation().size())
                .map(VirtAddr)
            else {
                return Err(ShootdownPushError {
                    reason: ShootdownError::AddressOverflow,
                    map_pin,
                    next_cursor: self.next_cursor(),
                });
            };

            if result_ppn(result.phys()) != map_pin.ppn() {
                return Err(ShootdownPushError {
                    reason: ShootdownError::MismatchedFrame,
                    map_pin,
                    next_cursor: self.next_cursor(),
                });
            }

            if self.len == N {
                return Err(ShootdownPushError {
                    reason: ShootdownError::Full,
                    map_pin,
                    next_cursor: self.next_cursor(),
                });
            }

            if let Err(error) = self.invalidations.push(result.invalidation(), next_cursor) {
                return Err(ShootdownPushError {
                    reason: shootdown_error_for_gather(error),
                    map_pin,
                    next_cursor: self.next_cursor(),
                });
            }
            self.entries[self.len].write(AcknowledgedMapReleaseToken::Page(map_pin));
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
                    next_cursor: self.next_cursor(),
                });
            }

            let Some(next_cursor) = result
                .virt()
                .0
                .checked_add(result.invalidation().size())
                .map(VirtAddr)
            else {
                return Err(ShootdownRunPushError {
                    reason: ShootdownError::AddressOverflow,
                    map_pin_run,
                    next_cursor: self.next_cursor(),
                });
            };

            if self.len == N {
                return Err(ShootdownRunPushError {
                    reason: ShootdownError::Full,
                    map_pin_run,
                    next_cursor: self.next_cursor(),
                });
            }

            if let Err(error) = self.invalidations.push(result.invalidation(), next_cursor) {
                return Err(ShootdownRunPushError {
                    reason: shootdown_error_for_gather(error),
                    map_pin_run,
                    next_cursor: self.next_cursor(),
                });
            }
            self.entries[self.len].write(AcknowledgedMapReleaseToken::Run(map_pin_run));
            self.len += 1;
            Ok(())
        }

        #[must_use = "acknowledged map releases must be consumed explicitly"]
        pub fn issue_and_wait_with(
            mut self,
            shootdown_mappings: fn(Asid, &[PmapInvalidation]),
        ) -> AcknowledgedMapReleaseBatch<'a, A, N> {
            shootdown_mappings(self.asid, self.invalidations.as_slice());

            let len = self.len;
            let entries =
                core::mem::replace(&mut self.entries, [const { MaybeUninit::uninit() }; N]);
            self.len = 0;
            let next_cursor = self.invalidations.next_cursor();
            let invalidations = core::mem::replace(
                &mut self.invalidations,
                InvalidationRunGather::new(next_cursor),
            );

            AcknowledgedMapReleaseBatch {
                invalidations,
                entries,
                len,
                _not_send: PhantomData,
            }
        }

        #[must_use = "acknowledged map releases must be consumed explicitly"]
        pub fn issue_and_wait<P: PmapIf>(self) -> AcknowledgedMapReleaseBatch<'a, A, N> {
            self.issue_and_wait_with(P::shootdown_mappings)
        }

        pub fn issue_and_release_with(self, shootdown_mappings: fn(Asid, &[PmapInvalidation])) {
            self.issue_and_wait_with(shootdown_mappings).release();
        }

        pub fn issue_and_release<P: PmapIf>(self) {
            self.issue_and_release_with(P::shootdown_mappings);
        }
    }

    impl<A: PageAllocator, const N: usize> Drop for PendingMapReleaseGather<'_, A, N> {
        fn drop(&mut self) {
            debug_assert!(
                self.len == 0 && self.invalidations.is_empty(),
                "AddressSpaceShootdownBatch must be issued before pending map pins release"
            );
        }
    }

    /// Compatibility name for the pre-R3 address-space batch API.
    pub type AddressSpaceShootdownBatch<'a, A, const N: usize> = PendingMapReleaseGather<'a, A, N>;

    fn result_ppn(phys: PhysAddr) -> Ppn {
        Ppn(phys.0 / PAGE_SIZE_4K)
    }

    fn shootdown_error_for_gather(error: InvalidationRunGatherError) -> ShootdownError {
        match error.kind() {
            InvalidationRunGatherErrorKind::Capacity => ShootdownError::Full,
            InvalidationRunGatherErrorKind::InvalidCursor => ShootdownError::InvalidCursor,
            InvalidationRunGatherErrorKind::AddressOverflow => ShootdownError::AddressOverflow,
        }
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
        em.phase_begin(phase_kind as u8, hart_id)
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
        em.phase_end(span);
    }
}
