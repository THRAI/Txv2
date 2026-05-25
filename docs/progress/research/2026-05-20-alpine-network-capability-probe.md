# 2026-05-20 Alpine network capability probe

## Context

The network track explicitly includes `CAP_NET_ADMIN` and `CAP_NET_RAW`.
`CAP_NET_ADMIN` was already enforced on rtnetlink/nfnetlink/ioctl mutation
paths. The remaining network-specific gap was raw socket creation:
`AF_PACKET` and `AF_INET/SOCK_RAW` should require `CAP_NET_RAW`.

## Change

Added `Capability::NET_RAW` and `require_net_raw`.

The Linux socket shim now pre-validates the requested socket type and requires
`CAP_NET_RAW` for:

- all `AF_PACKET` sockets
- `AF_INET` `SOCK_RAW`

It does not require `CAP_NET_RAW` for ordinary `AF_INET/SOCK_DGRAM` UDP.

Added host coverage:

- `dispatch_unprivileged_socket_denies_net_raw_families`

The test clears a process's capabilities through test support and proves:

- `socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL))` returns `EPERM`
- `socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` returns `EPERM`
- `socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)` still succeeds

Added `tools/user/netcap-probe.c`, installed as `/bin/netcap-probe` for Alpine
images, and `tools/shell-tests/alpine-network-capability-focused-probe.txt`.

The real Alpine probe verifies:

- root can run `/bin/packet-probe lo`
- root can create `capadmin0` with `ip link add name capadmin0 type bridge`
- root can open AF_PACKET and raw ICMP sockets through `/bin/netcap-probe`
- after `setresuid(1000,1000,1000)`, the helper reports the current generic
  capability lifecycle behavior

## Verification

- `cargo fmt --check`
- `cargo test -p tx-shims dispatch_unprivileged_socket_denies_net_raw_families -- --test-threads=1`
- `cargo test -p tx-shims dispatch_packet_bind_getsockname_and_ioctl_round_trip_sockaddr_ll -- --test-threads=1`
- `riscv64-linux-gnu-gcc -nostdlib -static -ffreestanding -fno-builtin -fno-stack-protector -O2 -Wall -Wextra tools/user/netcap-probe.c -o /tmp/netcap-probe-riscv64`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-openrc-rv64-qemu cargo xtask image cpio --profile alpine --target rv64-qemu`
- staged `/bin/netcap-probe` ELF check
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-openrc-rv64-qemu cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-network-capability-focused-probe.txt`

## Findings

The network-specific enforcement is now in place at the syscall boundary:
without effective `CAP_NET_RAW`, AF_PACKET and AF_INET raw sockets are denied.

The Alpine helper observed:

- `tx-netcap-probe-root-raw-ok`
- `tx-netcap-probe-setuid-caps-preserved`
- `tx-netcap-probe-success`

That means the current process credential model preserves capabilities after a
plain `setresuid(1000,1000,1000)`. This is a generic capability lifecycle
blocker, not a network ABI blocker. Do not expand the network track into
file-capability, securebits, `prctl`, or full POSIX credential semantics just to
make the real Alpine negative case observable.

## Next

Continue with concrete Docker-adjacent network probes. If a real tool fails
because a network mutation is not checking `CAP_NET_ADMIN` or a raw socket is
not checking `CAP_NET_RAW`, fix it in the network track. If it fails because
capabilities cannot be dropped/raised like Linux, record that as a non-network
credential blocker.
