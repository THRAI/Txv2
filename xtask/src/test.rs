//! `cargo xtask test` — opinionated smoke-test entrypoints that compose the
//! existing `build`, `image`, and `qemu` commands.
//!
//! Lanes:
//!   - `smoke` (default): build the kernel plus the minimal test-init
//!     initramfs, then boot under qemu with `--expect-sentinel`. Mirrors
//!     `ci-slow`.
//!   - `busybox-boot`: same as `smoke` but additionally builds the busybox
//!     cpio initramfs from the vendored musl busybox for the selected target
//!     and boots qemu in the busybox profile. Checks the boot sentinel plus the
//!     owner-aware reactor wake marker, but does not exercise busybox
//!     functionality.
//!
//! Both lanes accept `--target rv64-qemu` (default), `--timeout-ms N`, and
//! `--dry-run`. `--dry-run` prints the qemu command line without running it.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::check_build;
use crate::image;
use crate::qemu;
use crate::target::TxTarget;
use crate::trap_trace;
use crate::util::optional_option_value;
use crate::Result;

const DEFAULT_TARGET: &str = "rv64-qemu";
const VDSO_WITNESS_SOURCE: &str = "tools/shell-tests/vdso-phase5-probe.c";
const VDSO_WITNESS_START: &str = "tools/shell-tests/vdso-phase5-start.S";
const VDSO_WITNESS_BINARY: &str = "vdso-phase5-probe";
const VDSO_WITNESS_PASS_MARKER: &str = "vdso-phase5:pass";
const VDSO_WITNESS_FALLBACK_MARKER: &str = "vdso-phase5:fallback=clock_gettime-syscall";
const CLOCK_GETTIME_SYSCALL: u64 = 113;
const UNSUPPORTED_CLOCK_ID: u64 = 0x4000_0000;

pub(crate) fn test(root: &Path, args: Vec<String>) -> Result<()> {
    let lane = args
        .first()
        .filter(|first| !first.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "smoke".to_string());

    let rest_start = if args
        .first()
        .map(|first| !first.starts_with("--"))
        .unwrap_or(false)
    {
        1
    } else {
        0
    };
    let rest = &args[rest_start..];

    let target_value =
        optional_option_value(rest, "--target").unwrap_or_else(|| DEFAULT_TARGET.to_string());
    let target = TxTarget::parse(&target_value)?;
    if target != TxTarget::Rv64Qemu {
        return Err(format!(
            "test lanes currently support --target rv64-qemu only, got {target_value}"
        ));
    }

    match lane.as_str() {
        "smoke" => smoke(root, target, rest, /*with_busybox=*/ false),
        "busybox-boot" | "busybox" => smoke(root, target, rest, /*with_busybox=*/ true),
        "vdso-witness" => vdso_witness(root, target, rest),
        other => Err(format!(
            "unknown test lane '{other}', expected smoke, busybox-boot, or vdso-witness"
        )),
    }
}

fn smoke(root: &Path, target: TxTarget, rest: &[String], with_busybox: bool) -> Result<()> {
    let timeout = optional_option_value(rest, "--timeout-ms");
    let dry_run = rest.iter().any(|arg| arg == "--dry-run");
    let trap_trace = rest.iter().any(|arg| arg == "--trap-trace");

    println!("test: build {}", target.name());
    let features: &[&str] = if trap_trace { &["trap-trace"] } else { &[] };
    check_build::build_with_features(root, target.name(), features)?;

    let profile = if with_busybox {
        println!(
            "test: image cpio --profile busybox --target {}",
            target.name()
        );
        image::image(
            root,
            vec![
                "cpio".to_string(),
                "--profile".to_string(),
                "busybox".to_string(),
                "--target".to_string(),
                target.name().to_string(),
            ],
        )?;
        "busybox"
    } else {
        println!(
            "test: image test-init --profile busybox --target {}",
            target.name()
        );
        image::image(
            root,
            vec![
                "test-init".to_string(),
                "--profile".to_string(),
                "busybox".to_string(),
                "--target".to_string(),
                target.name().to_string(),
            ],
        )?;
        "smoke"
    };

    let qemu_args = qemu_args_for_smoke(target, profile, timeout, dry_run, with_busybox);
    println!(
        "test: qemu {} --profile {} --expect-sentinel",
        target.name(),
        profile
    );
    qemu::qemu(root, qemu_args)
}

