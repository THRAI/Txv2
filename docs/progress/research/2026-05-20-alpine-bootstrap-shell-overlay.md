# 2026-05-20 Alpine bootstrap shell overlay

## Context

After adding OpenRC to the Alpine RV64 rootfs, the staged initramfs still used a
repo-provided static BusyBox as the bootstrap shell. That was intentional for
reliable `/bin/sh` entry, but the implementation copied the static BusyBox over
`/bin/busybox`, replacing Alpine's real APK-provided BusyBox binary.

That mattered because Alpine packages install many applet symlinks that target
`/bin/busybox`, including `ifup`, `ifdown`, `sleep`, `kill`, and `timeout`.
Replacing `/bin/busybox` made those symlinks call the repo's bootstrap BusyBox
instead of the Alpine one, drifting away from the real userspace ABI probe.

## Change

`xtask/src/image.rs` now installs the repo static BusyBox as
`/bin/tx-bootstrap-busybox` and points only `/bin/sh` at that binary. Alpine's
own `/bin/busybox` is preserved from the rootfs.

The staged layout now has:

- `/bin/busybox` as Alpine's dynamic BusyBox from the APK rootfs
- `/bin/sh -> tx-bootstrap-busybox`
- `/bin/tx-bootstrap-busybox` as the repo static bootstrap shell
- `/usr/bin/timeout -> /bin/busybox`

## Findings

Preserving Alpine BusyBox keeps the image shape closer to real Alpine while
still letting txKernel enter `/bin/sh` through a known static executable.

The OpenRC focused probe still records `NO_TIMEOUT_FOR_NETWORKING_STATUS` and
`NO_TIMEOUT_FOR_NETWORKING_START`: `timeout true` is not usable enough under the
current shell-test runtime to wrap the observed-hanging OpenRC script safely.
That remains classified as a non-network coreutils/process/signal/OpenRC
service-manager blocker, not a network ABI issue.

## Verification

- `cargo test -p xtask alpine -- --test-threads=1`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-openrc-rv64-qemu cargo xtask image cpio --profile alpine --target rv64-qemu`
- staged layout check for `/bin/busybox`, `/bin/sh`,
  `/bin/tx-bootstrap-busybox`, and `/usr/bin/timeout`
- `cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-openrc-networking-focused-probe.txt`

## Next

Continue toward direct network-control probes or add a deliberately scoped
probe helper for OpenRC `status/start`. Do not expand the network track into
generic shell job control, signals, or OpenRC service-manager state unless a
later command exposes a concrete network ABI failure beyond that boundary.
