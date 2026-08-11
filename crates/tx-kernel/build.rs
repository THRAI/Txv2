//! Build script for `tx-kernel`.
//!
//! Userspace files are supplied by initramfs or block media. `TX_BUSYBOX`
//! remains an xtask image-generation input; it no longer bakes a binary into
//! the kernel image.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(tx_userspace_child_spread_smp1)");
    println!("cargo:rustc-check-cfg=cfg(tx_userspace_child_spread_smp4)");
    println!("cargo:rustc-check-cfg=cfg(tx_smp_scheduler_witness)");
    println!("cargo:rerun-if-env-changed=TX_OSCOMP_GROUPS");
}
