#![no_std]

use tx_hal::TxPlatform;

#[doc(hidden)]
pub mod boot_memory;

pub mod bitmap {
    pub struct BitmapReservation;
}

pub mod bus {
    pub struct RawPort;
    pub struct RawQueue;
    pub struct RawTrace;
}

pub mod epoch {
    pub struct Guard<'g> {
        _marker: core::marker::PhantomData<&'g ()>,
    }
}

pub mod index {
    pub struct IndexReservation;
}

pub mod mutation {
    pub struct CommitPoint;
}

pub mod pmap {
    pub use tx_hal::pmap::*;
}

pub mod page_allocator;
pub mod slab;

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

pub mod shootdown {
    use core::fmt;
    use core::marker::PhantomData;
    use core::mem::MaybeUninit;
    use tx_hal::{Asid, PhysAddr, PmapIf, PmapReserveKind, PmapUnmapResult, Ppn};

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
            self.len = 0;

            for index in 0..len {
                let entry = unsafe { self.entries[index].assume_init_read() };
                P::shootdown_kernel_mapping(entry.result.invalidation());
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

        pub fn issue_and_release<P: PmapIf>(mut self) {
            let len = self.len;
            self.len = 0;

            for index in 0..len {
                let entry = unsafe { self.entries[index].assume_init_read() };
                P::shootdown_mapping(self.asid, entry.result.invalidation());
                entry.token.release();
            }
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

pub mod zone {
    pub struct Cap<T: ?Sized> {
        _marker: core::marker::PhantomData<T>,
    }

    pub struct Weak<T: ?Sized> {
        _marker: core::marker::PhantomData<T>,
    }

    pub struct IdentRef<'g, T: ?Sized> {
        _marker: core::marker::PhantomData<&'g T>,
    }

    pub struct ZoneReservation<T> {
        _marker: core::marker::PhantomData<T>,
    }
}

pub fn init<P: TxPlatform>() {
    let _ = P::platform_info();
    boot_memory::init_from_hal::<P>();
    slab::init::<P>().expect("tx_substrate::init slab heap initialization failed");
    slab::allocation_smoke().expect("tx_substrate::init slab allocation smoke failed");
}
