use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker = manifest_dir
        .join("../tx-hal-loongarch64-qemu-virt/linker-la64-qemu-virt.ld")
        .canonicalize()
        .unwrap();

    println!("cargo:rerun-if-changed={}", linker.display());
    println!(
        "cargo:rustc-link-arg-bin=tx-kernel-loongarch64-qemu-virt=-T{}",
        linker.display()
    );
}
