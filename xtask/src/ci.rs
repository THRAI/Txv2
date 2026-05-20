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
                "--no-deps",
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
        // Retired-vocabulary regression gate. Enforces clippy.toml's
        // `disallowed-names` (et al.) without inheriting the broad
        // `-D warnings` of the step above so that unrelated cosmetic
        // lints (e.g. PR-2 dead_code scaffolding, byte-grouping style)
        // do not gate this check. See:
        //   docs/progress/decisions/2026-05-12-d13-tdd-retirement-via-clippy.md
        //   docs/progress/decisions/2026-05-12-d10-vocabulary-retire-audit.md
        ci_run(
            root,
            "retired vocabulary gate",
            "cargo",
            &[
                "clippy",
                "--no-deps",
                "--workspace",
                "--lib",
                "--bins",
                "--",
                "-A",
                "clippy::all",
                "-D",
                "clippy::disallowed_names",
                "-D",
                "clippy::disallowed_types",
                "-D",
                "clippy::disallowed_methods",
            ],
            "txdoc:CI-GATE-RETIRED-VOCAB",
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
                // Tests that touch zone-allocated entities serialize on the
                // shared `test_support::EPOCH_TEST_LOCK` (std::sync::Mutex);
                // running them in parallel races on zone registration and
                // reset_for_tests state. Match the local-dev `--test-threads=1`
                // pattern. Tracked in the Phase 2-4 decision note.
                "--",
                "--test-threads=1",
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
            "architecture boundary ratchet",
            "cargo",
            &["xtask", "lint", "boundary"],
            "txdoc:CI-GATE-BOUNDARY-RATCHET",
        ),
        ci_run(
            root,
            "invariants lint (all)",
            "cargo",
            &["xtask", "lint", "invariants", "all"],
            "txdoc:CI-GATE-INVARIANTS-LINT",
        ),
        // The dispatch table at
        // `crates/tx-shims/src/linux_syscall/{numbers,mod}.rs` is the
        // single source of truth for syscall progress; this gate fails
        // if the auto-maintained section in `SYSCALL_STATUS.md` drifts
        // from the dispatch table. Fix with `cargo xtask syscall sync`.
        ci_run(
            root,
            "syscall-status doc sync",
            "cargo",
            &["xtask", "lint", "syscall-status"],
            "txdoc:CI-GATE-SYSCALL-STATUS",
        ),
        ci_run(
            root,
            "progress json",
            "cargo",
            &["xtask", "progress", "validate"],
            "txdoc:CI-GATE-PROGRESS-JSON",
        ),
        ci_run(
            root,
            "syscall-status autogen",
            "cargo",
            &["xtask", "syscall-status", "--check"],
            "txdoc:CI-GATE-SYSCALL-STATUS",
        ),
        // Quick observe smoke: demo writes a synthetic .txtrace, validate
        // parses the header and counts records. No daemon build required.
        ci_run(
            root,
            "observe demo+validate smoke",
            "cargo",
            &[
                "xtask",
                "observe",
                "demo",
                "--output",
                "/tmp/txkernel-ci-observe-demo.txtrace",
            ],
            "txdoc:CI-GATE-OBSERVE-SMOKE",
        ),
    ];
    // observe validate runs after demo produces the file — chain separately.
    results.push(ci_run(
        root,
        "observe validate smoke",
        "cargo",
        &[
            "xtask",
            "observe",
            "validate",
            "--file",
            "/tmp/txkernel-ci-observe-demo.txtrace",
        ],
        "txdoc:CI-GATE-OBSERVE-SMOKE",
    ));

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
    let rv64_busybox = crate::image::vendored_busybox_relpath(TxTarget::Rv64Qemu);
    let busybox_present =
        root.join(rv64_busybox).exists() || std::env::var_os("TX_BUSYBOX").is_some();

    let mut results = vec![
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
                // GitHub Actions runs qemu-system-riscv64 under software
                // emulation (no KVM); the 10s dev default isn't enough
                // headroom for SMP=4 boot through reactor + process +
                // tty + mount + init. 30s is empirically comfortable.
                "--timeout-ms",
                "30000",
            ],
            "txdoc:CI-GATE-QEMU-SMOKE",
        ),
    ];

    if busybox_present {
        results.push(ci_run(
            root,
            "rv64 qemu busybox boot sentinel",
            "cargo",
            &["xtask", "test", "busybox-boot", "--target", "rv64-qemu"],
            "txdoc:CI-GATE-QEMU-BUSYBOX-BOOT",
        ));
    } else {
        results.push(CiStepResult {
            name: "rv64 qemu busybox boot sentinel",
            reference: "txdoc:CI-GATE-QEMU-BUSYBOX-BOOT",
            command: "cargo xtask test busybox-boot --target rv64-qemu".to_string(),
            outcome: CiOutcome::Skipped(format!(
                "vendored busybox missing at {}; run tools/images/fetch-busybox.sh",
                rv64_busybox
            )),
        });
    }

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
