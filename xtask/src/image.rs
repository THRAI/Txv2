use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::target::{Profile, TxTarget};
use crate::util::{
    command_exists, option_value, optional_option_value, run_cmd_owned, run_shell, shell_escape,
};
use crate::Result;

pub(crate) fn image(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(kind) = args.first() else {
        return Err("image command needs a kind: cpio, initramfs, ext4, vf2-uimage, or la-uimage".into());
    };
    if kind.as_str() == "vf2-uimage" {
        return image_vf2_uimage(root, &args[1..]);
    }
    if kind.as_str() == "la-uimage" {
        return image_la_uimage(root, &args[1..]);
    }
    let profile = Profile::parse(&option_value(&args[1..], "--profile")?)?;
    let target = image_target(&args[1..])?;
    match (kind.as_str(), profile) {
        ("cpio" | "initramfs", Profile::Busybox) => image_cpio_busybox(root, target),
        ("cpio" | "initramfs", Profile::Alpine) => image_cpio_alpine(root, &args[1..], target),
        ("ext4", Profile::Busybox) => {
            let name = busybox_root_ext4_name(target);
            image_ext4_busybox(root, &args[1..], target, &name)
        }
        ("m1dock-sd", Profile::Busybox) => {
            image_ext4_busybox(root, &args[1..], target, "m1dock-sd.img")
        }
        ("ext4" | "m1dock-sd", Profile::Alpine) => {
            Err("image alpine profile currently supports cpio/initramfs only".into())
        }
        ("cpio" | "initramfs" | "ext4" | "m1dock-sd", Profile::Smoke) => {
            Err("image smoke profile is not defined; use --profile busybox".into())
        }
        (other, _) => Err(format!(
            "unknown image kind '{other}', expected cpio, ext4, or m1dock-sd"
        )),
    }
}

/// VF2 U-Boot load/entry address. Intentionally the QEMU link address:
/// VF2 DDR spans 0x4000_0000..(+2..8 GiB), so 0x8020_0000 is valid RAM
/// there and the same kernel binary boots QEMU and the board (see
/// ljs/03-上板完整计划.md P2; proven by Chronix onsite).
const VF2_LOAD_ADDR: &str = "0x80200000";

