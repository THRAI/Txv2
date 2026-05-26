# 2026-05-20 OpenRC-rootfs Docker control-plane probe

## Context

After preparing an Alpine rootfs with OpenRC packages and fixing the bootstrap
shell overlay so Alpine's real BusyBox remains intact, the next ordered network
stage was to confirm that the Docker-shaped network control plane still works
under that richer rootfs.

This is still not full OpenRC service startup. The OpenRC
`networking status/start` path remains blocked by non-network
shell/coreutils/process/signal/service-manager behavior and is recorded
separately.

## Verification

Ran the existing real Alpine Docker-shaped probe against the current
OpenRC-enabled Alpine initramfs:

- `cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-network-abi-focused-probe.txt`

The initramfs used for this run was built from
`target/rootfs/alpine-openrc-rv64-qemu`.

## Findings

The full focused control-plane path passed:

- `ip link`, `ip addr`, `ip route`, and `ip neigh`
- `/proc/net/dev`, `/proc/net/route`, and `/proc/net/snmp`
- `/sys/class/net` and link attributes
- `nft list ruleset`
- `iptables -t nat -S`
- `ip link add name docker0 type bridge`
- `ip link set docker0 up`
- `ip addr add 172.17.0.1/16 dev docker0`
- `ip link add veth0 type veth peer name eth0`
- `ip link set veth0 master docker0`
- `ip link set veth0 up`
- route projection for `172.17.0.0/16 dev docker0`
- `/proc/net/dev` and `/sys/class/net` projection of `docker0`, `veth0`, and
  `eth0`
- `iptables -t nat -A POSTROUTING -s 172.17.0.0/16 -j MASQUERADE`
- `iptables -t nat -S` rendering of the MASQUERADE rule
- `nft list ruleset` rendering of the same xtables MASQUERADE rule

No new txKernel network ABI gap was observed.

## Next

Move to the network namespace and capability stage:

- `unshare(CLONE_NEWNET)`
- `setns(CLONE_NEWNET)`
- `/proc/<pid>/ns/net`
- `CAP_NET_ADMIN`
- `CAP_NET_RAW`

Keep this next stage scoped to the network namespace path only. Generic process,
permission, shell, service-manager, cgroup, mount, and tty behavior should stay
recorded as non-network blockers unless a real network command requires a
specific network-facing ABI correction.
