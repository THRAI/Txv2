# 2026-05-20 Alpine OpenRC netns/veth bridge probe

## Context

After the OpenRC-enabled Alpine rootfs passed the Docker-shaped host namespace
control-plane probe, the next ordered network stage was to exercise the
network namespace path under the same real Alpine userspace.

The existing `tools/user/netns-helper.c` already covers the narrow network
namespace operations that shell alone cannot express reliably:

- `unshare(CLONE_NEWNET)` with a holder process
- opening `/proc/<pid>/ns/net`
- `setns(CLONE_NEWNET)` before exec
- rtnetlink fallback helpers for `IFLA_MASTER` and `IFLA_NET_NS_PID`

## Change

The Alpine image preparation path now calls `install_optional_user_smokes()`,
matching the BusyBox image path. When `riscv64-linux-gnu-gcc` is available,
the staged Alpine initramfs includes:

- `/bin/netns-helper`
- `/bin/nft-probe`
- `/bin/tcp-loopback-smoke`
- `/bin/udp-loopback-smoke`

Added `tools/shell-tests/alpine-netns-veth-bridge-focused-probe.txt` to drive
real Alpine `iproute2` through the network namespace control plane.

## Verification

Rebuilt the OpenRC-enabled Alpine initramfs:

- `TX_ALPINE_ROOTFS=target/rootfs/alpine-openrc-rv64-qemu cargo xtask image cpio --profile alpine --target rv64-qemu`

Confirmed the staged helper exists and is an RV64 static ELF:

- `ls -l target/rootfs/alpine-stage-rv64-qemu/bin/netns-helper`
- `file target/rootfs/alpine-stage-rv64-qemu/bin/netns-helper`

Ran the new QEMU shell probe:

- `cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-netns-veth-bridge-focused-probe.txt`

Ran targeted host tests:

- `cargo test -p tx-shims netns -- --test-threads=1`
- `cargo test -p tx-subsystems netns -- --test-threads=1`
- `cargo test -p xtask alpine -- --test-threads=1`

## Findings

The full focused namespace control path passed under real Alpine:

- `/bin/netns-helper` is present in the Alpine initramfs
- `netns-helper hold` creates a holder in a fresh network namespace
- `/proc/<pid>/ns/net` is visible
- `ip link add name docker0 type bridge`
- `ip addr add 172.17.0.1/16 dev docker0`
- `ip link set docker0 up`
- `ip link add veth0 type veth peer name eth0`
- `ip link set veth0 master docker0`, with helper fallback still available
- `ip link set eth0 netns <pid>`, with helper fallback still available
- `netns-helper exec <pid> ip link set eth0 up`
- `netns-helper exec <pid> ip addr add 172.17.0.2/16 dev eth0`
- host namespace `ip link` shows `docker0` and `veth0`
- target namespace `ip link` and `ip addr` show `eth0` and `172.17.0.2/16`
- target namespace default route via `172.17.0.1` is installed and rendered

No new txKernel network ABI gap was observed in this stage.

## Next

Move to the Alpine namespace data-plane stage:

- create two target network namespaces behind `docker0`
- move a veth peer into each namespace
- assign `172.17.0.2/16` and `172.17.0.3/16`
- ping the bridge host address and peer namespace address
- inspect neighbor and route state

Only fix failures that are network ABI work: `CAP_NET_RAW`, AF_PACKET, ICMP,
neighbor/ARP, bridge forwarding, rtnetlink, procfs/sysfs network projection,
or namespace behavior. Keep generic shell, process, service-manager, mount,
cgroup, and tty gaps recorded as non-network blockers.