/// Package the rv64 kernel ELF as a VF2 boot artifact: strip to a raw
/// binary, then wrap as a U-Boot uImage (mkimage -T kernel).
///
///   cargo xtask build --target rv64-qemu [--release]
///   cargo xtask image vf2-uimage [--release]
///   cp target/images/txv2-vf2.uimage /srv/tftp/
///   # U-Boot:  tftpboot 0x80200000 txv2-vf2.uimage
///   #          bootm 0x80200000 - ${fdtcontroladdr}
fn image_vf2_uimage(root: &Path, args: &[String]) -> Result<()> {
    let release = args.iter().any(|arg| arg == "--release");
    let kernel = TxTarget::Rv64Qemu.kernel_path_for_profile(root, release);
    if !kernel.exists() {
        return Err(format!(
            "kernel ELF not found at {}; run `cargo xtask build --target rv64-qemu{}` first",
            kernel.display(),
            if release { " --release" } else { "" }
        ));
    }

    let objcopy = [
        "rust-objcopy",
        "llvm-objcopy",
        "riscv64-unknown-elf-objcopy",
        "riscv64-linux-gnu-objcopy",
    ]
    .into_iter()
    .find(|program| command_exists(program))
    .ok_or("no objcopy found; install cargo-binutils (rust-objcopy) or llvm/riscv binutils")?;
    if !command_exists("mkimage") {
        return Err("mkimage not found; install u-boot-tools".into());
    }

    let out_dir = root.join("target").join("images");
    fs::create_dir_all(&out_dir).map_err(|err| err.to_string())?;
    let bin = out_dir.join("txv2-vf2.bin");
    let uimage = out_dir.join("txv2-vf2.uimage");

    run_cmd_owned(
        root,
        objcopy,
        &[
            "--strip-all".to_string(),
            "-O".to_string(),
            "binary".to_string(),
            kernel.display().to_string(),
            bin.display().to_string(),
        ],
    )?;
    run_cmd_owned(
        root,
        "mkimage",
        &[
            "-A".to_string(),
            "riscv".to_string(),
            "-O".to_string(),
            "linux".to_string(),
            "-T".to_string(),
            "kernel".to_string(),
            "-C".to_string(),
            "none".to_string(),
            "-a".to_string(),
            VF2_LOAD_ADDR.to_string(),
            "-e".to_string(),
            VF2_LOAD_ADDR.to_string(),
            "-n".to_string(),
            "Txv2".to_string(),
            "-d".to_string(),
            bin.display().to_string(),
            uimage.display().to_string(),
        ],
    )?;

    // Wrap the busybox initramfs as a legacy uImage ramdisk when one
    // has been built: `bootm <kernel> <ramdisk> <fdt>` with a wrapped
    // ramdisk is unambiguous on the VF2's 2021.10 U-Boot, unlike the
    // raw `addr:size` notation (only board-proven with booti).
    let initramfs = out_dir.join(busybox_initramfs_name(TxTarget::Rv64Qemu));
    let initrd_uimage = out_dir.join("txv2-vf2-initrd.uimage");
    let have_initrd = initramfs.exists();
    if have_initrd {
        run_cmd_owned(
            root,
            "mkimage",
            &[
                "-A".to_string(),
                "riscv".to_string(),
                "-O".to_string(),
                "linux".to_string(),
                "-T".to_string(),
                "ramdisk".to_string(),
                "-C".to_string(),
                "none".to_string(),
                "-n".to_string(),
                "Txv2-initrd".to_string(),
                "-d".to_string(),
                initramfs.display().to_string(),
                initrd_uimage.display().to_string(),
            ],
        )?;
    }

    println!("vf2 uimage ready: {}", uimage.display());
    if have_initrd {
        println!("vf2 initrd ready: {}", initrd_uimage.display());
        println!("next: cp {} {} /srv/tftp/", uimage.display(), initrd_uimage.display());
        println!("U-Boot> tftpboot {VF2_LOAD_ADDR} txv2-vf2.uimage");
        println!("U-Boot> tftpboot 0x88300000 txv2-vf2-initrd.uimage");
        println!("U-Boot> bootm {VF2_LOAD_ADDR} 0x88300000 ${{fdtcontroladdr}}");
    } else {
        println!("(no busybox initramfs found; build with `cargo xtask image cpio --profile busybox --target rv64-qemu` for an interactive shell)");
        println!("next: cp {} /srv/tftp/", uimage.display());
        println!("U-Boot> tftpboot {VF2_LOAD_ADDR} txv2-vf2.uimage");
        println!("U-Boot> bootm {VF2_LOAD_ADDR} - ${{fdtcontroladdr}}");
    }
    Ok(())
}

/// LS2K1000 U-Boot load/entry address, via the cached DMW window —
/// NPUcore-BLOSSOM's board-proven `mkimage -a/-e` value (their
/// `os/make/la_board/la64board.mk`). Physical base 0x9000_0000 equals
/// our unified `KERNEL_LOAD_BASE`, so the same kernel binary boots
/// QEMU 9.2.1 (whose high RAM covers it) and the board.
const LA_LOAD_ADDR: &str = "0x9000000090000000";

