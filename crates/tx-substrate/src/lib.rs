#![no_std]

use tx_hal::TxPlatform;

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
    pub struct PmapBatch;
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

pub mod shootdown {
    pub struct ShootdownToken;
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
    let _ = P::boot_info();
    let _ = P::platform_info();
}
