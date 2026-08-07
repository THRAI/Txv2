//! Static device binding and concrete transport adapters.

mod block;

pub mod ahci_block;
pub mod binder;
pub mod dwmac_net;
pub mod runtime;
pub mod virtio_mmio_net;
pub mod virtio_pci_net;

pub use block::KernelBlockDevices;
