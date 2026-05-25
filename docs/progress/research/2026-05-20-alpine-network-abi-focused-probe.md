# 2026-05-20 Alpine network ABI focused probe

## Context

After real Alpine `nft`/`iptables-nft` and `/proc`/`/sys` visibility worked,
the next network-only stage was to run real Alpine networking tools through a
Docker-shaped bridge/veth/NAT control-plane flow. The goal was not broader
OpenRC or POSIX compatibility; it was to close concrete network ABI gaps
observed from Alpine `iproute2`, `nft`, and `iptables-nft`.

## Findings

- Alpine `iproute2` uses `recvmsg(MSG_PEEK|MSG_TRUNC)` with a zero-length iov
  to size netlink datagrams. Returning `0` here makes libnetlink report
  `EOF on netlink` and discard the dump even though the kernel has queued
  `RTM_NEWLINK` messages.
- Real `ip link set <dev> up` uses non-create `RTM_NEWLINK` for link flag
  updates. Treating every `RTM_NEWLINK` as a create request rejects the command
  with `EINVAL`.
- Real `ip addr add 172.17.0.1/16 dev docker0` does a single-object
  `RTM_GETLINK` lookup by name before sending `RTM_NEWADDR`. Returning a full
  dump for that single lookup lets userspace consume the first link (`lo`) as
  the answer for `docker0`, so the following address add targets ifindex 1 and
  fails.
- `/proc/net/dev` must project namespace links, not only configured
  `EtherIface` runtime objects. Otherwise loopback and newly-created bridge/veth
  links are invisible until they have IP-backed runtime state.

## Changes

- Added `tools/shell-tests/alpine-network-abi-focused-probe.txt`, a real Alpine
  probe covering:
  - `ip link`, `ip addr`, `ip route`, `ip neigh`;
  - `/proc/net/dev`, `/proc/net/route`, `/proc/net/snmp`;
  - `/sys/class/net` loopback and dynamic link visibility;
  - `nft list ruleset`;
  - Docker-shaped `docker0` bridge creation, bridge address assignment, veth
    pair creation, veth master attachment, veth up, route/proc/sysfs
    projection, and `iptables-nft` MASQUERADE.
- Netlink route receive now reports the full datagram length for
  `MSG_TRUNC`, including zero-iov peek sizing, while copying only the bytes the
  caller provided room for. This applies to both `NETLINK_ROUTE` and
  `NETLINK_NETFILTER`.
- `recvfrom(2)` / `recvmsg(2)` netlink shims now preserve Linux-style
  `MSG_TRUNC` return semantics without slicing past the staging buffer.
- `RTM_NEWLINK` without `NLM_F_CREATE` now reuses the existing setlink path.
- Single-object `RTM_GETLINK` by ifname/ifindex now returns only the requested
  `RTM_NEWLINK`, while dump requests still return multipart link dumps with
  `NLMSG_DONE`.
- `/proc/net/dev` now derives its device rows from `NetNamespacePayload`
  `link_snapshot()`, overlaying `EtherIface` stats when available and otherwise
  reporting zero counters for loopback or links without runtime iface state.

## Verification

- `cargo fmt --check`
- `git diff --check`
- `cargo test -p tx-shims netlink -- --test-threads=1`
- `cargo test -p tx-subsystems rtnetlink_ -- --test-threads=1`
- `cargo test -p tx-fs procfs_net_ -- --test-threads=1`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-network-abi-focused-probe.txt`

## Next

The Docker default bridge control-plane shape now runs under real Alpine tools.
The next network-only step should expand the focused probe toward OpenRC
network startup or Docker daemon control-plane setup and keep using the same
classification rule: fix rtnetlink/nfnetlink/socket/ioctl/procfs/sysfs/netns
network gaps, but record loader, shell, generic service-manager, cgroup, mount,
tty, and non-network permission gaps as non-network blockers.
