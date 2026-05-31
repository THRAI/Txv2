# LTP native network runtest progress

Date: 2026-05-31

This file tracks native LTP network runtest modules such as `runtest/net.*`,
`runtest/net_stress.*`, and `runtest/can`. It is separate from
`docs/LTP/ltp-network-syscall-progress.md`, which tracks the 50 socket/network
syscall cases from `runtest/syscalls`.

本文档记录原生 LTP 网络模块的进度。它不是 syscall 50 例的表，而是
`ltp-runtest:<module>` 这一类 upstream `runtest/net.*` 模块的通过情况。

## Current status

| Module | Entries | Latest judge | Kernel-side status | Latest log |
| --- | ---: | ---: | --- | --- |
| `net.ipv6_lib` | 6 | `76/77` | Phase 1 kernel-side baseline complete; only `hopopt` is a musl test-image/libc table miss | `target/oscomp/ltp-net-ipv6-lib-final-lhost-hopopt-known-120s.txt` |
| `net.tcp_cmds` | 17 | not started in native runner | next target, start filtered instead of full module | - |
| other `net.*` / `net_stress.*` / `can` | many | not started | defer until command/procfs/rtnetlink/netns baseline is stable | - |

Latest full command:

```sh
timeout 120s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.ipv6_lib \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-ipv6-lib-final-lhost-hopopt-known-120s.txt

python3 tools/oscomp-judge.py \
  target/oscomp/ltp-net-ipv6-lib-final-lhost-hopopt-known-120s.txt \
  target/oscomp/testdata
```

Latest judge output:

```text
[ltp-musl] 76/77
  ✓ in6_01  5/5
  ✓ in6_02  3/3
  ✓ getaddrinfo_01  22/22
  ~ asapi_01  16/17
  ✓ asapi_02  12/12
  ✓ asapi_03  18/18

总分: 76/77
```

## `net.ipv6_lib` case ledger

| Case | Score | Status | What it proves / why it matters | Evidence |
| --- | ---: | --- | --- | --- |
| `in6_01` | `5/5` | pass | IPv6 libc-visible structure constants and address macros are usable under the current image. | final full log |
| `in6_02` | `3/3` | pass | Interface name/index APIs enumerate `lo` and `virtio-net0`; LTP now receives `LHOST_IFACES=virtio-net0` from the runner environment. | `target/oscomp/ltp-net-ipv6-lib-in6-02-lhost-ifaces-60s.txt` and final full log |
| `getaddrinfo_01` | `22/22` | pass | `/etc/hosts`, `/etc/services`, IPv4/IPv6 address-family handling, and `getaddrinfo()` paths satisfy the LTP witness. | final full log |
| `asapi_01` | `16/17` | partial | All `IPV6_CHECKSUM` socket-option subcases pass. The only miss is `getprotobyname("hopopt")`. | `target/oscomp/ltp-net-ipv6-lib-asapi01-rawv6-60s.txt`, `target/oscomp/ltp-net-ipv6-lib-asapi01-prefix-protocols-60s.txt`, and final full log |
| `asapi_02` | `12/12` | pass | `AF_INET6` raw ICMPv6 sockets, loopback delivery, and `ICMP6_FILTER` pass the LTP filter matrix. | `target/oscomp/ltp-net-ipv6-lib-asapi02-rawv6-60s.txt` and final full log |
| `asapi_03` | `18/18` | pass | IPv6 raw socket receive-option set/get and `recvmsg()` control-message surfaces pass, including `IPV6_PKTINFO`, `IPV6_HOPLIMIT`, `IPV6_TCLASS`, and old `IPV6_2292*` forms. | `target/oscomp/ltp-net-ipv6-lib-asapi03-rawv6-bindfix-60s.txt` and final full log |

## `asapi_01` `hopopt` interpretation

Do not spend kernel-network time on the remaining `asapi_01` point unless the
test image changes.

Observed failure:

```text
asapi_01    2  TFAIL  :  asapi_01.c:119: "hopopt" protocols entry
```

Reason:

- The test calls user-space libc `getprotobyname("hopopt")`.
- In the current musl-linked LTP binary, `getprotobyname()` walks musl's
  compiled-in protocol table in `src/network/proto.c`.
- That table contains entries such as `ip`, `ipv6`, `ipv6-route`,
  `ipv6-frag`, `esp`, `ah`, `ipv6-icmp`, `ipv6-nonxt`, and `ipv6-opts`, but
  not `hopopt`.
- A kernel/rootfs probe confirmed that adding `hopopt 0` to `/etc/protocols`
  and even copying it to `/musl/musl/etc/protocols` does not change this
  statically linked musl result.

Policy for this project:

- Do not modify LTP test scripts or prebuilt test binaries just to pass this
  point.
- Do not add kernel special cases that recognize `asapi_01`, patch user memory,
  or otherwise fake libc results.
- Record `net.ipv6_lib` as `76/77` with the single remaining point outside the
  kernel networking implementation surface.

If a future local experiment is explicitly allowed to rebuild the test image,
the clean libc-side fix is to patch/rebuild musl's protocol table and rebuild
the musl-linked LTP binaries. That is not part of the kernel submission path.

## Implemented kernel/userland prerequisites for this module

The current passing `net.ipv6_lib` coverage depends on these kernel-side or
boot-environment surfaces:

- Native `ltp-runtest:<module>` runner path for upstream runtest files.
- Writable minimal `/etc/hosts`, `/etc/services`, and `/etc/protocols`.
- IPv6 proc/sys projection needed by LTP helpers.
- Route-netlink/interface enumeration fast enough for `if_nameindex()`.
- LTP runner defaults for `LHOST_IFACES=virtio-net0` and
  `RHOST_IFACES=virtio-net0` when the caller does not override them.
- `AF_INET6` raw sockets for `IPPROTO_ICMPV6` and protocol `159`.
- `ICMP6_FILTER`.
- IPv6 raw loopback packet delivery.
- `IPV6_CHECKSUM` set/get.
- IPv6 receive-option set/get and ancillary control-message emission for the
  `asapi_03` matrix.

## Next native network step

Start `net.tcp_cmds` with filtered entries, not the whole module.

Recommended next target:

```sh
timeout 120s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:netstat \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-netstat-baseline-120s.txt
```

Expected purpose:

- Verify `/proc/net/*` projections used by `netstat`.
- Find the first command/runtime prerequisite before full `network.sh`.
- Keep the failure local and readable before moving to `iproute`, `ping`, or
  full `net.tcp_cmds`.
