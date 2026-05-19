# Netfilter Userspace ABI Notes

Date: 2026-05-19

Scope: N72M research for the Docker-shaped network path.

## Legacy iptables

The legacy IPv4 iptables ABI is not a new device node. User space opens an
IPv4 socket and talks to the kernel with `getsockopt(2)` / `setsockopt(2)` at
`IPPROTO_IP`.

Local Linux UAPI reference:

- `/usr/include/linux/netfilter_ipv4/ip_tables.h`
- `/usr/include/linux/netfilter/x_tables.h`

Relevant constants from `ip_tables.h`:

- `IPT_BASE_CTL = 64`
- `IPT_SO_SET_REPLACE = 64`
- `IPT_SO_SET_ADD_COUNTERS = 65`
- `IPT_SO_GET_INFO = 64`
- `IPT_SO_GET_ENTRIES = 65`

Relevant request layouts:

- `struct ipt_getinfo`: caller supplies table name; kernel fills hook metadata,
  entry count, and entry byte size.
- `struct ipt_get_entries`: caller supplies table name and entry byte size;
  kernel fills the serialized `struct ipt_entry` array.
- `struct ipt_replace`: mutation path used by legacy `iptables` to replace a
  whole table.

Current txKernel N72M implementation is intentionally a probe-level stub:

- `getsockopt(IPPROTO_IP, IPT_SO_GET_INFO)` returns an empty `ipt_getinfo`
  sized buffer.
- `getsockopt(IPPROTO_IP, IPT_SO_GET_ENTRIES)` returns an empty
  `ipt_get_entries` header-sized buffer.
- legacy table mutation through `setsockopt(IPPROTO_IP, IPT_SO_SET_REPLACE)`
  and `IPT_SO_SET_ADD_COUNTERS` is recognized but returns `EOPNOTSUPP`.

This is enough to distinguish "known ABI but table engine not implemented" from
`ENOPROTOOPT`, while keeping real rule mutation on the staging
`/proc/net/tx_nf_rules` path until x_tables serialization exists.

## nftables / nfnetlink

The nftables ABI uses netlink protocol `NETLINK_NETFILTER = 12`, not the
legacy IPv4 sockopt table replacement API.

Local Linux UAPI reference:

- `/usr/include/linux/netlink.h`
- `/usr/include/linux/netfilter/nfnetlink.h`
- `/usr/include/linux/netfilter/nf_tables.h`

Relevant constants from `nfnetlink.h`:

- `NFNETLINK_V0 = 0`
- `NFNL_SUBSYS_NFTABLES = 10`
- `NFNL_MSG_BATCH_BEGIN = NLMSG_MIN_TYPE`
- `NFNL_MSG_BATCH_END = NLMSG_MIN_TYPE + 1`

The real nftables path needs a `NETLINK_NETFILTER` socket, nfnetlink batch
message parsing, nf_tables object serialization, and mapping nft expressions
onto the txKernel netfilter rule/conntrack model. That is larger than N72M's
probe stub and should be a separate stage after DNAT and cleanup have stable
tests.

## Next Implementation Step

N72M+ should add a `NETLINK_NETFILTER` socket kind and return explicit
`NLMSG_ERROR` responses for unsupported nf_tables requests before attempting
real table creation. After that, implement read-only nft table/list-chain/list-
rule dumps, then mutation for a tiny subset equivalent to the current staging
commands:

- MASQUERADE for a source CIDR and output iface.
- DNAT for protocol, public destination/port, and private destination/port.
- FORWARD filter ACCEPT/DROP by input/output iface.
