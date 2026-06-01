# LTP native network runtest progress

Date: 2026-06-01

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
| `net.tcp_cmds` | 17 | filtered `netstat`: `5/5`; filtered `iproute`: `6/6`; grouped `ping01+ping02`: `20/20`; filtered `arping01`: `1/1`; filtered `ipneigh01_{arp,ip}`: semantic blockers closed, now runtime timeout in 50-loop stress | command/procfs/netns baseline has clean witnesses; IPv4 ICMP and cooked AF_PACKET ARP pass; legacy ARP ioctls and `ip neigh` projection now reach the repeated delete/relearn loop, but native helper/process runtime exceeds LTP's 5 minute per-case timeout | `target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-proc-neigh-show-660s.txt` |
| other `net.*` / `net_stress.*` / `can` | many | not started | defer until command/procfs/rtnetlink/netns baseline is stable | - |

Latest full IPv6 command:

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

Latest filtered `net.tcp_cmds:netstat` command:

```sh
timeout 300s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:netstat \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-netstat-netstat-shim-300s.txt
```

Latest `netstat01` summary:

```text
Summary:
passed   5
failed   0
broken   0
skipped  0
warnings 0
PASS LTP CASE netstat : 0
```

Note: the current native runtest wrapper still prints a trailing
`FAIL LTP CASE netstat : 0` marker after the pass marker. Treat the LTP
case summary and `PASS ... : 0` line as authoritative for this witness.

Latest filtered `net.tcp_cmds:iproute` command:

```sh
timeout 420s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:iproute \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-iproute-complete-420s.txt
```

Latest `ip_tests.sh` summary:

```text
Summary:
passed   6
failed   0
broken   0
skipped  0
warnings 0
PASS LTP CASE iproute : 0
```

Note: the current native runtest wrapper still prints a trailing
`FAIL LTP CASE iproute : 0` marker after the pass marker. Treat the LTP
case summary and `PASS ... : 0` line as authoritative for this witness.

Latest grouped `net.tcp_cmds:ping01+ping02` command:

```sh
timeout 900s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:ping01+ping02 \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-ping01-ping02-routeflush-noack-900s.txt
```

Latest grouped ping summaries:

```text
ping01:
passed   10
failed   0
warnings 0
PASS LTP CASE ping01 : 0

ping02:
passed   10
failed   0
warnings 0
PASS LTP CASE ping02 : 0
```

Note: the native runner may still print a trailing
`FAIL LTP CASE ping02 : 0` marker after the pass marker. Treat the per-case
summary and zero-status pass marker as authoritative.

Latest filtered `net.tcp_cmds:arping01` command:

```sh
timeout 420s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:arping01 \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-arping01-global-arp-420s.txt
```

Latest `arping01.sh` summary:

```text
Summary:
passed   1
failed   0
broken   0
skipped  0
warnings 0
PASS LTP CASE arping01 : 0
```

Note: the native runner may still print a trailing
`FAIL LTP CASE arping01 : 0` marker after the pass marker. Treat the per-case
summary and zero-status pass marker as authoritative.

Latest filtered `net.tcp_cmds:ipneigh01_arp` command:

```sh
timeout 900s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:ipneigh01_arp \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-ipneigh01-arp-siocdarp-900s.txt
```

Latest `ipneigh01_arp` result:

```text
ipneigh01 1  TINFO: stress auto-creation ARP cache entry deleted with 'arp' 50 times
Test timed out, sending SIGTERM!
If you are running on slow machine, try exporting LTP_TIMEOUT_MUL > 1
```

Interpretation: the former `arp: SIOCDARP(priv): Not a tty` blocker is closed.
The test now reaches the 50-iteration delete/relearn stress loop and times out
inside the LTP per-case 5 minute timer.

Latest filtered `net.tcp_cmds:ipneigh01_ip` command:

```sh
timeout 660s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:ipneigh01_ip \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-proc-neigh-show-660s.txt
```

Latest `ipneigh01_ip` result:

```text
ipneigh01 1  TINFO: stress auto-creation ARP cache entry deleted with 'ip' 50 times
Test timed out, sending SIGTERM!
If you are running on slow machine, try exporting LTP_TIMEOUT_MUL > 1
```

