# 2026-05-20 Alpine OpenRC networking focused probe

## Context

After the real Alpine network ABI probe could drive `iproute2`, `nft`, and
`iptables-nft` through a Docker-shaped control-plane flow, the next question was
whether the current Alpine rootfs could start the OpenRC `networking` script
without broadening the txKernel scope into generic service-manager behavior.

This probe intentionally keeps the boundary narrow: network ABI failures are
actionable, while missing OpenRC packaging, service-manager state, generic shell
script behavior, mount/cgroup/tty setup, or other non-network prerequisites are
recorded as blockers rather than fixed in the network track.

## Probe

Added `tools/shell-tests/alpine-openrc-networking-focused-probe.txt`.

The probe checks:

- `ls /etc/init.d`
- `ls /etc/network`
- `rc-service --version`
- `/etc/init.d/networking status`
- optional network-only `/etc/network/interfaces` rewrite for `lo` and
  `docker0`, only when that file already exists
- `/etc/init.d/networking start`, only when both the script and interfaces file
  exist
- `ip link`
- `ip addr`
- `ip route`
- `cat /proc/net/dev`
- `ls /sys/class/net`

## Findings

- The current Alpine RV64 rootfs has `/etc/network` hook directories:
  `if-down.d`, `if-post-down.d`, `if-post-up.d`, `if-pre-down.d`,
  `if-pre-up.d`, and `if-up.d`.
- The rootfs does not have `/etc/init.d`.
- The rootfs does not have `rc-service`.
- The rootfs does not have `/etc/init.d/networking`.
- The rootfs does not have `/etc/network/interfaces`.
- Because the networking init script and interfaces file are absent,
  `/etc/init.d/networking start` is skipped and classified as a non-network
  OpenRC/package/service-manager blocker.
- The network-facing surface remains healthy: Alpine `ip link`, `ip addr`, and
  `ip route` run; loopback is visible with `127.0.0.1`; `/proc/net/dev` lists
  `lo`; and `/sys/class/net` lists `lo`.

## Classification

No new txKernel network ABI gap was observed in this round.

The current blocker is outside the network subsystem implementation: the Alpine
rootfs used by the `alpine` profile does not yet carry the OpenRC/netifrc files
needed to exercise `/etc/init.d/networking`. This should stay recorded as image
or package preparation work unless a later OpenRC-enabled rootfs exposes a real
network failure through rtnetlink, net ioctl, procfs/sysfs projection,
capability checks, sockets, or network namespace behavior.

## Verification

- `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-openrc-networking-focused-probe.txt`

## Next

Prepare or point the `alpine` profile at an Alpine rootfs that includes the
OpenRC/netifrc networking scripts. Then rerun this focused probe and keep the
same fix/classify boundary: fix network ABI gaps, record non-network OpenRC or
service-manager blockers.
