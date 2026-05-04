use tx_hal::{console_write_str, TxPlatform};
use tx_substrate::{
    epoch,
    zone::{self, Zone, ZoneAllocated, ZoneError},
};

use crate::{page_backed::PageContainer, vm::AddressSpace};

pub(crate) struct ZoneSmokeObj {
    pub(crate) value: usize,
}

static ZONE_SMOKE: Zone<ZoneSmokeObj> = Zone::const_new();

unsafe impl ZoneAllocated for ZoneSmokeObj {
    fn zone() -> &'static Zone<Self> {
        &ZONE_SMOKE
    }
}

pub(crate) fn register_all() -> Result<(), ZoneError> {
    smoke::register_zones()?;
    process::register_zones()?;
    thread::register_zones()?;
    vm::register_zones()?;
    page_backed::register_zones()?;
    mount::register_zones()?;
    vfs::register_zones()?;
    tty::register_zones()?;
    Ok(())
}

pub(crate) fn run_smoke<P: TxPlatform>() -> Result<(), ZoneError> {
    register_all()?;

    let reservation = zone::reserve_for::<ZoneSmokeObj>()?;
    let cap = zone::sign_for(reservation, ZoneSmokeObj { value: 7 });
    let weak = cap.downgrade();
    let upgraded = {
        let guard = epoch::guard();
        let ident = weak.observe(&guard).ok_or(ZoneError::SlotNotFound)?;
        if ident.value != 7 {
            return Err(ZoneError::InvalidState);
        }
        ident.to_cap().map_err(|_| ZoneError::SlotNotFound)?
    };
    if upgraded.value != 7 {
        return Err(ZoneError::InvalidState);
    }

    drop(cap);
    drop(upgraded);
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);

    console_write_str::<P>("txkernel:zone:smoke:ok\n");
    Ok(())
}

mod smoke {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<ZoneSmokeObj>()?;
        Ok(())
    }
}

mod process {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        Ok(())
    }
}

mod thread {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        Ok(())
    }
}

mod vm {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<AddressSpace>()?;
        Ok(())
    }
}

mod page_backed {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<PageContainer>()?;
        Ok(())
    }
}

mod mount {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        Ok(())
    }
}

mod vfs {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        Ok(())
    }
}

mod tty {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::tty::structure::registry::register_zones()
    }
}
