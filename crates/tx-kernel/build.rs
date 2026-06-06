//! Build script for `tx-kernel`. Conditionally bakes a static-musl-built
//! busybox binary into the kernel image when `TX_BUSYBOX` is set.
//!
//! Shell-prompt roadmap Slice 10 (2026-05-08).
//!
//! When `TX_BUSYBOX=/path/to/busybox` is set in the build environment:
//! - Copies the binary to `$OUT_DIR/busybox.bin`.
//! - Emits `cargo:rustc-cfg=busybox_baked` so `init.rs`'s
//!   `mod busybox_fixture` and `register_busybox_into_tmpfs()` are
//!   compiled in.
//! - The kernel boot path then calls `register_busybox_into_tmpfs()` to
//!   install the binary at `/bin/sh` in the root tmpfs before
//!   driving `exec_script` against `/bin/sh`.
//!
//! When `TX_BUSYBOX` is unset, the bake-in is skipped and the kernel
//! falls back to the existing `init_fixture` path. This is what host
//! tests use (no real busybox binary required).
//!
//! When `TX_BUSYBOX` is set but the file is missing, the script emits
//! a warning (visible in `cargo build` output) and falls through to
//! the unset path.
//!
//! Re-runs whenever `TX_BUSYBOX` changes, so flipping the env var
//! between builds is honoured without a `cargo clean`.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    // Declare the custom cfg name so `#[cfg(busybox_baked)]` doesn't
    // trip `unexpected_cfgs`. Required by Rust 1.80+'s check-cfg lint.
    println!("cargo:rustc-check-cfg=cfg(busybox_baked)");
    println!("cargo:rustc-check-cfg=cfg(tx_userspace_child_spread_smp1)");
    println!("cargo:rustc-check-cfg=cfg(tx_userspace_child_spread_smp4)");
    println!("cargo:rerun-if-env-changed=TX_BUSYBOX");
    println!("cargo:rerun-if-env-changed=TX_OSCOMP_GROUPS");

    let Ok(busybox_path) = env::var("TX_BUSYBOX") else {
        // Unset — the most common case for host tests + CI without
        // a riscv64 cross-toolchain. Skip cleanly.
        return;
    };

    let src = PathBuf::from(&busybox_path);
    if !src.is_file() {
        // Set but missing — emit a warning and skip. The kernel will
        // fall back to the init_fixture path; the developer can fix
        // their TX_BUSYBOX env var without touching code.
        println!(
            "cargo:warning=TX_BUSYBOX={busybox_path} is not a regular file; busybox bake-in skipped"
        );
        return;
    }

    println!("cargo:rerun-if-changed={busybox_path}");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let dest = out_dir.join("busybox.bin");
    fs::copy(&src, &dest).unwrap_or_else(|e| {
        panic!("failed to copy {busybox_path} -> {dest:?}: {e}");
    });

    println!("cargo:rustc-cfg=busybox_baked");
}
