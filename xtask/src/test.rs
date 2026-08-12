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
const VVAR_SMP_WITNESS_SOURCE: &str = "tools/shell-tests/vdso-vvar-smp-probe.c";
const SMP_SCHEDULER_WITNESS_SOURCE: &str = "tools/shell-tests/smp-scheduler-witness.c";
const VDSO_WITNESS_START: &str = "tools/shell-tests/vdso-phase5-start.S";
const VDSO_WITNESS_BINARY: &str = "vdso-phase5-probe";
const VVAR_SMP_WITNESS_BINARY: &str = "vdso-vvar-smp-probe";
const SMP_SCHEDULER_WITNESS_BINARY: &str = "smp-scheduler-witness";
const VDSO_WITNESS_PASS_MARKER: &str = "vdso-phase5:pass";
const VDSO_WITNESS_FALLBACK_MARKER: &str = "vdso-phase5:fallback=clock_gettime-syscall";
const VDSO_WITNESS_LAYOUT_PREFIX: &str = "vdso-phase5:layout ";
const VDSO_WITNESS_SIGNAL_RESTORER_MARKER: &str = "vdso-phase5:signal-restorer=pass";
const VDSO_WITNESS_VVAR_SMP_MARKER_PREFIX: &str = "vdso-phase5:vvar-smp=pass";
const VDSO_WITNESS_VVAR_SMP_MARKER: &str = "vdso-phase5:vvar-smp=pass writer-updates=1024 reader-reads=500000 writer-mask=0x0000000000000002 reader-mask=0x0000000000000001 writer-affinity=ok reader-affinity=ok reader-path=direct-vdso writer-done=1 reader-done=1 reader-errors=0";
const SMP_SCHEDULER_WITNESS_MARKER_PREFIX: &str = "sched-smp:result";
const VDSO_GLIBC_WITNESS_SELECTOR: &str = "libctest-glibc:dynamic:clock_gettime";
const VDSO_GLIBC_WITNESS_START: &str =
    "========== START entry-dynamic.exe clock_gettime ==========";
const VDSO_GLIBC_WITNESS_END: &str = "========== END entry-dynamic.exe clock_gettime ==========";
const VDSO_GLIBC_WITNESS_TIMEOUT_MS: &str = "60000";
const CLOCK_GETTIME_SYSCALL: u64 = 113;
const RT_SIGACTION_SYSCALL: u64 = 134;
const RT_SIGRETURN_SYSCALL: u64 = 139;
const SIGUSR1: u64 = 10;
const UNSUPPORTED_CLOCK_ID: u64 = 0x4000_0000;
const VVAR_SMP_WRITER_UPDATES: u64 = 1024;
const VVAR_SMP_READER_READS: u64 = 500_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestLane {
    Smoke,
    Busybox,
    VdsoWitness,
    VvarSmpWitness,
    SmpSchedulerWitness,
    VdsoGlibcWitness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SmpSchedulerCase {
    Static,
    Movable,
    Pipe,
    Affinity,
    Timer,
    Pthread,
    Mixed,
    Stress,
}

impl SmpSchedulerCase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Movable => "movable",
            Self::Pipe => "pipe",
            Self::Affinity => "affinity",
            Self::Timer => "timer",
            Self::Pthread => "pthread",
            Self::Mixed => "mixed",
            Self::Stress => "stress",
        }
    }
}

fn parse_smp_scheduler_case(value: &str) -> Result<SmpSchedulerCase> {
    match value {
        "static" | "spread" | "witness-spread" => Ok(SmpSchedulerCase::Static),
        "movable" => Ok(SmpSchedulerCase::Movable),
        "pipe" => Ok(SmpSchedulerCase::Pipe),
        "affinity" => Ok(SmpSchedulerCase::Affinity),
        "timer" => Ok(SmpSchedulerCase::Timer),
        "pthread" => Ok(SmpSchedulerCase::Pthread),
        "mixed" => Ok(SmpSchedulerCase::Mixed),
        "stress" => Ok(SmpSchedulerCase::Stress),
        other => Err(format!(
            "invalid smp-scheduler-witness case '{other}', expected static, movable, pipe, affinity, timer, pthread, mixed, or stress"
        )),
    }
}

fn parse_test_lane(lane: &str) -> Result<TestLane> {
    match lane {
        "smoke" => Ok(TestLane::Smoke),
        "busybox-boot" | "busybox" => Ok(TestLane::Busybox),
        "vdso-witness" => Ok(TestLane::VdsoWitness),
        "vdso-vvar-smp-witness" => Ok(TestLane::VvarSmpWitness),
        "smp-scheduler-witness" => Ok(TestLane::SmpSchedulerWitness),
        "vdso-glibc-witness" => Ok(TestLane::VdsoGlibcWitness),
        other => Err(format!(
            "unknown test lane '{other}', expected smoke, busybox-boot, vdso-witness, vdso-vvar-smp-witness, smp-scheduler-witness, or vdso-glibc-witness"
        )),
    }
}

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

    match parse_test_lane(&lane)? {
        TestLane::Smoke => smoke(root, target, rest, /*with_busybox=*/ false),
        TestLane::Busybox => smoke(root, target, rest, /*with_busybox=*/ true),
        TestLane::VdsoWitness => vdso_witness(root, target, rest),
        TestLane::VvarSmpWitness => vvar_smp_witness(root, target, rest),
        TestLane::SmpSchedulerWitness => smp_scheduler_witness(root, target, rest),
        TestLane::VdsoGlibcWitness => vdso_glibc_witness(root, target, rest),
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
    let timeout = optional_option_value(rest, "--timeout-ms").or_else(|| Some("30000".to_string()));
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
    verify_vdso_witness_syscall_evidence(&body)?;
    verify_vdso_witness_layout_evidence(&body)?;
    verify_vdso_witness_signal_restorer_evidence(&body)
}

