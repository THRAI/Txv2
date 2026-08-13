//! Boot policy derived from parsed boot arguments.
//!
//! `boot_args` records what the command line says. This module records what
//! the kernel should do with it: which rootfs setup path is legal and whether
//! the OSComp sdcard command-chain entry is selected.

use super::boot_args::{BootArgs, BootMode};
use tx_hal::TxPlatform;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RootfsSetup {
    /// Linux-like/user-owned boot: the kernel publishes kernel filesystems and
    /// execs init, leaving `/etc`, scratch directories, and test helpers to
    /// the image's init.
    LinuxLike,
    /// Test-mode initramfs boot: `/tx-test-init` owns setup and child reaping.
    TestInit,
    /// Temporary compatibility fallback for old direct OSComp/LTP boots that
    /// do not carry the test-init initramfs yet.
    LegacyKernelShims,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FirstUserspace {
    /// Execute the init selected by `init=` / profile defaults.
    CmdlineInit,
    /// Prefer the OSComp/LTP sdcard suite entry when the `/musl` block mount is
    /// present. If the mount is absent or exec fails, bootstrap falls back to
    /// [`CmdlineInit`](Self::CmdlineInit).
    OscompSdcard { test_init: bool },
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BootPlan {
    pub(super) args: BootArgs,
    pub(super) rootfs_setup: RootfsSetup,
    pub(super) first_userspace: FirstUserspace,
}

impl BootPlan {
    pub(super) fn read<P: TxPlatform>() -> Self {
        let mut args = BootArgs::read::<P>();
        if super::preliminary_oscomp_media_detected() {
            args.mode = BootMode::Oscomp;
        }
        Self::from_args(args)
    }

    fn from_args(args: BootArgs) -> Self {
        let rootfs_setup = if args.test_init_requested {
            RootfsSetup::TestInit
        } else if args.mode.uses_kernel_rootfs_shims() {
            RootfsSetup::LegacyKernelShims
        } else {
            RootfsSetup::LinuxLike
        };

        let first_userspace = if args.mode.uses_oscomp_sdcard_entry() {
            FirstUserspace::OscompSdcard {
                test_init: args.test_init_requested,
            }
        } else {
            FirstUserspace::CmdlineInit
        };

        Self {
            args,
            rootfs_setup,
            first_userspace,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BootPlan, FirstUserspace, RootfsSetup};
    use crate::init::boot_args::{BootArgs, BootMode, InitSpec};

    fn args(mode: BootMode, test_init_requested: bool) -> BootArgs {
        BootArgs {
            mode,
            init: InitSpec {
                path: b"/init",
                argv0: b"init",
            },
            envp: &[b"PATH=/bin"],
            tty_winsize: None,
            test_init_requested,
            mount_sdcard: true,
        }
    }

    #[test]
    fn linux_like_modes_skip_rootfs_setup() {
        assert_eq!(
            BootPlan::from_args(args(BootMode::Busybox, false)).rootfs_setup,
            RootfsSetup::LinuxLike
        );
        assert_eq!(
            BootPlan::from_args(args(BootMode::Alpine, false)).rootfs_setup,
            RootfsSetup::LinuxLike
        );
        assert_eq!(
            BootPlan::from_args(args(BootMode::Contest, false)).rootfs_setup,
            RootfsSetup::LinuxLike
        );
    }

    #[test]
    fn compat_modes_use_legacy_shims_without_test_init() {
        let plan = BootPlan::from_args(args(BootMode::Oscomp, false));
        assert_eq!(plan.rootfs_setup, RootfsSetup::LegacyKernelShims);
        assert_eq!(
            plan.first_userspace,
            FirstUserspace::OscompSdcard { test_init: false }
        );
    }

    #[test]
    fn test_init_overrides_legacy_kernel_shims() {
        let plan = BootPlan::from_args(args(BootMode::Oscomp, true));
        assert_eq!(plan.rootfs_setup, RootfsSetup::TestInit);
        assert_eq!(
            plan.first_userspace,
            FirstUserspace::OscompSdcard { test_init: true }
        );
    }

    #[test]
    fn linux_like_first_userspace_is_cmdline_init() {
        let plan = BootPlan::from_args(args(BootMode::Alpine, false));
        assert_eq!(plan.first_userspace, FirstUserspace::CmdlineInit);
    }
}
