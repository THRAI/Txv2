use std::path::Path;

use crate::lint::{lint_arch, lint_docs, lint_unused};
use crate::target::{installed_targets, target_triple, TxTarget};
use crate::util::run_cmd;
use crate::Result;

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
        run_cmd(
            root,
            "cargo",
            &["build", "-p", target.package(), "--target", &triple],
        )?;
    }
    Ok(())
}