/// Package the la64 kernel ELF as an LS2K1000 boot artifact: strip to
/// a raw binary, then wrap as a U-Boot uImage (mkimage -T kernel).
///
///   cargo xtask build --target la64-qemu [--release]
///   cargo xtask image la-uimage [--release]
///   cp target/images/txv2-la.uimage /srv/tftp/
///   # U-Boot:  tftpboot txv2-la.uimage   (default $loadaddr)
///   #          bootm                     (relocates to -a and jumps)
fn image_la_uimage(root: &Path, args: &[String]) -> Result<()> {
    let release = args.iter().any(|arg| arg == "--release");
    let kernel = TxTarget::La64Qemu.kernel_path_for_profile(root, release);
    if !kernel.exists() {
        return Err(format!(
            "kernel ELF not found at {}; run `cargo xtask build --target la64-qemu{}` first",
            kernel.display(),
            if release { " --release" } else { "" }
        ));
    }

    let objcopy = [
        "rust-objcopy",
        "llvm-objcopy",
        "loongarch64-unknown-linux-gnu-objcopy",
        "loongarch64-linux-gnu-objcopy",
    ]
    .into_iter()
    .find(|program| command_exists(program))
    .ok_or("no objcopy found; install cargo-binutils (rust-objcopy) or llvm/loongarch binutils")?;
    // Distro u-boot-tools mkimage predates LoongArch; prefer the
    // vendored NPUcore binary (tools/README.md for provenance).
    let vendored_mkimage = root.join("tools").join("mkimage-loongarch");
    let mkimage = if vendored_mkimage.exists() {
        vendored_mkimage.display().to_string()
    } else if command_exists("mkimage") {
        "mkimage".to_string()
    } else {
        return Err("no mkimage found; expected tools/mkimage-loongarch or u-boot-tools".into());
    };

    let out_dir = root.join("target").join("images");
    fs::create_dir_all(&out_dir).map_err(|err| err.to_string())?;
    let bin = out_dir.join("txv2-la.bin");
    let uimage = out_dir.join("txv2-la.uimage");

    run_cmd_owned(
        root,
        objcopy,
        &[
            "--strip-all".to_string(),
            "-O".to_string(),
            "binary".to_string(),
            kernel.display().to_string(),
            bin.display().to_string(),
        ],
    )?;
    run_cmd_owned(
        root,
        &mkimage,
        &[
            "-A".to_string(),
            "loongarch".to_string(),
            "-O".to_string(),
            "linux".to_string(),
            "-T".to_string(),
            "kernel".to_string(),
            "-C".to_string(),
            "none".to_string(),
            "-a".to_string(),
            LA_LOAD_ADDR.to_string(),
            "-e".to_string(),
            LA_LOAD_ADDR.to_string(),
            "-n".to_string(),
            "Txv2-la".to_string(),
            "-d".to_string(),
            bin.display().to_string(),
            uimage.display().to_string(),
        ],
    )?;

    let initramfs = out_dir.join(busybox_initramfs_name(TxTarget::La64Qemu));
    let initrd_uimage = out_dir.join("txv2-la-initrd.uimage");
    let have_initrd = initramfs.exists();
    if have_initrd {
        run_cmd_owned(
            root,
            &mkimage,
            &[
                "-A".to_string(),
                "loongarch".to_string(),
                "-O".to_string(),
                "linux".to_string(),
                "-T".to_string(),
                "ramdisk".to_string(),
                "-C".to_string(),
                "none".to_string(),
                "-n".to_string(),
                "Txv2-la-initrd".to_string(),
                "-d".to_string(),
                initramfs.display().to_string(),
                initrd_uimage.display().to_string(),
            ],
        )?;
    }

    println!("la uimage ready: {}", uimage.display());
    if have_initrd {
        println!("la initrd ready: {}", initrd_uimage.display());
    }
    println!("next: cp target/images/txv2-la*.uimage /srv/tftp/");
    println!("U-Boot> tftpboot txv2-la.uimage");
    println!("U-Boot> bootm    # relocates payload to {LA_LOAD_ADDR} and jumps");
    Ok(())
}

pub(crate) fn busybox_initramfs_name(target: TxTarget) -> String {
    format!("busybox-initramfs-{}.cpio", target.name())
}

pub(crate) fn alpine_initramfs_name(target: TxTarget) -> String {
    format!("alpine-initramfs-{}.cpio", target.name())
}

pub(crate) fn busybox_root_ext4_name(target: TxTarget) -> String {
    format!("busybox-root-{}.ext4", target.name())
}

fn image_target(args: &[String]) -> Result<TxTarget> {
    optional_option_value(args, "--target")
        .map(|target| TxTarget::parse(&target))
        .transpose()
        .map(|target| target.unwrap_or(TxTarget::Rv64Qemu))
}

