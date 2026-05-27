use std::env;
use std::path::Path;

use crate::oscomp::OSCOMP_AUTOTEST;
use crate::target::{
    installed_components, installed_targets, require_target, target_triple, TxTarget, RV64_TARGET,
};
use crate::util::{check_version, command_exists};
use crate::Result;

pub(crate) fn doctor(root: &Path) -> Result<()> {
    println!("txKernel doctor");
    let mut missing_required = Vec::new();

    check_version("rustc", &["--version"], true, &mut missing_required)?;
    check_version("cargo", &["--version"], true, &mut missing_required)?;
    check_version("rustup", &["--version"], true, &mut missing_required)?;
    check_version(
        "qemu-system-riscv64",
        &["--version"],
        true,
        &mut missing_required,
    )?;
    check_version(
        "qemu-system-loongarch64",
        &["--version"],
        true,
        &mut missing_required,
    )?;

    let installed_targets = installed_targets().unwrap_or_default();
    require_target(&installed_targets, RV64_TARGET, &mut missing_required);
    let la64 = target_triple(TxTarget::La64Qemu)?;
    require_target(&installed_targets, &la64, &mut missing_required);

    let installed_components = installed_components().unwrap_or_default();
    for component in ["rustfmt", "clippy", "rust-src", "llvm-tools"] {
        if installed_components
            .iter()
            .any(|line| line.contains(component))
        {
            println!("ok: rustup component {component}");
        } else {
            let display = if component == "llvm-tools" {
                "llvm-tools-preview"
            } else {
                component
            };
            println!("missing: rustup component add {display}");
            missing_required.push(format!("rustup component add {display}"));
        }
    }

    for tool in [
        "cpio",
        "mkfs.ext4",
        "debugfs",
        "e2fsck",
        "mcopy",
        "zip",
        "jq",
    ] {
        if command_exists(tool) {
            println!("ok: optional host tool {tool}");
        } else {
            println!("warn: optional host tool {tool} not found");
        }
    }

    if let Ok(path) = env::var("TX_BUSYBOX") {
        if Path::new(&path).exists() {
            println!("ok: TX_BUSYBOX={path}");
        } else {
            println!("warn: TX_BUSYBOX points at missing file: {path}");
        }
    } else {
        for target in [TxTarget::Rv64Qemu, TxTarget::La64Qemu] {
            let relpath = crate::image::vendored_busybox_relpath(target);
            let vendored = root.join(relpath);
            if vendored.exists() {
                println!(
                    "ok: vendored {} busybox at {}",
                    target.name(),
                    vendored.display()
                );
            } else {
                let help = match target {
                    TxTarget::Rv64Qemu => "tools/images/fetch-busybox.sh",
                    TxTarget::La64Qemu => "tools/images/build-busybox-loongarch64.sh",
                    TxTarget::Rv64M1DockMock => unreachable!("not checked here"),
                };
                println!(
                    "warn: TX_BUSYBOX not set and {} missing; run `{}`",
                    relpath, help
                );
            }
        }
    }
    if let Ok(path) = env::var("TX_MUSL_LIBC") {
        if Path::new(&path).exists() {
            println!("ok: TX_MUSL_LIBC={path}");
        } else {
            println!("warn: TX_MUSL_LIBC points at missing file: {path}");
        }
    } else {
        println!("warn: TX_MUSL_LIBC not set; cpio/ext4 builders will omit dynamic musl libc");
    }
    let oscomp = Path::new(OSCOMP_AUTOTEST);
    if oscomp.join("kernel").join("run.py").exists() {
        println!("ok: OSComp autotest submodule at {OSCOMP_AUTOTEST}");
    } else {
        println!("warn: OSComp autotest submodule missing; run `git submodule update --init --recursive`");
    }

    if missing_required.is_empty() {
        println!("doctor: ok");
        Ok(())
    } else {
        println!("doctor: missing required tools or targets:");
        for item in &missing_required {
            println!("  - {item}");
        }
        Err("doctor found missing required dependencies".into())
    }
}
