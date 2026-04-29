use std::path::Path;
use std::process::Command;

use crate::target::{installed_targets, target_triple, TxTarget};
use crate::util::{command_display, tail_lines};
use crate::Result;

const CI_REPORTING_REF: &str = "docs/design/00_meta-framework/CI_REPORTING_v1.md";

#[derive(Debug)]
enum CiOutcome {
    Passed,
    Skipped(String),
    Failed { status: String, output: String },
}

#[derive(Debug)]
struct CiStepResult {
    name: &'static str,
    reference: &'static str,
    command: String,
    outcome: CiOutcome,
}

pub(crate) fn ci(root: &Path) -> Result<()> {
    let mut results = vec![
        ci_run(
            root,
            "format",
            "cargo",
            &["fmt", "--check"],
            "txdoc:CI-GATE-FMT",
        ),
        ci_run(
            root,
            "clippy",
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
            "txdoc:CI-GATE-CLIPPY",
        ),
        ci_run(
            root,
            "host workspace check",
            "cargo",
            &["check", "--workspace"],
            "txdoc:CI-GATE-HOST-CHECK",
        ),
        ci_run(
            root,
            "host unit tests",
            "cargo",
            &[
                "test",
                "--workspace",
                "--exclude",
                "tx-kernel-riscv64-qemu-virt",
                "--exclude",
                "tx-kernel-riscv64-m1dock-mock",
                "--exclude",
                "tx-kernel-loongarch64-qemu-virt",
            ],
            "txdoc:CI-GATE-UNIT-TESTS",
        ),
        ci_run(
            root,
            "architecture lint",
            "cargo",
            &["xtask", "lint", "arch"],
            "txdoc:CI-GATE-ARCH-LINT",
        ),
        ci_run(
            root,
            "documentation lint",
            "cargo",
            &["xtask", "lint", "docs"],
            "txdoc:CI-GATE-DOC-LINT",
        ),
        ci_run(
            root,
            "unused lint",
            "cargo",
            &["xtask", "lint", "unused"],
            "txdoc:CI-GATE-UNUSED-LINT",
        ),
        ci_run(
            root,
            "progress json",
            "cargo",
            &["xtask", "progress", "validate"],
            "txdoc:CI-GATE-PROGRESS-JSON",
        ),
    ];

    results.push(ci_target_check(
        root,
        "rv64 qemu target",
        TxTarget::Rv64Qemu,
        "txdoc:CI-GATE-RV64",
        true,
    ));
    results.push(ci_target_check(
        root,
        "rv64 m1dock mock target",
        TxTarget::Rv64M1DockMock,
        "txdoc:CI-GATE-M1DOCK-MOCK",
        true,
    ));
    results.push(ci_target_check(
        root,
        "la64 qemu target",
        TxTarget::La64Qemu,
        "txdoc:CI-GATE-LA64",
        false,
    ));

    print_ci_report(&results);
    let failures = results
        .iter()
        .filter(|result| matches!(result.outcome, CiOutcome::Failed { .. }))
        .count();
    if failures == 0 {
        Ok(())
    } else {
        Err(format!("ci failed with {failures} failing check(s)"))
    }
}

pub(crate) fn ci_slow(root: &Path) -> Result<()> {
    let results = vec![
        ci_run(
            root,
            "rv64 qemu build",
            "cargo",
            &["xtask", "build", "--target", "rv64-qemu"],
            "txdoc:CI-GATE-RV64",
        ),
        ci_run(
            root,
            "rv64 qemu smoke sentinel",
            "cargo",
            &[
                "xtask",
                "qemu",
                "--target",
                "rv64-qemu",
                "--profile",
                "smoke",
                "--expect-sentinel",
            ],
            "txdoc:CI-GATE-QEMU-SMOKE",
        ),
    ];

    print_ci_report(&results);
    let failures = results
        .iter()
        .filter(|result| matches!(result.outcome, CiOutcome::Failed { .. }))
        .count();
    if failures == 0 {
        Ok(())
    } else {
        Err(format!("ci-slow failed with {failures} failing check(s)"))
    }
}

