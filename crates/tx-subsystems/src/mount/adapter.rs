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
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity,
        Dead, Entity, IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy,
        RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
}
