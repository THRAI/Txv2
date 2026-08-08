use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::image::{
    alpine_initramfs_name, busybox_initramfs_name, busybox_root_ext4_name, test_initramfs_name,
};
use crate::target::{Profile, TxTarget};
use crate::util::{
    append_tty_winsize_cmdline, default_boot_mode_for_profile, option_value, optional_option_value,
    resolve_path, shell_join, tail_lines, validate_boot_mode_value,
};
use crate::Result;

const DEFAULT_SENTINEL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
struct QemuOptions {
    expect_sentinel: bool,
    expect_markers: Vec<String>,
    timeout: Duration,
    smp: Option<usize>,
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
    boot_mode: Option<String>,
    append_cmdline: Option<String>,
    extra_rv64_ext4: Vec<PathBuf>,
    net: QemuNet,
    host_ping: Option<HostPingOptions>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum QemuNet {
    None,
    User,
    Tap(String),
    Bridge(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HostPingOptions {
    guest_ip: String,
    count: u32,
    timeout: Duration,
}

impl HostPingOptions {
    fn command_args(&self) -> Vec<String> {
        let timeout_secs = self.timeout.as_millis().div_ceil(1000).max(1);
        vec![
            "ping".into(),
            "-c".into(),
            self.count.to_string(),
            "-W".into(),
            timeout_secs.to_string(),
            self.guest_ip.clone(),
        ]
    }
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
    let options = qemu_options(root, &args)?;
    let command = qemu_command(root, target, profile, &options)?;

    println!("{}", shell_join(&command));
    if options.expect_sentinel {
        println!("serial: {}", serial_log_relative(target, profile).display());
        println!("expect: {}", expected_sentinel(target));
        for marker in &options.expect_markers {
            println!("expect-marker: {marker}");
        }
        println!("timeout-ms: {}", options.timeout.as_millis());
    }
    if let Some(host_ping) = &options.host_ping {
        println!("host-ping: {}", shell_join(&host_ping.command_args()));
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

fn qemu_options(root: &Path, args: &[String]) -> Result<QemuOptions> {
    let timeout = optional_option_value(args, "--timeout-ms")
        .map(|value| {
            value
                .parse::<u64>()
                .map(Duration::from_millis)
                .map_err(|err| format!("invalid --timeout-ms value '{value}': {err}"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_SENTINEL_TIMEOUT);
    let smp = optional_option_value(args, "--smp")
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|err| format!("invalid --smp value '{value}': {err}"))
                .and_then(|value| {
                    if value == 0 {
                        Err("--smp must be greater than zero".into())
                    } else {
                        Ok(value)
                    }
                })
        })
        .transpose()?;

    let expect_sentinel = args.iter().any(|arg| arg == "--expect-sentinel");
    let expect_markers = option_values(args, "--expect-marker")?;
    let boot_mode = optional_option_value(args, "--boot-mode")
        .map(|value| validate_boot_mode_value(&value).map(|()| value))
        .transpose()?;
    let append_cmdline = optional_option_value(args, "--append-cmdline");
    let extra_rv64_ext4 = option_values(args, "--extra-rv64-ext4")?
        .into_iter()
        .map(|path| resolve_path(root, PathBuf::from(path)))
        .collect();
    let net = qemu_net(args)?;
    let host_ping = host_ping_options(args, expect_sentinel, &net)?;

    Ok(QemuOptions {
        expect_sentinel,
        expect_markers,
        timeout,
        smp,
        no_block: args.iter().any(|arg| arg == "--no-block"),
        interactive: args.iter().any(|arg| arg == "--interactive"),
        boot_mode,
        append_cmdline,
        extra_rv64_ext4,
        net,
        host_ping,
    })
}

fn option_values(args: &[String], name: &str) -> Result<Vec<String>> {
    let mut values = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        if args[idx] == name {
            let Some(value) = args.get(idx + 1) else {
                return Err(format!("option {name} needs a value"));
            };
            values.push(value.clone());
            idx += 2;
        } else {
            idx += 1;
        }
    }
    Ok(values)
}

pub(crate) fn qemu_net(args: &[String]) -> Result<QemuNet> {
    let Some(value) = optional_option_value(args, "--net") else {
        return Ok(QemuNet::None);
    };

    match value.as_str() {
        "none" => Ok(QemuNet::None),
        "user" => Ok(QemuNet::User),
        value if value.starts_with("tap:") => {
            let ifname = value.trim_start_matches("tap:");
            if ifname.is_empty() {
                Err("--net tap:<ifname> requires a non-empty interface name".into())
            } else {
                Ok(QemuNet::Tap(ifname.into()))
            }
        }
        value if value.starts_with("bridge:") => {
            let bridge = value.trim_start_matches("bridge:");
            if bridge.is_empty() {
                Err("--net bridge:<bridge> requires a non-empty bridge name".into())
            } else {
                Ok(QemuNet::Bridge(bridge.into()))
            }
        }
        other => Err(format!(
            "invalid --net value '{other}', expected none, user, tap:<ifname>, or bridge:<bridge>"
        )),
    }
}

fn host_ping_options(
    args: &[String],
    expect_sentinel: bool,
    net: &QemuNet,
) -> Result<Option<HostPingOptions>> {
    let guest_ip = optional_option_value(args, "--host-ping-guest");
    let has_count = args.iter().any(|arg| arg == "--host-ping-count");
    let has_timeout = args.iter().any(|arg| arg == "--host-ping-timeout-ms");

    let Some(guest_ip) = guest_ip else {
        if has_count || has_timeout {
            return Err(
                "--host-ping-count/--host-ping-timeout-ms require --host-ping-guest".into(),
            );
        }
        return Ok(None);
    };
    if guest_ip.is_empty() {
        return Err("--host-ping-guest requires a non-empty guest IP".into());
    }
    if !expect_sentinel {
        return Err("--host-ping-guest requires --expect-sentinel".into());
    }
    if !matches!(net, QemuNet::Tap(_) | QemuNet::Bridge(_)) {
        return Err(
            "--host-ping-guest requires --net tap:<ifname> or --net bridge:<bridge>".into(),
        );
    }

    let count = optional_option_value(args, "--host-ping-count")
        .map(|value| {
            let count = value
                .parse::<u32>()
                .map_err(|err| format!("invalid --host-ping-count value '{value}': {err}"))?;
            if count == 0 {
                Err("--host-ping-count must be greater than 0".to_string())
            } else {
                Ok(count)
            }
        })
        .transpose()?
        .unwrap_or(3);
    let timeout = optional_option_value(args, "--host-ping-timeout-ms")
        .map(|value| {
            let millis = value
                .parse::<u64>()
                .map_err(|err| format!("invalid --host-ping-timeout-ms value '{value}': {err}"))?;
            if millis == 0 {
                Err("--host-ping-timeout-ms must be greater than 0".to_string())
            } else {
                Ok(Duration::from_millis(millis))
            }
        })
        .transpose()?
        .unwrap_or(Duration::from_millis(6000));

    Ok(Some(HostPingOptions {
        guest_ip,
        count,
        timeout,
    }))
}

fn qemu_command(
    root: &Path,
    target: TxTarget,
    profile: Profile,
    options: &QemuOptions,
) -> Result<Vec<String>> {
    if !options.extra_rv64_ext4.is_empty() && target != TxTarget::Rv64Qemu {
        return Err("--extra-rv64-ext4 is only supported for rv64-qemu".into());
    }
    if !options.extra_rv64_ext4.is_empty() && profile == Profile::Busybox && !options.no_block {
        return Err("--extra-rv64-ext4 conflicts with the busybox default block image; pass --no-block or use --profile alpine".into());
    }
    if options.extra_rv64_ext4.len() > 3 {
        return Err("--extra-rv64-ext4 supports at most three RV64 virtio-mmio drives".into());
    }

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
        qemu_memory(target, profile).to_string(),
        "-smp".to_string(),
        qemu_smp(target, profile, options).to_string(),
        // Force multi-threaded TCG: vCPUs run on parallel host threads
        // instead of round-robin time-slicing on one host thread. Without
        // this, the boot smoke's BSP busy-spin for AP reactor task
        // completion (init.rs `wait_for_ap_reactor_task_done`) starves
        // the AP — the AP never gets CPU time to mark the task done,
        // which manifests as a smoke panic on the slow GitHub Actions
        // emulated runner (passes on Apple-silicon TCG because its
        // round-robin is much faster).
        "-accel".to_string(),
        "tcg,thread=multi".to_string(),
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
            args.push(opensbi_bios(root));
        }
        TxTarget::La64Qemu => {}
    }

    if options.expect_sentinel {
        args.push("-no-shutdown".into());
    }

    if matches!(profile, Profile::Busybox | Profile::Alpine) {
        let initramfs_name = match profile {
            Profile::Busybox => busybox_initramfs_name(target),
            Profile::Alpine => alpine_initramfs_name(target),
            Profile::Smoke => unreachable!("handled by outer profile match"),
        };
        let initramfs = root.join("target").join("images").join(initramfs_name);
        let boot_mode = options
            .boot_mode
            .as_deref()
            .unwrap_or_else(|| default_boot_mode_for_profile(profile));
        let cmdline_base = match (profile, target) {
            (Profile::Busybox, TxTarget::Rv64M1DockMock) => {
                format!(
                    "tx.profile=busybox tx.boot.mode={boot_mode} tx.board=m1dock-mock tx.mock.spi0.cs0=target/images/m1dock-sd.img console=ttyS0"
                )
            }
            (Profile::Busybox, _) => {
                format!("tx.profile=busybox tx.boot.mode={boot_mode} console=ttyS0")
            }
            (Profile::Alpine, _) => {
                format!(
                    "tx.profile=alpine tx.boot.mode={boot_mode} init=/bin/tx-bootstrap-busybox console=ttyS0"
                )
            }
            (Profile::Smoke, _) => unreachable!("handled by outer profile match"),
        };
        let cmdline_base = append_extra_cmdline(&cmdline_base, options.append_cmdline.as_deref());
        let cmdline = append_tty_winsize_cmdline(&cmdline_base);
        args.push("-initrd".into());
        args.push(initramfs.display().to_string());
        args.push("-append".into());
        args.push(cmdline.clone());
        if target == TxTarget::La64Qemu {
            args.push("-fw_cfg".into());
            args.push(format!("name=opt/tx.cmdline,string={cmdline}"));
            args.push("-fw_cfg".into());
            args.push(format!("name=opt/tx.initrd,file={}", initramfs.display()));
        }
    } else {
        let boot_mode = options
            .boot_mode
            .as_deref()
            .unwrap_or_else(|| default_boot_mode_for_profile(profile));
        let cmdline_base = if target == TxTarget::Rv64M1DockMock {
            format!(
                "tx.profile=smoke tx.boot.mode={boot_mode} tx.board=m1dock-mock init=/tx-test-init tx.test_init=1 console=ttyS0"
            )
        } else {
            format!(
                "tx.profile=smoke tx.boot.mode={boot_mode} init=/tx-test-init tx.test_init=1 console=ttyS0"
            )
        };
        let cmdline_base = append_extra_cmdline(&cmdline_base, options.append_cmdline.as_deref());
        let cmdline = append_tty_winsize_cmdline(&cmdline_base);
        let initramfs = root
            .join("target")
            .join("images")
            .join(test_initramfs_name(target));
        args.push("-initrd".into());
        args.push(initramfs.display().to_string());
        args.push("-append".into());
        args.push(cmdline.clone());
        if target == TxTarget::La64Qemu {
            args.push("-fw_cfg".into());
            args.push(format!("name=opt/tx.cmdline,string={cmdline}"));
            args.push("-fw_cfg".into());
            args.push(format!("name=opt/tx.initrd,file={}", initramfs.display()));
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
    for (idx, path) in options.extra_rv64_ext4.iter().enumerate() {
        args.push("-drive".into());
        args.push(format!(
            "file={},format=raw,if=none,id=txblk{idx}",
            path.display(),
        ));
        args.push("-device".into());
        args.push(format!(
            "virtio-blk-device,drive=txblk{idx},bus=virtio-mmio-bus.{idx}"
        ));
    }
    append_net_args(&mut args, target, &options.net);
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

fn append_extra_cmdline(base: &str, extra: Option<&str>) -> String {
    match extra {
        Some(extra) if !extra.trim().is_empty() => format!("{base} {}", extra.trim()),
        _ => base.to_string(),
    }
}

fn qemu_smp(target: TxTarget, profile: Profile, options: &QemuOptions) -> usize {
    if let Some(smp) = options.smp {
        return smp;
    }

    match (target, profile) {
        (TxTarget::Rv64Qemu, Profile::Alpine) => 1,
        (TxTarget::Rv64Qemu | TxTarget::La64Qemu, _) => 4,
        (TxTarget::Rv64M1DockMock, _) => 1,
    }
}

pub(crate) fn append_net_args(args: &mut Vec<String>, target: TxTarget, net: &QemuNet) {
    match net {
        QemuNet::None => {}
        QemuNet::User => {
            args.push("-netdev".into());
            args.push("user,id=net0".into());
            push_net_device(args, target);
        }
        QemuNet::Tap(ifname) => {
            args.push("-netdev".into());
            args.push(format!(
                "tap,id=net0,ifname={ifname},script=no,downscript=no"
            ));
            push_net_device(args, target);
        }
        QemuNet::Bridge(bridge) => {
            args.push("-netdev".into());
            args.push(format!("bridge,id=net0,br={bridge}"));
            push_net_device(args, target);
        }
    }
}

fn push_net_device(args: &mut Vec<String>, target: TxTarget) {
    args.push("-device".into());
    match target {
        TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock => {
            args.push("virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0".into());
        }
        TxTarget::La64Qemu => {
            args.push("virtio-net-pci,netdev=net0".into());
        }
    }
}

fn qemu_cpu(target: TxTarget) -> Option<&'static str> {
    match target {
        TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock => None,
        TxTarget::La64Qemu => Some("la464"),
    }
}

fn qemu_memory(target: TxTarget, profile: Profile) -> &'static str {
    match (target, profile) {
        (TxTarget::Rv64Qemu, Profile::Alpine) => "1024M",
        (TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock, _) => "256M",
        (TxTarget::La64Qemu, _) => "1152M",
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
        match sentinel_state(
            &serial,
            &sentinel,
            &options.expect_markers,
            started.elapsed(),
            options.timeout,
        ) {
            SentinelState::Found => {
                if let Some(host_ping) = &options.host_ping {
                    run_host_ping(root, host_ping).inspect_err(|_| {
                        let _ = child.kill();
                        let _ = child.wait();
                    })?;
                }
                let _ = child.kill();
                let _ = child.wait();
                println!(
                    "qemu smoke sentinel observed: {}",
                    expected_marker_summary(&sentinel, &options.expect_markers)
                );
                return Ok(());
            }
            SentinelState::TimedOut => {
                let _ = child.kill();
                let _ = child.wait();
                let annotation = fault_decode_annotation(root, target, &serial_log, &serial);
                return Err(format!(
                    "qemu timed out after {} ms waiting for {}\nserial tail:\n{}{}",
                    options.timeout.as_millis(),
                    expected_marker_summary(&sentinel, &options.expect_markers),
                    tail_lines(&serial, 80),
                    annotation.unwrap_or_default()
                ));
            }
            SentinelState::Trapped => {
                let _ = child.kill();
                let _ = child.wait();
                let annotation = fault_decode_annotation(root, target, &serial_log, &serial);
                return Err(format!(
                    "qemu observed trap before sentinel {}\nserial tail:\n{}{}",
                    expected_marker_summary(&sentinel, &options.expect_markers),
                    tail_lines(&serial, 80),
                    annotation.unwrap_or_default()
                ));
            }
            SentinelState::Pending => {}
        }

        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            let serial = fs::read_to_string(&serial_log).unwrap_or_default();
            if serial_contains_expected_markers(&serial, &sentinel, &options.expect_markers) {
                if options.host_ping.is_some() {
                    return Err(format!(
                        "qemu exited before host ping could run after observing {sentinel}"
                    ));
                }
                println!(
                    "qemu smoke sentinel observed: {}",
                    expected_marker_summary(&sentinel, &options.expect_markers)
                );
                return Ok(());
            }
            let annotation = fault_decode_annotation(root, target, &serial_log, &serial);
            return Err(format!(
                "qemu exited with {status} before sentinel {}\nserial tail:\n{}{}",
                expected_marker_summary(&sentinel, &options.expect_markers),
                tail_lines(&serial, 80),
                annotation.unwrap_or_default()
            ));
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn opensbi_bios(root: &Path) -> String {
    let silent = root.join("external/opensbi-silent/fw_dynamic.bin");
    if silent.exists() {
        silent.display().to_string()
    } else {
        "default".to_string()
    }
}

fn run_host_ping(root: &Path, options: &HostPingOptions) -> Result<()> {
    let command = options.command_args();
    println!("$ {}", shell_join(&command));
    let Some((program, rest)) = command.split_first() else {
        return Err("empty host ping command".into());
    };
    let mut child = Command::new(program)
        .args(rest)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run host ping: {err}"))?;
    let started = Instant::now();

    loop {
        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            let output = child.wait_with_output().map_err(|err| err.to_string())?;
            if status.success() {
                println!("host ping succeeded: {}", options.guest_ip);
                return Ok(());
            }
            return Err(format!(
                "host ping exited with {status}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout).trim_end(),
                String::from_utf8_lossy(&output.stderr).trim_end()
            ));
        }

        if started.elapsed() >= options.timeout {
            let _ = child.kill();
            let output = child.wait_with_output().map_err(|err| err.to_string())?;
            return Err(format!(
                "host ping timed out after {} ms\nstdout:\n{}\nstderr:\n{}",
                options.timeout.as_millis(),
                String::from_utf8_lossy(&output.stdout).trim_end(),
                String::from_utf8_lossy(&output.stderr).trim_end()
            ));
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn expected_sentinel(target: TxTarget) -> String {
    format!("txkernel:{}:boot:ok", target.board_name())
}

pub(crate) fn expected_owner_wake_smp_marker(target: TxTarget) -> String {
    format!("txkernel:{}:reactor:owner-wake:smp:ok", target.board_name())
}

pub(crate) fn expected_rcu_smp_markers(target: TxTarget) -> [String; 6] {
    let board = target.board_name();
    [
        format!("txkernel:{board}:rcu:smp:cpus:ok"),
        format!("txkernel:{board}:rcu:smp:guarded-overlap:ok"),
        format!("txkernel:{board}:rcu:smp:no-early-reclaim:ok"),
        format!("txkernel:{board}:rcu:smp:maintenance-ack:ok"),
        format!("txkernel:{board}:rcu:smp:bounded-drain:ok"),
        format!("txkernel:{board}:rcu:smp:ok"),
    ]
}

fn serial_contains_sentinel(serial: &str, sentinel: &str) -> bool {
    serial.contains(sentinel)
}

fn serial_contains_expected_markers(serial: &str, sentinel: &str, markers: &[String]) -> bool {
    serial_contains_sentinel(serial, sentinel)
        && markers
            .iter()
            .all(|marker| serial_contains_sentinel(serial, marker))
}

fn expected_marker_summary(sentinel: &str, markers: &[String]) -> String {
    if markers.is_empty() {
        sentinel.to_string()
    } else {
        format!("{sentinel} plus {}", markers.join(", "))
    }
}

fn sentinel_state(
    serial: &str,
    sentinel: &str,
    markers: &[String],
    elapsed: Duration,
    timeout: Duration,
) -> SentinelState {
    if serial_contains_expected_markers(serial, sentinel, markers) {
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

pub(crate) fn serial_log_relative(target: TxTarget, profile: Profile) -> PathBuf {
    PathBuf::from(format!(
        "target/qemu-{}-{}.serial.log",
        target.name(),
        profile.name()
    ))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;

    fn test_options() -> QemuOptions {
        QemuOptions {
            expect_sentinel: false,
            expect_markers: Vec::new(),
            timeout: Duration::from_secs(10),
            smp: None,
            no_block: false,
            interactive: false,
            boot_mode: None,
            append_cmdline: None,
            extra_rv64_ext4: Vec::new(),
            net: QemuNet::None,
            host_ping: None,
        }
    }

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
                &[],
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
                &[],
                Duration::from_millis(10),
                Duration::from_secs(10)
            ),
            SentinelState::Trapped
        );
    }

    #[test]
    fn extra_expected_marker_is_required_with_boot_sentinel() {
        let sentinel = "txkernel:qemu-riscv64-virt:boot:ok";
        let marker = expected_owner_wake_smp_marker(TxTarget::Rv64Qemu);
        let markers = vec![marker.clone()];

        assert_eq!(
            sentinel_state(
                "OpenSBI\ntxkernel:qemu-riscv64-virt:boot:ok\n",
                sentinel,
                &markers,
                Duration::from_millis(10),
                Duration::from_secs(10)
            ),
            SentinelState::Pending
        );
        assert_eq!(
            sentinel_state(
                "OpenSBI\ntxkernel:qemu-riscv64-virt:reactor:owner-wake:smp:ok\ntxkernel:qemu-riscv64-virt:boot:ok\n",
                sentinel,
                &markers,
                Duration::from_millis(10),
                Duration::from_secs(10)
            ),
            SentinelState::Found
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
            ..test_options()
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
        assert!(
            rendered.contains("-initrd /tmp/tx/target/images/test-init-initramfs-rv64-qemu.cpio")
        );
        assert!(rendered.contains(
            "tx.profile=smoke tx.boot.mode=smoke init=/tx-test-init tx.test_init=1 console=ttyS0"
        ));
        assert!(!rendered.contains("-drive file=target/images/smoke.ext4"));
    }

    #[test]
    fn la64_qemu_command_uses_la464_cpu_and_larger_memory() {
        let options = QemuOptions {
            expect_sentinel: true,
            ..test_options()
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
        assert!(rendered.contains(
            "-fw_cfg name=opt/tx.cmdline,string=tx.profile=smoke tx.boot.mode=smoke init=/tx-test-init tx.test_init=1 console=ttyS0"
        ));
        assert!(rendered.contains(
            "-fw_cfg name=opt/tx.initrd,file=/tmp/tx/target/images/test-init-initramfs-la64-qemu.cpio"
        ));
        assert!(rendered.contains("tx.tty.rows="));
        assert!(rendered.contains("tx.tty.cols="));
        assert!(rendered.contains("tx-kernel-loongarch64-qemu-virt"));
    }

    #[test]
    fn la64_qemu_command_accepts_smp_override() {
        let options = QemuOptions {
            expect_sentinel: true,
            smp: Some(1),
            ..test_options()
        };
        let command = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::La64Qemu,
            Profile::Smoke,
            &options,
        )
        .unwrap();

        assert!(command.join(" ").contains("-smp 1"));
    }

    #[test]
    fn la64_busybox_block_device_uses_pci_transport() {
        let options = test_options();
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
            "-fw_cfg name=opt/tx.cmdline,string=tx.profile=busybox tx.boot.mode=busybox console=ttyS0"
        ));
        assert!(rendered.contains("tx.tty.rows="));
        assert!(rendered.contains("tx.tty.cols="));
        assert!(rendered.contains(
            "-fw_cfg name=opt/tx.initrd,file=/tmp/tx/target/images/busybox-initramfs-la64-qemu.cpio"
        ));
        assert!(rendered.contains(
            "-drive driver=raw,file.driver=file,file.filename=target/images/busybox-root-la64-qemu.ext4,file.locking=off,if=none,id=txblk0,read-only=on"
        ));
    }

    #[test]
    fn qemu_net_user_adds_target_specific_virtio_net_device() {
        let rv64 = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::Rv64Qemu,
            Profile::Smoke,
            &QemuOptions {
                expect_sentinel: true,
                no_block: true,
                net: QemuNet::User,
                ..test_options()
            },
        )
        .unwrap()
        .join(" ");
        assert!(rv64.contains("-netdev user,id=net0"));
        assert!(rv64.contains("-device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0"));

        let la64 = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::La64Qemu,
            Profile::Smoke,
            &QemuOptions {
                expect_sentinel: true,
                no_block: true,
                net: QemuNet::User,
                ..test_options()
            },
        )
        .unwrap()
        .join(" ");
        assert!(la64.contains("-netdev user,id=net0"));
        assert!(la64.contains("-device virtio-net-pci,netdev=net0"));
    }

    #[test]
    fn qemu_net_parses_tap_and_bridge_backends() {
        assert_eq!(
            qemu_net(&["--net".into(), "tap:tap0".into()]).unwrap(),
            QemuNet::Tap("tap0".into())
        );
        assert_eq!(
            qemu_net(&["--net".into(), "bridge:br0".into()]).unwrap(),
            QemuNet::Bridge("br0".into())
        );
        assert!(qemu_net(&["--net".into(), "tap:".into()]).is_err());
    }

    #[test]
    fn qemu_host_ping_requires_sentinel_and_tap_or_bridge() {
        let without_sentinel = qemu_options(
            Path::new("/tmp/tx"),
            &[
                "--target".into(),
                "rv64-qemu".into(),
                "--profile".into(),
                "smoke".into(),
                "--net".into(),
                "tap:txv2tap0".into(),
                "--host-ping-guest".into(),
                "10.0.2.15".into(),
            ],
        )
        .unwrap_err();
        assert!(without_sentinel.contains("--expect-sentinel"));

        let user_net = qemu_options(
            Path::new("/tmp/tx"),
            &[
                "--target".into(),
                "rv64-qemu".into(),
                "--profile".into(),
                "smoke".into(),
                "--expect-sentinel".into(),
                "--net".into(),
                "user".into(),
                "--host-ping-guest".into(),
                "10.0.2.15".into(),
            ],
        )
        .unwrap_err();
        assert!(user_net.contains("--net tap:<ifname> or --net bridge:<bridge>"));
    }

    #[test]
    fn qemu_host_ping_parses_options_and_command() {
        let options = qemu_options(
            Path::new("/tmp/tx"),
            &[
                "--target".into(),
                "rv64-qemu".into(),
                "--profile".into(),
                "smoke".into(),
                "--expect-sentinel".into(),
                "--net".into(),
                "bridge:br0".into(),
                "--host-ping-guest".into(),
                "10.0.2.15".into(),
                "--host-ping-count".into(),
                "2".into(),
                "--host-ping-timeout-ms".into(),
                "2500".into(),
            ],
        )
        .unwrap();
        let host_ping = options.host_ping.expect("host ping should parse");

        assert_eq!(host_ping.guest_ip, "10.0.2.15");
        assert_eq!(host_ping.count, 2);
        assert_eq!(host_ping.timeout, Duration::from_millis(2500));
        assert_eq!(
            host_ping.command_args(),
            vec!["ping", "-c", "2", "-W", "3", "10.0.2.15"]
        );
    }

    #[test]
    fn qemu_smp_option_overrides_target_default() {
        let options = qemu_options(
            Path::new("/tmp/tx"),
            &[
                "--expect-sentinel".into(),
                "--target".into(),
                "rv64-qemu".into(),
                "--profile".into(),
                "smoke".into(),
                "--smp".into(),
                "1".into(),
            ],
        )
        .unwrap();

        let command = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::Rv64Qemu,
            Profile::Smoke,
            &options,
        )
        .unwrap()
        .join(" ");

        assert!(command.contains("-smp 1"));
    }

    #[test]
    fn qemu_can_append_kernel_cmdline_tokens() {
        let options = QemuOptions {
            append_cmdline: Some("tx.mount.sdcard=0".into()),
            ..test_options()
        };

        let command = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            &options,
        )
        .unwrap()
        .join(" ");

        assert!(command.contains("tx.profile=alpine"));
        assert!(command.contains("tx.boot.mode=alpine"));
        assert!(command.contains("tx.mount.sdcard=0"));
        assert!(command.contains("tx.tty.rows="));
        assert!(command.contains("tx.tty.cols="));
    }

    #[test]
    fn qemu_boot_mode_option_overrides_profile_default() {
        let options = QemuOptions {
            boot_mode: Some("contest".into()),
            ..test_options()
        };

        let command = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            &options,
        )
        .unwrap()
        .join(" ");

        assert!(command.contains("tx.profile=alpine"));
        assert!(command.contains("tx.boot.mode=contest"));
        assert!(!command.contains("tx.boot.mode=alpine"));
    }