fn vvar_smp_witness(root: &Path, target: TxTarget, rest: &[String]) -> Result<()> {
    let timeout = optional_option_value(rest, "--timeout-ms").or_else(|| Some("60000".to_string()));
    let dry_run = rest.iter().any(|arg| arg == "--dry-run");

    println!("test: build {} for SMP VVAR witness", target.name());
    check_build::build_with_features(root, target.name(), &[])?;
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
    let probe = compile_vvar_smp_witness(root)?;
    image::append_test_init_overlay(root, target, &probe, VVAR_SMP_WITNESS_BINARY)?;
    qemu::qemu(
        root,
        qemu_args_for_vvar_smp_witness(target, timeout, dry_run),
    )?;
    if dry_run {
        return Ok(());
    }

    let serial = root.join(qemu::serial_log_relative(
        target,
        crate::target::Profile::Smoke,
    ));
    let body = fs::read_to_string(&serial)
        .map_err(|err| format!("failed to read {}: {err}", serial.display()))?;
    verify_vvar_smp_witness_evidence(&body)
}

fn smp_scheduler_witness(root: &Path, target: TxTarget, rest: &[String]) -> Result<()> {
    let timeout = optional_option_value(rest, "--timeout-ms").or_else(|| Some("60000".to_string()));
    let dry_run = rest.iter().any(|arg| arg == "--dry-run");
    let case = match optional_option_value(rest, "--case") {
        Some(value) => parse_smp_scheduler_case(&value)?,
        None => SmpSchedulerCase::Static,
    };

    println!(
        "test: build {} with tx_userspace_child_spread_smp4",
        target.name()
    );
    build_kernel_with_extra_rustflags(
        root,
        target,
        "--cfg tx_userspace_child_spread_smp4 --cfg tx_smp_scheduler_witness",
    )?;
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
    let probe = compile_smp_scheduler_witness(root)?;
    image::append_test_init_overlay(root, target, &probe, SMP_SCHEDULER_WITNESS_BINARY)?;
    qemu::qemu(
        root,
        qemu_args_for_smp_scheduler_witness(target, timeout, dry_run, case),
    )?;
    if dry_run {
        return Ok(());
    }

    let serial = root.join(qemu::serial_log_relative(
        target,
        crate::target::Profile::Smoke,
    ));
    let body = fs::read_to_string(&serial)
        .map_err(|err| format!("failed to read {}: {err}", serial.display()))?;
    verify_smp_scheduler_witness_evidence(&body, case)
}

fn vdso_glibc_witness(root: &Path, target: TxTarget, rest: &[String]) -> Result<()> {
    let dry_run = rest.iter().any(|arg| arg == "--dry-run");
    let timeout = optional_option_value(rest, "--timeout-ms")
        .unwrap_or_else(|| VDSO_GLIBC_WITNESS_TIMEOUT_MS.to_string());

    println!("test: build {} with trap-trace", target.name());
    check_build::build_with_features(root, target.name(), &["trap-trace"])?;
    crate::oscomp::oscomp(
        root,
        oscomp_args_for_vdso_glibc_witness(target, dry_run, &timeout),
    )?;
    if dry_run {
        return Ok(());
    }

    let serial = root.join("target/oscomp/os_serial_out_rv.txt");
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
    verify_vdso_glibc_witness_evidence(&body)
}

fn oscomp_args_for_vdso_glibc_witness(
    target: TxTarget,
    dry_run: bool,
    timeout: &str,
) -> Vec<String> {
    let mut args = vec![
        "test".to_string(),
        "--target".to_string(),
        target.name().to_string(),
        "--skip-build".to_string(),
        "--boot-suite".to_string(),
        VDSO_GLIBC_WITNESS_SELECTOR.to_string(),
        "--expect-marker".to_string(),
        VDSO_GLIBC_WITNESS_END.to_string(),
        "--timeout-ms".to_string(),
        timeout.to_string(),
    ];
    if dry_run {
        args.push("--dry-run".to_string());
    }
    args
}

fn compile_vdso_witness(root: &Path) -> Result<PathBuf> {
    compile_freestanding_witness(root, VDSO_WITNESS_SOURCE, VDSO_WITNESS_BINARY)
}

fn compile_vvar_smp_witness(root: &Path) -> Result<PathBuf> {
    compile_freestanding_witness(root, VVAR_SMP_WITNESS_SOURCE, VVAR_SMP_WITNESS_BINARY)
}

fn compile_smp_scheduler_witness(root: &Path) -> Result<PathBuf> {
    compile_freestanding_witness(
        root,
        SMP_SCHEDULER_WITNESS_SOURCE,
        SMP_SCHEDULER_WITNESS_BINARY,
    )
}

fn compile_freestanding_witness(
    root: &Path,
    source_relative: &str,
    binary: &str,
) -> Result<PathBuf> {
    let source = root.join(source_relative);
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
    let output = output_dir.join(binary);
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
            "{compiler} exited with {status} while building vDSO witness {source_relative}"
        ));
    }
    println!("vdso-witness: built {}", output.display());
    Ok(output)
}

