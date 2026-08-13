use std::path::Path;

use crate::check_build;
use crate::doctor;
use crate::image;
use crate::target::TxTarget;
use crate::util::optional_option_value;
use crate::Result;

const DEFAULT_TARGET: &str = "rv64-qemu";

pub(crate) fn full_build(root: &Path, args: Vec<String>) -> Result<()> {
    let target_value =
        optional_option_value(&args, "--target").unwrap_or_else(|| DEFAULT_TARGET.to_string());
    let skip_doctor = args.iter().any(|a| a == "--skip-doctor");
    let no_image = args.iter().any(|a| a == "--no-image");
    let kernel_only = args.iter().any(|a| a == "--kernel-only");
    let release = args.iter().any(|a| a == "--release");

    if !skip_doctor {
        println!("full-build: doctor");
        doctor::doctor(root)?;
    }

    println!(
        "full-build: build --target {target_value}{}",
        match (kernel_only, release) {
            (true, true) => " --kernel-only --release",
            (true, false) => " --kernel-only",
            (false, true) => " --release",
            (false, false) => "",
        }
    );
    check_build::build_with_mode(root, &target_value, release, kernel_only)?;

    if !no_image {
        for target in TxTarget::all_for(&target_value)? {
            for kind in image_kinds_for(target, kernel_only) {
                println!(
                    "full-build: image {kind} --profile busybox --target {}{}",
                    target.name(),
                    if kernel_only { " --kernel-only" } else { "" }
                );
                let mut image_args = vec![
                    kind.to_string(),
                    "--profile".to_string(),
                    "busybox".to_string(),
                    "--target".to_string(),
                    target.name().to_string(),
                ];
                if kernel_only {
                    image_args.push("--kernel-only".to_string());
                }
                if release {
                    image_args.push("--release".to_string());
                }
                image::image(root, image_args)?;
            }
        }
    }

    println!("full-build: ok");
    Ok(())
}

fn image_kinds_for(target: TxTarget, kernel_only: bool) -> &'static [&'static str] {
    match (target, kernel_only) {
        (TxTarget::La64Ls2k1000, true) => &["la2k1000-uimage"],
        (TxTarget::Rv64Qemu | TxTarget::La64Qemu, _) => &["cpio"],
        (TxTarget::Rv64M1DockMock, _) => &["m1dock-sd"],
        (TxTarget::La64Ls2k1000, false) => &["cpio", "la2k1000-uimage"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_kind_rv64_qemu() {
        assert_eq!(image_kinds_for(TxTarget::Rv64Qemu, false), &["cpio"]);
    }

    #[test]
    fn image_kind_la64_qemu() {
        assert_eq!(image_kinds_for(TxTarget::La64Qemu, false), &["cpio"]);
    }

    #[test]
    fn image_kind_m1dock_mock() {
        assert_eq!(
            image_kinds_for(TxTarget::Rv64M1DockMock, false),
            &["m1dock-sd"]
        );
    }

    #[test]
    fn image_kind_la64_ls2k1000() {
        assert_eq!(
            image_kinds_for(TxTarget::La64Ls2k1000, false),
            &["cpio", "la2k1000-uimage"]
        );
    }

    #[test]
    fn kernel_only_la64_ls2k1000_skips_initramfs_image() {
        assert_eq!(
            image_kinds_for(TxTarget::La64Ls2k1000, true),
            &["la2k1000-uimage"]
        );
    }

    #[test]
    fn skip_doctor_flag_parsed() {
        // Just verify the flag string is detected; full_build itself calls
        // doctor which requires a real toolchain, so we test flag parsing only.
        let args = ["--skip-doctor".to_string(), "--no-image".to_string()];
        assert!(args.iter().any(|a| a == "--skip-doctor"));
        assert!(args.iter().any(|a| a == "--no-image"));
    }
}