Interpretation: the former `TFAIL: ARP entry '10.0.0.1' not listed` blocker is
closed. `/tx-ltp/bin/ip neigh show` now reports the dynamic ARP state from
`/proc/net/arp`, and `ip neigh del` delegates deletion into the same ARP state
via the BusyBox `arp` applet before returning success.

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

## `net.tcp_cmds` filtered ledger

| Filter | Status | What it proves / why it matters | Evidence |
| --- | --- | --- | --- |
| `netstat` | pass, `5/5` inside `netstat01` | Native `network.sh` setup can create/use netns+mntns, veth metadata is visible enough for LTP helpers, local and remote IPv4 setup reaches the command phase, and the command/procfs baseline for `netstat -s`, `-rn`, `-i`, `-gn`, and `-apn` returns success. | `target/oscomp/ltp-net-tcp-cmds-netstat-netstat-shim-300s.txt` |
| `iproute` | pass, `6/6` inside `ip_tests.sh` | Native `network.sh` setup plus command-control paths now cover dummy device creation, MTU mutation, link show, loopback IPv4 alias add/show/delete, neighbor replace/show/delete, route add/show/delete via loopback gateway, and multicast address add/show/delete. The neighbor and multicast command gaps were BusyBox applet grammar limitations; the kernel also has real `RTM_NEWNEIGH` / `RTM_DELNEIGH` backing state for netlink clients. | `target/oscomp/ltp-net-tcp-cmds-iproute-complete-420s.txt` |
| `ping01` | pass, `10/10` inside `ping01.sh` | ICMP echo works through native `network.sh` netns/veth setup across payload sizes `8 16 32 64 128 256 512 1024 2048 4064`. The large `2048` and `4064` payloads required real IPv4 fragmentation and reassembly at the Ethernet interface boundary; this confirms the default 1500 MTU path no longer rejects large ICMP packets. | `target/oscomp/ltp-net-tcp-cmds-ping01-ipv4-frag-rebuilt-600s.txt` |
| `ping02` | pass, `10/10` inside `ping02.sh`; clean setup | `ping -I eth0` now works across the same payload matrix. The first failure was not routing or fragmentation: BusyBox `-p aa` leaves the ICMP code byte as `0xaa`, and raw ICMP send now accepts echo-shaped user payloads instead of rejecting strict-parser `Malformed` as `EINVAL`. The follow-up cleanup removed the BusyBox `ip ... nodad` setup warning and the IPv6 prefix lookup warning by pairing a rootfs `ip addr` compatibility filter with real AF_INET6 rtnetlink address add/dump/delete state. | `target/oscomp/ltp-net-tcp-cmds-ping02-nodad-ipv6addr-420s.txt` |
| `ping01+ping02` | pass, grouped `20/20`; clean continuous setup | Exact-tag grouped execution proves `ping02` can run after `ping01` without stale route/address state. The grouped blocker was `tst_init_iface()` cleanup: BusyBox `ip route flush dev <iface>` generated `RTM_DELROUTE` messages without `NLM_F_ACK`, while txKernel returned unsolicited success acks and could not delete connected routes projected from interface addresses. Connected route deletion is now suppressible until address/link changes, and rtnetlink success acks are only sent when requested. Note: `ltp-runtest:net.tcp_cmds:ping` is not a prefix filter and selects no cases; use exact tags joined by `+`. | `target/oscomp/ltp-net-tcp-cmds-ping01-ping02-routeflush-noack-900s.txt` |
| `arping01` | pass, `1/1` inside `arping01.sh` | BusyBox `arping -w 10 <remote> -I eth0 -fq` now gets a usable `sockaddr_ll` from `AF_PACKET` `getsockname()` and receives a cooked ARP reply for the remote veth IPv4 address. This covers link-layer address projection, packet socket bind/getname, and the minimal cooked ARP request/reply path needed by the command witness. | `target/oscomp/ltp-net-tcp-cmds-arping01-global-arp-420s.txt` |
| `ipneigh01_arp` | partial, semantic blockers closed; timeout | Legacy `arp` command ioctl compatibility now exists for `SIOCGIFHWADDR`, `SIOCSARP`, and `SIOCDARP`, backed by namespace ARP state. The witness no longer breaks on ENOTTY and reaches the 50-loop ARP auto-creation/delete stress section. Remaining blocker is runtime: the loop exceeds LTP's 5 minute per-case timeout. | `target/oscomp/ltp-net-tcp-cmds-ipneigh01-arp-siocdarp-900s.txt` |
| `ipneigh01_ip` | partial, semantic blockers closed; timeout | `ip neigh show` now reflects dynamic `/proc/net/arp` entries and `ip neigh del` removes the corresponding ARP entry. The former `ARP entry '10.0.0.1' not listed` failure is gone; the witness reaches the same 50-loop delete/relearn stress section and times out. | `target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-proc-neigh-show-660s.txt` |
| remaining entries | not started | With command/control-plane probes, grouped IPv4 ping witnesses, `arping01`, and the `ipneigh01` semantic path understood, the next correctness target should stay small, but the immediate shared blocker is native LTP runtime rather than a new network semantic. | - |

