# Research: Alpine RV64 Real nft Stage-6 ABI

**Date:** 2026-05-20

## Question

Can txKernel move from the local `/bin/nft-probe` fixture to a real RV64
Alpine `nft` frontend, and what kernel/ABI gaps does that expose before the
Docker networking work continues?

## Findings

- Alpine latest-stable riscv64 minirootfs plus APK-expanded `nftables`,
  `iptables`, and `iproute2` is enough for the next ABI probe. The staged
  rootfs lives under `target/rootfs/alpine-rv64-qemu`; the generated initramfs
  is `target/images/alpine-initramfs-rv64-qemu.cpio`.
- Alpine's `/usr/sbin/nft` is a dynamic PIE using `/lib/ld-musl-riscv64.so.1`.
  txKernel needed to treat ET_DYN programs with `PT_INTERP` as interpreter-led
  dynamic binaries, not as already-final static PIE layouts.
- Real `nft/libnftables` uses a deeper userspace stack and larger netlink
  send/recv buffers than the previous probes. The userspace stack reservation
  is now 8 MiB, and `NETLINK_NETFILTER` sendmsg/recvmsg accepts large batches
  and large receive buffers.
- Real nft sets `SOL_NETLINK/NETLINK_EXT_ACK`; txKernel now accepts and reports
  that option for route and netfilter netlink sockets.
- Real nf_tables numeric attributes use network-order values, while older local
  test helpers used little-endian fixtures. The nfnetlink parser accepts both,
  and txKernel renders nftables dump attributes in network order.
- `nft list ruleset` probes more than tables/chains/rules. Empty set, setelem,
  object, object-reset, and flowtable dumps must return `NLMSG_DONE` rather
  than `EOPNOTSUPP`, because "no objects" is different from "unsupported ABI".
- Real nft optimizes `ip saddr 172.17.0.0/16` as `payload load 2b @ network
  header + 12` plus `cmp`, rather than always emitting a 4-byte load plus
  bitwise mask. The parser now treats 1/2/3/4 byte address loads as /8, /16,
  /24, and /32 CIDR matches when no explicit mask is present.
- Dumped MASQUERADE rules must contain real nft expressions. Userdata-only rule
  summaries are visible to txKernel tests but are ignored by the real frontend.
  The dump path now emits nested `payload`, `cmp`, and `masq` expressions for
  the staged MASQUERADE rule.
- Alpine `iptables` is present and points at the nft backend, but
  `iptables --version` still reports `Failed to initialize nft: Invalid
  argument`. That is now a focused next ABI target, not a rootfs/exec blocker.

## Applicability To txKernel

- Adopt the Alpine profile as the real-userspace ABI probe path while keeping
  the existing static BusyBox bootstrap shell. This avoids conflating shell
  startup with the real `nft` frontend.
- Keep mapping real nf_tables messages into the existing txKernel
  `NetfilterRule` model. The implementation should not grow a second firewall
  engine just to satisfy frontend syntax.
- Defer full nftables set/map/object/flowtable semantics. Empty dumps are
  enough for the current Docker bridge NAT path, and later stages can replace
  those stubs only when a real command needs them.
- Treat `iptables-nft` as the next frontend-specific compatibility slice after
  this `nft` stage-6 success.

## Sources

- `tools/images/fetch-alpine-rv64.sh`
- `tools/images/alpine-rv64.SOURCE`
- `tools/shell-tests/alpine-nft-iptables-smoke.txt`
- `tools/shell-tests/alpine-nft-nat-ruleset.txt`
- Alpine latest-stable riscv64 minirootfs and packages from
  `https://mirrors.tuna.tsinghua.edu.cn/alpine`
