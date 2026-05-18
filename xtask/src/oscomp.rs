use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::target::TxTarget;
use crate::util::{
    command_exists, copy_dir_contents, option_value, optional_option_value, resolve_path,
    run_cmd_owned_in, shell_join,
};
use crate::Result;

pub(crate) const OSCOMP_AUTOTEST: &str = "external/oscomp-autotest";
const OSCOMP_DOCKER_IMAGE: &str = "zhouzhouyi/os-contest:20260104";
const OSCOMP_SDCARD_RV_URL: &str =
    "https://github.com/oscomp/testsuits-for-oskernel/releases/download/pre-20250615/sdcard-rv.img.xz";
const OSCOMP_SDCARD_LA_URL: &str =
    "https://github.com/oscomp/testsuits-for-oskernel/releases/download/pre-20250615/sdcard-la.img.xz";

pub(crate) fn oscomp(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(kind) = args.first() else {
        return Err("oscomp command needs doctor, prepare, submit, run, or qemu".into());
    };
    match kind.as_str() {
        "doctor" => oscomp_doctor(root),
        "prepare" => oscomp_prepare(root, &args[1..]),
        "submit" => oscomp_submit(root, &args[1..]),
        "run" => oscomp_run(root, &args[1..]),
        "qemu" => oscomp_qemu(root, &args[1..]),
        other => Err(format!(
            "unknown oscomp command '{other}', expected doctor, prepare, submit, run, or qemu"
        )),
    }
}

fn oscomp_doctor(root: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let submodule = root.join(OSCOMP_AUTOTEST);
    if submodule.join("kernel").join("run.py").exists() {
        println!("ok: {OSCOMP_AUTOTEST}");
    } else {
        println!("missing: {OSCOMP_AUTOTEST}");
        missing.push("git submodule update --init --recursive".to_string());
    }
    for tool in ["python3", "zip", "docker", "gzip"] {
        if command_exists(tool) {
            println!("ok: {tool}");
        } else {
            println!("warn: {tool} not found");
        }
    }
    let data = oscomp_data_dir(root, &[]);
    for stem in ["sdcard-rv.img", "sdcard-la.img"] {
        let img = data.join(stem);
        let xz = data.join(format!("{stem}.xz"));
        let gz = data.join(format!("{stem}.gz"));
        if img.exists() {
            println!("ok: {}", img.display());
        } else if xz.exists() {
            println!("ok: {} (compressed; will decompress on prepare)", xz.display());
        } else if gz.exists() {
            println!("ok: {} (compressed; will decompress on prepare)", gz.display());
        } else {
            println!("warn: missing {} (also looked for .xz / .gz)", img.display());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing.join("; "))
    }
}

fn oscomp_prepare(root: &Path, args: &[String]) -> Result<()> {
    let data = oscomp_data_dir(root, args);
    let cg = root.join("target").join("oscomp").join("cg");
    fs::create_dir_all(&data).map_err(|err| err.to_string())?;
    fs::create_dir_all(&cg).map_err(|err| err.to_string())?;

    let judge = root.join(OSCOMP_AUTOTEST).join("kernel").join("judge");
    if !judge.exists() {
        return Err(format!(
            "missing OSComp judge directory {}; run `git submodule update --init --recursive`",
            judge.display()
        ));
    }
    copy_dir_contents(&judge, &data)?;

    if !command_exists("zip") {
        return Err("zip is required to create kernel.zip for OSComp autotest".into());
    }
    let kernel_dir = root.join(OSCOMP_AUTOTEST).join("kernel");
    let kernel_zip = cg.join("kernel.zip");
    if kernel_zip.exists() {
        fs::remove_file(&kernel_zip).map_err(|err| err.to_string())?;
    }
    run_cmd_owned_in(
        &kernel_dir,
        "zip",
        &["-qr".into(), kernel_zip.display().to_string(), ".".into()],
    )?;

    println!("prepared OSComp judge data at {}", data.display());
    println!("prepared OSComp kernel zip at {}", kernel_zip.display());

    // Sdcard images. The qemu launch reads `.img` (uncompressed); the GitHub
    // release ships `.xz`. We tolerate any of `.img`, `.img.xz`, `.img.gz`
    // present locally and decompress to `.img` if needed so a downstream
    // `cargo xtask oscomp qemu` run finds what it expects.
    for (stem, url) in [
        ("sdcard-rv.img", OSCOMP_SDCARD_RV_URL),
        ("sdcard-la.img", OSCOMP_SDCARD_LA_URL),
    ] {
        let img = data.join(stem);
        if img.exists() {
            println!("ok: {}", img.display());
            continue;
        }
        match ensure_sdcard_image(&data, stem) {
            Ok(()) => println!("ok: {}", img.display()),
            Err(why) => {
                println!(
                    "missing {stem}; download {url} into {} (any of {stem}, {stem}.xz, {stem}.gz) \
                     and re-run prepare ({why})",
                    data.display()
                );
            }
        }
    }
    Ok(())
}