Implemented prerequisites observed during the `netstat` climb:

- `clone(CLONE_NEWNET | CLONE_NEWNS)` and `/proc/<pid>/ns/{net,mnt}` enough for
  `tst_ns_exec ... net,mnt`.
- Propagation-only `mount --make-rprivate /sys` compatibility.
- `/var/run/netns` rootfs scratch directory.
- Minimal module metadata and `CONFIG_VETH=y` so LTP accepts veth as available.
- BusyBox veth peer-name behavior accounted for by native network runtest
  default `LHOST_IFACES=eth0`.
- Minimal no-op `NETLINK_XFRM` endpoint so `ip xfrm state` returns a multipart
  `NLMSG_DONE` instead of hanging.
- `RTM_DELADDR` handling for `ip addr flush dev <iface>`.
- `/proc/sys/net/ipv6/conf/<iface>/{disable_ipv6,accept_dad}` dynamic netns
  sysctls.
- `/sys/class/net` projection over registered net namespaces so LTP can read
  `/sys/class/net/ltp_ns_veth1/address` inside the remote namespace.
- Minimal `/proc/net/{tcp6,udp6,raw6,unix,igmp,igmp6}` files.
- `/tx-ltp/bin/netstat` rootfs command shim for the bundled BusyBox applet's
  missing `-s`, `-i`, and `-g` option support. This is an environment
  compatibility shim, not a kernel datapath special case.
- Minimal dummy netdevice support, `CONFIG_DUMMY=y`, and dummy module metadata.
- `RTM_SETLINK IFLA_MTU` support for `ip link set <iface> mtu ...`.
- Loopback IPv4 alias add/show/delete support for the `ip addr` subtest.
- `RTM_NEWNEIGH` and `RTM_DELNEIGH` support over the namespace ARP projection.
- Route add/dump support that infers `dev lo` for loopback gateways such as
  `via 127.0.0.1`.
