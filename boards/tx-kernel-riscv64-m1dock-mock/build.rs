use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker = manifest_dir
        .join("../tx-hal-riscv64-m1dock-mock/linker-rv64-m1dock-mock.ld")
        .canonicalize()
        .unwrap();

    println!("cargo:rerun-if-changed={}", linker.display());
    println!(
        "cargo:rustc-link-arg-bin=tx-kernel-riscv64-m1dock-mock=-T{}",
        linker.display()
    );
}
