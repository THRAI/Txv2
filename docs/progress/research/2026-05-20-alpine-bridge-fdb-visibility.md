# 2026-05-20 Alpine bridge/FDB visibility probe

## Context

After N73F added minimal AF_PACKET ABI support, the next Docker-adjacent surface
was bridge visibility through real Alpine `iproute2`. This stage probes
additional rtnetlink bridge/link/neigh behavior without expanding into Docker
daemon startup or OpenRC service-manager work.

## Change

Added `tools/shell-tests/alpine-bridge-fdb-visibility-focused-probe.txt`.

The probe:

- creates a holder network namespace with `/bin/netns-helper`
- creates host `docker0`
- assigns `172.17.0.1/16` to `docker0`
- creates `veth0`/`eth0`
- attaches `veth0` to `docker0`
- moves `eth0` into the holder namespace
- assigns `172.17.0.2/16` to namespace `eth0`
- verifies `ip -d link show dev docker0` reports `bridge`
- verifies `ip -d link show dev veth0` reports `veth`
- verifies `bridge link show` reports `veth0 master docker0`
- verifies `bridge fdb show br docker0` completes successfully
- pings `172.17.0.1` from the namespace
- verifies `ip neigh show dev docker0` reports `172.17.0.2`

## Verification

- `TX_ALPINE_ROOTFS=target/rootfs/alpine-openrc-rv64-qemu cargo xtask shell-test --target rv64-qemu --profile alpine --script tools/shell-tests/alpine-bridge-fdb-visibility-focused-probe.txt`

## Findings

No new network ABI gap was observed.

The existing rtnetlink/link and bridge model is enough for these real Alpine
surfaces:

- detailed link dumps include link kind for bridge and veth
- bridge master projection is visible to `bridge link show`
- `bridge fdb show br docker0` receives a successful empty dump
- namespace-to-bridge ping populates host neighbor state
- `ip neigh show dev docker0` reports the namespace peer as reachable

The bridge FDB command does not yet project learned bridge MAC rows. That is
recorded as a future network enhancement rather than a blocker, because the
real Alpine command succeeded and no current Docker-shaped probe requires those
rows.

## Next

Continue with Docker-daemon-adjacent probes that expose concrete network ABI
gaps:

- route/neigh detail variants not covered by the current scripts
- capability checks around `CAP_NET_ADMIN` and `CAP_NET_RAW`
- procfs/sysfs visibility for bridge/NAT/conntrack state
- nfnetlink/netfilter requests beyond the current iptables-nft/nft paths
