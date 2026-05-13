//! tx-ext4 adapters.

#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;
pub mod adapter;

#[cfg(feature = "std")]
pub mod host_async;
pub mod mount;
pub mod namespace;
pub mod pager;
mod read_backend;

#[cfg(test)]
mod tests_v3;
