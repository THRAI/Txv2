use crate::adapter::step_engine::ZoneError;
use tx_hal::TxPlatform;

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
    if normal_shutdown_zone_summary_enabled::<P>() {
        tx_subsystems::zones::shutdown_with_zone_cleanup::<P>()
    } else {
        tx_subsystems::zones::shutdown_with_quiet_zone_cleanup::<P>()
    }
}

fn normal_shutdown_zone_summary_enabled<P: TxPlatform>() -> bool {
    let Some(cmdline) = <P as tx_hal::BootInfoIf>::boot_info().cmdline else {
        return false;
    };
    cmdline.split_ascii_whitespace().any(|token| {
        matches!(
            token,
            "tx.zone_summary=1" | "tx.zone_summary=true" | "tx.resource_summary=1"
        )
    })
}
