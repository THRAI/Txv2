use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use crate::target::Profile;
use crate::util::{
    command_exists, option_value, optional_option_value, run_cmd_owned, run_shell, shell_escape,
};
use crate::Result;

pub(crate) fn image(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(kind) = args.first() else {
        return Err("image command needs a kind: cpio, initramfs, or ext4".into());
    };
    let profile = Profile::parse(&option_value(&args[1..], "--profile")?)?;
    match (kind.as_str(), profile) {
        ("cpio" | "initramfs", Profile::Busybox) => image_cpio_busybox(root),
        ("ext4", Profile::Busybox) => image_ext4_busybox(root, &args[1..], "busybox-root.ext4"),
        ("m1dock-sd", Profile::Busybox) => image_ext4_busybox(root, &args[1..], "m1dock-sd.img"),
        ("cpio" | "initramfs" | "ext4" | "m1dock-sd", Profile::Smoke) => {
            Err("image smoke profile is not defined; use --profile busybox".into())
        }
        (other, _) => Err(format!(
            "unknown image kind '{other}', expected cpio, ext4, or m1dock-sd"
        )),
    }
}

fn image_cpio_busybox(root: &Path) -> Result<()> {
    if !command_exists("cpio") {
        return Err("cpio is required to create the busybox initramfs".into());
    }
    let layout = prepare_busybox_rootfs(root)?;
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

fn image_ext4_busybox(root: &Path, args: &[String], output_name: &str) -> Result<()> {
    if !command_exists("mkfs.ext4") {
        return Err("mkfs.ext4 is required to create the busybox ext4 image".into());
    }
    let size = optional_option_value(args, "--size").unwrap_or_else(|| "64M".to_string());
    let layout = prepare_busybox_rootfs(root)?;
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

fn prepare_busybox_rootfs(root: &Path) -> Result<PathBuf> {
    let busybox = env::var("TX_BUSYBOX").map_err(|_| {
        "TX_BUSYBOX is not set; point it at a BusyBox binary before building images".to_string()
    })?;
    let busybox = PathBuf::from(busybox);
    if !busybox.is_file() {
        return Err(format!(
            "TX_BUSYBOX must point at a file, got {}",
            busybox.display()
        ));
    }

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
        layout.join("init"),
        "#!/bin/sh\nmount -t proc proc /proc 2>/dev/null || true\nmount -t sysfs sysfs /sys 2>/dev/null || true\n/bin/busybox --install -s /bin\nexec /bin/sh\n",
    )
    .map_err(|err| err.to_string())?;
    fs::write(
        layout.join("etc").join("inittab"),
        "::sysinit:/etc/init.d/rcS\n",
    )
    .map_err(|err| err.to_string())?;

    #[cfg(unix)]
    {
        for (name, target) in [
            ("sh", "busybox"),
            ("mount", "busybox"),
            ("cat", "busybox"),
            ("ls", "busybox"),
            ("echo", "busybox"),
        ] {
            let path = layout.join("bin").join(name);
            if path.exists() {
                fs::remove_file(&path).map_err(|err| err.to_string())?;
            }
            unix_fs::symlink(target, path).map_err(|err| err.to_string())?;
        }
    }

    Ok(layout)
}
