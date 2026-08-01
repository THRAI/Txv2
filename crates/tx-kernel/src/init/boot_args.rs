//! Boot command-line parsing.
//!
//! This module turns the firmware command line into typed facts. It does not
//! decide boot policy beyond parsing legacy aliases; `boot_plan` owns the
//! policy choices that compose these facts into kernel actions.

use tx_hal::TxPlatform;
use tx_subsystems::tty::structure::Winsize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BootMode {
    Normal,
    Smoke,
    Busybox,
    Alpine,
    Contest,
    Oscomp,
    Ltp,
    Test,
}

impl BootMode {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Smoke => "smoke",
            Self::Busybox => "busybox",
            Self::Alpine => "alpine",
            Self::Contest => "contest",
            Self::Oscomp => "oscomp",
            Self::Ltp => "ltp",
            Self::Test => "test",
        }
    }

    pub(super) const fn uses_kernel_rootfs_shims(self) -> bool {
        match self {
            Self::Normal | Self::Smoke | Self::Busybox | Self::Alpine | Self::Contest => false,
            Self::Oscomp | Self::Ltp | Self::Test => true,
        }
    }

    pub(super) const fn uses_oscomp_sdcard_entry(self) -> bool {
        matches!(self, Self::Oscomp | Self::Ltp)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InitSpec {
    pub(super) path: &'static [u8],
    pub(super) argv0: &'static [u8],
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BootArgs {
    pub(super) mode: BootMode,
    pub(super) init: InitSpec,
    pub(super) envp: &'static [&'static [u8]],
    pub(super) tty_winsize: Option<Winsize>,
    pub(super) test_init_requested: bool,
    pub(super) mount_sdcard: bool,
}

impl BootArgs {
    pub(super) fn read<P: TxPlatform>() -> Self {
        Self::from_cmdline(<P as tx_hal::BootInfoIf>::boot_info().cmdline)
    }

    fn from_cmdline(cmdline: Option<&'static str>) -> Self {
        let mode = boot_mode_from_cmdline_str(cmdline);
        let init = parse_init_from_cmdline_opt(cmdline);
        let envp = cmdline
            .map(bootstrap_envp_from_cmdline_str)
            .unwrap_or(&[b"PATH=/bin"]);
        let tty_winsize = cmdline.and_then(tty_winsize_from_cmdline_str);
        let test_init_requested = cmdline
            .map(test_init_requested_from_cmdline_str)
            .unwrap_or(false);
        let mount_sdcard = cmdline_value_str(cmdline.unwrap_or(""), "tx.mount.sdcard") != Some("0");

        Self {
            mode,
            init,
            envp,
            tty_winsize,
            test_init_requested,
            mount_sdcard,
        }
    }
}

pub(super) fn boot_mode_from_cmdline_str(cmdline: Option<&str>) -> BootMode {
    let Some(cmdline) = cmdline else {
        return BootMode::Normal;
    };
    if let Some(mode) = cmdline_value_str(cmdline, "tx.boot.mode").and_then(parse_boot_mode_value) {
        return mode;
    }
    if cmdline_value_str(cmdline, "tx.oscomp.groups").is_some()
        || cmdline_value_str(cmdline, "tx.oscomp").is_some()
    {
        return BootMode::Oscomp;
    }
    if cmdline_has_profile_str(cmdline, "alpine") {
        return BootMode::Alpine;
    }
    if cmdline_has_profile_str(cmdline, "busybox") {
        return BootMode::Busybox;
    }
    if cmdline_has_profile_str(cmdline, "smoke") {
        return BootMode::Smoke;
    }
    BootMode::Normal
}

fn parse_boot_mode_value(value: &str) -> Option<BootMode> {
    match value {
        "normal" | "linux" | "linux-like" | "user" | "userland" => Some(BootMode::Normal),
        "smoke" => Some(BootMode::Smoke),
        "busybox" => Some(BootMode::Busybox),
        "alpine" => Some(BootMode::Alpine),
        "contest" | "competition" => Some(BootMode::Contest),
        "oscomp" => Some(BootMode::Oscomp),
        "ltp" => Some(BootMode::Ltp),
        "test" | "compat" | "shim" | "shims" => Some(BootMode::Test),
        _ => None,
    }
}

fn test_init_requested_from_cmdline_str(cmdline: &str) -> bool {
    if matches!(
        cmdline_value_str(cmdline, "tx.test_init"),
        Some("1" | "true" | "yes" | "on")
    ) {
        return true;
    }
    matches!(cmdline_value_str(cmdline, "init"), Some("/tx-test-init"))
}

fn parse_init_from_cmdline_opt(cmdline: Option<&'static str>) -> InitSpec {
    let Some(cmdline) = cmdline else {
        return InitSpec {
            path: b"/init",
            argv0: b"init",
        };
    };
    parse_init_from_cmdline_str(cmdline)
}

fn parse_init_from_cmdline_str(cmdline: &'static str) -> InitSpec {
    for token in cmdline.split_ascii_whitespace() {
        if let Some(path) = token.strip_prefix("init=") {
            let argv0 = match path.rfind('/') {
                Some(idx) => &path[idx + 1..],
                None => path,
            };
            if path == "/bin/tx-bootstrap-busybox" {
                return InitSpec {
                    path: path.as_bytes(),
                    argv0: b"sh",
                };
            }
            return InitSpec {
                path: path.as_bytes(),
                argv0: argv0.as_bytes(),
            };
        }
    }
    if cmdline
        .split_ascii_whitespace()
        .any(|t| t == "tx.profile=busybox")
    {
        return InitSpec {
            path: b"/bin/busybox",
            argv0: b"sh",
        };
    }
    InitSpec {
        path: b"/init",
        argv0: b"init",
    }
}

fn bootstrap_envp_from_cmdline_str(cmdline: &str) -> &'static [&'static [u8]] {
    if cmdline_has_profile_str(cmdline, "alpine") {
        return &[b"PATH=/bin:/usr/bin:/sbin:/usr/sbin"];
    }
    &[b"PATH=/bin"]
}

