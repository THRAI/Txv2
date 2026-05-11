use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use crate::target::{Profile, TxTarget};
use crate::util::{
    command_exists, option_value, optional_option_value, run_cmd_owned, run_shell, shell_escape,
};
use crate::Result;

pub(crate) fn image(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(kind) = args.first() else {
        return Err("image command needs a kind: cpio, initramfs, or ext4".into());
    };
    let profile = Profile::parse(&option_value(&args[1..], "--profile")?)?;
    let target = image_target(&args[1..])?;
    match (kind.as_str(), profile) {
        ("cpio" | "initramfs", Profile::Busybox) => image_cpio_busybox(root, target),
        ("ext4", Profile::Busybox) => {
            image_ext4_busybox(root, &args[1..], target, "busybox-root.ext4")
        }
        ("m1dock-sd", Profile::Busybox) => {
            image_ext4_busybox(root, &args[1..], target, "m1dock-sd.img")
        }
        ("cpio" | "initramfs" | "ext4" | "m1dock-sd", Profile::Smoke) => {
            Err("image smoke profile is not defined; use --profile busybox".into())
        }
        (other, _) => Err(format!(
            "unknown image kind '{other}', expected cpio, ext4, or m1dock-sd"
        )),
    }
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
        .join("busybox-initramfs.cpio");
    fs::create_dir_all(out.parent().expect("image path has parent"))
        .map_err(|err| err.to_string())?;

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

    let layout = root.join("target").join("rootfs").join("busybox-musl");
    if layout.exists() {
        fs::remove_dir_all(&layout).map_err(|err| err.to_string())?;
    }
    for dir in ["bin", "dev", "etc", "lib", "proc", "sys", "tmp", "usr/bin"] {
        fs::create_dir_all(layout.join(dir)).map_err(|err| err.to_string())?;
    }
    fs::copy(&busybox, layout.join("bin").join("busybox")).map_err(|err| err.to_string())?;

    if let Ok(musl) = env::var("TX_MUSL_LIBC") {
        let musl = PathBuf::from(musl);
        if !musl.is_file() {
            return Err(format!(
                "TX_MUSL_LIBC must point at a file, got {}",
                musl.display()
            ));
        }
        let libc = layout.join("lib").join("libc.so");
        fs::copy(&musl, &libc).map_err(|err| err.to_string())?;
        #[cfg(unix)]
        {
            for loader in ["ld-musl-riscv64.so.1", "ld-musl-loongarch64.so.1"] {
                let loader_path = layout.join("lib").join(loader);
                if loader_path.exists() {
                    fs::remove_file(&loader_path).map_err(|err| err.to_string())?;
                }
                unix_fs::symlink("libc.so", loader_path).map_err(|err| err.to_string())?;
            }
        }
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
