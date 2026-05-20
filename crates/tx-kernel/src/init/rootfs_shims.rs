// Rootfs-side shebang shims and small init-time tmpfs population.
//
// Carved out of `init.rs` to keep that file under the 1800-line
// per-file authoring cap enforced by the `arch` lint
// (`txdoc:CI-GATE-ARCH-LINT`). The single method here augments the
// `impl<P: TxPlatform> CoreInit<P>` block in `init.rs` and
// `init::exec`.
//
// The helper creates `/bin/{sh,busybox}` and `/usr/bin/env` as
// rootfs-tmpfs symlinks pointing at `/musl/musl/busybox` so the
// OSComp `libctest`, `lua`, and `libcbench` wrapper scripts find
// their shebang interpreters. See the doc-comment on
// `populate_rootfs_shebang_shims` for the full design rationale.

use super::*;
use crate::adapter::step_engine::{self as step_engine, StepOutcome};

impl<P: TxPlatform> CoreInit<P> {
    /// Populate the rootfs tmpfs with the shebang shims the
    /// OSComp `libctest`, `lua`, and `libcbench` suites' wrapper
    /// scripts expect on disk:
    ///
    /// ```text
    /// /bin/busybox    → /musl/musl/busybox  (handles `#!/bin/busybox sh …`)
    /// /bin/sh         → /musl/musl/busybox  (handles `#!/bin/sh`)
    /// /usr/bin/env    → /musl/musl/busybox  (handles `#!/usr/bin/env …`)
    /// ```
    ///
    /// The wrapper scripts (`scripts/lua/test.sh`, `run-static.sh`,
    /// `run-dynamic.sh`, …) all start with a `#!` line referencing
    /// one of these interpreter paths. Without the shims the kernel
    /// reports `not found` at `execve` time and the entire suite
    /// scores 0/N (libctest 0/220, lua 0/9 in the 2026-05-18
    /// scoreboard — see `docs/progress/SYSCALL_STATUS.md`).
    ///
    /// **Order invariant:** must run after
    /// [`Self::mount_sdcard_at_musl`] so `/musl/musl/busybox` is a
    /// reachable target (symlink resolution happens at exec time,
    /// not at symlink-creation time, so the order isn't strictly
    /// required for the symlink to succeed — but if the target's
    /// mount isn't yet attached the very first exec attempt fails,
    /// not a later one). Runs after [`Self::mount_procfs_at_proc`]
    /// so its sentinel comes first in the boot log.
    ///
    /// Failures are non-fatal — the helper logs a sentinel and
    /// returns. The kernel boots; the libctest / lua suites stay
    /// at 0/N until the shim is created.
    pub(crate) fn populate_rootfs_shebang_shims() {
        let root_mount = ROOT_MOUNT
            .lock()
            .clone()
            .expect("populate_rootfs_shebang_shims: ROOT_MOUNT must be populated");
        let rootfs_payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .clone();
        let cred = Credential::root();
        let root_fs_object_id = root_mount.root().fs_object_id();
        let fs_ops = &rootfs_payload.fs_ops;

        // /bin
        let bin_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"bin", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":shebang-shims:err:mkdir-bin\n");
                return;
            }
        };
        let _ = symlink_into(fs_ops, bin_id, b"busybox", b"/musl/musl/busybox", &cred);
        let _ = symlink_into(fs_ops, bin_id, b"sh", b"/musl/musl/busybox", &cred);

        // /usr and /usr/bin
        let usr_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"usr", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":shebang-shims:err:mkdir-usr\n");
                return;
            }
        };
        let usr_bin_id = match mkdir_or_find(fs_ops, usr_id, b"bin", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":shebang-shims:err:mkdir-usr-bin\n");
                return;
            }
        };
        let _ = symlink_into(fs_ops, usr_bin_id, b"env", b"/musl/musl/busybox", &cred);

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":shebang-shims:ok\n");
    }

    /// Populate the rootfs tmpfs with the writable scratch
    /// directories that POSIX-shaped userspace expects to exist.
    /// Today this covers `/tmp/`, `/var/`, and `/var/tmp/`.
    ///
    /// **Why this exists.** The OSComp `lmbench-musl` suite (and
    /// many libc/libctest tests) `open(O_RDWR|O_CREAT, "/var/tmp/…")`
    /// during setup. Without these directories `open` returns
    /// `-ENOENT` and the entire suite scores 0/N. The kernel does
    /// not auto-create them at boot the way Linux's initrd would —
    /// the rootfs is a fresh tmpfs.
    ///
    /// **Order invariant:** must run after
    /// [`Self::populate_rootfs_shebang_shims`] so the
    /// `:shebang-shims:ok` sentinel comes first in the boot log
    /// (purely for grep-stability — there is no functional
    /// dependency between the two helpers).
    ///
    /// Failures are non-fatal — the helper logs a sentinel and
    /// returns. The kernel boots; the affected suites stay at 0/N
    /// until the scratch dirs are populated.
    /// Seed the kernel CSPRNG from platform entropy before userspace
    /// starts.  Must run after rootfs is mounted (the CSPRNG owns no
    /// filesystem state, but the ordering convention keeps all Phase
    /// 3b boot wiring in one place).
    pub(crate) fn init_csprng() {
        let seed = tx_services::random::platform_seed();
        tx_services::random::init(&seed);
    }

    pub(crate) fn populate_rootfs_tmp_dirs() {
        let root_mount = ROOT_MOUNT
            .lock()
            .clone()
            .expect("populate_rootfs_tmp_dirs: ROOT_MOUNT must be populated");
        let rootfs_payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .clone();
        let cred = Credential::root();
        let root_fs_object_id = root_mount.root().fs_object_id();
        let fs_ops = &rootfs_payload.fs_ops;

        // /tmp (world-writable, sticky-style — the slice doesn't
        // honour the sticky bit yet so 0o777 is the practical
        // equivalent).
        if mkdir_or_find(fs_ops, root_fs_object_id, b"tmp", 0o777, &cred).is_none() {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":tmp-dirs:err:mkdir-tmp\n");
            return;
        }

        // /var and /var/tmp
        let var_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"var", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":tmp-dirs:err:mkdir-var\n");
                return;
            }
        };
        if mkdir_or_find(fs_ops, var_id, b"tmp", 0o777, &cred).is_none() {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":tmp-dirs:err:mkdir-var-tmp\n");
            return;
        }

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":tmp-dirs:ok\n");
    }
}