    #[test]
    fn qemu_boot_mode_option_rejects_unknown_values() {
        let err = qemu_options(
            Path::new("/tmp/tx"),
            &[
                "--target".into(),
                "rv64-qemu".into(),
                "--profile".into(),
                "alpine".into(),
                "--boot-mode".into(),
                "mystery".into(),
            ],
        )
        .unwrap_err();

        assert!(err.contains("invalid --boot-mode"));
    }

    #[test]
    fn qemu_can_attach_rv64_ext4_drive_on_bus0() {
        let image = PathBuf::from("/tmp/tx/target/images/alpine-tcc-dev-root-rv64-qemu.ext4");
        let options = QemuOptions {
            extra_rv64_ext4: vec![image],
            ..test_options()
        };

        let command = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            &options,
        )
        .unwrap()
        .join(" ");

        assert!(command.contains(
            "-drive file=/tmp/tx/target/images/alpine-tcc-dev-root-rv64-qemu.ext4,format=raw,if=none,id=txblk0"
        ));
        assert!(command.contains("-device virtio-blk-device,drive=txblk0,bus=virtio-mmio-bus.0"));
    }

    #[test]
    fn qemu_can_attach_three_rv64_ext4_drives_on_stable_buses() {
        let options = QemuOptions {
            extra_rv64_ext4: vec![
                PathBuf::from("/tmp/tx/test.img"),
                PathBuf::from("/tmp/tx/scratch.img"),
                PathBuf::from("/tmp/tx/workload.img"),
            ],
            ..test_options()
        };

        let command = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            &options,
        )
        .unwrap()
        .join(" ");

        for (idx, name) in ["test", "scratch", "workload"].iter().enumerate() {
            assert!(command.contains(&format!(
                "-drive file=/tmp/tx/{name}.img,format=raw,if=none,id=txblk{idx}"
            )));
            assert!(command.contains(&format!(
                "-device virtio-blk-device,drive=txblk{idx},bus=virtio-mmio-bus.{idx}"
            )));
        }
    }

    #[test]
    fn alpine_qemu_defaults_to_single_vcpu_for_interactive_stability() {
        let options = QemuOptions {
            interactive: true,
            ..test_options()
        };
        let command = qemu_command(
            Path::new("/tmp/tx"),
            TxTarget::Rv64Qemu,
            Profile::Alpine,
            &options,
        )
        .unwrap()
        .join(" ");

        assert!(command.contains("-smp 1"));
    }
}
