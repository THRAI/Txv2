use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Result;

pub(crate) const RV64_TARGET: &str = "riscv64gc-unknown-none-elf";
pub(crate) const LA64_TARGET_PREFERRED: &str = "loongarch64-unknown-none-softfloat";
pub(crate) const LA64_TARGET_FALLBACK: &str = "loongarch64-unknown-none";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TxTarget {
    Rv64Qemu,
    Rv64M1DockMock,
    La64Qemu,
}

impl TxTarget {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "rv64-qemu" => Ok(Self::Rv64Qemu),
            "rv64-m1dock-mock" => Ok(Self::Rv64M1DockMock),
            "la64-qemu" => Ok(Self::La64Qemu),
            other => Err(format!(
                "unknown target '{other}', expected rv64-qemu, rv64-m1dock-mock, la64-qemu, or all"
            )),
        }
    }

    pub(crate) fn all_for(value: &str) -> Result<Vec<Self>> {
        if value == "all" {
            Ok(vec![Self::Rv64Qemu, Self::Rv64M1DockMock, Self::La64Qemu])
        } else {
            Ok(vec![Self::parse(value)?])
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Rv64Qemu => "rv64-qemu",
            Self::Rv64M1DockMock => "rv64-m1dock-mock",
            Self::La64Qemu => "la64-qemu",
        }
    }

    pub(crate) fn package(self) -> &'static str {
        match self {
            Self::Rv64Qemu => "tx-kernel-riscv64-qemu-virt",
            Self::Rv64M1DockMock => "tx-kernel-riscv64-m1dock-mock",
            Self::La64Qemu => "tx-kernel-loongarch64-qemu-virt",
        }
    }

    pub(crate) fn board_name(self) -> &'static str {
        match self {
            Self::Rv64Qemu => "qemu-riscv64-virt",
            Self::Rv64M1DockMock => "sipeed-m1-dock-mock",
            Self::La64Qemu => "qemu-loongarch64-virt",
        }
    }

    pub(crate) fn qemu_binary(self) -> &'static str {
        match self {
            Self::Rv64Qemu | Self::Rv64M1DockMock => "qemu-system-riscv64",
            Self::La64Qemu => "qemu-system-loongarch64",
        }
    }

    pub(crate) fn qemu_machine(self) -> &'static str {
        match self {
            Self::Rv64Qemu | Self::Rv64M1DockMock => "virt",
            Self::La64Qemu => "virt",
        }
    }

    pub(crate) fn kernel_path(self, root: &Path) -> PathBuf {
        root.join("target")
            .join(target_triple(self).unwrap_or_else(|_| match self {
                Self::Rv64Qemu | Self::Rv64M1DockMock => RV64_TARGET.to_string(),
                Self::La64Qemu => LA64_TARGET_PREFERRED.to_string(),
            }))
            .join("debug")
            .join(self.package())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Profile {
    Smoke,
    Busybox,
}

impl Profile {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "smoke" => Ok(Self::Smoke),
            "busybox" => Ok(Self::Busybox),
            other => Err(format!(
                "unknown profile '{other}', expected smoke or busybox"
            )),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::Busybox => "busybox",
        }
    }
}

pub(crate) fn target_triple(target: TxTarget) -> Result<String> {
    match target {
        TxTarget::Rv64Qemu | TxTarget::Rv64M1DockMock => Ok(RV64_TARGET.to_string()),
        TxTarget::La64Qemu => {
            let installed = installed_targets().ok();
            let supported = supported_targets()?;
            select_la64_target(installed.as_ref(), &supported)
        }
    }
}

fn select_la64_target(
    installed: Option<&BTreeSet<String>>,
    supported: &BTreeSet<String>,
) -> Result<String> {
    if let Some(installed) = installed {
        if installed.contains(LA64_TARGET_PREFERRED) {
            return Ok(LA64_TARGET_PREFERRED.to_string());
        }
        if installed.contains(LA64_TARGET_FALLBACK) {
            return Ok(LA64_TARGET_FALLBACK.to_string());
        }
    }

    if supported.contains(LA64_TARGET_PREFERRED) {
        Ok(LA64_TARGET_PREFERRED.to_string())
    } else if supported.contains(LA64_TARGET_FALLBACK) {
        Ok(LA64_TARGET_FALLBACK.to_string())
    } else {
        Err(format!(
            "compiler supports neither {LA64_TARGET_PREFERRED} nor {LA64_TARGET_FALLBACK}"
        ))
    }
}

fn supported_targets() -> Result<BTreeSet<String>> {
    let output = Command::new("rustc")
        .args(["--print", "target-list"])
        .output()
        .map_err(|err| format!("failed to run rustc --print target-list: {err}"))?;
    if !output.status.success() {
        return Err("rustc --print target-list failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

pub(crate) fn installed_targets() -> Result<BTreeSet<String>> {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map_err(|err| format!("failed to run rustup target list --installed: {err}"))?;
    if !output.status.success() {
        return Err("rustup target list --installed failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

pub(crate) fn installed_components() -> Result<Vec<String>> {
    let output = Command::new("rustup")
        .args(["component", "list", "--installed"])
        .output()
        .map_err(|err| format!("failed to run rustup component list --installed: {err}"))?;
    if !output.status.success() {
        return Err("rustup component list --installed failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

pub(crate) fn require_target(
    installed: &BTreeSet<String>,
    target: &str,
    missing: &mut Vec<String>,
) {
    if installed.contains(target) {
        println!("ok: rust target {target}");
    } else {
        println!("missing: rustup target add {target}");
        missing.push(format!("rustup target add {target}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn la64_target_prefers_installed_fallback_over_uninstalled_softfloat() {
        let installed = set(&[LA64_TARGET_FALLBACK]);
        let supported = set(&[LA64_TARGET_PREFERRED, LA64_TARGET_FALLBACK]);

        assert_eq!(
            select_la64_target(Some(&installed), &supported).unwrap(),
            LA64_TARGET_FALLBACK
        );
    }

    #[test]
    fn la64_target_uses_preferred_when_both_are_installed() {
        let installed = set(&[LA64_TARGET_PREFERRED, LA64_TARGET_FALLBACK]);
        let supported = set(&[LA64_TARGET_PREFERRED, LA64_TARGET_FALLBACK]);

        assert_eq!(
            select_la64_target(Some(&installed), &supported).unwrap(),
            LA64_TARGET_PREFERRED
        );
    }

    #[test]
    fn la64_target_falls_back_to_supported_targets_when_nothing_installed() {
        let installed = BTreeSet::new();
        let supported = set(&[LA64_TARGET_PREFERRED, LA64_TARGET_FALLBACK]);

        assert_eq!(
            select_la64_target(Some(&installed), &supported).unwrap(),
            LA64_TARGET_PREFERRED
        );
    }
}