fn image_cpio_busybox(root: &Path, target: TxTarget) -> Result<()> {
    if !command_exists("cpio") {
        return Err("cpio is required to create the busybox initramfs".into());
    }
    let layout = prepare_busybox_rootfs(root, target)?;
    let out = root
        .join("target")
        .join("images")
        .join(busybox_initramfs_name(target));
    fs::create_dir_all(out.parent().expect("image path has parent"))
        .map_err(|err| err.to_string())?;
    remove_existing_image(&out)?;

    let script = format!(
        "cd '{}' && find . -print | cpio -o -H newc > '{}'",
        shell_escape(&layout.display().to_string()),
        shell_escape(&out.display().to_string())
    );
    run_shell(root, &script)?;
    println!("wrote {}", out.display());
    Ok(())
}

fn image_cpio_alpine(root: &Path, args: &[String], target: TxTarget) -> Result<()> {
    if target != TxTarget::Rv64Qemu {
        return Err("alpine image profile is currently supported only for rv64-qemu".into());
    }
    if !command_exists("cpio") {
        return Err("cpio is required to create the alpine initramfs".into());
    }
    let layout = prepare_alpine_rootfs(root, args, target)?;
    let out = root
        .join("target")
        .join("images")
        .join(alpine_initramfs_name(target));
    fs::create_dir_all(out.parent().expect("image path has parent"))
        .map_err(|err| err.to_string())?;
    remove_existing_image(&out)?;

    let script = format!(
        "cd '{}' && find . -print | cpio -o -H newc > '{}'",
        shell_escape(&layout.display().to_string()),
        shell_escape(&out.display().to_string())
    );
    run_shell(root, &script)?;
    println!("wrote {}", out.display());
    Ok(())
}

fn image_ext4_busybox(
    root: &Path,
    args: &[String],
    target: TxTarget,
    output_name: &str,
) -> Result<()> {
    if !command_exists("mkfs.ext4") {
        return Err("mkfs.ext4 is required to create the busybox ext4 image".into());
    }
    let size = optional_option_value(args, "--size").unwrap_or_else(|| "64M".to_string());
    let layout = prepare_busybox_rootfs(root, target)?;
    let out = root.join("target").join("images").join(output_name);
    fs::create_dir_all(out.parent().expect("image path has parent"))
        .map_err(|err| err.to_string())?;
    remove_existing_image(&out)?;

    run_cmd_owned(
        root,
        "truncate",
        &["-s".into(), size.clone(), out.display().to_string()],
    )?;
    run_cmd_owned(
        root,
        "mkfs.ext4",
        &[
            "-F".into(),
            "-L".into(),
            "TXROOT".into(),
            "-d".into(),
            layout.display().to_string(),
            out.display().to_string(),
        ],
    )?;
    println!("wrote {} ({size})", out.display());
    Ok(())
}

fn remove_existing_image(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("failed to remove {}: {err}", path.display())),
    }
}

/// RV64 in-tree vendored busybox used when `TX_BUSYBOX` is not set.
///
/// Populated by `tools/images/fetch-busybox.sh`. Kept relative so the path
/// printed in errors matches what's checked into the repo.
pub(crate) const VENDORED_BUSYBOX_RELPATH: &str = "tools/images/vendor/busybox-riscv64-musl";

/// LA64 in-tree vendored busybox used when `TX_BUSYBOX` is not set.
///
/// Populated by `tools/images/build-busybox-loongarch64.sh`.
pub(crate) const VENDORED_LA64_BUSYBOX_RELPATH: &str =
    "tools/images/vendor/busybox-loongarch64-musl";

pub(crate) fn vendored_busybox_relpath(target: TxTarget) -> &'static str {
    match target {
        TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock => VENDORED_BUSYBOX_RELPATH,
        TxTarget::La64Qemu => VENDORED_LA64_BUSYBOX_RELPATH,
    }
}