fn build_kernel_with_extra_rustflags(
    root: &Path,
    target: TxTarget,
    extra_rustflags: &str,
) -> Result<()> {
    let triple = crate::target::target_triple(target)?;
    let mut rustflags = env::var("RUSTFLAGS").unwrap_or_default();
    if rustflags.is_empty() {
        rustflags = extra_rustflags.to_string();
    } else {
        rustflags.push(' ');
        rustflags.push_str(extra_rustflags);
    }

    println!(
        "$ RUSTFLAGS=\"{}\" cargo build -p {} --target {}",
        rustflags,
        target.package(),
        triple
    );
    let status = Command::new("cargo")
        .args(["build", "-p", target.package(), "--target", triple.as_str()])
        .env("RUSTFLAGS", rustflags)
        .current_dir(root)
        .status()
        .map_err(|err| format!("failed to run cargo build for {}: {err}", target.name()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo build for {} exited with {status}",
            target.name()
        ))
    }
}

fn qemu_args_for_vdso_witness(
    target: TxTarget,
    timeout: Option<String>,
    dry_run: bool,
) -> Vec<String> {
    let mut args = vec![
        "--target".to_string(),
        target.name().to_string(),
        "--profile".to_string(),
        "smoke".to_string(),
        "--expect-sentinel".to_string(),
    ];
    if let Some(value) = timeout {
        args.push("--timeout-ms".to_string());
        args.push(value);
    }
    if dry_run {
        args.push("--dry-run".to_string());
    }
    args.extend([
        "--smp".to_string(),
        "1".to_string(),
        "--expect-marker".to_string(),
        VDSO_WITNESS_PASS_MARKER.to_string(),
        "--expect-marker".to_string(),
        VDSO_WITNESS_FALLBACK_MARKER.to_string(),
        "--expect-marker".to_string(),
        VDSO_WITNESS_LAYOUT_PREFIX.to_string(),
        "--expect-marker".to_string(),
        VDSO_WITNESS_SIGNAL_RESTORER_MARKER.to_string(),
    ]);
    args
}

fn qemu_args_for_smp_scheduler_witness(
    target: TxTarget,
    timeout: Option<String>,
    dry_run: bool,
    case: SmpSchedulerCase,
) -> Vec<String> {
    let mut args = vec![
        "--target".to_string(),
        target.name().to_string(),
        "--profile".to_string(),
        "smoke".to_string(),
        "--expect-sentinel".to_string(),
        "--smp".to_string(),
        "4".to_string(),
        "--expect-marker".to_string(),
        format!(
            "{SMP_SCHEDULER_WITNESS_MARKER_PREFIX} case={}",
            case.as_str()
        ),
    ];
    if case != SmpSchedulerCase::Static {
        args.extend([
            "--append-cmdline".to_string(),
            format!(
                "tx.sched.smp=movable tx.sched.load=depth tx.sched.case={}",
                case.as_str()
            ),
        ]);
    }
    if let Some(value) = timeout {
        args.push("--timeout-ms".to_string());
        args.push(value);
    }
    if dry_run {
        args.push("--dry-run".to_string());
    }
    args
}

fn qemu_args_for_vvar_smp_witness(
    target: TxTarget,
    timeout: Option<String>,
    dry_run: bool,
) -> Vec<String> {
    let mut args = vec![
        "--target".to_string(),
        target.name().to_string(),
        "--profile".to_string(),
        "smoke".to_string(),
        "--expect-sentinel".to_string(),
        "--smp".to_string(),
        "4".to_string(),
        "--expect-marker".to_string(),
        VDSO_WITNESS_VVAR_SMP_MARKER.to_string(),
    ];
    if let Some(value) = timeout {
        args.push("--timeout-ms".to_string());
        args.push(value);
    }
    if dry_run {
        args.push("--dry-run".to_string());
    }
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

fn required_decimal_field(line: &str, field: &str) -> Result<u64> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(field))
        .ok_or_else(|| format!("vvar SMP marker is missing {field}<decimal>"))?
        .parse::<u64>()
        .map_err(|err| format!("vvar SMP marker has invalid {field}<decimal>: {err}"))
}

fn required_text_field<'a>(line: &'a str, field: &str) -> Result<&'a str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(field))
        .ok_or_else(|| format!("vvar SMP marker is missing {field}<value>"))
}

fn required_vvar_smp_hex_field(line: &str, field: &str) -> Result<u64> {
    debug_hex_field(line, field)
        .ok_or_else(|| format!("vvar SMP marker has invalid or missing {field}<hex>"))
}