/// Create-or-find a directory under `parent`. Treats EEXIST as
/// "fine, look it up" rather than a hard error so a second boot in
/// the same test fixture doesn't panic.
///
/// Discipline: each FS call sits in its own scope so the `Guard` is
/// dropped before the next call acquires a new one. txKernel's
/// epoch discipline panics on nested guards
/// (`tx-substrate::epoch::local:55`).
fn mkdir_or_find(
    fs_ops: &alloc::sync::Arc<dyn tx_subsystems::vfs::FsOps>,
    parent: tx_subsystems::vfs::FsObjectId,
    name: &[u8],
    mode: u16,
    cred: &Credential,
) -> Option<tx_subsystems::vfs::FsObjectId> {
    let mkdir_outcome = {
        let guard = step_engine::guard();
        fs_ops.mkdir(parent, name, mode, cred, &guard)
    };
    match mkdir_outcome {
        StepOutcome::Done((id, _meta)) => Some(id),
        // EEXIST: look up the existing entry (e.g. previous boot
        // ran this helper). Treating it as fatal would make
        // re-boots panic.
        StepOutcome::Err(step_engine::Errno::EEXIST) => {
            let guard = step_engine::guard();
            match fs_ops.lookup(parent, name, &guard) {
                StepOutcome::Done(existing) => Some(existing),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Best-effort symlink — ignores errors so a re-boot doesn't panic
/// when the symlink is already present.
fn symlink_into(
    fs_ops: &alloc::sync::Arc<dyn tx_subsystems::vfs::FsOps>,
    parent: tx_subsystems::vfs::FsObjectId,
    name: &[u8],
    target: &[u8],
    cred: &Credential,
) -> bool {
    let guard = step_engine::guard();
    matches!(
        fs_ops.symlink(parent, name, target, cred, &guard),
        StepOutcome::Done(_)
    )
}
