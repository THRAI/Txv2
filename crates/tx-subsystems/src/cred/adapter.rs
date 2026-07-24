//! Substrate adapter for cred.
//!
//! Cred has no reactor surface, only substrate: `step_v3` (the
//! `CredentialView` marker trait plus `StepOp` impls for the
//! set{uid,gid,reuid,resuid,…} mutators), `epoch::Guard` (EBR), and
//! `zone` allocation (cred is a zone-allocated `Entity`, and the
//! restriction-stack handle is also zone-allocated).
//!
//! One adapter domain: `step_engine` — bundles the step-v3 types,
//! the epoch guard primitive, and role-shaped zone allocation.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch"],
    reason = "expose substrate step engine (StepOp/StepOutcome, CredentialView, RestrictionStackHandle), EBR guard, and zone allocation as cred-side primitives"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::ProcessIdentity as PlaceholderProcessSubject;
    pub use tx_substrate::step::{
        drive_oneshot, CredentialView, NoProgress, OneShotStepOp, RestrictionStackHandle,
        ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::zone::{
        sign, Cap, Dead, Entity, PayloadCap, Weak, Zone, ZoneAllocated, ZoneError,
    };
}