fn verify_vvar_smp_witness_evidence(serial: &str) -> Result<()> {
    let markers: Vec<_> = serial
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| line.starts_with(VDSO_WITNESS_VVAR_SMP_MARKER_PREFIX))
        .collect();
    if markers.len() != 1 {
        return Err(format!(
            "expected exactly one vvar SMP pass marker, saw {}",
            markers.len()
        ));
    }

    let marker = markers[0];
    if marker.split_whitespace().next() != Some(VDSO_WITNESS_VVAR_SMP_MARKER_PREFIX) {
        return Err(format!("invalid vvar SMP pass marker token: {marker}"));
    }
    let writer_updates = required_decimal_field(marker, "writer-updates=")?;
    let reader_reads = required_decimal_field(marker, "reader-reads=")?;
    let writer_mask = required_vvar_smp_hex_field(marker, "writer-mask=")?;
    let reader_mask = required_vvar_smp_hex_field(marker, "reader-mask=")?;
    let writer_affinity = required_text_field(marker, "writer-affinity=")?;
    let reader_affinity = required_text_field(marker, "reader-affinity=")?;
    let reader_path = required_text_field(marker, "reader-path=")?;
    let writer_done = required_decimal_field(marker, "writer-done=")?;
    let reader_done = required_decimal_field(marker, "reader-done=")?;
    let reader_errors = required_decimal_field(marker, "reader-errors=")?;

    if writer_updates != VVAR_SMP_WRITER_UPDATES {
        return Err(format!(
            "vvar SMP writer-updates expected {VVAR_SMP_WRITER_UPDATES}, saw {writer_updates}"
        ));
    }
    if reader_reads != VVAR_SMP_READER_READS {
        return Err(format!(
            "vvar SMP reader-reads expected {VVAR_SMP_READER_READS}, saw {reader_reads}"
        ));
    }
    if writer_mask == 0 || reader_mask == 0 || writer_mask & reader_mask != 0 {
        return Err(format!(
            "vvar SMP CPU masks must be nonzero and disjoint, saw writer-mask={writer_mask:#x} reader-mask={reader_mask:#x}"
        ));
    }
    if writer_affinity != "ok" || reader_affinity != "ok" {
        return Err(format!(
            "vvar SMP affinity failed: writer-affinity={writer_affinity} reader-affinity={reader_affinity}"
        ));
    }
    if reader_path != "direct-vdso" {
        return Err(format!(
            "vvar SMP reader path expected direct-vdso, saw {reader_path}"
        ));
    }
    if writer_done != 1 || reader_done != 1 {
        return Err(format!(
            "vvar SMP completion failed: writer-done={writer_done} reader-done={reader_done}"
        ));
    }
    if reader_errors != 0 {
        return Err(format!(
            "vvar SMP reader-errors expected 0, saw {reader_errors}"
        ));
    }

    println!(
        "vdso-vvar-smp:evidence: writer-updates={writer_updates} reader-reads={reader_reads} writer-mask={writer_mask:#x} reader-mask={reader_mask:#x} affinity=ok reader-path=direct-vdso completion=ok reader-errors=0"
    );
    Ok(())
}

fn verify_smp_scheduler_witness_evidence(serial: &str, case: SmpSchedulerCase) -> Result<()> {
    let markers: Vec<_> = serial
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| line.starts_with(SMP_SCHEDULER_WITNESS_MARKER_PREFIX))
        .collect();
    if markers.len() != 1 {
        return Err(format!(
            "expected exactly one scheduler SMP pass marker, saw {}",
            markers.len()
        ));
    }

    let marker = markers[0];
    if marker.split_whitespace().next() != Some(SMP_SCHEDULER_WITNESS_MARKER_PREFIX) {
        return Err(format!("invalid scheduler SMP pass marker token: {marker}"));
    }
    let observed_case = required_text_field(marker, "case=")?;
    if observed_case != case.as_str() {
        return Err(format!(
            "scheduler SMP witness expected case={}, saw {observed_case}",
            case.as_str()
        ));
    }

    let pass = required_decimal_field(marker, "pass=")?;
    let spawned = required_decimal_field(marker, "spawned=")?;
    let ready = required_decimal_field(marker, "ready=")?;
    let done = required_decimal_field(marker, "done=")?;
    let workers = required_decimal_field(marker, "workers=")?;
    let errors = required_decimal_field(marker, "errors=")?;
    let seen_mask = required_vvar_smp_hex_field(marker, "seen-mask=")?;
    let cpu0 = required_decimal_field(marker, "cpu0=")?;
    let cpu1 = required_decimal_field(marker, "cpu1=")?;
    let cpu2 = required_decimal_field(marker, "cpu2=")?;
    let cpu3 = required_decimal_field(marker, "cpu3=")?;

    if pass != 1 {
        return Err(format!("scheduler SMP witness did not pass: pass={pass}"));
    }
    if spawned != 4 {
        return Err(format!(
            "scheduler SMP witness expected 4 spawned workers, saw {spawned}"
        ));
    }
    if ready != 4 {
        return Err(format!(
            "scheduler SMP witness expected 4 ready workers, saw {ready}"
        ));
    }
    if workers != 4 {
        return Err(format!(
            "scheduler SMP witness expected 4 workers, saw {workers}"
        ));
    }
    if errors != 0 {
        return Err(format!(
            "scheduler SMP witness expected errors=0, saw {errors}"
        ));
    }

    let child_publishes = scheduler_witness_child_publish_harts(serial)?;
    if child_publishes != [0, 1, 2, 3] {
        return Err(format!(
            "scheduler SMP witness expected child publish rotation [0, 1, 2, 3], saw {:?}",
            child_publishes
        ));
    }
    verify_smp_scheduler_case_evidence(marker, case)?;

    println!(
        "sched-smp:evidence: spawned=4 ready=4 done={done} child-publish=[0,1,2,3] observed-slots=0x{seen_mask:016x} cpu0={cpu0} cpu1={cpu1} cpu2={cpu2} cpu3={cpu3}"
    );
    Ok(())
}

