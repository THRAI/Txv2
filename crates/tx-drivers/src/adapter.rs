//! Substrate adapter for tx-drivers.
//!
//! Two virtio files (`virtio/blk.rs`, `virtio/dma.rs`) consume
//! substrate's step engine, page allocator, and the tx-drivers lock facade. Single
//! `step_engine` domain bundles them so driver implementations import
//! from `crate::adapter::step_engine::*` and any future substrate
//! refactor is contained here.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "page_allocator"],
    reason = "expose substrate step-v3 outcome types, page-allocator primitives (BitmapPageAllocator, DmaPin, OwnedFrameRun, ZeroPolicy), and the tx-drivers lock facade used by tx-drivers virtio block + DMA layers"
)]
pub mod step_engine {
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::page_allocator::{
        self, BitmapPageAllocator, DmaPin, OwnedFrameRun, ZeroPolicy,
    };
    pub use tx_substrate::step::{NoProgress, StepOutcome};
}
