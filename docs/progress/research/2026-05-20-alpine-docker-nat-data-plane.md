# 2026-05-20 Alpine Docker NAT data-plane probe

## Context

N73D proved that real Alpine can ping across two network namespaces behind a
Docker-shaped bridge. N73E moved to the next Docker-default data-plane shape:
a container namespace behind `docker0`, a host uplink, `ip_forward=1`, and
MASQUERADE installed by real Alpine `iptables-nft`.

The probe is intentionally self-contained. Instead of requiring a physical or
virtio NIC, it creates a gateway namespace connected to host `uplink0` through
a veth pair.

## Change

Added `tools/shell-tests/alpine-docker-nat-data-plane-focused-probe.txt`.

The probe:

- starts a container holder namespace and a gateway holder namespace
- creates host `docker0` at `172.17.0.1/16`
- moves container `eth0` to `172.17.0.2/16`
- creates host `uplink0` at `10.0.2.15/24`
- moves gateway `gw0` to `10.0.2.2/24`
- adds the host default route via `10.0.2.2 dev uplink0`
- enables `/proc/sys/net/ipv4/ip_forward`
- installs `iptables -t nat -A POSTROUTING -s 172.17.0.0/16 -j MASQUERADE`
- verifies `iptables -t nat -S` and `nft list ruleset`
- pings `10.0.2.2` from the container namespace
- checks `/proc/net/nf_conntrack`
- checks container and gateway neighbor state with real Alpine `ip neigh`

The first run exposed a real txKernel network gap: forwarded IPv4 packets that
hit egress ARP resolution were consumed. Socket-originated packets already had
their own retry path, but namespace forwarding did not keep a pending packet
after `PacketTxResult::PendingResolution`.

`NetNamespacePayload` now has a bounded pending IPv4 forwarding queue. The
forwarding path enqueues packets only after route selection, FORWARD hook, and
postrouting/NAT have succeeded. The namespace runtime retries that queued
egress packet after ARP flush/learn processing.

Added
`namespace_runtime_retries_masqueraded_forward_after_uplink_arp_resolution` to
cover the no-preinstalled-ARP path at host-test level.

## Verification

Observed the original failure:

- `cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-docker-nat-data-plane-focused-probe.txt`

The failing symptom was:

- all control-plane setup passed
- `ip_forward=1` was visible
- `iptables-nft` and `nft list ruleset` showed MASQUERADE
- container `ping -c 1 10.0.2.2` timed out

After the forwarding pending-ARP fix:

- `cargo fmt --check`
- `cargo test -p tx-subsystems namespace_runtime_masquerades_icmp_and_conntrack_dnat_reply -- --test-threads=1`
- `cargo test -p tx-subsystems namespace_runtime_retries_masqueraded_forward_after_uplink_arp_resolution -- --test-threads=1`
- `cargo test -p tx-subsystems namespace_runtime_ -- --test-threads=1`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-docker-nat-data-plane-focused-probe.txt`

## Findings

The Alpine NAT data-plane path now passes:

- container namespace `ping -c 1 10.0.2.2` succeeds
- `/proc/net/nf_conntrack` shows an ICMP MASQUERADE entry
- the conntrack entry records `original=172.17.0.2`
- the conntrack entry records `translated=10.0.2.15`
- container `ip neigh` sees `172.17.0.1`
- gateway `ip neigh` sees `10.0.2.15`

This validates forwarding, egress ARP retry, MASQUERADE, conntrack reverse
translation, veth, bridge, network namespace routing, `/proc/sys/net`, and
`/proc/net/nf_conntrack` projection in one real Alpine path.

## Next

Move toward Docker daemon adjacent probes while keeping scope network-only:

- Docker bridge and NAT state visibility through procfs/sysfs/netlink
- AF_PACKET behavior if Docker or helper tools request packet sockets
- network ioctl coverage such as `SIOCGIF*` and `SIOCSIFFLAGS`
- capability checks for `CAP_NET_ADMIN` and `CAP_NET_RAW`
- route/neigh dump compatibility beyond the focused probes
- conntrack projection if real tools need more than the current minimal text

Continue recording OpenRC service-manager, generic shell, mount, cgroup, tty,
or non-network process/runtime failures as non-network blockers.
