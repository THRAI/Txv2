//! Substrate adapter for cred.
//!
//! Cred has no reactor surface, only substrate: `step_v3` (the
//! `CredentialView` marker trait plus `StepOp` impls for the
//! set{uid,gid,reuid,resuid,…} mutators), `epoch::Guard` (EBR), and
//! `zone` allocation (cred is a zone-allocated `Entity`, and the
//! restriction-stack handle is also zone-allocated).
//!
//! One adapter domain: `step_engine` — bundles the step-v3 types,
//! the epoch guard primitive, and a `sign_zone_for` allocation verb.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step_v3", "zone", "epoch"],
    reason = "expose substrate step engine (StepOp/StepOutcome, CredentialView, RestrictionStackHandle), EBR guard, and zone allocation as cred-side primitives"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step_v3::{
        CredentialView, NoProgress, RestrictionStackHandle, ScriptCtx, StepOp, StepOutcome,
        SubjectIdentity,
    };
    pub use tx_substrate::zone::{Cap, Zone, ZoneAllocated, ZoneError};

    /// Reserve + sign in one step: mint a `Cap<T>` from `T`'s zone.
    /// Mirrors the pipe/mount sign_zone_for verb.
    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }
}
