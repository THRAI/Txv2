use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::image::{busybox_initramfs_name, busybox_root_ext4_name};
use crate::target::{Profile, TxTarget};
use crate::util::{option_value, optional_option_value, shell_join, tail_lines};
use crate::Result;

const DEFAULT_SENTINEL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
struct QemuOptions {
    expect_sentinel: bool,
    timeout: Duration,
    /// Skip the busybox-profile virtio-blk drive wiring. Used by smoke
    /// runs that only need the initramfs to come up; it sidesteps the
    /// `mkfs.ext4` host-tool dependency.
    no_block: bool,
    /// Run with `-serial mon:stdio` instead of `-serial file:...` so
    /// stdin/stdout connect to the host terminal. Drops the per-run
    /// serial log file and the sentinel-watcher; the user (or the
    /// shell-test driver) sees output directly and types into stdin.
    /// Ctrl-A C to drop into the QEMU monitor, Ctrl-A X to quit.
    interactive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SentinelState {
    Found,
    Trapped,
    Pending,
    TimedOut,
}

pub(crate) fn qemu(root: &Path, args: Vec<String>) -> Result<()> {
    let target = TxTarget::parse(&option_value(&args, "--target")?)?;
    let profile = Profile::parse(&option_value(&args, "--profile")?)?;
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let options = qemu_options(&args)?;
    let command = qemu_command(root, target, profile, &options)?;

    println!("{}", shell_join(&command));
    if options.expect_sentinel {
        println!("serial: {}", serial_log_relative(target, profile).display());
        println!("expect: {}", expected_sentinel(target));
        println!("timeout-ms: {}", options.timeout.as_millis());
    }
    if dry_run {
        return Ok(());
    }

    if options.expect_sentinel {
        return run_with_sentinel(root, &command, target, profile, options);
    }

    let Some((program, rest)) = command.split_first() else {
        return Err("empty qemu command".into());
    };
    let status = Command::new(program)
        .args(rest)
        .current_dir(root)
        .status()
        .map_err(|err| format!("failed to run {program}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("qemu exited with {status}"))
    }
}

fn qemu_options(args: &[String]) -> Result<QemuOptions> {
    let timeout = optional_option_value(args, "--timeout-ms")
        .map(|value| {
            value
                .parse::<u64>()
                .map(Duration::from_millis)
                .map_err(|err| format!("invalid --timeout-ms value '{value}': {err}"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_SENTINEL_TIMEOUT);

    Ok(QemuOptions {
        expect_sentinel: args.iter().any(|arg| arg == "--expect-sentinel"),
        timeout,
        no_block: args.iter().any(|arg| arg == "--no-block"),
        interactive: args.iter().any(|arg| arg == "--interactive"),
    })
}

fn qemu_command(
    root: &Path,
    target: TxTarget,
    profile: Profile,
    options: &QemuOptions,
) -> Result<Vec<String>> {
    let kernel = target.kernel_path(root);
    let serial_log = serial_log_relative(target, profile);
    let mut args = vec![
        target.qemu_binary().to_string(),
        "-machine".to_string(),
        target.qemu_machine().to_string(),
    ];

    if let Some(cpu) = qemu_cpu(target) {
        args.push("-cpu".to_string());
        args.push(cpu.to_string());
    }

    args.extend([
        "-m".to_string(),
        qemu_memory(target).to_string(),
        "-smp".to_string(),
        match target {
            TxTarget::Rv64Qemu => "4",
            TxTarget::La64Qemu => {
                // LA64's current boot path expects the QEMU virt SMP
                // shape; Debian QEMU 8.2.2 can SIGSEGV with this kernel
                // under `-smp 1`, including interactive runs.
                "4"
            }
            TxTarget::Rv64M1DockMock => "1",
        }
        .to_string(),
        "-display".to_string(),
        "none".to_string(),
    ]);

    if options.interactive {
        // `-serial mon:stdio` multiplexes the QEMU monitor and the
        // guest's UART onto the host stdio. Ctrl-A C drops into the
        // monitor; Ctrl-A X quits. No `-monitor` here — it's already
        // multiplexed in.
        args.push("-serial".to_string());
        args.push("mon:stdio".to_string());
    } else {
        args.push("-monitor".to_string());
        args.push("none".to_string());
        args.push("-serial".to_string());
        args.push(format!("file:{}", serial_log.display()));
    }
    args.push("-no-reboot".to_string());
    args.push("-kernel".to_string());
    args.push(kernel.display().to_string());

    match target {
        TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock => {
            args.push("-bios".into());
            args.push("default".into());
        }
        TxTarget::La64Qemu => {}
    }

    if options.expect_sentinel {
        args.push("-no-shutdown".into());
    }

    if profile == Profile::Busybox {
        let initramfs = root
            .join("target")
            .join("images")
            .join(busybox_initramfs_name(target));
        args.push("-initrd".into());
        args.push(initramfs.display().to_string());
        args.push("-append".into());
        if target == TxTarget::Rv64M1DockMock {
            args.push("tx.profile=busybox tx.board=m1dock-mock tx.mock.spi0.cs0=target/images/m1dock-sd.img console=ttyS0".into());
        } else {
            args.push("tx.profile=busybox console=ttyS0".into());
        }
    } else {
        args.push("-append".into());
        if target == TxTarget::Rv64M1DockMock {
            args.push("tx.profile=smoke tx.board=m1dock-mock console=ttyS0".into());
        } else {
            args.push("tx.profile=smoke console=ttyS0".into());
        }
    }

    if profile == Profile::Busybox && !options.no_block {
        args.push("-device".into());
        match target {
            TxTarget::Rv64M1DockMock => {
                args.push("virtio-blk-device,drive=m1sd,bus=virtio-mmio-bus.0".into());
            }
            TxTarget::La64Qemu => {
                args.push("virtio-blk-pci-non-transitional,drive=txblk0,rombar=0".into());
            }
            TxTarget::Rv64Qemu => {
                args.push("virtio-blk-device,drive=txblk0".into());
            }
        }
        args.push("-drive".into());
        if target == TxTarget::Rv64M1DockMock {
            args.push("file=target/images/m1dock-sd.img,format=raw,if=none,id=m1sd".into());
        } else if target == TxTarget::La64Qemu {
            args.push(format!(
                "driver=raw,file.driver=file,file.filename=target/images/{},file.locking=off,if=none,id=txblk0,read-only=on",
                busybox_root_ext4_name(target)
            ));
        } else {
            args.push(format!(
                "file=target/images/{},format=raw,if=none,id=txblk0",
                busybox_root_ext4_name(target)
            ));
        }
    }
    args.push("-d".into());
    args.push("guest_errors".into());
    args.push("-D".into());
    args.push(format!(
        "target/qemu-{}-{}.log",
        target.name(),
        profile.name()
    ));
    Ok(args)
}

fn qemu_cpu(target: TxTarget) -> Option<&'static str> {
    match target {
        TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock => None,
        TxTarget::La64Qemu => Some("la464"),
    }
}

fn qemu_memory(target: TxTarget) -> &'static str {
    match target {
        TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock => "256M",
        TxTarget::La64Qemu => "1152M",
    }
}

fn run_with_sentinel(
    root: &Path,
    command: &[String],
    target: TxTarget,
    profile: Profile,
    options: QemuOptions,
) -> Result<()> {
    let serial_log = root.join(serial_log_relative(target, profile));
    if let Some(parent) = serial_log.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    match fs::remove_file(&serial_log) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("failed to reset {}: {err}", serial_log.display())),
    }

    let Some((program, rest)) = command.split_first() else {
        return Err("empty qemu command".into());
    };
    let mut child = Command::new(program)
        .args(rest)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|err| format!("failed to run {program}: {err}"))?;