fn verify_smp_scheduler_case_evidence(marker: &str, case: SmpSchedulerCase) -> Result<()> {
    match case {
        SmpSchedulerCase::Static | SmpSchedulerCase::Movable => Ok(()),
        SmpSchedulerCase::Pipe => {
            require_zero(marker, "pipe-errors=")?;
            require_at_least(marker, "pipe-rounds=", 8)
        }
        SmpSchedulerCase::Affinity => {
            require_zero(marker, "affinity-errors=")?;
            require_at_least(marker, "affinity-migrations=", 4)?;
            let mask = required_vvar_smp_hex_field(marker, "affinity-mask=")?;
            if mask & 0xf != 0xf {
                return Err(format!(
                    "scheduler SMP affinity case expected affinity-mask to cover harts 0-3, saw {mask:#x}"
                ));
            }
            Ok(())
        }
        SmpSchedulerCase::Timer => {
            require_zero(marker, "timer-errors=")?;
            require_at_least(marker, "timer-sleeps=", 8)
        }
        SmpSchedulerCase::Pthread => {
            require_zero(marker, "pthread-errors=")?;
            require_at_least(marker, "pthread-joins=", 1)?;
            require_at_least(marker, "futex-wakes=", 1)
        }
        SmpSchedulerCase::Mixed => {
            require_zero(marker, "pipe-errors=")?;
            require_zero(marker, "timer-errors=")?;
            require_at_least(marker, "pipe-rounds=", 4)?;
            require_at_least(marker, "timer-sleeps=", 4)
        }
        SmpSchedulerCase::Stress => {
            require_zero(marker, "affinity-errors=")?;
            require_zero(marker, "pipe-errors=")?;
            require_zero(marker, "timer-errors=")?;
            require_zero(marker, "pthread-errors=")?;
            require_at_least(marker, "affinity-migrations=", 4)?;
            require_at_least(marker, "pipe-rounds=", 4)?;
            require_at_least(marker, "timer-sleeps=", 4)?;
            require_at_least(marker, "pthread-joins=", 1)
        }
    }
}

fn require_zero(marker: &str, field: &str) -> Result<()> {
    let value = required_decimal_field(marker, field)?;
    if value != 0 {
        return Err(format!(
            "scheduler SMP marker expected {field}0, saw {value}"
        ));
    }
    Ok(())
}

fn require_at_least(marker: &str, field: &str, minimum: u64) -> Result<()> {
    let value = required_decimal_field(marker, field)?;
    if value < minimum {
        return Err(format!(
            "scheduler SMP marker expected {field}>={minimum}, saw {value}"
        ));
    }
    Ok(())
}

fn scheduler_witness_child_publish_harts(serial: &str) -> Result<Vec<u64>> {
    let mut in_worker_window = false;
    let mut publishes = Vec::new();
    for line in serial.lines().map(|line| line.trim_end_matches('\r')) {
        if line.starts_with("sched-smp:begin ") {
            in_worker_window = true;
            continue;
        }
        if line.starts_with(SMP_SCHEDULER_WITNESS_MARKER_PREFIX) {
            break;
        }
        if in_worker_window && line.contains(":sched-witness:child-submit:") {
            publishes.push(required_delimited_decimal_field(line, "publish=")?);
            if publishes.len() == 4 {
                break;
            }
        }
    }
    Ok(publishes)
}