/// Locate a compressed sdcard image next to `data` and decompress it
/// in-place to `<stem>`.
///
/// Probes `<stem>.xz` then `<stem>.gz`. Returns `Err` when neither is
/// present (signals "missing — user must download"). Decompression uses
/// the host `xz` / `gunzip` binaries; both are common on the macOS dev
/// machines we run from.
fn ensure_sdcard_image(data: &Path, stem: &str) -> Result<()> {
    let xz_path = data.join(format!("{stem}.xz"));
    let gz_path = data.join(format!("{stem}.gz"));
    let out_path = data.join(stem);
    let (src, tool, args): (PathBuf, &str, Vec<String>) = if xz_path.exists() {
        (
            xz_path,
            "xz",
            vec!["--decompress".into(), "--keep".into(), "--force".into()],
        )
    } else if gz_path.exists() {
        (
            gz_path,
            "gunzip",
            vec!["--keep".into(), "--force".into()],
        )
    } else {
        return Err("no .xz or .gz archive present".into());
    };
    if !command_exists(tool) {
        return Err(format!("{tool} not found; install via Homebrew or apt"));
    }
    println!("decompressing {} → {}", src.display(), out_path.display());
    let mut full_args = args;
    full_args.push(src.display().to_string());
    run_cmd_owned_in(data, tool, &full_args)?;
    if !out_path.exists() {
        return Err(format!(
            "{tool} ran but {} was not produced",
            out_path.display()
        ));
    }
    Ok(())
}

fn oscomp_submit(root: &Path, args: &[String]) -> Result<()> {
    let submit = optional_option_value(args, "--submit")
        .map(PathBuf::from)
        .map(|path| resolve_path(root, path))
        .unwrap_or_else(|| root.join("target").join("oscomp").join("submit"));
    fs::create_dir_all(&submit).map_err(|err| err.to_string())?;
    copy_kernel_for_oscomp(root, TxTarget::Rv64Qemu, &submit.join("kernel-rv"))?;
    copy_kernel_for_oscomp(root, TxTarget::La64Qemu, &submit.join("kernel-la"))?;
    println!("prepared OSComp submit dir at {}", submit.display());
    Ok(())
}

