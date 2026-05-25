# N72R/S nftables Userspace Probe

Date: 2026-05-20

## Context

The N72N-O stages added a minimal `NETLINK_NETFILTER` / nf_tables adapter over
the existing txKernel netfilter model. The next requested goal was to continue
through N72S: move beyond host-side fixtures and prove the ABI through a
userspace path.

## Empirical Result

The current in-repository RV64 BusyBox image does not contain `nft`,
`iptables`, `iptables-nft`, or `iptables-legacy`. The host has `/usr/sbin/nft`
and `/usr/sbin/iptables`, but those are host-architecture binaries and cannot
run inside the RV64 QEMU guest. No RV64 nftables binary was present under the
local workspace search path.

Because of that, N72R could not yet run the real Alpine `nft` frontend. Instead
this stage adds a small RV64 userspace probe, `tools/user/nft-probe.c`, that
uses the same kernel-facing syscall surface:

- `socket(AF_NETLINK, SOCK_RAW | SOCK_NONBLOCK | SOCK_CLOEXEC,
  NETLINK_NETFILTER)`
- `sendto()` with nfnetlink/nf_tables batch messages
- `recvfrom()` for `NLMSG_ERROR`, dump messages, and `NLMSG_DONE`

This is still not proof that Alpine's exact `nft` binary runs. It is proof that
the production userspace syscall path can create, dump, and delete the current
minimal nftables ruleset shape.

## Implemented Coverage

The probe sends a batch create sequence:

- `NFNL_MSG_BATCH_BEGIN`
- `NFT_MSG_NEWTABLE` for table `nat`
- `NFT_MSG_NEWCHAIN` for base chain `postrouting`
- `NFT_MSG_NEWRULE` with `oifname docker0`, source CIDR `172.18.0.0/16`, and
  `masq`
- `NFNL_MSG_BATCH_END`

Then it dumps:

- `NFT_MSG_GETTABLE`
- `NFT_MSG_GETCHAIN`
- `NFT_MSG_GETRULE`

Finally it deletes:

- `NFT_MSG_DELRULE` handle `1`
- `NFT_MSG_DELCHAIN`
- `NFT_MSG_DELTABLE`

The focused shell-test is
`tools/shell-tests/busybox-nft-probe.txt`.

## Kernel Test Coverage

`nfnetlink_batch_create_dump_and_delete_masquerade_rule` mirrors the same
batch/create/dump/delete shape inside `tx-subsystems`, so parser regressions
are caught without booting QEMU.

## Verification

- `cargo fmt --check`
- `cargo test -p tx-subsystems nfnetlink`
- `cargo test -p tx-shims netlink`
- `cargo xtask image cpio --profile busybox --target rv64-qemu`
- `cargo xtask shell-test --target rv64-qemu --script tools/shell-tests/busybox-nft-probe.txt`
- `cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`

## Next

When an RV64 Alpine userspace or standalone static `nft` binary is available,
replace `nft-probe` with real commands and keep the probe as a narrow regression
tool. The next likely ABI gaps will be richer nft expression forms, rule
handles/positions, sets/maps, and frontend-specific dump expectations.
