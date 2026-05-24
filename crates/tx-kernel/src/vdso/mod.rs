//! Kernel vDSO bootstrap.
//!
//! The kernel calls [`init`] once during early boot to populate the
//! shared vDSO singleton in `tx_subsystems::vdso`.  After that, the
//! exec script path reads the frames through the same singleton.

use tx_hal::TxPlatform;

/// Initialise the global vDSO singleton and the high-resolution
/// clock conversion parameters.
///
/// Delegates frame allocation to `tx_subsystems::vdso::init_vdso()`,
/// then computes `mult` / `shift` from the platform's timebase
/// frequency so the vDSO can convert `rdtime` ticks to nanoseconds.
pub fn init<P: TxPlatform>() -> Result<(), tx_subsystems::vdso::VdsoInitError> {
    tx_subsystems::vdso::init_vdso()?;

    // Compute the clock conversion parameters from the platform's
    // timebase frequency.
    let info = P::platform_info();
    tx_subsystems::vdso::vvar_page().init_clock_params(info.timebase_frequency_hz);
    tx_subsystems::wall_clock::publish_vvar::<P>();

    Ok(())
}
