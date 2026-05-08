#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod bdevfs {}
pub mod devfs;
pub mod devpts {}
pub mod procfs {}
pub mod tmpfs;
pub mod tx_ext4 {}

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
