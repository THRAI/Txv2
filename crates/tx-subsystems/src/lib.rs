#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod device;
pub mod execution;
pub mod mount;
pub mod page_backed;
pub mod tty;
pub mod vfs;
pub mod vm;
pub mod wait_carrier;
pub mod zones;

mod sync;

pub mod process {}
pub mod thread_runtime {}

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