fn resolve_busybox(root: &Path, target: TxTarget) -> Result<PathBuf> {
    if let Ok(value) = env::var("TX_BUSYBOX") {
        let busybox = PathBuf::from(value);
        if !busybox.is_file() {
            return Err(format!(
                "TX_BUSYBOX must point at a file, got {}",
                busybox.display()
            ));
        }
        return Ok(busybox);
    }
    let relpath = vendored_busybox_relpath(target);
    let vendored = root.join(relpath);
    if vendored.is_file() {
        return Ok(vendored);
    }
    let help = match target {
        TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock => {
            "run `tools/images/fetch-busybox.sh` or set TX_BUSYBOX"
        }
        TxTarget::La64Qemu => "run `tools/images/build-busybox-loongarch64.sh` or set TX_BUSYBOX",
    };
    Err(format!(
        "TX_BUSYBOX is not set and vendored busybox for {} is missing at {}; {help}",
        target.name(),
        relpath
    ))
}

fn prepare_busybox_rootfs(root: &Path, target: TxTarget) -> Result<PathBuf> {
    let busybox = resolve_busybox(root, target)?;

    let layout = root
        .join("target")
        .join("rootfs")
        .join(format!("busybox-musl-{}", target.name()));
    if layout.exists() {
        fs::remove_dir_all(&layout).map_err(|err| err.to_string())?;
    }
    for dir in ["bin", "dev", "etc", "lib", "proc", "sys", "tmp", "usr/bin"] {
        fs::create_dir_all(layout.join(dir)).map_err(|err| err.to_string())?;
    }
    fs::copy(&busybox, layout.join("bin").join("busybox")).map_err(|err| err.to_string())?;
    install_optional_user_smokes(root, target, &layout)?;
    install_optional_oscomp_net_tools(target, &layout)?;

    // IPC smoke test binary — copy if present
    let ipc_test_src = root.join("tools/images/ipc_test");
    if ipc_test_src.is_file() {
        fs::copy(&ipc_test_src, layout.join("bin").join("ipc_test"))
            .map_err(|err| err.to_string())?;
    }

    // IPC smoke test binary — copy if present
    let ipc_test_src = root.join("tools/images/ipc_test");
    if ipc_test_src.is_file() {
        fs::copy(&ipc_test_src, layout.join("bin").join("ipc_test"))
            .map_err(|err| err.to_string())?;
    }

    if let Ok(musl) = env::var("TX_MUSL_LIBC") {
        let musl = PathBuf::from(musl);
        if !musl.is_file() {
            return Err(format!(
                "TX_MUSL_LIBC must point at a file, got {}",
                musl.display()
            ));
        }
        install_musl_libc(&layout, &musl)?;
    }

    fs::write(
        layout.join("etc").join("inittab"),
        "::sysinit:/etc/init.d/rcS\n",
    )
    .map_err(|err| err.to_string())?;

    #[cfg(unix)]
    {
        // BusyBox applet wiring: symlink each applet name to the
        // single `busybox` binary. Previously this used `hard_link`,
        // which created 28 directory entries all pointing at the same
        // inode — and `find … | cpio -o -H newc` honours the newc
        // hardlink convention by emitting file data on the LAST
        // hardlink only, leaving the others as 0-byte stubs. The
        // kernel's initramfs unpacker does not resolve newc
        // hardlinks (each entry is a fresh inode), so 27 of 28
        // applets — including `/bin/sh` and `/bin/busybox` itself —
        // landed empty, and exec rejected them with `not-executable`.
        // Symlinks sidestep the issue: `cpio` emits each entry as a
        // first-class symlink, the unpacker calls
        // `FsOps::symlink(...)`, and walker chases through to the
        // single 1.4 MiB regular file. Matches BusyBox's standard
        // install layout, where applets are also symlinks.
        for name in [
            "sh", "ls", "cat", "mkdir", "rm", "rmdir", "mv", "cp", "touch", "pwd", "echo", "ln",
            "chmod", "chown", "uname", "ps", "kill", "grep", "find", "head", "tail", "wc", "sort",
            "sed", "awk", "mount",
        ] {
            let path = layout.join("bin").join(name);
            if path.exists() {
                fs::remove_file(&path).map_err(|err| err.to_string())?;
            }
            // Relative target so the symlink resolves to the sibling
            // `busybox` regardless of where the rootfs is mounted.
            unix_fs::symlink("busybox", path).map_err(|err| err.to_string())?;
        }

        // Initramfs slice (2026-05-08): replace the shebang `/init`
        // wrapper with a symlink to `/bin/sh`. The kernel's exec
        // hits busybox directly (no script-interpreter walk) and
        // busybox sees argv[0]=sh, running the shell applet.
        let init_path = layout.join("init");
        if init_path.exists() {
            fs::remove_file(&init_path).map_err(|err| err.to_string())?;
        }
        unix_fs::symlink("/bin/sh", &init_path).map_err(|err| err.to_string())?;
    }

    Ok(layout)
}