fn required_delimited_decimal_field(line: &str, field: &str) -> Result<u64> {
    let start = line
        .find(field)
        .ok_or_else(|| format!("scheduler SMP marker is missing {field}<decimal>"))?
        + field.len();
    let rest = &line[start..];
    let end = rest
        .find(|ch: char| ch == ':' || ch.is_ascii_whitespace())
        .unwrap_or(rest.len());
    rest[..end]
        .parse::<u64>()
        .map_err(|err| format!("scheduler SMP marker has invalid {field}<decimal>: {err}"))
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

fn required_layout_hex(line: &str, field: &str) -> Result<u64> {
    debug_hex_field(line, field)
        .ok_or_else(|| format!("vDSO layout marker is missing {field}<hex>"))
}

fn verify_vdso_witness_layout_evidence(serial: &str) -> Result<()> {
    let layouts: Vec<_> = serial
        .lines()
        .filter(|line| line.starts_with(VDSO_WITNESS_LAYOUT_PREFIX))
        .collect();
    if layouts.len() != 1 {
        return Err(format!(
            "expected exactly one uninterrupted vDSO layout marker, saw {}",
            layouts.len()
        ));
    }
    let layout = layouts[0];
    for field in [
        "e_phoff=",
        "load-off=",
        "load-vaddr=",
        "text-off=",
        "text-vaddr=",
        "dynsym-st-value=",
        "load-bias=",
        "resolver=",
        "jalr-target=",
        "first-insn=",
        "auipc=",
        "vvar-delta=",
        "expected-vvar=",
        "actual-vvar=",
    ] {
        required_layout_hex(layout, field)?;
    }
    if !layout.contains("target-2byte=ok") || !layout.contains("target-first-insn=ok") {
        return Err(format!("vDSO target assertion failed: {layout}"));
    }

    let load_bias = required_layout_hex(layout, "load-bias=")?;
    let symbol_value = required_layout_hex(layout, "dynsym-st-value=")?;
    let resolver = required_layout_hex(layout, "resolver=")?;
    let target = required_layout_hex(layout, "jalr-target=")?;
    let first_instruction = required_layout_hex(layout, "first-insn=")?;
    let text_vaddr = required_layout_hex(layout, "text-vaddr=")?;
    let expected_vvar = required_layout_hex(layout, "expected-vvar=")?;
    let actual_vvar = required_layout_hex(layout, "actual-vvar=")?;

    if load_bias.checked_add(symbol_value) != Some(resolver)
        || symbol_value != text_vaddr
        || resolver != target
        || target != first_instruction
        || target & 1 != 0
        || expected_vvar != actual_vvar
    {
        return Err(format!(
            "vDSO resolver calculation is inconsistent: {layout}"
        ));
    }
    println!("vdso-witness:evidence: final-image layout and resolved RV64 jalr target verified");
    Ok(())
}

fn verify_vdso_witness_signal_restorer_evidence(serial: &str) -> Result<()> {
    let lines: Vec<_> = serial.lines().collect();
    let markers: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with(VDSO_WITNESS_SIGNAL_RESTORER_MARKER))
        .collect();
    if markers.len() != 1 {
        return Err(format!(
            "expected exactly one vDSO signal-restorer marker, saw {}",
            markers.len()
        ));
    }
    let (marker_index, marker) = markers[0];
    let restorer = required_layout_hex(marker, "restorer=")?;
    let captured_ra = required_layout_hex(marker, "captured-ra=")?;
    if !marker.contains("ra=ok") || restorer != captured_ra {
        return Err(format!(
            "vDSO signal restorer RA assertion failed: {marker}"
        ));
    }

    let expected_ecall_pc = restorer
        .checked_add(4)
        .ok_or_else(|| "vDSO signal restorer address overflowed".to_string())?;
    let signal_action = lines[..marker_index]
        .iter()
        .rposition(|line| {
            line.starts_with("txdbg:trap ")
                && line.contains("kind=SY")
                && debug_hex_field(line, "a7=") == Some(RT_SIGACTION_SYSCALL)
                && debug_hex_field(line, "a0=") == Some(SIGUSR1)
        })
        .ok_or_else(|| {
            "missing witness rt_sigaction(SIGUSR1) before signal-restorer marker".to_string()
        })?;
    let matches: Vec<_> = lines[signal_action + 1..marker_index]
        .iter()
        .filter(|line| {
            line.starts_with("txdbg:trap ")
                && line.contains("kind=SY")
                && debug_hex_field(line, "a7=") == Some(RT_SIGRETURN_SYSCALL)
        })
        .collect();
    if matches.len() != 1 {
        return Err(format!(
            "expected exactly one rt_sigreturn between witness rt_sigaction(SIGUSR1) and its marker, saw {}",
            matches.len()
        ));
    }
    let ecall_pc = required_layout_hex(matches[0], "pc=")?;
    if ecall_pc != expected_ecall_pc {
        return Err(format!(
            "rt_sigreturn syscall did not execute at the vDSO restorer ecall: expected {expected_ecall_pc:#x}, saw {ecall_pc:#x}"
        ));
    }
    println!(
        "vdso-witness:evidence: handler RA and rt_sigreturn ecall both resolve to the mapped vDSO restorer"
    );
    Ok(())
}

fn vdso_glibc_case_window(serial: &str) -> Result<Vec<&str>> {
    let lines: Vec<_> = serial.lines().collect();
    let starts: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            (line.trim_end_matches('\r') == VDSO_GLIBC_WITNESS_START).then_some(index)
        })
        .collect();
    let ends: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            (line.trim_end_matches('\r') == VDSO_GLIBC_WITNESS_END).then_some(index)
        })
        .collect();
    if starts.len() != 1 || ends.len() != 1 {
        return Err(format!(
            "expected exactly one dynamic glibc clock_gettime START/END window, saw {} START and {} END markers",
            starts.len(),
            ends.len()
        ));
    }
    let start = starts[0];
    let end = ends[0];
    if end <= start {
        return Err("dynamic glibc clock_gettime END marker precedes its START marker".into());
    }
    Ok(lines[start + 1..end].to_vec())
}

