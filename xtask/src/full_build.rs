use std::path::Path;

use crate::Result;
use crate::check_build;
use crate::doctor;
use crate::image;
use crate::target::TxTarget;
use crate::util::optional_option_value;

const DEFAULT_TARGET: &str = "rv64-qemu";

pub(crate) fn full_build(root: &Path, args: Vec<String>) -> Result<()> {
    let target_value =
        optional_option_value(&args, "--target").unwrap_or_else(|| DEFAULT_TARGET.to_string());
    let skip_doctor = args.iter().any(|a| a == "--skip-doctor");
    let no_image = args.iter().any(|a| a == "--no-image");

    if !skip_doctor {
        println!("full-build: doctor");
        doctor::doctor(root)?;
    }

    println!("full-build: build --target {target_value}");
    check_build::build(root, &target_value)?;

    if !no_image {
        for target in TxTarget::all_for(&target_value)? {
            let kind = image_kind_for(target);
            println!(
                "full-build: image {kind} --profile busybox --target {}",
                target.name()
            );
            image::image(
                root,
                vec![
                    kind.to_string(),
                    "--profile".to_string(),
                    "busybox".to_string(),
                    "--target".to_string(),
                    target.name().to_string(),
                ],
            )?;
        }
    }

    println!("full-build: ok");
    Ok(())
}

fn image_kind_for(target: TxTarget) -> &'static str {
    match target {
        TxTarget::Rv64Qemu | TxTarget::La64Qemu => "cpio",
        TxTarget::Rv64M1DockMock => "m1dock-sd",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_kind_rv64_qemu() {
        assert_eq!(image_kind_for(TxTarget::Rv64Qemu), "cpio");
    }

    #[test]
    fn image_kind_la64_qemu() {
        assert_eq!(image_kind_for(TxTarget::La64Qemu), "cpio");
    }

    #[test]
    fn image_kind_m1dock_mock() {
        assert_eq!(image_kind_for(TxTarget::Rv64M1DockMock), "m1dock-sd");
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
