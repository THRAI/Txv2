//! tx-ext4 adapters.
#![no_std]

extern crate alloc;
#[cfg(any(test, feature = "host-async"))]
extern crate std;

pub mod adapter;

#[cfg(feature = "host-async")]
pub mod host_async;
pub mod journal;
pub mod mount;
pub mod namespace;
pub mod pager;
pub mod planner;
mod read_backend;
mod sync;

#[cfg(test)]
mod tests_v3;
