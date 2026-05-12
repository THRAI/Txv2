use tx_hal::TxPlatform;
use crate::adapter::step_engine::ZoneError;

pub(crate) fn register_all() -> Result<(), ZoneError> {
    tx_subsystems::zones::register_all()
}

pub(crate) fn try_bounded_maintenance_tick() {
    tx_subsystems::zones::try_bounded_maintenance_tick();
}

pub(crate) fn panic_shutdown<P: TxPlatform>() -> ! {
    tx_subsystems::zones::panic_shutdown::<P>()
}

pub(crate) fn shutdown_with_zone_cleanup<P: TxPlatform>() -> ! {
    tx_subsystems::zones::shutdown_with_zone_cleanup::<P>()
}
