//! Hardware TTY registration helpers.

use tx_substrate::zone::{self, Cap, PayloadCap};

use crate::device::CharDeviceBinding;
use crate::execution::{Errno, Guard, StepOutcome};
use crate::tty::structure::registry;
use crate::tty::structure::{TtyIdentity, TtyKind, TtyPayload};

/// Create a hardware-backed TTY identity/payload and publish it to the tty
/// registry. devfs aliases can then materialize RNodes pointing at it.
pub fn register_hardware(
    name: &str,
    index: u32,
    binding: &'static CharDeviceBinding,
    _guard: &Guard<'_>,
) -> StepOutcome<Cap<TtyIdentity>> {
    let id_res = match zone::reserve_for::<TtyIdentity>() {
        Ok(reservation) => reservation,
        Err(_) => return StepOutcome::Err(Errno::EIO),
    };
    let payload_res = match zone::reserve_for::<TtyPayload>() {
        Ok(reservation) => reservation,
        Err(_) => return StepOutcome::Err(Errno::EIO),
    };

    let tty = zone::sign_for(
        id_res,
        TtyIdentity::new(TtyKind::SerialHardware, index, name),
    );
    let payload = PayloadCap::from_cap(zone::sign_for(
        payload_res,
        TtyPayload::new_hardware(binding),
    ));
    tty.install_payload(payload);

    if registry::register_hardware_tty(index, tty.clone()).is_err()
        || registry::register_devfs_alias(name, tty.clone()).is_err()
    {
        return StepOutcome::Err(Errno::EIO);
    }

    StepOutcome::Done(tty)
}

/// Register a devfs alias, such as `/dev/console`, for an existing TTY.
pub fn register_console_alias(
    name: &str,
    tty: Cap<TtyIdentity>,
) -> tx_substrate::step_v3::StepOutcome<(), tx_substrate::step_v3::NoProgress> {
    use tx_substrate::step_v3::StepOutcome as V3Out;
    if registry::register_devfs_alias(name, tty).is_err() {
        return V3Out::err(Errno::EIO.into());
    }
    V3Out::done(())
}
