use tx_substrate::zone::Cap;

use crate::mount::structure::{MountIdentity, MountNamespace};
use crate::step::{Errno, StepOutcome};

pub struct MountSpec;
pub struct BootstrapMountSpec;

pub fn step_mount(_spec: MountSpec) -> StepOutcome<Cap<MountIdentity>> {
    StepOutcome::Err(Errno::NotImplemented)
}

pub fn step_mount_bootstrap(
    _spec: BootstrapMountSpec,
) -> StepOutcome<(Cap<MountNamespace>, Cap<MountIdentity>)> {
    StepOutcome::Err(Errno::NotImplemented)
}
