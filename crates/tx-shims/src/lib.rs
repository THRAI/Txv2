#![no_std]

// Required so submodules under `linux_syscall/` can resolve `alloc::*`
// paths (e.g. `alloc::vec::Vec`, `alloc::sync::Arc`). The lib root does
// not reference `alloc::*` directly, but child modules do; this declaration
// brings the crate into the namespace they share.
#[cfg_attr(not(test), allow(unused_extern_crates))]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod linux_syscall;
pub mod posix_signal {}
