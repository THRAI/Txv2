//! tx-ext4 adapters.

extern crate alloc;
pub mod adapter;

pub mod host_async;
pub mod mount;
pub mod namespace;
pub mod pager;
mod read_backend;

#[cfg(test)]
mod tests_v3;
