//! Substrate adapter for tx-drivers.
//!
//! Two virtio files (`virtio/blk.rs`, `virtio/dma.rs`) consume
//! substrate's step engine, page allocator, and `SpinMutex`. Single
//! `step_engine` domain bundles them so driver implementations import
//! from `crate::adapter::step_engine::*` and any future substrate
//! refactor is contained here.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step_v3", "page_allocator"],
    reason = "expose substrate step-v3 outcome types, page-allocator primitives (BitmapPageAllocator, DmaPin, OwnedFrameRun, ZeroPolicy), and SpinMutex used by tx-drivers virtio block + DMA layers"
)]
pub mod step_engine {
    pub use tx_substrate::page_allocator::{
        self, BitmapPageAllocator, DmaPin, OwnedFrameRun, ZeroPolicy,
    };
    pub use tx_substrate::step_v3::{NoProgress, StepOutcome};
    pub use tx_substrate::SpinMutex;
}
