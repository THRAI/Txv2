# 2026-05-20 Alpine iptables-nft ABI and sysfs/procfs visibility

## Context

After N72T, real Alpine RV64 `nft` could create/list/delete a simple NAT
ruleset, but real Alpine `iptables v1.8.11 (nf_tables)` still exposed more
nf_tables ABI shape than the local `/bin/nft-probe` and the initial nft
smoke. N72W also needed the Linux-visible network status files that OpenRC and
diagnostic tools commonly probe: `/proc/net/*` and `/sys/class/net/*`.

## Findings

- `iptables --version` calls `getsockname()` on `NETLINK_NETFILTER`; returning
  `sockaddr_nl` for netlink route/netfilter sockets is enough for the real
  Alpine frontend to print `iptables v1.8.11 (nf_tables)`.
- `iptables -L` expects single-object nft table/chain replies to be shaped as
  non-multipart single messages. If table replies are mixed into chain parsing
  or each response message is queued as a separate datagram, libnftnl can report
  `chain.c:559 reason: Result not representable` because table attrs are
  interpreted as chain attrs.
- `iptables-nft` emits xtables compat `target` expressions for MASQUERADE,
  not the native nft `masq` expression. The nfnetlink adapter now accepts and
  dumps that expression shape while preserving native nft MASQUERADE dumps.
- Real nft interface predicates need dump support for `oifname "docker0"` so
  Docker-style NAT rules survive `nft list ruleset`.
- `/sys/class/net` is best implemented as a projection filesystem, not as new
  network state. Lookup and reads derive from `NetNamespacePayload` link
  snapshots and existing `EtherIface` stats.

## Changes

- Netlink `getsockname()` now returns `sockaddr_nl` for `NETLINK_ROUTE` and
  `NETLINK_NETFILTER`.
- nfnetlink now:
  - concatenates response messages from one send into one queued netlink
    datagram;
  - distinguishes single GETTABLE/GETCHAIN/GETRULE responses from dump
    responses;
  - exposes default `filter` and `nat` compat tables/chains;
  - handles the minimal `NFNL_SUBSYS_NFT_COMPAT` revision probe;
  - parses and dumps xtables `target` MASQUERADE expressions;
  - renders `meta` + `cmp` interface-name matches for nft dumps.
- procfs now includes minimal `/proc/net/tcp`, `/proc/net/udp`,
  `/proc/net/raw`, `/proc/net/snmp`, `/proc/net/netlink`, and
  `/proc/net/if_inet6`.
- sysfs now mounts at `/sys` during boot, supports `mount -t sysfs`, and
  projects `/sys/class/net/<ifname>/{ifindex,address,broadcast,mtu,flags,
  operstate,carrier,statistics/*}`.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-fs sysfs_ -- --test-threads=1`
- `cargo test -p tx-fs procfs_net_ -- --test-threads=1`
- `cargo test -p tx-subsystems nfnetlink_ -- --test-threads=1`
- `cargo test -p tx-shims netlink_netfilter -- --test-threads=1`
- `cargo test -p tx-shims dispatch_netlink_netfilter_getsockname_returns_sockaddr_nl`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-proc-net-sysfs-smoke.txt`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-iptables-nft-nat-smoke.txt`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-nft-oifname-masquerade.txt`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-nft-iptables-smoke.txt`
- `TX_ALPINE_ROOTFS=target/rootfs/alpine-rv64-qemu cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-nft-nat-ruleset.txt`

## Next

N72W leaves deeper sysfs parity for later. The next network phase should move
toward OpenRC service startup and real Docker control-plane probes: add the
next missing socket options/ioctls only when a real Alpine command exposes
them, and keep the authoritative state in `NetNamespacePayload`, netfilter, and
the existing device/runtime structures.