fn prepare_alpine_rootfs(root: &Path, args: &[String], target: TxTarget) -> Result<PathBuf> {
    let source = resolve_alpine_rootfs(root, args, target)?;
    if !source.is_dir() {
        return Err(format!(
            "alpine rootfs source must be a directory, got {}",
            source.display()
        ));
    }

    let layout = root
        .join("target")
        .join("rootfs")
        .join(format!("alpine-stage-{}", target.name()));
    if layout.exists() {
        fs::remove_dir_all(&layout).map_err(|err| err.to_string())?;
    }
    fs::create_dir_all(&layout).map_err(|err| err.to_string())?;

    let script = format!(
        "cp -a '{}'/'.' '{}'",
        shell_escape(&source.display().to_string()),
        shell_escape(&layout.display().to_string())
    );
    run_shell(root, &script)?;

    for dir in ["dev", "proc", "sys", "tmp", "run", "var/run"] {
        fs::create_dir_all(layout.join(dir)).map_err(|err| err.to_string())?;
    }
    install_alpine_bootstrap_busybox(root, target, args, &layout)?;
    install_optional_user_smokes(root, target, &layout)?;
    Ok(layout)
}

fn resolve_alpine_rootfs(root: &Path, args: &[String], target: TxTarget) -> Result<PathBuf> {
    if let Some(value) = optional_option_value(args, "--rootfs") {
        return Ok(resolve_root_path(root, &value));
    }
    if let Ok(value) = env::var("TX_ALPINE_ROOTFS") {
        return Ok(resolve_root_path(root, &value));
    }
    Ok(root
        .join("target")
        .join("rootfs")
        .join(format!("alpine-{}", target.name())))
}

