use std::path::Path;

use crate::Result;
use crate::lint::{lint_arch, lint_docs, lint_unused};
use crate::target::{TxTarget, installed_targets, target_triple};
use crate::util::run_cmd;

pub(crate) fn check(root: &Path) -> Result<()> {
    run_cmd(root, "cargo", &["fmt", "--check"])?;
    run_cmd(
        root,
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--exclude",
            "tx-kernel-riscv64-qemu-virt",
            "--exclude",
            "tx-kernel-riscv64-m1dock-mock",
            "--exclude",
            "tx-kernel-loongarch64-qemu-virt",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run_cmd(root, "cargo", &["check", "--workspace"])?;
    lint_arch(root)?;
    lint_docs(root)?;
    lint_unused(root)?;
    crate::kernel_user_layouts::kernel_user_layouts(root, Vec::new())?;

    let installed = installed_targets().unwrap_or_default();
    for target in [
        TxTarget::Rv64Qemu,
        TxTarget::Rv64M1DockMock,
        TxTarget::La64Qemu,
    ] {
        let triple = target_triple(target)?;
        if installed.contains(&triple) {
            run_cmd(
                root,
                "cargo",
                &["check", "-p", target.package(), "--target", &triple],
            )?;
        } else {
            println!(
                "skip: {} target check needs `rustup target add {}`",
                target.name(),
                triple
            );
        }
    }

    Ok(())
}

pub(crate) fn build(root: &Path, target_value: &str) -> Result<()> {
    build_with_features_with_profile(root, target_value, &[], false)
}

pub(crate) fn build_release(root: &Path, target_value: &str) -> Result<()> {
    build_with_features_with_profile(root, target_value, &[], true)
}

/// Build the kernel binary for `target_value`, forwarding cargo
/// `--features` flags to the build command. Used by the `test` lane
/// to plumb `--trap-trace` (et al.) through to the kernel binary
/// without requiring users to invoke cargo directly.
pub(crate) fn build_with_features(
    root: &Path,
    target_value: &str,
    features: &[&str],
) -> Result<()> {
    build_with_features_with_profile(root, target_value, features, false)
}

fn build_with_features_with_profile(
    root: &Path,
    target_value: &str,
    features: &[&str],
    release: bool,
) -> Result<()> {
    let installed = installed_targets().unwrap_or_default();
    for target in TxTarget::all_for(target_value)? {
        let triple = target_triple(target)?;
        if !installed.contains(&triple) {
            return Err(format!(
                "{} requires target `{}`; run `rustup target add {}`",
                target.name(),
                triple,
                triple
            ));
        }
        let mut cmd: Vec<&str> = vec!["build"];
        if release {
            cmd.push("--release");
        }
        cmd.extend(["-p", target.package(), "--target", &triple]);
        if !features.is_empty() {
            cmd.push("--features");
            for (i, f) in features.iter().enumerate() {
                if i > 0 {
                    return Err("multi-feature passthrough not yet supported".into());
                }
                cmd.push(f);
            }
        }
        run_cmd(root, "cargo", &cmd)?;
    }
    Ok(())
}
