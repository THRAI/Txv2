# 2026-05-20 Alpine OpenRC rootfs preparation

## Context

The previous OpenRC networking probe showed that the Alpine RV64 rootfs used by
the `alpine` profile contained network hook directories, but not enough OpenRC
packaging to exercise `/etc/init.d/networking`. The next ordered stage was image
preparation rather than kernel work.

## Changes

- `tools/images/fetch-alpine-rv64.sh` now defaults to:
  `busybox openrc nftables iptables iproute2`.
- Pulling `openrc` resolves the needed OpenRC and ifupdown surface through the
  APK closure, including `busybox-ifupdown` and `openrc-user`.
- The helper now writes a default `/etc/network/interfaces` only when packages
  did not already provide one. The seeded file is intentionally minimal:
  loopback only. The focused shell-test rewrites it at runtime when it wants to
  probe a `docker0` configuration.
- `tools/shell-tests/alpine-openrc-networking-focused-probe.txt` now classifies
  missing safe timeout support before attempting OpenRC `networking
  status/start`. This avoids letting a known service-manager hang turn into a
  broken shell-test.

## Findings

Generating `target/rootfs/alpine-openrc-rv64-qemu` with the new defaults
confirmed the rootfs now contains:

- `/etc/init.d/networking`
- `/sbin/rc-service`
- `/sbin/ifup`
- `/sbin/ifdown`
- `/etc/network/interfaces`

Running the focused OpenRC probe against that image confirms the OpenRC package
surface is now present. The next blocker is not a txKernel network ABI gap:
there is no `timeout` applet in the image, and raw
`/etc/init.d/networking status` was observed to hang when run directly under the
shell-test. The probe therefore records `NO_TIMEOUT_FOR_NETWORKING_STATUS` and
`NO_TIMEOUT_FOR_NETWORKING_START` as non-network shell/coreutils/OpenRC
service-manager blockers.

The network visibility checks still pass:

- Alpine `ip link`
- Alpine `ip addr`
- Alpine `ip route`
- `/proc/net/dev`
- `/sys/class/net`

## Verification

- `TX_ALPINE_ROOTFS=target/rootfs/alpine-openrc-rv64-qemu tools/images/fetch-alpine-rv64.sh`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-openrc-rv64-qemu cargo xtask image cpio --profile alpine --target rv64-qemu`
- `cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-openrc-networking-focused-probe.txt`

## Next

The OpenRC rootfs preparation stage is complete. The next decision is whether
to add a narrow probe-only timeout provider to image preparation, or keep that
as a non-network blocker and continue with direct network-control probes that do
not depend on OpenRC service-manager state.