fn resolve_root_path(root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn install_alpine_bootstrap_busybox(
    root: &Path,
    target: TxTarget,
    args: &[String],
    layout: &Path,
) -> Result<()> {
    if target != TxTarget::Rv64Qemu {
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--no-bootstrap-busybox")
        || env::var("TX_ALPINE_BOOTSTRAP_BUSYBOX").as_deref() == Ok("0")
    {
        return Ok(());
    }

    let busybox = resolve_busybox(root, target)?;
    let bin = layout.join("bin");
    fs::create_dir_all(&bin).map_err(|err| err.to_string())?;
    let bootstrap_name = "tx-bootstrap-busybox";
    fs::copy(&busybox, bin.join(bootstrap_name)).map_err(|err| err.to_string())?;

    #[cfg(unix)]
    {
        let sh = bin.join("sh");
        if sh.exists() {
            fs::remove_file(&sh).map_err(|err| err.to_string())?;
        }
        unix_fs::symlink(bootstrap_name, sh).map_err(|err| err.to_string())?;
    }

    println!(
        "alpine: installed static bootstrap shell from {} as /bin/{}",
        busybox.display(),
        bootstrap_name
    );
    Ok(())
}

/// Optionally install OSComp/RustOS network benchmark binaries into
/// the BusyBox rootfs.
///
/// This is deliberately opt-in: the benchmark binaries are large,
/// live outside this repository, and mix static (`iperf3`) with
/// dynamic (`netperf`/`netserver`) musl layouts depending on the
/// source tree.
///
/// Supported knobs:
///
/// - `TX_OSCOMP_RISCV_MUSL_DIR=/path/to/testcase/riscv/musl` copies
///   `iperf3`, `netperf`, `netserver` when present and installs
///   `lib/libc.so` as the musl loader payload.
/// - `TX_IPERF3`, `TX_NETPERF`, `TX_NETSERVER` copy individual
///   binaries and override files from the directory knob.
fn install_optional_oscomp_net_tools(target: TxTarget, layout: &Path) -> Result<()> {
    if target != TxTarget::Rv64Qemu {
        return Ok(());
    }

    if let Ok(dir) = env::var("TX_OSCOMP_RISCV_MUSL_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.is_dir() {
            return Err(format!(
                "TX_OSCOMP_RISCV_MUSL_DIR must point at a directory, got {}",
                dir.display()
            ));
        }
        for name in ["iperf3", "netperf", "netserver"] {
            copy_optional_binary(&dir.join(name), &layout.join("bin").join(name))?;
        }
        let libc = dir.join("lib").join("libc.so");
        if libc.is_file() {
            install_musl_libc(layout, &libc)?;
        }
    }

    copy_env_binary("TX_IPERF3", &layout.join("bin").join("iperf3"))?;
    copy_env_binary("TX_NETPERF", &layout.join("bin").join("netperf"))?;
    copy_env_binary("TX_NETSERVER", &layout.join("bin").join("netserver"))?;

    Ok(())
}

fn copy_env_binary(var: &str, dest: &Path) -> Result<()> {
    let Ok(value) = env::var(var) else {
        return Ok(());
    };
    let source = PathBuf::from(value);
    if !source.is_file() {
        return Err(format!(
            "{var} must point at a file, got {}",
            source.display()
        ));
    }
    fs::copy(&source, dest).map_err(|err| err.to_string())?;
    Ok(())
}

fn copy_optional_binary(source: &Path, dest: &Path) -> Result<()> {
    if source.is_file() {
        fs::copy(source, dest).map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn install_musl_libc(layout: &Path, musl: &Path) -> Result<()> {
    let libc = layout.join("lib").join("libc.so");
    fs::copy(musl, &libc).map_err(|err| err.to_string())?;
    install_musl_loader_links(layout)
}

#[cfg(unix)]
fn install_musl_loader_links(layout: &Path) -> Result<()> {
    for loader in [
        "ld-musl-riscv64.so.1",
        "ld-musl-riscv64-sf.so.1",
        "ld-musl-loongarch64.so.1",
    ] {
        let loader_path = layout.join("lib").join(loader);
        if loader_path.exists() {
            fs::remove_file(&loader_path).map_err(|err| err.to_string())?;
        }
        unix_fs::symlink("libc.so", loader_path).map_err(|err| err.to_string())?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn install_musl_loader_links(_layout: &Path) -> Result<()> {
    Ok(())
}

fn install_optional_user_smokes(root: &Path, target: TxTarget, layout: &Path) -> Result<()> {
    if target != TxTarget::Rv64Qemu {
        return Ok(());
    }
    if !command_exists("riscv64-linux-gnu-gcc") {
        println!("warn: riscv64-linux-gnu-gcc not found; skipping optional user smokes");
        return Ok(());
    }

    for name in [
        "udp-loopback-smoke",
        "tcp-loopback-smoke",
        "tcp-external-smoke",
        "tcp-external-accept-smoke",
        "tcp-external-bulk-smoke",
        "tcp-external-seq-smoke",
        "udp-external-dns-smoke",
        "epoll-external-smoke",
        "netns-helper",
        "nft-probe",
        "packet-probe",
        "netcap-probe",
    ] {
        let source = root.join("tools").join("user").join(format!("{name}.c"));
        if !source.is_file() {
            continue;
        }
        let out = root
            .join("target")
            .join("images")
            .join(format!("{name}-riscv64"));
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let status = Command::new("riscv64-linux-gnu-gcc")
            .args([
                "-nostdlib",
                "-static",
                "-ffreestanding",
                "-fno-builtin",
                "-fno-stack-protector",
                "-O2",
                "-Wall",
                "-Wextra",
            ])
            .arg(&source)
            .arg("-o")
            .arg(&out)
            .status()
            .map_err(|err| format!("failed to run riscv64-linux-gnu-gcc: {err}"))?;
        if !status.success() {
            return Err(format!("riscv64-linux-gnu-gcc exited with {status}"));
        }
        fs::copy(&out, layout.join("bin").join(name)).map_err(|err| err.to_string())?;
    }
    Ok(())
}