fn tty_winsize_from_cmdline_str(cmdline: &str) -> Option<Winsize> {
    let rows = cmdline_value_str(cmdline, "tx.tty.rows")?
        .parse::<u16>()
        .ok()?;
    let cols = cmdline_value_str(cmdline, "tx.tty.cols")?
        .parse::<u16>()
        .ok()?;
    if rows == 0 || cols == 0 {
        return None;
    }
    Some(Winsize::new(rows, cols))
}

fn cmdline_has_profile_str(cmdline: &str, profile: &str) -> bool {
    cmdline
        .split_ascii_whitespace()
        .any(|token| token.strip_prefix("tx.profile=") == Some(profile))
}

fn cmdline_value_str<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    for token in cmdline.split_ascii_whitespace() {
        let Some((token_key, value)) = token.split_once('=') else {
            continue;
        };
        if token_key == key && !value.is_empty() {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        boot_mode_from_cmdline_str, bootstrap_envp_from_cmdline_str, cmdline_has_profile_str,
        cmdline_value_str, parse_init_from_cmdline_str, test_init_requested_from_cmdline_str,
        tty_winsize_from_cmdline_str, BootMode,
    };
    use tx_subsystems::tty::structure::Winsize;

    #[test]
    fn alpine_bootstrap_busybox_uses_shell_argv0() {
        let init = parse_init_from_cmdline_str(
            "tx.profile=alpine init=/bin/tx-bootstrap-busybox console=ttyS0",
        );

        assert_eq!(init.path, b"/bin/tx-bootstrap-busybox");
        assert_eq!(init.argv0, b"sh");
    }

    #[test]
    fn explicit_init_uses_basename_as_argv0() {
        let init = parse_init_from_cmdline_str("init=/sbin/init console=ttyS0");

        assert_eq!(init.path, b"/sbin/init");
        assert_eq!(init.argv0, b"init");
    }

    #[test]
    fn cmdline_profile_match_is_token_exact() {
        assert!(cmdline_has_profile_str(
            "tx.profile=alpine init=/bin/tx-bootstrap-busybox",
            "alpine"
        ));
        assert!(!cmdline_has_profile_str(
            "tx.profile=alpine-extra",
            "alpine"
        ));
        assert!(!cmdline_has_profile_str("foo=tx.profile=alpine", "alpine"));
    }

    #[test]
    fn alpine_bootstrap_env_includes_usr_paths() {
        assert_eq!(
            bootstrap_envp_from_cmdline_str("tx.profile=alpine init=/bin/tx-bootstrap-busybox"),
            &[b"PATH=/bin:/usr/bin:/sbin:/usr/sbin"]
        );
    }

    #[test]
    fn default_bootstrap_env_keeps_bin_only_path() {
        assert_eq!(
            bootstrap_envp_from_cmdline_str("tx.profile=busybox"),
            &[b"PATH=/bin"]
        );
    }

    #[test]
    fn tty_winsize_cmdline_requires_positive_rows_and_cols() {
        assert_eq!(
            tty_winsize_from_cmdline_str("tx.profile=alpine tx.tty.rows=33 tx.tty.cols=101"),
            Some(Winsize::new(33, 101))
        );
        assert_eq!(
            tty_winsize_from_cmdline_str("tx.tty.rows=0 tx.tty.cols=101"),
            None
        );
        assert_eq!(tty_winsize_from_cmdline_str("tx.tty.rows=33"), None);
        assert_eq!(
            tty_winsize_from_cmdline_str("tx.tty.rows=nope tx.tty.cols=101"),
            None
        );
    }

    #[test]
    fn cmdline_value_str_matches_exact_key_and_value() {
        assert_eq!(
            cmdline_value_str("tx.mount.sdcard=0 tx.profile=alpine", "tx.mount.sdcard"),
            Some("0")
        );
        assert_eq!(
            cmdline_value_str("foo.tx.mount.sdcard=0", "tx.mount.sdcard"),
            None
        );
    }

    #[test]
    fn explicit_boot_mode_overrides_profile_for_startup_policy() {
        assert_eq!(
            boot_mode_from_cmdline_str(Some("tx.profile=alpine tx.boot.mode=oscomp")),
            BootMode::Oscomp
        );
        assert_eq!(
            boot_mode_from_cmdline_str(Some("tx.profile=busybox tx.boot.mode=contest")),
            BootMode::Contest
        );
    }

    #[test]
    fn oscomp_cmdline_selects_compat_mode_without_profile() {
        assert_eq!(
            boot_mode_from_cmdline_str(Some("tx.oscomp.groups=libctest-musl console=ttyS0")),
            BootMode::Oscomp
        );
        assert_eq!(
            boot_mode_from_cmdline_str(Some("tx.oscomp=libcbench-musl console=ttyS0")),
            BootMode::Oscomp
        );
    }

    #[test]
    fn test_init_is_explicit_opt_in() {
        assert!(test_init_requested_from_cmdline_str(
            "tx.boot.mode=oscomp init=/tx-test-init console=ttyS0"
        ));
        assert!(test_init_requested_from_cmdline_str(
            "tx.boot.mode=test tx.test_init=1 console=ttyS0"
        ));
        assert!(!test_init_requested_from_cmdline_str(
            "tx.boot.mode=oscomp init=/sbin/init console=ttyS0"
        ));
    }
}
