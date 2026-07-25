# Boot Mode Shim Split

Date: 2026-07-06

## Decision

txKernel boot now separates Linux-like user boot from compat/test boot through
`tx.boot.mode=...`.

- Linux-like modes: `normal`, `smoke`, `busybox`, `alpine`, `contest`.
- Compat/test modes: `oscomp`, `ltp`, `test`/`compat`.

Linux-like modes should do only kernel-owned startup work: platform/substrate
bring-up, rootfs/initramfs/block-media publication, required devfs/procfs/sysfs
mounts, console/fd binding, and first userspace exec. User-space files such as
`/etc/passwd`, `/etc/services`, `/boot/config-*`, `/tx-ltp/bin/*`, shebang
busybox links, and test scratch layouts belong to compat/test startup assets,
image overlays, or a Tx-owned userspace init program.

Compat/test modes may keep temporary kernel-side rootfs shims while OSComp/LTP
coverage still depends on them. Those shims are a boot-flow compatibility
surface, not semantic subsystem responsibility.

## Context

The previous startup path mixed Alpine boot with OSComp/LTP conveniences.
`CoreInit::init_substrate_if_ready` unconditionally populated rootfs shim files
after mounting `/proc`, `/sys`, `/dev`, and block devices, while the OSComp
sdcard exec path was selected by negative inference: no initrd and no `init=`
or `tx.profile=busybox`. That made normal Alpine/contest startup inherit
test-harness behavior and made it hard to align with Linux, where init or the
image owns most user-space setup.

## Consequences

- `tx.profile=alpine` defaults to `tx.boot.mode=alpine` and skips kernel-side
  rootfs shims.
- `tx.profile=busybox` / `tx.boot.mode=busybox` is also Linux-like: the
  busybox initramfs owns `/bin/busybox` and `/bin/sh`, so kernel-side
  OSComp/LTP shims must not pre-create `/bin/busybox -> /musl/musl/busybox`.
- `tx.boot.mode=contest` gives a normal image-owned boot path without mixing in
  LTP assumptions.
- `tx.oscomp.groups=...`, legacy `tx.oscomp=...`, `tx.boot.mode=oscomp`, and
  `tx.boot.mode=ltp` select the compat sdcard/test path.
- `xtask qemu --boot-mode MODE` is the supported local override for trying a
  profile image under a different startup policy.
- `xtask shell-test --boot-mode MODE` mirrors the `qemu` override so shell
  witnesses can run a profile image under normal, contest, or compat startup
  policy without hand-writing raw cmdline tokens.
- OSComp/LTP direct runners and Python helpers stamp the mode explicitly:
  `xtask oscomp qemu`, Makefile OSComp targets, `tools/oscomp-custom-run.py`,
  `tools/oscomp-observe-live.py`, and the LTP witness shell scripts no longer
  rely on a missing `init=` or a legacy `tx.oscomp=` token to select compat
  startup.
- The default local OSComp QEMU path now uses a separate Tx-owned test init
  overlay: `xtask image test-init --profile busybox --target ...` creates a
  tiny initramfs with `/tx-test-init` and BusyBox, and OSComp launchers add
  `init=/tx-test-init tx.test_init=1`. When that opt-in is present, the kernel
  skips the rootfs shim population and passes the selected suite command to the
  userspace test init, which performs test-mode setup and child reaping before
  exiting. Old no-initramfs direct boots keep the kernel shim path as an
  explicit compatibility fallback until the remaining helpers have moved.
- Boot command-line parsing and boot policy now have separate code homes:
  `init/boot_args.rs` parses typed facts from the firmware cmdline, while
  `init/boot_plan.rs` chooses `RootfsSetup` and `FirstUserspace`. `init.rs` and
  `init/exec.rs` consume those enums instead of re-deriving startup policy from
  ad hoc cmdline checks.

## Follow-Up

Continue moving the remaining compat rootfs population out of the kernel:
prefer the test init overlay or a more complete userspace init asset for
OSComp/LTP/test mode. Do not move this work into semantic subsystems.
