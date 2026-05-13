//! tx-ext4 adapters.

extern crate alloc;
pub mod adapter;

extern crate alloc;
#[cfg(any(test, feature = "std"))]
extern crate std;

#[cfg(feature = "std")]
pub mod host_async;
pub mod mount;
pub mod namespace;
pub mod pager;
mod read_backend;

#[cfg(test)]
mod tests_v3;
