//! Substrate adapter for page_backed.
//!
//! Page_backed has no reactor surface in production (the two
//! tx_reactor refs in the directory are inside test files). Single
//! domain `step_engine` covers everything: step_v3 types used by the
//! per-variant fetch / write step ops, zone role types for the
//! `PageContainer` zone, the `page_allocator` surface (allocator,
//! cache pins, device frames, map pins, zero policy), EBR Guard +
//! guard, and SpinMutex.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "page_allocator"],
    reason = "expose substrate step engine outcome types, zone role types, page-allocator primitives (BitmapPageAllocator, CachePin, DeviceFrame, MapPin, ZeroPolicy), EBR guard, and SpinMutex used by the page_backed subsystem's per-variant fetch/write step ops"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::page_allocator::{
        self, AllocError, BitmapPageAllocator, CachePin, DeviceFrame, MapPin, ZeroPolicy,
    };
    pub use tx_substrate::step::{
        ByteProgress, Errno, InterestMask, NoProgress, PageProgress,
        ProcessIdentity as PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome,
        SubjectIdentity, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign, sign_for, Cap, PayloadCap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::SpinMutex;
}
