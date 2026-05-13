//! tx-ext4 adapters.
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod adapter;

#[cfg(feature = "host-async")]
pub mod host_async;
pub mod mount;
pub mod namespace;
pub mod pager;
mod read_backend;

#[cfg(test)]
mod tests_v3;