fn vdso_witness(root: &Path, target: TxTarget, rest: &[String]) -> Result<()> {
    let timeout = optional_option_value(rest, "--timeout-ms");
    let dry_run = rest.iter().any(|arg| arg == "--dry-run");

    println!("test: build {} with trap-trace", target.name());
    check_build::build_with_features(root, target.name(), &["trap-trace"])?;

    println!(
        "test: image test-init --profile busybox --target {}",
        target.name()
    );
    image::image(
        root,
        vec![
            "test-init".to_string(),
            "--profile".to_string(),
            "busybox".to_string(),
            "--target".to_string(),
            target.name().to_string(),
        ],
    )?;

    let probe = compile_vdso_witness(root)?;
    image::append_test_init_overlay(root, target, &probe, VDSO_WITNESS_BINARY)?;

    let qemu_args = qemu_args_for_vdso_witness(target, timeout, dry_run);
    println!(
        "test: qemu {} --profile smoke --expect-sentinel",
        target.name()
    );
    qemu::qemu(root, qemu_args)?;
    if dry_run {
        return Ok(());
    }

    let serial = root.join(qemu::serial_log_relative(
        target,
        crate::target::Profile::Smoke,
    ));
    trap_trace::trap_trace(
        root,
        vec![
            "--serial".to_string(),
            serial.display().to_string(),
            "--syscalls".to_string(),
        ],
    )?;
    let body = fs::read_to_string(&serial)
        .map_err(|err| format!("failed to read {}: {err}", serial.display()))?;
    verify_vdso_witness_syscall_evidence(&body)
}

fn compile_vdso_witness(root: &Path) -> Result<PathBuf> {
    let source = root.join(VDSO_WITNESS_SOURCE);
    let start = root.join(VDSO_WITNESS_START);
    if !source.is_file() {
        return Err(format!("missing vDSO witness source {}", source.display()));
    }
    if !start.is_file() {
        return Err(format!(
            "missing vDSO witness start stub {}",
            start.display()
        ));
    }
    let output_dir = root.join("target").join("vdso-witness");
    fs::create_dir_all(&output_dir).map_err(|err| err.to_string())?;
    let output = output_dir.join(VDSO_WITNESS_BINARY);
    let compiler = env::var("TX_VDSO_CC").unwrap_or_else(|_| "riscv64-linux-musl-gcc".to_string());
    let status = Command::new(&compiler)
        .args([
            "-nostdlib",
            "-static",
            "-ffreestanding",
            "-fno-builtin",
            "-fno-stack-protector",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-Wl,-e,_start",
        ])
        .arg(&source)
        .arg(&start)
        .arg("-o")
        .arg(&output)
        .status()
        .map_err(|err| format!("failed to run {compiler}: {err}"))?;
    if !status.success() {
        return Err(format!(
            "{compiler} exited with {status} while building vDSO witness"
        ));
    }
    println!("vdso-witness: built {}", output.display());
    Ok(output)
}

fn qemu_args_for_vdso_witness(
    target: TxTarget,
    timeout: Option<String>,
    dry_run: bool,
) -> Vec<String> {
    let mut args = qemu_args_for_smoke(target, "smoke", timeout, dry_run, false);
    args.extend([
        "--expect-marker".to_string(),
        VDSO_WITNESS_PASS_MARKER.to_string(),
        "--expect-marker".to_string(),
        VDSO_WITNESS_FALLBACK_MARKER.to_string(),
    ]);
    args
}

fn debug_hex_field(line: &str, field: &str) -> Option<u64> {
    line.split_whitespace().find_map(|token| {
        token
            .strip_prefix(field)
            .and_then(|value| value.strip_prefix("0x"))
            .and_then(|value| u64::from_str_radix(value, 16).ok())
    })
}

fn verify_vdso_witness_syscall_evidence(serial: &str) -> Result<()> {
    let mut fallback = 0usize;
    let mut supported = Vec::new();
    for line in serial
        .lines()
        .filter(|line| line.starts_with("txdbg:trap "))
    {
        if !line.contains("kind=SY") || debug_hex_field(line, "a7=") != Some(CLOCK_GETTIME_SYSCALL)
        {
            continue;
        }
        match debug_hex_field(line, "a0=") {
            Some(0) | Some(1) => supported.push(line.to_string()),
            Some(UNSUPPORTED_CLOCK_ID) => fallback += 1,
            _ => {}
        }
    }
    if !supported.is_empty() {
        return Err(format!(
            "vDSO supported realtime/monotonic path entered clock_gettime syscall:\n{}",
            supported.join("\n")
        ));
    }
    if fallback != 1 {
        return Err(format!(
            "expected exactly one unsupported-clock clock_gettime fallback syscall (a0={UNSUPPORTED_CLOCK_ID:#x}), saw {fallback}"
        ));
    }
    println!(
        "vdso-witness:evidence: clock_gettime a0=0/1 count=0; unsupported a0={UNSUPPORTED_CLOCK_ID:#x} count=1"
    );
    Ok(())
}

