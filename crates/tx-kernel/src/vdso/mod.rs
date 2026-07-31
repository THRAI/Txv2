//! Kernel vDSO bootstrap.
//!
//! The kernel calls [`init`] once during early boot to populate the
//! shared vDSO singleton in `tx_subsystems::vdso`.  After that, the
//! exec script path reads the frames through the same singleton.

use tx_hal::TxPlatform;
use tx_services::time::{
    timekeeper_clock, RealtimeControl, TimeError, TimekeeperClock, VvarPublisher,
};

#[cfg(test)]
mod tests;

/// Initialise the global vDSO singleton and the high-resolution
/// clock conversion parameters.
///
/// Delegates frame allocation to `tx_subsystems::vdso::init_vdso()`,
/// then computes `mult` / `shift` from the platform's timebase
/// frequency so the vDSO can convert `rdtime` ticks to nanoseconds.
pub fn init<P: TxPlatform>() -> Result<(), tx_subsystems::vdso::VdsoInitError>
where
    TimekeeperClock<P>: RealtimeControl + VvarPublisher,
{
    tx_subsystems::vdso::init_vdso()?;

    // Compute the clock conversion parameters from the platform's
    // timebase frequency.
    let info = P::platform_info();
    tx_subsystems::vdso::vvar_page().init_clock_params(info.timebase_frequency_hz);
    let _ = seed_realtime_from_persistent::<P>();
    timekeeper_clock::<P>().publish_vvar();

    Ok(())
}

fn seed_realtime_from_persistent<P>() -> Result<u64, TimeError>
where
    TimekeeperClock<P>: RealtimeControl,
{
    timekeeper_clock::<P>()
        .seed_realtime_from_persistent()
        .map(|report| report.generation)
}
