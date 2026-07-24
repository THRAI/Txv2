//! tx-vdso — build-time vDSO ELF image.
//!
//! This crate compiles a minimal RISC-V vDSO `.so` at build time
//! and embeds the resulting bytes.  The kernel maps these bytes into
//! every user address space as an ABI fast-path trampoline.
//!
//! ## Exports
//!
//! | Symbol                     | Meaning                                   |
//! |----------------------------|-------------------------------------------|
//! | `VDSO_IMAGE`               | `&[u8]` — raw vDSO ELF bytes              |
//! | `VDSO_IMAGE_SIZE`          | `usize` — byte count, 0 when unavailable  |
//! | `VDSO_AVAILABLE`           | `bool` — true when a real image was built |
//!
//! ## Feature gates
//!
//! When the RISC-V cross-compiler is absent the build emits an empty
//! stub and `VDSO_AVAILABLE` is `false`.  The kernel checks this flag
//! before wiring vDSO pages into exec.

#![no_std]

#[cfg(test)]
extern crate std;

/// The VVAR page is one page immediately below the vDSO image base.
///
/// VM owns both runtime addresses; this image-relative ABI value is the only
/// placement fact the vDSO needs.
pub const VVAR_DELTA: isize = -4096;

/// Whether a real vDSO image was produced at build time.
#[cfg(not(vdso_stub))]
pub const VDSO_AVAILABLE: bool = true;
#[cfg(vdso_stub)]
pub const VDSO_AVAILABLE: bool = false;

/// Raw vDSO ELF bytes.
///
/// When `VDSO_AVAILABLE` is `false` this is a zero-length slice.
/// Callers must check `VDSO_AVAILABLE` before interpreting the bytes.
#[cfg(not(vdso_stub))]
pub static VDSO_IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso.so"));
#[cfg(vdso_stub)]
pub static VDSO_IMAGE: &[u8] = &[];

/// Total byte count of the embedded vDSO image.
///
/// When `VDSO_SIZE == 0` the kernel should skip vDSO mapping
/// entirely — no AT_SYSINFO_EHDR, no VmEntry registration.
pub const VDSO_IMAGE_SIZE: usize = VDSO_IMAGE.len();

/// Number of 4 KiB pages the vDSO image occupies.
pub const VDSO_NUM_PAGES: usize = VDSO_IMAGE_SIZE.div_ceil(4096);

include!(concat!(env!("OUT_DIR"), "/symbol_offsets.rs"));

#[cfg(test)]
mod tests;