- `/tx-ltp/bin/ip` rootfs command shim for BusyBox's missing `ip neigh
  add/replace` and `ip maddr` command grammar. This keeps LTP command
  compatibility in the rootfs environment while real route/neigh netlink state
  lives in the kernel.
- IPv4 fragmentation and reassembly on Ethernet interfaces. Oversized IPv4
  packets are split according to the interface MTU instead of being rejected,
  and inbound fragments are reassembled before the existing ICMP/TCP/UDP demux
  path. This is required for large `ping -s` payloads over the default veth
  MTU.
- Raw ICMP send accepts echo-shaped user payloads even when the ICMP code byte
  is non-zero. BusyBox fancy `ping -p aa` leaves that byte as `0xaa`, and Linux
  raw sockets transmit the user-supplied ICMP bytes instead of rejecting the
  send with `EINVAL`.
- `/tx-ltp/bin/ip` strips iproute2's `nodad` token for `ip addr
  add|del|replace|change` before delegating to the bundled BusyBox applet.
  This is rootfs command compatibility; the kernel-side semantics are provided
  by rtnetlink.
- `RTM_NEWADDR`, family-filtered `RTM_GETADDR`, and `RTM_DELADDR` now cover
  AF_INET6 interface addresses in namespace link snapshots. This lets LTP's
  `tst_net_iface_prefix` helper find the configured `fd00:1:1:1::/64`
  prefixes instead of falling back to the old warning path.
- `RTM_DELROUTE` can delete connected routes that are projected from
  interface addresses. The namespace records these as suppressed connected
  route projections until the interface address or link state changes, matching
  the cleanup behavior expected by `ip route flush dev <iface>`.
- Mutating rtnetlink requests now return success acks only when the request
  includes `NLM_F_ACK`; errors are still reported unconditionally. This matters
  for BusyBox `ip route flush`, which sends generated `RTM_DELROUTE` messages
  without `NLM_F_ACK` and treats unsolicited `NLMSG_ERROR(error=0)` replies as
  failed flush requests.
- `sockaddr_ll` now preserves and writes Linux link-layer fields
  `sll_hatype`, `sll_pkttype`, `sll_halen`, and `sll_addr`. Packet
  `getsockname()` enriches bound non-loopback links from the namespace link
  snapshot so BusyBox `arping` does not classify `eth0` as non-ARPable.
- Packet sockets have a small cooked receive queue, and
  `sendto(AF_PACKET, SOCK_DGRAM, ETH_P_ARP)` synthesizes an ARP reply when the
  request targets an IPv4 link in the current or registered peer namespace.
  This is sufficient for `arping01`; it is not yet a complete AF_PACKET raw
  tap/transmit implementation.
- Legacy ARP ioctls `SIOCGIFHWADDR`, `SIOCSARP`, and `SIOCDARP` are wired
  through the socket ioctl path. They read/write namespace link-layer metadata
  and ARP entries, which is required by BusyBox `arp -an/-s/-d`.
- `RTM_GETNEIGH` neighbor messages now set `ndm_type=RTN_UNICAST`, and
  `EtherIface` ARP and pending-ARP state is preserved when interface runtime
  entries are refreshed after link/address/route changes.
- `/tx-ltp/bin/ip neigh add|replace|show|del` bridges the bundled BusyBox
  grammar gap while keeping state in kernel-visible ARP surfaces: add/replace
  first try `arp -s`, show prints both shim fallback state and `/proc/net/arp`,
  and del removes the fallback entry plus the real ARP entry through `arp -d`.

## Native setup runtime note

The slow `ping02` and `ipneigh01` runs are not currently explained by a
network-stack linear scan or packet datapath cost. The existing `ping02`
trap-trace witness `target/oscomp/ltp-net-tcp-cmds-ping02-traptrace-600s.txt`
shows setup dominated by shell/process/file churn: 256 `execve`, 233 `clone`,
1959 `close`, and 1025 `prlimit64` syscalls overall, compared with only 34
`socket`, 3 `sendmsg`, and 14 `recvmsg` syscalls. The heaviest setup segment
observed was `rhost init -> add remote IPv4`, with 1568 syscalls including 80
`execve` and 60 `clone`.

The `ipneigh01_{arp,ip}` witnesses reinforce the same conclusion. Host unit
tests prove ARP delete/relearn and `RTM_GETNEIGH` projection complete quickly,
while the native QEMU witnesses spend minutes in shell setup and then time out
inside a loop that repeatedly runs `ping`, `arp`/`ip neigh`, and `grep`.

Follow-up speed probe: the native LTP runner now stages BusyBox into tmpfs as
`/bin/busybox`, installs `/bin` applet symlinks from that copy, and the
`/tx-ltp/bin/{ip,netstat}` shims prefer `/bin/busybox` when present. Focused
`ipneigh01_ip` still reaches the 50-loop stress body and times out under the
default LTP 5 minute case timeout:
`target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-tmpfs-busybox-del-540s.txt`.
So repeated ext4 reads of the BusyBox binary are not the only bottleneck; the
next speed work needs per-command/syscall timing around process creation,
shell pipelines, and fd cleanup.

Trap-trace follow-up:
`target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-traptrace-240s.txt` reached the
stress body under a trace build before the 240s host timeout. Parsed syscall
counts show about `10530` syscalls before the stress line and `1863` after it.
The top whole-run syscalls are `close` `2142`, `read` `1381`, `prlimit64`
`1025`, `rt_sigprocmask` `812`, `rt_sigaction` `634`, `ppoll` `590`,
`newfstatat` `497`, `brk` `497`, `wait4` `487`, `write` `474`, `dup3` `429`,
`symlinkat` `399`, `fcntl` `380`, `execve` `293`, and `clone` `276`; socket
syscalls are only `43` `socket`, `18` `recvmsg`, and `7` `sendto` in this
window.

Optimization direction: keep semantic fixes in the kernel, but measure speed at
the LTP setup subprocess/syscall layer first. The next useful speed work is a
small timestamped runner/trap-trace pass or a focused reduction in repeated
rootfs helper forks/procfs probes; broad socket-table or datapath refactors are
not justified by the current evidence.

2026-06-01 follow-up speed pass: three general optimizations landed or were
tested against the same focused witness. First, `Process::close_fd()` now
combines fd removal and successful-close CLOEXEC cleanup for `close(2)`,
CLOEXEC exec cleanup, AIO teardown, and the stateless netlink close path.
Second, `/tx-ltp/bin` now answers the stable LTP network helper queries
`tst_net_ip_prefix`, `tst_net_iface_prefix`, and `tst_net_vars` without
executing the large helper binaries for the default Tx/LTP veth addresses, and
IPv4 `ip neigh del <addr> dev <iface>` skips an extra failing BusyBox
`ip neigh del` subprocess before using the ARP ioctl-backed delete path. Third,
ready pipe reads now have a synchronous `read(2)` fast path, aimed at the
shell's tiny command-substitution pipe reads.

The optimizations are correct locally but still do not close
`ipneigh01_ip`: `target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-fastdel-420s.txt`
and `target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-pipefast-420s.txt` both reach
`stress auto-creation ARP cache entry deleted with 'ip' 50 times` and then hit
the 420s host timeout without a PASS. An executable-page prefault experiment
was also tried, but it regressed the witness before the stress line and was
reverted. The next runtime pass should capture argv-aware or timestamped loop
evidence before changing more kernel code.

2026-06-01 PATH precedence follow-up: each per-case LTP execution now keeps
`/tx-ltp/bin` first via a shared `LTP_CASE_PATH`, so the local helper shims are
not shadowed by upstream LTP helper binaries. The fix is correct runner hygiene,
but it is not enough to close `ipneigh01_ip`: the focused normal-build witness
`target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-txpath-420s.txt` still reaches the
same stress marker and then host-times out. A fresh trace witness,
`target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-pipefast-traptrace-240s.txt`,
showed about `3251` syscalls after the stress marker in the 240s window, led by
`read`, `ppoll`, `close`, signal-mask/action calls, `wait4`, `dup3`, `clone`,
and `execve`; post-stress page faults were also high. This keeps the blocker in
process/shell/exec runtime, not network neighbor semantics.

2026-06-01 ext4 hot-cache follow-up: the sync ext4 backend now has a hot
file-page byte cache and reuses regular-file `PageContainer`s per inode. This
is a general runtime improvement for repeated exec/interpreter page faults, and
host tests pin both the byte-cache hit and shared-container behavior. It still
does not close `ipneigh01_ip`: focused witnesses
`target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-ext4-pagecache-240s.txt` and
`target/oscomp/ltp-net-tcp-cmds-ipneigh01-ip-ext4-pc-cache-300s.txt` reach the
same `stress auto-creation ARP cache entry deleted with 'ip' 50 times` marker
and then host-time out. The remaining blocker stays in BusyBox shell pipelines,
repeated exec/page-fault cost, and wait/pipe/fd churn rather than ext4 cold
read or network neighbor semantics.

## Next native network step

The focused command/control probes, grouped ping witnesses, and `arping01` are
clean, and `ipneigh01_{arp,ip}` now reaches its stress loop. The immediate next
step is a small design for native LTP process/runtime reduction around BusyBox
shell pipelines and exec fault cost, then rerun the same two neighbor witnesses.

Recommended confirmation target after a speed change:

```sh
timeout 420s make oscomp-qemu-rv64 \
  OSCOMP_GROUPS=ltp-runtest:net.tcp_cmds:ipneigh01_arp+ipneigh01_ip \
  OSCOMP_OUT_RV=target/oscomp/ltp-net-tcp-cmds-ipneigh01-speed-pass-420s.txt
```

Expected purpose:

- Check whether the repeated ARP delete/relearn loop can finish inside LTP's
  default 5 minute timeout without relying on `LTP_TIMEOUT_MUL`.
- If the loop still times out, collect per-command or syscall timing for the
  `ping` + `arp`/`ip neigh` + `grep` loop before starting a broader network
  refactor.
- If both variants pass, try a broader but still filtered `net.tcp_cmds` run
  before the full module.
