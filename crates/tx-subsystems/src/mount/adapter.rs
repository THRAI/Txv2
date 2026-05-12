//! Substrate adapter for mount.
//!
//! Production mount code only touches `tx-substrate::zone` (role-typed
//! primitives `Cap`, `PayloadCap`, `Entity`, `Zone`, …) and
//! `tx-substrate::SpinMutex` (for the global `MOUNT_TABLE` lock). No
//! step-engine or wake routing in mount production — those live in the
//! per-fs implementations (tmpfs/devfs/ext4) and in mount's
//! `#[cfg(test)]` block (phase 7).
//!
//! The adapter is therefore a single `runtime` domain that bundles the
//! zone role types and the in-kernel lock primitive mount needs for
//! its identity / payload / namespace zones and the mount table.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "runtime",
    apis = ["zone"],
    reason = "expose the substrate zone role types (Cap, PayloadCap, Entity, ...) and SpinMutex primitive that mount uses for its identity/payload/namespace zones and global mount table"
)]
pub mod runtime {
    use tx_substrate::zone;

    pub use tx_substrate::zone::{
        Cap, Dead, Entity, PayloadBinding, PayloadCap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::SpinMutex;

    /// Reserve + sign in one step: mint a `Cap<T>` from `T`'s zone.
    /// Mirrors the pipe adapter's `sign_zone_for` verb — bundles the
    /// substrate two-step alloc into a single mount-side helper so
    /// the production call sites read as "mint a mount cap" rather
    /// than "reserve then sign in substrate vocabulary".
    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }
}
