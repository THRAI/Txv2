use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "wake"],
    reason = "expose substrate step engine outcome/error types, zone allocation, EBR guard, and WaitSource for epoll fd table and OnEdge yield"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::epoch::guard;
    pub use tx_substrate::step::{
        ByteProgress, Errno as V3Errno, InterestMask, NoProgress, StepOutcome, WaitSourceId,
        YieldShape,
    };
    pub use tx_substrate::wake::WaitSource;
    pub use tx_substrate::zone::{Cap, Zone, ZoneAllocated, ZoneError};

    pub fn register_zone_for<T: ZoneAllocated>() -> Result<(), ZoneError> {
        zone::register_zone_for::<T>().map(|_| ())
    }
}

#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "reserved epoll reactor wait-routing adapter boundary; no current public re-exports"
)]
pub mod wait_routing {}
