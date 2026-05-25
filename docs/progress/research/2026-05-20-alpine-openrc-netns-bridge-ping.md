# 2026-05-20 Alpine OpenRC netns bridge ping probe

## Context

N73C proved that real Alpine `iproute2` can drive the network namespace
control plane under the OpenRC-enabled Alpine initramfs. N73D extends that
same shape into packet-carrying behavior so the Docker-default bridge track
has a real Alpine data-plane sentinel.

This stage stays network-scoped. It does not expand OpenRC service-manager,
shell, mount, cgroup, tty, or generic process behavior.

## Change

Added `tools/shell-tests/alpine-netns-veth-bridge-ping-focused-probe.txt`.

The probe:

- starts two `/bin/netns-helper hold` processes in fresh network namespaces
- creates host `docker0` and assigns `172.17.0.1/16`
- creates `veth0 <-> eth0` and moves `eth0` into namespace A
- creates `veth1 <-> eth0` and moves `eth0` into namespace B
- attaches both host-side veth devices to `docker0`
- configures namespace A as `172.17.0.2/16`
- configures namespace B as `172.17.0.3/16`
- installs default routes through `172.17.0.1`
- runs Alpine `/bin/ping` from namespace A to the host gateway and namespace B
- checks namespace A neighbor state with real Alpine `ip neigh`

## Verification

Ran the new QEMU shell probe:

- `cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-netns-veth-bridge-ping-focused-probe.txt`

Ran the matching host runtime tests:

- `cargo test -p tx-subsystems namespace_runtime_drives_container_ping -- --test-threads=1`

## Findings

The full data-plane probe passed under real Alpine:

- namespace A pinged host `docker0` at `172.17.0.1`
- namespace A pinged namespace B at `172.17.0.3`
- namespace A `ip neigh` reported both `172.17.0.1` and `172.17.0.3`
  reachable on `eth0`

No new txKernel network ABI gap was observed. In particular, this stage did
not require CAP_NET_RAW, raw ICMP, neighbor, bridge forwarding, or namespace
data-plane changes.

## Next

Move to the Docker-default forwarding/NAT data-plane shape under real Alpine:

- enable `/proc/sys/net/ipv4/ip_forward`
- create a container namespace behind `docker0`
- create an uplink veth or routed peer outside the bridge subnet
- add a default route from the container through the bridge gateway
- install MASQUERADE with Alpine `iptables-nft`
- drive ICMP or UDP traffic through the forwarding/NAT path
- inspect `nft list ruleset`, conntrack state if exposed, route state, and
  neighbor state

Only fix failures that are network ABI work: forwarding, NAT, conntrack,
netfilter, route, neighbor/ARP, bridge, veth, namespace, AF_PACKET, raw ICMP,
or network procfs/sysfs projection. Keep generic userspace/runtime blockers
recorded separately.