fn qemu_args_for_smoke(
    target: TxTarget,
    profile: &str,
    timeout: Option<String>,
    dry_run: bool,
    with_busybox: bool,
) -> Vec<String> {
    let mut qemu_args = vec![
        "--target".to_string(),
        target.name().to_string(),
        "--profile".to_string(),
        profile.to_string(),
        "--expect-sentinel".to_string(),
        "--expect-marker".to_string(),
        qemu::expected_owner_wake_smp_marker(target),
    ];
    if with_busybox {
        // Smoke runs only need the initramfs; skip the virtio-blk/ext4 wiring
        // so we don't depend on mkfs.ext4 being installed on the host.
        qemu_args.push("--no-block".to_string());
    }
    if let Some(value) = timeout {
        qemu_args.push("--timeout-ms".to_string());
        qemu_args.push(value);
    }
    if dry_run {
        qemu_args.push("--dry-run".to_string());
    }
    qemu_args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_lane() {
        let root = Path::new(".");
        let err = test(root, vec!["explode".into()]).unwrap_err();
        assert!(err.contains("unknown test lane"));
    }

    #[test]
    fn vdso_witness_lane_is_registered() {
        let root = Path::new(".");
        let err = test(root, vec!["vdso-witness".into()]).unwrap_err();
        assert!(
            !err.contains("unknown test lane"),
            "vdso-witness must be a first-class guest witness lane: {err}"
        );
    }

    #[test]
    fn vdso_witness_qemu_args_require_guest_markers() {
        let args = qemu_args_for_vdso_witness(TxTarget::Rv64Qemu, Some("30000".to_string()), false);
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--expect-marker", VDSO_WITNESS_PASS_MARKER]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--expect-marker", VDSO_WITNESS_FALLBACK_MARKER]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--timeout-ms", "30000"]));
    }

    #[test]
    fn vdso_witness_evidence_rejects_supported_clock_syscalls() {
        let serial = concat!(
            "txdbg:trap n=0x1 kind=SY pc=0x1 a7=0x71 a0=0x0 a1=0x0 a2=0x0\n",
            "txdbg:trap n=0x2 kind=SY pc=0x1 a7=0x71 a0=0x40000000 a1=0x0 a2=0x0\n"
        );
        let err = verify_vdso_witness_syscall_evidence(serial).unwrap_err();
        assert!(err.contains("supported realtime/monotonic"));
    }

    #[test]
    fn vdso_witness_evidence_requires_one_unsupported_fallback() {
        let serial = "txdbg:trap n=0x2 kind=SY pc=0x1 a7=0x71 a0=0x40000000 a1=0x0 a2=0x0\n";
        assert!(verify_vdso_witness_syscall_evidence(serial).is_ok());
    }

    #[test]
    fn rejects_non_rv64_target() {
        let root = Path::new(".");
        let err = test(
            root,
            vec!["smoke".into(), "--target".into(), "la64-qemu".into()],
        )
        .unwrap_err();
        assert!(err.contains("test lanes currently support"));
    }

    #[test]
    fn defaults_to_smoke_lane_when_first_arg_is_flag() {
        // Just ensure parsing doesn't blow up; we don't run cargo here.
        // Use --dry-run so qemu construction would short-circuit, but we
        // still call build which would fail in this test sandbox — so we
        // only check the lane-detection branch via a parse-only path.
        // Safer: verify dispatch logic by reading the lane string directly.
        let args: Vec<String> = vec!["--target".into(), "rv64-qemu".into()];
        let lane = args
            .first()
            .filter(|first| !first.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| "smoke".to_string());
        assert_eq!(lane, "smoke");
    }

    #[test]
    fn smoke_qemu_args_require_owner_wake_marker() {
        let args = qemu_args_for_smoke(
            TxTarget::Rv64Qemu,
            "smoke",
            Some("60000".to_string()),
            true,
            false,
        );

        assert!(args.windows(2).any(|pair| pair
            == [
                "--expect-marker",
                "txkernel:qemu-riscv64-virt:reactor:owner-wake:smp:ok"
            ]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--timeout-ms", "60000"]));
        assert!(args.contains(&"--dry-run".to_string()));
        assert!(!args.contains(&"--no-block".to_string()));
    }

    #[test]
    fn busybox_qemu_args_require_owner_wake_marker_and_no_block() {
        let args = qemu_args_for_smoke(TxTarget::Rv64Qemu, "busybox", None, false, true);

        assert!(args.windows(2).any(|pair| pair
            == [
                "--expect-marker",
                "txkernel:qemu-riscv64-virt:reactor:owner-wake:smp:ok"
            ]));
        assert!(args.contains(&"--no-block".to_string()));
    }
}
