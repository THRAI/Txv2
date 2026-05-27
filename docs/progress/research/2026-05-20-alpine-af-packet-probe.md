# 2026-05-20 Alpine AF_PACKET probe

## Context

N73E proved the Docker-default bridge/NAT data plane under real Alpine. The
next Docker-adjacent ABI surface is link-layer sockets: Docker and network
tools commonly probe AF_PACKET for interface-facing behavior even when the
kernel does not yet need a full packet-capture data path.

This stage intentionally keeps AF_PACKET minimal and network-scoped. It exposes
the Linux socket ABI that real tools need to open and bind a packet socket, but
does not create a second frame path outside the existing veth, bridge, routing,
netfilter, and namespace runtime.

## Change

Added minimal packet socket state to the network subsystem:

- `AddressFamily::Packet`
- `SocketKind::Packet`
- `SocketProtocol::Packet(PacketSocketState)`
- `SockAddrLl`
- packet `bind(sockaddr_ll)` state
- packet `getsockname(sockaddr_ll)` state
- protocol normalization from the AF_PACKET socket argument, which is passed in
  network byte order by userspace

The syscall shim now decodes and writes `sockaddr_ll` for packet sockets. It
continues to route network ioctls through the existing process network
namespace, so `SIOCGIFINDEX` sees the caller's current namespace links.

Added `tools/user/packet-probe.c`, installed by the Alpine image path as
`/bin/packet-probe` when `riscv64-linux-gnu-gcc` is available. The helper is a
freestanding static RV64 binary that:

- opens `socket(AF_PACKET, SOCK_RAW|SOCK_CLOEXEC, htons(ETH_P_ALL))`
- resolves the requested interface with `SIOCGIFINDEX`
- binds `sockaddr_ll` to that ifindex
- verifies `getsockname(sockaddr_ll)` returns AF_PACKET, ETH_P_ALL, and the
  same ifindex
- verifies empty nonblocking `recvfrom()` reports `EAGAIN`

Added `tools/shell-tests/alpine-af-packet-focused-probe.txt`.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-subsystems socket_type_validation_maps_to_kind -- --test-threads=1`
- `cargo test -p tx-shims dispatch_packet_bind_getsockname_and_ioctl_round_trip_sockaddr_ll -- --test-threads=1`
- `riscv64-linux-gnu-gcc -nostdlib -static -ffreestanding -fno-builtin -fno-stack-protector -O2 -Wall -Wextra tools/user/packet-probe.c -o /tmp/packet-probe-riscv64`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-openrc-rv64-qemu cargo xtask image cpio --profile alpine --target rv64-qemu`
- staged `/bin/packet-probe` ELF check
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-openrc-rv64-qemu cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-af-packet-focused-probe.txt`

## Findings

The real Alpine shell-test now passes packet socket probes on:

- `lo`
- a newly created `docker0` bridge
- `eth0` inside a separate network namespace after a veth peer is moved there

No broader POSIX, OpenRC, cgroup, mount, tty, or service-manager behavior was
needed. No separate AF_PACKET capture/transmit data plane was added.

## Next

Continue with Docker-daemon-adjacent network probes:

- additional network ioctl coverage if real tools request it
- route/neigh dump compatibility beyond the current focused tests
- capability behavior around `CAP_NET_ADMIN` and `CAP_NET_RAW`
- procfs/sysfs/netlink visibility gaps around Docker bridge and NAT state

Only implement AF_PACKET frame capture or transmit semantics if a real network
tool exposes that requirement.
