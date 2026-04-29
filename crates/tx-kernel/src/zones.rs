use tx_substrate::zone::{self, Zone, ZoneAllocated, ZoneError};

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
    mount::register_zones()?;
    vfs::register_zones()?;
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