fn oscomp_run(root: &Path, args: &[String]) -> Result<()> {
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let data = oscomp_data_dir(root, args);
    let submit = optional_option_value(args, "--submit")
        .map(PathBuf::from)
        .map(|path| resolve_path(root, path))
        .unwrap_or_else(|| root.join("target").join("oscomp").join("submit"));
    let cg = root.join("target").join("oscomp").join("cg");
    let docker_image =
        optional_option_value(args, "--docker-image").unwrap_or_else(|| OSCOMP_DOCKER_IMAGE.into());
    let command = vec![
        "docker".to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "-v".to_string(),
        format!("{}:/coursegrader/submit", submit.display()),
        "-v".to_string(),
        format!("{}:/coursegrader/testdata", data.display()),
        "-v".to_string(),
        format!("{}:/cg", cg.display()),
        "-v".to_string(),
        format!("{}:/mnt/cghook/", data.display()),
        docker_image,
        "python3".to_string(),
        "/cg/kernel.zip".to_string(),
    ];
    println!("{}", shell_join(&command));
    if dry_run {
        return Ok(());
    }
    let Some((program, rest)) = command.split_first() else {
        return Err("empty docker command".into());
    };
    let status = Command::new(program)
        .args(rest)
        .current_dir(root)
        .status()
        .map_err(|err| format!("failed to run {program}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("OSComp docker run exited with {status}"))
    }
}

fn oscomp_qemu(root: &Path, args: &[String]) -> Result<()> {
    let target = TxTarget::parse(&option_value(args, "--target")?)?;
    let data = oscomp_data_dir(root, args);
    let submit = optional_option_value(args, "--submit")
        .map(PathBuf::from)
        .map(|path| resolve_path(root, path))
        .unwrap_or_else(|| root.join("target").join("oscomp").join("submit"));
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let (kernel, sdcard, out, qemu_args) = match target {
        TxTarget::Rv64Qemu => (
            submit.join("kernel-rv"),
            data.join("sdcard-rv.img"),
            root.join("target")
                .join("oscomp")
                .join("os_serial_out_rv.txt"),
            vec![
                "qemu-system-riscv64".to_string(),
                "-machine".into(),
                "virt".into(),
                "-kernel".into(),
                submit.join("kernel-rv").display().to_string(),
                "-m".into(),
                "1G".into(),
                "-nographic".into(),
                "-smp".into(),
                "1".into(),
                "-bios".into(),
                "default".into(),
                "-drive".into(),
                format!(
                    "file={},if=none,format=raw,id=x0",
                    data.join("sdcard-rv.img").display()
                ),
                "-device".into(),
                "virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0".into(),
                "-no-reboot".into(),
                "-device".into(),
                "virtio-net-device,netdev=net".into(),
                "-netdev".into(),
                "user,id=net".into(),
                "-rtc".into(),
                "base=utc".into(),
            ],
        ),
        TxTarget::La64Qemu => (
            submit.join("kernel-la"),
            data.join("sdcard-la.img"),
            root.join("target")
                .join("oscomp")
                .join("os_serial_out_la.txt"),
            vec![
                "qemu-system-loongarch64".to_string(),
                "-kernel".into(),
                submit.join("kernel-la").display().to_string(),
                "-m".into(),
                "1G".into(),
                "-nographic".into(),
                "-smp".into(),
                "1".into(),
                "-drive".into(),
                format!(
                    "file={},if=none,format=raw,id=x0",
                    data.join("sdcard-la.img").display()
                ),
                "-device".into(),
                "virtio-blk-pci,drive=x0".into(),
                "-no-reboot".into(),
                "-device".into(),
                "virtio-net-pci,netdev=net0".into(),
                "-netdev".into(),
                "user,id=net0".into(),
                "-rtc".into(),
                "base=utc".into(),
            ],
        ),
        TxTarget::Rv64M1DockMock => {
            return Err(
                "OSComp qemu supports rv64-qemu and la64-qemu, not rv64-m1dock-mock".into(),
            );
        }
    };
    println!("{}", shell_join(&qemu_args));
    println!("serial output: {}", out.display());
    if dry_run {
        return Ok(());
    }
    if !kernel.exists() {
        return Err(format!("missing {}", kernel.display()));
    }
    if !sdcard.exists() {
        return Err(format!("missing {}", sdcard.display()));
    }
    let Some((program, rest)) = qemu_args.split_first() else {
        return Err("empty OSComp qemu command".into());
    };
    fs::create_dir_all(out.parent().expect("output has parent")).map_err(|err| err.to_string())?;
    let output = fs::File::create(&out).map_err(|err| err.to_string())?;
    let status = Command::new(program)
        .args(rest)
        .current_dir(root)
        .stdout(output.try_clone().map_err(|err| err.to_string())?)
        .stderr(output)
        .status()
        .map_err(|err| format!("failed to run {program}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("OSComp qemu exited with {status}"))
    }
}

fn oscomp_data_dir(root: &Path, args: &[String]) -> PathBuf {
    let path = optional_option_value(args, "--data")
        .map(PathBuf::from)
        .or_else(|| env::var("TX_OSCOMP_DATA").ok().map(PathBuf::from))
        .unwrap_or_else(|| root.join("target").join("oscomp").join("testdata"));
    resolve_path(root, path)
}

fn copy_kernel_for_oscomp(root: &Path, target: TxTarget, dest: &Path) -> Result<()> {
    let source = target.kernel_path(root);
    if !source.exists() {
        return Err(format!(
            "missing {}; run `cargo xtask build --target {}` first",
            source.display(),
            target.name()
        ));
    }
    fs::copy(&source, dest).map_err(|err| err.to_string())?;
    println!("copied {} -> {}", source.display(), dest.display());
    Ok(())
}
