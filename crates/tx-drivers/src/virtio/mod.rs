//! VirtIO driver adapters for Txv2 tier-2 board devices.

pub mod blk;
mod block_async;
pub mod dma;
pub mod mmio;
pub mod net;
pub mod pci;

pub use blk::VirtioPciBlock;
pub use dma::TxVirtioHal;
pub use mmio::VirtioMmioBlock;
pub use net::{VirtioMmioNet, VirtioNetPollOutcome, VirtioPciNet};
