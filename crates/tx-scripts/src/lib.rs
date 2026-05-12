#![no_std]

extern crate alloc;
pub mod adapter;
#[cfg(test)]
extern crate std;

pub mod file_io {}
pub mod mount {}
pub mod postlude {}
pub mod prelude {}
pub mod process;
pub mod route {}
