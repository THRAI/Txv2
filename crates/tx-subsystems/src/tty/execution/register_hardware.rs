//! Hardware TTY registration helpers.

use crate::tty::adapter::step_engine::{self as step_engine, Cap, PayloadCap};

use crate::device::CharDeviceBinding;
use crate::execution::{Errno, Guard};
use crate::tty::adapter::step_engine::{NoProgress, StepOutcome};
use crate::tty::structure::registry;
use crate::tty::structure::{TtyIdentity, TtyKind, TtyPayload};

/// Create a hardware-backed TTY identity/payload and publish it to the tty
/// registry. devfs aliases can then materialize RNodes pointing at it.
pub fn register_hardware(
    name: &str,
    index: u32,
    binding: &'static CharDeviceBinding,
    _guard: &Guard<'_>,
) -> StepOutcome<Cap<TtyIdentity>, NoProgress> {
    use crate::tty::adapter::step_engine::StepOutcome as V3Out;
    let id_res = match step_engine::reserve_for::<TtyIdentity>() {
        Ok(reservation) => reservation,
        Err(_) => return V3Out::err(Errno::EIO.into()),
    };
    let payload_res = match step_engine::reserve_for::<TtyPayload>() {
        Ok(reservation) => reservation,
        Err(_) => return V3Out::err(Errno::EIO.into()),
    };

    let tty = step_engine::sign_for(
        id_res,
        TtyIdentity::new(TtyKind::SerialHardware, index, name),
    );
    let payload = PayloadCap::from_cap(step_engine::sign_for(
        payload_res,
        TtyPayload::new_hardware(binding),
    ));
    tty.install_payload(payload);

    if registry::register_hardware_tty(index, tty.clone()).is_err()
        || registry::register_devfs_alias(name, tty.clone()).is_err()
    {
        return V3Out::err(Errno::EIO.into());
    }

    V3Out::done(tty)
}

/// Register a devfs alias, such as `/dev/console`, for an existing TTY.
pub fn register_console_alias(name: &str, tty: Cap<TtyIdentity>) -> StepOutcome<(), NoProgress> {
    use crate::tty::adapter::step_engine::StepOutcome as V3Out;
    if registry::register_devfs_alias(name, tty).is_err() {
        return V3Out::err(Errno::EIO.into());
    }
    V3Out::done(())
}