    let sentinel = expected_sentinel(target);
    let started = Instant::now();

    loop {
        let serial = fs::read_to_string(&serial_log).unwrap_or_default();
        match sentinel_state(&serial, &sentinel, started.elapsed(), options.timeout) {
            SentinelState::Found => {
                let _ = child.kill();
                let _ = child.wait();
                println!("qemu smoke sentinel observed: {sentinel}");
                return Ok(());
            }
            SentinelState::TimedOut => {
                let _ = child.kill();
                let _ = child.wait();
                let annotation = fault_decode_annotation(root, target, &serial_log, &serial);
                return Err(format!(
                    "qemu timed out after {} ms waiting for {sentinel}\nserial tail:\n{}{}",
                    options.timeout.as_millis(),
                    tail_lines(&serial, 80),
                    annotation.unwrap_or_default()
                ));
            }
            SentinelState::Trapped => {
                let _ = child.kill();
                let _ = child.wait();
                let annotation = fault_decode_annotation(root, target, &serial_log, &serial);
                return Err(format!(
                    "qemu observed trap before sentinel {sentinel}\nserial tail:\n{}{}",
                    tail_lines(&serial, 80),
                    annotation.unwrap_or_default()
                ));
            }
            SentinelState::Pending => {}
        }

        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            let serial = fs::read_to_string(&serial_log).unwrap_or_default();
            if serial_contains_sentinel(&serial, &sentinel) {
                println!("qemu smoke sentinel observed: {sentinel}");
                return Ok(());
            }
            let annotation = fault_decode_annotation(root, target, &serial_log, &serial);
            return Err(format!(
                "qemu exited with {status} before sentinel {sentinel}\nserial tail:\n{}{}",
                tail_lines(&serial, 80),
                annotation.unwrap_or_default()
            ));
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn expected_sentinel(target: TxTarget) -> String {
    format!("txkernel:{}:boot:ok", target.board_name())
}

fn serial_contains_sentinel(serial: &str, sentinel: &str) -> bool {
    serial.contains(sentinel)
}

fn sentinel_state(
    serial: &str,
    sentinel: &str,
    elapsed: Duration,
    timeout: Duration,
) -> SentinelState {
    if serial_contains_sentinel(serial, sentinel) {
        SentinelState::Found
    } else if serial_contains_trap_summary(serial) {
        SentinelState::Trapped
    } else if elapsed >= timeout {
        SentinelState::TimedOut
    } else {
        SentinelState::Pending
    }
}

fn serial_contains_trap_summary(serial: &str) -> bool {
    serial
        .lines()
        .any(|line| line.contains("scause=") && line.contains("sepc=") && line.contains("stval="))
}

fn fault_decode_annotation(
    root: &Path,
    target: TxTarget,
    serial_log: &Path,
    serial: &str,
) -> Option<String> {
    fault_decode_annotation_with_runner(target, serial_log, serial, |args| {
        let exe = std::env::current_exe().ok()?;
        let output = Command::new(exe)
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
        Some(FaultDecodeRun {
            success: output.status.success(),
            status: output.status.to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FaultDecodeRun {
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

fn fault_decode_annotation_with_runner<F>(
    target: TxTarget,
    serial_log: &Path,
    serial: &str,
    run_fault_decode: F,
) -> Option<String>
where
    F: FnOnce(&[String]) -> Option<FaultDecodeRun>,
{
    if !serial_contains_trap_summary(serial) {
        return None;
    }

    let args = fault_decode_args_for_serial(target, serial_log)?;
    let output = run_fault_decode(&args)?;
    if output.success {
        return Some(format!("\n\nfault-decode:\n{}", output.stdout.trim_end()));
    }

    let stdout = output.stdout.trim_end();
    let stderr = output.stderr.trim_end();
    let mut details = String::new();
    if !stdout.is_empty() {
        details.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !details.is_empty() {
            details.push('\n');
        }
        details.push_str(stderr);
    }

    Some(format!(
        "\n\nfault-decode failed with {}:\n{}",
        output.status, details
    ))
}

fn fault_decode_args_for_serial(target: TxTarget, serial_log: &Path) -> Option<Vec<String>> {
    if target != TxTarget::Rv64Qemu {
        return None;
    }

    Some(vec![
        "fault-decode".to_string(),
        "--target".to_string(),
        target.name().to_string(),
        "--serial".to_string(),
        serial_log.display().to_string(),
    ])
}

fn serial_log_relative(target: TxTarget, profile: Profile) -> PathBuf {
    PathBuf::from(format!(
        "target/qemu-{}-{}.serial.log",
        target.name(),
        profile.name()
    ))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    #[test]
    fn derives_rv64_qemu_sentinel_from_board_name() {
        assert_eq!(
            expected_sentinel(TxTarget::Rv64Qemu),
            "txkernel:qemu-riscv64-virt:boot:ok"
        );
    }

    #[test]
    fn matches_sentinel_inside_serial_output() {
        assert!(serial_contains_sentinel(
            "OpenSBI\n txkernel:qemu-riscv64-virt:boot:ok\n",
            "txkernel:qemu-riscv64-virt:boot:ok"
        ));
        assert!(!serial_contains_sentinel(
            "OpenSBI\n txkernel:qemu-riscv64-virt:boot:pending\n",
            "txkernel:qemu-riscv64-virt:boot:ok"
        ));
    }

    #[test]
    fn reports_timeout_when_sentinel_is_absent_past_limit() {
        assert_eq!(
            sentinel_state(
                "OpenSBI\n",
                "txkernel:qemu-riscv64-virt:boot:ok",
                Duration::from_millis(1500),
                Duration::from_millis(1000)
            ),
            SentinelState::TimedOut
        );
    }

    #[test]
    fn reports_trap_before_sentinel_timeout() {
        assert_eq!(
            sentinel_state(
                "txkernel:qemu-riscv64-virt:trap\nscause=0xd sepc=0xffffffff80201234 stval=0x40001000\n",
                "txkernel:qemu-riscv64-virt:boot:ok",
                Duration::from_millis(10),
                Duration::from_secs(10)
            ),
            SentinelState::Trapped
        );
    }

    #[test]
    fn builds_fault_decode_serial_args_for_rv64_qemu_traps() {
        let args = fault_decode_args_for_serial(
            TxTarget::Rv64Qemu,
            Path::new("/tmp/tx/target/qemu-rv64-qemu-smoke.serial.log"),
        )
        .expect("rv64-qemu should support fault decode annotation");

        assert_eq!(
            args,
            vec![
                "fault-decode",
                "--target",
                "rv64-qemu",
                "--serial",
                "/tmp/tx/target/qemu-rv64-qemu-smoke.serial.log",
            ]
        );
        assert!(fault_decode_args_for_serial(
            TxTarget::La64Qemu,
            Path::new("/tmp/tx/target/qemu-la64-qemu-smoke.serial.log"),
        )
        .is_none());
    }

    #[test]
    fn fault_decode_annotation_embeds_runner_output_for_rv64_traps() {
        let mut observed_args = None;
        let annotation = fault_decode_annotation_with_runner(
            TxTarget::Rv64Qemu,
            Path::new("/tmp/tx/target/qemu-rv64-qemu-smoke.serial.log"),
            "scause=0xd sepc=0xffffffff80201234 stval=0x40001000\n",
            |args| {
                observed_args = Some(args.to_vec());
                Some(FaultDecodeRun {
                    success: true,
                    status: "exit status: 0".into(),
                    stdout: "trap #1\nsepc:\n  raw: 0xffffffff80201234\n".into(),
                    stderr: String::new(),
                })
            },
        )
        .expect("rv64 trap serial should produce annotation");

        assert_eq!(
            observed_args,
            Some(vec![
                "fault-decode".to_string(),
                "--target".to_string(),
                "rv64-qemu".to_string(),
                "--serial".to_string(),
                "/tmp/tx/target/qemu-rv64-qemu-smoke.serial.log".to_string(),
            ])
        );
        assert!(annotation.contains("fault-decode:\ntrap #1"));
        assert!(annotation.contains("0xffffffff80201234"));
    }

    #[test]
    fn fault_decode_annotation_reports_decoder_failure() {
        let annotation = fault_decode_annotation_with_runner(
            TxTarget::Rv64Qemu,
            Path::new("/tmp/tx/target/qemu-rv64-qemu-smoke.serial.log"),
            "scause=0xd sepc=0xffffffff80201234 stval=0x40001000\n",
            |_| {
                Some(FaultDecodeRun {
                    success: false,
                    status: "exit status: 2".into(),
                    stdout: String::new(),
                    stderr: "decoder failed\n".into(),
                })
            },
        )
        .expect("rv64 trap serial should report decoder failure");

        assert!(annotation.contains("fault-decode failed with exit status: 2"));
        assert!(annotation.contains("decoder failed"));
    }

    #[test]
    fn qemu_smoke_command_captures_serial_without_block_image() {
        let options = QemuOptions {
            expect_sentinel: true,
            timeout: Duration::from_secs(10),
            no_block: false,
            interactive: false,
        };
        let command = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::Rv64Qemu,
            Profile::Smoke,
            &options,
        )
        .unwrap();
        let rendered = command.join(" ");
        assert!(rendered.contains("qemu-system-riscv64"));
        assert!(rendered.contains("-machine virt"));
        assert!(rendered.contains("-smp 4"));
        assert!(rendered.contains("-serial file:target/qemu-rv64-qemu-smoke.serial.log"));
        assert!(rendered.contains(
            "-kernel /tmp/tx/target/riscv64gc-unknown-none-elf/debug/tx-kernel-riscv64-qemu-virt"
        ));
        assert!(!rendered.contains("-drive file=target/images/smoke.ext4"));
    }

    #[test]
    fn la64_qemu_command_uses_la464_cpu_and_larger_memory() {
        let options = QemuOptions {
            expect_sentinel: true,
            timeout: Duration::from_secs(10),
            no_block: false,
            interactive: false,
        };
        let command = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::La64Qemu,
            Profile::Smoke,
            &options,
        )
        .unwrap();
        let rendered = command.join(" ");

        assert!(rendered.contains("qemu-system-loongarch64"));
        assert!(rendered.contains("-machine virt"));
        assert!(rendered.contains("-cpu la464"));
        assert!(rendered.contains("-m 1152M"));
        assert!(rendered.contains("-smp 4"));
        assert!(rendered.contains("-serial file:target/qemu-la64-qemu-smoke.serial.log"));
        assert!(rendered.contains("tx-kernel-loongarch64-qemu-virt"));
    }

    #[test]
    fn la64_busybox_block_device_uses_pci_transport() {
        let options = QemuOptions {
            expect_sentinel: false,
            timeout: Duration::from_secs(10),
            no_block: false,
            interactive: false,
        };
        let command = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::La64Qemu,
            Profile::Busybox,
            &options,
        )
        .unwrap();
        let rendered = command.join(" ");

        assert!(rendered.contains("-device virtio-blk-pci-non-transitional,drive=txblk0,rombar=0"));
        assert!(!rendered.contains("virtio-blk-device,drive=txblk0"));
        assert!(rendered.contains(
            "-drive driver=raw,file.driver=file,file.filename=target/images/busybox-root-la64-qemu.ext4,file.locking=off,if=none,id=txblk0,read-only=on"
        ));
    }
}
