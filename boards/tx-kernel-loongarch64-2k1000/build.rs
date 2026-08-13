use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let linker = manifest_dir
        .join("../tx-hal-loongarch64-2k1000/linker-la64-2k1000.ld")
        .canonicalize()
        .unwrap();

    println!("cargo:rerun-if-changed={}", linker.display());
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_KERNEL_ONLY");
    println!(
        "cargo:rustc-link-arg-bin=tx-kernel-loongarch64-2k1000=-T{}",
        linker.display()
    );
    if env::var_os("CARGO_FEATURE_KERNEL_ONLY").is_some() {
        println!("cargo:rustc-link-arg-bin=tx-kernel-loongarch64-2k1000=--defsym=TX_KERNEL_ONLY=1");
    }
}