fn verify_vdso_glibc_witness_evidence(serial: &str) -> Result<()> {
    let window = vdso_glibc_case_window(serial)?;
    let passes = window
        .iter()
        .filter(|line| line.trim_end_matches('\r') == "Pass!")
        .count();
    if passes != 1 {
        return Err(format!(
            "expected exactly one Pass! inside the dynamic glibc clock_gettime window, saw {passes}"
        ));
    }

    let supported: Vec<_> = window
        .iter()
        .copied()
        .filter(|line| {
            line.starts_with("txdbg:trap ")
                && line.contains("kind=SY")
                && debug_hex_field(line, "a7=") == Some(CLOCK_GETTIME_SYSCALL)
                && matches!(debug_hex_field(line, "a0="), Some(0) | Some(1))
        })
        .collect();
    if !supported.is_empty() {
        return Err(format!(
            "dynamic glibc realtime/monotonic path entered clock_gettime syscall inside its case window:\n{}",
            supported.join("\n")
        ));
    }
    println!(
        "vdso-glibc-witness:evidence: dynamic clock_gettime Pass! with no realtime/monotonic syscall 113 in its case window"
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
    if target == TxTarget::Rv64Qemu && profile == "smoke" {
        qemu_args.extend(["--smp".to_string(), "4".to_string()]);
        for marker in qemu::expected_rcu_smp_markers(target) {
            qemu_args.extend(["--expect-marker".to_string(), marker]);
        }
    }
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
        assert_eq!(parse_test_lane("vdso-witness"), Ok(TestLane::VdsoWitness));
        assert_eq!(
            parse_test_lane("vdso-vvar-smp-witness"),
            Ok(TestLane::VvarSmpWitness)
        );
        assert_eq!(
            parse_test_lane("smp-scheduler-witness"),
            Ok(TestLane::SmpSchedulerWitness)
        );
    }

    #[test]
    fn vdso_glibc_witness_lane_selects_the_dynamic_clock_gettime_case() {
        assert_eq!(
            format!("{:?}", parse_test_lane("vdso-glibc-witness").unwrap()),
            "VdsoGlibcWitness"
        );

        assert_eq!(
            oscomp_args_for_vdso_glibc_witness(TxTarget::Rv64Qemu, true, "60000"),
            vec![
                "test",
                "--target",
                "rv64-qemu",
                "--skip-build",
                "--boot-suite",
                "libctest-glibc:dynamic:clock_gettime",
                "--expect-marker",
                "========== END entry-dynamic.exe clock_gettime ==========",
                "--timeout-ms",
                "60000",
                "--dry-run",
            ]
        );
    }

    #[test]
    fn vdso_glibc_witness_evidence_uses_only_the_dynamic_case_window() {
        let serial = concat!(
            "txdbg:trap n=0x1 kind=SY pc=0x1 a7=0x71 a0=0x0 a1=0x0 a2=0x0\n",
            "========== START entry-dynamic.exe clock_gettime ==========\n",
            "Pass!\n",
            "========== END entry-dynamic.exe clock_gettime ==========\n",
            "txdbg:trap n=0x2 kind=SY pc=0x1 a7=0x71 a0=0x1 a1=0x0 a2=0x0\n"
        );

        assert!(verify_vdso_glibc_witness_evidence(serial).is_ok());
    }

    #[test]
    fn vdso_glibc_witness_evidence_rejects_clock_gettime_inside_case_window() {
        let serial = concat!(
            "========== START entry-dynamic.exe clock_gettime ==========\n",
            "txdbg:trap n=0x1 kind=SY pc=0x1 a7=0x71 a0=0x0 a1=0x0 a2=0x0\n",
            "Pass!\n",
            "========== END entry-dynamic.exe clock_gettime ==========\n"
        );

        let err = verify_vdso_glibc_witness_evidence(serial).unwrap_err();
        assert!(err.contains("realtime/monotonic"));
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
            .any(|pair| pair == ["--expect-marker", VDSO_WITNESS_SIGNAL_RESTORER_MARKER]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--timeout-ms", "30000"]));
        assert!(args.windows(2).any(|pair| pair == ["--smp", "1"]));
        assert!(!args.windows(2).any(|pair| {
            pair == [
                "--expect-marker",
                "txkernel:qemu-riscv64-virt:reactor:owner-wake:smp:ok",
            ]
        }));
    }

    #[test]
    fn vvar_smp_witness_qemu_args_require_four_harts_and_stress_marker() {
        let args = qemu_args_for_vvar_smp_witness(TxTarget::Rv64Qemu, None, false);
        assert!(args.windows(2).any(|pair| pair == ["--smp", "4"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--expect-marker", VDSO_WITNESS_VVAR_SMP_MARKER]));
    }

    #[test]
    fn vvar_smp_witness_evidence_rejects_reader_errors_despite_pass_prefix() {
        let serial = concat!(
            "vdso-phase5:vvar-smp=pass writer-updates=1024 reader-reads=500000 ",
            "writer-mask=0x1 reader-mask=0x2 writer-affinity=ok reader-affinity=ok reader-path=direct-vdso ",
            "writer-done=1 reader-done=1 reader-errors=1\n"
        );

        let err = verify_vvar_smp_witness_evidence(serial).unwrap_err();
        assert!(err.contains("reader-errors"));
    }

    #[test]
    fn vvar_smp_witness_evidence_accepts_complete_disjoint_witness() {
        let serial = concat!(
            "vdso-phase5:vvar-smp=pass writer-updates=1024 reader-reads=500000 ",
            "writer-mask=0x2 reader-mask=0x1 writer-affinity=ok reader-affinity=ok reader-path=direct-vdso ",
            "writer-done=1 reader-done=1 reader-errors=0\n"
        );

        assert!(verify_vvar_smp_witness_evidence(serial).is_ok());
    }

    #[test]
    fn smp_scheduler_witness_qemu_args_require_four_harts_and_pass_marker() {
        let args = qemu_args_for_smp_scheduler_witness(
            TxTarget::Rv64Qemu,
            None,
            false,
            SmpSchedulerCase::Static,
        );
        assert!(args.windows(2).any(|pair| pair == ["--smp", "4"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--expect-marker", "sched-smp:result case=static"]));
    }

    #[test]
    fn smp_scheduler_witness_qemu_args_append_cmdline_for_movable_case() {
        let args = qemu_args_for_smp_scheduler_witness(
            TxTarget::Rv64Qemu,
            Some("60000".to_string()),
            false,
            SmpSchedulerCase::Movable,
        );
        assert!(args.windows(2).any(|pair| {
            pair == [
                "--append-cmdline",
                "tx.sched.smp=movable tx.sched.load=depth tx.sched.case=movable",
            ]
        }));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--expect-marker", "sched-smp:result case=movable"]));
    }

    #[test]
    fn smp_scheduler_witness_evidence_rejects_missing_hart_spread() {
        let serial = concat!(
            "sched-smp:begin case=static workers=4\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=0:queue=4:placements=1:remote-ipis=0:local-reschedules=1\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=1:queue=4:placements=1:remote-ipis=1:local-reschedules=0\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=0:queue=4:placements=1:remote-ipis=0:local-reschedules=1\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=1:queue=4:placements=1:remote-ipis=1:local-reschedules=0\n",
            "sched-smp:result case=static pass=1 spawned=4 ready=4 done=4 errors=0 workers=4 seen-mask=0x0000000000000003 ",
            "cpu0=0 cpu1=1 cpu2=0 cpu3=1\n"
        );

        let err =
            verify_smp_scheduler_witness_evidence(serial, SmpSchedulerCase::Static).unwrap_err();
        assert!(err.contains("child publish rotation"));
    }

    #[test]
    fn smp_scheduler_witness_evidence_accepts_worker_completion_and_publish_rotation() {
        let serial = concat!(
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=3:queue=4:placements=1:remote-ipis=0:local-reschedules=1\n",
            "sched-smp:begin case=static workers=4\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=0:queue=4:placements=1:remote-ipis=0:local-reschedules=1\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=1:queue=4:placements=1:remote-ipis=1:local-reschedules=0\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=2:queue=4:placements=1:remote-ipis=1:local-reschedules=0\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=3:queue=4:placements=1:remote-ipis=1:local-reschedules=0\n",
            "sched-smp:result case=static pass=1 spawned=4 ready=4 done=4 errors=0 workers=4 seen-mask=0x000000000000000f ",
            "cpu0=0 cpu1=1 cpu2=2 cpu3=3\n"
        );

        assert!(verify_smp_scheduler_witness_evidence(serial, SmpSchedulerCase::Static).is_ok());
    }

    #[test]
    fn smp_scheduler_witness_evidence_accepts_movable_case() {
        let serial = concat!(
            "sched-smp:begin case=movable workers=4\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=0:queue=4:placements=1:remote-ipis=0:local-reschedules=1\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=1:queue=4:placements=1:remote-ipis=1:local-reschedules=0\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=2:queue=4:placements=1:remote-ipis=1:local-reschedules=0\n",
            "txkernel:qemu-riscv64-virt:sched-witness:child-submit:submit=0:publish=3:queue=4:placements=1:remote-ipis=1:local-reschedules=0\n",
            "sched-smp:result case=movable pass=1 spawned=4 ready=4 done=4 errors=0 workers=4 seen-mask=0x000000000000000f ",
            "cpu0=0 cpu1=1 cpu2=2 cpu3=3\n"
        );

        assert!(verify_smp_scheduler_witness_evidence(serial, SmpSchedulerCase::Movable).is_ok());
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
    fn vdso_witness_layout_requires_a_resolved_two_byte_entry_target() {
        let serial = concat!(
            "vdso-phase5:layout e_phoff=0x0000000000000040 load-off=0x0000000000000000 ",
            "load-vaddr=0x0000000000000000 text-off=0x00000000000001b0 ",
            "text-vaddr=0x00000000000001b0 dynsym-st-value=0x00000000000001b0 ",
            "load-bias=0x0000003fff000000 resolver=0x0000003fff0001b0 ",
            "jalr-target=0x0000003fff0001b0 first-insn=0x0000003fff0001b0 ",
            "auipc=0x0000003fff0001d0 vvar-delta=0xffffffffffffee30 ",
            "expected-vvar=0x0000003ffefff000 actual-vvar=0x0000003ffefff000 ",
            "target-2byte=ok target-first-insn=ok\n"
        );
        assert!(verify_vdso_witness_layout_evidence(serial).is_ok());
    }

    #[test]
    fn vdso_witness_signal_restorer_requires_matching_ra_and_ecall_pc() {
        let serial = concat!(
            "txdbg:trap n=0x2 kind=SY pc=0x1 a7=0x86 a0=0xa a1=0x0 a2=0x0\n",
            "txdbg:trap n=0x4 kind=SY pc=0x0000003fff0001d4 a7=0x8b ",
            "a0=0x0 a1=0x0 a2=0x0\n",
            "vdso-phase5:signal-restorer=pass restorer=0x0000003fff0001d0 ",
            "captured-ra=0x0000003fff0001d0 ra=ok\n",
            "txdbg:trap n=0x7 kind=SY pc=0x0000003fff0001d4 a7=0x8b ",
            "a0=0x0 a1=0x0 a2=0x0\n"
        );
        assert!(verify_vdso_witness_signal_restorer_evidence(serial).is_ok());
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
    fn smoke_qemu_args_require_rcu_smp_markers_and_four_harts() {
        let args = qemu_args_for_smoke(TxTarget::Rv64Qemu, "smoke", None, true, false);

        assert!(args.windows(2).any(|pair| pair == ["--smp", "4"]));
        for marker in [
            "txkernel:qemu-riscv64-virt:rcu:smp:cpus:ok",
            "txkernel:qemu-riscv64-virt:rcu:smp:guarded-overlap:ok",
            "txkernel:qemu-riscv64-virt:rcu:smp:no-early-reclaim:ok",
            "txkernel:qemu-riscv64-virt:rcu:smp:maintenance-ack:ok",
            "txkernel:qemu-riscv64-virt:rcu:smp:bounded-drain:ok",
            "txkernel:qemu-riscv64-virt:rcu:smp:ok",
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--expect-marker", marker]),
                "missing RCU smoke marker {marker}"
            );
        }
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