fn ci_target_check(
    root: &Path,
    name: &'static str,
    target: TxTarget,
    reference: &'static str,
    required: bool,
) -> CiStepResult {
    let triple = match target_triple(target) {
        Ok(triple) => triple,
        Err(err) if !required => {
            return CiStepResult {
                name,
                reference,
                command: format!("cargo check -p {} --target <la64>", target.package()),
                outcome: CiOutcome::Skipped(err),
            };
        }
        Err(err) => {
            return CiStepResult {
                name,
                reference,
                command: format!("cargo check -p {} --target <unknown>", target.package()),
                outcome: CiOutcome::Failed {
                    status: "target resolution failed".into(),
                    output: err,
                },
            };
        }
    };

    let installed = installed_targets().unwrap_or_default();
    if !installed.contains(&triple) {
        let reason = format!("install with `rustup target add {triple}`");
        if required {
            return CiStepResult {
                name,
                reference,
                command: format!("cargo check -p {} --target {}", target.package(), triple),
                outcome: CiOutcome::Failed {
                    status: "missing required target".into(),
                    output: reason,
                },
            };
        }
        return CiStepResult {
            name,
            reference,
            command: format!("cargo check -p {} --target {}", target.package(), triple),
            outcome: CiOutcome::Skipped(reason),
        };
    }

    ci_run_owned(
        root,
        name,
        "cargo",
        &["check", "-p", target.package(), "--target", &triple],
        reference,
    )
}

fn ci_run(
    root: &Path,
    name: &'static str,
    program: &str,
    args: &[&str],
    reference: &'static str,
) -> CiStepResult {
    ci_run_owned(root, name, program, args, reference)
}

fn ci_run_owned(
    root: &Path,
    name: &'static str,
    program: &str,
    args: &[&str],
    reference: &'static str,
) -> CiStepResult {
    let command = command_display(program, args);
    let output = Command::new(program).args(args).current_dir(root).output();
    match output {
        Ok(output) if output.status.success() => CiStepResult {
            name,
            reference,
            command,
            outcome: CiOutcome::Passed,
        },
        Ok(output) => {
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&output.stdout));
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
            CiStepResult {
                name,
                reference,
                command,
                outcome: CiOutcome::Failed {
                    status: output.status.to_string(),
                    output: tail_lines(&combined, 80),
                },
            }
        }
        Err(err) => CiStepResult {
            name,
            reference,
            command,
            outcome: CiOutcome::Failed {
                status: "failed to start".into(),
                output: err.to_string(),
            },
        },
    }
}

fn print_ci_report(results: &[CiStepResult]) {
    println!("txKernel CI");
    println!("reference: {CI_REPORTING_REF} tag txdoc:CI-REPORT-1");
    println!();

    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for result in results {
        match &result.outcome {
            CiOutcome::Passed => {
                passed += 1;
                println!(
                    "✓ {} — passed ({})",
                    result.name,
                    ci_reference(result.reference)
                );
            }
            CiOutcome::Skipped(reason) => {
                skipped += 1;
                println!(
                    "↷ {} — skipped: {} ({})",
                    result.name,
                    reason,
                    ci_reference(result.reference)
                );
            }
            CiOutcome::Failed { status, output } => {
                failed += 1;
                println!(
                    "✗ {} — failed ({})",
                    result.name,
                    ci_reference(result.reference)
                );
                println!("  command: {}", result.command);
                println!("  status: {status}");
                if !output.trim().is_empty() {
                    println!("  details:");
                    for line in output.lines() {
                        println!("    {line}");
                    }
                }
            }
        }
    }

    println!();
    println!("summary: {passed} passed, {skipped} skipped, {failed} failed");
}

fn ci_reference(tag: &str) -> String {
    format!("{CI_REPORTING_REF} {tag}")
}
