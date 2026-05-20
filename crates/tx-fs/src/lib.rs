#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod bdevfs;
pub mod devfs;
pub mod devpts;
pub mod procfs;
pub mod sysfs;
pub mod tmpfs;

pub mod tx_ext4 {
    pub use crate::tx_ext4_bridge::*;
    pub use tx_ext4::mount::{
        mount_ext4_read_only, mount_ext4_read_write, Ext4MountWire, MountedExt4,
    };
    pub use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
    pub use tx_ext4_format::{Ext4FormatError, Result as Ext4Result};
}
mod tx_ext4_bridge;

pub mod tx_fat {
    pub use crate::fat_bridge::*;
    pub use tx_fat::mount::{mount_fat_read_only, mount_fat_read_write, MountedFat};
    pub use tx_fat_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
    pub use tx_fat_format::{FatFormatError, Result as FatResult};
}
mod fat_bridge;

#[cfg(test)]
mod initramfs_tests;

#[cfg(test)]
pub(crate) mod test_support {
    /// Shared serialisation lock for every test in this crate's lib
    /// binary. Multiple test modules (`devfs::tests`, `tmpfs::tests`)
    /// observe the same per-CPU epoch slot in `tx_substrate`, so they
    /// cannot concurrently create epoch guards (the substrate enforces
    /// no-nesting on the same logical CPU; see
    /// `crates/tx-substrate/src/epoch/local.rs`). Every test grabs
    /// this lock before touching epoch / zone state.
    pub(crate) static FS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
