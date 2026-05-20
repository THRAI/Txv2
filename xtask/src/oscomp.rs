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
        return Err(
            "oscomp command needs doctor, prepare, submit, run, qemu, score, list-suites, test, or slim-sdcard"
                .into(),
        );
    };
    match kind.as_str() {
        "doctor" => oscomp_doctor(root),
        "prepare" => oscomp_prepare(root, &args[1..]),
        "submit" => oscomp_submit(root, &args[1..]),
        "run" => oscomp_run(root, &args[1..]),
        "qemu" => oscomp_qemu(root, &args[1..]),
        "score" => oscomp_score(root, &args[1..]),
        "list-suites" => oscomp_list_suites(root, &args[1..]),
        "test" => oscomp_test(root, &args[1..]),
        "slim-sdcard" => oscomp_slim_sdcard(root, &args[1..]),
        other => Err(format!(
            "unknown oscomp command '{other}', expected doctor, prepare, submit, run, qemu, score, list-suites, test, or slim-sdcard"
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
            println!(
                "ok: {} (compressed; will decompress on prepare)",
                xz.display()
            );
        } else if gz.exists() {
            println!(
                "ok: {} (compressed; will decompress on prepare)",
                gz.display()
            );
        } else {
            println!(
                "warn: missing {} (also looked for .xz / .gz)",
                img.display()
            );
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
        (gz_path, "gunzip", vec!["--keep".into(), "--force".into()])
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
    let target = optional_option_value(args, "--target")
        .map(|value| TxTarget::parse(&value))
        .transpose()?;
    fs::create_dir_all(&submit).map_err(|err| err.to_string())?;
    match target {
        Some(TxTarget::Rv64Qemu) => {
            copy_kernel_for_oscomp(root, TxTarget::Rv64Qemu, &submit.join("kernel-rv"))?;
        }
        Some(TxTarget::La64Qemu) => {
            copy_kernel_for_oscomp(root, TxTarget::La64Qemu, &submit.join("kernel-la"))?;
        }
        Some(TxTarget::Rv64M1DockMock) => {
            return Err("OSComp submit supports rv64-qemu and la64-qemu".into());
        }
        None => {
            copy_kernel_for_oscomp(root, TxTarget::Rv64Qemu, &submit.join("kernel-rv"))?;
            copy_kernel_for_oscomp(root, TxTarget::La64Qemu, &submit.join("kernel-la"))?;
        }
    }
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
                    "file={},if=none,format=raw,id=x0,file.locking=off",
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
                    "file={},if=none,format=raw,id=x0,file.locking=off",
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

/// Score an existing serial output file with the OSComp judge scripts.
///
/// Options:
///   `--target rv64-qemu|la64-qemu`  Selects the default input file (default: rv64).
///   `--input FILE`                  Override the input serial-output file.
///   `--suite SUITE`                 Filter output to a single test suite.
///   `--data DIR`                    Override the judge-scripts directory.
///   `--dry-run`                     Print what would be scored and exit.
fn oscomp_score(root: &Path, args: &[String]) -> Result<()> {
    let data = oscomp_data_dir(root, args);
    let suite_filter = optional_option_value(args, "--suite");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    let input = optional_option_value(args, "--input")
        .map(PathBuf::from)
        .map(|p| resolve_path(root, p))
        .unwrap_or_else(|| {
            let suffix = optional_option_value(args, "--target")
                .map(|t| if t.starts_with("la") { "la" } else { "rv" })
                .unwrap_or("rv");
            root.join("target")
                .join("oscomp")
                .join(format!("os_serial_out_{suffix}.txt"))
        });

    let judge_py = root.join("tools").join("oscomp-judge.py");

    println!("score: {} vs {}", input.display(), data.display());
    if let Some(s) = &suite_filter {
        println!("suite:  {s}");
    }
    if dry_run {
        return Ok(());
    }

    if !judge_py.exists() {
        return Err(format!(
            "missing {}; ensure tools/oscomp-judge.py exists",
            judge_py.display()
        ));
    }
    if !input.exists() {
        return Err(format!(
            "missing serial output {}; run `cargo xtask oscomp qemu --target ...` first",
            input.display()
        ));
    }
    if !data.exists() {
        return Err(format!(
            "testdata dir {} not found; run `cargo xtask oscomp prepare` first",
            data.display()
        ));
    }

    let output = Command::new("python3")
        .arg(&judge_py)
        .arg(&input)
        .arg(&data)
        .current_dir(root)
        .output()
        .map_err(|e| format!("failed to run python3 {}: {e}", judge_py.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    if let Some(suite) = &suite_filter {
        print_score_suite(&stdout, suite);
    } else {
        print!("{stdout}");
    }

    if output.status.success() {
        Ok(())
    } else {
        Err(format!("judge exited with {}", output.status))
    }
}

/// Print only the output block for `suite` plus the final total line.
///
/// The judge emits lines like:
/// ```text
/// [busybox-musl] 52/55
///   ✓ ls  1/1
///   ...
///
/// 总分: 62/65
/// ```
/// We capture just the requested group's block and the 总分 line.
fn print_score_suite(output: &str, suite: &str) {
    let mut in_suite = false;
    let mut total_line: Option<&str> = None;

    for line in output.lines() {
        if line.starts_with('[') {
            let group_end = line.find(']').unwrap_or(0);
            in_suite = &line[1..group_end] == suite;
            if in_suite {
                println!("{line}");
            }
        } else if line.contains("总分") {
            total_line = Some(line);
        } else if in_suite {
            println!("{line}");
        }
    }

    if let Some(total) = total_line {
        println!();
        println!("{total}");
    }
}

/// List available test suites found in the judge-scripts directory.
///
/// Options:
///   `--target rv64-qemu|la64-qemu`  Hint which variant is primary for that platform.
///   `--data DIR`                    Override the judge-scripts directory.
fn oscomp_list_suites(root: &Path, args: &[String]) -> Result<()> {
    let data = oscomp_data_dir(root, args);
    let target_filter = optional_option_value(args, "--target");

    if !data.exists() {
        return Err(format!(
            "testdata dir {} not found; run `cargo xtask oscomp prepare` first",
            data.display()
        ));
    }

    let mut suites: Vec<String> = fs::read_dir(&data)
        .map_err(|e| e.to_string())?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            if name.starts_with("judge_") && name.ends_with(".py") {
                Some(name["judge_".len()..name.len() - 3].to_string())
            } else {
                None
            }
        })
        .collect();
    suites.sort();

    if let Some(target) = &target_filter {
        let hint = if target.starts_with("la") {
            "-glibc"
        } else {
            "-musl"
        };
        println!(
            "Suites in {} (primary variant for {target}: {hint}*):",
            data.display()
        );
    } else {
        println!("Suites in {}:", data.display());
    }
    for suite in &suites {
        println!("  {suite}");
    }
    Ok(())
}

/// Build the kernel, run it against the OSComp sdcard, and score the results.
///
/// Options:
///   `--target rv64-qemu|la64-qemu`  Required.
///   `--suite SUITE`                 Filter the final score display to one suite.
///   `--skip-build`                  Skip the full-build step.
///   `--data DIR`                    Override the judge-scripts / sdcard directory.
///   `--submit DIR`                  Override the submit directory.
///   `--dry-run`                     Print commands without executing them.
fn oscomp_test(root: &Path, args: &[String]) -> Result<()> {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let skip_build = args.iter().any(|a| a == "--skip-build");
    let target_str = option_value(args, "--target")?;
    let target = TxTarget::parse(&target_str)?;

    if matches!(target, TxTarget::Rv64M1DockMock) {
        return Err("oscomp test supports rv64-qemu and la64-qemu only".into());
    }

    let submit = optional_option_value(args, "--submit")
        .map(PathBuf::from)
        .map(|p| resolve_path(root, p))
        .unwrap_or_else(|| root.join("target").join("oscomp").join("submit"));
    let kernel_dest_name = match target {
        TxTarget::Rv64Qemu => "kernel-rv",
        TxTarget::La64Qemu => "kernel-la",
        TxTarget::Rv64M1DockMock => unreachable!(),
    };

    // Step 1: full-build
    if skip_build {
        println!("==> [skip] full-build --target {target_str}");
    } else {
        println!("==> full-build --target {target_str}");
        if !dry_run {
            crate::full_build::full_build(
                root,
                vec![
                    "--target".into(),
                    target_str.clone(),
                    "--skip-doctor".into(),
                ],
            )?;
        }
    }

    // Step 2: copy kernel to submit dir
    println!(
        "==> copy kernel → {}",
        submit.join(kernel_dest_name).display()
    );
    if !dry_run {
        fs::create_dir_all(&submit).map_err(|e| e.to_string())?;
        copy_kernel_for_oscomp(root, target, &submit.join(kernel_dest_name))?;
    }

    // Step 3: run QEMU (all suites from sdcard)
    println!("==> oscomp qemu --target {target_str}");
    oscomp_qemu(root, args)?;

    // Step 4: score
    println!("==> oscomp score --target {target_str}");
    oscomp_score(root, args)?;

    Ok(())
}

/// Build a trimmed SD card image with only specified test suites/cases.
///
/// Options:
///   `--suite SUITE`               Include a test suite (repeatable).
///   `--ltp-cases CASE1,CASE2`     LTP cases to include (comma-separated).
///   `--source IMG`                Source sdcard image (default: testdata/sdcard-rv.img).
///   `--output IMG`                Output image path.
///   `--size-mb N`                 Target image size in MB (default: 256).
///   `--config FILE`               TOML config file.
///   `--list-suites`               List available suites and exit.
///   `--list-cases SUITE`          List cases for a suite and exit.
fn oscomp_slim_sdcard(root: &Path, args: &[String]) -> Result<()> {
    let script = root.join("tools").join("build-slim-sdcard.py");
    if !script.exists() {
        return Err(format!(
            "missing {}; this command requires the Python helper script",
            script.display()
        ));
    }

    // Forward all arguments to the Python script, with --source defaulting
    // to the canonical testdata sdcard path.
    let data = oscomp_data_dir(root, args);
    let default_source = data.join("sdcard-rv.img");

    let mut cmd = Command::new("python3");
    cmd.arg(&script).current_dir(root);

    // If --source is not provided and default exists, add it
    let has_source = args.iter().any(|a| a == "--source" || a == "-s");
    if !has_source && default_source.exists() {
        cmd.arg("--source").arg(&default_source);
    }

    for arg in args {
        cmd.arg(arg);
    }

    let status = cmd.status().map_err(|e| format!("failed to run build-slim-sdcard.py: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("build-slim-sdcard.py exited with {status}"))
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
