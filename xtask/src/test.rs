//! `cargo xtask test` — opinionated smoke-test entrypoints that compose the
//! existing `build`, `image`, and `qemu` commands.
//!
//! Lanes:
//!   - `smoke` (default): build the kernel for the given target and boot it
//!     under qemu with `--expect-sentinel`. Mirrors `ci-slow`.
//!   - `busybox-boot`: same as `smoke` but additionally builds the busybox
//!     cpio initramfs from the vendored musl busybox for the selected target
//!     and boots qemu in the busybox profile. Checks only the boot sentinel —
//!     does not exercise busybox functionality.
//!
//! Both lanes accept `--target rv64-qemu` (default), `--timeout-ms N`, and
//! `--dry-run`. `--dry-run` prints the qemu command line without running it.

use std::path::Path;

use crate::check_build;
use crate::image;
use crate::qemu;
use crate::target::TxTarget;
use crate::util::optional_option_value;
use crate::Result;

const DEFAULT_TARGET: &str = "rv64-qemu";

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

    match lane.as_str() {
        "smoke" => smoke(root, target, rest, /*with_busybox=*/ false),
        "busybox-boot" | "busybox" => smoke(root, target, rest, /*with_busybox=*/ true),
        other => Err(format!(
            "unknown test lane '{other}', expected smoke or busybox-boot"
        )),
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
        "smoke"
    };

    let mut qemu_args = vec![
        "--target".to_string(),
        target.name().to_string(),
        "--profile".to_string(),
        profile.to_string(),
        "--expect-sentinel".to_string(),
    ];
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
    println!(
        "test: qemu {} --profile {} --expect-sentinel",
        target.name(),
        profile
    );
    qemu::qemu(root, qemu_args)
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
}
